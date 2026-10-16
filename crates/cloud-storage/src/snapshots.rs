//! Document snapshot metadata repository.
//!
//! A snapshot row only ever records *where a document's bytes will live* (`object_id`) and *how to
//! verify them* (`checksum`, `size_bytes`) — the bytes themselves live in S3-compatible object
//! storage, which is later (S3 milestone) work. Nothing in this module sees or moves file bytes.
//! `TIMESTAMPTZ` columns (`created_at`, `verified_at`) are never selected into a typed Rust value,
//! because no `chrono`/`time` sqlx feature is enabled in this crate (see
//! `crates/cloud-storage/Cargo.toml`); they are only ever written through SQL-side `now()`.

use std::fmt;

use figcraft_cloud_domain::{CloudDocumentId, TenantId, Uuid};

use crate::TenantTransaction;

/// Fixed, non-leaking categories for snapshot repository failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryError {
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    /// The row exists but its current lifecycle state does not permit the requested transition
    /// (for example, verifying a snapshot that has already been rejected).
    InvalidState,
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "snapshot repository error: {self:?}")
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

/// A snapshot metadata row. `state` is kept as a plain, snapshot-local vocabulary
/// (`"pending"` / `"verified"` / `"rejected"`), distinct from the shared `ResourceState` enum used
/// for tenants, workspaces, projects and documents. `checksum`, `created_at` and `verified_at` are
/// intentionally omitted: callers already hold the checksum they computed, and timestamps are not
/// decoded in this crate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRecord {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub document_id: CloudDocumentId,
    pub seq: i64,
    pub object_id: Uuid,
    pub size_bytes: i64,
    pub state: String,
}

type SnapshotRow = (String, String, String, i64, String, i64, String);

fn snapshot(row: SnapshotRow) -> Result<SnapshotRecord, RepositoryError> {
    Ok(SnapshotRecord {
        id: Uuid::parse_str(&row.0).map_err(|_| RepositoryError::CorruptRow)?,
        tenant_id: row.1.parse().map_err(|_| RepositoryError::CorruptRow)?,
        document_id: row.2.parse().map_err(|_| RepositoryError::CorruptRow)?,
        seq: row.3,
        object_id: Uuid::parse_str(&row.4).map_err(|_| RepositoryError::CorruptRow)?,
        size_bytes: row.5,
        state: row.6,
    })
}

/// Resolve why a conditional snapshot update matched zero rows: the snapshot does not exist
/// (`Ok(None)`), it already satisfies `settled` (the transition is idempotent), or it is in some
/// other state that does not permit the transition (`Err(InvalidState)`). Pure post-processing,
/// factored out of the SQL call sites per this crate's lifetime-safety convention for
/// `sqlx::query_as` (a shared helper must not take the SQL string itself as a parameter).
fn settle_or_reject_transition(existing: Option<SnapshotRecord>, settled: &str) -> Result<Option<SnapshotRecord>, RepositoryError> {
    match existing {
        Some(record) if record.state == settled => Ok(Some(record)),
        Some(_) => Err(RepositoryError::InvalidState),
        None => Ok(None),
    }
}

/// Look up a snapshot by its tenant-scoped natural key. Used only to classify a zero-row
/// transition update (see [`settle_or_reject_transition`]).
async fn get_snapshot(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    document_id: CloudDocumentId,
) -> Result<Option<SnapshotRecord>, RepositoryError> {
    let row: Option<SnapshotRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, document_id::text, seq, object_id::text, size_bytes, state \
         FROM document_snapshots WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(snapshot).transpose()
}

/// Register a snapshot attempt's metadata row. The row starts `'pending'`; it records only a
/// reference (`object_id`, `checksum`, `size_bytes`) to where the snapshot's bytes will live, not
/// the bytes themselves. A foreign-key or check violation (an unknown `document_id`, a negative
/// `seq`, a checksum of the wrong length, or a non-positive `size_bytes`) surfaces as
/// `RepositoryError::Constraint` from the table's own constraints; no additional Rust-side
/// validation is performed.
pub async fn register_snapshot(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    document_id: CloudDocumentId,
    seq: i64,
    object_id: Uuid,
    checksum: &[u8],
    size_bytes: i64,
) -> Result<SnapshotRecord, RepositoryError> {
    let row: SnapshotRow = sqlx::query_as(
        "INSERT INTO document_snapshots (id, tenant_id, document_id, seq, object_id, checksum, size_bytes) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5::uuid, $6, $7) \
         RETURNING id::text, tenant_id::text, document_id::text, seq, object_id::text, size_bytes, state",
    )
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(seq)
    .bind(object_id.to_string())
    .bind(checksum)
    .bind(size_bytes)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    snapshot(row)
}

/// Transition a snapshot from `'pending'` to `'verified'`, setting `verified_at` in the same
/// statement (the table's `CHECK ((state = 'verified') = (verified_at IS NOT NULL))` constraint
/// requires both to change together). Idempotent: verifying an already-verified snapshot is a safe
/// no-op that returns its current record. Returns `Err(InvalidState)` if the snapshot exists but is
/// `'rejected'`, and `Ok(None)` if no such snapshot exists at all.
pub async fn verify_snapshot(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    document_id: CloudDocumentId,
) -> Result<Option<SnapshotRecord>, RepositoryError> {
    let row: Option<SnapshotRow> = sqlx::query_as(
        "UPDATE document_snapshots SET state = 'verified', verified_at = now() \
         WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid AND state = 'pending' \
         RETURNING id::text, tenant_id::text, document_id::text, seq, object_id::text, size_bytes, state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if let Some(row) = row {
        return Ok(Some(snapshot(row)?));
    }
    settle_or_reject_transition(get_snapshot(transaction, id, document_id).await?, "verified")
}

/// Transition a snapshot from `'pending'` to `'rejected'`. `verified_at` is left `NULL`, matching
/// the table's state/`verified_at` check constraint. Idempotent: rejecting an already-rejected
/// snapshot is a safe no-op that returns its current record. Returns `Err(InvalidState)` if the
/// snapshot exists but is already `'verified'` (a verified snapshot cannot be un-verified by
/// rejecting it), and `Ok(None)` if no such snapshot exists at all.
pub async fn reject_snapshot(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    document_id: CloudDocumentId,
) -> Result<Option<SnapshotRecord>, RepositoryError> {
    let row: Option<SnapshotRow> = sqlx::query_as(
        "UPDATE document_snapshots SET state = 'rejected' \
         WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid AND state = 'pending' \
         RETURNING id::text, tenant_id::text, document_id::text, seq, object_id::text, size_bytes, state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if let Some(row) = row {
        return Ok(Some(snapshot(row)?));
    }
    settle_or_reject_transition(get_snapshot(transaction, id, document_id).await?, "rejected")
}

/// Point a document's `current_snapshot_id` at one of its own snapshot rows.
///
/// `documents.current_snapshot_id` carries a composite foreign key back to
/// `document_snapshots (tenant_id, document_id, id)`, so pointing it at a snapshot that does not
/// exist, or that belongs to a different document, surfaces as `RepositoryError::Constraint`
/// without any extra Rust-side check. A zero-row update (the document itself does not exist) is not
/// distinguished from success: this pointer-update helper intentionally does not carry
/// document-lifecycle-grade precision, which is out of scope for snapshot metadata registration.
pub async fn set_current_snapshot(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    snapshot_id: Uuid,
) -> Result<(), RepositoryError> {
    sqlx::query("UPDATE documents SET current_snapshot_id = $1::uuid, updated_at = now() WHERE tenant_id = $2::uuid AND id = $3::uuid")
        .bind(snapshot_id.to_string())
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RepositoryError, SnapshotRecord, settle_or_reject_transition};
    use figcraft_cloud_domain::{CloudDocumentId, TenantId, Uuid};

    fn snapshot_record(state: &str) -> SnapshotRecord {
        SnapshotRecord {
            id: Uuid::new_v4(),
            tenant_id: TenantId::random(),
            document_id: CloudDocumentId::random(),
            seq: 0,
            object_id: Uuid::new_v4(),
            size_bytes: 1,
            state: state.to_owned(),
        }
    }

    #[test]
    fn transition_settles_idempotently_or_rejects_other_states() {
        assert_eq!(settle_or_reject_transition(None, "verified"), Ok(None));
        let already_verified = snapshot_record("verified");
        assert_eq!(settle_or_reject_transition(Some(already_verified.clone()), "verified"), Ok(Some(already_verified)));
        let rejected = snapshot_record("rejected");
        assert_eq!(settle_or_reject_transition(Some(rejected), "verified"), Err(RepositoryError::InvalidState));
    }
}
