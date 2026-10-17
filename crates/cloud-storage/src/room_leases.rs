//! PostgreSQL room registry and renewable ownership leases (S5-003/004).
//!
//! All operations use a tenant transaction. Acquisition locks the document row before checking
//! the lease, then advances the document's fencing token in that same transaction. A caller must
//! commit the transaction before serving the room. Expiry uses PostgreSQL's clock, never a relay's
//! local clock. The fence is still checked by the event writer after any lease loss.

use figcraft_cloud_domain::{CloudDocumentId, TenantId, Uuid};
use url::Url;

use crate::TenantTransaction;
use crate::events::{EventRepositoryError, SequencerClaim, claim_sequencer};

/// Lease lifetime accepted by the repository. The relay renews well before this expires.
pub const MIN_LEASE_SECONDS: i32 = 5;
pub const MAX_LEASE_SECONDS: i32 = 120;
pub const MAX_ENDPOINT_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomLease {
    pub tenant_id: TenantId,
    pub document_id: CloudDocumentId,
    pub owner_id: Uuid,
    /// A public or gateway-reachable WebSocket URL with no credentials or document token.
    pub endpoint: String,
    pub fencing_token: i64,
    /// Informational Unix timestamp. Lease decisions always use the database clock.
    pub expires_at_unix: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcquireRoom {
    Acquired(RoomLease),
    Held(RoomLease),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoomLeaseError {
    InvalidInput,
    NotFound,
    InvalidState,
    FenceExhausted,
    Database,
}

impl std::fmt::Display for RoomLeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "room lease error: {self:?}")
    }
}

impl std::error::Error for RoomLeaseError {}

fn valid_endpoint(endpoint: &str) -> bool {
    if endpoint.len() > MAX_ENDPOINT_BYTES || endpoint.is_empty() {
        return false;
    }
    let Ok(url) = Url::parse(endpoint) else {
        return false;
    };
    matches!(url.scheme(), "ws" | "wss")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

type LeaseRow = (String, String, i64, i64);

fn lease_from_row(tenant_id: TenantId, document_id: CloudDocumentId, row: LeaseRow) -> Result<RoomLease, RoomLeaseError> {
    let (owner, endpoint, fencing_token, expires_at_unix) = row;
    let owner_id = owner.parse().map_err(|_| RoomLeaseError::Database)?;
    if !valid_endpoint(&endpoint) || fencing_token <= 0 {
        return Err(RoomLeaseError::Database);
    }
    Ok(RoomLease { tenant_id, document_id, owner_id, endpoint, fencing_token, expires_at_unix })
}

/// Return the current unexpired owner, scoped to this transaction's tenant.
pub async fn current_room(transaction: &mut TenantTransaction<'_>, document_id: CloudDocumentId) -> Result<Option<RoomLease>, RoomLeaseError> {
    let tenant_id = transaction.tenant_id();
    let row: Option<LeaseRow> = sqlx::query_as(
        "SELECT lease.owner_id::text, lease.endpoint, lease.fencing_token, extract(epoch from lease.expires_at)::bigint \
         FROM room_leases lease JOIN documents document ON document.tenant_id = lease.tenant_id AND document.id = lease.document_id \
         WHERE lease.tenant_id = $1::uuid AND lease.document_id = $2::uuid \
         AND lease.expires_at > clock_timestamp() AND lease.fencing_token = document.fencing_token AND document.state = 'active'",
    )
    .bind(tenant_id.to_string())
    .bind(document_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| RoomLeaseError::Database)?;
    row.map(|row| lease_from_row(tenant_id, document_id, row)).transpose()
}

/// Acquire an unowned or expired room and its PostgreSQL fence atomically. An existing live
/// owner is returned as `Held`, including when this caller supplied the same owner ID: renewal
/// is a separate compare-and-set operation. The document row serializes competing acquisitions.
pub async fn acquire_room(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    owner_id: Uuid,
    endpoint: &str,
    lifetime_seconds: i32,
) -> Result<AcquireRoom, RoomLeaseError> {
    acquire_room_claim(transaction, document_id, owner_id, endpoint, lifetime_seconds).await.map(|(acquired, _)| acquired)
}

/// [`acquire_room`], also returning the sequencer claim made with the lease (`None` when the room
/// is `Held`): the caller restores the room under this claim instead of claiming again, which would
/// advance the fence past the lease's.
pub async fn acquire_room_claim(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    owner_id: Uuid,
    endpoint: &str,
    lifetime_seconds: i32,
) -> Result<(AcquireRoom, Option<SequencerClaim>), RoomLeaseError> {
    if owner_id.is_nil() || !valid_endpoint(endpoint) || !(MIN_LEASE_SECONDS..=MAX_LEASE_SECONDS).contains(&lifetime_seconds) {
        return Err(RoomLeaseError::InvalidInput);
    }
    let tenant_id = transaction.tenant_id();
    let state: Option<(String, i64)> =
        sqlx::query_as("SELECT state, fencing_token FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
            .bind(tenant_id.to_string())
            .bind(document_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|_| RoomLeaseError::Database)?;
    let current_token = match state {
        None => return Err(RoomLeaseError::NotFound),
        Some((state, _)) if state != "active" => return Err(RoomLeaseError::InvalidState),
        Some((_, token)) => token,
    };
    if let Some(lease) = current_room(transaction, document_id).await? {
        return Ok((AcquireRoom::Held(lease), None));
    }
    if current_token == i64::MAX {
        return Err(RoomLeaseError::FenceExhausted);
    }
    let claim = claim_sequencer(transaction, document_id).await.map_err(|error| match error {
        EventRepositoryError::NotFound => RoomLeaseError::NotFound,
        EventRepositoryError::InvalidState => RoomLeaseError::InvalidState,
        EventRepositoryError::Constraint => RoomLeaseError::FenceExhausted,
        _ => RoomLeaseError::Database,
    })?;
    let row: LeaseRow = sqlx::query_as(
        "INSERT INTO room_leases (tenant_id, document_id, owner_id, endpoint, fencing_token, expires_at) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, clock_timestamp() + make_interval(secs => $6::double precision)) \
         ON CONFLICT (tenant_id, document_id) DO UPDATE SET owner_id = EXCLUDED.owner_id, \
         endpoint = EXCLUDED.endpoint, fencing_token = EXCLUDED.fencing_token, \
         expires_at = EXCLUDED.expires_at, updated_at = clock_timestamp() \
         RETURNING owner_id::text, endpoint, fencing_token, extract(epoch from expires_at)::bigint",
    )
    .bind(tenant_id.to_string())
    .bind(document_id.to_string())
    .bind(owner_id.to_string())
    .bind(endpoint)
    .bind(claim.fencing_token)
    .bind(lifetime_seconds)
    .fetch_one(transaction.connection())
    .await
    .map_err(|_| RoomLeaseError::Database)?;
    Ok((AcquireRoom::Acquired(lease_from_row(tenant_id, document_id, row)?), Some(claim)))
}

/// Extend only a still-live lease held by this exact owner and fence. A lost owner cannot
/// revive its expired lease or overwrite a new owner's endpoint.
pub async fn renew_room(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    owner_id: Uuid,
    fencing_token: i64,
    lifetime_seconds: i32,
) -> Result<bool, RoomLeaseError> {
    if owner_id.is_nil() || fencing_token <= 0 || !(MIN_LEASE_SECONDS..=MAX_LEASE_SECONDS).contains(&lifetime_seconds) {
        return Err(RoomLeaseError::InvalidInput);
    }
    let updated = sqlx::query(
        "UPDATE room_leases SET expires_at = clock_timestamp() + make_interval(secs => $5::double precision), updated_at = clock_timestamp() \
         WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND owner_id = $3::uuid \
         AND fencing_token = $4 AND expires_at > clock_timestamp() \
         AND EXISTS (SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid \
                     AND fencing_token = $4 AND state = 'active')",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(owner_id.to_string())
    .bind(fencing_token)
    .bind(lifetime_seconds)
    .execute(transaction.connection())
    .await
    .map_err(|_| RoomLeaseError::Database)?;
    Ok(updated.rows_affected() == 1)
}

/// Release only this owner's exact lease. A stale owner cannot delete a successor's row.
pub async fn release_room(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    owner_id: Uuid,
    fencing_token: i64,
) -> Result<bool, RoomLeaseError> {
    if owner_id.is_nil() || fencing_token <= 0 {
        return Err(RoomLeaseError::InvalidInput);
    }
    let deleted = sqlx::query(
        "DELETE FROM room_leases WHERE tenant_id = $1::uuid AND document_id = $2::uuid \
         AND owner_id = $3::uuid AND fencing_token = $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(owner_id.to_string())
    .bind(fencing_token)
    .execute(transaction.connection())
    .await
    .map_err(|_| RoomLeaseError::Database)?;
    Ok(deleted.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_and_lifetime_inputs_are_bounded() {
        for bad in
            ["", "https://relay.example/ws", "ws://user:pass@relay.example/ws", "ws://relay.example/ws?token=secret", "ws://relay.example/#fragment"]
        {
            assert!(!valid_endpoint(bad));
        }
        assert!(valid_endpoint("wss://relay.example/ws/v1/collab"));
        assert!(!valid_endpoint(&format!("ws://relay.example/{}", "x".repeat(MAX_ENDPOINT_BYTES))));
    }
}
