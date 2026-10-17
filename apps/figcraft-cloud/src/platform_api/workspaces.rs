//! Workspace management and the platform audit log under `/platform/v1`.
//!
//! The platform sees workspace metadata only (slug, name, state, plan, region, timestamps, active
//! member count). It never reads anything below a workspace.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use figcraft_cloud_domain::{ErrorCode, PrincipalId, ResourceState, TenantId};
use figcraft_cloud_storage::metadata::PageSize;
use figcraft_cloud_storage::platform::{self, AuditFilter, PlatformWorkspace, WorkspaceFilter, begin_platform_transaction};
use figcraft_cloud_storage::platform_workspaces::{self, Cause, DEFAULT_OWNER_INVITE_SECS, NewWorkspace, OwnerSpec};
use serde::Deserialize;

use super::{PlatformCaller, api_error};
use crate::api::AppState;
use crate::api::context::RequestContext;
use crate::api::cursor::{CursorScope, PageBody, PageRequest, Position};
use crate::api::error::{ApiError, ApiJson};

const DEFAULT_PLAN: &str = "free";
const DEFAULT_REGION: &str = "local";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces", get(list).post(create))
        .route("/workspaces/{id}", get(read))
        .route("/workspaces/{id}/suspend", post(suspend))
        .route("/workspaces/{id}/restore", post(restore))
        .route("/workspaces/{id}/archive", post(archive))
        .route("/workspaces/{id}/request-deletion", post(request_deletion))
        .route("/workspaces/{id}/cancel-deletion", post(cancel_deletion))
        .route("/workspaces/{id}/plan", put(set_plan))
        .route("/workspaces/{id}/owner-invitation", post(reissue_owner_invitation))
}

fn invalid() -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest)
}

fn page_size(limit: u32) -> Result<PageSize, ApiError> {
    PageSize::new(limit).map_err(|_| invalid())
}

pub(crate) fn workspace_json(workspace: &PlatformWorkspace) -> serde_json::Value {
    serde_json::json!({
        "id": workspace.id.to_string(),
        "slug": workspace.slug,
        "name": workspace.name,
        "state": platform::state_name(workspace.state),
        "plan_id": workspace.plan_id,
        "home_region": workspace.home_region,
        "created_at": workspace.created_at,
        "updated_at": workspace.updated_at,
        "active_members": workspace.active_members,
    })
}

async fn fetch(state: &AppState, id: TenantId) -> Result<PlatformWorkspace, ApiError> {
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let found = platform::get_workspace(&mut tx, id).await.map_err(api_error)?;
    let _ = tx.rollback().await;
    found.ok_or(ApiError::not_found())
}

fn cause<'a>(caller: &PlatformCaller, request_id: &'a str) -> Cause<'a> {
    Cause { actor: caller.actor, request_id: Some(request_id) }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ListWorkspacesQuery {
    limit: Option<u32>,
    cursor: Option<String>,
    state: Option<String>,
    slug_prefix: Option<String>,
}

fn parse_state(value: &str) -> Option<ResourceState> {
    Some(match value {
        "pending" => ResourceState::Pending,
        "active" => ResourceState::Active,
        "suspended" => ResourceState::Suspended,
        "archived" => ResourceState::Archived,
        "deleting" => ResourceState::Deleting,
        "deleted" => ResourceState::Deleted,
        _ => return None,
    })
}

/// `GET /platform/v1/workspaces?state=&slug_prefix=&limit=&cursor=`
pub(crate) async fn list(
    State(state): State<AppState>,
    _caller: PlatformCaller,
    Query(query): Query<ListWorkspacesQuery>,
) -> Result<Response, ApiError> {
    let wanted = query.state.as_deref().map(|value| parse_state(value).ok_or_else(invalid)).transpose()?;
    let prefix = query.slug_prefix.as_deref().filter(|prefix| !prefix.is_empty());
    let filter_key = format!("{}|{}", query.state.as_deref().unwrap_or(""), prefix.unwrap_or(""));
    let request = PageRequest::new(CursorScope::new("platform-workspaces", &[&filter_key]), query.limit, query.cursor.as_deref())?;
    let after =
        request.after.as_ref().map(|position| TenantId::try_from(position.id)).transpose().map_err(|_| crate::api::cursor::invalid_cursor())?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let page = platform::list_workspaces(&mut tx, after, page_size(request.limit)?, WorkspaceFilter { state: wanted, slug_prefix: prefix })
        .await
        .map_err(api_error)?;
    let _ = tx.rollback().await;
    let next_cursor = page.next_cursor.and_then(|id| request.cursor_for(&Position { sort_key: String::new(), id: id.as_uuid() }));
    Ok(Json(PageBody { items: page.items.iter().map(workspace_json).collect(), next_cursor }).into_response())
}

/// `GET /platform/v1/workspaces/{id}`
pub(crate) async fn read(State(state): State<AppState>, _caller: PlatformCaller, Path(id): Path<TenantId>) -> Result<Response, ApiError> {
    Ok(Json(workspace_json(&fetch(&state, id).await?)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerBody {
    principal_id: Option<PrincipalId>,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateWorkspace {
    slug: String,
    name: String,
    plan_id: Option<String>,
    home_region: Option<String>,
    owner: OwnerBody,
    /// Seconds the owner invitation stays valid (default 7 days).
    invitation_lifetime_secs: Option<i64>,
}

fn invitation_json(invitation: &platform_workspaces::OwnerInvitation) -> serde_json::Value {
    serde_json::json!({ "id": invitation.id.to_string(), "token": invitation.token, "expires_at": invitation.expires_at })
}

/// `POST /platform/v1/workspaces`: with `owner.principal_id` the workspace is active at once; with
/// `owner.email` it is pending and the response carries the owner invitation token once.
pub(crate) async fn create(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    ApiJson(body): ApiJson<CreateWorkspace>,
) -> Result<Response, ApiError> {
    let owner = match (body.owner.principal_id, body.owner.email.as_deref()) {
        (Some(principal), None) => OwnerSpec::Principal(principal),
        (None, Some(email)) => OwnerSpec::Invite { email },
        _ => return Err(invalid()),
    };
    let request_id = context.request_id.to_string();
    let new = NewWorkspace {
        slug: &body.slug,
        name: &body.name,
        plan_id: body.plan_id.as_deref().unwrap_or(DEFAULT_PLAN),
        home_region: body.home_region.as_deref().unwrap_or(DEFAULT_REGION),
        owner,
    };
    let created = platform_workspaces::create_workspace(
        state.pool(),
        &new,
        cause(&caller, &request_id),
        body.invitation_lifetime_secs.unwrap_or(DEFAULT_OWNER_INVITE_SECS),
    )
    .await
    .map_err(api_error)?;
    let workspace = fetch(&state, created.workspace.id).await?;
    let mut json = workspace_json(&workspace);
    if let Some(invitation) = &created.owner_invitation {
        json["owner_invitation"] = invitation_json(invitation);
    }
    Ok((StatusCode::CREATED, Json(json)).into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReasonBody {
    reason: Option<String>,
}

async fn transition(
    state: &AppState,
    context: &RequestContext,
    caller: &PlatformCaller,
    id: TenantId,
    to: ResourceState,
    reason: Option<&str>,
) -> Result<Response, ApiError> {
    let request_id = context.request_id.to_string();
    platform_workspaces::set_lifecycle(state.pool(), id, to, reason, cause(caller, &request_id)).await.map_err(api_error)?;
    Ok(Json(workspace_json(&fetch(state, id).await?)).into_response())
}

/// `POST /platform/v1/workspaces/{id}/suspend`: members, sessions, relay, share links, host tokens and
/// webhooks stop at once; nothing is deleted.
pub(crate) async fn suspend(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
    ApiJson(body): ApiJson<ReasonBody>,
) -> Result<Response, ApiError> {
    let reason = body.reason;
    transition(&state, &context, &caller, id, ResourceState::Suspended, reason.as_deref()).await
}

/// `POST /platform/v1/workspaces/{id}/restore`: a suspended or archived workspace becomes active.
pub(crate) async fn restore(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
) -> Result<Response, ApiError> {
    transition(&state, &context, &caller, id, ResourceState::Active, None).await
}

/// `POST /platform/v1/workspaces/{id}/archive`
pub(crate) async fn archive(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
    ApiJson(body): ApiJson<ReasonBody>,
) -> Result<Response, ApiError> {
    let reason = body.reason;
    transition(&state, &context, &caller, id, ResourceState::Archived, reason.as_deref()).await
}

/// `POST /platform/v1/workspaces/{id}/request-deletion`: moves an archived or suspended workspace to
/// pending deletion. Deleting data is a later, separate job (S8); nothing is removed here.
pub(crate) async fn request_deletion(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
    ApiJson(body): ApiJson<ReasonBody>,
) -> Result<Response, ApiError> {
    let reason = body.reason;
    transition(&state, &context, &caller, id, ResourceState::Deleting, reason.as_deref()).await
}

/// `POST /platform/v1/workspaces/{id}/cancel-deletion`: back to archived.
pub(crate) async fn cancel_deletion(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
) -> Result<Response, ApiError> {
    // Only a workspace in pending deletion can be taken back this way (otherwise it is just archive).
    if fetch(&state, id).await?.state != ResourceState::Deleting {
        return Err(ApiError::new(ErrorCode::Conflict));
    }
    transition(&state, &context, &caller, id, ResourceState::Archived, None).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanBody {
    plan_id: String,
}

/// `PUT /platform/v1/workspaces/{id}/plan`
pub(crate) async fn set_plan(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
    ApiJson(body): ApiJson<PlanBody>,
) -> Result<Response, ApiError> {
    let request_id = context.request_id.to_string();
    platform_workspaces::set_plan(state.pool(), id, &body.plan_id, cause(&caller, &request_id)).await.map_err(api_error)?;
    Ok(Json(workspace_json(&fetch(&state, id).await?)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReissueBody {
    email: String,
    invitation_lifetime_secs: Option<i64>,
}

/// `POST /platform/v1/workspaces/{id}/owner-invitation`: replace the owner link of a pending workspace.
pub(crate) async fn reissue_owner_invitation(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<TenantId>,
    ApiJson(body): ApiJson<ReissueBody>,
) -> Result<Response, ApiError> {
    let request_id = context.request_id.to_string();
    let invitation = platform_workspaces::reissue_owner_invitation(
        state.pool(),
        id,
        &body.email,
        body.invitation_lifetime_secs.unwrap_or(DEFAULT_OWNER_INVITE_SECS),
        cause(&caller, &request_id),
    )
    .await
    .map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(invitation_json(&invitation))).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct AuditQuery {
    limit: Option<u32>,
    cursor: Option<String>,
    workspace_id: Option<TenantId>,
    action_prefix: Option<String>,
}

/// `GET /platform/v1/audit?workspace_id=&action_prefix=&limit=&cursor=`: newest first.
pub(crate) async fn audit_log(State(state): State<AppState>, _caller: PlatformCaller, Query(query): Query<AuditQuery>) -> Result<Response, ApiError> {
    let workspace = query.workspace_id.map(|id| id.to_string());
    let prefix = query.action_prefix.as_deref().filter(|prefix| !prefix.is_empty());
    let filter_key = format!("{}|{}", workspace.as_deref().unwrap_or(""), prefix.unwrap_or(""));
    let request = PageRequest::new(CursorScope::new("platform-audit", &[&filter_key]), query.limit, query.cursor.as_deref())?;
    let after = request
        .after
        .as_ref()
        .map(|position| position.sort_key.parse::<i64>().map(|micros| (micros, position.id)).map_err(|_| crate::api::cursor::invalid_cursor()))
        .transpose()?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let page = platform::list_platform_audit(
        &mut tx,
        after,
        page_size(request.limit)?,
        AuditFilter { tenant_id: query.workspace_id, action_prefix: prefix },
    )
    .await
    .map_err(api_error)?;
    let _ = tx.rollback().await;
    let next_cursor = page.next_cursor.and_then(|(micros, id)| request.cursor_for(&Position { sort_key: micros.to_string(), id }));
    let items = page
        .items
        .iter()
        .map(|event| {
            serde_json::json!({
                "id": event.id.to_string(),
                "at_micros": event.created_at_micros,
                "actor_kind": event.actor_kind,
                "actor_id": event.actor_id.map(|id| id.to_string()),
                "action": event.action,
                "resource_kind": event.resource_kind,
                "resource_id": event.resource_id.map(|id| id.to_string()),
                "workspace_id": event.tenant_id.map(|id| id.to_string()),
                "outcome": event.outcome,
                "metadata": event.metadata,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(PageBody { items, next_cursor }).into_response())
}
