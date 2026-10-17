//! Administering password accounts (S6-037).
//!
//! | Route (under `/api/v1`) | Permission | What it does |
//! |---|---|---|
//! | `GET tenants/{t}/users?q=&limit=&cursor=` | `member.view` | the workspace's password accounts: `{principal_id, login, display_name, role, member_state, state, locked}` |
//! | `POST tenants/{t}/users` | `member.manage` (`member.manage_owners` for an owner) | `{login, display_name, role?, setup_lifetime_secs?}` creates the account and membership and returns a one-time **setup link token** (`201`; shown once; the person chooses their password with `POST auth/password/reset`) |
//! | `POST tenants/{t}/users/{p}/disable` | `member.manage` (+owners) | credential disabled, membership suspended, live sessions ended at once (the per-request membership check refuses them immediately; sessions are revoked and the relay sweeper closes rooms) |
//! | `POST tenants/{t}/users/{p}/enable` | `member.manage` (+owners) | the reverse; failure counters cleared |
//! | `POST tenants/{t}/users/{p}/password` | `member.manage`, caller strictly above the target | `{password}` or `{generate: true}`: sets a temporary password (shown once when generated), the person must change it at next sign-in, their sessions end; never on an owner or on yourself |
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
        .route("/tenants/{tenant}/users/{principal}/password", post(set_user_password))
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

/// `(principal, login, display name, role, member state, credential state, locked, must change)`.
type UserRow = (String, String, String, String, String, String, bool, bool);

#[derive(serde::Serialize)]
struct UserBody {
    principal_id: PrincipalId,
    login: String,
    display_name: String,
    role: String,
    member_state: String,
    state: &'static str,
    locked: bool,
    must_change_password: bool,
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
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT c.principal_id::text, c.login, p.display_name, m.role, m.state, c.state, \
                (c.locked_until IS NOT NULL AND c.locked_until > now()), c.must_change_password \
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
    for (principal, login, display_name, role, member_state, state_text, locked, must_change_password) in rows {
        users.push(UserBody {
            principal_id: principal.parse().map_err(|_| ApiError::new(ErrorCode::Internal))?,
            login,
            display_name,
            role,
            member_state,
            state: if state_text == "disabled" { "disabled" } else { "active" },
            locked,
            must_change_password,
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
    /// A password the administrator types (temporary: the person must change it at first sign-in).
    initial_password: Option<String>,
    /// Or let the server generate one, returned once in the response.
    generate_password: Option<bool>,
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
    let generate = body.generate_password.unwrap_or(false);
    if body.initial_password.is_some() && generate {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    // Typed or generated initial password (temporary), or a password nobody knows plus a setup link.
    let (hash, shown_password, temporary) = if body.initial_password.is_some() || generate {
        let password = match body.initial_password.clone() {
            Some(typed) => typed,
            None => generate_password().ok_or(ApiError::unavailable())?,
        };
        let hash = hash_password(service, password.clone(), Some(body.login.clone())).await?;
        (hash, if generate { Some(password) } else { None }, true)
    } else {
        let placeholder = figcraft_cloud_storage::password_resets::random_secret().ok_or(ApiError::unavailable())?;
        (service.hash_unchecked(placeholder).await.map_err(|_| ApiError::unavailable())?, None, false)
    };
    let principal =
        accounts::create_account(&mut call.transaction, &body.display_name, &body.login, &hash, Some(call.principal)).await.map_err(|error| {
            match error {
                AccountError::InvalidName | AccountError::InvalidLogin | AccountError::InvalidHash => ApiError::new(ErrorCode::InvalidRequest),
                AccountError::LoginTaken => ApiError::new(ErrorCode::Conflict).with_note("login_taken", "That login name is already taken."),
                AccountError::Database => ApiError::unavailable(),
                _ => ApiError::new(ErrorCode::Internal),
            }
        })?;
    memberships::add_tenant_member(&mut call.transaction, principal, role).await.map_err(membership_error)?;
    let issued = if temporary {
        credentials::set_must_change(&mut call.transaction, principal, true).await.map_err(storage_unavailable)?;
        None
    } else {
        Some(
            password_resets::issue(&mut call.transaction, principal, ResetPurpose::Setup, body.setup_lifetime_secs, call.principal)
                .await
                .map_err(reset_error)?,
        )
    };
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
    let mut reply = json!({
        "principal_id": principal.to_string(),
        "login": login,
        "role": role_str(role),
        "must_change_password": temporary,
    });
    if let Some(issued) = issued {
        reply["setup_token"] = json!(issued.token);
        reply["expires_in_secs"] = json!(issued.expires_in_secs);
    }
    if let Some(password) = shown_password {
        reply["generated_password"] = json!(password);
    }
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

// ---------------------------------------------------------------------------------------------

/// Characters for generated passwords: lowercase letters and digits without look-alikes.
const GENERATED_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// A random password of four groups of five characters, `xxxxx-xxxxx-xxxxx-xxxxx` (about 99 bits),
/// that satisfies the password policy.
pub(crate) fn generate_password() -> Option<String> {
    use ring::rand::SecureRandom as _;
    let rng = ring::rand::SystemRandom::new();
    for _ in 0..8 {
        let mut password = String::with_capacity(23);
        let mut count = 0;
        while count < 20 {
            let mut byte = [0u8; 1];
            rng.fill(&mut byte).ok()?;
            // Rejection sampling keeps every character equally likely.
            let limit = 248u8; // the largest multiple of 31 below 256
            if byte[0] >= limit {
                continue;
            }
            if count > 0 && count % 5 == 0 {
                password.push('-');
            }
            password.push(char::from(*GENERATED_ALPHABET.get(usize::from(byte[0]) % GENERATED_ALPHABET.len())?));
            count += 1;
        }
        if figcraft_cloud_auth::password::check_policy(&password, None).is_ok() {
            return Some(password);
        }
    }
    None
}

/// How far up the workspace a role sits for the "act only on people below you" rule.
const fn rank(role: Role) -> u8 {
    match role {
        Role::TenantOwner => 5,
        Role::TenantAdmin => 4,
        Role::TeamManager | Role::WorkspaceAdmin => 3,
        Role::Editor => 2,
        Role::Commenter => 1,
        Role::Viewer => 0,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPasswordBody {
    /// A temporary password the administrator types ...
    password: Option<String>,
    /// ... or `true` to have the server generate one (returned once).
    generate: Option<bool>,
}

/// `POST tenants/{t}/users/{p}/password`: set a temporary password for a person (TS-013). Only on
/// people ranked strictly below the caller (so never an owner, never the caller), the same password
/// policy applies, the person must choose their own at the next sign-in, their sessions end, and the
/// action is audited without the value.
async fn set_user_password(
    State(state): State<AppState>,
    caller: Authenticated,
    context: RequestContext,
    Path((tenant, principal)): Path<(String, String)>,
    ApiJson(body): ApiJson<SetPasswordBody>,
) -> Result<Response, ApiError> {
    let service = state.passwords().ok_or(ApiError::unavailable())?;
    let generate = body.generate.unwrap_or(false);
    if body.password.is_some() == generate {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::MemberView)?;
    let (id, target_role, record) = target(&mut call, &principal).await?;
    let actor_role = call.subject.tenant_role.ok_or(ApiError::forbidden())?;
    if id == call.principal || rank(actor_role) <= rank(target_role) {
        return Err(ApiError::forbidden());
    }
    if record.state != CredentialState::Active {
        return Err(ApiError::new(ErrorCode::Conflict));
    }
    let password = match body.password {
        Some(typed) => typed,
        None => generate_password().ok_or(ApiError::unavailable())?,
    };
    let hash = hash_password(service, password.clone(), Some(record.login.clone())).await?;
    credentials::set_password(&mut call.transaction, id, &hash, credentials::PasswordChange::AdminSet, Some(call.principal))
        .await
        .map_err(storage_unavailable)?;
    password_resets::revoke_links(&mut call.transaction, id).await.map_err(reset_error)?;
    audit(&mut call.transaction, call.principal, "account.password_admin_set", "principal", Some(id.as_uuid()), &context, &json!({})).await?;
    commit(call.transaction).await?;
    // The person's other sessions end now, and their in-memory backoff is forgotten.
    sessions::revoke_all_for_principal(state.pool(), id, RevokeReason::Operator).await.map_err(|_| ApiError::unavailable())?;
    state.login_throttle().succeeded(caller.tenant.tenant_id(), &record.login);
    let mut reply = json!({ "must_change_password": true });
    if generate {
        reply["generated_password"] = json!(password);
    }
    respond(StatusCode::OK, &[], None, Some(reply))
}
