//! Workspace, project, document, member, team-grant and share-link APIs (S4-019), and the
//! collaboration link the browser editor joins with.
//!
//! Every handler follows the same order: [`Authenticated`] (401 before any lookup) → a tenant
//! transaction in the caller's tenant → `effective_roles` → [`authz::decide`] → the disclosure
//! policy ([`authorize_resource`]): a resource that does not exist, lives in another tenant or is
//! invisible to the caller answers `404`; a visible one the caller may not act on answers `403`.
//! "Visible" means the caller may view it *or* perform the requested action (a workspace admin
//! restoring a soft-deleted document can see it even though nobody can `view` a deleting one).
//!
//! Share-link credentials ([`CredentialKind::ShareLink`]) are refused on these routes (`403`):
//! a link grants one document's realtime room, never the management API.
//!
//! Routes (relative to `/api/v1`) and the action each one needs are listed in [`ROUTES`]; the
//! permission-matrix tests (S4-021) check every entry against an independently written table.
//!
//! Every successful mutation writes an `audit_events` row (`outcome = succeeded`, actor = the
//! caller) in the same transaction as the change: `workspace.create|rename|archive|restore|delete`,
//! `workspace.member.set|remove`, `project.*` and `document.*` likewise,
//! `document.member.set|remove`, `document.team_grant.set|remove`, `share_link.create|revoke`
//! and `document.collab_link` (a minted collaboration share link). Metadata carries ids and roles
//! only, never a secret. A removed or changed direct membership or team grant is followed, after
//! commit, by `revocation::apply_revocation(GrantRevoked)`: grants change no session, so the next
//! request and the relay's next sweep re-resolve access (see `revocation`).
//!
//! Team grants exist on workspaces and documents only. `GET /workspaces/{id}/team-grants` lists a
//! workspace's grants; they are written through the team routes
//! (`/tenants/{t}/teams/{team}/grants/workspaces/{w}`, which need `team.manage` as well as
//! `workspace.manage_members`). There are no project-level team grants: the S2 schema has no
//! project grant table and `effective_roles` resolves a project's access from its workspace, so a
//! project inherits its workspace's direct members and team grants. Adding project grants would
//! need a migration plus a new inheritance rule in `effective_roles` and the relay's admission
//! check; it is deliberately left out until the product needs it (projects are owned by a team
//! through `/teams/{team}/owned/projects/{p}`).

pub(crate) mod admission;
#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use figcraft_cloud_auth::authz::{Action, Resource, SensitivePermission, Subject, decide};
use figcraft_cloud_domain::{CloudDocumentId, ErrorCode, PrincipalId, ProjectId, ResourceState, Role, Uuid, WorkspaceId};
use figcraft_cloud_storage::access_guard::MembershipRequirement;
use figcraft_cloud_storage::audit::{RepositoryError as AuditError, record_audit_event};
use figcraft_cloud_storage::effective_roles::{
    ResourceAccess, TenantAccess, resolve_document_access, resolve_project_access, resolve_tenant_access, resolve_workspace_access,
};
use figcraft_cloud_storage::host_systems::HostRole;
use figcraft_cloud_storage::memberships::{
    MembershipError, list_workspace_team_grants, parse_role, remove_document_member, remove_document_team_grant, remove_workspace_member, role_str,
    set_document_member, set_document_team_grant, set_workspace_member,
};
use figcraft_cloud_storage::metadata::{
    DocumentRecord, PageSize, ProjectRecord, RepositoryError, WorkspaceRecord, archive_document, create_document, create_project, create_workspace,
    get_document, get_workspace, list_projects, rename_document, restore_document, soft_delete_document,
};
use figcraft_cloud_storage::resource_admin::{
    MAX_LISTED, Transition, get_project_by_id, list_document_members, list_document_team_grants, list_documents, list_visible_workspaces,
    list_workspace_members, rename_project, rename_workspace, transition_project, transition_workspace,
};
use figcraft_cloud_storage::revocation::AccessRevocation;
use figcraft_cloud_storage::share_links::{ShareLinkError, ShareRole, create_share_link, list_share_links, revoke_share_link};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::auth::{Authenticated, CredentialKind, Grant};
use super::authenticator::hold_caller_access;
use super::cursor::{CursorScope, ListQuery, PageRequest, Position, invalid_cursor};
use super::error::{ApiError, ApiJson, authorize_resource};

/// Default and largest page sizes for list endpoints.
pub(crate) const DEFAULT_PAGE: u32 = 50;
/// Longest a resource name may be (the schema's limit).
pub(crate) const MAX_NAME_CHARS: usize = 255;
/// Lifetime of the share secret a collaboration link carries. The relay disconnects a session
/// when its link expires, so this is a working session; the browser fetches a new link to
/// reconnect after it.
pub(crate) const COLLAB_LINK_SECS: u64 = 8 * 60 * 60;
/// The label collaboration links are stored with (so managers can tell them from shared links).
pub(crate) const COLLAB_LINK_LABEL: &str = "collaboration session";
const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:8080";

/// One route: method, path template (relative to `/api/v1`) and the action it authorizes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteSpec {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    pub(crate) action: Action,
}

const fn route(method: &'static str, path: &'static str, action: Action) -> RouteSpec {
    RouteSpec { method, path, action }
}

/// Every route this module registers, with its action.
pub(crate) const ROUTES: &[RouteSpec] = &[
    route("GET", "/workspaces", Action::WorkspaceView),
    route("POST", "/workspaces", Action::WorkspaceCreate),
    route("GET", "/workspaces/{workspace_id}", Action::WorkspaceView),
    route("PATCH", "/workspaces/{workspace_id}", Action::WorkspaceUpdate),
    route("DELETE", "/workspaces/{workspace_id}", Action::WorkspaceDelete),
    route("POST", "/workspaces/{workspace_id}/archive", Action::WorkspaceArchive),
    route("POST", "/workspaces/{workspace_id}/restore", Action::WorkspaceRestore),
    route("GET", "/workspaces/{workspace_id}/members", Action::WorkspaceView),
    route("PUT", "/workspaces/{workspace_id}/members/{principal_id}", Action::WorkspaceManageMembers),
    route("DELETE", "/workspaces/{workspace_id}/members/{principal_id}", Action::WorkspaceManageMembers),
    route("GET", "/workspaces/{workspace_id}/team-grants", Action::WorkspaceView),
    route("GET", "/workspaces/{workspace_id}/projects", Action::WorkspaceView),
    route("POST", "/workspaces/{workspace_id}/projects", Action::ProjectCreate),
    route("GET", "/projects/{project_id}", Action::ProjectView),
    route("PATCH", "/projects/{project_id}", Action::ProjectUpdate),
    route("DELETE", "/projects/{project_id}", Action::ProjectDelete),
    route("POST", "/projects/{project_id}/archive", Action::ProjectArchive),
    route("POST", "/projects/{project_id}/restore", Action::ProjectRestore),
    route("GET", "/projects/{project_id}/documents", Action::ProjectView),
    route("POST", "/projects/{project_id}/documents", Action::DocumentCreate),
    route("GET", "/documents/{document_id}", Action::DocumentView),
    route("PATCH", "/documents/{document_id}", Action::DocumentRename),
    route("DELETE", "/documents/{document_id}", Action::DocumentDelete),
    route("POST", "/documents/{document_id}/archive", Action::DocumentArchive),
    route("POST", "/documents/{document_id}/restore", Action::DocumentRestore),
    route("GET", "/documents/{document_id}/members", Action::DocumentView),
    route("PUT", "/documents/{document_id}/members/{principal_id}", Action::DocumentShare),
    route("DELETE", "/documents/{document_id}/members/{principal_id}", Action::DocumentShare),
    route("GET", "/documents/{document_id}/team-grants", Action::DocumentView),
    route("PUT", "/documents/{document_id}/team-grants/{team_id}", Action::DocumentShare),
    route("DELETE", "/documents/{document_id}/team-grants/{team_id}", Action::DocumentShare),
    route("GET", "/documents/{document_id}/share-links", Action::DocumentShare),
    route("POST", "/documents/{document_id}/share-links", Action::DocumentShare),
    route("DELETE", "/documents/{document_id}/share-links/{link_id}", Action::DocumentShare),
    route("GET", "/documents/{document_id}/collab-link", Action::DocumentJoinRealtime),
    route("POST", "/documents/{document_id}/realtime-ticket", Action::DocumentJoinRealtime),
];

/// The router for [`ROUTES`] (mounted under `/api/v1`, in the `resources` router).
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/workspaces", get(list_workspaces_handler).post(create_workspace_handler))
        .route("/workspaces/{workspace_id}", get(get_workspace_handler).patch(rename_workspace_handler).delete(delete_workspace_handler))
        .route("/workspaces/{workspace_id}/archive", post(archive_workspace_handler))
        .route("/workspaces/{workspace_id}/restore", post(restore_workspace_handler))
        .route("/workspaces/{workspace_id}/members", get(list_workspace_members_handler))
        .route("/workspaces/{workspace_id}/members/{principal_id}", put(set_workspace_member_handler).delete(remove_workspace_member_handler))
        .route("/workspaces/{workspace_id}/team-grants", get(list_workspace_team_grants_handler))
        .route("/workspaces/{workspace_id}/projects", get(list_projects_handler).post(create_project_handler))
        .route("/projects/{project_id}", get(get_project_handler).patch(rename_project_handler).delete(delete_project_handler))
        .route("/projects/{project_id}/archive", post(archive_project_handler))
        .route("/projects/{project_id}/restore", post(restore_project_handler))
        .route("/projects/{project_id}/documents", get(list_documents_handler).post(create_document_handler))
        .route("/documents/{document_id}", get(get_document_handler).patch(rename_document_handler).delete(delete_document_handler))
        .route("/documents/{document_id}/archive", post(archive_document_handler))
        .route("/documents/{document_id}/restore", post(restore_document_handler))
        .route("/documents/{document_id}/members", get(list_document_members_handler))
        .route("/documents/{document_id}/members/{principal_id}", put(set_document_member_handler).delete(remove_document_member_handler))
        .route("/documents/{document_id}/team-grants", get(list_document_team_grants_handler))
        .route("/documents/{document_id}/team-grants/{team_id}", put(set_document_team_grant_handler).delete(remove_document_team_grant_handler))
        .route("/documents/{document_id}/share-links", get(list_share_links_handler).post(create_share_link_handler))
        .route("/documents/{document_id}/share-links/{link_id}", axum::routing::delete(revoke_share_link_handler))
        .route("/documents/{document_id}/collab-link", get(collab_link_handler))
        .route("/documents/{document_id}/realtime-ticket", post(super::realtime::realtime_ticket_handler))
}

// ---------------------------------------------------------------------------------------------
// Cursors

/// Paging for a list endpoint through the shared, HMAC-signed cursors ([`super::cursor`],
/// S4-020). `limit` must be a valid page size (else `400 invalid_request`); a cursor must be one
/// this list issued for the same tenant, caller and parent within its lifetime (else
/// `400 invalid_cursor`). The cursor's row id is all a page resumes from: the query is re-scoped
/// by the tenant transaction and every row re-authorized.
pub(crate) struct Paging {
    request: PageRequest,
    size: PageSize,
}

impl Paging {
    /// `kind` names the list (e.g. `"documents"`), `parent` the path resource it lists under
    /// (empty for top-level lists).
    pub(crate) fn new(caller: &Authenticated, kind: &'static str, parent: &str, query: &ListQuery) -> Result<Self, ApiError> {
        let limit = query.limit.unwrap_or(DEFAULT_PAGE);
        let size = PageSize::new(limit).map_err(|_| invalid())?;
        Ok(Self { request: PageRequest::new(cursor_scope(caller, kind, parent), Some(limit), query.cursor.as_deref())?, size })
    }

    /// The row the page resumes after, as the list's id type.
    fn after<T: std::str::FromStr>(&self) -> Result<Option<T>, ApiError> {
        self.request.after.as_ref().map(|position| position.id.to_string().parse().map_err(|_| invalid_cursor())).transpose()
    }

    /// `next_cursor` for a page whose last row is `last` (`null` on the last page).
    fn next(&self, last: Option<Uuid>) -> Value {
        last.and_then(|id| self.request.cursor_for(&Position { sort_key: String::new(), id })).map_or(Value::Null, Value::String)
    }
}

/// The scope a list's cursors are bound to: the list, the caller's tenant and principal, and the
/// parent resource.
pub(crate) fn cursor_scope(caller: &Authenticated, kind: &'static str, parent: &str) -> CursorScope {
    let tenant = caller.tenant.tenant_id().to_string();
    let principal = caller.principal.to_string();
    CursorScope::new(kind, &[&tenant, &principal, parent])
}

// ---------------------------------------------------------------------------------------------
// Errors and authorization

const fn invalid() -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest)
}

pub(crate) fn repository_error(error: RepositoryError) -> ApiError {
    match error {
        RepositoryError::InvalidPageSize | RepositoryError::Constraint => invalid(),
        RepositoryError::Conflict => ApiError::new(ErrorCode::Conflict),
        RepositoryError::InvalidState => ApiError::new(ErrorCode::ResourceInactive),
        RepositoryError::Database => ApiError::unavailable(),
        RepositoryError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

pub(crate) fn membership_error(error: MembershipError) -> ApiError {
    match error {
        MembershipError::InvalidRole | MembershipError::Constraint | MembershipError::NotActiveMember => invalid(),
        MembershipError::NotFound => ApiError::not_found(),
        MembershipError::LastOwner | MembershipError::Conflict => ApiError::new(ErrorCode::Conflict),
        MembershipError::Database => ApiError::unavailable(),
        MembershipError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn share_error(error: ShareLinkError) -> ApiError {
    match error {
        ShareLinkError::InvalidInput => invalid(),
        ShareLinkError::Refused => ApiError::not_found(),
        _ => ApiError::unavailable(),
    }
}

/// The policy's view of the caller's tenant-level facts.
pub(crate) fn subject(tenant: &TenantAccess) -> Subject {
    Subject {
        principal_active: tenant.principal_active,
        tenant_state: tenant.tenant_state,
        tenant_role: tenant.tenant_role,
        permissions: tenant.permissions.iter().filter_map(|permission| SensitivePermission::parse(permission.as_str())).collect(),
    }
}

/// The kind of resource a route acts on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Workspace,
    Project,
    Document,
}

impl Kind {
    pub(crate) const fn resource(self, state: ResourceState, role: Option<Role>) -> Resource {
        match self {
            Self::Workspace => Resource::Workspace { state, role },
            Self::Project => Resource::Project { state, role },
            Self::Document => Resource::Document { state, role },
        }
    }

    pub(crate) const fn view(self) -> Action {
        match self {
            Self::Workspace => Action::WorkspaceView,
            Self::Project => Action::ProjectView,
            Self::Document => Action::DocumentView,
        }
    }
}

pub(crate) fn allows(access: &ResourceAccess, kind: Kind, action: Action) -> bool {
    decide(&subject(&access.tenant), &kind.resource(access.state, access.role), action).allow
}

/// The disclosure policy over a resolved resource: `404` unless the caller may view it or perform
/// `action`, `403` when visible but not permitted.
pub(crate) fn check(access: Option<ResourceAccess>, kind: Kind, action: Action) -> Result<ResourceAccess, ApiError> {
    authorize_resource(access, |access| allows(access, kind, kind.view()) || allows(access, kind, action), |access| allows(access, kind, action))
}

async fn begin<'a>(state: &'a AppState, caller: &Authenticated) -> Result<TenantTransaction<'a>, ApiError> {
    if caller.session.kind == CredentialKind::ShareLink {
        return Err(ApiError::forbidden());
    }
    let mut transaction = begin_tenant_transaction(state.pool(), caller.tenant.tenant_id()).await.map_err(|_| ApiError::unavailable())?;
    hold_caller_access(state, &mut transaction, caller, MembershipRequirement::ActiveMember).await?;
    Ok(transaction)
}

async fn commit(transaction: TenantTransaction<'_>) -> Result<(), ApiError> {
    transaction.commit().await.map_err(|_| ApiError::unavailable())
}

/// Record a successful mutation by `caller` on `resource_kind`/`resource_id` in `transaction`
/// (before commit, so the change and its audit row land together). `metadata` holds ids and roles
/// only.
pub(crate) async fn audit(
    transaction: &mut TenantTransaction<'_>,
    caller: &Authenticated,
    action: &str,
    resource_kind: &str,
    resource_id: Uuid,
    metadata: Value,
) -> Result<(), ApiError> {
    record_audit_event(
        transaction,
        Uuid::new_v4(),
        Some(caller.principal.as_uuid()),
        action,
        resource_kind,
        Some(resource_id),
        None,
        None,
        "succeeded",
        &metadata,
    )
    .await
    .map(|_| ())
    .map_err(|error| match error {
        AuditError::Database => ApiError::unavailable(),
        AuditError::Conflict | AuditError::Constraint | AuditError::CorruptRow => ApiError::new(ErrorCode::Internal),
    })
}

async fn workspace_access(
    transaction: &mut TenantTransaction<'_>,
    caller: &Authenticated,
    id: &str,
    action: Action,
) -> Result<WorkspaceId, ApiError> {
    let id: WorkspaceId = id.parse().map_err(|_| ApiError::not_found())?;
    let access = resolve_workspace_access(transaction, caller.principal, id).await.map_err(membership_error)?;
    check(access, Kind::Workspace, action)?;
    Ok(id)
}

async fn project_access(transaction: &mut TenantTransaction<'_>, caller: &Authenticated, id: &str, action: Action) -> Result<ProjectId, ApiError> {
    let id: ProjectId = id.parse().map_err(|_| ApiError::not_found())?;
    let access = resolve_project_access(transaction, caller.principal, id).await.map_err(membership_error)?;
    check(access, Kind::Project, action)?;
    Ok(id)
}

/// An embedded session never holds more than the role its host's token granted (S6-010): the
/// member's own role is lowered to that cap, whatever the member could do outside the embed.
fn cap_embedded(caller: &Authenticated, mut access: ResourceAccess) -> ResourceAccess {
    if let Grant::Embed { role, .. } = &caller.grant {
        let cap = match role {
            HostRole::Viewer => Role::Viewer,
            HostRole::Commenter => Role::Commenter,
            HostRole::Editor => Role::Editor,
        };
        access.role = access.role.map(|own| if rank(own) > rank(cap) { cap } else { own });
        // Tenant-level roles also grant resource actions; the embed must not use them.
        access.tenant.tenant_role = access.tenant.tenant_role.map(|own| if rank(own) > rank(cap) { cap } else { own });
        access.tenant.permissions.clear();
    }
    access
}

/// Strength of a role, higher is stronger.
const fn rank(role: Role) -> u8 {
    match role {
        Role::TenantOwner => 6,
        Role::TenantAdmin => 5,
        Role::TeamManager => 4,
        Role::WorkspaceAdmin => 3,
        Role::Editor => 2,
        Role::Commenter => 1,
        Role::Viewer => 0,
    }
}

async fn document_access(
    transaction: &mut TenantTransaction<'_>,
    caller: &Authenticated,
    id: &str,
    action: Action,
) -> Result<(CloudDocumentId, ResourceAccess), ApiError> {
    let id: CloudDocumentId = id.parse().map_err(|_| ApiError::not_found())?;
    let access = resolve_document_access(transaction, caller.principal, id).await.map_err(membership_error)?;
    let access = access.map(|access| cap_embedded(caller, access));
    Ok((id, check(access, Kind::Document, action)?))
}

/// Tenant-scope actions (no resource identifier): `403` when not permitted (policy rule 4).
async fn tenant_access(transaction: &mut TenantTransaction<'_>, caller: &Authenticated, action: Action) -> Result<TenantAccess, ApiError> {
    let access = resolve_tenant_access(transaction, caller.principal).await.map_err(membership_error)?.ok_or(ApiError::forbidden())?;
    if decide(&subject(&access), &Resource::Tenant, action).allow { Ok(access) } else { Err(ApiError::forbidden()) }
}

// ---------------------------------------------------------------------------------------------
// Bodies and views

pub(crate) fn valid_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let chars = name.chars().count();
    if chars == 0 || chars > MAX_NAME_CHARS || name.chars().any(char::is_control) { Err(invalid()) } else { Ok(name) }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NameBody {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateWorkspaceBody {
    name: String,
    #[serde(default)]
    owning_team_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoleBody {
    role: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateShareLinkBody {
    role: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    expires_in_secs: Option<u64>,
}

pub(crate) const fn state_str(state: ResourceState) -> &'static str {
    match state {
        ResourceState::Pending => "pending",
        ResourceState::Active => "active",
        ResourceState::Suspended => "suspended",
        ResourceState::Archived => "archived",
        ResourceState::Deleting => "deleting",
        ResourceState::Deleted => "deleted",
    }
}

fn workspace_view(record: &WorkspaceRecord) -> Value {
    json!({
        "id": record.id.to_string(),
        "name": record.name,
        "state": state_str(record.state),
        "owning_team_id": record.owning_team_id.map(|id| id.to_string()),
    })
}

fn project_view(record: &ProjectRecord) -> Value {
    json!({
        "id": record.id.to_string(),
        "workspace_id": record.workspace_id.to_string(),
        "name": record.name,
        "state": state_str(record.state),
    })
}

fn document_view(record: &DocumentRecord) -> Value {
    json!({
        "id": record.id.to_string(),
        "workspace_id": record.workspace_id.to_string(),
        "project_id": record.project_id.to_string(),
        "name": record.name,
        "state": state_str(record.state),
    })
}

/// What the caller may do with a workspace, so a client can offer only those actions. The server
/// still authorizes every request; this is a convenience, not a grant.
pub(crate) fn workspace_can(access: &ResourceAccess) -> Value {
    json!({
        "create_project": allows(access, Kind::Workspace, Action::ProjectCreate),
        "rename": allows(access, Kind::Workspace, Action::WorkspaceUpdate),
        "archive": allows(access, Kind::Workspace, Action::WorkspaceArchive),
        "restore": allows(access, Kind::Workspace, Action::WorkspaceRestore),
        "manage_members": allows(access, Kind::Workspace, Action::WorkspaceManageMembers),
    })
}

/// What the caller may do with a project (see [`workspace_can`]).
pub(crate) fn project_can(access: &ResourceAccess) -> Value {
    json!({
        "create_document": allows(access, Kind::Project, Action::DocumentCreate),
        "rename": allows(access, Kind::Project, Action::ProjectUpdate),
        "archive": allows(access, Kind::Project, Action::ProjectArchive),
        "restore": allows(access, Kind::Project, Action::ProjectRestore),
    })
}

/// What the caller may do with a document (see [`workspace_can`]).
pub(crate) fn document_can(access: &ResourceAccess) -> Value {
    json!({
        "edit": allows(access, Kind::Document, Action::DocumentEdit),
        "rename": allows(access, Kind::Document, Action::DocumentRename),
        "archive": allows(access, Kind::Document, Action::DocumentArchive),
        "restore": allows(access, Kind::Document, Action::DocumentRestore),
        "share": allows(access, Kind::Document, Action::DocumentShare),
    })
}

/// Add `updated_at` (Unix seconds of the last edit, rename or state change) to document views,
/// from one query for the whole page. A view whose row cannot be read keeps no `updated_at`.
async fn attach_updated_at(transaction: &mut TenantTransaction<'_>, views: &mut [Value]) -> Result<(), ApiError> {
    let ids: Vec<String> = views.iter().filter_map(|view| view["id"].as_str().map(str::to_owned)).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT id::text, floor(extract(epoch FROM updated_at))::bigint FROM documents WHERE tenant_id = $1::uuid AND id = ANY($2::uuid[])",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(&ids)
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| ApiError::unavailable())?;
    for view in views.iter_mut() {
        let found = view["id"].as_str().and_then(|id| rows.iter().find(|(row, _)| row == id)).map(|(_, at)| *at);
        if let Some(at) = found {
            view["updated_at"] = Value::from(at);
        }
    }
    Ok(())
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn created(body: Value) -> Response {
    (StatusCode::CREATED, Json(body)).into_response()
}

// ---------------------------------------------------------------------------------------------
// Workspaces

async fn list_workspaces_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "workspaces", "", &query)?;
    let (size, after): (PageSize, Option<WorkspaceId>) = (paging.size, paging.after()?);
    let mut transaction = begin(&state, &caller).await?;
    let tenant = tenant_access(&mut transaction, &caller, Action::TenantView).await?;
    let every = matches!(tenant.tenant_role, Some(Role::TenantOwner | Role::TenantAdmin));
    let page = list_visible_workspaces(&mut transaction, caller.principal, every, after, size).await.map_err(repository_error)?;
    let mut items = Vec::with_capacity(page.items.len());
    for record in &page.items {
        // Re-decide each row: the listing query is only a pre-filter.
        let access = resolve_workspace_access(&mut transaction, caller.principal, record.id).await.map_err(membership_error)?;
        if let Some(access) = access
            .filter(|access| allows(access, Kind::Workspace, Action::WorkspaceView) || allows(access, Kind::Workspace, Action::WorkspaceRestore))
        {
            let mut view = workspace_view(record);
            view["can"] = workspace_can(&access);
            items.push(view);
        }
    }
    Ok(Json(json!({"items": items, "next_cursor": paging.next(page.next_cursor.map(WorkspaceId::as_uuid))})))
}

async fn create_workspace_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    ApiJson(body): ApiJson<CreateWorkspaceBody>,
) -> Result<Response, ApiError> {
    let name = valid_name(&body.name)?;
    let team = body.owning_team_id.as_deref().map(Uuid::parse_str).transpose().map_err(|_| invalid())?;
    let mut transaction = begin(&state, &caller).await?;
    tenant_access(&mut transaction, &caller, Action::WorkspaceCreate).await?;
    let record = create_workspace(&mut transaction, WorkspaceId::random(), name, team).await.map_err(repository_error)?;
    audit(&mut transaction, &caller, "workspace.create", "workspace", record.id.as_uuid(), json!({})).await?;
    commit(transaction).await?;
    Ok(created(workspace_view(&record)))
}

async fn get_workspace_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let id: WorkspaceId = id.parse().map_err(|_| ApiError::not_found())?;
    let access = resolve_workspace_access(&mut transaction, caller.principal, id).await.map_err(membership_error)?;
    let access = check(access, Kind::Workspace, Action::WorkspaceView)?;
    let record = get_workspace(&mut transaction, id).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    let mut view = workspace_view(&record);
    view["can"] = workspace_can(&access);
    Ok(Json(view))
}

async fn rename_workspace_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> Result<Json<Value>, ApiError> {
    let name = valid_name(&body.name)?;
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::WorkspaceUpdate).await?;
    let record = rename_workspace(&mut transaction, id, name).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    audit(&mut transaction, &caller, "workspace.rename", "workspace", id.as_uuid(), json!({})).await?;
    commit(transaction).await?;
    Ok(Json(workspace_view(&record)))
}

async fn workspace_transition(
    state: &AppState,
    caller: &Authenticated,
    id: &str,
    action: Action,
    transition: Transition,
) -> Result<Json<Value>, ApiError> {
    let mut transaction = begin(state, caller).await?;
    let id = workspace_access(&mut transaction, caller, id, action).await?;
    let record = transition_workspace(&mut transaction, id, transition).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    audit(&mut transaction, caller, transition_action(Kind::Workspace, transition), "workspace", id.as_uuid(), json!({})).await?;
    commit(transaction).await?;
    Ok(Json(workspace_view(&record)))
}

async fn delete_workspace_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    workspace_transition(&state, &caller, &id, Action::WorkspaceDelete, Transition::SoftDelete).await
}

async fn archive_workspace_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    workspace_transition(&state, &caller, &id, Action::WorkspaceArchive, Transition::Archive).await
}

async fn restore_workspace_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    workspace_transition(&state, &caller, &id, Action::WorkspaceRestore, Transition::Restore).await
}

fn grants_view<Id: ToString>(paging: &Paging, key: &'static str, grants: &[figcraft_cloud_storage::resource_admin::Grant<Id>]) -> Value {
    let items: Vec<Value> = grants.iter().map(|grant| json!({key: grant.id.to_string(), "role": role_str(grant.role)})).collect();
    let last = (i64::try_from(grants.len()).unwrap_or(i64::MAX) >= MAX_LISTED)
        .then(|| grants.last().and_then(|grant| Uuid::parse_str(&grant.id.to_string()).ok()))
        .flatten();
    json!({"items": items, "next_cursor": paging.next(last)})
}

async fn list_workspace_members_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "workspace_members", &id, &query)?;
    let after: Option<PrincipalId> = paging.after()?;
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::WorkspaceView).await?;
    let members = list_workspace_members(&mut transaction, id, after).await.map_err(repository_error)?;
    Ok(Json(grants_view(&paging, "principal_id", &members)))
}

async fn set_workspace_member_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, principal)): Path<(String, String)>,
    ApiJson(body): ApiJson<RoleBody>,
) -> Result<Json<Value>, ApiError> {
    let role = parse_role(&body.role).ok_or_else(invalid)?;
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::WorkspaceManageMembers).await?;
    let principal: PrincipalId = principal.parse().map_err(|_| ApiError::not_found())?;
    set_workspace_member(&mut transaction, id, principal, role).await.map_err(membership_error)?;
    let metadata = json!({"principal_id": principal.to_string(), "role": role_str(role)});
    audit(&mut transaction, &caller, "workspace.member.set", "workspace", id.as_uuid(), metadata).await?;
    commit(transaction).await?;
    // A changed role may be a downgrade.
    super::tenants_api::revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(Json(json!({"principal_id": principal.to_string(), "role": role_str(role)})))
}

async fn remove_workspace_member_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, principal)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::WorkspaceManageMembers).await?;
    let principal: PrincipalId = principal.parse().map_err(|_| ApiError::not_found())?;
    if !remove_workspace_member(&mut transaction, id, principal).await.map_err(membership_error)? {
        return Err(ApiError::not_found());
    }
    audit(&mut transaction, &caller, "workspace.member.remove", "workspace", id.as_uuid(), json!({"principal_id": principal.to_string()})).await?;
    commit(transaction).await?;
    super::tenants_api::revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /workspaces/{id}/team-grants`: the workspace's team grants (the workspace's viewers may
/// list them, like its direct members).
async fn list_workspace_team_grants_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "workspace_team_grants", &id, &query)?;
    let after: Option<Uuid> = paging.after()?;
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::WorkspaceView).await?;
    let grants = list_workspace_team_grants(&mut transaction, id, after).await.map_err(membership_error)?;
    Ok(Json(grants_view(&paging, "team_id", &grants)))
}

/// The audit action for a lifecycle transition of `kind`.
const fn transition_action(kind: Kind, transition: Transition) -> &'static str {
    match (kind, transition) {
        (Kind::Workspace, Transition::Archive) => "workspace.archive",
        (Kind::Workspace, Transition::Restore) => "workspace.restore",
        (Kind::Workspace, Transition::SoftDelete) => "workspace.delete",
        (_, Transition::Archive) => "project.archive",
        (_, Transition::Restore) => "project.restore",
        (_, Transition::SoftDelete) => "project.delete",
    }
}

// ---------------------------------------------------------------------------------------------
// Projects

async fn list_projects_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "projects", &id, &query)?;
    let (size, after): (PageSize, Option<ProjectId>) = (paging.size, paging.after()?);
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::WorkspaceView).await?;
    let page = list_projects(&mut transaction, id, after, size).await.map_err(repository_error)?;
    let items: Vec<Value> = page.items.iter().filter(|record| record.state != ResourceState::Deleted).map(project_view).collect();
    Ok(Json(json!({"items": items, "next_cursor": paging.next(page.next_cursor.map(ProjectId::as_uuid))})))
}

async fn create_project_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> Result<Response, ApiError> {
    let name = valid_name(&body.name)?;
    let mut transaction = begin(&state, &caller).await?;
    let id = workspace_access(&mut transaction, &caller, &id, Action::ProjectCreate).await?;
    let record = create_project(&mut transaction, ProjectId::random(), id, name, None).await.map_err(repository_error)?;
    audit(&mut transaction, &caller, "project.create", "project", record.id.as_uuid(), json!({"workspace_id": id.to_string()})).await?;
    commit(transaction).await?;
    Ok(created(project_view(&record)))
}

async fn get_project_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let id: ProjectId = id.parse().map_err(|_| ApiError::not_found())?;
    let access = resolve_project_access(&mut transaction, caller.principal, id).await.map_err(membership_error)?;
    let access = check(access, Kind::Project, Action::ProjectView)?;
    let record = get_project_by_id(&mut transaction, id).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    let mut view = project_view(&record);
    view["can"] = project_can(&access);
    Ok(Json(view))
}

async fn rename_project_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> Result<Json<Value>, ApiError> {
    let name = valid_name(&body.name)?;
    let mut transaction = begin(&state, &caller).await?;
    let id = project_access(&mut transaction, &caller, &id, Action::ProjectUpdate).await?;
    let record = rename_project(&mut transaction, id, name).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    audit(&mut transaction, &caller, "project.rename", "project", id.as_uuid(), json!({})).await?;
    commit(transaction).await?;
    Ok(Json(project_view(&record)))
}

async fn project_transition(
    state: &AppState,
    caller: &Authenticated,
    id: &str,
    action: Action,
    transition: Transition,
) -> Result<Json<Value>, ApiError> {
    let mut transaction = begin(state, caller).await?;
    let id = project_access(&mut transaction, caller, id, action).await?;
    let record = transition_project(&mut transaction, id, transition).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    audit(&mut transaction, caller, transition_action(Kind::Project, transition), "project", id.as_uuid(), json!({})).await?;
    commit(transaction).await?;
    Ok(Json(project_view(&record)))
}

async fn delete_project_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    project_transition(&state, &caller, &id, Action::ProjectDelete, Transition::SoftDelete).await
}

async fn archive_project_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    project_transition(&state, &caller, &id, Action::ProjectArchive, Transition::Archive).await
}

async fn restore_project_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    project_transition(&state, &caller, &id, Action::ProjectRestore, Transition::Restore).await
}

// ---------------------------------------------------------------------------------------------
// Documents

async fn list_documents_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "documents", &id, &query)?;
    let (size, after): (PageSize, Option<CloudDocumentId>) = (paging.size, paging.after()?);
    let mut transaction = begin(&state, &caller).await?;
    let id = project_access(&mut transaction, &caller, &id, Action::ProjectView).await?;
    let page = list_documents(&mut transaction, id, after, size).await.map_err(repository_error)?;
    let mut items: Vec<Value> = page.items.iter().map(document_view).collect();
    attach_updated_at(&mut transaction, &mut items).await?;
    Ok(Json(json!({"items": items, "next_cursor": paging.next(page.next_cursor.map(CloudDocumentId::as_uuid))})))
}

async fn create_document_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> Result<Response, ApiError> {
    let name = valid_name(&body.name)?;
    let mut transaction = begin(&state, &caller).await?;
    let id = project_access(&mut transaction, &caller, &id, Action::DocumentCreate).await?;
    let project = get_project_by_id(&mut transaction, id).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    let record = create_document(&mut transaction, CloudDocumentId::random(), project.workspace_id, id, name).await.map_err(repository_error)?;
    audit(&mut transaction, &caller, "document.create", "document", record.id.as_uuid(), json!({"project_id": id.to_string()})).await?;
    commit(transaction).await?;
    Ok(created(document_view(&record)))
}

async fn get_document_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (id, access) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let record = get_document(&mut transaction, id).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    let mut view = document_view(&record);
    view["role"] = access.role.map_or(Value::Null, |role| Value::String(role_str(role).to_string()));
    view["can_edit"] = Value::Bool(allows(&access, Kind::Document, Action::DocumentEdit));
    view["can"] = document_can(&access);
    attach_updated_at(&mut transaction, std::slice::from_mut(&mut view)).await?;
    Ok(Json(view))
}

async fn rename_document_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<NameBody>,
) -> Result<Json<Value>, ApiError> {
    let name = valid_name(&body.name)?;
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentRename).await?;
    let record = rename_document(&mut transaction, id, name).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    audit(&mut transaction, &caller, "document.rename", "document", id.as_uuid(), json!({})).await?;
    super::host_events::publish_document_event(&mut transaction, "document.updated", &record).await?;
    commit(transaction).await?;
    Ok(Json(document_view(&record)))
}

#[derive(Clone, Copy)]
enum DocumentTransition {
    Archive,
    Restore,
    Delete,
}

async fn document_transition(state: &AppState, caller: &Authenticated, id: &str, transition: DocumentTransition) -> Result<Json<Value>, ApiError> {
    let (action, audited) = match transition {
        DocumentTransition::Archive => (Action::DocumentArchive, "document.archive"),
        DocumentTransition::Restore => (Action::DocumentRestore, "document.restore"),
        DocumentTransition::Delete => (Action::DocumentDelete, "document.delete"),
    };
    let mut transaction = begin(state, caller).await?;
    let (id, _) = document_access(&mut transaction, caller, id, action).await?;
    let record = match transition {
        DocumentTransition::Archive => archive_document(&mut transaction, id).await,
        DocumentTransition::Restore => restore_document(&mut transaction, id).await,
        DocumentTransition::Delete => soft_delete_document(&mut transaction, id).await,
    }
    .map_err(repository_error)?
    .ok_or(ApiError::not_found())?;
    audit(&mut transaction, caller, audited, "document", id.as_uuid(), json!({})).await?;
    let event = match transition {
        DocumentTransition::Archive => "document.archived",
        DocumentTransition::Restore => "document.restored",
        DocumentTransition::Delete => "document.deleted",
    };
    super::host_events::publish_document_event(&mut transaction, event, &record).await?;
    commit(transaction).await?;
    Ok(Json(document_view(&record)))
}

async fn delete_document_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    document_transition(&state, &caller, &id, DocumentTransition::Delete).await
}

async fn archive_document_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    document_transition(&state, &caller, &id, DocumentTransition::Archive).await
}

async fn restore_document_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    document_transition(&state, &caller, &id, DocumentTransition::Restore).await
}

// ---------------------------------------------------------------------------------------------
// Document members and team grants

async fn list_document_members_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "document_members", &id, &query)?;
    let after: Option<PrincipalId> = paging.after()?;
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let members = list_document_members(&mut transaction, id, after).await.map_err(repository_error)?;
    Ok(Json(grants_view(&paging, "principal_id", &members)))
}

async fn set_document_member_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, principal)): Path<(String, String)>,
    ApiJson(body): ApiJson<RoleBody>,
) -> Result<Json<Value>, ApiError> {
    let role = parse_role(&body.role).ok_or_else(invalid)?;
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    let principal: PrincipalId = principal.parse().map_err(|_| ApiError::not_found())?;
    set_document_member(&mut transaction, id, principal, role).await.map_err(membership_error)?;
    let metadata = json!({"principal_id": principal.to_string(), "role": role_str(role)});
    audit(&mut transaction, &caller, "document.member.set", "document", id.as_uuid(), metadata).await?;
    commit(transaction).await?;
    super::tenants_api::revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(Json(json!({"principal_id": principal.to_string(), "role": role_str(role)})))
}

async fn remove_document_member_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, principal)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    let principal: PrincipalId = principal.parse().map_err(|_| ApiError::not_found())?;
    if !remove_document_member(&mut transaction, id, principal).await.map_err(membership_error)? {
        return Err(ApiError::not_found());
    }
    audit(&mut transaction, &caller, "document.member.remove", "document", id.as_uuid(), json!({"principal_id": principal.to_string()})).await?;
    commit(transaction).await?;
    super::tenants_api::revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_document_team_grants_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let paging = Paging::new(&caller, "document_team_grants", &id, &query)?;
    let after: Option<Uuid> = paging.after()?;
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let grants = list_document_team_grants(&mut transaction, id, after).await.map_err(repository_error)?;
    Ok(Json(grants_view(&paging, "team_id", &grants)))
}

async fn set_document_team_grant_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, team)): Path<(String, String)>,
    ApiJson(body): ApiJson<RoleBody>,
) -> Result<Json<Value>, ApiError> {
    let role = parse_role(&body.role).ok_or_else(invalid)?;
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    let team = Uuid::parse_str(&team).map_err(|_| ApiError::not_found())?;
    set_document_team_grant(&mut transaction, id, team, role).await.map_err(membership_error)?;
    let metadata = json!({"team_id": team.to_string(), "role": role_str(role)});
    audit(&mut transaction, &caller, "document.team_grant.set", "document", id.as_uuid(), metadata).await?;
    commit(transaction).await?;
    super::tenants_api::revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(Json(json!({"team_id": team.to_string(), "role": role_str(role)})))
}

async fn remove_document_team_grant_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, team)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    let team = Uuid::parse_str(&team).map_err(|_| ApiError::not_found())?;
    if !remove_document_team_grant(&mut transaction, id, team).await.map_err(membership_error)? {
        return Err(ApiError::not_found());
    }
    audit(&mut transaction, &caller, "document.team_grant.remove", "document", id.as_uuid(), json!({"team_id": team.to_string()})).await?;
    commit(transaction).await?;
    super::tenants_api::revoke_after_commit(&state, AccessRevocation::GrantRevoked).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Share links and the collaboration link

async fn list_share_links_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Response, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    let links = list_share_links(&mut transaction, id).await.map_err(share_error)?;
    let items: Vec<Value> = links
        .iter()
        .map(|link| {
            json!({
                "id": link.id.to_string(),
                "role": link.role.as_str(),
                "label": link.label,
                "created_by": link.created_by.to_string(),
                "created_at": link.created_at,
                "expires_at": link.expires_at,
                "revoked_at": link.revoked_at,
            })
        })
        .collect();
    Ok(no_store(Json(json!({"items": items})).into_response()))
}

async fn create_share_link_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CreateShareLinkBody>,
) -> Result<Response, ApiError> {
    let role = ShareRole::parse(&body.role).ok_or_else(invalid)?;
    let mut transaction = begin(&state, &caller).await?;
    let (id, access) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    // A link never grants more than its creator holds.
    if role == ShareRole::Editor && !allows(&access, Kind::Document, Action::DocumentEdit) {
        return Err(ApiError::forbidden());
    }
    let link =
        create_share_link(&mut transaction, id, role, body.label.as_deref(), caller.principal, body.expires_in_secs).await.map_err(share_error)?;
    let metadata = json!({"link_id": link.id.to_string(), "role": role.as_str()});
    audit(&mut transaction, &caller, "share_link.create", "document", id.as_uuid(), metadata).await?;
    commit(transaction).await?;
    // The secret is returned once and never stored or logged.
    Ok(no_store(created(json!({"id": link.id.to_string(), "role": role.as_str(), "secret": link.secret}))))
}

async fn revoke_share_link_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, link)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (id, _) = document_access(&mut transaction, &caller, &id, Action::DocumentShare).await?;
    let link = Uuid::parse_str(&link).map_err(|_| ApiError::not_found())?;
    // The link must belong to this document (the path is authorized per document).
    let links = list_share_links(&mut transaction, id).await.map_err(share_error)?;
    if !links.iter().any(|record| record.id == link) || !revoke_share_link(&mut transaction, link).await.map_err(share_error)? {
        return Err(ApiError::not_found());
    }
    audit(&mut transaction, &caller, "share_link.revoke", "document", id.as_uuid(), json!({"link_id": link.to_string()})).await?;
    commit(transaction).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `FIGCRAFT_PUBLIC_URL` (read once; the relay bootstrap reads the same variable).
pub(crate) fn public_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("FIGCRAFT_PUBLIC_URL").ok().filter(|value| !value.trim().is_empty()).unwrap_or_else(|| DEFAULT_PUBLIC_URL.to_string())
    })
}

/// The relay invite (`ws(s)://<host>/ws/v1/collab?file=<id>&token=<secret>`) and the editor URL
/// that joins with it, for `public_url`.
pub(crate) fn collab_urls(public_url: &str, document: CloudDocumentId, secret: &str) -> Result<(String, String), ApiError> {
    let base = public_url.trim_end_matches('/');
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(ApiError::unavailable());
    };
    let invite = format!("{socket}{}?file={document}&token={secret}", crate::relay::RELAY_PATH);
    let editor = crate::relay::editor_url(public_url, document, secret).map_err(|_| ApiError::unavailable())?;
    Ok((invite, editor))
}

/// Authorize joining `id`'s realtime room: the document must be visible (else `404`) and
/// `document.join_realtime` allowed (else `403`). Returns the document and whether the caller
/// may edit it.
pub(crate) async fn authorize_realtime(state: &AppState, caller: &Authenticated, id: &str) -> Result<(CloudDocumentId, bool), ApiError> {
    let mut transaction = begin(state, caller).await?;
    let (id, access) = document_access(&mut transaction, caller, id, Action::DocumentJoinRealtime).await?;
    let _ = transaction.rollback().await;
    Ok((id, allows(&access, Kind::Document, Action::DocumentEdit)))
}

/// The collaboration invite for the caller. A signed-in browser gets a ticket-based invite (a
/// realtime ticket as the hello token, valid [`super::realtime::TICKET_SECS`] seconds: the editor
/// fetches a fresh one to reconnect); other callers, or a deployment without
/// `SESSION_SIGNING_KEY`, get an 8-hour collaboration share link as before.
async fn collab_link_handler(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Response, ApiError> {
    if caller.session.kind == CredentialKind::BrowserSession && state.realtime_key().is_some() {
        let (id, edit) = authorize_realtime(&state, &caller, &id).await?;
        let minted = super::realtime::mint_for(&state, &caller, id, edit)?;
        let (invite, editor_url) = collab_urls(public_url(), id, &minted.ticket)?;
        return Ok(no_store(
            Json(json!({
                "document_id": id.to_string(),
                "access": if edit { "edit" } else { "view" },
                "kind": "ticket",
                "link_id": Value::Null,
                "expires_in_secs": super::realtime::TICKET_SECS,
                "invite": invite,
                "editor_url": editor_url,
            }))
            .into_response(),
        ));
    }
    let mut transaction = begin(&state, &caller).await?;
    let (id, access) = document_access(&mut transaction, &caller, &id, Action::DocumentJoinRealtime).await?;
    let edit = allows(&access, Kind::Document, Action::DocumentEdit);
    let role = if edit { ShareRole::Editor } else { ShareRole::Viewer };
    let link = create_share_link(&mut transaction, id, role, Some(COLLAB_LINK_LABEL), caller.principal, Some(COLLAB_LINK_SECS))
        .await
        .map_err(share_error)?;
    let metadata = json!({"link_id": link.id.to_string(), "role": role.as_str()});
    audit(&mut transaction, &caller, "document.collab_link", "document", id.as_uuid(), metadata).await?;
    commit(transaction).await?;
    let (invite, editor_url) = collab_urls(public_url(), id, &link.secret)?;
    Ok(no_store(
        Json(json!({
            "document_id": id.to_string(),
            "access": if edit { "edit" } else { "view" },
            "kind": "share_link",
            "link_id": link.id.to_string(),
            "expires_in_secs": COLLAB_LINK_SECS,
            "invite": invite,
            "editor_url": editor_url,
        }))
        .into_response(),
    ))
}
