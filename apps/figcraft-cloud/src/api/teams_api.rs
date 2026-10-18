//! Team, team-member, team-grant and invitation APIs (S4-019).
//!
//! Routes (under `/api/v1/tenants/{tenant}` unless noted):
//!
//! | Method and path | Action |
//! |---|---|
//! | `GET /teams?q=` | `member.view` (prefix search on name, keyset on `(name, id)`) |
//! | `POST /teams` | `team.create` (the creator becomes the first team manager) |
//! | `GET /teams/{team}` | `team.view` |
//! | `PATCH /teams/{team}` | rename or archive: `team.manage` |
//! | `DELETE /teams/{team}` | archive: `team.manage` |
//! | `GET /teams/{team}/members?q=` | `team.view` (prefix search on display name, keyset on `(display_name, principal)`) |
//! | `POST /teams/{team}/members` | `team.manage` |
//! | `PATCH`/`DELETE /teams/{team}/members/{principal}` | `team.manage` |
//! | `PUT /teams/{team}/grants/workspaces/{workspace}` | `team.manage` and `workspace.manage_members` |
//! | `DELETE /teams/{team}/grants/workspaces/{workspace}` | `workspace.manage_members` (works on archived teams) |
//! | `PUT`/`DELETE /teams/{team}/owned/workspaces/{workspace}` | `team.manage` and `workspace.manage_members` |
//! | `PUT`/`DELETE /teams/{team}/owned/projects/{project}` | `team.manage` and `workspace.manage_members` on its workspace |
//! | `GET /invitations` | `member.manage` (every pending invitation in the tenant) |
//! | `GET /teams/{team}/invitations` | `team.manage` |
//! | `POST /teams/{team}/invitations` | `team.manage` and `member.manage` (returns the token once) |
//! | `DELETE /teams/{team}/invitations/{invitation}` | `team.manage` |
//! | `POST /api/v1/invitations/accept` | a browser session; the token names the tenant |
//!
//! Disclosure: a team the caller cannot view (`team.view` denied: not a member and not a tenant
//! owner or admin, or the team is archived) is `404`, a visible team without the permission
//! `403`; a workspace or project the caller holds no role on is `404`.
//!
//! An invitation adds its acceptor to the *tenant* (as `viewer` when new), so creating one needs
//! the tenant's `member.manage` as well as the team's `team.manage`.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use figcraft_cloud_auth::authz::{Action, Resource, Subject, decide};
use figcraft_cloud_domain::policy::Inviter;
use figcraft_cloud_domain::{ErrorCode, PrincipalId, ProjectId, ResourceState, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::TenantTransaction;
use figcraft_cloud_storage::effective_roles::{resolve_team_role, resolve_workspace_access};
use figcraft_cloud_storage::invitations::{self, InvitationError, InvitationRecord};
use figcraft_cloud_storage::metadata::PageSize;
use figcraft_cloud_storage::teams::{self, GrantTarget, OwnedResource, TeamError, TeamGrant, TeamRecord, TeamRole, WorkspaceGrantRole};
use figcraft_cloud_storage::workspace_policy;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Authenticated, CredentialKind};
use super::cursor::{CursorScope, ListQuery, PageBody, PageRequest, Position, SearchTerm, invalid_cursor};
use super::error::{ApiError, ApiJson, denied};
use super::tenants_api::{
    TenantCall, commit, conflict, invalid, open, path_principal, path_uuid, require_tenant, revoke_after_commit, storage_unavailable,
};
use figcraft_cloud_storage::revocation::AccessRevocation;

pub(crate) fn routes() -> Router<AppState> {
    let tenant = "/tenants/{tenant}";
    Router::new()
        .route(&format!("{tenant}/teams"), get(list_teams).post(create_team))
        .route(&format!("{tenant}/teams/{{team}}"), get(get_team).patch(update_team).delete(archive_team))
        .route(&format!("{tenant}/teams/{{team}}/members"), get(list_team_members).post(add_team_member))
        .route(&format!("{tenant}/teams/{{team}}/members/{{principal}}"), axum::routing::patch(update_team_member).delete(remove_team_member))
        .route(&format!("{tenant}/teams/{{team}}/grants/workspaces/{{workspace}}"), put(grant_workspace).delete(revoke_workspace))
        .route(&format!("{tenant}/teams/{{team}}/owned/workspaces/{{workspace}}"), put(own_workspace).delete(disown_workspace))
        .route(&format!("{tenant}/teams/{{team}}/owned/projects/{{project}}"), put(own_project).delete(disown_project))
        .route(&format!("{tenant}/invitations"), get(list_tenant_invitations))
        .route(&format!("{tenant}/teams/{{team}}/invitations"), get(list_team_invitations).post(create_invitation))
        .route(&format!("{tenant}/teams/{{team}}/invitations/{{invitation}}"), axum::routing::delete(revoke_invitation))
        .route("/invitations/accept", post(accept_invitation))
}

pub(super) fn team_error(error: TeamError) -> ApiError {
    match error {
        TeamError::InvalidSlug | TeamError::InvalidName | TeamError::InvalidPageSize => invalid(),
        TeamError::Conflict | TeamError::InvalidState | TeamError::NotTenantMember | TeamError::LastManager => conflict(),
        TeamError::NotFound | TeamError::ResourceNotFound => ApiError::not_found(),
        TeamError::Database => ApiError::unavailable(),
        TeamError::Audit | TeamError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn invitation_error(error: InvitationError) -> ApiError {
    match error {
        InvitationError::InvalidEmail | InvitationError::InvalidLifetime | InvitationError::InvalidPageSize => invalid(),
        InvitationError::TeamUnavailable => conflict(),
        // Token holders cannot tell an unknown token from any other refusal.
        InvitationError::Refused | InvitationError::EmailMismatch | InvitationError::PrincipalUnavailable | InvitationError::TenantInactive => {
            ApiError::not_found()
        }
        InvitationError::Database => ApiError::unavailable(),
        InvitationError::Random | InvitationError::Audit | InvitationError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn page_size(limit: u32) -> Result<PageSize, ApiError> {
    PageSize::new(limit).map_err(|_| invalid())
}

fn team_role(value: &str) -> Result<TeamRole, ApiError> {
    TeamRole::parse(value).ok_or_else(invalid)
}

/// Resolve `team` for the caller and authorize `action` on it under the disclosure policy.
pub(super) async fn authorize_team(call: &mut TenantCall<'_>, team: &str, action: Action) -> Result<Uuid, ApiError> {
    let id = path_uuid(team)?;
    let (state, member_role) =
        resolve_team_role(&mut call.transaction, call.principal, id).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    let resource = Resource::Team { state, member_role };
    if decide(&call.subject, &resource, action).allow {
        return Ok(id);
    }
    Err(denied(decide(&call.subject, &resource, Action::TeamView).allow))
}

/// Who is inviting to `team`: a workspace administrator, the team's lead, or an editor-level member
/// of the team. Anyone who cannot even see the team gets `404`; a visible team member with no right to
/// invite `403`.
async fn authorize_inviter(call: &mut TenantCall<'_>, team: &str) -> Result<(Uuid, Inviter), ApiError> {
    let id = path_uuid(team)?;
    let (state, member_role) =
        resolve_team_role(&mut call.transaction, call.principal, id).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    let admin = decide(&call.subject, &Resource::Tenant, Action::MemberManage).allow;
    let resource = Resource::Team { state, member_role };
    if !decide(&call.subject, &resource, Action::TeamView).allow {
        return Err(ApiError::not_found());
    }
    let inviter = if admin {
        Inviter::Admin
    } else {
        match member_role {
            Some(Role::TeamManager) => Inviter::TeamLead,
            Some(Role::Editor) => Inviter::TeamMember,
            _ => return Err(ApiError::forbidden()),
        }
    };
    if state != ResourceState::Active {
        return Err(conflict());
    }
    Ok((id, inviter))
}

/// Authorize `workspace.manage_members` on `workspace` (`404` when the caller holds no role).
async fn authorize_workspace_admin(
    transaction: &mut TenantTransaction<'_>,
    subject: &Subject,
    principal: PrincipalId,
    workspace: WorkspaceId,
) -> Result<(), ApiError> {
    let access = resolve_workspace_access(transaction, principal, workspace).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    let resource = Resource::Workspace { state: access.state, role: access.role };
    if decide(subject, &resource, Action::WorkspaceManageMembers).allow {
        return Ok(());
    }
    Err(denied(decide(subject, &resource, Action::WorkspaceView).allow))
}

fn path_workspace(value: &str) -> Result<WorkspaceId, ApiError> {
    WorkspaceId::try_from(path_uuid(value)?).map_err(|_| ApiError::not_found())
}

// ---------------------------------------------------------------------------------------------
// Teams.

#[derive(Debug, Serialize)]
struct TeamBody {
    id: Uuid,
    slug: String,
    name: String,
    state: ResourceState,
}

impl From<TeamRecord> for TeamBody {
    fn from(record: TeamRecord) -> Self {
        Self { id: record.id, slug: record.slug, name: record.name, state: record.state }
    }
}

async fn list_teams(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let search = SearchTerm::parse(query.q.as_deref())?;
    let tenant_text = call.transaction.tenant_id().to_string();
    let scope = CursorScope::new("teams", &[&tenant_text, search.as_ref().map_or("", SearchTerm::as_str)]);
    let request = PageRequest::new(scope, query.limit, query.cursor.as_deref())?;
    let (after_name, after_id) = request.after_binds();
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT id::text, slug, name, state FROM teams \
         WHERE tenant_id = $1::uuid AND state NOT IN ('deleting', 'deleted') \
           AND ($2::text IS NULL OR name ILIKE $2 ESCAPE '\\') \
           AND ($3::text IS NULL OR (name COLLATE \"C\", id) > ($3::text COLLATE \"C\", $4::uuid)) \
         ORDER BY name COLLATE \"C\", id LIMIT $5",
    )
    .bind(&tenant_text)
    .bind(search.as_ref().map(SearchTerm::prefix_pattern))
    .bind(after_name)
    .bind(after_id)
    .bind(request.fetch_limit())
    .fetch_all(call.transaction.connection())
    .await
    .map_err(storage_unavailable)?;
    let mut found = Vec::with_capacity(rows.len());
    for (id, slug, name, team_state) in rows {
        let id = Uuid::parse_str(&id).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let team_state: ResourceState =
            serde_json::from_value(serde_json::Value::String(team_state)).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        found.push(TeamBody { id, slug, name, state: team_state });
    }
    let (items, next_cursor) = request.finish(found, |team| Position { sort_key: team.name.clone(), id: team.id });
    Ok(axum::Json(PageBody { items, next_cursor }).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTeam {
    slug: String,
    name: String,
}

async fn create_team(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    ApiJson(body): ApiJson<CreateTeam>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::TeamCreate)?;
    let record = teams::create_team(&mut call.transaction, Uuid::new_v4(), &body.slug, &body.name, call.principal).await.map_err(team_error)?;
    commit(call.transaction).await?;
    Ok((StatusCode::CREATED, axum::Json(TeamBody::from(record))).into_response())
}

async fn get_team(State(state): State<AppState>, caller: Authenticated, Path((tenant, team)): Path<(String, String)>) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamView).await?;
    let record = teams::get_team(&mut call.transaction, id).await.map_err(team_error)?.ok_or(ApiError::not_found())?;
    Ok(axum::Json(TeamBody::from(record)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateTeam {
    name: Option<String>,
    /// Only `archived`.
    state: Option<String>,
}

async fn update_team(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateTeam>,
) -> Result<Response, ApiError> {
    let archive = match body.state.as_deref() {
        None => false,
        Some("archived") => true,
        Some(_) => return Err(invalid()),
    };
    if body.name.is_none() && !archive {
        return Err(invalid());
    }
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    let mut record = None;
    if let Some(name) = &body.name {
        record = Some(teams::rename_team(&mut call.transaction, id, name, call.principal).await.map_err(team_error)?);
    }
    if archive {
        record = Some(teams::archive_team(&mut call.transaction, id, call.principal).await.map_err(team_error)?);
    }
    commit(call.transaction).await?;
    if archive {
        revoke_after_commit(&state, AccessRevocation::TeamArchived { team: id }).await;
    }
    let record = record.ok_or_else(invalid)?;
    Ok(axum::Json(TeamBody::from(record)).into_response())
}

async fn archive_team(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    teams::archive_team(&mut call.transaction, id, call.principal).await.map_err(team_error)?;
    commit(call.transaction).await?;
    revoke_after_commit(&state, AccessRevocation::TeamArchived { team: id }).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------------------------
// Team members.

#[derive(Debug, Serialize)]
struct TeamMemberBody {
    principal_id: PrincipalId,
    display_name: String,
    role: &'static str,
}

/// `GET /teams/{team}/members?q=&limit=&cursor=`: the team's active members, ordered by
/// `(display_name COLLATE "C", principal_id)`. `q` is a bounded prefix search on the display name
/// ([`SearchTerm`]: at most 64 characters, backslash, `%` and `_` escaped); cursors are bound to the tenant,
/// the caller, the team and the search term.
async fn list_team_members(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamView).await?;
    let search = SearchTerm::parse(query.q.as_deref())?;
    let tenant_text = call.transaction.tenant_id().to_string();
    let principal_text = call.principal.to_string();
    let team_text = id.to_string();
    let scope =
        CursorScope::new("team_members_by_name", &[&tenant_text, &principal_text, &team_text, search.as_ref().map_or("", SearchTerm::as_str)]);
    let request = PageRequest::new(scope, query.limit, query.cursor.as_deref())?;
    let after = match request.after.as_ref() {
        Some(position) => Some((position.sort_key.as_str(), PrincipalId::try_from(position.id).map_err(|_| invalid_cursor())?)),
        None => None,
    };
    let pattern = search.as_ref().map(SearchTerm::prefix_pattern);
    let rows = teams::search_team_members(&mut call.transaction, id, pattern.as_deref(), after, request.fetch_limit()).await.map_err(team_error)?;
    let found = rows
        .into_iter()
        .map(|member| TeamMemberBody { principal_id: member.principal_id, display_name: member.display_name, role: member.role.as_str() })
        .collect();
    let (items, next_cursor) = request.finish(found, |member| Position { sort_key: member.display_name.clone(), id: member.principal_id.as_uuid() });
    Ok(axum::Json(PageBody { items, next_cursor }).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddTeamMember {
    principal_id: PrincipalId,
    role: String,
}

async fn add_team_member(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team)): Path<(String, String)>,
    ApiJson(body): ApiJson<AddTeamMember>,
) -> Result<Response, ApiError> {
    let role = team_role(&body.role)?;
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    teams::add_team_member(&mut call.transaction, id, body.principal_id, role, call.principal).await.map_err(team_error)?;
    commit(call.transaction).await?;
    Ok(StatusCode::CREATED.into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateTeamMember {
    role: String,
}

async fn update_team_member(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, principal)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<UpdateTeamMember>,
) -> Result<Response, ApiError> {
    let role = team_role(&body.role)?;
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    let target = path_principal(&principal)?;
    teams::change_team_member_role(&mut call.transaction, id, target, role, call.principal).await.map_err(team_error)?;
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_team_member(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, principal)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    let target = path_principal(&principal)?;
    if !teams::remove_team_member(&mut call.transaction, id, target, call.principal).await.map_err(team_error)? {
        return Err(ApiError::not_found());
    }
    commit(call.transaction).await?;
    revoke_after_commit(&state, AccessRevocation::TeamMemberRemoved { team: id, principal: target }).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------------------------
// Team grants and ownership.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GrantBody {
    role: String,
}

impl GrantBody {
    pub(super) fn role(&self) -> &str {
        &self.role
    }
}

pub(super) fn workspace_grant_role(value: &str) -> Result<WorkspaceGrantRole, ApiError> {
    match value {
        "workspace_admin" => Ok(WorkspaceGrantRole::WorkspaceAdmin),
        "editor" => Ok(WorkspaceGrantRole::Editor),
        "commenter" => Ok(WorkspaceGrantRole::Commenter),
        "viewer" => Ok(WorkspaceGrantRole::Viewer),
        _ => Err(invalid()),
    }
}

async fn grant_workspace(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, workspace)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<GrantBody>,
) -> Result<Response, ApiError> {
    let role = workspace_grant_role(&body.role)?;
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    let workspace = path_workspace(&workspace)?;
    authorize_workspace_admin(&mut call.transaction, &call.subject, call.principal, workspace).await?;
    teams::grant_to_team(&mut call.transaction, id, TeamGrant::Workspace(workspace, role), call.principal).await.map_err(team_error)?;
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn revoke_workspace(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, workspace)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let team = path_uuid(&team)?;
    let workspace = path_workspace(&workspace)?;
    // Withdrawing access is the workspace admin's call and works on archived teams too.
    authorize_workspace_admin(&mut call.transaction, &call.subject, call.principal, workspace).await?;
    if !teams::revoke_team_grant(&mut call.transaction, team, GrantTarget::Workspace(workspace), call.principal).await.map_err(team_error)? {
        return Err(ApiError::not_found());
    }
    commit(call.transaction).await?;
    revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn set_workspace_owner(
    state: &AppState,
    caller: &Authenticated,
    tenant: &str,
    team: &str,
    workspace: &str,
    own: bool,
) -> Result<Response, ApiError> {
    let mut call = open(state, caller, tenant).await?;
    let id = authorize_team(&mut call, team, Action::TeamManage).await?;
    let workspace = path_workspace(workspace)?;
    authorize_workspace_admin(&mut call.transaction, &call.subject, call.principal, workspace).await?;
    let current = owning_team(&mut call.transaction, "workspace", workspace.as_uuid()).await?;
    if !own && current != Some(id) {
        return Err(ApiError::not_found());
    }
    teams::set_owning_team(&mut call.transaction, OwnedResource::Workspace(workspace), own.then_some(id), call.principal)
        .await
        .map_err(team_error)?;
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn own_workspace(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, workspace)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    set_workspace_owner(&state, &caller, &tenant, &team, &workspace, true).await
}

async fn disown_workspace(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, workspace)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    set_workspace_owner(&state, &caller, &tenant, &team, &workspace, false).await
}

/// The owning team of a workspace or project (`404` when the resource is missing).
async fn owning_team(transaction: &mut TenantTransaction<'_>, kind: &str, id: Uuid) -> Result<Option<Uuid>, ApiError> {
    let tenant = transaction.tenant_id().to_string();
    let row: Option<(Option<String>,)> = if kind == "workspace" {
        sqlx::query_as("SELECT owning_team_id::text FROM workspaces WHERE tenant_id = $1::uuid AND id = $2::uuid")
            .bind(tenant)
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
    } else {
        sqlx::query_as("SELECT owning_team_id::text FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid")
            .bind(tenant)
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
    }
    .map_err(storage_unavailable)?;
    let (team,) = row.ok_or(ApiError::not_found())?;
    team.map(|team| Uuid::parse_str(&team).map_err(|_| ApiError::new(ErrorCode::Internal))).transpose()
}

async fn set_project_owner(
    state: &AppState,
    caller: &Authenticated,
    tenant: &str,
    team: &str,
    project: &str,
    own: bool,
) -> Result<Response, ApiError> {
    let mut call = open(state, caller, tenant).await?;
    let id = authorize_team(&mut call, team, Action::TeamManage).await?;
    let project = ProjectId::try_from(path_uuid(project)?).map_err(|_| ApiError::not_found())?;
    let workspace: Option<String> = sqlx::query_scalar("SELECT workspace_id::text FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(call.transaction.tenant_id().to_string())
        .bind(project.to_string())
        .fetch_optional(call.transaction.connection())
        .await
        .map_err(storage_unavailable)?;
    let workspace = workspace.ok_or(ApiError::not_found())?;
    let workspace = path_workspace(&workspace).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    authorize_workspace_admin(&mut call.transaction, &call.subject, call.principal, workspace).await?;
    let current = owning_team(&mut call.transaction, "project", project.as_uuid()).await?;
    if !own && current != Some(id) {
        return Err(ApiError::not_found());
    }
    teams::set_owning_team(&mut call.transaction, OwnedResource::Project(project.as_uuid()), own.then_some(id), call.principal)
        .await
        .map_err(team_error)?;
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn own_project(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, project)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    set_project_owner(&state, &caller, &tenant, &team, &project, true).await
}

async fn disown_project(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, project)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    set_project_owner(&state, &caller, &tenant, &team, &project, false).await
}

// ---------------------------------------------------------------------------------------------
// Invitations.

#[derive(Debug, Serialize)]
struct InvitationBody {
    id: Uuid,
    team_id: Uuid,
    /// The address noted on the invitation; absent for a link-only invitation (TS-014).
    email: Option<String>,
    role: &'static str,
    invited_by: PrincipalId,
    expires_in_secs: i64,
}

impl From<InvitationRecord> for InvitationBody {
    fn from(record: InvitationRecord) -> Self {
        Self {
            id: record.id,
            team_id: record.team_id,
            email: record.email,
            role: record.role.as_str(),
            invited_by: record.invited_by,
            expires_in_secs: record.expires_in_secs,
        }
    }
}

async fn list_invitations(call: &mut TenantCall<'_>, team: Option<Uuid>, query: &ListQuery) -> Result<Response, ApiError> {
    let tenant_text = call.transaction.tenant_id().to_string();
    let team_text = team.map(|team| team.to_string()).unwrap_or_default();
    let request = PageRequest::new(CursorScope::new("invitations", &[&tenant_text, &team_text]), query.limit, query.cursor.as_deref())?;
    let after = request.after.as_ref().map(|position| position.id);
    let page =
        invitations::list_pending_invitations(&mut call.transaction, team, after, page_size(request.limit)?).await.map_err(invitation_error)?;
    let next_cursor = page.next_cursor.and_then(|id| request.cursor_for(&Position { sort_key: String::new(), id }));
    let items: Vec<InvitationBody> = page.items.into_iter().map(InvitationBody::from).collect();
    Ok(axum::Json(PageBody { items, next_cursor }).into_response())
}

async fn list_tenant_invitations(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberManage)?;
    list_invitations(&mut call, None, &query).await
}

async fn list_team_invitations(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    list_invitations(&mut call, Some(id), &query).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInvitation {
    /// Optional (TS-014): without one the invitation is a link anyone can accept by choosing a login
    /// name and password; with one it is only noted (it enables sending, and the verified-email check
    /// at OIDC acceptance).
    email: Option<String>,
    role: String,
    expires_in_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CreatedInvitationBody {
    id: Uuid,
    /// Shown once; only its hash is stored.
    token: String,
}

async fn create_invitation(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team)): Path<(String, String)>,
    ApiJson(body): ApiJson<CreateInvitation>,
) -> Result<Response, ApiError> {
    let role = team_role(&body.role)?;
    let mut call = open(&state, &caller, &tenant).await?;
    let (id, inviter) = authorize_inviter(&mut call, &team).await?;
    let policy = workspace_policy::load(&mut call.transaction).await.map_err(|_| storage_unavailable(()))?;
    // Delegation (S6-055): administrators always may; team leads and members only as far as the
    // workspace policy lets them, never above their own level, and never into guest roles when
    // guests are off.
    if !policy.may_invite(inviter) || (inviter != Inviter::Admin && role == TeamRole::TeamManager) {
        return Err(ApiError::forbidden());
    }
    if !policy.permits_new_member(match role {
        TeamRole::Commenter => Role::Commenter,
        TeamRole::Viewer => Role::Viewer,
        TeamRole::Editor | TeamRole::TeamManager => Role::Editor,
    }) {
        return Err(conflict());
    }
    let created = invitations::create_link_invitation(&mut call.transaction, id, body.email.as_deref(), role, call.principal, body.expires_in_secs)
        .await
        .map_err(invitation_error)?;
    commit(call.transaction).await?;
    let mut response = (StatusCode::CREATED, axum::Json(CreatedInvitationBody { id: created.id, token: created.token })).into_response();
    response.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn revoke_invitation(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, invitation)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let id = authorize_team(&mut call, &team, Action::TeamManage).await?;
    let invitation = path_uuid(&invitation)?;
    let belongs: Option<String> = sqlx::query_scalar("SELECT team_id::text FROM team_invitations WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(call.transaction.tenant_id().to_string())
        .bind(invitation.to_string())
        .fetch_optional(call.transaction.connection())
        .await
        .map_err(storage_unavailable)?;
    if belongs.as_deref() != Some(id.to_string().as_str()) {
        return Err(ApiError::not_found());
    }
    if !invitations::revoke_invitation(&mut call.transaction, invitation, call.principal).await.map_err(invitation_error)? {
        return Err(ApiError::not_found());
    }
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptInvitation {
    token: String,
}

#[derive(Debug, Serialize)]
struct AcceptedBody {
    tenant_id: TenantId,
    team_id: Uuid,
    team_role: &'static str,
    joined_tenant: bool,
}

/// The caller's most recently linked verified email, if any identity link carries one.
async fn verified_email(state: &AppState, principal: PrincipalId) -> Result<Option<String>, ApiError> {
    sqlx::query_scalar(
        "SELECT verified_metadata->>'email' FROM identity_links \
         WHERE principal_id = $1::uuid AND verified_metadata->>'email' IS NOT NULL \
           AND verified_metadata->'email_verified' = 'true'::jsonb \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(principal.to_string())
    .fetch_optional(state.pool())
    .await
    .map_err(storage_unavailable)
}

async fn accept_invitation(
    State(state): State<AppState>,
    caller: Authenticated,
    ApiJson(body): ApiJson<AcceptInvitation>,
) -> Result<Response, ApiError> {
    // Only people accept invitations: not service accounts, not share links.
    if caller.session.kind != CredentialKind::BrowserSession {
        return Err(ApiError::forbidden());
    }
    if body.token.len() > invitations::TOKEN_CHARS {
        return Err(ApiError::not_found());
    }
    let email = verified_email(&state, caller.principal).await?;
    let accepted = invitations::accept_invitation(state.pool(), &body.token, caller.principal, email.as_deref()).await.map_err(invitation_error)?;
    let body = AcceptedBody {
        tenant_id: accepted.tenant_id,
        team_id: accepted.team_id,
        team_role: accepted.team_role.as_str(),
        joined_tenant: accepted.joined_tenant,
    };
    Ok(axum::Json(body).into_response())
}
