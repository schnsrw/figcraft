//! Tenant and tenant-member APIs (S4-019), plus the access helpers the team API shares.
//!
//! Routes (under `/api/v1`):
//!
//! | Method and path | Action |
//! |---|---|
//! | `GET /tenants` | the caller's tenants (any credential but a share link) |
//! | `POST /tenants` | create a tenant owned by the caller; multi-tenant mode, browser sessions only |
//! | `GET /tenants/{tenant}` | `tenant.view` |
//! | `PATCH /tenants/{tenant}` | rename, suspend or reactivate: `tenant.update_policy` |
//! | `GET /tenants/{tenant}/members?q=` | `member.view` (prefix search on display name) |
//! | `POST /tenants/{tenant}/members` | `member.manage` (`member.manage_owners` for owners) |
//! | `PATCH /tenants/{tenant}/members/{principal}` | role or state change, as above |
//! | `DELETE /tenants/{tenant}/members/{principal}` | remove, as above |
//! | `GET /tenants/{tenant}/members/{principal}/permissions` | `member.manage_permissions` |
//! | `PUT`/`DELETE /tenants/{tenant}/members/{principal}/permissions/{permission}` | `member.manage_permissions` + grant policy |
//!
//! Every handler: [`Authenticated`] → the path tenant must be the credential's tenant (else
//! `404`) → [`resolve_tenant_access`] in a tenant transaction → [`decide`] → the disclosure
//! policy (no visible tenant role: `404`; a member lacking the permission: `403`). Mutations
//! record an audit event in the same transaction.
//!
//! # Sensitive-permission grant policy
//!
//! Granting is owner-only (`member.manage_permissions`). A permission may only be granted to an
//! active member whose *current* tenant role could use it: some action that needs the permission
//! must be allowed for that role by [`role_allows`]. So `billing`, `export` (tenant export) and
//! `deletion` go to owners, `provider_keys`, `support` and audit `export` to owners and admins,
//! and nothing goes to editors, commenters or viewers.

use std::collections::BTreeSet;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use figcraft_cloud_auth::authz::{Action, Resource, SensitivePermission, Subject, decide, role_allows};
use figcraft_cloud_domain::{DeploymentMode, ErrorCode, PrincipalId, ResourceState, Role, TenantId, Uuid};
use figcraft_cloud_storage::access_guard::MembershipRequirement;
use figcraft_cloud_storage::audit::record_audit_event;
use figcraft_cloud_storage::effective_roles::{TenantAccess, resolve_tenant_access};
use figcraft_cloud_storage::memberships::{self, MemberPermission, MemberState, MembershipError, parse_role, role_str, valid_tenant_role};
use figcraft_cloud_storage::metadata::{PageSize, TenantRecord};
use figcraft_cloud_storage::revocation::{AccessRevocation, apply_revocation};
use figcraft_cloud_storage::tenants::{self, NewTenant, TenantError};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Authenticated, CredentialKind};
use super::authenticator::hold_caller_access;
use super::context::RequestContext;
use super::cursor::{CursorScope, ListQuery, PageBody, PageRequest, Position, SearchTerm};
use super::error::{ApiError, ApiJson};

/// Default plan and region for a self-service tenant.
const DEFAULT_PLAN: &str = "free";
const DEFAULT_REGION: &str = "default";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/tenants", get(list_tenants).post(create_tenant))
        .route("/tenants/{tenant}", get(get_tenant).patch(update_tenant))
        .route("/tenants/{tenant}/members", get(list_members).post(add_member))
        .route("/tenants/{tenant}/members/{principal}", axum::routing::patch(update_member).delete(remove_member))
        .route("/tenants/{tenant}/members/{principal}/permissions", get(list_permissions))
        .route("/tenants/{tenant}/members/{principal}/permissions/{permission}", put(grant_permission).delete(revoke_permission))
        .merge(super::teams_api::routes())
}

// ---------------------------------------------------------------------------------------------
// Shared access helpers (also used by `teams_api`).

/// Share links never reach tenant administration.
pub(crate) fn require_member_credential(caller: &Authenticated) -> Result<(), ApiError> {
    if caller.session.kind == CredentialKind::ShareLink { Err(ApiError::forbidden()) } else { Ok(()) }
}

/// Parse an opaque identifier from a path segment; anything malformed is `404`.
pub(crate) fn path_uuid(value: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value).map_err(|_| ApiError::not_found())
}

/// The path tenant, which must be the tenant the credential is bound to (else `404`).
pub(crate) fn path_tenant(caller: &Authenticated, value: &str) -> Result<TenantId, ApiError> {
    let tenant = TenantId::try_from(path_uuid(value)?).map_err(|_| ApiError::not_found())?;
    if tenant != caller.tenant.tenant_id() {
        return Err(ApiError::not_found());
    }
    Ok(tenant)
}

pub(crate) fn path_principal(value: &str) -> Result<PrincipalId, ApiError> {
    PrincipalId::try_from(path_uuid(value)?).map_err(|_| ApiError::not_found())
}

pub(crate) fn storage_unavailable<E>(_: E) -> ApiError {
    ApiError::unavailable()
}

/// The authorization subject for resolved tenant facts.
pub(crate) fn subject(access: &TenantAccess) -> Subject {
    let permissions: BTreeSet<SensitivePermission> =
        access.permissions.iter().filter_map(|permission| SensitivePermission::parse(permission.as_str())).collect();
    Subject { principal_active: access.principal_active, tenant_state: access.tenant_state, tenant_role: access.tenant_role, permissions }
}

/// A tenant transaction for the caller with their authorization subject.
pub(crate) struct TenantCall<'a> {
    pub(crate) transaction: TenantTransaction<'a>,
    pub(crate) subject: Subject,
    pub(crate) principal: PrincipalId,
}

/// Open the caller's tenant: share links `403`; another tenant, an unknown tenant or no active
/// membership `404`; a suspended tenant the caller is an active member of `409 resource_inactive`.
pub(crate) async fn open<'a>(state: &'a AppState, caller: &Authenticated, tenant: &str) -> Result<TenantCall<'a>, ApiError> {
    require_member_credential(caller)?;
    let tenant = path_tenant(caller, tenant)?;
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    hold_caller_access(state, &mut transaction, caller, MembershipRequirement::ActiveMember).await?;
    let access = resolve_tenant_access(&mut transaction, caller.principal).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    if access.tenant_role.is_none() {
        if access.tenant_state != ResourceState::Active
            && access.principal_active
            && raw_active_role(&mut transaction, caller.principal).await?.is_some()
        {
            return Err(ApiError::new(ErrorCode::ResourceInactive));
        }
        return Err(ApiError::not_found());
    }
    Ok(TenantCall { transaction, subject: subject(&access), principal: caller.principal })
}

/// The caller's tenant role when their membership row is active, whatever the tenant's state.
async fn raw_active_role(transaction: &mut TenantTransaction<'_>, principal: PrincipalId) -> Result<Option<Role>, ApiError> {
    let member = memberships::get_tenant_member(transaction, principal).await.map_err(storage_unavailable)?;
    Ok(member.filter(|member| member.state == MemberState::Active).map(|member| member.role))
}

/// A tenant-scope check for a member who can see the tenant: denied is `403`.
pub(crate) fn require_tenant(subject: &Subject, action: Action) -> Result<(), ApiError> {
    if decide(subject, &Resource::Tenant, action).allow { Ok(()) } else { Err(ApiError::forbidden()) }
}

/// Record a succeeded audit event in the caller's transaction.
pub(crate) async fn audit(
    transaction: &mut TenantTransaction<'_>,
    actor: PrincipalId,
    action: &str,
    kind: &str,
    resource: Option<Uuid>,
    request: &RequestContext,
    metadata: &serde_json::Value,
) -> Result<(), ApiError> {
    let request_id = Uuid::parse_str(&request.request_id.to_string()).ok();
    record_audit_event(transaction, Uuid::new_v4(), Some(actor.as_uuid()), action, kind, resource, request_id, None, "succeeded", metadata)
        .await
        .map(|_| ())
        .map_err(|_| ApiError::new(ErrorCode::Internal))
}

pub(crate) async fn commit(transaction: TenantTransaction<'_>) -> Result<(), ApiError> {
    transaction.commit().await.map_err(storage_unavailable)
}

/// Close what a committed access change leaves open (S4-017 policy,
/// `revocation::apply_revocation`): a principal left with no active membership in any active
/// tenant loses every browser session. Called only after the change commits. A failure here does
/// not undo the change or fail the request: every later API request re-resolves tenant access
/// (`authorize_session_in_tenant`) and the relay sweep re-checks live members, so access is
/// already refused; the sessions are only left to expire, and the failure is reported (ids only).
pub(crate) async fn revoke_after_commit(state: &AppState, revocation: AccessRevocation) {
    if apply_revocation(state.pool(), revocation).await.is_err() {
        eprintln!("figcraft-cloud: revocation follow-up failed ({revocation:?}); access is still refused per request");
    }
}

pub(crate) fn invalid() -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest)
}

pub(crate) fn conflict() -> ApiError {
    ApiError::new(ErrorCode::Conflict)
}

fn membership_error(error: MembershipError) -> ApiError {
    match error {
        MembershipError::InvalidRole | MembershipError::Constraint => invalid(),
        MembershipError::NotFound => ApiError::not_found(),
        MembershipError::NotActiveMember | MembershipError::LastOwner | MembershipError::Conflict => conflict(),
        MembershipError::Database => ApiError::unavailable(),
        MembershipError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn tenant_error(error: TenantError) -> ApiError {
    match error {
        TenantError::NotFound => ApiError::not_found(),
        TenantError::Conflict | TenantError::InvalidState => conflict(),
        TenantError::Database => ApiError::unavailable(),
        TenantError::Audit | TenantError::CorruptRow => ApiError::new(ErrorCode::Internal),
        _ => invalid(),
    }
}

fn page_size(limit: u32) -> Result<PageSize, ApiError> {
    PageSize::new(limit).map_err(|_| invalid())
}

// ---------------------------------------------------------------------------------------------
// Tenants.

#[derive(Debug, Serialize)]
pub(crate) struct TenantBody {
    id: TenantId,
    slug: String,
    name: String,
    state: ResourceState,
    plan_id: String,
    home_region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
}

impl TenantBody {
    fn new(record: TenantRecord, role: Option<String>) -> Self {
        Self {
            id: record.id,
            slug: record.slug,
            name: record.name,
            state: record.state,
            plan_id: record.plan_id,
            home_region: record.home_region,
            role,
        }
    }
}

async fn list_tenants(State(state): State<AppState>, caller: Authenticated, Query(query): Query<ListQuery>) -> Result<Response, ApiError> {
    require_member_credential(&caller)?;
    let principal = caller.principal.to_string();
    let request = PageRequest::new(CursorScope::new("tenants", &[&principal]), query.limit, query.cursor.as_deref())?;
    let after = request.after.as_ref().map(|position| TenantId::try_from(position.id)).transpose().map_err(|_| super::cursor::invalid_cursor())?;
    let page = tenants::list_tenants_for_principal(state.pool(), caller.principal, after, page_size(request.limit)?).await.map_err(tenant_error)?;
    let next_cursor = page.next_cursor.and_then(|id| request.cursor_for(&Position { sort_key: String::new(), id: id.as_uuid() }));
    let items = page.items.into_iter().map(|item| TenantBody::new(item.tenant, Some(item.role))).collect();
    Ok(axum::Json(PageBody { items, next_cursor }).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTenant {
    slug: String,
    name: String,
    plan_id: Option<String>,
    home_region: Option<String>,
}

async fn create_tenant(State(state): State<AppState>, caller: Authenticated, ApiJson(body): ApiJson<CreateTenant>) -> Result<Response, ApiError> {
    // Self-service tenants exist only in multi-tenant mode, and only people (not service
    // accounts or share links) can own one.
    if state.config().deployment != DeploymentMode::MultiTenant || caller.session.kind != CredentialKind::BrowserSession {
        return Err(ApiError::forbidden());
    }
    let id = TenantId::random();
    let mut transaction = begin_tenant_transaction(state.pool(), id).await.map_err(storage_unavailable)?;
    hold_caller_access(&state, &mut transaction, &caller, MembershipRequirement::None).await?;
    let new = NewTenant {
        slug: &body.slug,
        name: &body.name,
        plan_id: body.plan_id.as_deref().unwrap_or(DEFAULT_PLAN),
        home_region: body.home_region.as_deref().unwrap_or(DEFAULT_REGION),
    };
    let record = tenants::create_tenant_with_owner(&mut transaction, new, caller.principal).await.map_err(|error| match error {
        TenantError::OwnerUnavailable => ApiError::forbidden(),
        other => tenant_error(other),
    })?;
    commit(transaction).await?;
    Ok((StatusCode::CREATED, axum::Json(TenantBody::new(record, Some(role_str(Role::TenantOwner).to_owned())))).into_response())
}

async fn tenant_record(transaction: &mut TenantTransaction<'_>) -> Result<TenantRecord, ApiError> {
    let row: Option<(String, String, String, String, String, String)> =
        sqlx::query_as("SELECT id::text, slug, name, state, plan_id, home_region FROM tenants WHERE id = $1::uuid")
            .bind(transaction.tenant_id().to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(storage_unavailable)?;
    let (id, slug, name, state, plan_id, home_region) = row.ok_or(ApiError::not_found())?;
    let state: ResourceState = serde_json::from_value(serde_json::Value::String(state)).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(TenantRecord { id: id.parse().map_err(|_| ApiError::new(ErrorCode::Internal))?, slug, name, state, plan_id, home_region })
}

async fn get_tenant(State(state): State<AppState>, caller: Authenticated, Path(tenant): Path<String>) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::TenantView)?;
    let record = tenant_record(&mut call.transaction).await?;
    let role = call.subject.tenant_role.map(|role| role_str(role).to_owned());
    Ok(axum::Json(TenantBody::new(record, role)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateTenant {
    name: Option<String>,
    /// `suspended` or `active`.
    state: Option<String>,
    reason: Option<String>,
}

async fn update_tenant(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path(tenant): Path<String>,
    ApiJson(body): ApiJson<UpdateTenant>,
) -> Result<Response, ApiError> {
    if body.name.is_none() && body.state.is_none() {
        return Err(invalid());
    }
    let target = match body.state.as_deref() {
        None => None,
        Some("active") => Some(ResourceState::Active),
        Some("suspended") => Some(ResourceState::Suspended),
        Some(_) => return Err(invalid()),
    };
    // Reactivation is the one change allowed while the tenant is suspended: decide as if it were
    // active, with the caller's raw (active) membership role.
    let mut call = match open(&state, &caller, &tenant).await {
        Err(error) if error.code() == ErrorCode::ResourceInactive && target == Some(ResourceState::Active) && body.name.is_none() => {
            open_suspended(&state, &caller, &tenant).await?
        }
        other => other?,
    };
    require_tenant(&call.subject, Action::TenantUpdatePolicy)?;
    if let Some(name) = &body.name {
        rename_tenant(&mut call.transaction, name, call.principal, &context).await?;
    }
    match target {
        Some(ResourceState::Suspended) => {
            tenants::suspend_tenant(&mut call.transaction, call.principal, body.reason.as_deref()).await.map_err(tenant_error)?;
        }
        Some(_) => {
            tenants::reactivate_tenant(&mut call.transaction, call.principal).await.map_err(tenant_error)?;
        }
        None => {}
    }
    let record = tenant_record(&mut call.transaction).await?;
    commit(call.transaction).await?;
    if target == Some(ResourceState::Suspended) {
        revoke_after_commit(&state, AccessRevocation::TenantSuspended).await;
    }
    let role = call.subject.tenant_role.map(|role| role_str(role).to_owned());
    Ok(axum::Json(TenantBody::new(record, role)).into_response())
}

/// [`open`] for a suspended tenant: the subject's tenant state is treated as active so only the
/// role matrix decides, using the caller's active membership role.
async fn open_suspended<'a>(state: &'a AppState, caller: &Authenticated, tenant: &str) -> Result<TenantCall<'a>, ApiError> {
    let tenant = path_tenant(caller, tenant)?;
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    hold_caller_access(state, &mut transaction, caller, MembershipRequirement::ActiveMember).await?;
    let access = resolve_tenant_access(&mut transaction, caller.principal).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    if access.tenant_state != ResourceState::Suspended || !access.principal_active {
        return Err(ApiError::not_found());
    }
    let role = raw_active_role(&mut transaction, caller.principal).await?.ok_or(ApiError::not_found())?;
    let subject = Subject { principal_active: true, tenant_state: ResourceState::Active, tenant_role: Some(role), permissions: BTreeSet::new() };
    Ok(TenantCall { transaction, subject, principal: caller.principal })
}

async fn rename_tenant(transaction: &mut TenantTransaction<'_>, name: &str, actor: PrincipalId, context: &RequestContext) -> Result<(), ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > tenants::MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(invalid());
    }
    let tenant = transaction.tenant_id();
    let changed: Option<String> = sqlx::query_scalar(
        "UPDATE tenants SET name = $2, updated_at = now() WHERE id = $1::uuid AND state = 'active' AND name <> $2 RETURNING id::text",
    )
    .bind(tenant.to_string())
    .bind(name)
    .fetch_optional(transaction.connection())
    .await
    .map_err(storage_unavailable)?;
    if changed.is_some() {
        audit(transaction, actor, "tenant.rename", "tenant", Some(tenant.as_uuid()), context, &serde_json::json!({})).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Members.

#[derive(Debug, Serialize)]
struct MemberBody {
    principal_id: PrincipalId,
    display_name: String,
    role: String,
    state: String,
}

async fn list_members(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let search = SearchTerm::parse(query.q.as_deref())?;
    let tenant_text = call.transaction.tenant_id().to_string();
    let scope = CursorScope::new("tenant_members", &[&tenant_text, search.as_ref().map_or("", SearchTerm::as_str)]);
    let request = PageRequest::new(scope, query.limit, query.cursor.as_deref())?;
    let (after_name, after_id) = request.after_binds();
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT m.principal_id::text, p.display_name, m.role, m.state FROM tenant_members m JOIN principals p ON p.id = m.principal_id \
         WHERE m.tenant_id = $1::uuid AND m.state <> 'deleted' \
           AND ($2::text IS NULL OR p.display_name ILIKE $2 ESCAPE '\\') \
           AND ($3::text IS NULL OR (p.display_name COLLATE \"C\", m.principal_id) > ($3::text COLLATE \"C\", $4::uuid)) \
         ORDER BY p.display_name COLLATE \"C\", m.principal_id LIMIT $5",
    )
    .bind(&tenant_text)
    .bind(search.as_ref().map(SearchTerm::prefix_pattern))
    .bind(after_name)
    .bind(after_id)
    .bind(request.fetch_limit())
    .fetch_all(call.transaction.connection())
    .await
    .map_err(storage_unavailable)?;
    let mut members = Vec::with_capacity(rows.len());
    for (principal, display_name, role, member_state) in rows {
        let principal_id: PrincipalId = principal.parse().map_err(|_| ApiError::new(ErrorCode::Internal))?;
        members.push(MemberBody { principal_id, display_name, role, state: member_state });
    }
    let (items, next_cursor) =
        request.finish(members, |member| Position { sort_key: member.display_name.clone(), id: member.principal_id.as_uuid() });
    Ok(axum::Json(PageBody { items, next_cursor }).into_response())
}

fn tenant_role(value: &str) -> Result<Role, ApiError> {
    parse_role(value).filter(|role| valid_tenant_role(*role)).ok_or_else(invalid)
}

/// `member.manage_owners` when an owner is involved (as the target or the new role).
fn member_action(current: Option<Role>, new: Option<Role>) -> Action {
    if current == Some(Role::TenantOwner) || new == Some(Role::TenantOwner) { Action::MemberManageOwners } else { Action::MemberManage }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddMember {
    principal_id: PrincipalId,
    role: String,
}

async fn add_member(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path(tenant): Path<String>,
    ApiJson(body): ApiJson<AddMember>,
) -> Result<Response, ApiError> {
    let role = tenant_role(&body.role)?;
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberManage)?;
    require_tenant(&call.subject, member_action(None, Some(role)))?;
    let principal_ok: Option<bool> = sqlx::query_scalar("SELECT state = 'active' FROM principals WHERE id = $1::uuid")
        .bind(body.principal_id.to_string())
        .fetch_optional(call.transaction.connection())
        .await
        .map_err(storage_unavailable)?;
    if principal_ok != Some(true) {
        return Err(invalid());
    }
    memberships::add_tenant_member(&mut call.transaction, body.principal_id, role).await.map_err(membership_error)?;
    let metadata = serde_json::json!({ "principal_id": body.principal_id.to_string(), "role": role_str(role) });
    audit(&mut call.transaction, call.principal, "member.add", "tenant_member", Some(body.principal_id.as_uuid()), &context, &metadata).await?;
    commit(call.transaction).await?;
    Ok(StatusCode::CREATED.into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateMember {
    role: Option<String>,
    /// `active` or `suspended`.
    state: Option<String>,
}

async fn update_member(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path((tenant, principal)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateMember>,
) -> Result<Response, ApiError> {
    let role = body.role.as_deref().map(tenant_role).transpose()?;
    let target_state = match body.state.as_deref() {
        None => None,
        Some("active") => Some(MemberState::Active),
        Some("suspended") => Some(MemberState::Suspended),
        Some(_) => return Err(invalid()),
    };
    if role.is_none() && target_state.is_none() {
        return Err(invalid());
    }
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let target = path_principal(&principal)?;
    let current = memberships::get_tenant_member(&mut call.transaction, target)
        .await
        .map_err(membership_error)?
        .filter(|member| member.state != MemberState::Deleted)
        .ok_or(ApiError::not_found())?;
    require_tenant(&call.subject, Action::MemberManage)?;
    require_tenant(&call.subject, member_action(Some(current.role), role))?;
    if let Some(role) = role {
        memberships::change_tenant_member_role(&mut call.transaction, target, role).await.map_err(membership_error)?;
    }
    match target_state {
        Some(MemberState::Suspended) => memberships::suspend_tenant_member(&mut call.transaction, target).await.map_err(membership_error)?,
        Some(_) => memberships::reactivate_tenant_member(&mut call.transaction, target).await.map_err(membership_error)?,
        None => {}
    }
    let metadata = serde_json::json!({
        "principal_id": target.to_string(),
        "from_role": role_str(current.role),
        "role": role.map(role_str),
        "state": target_state.map(MemberState::as_str),
    });
    audit(&mut call.transaction, call.principal, "member.update", "tenant_member", Some(target.as_uuid()), &context, &metadata).await?;
    commit(call.transaction).await?;
    if target_state == Some(MemberState::Suspended) {
        revoke_after_commit(&state, AccessRevocation::TenantMemberSuspended { principal: target }).await;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_member(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path((tenant, principal)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let target = path_principal(&principal)?;
    let current = memberships::get_tenant_member(&mut call.transaction, target)
        .await
        .map_err(membership_error)?
        .filter(|member| member.state != MemberState::Deleted)
        .ok_or(ApiError::not_found())?;
    require_tenant(&call.subject, Action::MemberManage)?;
    require_tenant(&call.subject, member_action(Some(current.role), None))?;
    memberships::remove_tenant_member(&mut call.transaction, target).await.map_err(membership_error)?;
    let metadata = serde_json::json!({ "principal_id": target.to_string(), "role": role_str(current.role) });
    audit(&mut call.transaction, call.principal, "member.remove", "tenant_member", Some(target.as_uuid()), &context, &metadata).await?;
    commit(call.transaction).await?;
    revoke_after_commit(&state, AccessRevocation::TenantMemberRemoved { principal: target }).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------------------------
// Sensitive permissions.

/// Whether `role` could use `permission`: some action needing it is allowed for the role.
pub(crate) fn grant_policy_allows(permission: SensitivePermission, role: Role) -> bool {
    Action::ALL.iter().any(|action| action.sensitive_permission() == Some(permission) && role_allows(role, *action))
}

fn path_permission(value: &str) -> Result<(SensitivePermission, MemberPermission), ApiError> {
    let sensitive = SensitivePermission::parse(value).ok_or(ApiError::not_found())?;
    let member = MemberPermission::parse(value).ok_or(ApiError::not_found())?;
    Ok((sensitive, member))
}

#[derive(Debug, Serialize)]
struct PermissionsBody {
    permissions: Vec<&'static str>,
}

async fn list_permissions(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, principal)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberManagePermissions)?;
    let target = path_principal(&principal)?;
    memberships::get_tenant_member(&mut call.transaction, target)
        .await
        .map_err(membership_error)?
        .filter(|member| member.state != MemberState::Deleted)
        .ok_or(ApiError::not_found())?;
    let permissions = memberships::list_member_permissions(&mut call.transaction, target).await.map_err(membership_error)?;
    Ok(axum::Json(PermissionsBody { permissions: permissions.into_iter().map(MemberPermission::as_str).collect() }).into_response())
}

async fn grant_permission(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path((tenant, principal, permission)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberManagePermissions)?;
    let target = path_principal(&principal)?;
    let (sensitive, member_permission) = path_permission(&permission)?;
    let member = memberships::get_tenant_member(&mut call.transaction, target)
        .await
        .map_err(membership_error)?
        .filter(|member| member.state != MemberState::Deleted)
        .ok_or(ApiError::not_found())?;
    if member.state != MemberState::Active || !grant_policy_allows(sensitive, member.role) {
        return Err(conflict());
    }
    let granted =
        memberships::grant_member_permission(&mut call.transaction, target, member_permission, call.principal).await.map_err(membership_error)?;
    if granted {
        let metadata = serde_json::json!({ "principal_id": target.to_string(), "permission": sensitive.as_str() });
        audit(&mut call.transaction, call.principal, "member.permission_grant", "tenant_member", Some(target.as_uuid()), &context, &metadata).await?;
    }
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn revoke_permission(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path((tenant, principal, permission)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let mut call = open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberManagePermissions)?;
    let target = path_principal(&principal)?;
    let (sensitive, member_permission) = path_permission(&permission)?;
    let revoked = memberships::revoke_member_permission(&mut call.transaction, target, member_permission).await.map_err(membership_error)?;
    if !revoked {
        return Err(ApiError::not_found());
    }
    let metadata = serde_json::json!({ "principal_id": target.to_string(), "permission": sensitive.as_str() });
    audit(&mut call.transaction, call.principal, "member.permission_revoke", "tenant_member", Some(target.as_uuid()), &context, &metadata).await?;
    commit(call.transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
#[path = "tenants_api_tests.rs"]
mod tests;
