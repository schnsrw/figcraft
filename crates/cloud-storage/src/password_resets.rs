//! Single-use password links (S6-037): `reset` (an administrator helps someone who lost their
//! password; at most one hour) and `setup` (an administrator created the account; the person picks
//! their first password; at most seven days).
//!
//! The token is 32 random bytes (43 URL-safe characters) shown **once** to the administrator; only its
//! SHA-256 is stored. Issuing a link revokes the person's older unused ones. [`consume`] works from
//! the pool: the token names its tenant through a SECURITY DEFINER lookup (as invitations do), then
//! one tenant transaction locks the link row (`FOR UPDATE`, predicate re-evaluated after any wait, so
//! of any number of concurrent uses exactly one wins), checks the account is enabled and still a
//! member, stores the new hash, marks the link used and audits, atomically. Every problem with a
//! token (unknown, malformed, expired, revoked, used, disabled account) is the same
//! [`ResetError::Refused`].

use base64::Engine as _;
use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use sqlx::PgPool;
use std::fmt;

use crate::credentials::{CredentialState, PasswordChange, find_for_principal, set_password};
use crate::sessions::{random_token, session_token_hash, well_formed_token};
use crate::tenants::audit_success;
use crate::{TenantTransaction, begin_tenant_transaction};

/// Longest lifetime of a `reset` link.
pub const MAX_RESET_SECS: i64 = 3600;
/// Longest lifetime of a `setup` link.
pub const MAX_SETUP_SECS: i64 = 7 * 24 * 3600;
/// Default lifetime of a `setup` link.
pub const DEFAULT_SETUP_SECS: i64 = 24 * 3600;

/// Fixed, non-leaking categories for reset failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetError {
    /// The lifetime is out of range.
    InvalidLifetime,
    /// The principal has no password account in this tenant.
    NoAccount,
    Random,
    /// The token opens nothing.
    Refused,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for ResetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "password reset error: {self:?}")
    }
}

impl std::error::Error for ResetError {}

/// Why a link exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetPurpose {
    Reset,
    Setup,
}

impl ResetPurpose {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Setup => "setup",
        }
    }
}

/// A link just issued: the token is never readable again.
#[derive(Clone, Eq, PartialEq)]
pub struct IssuedReset {
    pub id: Uuid,
    pub token: String,
    pub expires_in_secs: i64,
}

impl fmt::Debug for IssuedReset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedReset")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// Issue a link for `principal_id`'s password account in the transaction's tenant, revoking their
/// older unused links. `lifetime_secs` defaults to the purpose's maximum for `reset` and one day for
/// `setup`, and may not exceed that purpose's maximum. Audited as `credential.reset_issue`.
pub async fn issue(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    purpose: ResetPurpose,
    lifetime_secs: Option<i64>,
    actor: PrincipalId,
) -> Result<IssuedReset, ResetError> {
    let (default, max) = match purpose {
        ResetPurpose::Reset => (MAX_RESET_SECS, MAX_RESET_SECS),
        ResetPurpose::Setup => (DEFAULT_SETUP_SECS, MAX_SETUP_SECS),
    };
    let lifetime = lifetime_secs.unwrap_or(default);
    if !(60..=max).contains(&lifetime) {
        return Err(ResetError::InvalidLifetime);
    }
    if find_for_principal(transaction, principal_id).await.map_err(|_| ResetError::Database)?.is_none() {
        return Err(ResetError::NoAccount);
    }
    let token = random_token().ok_or(ResetError::Random)?;
    let id = Uuid::new_v4();
    sqlx::query(
        "UPDATE password_resets SET revoked_at = now() WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|_| ResetError::Database)?;
    sqlx::query(concat!(
        "INSERT INTO password_resets (tenant_id, id, principal_id, purpose, token_hash, created_by, created_at, expires_at) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6::uuid, now(), now() + make_interval(secs => $7::bigint))"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(principal_id.to_string())
    .bind(purpose.as_str())
    .bind(session_token_hash(&token).to_vec())
    .bind(actor.to_string())
    .bind(lifetime)
    .execute(transaction.connection())
    .await
    .map_err(|_| ResetError::Database)?;
    audit_success(
        transaction,
        Some(actor),
        "credential.reset_issue",
        "principal",
        Some(principal_id.as_uuid()),
        &serde_json::json!({ "purpose": purpose.as_str(), "expires_in_secs": lifetime }),
    )
    .await
    .map_err(|_| ResetError::Audit)?;
    Ok(IssuedReset { id, token, expires_in_secs: lifetime })
}

/// Revoke the person's live links (without issuing another). Returns how many.
pub async fn revoke_links(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<u64, ResetError> {
    let result = sqlx::query(
        "UPDATE password_resets SET revoked_at = now() WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|_| ResetError::Database)?;
    Ok(result.rows_affected())
}

/// Who a link was used for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Consumed {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub purpose: ResetPurpose,
}

/// Use `token` to set `new_hash` (a PHC string the caller made from a policy-checked password).
/// Single use; see the module docs. The caller then ends the person's sessions.
pub async fn consume(pool: &PgPool, token: &str, new_hash: &str) -> Result<Consumed, ResetError> {
    if !well_formed_token(token) {
        return Err(ResetError::Refused);
    }
    let hash = session_token_hash(token).to_vec();
    let tenant: Option<String> = sqlx::query_scalar("SELECT tenant FROM figcraft_reset_token_tenant($1) AS tenant WHERE tenant IS NOT NULL")
        .bind(&hash)
        .fetch_optional(pool)
        .await
        .map_err(|_| ResetError::Database)?;
    let tenant_id: TenantId = tenant.ok_or(ResetError::Refused)?.parse().map_err(|_| ResetError::CorruptRow)?;
    let mut transaction = begin_tenant_transaction(pool, tenant_id).await.map_err(|_| ResetError::Database)?;
    let row: Option<(String, String, String)> = sqlx::query_as(concat!(
        "SELECT id::text, principal_id::text, purpose FROM password_resets WHERE tenant_id = $1::uuid AND token_hash = $2 ",
        "AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now() FOR UPDATE"
    ))
    .bind(tenant_id.to_string())
    .bind(&hash)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| ResetError::Database)?;
    let (id, principal, purpose) = row.ok_or(ResetError::Refused)?;
    let principal_id: PrincipalId = principal.parse().map_err(|_| ResetError::CorruptRow)?;
    let purpose = match purpose.as_str() {
        "reset" => ResetPurpose::Reset,
        "setup" => ResetPurpose::Setup,
        _ => return Err(ResetError::CorruptRow),
    };
    // The account must still be enabled and the person still a member: a disabled account is not
    // revived by an old link.
    let account = find_for_principal(&mut transaction, principal_id).await.map_err(|_| ResetError::Database)?;
    let member_active: Option<bool> =
        sqlx::query_scalar("SELECT state = 'active' FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
            .bind(tenant_id.to_string())
            .bind(principal_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|_| ResetError::Database)?;
    if account.is_none_or(|account| account.state != CredentialState::Active) || member_active != Some(true) {
        return Err(ResetError::Refused);
    }
    set_password(&mut transaction, principal_id, new_hash, PasswordChange::Reset, Some(principal_id)).await.map_err(|_| ResetError::Database)?;
    sqlx::query("UPDATE password_resets SET used_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant_id.to_string())
        .bind(&id)
        .execute(transaction.connection())
        .await
        .map_err(|_| ResetError::Database)?;
    audit_success(
        &mut transaction,
        Some(principal_id),
        "credential.reset_used",
        "principal",
        Some(principal_id.as_uuid()),
        &serde_json::json!({ "purpose": purpose.as_str() }),
    )
    .await
    .map_err(|_| ResetError::Audit)?;
    transaction.commit().await.map_err(|_| ResetError::Database)?;
    Ok(Consumed { tenant_id, principal_id, purpose })
}

/// URL-safe base64 of random bytes (kept for callers that need an unguessable placeholder secret).
#[must_use]
pub fn random_secret() -> Option<String> {
    let token = random_token()?;
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_links_never_print_their_token() {
        let issued = IssuedReset { id: Uuid::nil(), token: "top-secret".into(), expires_in_secs: 60 };
        assert!(!format!("{issued:?}").contains("top-secret"));
    }

    #[test]
    fn placeholder_secrets_are_long_and_distinct() {
        let (a, b) = (random_secret().expect("secret"), random_secret().expect("secret"));
        assert!(a.len() >= 43 && a != b);
    }
}
