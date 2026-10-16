//! Staging upload reservations.
//!
//! A client that wants to upload bytes first reserves the upload: [`reserve_upload`] records the
//! tenant, principal, object kind, expected SHA-256, expected size, media type and expiry in an
//! `upload_reservations` row, and only then signs one presigned `PUT` for exactly that row's
//! staging key, `tenants/<tenant>/staging/<reservation>` (fixed by a table constraint). Staging keys
//! are never content-addressed final keys, so staged bytes cannot be referenced as verified objects.
//!
//! Sizes, kinds and media types are checked against the per-kind policy below before any SQL runs.
//! The presigned request is signed before the row is inserted and the row's expiry is computed from
//! the database clock afterwards, so the URL never outlives the reservation by more than the clock
//! skew between this process and PostgreSQL. [`expire_reservations`] turns overdue `reserved` rows
//! into `expired` in bounded batches and hands back their staging keys for deletion; bucket
//! lifecycle expiry of the staging prefix (S3-014) is the backstop for keys a sweep never reaches.
//!
//! `TIMESTAMPTZ` columns are only read as whole Unix seconds, because no `chrono`/`time` SQLx
//! feature is enabled in this crate.

use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};

use crate::TenantTransaction;
use crate::object_metadata::{ObjectKind, keys};
use crate::objects::{MAX_PRESIGN_TTL, ObjectStore, ObjectStoreError};

/// Largest image or binary asset a client may upload (S3-009 decodes images within S3-013 pixel
/// budgets; this bound only caps transfer and storage).
pub const MAX_ASSET_BYTES: i64 = 32 * 1024 * 1024;
/// Largest encoded document snapshot.
pub const MAX_SNAPSHOT_BYTES: i64 = 128 * 1024 * 1024;
/// Largest published team-library version.
pub const MAX_LIBRARY_BYTES: i64 = 128 * 1024 * 1024;
/// Largest plugin bundle.
pub const MAX_PLUGIN_BYTES: i64 = 16 * 1024 * 1024;
/// Most unexpired `reserved` uploads one principal may hold in one tenant, so retries or a hostile
/// client cannot fill the table or the staging prefix.
pub const MAX_OPEN_RESERVATIONS_PER_PRINCIPAL: i64 = 100;
/// Most reservations one [`expire_reservations`] call transitions.
pub const MAX_EXPIRE_BATCH: i64 = 500;

/// Media types accepted for client-uploaded assets.
///
/// Raster images FigCraft can place, plus `application/octet-stream` for the opaque binary assets
/// S3-009 stores (for example embedded font or data blobs); opaque assets are only ever served as
/// attachment downloads, never rendered inline. `image/svg+xml` is deliberately absent: SVG can
/// carry script, and imported SVG becomes vector nodes in the document rather than a stored file.
pub const ASSET_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "application/octet-stream"];
/// FigCraft's own encoded formats, one vendor type per kind so a snapshot cannot be reserved as a
/// library or plugin.
pub const SNAPSHOT_MEDIA_TYPES: &[&str] = &["application/vnd.figcraft.snapshot"];
pub const LIBRARY_MEDIA_TYPES: &[&str] = &["application/vnd.figcraft.library"];
pub const PLUGIN_MEDIA_TYPES: &[&str] = &["application/vnd.figcraft.plugin"];

/// The upload policy for `kind`: its largest size and accepted media types. Exports and thumbnails
/// are produced and written by workers (S3-012), never uploaded through a client reservation.
#[must_use]
pub const fn upload_policy(kind: ObjectKind) -> Option<(i64, &'static [&'static str])> {
    match kind {
        ObjectKind::Asset => Some((MAX_ASSET_BYTES, ASSET_MEDIA_TYPES)),
        ObjectKind::Snapshot => Some((MAX_SNAPSHOT_BYTES, SNAPSHOT_MEDIA_TYPES)),
        ObjectKind::Library => Some((MAX_LIBRARY_BYTES, LIBRARY_MEDIA_TYPES)),
        ObjectKind::Plugin => Some((MAX_PLUGIN_BYTES, PLUGIN_MEDIA_TYPES)),
        ObjectKind::Export | ObjectKind::Thumbnail => None,
    }
}

/// Fixed, non-leaking categories for upload failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploadError {
    /// The kind is not uploaded through reservations, or completion is not implemented for it yet.
    UnsupportedKind,
    /// The media type is not on the kind's allowlist.
    UnsupportedMediaType,
    /// The expected size is not positive or exceeds the kind's maximum.
    InvalidSize,
    /// The requested validity is shorter than one second.
    InvalidTtl,
    /// The principal already holds the maximum number of open reservations.
    TooManyReservations,
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    /// The object store refused to sign or failed in a way that says nothing about the bytes.
    Store(ObjectStoreError),
}

impl fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "upload error: {self:?}")
    }
}

impl std::error::Error for UploadError {}

fn database_error(error: sqlx::Error) -> UploadError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => UploadError::Conflict,
        Some("23503" | "23514") => UploadError::Constraint,
        _ => UploadError::Database,
    }
}

/// A reservation's lifecycle state, matching the `upload_reservations.state` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationState {
    Reserved,
    Completed,
    Promoted,
    Rejected,
    Expired,
}

impl ReservationState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "reserved" => Some(Self::Reserved),
            "completed" => Some(Self::Completed),
            "promoted" => Some(Self::Promoted),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

fn parse_kind(value: &str) -> Option<ObjectKind> {
    [ObjectKind::Snapshot, ObjectKind::Asset, ObjectKind::Library, ObjectKind::Plugin, ObjectKind::Export, ObjectKind::Thumbnail]
        .into_iter()
        .find(|kind| kind.as_str() == value)
}

/// An `upload_reservations` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadReservation {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub kind: ObjectKind,
    pub staging_key: String,
    pub expected_sha256: [u8; 32],
    pub expected_size: i64,
    pub media_type: String,
    pub state: ReservationState,
    /// Expiry as whole Unix seconds (rounded down).
    pub expires_at_unix: i64,
    /// Whether the database clock had passed the expiry when the row was read.
    pub expired: bool,
    pub object_id: Option<Uuid>,
}

/// A new reservation and the one request the client may send to fill it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedUpload {
    pub reservation: UploadReservation,
    /// Always `PUT`, the only method the table allows.
    pub method: &'static str,
    /// A presigned `PUT` for exactly the staging key.
    pub url: String,
    /// The validity signed into `url`, equal to the reservation's lifetime.
    pub ttl: Duration,
}

type ReservationRow = (String, String, String, String, String, Vec<u8>, i64, String, String, i64, bool, Option<String>);

/// The selected columns, as a literal so every query stays a compile-time string.
macro_rules! columns {
    () => {
        concat!(
            "id::text, tenant_id::text, principal_id::text, kind, staging_key, expected_sha256, expected_size, media_type, state, ",
            "floor(extract(epoch FROM expires_at))::bigint, expires_at <= clock_timestamp(), object_id::text"
        )
    };
}

fn reservation(row: ReservationRow) -> Result<UploadReservation, UploadError> {
    let (id, tenant_id, principal_id, kind, staging_key, sha256, expected_size, media_type, state, expires_at_unix, expired, object_id) = row;
    Ok(UploadReservation {
        id: id.parse().map_err(|_| UploadError::CorruptRow)?,
        tenant_id: tenant_id.parse().map_err(|_| UploadError::CorruptRow)?,
        principal_id: principal_id.parse().map_err(|_| UploadError::CorruptRow)?,
        kind: parse_kind(&kind).ok_or(UploadError::CorruptRow)?,
        staging_key,
        expected_sha256: sha256.try_into().map_err(|_| UploadError::CorruptRow)?,
        expected_size,
        media_type,
        state: ReservationState::parse(&state).ok_or(UploadError::CorruptRow)?,
        expires_at_unix,
        expired,
        object_id: object_id.map(|id| id.parse()).transpose().map_err(|_| UploadError::CorruptRow)?,
    })
}

/// Check a requested upload against the kind's policy, returning the media type as stored.
///
/// Media types are matched exactly against the allowlist (lowercase, no parameters), so a caller
/// cannot smuggle `image/png; charset=...` or mixed case into stored metadata.
pub fn validate_upload_request(kind: ObjectKind, expected_size: i64, media_type: &str) -> Result<&'static str, UploadError> {
    let (max_bytes, media_types) = upload_policy(kind).ok_or(UploadError::UnsupportedKind)?;
    if expected_size <= 0 || expected_size > max_bytes {
        return Err(UploadError::InvalidSize);
    }
    media_types.iter().copied().find(|allowed| *allowed == media_type).ok_or(UploadError::UnsupportedMediaType)
}

/// The reservation lifetime for a requested `ttl`: whole seconds, at least one, capped to
/// [`MAX_PRESIGN_TTL`].
pub fn reservation_ttl(ttl: Duration) -> Result<Duration, UploadError> {
    if ttl < Duration::from_secs(1) {
        return Err(UploadError::InvalidTtl);
    }
    Ok(Duration::from_secs(ttl.min(MAX_PRESIGN_TTL).as_secs()))
}

/// Reserve a staging upload for `principal` in the transaction's tenant and sign its `PUT`.
///
/// Callers must have authorized `principal` for the tenant and checked quota first; this function
/// binds what was authorized (kind, digest, size, type, expiry) but decides nothing about who may
/// upload. Commit the transaction before handing the URL out: completion only finds committed rows.
#[allow(clippy::too_many_arguments)]
pub async fn reserve_upload(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    principal: PrincipalId,
    kind: ObjectKind,
    expected_sha256: [u8; 32],
    expected_size: i64,
    media_type: &str,
    ttl: Duration,
) -> Result<ReservedUpload, UploadError> {
    let media_type = validate_upload_request(kind, expected_size, media_type)?;
    let ttl = reservation_ttl(ttl)?;
    let ttl_seconds = i64::try_from(ttl.as_secs()).map_err(|_| UploadError::InvalidTtl)?;

    let open: i64 = sqlx::query_scalar(concat!(
        "SELECT count(*) FROM upload_reservations WHERE tenant_id = $1::uuid AND principal_id = $2::uuid ",
        "AND state = 'reserved' AND expires_at > clock_timestamp()"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    if open >= MAX_OPEN_RESERVATIONS_PER_PRINCIPAL {
        return Err(UploadError::TooManyReservations);
    }

    let id = Uuid::new_v4();
    let staging_key = keys::staging(transaction.tenant_id(), id);
    // Signed before the row exists, so the URL expires no later than the reservation.
    let url = store.presign_put(&staging_key, ttl).map_err(UploadError::Store)?;
    let row: ReservationRow = sqlx::query_as(concat!(
        "INSERT INTO upload_reservations (id, tenant_id, principal_id, kind, staging_key, expected_sha256, expected_size, media_type, expires_at) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6, $7, $8, clock_timestamp() + make_interval(secs => $9)) RETURNING ",
        columns!()
    ))
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(principal.to_string())
    .bind(kind.as_str())
    .bind(&staging_key)
    .bind(expected_sha256.as_slice())
    .bind(expected_size)
    .bind(media_type)
    .bind(ttl_seconds)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(ReservedUpload { reservation: reservation(row)?, method: "PUT", url, ttl })
}

/// Read one reservation in the transaction's tenant.
pub async fn get_reservation(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<UploadReservation>, UploadError> {
    let row: Option<ReservationRow> =
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM upload_reservations WHERE tenant_id = $1::uuid AND id = $2::uuid"))
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    row.map(reservation).transpose()
}

/// Mark up to `limit` (capped to [`MAX_EXPIRE_BATCH`]) of the tenant's overdue `reserved` uploads
/// `expired`, oldest expiry first, and return their staging keys.
///
/// Rows another transaction holds (a completion in progress) are skipped, not waited for. Delete
/// the returned keys only after committing; a key whose deletion fails is left to the staging
/// lifecycle rule. Call again while a full batch comes back.
pub async fn expire_reservations(transaction: &mut TenantTransaction<'_>, limit: i64) -> Result<Vec<String>, UploadError> {
    let limit = limit.clamp(0, MAX_EXPIRE_BATCH);
    if limit == 0 {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(concat!(
        "WITH due AS (SELECT id FROM upload_reservations WHERE tenant_id = $1::uuid AND state = 'reserved' ",
        "AND expires_at <= clock_timestamp() ORDER BY expires_at, id LIMIT $2 FOR UPDATE SKIP LOCKED) ",
        "UPDATE upload_reservations AS reservation SET state = 'expired' FROM due ",
        "WHERE reservation.tenant_id = $1::uuid AND reservation.id = due.id RETURNING reservation.staging_key"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::MAX_SINGLE_TRANSFER_BYTES;

    const ALL_KINDS: [ObjectKind; 6] =
        [ObjectKind::Snapshot, ObjectKind::Asset, ObjectKind::Library, ObjectKind::Plugin, ObjectKind::Export, ObjectKind::Thumbnail];

    #[test]
    fn policies_fit_one_transfer_and_name_only_exact_types() {
        for kind in ALL_KINDS {
            assert_eq!(parse_kind(kind.as_str()), Some(kind));
            let Some((max_bytes, media_types)) = upload_policy(kind) else {
                assert!(matches!(kind, ObjectKind::Export | ObjectKind::Thumbnail));
                assert_eq!(validate_upload_request(kind, 1, "image/png"), Err(UploadError::UnsupportedKind));
                continue;
            };
            assert!(max_bytes > 0 && u64::try_from(max_bytes).unwrap() <= MAX_SINGLE_TRANSFER_BYTES);
            for media_type in media_types {
                assert!(media_type.bytes().all(|byte| byte.is_ascii_lowercase() || b"/.+-".contains(&byte)), "{media_type}");
                assert_eq!(validate_upload_request(kind, 1, media_type), Ok(*media_type));
                assert_eq!(validate_upload_request(kind, max_bytes, media_type), Ok(*media_type));
                assert_eq!(validate_upload_request(kind, max_bytes + 1, media_type), Err(UploadError::InvalidSize));
                assert_eq!(validate_upload_request(kind, 0, media_type), Err(UploadError::InvalidSize));
                assert_eq!(validate_upload_request(kind, -1, media_type), Err(UploadError::InvalidSize));
                assert_eq!(validate_upload_request(kind, i64::MIN, media_type), Err(UploadError::InvalidSize));
            }
        }
        assert!(!ASSET_MEDIA_TYPES.contains(&"image/svg+xml"));
        assert_eq!(validate_upload_request(ObjectKind::Snapshot, 1, "image/png"), Err(UploadError::UnsupportedMediaType));
        assert_eq!(validate_upload_request(ObjectKind::Asset, 1, "application/vnd.figcraft.snapshot"), Err(UploadError::UnsupportedMediaType));
    }

    #[test]
    fn junk_media_types_and_states_are_refused_without_panicking() {
        let mut junk: Vec<String> = [
            "",
            " ",
            "image/PNG",
            "IMAGE/png",
            "image/png ",
            " image/png",
            "image/png; charset=utf-8",
            "image/png\0",
            "image/png\n",
            "image/svg+xml",
            "text/html",
            "image/",
            "/png",
            "image",
            "é/png",
            "image/pngé",
        ]
        .iter()
        .map(|value| (*value).to_string())
        .collect();
        junk.push("a".repeat(10_000));
        junk.push("image/png".repeat(1000));
        junk.extend((0u8..=255).map(|byte| String::from_utf8_lossy(&[b'i', byte]).into_owned()));
        for value in &junk {
            for kind in ALL_KINDS {
                assert!(validate_upload_request(kind, 1, value).is_err(), "{kind:?} {value:?}");
            }
            assert_eq!(ReservationState::parse(value), None);
            assert_eq!(parse_kind(value), None);
        }
    }

    #[test]
    fn ttls_are_whole_seconds_between_one_second_and_the_presign_cap() {
        assert_eq!(reservation_ttl(Duration::ZERO), Err(UploadError::InvalidTtl));
        assert_eq!(reservation_ttl(Duration::from_millis(999)), Err(UploadError::InvalidTtl));
        assert_eq!(reservation_ttl(Duration::from_millis(1500)), Ok(Duration::from_secs(1)));
        assert_eq!(reservation_ttl(Duration::from_secs(300)), Ok(Duration::from_secs(300)));
        assert_eq!(reservation_ttl(MAX_PRESIGN_TTL + Duration::from_secs(1)), Ok(MAX_PRESIGN_TTL));
        assert_eq!(reservation_ttl(Duration::MAX), Ok(MAX_PRESIGN_TTL));
    }
}
