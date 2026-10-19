//! Named versions of a document (S6-041): create, list, rename and delete. Rebuilding and restoring
//! a version is the relay's business (`figcraft_cloud_relay::{server_at, restore}`); this module only
//! keeps the names.
//!
//! A version points at an event sequence of the append-only log. The caller proves it is restorable
//! (it rebuilt the state at that sequence) *before* [`create_version`]. At most [`MAX_VERSIONS`]
//! live versions per document; names are 1 to 120 characters without control characters (duplicates
//! are allowed: people name things "Final" more than once). Every change is audited without the
//! name's content beyond its length.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, Uuid};
use std::fmt;

use crate::TenantTransaction;
use crate::tenants::audit_success;

/// Most live versions one document keeps.
pub const MAX_VERSIONS: i64 = 500;
/// Longest version name, in characters.
pub const MAX_NAME_CHARS: usize = 120;
/// Most versions a listing page returns.
pub const MAX_PAGE: i64 = 100;

/// Fixed, non-leaking categories for version failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionError {
    InvalidName,
    /// The sequence is beyond the document's committed sequence.
    InvalidSequence,
    /// The document has [`MAX_VERSIONS`] versions already.
    TooMany,
    NotFound,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for VersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "version error: {self:?}")
    }
}

impl std::error::Error for VersionError {}

/// A name for storage: trimmed, 1 to [`MAX_NAME_CHARS`] characters, no control characters.
#[must_use]
pub fn normalize_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    let chars = trimmed.chars().count();
    ((1..=MAX_NAME_CHARS).contains(&chars) && !trimmed.chars().any(char::is_control)).then(|| trimmed.to_string())
}

/// A saved version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    pub id: Uuid,
    pub document_id: CloudDocumentId,
    pub seq: i64,
    pub name: String,
    pub created_by: PrincipalId,
    pub created_by_name: String,
    pub created_at: i64,
}

type Row = (String, String, i64, String, String, String, i64);

const GET_SQL: &str = concat!(
    "SELECT v.id::text, v.document_id::text, v.seq, v.name, v.created_by::text, p.display_name, ",
    "extract(epoch FROM v.created_at)::bigint FROM document_versions v JOIN principals p ON p.id = v.created_by ",
    "WHERE v.tenant_id = $1::uuid AND v.document_id = $2::uuid AND v.id = $3::uuid AND v.deleted_at IS NULL"
);

const LIST_SQL: &str = concat!(
    "SELECT v.id::text, v.document_id::text, v.seq, v.name, v.created_by::text, p.display_name, ",
    "extract(epoch FROM v.created_at)::bigint FROM document_versions v JOIN principals p ON p.id = v.created_by ",
    "WHERE v.tenant_id = $1::uuid AND v.document_id = $2::uuid AND v.deleted_at IS NULL ",
    "AND ($3::bigint IS NULL OR (extract(epoch FROM v.created_at)::bigint, v.id) < ($3::bigint, $4::uuid)) ",
    "ORDER BY extract(epoch FROM v.created_at)::bigint DESC, v.id DESC LIMIT $5"
);

fn version(row: Row) -> Result<Version, VersionError> {
    let (id, document, seq, name, created_by, created_by_name, created_at) = row;
    Ok(Version {
        id: Uuid::parse_str(&id).map_err(|_| VersionError::CorruptRow)?,
        document_id: document.parse().map_err(|_| VersionError::CorruptRow)?,
        seq,
        name,
        created_by: created_by.parse().map_err(|_| VersionError::CorruptRow)?,
        created_by_name,
        created_at,
    })
}

/// Save the document as it was at `seq` under `name`. `seq` may not exceed the committed sequence.
pub async fn create_version(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    seq: i64,
    name: &str,
    actor: PrincipalId,
) -> Result<Version, VersionError> {
    let name = normalize_name(name).ok_or(VersionError::InvalidName)?;
    let current: Option<i64> = sqlx::query_scalar("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR SHARE")
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| VersionError::Database)?;
    let current = current.ok_or(VersionError::NotFound)?;
    if !(0..=current).contains(&seq) {
        return Err(VersionError::InvalidSequence);
    }
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM document_versions WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND deleted_at IS NULL")
            .bind(transaction.tenant_id().to_string())
            .bind(document_id.to_string())
            .fetch_one(transaction.connection())
            .await
            .map_err(|_| VersionError::Database)?;
    if live >= MAX_VERSIONS {
        return Err(VersionError::TooMany);
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO document_versions (tenant_id, id, document_id, seq, name, created_by) VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6::uuid)",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(document_id.to_string())
    .bind(seq)
    .bind(&name)
    .bind(actor.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|_| VersionError::Database)?;
    audit_success(
        transaction,
        Some(actor),
        "document.version_create",
        "document",
        Some(document_id.as_uuid()),
        &serde_json::json!({ "version_id": id.to_string(), "seq": seq }),
    )
    .await
    .map_err(|_| VersionError::Audit)?;
    get_version(transaction, document_id, id).await?.ok_or(VersionError::NotFound)
}

/// The live version `id` of `document_id`.
pub async fn get_version(transaction: &mut TenantTransaction<'_>, document_id: CloudDocumentId, id: Uuid) -> Result<Option<Version>, VersionError> {
    let row: Option<Row> = sqlx::query_as(GET_SQL)
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| VersionError::Database)?;
    row.map(version).transpose()
}

/// Live versions newest first, older than `(created_at, id)` when given, `limit + 1` of them so the
/// caller can tell whether another page exists.
pub async fn list_versions(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    after: Option<(i64, Uuid)>,
    limit: i64,
) -> Result<Vec<Version>, VersionError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let (after_at, after_id) = after.map_or((None, None), |(at, id)| (Some(at), Some(id.to_string())));
    let rows: Vec<Row> = sqlx::query_as(LIST_SQL)
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .bind(after_at)
        .bind(after_id)
        .bind(limit + 1)
        .fetch_all(transaction.connection())
        .await
        .map_err(|_| VersionError::Database)?;
    rows.into_iter().map(version).collect()
}

/// Rename a live version.
pub async fn rename_version(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    id: Uuid,
    name: &str,
    actor: PrincipalId,
) -> Result<Version, VersionError> {
    let name = normalize_name(name).ok_or(VersionError::InvalidName)?;
    let updated = sqlx::query(concat!(
        "UPDATE document_versions SET name = $4, updated_at = clock_timestamp() ",
        "WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid AND deleted_at IS NULL"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(id.to_string())
    .bind(&name)
    .execute(transaction.connection())
    .await
    .map_err(|_| VersionError::Database)?;
    if updated.rows_affected() != 1 {
        return Err(VersionError::NotFound);
    }
    audit_success(
        transaction,
        Some(actor),
        "document.version_rename",
        "document",
        Some(document_id.as_uuid()),
        &serde_json::json!({ "version_id": id.to_string() }),
    )
    .await
    .map_err(|_| VersionError::Audit)?;
    get_version(transaction, document_id, id).await?.ok_or(VersionError::NotFound)
}

/// Hide a version's name. The history it pointed into is untouched.
pub async fn delete_version(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    id: Uuid,
    actor: PrincipalId,
) -> Result<(), VersionError> {
    let updated = sqlx::query(concat!(
        "UPDATE document_versions SET deleted_at = clock_timestamp(), deleted_by = $4::uuid ",
        "WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid AND deleted_at IS NULL"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(id.to_string())
    .bind(actor.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|_| VersionError::Database)?;
    if updated.rows_affected() != 1 {
        return Err(VersionError::NotFound);
    }
    audit_success(
        transaction,
        Some(actor),
        "document.version_delete",
        "document",
        Some(document_id.as_uuid()),
        &serde_json::json!({ "version_id": id.to_string() }),
    )
    .await
    .map_err(|_| VersionError::Audit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(normalize_name("  Before the rebrand ").as_deref(), Some("Before the rebrand"));
        for bad in ["", "   ", "tab\there", "nul\u{0}", &"x".repeat(121)] {
            assert_eq!(normalize_name(bad), None, "{bad:?}");
        }
        assert!(normalize_name(&"x".repeat(120)).is_some());
        assert!(normalize_name("日本語の名前").is_some());
    }
}
