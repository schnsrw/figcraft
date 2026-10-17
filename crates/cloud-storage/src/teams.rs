//! Teams (S4-009): create, rename, archive; membership with the team-manager role; and team
//! ownership of, or grants on, workspaces, projects and libraries.
//!
//! Membership changes lock the team row first (`FOR UPDATE`), so concurrent removals or demotions
//! are serialised and the "at least one active team manager" rule cannot be raced past. Every
//! write records an audit event in the same transaction; the caller authorizes and commits.

use figcraft_cloud_domain::{PrincipalId, ResourceState, Uuid, WorkspaceId};
use std::fmt;

use crate::TenantTransaction;
use crate::metadata::{Page, PageSize, page, state};
use crate::tenants::{audit_success, display_name, slug_shape_ok};

/// A member's role inside one team.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TeamRole {
    TeamManager,
    Editor,
    Commenter,
    Viewer,
}

impl TeamRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TeamManager => "team_manager",
            Self::Editor => "editor",
            Self::Commenter => "commenter",
            Self::Viewer => "viewer",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "team_manager" => Some(Self::TeamManager),
            "editor" => Some(Self::Editor),
            "commenter" => Some(Self::Commenter),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

/// A role a team can be granted on a workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceGrantRole {
    WorkspaceAdmin,
    Editor,
    Commenter,
    Viewer,
}

impl WorkspaceGrantRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceAdmin => "workspace_admin",
            Self::Editor => "editor",
            Self::Commenter => "commenter",
            Self::Viewer => "viewer",
        }
    }
}

/// A role a team can be granted on a library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibraryGrantRole {
    Viewer,
    Publisher,
}

impl LibraryGrantRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Publisher => "publisher",
        }
    }
}

/// Fixed, non-leaking categories for team failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamError {
    InvalidSlug,
    InvalidName,
    InvalidPageSize,
    /// The slug is already used by another team in this tenant, or the member is already active.
    Conflict,
    NotFound,
    /// The team (or the member) is not in a state that permits the change.
    InvalidState,
    /// The principal is not an active member of the tenant.
    NotTenantMember,
    /// The change would leave the team without an active team manager.
    LastManager,
    /// The workspace, project or library does not exist in this tenant.
    ResourceNotFound,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for TeamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "team error: {self:?}")
    }
}

impl std::error::Error for TeamError {}

fn database_error(error: &sqlx::Error) -> TeamError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => TeamError::Conflict,
        Some("23503") => TeamError::ResourceNotFound,
        Some("23514") => TeamError::InvalidState,
        _ => TeamError::Database,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamRecord {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub state: ResourceState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamMemberRecord {
    pub principal_id: PrincipalId,
    pub role: TeamRole,
}

type TeamRow = (String, String, String, String);

fn team(row: TeamRow) -> Result<TeamRecord, TeamError> {
    Ok(TeamRecord {
        id: Uuid::parse_str(&row.0).map_err(|_| TeamError::CorruptRow)?,
        slug: row.1,
        name: row.2,
        state: state(&row.3).map_err(|_| TeamError::CorruptRow)?,
    })
}

async fn audit(
    transaction: &mut TenantTransaction<'_>,
    actor: PrincipalId,
    action: &str,
    team_id: Uuid,
    metadata: serde_json::Value,
) -> Result<(), TeamError> {
    audit_success(transaction, Some(actor), action, "team", Some(team_id), &metadata).await.map_err(|_| TeamError::Audit)
}

async fn require_active_tenant_member(transaction: &mut TenantTransaction<'_>, principal: PrincipalId) -> Result<(), TeamError> {
    let active: Option<(bool,)> = sqlx::query_as(
        "SELECT m.state = 'active' AND p.state = 'active' FROM tenant_members m JOIN principals p ON p.id = m.principal_id \
         WHERE m.tenant_id = $1::uuid AND m.principal_id = $2::uuid FOR SHARE OF m",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if active == Some((true,)) { Ok(()) } else { Err(TeamError::NotTenantMember) }
}

/// Lock `id`'s team row for the rest of the transaction and return it.
async fn lock_team(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<TeamRecord, TeamError> {
    let row: Option<TeamRow> =
        sqlx::query_as("SELECT id::text, slug, name, state FROM teams WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    row.map(team).transpose()?.ok_or(TeamError::NotFound)
}

async fn lock_active_team(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<TeamRecord, TeamError> {
    let record = lock_team(transaction, id).await?;
    if record.state == ResourceState::Active { Ok(record) } else { Err(TeamError::InvalidState) }
}

/// Create an active team whose first team manager is `creator` (an active tenant member), so a
/// team never exists without a manager. Records `team.create`.
pub async fn create_team(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    slug: &str,
    name: &str,
    creator: PrincipalId,
) -> Result<TeamRecord, TeamError> {
    if !slug_shape_ok(slug) {
        return Err(TeamError::InvalidSlug);
    }
    let name = display_name(name).ok_or(TeamError::InvalidName)?;
    require_active_tenant_member(transaction, creator).await?;
    let row: TeamRow =
        sqlx::query_as("INSERT INTO teams (id, tenant_id, slug, name) VALUES ($1::uuid, $2::uuid, $3, $4) RETURNING id::text, slug, name, state")
            .bind(id.to_string())
            .bind(transaction.tenant_id().to_string())
            .bind(slug)
            .bind(name)
            .fetch_one(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    let record = team(row)?;
    sqlx::query("INSERT INTO team_members (tenant_id, team_id, principal_id, role) VALUES ($1::uuid, $2::uuid, $3::uuid, 'team_manager')")
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .bind(creator.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    audit(transaction, creator, "team.create", id, serde_json::json!({ "slug": record.slug, "manager": creator.to_string() })).await?;
    Ok(record)
}

/// `id`'s team in the transaction's tenant.
pub async fn get_team(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<TeamRecord>, TeamError> {
    let row: Option<TeamRow> = sqlx::query_as("SELECT id::text, slug, name, state FROM teams WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    row.map(team).transpose()
}

/// The tenant's teams that are not deleted, ordered by id, keyset-paged.
pub async fn list_teams(transaction: &mut TenantTransaction<'_>, after: Option<Uuid>, size: PageSize) -> Result<Page<TeamRecord, Uuid>, TeamError> {
    let rows: Vec<TeamRow> = sqlx::query_as(
        "SELECT id::text, slug, name, state FROM teams WHERE tenant_id = $1::uuid AND state <> 'deleted' \
           AND ($2::uuid IS NULL OR id > $2::uuid) ORDER BY id LIMIT $3",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(page(rows.into_iter().map(team).collect::<Result<Vec<_>, _>>()?, size, |item| item.id))
}

/// Rename an active team. Records `team.rename`.
pub async fn rename_team(transaction: &mut TenantTransaction<'_>, id: Uuid, name: &str, actor: PrincipalId) -> Result<TeamRecord, TeamError> {
    let name = display_name(name).ok_or(TeamError::InvalidName)?;
    let current = lock_active_team(transaction, id).await?;
    if current.name == name {
        return Ok(current);
    }
    let row: TeamRow = sqlx::query_as(
        "UPDATE teams SET name = $3, updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid RETURNING id::text, slug, name, state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(name)
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let record = team(row)?;
    audit(transaction, actor, "team.rename", id, serde_json::json!({})).await?;
    Ok(record)
}

/// Archive a team (`active` → `archived`). Idempotent for an archived team. Archived teams accept
/// no new members, invitations, ownership or grants; existing grants stay for S4-012 to ignore.
/// Records `team.archive`.
pub async fn archive_team(transaction: &mut TenantTransaction<'_>, id: Uuid, actor: PrincipalId) -> Result<TeamRecord, TeamError> {
    let current = lock_team(transaction, id).await?;
    match current.state {
        ResourceState::Archived => return Ok(current),
        ResourceState::Active => {}
        _ => return Err(TeamError::InvalidState),
    }
    let row: TeamRow = sqlx::query_as(
        "UPDATE teams SET state = 'archived', updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid \
         RETURNING id::text, slug, name, state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let record = team(row)?;
    audit(transaction, actor, "team.archive", id, serde_json::json!({})).await?;
    Ok(record)
}

async fn member_role(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    principal: PrincipalId,
) -> Result<Option<(TeamRole, String)>, TeamError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT role, state FROM team_members WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND principal_id = $3::uuid")
            .bind(transaction.tenant_id().to_string())
            .bind(team_id.to_string())
            .bind(principal.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    row.map(|(role, state)| TeamRole::parse(&role).map(|role| (role, state)).ok_or(TeamError::CorruptRow)).transpose()
}

async fn other_active_managers(transaction: &mut TenantTransaction<'_>, team_id: Uuid, principal: PrincipalId) -> Result<i64, TeamError> {
    sqlx::query_scalar(
        "SELECT count(*) FROM team_members WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND principal_id <> $3::uuid \
           AND role = 'team_manager' AND state = 'active'",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .bind(principal.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))
}

/// Insert or reactivate a membership row. Used by invitation acceptance as well; the team row must
/// already be locked by the caller. Returns `false` when an active membership already existed.
pub(crate) async fn upsert_member(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    principal: PrincipalId,
    role: TeamRole,
) -> Result<bool, TeamError> {
    match member_role(transaction, team_id, principal).await? {
        Some((_, state)) if state == "active" => Ok(false),
        Some((_, state)) if state == "suspended" => Err(TeamError::InvalidState),
        Some(_) => {
            sqlx::query(
                "UPDATE team_members SET role = $4, state = 'active', updated_at = now() \
                 WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND principal_id = $3::uuid",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(team_id.to_string())
            .bind(principal.to_string())
            .bind(role.as_str())
            .execute(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
            Ok(true)
        }
        None => {
            sqlx::query("INSERT INTO team_members (tenant_id, team_id, principal_id, role) VALUES ($1::uuid, $2::uuid, $3::uuid, $4)")
                .bind(transaction.tenant_id().to_string())
                .bind(team_id.to_string())
                .bind(principal.to_string())
                .bind(role.as_str())
                .execute(transaction.connection())
                .await
                .map_err(|error| database_error(&error))?;
            Ok(true)
        }
    }
}

/// Add an active tenant member to an active team with `role`. An already active member is
/// `Conflict` (use [`change_team_member_role`]); a removed member is re-added. Records
/// `team.member_add`.
pub async fn add_team_member(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    principal: PrincipalId,
    role: TeamRole,
    actor: PrincipalId,
) -> Result<(), TeamError> {
    lock_active_team(transaction, team_id).await?;
    require_active_tenant_member(transaction, principal).await?;
    if !upsert_member(transaction, team_id, principal, role).await? {
        return Err(TeamError::Conflict);
    }
    audit(transaction, actor, "team.member_add", team_id, serde_json::json!({ "principal": principal.to_string(), "role": role.as_str() })).await
}

/// Remove `principal` from the team. Refused with `LastManager` when they are its only active
/// team manager. Returns `false` when they were not a member. Records `team.member_remove`.
pub async fn remove_team_member(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    principal: PrincipalId,
    actor: PrincipalId,
) -> Result<bool, TeamError> {
    lock_team(transaction, team_id).await?;
    let Some((role, state)) = member_role(transaction, team_id, principal).await? else {
        return Ok(false);
    };
    if state == "deleted" {
        return Ok(false);
    }
    if role == TeamRole::TeamManager && state == "active" && other_active_managers(transaction, team_id, principal).await? == 0 {
        return Err(TeamError::LastManager);
    }
    sqlx::query(
        "UPDATE team_members SET state = 'deleted', updated_at = now() \
         WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND principal_id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .bind(principal.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    audit(transaction, actor, "team.member_remove", team_id, serde_json::json!({ "principal": principal.to_string(), "role": role.as_str() }))
        .await?;
    Ok(true)
}

/// Change an active member's role. Demoting the only active team manager is `LastManager`; a
/// non-member is `NotFound`. Setting the current role is a no-op. Records `team.member_role`.
pub async fn change_team_member_role(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    principal: PrincipalId,
    role: TeamRole,
    actor: PrincipalId,
) -> Result<(), TeamError> {
    lock_active_team(transaction, team_id).await?;
    let (current, state) = member_role(transaction, team_id, principal).await?.ok_or(TeamError::NotFound)?;
    if state != "active" {
        return Err(TeamError::NotFound);
    }
    if current == role {
        return Ok(());
    }
    if current == TeamRole::TeamManager && other_active_managers(transaction, team_id, principal).await? == 0 {
        return Err(TeamError::LastManager);
    }
    sqlx::query(
        "UPDATE team_members SET role = $4, updated_at = now() WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND principal_id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .bind(principal.to_string())
    .bind(role.as_str())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    audit(
        transaction,
        actor,
        "team.member_role",
        team_id,
        serde_json::json!({ "principal": principal.to_string(), "from": current.as_str(), "to": role.as_str() }),
    )
    .await
}

/// The team's active members ordered by principal id, keyset-paged.
pub async fn list_team_members(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    after: Option<PrincipalId>,
    size: PageSize,
) -> Result<Page<TeamMemberRecord, PrincipalId>, TeamError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT principal_id::text, role FROM team_members WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND state = 'active' \
           AND ($3::uuid IS NULL OR principal_id > $3::uuid) ORDER BY principal_id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let items = rows
        .into_iter()
        .map(|(principal, role)| {
            Ok(TeamMemberRecord {
                principal_id: principal.parse().map_err(|_| TeamError::CorruptRow)?,
                role: TeamRole::parse(&role).ok_or(TeamError::CorruptRow)?,
            })
        })
        .collect::<Result<Vec<_>, TeamError>>()?;
    Ok(page(items, size, |item| item.principal_id))
}

/// The kind of tenant resource a team can own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnedResource {
    Workspace(WorkspaceId),
    Project(Uuid),
    Library(Uuid),
}

/// Set (or with `None`, clear) the team that owns a workspace, project or library. The new owner
/// must be an active team in this tenant. Records `team.ownership_set` against the new owner (or
/// `team.ownership_clear` against the previous one).
pub async fn set_owning_team(
    transaction: &mut TenantTransaction<'_>,
    resource: OwnedResource,
    team_id: Option<Uuid>,
    actor: PrincipalId,
) -> Result<(), TeamError> {
    if let Some(team_id) = team_id {
        lock_active_team(transaction, team_id).await?;
    }
    let (kind, resource_id, previous): (&str, Uuid, Option<Option<String>>) = match resource {
        OwnedResource::Workspace(id) => (
            "workspace",
            id.as_uuid(),
            sqlx::query_scalar(
                "WITH old AS (SELECT owning_team_id FROM workspaces WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE) \
                 UPDATE workspaces w SET owning_team_id = $3::uuid, updated_at = now() FROM old \
                 WHERE w.tenant_id = $1::uuid AND w.id = $2::uuid RETURNING old.owning_team_id::text",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .bind(team_id.map(|id| id.to_string()))
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?,
        ),
        OwnedResource::Project(id) => (
            "project",
            id,
            sqlx::query_scalar(
                "WITH old AS (SELECT owning_team_id FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE) \
                 UPDATE projects p SET owning_team_id = $3::uuid, updated_at = now() FROM old \
                 WHERE p.tenant_id = $1::uuid AND p.id = $2::uuid RETURNING old.owning_team_id::text",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .bind(team_id.map(|id| id.to_string()))
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?,
        ),
        OwnedResource::Library(id) => (
            "library",
            id,
            sqlx::query_scalar(
                "WITH old AS (SELECT owning_team_id FROM libraries WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE) \
                 UPDATE libraries l SET owning_team_id = $3::uuid, updated_at = now() FROM old \
                 WHERE l.tenant_id = $1::uuid AND l.id = $2::uuid RETURNING old.owning_team_id::text",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .bind(team_id.map(|id| id.to_string()))
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?,
        ),
    };
    let previous = previous.ok_or(TeamError::ResourceNotFound)?;
    let previous = previous.map(|value| Uuid::parse_str(&value).map_err(|_| TeamError::CorruptRow)).transpose()?;
    if previous == team_id {
        return Ok(());
    }
    let metadata = serde_json::json!({ "resource_kind": kind, "resource_id": resource_id.to_string(),
        "previous_team": previous.map(|id| id.to_string()) });
    match (team_id, previous) {
        (Some(team), _) => audit(transaction, actor, "team.ownership_set", team, metadata).await,
        (None, Some(team)) => audit(transaction, actor, "team.ownership_clear", team, metadata).await,
        (None, None) => Ok(()),
    }
}

/// A grant a team can hold on a workspace or library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamGrant {
    Workspace(WorkspaceId, WorkspaceGrantRole),
    Library(Uuid, LibraryGrantRole),
}

/// Grant an active team a role on a workspace or library, replacing any existing grant's role.
/// Records `team.grant_set`.
pub async fn grant_to_team(transaction: &mut TenantTransaction<'_>, team_id: Uuid, grant: TeamGrant, actor: PrincipalId) -> Result<(), TeamError> {
    lock_active_team(transaction, team_id).await?;
    let (kind, resource_id, role) = match grant {
        TeamGrant::Workspace(id, role) => {
            sqlx::query(
                "INSERT INTO workspace_team_grants (tenant_id, workspace_id, team_id, role) VALUES ($1::uuid, $2::uuid, $3::uuid, $4) \
                 ON CONFLICT (tenant_id, workspace_id, team_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now()",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .bind(team_id.to_string())
            .bind(role.as_str())
            .execute(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
            ("workspace", id.as_uuid(), role.as_str())
        }
        TeamGrant::Library(id, role) => {
            sqlx::query(
                "INSERT INTO library_team_grants (tenant_id, library_id, team_id, role) VALUES ($1::uuid, $2::uuid, $3::uuid, $4) \
                 ON CONFLICT (tenant_id, library_id, team_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now()",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .bind(team_id.to_string())
            .bind(role.as_str())
            .execute(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
            ("library", id, role.as_str())
        }
    };
    audit(
        transaction,
        actor,
        "team.grant_set",
        team_id,
        serde_json::json!({ "resource_kind": kind, "resource_id": resource_id.to_string(), "role": role }),
    )
    .await
}

/// The resource a team grant is on, for revocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantTarget {
    Workspace(WorkspaceId),
    Library(Uuid),
}

/// Remove a team's grant on a workspace or library. Returns `false` when there was none. Records
/// `team.grant_revoke`. Works on archived teams too, so access can always be withdrawn.
pub async fn revoke_team_grant(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    target: GrantTarget,
    actor: PrincipalId,
) -> Result<bool, TeamError> {
    lock_team(transaction, team_id).await?;
    let (kind, resource_id, affected) = match target {
        GrantTarget::Workspace(id) => (
            "workspace",
            id.as_uuid(),
            sqlx::query("DELETE FROM workspace_team_grants WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND team_id = $3::uuid")
                .bind(transaction.tenant_id().to_string())
                .bind(id.to_string())
                .bind(team_id.to_string())
                .execute(transaction.connection())
                .await
                .map_err(|error| database_error(&error))?
                .rows_affected(),
        ),
        GrantTarget::Library(id) => (
            "library",
            id,
            sqlx::query("DELETE FROM library_team_grants WHERE tenant_id = $1::uuid AND library_id = $2::uuid AND team_id = $3::uuid")
                .bind(transaction.tenant_id().to_string())
                .bind(id.to_string())
                .bind(team_id.to_string())
                .execute(transaction.connection())
                .await
                .map_err(|error| database_error(&error))?
                .rows_affected(),
        ),
    };
    if affected == 0 {
        return Ok(false);
    }
    audit(transaction, actor, "team.grant_revoke", team_id, serde_json::json!({ "resource_kind": kind, "resource_id": resource_id.to_string() }))
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::TeamRole;

    #[test]
    fn team_roles_round_trip() {
        for role in [TeamRole::TeamManager, TeamRole::Editor, TeamRole::Commenter, TeamRole::Viewer] {
            assert_eq!(TeamRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(TeamRole::parse("tenant_owner"), None);
    }
}
