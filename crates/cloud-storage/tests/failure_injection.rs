//! S2-020: failure injection at the commit, connection-loss and retry boundaries of the event log
//! and idempotency keys, against real PostgreSQL.
//!
//! Faults are injected the way production meets them: a second runtime connection terminates the
//! victim's backend with `pg_terminate_backend` (connection loss, crashed process), or the victim's
//! own `lock_timeout` / `statement_timeout` cancels a statement. Victims are found by backend pid;
//! every wait polls `pg_stat_activity` against a deadline, so no sleep stands in for
//! synchronisation. Fixture documents keep their history between runs, so each test reads its
//! starting sequence and token from the database and asserts relative to them.

use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::events::{
    Appended, EventRepositoryError, MAX_REPLAY_BYTES, ReplayedEvent, SequencerClaim, append_events, claim_sequencer, read_events,
};
use figcraft_cloud_storage::idempotency::{RepositoryError, ReservationOutcome, complete_idempotency_key, reserve_idempotency_key};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::{Accepted, Change, NodeId};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::task::JoinHandle;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const TENANT: &str = "98000000-0000-4000-8000-000000000001";
const ACTOR: &str = "98000000-0000-4000-8000-000000000009";
const KILL_AFTER_APPEND_DOCUMENT: &str = "98300000-0000-4000-8000-000000000001";
const KILL_IN_LOCK_WAIT_DOCUMENT: &str = "98300000-0000-4000-8000-000000000002";
const LOST_ACK_DOCUMENT: &str = "98300000-0000-4000-8000-000000000003";
const TIMEOUT_DOCUMENT: &str = "98300000-0000-4000-8000-000000000004";
const ROUTE: &str = "test.route.failure-injection";

/// Upper bound on any single wait, so a lost wake-up fails the test instead of hanging it.
const BOUND: Duration = Duration::from_secs(30);

/// A batch whose `batch_id` is unique to this run, so a replayed batch can only equal the one this
/// run sent, never an earlier run's or another writer's batch at the same sequence.
fn batch(seq: i64, batch_id: u64) -> TestResult<Accepted> {
    let seq = u64::try_from(seq)?;
    Ok(Accepted { seq, author: 7, batch_id, changes: vec![Change::Delete { id: NodeId(seq) }] })
}

fn run_batch_id() -> u64 {
    let digest = Sha256::digest(CloudDocumentId::random().to_string());
    digest.iter().take(8).fold(0_u64, |id, byte| (id << 8) | u64::from(*byte))
}

/// A key or request hash unique to this run, so reruns never meet an earlier run's rows.
fn run_hash(label: &str) -> [u8; 32] {
    Sha256::digest(format!("{label}:{}", CloudDocumentId::random())).into()
}

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL failure-injection probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(6).acquire_timeout(BOUND).connect(database_url.trim_end()).await?))
}

/// A tenant transaction whose lock waits give up well inside [`BOUND`].
async fn begin(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    let tenant: TenantId = TENANT.parse()?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    sqlx::query("SET LOCAL lock_timeout = '20s'").execute(transaction.connection()).await?;
    Ok(transaction)
}

async fn backend_pid(transaction: &mut TenantTransaction<'_>) -> TestResult<i32> {
    Ok(sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(transaction.connection()).await?)
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

/// Poll the server's view of `pid` until `done` holds for it, or fail at the deadline.
async fn wait_for(pool: &PgPool, pid: i32, what: &str, query: &'static str) -> TestResult {
    let deadline = Instant::now() + BOUND;
    loop {
        let done: bool = sqlx::query_scalar(query).bind(pid).fetch_one(pool).await?;
        if done {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("backend {pid} never {what}").into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_until_blocked(pool: &PgPool, pid: i32) -> TestResult {
    wait_for(pool, pid, "blocked on a lock", "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')").await
}

/// Terminate `pid` from another connection, as a lost network link or crashed process would, and
/// wait until the backend has exited and released everything it held.
async fn kill(pool: &PgPool, pid: i32) -> TestResult {
    let signalled: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)").bind(pid).fetch_one(pool).await?;
    assert!(signalled, "backend {pid} could not be terminated");
    wait_for(pool, pid, "exited", "SELECT NOT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)").await?;
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE pid = $1").bind(pid).fetch_one(pool).await?;
    assert_eq!(held, 0, "a terminated backend still holds locks");
    Ok(())
}

async fn join<T>(handle: JoinHandle<TestResult<T>>) -> TestResult<T> {
    timeout(BOUND, handle).await??
}

/// The document's committed counters and the shape of its event log.
#[derive(Debug, Eq, PartialEq)]
struct LogShape {
    current_seq: i64,
    fencing_token: i64,
    rows: i64,
    distinct_seqs: i64,
    max_seq: Option<i64>,
}

impl LogShape {
    /// A gap-free, duplicate-free `1..=current_seq` log.
    fn is_exact(&self) -> bool {
        let top = if self.current_seq == 0 { None } else { Some(self.current_seq) };
        self.rows == self.current_seq && self.distinct_seqs == self.rows && self.max_seq == top
    }
}

async fn log_shape(pool: &PgPool, document: CloudDocumentId) -> TestResult<LogShape> {
    let mut transaction = begin(pool).await?;
    let (current_seq, fencing_token): (i64, i64) =
        sqlx::query_as("SELECT current_seq, fencing_token FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
            .bind(TENANT)
            .bind(document.to_string())
            .fetch_one(transaction.connection())
            .await?;
    let (rows, distinct_seqs, max_seq): (i64, i64, Option<i64>) =
        sqlx::query_as("SELECT count(*), count(DISTINCT seq), max(seq) FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid")
            .bind(TENANT)
            .bind(document.to_string())
            .fetch_one(transaction.connection())
            .await?;
    transaction.rollback().await?;
    Ok(LogShape { current_seq, fencing_token, rows, distinct_seqs, max_seq })
}

/// The committed event at `seq`, read back through the verified replay path.
async fn replayed(pool: &PgPool, document: CloudDocumentId, seq: i64) -> TestResult<ReplayedEvent> {
    let mut transaction = begin(pool).await?;
    let page = read_events(&mut transaction, document, seq - 1, 1, MAX_REPLAY_BYTES).await?;
    transaction.rollback().await?;
    let mut events = page.events.into_iter();
    let (Some(event), None) = (events.next(), events.next()) else {
        return Err(format!("expected exactly one event at {seq}").into());
    };
    Ok(event)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_killed_after_append_commits_nothing_and_the_retry_lands_once() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = KILL_AFTER_APPEND_DOCUMENT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let claim = claim_committed(&pool, document).await?;
    let before = log_shape(&pool, document).await?;
    assert!(before.is_exact(), "fixture log is not exact: {before:?}");
    let next = claim.current_seq + 1;
    let sent = batch(next, run_batch_id())?;

    // The append itself succeeds and holds the row lock; the link dies before COMMIT reaches the
    // server, so the server rolls the transaction back and the commit reports failure.
    let mut victim = begin(&pool).await?;
    let pid = backend_pid(&mut victim).await?;
    let appended = append_events(&mut victim, document, claim.fencing_token, actor, std::slice::from_ref(&sent)).await?;
    assert_eq!(appended, Appended { first_seq: next, last_seq: next });
    kill(&pool, pid).await?;
    assert!(victim.commit().await.is_err(), "a commit on a terminated connection must not report success");
    assert_eq!(log_shape(&pool, document).await?, before, "nothing of the killed append is durable");

    // The caller retries the same batch on a fresh connection: the sequence is still free, the
    // fencing token still current, and the batch lands exactly once.
    let mut retry = begin(&pool).await?;
    assert_eq!(append_events(&mut retry, document, claim.fencing_token, actor, std::slice::from_ref(&sent)).await?, appended);
    retry.commit().await?;
    let after = log_shape(&pool, document).await?;
    assert!(after.is_exact(), "log is not exact after the retry: {after:?}");
    assert_eq!((after.current_seq, after.rows), (next, before.rows + 1));
    let event = replayed(&pool, document, next).await?;
    assert_eq!(event, ReplayedEvent { seq: next, fencing_token: claim.fencing_token, actor, batch: sent.clone() });

    // A second retry of the now-committed batch is a conflict, never a duplicate.
    let mut again = begin(&pool).await?;
    assert_eq!(
        append_events(&mut again, document, claim.fencing_token, actor, std::slice::from_ref(&sent)).await,
        Err(EventRepositoryError::SequenceConflict { current: next })
    );
    again.rollback().await?;
    assert_eq!(log_shape(&pool, document).await?, after);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_killed_in_the_row_lock_queue_writes_nothing_and_frees_the_queue() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = KILL_IN_LOCK_WAIT_DOCUMENT.parse()?;
    let claim = claim_committed(&pool, document).await?;
    let before = log_shape(&pool, document).await?;
    assert!(before.is_exact(), "fixture log is not exact: {before:?}");
    let next = claim.current_seq + 1;
    let (victim_batch, writer_batch) = (batch(next, run_batch_id())?, batch(next, run_batch_id())?);

    // The gate holds the document row; the victim queues first and the writer behind it.
    let mut gate = begin(&pool).await?;
    lock_document(&mut gate, document).await?;
    let spawn_writer = |batch: Accepted, pids: tokio::sync::oneshot::Sender<i32>| {
        let pool = pool.clone();
        tokio::spawn(async move {
            let actor: PrincipalId = ACTOR.parse()?;
            let mut transaction = begin(&pool).await?;
            pids.send(backend_pid(&mut transaction).await?).map_err(|_| "pid receiver dropped")?;
            let result = append_events(&mut transaction, document, claim.fencing_token, actor, &[batch]).await;
            if result.is_ok() {
                transaction.commit().await?;
            } else {
                // The victim's connection is gone, so its rollback can fail; dropping is enough.
                let _ = transaction.rollback().await;
            }
            TestResult::Ok(result)
        })
    };
    let (victim_pid, victim_pid_receiver) = tokio::sync::oneshot::channel();
    let victim = spawn_writer(victim_batch, victim_pid);
    let victim_pid = timeout(BOUND, victim_pid_receiver).await??;
    wait_until_blocked(&pool, victim_pid).await?;
    let (writer_pid, writer_pid_receiver) = tokio::sync::oneshot::channel();
    let writer = spawn_writer(writer_batch.clone(), writer_pid);
    let writer_pid = timeout(BOUND, writer_pid_receiver).await??;
    wait_until_blocked(&pool, writer_pid).await?;

    // Killing the queued victim surfaces as a typed connection-loss error and leaves its queue
    // slot. Connection loss is ambiguous in general (a commit may have landed), so it is not
    // flagged as blindly retryable.
    kill(&pool, victim_pid).await?;
    let killed = join(victim).await?;
    assert_eq!(killed, Err(EventRepositoryError::Unavailable));
    assert!(killed.is_err_and(|error| error.is_ambiguous() && !error.is_retryable()));
    assert_eq!(log_shape(&pool, document).await?, before, "the killed waiter wrote nothing");
    wait_until_blocked(&pool, writer_pid).await?;

    // Releasing the gate lets the writer behind the dead victim take the lock and commit.
    gate.rollback().await?;
    assert_eq!(join(writer).await?, Ok(Appended { first_seq: next, last_seq: next }));
    let after = log_shape(&pool, document).await?;
    assert!(after.is_exact(), "log is not exact after the writer: {after:?}");
    assert_eq!(after.current_seq, next);
    assert_eq!(replayed(&pool, document, next).await?.batch, writer_batch);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_acknowledgement_is_recognised_on_retry_without_a_duplicate() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = LOST_ACK_DOCUMENT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let claim = claim_committed(&pool, document).await?;
    let before = log_shape(&pool, document).await?;
    assert!(before.is_exact(), "fixture log is not exact: {before:?}");
    let next = claim.current_seq + 1;
    let sent = batch(next, run_batch_id())?;

    // The commit succeeds on the server, but the caller never hears so: it must treat the batch's
    // fate as unknown and retry.
    let mut first = begin(&pool).await?;
    append_events(&mut first, document, claim.fencing_token, actor, std::slice::from_ref(&sent)).await?;
    first.commit().await?;

    // The retry is refused with the committed sequence equal to the batch's own: the batch, or a
    // rival's at that sequence, is already durable. Replay tells the two apart.
    let mut retry = begin(&pool).await?;
    let refused = append_events(&mut retry, document, claim.fencing_token, actor, std::slice::from_ref(&sent)).await;
    retry.rollback().await?;
    assert_eq!(refused, Err(EventRepositoryError::SequenceConflict { current: next }));
    let event = replayed(&pool, document, next).await?;
    assert_eq!(
        event,
        ReplayedEvent { seq: next, fencing_token: claim.fencing_token, actor, batch: sent.clone() },
        "the replayed batch is the one sent"
    );
    let mut different = sent.clone();
    different.batch_id = different.batch_id.wrapping_add(1);
    assert_ne!(event.batch, different, "a rival batch at the same sequence would not match");

    let after = log_shape(&pool, document).await?;
    assert!(after.is_exact(), "log is not exact: {after:?}");
    assert_eq!((after.current_seq, after.rows), (next, before.rows + 1), "the retry wrote no duplicate");
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_and_statement_timeouts_cancel_cleanly_with_a_typed_error() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = TIMEOUT_DOCUMENT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let claim = claim_committed(&pool, document).await?;
    let before = log_shape(&pool, document).await?;
    assert!(before.is_exact(), "fixture log is not exact: {before:?}");
    let next = claim.current_seq + 1;

    let mut gate = begin(&pool).await?;
    lock_document(&mut gate, document).await?;

    // `lock_timeout` cancels the append while it queues on the row lock.
    let mut victim = begin(&pool).await?;
    sqlx::query("SET LOCAL lock_timeout = '50ms'").execute(victim.connection()).await?;
    let started = Instant::now();
    let result = append_events(&mut victim, document, claim.fencing_token, actor, &[batch(next, run_batch_id())?]).await;
    assert_eq!(result, Err(EventRepositoryError::Timeout));
    assert!(result.is_err_and(EventRepositoryError::is_retryable), "an aborted, cancelled transaction is safe to retry");
    assert!(started.elapsed() < BOUND, "the lock timeout did not fire");
    // The transaction is aborted: further statements fail, and a COMMIT the caller sends anyway is
    // turned into a rollback by the server, so nothing partial can become durable.
    assert!(sqlx::query("SELECT 1").execute(victim.connection()).await.is_err(), "an aborted transaction accepted a statement");
    let _ = victim.commit().await;
    assert_eq!(log_shape(&pool, document).await?, before);

    // `statement_timeout` cancels a takeover the same way; the fencing token does not move.
    let mut takeover = begin(&pool).await?;
    sqlx::query("SET LOCAL statement_timeout = '50ms'").execute(takeover.connection()).await?;
    assert_eq!(claim_sequencer(&mut takeover, document).await, Err(EventRepositoryError::Timeout));
    let _ = takeover.commit().await;
    assert_eq!(log_shape(&pool, document).await?, before, "a cancelled takeover moved nothing");

    // Neither cancellation left a lock behind: once the gate goes, the owner's append proceeds.
    gate.rollback().await?;
    let sent = batch(next, run_batch_id())?;
    let mut writer = begin(&pool).await?;
    assert_eq!(
        append_events(&mut writer, document, claim.fencing_token, actor, std::slice::from_ref(&sent)).await,
        Ok(Appended { first_seq: next, last_seq: next })
    );
    writer.commit().await?;
    let after = log_shape(&pool, document).await?;
    assert!(after.is_exact(), "log is not exact: {after:?}");
    assert_eq!((after.current_seq, after.fencing_token), (next, claim.fencing_token));
    assert_eq!(replayed(&pool, document, next).await?.batch, sent);
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_keys_survive_crashes_at_each_retry_boundary() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let principal: PrincipalId = ACTOR.parse()?;
    let (key, request) = (run_hash("failure-key"), run_hash("failure-request"));
    let body = br#"{"created":"failure-injection"}"#.to_vec();

    // A crash before the reserving transaction commits releases the key.
    let mut reserving = begin(&pool).await?;
    let pid = backend_pid(&mut reserving).await?;
    assert_eq!(reserve_idempotency_key(&mut reserving, principal, ROUTE, &key, &request, 3600).await?, ReservationOutcome::Reserved);
    kill(&pool, pid).await?;
    assert!(reserving.commit().await.is_err(), "a commit on a terminated connection must not report success");

    // So does an explicit rollback.
    let mut rolled_back = begin(&pool).await?;
    assert_eq!(reserve_idempotency_key(&mut rolled_back, principal, ROUTE, &key, &request, 3600).await?, ReservationOutcome::Reserved);
    rolled_back.rollback().await?;

    // A committed reservation whose worker crashes before completing stays pending: every retry
    // is told the first attempt is in flight rather than redoing the work.
    let mut reservation = begin(&pool).await?;
    assert_eq!(reserve_idempotency_key(&mut reservation, principal, ROUTE, &key, &request, 3600).await?, ReservationOutcome::Reserved);
    reservation.commit().await?;
    let mut completing = begin(&pool).await?;
    let pid = backend_pid(&mut completing).await?;
    complete_idempotency_key(&mut completing, principal, ROUTE, &key, 201, b"lost", "application/json").await?;
    kill(&pool, pid).await?;
    assert!(completing.commit().await.is_err(), "a commit on a terminated connection must not report success");
    for _ in 0..2 {
        let mut retry = begin(&pool).await?;
        assert_eq!(reserve_idempotency_key(&mut retry, principal, ROUTE, &key, &request, 3600).await?, ReservationOutcome::InFlight);
        retry.rollback().await?;
    }

    // The completion that does commit is the one every later retry replays, byte for byte.
    let mut completion = begin(&pool).await?;
    complete_idempotency_key(&mut completion, principal, ROUTE, &key, 201, &body, "application/json").await?;
    completion.commit().await?;
    let expected = ReservationOutcome::Replay { response_status: 201, response_body: body, response_content_type: "application/json".to_owned() };
    for _ in 0..2 {
        let mut retry = begin(&pool).await?;
        assert_eq!(reserve_idempotency_key(&mut retry, principal, ROUTE, &key, &request, 3600).await?, expected);
        assert_eq!(
            complete_idempotency_key(&mut retry, principal, ROUTE, &key, 200, b"again", "text/plain").await,
            Err(RepositoryError::InvalidState),
            "a completed key cannot be completed again"
        );
        retry.rollback().await?;
    }
    let mut mismatched = begin(&pool).await?;
    assert_eq!(
        reserve_idempotency_key(&mut mismatched, principal, ROUTE, &key, &run_hash("other-request"), 3600).await,
        Err(RepositoryError::Conflict)
    );
    mismatched.rollback().await?;
    pool.close().await;
    Ok(())
}
