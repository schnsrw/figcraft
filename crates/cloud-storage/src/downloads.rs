//! Authorized, short-lived presigned downloads of verified objects.
//!
//! A download URL is signed only through [`presign_download`], which consumes a
//! [`DownloadAuthorization`]. That grant has private fields and no `Clone`, `Default` or public
//! constructor: the only way to obtain one is [`authorize_download`], the explicit authorization
//! step. Today it checks that the principal is an active member of the transaction's active tenant
//! and that the object is verified and in that tenant; S4 will extend the same seam with document
//! and asset access checks, and callers must not sign anything on a path that skipped it.
//!
//! Each grant signs one `GET` for one key, once, with a TTL clamped to [`MAX_PRESIGN_TTL`]
//! (default [`DEFAULT_DOWNLOAD_TTL`]), and every issued URL is recorded as an
//! `object.download_signed` audit event in the same transaction. The result carries only the URL
//! and public object facts, never the bucket credentials. Errors are fixed categories; a missing,
//! foreign, unverified or deleted object is the same `Unavailable`, so a caller cannot probe
//! another tenant's object ids.

use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};

use crate::TenantTransaction;
use crate::audit::record_audit_event;
use crate::objects::{MAX_PRESIGN_TTL, ObjectStore, ObjectStoreError};

/// Validity of a download URL when the caller does not ask for one.
pub const DEFAULT_DOWNLOAD_TTL: Duration = Duration::from_secs(5 * 60);
/// Shortest validity a caller may ask for.
pub const MIN_DOWNLOAD_TTL: Duration = Duration::from_secs(1);
/// The audit action recorded for each issued URL.
pub const DOWNLOAD_SIGNED_ACTION: &str = "object.download_signed";

/// Fixed, non-leaking categories for download signing failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadError {
    /// The principal is not an active member of the active tenant, or the grant does not match
    /// this transaction or object.
    NotAuthorized,
    /// The object does not exist in this tenant, is not verified, or is being deleted.
    Unavailable,
    /// The requested TTL is shorter than [`MIN_DOWNLOAD_TTL`].
    InvalidTtl,
    Database,
    CorruptRow,
    /// The object lives in a bucket other than the store's, or its key cannot be signed.
    Store(ObjectStoreError),
    /// The audit event could not be recorded, so no URL is returned.
    Audit,
}

impl fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "download error: {self:?}")
    }
}

impl std::error::Error for DownloadError {}

/// Proof that one principal passed the download authorization step for one object in one
/// tenant. Only [`authorize_download`] constructs it, and [`presign_download`] consumes it.
#[derive(Debug, Eq, PartialEq)]
pub struct DownloadAuthorization {
    tenant_id: TenantId,
    principal_id: PrincipalId,
    object_id: Uuid,
}

impl DownloadAuthorization {
    #[must_use]
    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn object_id(&self) -> Uuid {
        self.object_id
    }
}

/// A signed download: the URL and the object facts a client needs to check what it fetched.
#[derive(Clone, Eq, PartialEq)]
pub struct PresignedDownload {
    pub url: String,
    pub expires_in: Duration,
    pub object_id: Uuid,
    pub size_bytes: i64,
    pub sha256: [u8; 32],
    pub media_type: String,
}

impl fmt::Debug for PresignedDownload {
    // The URL carries a bearer signature, so it is never formatted.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PresignedDownload")
            .field("expires_in", &self.expires_in)
            .field("object_id", &self.object_id)
            .field("size_bytes", &self.size_bytes)
            .finish_non_exhaustive()
    }
}

fn database_error(_: sqlx::Error) -> DownloadError {
    DownloadError::Database
}

/// The authorization step: check that `principal` is an active member of the transaction's
/// active tenant (and an active principal), and that `object_id` is a verified object in that
/// tenant.
///
/// This is the seam S4 extends with document and asset access checks. Until then, callers must
/// have checked that the principal may read the document or asset that references the object.
pub async fn authorize_download(
    transaction: &mut TenantTransaction<'_>,
    principal: PrincipalId,
    object_id: Uuid,
) -> Result<DownloadAuthorization, DownloadError> {
    let member: Option<(i32,)> = sqlx::query_as(concat!(
        "SELECT 1 FROM tenant_members member ",
        "JOIN tenants tenant ON tenant.id = member.tenant_id ",
        "JOIN principals principal ON principal.id = member.principal_id ",
        "WHERE member.tenant_id = $1::uuid AND member.principal_id = $2::uuid ",
        "AND member.state = 'active' AND tenant.state = 'active' AND principal.state = 'active'"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if member.is_none() {
        return Err(DownloadError::NotAuthorized);
    }
    let verified: Option<(i32,)> =
        sqlx::query_as("SELECT 1 FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'verified' AND deleted_at IS NULL")
            .bind(transaction.tenant_id().to_string())
            .bind(object_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    if verified.is_none() {
        return Err(DownloadError::Unavailable);
    }
    Ok(DownloadAuthorization { tenant_id: transaction.tenant_id(), principal_id: principal, object_id })
}

/// The TTL a download is signed for: the default when none is asked, whole seconds, at most
/// [`MAX_PRESIGN_TTL`], and refused below [`MIN_DOWNLOAD_TTL`].
pub fn download_ttl(requested: Option<Duration>) -> Result<Duration, DownloadError> {
    let ttl = requested.unwrap_or(DEFAULT_DOWNLOAD_TTL);
    if ttl < MIN_DOWNLOAD_TTL {
        return Err(DownloadError::InvalidTtl);
    }
    Ok(Duration::from_secs(ttl.min(MAX_PRESIGN_TTL).as_secs()))
}

type DownloadRow = (String, String, Vec<u8>, i64, String, String);

/// Sign one `GET` for `object_id` with `grant`, valid for `ttl` (see [`download_ttl`]), and
/// record an `object.download_signed` audit event in `transaction`.
///
/// The object is re-read under a share lock, so a row deleted or rejected since authorization is
/// `Unavailable`. The grant must come from this transaction's tenant and name `object_id`.
pub async fn presign_download(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    grant: DownloadAuthorization,
    object_id: Uuid,
    ttl: Option<Duration>,
) -> Result<PresignedDownload, DownloadError> {
    if grant.tenant_id != transaction.tenant_id() || grant.object_id != object_id {
        return Err(DownloadError::NotAuthorized);
    }
    let ttl = download_ttl(ttl)?;
    let row: Option<DownloadRow> = sqlx::query_as(concat!(
        "SELECT kind, bucket, sha256, size_bytes, media_type, object_key FROM objects ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'verified' AND deleted_at IS NULL FOR SHARE"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(object_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some((kind, bucket, sha256, size_bytes, media_type, object_key)) = row else {
        return Err(DownloadError::Unavailable);
    };
    if bucket != store.bucket_name() {
        return Err(DownloadError::Store(ObjectStoreError::InvalidConfig));
    }
    let sha256: [u8; 32] = sha256.try_into().map_err(|_| DownloadError::CorruptRow)?;
    let url = store.presign_get(&object_key, ttl).map_err(DownloadError::Store)?;
    let metadata = serde_json::json!({ "kind": kind, "ttl_seconds": ttl.as_secs() });
    record_audit_event(
        transaction,
        Uuid::new_v4(),
        Some(grant.principal_id.as_uuid()),
        DOWNLOAD_SIGNED_ACTION,
        "object",
        Some(object_id),
        None,
        None,
        "succeeded",
        &metadata,
    )
    .await
    .map_err(|_| DownloadError::Audit)?;
    Ok(PresignedDownload { url, expires_in: ttl, object_id, size_bytes, sha256, media_type })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_defaults_short_and_is_clamped_to_the_presign_maximum() {
        assert_eq!(download_ttl(None), Ok(DEFAULT_DOWNLOAD_TTL));
        assert!(DEFAULT_DOWNLOAD_TTL <= MAX_PRESIGN_TTL);
        assert_eq!(download_ttl(Some(Duration::from_secs(1))), Ok(Duration::from_secs(1)));
        assert_eq!(download_ttl(Some(Duration::from_millis(1500))), Ok(Duration::from_secs(1)));
        assert_eq!(download_ttl(Some(MAX_PRESIGN_TTL)), Ok(MAX_PRESIGN_TTL));
        assert_eq!(download_ttl(Some(Duration::MAX)), Ok(MAX_PRESIGN_TTL));
        assert_eq!(download_ttl(Some(Duration::ZERO)), Err(DownloadError::InvalidTtl));
        assert_eq!(download_ttl(Some(Duration::from_millis(999))), Err(DownloadError::InvalidTtl));
    }

    #[test]
    fn ttl_sweep_never_escapes_the_bounds() {
        // Junk sweep over nanosecond to multi-century TTLs, including the extremes.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut samples = vec![Duration::ZERO, Duration::MAX, Duration::from_nanos(1), Duration::new(u64::MAX, 999_999_999)];
        for _ in 0..4096 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            samples.push(Duration::new(seed >> (seed % 64), u32::try_from(seed % 1_000_000_000).unwrap()));
        }
        for requested in samples {
            match download_ttl(Some(requested)) {
                Ok(ttl) => {
                    assert!((MIN_DOWNLOAD_TTL..=MAX_PRESIGN_TTL).contains(&ttl), "{requested:?}");
                    assert_eq!(ttl.subsec_nanos(), 0);
                }
                Err(error) => {
                    assert_eq!(error, DownloadError::InvalidTtl);
                    assert!(requested < MIN_DOWNLOAD_TTL);
                }
            }
        }
    }
}
