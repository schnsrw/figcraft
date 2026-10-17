//! Tenant, workspace and document membership, team grants and sensitive permissions (S4-008,
//! S4-010, S4-011).
//!
//! Every function is tenant-scoped through [`TenantTransaction`] and leaves committing to the
//! caller, who also authorizes the change (`figcraft_cloud_auth::authz`) and records the audit
//! event in the same transaction.
//!
//! * Tenant members are never hard-deleted (other rows reference them): removal sets the state to
//!   `deleted` and drops the member's direct workspace/document memberships, team memberships and
//!   sensitive permissions in the same transaction.
//! * Last-owner protection: demoting, suspending or removing the last *active* `tenant_owner`
//!   fails with [`MembershipError::LastOwner`]. The tenant row is locked first (`FOR NO KEY UPDATE`: it serialises owner changes but, unlike
//!   `FOR UPDATE`, does not block the foreign-key checks of concurrent audit inserts), so
//!   concurrent owner changes serialize and cannot both pass the check.
//! * Direct memberships and sensitive grants can only be given to *active* tenant members.
//! * Sensitive permissions are only ever explicit rows; nothing here derives one from a role.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, Role, Uuid, WorkspaceId};
use std::fmt;

use crate::TenantTransaction;

/// Fixed, non-leaking categories for membership repository failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipError {
    /// The role is not valid at this boundary (for example `workspace_admin` on a document).
    InvalidRole,
    /// The member, resource, team or grant does not exist in this tenant.
    NotFound,
    /// The principal is not an active member of the tenant.
    NotActiveMember,
    /// The change would leave the tenant without an active owner.
    LastOwner,
    /// The row already exists in a state that forbids this change.
    Conflict,
    Constraint,
    Database,
    CorruptRow,
}

impl fmt::Display for MembershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "membership repository error: {self:?}")
    }
}

impl std::error::Error for MembershipError {}

fn database_error(error: &sqlx::Error) -> MembershipError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => MembershipError::Conflict,
        Some("23503" | "23514") => MembershipError::Constraint,
        _ => MembershipError::Database,
    }
}

/// Membership lifecycle shared by tenant and team members.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MemberState {
    Pending,
    Active,
    Suspended,
    Deleted,
}

impl MemberState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// The database spelling of a role.
#[must_use]
pub const fn role_str(role: Role) -> &'static str {
    match role {
        Role::TenantOwner => "tenant_owner",
        Role::TenantAdmin => "tenant_admin",
        Role::TeamManager => "team_manager",
        Role::WorkspaceAdmin => "workspace_admin",
        Role::Editor => "editor",
        Role::Commenter => "commenter",
        Role::Viewer => "viewer",
    }
}

#[must_use]
pub fn parse_role(value: &str) -> Option<Role> {
    match value {
        "tenant_owner" => Some(Role::TenantOwner),
        "tenant_admin" => Some(Role::TenantAdmin),
        "team_manager" => Some(Role::TeamManager),
        "workspace_admin" => Some(Role::WorkspaceAdmin),
        "editor" => Some(Role::Editor),
        "commenter" => Some(Role::Commenter),
        "viewer" => Some(Role::Viewer),
        _ => None,
    }
}

/// Roles a tenant membership may hold.
#[must_use]
pub const fn valid_tenant_role(role: Role) -> bool {
    matches!(role, Role::TenantOwner | Role::TenantAdmin | Role::Editor | Role::Commenter | Role::Viewer)
}

/// Roles a workspace membership or workspace team grant may hold.
#[must_use]
pub const fn valid_workspace_role(role: Role) -> bool {
    matches!(role, Role::WorkspaceAdmin | Role::Editor | Role::Commenter | Role::Viewer)
}

/// Roles a document membership or document team grant may hold.
#[must_use]
pub const fn valid_document_role(role: Role) -> bool {
    matches!(role, Role::Editor | Role::Commenter | Role::Viewer)
}

/// Explicit sensitive grants (`member_permissions.permission`). The same spellings as
/// `figcraft_cloud_auth::authz::SensitivePermission`; callers map between them by string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MemberPermission {
    Billing,
    ProviderKeys,
    Support,
    Export,
    Deletion,
}

impl MemberPermission {
    pub const ALL: [Self; 5] = [Self::Billing, Self::ProviderKeys, Self::Support, Self::Export, Self::Deletion];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Billing => "billing",
            Self::ProviderKeys => "provider_keys",
            Self::Support => "support",
            Self::Export => "export",
            Self::Deletion => "deletion",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|permission| permission.as_str() == value)
    }
}

/// One tenant member row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TenantMember {
    pub principal_id: PrincipalId,
    pub role: Role,
    pub state: MemberState,
}

/// Most rows a membership listing returns.
pub const MAX_LIST: i64 = 100;

async fn lock_tenant(transaction: &mut TenantTransaction<'_>) -> Result<(), MembershipError> {
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM tenants WHERE id = $1::uuid FOR NO KEY UPDATE")
        .bind(transaction.tenant_id().to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    found.map(|_| ()).ok_or(MembershipError::NotFound)
}

async fn current_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<Option<TenantMember>, MembershipError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT role, state FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid FOR UPDATE")
            .bind(transaction.tenant_id().to_string())
            .bind(principal_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    row.map(|(role, state)| {
        Ok(TenantMember {
            principal_id,
            role: parse_role(&role).ok_or(MembershipError::CorruptRow)?,
            state: MemberState::parse(&state).ok_or(MembershipError::CorruptRow)?,
        })
    })
    .transpose()
}

/// Fail with `LastOwner` when `member` is the only active owner. Call with the tenant locked.
async fn protect_last_owner(transaction: &mut TenantTransaction<'_>, member: &TenantMember) -> Result<(), MembershipError> {
    if member.role != Role::TenantOwner || member.state != MemberState::Active {
        return Ok(());
    }
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tenant_members \
         WHERE tenant_id = $1::uuid AND role = 'tenant_owner' AND state = 'active' AND principal_id <> $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(member.principal_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if others == 0 { Err(MembershipError::LastOwner) } else { Ok(()) }
}

async fn set_member(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    role: Role,
    state: MemberState,
) -> Result<(), MembershipError> {
    sqlx::query("UPDATE tenant_members SET role = $3, state = $4, updated_at = now() WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(principal_id.to_string())
        .bind(role_str(role))
        .bind(state.as_str())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(())
}

/// Add `principal_id` to the tenant as an active member, or re-activate a previously removed
/// member with the new role. An existing pending, active or suspended member is a `Conflict`
/// (use [`change_tenant_member_role`] or [`reactivate_tenant_member`]).
pub async fn add_tenant_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId, role: Role) -> Result<(), MembershipError> {
    if !valid_tenant_role(role) {
        return Err(MembershipError::InvalidRole);
    }
    lock_tenant(transaction).await?;
    match current_member(transaction, principal_id).await? {
        Some(member) if member.state == MemberState::Deleted => set_member(transaction, principal_id, role, MemberState::Active).await,
        Some(_) => Err(MembershipError::Conflict),
        None => {
            sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, $3, 'active')")
                .bind(transaction.tenant_id().to_string())
                .bind(principal_id.to_string())
                .bind(role_str(role))
                .execute(transaction.connection())
                .await
                .map_err(|error| database_error(&error))?;
            Ok(())
        }
    }
}

pub async fn get_tenant_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<Option<TenantMember>, MembershipError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT role, state FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
            .bind(transaction.tenant_id().to_string())
            .bind(principal_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    row.map(|(role, state)| {
        Ok(TenantMember {
            principal_id,
            role: parse_role(&role).ok_or(MembershipError::CorruptRow)?,
            state: MemberState::parse(&state).ok_or(MembershipError::CorruptRow)?,
        })
    })
    .transpose()
}

/// Up to [`MAX_LIST`] non-deleted members after `after` (principal id order).
pub async fn list_tenant_members(transaction: &mut TenantTransaction<'_>, after: Option<PrincipalId>) -> Result<Vec<TenantMember>, MembershipError> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT principal_id::text, role, state FROM tenant_members \
         WHERE tenant_id = $1::uuid AND state <> 'deleted' AND ($2::uuid IS NULL OR principal_id > $2::uuid) \
         ORDER BY principal_id LIMIT $3",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(MAX_LIST)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    rows.into_iter()
        .map(|(principal, role, state)| {
            Ok(TenantMember {
                principal_id: principal.parse().map_err(|_| MembershipError::CorruptRow)?,
                role: parse_role(&role).ok_or(MembershipError::CorruptRow)?,
                state: MemberState::parse(&state).ok_or(MembershipError::CorruptRow)?,
            })
        })
        .collect()
}

/// Change a non-deleted member's tenant role. Demoting the last active owner is `LastOwner`.
pub async fn change_tenant_member_role(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    role: Role,
) -> Result<(), MembershipError> {
    if !valid_tenant_role(role) {
        return Err(MembershipError::InvalidRole);
    }
    lock_tenant(transaction).await?;
    let member = current_member(transaction, principal_id).await?.ok_or(MembershipError::NotFound)?;
    if member.state == MemberState::Deleted {
        return Err(MembershipError::NotFound);
    }
    if role != Role::TenantOwner {
        protect_last_owner(transaction, &member).await?;
    }
    set_member(transaction, principal_id, role, member.state).await
}

/// Suspend an active or pending member: they keep their grants but every check denies them.
pub async fn suspend_tenant_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<(), MembershipError> {
    lock_tenant(transaction).await?;
    let member = current_member(transaction, principal_id).await?.ok_or(MembershipError::NotFound)?;
    match member.state {
        MemberState::Deleted => Err(MembershipError::NotFound),
        MemberState::Suspended => Ok(()),
        MemberState::Active | MemberState::Pending => {
            protect_last_owner(transaction, &member).await?;
            set_member(transaction, principal_id, member.role, MemberState::Suspended).await
        }
    }
}

/// Re-activate a suspended or pending member.
pub async fn reactivate_tenant_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<(), MembershipError> {
    lock_tenant(transaction).await?;
    let member = current_member(transaction, principal_id).await?.ok_or(MembershipError::NotFound)?;
    match member.state {
        MemberState::Deleted => Err(MembershipError::NotFound),
        MemberState::Active => Ok(()),
        MemberState::Suspended | MemberState::Pending => set_member(transaction, principal_id, member.role, MemberState::Active).await,
    }
}

/// Remove a member: state `deleted`, and their direct memberships, team memberships and sensitive
/// permissions are dropped. Removing the last active owner is `LastOwner`.
pub async fn remove_tenant_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<(), MembershipError> {
    lock_tenant(transaction).await?;
    let member = current_member(transaction, principal_id).await?.ok_or(MembershipError::NotFound)?;
    if member.state == MemberState::Deleted {
        return Err(MembershipError::NotFound);
    }
    protect_last_owner(transaction, &member).await?;
    let tenant = transaction.tenant_id().to_string();
    let principal = principal_id.to_string();
    for statement in [
        "DELETE FROM member_permissions WHERE tenant_id = $1::uuid AND principal_id = $2::uuid",
        "DELETE FROM workspace_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid",
        "DELETE FROM document_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid",
        "UPDATE team_members SET state = 'deleted', updated_at = now() WHERE tenant_id = $1::uuid AND principal_id = $2::uuid",
        "UPDATE tenant_members SET state = 'deleted', updated_at = now() WHERE tenant_id = $1::uuid AND principal_id = $2::uuid",
    ] {
        sqlx::query(statement).bind(&tenant).bind(&principal).execute(transaction.connection()).await.map_err(|error| database_error(&error))?;
    }
    Ok(())
}

/// Insert or update a direct workspace membership for an active tenant member.
pub async fn set_workspace_member(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    principal_id: PrincipalId,
    role: Role,
) -> Result<(), MembershipError> {
    if !valid_workspace_role(role) {
        return Err(MembershipError::InvalidRole);
    }
    require_active_member(transaction, principal_id).await?;
    let result = sqlx::query(
        "INSERT INTO workspace_members (tenant_id, workspace_id, principal_id, role) \
         SELECT w.tenant_id, w.id, $3::uuid, $4 FROM workspaces w WHERE w.tenant_id = $1::uuid AND w.id = $2::uuid \
         ON CONFLICT (tenant_id, workspace_id, principal_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now()",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(principal_id.to_string())
    .bind(role_str(role))
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if result.rows_affected() == 0 { Err(MembershipError::NotFound) } else { Ok(()) }
}

/// Remove a direct workspace membership: true when one was removed.
pub async fn remove_workspace_member(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    principal_id: PrincipalId,
) -> Result<bool, MembershipError> {
    let result = sqlx::query("DELETE FROM workspace_members WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND principal_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(workspace_id.to_string())
        .bind(principal_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() > 0)
}

/// Insert or update a direct document membership for an active tenant member.
pub async fn set_document_member(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    principal_id: PrincipalId,
    role: Role,
) -> Result<(), MembershipError> {
    if !valid_document_role(role) {
        return Err(MembershipError::InvalidRole);
    }
    require_active_member(transaction, principal_id).await?;
    let result = sqlx::query(
        "INSERT INTO document_members (tenant_id, document_id, principal_id, role) \
         SELECT d.tenant_id, d.id, $3::uuid, $4 FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $2::uuid \
         ON CONFLICT (tenant_id, document_id, principal_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now()",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(principal_id.to_string())
    .bind(role_str(role))
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if result.rows_affected() == 0 { Err(MembershipError::NotFound) } else { Ok(()) }
}

pub async fn remove_document_member(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    principal_id: PrincipalId,
) -> Result<bool, MembershipError> {
    let result = sqlx::query("DELETE FROM document_members WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND principal_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .bind(principal_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() > 0)
}

/// Grant team `team_id` a role on a workspace (insert or update).
pub async fn set_workspace_team_grant(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    team_id: Uuid,
    role: Role,
) -> Result<(), MembershipError> {
    if !valid_workspace_role(role) {
        return Err(MembershipError::InvalidRole);
    }
    let result = sqlx::query(
        "INSERT INTO workspace_team_grants (tenant_id, workspace_id, team_id, role) \
         SELECT w.tenant_id, w.id, t.id, $4 FROM workspaces w JOIN teams t ON t.tenant_id = w.tenant_id AND t.id = $3::uuid \
         WHERE w.tenant_id = $1::uuid AND w.id = $2::uuid \
         ON CONFLICT (tenant_id, workspace_id, team_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now()",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(team_id.to_string())
    .bind(role_str(role))
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if result.rows_affected() == 0 { Err(MembershipError::NotFound) } else { Ok(()) }
}

pub async fn remove_workspace_team_grant(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    team_id: Uuid,
) -> Result<bool, MembershipError> {
    let result = sqlx::query("DELETE FROM workspace_team_grants WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND team_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(workspace_id.to_string())
        .bind(team_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() > 0)
}

/// A workspace's team grants by team ID after `after`, at most
/// [`crate::resource_admin::MAX_LISTED`] (the same shape as the document listing).
pub async fn list_workspace_team_grants(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    after: Option<Uuid>,
) -> Result<Vec<crate::resource_admin::Grant<Uuid>>, MembershipError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT team_id::text, role FROM workspace_team_grants \
         WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND ($3::uuid IS NULL OR team_id > $3::uuid) \
         ORDER BY team_id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(crate::resource_admin::MAX_LISTED)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    rows.into_iter()
        .map(|(id, role)| {
            Ok(crate::resource_admin::Grant {
                id: id.parse().map_err(|_| MembershipError::CorruptRow)?,
                role: parse_role(&role).ok_or(MembershipError::CorruptRow)?,
            })
        })
        .collect()
}

/// Grant team `team_id` a role on a document (insert or update).
pub async fn set_document_team_grant(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    team_id: Uuid,
    role: Role,
) -> Result<(), MembershipError> {
    if !valid_document_role(role) {
        return Err(MembershipError::InvalidRole);
    }
    let result = sqlx::query(
        "INSERT INTO document_team_grants (tenant_id, document_id, team_id, role) \
         SELECT d.tenant_id, d.id, t.id, $4 FROM documents d JOIN teams t ON t.tenant_id = d.tenant_id AND t.id = $3::uuid \
         WHERE d.tenant_id = $1::uuid AND d.id = $2::uuid \
         ON CONFLICT (tenant_id, document_id, team_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now()",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(team_id.to_string())
    .bind(role_str(role))
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if result.rows_affected() == 0 { Err(MembershipError::NotFound) } else { Ok(()) }
}

pub async fn remove_document_team_grant(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    team_id: Uuid,
) -> Result<bool, MembershipError> {
    let result = sqlx::query("DELETE FROM document_team_grants WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND team_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .bind(team_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() > 0)
}

async fn require_active_member(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<(), MembershipError> {
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT m.state = 'active' AND p.state = 'active' FROM tenant_members m JOIN principals p ON p.id = m.principal_id \
         WHERE m.tenant_id = $1::uuid AND m.principal_id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if active == Some(true) { Ok(()) } else { Err(MembershipError::NotActiveMember) }
}

/// Grant an explicit sensitive permission to an active member: true when newly granted.
pub async fn grant_member_permission(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    permission: MemberPermission,
    granted_by: PrincipalId,
) -> Result<bool, MembershipError> {
    require_active_member(transaction, principal_id).await?;
    let result = sqlx::query(
        "INSERT INTO member_permissions (tenant_id, principal_id, permission, granted_by) VALUES ($1::uuid, $2::uuid, $3, $4::uuid) \
         ON CONFLICT (tenant_id, principal_id, permission) DO NOTHING",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(permission.as_str())
    .bind(granted_by.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() > 0)
}

/// Revoke an explicit sensitive permission: true when one was revoked.
pub async fn revoke_member_permission(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    permission: MemberPermission,
) -> Result<bool, MembershipError> {
    let result = sqlx::query("DELETE FROM member_permissions WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND permission = $3")
        .bind(transaction.tenant_id().to_string())
        .bind(principal_id.to_string())
        .bind(permission.as_str())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    Ok(result.rows_affected() > 0)
}

/// The member's explicit sensitive permissions (whatever their membership state; resolution in
/// [`crate::effective_roles`] ignores them unless the member is active).
pub async fn list_member_permissions(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
) -> Result<Vec<MemberPermission>, MembershipError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT permission FROM member_permissions WHERE tenant_id = $1::uuid AND principal_id = $2::uuid ORDER BY permission LIMIT 16",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    rows.iter().map(|value| MemberPermission::parse(value).ok_or(MembershipError::CorruptRow)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_states_round_trip_and_boundaries_are_enforced() {
        for role in [Role::TenantOwner, Role::TenantAdmin, Role::TeamManager, Role::WorkspaceAdmin, Role::Editor, Role::Commenter, Role::Viewer] {
            assert_eq!(parse_role(role_str(role)), Some(role));
        }
        assert_eq!(parse_role("owner"), None);
        assert!(!valid_tenant_role(Role::WorkspaceAdmin) && !valid_tenant_role(Role::TeamManager));
        assert!(!valid_workspace_role(Role::TenantOwner) && !valid_workspace_role(Role::TeamManager));
        assert!(!valid_document_role(Role::WorkspaceAdmin));
        for state in [MemberState::Pending, MemberState::Active, MemberState::Suspended, MemberState::Deleted] {
            assert_eq!(MemberState::parse(state.as_str()), Some(state));
        }
        for permission in MemberPermission::ALL {
            assert_eq!(MemberPermission::parse(permission.as_str()), Some(permission));
        }
    }
}
