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
//! creating and revoking are tenant-scoped.

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
    /// document that is not active). Deliberately one category.
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

/// What `secret` opens: a link that exists, is neither revoked nor expired, on a document that is
/// active. Anything else is [`ShareLinkError::Refused`].
pub async fn resolve_share_link(pool: &PgPool, secret: &str) -> Result<ResolvedShareLink, ShareLinkError> {
    if !well_formed(secret) {
        return Err(ShareLinkError::Refused);
    }
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT s.id::text, s.tenant_id::text, s.document_id::text, s.role, s.created_by::text \
         FROM share_links s JOIN documents d ON d.tenant_id = s.tenant_id AND d.id = s.document_id \
         WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND (s.expires_at IS NULL OR s.expires_at > now()) \
           AND d.state = 'active'",
    )
    .bind(share_token_hash(secret).to_vec())
    .fetch_optional(pool)
    .await
    .map_err(|_| ShareLinkError::Database)?;
    let (id, tenant_id, document_id, role, created_by) = row.ok_or(ShareLinkError::Refused)?;
    Ok(ResolvedShareLink {
        id: id.parse().map_err(|_| ShareLinkError::Database)?,
        tenant_id: tenant_id.parse().map_err(|_| ShareLinkError::Database)?,
        document_id: document_id.parse().map_err(|_| ShareLinkError::Database)?,
        role: ShareRole::parse(&role).ok_or(ShareLinkError::Database)?,
        created_by: created_by.parse().map_err(|_| ShareLinkError::Database)?,
    })
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
}
