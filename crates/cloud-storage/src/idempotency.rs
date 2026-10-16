//! Idempotency-key reservation, replay and mismatched-request rejection.
//!
//! The client-supplied `Idempotency-Key` header value is never stored raw: callers pass a
//! pre-computed 32-byte hash of the key (`key_hash`) and a 32-byte hash of the request body or
//! params (`request_hash`); hashing itself happens outside this crate. `TIMESTAMPTZ` columns are
//! never selected into a typed Rust value because no `chrono`/`time` sqlx feature is enabled in
//! this crate (see `crates/cloud-storage/Cargo.toml`); they are only ever written through SQL-side
//! expressions such as `now()`.

use figcraft_cloud_domain::PrincipalId;
use std::fmt;

use crate::TenantTransaction;

/// Fixed, non-leaking categories for idempotency repository failures.
///
/// Reuses the same error shape as [`crate::metadata::RepositoryError`] rather than inventing a
/// parallel enum, per this crate's established pattern.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryError {
    InvalidPageSize,
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    InvalidState,
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "idempotency repository error: {self:?}")
    }
}

impl std::error::Error for RepositoryError {}

fn database_error(error: sqlx::Error) -> RepositoryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => RepositoryError::Conflict,
        Some("23503" | "23514") => RepositoryError::Constraint,
        _ => RepositoryError::Database,
    }
}

/// Outcome of attempting to reserve an idempotency key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReservationOutcome {
    /// No prior attempt exists for this key. The caller should perform the real work and then
    /// call [`complete_idempotency_key`].
    Reserved,
    /// A prior attempt with the same request already completed. The caller should return this
    /// stored response directly, without re-executing anything.
    Replay { response_status: i16, response_body: Vec<u8>, response_content_type: String },
    /// A prior attempt with the same request is still in flight (a concurrent duplicate, or a
    /// retry that caught the first attempt before it completed). The caller should signal this
    /// distinctly (for example, a 409) rather than proceed.
    InFlight,
    /// A prior attempt with the same request reserved the key more than
    /// [`RESERVATION_LEASE_SECONDS`] ago and never completed: its worker is presumed dead. The
    /// reservation now belongs to this caller, which should perform the real work again and then
    /// call [`complete_idempotency_key`]. If the earlier worker was only slow and completes first,
    /// this caller's completion is refused with [`RepositoryError::InvalidState`] and the next
    /// retry replays the earlier response.
    TakenOver,
}

/// How long a `pending` reservation is honoured before a retry with the same request may take it
/// over. It must exceed the longest time a request may legitimately take between reserving and
/// completing its key, or a slow first attempt and its takeover both execute.
pub const RESERVATION_LEASE_SECONDS: i64 = 5 * 60;

/// Validate a requested TTL before issuing any SQL. Mirrors `PageSize::new`'s up-front validation
/// style in `metadata.rs`.
fn validate_ttl_seconds(ttl_seconds: i64) -> Result<(), RepositoryError> {
    if ttl_seconds <= 0 {
        return Err(RepositoryError::Constraint);
    }
    Ok(())
}

/// Validate a response status before issuing any SQL. The `idempotency_keys` table also enforces
/// `response_status BETWEEN 100 AND 599` as a backstop, but this guard fails fast and keeps the
/// error category consistent with other up-front validations in this crate.
fn validate_response_status(response_status: i16) -> Result<(), RepositoryError> {
    if !(100..=599).contains(&response_status) {
        return Err(RepositoryError::Constraint);
    }
    Ok(())
}

type StoredAttemptRow = (String, Vec<u8>, Option<i16>, Option<Vec<u8>>, Option<String>);

/// Compare a newly observed attempt against the request hash and lifecycle state of a row that
/// already exists for this key, and decide the outcome. Pure post-processing, factored out of the
/// SQL call sites per this crate's lifetime-safety convention for `sqlx::query_as`.
fn resolve_existing_attempt(row: StoredAttemptRow, request_hash: &[u8]) -> Result<ReservationOutcome, RepositoryError> {
    let (state, stored_request_hash, response_status, response_body, response_content_type) = row;
    if stored_request_hash != request_hash {
        return Err(RepositoryError::Conflict);
    }
    match state.as_str() {
        "completed" => {
            let response_status = response_status.ok_or(RepositoryError::CorruptRow)?;
            let response_body = response_body.ok_or(RepositoryError::CorruptRow)?;
            let response_content_type = response_content_type.ok_or(RepositoryError::CorruptRow)?;
            Ok(ReservationOutcome::Replay { response_status, response_body, response_content_type })
        }
        "pending" => Ok(ReservationOutcome::InFlight),
        _ => Err(RepositoryError::CorruptRow),
    }
}

/// Reserve an idempotency key for `(tenant, principal, route, key_hash)`, or report the fate of a
/// prior attempt.
///
/// On first use the row does not exist: a new `pending` row is inserted and
/// [`ReservationOutcome::Reserved`] is returned. A row whose `expires_at` has passed is treated as
/// absent and reset to a fresh reservation (also `Reserved`). On repeated use with the same
/// `request_hash`, the stored response is replayed if the prior attempt completed, the reservation
/// is taken over ([`ReservationOutcome::TakenOver`]) if it is still pending after
/// [`RESERVATION_LEASE_SECONDS`], or [`ReservationOutcome::InFlight`] is returned if it is pending
/// within its lease. On repeated use with a *different* `request_hash`, the same
/// key was reused for a materially different request and `Err(RepositoryError::Conflict)` is
/// returned.
///
/// `ttl_seconds` must be positive; it is applied as `now() + make_interval(secs => ttl_seconds)`
/// for `expires_at`.
pub async fn reserve_idempotency_key(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    route: &str,
    key_hash: &[u8],
    request_hash: &[u8],
    ttl_seconds: i64,
) -> Result<ReservationOutcome, RepositoryError> {
    validate_ttl_seconds(ttl_seconds)?;

    // Attempt the reservation first. A failed `ON CONFLICT DO NOTHING` is not a Postgres error and
    // does not abort the surrounding transaction, unlike a hard constraint violation, so this
    // two-step flow is safe inside one transaction.
    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO idempotency_keys (tenant_id, principal_id, route, key_hash, request_hash, expires_at) \
         VALUES ($1::uuid, $2::uuid, $3, $4, $5, now() + make_interval(secs => $6)) \
         ON CONFLICT (tenant_id, principal_id, route, key_hash) DO NOTHING \
         RETURNING tenant_id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(route)
    .bind(key_hash)
    .bind(request_hash)
    .bind(ttl_seconds)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;

    if inserted.is_some() {
        return Ok(ReservationOutcome::Reserved);
    }

    // An expired key is treated as absent, whatever its state or request: the row is reset to a
    // fresh reservation for this request. The `UPDATE` takes the row lock, so of several racing
    // retries exactly one resets it; the others re-check the predicate against the reset row
    // (READ COMMITTED) and fall through to the lease and replay checks below.
    let reset: Option<(String,)> = sqlx::query_as(
        "UPDATE idempotency_keys \
         SET request_hash = $5, state = 'pending', response_status = NULL, response_body = NULL, response_content_type = NULL, \
             created_at = now(), updated_at = now(), expires_at = now() + make_interval(secs => $6) \
         WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND route = $3 AND key_hash = $4 AND expires_at <= now() \
         RETURNING tenant_id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(route)
    .bind(key_hash)
    .bind(request_hash)
    .bind(ttl_seconds)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;

    if reset.is_some() {
        return Ok(ReservationOutcome::Reserved);
    }

    // A pending reservation for the same request whose lease ran out is taken over. `updated_at`
    // is the reservation time of a pending row (it is set on insert, reset, takeover and
    // completion only), so a takeover renews the lease and a racing second takeover no longer
    // matches. A different request never takes over: it is a key reuse, refused below.
    let taken_over: Option<(String,)> = sqlx::query_as(
        "UPDATE idempotency_keys SET updated_at = now() \
         WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND route = $3 AND key_hash = $4 \
           AND state = 'pending' AND request_hash = $5 AND expires_at > now() \
           AND updated_at <= now() - make_interval(secs => $6) \
         RETURNING tenant_id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(route)
    .bind(key_hash)
    .bind(request_hash)
    .bind(RESERVATION_LEASE_SECONDS)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;

    if taken_over.is_some() {
        return Ok(ReservationOutcome::TakenOver);
    }

    let existing: Option<StoredAttemptRow> = sqlx::query_as(
        "SELECT state, request_hash, response_status, response_body, response_content_type \
         FROM idempotency_keys \
         WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND route = $3 AND key_hash = $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(route)
    .bind(key_hash)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;

    // The row that caused the conflict must exist (we are inside the same transaction and no
    // other repository call removes idempotency_keys rows); its disappearance would mean the
    // stored data is not trustworthy.
    let row = existing.ok_or(RepositoryError::CorruptRow)?;
    resolve_existing_attempt(row, request_hash)
}

/// Mark a reserved idempotency key completed, storing the response to replay for future repeats.
///
/// Returns `Err(RepositoryError::InvalidState)` if the key does not exist or was already
/// completed: a precondition failure, not a database error.
pub async fn complete_idempotency_key(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    route: &str,
    key_hash: &[u8],
    response_status: i16,
    response_body: &[u8],
    response_content_type: &str,
) -> Result<(), RepositoryError> {
    validate_response_status(response_status)?;

    let row: Option<(String,)> = sqlx::query_as(
        "UPDATE idempotency_keys \
         SET state = 'completed', response_status = $1, response_body = $2, response_content_type = $3, updated_at = now() \
         WHERE tenant_id = $4::uuid AND principal_id = $5::uuid AND route = $6 AND key_hash = $7 AND state = 'pending' \
         RETURNING tenant_id::text",
    )
    .bind(response_status)
    .bind(response_body)
    .bind(response_content_type)
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(route)
    .bind(key_hash)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;

    match row {
        Some(_) => Ok(()),
        None => Err(RepositoryError::InvalidState),
    }
}

#[cfg(test)]
mod tests {
    use super::{RepositoryError, ReservationOutcome, resolve_existing_attempt, validate_response_status, validate_ttl_seconds};

    #[test]
    fn ttl_must_be_strictly_positive() {
        assert_eq!(validate_ttl_seconds(0), Err(RepositoryError::Constraint));
        assert_eq!(validate_ttl_seconds(-1), Err(RepositoryError::Constraint));
        assert_eq!(validate_ttl_seconds(i64::MIN), Err(RepositoryError::Constraint));
        assert_eq!(validate_ttl_seconds(1), Ok(()));
        assert_eq!(validate_ttl_seconds(3600), Ok(()));
    }

    #[test]
    fn response_status_must_be_a_valid_http_status_range() {
        assert_eq!(validate_response_status(99), Err(RepositoryError::Constraint));
        assert_eq!(validate_response_status(600), Err(RepositoryError::Constraint));
        assert_eq!(validate_response_status(0), Err(RepositoryError::Constraint));
        assert_eq!(validate_response_status(100), Ok(()));
        assert_eq!(validate_response_status(599), Ok(()));
        assert_eq!(validate_response_status(200), Ok(()));
    }

    #[test]
    fn differing_request_hash_is_rejected_as_conflict() {
        let row = ("pending".to_owned(), vec![1, 2, 3], None, None, None);
        assert_eq!(resolve_existing_attempt(row, &[9, 9, 9]), Err(RepositoryError::Conflict));
    }

    #[test]
    fn matching_pending_attempt_is_in_flight() {
        let row = ("pending".to_owned(), vec![1, 2, 3], None, None, None);
        assert_eq!(resolve_existing_attempt(row, &[1, 2, 3]), Ok(ReservationOutcome::InFlight));
    }

    #[test]
    fn matching_completed_attempt_replays_the_stored_response() {
        let row = ("completed".to_owned(), vec![1, 2, 3], Some(201_i16), Some(vec![0x7b, 0x7d]), Some("application/json".to_owned()));
        assert_eq!(
            resolve_existing_attempt(row, &[1, 2, 3]),
            Ok(ReservationOutcome::Replay {
                response_status: 201,
                response_body: vec![0x7b, 0x7d],
                response_content_type: "application/json".to_owned(),
            })
        );
    }

    #[test]
    fn completed_attempt_missing_response_fields_is_a_corrupt_row() {
        let row = ("completed".to_owned(), vec![1, 2, 3], None, None, None);
        assert_eq!(resolve_existing_attempt(row, &[1, 2, 3]), Err(RepositoryError::CorruptRow));
    }

    #[test]
    fn unknown_state_is_a_corrupt_row() {
        let row = ("unknown".to_owned(), vec![1, 2, 3], None, None, None);
        assert_eq!(resolve_existing_attempt(row, &[1, 2, 3]), Err(RepositoryError::CorruptRow));
    }
}
