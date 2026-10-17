//! Per-tenant account settings (S6-034): which sign-in methods the tenant allows, whether open
//! registration is on, and whether a second factor is required.
//!
//! No row means the defaults: both methods allowed, open registration **off**, no second factor
//! required. The settings are tenant data (forced row-level security); changing them is a tenant
//! administration action authorised by the caller (`tenant.update_policy`) and audited here as
//! `tenant.account_settings`.
//!
//! The pre-tenant slug lookup ([`tenant_by_slug`]) serves the sign-in form of a multi-tenant
//! deployment; it returns the id of an *active* tenant only.

use figcraft_cloud_domain::{PrincipalId, TenantId};
use sqlx::PgPool;
use std::fmt;

use crate::TenantTransaction;
use crate::tenants::audit_success;

/// Fixed, non-leaking categories for settings failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsError {
    /// At least one sign-in method must stay on, and open registration needs passwords on.
    InvalidCombination,
    Audit,
    Database,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "account settings error: {self:?}")
    }
}

impl std::error::Error for SettingsError {}

/// The tenant's account settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountSettings {
    /// Password sign-in and password sign-up are allowed.
    pub password_enabled: bool,
    /// OIDC sign-in is allowed.
    pub oidc_enabled: bool,
    /// Anyone who can reach the workspace may create a password account (never default).
    pub open_registration: bool,
    /// Password sign-in needs a second factor (S6-038).
    pub require_totp: bool,
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self { password_enabled: true, oidc_enabled: true, open_registration: false, require_totp: false }
    }
}

impl AccountSettings {
    /// True when at least one method is on and open registration implies passwords.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        (self.password_enabled || self.oidc_enabled) && (!self.open_registration || self.password_enabled)
    }
}

/// The settings of the transaction's tenant (the defaults when none were saved).
pub async fn get_settings(transaction: &mut TenantTransaction<'_>) -> Result<AccountSettings, SettingsError> {
    let row: Option<(bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT password_enabled, oidc_enabled, open_registration, require_totp FROM tenant_account_settings WHERE tenant_id = $1::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| SettingsError::Database)?;
    Ok(row.map_or_else(AccountSettings::default, |(password_enabled, oidc_enabled, open_registration, require_totp)| AccountSettings {
        password_enabled,
        oidc_enabled,
        open_registration,
        require_totp,
    }))
}

/// Replace the transaction's tenant's settings and audit the change (the previous and new values,
/// never anything secret). The caller authorises and commits.
pub async fn set_settings(transaction: &mut TenantTransaction<'_>, settings: AccountSettings, actor: PrincipalId) -> Result<(), SettingsError> {
    if !settings.is_valid() {
        return Err(SettingsError::InvalidCombination);
    }
    let before = get_settings(transaction).await?;
    sqlx::query(concat!(
        "INSERT INTO tenant_account_settings (tenant_id, password_enabled, oidc_enabled, open_registration, require_totp, updated_by) ",
        "VALUES ($1::uuid, $2, $3, $4, $5, $6::uuid) ON CONFLICT (tenant_id) DO UPDATE SET ",
        "password_enabled = EXCLUDED.password_enabled, oidc_enabled = EXCLUDED.oidc_enabled, ",
        "open_registration = EXCLUDED.open_registration, require_totp = EXCLUDED.require_totp, ",
        "updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(settings.password_enabled)
    .bind(settings.oidc_enabled)
    .bind(settings.open_registration)
    .bind(settings.require_totp)
    .bind(actor.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23514") => SettingsError::InvalidCombination,
        _ => SettingsError::Database,
    })?;
    let metadata = serde_json::json!({
        "before": { "password": before.password_enabled, "oidc": before.oidc_enabled, "open_registration": before.open_registration, "require_totp": before.require_totp },
        "after": { "password": settings.password_enabled, "oidc": settings.oidc_enabled, "open_registration": settings.open_registration, "require_totp": settings.require_totp },
    });
    audit_success(transaction, Some(actor), "tenant.account_settings", "tenant", Some(transaction.tenant_id().as_uuid()), &metadata)
        .await
        .map_err(|_| SettingsError::Audit)
}

/// The id of the *active* tenant with `slug`, for the pre-tenant sign-in form. `None` for an
/// unknown, inactive or malformed slug.
pub async fn tenant_by_slug(pool: &PgPool, slug: &str) -> Result<Option<TenantId>, SettingsError> {
    if slug.is_empty() || slug.len() > 63 || !slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Ok(None);
    }
    let found: Option<String> =
        sqlx::query_scalar("SELECT figcraft_tenant_by_slug($1)").bind(slug).fetch_one(pool).await.map_err(|_| SettingsError::Database)?;
    Ok(found.and_then(|id| id.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_allow_both_methods_and_keep_registration_closed() {
        let settings = AccountSettings::default();
        assert!(settings.password_enabled && settings.oidc_enabled);
        assert!(!settings.open_registration && !settings.require_totp);
        assert!(settings.is_valid());
    }

    #[test]
    fn invalid_combinations_are_refused() {
        let base = AccountSettings::default();
        assert!(!AccountSettings { password_enabled: false, oidc_enabled: false, ..base }.is_valid());
        assert!(!AccountSettings { password_enabled: false, open_registration: true, ..base }.is_valid());
        assert!(AccountSettings { password_enabled: false, ..base }.is_valid());
        assert!(AccountSettings { oidc_enabled: false, open_registration: true, ..base }.is_valid());
    }
}
