//! Creating built-in accounts (S6-034): a person (principal) with a password credential and the
//! membership it grants, in one transaction.
//!
//! Three doors, all atomic (a failure leaves no principal, credential or membership behind):
//!
//! * [`register_with_invitation`]: the holder of an invitation token signs up. The login is the
//!   invited address (so the account is the person who was invited and nobody can squat it), the
//!   token is locked first so of two concurrent sign-ups exactly one wins, and the invitation's
//!   team membership and a `viewer` tenant membership are created by the same code that accepts an
//!   invitation for an existing principal. Needs password sign-in enabled for the tenant.
//! * [`register_open`]: only when the tenant turned `open_registration` on (default off); a
//!   `viewer` of the tenant with no team.
//! * [`create_account`]: the building block for bootstrap and administrator-created users; the
//!   caller adds the membership.
//!
//! Display names are 1 to 64 characters (NFC, no control characters), which is tighter than the
//! 255 the principal row allows, because they are shown on cursors and in history.

use figcraft_cloud_domain::{PrincipalId, TenantId};
use sqlx::PgPool;
use std::fmt;
use unicode_normalization::UnicodeNormalization;

use crate::account_settings::get_settings;
use crate::credentials::{CredentialError, create_credential, normalize_login};
use crate::invitations::{AcceptedInvitation, InvitationError, accept_in, invitation_token_hash, well_formed};
use crate::tenants::audit_success;
use crate::{TenantTransaction, begin_tenant_transaction};

/// Longest display name, in characters.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// Fixed, non-leaking categories for account creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountError {
    InvalidName,
    InvalidLogin,
    InvalidHash,
    /// The invitation token opens nothing (unknown, expired, revoked, used, or the wrong tenant).
    Refused,
    /// Password sign-up is not allowed here (method off, or open registration off).
    Closed,
    /// The login is already in use in this tenant.
    LoginTaken,
    TenantInactive,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for AccountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "account error: {self:?}")
    }
}

impl std::error::Error for AccountError {}

fn credential_error(error: CredentialError) -> AccountError {
    match error {
        CredentialError::InvalidLogin => AccountError::InvalidLogin,
        CredentialError::InvalidHash => AccountError::InvalidHash,
        CredentialError::Conflict => AccountError::LoginTaken,
        CredentialError::Audit => AccountError::Audit,
        CredentialError::CorruptRow => AccountError::CorruptRow,
        CredentialError::InvalidPolicy | CredentialError::UnknownPrincipal | CredentialError::NotFound | CredentialError::Database => {
            AccountError::Database
        }
    }
}

/// A display name for storage: trimmed, NFC, 1 to [`MAX_DISPLAY_NAME_CHARS`] characters, no control
/// characters.
#[must_use]
pub fn normalize_display_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_DISPLAY_NAME_CHARS * 8 {
        return None;
    }
    let normalized: String = trimmed.nfc().collect();
    let chars = normalized.chars().count();
    ((1..=MAX_DISPLAY_NAME_CHARS).contains(&chars) && !normalized.chars().any(char::is_control)).then_some(normalized)
}

/// A new active `user` principal named `display_name`.
async fn create_principal(transaction: &mut TenantTransaction<'_>, display_name: &str) -> Result<PrincipalId, AccountError> {
    let name = normalize_display_name(display_name).ok_or(AccountError::InvalidName)?;
    let id = PrincipalId::random();
    sqlx::query("INSERT INTO principals (id, kind, display_name, state) VALUES ($1::uuid, 'user', $2, 'active')")
        .bind(id.to_string())
        .bind(&name)
        .execute(transaction.connection())
        .await
        .map_err(|_| AccountError::Database)?;
    Ok(id)
}

/// A new person with a password credential: the principal and the credential, nothing else.
/// `login` is normalised; `password_hash` is a PHC string. The caller grants a membership, audits
/// the larger action and commits. Without an `actor` the new person is recorded as the actor
/// (they created their own credential).
pub async fn create_account(
    transaction: &mut TenantTransaction<'_>,
    display_name: &str,
    login: &str,
    password_hash: &str,
    actor: Option<PrincipalId>,
) -> Result<PrincipalId, AccountError> {
    if normalize_login(login).is_none() {
        return Err(AccountError::InvalidLogin);
    }
    let principal = create_principal(transaction, display_name).await?;
    create_credential(transaction, principal, login, password_hash, actor.or(Some(principal))).await.map_err(credential_error)?;
    Ok(principal)
}

async fn require_active_tenant(transaction: &mut TenantTransaction<'_>) -> Result<(), AccountError> {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM tenants WHERE id = $1::uuid FOR SHARE")
        .bind(transaction.tenant_id().to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| AccountError::Database)?;
    if state.as_deref() == Some("active") { Ok(()) } else { Err(AccountError::TenantInactive) }
}

/// A person who signed up through an invitation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvitedAccount {
    pub principal_id: PrincipalId,
    pub tenant_id: TenantId,
    pub accepted: AcceptedInvitation,
}

/// Sign up with the invitation `token`: create the person, their credential (the `login` they chose,
/// or the invited address when the invitation noted one and none was given) and the memberships the invitation grants, atomically. Every problem with the token is
/// [`AccountError::Refused`]; a tenant that disallows passwords is [`AccountError::Closed`].
pub async fn register_with_invitation(
    pool: &PgPool,
    token: &str,
    login: Option<&str>,
    display_name: &str,
    password_hash: &str,
) -> Result<InvitedAccount, AccountError> {
    if !well_formed(token) {
        return Err(AccountError::Refused);
    }
    if normalize_display_name(display_name).is_none() {
        return Err(AccountError::InvalidName);
    }
    let hash = invitation_token_hash(token).to_vec();
    let tenant: Option<String> = sqlx::query_scalar("SELECT tenant FROM figcraft_invitation_tenant($1) AS tenant WHERE tenant IS NOT NULL")
        .bind(&hash)
        .fetch_optional(pool)
        .await
        .map_err(|_| AccountError::Database)?;
    let tenant_id: TenantId = tenant.ok_or(AccountError::Refused)?.parse().map_err(|_| AccountError::CorruptRow)?;
    let mut transaction = begin_tenant_transaction(pool, tenant_id).await.map_err(|_| AccountError::Database)?;
    // Lock the pending invitation first: concurrent sign-ups with one token queue here and every
    // later one finds it accepted.
    let email: Option<Option<String>> = sqlx::query_scalar(concat!(
        "SELECT email FROM team_invitations WHERE tenant_id = $1::uuid AND token_hash = $2 ",
        "AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now() FOR UPDATE"
    ))
    .bind(tenant_id.to_string())
    .bind(&hash)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| AccountError::Database)?;
    let email = email.ok_or(AccountError::Refused)?;
    // The person chooses their login; without one an invitation that noted an address uses it.
    let login = match (login, email.as_deref()) {
        (Some(login), _) => login.to_string(),
        (None, Some(email)) => email.to_string(),
        (None, None) => return Err(AccountError::InvalidLogin),
    };
    require_active_tenant(&mut transaction).await?;
    if !get_settings(&mut transaction).await.map_err(|_| AccountError::Database)?.password_enabled {
        return Err(AccountError::Closed);
    }
    let principal = create_account(&mut transaction, display_name, &login, password_hash, None).await?;
    let accepted = accept_in(&mut transaction, &hash, principal, None).await.map_err(|error| match error {
        InvitationError::Refused => AccountError::Refused,
        InvitationError::TenantInactive => AccountError::TenantInactive,
        InvitationError::TeamUnavailable => AccountError::Refused,
        InvitationError::Audit => AccountError::Audit,
        InvitationError::CorruptRow => AccountError::CorruptRow,
        _ => AccountError::Database,
    })?;
    audit_success(
        &mut transaction,
        Some(principal),
        "account.register",
        "principal",
        Some(principal.as_uuid()),
        &serde_json::json!({ "via": "invitation", "invitation_id": accepted.id.to_string() }),
    )
    .await
    .map_err(|_| AccountError::Audit)?;
    transaction.commit().await.map_err(|_| AccountError::Database)?;
    Ok(InvitedAccount { principal_id: principal, tenant_id, accepted })
}

/// Open registration: create a `viewer` account in the transaction's tenant, only when the tenant
/// enabled `open_registration` (and passwords). The caller commits.
pub async fn register_open(
    transaction: &mut TenantTransaction<'_>,
    display_name: &str,
    login: &str,
    password_hash: &str,
) -> Result<PrincipalId, AccountError> {
    require_active_tenant(transaction).await?;
    let settings = get_settings(transaction).await.map_err(|_| AccountError::Database)?;
    if !settings.open_registration || !settings.password_enabled {
        return Err(AccountError::Closed);
    }
    let principal = create_account(transaction, display_name, login, password_hash, None).await?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, 'viewer')")
        .bind(transaction.tenant_id().to_string())
        .bind(principal.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|_| AccountError::Database)?;
    audit_success(
        transaction,
        Some(principal),
        "account.register",
        "principal",
        Some(principal.as_uuid()),
        &serde_json::json!({ "via": "open_registration" }),
    )
    .await
    .map_err(|_| AccountError::Audit)?;
    Ok(principal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_normalised_and_bounded() {
        assert_eq!(normalize_display_name("  Ada  Lovelace ").as_deref(), Some("Ada  Lovelace"));
        assert_eq!(normalize_display_name("Jose\u{301}"), Some("Jos\u{e9}".to_string()));
        for bad in ["", "   ", "tab\there", "nul\u{0}", &"x".repeat(65)] {
            assert_eq!(normalize_display_name(bad), None, "{bad:?}");
        }
        assert!(normalize_display_name(&"x".repeat(64)).is_some());
        assert_eq!(normalize_display_name(&"x".repeat(100_000)), None);
    }
}
