//! Project-level team access (S6-052) and the workspace policy (S6-056).
//!
//! | Method and path | Needs |
//! |---|---|
//! | `GET /tenants/{t}/policy` | `tenant.view` (any member) |
//! | `PUT /tenants/{t}/policy` | `tenant.update_policy` (owner and administrators) |
//!
//! The policy is `{allowed_sign_in_methods, who_may_invite, guest_access, share_links}`
//! ([`figcraft_cloud_domain::policy::WorkspacePolicy`]); the response adds `oidc_available`. Saving
//! a stricter share-link policy revokes the links it no longer allows (`share_links_revoked`).
//!
//! A project can be *owned* by one team (`PUT /tenants/{t}/teams/{team}/owned/projects/{project}`,
//! see `teams_api`): that team's members inherit access to the project and its files, the team lead
//! administering it. This module adds *sharing* a project with other teams at a chosen role:
//!
//! | Method and path | Needs |
//! |---|---|
//! | `GET /tenants/{t}/projects/{project}/team-grants` | `project.view` |
//! | `PUT /tenants/{t}/teams/{team}/grants/projects/{project}` | `team.manage` on the team and project admin (`project.archive`) |
//! | `DELETE /tenants/{t}/teams/{team}/grants/projects/{project}` | project admin (works on archived teams) |
//!
//! Each member's access is the grant's role capped by their role in the team (a team `viewer` never
//! edits). A project the caller holds no role on is `404`; one they can see but not administer `403`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use figcraft_cloud_auth::authz::{Action, Resource, decide};
use figcraft_cloud_domain::policy::{SignInMethod, WorkspacePolicy};
use figcraft_cloud_domain::{ProjectId, Uuid};
use figcraft_cloud_storage::effective_roles::resolve_project_access;
use figcraft_cloud_storage::revocation::AccessRevocation;
use figcraft_cloud_storage::teams::{self, GrantTarget, TeamGrant};
use figcraft_cloud_storage::workspace_policy::{self, PolicyError};
use serde_json::json;

use super::AppState;
use super::auth::Authenticated;
use super::error::{ApiError, ApiJson, denied};
use super::teams_api::{GrantBody, authorize_team, team_error, workspace_grant_role};
use super::tenants_api::{TenantCall, commit, invalid, open, path_uuid, require_tenant, revoke_after_commit, storage_unavailable};

pub(crate) fn routes() -> Router<AppState> {
    let tenant = "/tenants/{tenant}";
    Router::new()
        .route(&format!("{tenant}/policy"), get(read_policy).put(write_policy))
        .route(&format!("{tenant}/projects/{{project}}/team-grants"), get(list_grants))
        .route(&format!("{tenant}/teams/{{team}}/grants/projects/{{project}}"), put(grant_project).delete(revoke_project))
}

fn path_project(value: &str) -> Result<ProjectId, ApiError> {
    ProjectId::try_from(path_uuid(value)?).map_err(|_| ApiError::not_found())
}

/// `404` when the caller holds no role on the project, `403` when they hold one that does not allow
/// `action`.
async fn authorize_project(call: &mut TenantCall<'_>, project: ProjectId, action: Action) -> Result<(), ApiError> {
    let access =
        resolve_project_access(&mut call.transaction, call.principal, project).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    let resource = Resource::Project { state: access.state, role: access.role };
    if decide(&call.subject, &resource, action).allow {
        return Ok(());
    }
    Err(denied(decide(&call.subject, &resource, Action::ProjectView).allow))
}

async fn list_grants(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, project)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let project = path_project(&project)?;
    authorize_project(&mut call, project, Action::ProjectView).await?;
    let owner: Option<Option<String>> = sqlx::query_scalar("SELECT owning_team_id::text FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(call.transaction.tenant_id().to_string())
        .bind(project.to_string())
        .fetch_optional(call.transaction.connection())
        .await
        .map_err(storage_unavailable)?;
    let grants = teams::list_project_team_grants(&mut call.transaction, project.as_uuid()).await.map_err(team_error)?;
    let _ = call.transaction.rollback().await;
    let items: Vec<_> =
        grants.iter().map(|grant| json!({"team_id": grant.team_id.to_string(), "team_name": grant.team_name, "role": grant.role})).collect();
    Ok(Json(json!({"owning_team_id": owner.flatten(), "items": items})).into_response())
}

async fn grant_project(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, project)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<GrantBody>,
) -> Result<Response, ApiError> {
    let role = workspace_grant_role(body.role())?;
    let mut call = open(&state, &caller, &tenant).await?;
    let team: Uuid = authorize_team(&mut call, &team, Action::TeamManage).await?;
    let project = path_project(&project)?;
    authorize_project(&mut call, project, Action::ProjectArchive).await?;
    teams::grant_to_team(&mut call.transaction, team, TeamGrant::Project(project.as_uuid(), role), call.principal).await.map_err(team_error)?;
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn revoke_project(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, team, project)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    let team = path_uuid(&team)?;
    let project = path_project(&project)?;
    authorize_project(&mut call, project, Action::ProjectArchive).await?;
    if !teams::revoke_team_grant(&mut call.transaction, team, GrantTarget::Project(project.as_uuid()), call.principal).await.map_err(team_error)? {
        return Err(ApiError::not_found());
    }
    commit(call.transaction).await?;
    revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn policy_error(error: PolicyError) -> ApiError {
    match error {
        PolicyError::Invalid => invalid(),
        PolicyError::NotFound => ApiError::not_found(),
        PolicyError::Database => ApiError::unavailable(),
        PolicyError::Audit | PolicyError::CorruptRow => ApiError::new(figcraft_cloud_domain::ErrorCode::Internal),
    }
}

fn policy_body(policy: &WorkspacePolicy, oidc_available: bool, revoked: Option<u64>) -> serde_json::Value {
    let mut body = json!({
        "allowed_sign_in_methods": policy.allowed_sign_in_methods,
        "who_may_invite": policy.who_may_invite,
        "guest_access": policy.guest_access,
        "share_links": policy.share_links,
        "oidc_available": oidc_available,
    });
    if let Some(revoked) = revoked {
        body["share_links_revoked"] = json!(revoked);
    }
    body
}

async fn read_policy(State(state): State<AppState>, caller: Authenticated, Path(tenant): Path<String>) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::TenantView)?;
    let policy = workspace_policy::load(&mut call.transaction).await.map_err(policy_error)?;
    let _ = call.transaction.rollback().await;
    Ok(Json(policy_body(&policy, state.oidc().is_some(), None)).into_response())
}

async fn write_policy(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    ApiJson(policy): ApiJson<WorkspacePolicy>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::TenantUpdatePolicy)?;
    // Turning an identity provider on when the server has none would lock people out.
    if policy.allows_sign_in(SignInMethod::Oidc) && state.oidc().is_none() {
        let current = workspace_policy::load(&mut call.transaction).await.map_err(policy_error)?;
        if !current.allows_sign_in(SignInMethod::Oidc) {
            return Err(ApiError::new(figcraft_cloud_domain::ErrorCode::Conflict));
        }
    }
    let revoked = workspace_policy::save(&mut call.transaction, call.principal, &policy).await.map_err(policy_error)?;
    commit(call.transaction).await?;
    Ok(Json(policy_body(&policy, state.oidc().is_some(), Some(revoked))).into_response())
}
