//! Tenant lifecycle (S4-007): creation with an owner, suspension, reactivation and plan
//! assignment.
//!
//! Every write takes the tenant's own [`TenantTransaction`] and records an audit event in the same
//! transaction, so a rolled-back change leaves no audit row and a committed one always has one.
//! The caller commits. Authorization (who may suspend or change a plan) is decided by the caller
//! before these functions run; the acting principal is passed only for the audit trail.
//!
//! Slugs are validated here with stricter rules than the database `CHECK` (no trailing or doubled
//! hyphen, no reserved route names) so a slug is always safe to use as a URL path segment.

use figcraft_cloud_domain::{PrincipalId, ResourceState, TenantId, Uuid};
use sqlx::PgPool;
use std::fmt;

use crate::TenantTransaction;
use crate::metadata::{Page, PageSize, TenantRecord, TenantRow, page, tenant};

/// Longest slug accepted (matches the database `CHECK`).
pub const MAX_SLUG_CHARS: usize = 63;
/// Longest display name accepted (matches the database `CHECK`).
pub const MAX_NAME_CHARS: usize = 255;
/// Longest home-region identifier accepted.
pub const MAX_REGION_CHARS: usize = 64;
/// Longest suspension reason kept in the audit trail.
pub const MAX_REASON_CHARS: usize = 500;

/// Placeholder plan catalogue until billing (S9) owns plans. Only these ids can be assigned.
pub const PLAN_ALLOWLIST: &[&str] = &["free", "starter", "team", "business", "enterprise", "self_hosted"];

/// Slugs that collide with product routes or well-known hostnames and are never given to a tenant.
pub const RESERVED_SLUGS: &[&str] = &[
    "admin", "api", "app", "assets", "auth", "billing", "cdn", "docs", "edit", "figcraft", "help", "internal", "login", "logout", "mail", "oauth",
    "oidc", "root", "scim", "static", "status", "support", "system", "v1", "www", "ws",
];

/// Fixed, non-leaking categories for tenant lifecycle failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantError {
    InvalidSlug,
    InvalidName,
    InvalidRegion,
    InvalidReason,
    UnknownPlan,
    /// The slug (or id) is already used by another tenant.
    Conflict,
    /// The owner principal does not exist or is not an active user.
    OwnerUnavailable,
    NotFound,
    /// The tenant's current state does not permit the transition.
    InvalidState,
    InvalidPageSize,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for TenantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tenant error: {self:?}")
    }
}

impl std::error::Error for TenantError {}

fn database_error(error: &sqlx::Error) -> TenantError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => TenantError::Conflict,
        Some("23503" | "23514") => TenantError::InvalidState,
        _ => TenantError::Database,
    }
}

fn metadata_error(error: crate::metadata::RepositoryError) -> TenantError {
    use crate::metadata::RepositoryError as E;
    match error {
        E::InvalidPageSize => TenantError::InvalidPageSize,
        E::Conflict => TenantError::Conflict,
        E::Constraint | E::InvalidState => TenantError::InvalidState,
        E::Database => TenantError::Database,
        E::CorruptRow => TenantError::CorruptRow,
    }
}

/// Lowercase ASCII letters, digits and single interior hyphens; 1 to 63 characters; starts with a
/// letter or digit and does not end with a hyphen. Shared with team slugs.
#[must_use]
pub fn slug_shape_ok(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    bytes.len() <= MAX_SLUG_CHARS
        && first.is_ascii_alphanumeric()
        && *last != b'-'
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !slug.contains("--")
}

/// A tenant slug: [`slug_shape_ok`] and not one of [`RESERVED_SLUGS`].
#[must_use]
pub fn valid_tenant_slug(slug: &str) -> bool {
    slug_shape_ok(slug) && !RESERVED_SLUGS.contains(&slug)
}

/// A display name, trimmed: 1 to 255 characters with no control characters.
pub(crate) fn display_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let count = name.chars().count();
    ((1..=MAX_NAME_CHARS).contains(&count) && !name.chars().any(char::is_control)).then_some(name)
}

fn valid_region(region: &str) -> bool {
    !region.is_empty() && region.len() <= MAX_REGION_CHARS && region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `plan_id` is in the [`PLAN_ALLOWLIST`].
#[must_use]
pub fn plan_allowed(plan_id: &str) -> bool {
    PLAN_ALLOWLIST.contains(&plan_id)
}

/// Append a `succeeded` audit row in the transaction's tenant.
pub(crate) async fn audit_success(
    transaction: &mut TenantTransaction<'_>,
    actor: Option<PrincipalId>,
    action: &str,
    resource_kind: &str,
    resource_id: Option<Uuid>,
    metadata: &serde_json::Value,
) -> Result<(), crate::audit::RepositoryError> {
    crate::audit::record_audit_event(
        transaction,
        Uuid::new_v4(),
        actor.map(PrincipalId::as_uuid),
        action,
        resource_kind,
        resource_id,
        None,
        None,
        "succeeded",
        metadata,
    )
    .await
    .map(|_| ())
}

/// What a new tenant is created with.
#[derive(Clone, Copy, Debug)]
pub struct NewTenant<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    pub plan_id: &'a str,
    pub home_region: &'a str,
}

/// Create the transaction's tenant and make `owner` its `tenant_owner`, in that one transaction.
/// The owner must be an existing, active `user` principal. Records `tenant.create`. The caller
/// commits; any error leaves nothing behind once the transaction is rolled back or dropped.
pub async fn create_tenant_with_owner(
    transaction: &mut TenantTransaction<'_>,
    new: NewTenant<'_>,
    owner: PrincipalId,
) -> Result<TenantRecord, TenantError> {
    if !valid_tenant_slug(new.slug) {
        return Err(TenantError::InvalidSlug);
    }
    let name = display_name(new.name).ok_or(TenantError::InvalidName)?;
    if !plan_allowed(new.plan_id) {
        return Err(TenantError::UnknownPlan);
    }
    if !valid_region(new.home_region) {
        return Err(TenantError::InvalidRegion);
    }
    let owner_ok: Option<(bool,)> = sqlx::query_as("SELECT kind = 'user' AND state = 'active' FROM principals WHERE id = $1::uuid FOR SHARE")
        .bind(owner.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if owner_ok != Some((true,)) {
        return Err(TenantError::OwnerUnavailable);
    }
    let record = crate::metadata::create_tenant(transaction, new.slug, name, new.plan_id, new.home_region).await.map_err(metadata_error)?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, 'tenant_owner')")
        .bind(transaction.tenant_id().to_string())
        .bind(owner.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    let metadata = serde_json::json!({ "slug": record.slug, "plan_id": record.plan_id, "home_region": record.home_region });
    audit_success(transaction, Some(owner), "tenant.create", "tenant", Some(record.id.as_uuid()), &metadata).await.map_err(|_| TenantError::Audit)?;
    Ok(record)
}

async fn locked_tenant(transaction: &mut TenantTransaction<'_>) -> Result<TenantRecord, TenantError> {
    let row: Option<TenantRow> =
        sqlx::query_as("SELECT id::text, slug, name, state, plan_id, home_region FROM tenants WHERE id = $1::uuid FOR UPDATE")
            .bind(transaction.tenant_id().to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    row.map(tenant).transpose().map_err(metadata_error)?.ok_or(TenantError::NotFound)
}

async fn set_state(transaction: &mut TenantTransaction<'_>, state: &str) -> Result<TenantRecord, TenantError> {
    let row: TenantRow = sqlx::query_as(
        "UPDATE tenants SET state = $2, updated_at = now() WHERE id = $1::uuid \
         RETURNING id::text, slug, name, state, plan_id, home_region",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(state)
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    tenant(row).map_err(metadata_error)
}

/// Suspend the transaction's tenant (`active` → `suspended`). Idempotent: an already suspended
/// tenant is returned unchanged with no new audit row. Any other state is `InvalidState`.
/// Records `tenant.suspend` with the optional reason.
pub async fn suspend_tenant(transaction: &mut TenantTransaction<'_>, actor: PrincipalId, reason: Option<&str>) -> Result<TenantRecord, TenantError> {
    let reason = match reason.map(str::trim) {
        Some(reason) if reason.is_empty() || reason.chars().count() > MAX_REASON_CHARS || reason.chars().any(char::is_control) => {
            return Err(TenantError::InvalidReason);
        }
        other => other,
    };
    let current = locked_tenant(transaction).await?;
    match current.state {
        ResourceState::Suspended => return Ok(current),
        ResourceState::Active => {}
        _ => return Err(TenantError::InvalidState),
    }
    let record = set_state(transaction, "suspended").await?;
    audit_success(transaction, Some(actor), "tenant.suspend", "tenant", Some(record.id.as_uuid()), &serde_json::json!({ "reason": reason }))
        .await
        .map_err(|_| TenantError::Audit)?;
    Ok(record)
}

/// Reactivate a suspended tenant (`suspended` → `active`). Idempotent for an active tenant; any
/// other state is `InvalidState`. Records `tenant.reactivate`.
pub async fn reactivate_tenant(transaction: &mut TenantTransaction<'_>, actor: PrincipalId) -> Result<TenantRecord, TenantError> {
    let current = locked_tenant(transaction).await?;
    match current.state {
        ResourceState::Active => return Ok(current),
        ResourceState::Suspended => {}
        _ => return Err(TenantError::InvalidState),
    }
    let record = set_state(transaction, "active").await?;
    audit_success(transaction, Some(actor), "tenant.reactivate", "tenant", Some(record.id.as_uuid()), &serde_json::json!({}))
        .await
        .map_err(|_| TenantError::Audit)?;
    Ok(record)
}

/// Assign an allowlisted plan to the transaction's tenant. Allowed while `active` or `suspended`
/// (a lapsed payment may suspend a tenant and a new plan reactivate it later). Assigning the
/// current plan is a no-op without an audit row. Records `tenant.plan_assign` with both plan ids.
pub async fn assign_plan(transaction: &mut TenantTransaction<'_>, actor: PrincipalId, plan_id: &str) -> Result<TenantRecord, TenantError> {
    if !plan_allowed(plan_id) {
        return Err(TenantError::UnknownPlan);
    }
    let current = locked_tenant(transaction).await?;
    if !matches!(current.state, ResourceState::Active | ResourceState::Suspended) {
        return Err(TenantError::InvalidState);
    }
    if current.plan_id == plan_id {
        return Ok(current);
    }
    let row: TenantRow = sqlx::query_as(
        "UPDATE tenants SET plan_id = $2, updated_at = now() WHERE id = $1::uuid \
         RETURNING id::text, slug, name, state, plan_id, home_region",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(plan_id)
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let record = tenant(row).map_err(metadata_error)?;
    let metadata = serde_json::json!({ "from": current.plan_id, "to": record.plan_id });
    audit_success(transaction, Some(actor), "tenant.plan_assign", "tenant", Some(record.id.as_uuid()), &metadata)
        .await
        .map_err(|_| TenantError::Audit)?;
    Ok(record)
}

/// A tenant the principal belongs to, with the principal's tenant role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantMembership {
    pub tenant: TenantRecord,
    /// `tenant_owner`, `tenant_admin`, `editor`, `commenter` or `viewer`.
    pub role: String,
}

/// The tenants where `principal_id` has an active membership, excluding tenants being deleted,
/// ordered by tenant id with a keyset cursor and at most [`PageSize::MAX`] rows. Suspended
/// tenants are listed (with their state) so the UI can explain why they cannot be opened. Runs
/// before any tenant is selected, so it reads through the pool.
pub async fn list_tenants_for_principal(
    pool: &PgPool,
    principal_id: PrincipalId,
    after: Option<TenantId>,
    size: PageSize,
) -> Result<Page<TenantMembership, TenantId>, TenantError> {
    let rows: Vec<(String, String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT t.id::text, t.slug, t.name, t.state, t.plan_id, t.home_region, m.role \
         FROM tenants t JOIN tenant_members m ON m.tenant_id = t.id \
         WHERE m.principal_id = $1::uuid AND m.state = 'active' AND t.state NOT IN ('deleting', 'deleted') \
           AND ($2::uuid IS NULL OR t.id > $2::uuid) \
         ORDER BY t.id LIMIT $3",
    )
    .bind(principal_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(pool)
    .await
    .map_err(|error| database_error(&error))?;
    let items = rows
        .into_iter()
        .map(|(id, slug, name, state, plan_id, home_region, role)| {
            Ok(TenantMembership { tenant: tenant((id, slug, name, state, plan_id, home_region)).map_err(metadata_error)?, role })
        })
        .collect::<Result<Vec<_>, TenantError>>()?;
    Ok(page(items, size, |item| item.tenant.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_url_safe_and_not_reserved() {
        for good in ["a", "acme", "acme-design", "a1", "0day", &"a".repeat(63)] {
            assert!(valid_tenant_slug(good), "{good}");
        }
        for bad in ["", "-a", "a-", "a--b", "Acme", "acme_design", "acme.io", "admin", "api", "ünï", &"a".repeat(64), "a b"] {
            assert!(!valid_tenant_slug(bad), "{bad}");
        }
        assert!(slug_shape_ok("admin"));
    }

    #[test]
    fn names_are_trimmed_bounded_and_printable() {
        assert_eq!(display_name("  Acme  "), Some("Acme"));
        assert_eq!(display_name("   "), None);
        assert_eq!(display_name("a\u{0}b"), None);
        assert_eq!(display_name(&"é".repeat(255)).map(str::len), Some(510));
        assert_eq!(display_name(&"é".repeat(256)), None);
    }

    #[test]
    fn plans_and_regions_are_allowlisted() {
        assert!(plan_allowed("team"));
        assert!(!plan_allowed("Team"));
        assert!(!plan_allowed("unlimited"));
        assert!(valid_region("eu-west-1"));
        assert!(!valid_region(""));
        assert!(!valid_region("EU"));
        assert!(!valid_region(&"a".repeat(65)));
    }
}
