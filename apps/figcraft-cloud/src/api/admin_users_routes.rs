//! Administering password accounts (S6-037).
//!
//! | Route (under `/api/v1`) | Permission | What it does |
//! |---|---|---|
//! | `GET tenants/{t}/users?q=&limit=&cursor=` | `member.view` | the workspace's password accounts: `{principal_id, login, display_name, role, member_state, state, locked}` |
//! | `POST tenants/{t}/users` | `member.manage` (`member.manage_owners` for an owner) | `{login, display_name, role?, setup_lifetime_secs?}` creates the account and membership and returns a one-time **setup link token** (`201`; shown once; the person chooses their password with `POST auth/password/reset`) |
//! | `POST tenants/{t}/users/{p}/disable` | `member.manage` (+owners) | credential disabled, membership suspended, live sessions ended at once (the per-request membership check refuses them immediately; sessions are revoked and the relay sweeper closes rooms) |
//! | `POST tenants/{t}/users/{p}/enable` | `member.manage` (+owners) | the reverse; failure counters cleared |
//! | `POST tenants/{t}/users/{p}/reset-link` | `member.manage` (+owners) | issues a single-use reset token that expires in one hour, replacing older links; shown once |
//! | `POST tenants/{t}/users/{p}/revoke-sessions` | `member.manage` (+owners) | ends every browser session of the person (they are signed out everywhere) |
//! | `POST auth/password/reset` | none (the token is the credential) | `{token, new_password}`: sets the password, ends all the person's sessions, `200 {tenant_id}`; a token that is unknown, expired, revoked, used, or belongs to a disabled account is one fixed `404` |
//!
//! Everything is audited (`account.admin_create`, `credential.reset_issue`, `credential.reset_used`,
//! `credential.disable`/`enable`, `account.revoke_sessions`) without logins, passwords or tokens in
//! the trail. Administrators cannot touch an owner's account without the owners permission, and
//! the last active owner cannot be disabled.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use figcraft_cloud_auth::authz::Action;
use figcraft_cloud_domain::{ErrorCode, PrincipalId, Role};
use figcraft_cloud_storage::accounts::{self, AccountError};
use figcraft_cloud_storage::credentials::{self, CredentialState};
use figcraft_cloud_storage::memberships::{self, MemberState, MembershipError, parse_role, role_str, valid_tenant_role};
use figcraft_cloud_storage::password_resets::{self, ResetError, ResetPurpose};
use figcraft_cloud_storage::revocation::AccessRevocation;
use figcraft_cloud_storage::sessions::{self, RevokeReason};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::account_routes::hash_password;
use super::auth::Authenticated;
use super::auth_routes::{identity, require_same_origin, respond};
use super::context::RequestContext;
use super::cursor::{CursorScope, ListQuery, PageBody, PageRequest, Position, SearchTerm};
use super::error::{ApiError, ApiJson};
use super::tenants_api::{self, TenantCall, audit, commit, require_tenant, revoke_after_commit, storage_unavailable};

pub(crate) fn auth_routes() -> Router<AppState> {
    Router::new().route("/password/reset", post(reset_password))
}

pub(crate) fn resource_routes() -> Router<AppState> {
    Router::new()
        .route("/tenants/{tenant}/users", get(list_users).post(create_user))
        .route("/tenants/{tenant}/users/{principal}/disable", post(disable_user))
        .route("/tenants/{tenant}/users/{principal}/enable", post(enable_user))
        .route("/tenants/{tenant}/users/{principal}/reset-link", post(reset_link))
        .route("/tenants/{tenant}/users/{principal}/revoke-sessions", post(revoke_sessions))
}

fn membership_error(error: MembershipError) -> ApiError {
    match error {
        MembershipError::InvalidRole | MembershipError::Constraint => ApiError::new(ErrorCode::InvalidRequest),
        MembershipError::NotFound => ApiError::not_found(),
        MembershipError::NotActiveMember | MembershipError::LastOwner | MembershipError::Conflict => ApiError::new(ErrorCode::Conflict),
        MembershipError::Database => ApiError::unavailable(),
        MembershipError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn reset_error(error: ResetError) -> ApiError {
    match error {
        ResetError::InvalidLifetime => ApiError::new(ErrorCode::InvalidRequest),
        ResetError::NoAccount => ApiError::not_found(),
        ResetError::Refused => ApiError::not_found(),
        ResetError::Database | ResetError::Random => ApiError::unavailable(),
        ResetError::Audit | ResetError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

/// `member.manage_owners` when an owner is involved, otherwise `member.manage`.
fn manage_action(role: Role) -> Action {
    if role == Role::TenantOwner { Action::MemberManageOwners } else { Action::MemberManage }
}

/// The person (a member with a password account here) an administrator may act on.
async fn target(call: &mut TenantCall<'_>, principal: &str) -> Result<(PrincipalId, Role, credentials::CredentialRecord), ApiError> {
    let id = tenants_api::path_principal(principal)?;
    let member = memberships::get_tenant_member(&mut call.transaction, id)
        .await
        .map_err(membership_error)?
        .filter(|member| member.state != MemberState::Deleted)
        .ok_or(ApiError::not_found())?;
    let record = credentials::find_for_principal(&mut call.transaction, id).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())?;
    require_tenant(&call.subject, Action::MemberManage)?;
    require_tenant(&call.subject, manage_action(member.role))?;
    Ok((id, member.role, record))
}

// ---------------------------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct UserBody {
    principal_id: PrincipalId,
    login: String,
    display_name: String,
    role: String,
    member_state: String,
    state: &'static str,
    locked: bool,
}

async fn list_users(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let search = SearchTerm::parse(query.q.as_deref())?;
    let tenant_text = call.transaction.tenant_id().to_string();
    let scope = CursorScope::new("tenant_users", &[&tenant_text, search.as_ref().map_or("", SearchTerm::as_str)]);
    let request = PageRequest::new(scope, query.limit, query.cursor.as_deref())?;
    let (after_name, after_id) = request.after_binds();
    let rows: Vec<(String, String, String, String, String, String, bool)> = sqlx::query_as(
        "SELECT c.principal_id::text, c.login, p.display_name, m.role, m.state, c.state, \
                (c.locked_until IS NOT NULL AND c.locked_until > now()) \
         FROM local_credentials c JOIN principals p ON p.id = c.principal_id \
         JOIN tenant_members m ON m.tenant_id = c.tenant_id AND m.principal_id = c.principal_id \
         WHERE c.tenant_id = $1::uuid AND m.state <> 'deleted' \
           AND ($2::text IS NULL OR p.display_name ILIKE $2 ESCAPE '\\') \
           AND ($3::text IS NULL OR (p.display_name COLLATE \"C\", c.principal_id) > ($3::text COLLATE \"C\", $4::uuid)) \
         ORDER BY p.display_name COLLATE \"C\", c.principal_id LIMIT $5",
    )
    .bind(&tenant_text)
    .bind(search.as_ref().map(SearchTerm::prefix_pattern))
    .bind(after_name)
    .bind(after_id)
    .bind(request.fetch_limit())
    .fetch_all(call.transaction.connection())
    .await
    .map_err(storage_unavailable)?;
    let mut users = Vec::with_capacity(rows.len());
    for (principal, login, display_name, role, member_state, state_text, locked) in rows {
        users.push(UserBody {
            principal_id: principal.parse().map_err(|_| ApiError::new(ErrorCode::Internal))?,
            login,
            display_name,
            role,
            member_state,
            state: if state_text == "disabled" { "disabled" } else { "active" },
            locked,
        });
    }
    let (items, next_cursor) = request.finish(users, |user| Position { sort_key: user.display_name.clone(), id: user.principal_id.as_uuid() });
    Ok(axum::Json(PageBody { items, next_cursor }).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUser {
    login: String,
    display_name: String,
    role: Option<String>,
    setup_lifetime_secs: Option<i64>,
}

async fn create_user(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path(tenant): Path<String>,
    ApiJson(body): ApiJson<CreateUser>,
) -> Result<Response, ApiError> {
    let service = state.passwords().ok_or(ApiError::unavailable())?;
    let role = match body.role.as_deref() {
        None => Role::Viewer,
        Some(text) => parse_role(text).filter(|role| valid_tenant_role(*role)).ok_or(ApiError::new(ErrorCode::InvalidRequest))?,
    };
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    require_tenant(&call.subject, Action::MemberManage)?;
    require_tenant(&call.subject, manage_action(role))?;
    if !figcraft_cloud_storage::account_settings::get_settings(&mut call.transaction).await.map_err(storage_unavailable)?.password_enabled {
        return Err(ApiError::forbidden());
    }
    // The account starts with a password nobody knows; the setup link is how the person sets theirs.
    let placeholder = figcraft_cloud_storage::password_resets::random_secret().ok_or(ApiError::unavailable())?;
    let placeholder_hash = service.hash_unchecked(placeholder).await.map_err(|_| ApiError::unavailable())?;
    let principal = accounts::create_account(&mut call.transaction, &body.display_name, &body.login, &placeholder_hash, Some(call.principal))
        .await
        .map_err(|error| match error {
        AccountError::InvalidName | AccountError::InvalidLogin | AccountError::InvalidHash => ApiError::new(ErrorCode::InvalidRequest),
        AccountError::LoginTaken => ApiError::new(ErrorCode::Conflict),
        AccountError::Database => ApiError::unavailable(),
        _ => ApiError::new(ErrorCode::Internal),
    })?;
    memberships::add_tenant_member(&mut call.transaction, principal, role).await.map_err(membership_error)?;
    let issued = password_resets::issue(&mut call.transaction, principal, ResetPurpose::Setup, body.setup_lifetime_secs, call.principal)
        .await
        .map_err(reset_error)?;
    audit(
        &mut call.transaction,
        call.principal,
        "account.admin_create",
        "principal",
        Some(principal.as_uuid()),
        &context,
        &json!({ "role": role_str(role) }),
    )
    .await?;
    let login = credentials::find_for_principal(&mut call.transaction, principal)
        .await
        .map_err(storage_unavailable)?
        .map(|record| record.login)
        .unwrap_or_default();
    commit(call.transaction).await?;
    let reply = json!({
        "principal_id": principal.to_string(),
        "login": login,
        "role": role_str(role),
        "setup_token": issued.token,
        "expires_in_secs": issued.expires_in_secs,
    });
    respond(StatusCode::CREATED, &[], None, Some(reply))
}

async fn disable_user(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, principal)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let (id, _, _) = target(&mut call, &principal).await?;
    // Suspension first: it refuses the last active owner before anything else changes.
    memberships::suspend_tenant_member(&mut call.transaction, id).await.map_err(membership_error)?;
    credentials::set_state(&mut call.transaction, id, CredentialState::Disabled, call.principal).await.map_err(storage_unavailable)?;
    password_resets::revoke_links(&mut call.transaction, id).await.map_err(reset_error)?;
    commit(call.transaction).await?;
    // Live sessions end now; the relay sweeper closes this person's rooms on its next pass.
    revoke_after_commit(&state, AccessRevocation::TenantMemberSuspended { principal: id }).await;
    respond(StatusCode::NO_CONTENT, &[], None, None)
}

async fn enable_user(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, principal)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let (id, _, _) = target(&mut call, &principal).await?;
    memberships::reactivate_tenant_member(&mut call.transaction, id).await.map_err(membership_error)?;
    credentials::set_state(&mut call.transaction, id, CredentialState::Active, call.principal).await.map_err(storage_unavailable)?;
    credentials::record_success(&mut call.transaction, id).await.map_err(storage_unavailable)?;
    commit(call.transaction).await?;
    respond(StatusCode::NO_CONTENT, &[], None, None)
}

async fn reset_link(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((tenant, principal)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let (id, _, record) = target(&mut call, &principal).await?;
    if record.state != CredentialState::Active {
        return Err(ApiError::new(ErrorCode::Conflict));
    }
    let issued = password_resets::issue(&mut call.transaction, id, ResetPurpose::Reset, None, call.principal).await.map_err(reset_error)?;
    commit(call.transaction).await?;
    respond(
        StatusCode::CREATED,
        &[],
        None,
        Some(json!({ "reset_token": issued.token, "expires_in_secs": issued.expires_in_secs, "purpose": "reset" })),
    )
}

async fn revoke_sessions(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path((tenant, principal)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let (id, _, _) = target(&mut call, &principal).await?;
    audit(&mut call.transaction, call.principal, "account.revoke_sessions", "principal", Some(id.as_uuid()), &context, &json!({})).await?;
    commit(call.transaction).await?;
    let revoked = sessions::revoke_all_for_principal(state.pool(), id, RevokeReason::Operator).await.map_err(|_| ApiError::unavailable())?;
    respond(StatusCode::OK, &[], None, Some(json!({ "revoked": revoked })))
}

// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetBody {
    token: String,
    new_password: String,
}

async fn reset_password(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<ResetBody>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    require_same_origin(identity, &headers)?;
    let service = state.passwords().ok_or(ApiError::unavailable())?;
    if body.token.is_empty() || body.token.len() > 128 {
        return Err(ApiError::not_found());
    }
    let hash = hash_password(service, body.new_password, None).await?;
    let consumed = password_resets::consume(state.pool(), &body.token, &hash).await.map_err(reset_error)?;
    // Whoever held the old password or any session is signed out everywhere.
    sessions::revoke_all_for_principal(state.pool(), consumed.principal_id, RevokeReason::Rotated).await.map_err(|_| ApiError::unavailable())?;
    // A person who lost the password may also be locked out by their own guesses: the new password clears it
    // (set_password does), and so does the in-memory backoff for their login.
    if let Some(record) = {
        let mut transaction =
            figcraft_cloud_storage::begin_tenant_transaction(state.pool(), consumed.tenant_id).await.map_err(storage_unavailable)?;
        let record = credentials::find_for_principal(&mut transaction, consumed.principal_id).await.map_err(storage_unavailable)?;
        transaction.rollback().await.map_err(storage_unavailable)?;
        record
    } {
        state.login_throttle().succeeded(consumed.tenant_id, &record.login);
    }
    respond(StatusCode::OK, &[], None, Some(json!({ "tenant_id": consumed.tenant_id.to_string() })))
}
