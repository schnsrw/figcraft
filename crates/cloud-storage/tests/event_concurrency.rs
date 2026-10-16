//! S2-019: multi-connection PostgreSQL races for event sequencing, sequencer fencing and
//! idempotency-key reservation.
//!
//! Every racer is its own connection and transaction on a multi-thread runtime, and the data is
//! committed, so the database's own row locks decide each race. Where an ordering matters, a gate
//! transaction holds the contended lock and the test waits, through `pg_stat_activity`, until every
//! racer is blocked on it before releasing; no sleep stands in for synchronisation. The fixture
//! documents keep their history between runs, so each test reads its starting sequence and token
//! from the database and asserts relative to them.

use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::events::{Appended, EventRepositoryError, SequencerClaim, append_events, claim_sequencer};
use figcraft_cloud_storage::idempotency::{RepositoryError, ReservationOutcome, complete_idempotency_key, reserve_idempotency_key};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::{Accepted, Change, NodeId};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::{Barrier, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;
/// One spawned racer, finishing with its own outcome or a test-harness error.
type Racer<T> = JoinHandle<TestResult<T>>;
/// The body a completer offered and whether its completion won.
type Completion = (Vec<u8>, Result<(), RepositoryError>);

const TENANT: &str = "96000000-0000-4000-8000-000000000001";
const ACTOR: &str = "96000000-0000-4000-8000-000000000009";
const SAME_SEQ_DOCUMENT: &str = "96300000-0000-4000-8000-000000000001";
const CONSECUTIVE_DOCUMENT: &str = "96300000-0000-4000-8000-000000000002";
const FENCING_DOCUMENT: &str = "96300000-0000-4000-8000-000000000003";
const TAKEOVER_DOCUMENT: &str = "96300000-0000-4000-8000-000000000004";
const ROUTE: &str = "test.route.concurrency";

/// Concurrent racers per scenario. The pool holds them plus a gate and the lock-wait poller.
const RACERS: usize = 8;
/// Upper bound on any single wait, so a lost wake-up fails the test instead of hanging it.
const BOUND: Duration = Duration::from_secs(30);

fn batch(seq: i64) -> TestResult<Accepted> {
    let seq = u64::try_from(seq)?;
    Ok(Accepted { seq, author: 1, batch_id: seq, changes: vec![Change::Delete { id: NodeId(seq) }] })
}

/// A key or request hash unique to this run, so reruns never meet an earlier run's rows.
fn run_hash(label: &str) -> [u8; 32] {
    Sha256::digest(format!("{label}:{}", CloudDocumentId::random())).into()
}

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL concurrency probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let connections = u32::try_from(RACERS + 2)?;
    Ok(Some(PgPoolOptions::new().max_connections(connections).acquire_timeout(BOUND).connect(database_url.trim_end()).await?))
}

/// A tenant transaction whose lock waits give up well inside [`BOUND`], so a failed test never
/// leaves a backend queued behind a lock forever.
async fn begin(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    let tenant: TenantId = TENANT.parse()?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    sqlx::query("SET LOCAL lock_timeout = '20s'").execute(transaction.connection()).await?;
    Ok(transaction)
}

async fn backend_pid(transaction: &mut TenantTransaction<'_>) -> TestResult<i32> {
    Ok(sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(transaction.connection()).await?)
}

async fn finish(transaction: TenantTransaction<'_>, commit: bool) -> TestResult {
    if commit {
        transaction.commit().await?
    } else {
        transaction.rollback().await?
    }
    Ok(())
}

async fn claim_committed(pool: &PgPool, document: CloudDocumentId) -> TestResult<SequencerClaim> {
    let mut transaction = begin(pool).await?;
    let claim = claim_sequencer(&mut transaction, document).await?;
    transaction.commit().await?;
    Ok(claim)
}

/// Take the document row lock that `append_events` and `claim_sequencer` contend on.
async fn lock_document(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId) -> TestResult {
    sqlx::query("SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
        .bind(TENANT)
        .bind(document.to_string())
        .fetch_one(transaction.connection())
        .await?;
    Ok(())
}

async fn receive_pids(receiver: &mut mpsc::UnboundedReceiver<i32>, count: usize) -> TestResult<Vec<i32>> {
    let mut pids = Vec::with_capacity(count);
    while pids.len() < count {
        pids.push(timeout(BOUND, receiver.recv()).await?.ok_or("racer exited before reporting its backend")?);
    }
    Ok(pids)
}

/// Wait until every listed backend is blocked on a heavyweight lock. Polling the server's view of
/// the waits is what makes the release ordering deterministic.
async fn wait_until_blocked(pool: &PgPool, pids: &[i32]) -> TestResult {
    let expected = i64::try_from(pids.len())?;
    let deadline = Instant::now() + BOUND;
    loop {
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = ANY($1) AND wait_event_type = 'Lock'")
            .bind(pids)
            .fetch_one(pool)
            .await?;
        if waiting == expected {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("only {waiting} of {expected} racers blocked on the lock").into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn join_all<T>(handles: Vec<Racer<T>>) -> TestResult<Vec<T>> {
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(timeout(BOUND, handle).await???);
    }
    Ok(results)
}

/// The document's committed counters and the shape of its event log.
#[derive(Debug, Eq, PartialEq)]
struct LogShape {
    current_seq: i64,
    rows: i64,
    distinct_seqs: i64,
    min_seq: Option<i64>,
    max_seq: Option<i64>,
    token_rewinds: i64,
}

impl LogShape {
    /// A gap-free, duplicate-free `1..=current_seq` log whose fencing tokens never go backwards.
    fn is_exact(&self) -> bool {
        let populated = self.current_seq == 0 || (self.min_seq == Some(1) && self.max_seq == Some(self.current_seq));
        populated && self.rows == self.current_seq && self.distinct_seqs == self.rows && self.token_rewinds == 0
    }
}

async fn log_shape(pool: &PgPool, document: CloudDocumentId) -> TestResult<LogShape> {
    let mut transaction = begin(pool).await?;
    let current_seq: i64 = sqlx::query_scalar("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(document.to_string())
        .fetch_one(transaction.connection())
        .await?;
    let (rows, distinct_seqs, min_seq, max_seq, token_rewinds): (i64, i64, Option<i64>, Option<i64>, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT seq), min(seq), max(seq), count(*) FILTER (WHERE rewound) \
         FROM (SELECT seq, fencing_token < lag(fencing_token) OVER (ORDER BY seq) AS rewound \
               FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid) AS log",
    )
    .bind(TENANT)
    .bind(document.to_string())
    .fetch_one(transaction.connection())
    .await?;
    transaction.rollback().await?;
    Ok(LogShape { current_seq, rows, distinct_seqs, min_seq, max_seq, token_rewinds })
}

/// Start one racer that reports its backend, then runs `append_events` and commits on success.
fn spawn_append(
    pool: &PgPool,
    pids: &mpsc::UnboundedSender<i32>,
    document: CloudDocumentId,
    fencing_token: i64,
    seq: i64,
) -> Racer<Result<Appended, EventRepositoryError>> {
    let (pool, pids) = (pool.clone(), pids.clone());
    tokio::spawn(async move {
        let actor: PrincipalId = ACTOR.parse()?;
        let mut transaction = begin(&pool).await?;
        pids.send(backend_pid(&mut transaction).await?)?;
        let result = append_events(&mut transaction, document, fencing_token, actor, &[batch(seq)?]).await;
        finish(transaction, result.is_ok()).await?;
        Ok(result)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_appends_of_one_sequence_commit_exactly_once() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = SAME_SEQ_DOCUMENT.parse()?;
    let start = claim_committed(&pool, document).await?;
    let before = log_shape(&pool, document).await?;
    assert!(before.is_exact(), "fixture log is not exact: {before:?}");
    let next = start.current_seq + 1;

    // Every racer queues behind the gate's row lock, so all of them contend for the same
    // sequence when it is released; they then serialize on the lock one at a time.
    let mut gate = begin(&pool).await?;
    lock_document(&mut gate, document).await?;
    let (pid_sender, mut pid_receiver) = mpsc::unbounded_channel();
    let racers: Vec<_> = (0..RACERS).map(|_| spawn_append(&pool, &pid_sender, document, start.fencing_token, next)).collect();
    let pids = receive_pids(&mut pid_receiver, RACERS).await?;
    wait_until_blocked(&pool, &pids).await?;
    gate.rollback().await?;

    let results = join_all(racers).await?;
    let winners = results.iter().filter(|result| **result == Ok(Appended { first_seq: next, last_seq: next })).count();
    let losers = results.iter().filter(|result| **result == Err(EventRepositoryError::SequenceConflict { current: next })).count();
    assert_eq!((winners, losers), (1, RACERS - 1), "unexpected outcomes: {results:?}");

    let after = log_shape(&pool, document).await?;
    assert!(after.is_exact(), "log is not exact after the race: {after:?}");
    assert_eq!((after.current_seq, after.rows), (next, before.rows + 1));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consecutive_writers_that_retry_on_conflict_build_a_gap_free_log() -> TestResult {
    const WRITERS: usize = 4;
    const PER_WRITER: usize = 6;
    // A conflict means another writer committed in between, so no writer can conflict more often
    // than the other writers' total commits; the bound only catches a livelock.
    const MAX_ATTEMPTS: usize = WRITERS * PER_WRITER * 2;

    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = CONSECUTIVE_DOCUMENT.parse()?;
    let start = claim_committed(&pool, document).await?;
    let before = log_shape(&pool, document).await?;
    assert!(before.is_exact(), "fixture log is not exact: {before:?}");

    let barrier = std::sync::Arc::new(Barrier::new(WRITERS));
    let writers: Vec<Racer<Vec<i64>>> = (0..WRITERS)
        .map(|_| {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            tokio::spawn(async move {
                let actor: PrincipalId = ACTOR.parse()?;
                barrier.wait().await;
                let mut written = Vec::with_capacity(PER_WRITER);
                let mut next = start.current_seq + 1;
                for _ in 0..MAX_ATTEMPTS {
                    if written.len() == PER_WRITER {
                        return Ok(written);
                    }
                    let mut transaction = begin(&pool).await?;
                    match append_events(&mut transaction, document, start.fencing_token, actor, &[batch(next)?]).await {
                        Ok(appended) => {
                            transaction.commit().await?;
                            written.push(appended.first_seq);
                            next = appended.last_seq + 1;
                        }
                        Err(EventRepositoryError::SequenceConflict { current }) => {
                            transaction.rollback().await?;
                            next = current + 1;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                if written.len() == PER_WRITER { Ok(written) } else { Err("writer exceeded its retry bound".into()) }
            })
        })
        .collect();

    let mut written: Vec<i64> = join_all(writers).await?.into_iter().flatten().collect();
    written.sort_unstable();
    let total = i64::try_from(WRITERS * PER_WRITER)?;
    let expected: Vec<i64> = (start.current_seq + 1..=start.current_seq + total).collect();
    assert_eq!(written, expected, "each sequence must be written by exactly one writer");

    let after = log_shape(&pool, document).await?;
    assert!(after.is_exact(), "log is not exact after the race: {after:?}");
    assert_eq!((after.current_seq, after.rows), (start.current_seq + total, before.rows + total));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_orders_strictly_against_an_in_flight_append() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = FENCING_DOCUMENT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let old = claim_committed(&pool, document).await?;
    let (pid_sender, mut pid_receiver) = mpsc::unbounded_channel();

    // The old owner's append holds the row lock first: the takeover waits for it, and on commit
    // the append has landed under the token that was current then. The takeover resumes after it.
    let mut in_flight = begin(&pool).await?;
    let first = old.current_seq + 1;
    assert_eq!(
        append_events(&mut in_flight, document, old.fencing_token, actor, &[batch(first)?]).await?,
        Appended { first_seq: first, last_seq: first }
    );
    let takeover = {
        let (pool, pid_sender) = (pool.clone(), pid_sender.clone());
        tokio::spawn(async move {
            let mut transaction = begin(&pool).await?;
            pid_sender.send(backend_pid(&mut transaction).await?)?;
            let claim = claim_sequencer(&mut transaction, document).await?;
            transaction.commit().await?;
            TestResult::Ok(claim)
        })
    };
    wait_until_blocked(&pool, &receive_pids(&mut pid_receiver, 1).await?).await?;
    in_flight.commit().await?;
    let second = timeout(BOUND, takeover).await???;
    assert_eq!(second, SequencerClaim { fencing_token: old.fencing_token + 1, current_seq: first });
    let late = spawn_append(&pool, &pid_sender, document, old.fencing_token, first + 1);
    assert_eq!(timeout(BOUND, late).await???, Err(EventRepositoryError::StaleFence));
    receive_pids(&mut pid_receiver, 1).await?;

    // The takeover holds the row lock first: an append with the previous token that started
    // before the takeover committed waits, then re-reads the committed token and is fenced.
    let mut takeover = begin(&pool).await?;
    let third = claim_sequencer(&mut takeover, document).await?;
    assert_eq!(third, SequencerClaim { fencing_token: second.fencing_token + 1, current_seq: first });
    let fenced = spawn_append(&pool, &pid_sender, document, second.fencing_token, first + 1);
    wait_until_blocked(&pool, &receive_pids(&mut pid_receiver, 1).await?).await?;
    takeover.commit().await?;
    assert_eq!(timeout(BOUND, fenced).await???, Err(EventRepositoryError::StaleFence));

    // A takeover that rolls back fences nobody: the waiting append keeps the committed token.
    let mut abandoned = begin(&pool).await?;
    assert_eq!(claim_sequencer(&mut abandoned, document).await?.fencing_token, third.fencing_token + 1);
    let kept = spawn_append(&pool, &pid_sender, document, third.fencing_token, first + 1);
    wait_until_blocked(&pool, &receive_pids(&mut pid_receiver, 1).await?).await?;
    abandoned.rollback().await?;
    assert_eq!(timeout(BOUND, kept).await???, Ok(Appended { first_seq: first + 1, last_seq: first + 1 }));

    // Every row carries the token that was current when it committed.
    let mut reader = begin(&pool).await?;
    let tokens: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT seq, fencing_token FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq > $3 ORDER BY seq",
    )
    .bind(TENANT)
    .bind(FENCING_DOCUMENT)
    .bind(old.current_seq)
    .fetch_all(reader.connection())
    .await?;
    reader.rollback().await?;
    assert_eq!(tokens, vec![(first, old.fencing_token), (first + 1, third.fencing_token)]);
    let shape = log_shape(&pool, document).await?;
    assert!(shape.is_exact(), "log is not exact: {shape:?}");
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_free_running_owner_is_fenced_at_the_takeover_commit() -> TestResult {
    // The owner appends until fenced; the bound only stops a runaway if the takeover never lands.
    const MAX_OWNER_APPENDS: usize = 2_000;

    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = TAKEOVER_DOCUMENT.parse()?;
    let start = claim_committed(&pool, document).await?;
    let (first_commit_sender, first_commit) = oneshot::channel::<()>();

    let owner: Racer<Vec<i64>> = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let actor: PrincipalId = ACTOR.parse()?;
            let mut committed = Vec::new();
            let mut first_commit_sender = Some(first_commit_sender);
            let mut next = start.current_seq + 1;
            for _ in 0..MAX_OWNER_APPENDS {
                let mut transaction = begin(&pool).await?;
                match append_events(&mut transaction, document, start.fencing_token, actor, &[batch(next)?]).await {
                    Ok(appended) => {
                        transaction.commit().await?;
                        committed.push(appended.first_seq);
                        next = appended.last_seq + 1;
                        if let Some(sender) = first_commit_sender.take() {
                            let _ = sender.send(());
                        }
                    }
                    Err(EventRepositoryError::StaleFence) => {
                        transaction.rollback().await?;
                        return Ok(committed);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err("the old owner was never fenced".into())
        })
    };

    // The takeover races the owner's later appends once the owner is demonstrably writing. After
    // its claim commits, nothing else may write, so its own append must continue the sequence it
    // claimed at.
    let takeover: Racer<(SequencerClaim, Appended)> = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let actor: PrincipalId = ACTOR.parse()?;
            timeout(BOUND, first_commit).await??;
            let claim = claim_committed(&pool, document).await?;
            let mut transaction = begin(&pool).await?;
            let next = claim.current_seq + 1;
            let appended = append_events(&mut transaction, document, claim.fencing_token, actor, &[batch(next)?, batch(next + 1)?]).await?;
            transaction.commit().await?;
            Ok((claim, appended))
        })
    };

    let committed = timeout(BOUND, owner).await???;
    let (claim, appended) = timeout(BOUND, takeover).await???;
    assert_eq!(claim.fencing_token, start.fencing_token + 1);
    let owner_expected: Vec<i64> = (start.current_seq + 1..=claim.current_seq).collect();
    assert!(!committed.is_empty(), "the owner must have written before the takeover");
    assert_eq!(committed, owner_expected, "the owner wrote exactly up to the sequence the takeover claimed at");
    assert_eq!(appended, Appended { first_seq: claim.current_seq + 1, last_seq: claim.current_seq + 2 });

    let mut reader = begin(&pool).await?;
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT seq, fencing_token FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq > $3 ORDER BY seq",
    )
    .bind(TENANT)
    .bind(TAKEOVER_DOCUMENT)
    .bind(start.current_seq)
    .fetch_all(reader.connection())
    .await?;
    reader.rollback().await?;
    let expected: Vec<(i64, i64)> = (start.current_seq + 1..=appended.last_seq)
        .map(|seq| (seq, if seq <= claim.current_seq { start.fencing_token } else { claim.fencing_token }))
        .collect();
    assert_eq!(rows, expected, "every row carries the token current at its commit");
    let shape = log_shape(&pool, document).await?;
    assert!(shape.is_exact(), "log is not exact: {shape:?}");
    assert_eq!(shape.current_seq, appended.last_seq);
    pool.close().await;
    Ok(())
}

/// Start `request_hashes.len()` racers that pass a barrier together, each reserving `key_hash`
/// with its own request hash in its own committed transaction.
fn spawn_reservations(pool: &PgPool, key_hash: [u8; 32], request_hashes: Vec<[u8; 32]>) -> Vec<Racer<Result<ReservationOutcome, RepositoryError>>> {
    let barrier = std::sync::Arc::new(Barrier::new(request_hashes.len()));
    request_hashes
        .into_iter()
        .map(|request_hash| {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            tokio::spawn(async move {
                let principal: PrincipalId = ACTOR.parse()?;
                let mut transaction = begin(&pool).await?;
                barrier.wait().await;
                let outcome = reserve_idempotency_key(&mut transaction, principal, ROUTE, &key_hash, &request_hash, 3600).await;
                finish(transaction, outcome.is_ok()).await?;
                Ok(outcome)
            })
        })
        .collect()
}

fn count<T: PartialEq>(results: &[T], wanted: &T) -> usize {
    results.iter().filter(|result| *result == wanted).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_reservations_of_one_key_reserve_exactly_once() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };

    // Same request: the losers wait on the winner's uncommitted insert, then see it in flight.
    let request = run_hash("request");
    let results = join_all(spawn_reservations(&pool, run_hash("same-request-key"), vec![request; RACERS])).await?;
    assert_eq!(count(&results, &Ok(ReservationOutcome::Reserved)), 1, "outcomes: {results:?}");
    assert_eq!(count(&results, &Ok(ReservationOutcome::InFlight)), RACERS - 1, "outcomes: {results:?}");

    // Different requests reusing the key: exactly one reserves, every other is refused as a
    // conflict rather than leaking the unique violation as a database error.
    let requests: Vec<[u8; 32]> = (0..RACERS).map(|racer| run_hash(&format!("request-{racer}"))).collect();
    let results = join_all(spawn_reservations(&pool, run_hash("mixed-request-key"), requests)).await?;
    assert_eq!(count(&results, &Ok(ReservationOutcome::Reserved)), 1, "outcomes: {results:?}");
    assert_eq!(count(&results, &Err(RepositoryError::Conflict)), RACERS - 1, "outcomes: {results:?}");
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rolled_back_reservation_passes_the_key_to_exactly_one_waiter() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let principal: PrincipalId = ACTOR.parse()?;
    let (key, request) = (run_hash("rollback-key"), run_hash("rollback-request"));

    let mut holder = begin(&pool).await?;
    assert_eq!(reserve_idempotency_key(&mut holder, principal, ROUTE, &key, &request, 3600).await?, ReservationOutcome::Reserved);
    let (pid_sender, mut pid_receiver) = mpsc::unbounded_channel();
    let waiters: Vec<_> = (1..RACERS)
        .map(|_| {
            let (pool, pid_sender) = (pool.clone(), pid_sender.clone());
            tokio::spawn(async move {
                let mut transaction = begin(&pool).await?;
                pid_sender.send(backend_pid(&mut transaction).await?)?;
                let outcome = reserve_idempotency_key(&mut transaction, principal, ROUTE, &key, &request, 3600).await;
                finish(transaction, outcome.is_ok()).await?;
                TestResult::Ok(outcome)
            })
        })
        .collect();
    wait_until_blocked(&pool, &receive_pids(&mut pid_receiver, RACERS - 1).await?).await?;
    holder.rollback().await?;

    let results = join_all(waiters).await?;
    assert_eq!(count(&results, &Ok(ReservationOutcome::Reserved)), 1, "outcomes: {results:?}");
    assert_eq!(count(&results, &Ok(ReservationOutcome::InFlight)), RACERS - 2, "outcomes: {results:?}");
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_completions_store_one_response_that_every_retry_replays() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let principal: PrincipalId = ACTOR.parse()?;
    let (key, request) = (run_hash("complete-key"), run_hash("complete-request"));
    let mut reservation = begin(&pool).await?;
    assert_eq!(reserve_idempotency_key(&mut reservation, principal, ROUTE, &key, &request, 3600).await?, ReservationOutcome::Reserved);
    reservation.commit().await?;

    // Each completer offers a distinct body; the waiters re-check `state = 'pending'` after the
    // winner commits and find nothing to update.
    let barrier = std::sync::Arc::new(Barrier::new(RACERS));
    let completers: Vec<Racer<Completion>> = (0..RACERS)
        .map(|racer| {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            tokio::spawn(async move {
                let body = format!("{{\"racer\":{racer}}}").into_bytes();
                let mut transaction = begin(&pool).await?;
                barrier.wait().await;
                let outcome = complete_idempotency_key(&mut transaction, principal, ROUTE, &key, 201, &body, "application/json").await;
                finish(transaction, outcome.is_ok()).await?;
                Ok((body, outcome))
            })
        })
        .collect();
    let results = join_all(completers).await?;
    let winners: Vec<&Vec<u8>> = results.iter().filter(|(_, outcome)| outcome.is_ok()).map(|(body, _)| body).collect();
    let refused = results.iter().filter(|(_, outcome)| *outcome == Err(RepositoryError::InvalidState)).count();
    assert_eq!((winners.len(), refused), (1, RACERS - 1), "outcomes: {results:?}");
    let stored = winners.first().map(|body| (*body).clone()).ok_or("no completion won")?;

    let replay = Ok(ReservationOutcome::Replay { response_status: 201, response_body: stored, response_content_type: "application/json".to_owned() });
    let results = join_all(spawn_reservations(&pool, key, vec![request; RACERS])).await?;
    assert_eq!(count(&results, &replay), RACERS, "outcomes: {results:?}");
    pool.close().await;
    Ok(())
}
