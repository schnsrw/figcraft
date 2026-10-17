//! Effective-role resolution (S4-012, S4-010).
//!
//! Resolves, inside one tenant transaction, everything `figcraft_cloud_auth::authz::decide` needs
//! about a principal: principal state, tenant state, active tenant role, explicit sensitive
//! permissions, and on a workspace, project or document the resource's combined lifecycle state
//! and the principal's *strongest* role from every source:
//!
//! * the tenant role: `tenant_owner` and `tenant_admin` act as `workspace_admin` on every
//!   resource; `editor`, `commenter` and `viewer` grant no resource access by themselves;
//! * direct `workspace_members` (applies to the workspace, its projects and their documents);
//! * `workspace_team_grants` through an active team membership in an active team;
//! * direct `document_members` and `document_team_grants` (that document only).
//!
//! Team grants are capped by the member's team role: a team `editor` gets at most `editor` from a
//! `workspace_admin` grant, `commenter`/`viewer` members at most their role, and a
//! `team_manager` receives the grant's role (managing a team never adds more than the grant).
//! A `commenter` or `viewer` *tenant* role is a ceiling over every resource role (guests stay
//! guests); `editor` tenant members are not capped.
//!
//! Nothing is granted when the principal is not active, the tenant is not `active`, or the tenant
//! membership is not `active`: the role is `None` and the permission set is empty. The combined
//! resource state is the most restrictive state along document → project → workspace. A direct
//! membership never *downgrades* a stronger grant from another source.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, ResourceState, Role, WorkspaceId};

use crate::TenantTransaction;
use crate::memberships::{MemberPermission, MembershipError, parse_role};

/// Where an effective role came from (for audit "effective grant" records).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RoleSource {
    Tenant,
    WorkspaceMember,
    WorkspaceTeamGrant,
    DocumentMember,
    DocumentTeamGrant,
}

impl RoleSource {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "workspace_member" => Some(Self::WorkspaceMember),
            "workspace_team" => Some(Self::WorkspaceTeamGrant),
            "document_member" => Some(Self::DocumentMember),
            "document_team" => Some(Self::DocumentTeamGrant),
            _ => None,
        }
    }
}

/// Tenant-level facts for one principal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantAccess {
    pub principal_active: bool,
    pub tenant_state: ResourceState,
    /// The tenant role, only when principal, tenant and membership are all active.
    pub tenant_role: Option<Role>,
    /// Explicit sensitive grants; empty unless `tenant_role` is `Some`.
    pub permissions: Vec<MemberPermission>,
}

impl TenantAccess {
    fn grants(&self) -> bool {
        self.tenant_role.is_some()
    }
}

/// Facts for one principal on one workspace, project or document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceAccess {
    pub tenant: TenantAccess,
    /// Most restrictive state along document → project → workspace.
    pub state: ResourceState,
    /// Strongest effective role, `None` when nothing grants access.
    pub role: Option<Role>,
    /// The source(s) that supplied `role` (every source whose role equals it).
    pub sources: Vec<RoleSource>,
}

fn parse_state(value: &str) -> Result<ResourceState, MembershipError> {
    match value {
        "pending" => Ok(ResourceState::Pending),
        "active" => Ok(ResourceState::Active),
        "suspended" => Ok(ResourceState::Suspended),
        "archived" => Ok(ResourceState::Archived),
        "deleting" => Ok(ResourceState::Deleting),
        "deleted" => Ok(ResourceState::Deleted),
        _ => Err(MembershipError::CorruptRow),
    }
}

/// Higher is more restrictive.
const fn restrictiveness(state: ResourceState) -> u8 {
    match state {
        ResourceState::Active => 0,
        ResourceState::Archived => 1,
        ResourceState::Pending => 2,
        ResourceState::Suspended => 3,
        ResourceState::Deleting => 4,
        ResourceState::Deleted => 5,
    }
}

/// The most restrictive of `states`.
#[must_use]
pub fn combined_state(states: &[ResourceState]) -> ResourceState {
    states.iter().copied().max_by_key(|state| restrictiveness(*state)).unwrap_or(ResourceState::Deleted)
}

/// Resource-scope rank (0 grants nothing).
#[must_use]
pub const fn resource_rank(role: Role) -> u8 {
    match role {
        Role::TenantOwner | Role::TenantAdmin | Role::WorkspaceAdmin => 4,
        Role::Editor => 3,
        Role::Commenter => 2,
        Role::Viewer => 1,
        Role::TeamManager => 0,
    }
}

const fn rank_role(rank: u8) -> Option<Role> {
    match rank {
        4 => Some(Role::WorkspaceAdmin),
        3 => Some(Role::Editor),
        2 => Some(Role::Commenter),
        1 => Some(Role::Viewer),
        _ => None,
    }
}

/// The role a team grant gives a member holding `team_role` in the granted team.
#[must_use]
pub const fn capped_team_grant(grant: Role, team_role: Role) -> Option<Role> {
    let cap = match team_role {
        Role::TeamManager => 4,
        other => resource_rank(other),
    };
    let grant_rank = resource_rank(grant);
    rank_role(if grant_rank < cap { grant_rank } else { cap })
}

/// The resource role the tenant membership gives on its own.
#[must_use]
pub const fn tenant_baseline(tenant_role: Role) -> Option<Role> {
    match tenant_role {
        Role::TenantOwner | Role::TenantAdmin => Some(Role::WorkspaceAdmin),
        _ => None,
    }
}

/// The ceiling a tenant role places on every resource role.
#[must_use]
pub const fn tenant_ceiling(tenant_role: Role) -> u8 {
    match tenant_role {
        Role::Commenter => 2,
        Role::Viewer => 1,
        _ => 4,
    }
}

/// Combine candidate `(source, role)` pairs into the strongest role and its sources.
#[must_use]
pub fn strongest(tenant_role: Role, candidates: &[(RoleSource, Role)]) -> (Option<Role>, Vec<RoleSource>) {
    let ceiling = tenant_ceiling(tenant_role);
    let mut all: Vec<(RoleSource, u8)> = candidates.iter().map(|(source, role)| (*source, resource_rank(*role).min(ceiling))).collect();
    if let Some(baseline) = tenant_baseline(tenant_role) {
        all.push((RoleSource::Tenant, resource_rank(baseline).min(ceiling)));
    }
    let best = all.iter().map(|(_, rank)| *rank).max().unwrap_or(0);
    let Some(role) = rank_role(best) else {
        return (None, Vec::new());
    };
    let mut sources: Vec<RoleSource> = Vec::new();
    for (source, rank) in &all {
        if *rank == best && !sources.contains(source) {
            sources.push(*source);
        }
    }
    (Some(role), sources)
}

/// Tenant-level facts for `principal_id` in the transaction's tenant. `None` when the tenant or
/// the principal does not exist.
pub async fn resolve_tenant_access(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
) -> Result<Option<TenantAccess>, MembershipError> {
    let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.state, p.state, m.role, m.state FROM tenants t CROSS JOIN principals p \
         LEFT JOIN tenant_members m ON m.tenant_id = t.id AND m.principal_id = p.id \
         WHERE t.id = $1::uuid AND p.id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| MembershipError::Database)?;
    let Some((tenant_state, principal_state, member_role, member_state)) = row else {
        return Ok(None);
    };
    let tenant_state = parse_state(&tenant_state)?;
    let principal_active = principal_state == "active";
    let member_role = member_role.map(|role| parse_role(&role).ok_or(MembershipError::CorruptRow)).transpose()?;
    let tenant_role =
        if principal_active && tenant_state == ResourceState::Active && member_state.as_deref() == Some("active") { member_role } else { None };
    let permissions = if tenant_role.is_some() {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT permission FROM member_permissions WHERE tenant_id = $1::uuid AND principal_id = $2::uuid ORDER BY permission LIMIT 16",
        )
        .bind(transaction.tenant_id().to_string())
        .bind(principal_id.to_string())
        .fetch_all(transaction.connection())
        .await
        .map_err(|_| MembershipError::Database)?;
        rows.iter().map(|value| MemberPermission::parse(value).ok_or(MembershipError::CorruptRow)).collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(Some(TenantAccess { principal_active, tenant_state, tenant_role, permissions }))
}

/// Candidate roles on a workspace (and, when `document` is given, that document).
async fn candidates(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    workspace: &str,
    document: Option<&str>,
) -> Result<Vec<(RoleSource, Role)>, MembershipError> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT 'workspace_member', role, NULL::text FROM workspace_members \
           WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND principal_id = $4::uuid \
         UNION ALL \
         SELECT 'workspace_team', g.role, tm.role FROM workspace_team_grants g \
           JOIN teams t ON t.tenant_id = g.tenant_id AND t.id = g.team_id AND t.state = 'active' \
           JOIN team_members tm ON tm.tenant_id = g.tenant_id AND tm.team_id = g.team_id AND tm.state = 'active' \
           WHERE g.tenant_id = $1::uuid AND g.workspace_id = $2::uuid AND tm.principal_id = $4::uuid \
         UNION ALL \
         SELECT 'document_member', role, NULL::text FROM document_members \
           WHERE tenant_id = $1::uuid AND document_id = $3::uuid AND principal_id = $4::uuid \
         UNION ALL \
         SELECT 'document_team', g.role, tm.role FROM document_team_grants g \
           JOIN teams t ON t.tenant_id = g.tenant_id AND t.id = g.team_id AND t.state = 'active' \
           JOIN team_members tm ON tm.tenant_id = g.tenant_id AND tm.team_id = g.team_id AND tm.state = 'active' \
           WHERE g.tenant_id = $1::uuid AND g.document_id = $3::uuid AND tm.principal_id = $4::uuid \
         LIMIT 512",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace)
    .bind(document)
    .bind(principal_id.to_string())
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| MembershipError::Database)?;
    let mut out = Vec::with_capacity(rows.len());
    for (source, role, team_role) in rows {
        let source = RoleSource::parse(&source).ok_or(MembershipError::CorruptRow)?;
        let role = parse_role(&role).ok_or(MembershipError::CorruptRow)?;
        let role = match team_role {
            None => Some(role),
            Some(team_role) => capped_team_grant(role, parse_role(&team_role).ok_or(MembershipError::CorruptRow)?),
        };
        if let Some(role) = role {
            out.push((source, role));
        }
    }
    Ok(out)
}

async fn resolve(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    states: Vec<ResourceState>,
    workspace: String,
    document: Option<String>,
) -> Result<Option<ResourceAccess>, MembershipError> {
    let Some(tenant) = resolve_tenant_access(transaction, principal_id).await? else {
        return Ok(None);
    };
    let state = combined_state(&states);
    let (role, sources) = match tenant.tenant_role {
        Some(tenant_role) if tenant.grants() => {
            let found = candidates(transaction, principal_id, &workspace, document.as_deref()).await?;
            strongest(tenant_role, &found)
        }
        _ => (None, Vec::new()),
    };
    Ok(Some(ResourceAccess { tenant, state, role, sources }))
}

/// Facts for `principal_id` on a workspace. `None` when the workspace, tenant or principal does
/// not exist in this tenant.
pub async fn resolve_workspace_access(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    workspace_id: WorkspaceId,
) -> Result<Option<ResourceAccess>, MembershipError> {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM workspaces WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(workspace_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| MembershipError::Database)?;
    let Some(state) = state else {
        return Ok(None);
    };
    resolve(transaction, principal_id, vec![parse_state(&state)?], workspace_id.to_string(), None).await
}

/// Facts for `principal_id` on a project (inherits the workspace's grants).
pub async fn resolve_project_access(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    project_id: ProjectId,
) -> Result<Option<ResourceAccess>, MembershipError> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT p.state, w.state, w.id::text FROM projects p JOIN workspaces w ON w.tenant_id = p.tenant_id AND w.id = p.workspace_id \
         WHERE p.tenant_id = $1::uuid AND p.id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(project_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| MembershipError::Database)?;
    let Some((project_state, workspace_state, workspace)) = row else {
        return Ok(None);
    };
    resolve(transaction, principal_id, vec![parse_state(&project_state)?, parse_state(&workspace_state)?], workspace, None).await
}

/// Facts for `principal_id` on a document (workspace grants plus the document's own).
pub async fn resolve_document_access(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    document_id: CloudDocumentId,
) -> Result<Option<ResourceAccess>, MembershipError> {
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT d.state, p.state, w.state, w.id::text FROM documents d \
         JOIN projects p ON p.tenant_id = d.tenant_id AND p.id = d.project_id \
         JOIN workspaces w ON w.tenant_id = d.tenant_id AND w.id = d.workspace_id \
         WHERE d.tenant_id = $1::uuid AND d.id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| MembershipError::Database)?;
    let Some((document_state, project_state, workspace_state, workspace)) = row else {
        return Ok(None);
    };
    let states = vec![parse_state(&document_state)?, parse_state(&project_state)?, parse_state(&workspace_state)?];
    resolve(transaction, principal_id, states, workspace, Some(document_id.to_string())).await
}

/// The principal's active role in a team (`None` when not an active member of an active team),
/// plus the team's state. `None` overall when the team does not exist in this tenant.
pub async fn resolve_team_role(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    team_id: figcraft_cloud_domain::Uuid,
) -> Result<Option<(ResourceState, Option<Role>)>, MembershipError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT t.state, tm.role FROM teams t \
         LEFT JOIN team_members tm ON tm.tenant_id = t.tenant_id AND tm.team_id = t.id AND tm.principal_id = $3::uuid AND tm.state = 'active' \
         WHERE t.tenant_id = $1::uuid AND t.id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .bind(principal_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| MembershipError::Database)?;
    let Some((state, role)) = row else {
        return Ok(None);
    };
    let role = role.map(|role| parse_role(&role).ok_or(MembershipError::CorruptRow)).transpose()?;
    Ok(Some((parse_state(&state)?, role)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strongest_role_wins_and_direct_viewer_never_downgrades() {
        let (role, sources) = strongest(Role::Editor, &[(RoleSource::DocumentMember, Role::Viewer), (RoleSource::WorkspaceTeamGrant, Role::Editor)]);
        assert_eq!(role, Some(Role::Editor));
        assert_eq!(sources, vec![RoleSource::WorkspaceTeamGrant]);
        assert_eq!(strongest(Role::Editor, &[]), (None, Vec::new()));
        // Duplicate sources (two team grants) are reported once, even when not adjacent.
        let (_, sources) = strongest(
            Role::Editor,
            &[
                (RoleSource::WorkspaceTeamGrant, Role::Editor),
                (RoleSource::DocumentMember, Role::Editor),
                (RoleSource::WorkspaceTeamGrant, Role::Editor),
            ],
        );
        assert_eq!(sources, vec![RoleSource::WorkspaceTeamGrant, RoleSource::DocumentMember]);
        assert_eq!(strongest(Role::TenantAdmin, &[]).0, Some(Role::WorkspaceAdmin));
        // Guests are capped by their tenant role.
        assert_eq!(strongest(Role::Viewer, &[(RoleSource::DocumentMember, Role::Editor)]).0, Some(Role::Viewer));
        assert_eq!(strongest(Role::Commenter, &[(RoleSource::WorkspaceMember, Role::WorkspaceAdmin)]).0, Some(Role::Commenter));
        // Editors are not capped.
        assert_eq!(strongest(Role::Editor, &[(RoleSource::WorkspaceMember, Role::WorkspaceAdmin)]).0, Some(Role::WorkspaceAdmin));
    }

    #[test]
    fn team_grants_are_capped_by_the_team_role() {
        assert_eq!(capped_team_grant(Role::WorkspaceAdmin, Role::TeamManager), Some(Role::WorkspaceAdmin));
        assert_eq!(capped_team_grant(Role::Viewer, Role::TeamManager), Some(Role::Viewer));
        assert_eq!(capped_team_grant(Role::WorkspaceAdmin, Role::Editor), Some(Role::Editor));
        assert_eq!(capped_team_grant(Role::Editor, Role::Viewer), Some(Role::Viewer));
        assert_eq!(capped_team_grant(Role::Commenter, Role::Editor), Some(Role::Commenter));
    }

    #[test]
    fn combined_state_is_the_most_restrictive() {
        use ResourceState as S;
        assert_eq!(combined_state(&[S::Active, S::Active]), S::Active);
        assert_eq!(combined_state(&[S::Active, S::Archived, S::Active]), S::Archived);
        assert_eq!(combined_state(&[S::Archived, S::Deleting]), S::Deleting);
        assert_eq!(combined_state(&[S::Suspended, S::Archived]), S::Suspended);
        assert_eq!(combined_state(&[]), S::Deleted);
    }
}
