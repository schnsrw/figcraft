//! Content-addressed image assets and the documents that use them.
//!
//! This is the document-facing asset API on top of objects (S3-002), client upload reservations
//! (S3-004/005), reference accounting (S3-007) and presigned downloads (S3-006):
//!
//! - [`find_asset`] answers a client's dedupe question before it reserves an upload: does the
//!   tenant already hold a verified asset with this SHA-256? If so the client attaches that object
//!   and uploads nothing.
//! - [`store_asset_bytes`] stores bytes produced inside the service (a worker rasterising a paste,
//!   an import), with the same checks a client upload passes at completion: the exact media-type
//!   allowlist and size cap of [`crate::uploads::upload_policy`], content validation by
//!   [`crate::validation::validate`], the content-addressed key [`keys::asset`], dedupe against an
//!   identical verified object, and registration plus read-back verification.
//! - [`attach_asset`] and [`detach_asset`] record and drop the `object_references` row with
//!   `owner_kind = 'document'` that keeps an asset alive for one document. Only verified asset
//!   objects of the transaction's tenant can be attached, only to that tenant's `pending` or
//!   `active` documents, and both calls are idempotent.
//! - [`list_document_assets`] pages a document's attached assets by object id (keyset, bounded).
//! - [`presign_document_asset_download`] signs a download only for an asset attached to a readable
//!   document, through [`crate::downloads`] so the membership check, TTL clamp and audit event are
//!   the same as for any other download.
//!
//! # Binary assets
//!
//! Only the raster images in [`ASSET_MEDIA_TYPES`] are accepted. Opaque binary assets (fonts,
//! arbitrary attachments, SVG, PDF) are refused with [`AssetError::UnsupportedMediaType`] until a
//! validator for each such type exists in [`crate::validation`]: an asset is served back to every
//! reader of the document, so nothing is stored that the service cannot vouch for. Adding a type
//! means adding it to the validator and the upload policy together, never only here.
//!
//! # Locking
//!
//! [`attach_asset`] locks the document row `FOR NO KEY UPDATE`, so attaches to one document
//! serialize (the [`MAX_ASSETS_PER_DOCUMENT`] cap is exact) and a concurrent archive or soft
//! delete either waits or is waited for. It then locks the object row `FOR SHARE`, which conflicts
//! with [`crate::object_references::begin_object_deletion`], so an attach never lands on an object
//! being collected. The document-scoped download locks the reference row `FOR SHARE`, so a
//! concurrent detach waits until the URL is signed and audited.
//!
//! Errors are fixed categories. A missing, foreign, unverified or non-asset object is the same
//! `AssetUnavailable`, and another tenant's document is `DocumentNotFound`, so a caller cannot
//! probe another tenant's ids.

use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, Uuid};
use sha2::{Digest, Sha256};

use crate::TenantTransaction;
use crate::downloads::{DownloadError, PresignedDownload, authorize_download, presign_download};
use crate::object_metadata::{
    NewObject, ObjectKind, ObjectRecord, ObjectRepositoryError, Verification, get_object, keys, register_object, verify_object,
};
use crate::object_references::{Change, ObjectReferenceError, OwnerKind, add_reference, remove_reference};
use crate::objects::{ObjectStore, ObjectStoreError};
use crate::uploads::{UploadError, valid_producer_version, validate_upload_request};
use crate::validation::{Budgets, Rejection, validate};

pub use crate::uploads::{ASSET_MEDIA_TYPES, MAX_ASSET_BYTES};

/// The most assets one document may have attached.
pub const MAX_ASSETS_PER_DOCUMENT: i64 = 10_000;
/// The most assets one [`list_document_assets`] call returns.
pub const MAX_ASSET_PAGE: u32 = 500;

/// Fixed, non-leaking categories for asset failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetError {
    /// The media type is not one of [`ASSET_MEDIA_TYPES`] (exact, lowercase, no parameters).
    /// Non-image binary assets are refused until a validator exists for them.
    UnsupportedMediaType,
    /// The bytes are empty or larger than [`MAX_ASSET_BYTES`].
    InvalidSize,
    /// The bytes failed content validation (magic bytes, header or pixel budgets).
    Invalid(Rejection),
    /// The producer version is empty, too long or not printable ASCII.
    InvalidProducerVersion,
    /// No such document in the transaction's tenant.
    DocumentNotFound,
    /// The document's state does not permit the operation (attaching to an archived, suspended or
    /// deleting document; downloading from a suspended or deleting one).
    DocumentUnavailable,
    /// The object does not exist in this tenant, is not an asset, or is not verified.
    AssetUnavailable,
    /// The asset is not attached to the document.
    NotAttached,
    /// The document already has [`MAX_ASSETS_PER_DOCUMENT`] assets attached.
    TooManyAssets,
    /// A page limit of zero or above [`MAX_ASSET_PAGE`].
    InvalidLimit,
    /// The content-addressed key is held by an object that cannot be filled (rejected, being
    /// deleted, or disagreeing with the bytes).
    ObjectConflict,
    /// The stored bytes did not read back as written.
    VerificationFailed,
    /// A unique violation or serialization failure; the caller may retry.
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    /// The object store failed in a way that says nothing about the bytes.
    Store(ObjectStoreError),
    /// The download step refused or failed (membership, TTL, signing or audit).
    Download(DownloadError),
}

impl fmt::Display for AssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "asset error: {self:?}")
    }
}

impl std::error::Error for AssetError {}

fn database_error(error: sqlx::Error) -> AssetError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => AssetError::Conflict,
        Some("23503" | "23514") => AssetError::Constraint,
        _ => AssetError::Database,
    }
}

fn object_error(error: ObjectRepositoryError) -> AssetError {
    match error {
        ObjectRepositoryError::Conflict => AssetError::Conflict,
        ObjectRepositoryError::Constraint => AssetError::Constraint,
        ObjectRepositoryError::Database => AssetError::Database,
        ObjectRepositoryError::CorruptRow => AssetError::CorruptRow,
        ObjectRepositoryError::InvalidState => AssetError::ObjectConflict,
        ObjectRepositoryError::Store(error) => AssetError::Store(error),
    }
}

fn reference_error(error: ObjectReferenceError) -> AssetError {
    match error {
        ObjectReferenceError::NotFound | ObjectReferenceError::InvalidState => AssetError::AssetUnavailable,
        ObjectReferenceError::Conflict => AssetError::Conflict,
        ObjectReferenceError::Constraint => AssetError::Constraint,
        ObjectReferenceError::CorruptRow => AssetError::CorruptRow,
        ObjectReferenceError::InvalidLimit => AssetError::InvalidLimit,
        ObjectReferenceError::Database | ObjectReferenceError::Referenced | ObjectReferenceError::TooYoung | ObjectReferenceError::TooManyHolds => {
            AssetError::Database
        }
    }
}

/// Bytes that passed every check that needs no I/O, with their canonical media type and digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedAsset {
    pub media_type: &'static str,
    pub sha256: [u8; 32],
    pub size_bytes: i64,
}

/// Check `bytes` declared as `media_type` against the asset policy and content validation, and
/// hash them. Pure and bounded: the size cap is checked before anything reads the bytes.
pub fn prepare_asset(bytes: &[u8], media_type: &str, producer_version: &str) -> Result<PreparedAsset, AssetError> {
    if !valid_producer_version(producer_version) {
        return Err(AssetError::InvalidProducerVersion);
    }
    let size_bytes = i64::try_from(bytes.len()).map_err(|_| AssetError::InvalidSize)?;
    let media_type = validate_upload_request(ObjectKind::Asset, size_bytes, media_type).map_err(|error| match error {
        UploadError::InvalidSize => AssetError::InvalidSize,
        _ => AssetError::UnsupportedMediaType,
    })?;
    validate(media_type, bytes, &Budgets::default()).map_err(AssetError::Invalid)?;
    Ok(PreparedAsset { media_type, sha256: Sha256::digest(bytes).into(), size_bytes })
}

/// The oldest verified asset object at `sha256`'s content-addressed key in the transaction's
/// tenant, optionally only in `bucket`.
async fn find_verified(transaction: &mut TenantTransaction<'_>, sha256: &[u8; 32], bucket: Option<&str>) -> Result<Option<ObjectRecord>, AssetError> {
    let key = keys::asset(transaction.tenant_id(), sha256);
    let id: Option<String> = sqlx::query_scalar(concat!(
        "SELECT id::text FROM objects WHERE tenant_id = $1::uuid AND kind = 'asset' AND sha256 = $2 AND object_key = $3 ",
        "AND state = 'verified' AND deleted_at IS NULL AND ($4::text IS NULL OR bucket = $4) ORDER BY created_at, id LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(sha256.as_slice())
    .bind(&key)
    .bind(bucket)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some(id) = id else {
        return Ok(None);
    };
    let id = id.parse().map_err(|_| AssetError::CorruptRow)?;
    get_object(transaction, id).await.map_err(object_error)?.ok_or(AssetError::CorruptRow).map(Some)
}

/// The verified asset with digest `sha256` in the transaction's tenant, if one exists, so a client
/// can attach it instead of uploading the same bytes again. Pending, rejected and deleting objects
/// are never returned, and another tenant's identical bytes are invisible. Callers must have
/// authorized the principal for the tenant.
pub async fn find_asset(transaction: &mut TenantTransaction<'_>, sha256: &[u8; 32]) -> Result<Option<ObjectRecord>, AssetError> {
    find_verified(transaction, sha256, None).await
}

/// The result of [`store_asset_bytes`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredAsset {
    pub object: ObjectRecord,
    /// True when an identical verified asset already existed and no bytes were written.
    pub reused: bool,
}

/// Store service-produced `bytes` as a verified, content-addressed asset in the transaction's
/// tenant, or return the identical verified asset it already holds.
///
/// The bytes pass [`prepare_asset`] first. A `pending` row already holding the key with the same
/// digest and size (an interrupted earlier write) is filled and verified; a rejected or deleting
/// one, or a row that disagrees with the bytes, is `ObjectConflict` until garbage collection
/// retires it. On any `Err` roll back. The stored object is unreferenced until it is attached, and
/// [`crate::object_references::MIN_COLLECTION_AGE`] keeps it safe in the meantime.
pub async fn store_asset_bytes(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    bytes: Vec<u8>,
    media_type: &str,
    producer_version: &str,
) -> Result<StoredAsset, AssetError> {
    let prepared = prepare_asset(&bytes, media_type, producer_version)?;
    if let Some(object) = find_verified(transaction, &prepared.sha256, Some(store.bucket_name())).await? {
        return Ok(StoredAsset { object, reused: true });
    }
    let tenant = transaction.tenant_id();
    let object_key = keys::asset(tenant, &prepared.sha256);
    let live: Option<(String, String, i64, Vec<u8>, String)> = sqlx::query_as(concat!(
        "SELECT id::text, state, size_bytes, sha256, kind FROM objects WHERE tenant_id = $1::uuid AND bucket = $2 AND object_key = $3 ",
        "AND state <> 'deleted' FOR UPDATE"
    ))
    .bind(tenant.to_string())
    .bind(store.bucket_name())
    .bind(&object_key)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let object_id = match live {
        Some((id, state, size_bytes, sha256, kind))
            if state == "pending" && size_bytes == prepared.size_bytes && sha256 == prepared.sha256 && kind == "asset" =>
        {
            id.parse().map_err(|_| AssetError::CorruptRow)?
        }
        Some(_) => return Err(AssetError::ObjectConflict),
        None => {
            let object = NewObject {
                id: Uuid::new_v4(),
                kind: ObjectKind::Asset,
                object_key: object_key.clone(),
                sha256: prepared.sha256,
                size_bytes: prepared.size_bytes,
                media_type: prepared.media_type.to_string(),
                producer_version: producer_version.to_string(),
            };
            register_object(transaction, store.bucket_name(), &object).await.map_err(object_error)?.id
        }
    };
    store.put_object(&object_key, bytes, prepared.media_type).await.map_err(AssetError::Store)?;
    match verify_object(transaction, store, object_id).await.map_err(object_error)? {
        Some(Verification::Verified(object)) => Ok(StoredAsset { object, reused: false }),
        Some(Verification::Rejected(..)) => Err(AssetError::VerificationFailed),
        None => Err(AssetError::CorruptRow),
    }
}

/// Read a document's state in the transaction's tenant, optionally locking it against concurrent
/// state changes and attaches.
async fn document_state(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId, lock: bool) -> Result<String, AssetError> {
    let query = if lock {
        "SELECT state FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR NO KEY UPDATE"
    } else {
        "SELECT state FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid"
    };
    let state: Option<String> = sqlx::query_scalar(query)
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    state.ok_or(AssetError::DocumentNotFound)
}

async fn attached(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId, object: Uuid, lock: bool) -> Result<bool, AssetError> {
    let query = if lock {
        concat!(
            "SELECT 1 FROM object_references WHERE tenant_id = $1::uuid AND object_id = $2::uuid ",
            "AND owner_kind = 'document' AND owner_id = $3::uuid FOR SHARE"
        )
    } else {
        concat!(
            "SELECT 1 FROM object_references WHERE tenant_id = $1::uuid AND object_id = $2::uuid ",
            "AND owner_kind = 'document' AND owner_id = $3::uuid"
        )
    };
    let row: Option<i32> = sqlx::query_scalar(query)
        .bind(transaction.tenant_id().to_string())
        .bind(object.to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(row.is_some())
}

/// Attach verified asset `object` to `document`, both in the transaction's tenant, recording an
/// `object_references` row with `owner_kind = 'document'`.
///
/// Idempotent: an existing attachment is `AlreadyApplied` whatever the document's state now. A new
/// attachment needs a `pending` or `active` document (`DocumentUnavailable` otherwise) and a
/// verified, non-deleted object of kind `asset` (`AssetUnavailable` otherwise), and is refused past
/// [`MAX_ASSETS_PER_DOCUMENT`] (`TooManyAssets`). Callers must have authorized the principal to
/// edit the document.
pub async fn attach_asset(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId, object: Uuid) -> Result<Change, AssetError> {
    let state = document_state(transaction, document, true).await?;
    if attached(transaction, document, object, false).await? {
        return Ok(Change::AlreadyApplied);
    }
    if !matches!(state.as_str(), "pending" | "active") {
        return Err(AssetError::DocumentUnavailable);
    }
    // Locked so a concurrent collection cannot mark it `deleting` before the reference lands.
    let usable: Option<i32> = sqlx::query_scalar(concat!(
        "SELECT 1 FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid AND kind = 'asset' ",
        "AND state = 'verified' AND deleted_at IS NULL FOR SHARE"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(object.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if usable.is_none() {
        return Err(AssetError::AssetUnavailable);
    }
    let count: i64 = sqlx::query_scalar(concat!(
        "SELECT count(*) FROM object_references r JOIN objects o ON o.tenant_id = r.tenant_id AND o.id = r.object_id ",
        "WHERE r.tenant_id = $1::uuid AND r.owner_kind = 'document' AND r.owner_id = $2::uuid AND o.kind = 'asset'"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    if count >= MAX_ASSETS_PER_DOCUMENT {
        return Err(AssetError::TooManyAssets);
    }
    add_reference(transaction, object, OwnerKind::Document, document.as_uuid()).await.map_err(reference_error)
}

/// Detach asset `object` from `document`. Idempotent: a missing attachment is `AlreadyApplied`.
/// Another tenant's or an unknown document is `DocumentNotFound`. The object stays until garbage
/// collection finds it unreferenced. Callers must have authorized the principal to edit the
/// document.
pub async fn detach_asset(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId, object: Uuid) -> Result<Change, AssetError> {
    document_state(transaction, document, false).await?;
    remove_reference(transaction, object, OwnerKind::Document, document.as_uuid()).await.map_err(reference_error)
}

/// An asset attached to a document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentAsset {
    pub object_id: Uuid,
    pub sha256: [u8; 32],
    pub size_bytes: i64,
    pub media_type: String,
}

/// One page of [`list_document_assets`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetPage {
    pub assets: Vec<DocumentAsset>,
    /// Pass as `after` to read the next page; `None` when this was the last.
    pub next_after: Option<Uuid>,
}

type AssetRow = (String, Vec<u8>, i64, String);

fn document_asset(row: AssetRow) -> Result<DocumentAsset, AssetError> {
    let (object_id, sha256, size_bytes, media_type) = row;
    Ok(DocumentAsset {
        object_id: object_id.parse().map_err(|_| AssetError::CorruptRow)?,
        sha256: sha256.try_into().map_err(|_| AssetError::CorruptRow)?,
        size_bytes,
        media_type,
    })
}

/// Up to `limit` (1..=[`MAX_ASSET_PAGE`]) of `document`'s attached verified assets, ordered by
/// object id, strictly after `after` when given. Another tenant's or an unknown document is
/// `DocumentNotFound`. Callers must have authorized the principal to read the document.
pub async fn list_document_assets(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    after: Option<Uuid>,
    limit: u32,
) -> Result<AssetPage, AssetError> {
    if limit == 0 || limit > MAX_ASSET_PAGE {
        return Err(AssetError::InvalidLimit);
    }
    document_state(transaction, document, false).await?;
    // One extra row says whether another page follows.
    let rows: Vec<AssetRow> = sqlx::query_as(concat!(
        "SELECT o.id::text, o.sha256, o.size_bytes, o.media_type FROM object_references r ",
        "JOIN objects o ON o.tenant_id = r.tenant_id AND o.id = r.object_id ",
        "WHERE r.tenant_id = $1::uuid AND r.owner_kind = 'document' AND r.owner_id = $2::uuid ",
        "AND o.kind = 'asset' AND o.state = 'verified' AND ($3::uuid IS NULL OR r.object_id > $3::uuid) ",
        "ORDER BY r.object_id LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(i64::from(limit) + 1)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    let mut assets = rows.into_iter().map(document_asset).collect::<Result<Vec<_>, _>>()?;
    assets.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let next_after = if more { assets.last().map(|asset| asset.object_id) } else { None };
    Ok(AssetPage { assets, next_after })
}

/// Sign a short-lived download of asset `object` for `principal`, but only while it is attached to
/// `document` and the document is readable (`pending`, `active` or `archived`).
///
/// The principal is checked first through [`authorize_download`] (an active member of the active
/// tenant; a refusal is `Download(NotAuthorized)` and reveals nothing about the document), then the
/// document and the attachment, then [`presign_download`] signs and audits the URL in
/// `transaction`. S4 adds the per-document role check at this seam; until then callers must have
/// authorized the principal to read the document.
pub async fn presign_document_asset_download(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    principal: PrincipalId,
    document: CloudDocumentId,
    object: Uuid,
    ttl: Option<Duration>,
) -> Result<PresignedDownload, AssetError> {
    let grant = authorize_download(transaction, principal, object).await.map_err(|error| match error {
        DownloadError::Unavailable => AssetError::NotAttached,
        error => AssetError::Download(error),
    })?;
    let state = document_state(transaction, document, false).await?;
    if !matches!(state.as_str(), "pending" | "active" | "archived") {
        return Err(AssetError::DocumentUnavailable);
    }
    if !attached(transaction, document, object, true).await? {
        return Err(AssetError::NotAttached);
    }
    presign_download(transaction, store, grant, object, ttl).await.map_err(|error| match error {
        DownloadError::Unavailable => AssetError::NotAttached,
        error => AssetError::Download(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid 1x1 greyscale PNG.
    fn png() -> Vec<u8> {
        fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            png.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
            let mut crc = crc32fast::Hasher::new();
            crc.update(kind);
            crc.update(data);
            png.extend_from_slice(kind);
            png.extend_from_slice(data);
            png.extend_from_slice(&crc.finalize().to_be_bytes());
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn images_are_prepared_with_their_canonical_type_and_digest() {
        let bytes = png();
        let prepared = prepare_asset(&bytes, "image/png", "figcraft-cloud test").unwrap();
        assert_eq!(prepared.media_type, "image/png");
        assert_eq!(prepared.sha256, <[u8; 32]>::from(Sha256::digest(&bytes)));
        assert_eq!(prepared.size_bytes, i64::try_from(bytes.len()).unwrap());
    }

    #[test]
    fn binary_and_active_formats_are_refused_until_a_validator_exists() {
        let bytes = png();
        for media_type in ["application/octet-stream", "image/svg+xml", "application/pdf", "font/woff2", "font/ttf", "text/plain", ""] {
            assert_eq!(prepare_asset(&bytes, media_type, "v"), Err(AssetError::UnsupportedMediaType), "{media_type}");
        }
        assert_eq!(prepare_asset(b"not an image", "image/png", "v"), Err(AssetError::Invalid(Rejection::MediaMismatch)));
        assert_eq!(prepare_asset(&bytes, "image/jpeg", "v"), Err(AssetError::Invalid(Rejection::MediaMismatch)));
        assert_eq!(prepare_asset(&[], "image/png", "v"), Err(AssetError::InvalidSize));
        assert_eq!(prepare_asset(&bytes, "image/png", ""), Err(AssetError::InvalidProducerVersion));
        assert_eq!(prepare_asset(&bytes, "image/png", "v\n"), Err(AssetError::InvalidProducerVersion));
    }

    #[test]
    fn oversize_bytes_are_refused_before_validation() {
        let mut bytes = png();
        bytes.resize(usize::try_from(MAX_ASSET_BYTES).unwrap() + 1, 0);
        assert_eq!(prepare_asset(&bytes, "image/png", "v"), Err(AssetError::InvalidSize));
    }

    #[test]
    fn junk_bytes_and_types_never_panic_or_pass_as_images() {
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let valid = png();
        let types = ["image/png", "image/jpeg", "image/webp", "image/gif", "IMAGE/PNG", "image/png; q=1", "application/octet-stream"];
        for round in 0..4096 {
            let len = usize::try_from(next() % 256).unwrap();
            let mut bytes: Vec<u8> = (0..len).map(|_| u8::try_from(next() & 0xff).unwrap()).collect();
            // Every eighth sample is a valid PNG with a few bytes flipped, to reach deeper checks.
            if round % 8 == 0 {
                bytes.clone_from(&valid);
                for _ in 0..3 {
                    let index = usize::try_from(next()).unwrap() % bytes.len();
                    bytes[index] ^= u8::try_from(next() & 0xff).unwrap() | 1;
                }
            }
            for media_type in types {
                if let Ok(prepared) = prepare_asset(&bytes, media_type, "v") {
                    assert!(ASSET_MEDIA_TYPES.contains(&prepared.media_type));
                    assert_eq!(prepared.media_type, media_type);
                    assert_eq!(prepared.sha256, <[u8; 32]>::from(Sha256::digest(&bytes)));
                }
            }
        }
    }

    #[test]
    fn reference_errors_map_to_fixed_asset_categories() {
        assert_eq!(reference_error(ObjectReferenceError::NotFound), AssetError::AssetUnavailable);
        assert_eq!(reference_error(ObjectReferenceError::InvalidState), AssetError::AssetUnavailable);
        assert_eq!(reference_error(ObjectReferenceError::Conflict), AssetError::Conflict);
        assert_eq!(object_error(ObjectRepositoryError::InvalidState), AssetError::ObjectConflict);
    }
}
