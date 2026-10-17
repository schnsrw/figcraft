//! One-command setup for a team that hosts FigCraft itself (TS-001, S6-051).
//!
//! [`setup_self_hosted`] creates, in what is missing: the first workspace with its owner (a
//! password account, no identity provider) and the platform admin who installed the system. It is
//! safe to run again: whatever already exists is left alone, and when nothing is missing it says so
//! ([`SetupError::AlreadySetUp`]). Password hashing happens in the caller (this crate stores only
//! the PHC strings). Nothing secret is written to any log or audit event.

use figcraft_cloud_domain::{PrincipalId, Role, TenantId};
use sqlx::PgPool;
use std::fmt;

use crate::platform::{
    PlatformActor, PlatformAudit, PlatformError, PlatformOutcome, begin_platform_transaction, enter_platform_scope, record_platform_audit,
    record_platform_audit_on,
};
use crate::tenants::{audit_success, display_name, valid_tenant_slug};
use crate::{accounts, begin_tenant_transaction, memberships, platform_admins};

/// The first administrator of the installation, in the platform realm.
#[derive(Clone, Copy, Debug)]
pub struct PlatformAdminSpec<'a> {
    pub login: &'a str,
    pub display_name: &'a str,
    pub password_hash: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct SelfHostSpec<'a> {
    /// The workspace id (single-tenant installs use the configured tenant id).
    pub tenant: TenantId,
    pub slug: &'a str,
    pub name: &'a str,
    pub owner_login: &'a str,
    pub owner_name: &'a str,
    pub owner_password_hash: &'a str,
    pub platform_admin: Option<PlatformAdminSpec<'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetupOutcome {
    pub workspace_created: bool,
    pub platform_admin_created: bool,
}

/// Fixed, non-leaking categories for setup failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupError {
    /// A name, slug or login failed validation.
    Invalid,
    /// The workspace and the platform admin already exist.
    AlreadySetUp,
    /// The slug or login is already used by someone else.
    Conflict,
    Database,
}

impl fmt::Display for SetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "setup error: {self:?}")
    }
}

impl std::error::Error for SetupError {}

fn platform_error(error: PlatformError) -> SetupError {
    match error {
        PlatformError::Invalid => SetupError::Invalid,
        PlatformError::Conflict => SetupError::Conflict,
        _ => SetupError::Database,
    }
}

fn account_error(error: accounts::AccountError) -> SetupError {
    use accounts::AccountError as E;
    match error {
        E::InvalidName | E::InvalidLogin | E::InvalidHash => SetupError::Invalid,
        E::LoginTaken => SetupError::Conflict,
        _ => SetupError::Database,
    }
}

fn database(error: &sqlx::Error) -> SetupError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => SetupError::Conflict,
        Some("23514") => SetupError::Invalid,
        _ => SetupError::Database,
    }
}

/// A slug for `name`: lowercase letters and digits, single hyphens between words, at most 40
/// characters, never empty and never reserved. `None` when nothing usable is left.
#[must_use]
pub fn slug_for(name: &str) -> Option<String> {
    let mut slug = String::new();
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches('-').to_owned();
    valid_tenant_slug(&slug).then_some(slug)
}

/// Create what is missing. See the module documentation.
pub async fn setup_self_hosted(pool: &PgPool, spec: &SelfHostSpec<'_>) -> Result<SetupOutcome, SetupError> {
    if !valid_tenant_slug(spec.slug) || display_name(spec.name).is_none() {
        return Err(SetupError::Invalid);
    }
    let workspace_created = setup_workspace(pool, spec).await?;
    let platform_admin_created = match &spec.platform_admin {
        Some(admin) => setup_platform_admin(pool, admin).await?,
        None => false,
    };
    if workspace_created || platform_admin_created {
        Ok(SetupOutcome { workspace_created, platform_admin_created })
    } else {
        Err(SetupError::AlreadySetUp)
    }
}

async fn setup_workspace(pool: &PgPool, spec: &SelfHostSpec<'_>) -> Result<bool, SetupError> {
    let mut transaction = begin_tenant_transaction(pool, spec.tenant).await.map_err(|_| SetupError::Database)?;
    enter_platform_scope(&mut transaction).await.map_err(platform_error)?;
    let existing: Option<(String,)> = sqlx::query_as("SELECT state FROM tenants WHERE id = $1::uuid FOR NO KEY UPDATE")
        .bind(spec.tenant.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database(&error))?;
    if existing.is_some() {
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM tenant_members WHERE tenant_id = $1::uuid AND role = 'tenant_owner' AND state = 'active')",
        )
        .bind(spec.tenant.to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(|error| database(&error))?;
        if owned {
            return Ok(false);
        }
        // A tenant row created by an earlier bootstrap that never got an owner: complete it.
        sqlx::query("UPDATE tenants SET slug = $2, name = $3, state = 'active', plan_id = 'self_hosted', updated_at = now() WHERE id = $1::uuid")
            .bind(spec.tenant.to_string())
            .bind(spec.slug)
            .bind(spec.name.trim())
            .execute(transaction.connection())
            .await
            .map_err(|error| database(&error))?;
    } else {
        sqlx::query("INSERT INTO tenants (id, slug, name, state, plan_id, home_region) VALUES ($1::uuid, $2, $3, 'active', 'self_hosted', 'local')")
            .bind(spec.tenant.to_string())
            .bind(spec.slug)
            .bind(spec.name.trim())
            .execute(transaction.connection())
            .await
            .map_err(|error| database(&error))?;
    }
    let owner: PrincipalId =
        accounts::create_account(&mut transaction, spec.owner_name, spec.owner_login, spec.owner_password_hash, None).await.map_err(account_error)?;
    memberships::add_tenant_member(&mut transaction, owner, Role::TenantOwner).await.map_err(|_| SetupError::Database)?;
    audit_success(
        &mut transaction,
        Some(owner),
        "tenant.create",
        "tenant",
        Some(spec.tenant.as_uuid()),
        &serde_json::json!({ "slug": spec.slug, "self_hosted": true }),
    )
    .await
    .map_err(|_| SetupError::Database)?;
    let metadata = serde_json::json!({ "slug": spec.slug });
    record_platform_audit_on(
        transaction.connection(),
        &PlatformAudit {
            actor: PlatformActor::System,
            action: "platform.setup.workspace",
            resource_kind: "workspace",
            resource_id: Some(spec.tenant.as_uuid()),
            tenant_id: Some(spec.tenant),
            outcome: PlatformOutcome::Succeeded,
            request_id: None,
            metadata: &metadata,
        },
    )
    .await
    .map_err(platform_error)?;
    transaction.commit().await.map_err(|error| database(&error))?;
    Ok(true)
}

async fn setup_platform_admin(pool: &PgPool, admin: &PlatformAdminSpec<'_>) -> Result<bool, SetupError> {
    let mut transaction = begin_platform_transaction(pool).await.map_err(platform_error)?;
    // Serialize concurrent setups on the bootstrap row's table-level count.
    sqlx::query("LOCK TABLE platform_admins IN SHARE ROW EXCLUSIVE MODE")
        .execute(transaction.connection())
        .await
        .map_err(|error| database(&error))?;
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_admins").fetch_one(transaction.connection()).await.map_err(|error| database(&error))?;
    if existing > 0 {
        return Ok(false);
    }
    let created =
        platform_admins::create_admin(&mut transaction, None, admin.login, admin.display_name, admin.password_hash).await.map_err(platform_error)?;
    let metadata = serde_json::json!({});
    record_platform_audit(
        &mut transaction,
        &PlatformAudit {
            actor: PlatformActor::System,
            action: "platform.setup.admin",
            resource_kind: "platform_admin",
            resource_id: Some(created.id),
            tenant_id: None,
            outcome: PlatformOutcome::Succeeded,
            request_id: None,
            metadata: &metadata,
        },
    )
    .await
    .map_err(platform_error)?;
    transaction.commit().await.map_err(platform_error)?;
    Ok(true)
}

/// A random password of 24 letters and digits in five groups ("k7mqz-w4xrt-..."), from an alphabet
/// without look-alike characters. About 120 bits. `None` when no secure random numbers exist.
#[must_use]
pub fn generate_password() -> Option<String> {
    use ring::rand::SecureRandom as _;
    const ALPHABET: &[u8; 32] = b"abcdefghjkmnpqrstuvwxyz23456789_";
    let mut bytes = [0u8; 25];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    let mut out = String::with_capacity(29);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && index % 5 == 0 {
            out.push('-');
        }
        // The last alphabet entry is replaced so the password only contains letters and digits.
        let character = ALPHABET.get(usize::from(byte & 31)).copied().unwrap_or(b'a');
        out.push(char::from(if character == b'_' { b'x' } else { character }));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_derived_from_names() {
        assert_eq!(slug_for("Acme Design Team").as_deref(), Some("acme-design-team"));
        assert_eq!(slug_for("  --Ünï & Co.  ").as_deref(), Some("n-co"));
        assert_eq!(slug_for("Q3 2026").as_deref(), Some("q3-2026"));
        assert_eq!(slug_for("   "), None);
        assert_eq!(slug_for("Admin"), None, "reserved names are not usable");
        assert_eq!(slug_for("!!!"), None);
        let long = slug_for(&"word ".repeat(30)).unwrap_or_default();
        assert!(long.len() <= 40 && !long.ends_with('-'));
    }

    #[test]
    fn generated_passwords_are_long_distinct_and_plain() {
        let first = generate_password().unwrap_or_default();
        assert_eq!(first.len(), 29);
        assert!(first.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_eq!(first.matches('-').count(), 4);
        assert_ne!(Some(first), generate_password());
    }
}
