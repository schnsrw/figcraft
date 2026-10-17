//! The client ids a room has issued (S5-008): recorded with the edits, read back when a room is
//! restored, so ids are never reissued and a reconnecting client keeps its own.
//!
//! All operations use a tenant transaction (row-level security applies). A document remembers at
//! most [`MAX_REMEMBERED`] ids (what the collaboration server itself remembers); older ones are
//! dropped when a room opens.

use figcraft_cloud_domain::CloudDocumentId;

use crate::TenantTransaction;

/// The most issued client ids kept per document (the collaboration server's own bound).
pub const MAX_REMEMBERED: usize = 65_536;

/// Ids recorded in one call, at most.
pub const MAX_RECORD_BATCH: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoomClientsError {
    /// More ids than one call may record.
    TooMany,
    Database,
}

impl std::fmt::Display for RoomClientsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "room clients error: {self:?}")
    }
}

impl std::error::Error for RoomClientsError {}

/// Record `clients` as issued for `document` (ids already recorded are left as they are).
pub async fn record_issued(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId, clients: &[u32]) -> Result<(), RoomClientsError> {
    if clients.is_empty() {
        return Ok(());
    }
    if clients.len() > MAX_RECORD_BATCH {
        return Err(RoomClientsError::TooMany);
    }
    let ids: Vec<i64> = clients.iter().map(|id| i64::from(*id)).collect();
    sqlx::query(
        "INSERT INTO room_clients (tenant_id, document_id, client_id) \
         SELECT $1::uuid, $2::uuid, id FROM unnest($3::bigint[]) AS id ON CONFLICT DO NOTHING",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(ids)
    .execute(transaction.connection())
    .await
    .map_err(|_| RoomClientsError::Database)?;
    Ok(())
}

/// The ids issued for `document`, oldest first, after dropping all but the newest
/// [`MAX_REMEMBERED`]. Called when a room opens.
pub async fn load_and_trim(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId) -> Result<Vec<u32>, RoomClientsError> {
    let tenant = transaction.tenant_id().to_string();
    let keep = i64::try_from(MAX_REMEMBERED).unwrap_or(i64::MAX);
    sqlx::query(
        "DELETE FROM room_clients WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND client_id < \
         (SELECT client_id FROM room_clients WHERE tenant_id = $1::uuid AND document_id = $2::uuid \
          ORDER BY client_id DESC OFFSET $3 - 1 LIMIT 1)",
    )
    .bind(&tenant)
    .bind(document.to_string())
    .bind(keep)
    .execute(transaction.connection())
    .await
    .map_err(|_| RoomClientsError::Database)?;
    let rows: Vec<i64> =
        sqlx::query_scalar("SELECT client_id FROM room_clients WHERE tenant_id = $1::uuid AND document_id = $2::uuid ORDER BY client_id")
            .bind(&tenant)
            .bind(document.to_string())
            .fetch_all(transaction.connection())
            .await
            .map_err(|_| RoomClientsError::Database)?;
    Ok(rows.into_iter().filter_map(|id| u32::try_from(id).ok()).collect())
}
