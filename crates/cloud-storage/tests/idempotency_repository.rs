use figcraft_cloud_domain::{PrincipalId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::idempotency::{RepositoryError, ReservationOutcome, complete_idempotency_key, reserve_idempotency_key};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn idempotency_keys_reserve_replay_and_reject_mismatched_requests() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL idempotency repository probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let tenant: TenantId = "91000000-0000-4000-8000-000000000001".parse()?;
    let principal: PrincipalId = "91000000-0000-4000-8000-000000000002".parse()?;
    let route = "test.route.idempotency";

    let key_hash_one = [0x01u8; 32];
    let request_hash_a = [0xAAu8; 32];
    let key_hash_two = [0x02u8; 32];
    let request_hash_b = [0xBBu8; 32];
    let request_hash_c = [0xCCu8; 32];
    let response_body = b"{\"ok\":true}".to_vec();
    let response_content_type = "application/json";

    // None of the behaviors exercised here trip a hard Postgres constraint violation (unlike,
    // say, document_repositories.rs's wrong-parent FK case): a conflicting reservation is
    // resolved with `ON CONFLICT ... DO NOTHING`, which does not error, and a rejected completion
    // is a zero-row `UPDATE ... RETURNING`, also not an error. So everything below safely shares
    // one transaction.
    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;

    // First attempt: no row exists yet, so the key is reserved for the caller to do the real work.
    let first_attempt = reserve_idempotency_key(&mut scoped, principal, route, &key_hash_one, &request_hash_a, 3600).await?;
    assert_eq!(first_attempt, ReservationOutcome::Reserved);

    // Completing it stores the response for future replay.
    complete_idempotency_key(&mut scoped, principal, route, &key_hash_one, 201, &response_body, response_content_type).await?;

    // A repeat with the same key and the same request hash replays the stored response exactly.
    let replay = reserve_idempotency_key(&mut scoped, principal, route, &key_hash_one, &request_hash_a, 3600).await?;
    assert_eq!(
        replay,
        ReservationOutcome::Replay {
            response_status: 201,
            response_body: response_body.clone(),
            response_content_type: response_content_type.to_owned(),
        }
    );

    // A second key reserved but not yet completed: a repeat with the same request hash reports
    // that the first attempt is still in flight.
    let second_attempt = reserve_idempotency_key(&mut scoped, principal, route, &key_hash_two, &request_hash_b, 3600).await?;
    assert_eq!(second_attempt, ReservationOutcome::Reserved);
    let still_in_flight = reserve_idempotency_key(&mut scoped, principal, route, &key_hash_two, &request_hash_b, 3600).await?;
    assert_eq!(still_in_flight, ReservationOutcome::InFlight);

    // Reusing the same key with a materially different request is rejected as a conflict.
    assert_eq!(reserve_idempotency_key(&mut scoped, principal, route, &key_hash_two, &request_hash_c, 3600).await, Err(RepositoryError::Conflict));

    // Completing an already-completed key is a precondition failure, not a database error.
    assert_eq!(
        complete_idempotency_key(&mut scoped, principal, route, &key_hash_one, 200, b"ignored", "text/plain").await,
        Err(RepositoryError::InvalidState)
    );

    // Completing a key that was never reserved is the same precondition failure.
    let never_reserved_key_hash = [0x03u8; 32];
    assert_eq!(
        complete_idempotency_key(&mut scoped, principal, route, &never_reserved_key_hash, 200, b"ignored", "text/plain").await,
        Err(RepositoryError::InvalidState)
    );

    scoped.rollback().await?;
    pool.close().await;
    Ok(())
}
