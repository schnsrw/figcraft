//! Registered embedding host systems, issuers, public verification keys, audiences and allowed
//! parent origins (S6-001).
//!
//! A host product (the application that embeds the editor) is registered per tenant. Only PUBLIC
//! verification keys are stored (EdDSA/Ed25519 and ES256), so no host secret exists to leak;
//! HMAC hosts are deliberately not supported. Every query derives its tenant predicate from
//! `TenantTransaction`. The one pre-tenant lookup (issuer to tenant) uses a SECURITY DEFINER
//! function that only ever returns an enabled host in an active tenant.

use std::fmt;

use figcraft_cloud_domain::TenantId;
use sqlx::PgPool;
use url::Url;

use crate::TenantTransaction;

pub const MAX_SYSTEM_ID_CHARS: usize = 128;
pub const MAX_ISSUER_CHARS: usize = 256;
pub const MAX_AUDIENCES: usize = 8;
pub const MAX_AUDIENCE_BYTES: usize = 256;
pub const MAX_ORIGINS: usize = 16;
pub const MAX_ORIGIN_BYTES: usize = 256;
pub const MAX_KID_CHARS: usize = 128;
/// Keys that may be live (not retired) at once, which bounds rotation overlap.
pub const MAX_LIVE_KEYS: i64 = 8;
pub const MAX_LISTED_HOSTS: i64 = 100;

/// Fixed, non-leaking categories for host registry failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSystemError {
    InvalidInput,
    Conflict,
    NotFound,
    TooManyKeys,
    Database,
}

impl fmt::Display for HostSystemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "host system error: {self:?}")
    }
}

impl std::error::Error for HostSystemError {}

fn database_error(error: sqlx::Error) -> HostSystemError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => HostSystemError::Conflict,
        Some("23503" | "23514" | "22001") => HostSystemError::InvalidInput,
        _ => HostSystemError::Database,
    }
}

/// The highest role a host may grant. Ordered: `Viewer < Commenter < Editor`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HostRole {
    Viewer,
    Commenter,
    Editor,
}

impl HostRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Commenter => "commenter",
            Self::Editor => "editor",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "commenter" => Some(Self::Commenter),
            "editor" => Some(Self::Editor),
            _ => None,
        }
    }
}

/// Signature algorithms a registered key may use. There is no `none` and no HMAC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyAlgorithm {
    EdDsa,
    Es256,
}

impl KeyAlgorithm {
    /// The JOSE `alg` header value.
    #[must_use]
    pub const fn jose_name(self) -> &'static str {
        match self {
            Self::EdDsa => "EdDSA",
            Self::Es256 => "ES256",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "EdDSA" => Some(Self::EdDsa),
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }

    /// The raw public key length: 32 bytes for Ed25519, 65 (uncompressed SEC1 point) for P-256.
    #[must_use]
    pub const fn public_key_len(self) -> usize {
        match self {
            Self::EdDsa => 32,
            Self::Es256 => 65,
        }
    }
}

/// A host registration request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewHostSystem {
    pub system_id: String,
    pub issuer: String,
    pub audiences: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub max_role: HostRole,
    /// Development only: also accept `http://localhost`, `127.0.0.1` and `[::1]` origins.
    pub allow_insecure_localhost: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSystem {
    pub tenant_id: TenantId,
    pub system_id: String,
    pub issuer: String,
    pub audiences: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub max_role: HostRole,
    pub enabled: bool,
}

/// One public verification key. `expires_at` and `not_before` are Unix seconds; `retired_at`
/// (set by [`retire_host_key`]) ends the key's use immediately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKey {
    pub kid: String,
    pub alg: KeyAlgorithm,
    pub public_key: Vec<u8>,
    pub not_before: i64,
    pub expires_at: Option<i64>,
    pub retired_at: Option<i64>,
}

impl HostKey {
    /// Whether the key may verify a signature at `now` (Unix seconds).
    #[must_use]
    pub fn usable_at(&self, now: i64) -> bool {
        self.retired_at.is_none_or(|retired| now < retired) && now >= self.not_before && self.expires_at.is_none_or(|expiry| now < expiry)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewHostKey {
    pub kid: String,
    pub alg: KeyAlgorithm,
    pub public_key: Vec<u8>,
    pub not_before: i64,
    pub expires_at: Option<i64>,
}

/// The tenant and system an issuer resolves to before any tenant context exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLocator {
    pub tenant_id: TenantId,
    pub system_id: String,
}

fn plain_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && value.trim() == value && !value.chars().any(char::is_control)
}

/// Validate an exact origin string: `https://host[:port]` (plus `http` for loopback hosts only when
/// `allow_insecure_localhost`), with no path, query, fragment, userinfo or wildcard, and equal to
/// its own canonical serialization (so `https://Example.com/` and `https://*.example.com` fail).
#[must_use]
pub fn valid_origin(origin: &str, allow_insecure_localhost: bool) -> bool {
    if !plain_text(origin, MAX_ORIGIN_BYTES) || origin.contains('*') {
        return false;
    }
    let Ok(url) = Url::parse(origin) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && allow_insecure_localhost && loopback);
    scheme_ok
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.origin().ascii_serialization() == origin
}

fn valid_registration(host: &NewHostSystem) -> bool {
    let id_ok = host.system_id.chars().count() <= MAX_SYSTEM_ID_CHARS && plain_text(&host.system_id, 4 * MAX_SYSTEM_ID_CHARS);
    let issuer_ok = host.issuer.chars().count() <= MAX_ISSUER_CHARS && plain_text(&host.issuer, 4 * MAX_ISSUER_CHARS);
    id_ok && issuer_ok && valid_policy(&host.audiences, &host.allowed_origins, host.allow_insecure_localhost)
}

fn valid_policy(audiences: &[String], origins: &[String], allow_insecure_localhost: bool) -> bool {
    !audiences.is_empty()
        && audiences.len() <= MAX_AUDIENCES
        && audiences.iter().all(|audience| plain_text(audience, MAX_AUDIENCE_BYTES))
        && origins.len() <= MAX_ORIGINS
        && origins.iter().all(|origin| valid_origin(origin, allow_insecure_localhost))
}

type HostRow = (String, String, String, Vec<String>, Vec<String>, String, bool);

fn host_from_row(row: HostRow) -> Result<HostSystem, HostSystemError> {
    let (tenant_id, system_id, issuer, audiences, allowed_origins, max_role, enabled) = row;
    Ok(HostSystem {
        tenant_id: tenant_id.parse().map_err(|_| HostSystemError::Database)?,
        system_id,
        issuer,
        audiences,
        allowed_origins,
        max_role: HostRole::parse(&max_role).ok_or(HostSystemError::Database)?,
        enabled,
    })
}

/// Register a host system in this transaction's tenant. A duplicate system id or a second tenant
/// claiming an issuer is [`HostSystemError::Conflict`]. New hosts start enabled.
pub async fn register_host_system(transaction: &mut TenantTransaction<'_>, host: &NewHostSystem) -> Result<HostSystem, HostSystemError> {
    if !valid_registration(host) {
        return Err(HostSystemError::InvalidInput);
    }
    let row: HostRow = sqlx::query_as(
        "INSERT INTO host_systems (tenant_id, system_id, issuer, audiences, allowed_origins, max_role) \
         VALUES ($1::uuid, $2, $3, $4, $5, $6) \
         RETURNING tenant_id::text, system_id, issuer, audiences, allowed_origins, max_role, enabled",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(&host.system_id)
    .bind(&host.issuer)
    .bind(&host.audiences)
    .bind(&host.allowed_origins)
    .bind(host.max_role.as_str())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    host_from_row(row)
}

pub async fn get_host_system(transaction: &mut TenantTransaction<'_>, system_id: &str) -> Result<Option<HostSystem>, HostSystemError> {
    let row: Option<HostRow> = sqlx::query_as(
        "SELECT tenant_id::text, system_id, issuer, audiences, allowed_origins, max_role, enabled \
         FROM host_systems WHERE tenant_id = $1::uuid AND system_id = $2",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(host_from_row).transpose()
}

/// This tenant's hosts ordered by system id, at most [`MAX_LISTED_HOSTS`].
pub async fn list_host_systems(transaction: &mut TenantTransaction<'_>) -> Result<Vec<HostSystem>, HostSystemError> {
    let rows: Vec<HostRow> = sqlx::query_as(
        "SELECT tenant_id::text, system_id, issuer, audiences, allowed_origins, max_role, enabled \
         FROM host_systems WHERE tenant_id = $1::uuid ORDER BY system_id LIMIT 100",
    )
    .bind(transaction.tenant_id().to_string())
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(host_from_row).collect()
}

/// Replace a host's audiences, allowed origins and maximum role. The issuer is immutable.
pub async fn update_host_policy(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    audiences: &[String],
    allowed_origins: &[String],
    max_role: HostRole,
    allow_insecure_localhost: bool,
) -> Result<bool, HostSystemError> {
    if !valid_policy(audiences, allowed_origins, allow_insecure_localhost) {
        return Err(HostSystemError::InvalidInput);
    }
    let result = sqlx::query(
        "UPDATE host_systems SET audiences = $3, allowed_origins = $4, max_role = $5, updated_at = clock_timestamp() \
         WHERE tenant_id = $1::uuid AND system_id = $2",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(audiences)
    .bind(allowed_origins)
    .bind(max_role.as_str())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(result.rows_affected() == 1)
}

/// Enable or disable a host. A disabled host's tokens are refused at the issuer lookup, before any
/// key is read (S6-005). Returns whether the host exists.
pub async fn set_host_enabled(transaction: &mut TenantTransaction<'_>, system_id: &str, enabled: bool) -> Result<bool, HostSystemError> {
    let result = sqlx::query("UPDATE host_systems SET enabled = $3, updated_at = clock_timestamp() WHERE tenant_id = $1::uuid AND system_id = $2")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .bind(enabled)
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(result.rows_affected() == 1)
}

type KeyRow = (String, String, Vec<u8>, i64, Option<i64>, Option<i64>);

fn key_from_row(row: KeyRow) -> Result<HostKey, HostSystemError> {
    let (kid, alg, public_key, not_before, expires_at, retired_at) = row;
    Ok(HostKey { kid, alg: KeyAlgorithm::parse(&alg).ok_or(HostSystemError::Database)?, public_key, not_before, expires_at, retired_at })
}

/// Add a public verification key. Several keys may be live at once (rotation overlap), capped at
/// [`MAX_LIVE_KEYS`] unretired keys per host; a duplicate `kid` is [`HostSystemError::Conflict`].
pub async fn add_host_key(transaction: &mut TenantTransaction<'_>, system_id: &str, key: &NewHostKey) -> Result<(), HostSystemError> {
    let kid_ok = key.kid.chars().count() <= MAX_KID_CHARS && plain_text(&key.kid, 4 * MAX_KID_CHARS);
    let window_ok = key.not_before >= 0 && key.expires_at.is_none_or(|expiry| expiry > key.not_before);
    let key_ok = key.public_key.len() == key.alg.public_key_len() && (key.alg != KeyAlgorithm::Es256 || key.public_key.first() == Some(&4));
    if !kid_ok || !window_ok || !key_ok {
        return Err(HostSystemError::InvalidInput);
    }
    // Lock the host row so concurrent additions cannot both pass the cap.
    let locked: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM host_systems WHERE tenant_id = $1::uuid AND system_id = $2 FOR UPDATE")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    locked.ok_or(HostSystemError::NotFound)?;
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM host_system_keys WHERE tenant_id = $1::uuid AND system_id = $2 AND retired_at IS NULL")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .fetch_one(transaction.connection())
        .await
        .map_err(database_error)?;
    if live >= MAX_LIVE_KEYS {
        return Err(HostSystemError::TooManyKeys);
    }
    sqlx::query(
        "INSERT INTO host_system_keys (tenant_id, system_id, kid, alg, public_key, not_before, expires_at) \
         VALUES ($1::uuid, $2, $3, $4, $5, $6, $7)",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(&key.kid)
    .bind(key.alg.jose_name())
    .bind(&key.public_key)
    .bind(key.not_before)
    .bind(key.expires_at)
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(())
}

/// Retire a key now (`now` in Unix seconds): it stops verifying immediately. Returns whether a
/// live key was retired.
pub async fn retire_host_key(transaction: &mut TenantTransaction<'_>, system_id: &str, kid: &str, now: i64) -> Result<bool, HostSystemError> {
    let result = sqlx::query(
        "UPDATE host_system_keys SET retired_at = $4 \
         WHERE tenant_id = $1::uuid AND system_id = $2 AND kid = $3 AND retired_at IS NULL",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(kid)
    .bind(now.max(0))
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(result.rows_affected() == 1)
}

/// All of a host's keys (retired and expired included, for audit), at most 64.
pub async fn list_host_keys(transaction: &mut TenantTransaction<'_>, system_id: &str) -> Result<Vec<HostKey>, HostSystemError> {
    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT kid, alg, public_key, not_before, expires_at, retired_at FROM host_system_keys \
         WHERE tenant_id = $1::uuid AND system_id = $2 ORDER BY not_before, kid LIMIT 64",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(key_from_row).collect()
}

/// Which enabled host (in an active tenant) registered `issuer`, before any tenant is known.
/// A disabled host, an unknown issuer and a closing tenant are all `None`.
pub async fn find_host_by_issuer(pool: &PgPool, issuer: &str) -> Result<Option<HostLocator>, HostSystemError> {
    if issuer.is_empty() || issuer.len() > 4 * MAX_ISSUER_CHARS {
        return Ok(None);
    }
    let row: Option<(String, String)> = sqlx::query_as("SELECT tenant_id, system_id FROM figcraft_host_issuer($1)")
        .bind(issuer)
        .fetch_optional(pool)
        .await
        .map_err(|_| HostSystemError::Database)?;
    row.map(|(tenant_id, system_id)| Ok(HostLocator { tenant_id: tenant_id.parse().map_err(|_| HostSystemError::Database)?, system_id })).transpose()
}

#[cfg(test)]
mod tests {
    use super::{HostKey, KeyAlgorithm, valid_origin};

    #[test]
    fn origins_are_exact_https_origins() {
        assert!(valid_origin("https://host.example", false));
        assert!(valid_origin("https://host.example:8443", false));
        for bad in [
            "http://host.example",
            "https://host.example/",
            "https://host.example/path",
            "https://*.example.com",
            "https://user@host.example",
            "https://host.example?x=1",
            "https://host.example#f",
            "https://HOST.example",
            "host.example",
            "",
            " https://host.example",
            "javascript:alert(1)",
            "null",
        ] {
            assert!(!valid_origin(bad, false), "{bad} must be refused");
        }
        assert!(!valid_origin("http://localhost:3000", false));
        assert!(valid_origin("http://localhost:3000", true));
        assert!(!valid_origin("http://host.example", true));
    }

    #[test]
    fn key_windows_cover_not_before_expiry_and_retirement() {
        let key =
            HostKey { kid: "k".into(), alg: KeyAlgorithm::EdDsa, public_key: vec![0; 32], not_before: 100, expires_at: Some(200), retired_at: None };
        assert!(!key.usable_at(99));
        assert!(key.usable_at(100));
        assert!(key.usable_at(199));
        assert!(!key.usable_at(200));
        let retired = HostKey { retired_at: Some(150), ..key };
        assert!(retired.usable_at(149));
        assert!(!retired.usable_at(150));
    }
}
