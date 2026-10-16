//! Tenant-scoped immutable object metadata and checksum verification.
//!
//! An `objects` row is registered `pending` before or while its bytes are written, and becomes
//! `verified` only after [`verify_object`] has read the stored bytes back and matched both the
//! recorded size and SHA-256. A mismatch or missing object marks the row `rejected`; nothing ever
//! turns a rejected or verified row back into `pending`, so corrupt bytes cannot replace verified
//! state. Keys follow the layout in `docs/saas/data-tenancy-integration.md` and are built here,
//! never from caller-supplied paths.

use std::fmt;

use figcraft_cloud_domain::{CloudDocumentId, TenantId, Uuid};
use sha2::{Digest, Sha256};

use crate::TenantTransaction;
use crate::objects::{ObjectStore, ObjectStoreError};

/// What an object holds, matching the `objects.kind` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Snapshot,
    Asset,
    Library,
    Plugin,
    Export,
    Thumbnail,
}

impl ObjectKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Asset => "asset",
            Self::Library => "library",
            Self::Plugin => "plugin",
            Self::Export => "export",
            Self::Thumbnail => "thumbnail",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::Snapshot, Self::Asset, Self::Library, Self::Plugin, Self::Export, Self::Thumbnail].into_iter().find(|kind| kind.as_str() == value)
    }
}

/// An object's lifecycle state, matching the `objects.state` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectState {
    Pending,
    Verified,
    Rejected,
    Deleting,
    Deleted,
}

impl ObjectState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "verified" => Some(Self::Verified),
            "rejected" => Some(Self::Rejected),
            "deleting" => Some(Self::Deleting),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// Lowercase hex of a SHA-256 digest, as used in content-addressed keys.
#[must_use]
pub fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Content-addressed and identity-scoped object keys.
pub mod keys {
    use super::{CloudDocumentId, TenantId, Uuid, hex_digest};

    #[must_use]
    pub fn snapshot(tenant: TenantId, document: CloudDocumentId, seq: u64, sha256: &[u8; 32]) -> String {
        format!("tenants/{tenant}/documents/{document}/snapshots/{seq}-{}.bin", hex_digest(sha256))
    }

    #[must_use]
    pub fn asset(tenant: TenantId, sha256: &[u8; 32]) -> String {
        format!("tenants/{tenant}/assets/{}", hex_digest(sha256))
    }

    #[must_use]
    pub fn library(tenant: TenantId, library: Uuid, version: u64, sha256: &[u8; 32]) -> String {
        format!("tenants/{tenant}/libraries/{library}/{version}-{}.figcraft", hex_digest(sha256))
    }

    #[must_use]
    pub fn plugin(tenant: TenantId, plugin: Uuid, version: u64, sha256: &[u8; 32]) -> String {
        format!("tenants/{tenant}/plugins/{plugin}/{version}-{}.bin", hex_digest(sha256))
    }

    #[must_use]
    pub fn thumbnail(tenant: TenantId, document: CloudDocumentId, seq: u64) -> String {
        format!("tenants/{tenant}/thumbnails/{document}/{seq}.webp")
    }

    #[must_use]
    pub fn staging(tenant: TenantId, reservation: Uuid) -> String {
        format!("tenants/{tenant}/staging/{reservation}")
    }
}

/// Fixed, non-leaking categories for object metadata failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectRepositoryError {
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    /// The row's state does not permit the operation.
    InvalidState,
    /// The object store failed in a way that says nothing about the bytes (unreachable,
    /// unauthorized); the row is left `pending` so verification can be retried.
    Store(ObjectStoreError),
}

impl fmt::Display for ObjectRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "object repository error: {self:?}")
    }
}

impl std::error::Error for ObjectRepositoryError {}

fn database_error(error: sqlx::Error) -> ObjectRepositoryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => ObjectRepositoryError::Conflict,
        Some("23503" | "23514") => ObjectRepositoryError::Constraint,
        _ => ObjectRepositoryError::Database,
    }
}

/// An object to register.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewObject {
    pub id: Uuid,
    pub kind: ObjectKind,
    pub object_key: String,
    pub sha256: [u8; 32],
    pub size_bytes: i64,
    pub media_type: String,
    pub producer_version: String,
}

/// An `objects` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectRecord {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub kind: ObjectKind,
    pub bucket: String,
    pub object_key: String,
    pub sha256: [u8; 32],
    pub size_bytes: i64,
    pub media_type: String,
    pub producer_version: String,
    pub state: ObjectState,
}

type ObjectRow = (String, String, String, String, String, Vec<u8>, i64, String, String, String);

/// The selected columns, as a literal so every query stays a compile-time string.
macro_rules! columns {
    () => {
        "id::text, tenant_id::text, kind, bucket, object_key, sha256, size_bytes, media_type, producer_version, state"
    };
}

fn record(row: ObjectRow) -> Result<ObjectRecord, ObjectRepositoryError> {
    let (id, tenant_id, kind, bucket, object_key, sha256, size_bytes, media_type, producer_version, state) = row;
    Ok(ObjectRecord {
        id: id.parse().map_err(|_| ObjectRepositoryError::CorruptRow)?,
        tenant_id: tenant_id.parse().map_err(|_| ObjectRepositoryError::CorruptRow)?,
        kind: ObjectKind::parse(&kind).ok_or(ObjectRepositoryError::CorruptRow)?,
        bucket,
        object_key,
        sha256: sha256.try_into().map_err(|_| ObjectRepositoryError::CorruptRow)?,
        size_bytes,
        media_type,
        producer_version,
        state: ObjectState::parse(&state).ok_or(ObjectRepositoryError::CorruptRow)?,
    })
}

/// Register a `pending` object in `bucket`. The table's constraints refuse keys outside the
/// tenant's prefix, a second live row for the same key (`Conflict`) and malformed fields
/// (`Constraint`).
pub async fn register_object(
    transaction: &mut TenantTransaction<'_>,
    bucket: &str,
    object: &NewObject,
) -> Result<ObjectRecord, ObjectRepositoryError> {
    let row: ObjectRow = sqlx::query_as(concat!(
        "INSERT INTO objects (id, tenant_id, kind, bucket, object_key, sha256, size_bytes, media_type, producer_version) ",
        "VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9) RETURNING ",
        columns!()
    ))
    .bind(object.id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(object.kind.as_str())
    .bind(bucket)
    .bind(&object.object_key)
    .bind(object.sha256.as_slice())
    .bind(object.size_bytes)
    .bind(&object.media_type)
    .bind(&object.producer_version)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    record(row)
}

pub async fn get_object(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<ObjectRecord>, ObjectRepositoryError> {
    let row: Option<ObjectRow> = sqlx::query_as(concat!("SELECT ", columns!(), " FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid"))
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    row.map(record).transpose()
}

/// Why verification rejected an object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Missing,
    SizeMismatch,
    ChecksumMismatch,
}

/// The result of [`verify_object`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verification {
    Verified(ObjectRecord),
    Rejected(ObjectRecord, Rejection),
}

/// Read `id`'s stored bytes back and compare them with the recorded size and SHA-256, then mark
/// the row `verified` or `rejected` in `transaction`.
///
/// Verifying an already verified object is a no-op that does not read the store again. A
/// rejected, deleting or deleted object is `InvalidState`. A store failure that says nothing about
/// the bytes leaves the row `pending` and is `Store`.
pub async fn verify_object(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    id: Uuid,
) -> Result<Option<Verification>, ObjectRepositoryError> {
    let row: Option<ObjectRow> =
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE"))
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    let Some(object) = row.map(record).transpose()? else {
        return Ok(None);
    };
    match object.state {
        ObjectState::Verified => return Ok(Some(Verification::Verified(object))),
        ObjectState::Pending => {}
        ObjectState::Rejected | ObjectState::Deleting | ObjectState::Deleted => return Err(ObjectRepositoryError::InvalidState),
    }
    if object.bucket != store.bucket_name() {
        return Err(ObjectRepositoryError::InvalidState);
    }
    let expected = u64::try_from(object.size_bytes).map_err(|_| ObjectRepositoryError::CorruptRow)?;
    let rejection = match store.get_object(&object.object_key, expected).await {
        Ok(bytes) if u64::try_from(bytes.len()).ok() != Some(expected) => Some(Rejection::SizeMismatch),
        Ok(bytes) if <[u8; 32]>::from(Sha256::digest(&bytes)) != object.sha256 => Some(Rejection::ChecksumMismatch),
        Ok(_) => None,
        Err(ObjectStoreError::NotFound) => Some(Rejection::Missing),
        Err(ObjectStoreError::TooLarge) => Some(Rejection::SizeMismatch),
        Err(error) => return Err(ObjectRepositoryError::Store(error)),
    };
    let state = if rejection.is_some() { "rejected" } else { "verified" };
    let row: ObjectRow = sqlx::query_as(concat!(
        "UPDATE objects SET state = $3, verified_at = CASE WHEN $3 = 'verified' THEN now() END ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'pending' RETURNING ",
        columns!()
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(state)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    let object = record(row)?;
    Ok(Some(match rejection {
        Some(reason) => Verification::Rejected(object, reason),
        None => Verification::Verified(object),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::valid_key;

    #[test]
    fn keys_follow_the_documented_layout_and_are_signable() {
        let tenant: TenantId = "10000000-0000-4000-8000-000000000001".parse().unwrap();
        let document: CloudDocumentId = "20000000-0000-4000-8000-000000000002".parse().unwrap();
        let id: Uuid = "30000000-0000-4000-8000-000000000003".parse().unwrap();
        let digest = [0xab; 32];
        let hex = "ab".repeat(32);
        let prefix = "tenants/10000000-0000-4000-8000-000000000001";
        for (key, expected) in [
            (keys::snapshot(tenant, document, 7, &digest), format!("{prefix}/documents/20000000-0000-4000-8000-000000000002/snapshots/7-{hex}.bin")),
            (keys::asset(tenant, &digest), format!("{prefix}/assets/{hex}")),
            (keys::library(tenant, id, 2, &digest), format!("{prefix}/libraries/30000000-0000-4000-8000-000000000003/2-{hex}.figcraft")),
            (keys::plugin(tenant, id, 3, &digest), format!("{prefix}/plugins/30000000-0000-4000-8000-000000000003/3-{hex}.bin")),
            (keys::thumbnail(tenant, document, 9), format!("{prefix}/thumbnails/20000000-0000-4000-8000-000000000002/9.webp")),
            (keys::staging(tenant, id), format!("{prefix}/staging/30000000-0000-4000-8000-000000000003")),
        ] {
            assert_eq!(key, expected);
            assert!(valid_key(&key), "{key}");
        }
    }

    #[test]
    fn kinds_round_trip() {
        for kind in [ObjectKind::Snapshot, ObjectKind::Asset, ObjectKind::Library, ObjectKind::Plugin, ObjectKind::Export, ObjectKind::Thumbnail] {
            assert_eq!(ObjectKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ObjectKind::parse("script"), None);
    }
}
