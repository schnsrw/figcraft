//! Built-in accounts beside OIDC (S6-034 and the routes that follow): sign-up, first-owner
//! bootstrap with a password, the sign-in methods a tenant offers, and the tenant's account
//! settings.
//!
//! | Route (under `/api/v1`) | What it does |
//! |---|---|
//! | `GET auth/methods?tenant=` | the sign-in methods the workspace offers: `{tenant_id, password, oidc, open_registration}`. Unauthenticated (the sign-in screen needs it); `tenant` is an id or a slug, ignored in single-tenant mode; `404` for an unknown or inactive tenant |
//! | `POST auth/password/signup` | `{invitation, display_name, password}` (the holder of an invitation creates their account; the login is the invited address) or `{tenant, login, display_name, password}` (only when the tenant turned open registration on). Creates the account and its membership atomically and signs the person in (session cookie, `201`) |
//! | `POST auth/password/login` | `{tenant?, login, password}`: password sign-in (S6-035). Every failure (unknown login, wrong password, disabled or locked account, suspended member) is the same fixed `401`; a login that keeps failing is answered `429` with `Retry-After` whatever it is. A tenant that turned passwords off answers `403`. Success rotates the browser's session (the old cookie is revoked) and sets a new one |
//! | `POST auth/password/bootstrap` | `{token, login, display_name, password}` while a single-tenant deployment awaits its owner: the person who holds the printed bootstrap token becomes `tenant_owner` with a password account, so a self-hoster needs no external identity provider (session cookie, `200`) |
//! | `GET tenants/{tenant}/account-settings` | `tenant.view`: `{password_enabled, oidc_enabled, open_registration, require_totp}` |
//! | `PUT tenants/{tenant}/account-settings` | `tenant.update_policy`: replaces them (audited) |
//!
//! Every `POST` passes the same-origin check of the OIDC routes and the auth router's per-address
//! token bucket. Misuse of an invitation token (unknown, expired, revoked, already used) is one
//! fixed `404`. A password that breaks the policy answers `400` with `error.reason =
//! "password_policy"` and a sentence the person can act on in `error.message`; a login already in
//! use answers `409 conflict`. A password is never logged, echoed, stored or audited; only its
//! Argon2id hash is stored.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use figcraft_cloud_auth::authz::Action;
use figcraft_cloud_auth::password::Verification;
use figcraft_cloud_auth::session_policy::COOKIE_NAME;
use figcraft_cloud_domain::{ErrorCode, PrincipalId, TenantId};
use figcraft_cloud_storage::account_settings::{self, AccountSettings, SettingsError};
use figcraft_cloud_storage::accounts::{self, AccountError};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::bootstrap::{self, BootstrapError};
use figcraft_cloud_storage::credentials::{self, CredentialState};
use figcraft_cloud_storage::memberships::{self, MemberState};
use figcraft_cloud_storage::sessions;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::auth::Authenticated;
use super::auth_routes::{identity, require_same_origin, respond, revoke_best_effort, session_cookie, session_error};
use super::authenticator::cookie;
use super::error::{ApiError, ApiJson};
use super::passwords::{PasswordService, ServiceError};
use super::tenants_api::{self, require_tenant, storage_unavailable};

/// Longest tenant reference (a UUID or a slug) accepted.
const MAX_TENANT_REF_CHARS: usize = 64;
/// Longest bootstrap or invitation token accepted in a body.
const MAX_TOKEN_CHARS: usize = 128;
/// Longest login accepted before normalisation.
const MAX_LOGIN_INPUT_CHARS: usize = 512;
/// Longest display name accepted before normalisation.
const MAX_NAME_INPUT_CHARS: usize = 512;

/// Routes under `/api/v1/auth` (rate limited with the rest of the auth router).
pub(crate) fn auth_routes() -> Router<AppState> {
    Router::new()
        .route("/methods", get(methods))
        .route("/password/signup", post(signup))
        .route("/password/bootstrap", post(bootstrap_password))
        .route("/password/login", post(password_login))
}

/// Routes under `/api/v1`.
pub(crate) fn resource_routes() -> Router<AppState> {
    Router::new().route("/tenants/{tenant}/account-settings", get(read_settings).put(write_settings))
}

fn passwords(state: &AppState) -> Result<&PasswordService, ApiError> {
    state.passwords().ok_or(ApiError::unavailable())
}

/// A `400` that tells the person why their password was refused (`error.reason =
/// "password_policy"`, `error.message` a fixed sentence).
pub(crate) fn policy_error(violation: figcraft_cloud_auth::password::PolicyViolation) -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest).with_note("password_policy", violation.message())
}

/// Hash `password`; a policy failure becomes the explanatory `400`.
pub(crate) async fn hash_password(service: &PasswordService, password: String, login: Option<String>) -> Result<String, ApiError> {
    match service.hash(password, login).await {
        Ok(hash) => Ok(hash),
        Err(ServiceError::Policy(violation)) => Err(policy_error(violation)),
        Err(ServiceError::Unavailable) => Err(ApiError::unavailable()),
    }
}

/// The tenant a pre-authentication request means: the single tenant in a single-tenant
/// deployment, otherwise `reference` as a tenant id or an active tenant's slug.
pub(crate) async fn resolve_tenant(state: &AppState, reference: Option<&str>) -> Result<TenantId, ApiError> {
    let identity = identity(state)?;
    if let Some(single) = identity.bootstrap_tenant() {
        return Ok(single);
    }
    let reference = reference.map(str::trim).filter(|value| !value.is_empty() && value.len() <= MAX_TENANT_REF_CHARS).ok_or(ApiError::not_found())?;
    if let Ok(id) = reference.parse::<TenantId>() {
        return Ok(id);
    }
    account_settings::tenant_by_slug(state.pool(), reference).await.map_err(storage_unavailable)?.ok_or(ApiError::not_found())
}

/// Sign `principal` in: whatever session the browser held is revoked (rotation on login), a fresh
/// one is created.
pub(crate) async fn start_session(
    state: &AppState,
    headers: &HeaderMap,
    principal: PrincipalId,
    status: StatusCode,
    body: Value,
) -> Result<Response, ApiError> {
    let identity = identity(state)?;
    if let Some(previous) = cookie(headers, COOKIE_NAME) {
        revoke_best_effort(state, previous).await?;
    }
    let created = sessions::create_session(state.pool(), principal, identity.limits()).await.map_err(session_error)?;
    respond(status, &[session_cookie(identity, &created)?], None, Some(body))
}

fn account_error(error: AccountError) -> ApiError {
    match error {
        AccountError::InvalidName | AccountError::InvalidLogin | AccountError::InvalidHash => ApiError::new(ErrorCode::InvalidRequest),
        // Token holders cannot tell an unknown token from any other refusal.
        AccountError::Refused | AccountError::TenantInactive => ApiError::not_found(),
        AccountError::Closed => ApiError::forbidden(),
        AccountError::LoginTaken => ApiError::new(ErrorCode::Conflict),
        AccountError::Database => ApiError::unavailable(),
        AccountError::Audit | AccountError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct MethodsQuery {
    tenant: Option<String>,
}

async fn methods(State(state): State<AppState>, Query(query): Query<MethodsQuery>) -> Result<Response, ApiError> {
    identity(&state)?;
    let tenant = resolve_tenant(&state, query.tenant.as_deref()).await?;
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    let active: Option<String> = sqlx::query_scalar("SELECT state FROM tenants WHERE id = $1::uuid")
        .bind(tenant.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(storage_unavailable)?;
    if active.as_deref() != Some("active") {
        return Err(ApiError::not_found());
    }
    let settings = account_settings::get_settings(&mut transaction).await.map_err(storage_unavailable)?;
    transaction.rollback().await.map_err(storage_unavailable)?;
    // A method is offered only when the tenant allows it and this deployment can serve it.
    let password = settings.password_enabled && state.passwords().is_some();
    let body = json!({
        "tenant_id": tenant.to_string(),
        "password": password,
        "oidc": settings.oidc_enabled && state.oidc().is_some(),
        "open_registration": password && settings.open_registration,
    });
    respond(StatusCode::OK, &[], None, Some(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignupBody {
    invitation: Option<String>,
    tenant: Option<String>,
    login: Option<String>,
    display_name: String,
    password: String,
}

async fn signup(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<SignupBody>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    require_same_origin(identity, &headers)?;
    let service = passwords(&state)?;
    let invalid = || ApiError::new(ErrorCode::InvalidRequest);
    if body.display_name.len() > MAX_NAME_INPUT_CHARS || body.login.as_ref().is_some_and(|login| login.len() > MAX_LOGIN_INPUT_CHARS) {
        return Err(invalid());
    }
    match (body.invitation, body.tenant) {
        (Some(token), None) => {
            // An invitation fixes the login (the invited address): a login in the body is a mistake.
            if body.login.is_some() || token.is_empty() || token.len() > MAX_TOKEN_CHARS {
                return Err(invalid());
            }
            let hash = hash_password(service, body.password, None).await?;
            let account = accounts::register_with_invitation(state.pool(), &token, &body.display_name, &hash).await.map_err(account_error)?;
            let reply = json!({ "principal_id": account.principal_id.to_string(), "tenant_id": account.tenant_id.to_string() });
            start_session(&state, &headers, account.principal_id, StatusCode::CREATED, reply).await
        }
        (None, Some(reference)) => {
            let login = body.login.ok_or_else(invalid)?;
            let tenant = resolve_tenant(&state, Some(&reference)).await?;
            // Closed registration is refused before any hashing is paid for.
            let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
            let settings = account_settings::get_settings(&mut transaction).await.map_err(storage_unavailable)?;
            transaction.rollback().await.map_err(storage_unavailable)?;
            if !settings.open_registration || !settings.password_enabled {
                return Err(ApiError::forbidden());
            }
            let hash = hash_password(service, body.password, Some(login.clone())).await?;
            let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
            let principal = accounts::register_open(&mut transaction, &body.display_name, &login, &hash).await.map_err(account_error)?;
            transaction.commit().await.map_err(storage_unavailable)?;
            let reply = json!({ "principal_id": principal.to_string(), "tenant_id": tenant.to_string() });
            start_session(&state, &headers, principal, StatusCode::CREATED, reply).await
        }
        _ => Err(invalid()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapBody {
    token: String,
    login: String,
    display_name: String,
    password: String,
}

async fn bootstrap_password(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<BootstrapBody>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    let tenant = identity.bootstrap_tenant().ok_or(ApiError::not_found())?;
    require_same_origin(identity, &headers)?;
    let service = passwords(&state)?;
    if body.token.is_empty()
        || body.token.len() > MAX_TOKEN_CHARS
        || body.login.len() > MAX_LOGIN_INPUT_CHARS
        || body.display_name.len() > MAX_NAME_INPUT_CHARS
    {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let hash = hash_password(service, body.password, Some(body.login.clone())).await?;
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    // Password accounts must be allowed for this workspace.
    if !account_settings::get_settings(&mut transaction).await.map_err(storage_unavailable)?.password_enabled {
        return Err(ApiError::forbidden());
    }
    let principal = accounts::create_account(&mut transaction, &body.display_name, &body.login, &hash, None).await.map_err(account_error)?;
    if let Err(error) = bootstrap::complete_bootstrap(&mut transaction, &body.token, principal).await {
        let _ = transaction.rollback().await;
        return Err(match error {
            BootstrapError::Database | BootstrapError::Random => ApiError::unavailable(),
            _ => ApiError::forbidden(),
        });
    }
    transaction.commit().await.map_err(storage_unavailable)?;
    identity.mark_bootstrap_done();
    let reply = json!({ "tenant_id": tenant.to_string(), "principal_id": principal.to_string(), "role": "tenant_owner" });
    start_session(&state, &headers, principal, StatusCode::OK, reply).await
}

// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    tenant: Option<String>,
    login: String,
    password: String,
}

/// What the sign-in lookup found, read before any hashing so no connection is held during it.
struct Lookup {
    record: Option<credentials::CredentialRecord>,
    member_active: bool,
    password_enabled: bool,
    tenant_active: bool,
}

async fn lookup(state: &AppState, tenant: TenantId, login: &str) -> Result<Lookup, ApiError> {
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    let tenant_state: Option<String> = sqlx::query_scalar("SELECT state FROM tenants WHERE id = $1::uuid")
        .bind(tenant.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(storage_unavailable)?;
    let settings = account_settings::get_settings(&mut transaction).await.map_err(storage_unavailable)?;
    let record = credentials::find_by_login(&mut transaction, login).await.map_err(storage_unavailable)?;
    let member_active = match &record {
        Some(record) => memberships::get_tenant_member(&mut transaction, record.principal_id)
            .await
            .map_err(storage_unavailable)?
            .is_some_and(|member| member.state == MemberState::Active),
        None => false,
    };
    transaction.rollback().await.map_err(storage_unavailable)?;
    Ok(Lookup { record, member_active, password_enabled: settings.password_enabled, tenant_active: tenant_state.as_deref() == Some("active") })
}

async fn password_login(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<LoginBody>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    require_same_origin(identity, &headers)?;
    let service = passwords(&state)?;
    if body.login.len() > MAX_LOGIN_INPUT_CHARS || body.password.len() > figcraft_cloud_auth::password::MAX_PASSWORD_INPUT_BYTES {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let tenant = resolve_tenant(&state, body.tenant.as_deref()).await?;
    let normalised = credentials::normalize_login(&body.login);
    // Throttle on the normalised login (or the trimmed text when it is not a valid login).
    let throttle_login = normalised.clone().unwrap_or_else(|| body.login.trim().to_lowercase());
    let now = std::time::Instant::now();
    if let Some(wait) = state.login_throttle().blocked(tenant, &throttle_login, now) {
        return Err(ApiError::rate_limited(wait.as_secs() + 1));
    }
    let found = lookup(&state, tenant, &body.login).await?;
    if found.tenant_active && !found.password_enabled {
        return Err(ApiError::forbidden());
    }
    // Exactly one Argon2 computation on every path below, so timing says nothing about the account.
    let eligible = found.tenant_active
        && found.member_active
        && found.record.as_ref().is_some_and(|record| record.state == CredentialState::Active && record.locked_for_secs == 0);
    let verdict = match &found.record {
        Some(record) => {
            let verdict = service.verify(record.password_hash.clone(), body.password.clone()).await.map_err(|_| ApiError::unavailable())?;
            if eligible { verdict } else { Verification::Invalid }
        }
        None => service.verify_unknown(body.password.clone()).await.map_err(|_| ApiError::unavailable())?,
    };
    let (Verification::Valid { needs_rehash }, Some(record)) = (verdict, found.record.as_ref()) else {
        // Count the failure against an existing, unlocked account (the lock bounds the rows written).
        if let Some(record) = found.record.as_ref().filter(|_| eligible) {
            let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
            credentials::record_failure(&mut transaction, record.principal_id, state.lockout()).await.map_err(storage_unavailable)?;
            credentials::record_login_audit(&mut transaction, record.principal_id, false, "bad_credentials").await.map_err(storage_unavailable)?;
            transaction.commit().await.map_err(storage_unavailable)?;
        }
        state.login_throttle().failed(tenant, &throttle_login, now);
        return Err(ApiError::unauthenticated());
    };
    // A better hash of the same password, while we hold it (best effort; the sign-in stands either way).
    let upgraded = if needs_rehash { service.hash_unchecked(body.password.clone()).await.ok() } else { None };
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    credentials::record_success(&mut transaction, record.principal_id).await.map_err(storage_unavailable)?;
    if let Some(hash) = upgraded {
        credentials::upgrade_hash(&mut transaction, record.principal_id, &record.password_hash, &hash).await.map_err(storage_unavailable)?;
    }
    credentials::record_login_audit(&mut transaction, record.principal_id, true, "password").await.map_err(storage_unavailable)?;
    transaction.commit().await.map_err(storage_unavailable)?;
    state.login_throttle().succeeded(tenant, &throttle_login);
    let reply = json!({ "principal_id": record.principal_id.to_string(), "tenant_id": tenant.to_string() });
    start_session(&state, &headers, record.principal_id, StatusCode::OK, reply).await.map_err(|error| {
        // A principal that cannot hold a session is indistinguishable from a bad password.
        if error.code() == ErrorCode::ResourceInactive { ApiError::unauthenticated() } else { error }
    })
}

// ---------------------------------------------------------------------------------------------

fn settings_body(settings: AccountSettings) -> Value {
    json!({
        "password_enabled": settings.password_enabled,
        "oidc_enabled": settings.oidc_enabled,
        "open_registration": settings.open_registration,
        "require_totp": settings.require_totp,
    })
}

async fn read_settings(State(state): State<AppState>, caller: Authenticated, Path(tenant): Path<String>) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::TenantView)?;
    let settings = account_settings::get_settings(&mut call.transaction).await.map_err(storage_unavailable)?;
    Ok(axum::Json(settings_body(settings)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    password_enabled: bool,
    oidc_enabled: bool,
    open_registration: bool,
    require_totp: bool,
}

async fn write_settings(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(tenant): Path<String>,
    ApiJson(body): ApiJson<SettingsBody>,
) -> Result<Response, ApiError> {
    let mut call = tenants_api::open(&state, &caller, &tenant).await?;
    require_tenant(&call.subject, Action::TenantUpdatePolicy)?;
    let settings = AccountSettings {
        password_enabled: body.password_enabled,
        oidc_enabled: body.oidc_enabled,
        open_registration: body.open_registration,
        require_totp: body.require_totp,
    };
    account_settings::set_settings(&mut call.transaction, settings, call.principal).await.map_err(|error| match error {
        SettingsError::InvalidCombination => ApiError::new(ErrorCode::InvalidRequest),
        SettingsError::Audit => ApiError::new(ErrorCode::Internal),
        SettingsError::Database => ApiError::unavailable(),
    })?;
    tenants_api::commit(call.transaction).await?;
    Ok(axum::Json(settings_body(settings)).into_response())
}

#[cfg(test)]
#[path = "account_routes_tests.rs"]
mod tests;
