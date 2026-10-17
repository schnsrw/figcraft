//! Workspace lifecycle operations the platform performs (S6-046): create, move between lifecycle
//! states, assign a plan, and hand a pending workspace to its owner.
//!
//! Each operation runs in one transaction that carries both the workspace context and the platform
//! scope ([`crate::platform::enter_platform_scope`]), so the change, the workspace's own audit
//! event and the platform audit event commit together or not at all. These functions decide
//! nothing about who may call them; the `/platform/v1` authenticator does.
//!
//! Lifecycle: see [`figcraft_cloud_domain::policy`]. A `pending` workspace becomes `active` only by
//! its owner accepting the invitation, never by a platform request; `deleted` is set only by the
//! deletion job.

use figcraft_cloud_domain::policy::lifecycle_transition_allowed;
use figcraft_cloud_domain::{PrincipalId, ResourceState, TenantId, Uuid};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::begin_tenant_transaction;
use crate::metadata::{TenantRecord, TenantRow, tenant};
use crate::platform::{
    PlatformActor, PlatformAudit, PlatformError, PlatformOutcome, database_error, enter_platform_scope, record_platform_audit_on, state_name,
};
use crate::sessions::random_token;
use crate::tenants::{self, NewTenant, TenantError, audit_success, display_name, plan_allowed, valid_region, valid_tenant_slug};

/// Default life of an owner invitation (7 days) and the longest allowed (30 days).
pub const DEFAULT_OWNER_INVITE_SECS: i64 = 7 * 24 * 60 * 60;
pub const MAX_OWNER_INVITE_SECS: i64 = 30 * 24 * 60 * 60;
/// Longest reason kept with a lifecycle change.
pub const MAX_REASON_CHARS: usize = 500;

/// Who becomes the owner of a new workspace.
#[derive(Clone, Copy, Debug)]
pub enum OwnerSpec<'a> {
    /// An existing active user: the workspace is created `active` with this owner.
    Principal(PrincipalId),
    /// Someone identified by email: the workspace is created `pending` with an invitation link.
    Invite { email: &'a str },
}

#[derive(Clone, Copy, Debug)]
pub struct NewWorkspace<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    pub plan_id: &'a str,
    pub home_region: &'a str,
    pub owner: OwnerSpec<'a>,
}

/// An owner invitation just issued. The token is shown once.
#[derive(Clone, Eq, PartialEq)]
pub struct OwnerInvitation {
    pub id: Uuid,
    pub token: String,
    /// Unix seconds.
    pub expires_at: i64,
}

impl std::fmt::Debug for OwnerInvitation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OwnerInvitation").field("id", &self.id).field("token", &"[REDACTED]").field("expires_at", &self.expires_at).finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedWorkspace {
    pub workspace: TenantRecord,
    pub owner_invitation: Option<OwnerInvitation>,
}

/// Who asked, and under which request.
#[derive(Clone, Copy, Debug)]
pub struct Cause<'a> {
    pub actor: PlatformActor,
    pub request_id: Option<&'a str>,
}

fn tenant_error(error: TenantError) -> PlatformError {
    match error {
        TenantError::InvalidSlug | TenantError::InvalidName | TenantError::InvalidRegion | TenantError::InvalidReason | TenantError::UnknownPlan => {
            PlatformError::Invalid
        }
        TenantError::Conflict => PlatformError::Conflict,
        TenantError::OwnerUnavailable => PlatformError::Invalid,
        TenantError::NotFound => PlatformError::NotFound,
        TenantError::InvalidState => PlatformError::Conflict,
        TenantError::Database | TenantError::Audit | TenantError::InvalidPageSize | TenantError::CorruptRow => PlatformError::Database,
    }
}

fn transaction_error(_: crate::TenantTransactionError) -> PlatformError {
    PlatformError::Database
}

/// A plausible email: 3 to 320 characters, one `@` with text on both sides, no whitespace or
/// control characters. Returned lowercase. (Delivery and verification are the caller's business.)
#[must_use]
pub fn normalize_email(email: &str) -> Option<String> {
    let email = email.trim().to_ascii_lowercase();
    let (local, domain) = email.split_once('@')?;
    let ok = (3..=320).contains(&email.len())
        && !local.is_empty()
        && domain.contains('.')
        && !domain.contains('@')
        && !email.chars().any(|c| c.is_control() || c.is_whitespace());
    ok.then_some(email)
}

async fn audit_platform(
    transaction: &mut crate::TenantTransaction<'_>,
    cause: Cause<'_>,
    action: &str,
    outcome: PlatformOutcome,
    metadata: &serde_json::Value,
) -> Result<(), PlatformError> {
    let tenant_id = Some(transaction.tenant_id());
    record_platform_audit_on(
        transaction.connection(),
        &PlatformAudit {
            actor: cause.actor,
            action,
            resource_kind: "workspace",
            resource_id: tenant_id.map(TenantId::as_uuid),
            tenant_id,
            outcome,
            request_id: cause.request_id,
            metadata,
        },
    )
    .await
    .map(|_| ())
}

async fn issue_owner_invitation(
    transaction: &mut crate::TenantTransaction<'_>,
    email: &str,
    lifetime_secs: i64,
) -> Result<OwnerInvitation, PlatformError> {
    if !(3600..=MAX_OWNER_INVITE_SECS).contains(&lifetime_secs) {
        return Err(PlatformError::Invalid);
    }
    let token = random_token().ok_or(PlatformError::Database)?;
    let id = Uuid::new_v4();
    let (expires_at,): (i64,) = sqlx::query_as(
        "INSERT INTO workspace_owner_invitations (tenant_id, id, email, token_hash, expires_at) \
         VALUES ($1::uuid, $2::uuid, $3, $4, now() + make_interval(secs => $5::bigint)) \
         RETURNING floor(extract(epoch FROM expires_at))::bigint",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(email)
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(lifetime_secs)
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(OwnerInvitation { id, token, expires_at })
}

/// Create a workspace. With an existing principal as owner it is `active` at once; with an email it
/// is `pending` and the returned invitation token (shown once) completes it.
pub async fn create_workspace(
    pool: &PgPool,
    new: &NewWorkspace<'_>,
    cause: Cause<'_>,
    invite_lifetime_secs: i64,
) -> Result<CreatedWorkspace, PlatformError> {
    let id = TenantId::random();
    let mut transaction = begin_tenant_transaction(pool, id).await.map_err(transaction_error)?;
    enter_platform_scope(&mut transaction).await?;
    let (workspace, invitation, owner_kind) = match new.owner {
        OwnerSpec::Principal(owner) => {
            let record = tenants::create_tenant_with_owner(
                &mut transaction,
                NewTenant { slug: new.slug, name: new.name, plan_id: new.plan_id, home_region: new.home_region },
                owner,
            )
            .await
            .map_err(tenant_error)?;
            (record, None, "principal")
        }
        OwnerSpec::Invite { email } => {
            let email = normalize_email(email).ok_or(PlatformError::Invalid)?;
            if !valid_tenant_slug(new.slug) || !plan_allowed(new.plan_id) || !valid_region(new.home_region) {
                return Err(PlatformError::Invalid);
            }
            let name = display_name(new.name).ok_or(PlatformError::Invalid)?;
            let row: TenantRow = sqlx::query_as(
                "INSERT INTO tenants (id, slug, name, state, plan_id, home_region) VALUES ($1::uuid, $2, $3, 'pending', $4, $5) \
                 RETURNING id::text, slug, name, state, plan_id, home_region",
            )
            .bind(id.to_string())
            .bind(new.slug)
            .bind(name)
            .bind(new.plan_id)
            .bind(new.home_region)
            .fetch_one(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
            let record = tenant(row).map_err(|_| PlatformError::CorruptRow)?;
            let invitation = issue_owner_invitation(&mut transaction, &email, invite_lifetime_secs).await?;
            audit_success(
                &mut transaction,
                None,
                "tenant.create",
                "tenant",
                Some(id.as_uuid()),
                &serde_json::json!({ "slug": record.slug, "pending_owner": true }),
            )
            .await
            .map_err(|_| PlatformError::Database)?;
            (record, Some(invitation), "invitation")
        }
    };
    let metadata =
        serde_json::json!({ "slug": workspace.slug, "plan_id": workspace.plan_id, "home_region": workspace.home_region, "owner": owner_kind });
    audit_platform(&mut transaction, cause, "platform.workspace.create", PlatformOutcome::Succeeded, &metadata).await?;
    transaction.commit().await.map_err(|error| database_error(&error))?;
    Ok(CreatedWorkspace { workspace, owner_invitation: invitation })
}

fn lifecycle_action(from: ResourceState, to: ResourceState) -> &'static str {
    match (from, to) {
        (_, ResourceState::Suspended) => "platform.workspace.suspend",
        (ResourceState::Suspended | ResourceState::Archived, ResourceState::Active) => "platform.workspace.restore",
        (_, ResourceState::Archived) if from == ResourceState::Deleting => "platform.workspace.cancel_deletion",
        (_, ResourceState::Archived) => "platform.workspace.archive",
        (_, ResourceState::Deleting) => "platform.workspace.request_deletion",
        _ => "platform.workspace.state",
    }
}

/// Move a workspace to `to`. Idempotent for a workspace already there. Refuses transitions the
/// lifecycle policy does not allow, and any move out of `pending` (only the owner's acceptance
/// activates a workspace) as `Conflict`. Nothing is deleted. The caller ends live sessions after
/// the commit (the revocation sweep also finds them).
pub async fn set_lifecycle(
    pool: &PgPool,
    id: TenantId,
    to: ResourceState,
    reason: Option<&str>,
    cause: Cause<'_>,
) -> Result<TenantRecord, PlatformError> {
    let reason = match reason.map(str::trim) {
        Some(reason) if reason.is_empty() || reason.chars().count() > MAX_REASON_CHARS || reason.chars().any(char::is_control) => {
            return Err(PlatformError::Invalid);
        }
        other => other,
    };
    let mut transaction = begin_tenant_transaction(pool, id).await.map_err(transaction_error)?;
    enter_platform_scope(&mut transaction).await?;
    let row: Option<TenantRow> =
        sqlx::query_as("SELECT id::text, slug, name, state, plan_id, home_region FROM tenants WHERE id = $1::uuid FOR NO KEY UPDATE")
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    let current = row.map(tenant).transpose().map_err(|_| PlatformError::CorruptRow)?.ok_or(PlatformError::NotFound)?;
    if current.state == to {
        return Ok(current);
    }
    if current.state == ResourceState::Pending || !lifecycle_transition_allowed(current.state, to) {
        audit_platform(
            &mut transaction,
            cause,
            "platform.workspace.state",
            PlatformOutcome::Denied,
            &serde_json::json!({ "from": state_name(current.state), "to": state_name(to) }),
        )
        .await?;
        transaction.commit().await.map_err(|error| database_error(&error))?;
        return Err(PlatformError::Conflict);
    }
    let row: TenantRow = sqlx::query_as(
        "UPDATE tenants SET state = $2, updated_at = now() WHERE id = $1::uuid RETURNING id::text, slug, name, state, plan_id, home_region",
    )
    .bind(id.to_string())
    .bind(state_name(to))
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let record = tenant(row).map_err(|_| PlatformError::CorruptRow)?;
    let metadata = serde_json::json!({ "from": state_name(current.state), "to": state_name(to), "reason": reason, "by_platform": true });
    audit_success(&mut transaction, None, "tenant.lifecycle", "tenant", Some(id.as_uuid()), &metadata).await.map_err(|_| PlatformError::Database)?;
    audit_platform(&mut transaction, cause, lifecycle_action(current.state, to), PlatformOutcome::Succeeded, &metadata).await?;
    transaction.commit().await.map_err(|error| database_error(&error))?;
    Ok(record)
}

/// Assign an allowlisted plan to a workspace that is `active` or `suspended`.
pub async fn set_plan(pool: &PgPool, id: TenantId, plan_id: &str, cause: Cause<'_>) -> Result<TenantRecord, PlatformError> {
    if !plan_allowed(plan_id) {
        return Err(PlatformError::Invalid);
    }
    let mut transaction = begin_tenant_transaction(pool, id).await.map_err(transaction_error)?;
    enter_platform_scope(&mut transaction).await?;
    let row: Option<TenantRow> =
        sqlx::query_as("SELECT id::text, slug, name, state, plan_id, home_region FROM tenants WHERE id = $1::uuid FOR NO KEY UPDATE")
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    let current = row.map(tenant).transpose().map_err(|_| PlatformError::CorruptRow)?.ok_or(PlatformError::NotFound)?;
    if !matches!(current.state, ResourceState::Active | ResourceState::Suspended) {
        return Err(PlatformError::Conflict);
    }
    if current.plan_id == plan_id {
        return Ok(current);
    }
    let row: TenantRow = sqlx::query_as(
        "UPDATE tenants SET plan_id = $2, updated_at = now() WHERE id = $1::uuid RETURNING id::text, slug, name, state, plan_id, home_region",
    )
    .bind(id.to_string())
    .bind(plan_id)
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let record = tenant(row).map_err(|_| PlatformError::CorruptRow)?;
    let metadata = serde_json::json!({ "from": current.plan_id, "to": record.plan_id });
    audit_success(&mut transaction, None, "tenant.plan_assign", "tenant", Some(id.as_uuid()), &metadata)
        .await
        .map_err(|_| PlatformError::Database)?;
    audit_platform(&mut transaction, cause, "platform.workspace.plan", PlatformOutcome::Succeeded, &metadata).await?;
    transaction.commit().await.map_err(|error| database_error(&error))?;
    Ok(record)
}

/// Replace the pending owner invitation of a `pending` workspace (the old link stops working).
pub async fn reissue_owner_invitation(
    pool: &PgPool,
    id: TenantId,
    email: &str,
    lifetime_secs: i64,
    cause: Cause<'_>,
) -> Result<OwnerInvitation, PlatformError> {
    let email = normalize_email(email).ok_or(PlatformError::Invalid)?;
    let mut transaction = begin_tenant_transaction(pool, id).await.map_err(transaction_error)?;
    enter_platform_scope(&mut transaction).await?;
    let state: Option<(String,)> = sqlx::query_as("SELECT state FROM tenants WHERE id = $1::uuid FOR NO KEY UPDATE")
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    match state {
        None => return Err(PlatformError::NotFound),
        Some((state,)) if state != "pending" => return Err(PlatformError::Conflict),
        Some(_) => {}
    }
    sqlx::query("UPDATE workspace_owner_invitations SET state = 'revoked' WHERE tenant_id = $1::uuid AND state = 'pending'")
        .bind(id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    let invitation = issue_owner_invitation(&mut transaction, &email, lifetime_secs).await?;
    audit_platform(&mut transaction, cause, "platform.workspace.owner_invite", PlatformOutcome::Succeeded, &serde_json::json!({})).await?;
    transaction.commit().await.map_err(|error| database_error(&error))?;
    Ok(invitation)
}

/// Spend an owner-invitation token: `principal` becomes the `tenant_owner` and the workspace
/// `active`. The principal must be an active user; when `verified_email` is given it must equal the
/// invited address. Every failure (unknown, spent, expired, wrong email) is `Refused`.
pub async fn accept_owner_invitation(
    pool: &PgPool,
    token: &str,
    principal: PrincipalId,
    verified_email: Option<&str>,
) -> Result<TenantRecord, PlatformError> {
    if !crate::sessions::well_formed_token(token) {
        return Err(PlatformError::Refused);
    }
    let hash = Sha256::digest(token.as_bytes()).to_vec();
    let found: Option<String> = sqlx::query_scalar("SELECT figcraft_owner_invitation_tenant($1)")
        .bind(&hash)
        .fetch_one(pool)
        .await
        .map_err(|error| database_error(&error))?;
    let id: TenantId = found.ok_or(PlatformError::Refused)?.parse().map_err(|_| PlatformError::CorruptRow)?;
    let mut transaction = begin_tenant_transaction(pool, id).await.map_err(transaction_error)?;
    enter_platform_scope(&mut transaction).await?;
    let invitation: Option<(String, String)> = sqlx::query_as(
        "SELECT id::text, email FROM workspace_owner_invitations \
         WHERE tenant_id = $1::uuid AND token_hash = $2 AND state = 'pending' AND expires_at > now() FOR UPDATE",
    )
    .bind(id.to_string())
    .bind(&hash)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let (invitation_id, email) = invitation.ok_or(PlatformError::Refused)?;
    if verified_email.is_some_and(|verified| normalize_email(verified).as_deref() != Some(email.as_str())) {
        return Err(PlatformError::Refused);
    }
    let active_user: Option<(bool,)> = sqlx::query_as("SELECT kind = 'user' AND state = 'active' FROM principals WHERE id = $1::uuid FOR SHARE")
        .bind(principal.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if active_user != Some((true,)) {
        return Err(PlatformError::Refused);
    }
    let row: TenantRow = sqlx::query_as(
        "UPDATE tenants SET state = 'active', updated_at = now() WHERE id = $1::uuid AND state = 'pending' \
         RETURNING id::text, slug, name, state, plan_id, home_region",
    )
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?
    .ok_or(PlatformError::Refused)?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, 'tenant_owner')")
        .bind(id.to_string())
        .bind(principal.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    sqlx::query("UPDATE workspace_owner_invitations SET state = 'accepted', accepted_by = $3::uuid, accepted_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(id.to_string())
        .bind(&invitation_id)
        .bind(principal.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    audit_success(&mut transaction, Some(principal), "tenant.owner_accept", "tenant", Some(id.as_uuid()), &serde_json::json!({}))
        .await
        .map_err(|_| PlatformError::Database)?;
    audit_platform(
        &mut transaction,
        Cause { actor: PlatformActor::System, request_id: None },
        "platform.workspace.owner_accepted",
        PlatformOutcome::Succeeded,
        &serde_json::json!({}),
    )
    .await?;
    transaction.commit().await.map_err(|error| database_error(&error))?;
    tenant(row).map_err(|_| PlatformError::CorruptRow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_normalized_and_implausible_ones_refused() {
        assert_eq!(normalize_email("  Ada@Example.COM ").as_deref(), Some("ada@example.com"));
        for bad in
            ["", "a@b", "@example.com", "ada@@example.com", "ada @example.com", "ada@exam ple.com", "ada\u{0}@example.com", "no-at-sign.example"]
        {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
        assert_eq!(normalize_email(&format!("{}@example.com", "a".repeat(320))), None);
    }
}
