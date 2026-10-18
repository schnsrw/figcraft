//! Per-workspace policy settings (S6-056).
//!
//! [`load`] returns the workspace's [`WorkspacePolicy`]: who may invite, guest access and the
//! share-link policy come from `tenants.policy`; the allowed sign-in methods come from the
//! account settings (`tenant_account_settings`, S6-034), which the sign-in code already enforces,
//! so there is one source of truth for each field. [`save`] validates, writes both, audits the
//! change and, when the share-link policy gets stricter, revokes the links it no longer allows (so
//! the relay and the share exchange, which only honour live links, stop honouring them at once).
//! The caller authorizes (owner and administrators) and commits.

use figcraft_cloud_domain::PrincipalId;
use figcraft_cloud_domain::policy::{InvitePolicy, ShareLinkPolicy, SignInMethod, WorkspacePolicy};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::TenantTransaction;
use crate::account_settings::{self, SettingsError};
use crate::tenants::audit_success;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyError {
    /// The policy is not valid (no sign-in method, a duplicate method, or open registration that
    /// needs passwords while passwords are off).
    Invalid,
    NotFound,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "workspace policy error: {self:?}")
    }
}

impl std::error::Error for PolicyError {}

/// What `tenants.policy` holds. Unknown or missing fields fall back to the defaults.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
struct Stored {
    who_may_invite: InvitePolicy,
    guest_access: bool,
    share_links: ShareLinkPolicy,
}

impl Default for Stored {
    fn default() -> Self {
        let defaults = WorkspacePolicy::default();
        Self { who_may_invite: defaults.who_may_invite, guest_access: defaults.guest_access, share_links: defaults.share_links }
    }
}

fn methods(settings: account_settings::AccountSettings) -> Vec<SignInMethod> {
    let mut out = Vec::new();
    if settings.password_enabled {
        out.push(SignInMethod::Password);
    }
    if settings.oidc_enabled {
        out.push(SignInMethod::Oidc);
    }
    out
}

fn settings_error(error: SettingsError) -> PolicyError {
    match error {
        SettingsError::InvalidCombination => PolicyError::Invalid,
        SettingsError::Audit => PolicyError::Audit,
        SettingsError::Database => PolicyError::Database,
    }
}

/// The policy of the transaction's workspace.
pub async fn load(transaction: &mut TenantTransaction<'_>) -> Result<WorkspacePolicy, PolicyError> {
    let stored: Option<String> = sqlx::query_scalar("SELECT policy::text FROM tenants WHERE id = $1::uuid")
        .bind(transaction.tenant_id().to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| PolicyError::Database)?;
    let stored: Stored = serde_json::from_str(&stored.ok_or(PolicyError::NotFound)?).map_err(|_| PolicyError::CorruptRow)?;
    let settings = account_settings::get_settings(transaction).await.map_err(settings_error)?;
    Ok(WorkspacePolicy {
        allowed_sign_in_methods: methods(settings),
        who_may_invite: stored.who_may_invite,
        guest_access: stored.guest_access,
        share_links: stored.share_links,
    })
}

/// Replace the policy. Returns how many share links the new policy revoked.
pub async fn save(transaction: &mut TenantTransaction<'_>, actor: PrincipalId, policy: &WorkspacePolicy) -> Result<u64, PolicyError> {
    if !policy.is_valid() {
        return Err(PolicyError::Invalid);
    }
    let before = load(transaction).await?;
    // Sign-in methods go through the account settings so their own rules and audit apply; the
    // other account settings (open registration, second factor) are kept as they are.
    let mut settings = account_settings::get_settings(transaction).await.map_err(settings_error)?;
    settings.password_enabled = policy.allows_sign_in(SignInMethod::Password);
    settings.oidc_enabled = policy.allows_sign_in(SignInMethod::Oidc);
    if !settings.password_enabled {
        // Passwords off closes open registration with them.
        settings.open_registration = false;
    }
    account_settings::set_settings(transaction, settings, actor).await.map_err(settings_error)?;

    let stored =
        serde_json::to_string(&Stored { who_may_invite: policy.who_may_invite, guest_access: policy.guest_access, share_links: policy.share_links })
            .map_err(|_| PolicyError::CorruptRow)?;
    let updated = sqlx::query("UPDATE tenants SET policy = $2::jsonb, updated_at = now() WHERE id = $1::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(stored)
        .execute(transaction.connection())
        .await
        .map_err(|_| PolicyError::Database)?;
    if updated.rows_affected() == 0 {
        return Err(PolicyError::NotFound);
    }

    // A stricter share-link policy ends the links it no longer allows.
    let revoked = match policy.share_links {
        ShareLinkPolicy::ViewAndEdit => 0,
        ShareLinkPolicy::ViewOnly => revoke_links(transaction, true).await?,
        ShareLinkPolicy::Disabled => revoke_links(transaction, false).await?,
    };
    let metadata = serde_json::json!({
        "before": { "who_may_invite": before.who_may_invite, "guest_access": before.guest_access, "share_links": before.share_links },
        "after": { "who_may_invite": policy.who_may_invite, "guest_access": policy.guest_access, "share_links": policy.share_links },
        "share_links_revoked": revoked,
    });
    audit_success(transaction, Some(actor), "tenant.policy_update", "tenant", Some(transaction.tenant_id().as_uuid()), &metadata)
        .await
        .map_err(|_| PolicyError::Audit)?;
    Ok(revoked)
}

async fn revoke_links(transaction: &mut TenantTransaction<'_>, only_editor_links: bool) -> Result<u64, PolicyError> {
    let done = if only_editor_links {
        sqlx::query("UPDATE share_links SET revoked_at = now() WHERE tenant_id = $1::uuid AND revoked_at IS NULL AND role = 'editor'")
    } else {
        sqlx::query("UPDATE share_links SET revoked_at = now() WHERE tenant_id = $1::uuid AND revoked_at IS NULL")
    }
    .bind(transaction.tenant_id().to_string())
    .execute(transaction.connection())
    .await
    .map_err(|_| PolicyError::Database)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_policy_falls_back_to_defaults_field_by_field() {
        let empty: Stored = serde_json::from_str("{}").expect("empty");
        let defaults = WorkspacePolicy::default();
        assert_eq!(
            (empty.who_may_invite, empty.guest_access, empty.share_links),
            (defaults.who_may_invite, defaults.guest_access, defaults.share_links)
        );
        let partial: Stored = serde_json::from_str(r#"{"guest_access": false}"#).expect("partial");
        assert!(!partial.guest_access);
        assert_eq!(partial.share_links, defaults.share_links);
        assert!(serde_json::from_str::<Stored>(r#"{"who_may_invite": "nobody"}"#).is_err());
    }

    #[test]
    fn account_settings_map_to_sign_in_methods() {
        let on = account_settings::AccountSettings { password_enabled: true, oidc_enabled: false, open_registration: false, require_totp: false };
        assert_eq!(methods(on), vec![SignInMethod::Password]);
        let both = account_settings::AccountSettings { oidc_enabled: true, ..on };
        assert_eq!(methods(both), vec![SignInMethod::Password, SignInMethod::Oidc]);
        let oidc_only = account_settings::AccountSettings { password_enabled: false, ..both };
        assert_eq!(methods(oidc_only), vec![SignInMethod::Oidc]);
    }
}
