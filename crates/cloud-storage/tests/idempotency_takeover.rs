//! S2 follow-up: stale pending idempotency keys are taken over after the reservation lease, and
//! expired keys are treated as absent, against real PostgreSQL.
//!
//! Uses the S2-011 fixture (`deploy/saas/idempotency-repository-preflight.sql`). Leases and
//! expiries are aged by moving the row's timestamps back rather than by sleeping. Every key hash is
//! unique to the run, so reruns never meet an earlier run's rows.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::idempotency::{
    RESERVATION_LEASE_SECONDS, RepositoryError, ReservationOutcome, complete_idempotency_key, reserve_idempotency_key,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;
use tokio::sync::Barrier;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const TENANT: &str = "91000000-0000-4000-8000-000000000001";
const PRINCIPAL: &str = "91000000-0000-4000-8000-000000000002";
const ROUTE: &str = "test.route.idempotency-takeover";
const RACERS: usize = 6;
const BOUND: Duration = Duration::from_secs(30);

fn run_hash(label: &str) -> [u8; 32] {
    Sha256::digest(format!("{label}:{}", CloudDocumentId::random())).into()
}

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL idempotency takeover probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(8).acquire_timeout(BOUND).connect(database_url.trim_end()).await?))
}

async fn begin(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    let tenant: TenantId = TENANT.parse()?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    sqlx::query("SET LOCAL lock_timeout = '20s'").execute(transaction.connection()).await?;
    Ok(transaction)
}

async fn reserve(pool: &PgPool, key: &[u8; 32], request: &[u8; 32], ttl_seconds: i64) -> TestResult<Result<ReservationOutcome, RepositoryError>> {
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let mut transaction = begin(pool).await?;
    let outcome = reserve_idempotency_key(&mut transaction, principal, ROUTE, key, request, ttl_seconds).await;
    if outcome.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    Ok(outcome)
}

async fn complete(pool: &PgPool, key: &[u8; 32], body: &[u8]) -> TestResult<Result<(), RepositoryError>> {
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let mut transaction = begin(pool).await?;
    let outcome = complete_idempotency_key(&mut transaction, principal, ROUTE, key, 201, body, "application/json").await;
    if outcome.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    Ok(outcome)
}

/// Move the reservation time of `key` back by `seconds`, as if it had been reserved that long ago.
async fn age_reservation(pool: &PgPool, key: &[u8; 32], seconds: i64) -> TestResult {
    let mut transaction = begin(pool).await?;
    let moved = sqlx::query(
        "UPDATE idempotency_keys SET created_at = created_at - make_interval(secs => $3), updated_at = updated_at - make_interval(secs => $3) \
         WHERE tenant_id = $1::uuid AND route = $2 AND key_hash = $4",
    )
    .bind(TENANT)
    .bind(ROUTE)
    .bind(seconds)
    .bind(key.as_slice())
    .execute(transaction.connection())
    .await?
    .rows_affected();
    transaction.commit().await?;
    if moved != 1 {
        return Err(format!("aged {moved} rows, expected 1").into());
    }
    Ok(())
}

/// Make `key` expired: its creation and reservation move back a day and it expires a second ago.
async fn expire(pool: &PgPool, key: &[u8; 32]) -> TestResult {
    let mut transaction = begin(pool).await?;
    let moved = sqlx::query(
        "UPDATE idempotency_keys \
         SET created_at = now() - interval '1 day', updated_at = now() - interval '1 day', expires_at = now() - interval '1 second' \
         WHERE tenant_id = $1::uuid AND route = $2 AND key_hash = $3",
    )
    .bind(TENANT)
    .bind(ROUTE)
    .bind(key.as_slice())
    .execute(transaction.connection())
    .await?
    .rows_affected();
    transaction.commit().await?;
    if moved != 1 {
        return Err(format!("expired {moved} rows, expected 1").into());
    }
    Ok(())
}

fn count<T: PartialEq>(results: &[T], wanted: &T) -> usize {
    results.iter().filter(|result| *result == wanted).count()
}

async fn race(pool: &PgPool, key: [u8; 32], requests: Vec<[u8; 32]>) -> TestResult<Vec<Result<ReservationOutcome, RepositoryError>>> {
    let barrier = std::sync::Arc::new(Barrier::new(requests.len()));
    let handles: Vec<_> = requests
        .into_iter()
        .map(|request| {
            let (pool, barrier) = (pool.clone(), barrier.clone());
            tokio::spawn(async move {
                let principal: PrincipalId = PRINCIPAL.parse()?;
                let mut transaction = begin(&pool).await?;
                barrier.wait().await;
                let outcome = reserve_idempotency_key(&mut transaction, principal, ROUTE, &key, &request, 3600).await;
                if outcome.is_ok() {
                    transaction.commit().await?;
                } else {
                    transaction.rollback().await?;
                }
                TestResult::Ok(outcome)
            })
        })
        .collect();
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(timeout(BOUND, handle).await???);
    }
    Ok(results)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_pending_key_is_taken_over_only_by_the_same_request_after_the_lease() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (key, request) = (run_hash("stale-key"), run_hash("stale-request"));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::Reserved));

    // Just inside the lease the first attempt is still honoured as in flight.
    age_reservation(&pool, &key, RESERVATION_LEASE_SECONDS - 30).await?;
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::InFlight));

    // Past the lease, a different request is still a key reuse and never takes over.
    age_reservation(&pool, &key, 60).await?;
    assert_eq!(reserve(&pool, &key, &run_hash("other-request"), 3600).await?, Err(RepositoryError::Conflict));

    // The same request takes the reservation over, which renews the lease: the next retry is in
    // flight again rather than a second takeover.
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::TakenOver));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::InFlight));

    // The taker's completion is stored and replayed; the presumed-dead first worker coming back
    // late cannot overwrite it.
    let body = br#"{"taken":"over"}"#;
    assert_eq!(complete(&pool, &key, body).await?, Ok(()));
    assert_eq!(complete(&pool, &key, b"late").await?, Err(RepositoryError::InvalidState));
    let replay =
        ReservationOutcome::Replay { response_status: 201, response_body: body.to_vec(), response_content_type: "application/json".to_owned() };
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(replay.clone()));

    // A completed key is never taken over, however old.
    age_reservation(&pool, &key, RESERVATION_LEASE_SECONDS * 10).await?;
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(replay));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_key_is_treated_as_absent_in_any_state() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };

    // An expired completed key no longer replays: any request reserves it afresh, and the old
    // response is gone.
    let (key, request) = (run_hash("expired-completed-key"), run_hash("expired-completed-request"));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::Reserved));
    assert_eq!(complete(&pool, &key, b"{}").await?, Ok(()));
    expire(&pool, &key).await?;
    let fresh = run_hash("fresh-request");
    assert_eq!(reserve(&pool, &key, &fresh, 3600).await?, Ok(ReservationOutcome::Reserved));
    assert_eq!(reserve(&pool, &key, &fresh, 3600).await?, Ok(ReservationOutcome::InFlight), "the reset row is a new, unexpired reservation");
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Err(RepositoryError::Conflict), "the reset row belongs to the new request");

    // An expired pending key, for the same request, is reserved afresh rather than taken over.
    let (key, request) = (run_hash("expired-pending-key"), run_hash("expired-pending-request"));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::Reserved));
    expire(&pool, &key).await?;
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::Reserved));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::InFlight));

    // A key whose short TTL has not passed is still honoured.
    let (key, request) = (run_hash("short-ttl-key"), run_hash("short-ttl-request"));
    assert_eq!(reserve(&pool, &key, &request, 60).await?, Ok(ReservationOutcome::Reserved));
    assert_eq!(reserve(&pool, &key, &request, 60).await?, Ok(ReservationOutcome::InFlight));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_retries_take_over_or_reset_a_key_exactly_once() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };

    // Stale pending key, same request: one racer takes it over, the rest see it in flight.
    let (key, request) = (run_hash("race-stale-key"), run_hash("race-stale-request"));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::Reserved));
    age_reservation(&pool, &key, RESERVATION_LEASE_SECONDS + 60).await?;
    let results = race(&pool, key, vec![request; RACERS]).await?;
    assert_eq!(count(&results, &Ok(ReservationOutcome::TakenOver)), 1, "outcomes: {results:?}");
    assert_eq!(count(&results, &Ok(ReservationOutcome::InFlight)), RACERS - 1, "outcomes: {results:?}");

    // Expired key, different requests: one racer reserves it afresh, the rest are key reuses.
    let (key, request) = (run_hash("race-expired-key"), run_hash("race-expired-request"));
    assert_eq!(reserve(&pool, &key, &request, 3600).await?, Ok(ReservationOutcome::Reserved));
    expire(&pool, &key).await?;
    let requests: Vec<[u8; 32]> = (0..RACERS).map(|racer| run_hash(&format!("race-expired-{racer}"))).collect();
    let results = race(&pool, key, requests).await?;
    assert_eq!(count(&results, &Ok(ReservationOutcome::Reserved)), 1, "outcomes: {results:?}");
    assert_eq!(count(&results, &Err(RepositoryError::Conflict)), RACERS - 1, "outcomes: {results:?}");
    pool.close().await;
    Ok(())
}
