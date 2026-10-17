//! Document share links (core slice; the storage half of S4-005).
//!
//! A link's secret is 32 random bytes from the operating system's generator, written as URL-safe
//! base64 without padding (43 characters). It is returned once by [`create_share_link`]; only its
//! SHA-256 is stored. [`resolve_share_link`] hashes the presented secret and looks the hash up
//! (the unique index on `token_hash`): the secret itself is never compared or logged, so a lookup
//! reveals nothing about secrets that are close to a real one. Every refusal (unknown, malformed,
//! revoked, expired, or a document that no longer accepts collaborators) is the same
//! [`ShareLinkError::Refused`] so callers cannot tell them apart.
//!
//! Resolution runs before any tenant is known (the secret is what names it), so it reads through
//! the pool like [`list_tenants_for_principal`](crate::metadata::list_tenants_for_principal);
//! creating, listing, updating and revoking are tenant-scoped.
//!
//! [`exchange_share_token`] is what the HTTP API calls to turn a presented secret into a
//! [`ShareGrant`]: the link's tenant, document, role and id, and when a share session minted from
//! it must end (never later than the link itself, and never more than [`MAX_SHARE_SESSION_SECS`]
//! away). [`dead_share_links`] is the relay's revocation check: of the links it admitted, which no
//! longer open anything (S4-017).

use base64::Engine as _;
use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::TenantTransaction;

/// Random bytes in a share secret.
pub const SECRET_BYTES: usize = 32;
/// Characters in an encoded share secret (URL-safe base64 of 32 bytes, no padding).
pub const SECRET_CHARS: usize = 43;
/// Longest label accepted.
pub const MAX_LABEL_CHARS: usize = 255;
/// Longest lifetime a link may be given (one year).
pub const MAX_LIFETIME_SECS: u64 = 366 * 24 * 60 * 60;

/// Longest a share session minted by [`exchange_share_token`] may last (one hour); a link that
/// expires sooner ends it sooner.
pub const MAX_SHARE_SESSION_SECS: u64 = 60 * 60;
/// Most links [`list_share_links`] returns for one document (newest first).
pub const MAX_LISTED_LINKS: i64 = 500;
/// Most links one [`dead_share_links`] call checks.
pub const MAX_CHECKED_LINKS: usize = 1024;

/// What a link lets its holder do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShareRole {
    Editor,
    Viewer,
}

impl ShareRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

/// Fixed, non-leaking categories for share-link failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShareLinkError {
    /// The label or lifetime is unusable.
    InvalidInput,
    /// No secure random numbers are available.
    Random,
    /// The secret does not open anything (unknown, malformed, revoked or expired link, or a
    /// document or tenant that is not active). Deliberately one category.
    Refused,
    /// The document does not exist in this tenant, or the creator is unknown.
    Constraint,
    Database,
}

impl std::fmt::Display for ShareLinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "share link error: {self:?}")
    }
}

impl std::error::Error for ShareLinkError {}

/// A link just created: its id, and its secret, which is never readable again.
#[derive(Clone, Eq, PartialEq)]
pub struct CreatedShareLink {
    pub id: Uuid,
    pub secret: String,
}

impl std::fmt::Debug for CreatedShareLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CreatedShareLink").field("id", &self.id).field("secret", &"[REDACTED]").finish()
    }
}

/// What a valid secret opens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedShareLink {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub document_id: CloudDocumentId,
    pub role: ShareRole,
    pub created_by: PrincipalId,
}

/// What [`exchange_share_token`] grants: enough for the API to mint a short-lived share session
/// (it never carries the secret). Times are Unix seconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShareGrant {
    pub link_id: Uuid,
    pub tenant_id: TenantId,
    pub document_id: CloudDocumentId,
    pub role: ShareRole,
    /// Whom actions taken through the link are attributed to until its holder signs in.
    pub created_by: PrincipalId,
    /// When the link itself expires (`None`: never).
    pub link_expires_at: Option<i64>,
    /// When a session minted from this grant must end: the earlier of the link's expiry and
    /// [`MAX_SHARE_SESSION_SECS`] from now.
    pub session_expires_at: i64,
}

/// A link as its document's managers see it (never its secret or hash). Times are Unix seconds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShareLinkRecord {
    pub id: Uuid,
    pub document_id: CloudDocumentId,
    pub role: ShareRole,
    pub label: Option<String>,
    pub created_by: PrincipalId,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl ShareLinkRecord {
    /// Whether the link was live at Unix time `now` (not revoked and not expired; the document's
    /// own state is not part of the record).
    #[must_use]
    pub fn live_at(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|at| at > now)
    }
}

/// A change to a live link: `None` leaves a field as it is.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShareLinkUpdate<'a> {
    /// `Some(None)` clears the label.
    pub label: Option<Option<&'a str>>,
    /// `Some(Some(secs))` makes it expire `secs` from now (more than zero, at most
    /// [`MAX_LIFETIME_SECS`]); `Some(None)` makes it never expire.
    pub expires_in_secs: Option<Option<u64>>,
}

/// The SHA-256 stored for `secret`.
#[must_use]
pub fn share_token_hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn new_secret() -> Result<String, ShareLinkError> {
    let mut bytes = [0u8; SECRET_BYTES];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut bytes).map_err(|_| ShareLinkError::Random)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn well_formed(secret: &str) -> bool {
    secret.len() == SECRET_CHARS && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn database_error(error: &sqlx::Error) -> ShareLinkError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503" | "23514") => ShareLinkError::Constraint,
        _ => ShareLinkError::Database,
    }
}

/// Create a link to `document_id` in the transaction's tenant. `expires_in_secs` (at most
/// [`MAX_LIFETIME_SECS`], more than zero) makes it expire; `None` never expires. The caller
/// commits; the returned secret is the only copy.
pub async fn create_share_link(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    role: ShareRole,
    label: Option<&str>,
    created_by: PrincipalId,
    expires_in_secs: Option<u64>,
) -> Result<CreatedShareLink, ShareLinkError> {
    if label.is_some_and(|label| label.is_empty() || label.chars().count() > MAX_LABEL_CHARS) {
        return Err(ShareLinkError::InvalidInput);
    }
    let lifetime = match expires_in_secs {
        None => None,
        Some(secs) if secs == 0 || secs > MAX_LIFETIME_SECS => return Err(ShareLinkError::InvalidInput),
        Some(secs) => Some(i64::try_from(secs).map_err(|_| ShareLinkError::InvalidInput)?),
    };
    let secret = new_secret()?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO share_links (id, tenant_id, document_id, token_hash, role, label, created_by, expires_at) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6, $7::uuid, \
                 CASE WHEN $8::bigint IS NULL THEN NULL ELSE now() + make_interval(secs => $8::bigint) END)",
    )
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(share_token_hash(&secret).to_vec())
    .bind(role.as_str())
    .bind(label)
    .bind(created_by.to_string())
    .bind(lifetime)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(CreatedShareLink { id, secret })
}

type LiveRow = (String, String, String, String, String, Option<i64>, i64);

/// The live link `secret` names, with its expiry and the session end, or `Refused`.
async fn live_link(pool: &PgPool, secret: &str) -> Result<ShareGrant, ShareLinkError> {
    if !well_formed(secret) {
        return Err(ShareLinkError::Refused);
    }
    let max_session = i64::try_from(MAX_SHARE_SESSION_SECS).map_err(|_| ShareLinkError::InvalidInput)?;
    let row: Option<LiveRow> = sqlx::query_as(
        "SELECT s.id::text, s.tenant_id::text, s.document_id::text, s.role, s.created_by::text, \
                floor(extract(epoch FROM s.expires_at))::bigint, \
                floor(extract(epoch FROM LEAST(COALESCE(s.expires_at, 'infinity'::timestamptz), now() + make_interval(secs => $2::bigint))))::bigint \
         FROM share_links s JOIN documents d ON d.tenant_id = s.tenant_id AND d.id = s.document_id JOIN tenants t ON t.id = s.tenant_id \
         WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND (s.expires_at IS NULL OR s.expires_at > now()) \
           AND d.state = 'active' AND t.state = 'active'",
    )
    .bind(share_token_hash(secret).to_vec())
    .bind(max_session)
    .fetch_optional(pool)
    .await
    .map_err(|_| ShareLinkError::Database)?;
    let (id, tenant_id, document_id, role, created_by, link_expires_at, session_expires_at) = row.ok_or(ShareLinkError::Refused)?;
    Ok(ShareGrant {
        link_id: id.parse().map_err(|_| ShareLinkError::Database)?,
        tenant_id: tenant_id.parse().map_err(|_| ShareLinkError::Database)?,
        document_id: document_id.parse().map_err(|_| ShareLinkError::Database)?,
        role: ShareRole::parse(&role).ok_or(ShareLinkError::Database)?,
        created_by: created_by.parse().map_err(|_| ShareLinkError::Database)?,
        link_expires_at,
        session_expires_at,
    })
}

/// What `secret` opens: a link that exists, is neither revoked nor expired, on a document and in a
/// tenant that are both active. Anything else is [`ShareLinkError::Refused`].
pub async fn resolve_share_link(pool: &PgPool, secret: &str) -> Result<ResolvedShareLink, ShareLinkError> {
    let grant = live_link(pool, secret).await?;
    Ok(ResolvedShareLink {
        id: grant.link_id,
        tenant_id: grant.tenant_id,
        document_id: grant.document_id,
        role: grant.role,
        created_by: grant.created_by,
    })
}

/// Exchange a presented share secret for a [`ShareGrant`] (S4-005): the API mints a share session
/// that ends at [`ShareGrant::session_expires_at`] and re-checks the link (by
/// [`ShareGrant::link_id`], with [`dead_share_links`]) whenever it renews or acts. Refuses exactly
/// what [`resolve_share_link`] refuses, alike.
pub async fn exchange_share_token(pool: &PgPool, secret: &str) -> Result<ShareGrant, ShareLinkError> {
    live_link(pool, secret).await
}

type RecordRow = (String, String, String, Option<String>, String, i64, Option<i64>, Option<i64>);

fn record(row: RecordRow) -> Result<ShareLinkRecord, ShareLinkError> {
    let (id, document_id, role, label, created_by, created_at, expires_at, revoked_at) = row;
    Ok(ShareLinkRecord {
        id: id.parse().map_err(|_| ShareLinkError::Database)?,
        document_id: document_id.parse().map_err(|_| ShareLinkError::Database)?,
        role: ShareRole::parse(&role).ok_or(ShareLinkError::Database)?,
        label,
        created_by: created_by.parse().map_err(|_| ShareLinkError::Database)?,
        created_at,
        expires_at,
        revoked_at,
    })
}

/// `document_id`'s links in the transaction's tenant, newest first, at most [`MAX_LISTED_LINKS`]
/// (revoked and expired ones included, so managers can see what was shared).
pub async fn list_share_links(transaction: &mut TenantTransaction<'_>, document_id: CloudDocumentId) -> Result<Vec<ShareLinkRecord>, ShareLinkError> {
    let rows: Vec<RecordRow> = sqlx::query_as(
        "SELECT id::text, document_id::text, role, label, created_by::text, floor(extract(epoch FROM created_at))::bigint, \
                floor(extract(epoch FROM expires_at))::bigint, floor(extract(epoch FROM revoked_at))::bigint \
         FROM share_links WHERE tenant_id = $1::uuid AND document_id = $2::uuid \
         ORDER BY created_at DESC, id LIMIT $3",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(MAX_LISTED_LINKS)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    rows.into_iter().map(record).collect()
}

/// Change live link `id`'s label or expiry in the transaction's tenant: the link as it is now, or
/// `None` when there is no such live link (revoked links can't be changed). The caller commits.
pub async fn update_share_link(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    update: &ShareLinkUpdate<'_>,
) -> Result<Option<ShareLinkRecord>, ShareLinkError> {
    if let Some(Some(label)) = update.label
        && (label.is_empty() || label.chars().count() > MAX_LABEL_CHARS)
    {
        return Err(ShareLinkError::InvalidInput);
    }
    let lifetime = match update.expires_in_secs {
        Some(Some(secs)) if secs == 0 || secs > MAX_LIFETIME_SECS => return Err(ShareLinkError::InvalidInput),
        Some(Some(secs)) => Some(i64::try_from(secs).map_err(|_| ShareLinkError::InvalidInput)?),
        _ => None,
    };
    let row: Option<RecordRow> = sqlx::query_as(
        "UPDATE share_links SET \
             label = CASE WHEN $3 THEN $4 ELSE label END, \
             expires_at = CASE WHEN NOT $5 THEN expires_at \
                               WHEN $6::bigint IS NULL THEN NULL \
                               ELSE now() + make_interval(secs => $6::bigint) END \
         WHERE tenant_id = $1::uuid AND id = $2::uuid AND revoked_at IS NULL \
         RETURNING id::text, document_id::text, role, label, created_by::text, floor(extract(epoch FROM created_at))::bigint, \
                   floor(extract(epoch FROM expires_at))::bigint, floor(extract(epoch FROM revoked_at))::bigint",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(update.label.is_some())
    .bind(update.label.flatten())
    .bind(update.expires_in_secs.is_some())
    .bind(lifetime)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    row.map(record).transpose()
}

/// Of `links` (`(tenant, link id)` pairs, at most [`MAX_CHECKED_LINKS`]), those that no longer
/// open anything: revoked, expired, deleted, not in the tenant given, or on a document or in a tenant that is not
/// active. Like resolution it reads before any one tenant is chosen (the relay checks the links of
/// every open document at once).
pub async fn dead_share_links(pool: &PgPool, links: &[(TenantId, Uuid)]) -> Result<Vec<(TenantId, Uuid)>, ShareLinkError> {
    if links.is_empty() {
        return Ok(Vec::new());
    }
    if links.len() > MAX_CHECKED_LINKS {
        return Err(ShareLinkError::InvalidInput);
    }
    let tenants: Vec<String> = links.iter().map(|(tenant, _)| tenant.to_string()).collect();
    let ids: Vec<String> = links.iter().map(|(_, id)| id.to_string()).collect();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT x.tenant_id::text, x.id::text FROM unnest($1::text[]::uuid[], $2::text[]::uuid[]) AS x (tenant_id, id) \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM share_links s JOIN documents d ON d.tenant_id = s.tenant_id AND d.id = s.document_id JOIN tenants t ON t.id = s.tenant_id \
             WHERE s.tenant_id = x.tenant_id AND s.id = x.id AND s.revoked_at IS NULL \
               AND (s.expires_at IS NULL OR s.expires_at > now()) AND d.state = 'active' AND t.state = 'active')",
    )
    .bind(tenants)
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(|_| ShareLinkError::Database)?;
    rows.into_iter()
        .map(|(tenant, id)| Ok((tenant.parse().map_err(|_| ShareLinkError::Database)?, id.parse().map_err(|_| ShareLinkError::Database)?)))
        .collect()
}

/// Revoke link `id` in the transaction's tenant: true when it was live and is now revoked, false
/// when there is no such live link. The caller commits.
pub async fn revoke_share_link(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<bool, ShareLinkError> {
    let result = sqlx::query("UPDATE share_links SET revoked_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid AND revoked_at IS NULL")
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_43_url_safe_characters_and_distinct() {
        let (a, b) = (new_secret().unwrap(), new_secret().unwrap());
        assert_eq!(a.len(), SECRET_CHARS);
        assert!(well_formed(&a) && well_formed(&b));
        assert_ne!(a, b);
        assert_ne!(share_token_hash(&a), share_token_hash(&b));
    }

    #[test]
    fn malformed_secrets_are_not_well_formed() {
        assert!(!well_formed(""));
        assert!(!well_formed(&"a".repeat(42)));
        assert!(!well_formed(&"a".repeat(44)));
        assert!(!well_formed(&format!("{}+", "a".repeat(42))));
        assert!(!well_formed(&format!("{}é", "a".repeat(41))));
    }

    #[test]
    fn roles_round_trip() {
        for role in [ShareRole::Editor, ShareRole::Viewer] {
            assert_eq!(ShareRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(ShareRole::parse("owner"), None);
    }

    #[test]
    fn created_links_never_print_their_secret() {
        let link = CreatedShareLink { id: Uuid::nil(), secret: "top-secret".into() };
        assert!(!format!("{link:?}").contains("top-secret"));
    }

    #[test]
    fn records_are_live_until_revoked_or_expired() {
        let mut link = ShareLinkRecord {
            id: Uuid::nil(),
            document_id: CloudDocumentId::random(),
            role: ShareRole::Viewer,
            label: None,
            created_by: PrincipalId::random(),
            created_at: 100,
            expires_at: None,
            revoked_at: None,
        };
        assert!(link.live_at(1_000));
        link.expires_at = Some(500);
        assert!(link.live_at(499) && !link.live_at(500));
        link.expires_at = None;
        link.revoked_at = Some(200);
        assert!(!link.live_at(150));
    }
}
