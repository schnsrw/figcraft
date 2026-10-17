//! Account self-service (S6-036): the signed-in person's profile, password and browser sessions.
//!
//! | Route (under `/api/v1`) | What it does |
//! |---|---|
//! | `GET me` | `{principal_id, display_name, color, color_chosen, palette, tenant_id?, login?}`; `login` only when the person has a password account in the selected tenant |
//! | `PATCH me` | `{display_name?, color?}` (1 to 64 characters; a colour from `palette`) |
//! | `POST me/password` | `{current_password, new_password, tenant?}`: re-authenticates with the current password (wrong guesses count toward the same lockout and backoff as sign-in), stores the new hash, revokes **every** session of the person, signs this browser in again with a fresh one. `403` for a wrong current password |
//! | `GET me/sessions` | the person's live browser sessions `{items:[{id, created_at, last_seen_at, idle_expires_at, absolute_expires_at, current}]}` (never a token) |
//! | `DELETE me/sessions/{id}` | revoke one of the person's own sessions (`204`; the current one signs this browser out) |
//! | `POST me/sessions/revoke-others` | revoke every session but this one: `{revoked}` |
//!
//! These routes need a browser session only (no tenant role), so a person who belongs to no
//! workspace yet can still manage their account; unsafe methods pass the usual same-origin check
//! inside the authenticator.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use figcraft_cloud_auth::password::Verification;
use figcraft_cloud_auth::session_policy::{COOKIE_NAME, clear_cookie};
use figcraft_cloud_domain::{ErrorCode, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::credentials::{self, CredentialState, PasswordChange};
use figcraft_cloud_storage::profiles::{self, PALETTE, ProfileError};
use figcraft_cloud_storage::sessions::{self, RevokeReason};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::account_routes::hash_password;
use super::auth_routes::{authenticator, identity, respond, session_cookie, session_error};
use super::authenticator::{Caller, cookie, selected_tenant};
use super::error::{ApiError, ApiJson};
use super::tenants_api::storage_unavailable;

/// Longest password accepted in a body.
const MAX_PASSWORD_BYTES: usize = figcraft_cloud_auth::password::MAX_PASSWORD_INPUT_BYTES;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(read_profile).patch(update_profile))
        .route("/me/password", post(change_password))
        .route("/me/sessions", get(list_sessions))
        .route("/me/sessions/revoke-others", post(revoke_others))
        .route("/me/sessions/{id}", delete(revoke_session))
}

/// A browser session only: service accounts, share links and embedded sessions (which belong to a
/// host system) have no profile or sessions to manage.
struct Me {
    principal: PrincipalId,
    session_id: Uuid,
}

async fn me(state: &AppState, method: &Method, headers: &HeaderMap) -> Result<Me, ApiError> {
    match authenticator(state)?.identify(method, headers).await.map_err(ApiError::from)? {
        Caller::Browser(session) if session.embed.is_some() => Err(ApiError::forbidden()),
        Caller::Browser(session) => Ok(Me { principal: session.principal_id, session_id: session.id }),
        Caller::ServiceAccount(_) | Caller::Share(_) => Err(ApiError::forbidden()),
    }
}

fn profile_error(error: ProfileError) -> ApiError {
    match error {
        ProfileError::InvalidName | ProfileError::InvalidColor => ApiError::new(ErrorCode::InvalidRequest),
        ProfileError::NotFound => ApiError::not_found(),
        ProfileError::Database => ApiError::unavailable(),
        ProfileError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn profile_body(profile: &profiles::Profile) -> Value {
    json!({
        "principal_id": profile.principal_id.to_string(),
        "display_name": profile.display_name,
        "color": profile.color,
        "color_chosen": profile.color_chosen,
        "palette": PALETTE,
    })
}

/// The tenant a self-service call concerns: the request's selected tenant, if any.
fn selected(state: &AppState, uri_path: &str, headers: &HeaderMap) -> Result<Option<TenantId>, ApiError> {
    selected_tenant(identity(state)?, uri_path, headers).map_err(ApiError::from)
}

async fn read_profile(State(state): State<AppState>, method: Method, uri: axum::http::Uri, headers: HeaderMap) -> Result<Response, ApiError> {
    let me = me(&state, &method, &headers).await?;
    let profile = profiles::get_profile(state.pool(), me.principal).await.map_err(profile_error)?;
    let mut body = profile_body(&profile);
    if let Some(tenant) = selected(&state, uri.path(), &headers)? {
        let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
        let record = credentials::find_for_principal(&mut transaction, me.principal).await.map_err(storage_unavailable)?;
        transaction.rollback().await.map_err(storage_unavailable)?;
        body["tenant_id"] = json!(tenant.to_string());
        body["login"] = record.map_or(Value::Null, |record| json!(record.login));
    }
    respond(StatusCode::OK, &[], None, Some(body))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileBody {
    display_name: Option<String>,
    color: Option<String>,
}

async fn update_profile(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<ProfileBody>) -> Result<Response, ApiError> {
    let me = me(&state, &Method::PATCH, &headers).await?;
    if body.display_name.is_none() && body.color.is_none() {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let profile =
        profiles::update_profile(state.pool(), me.principal, body.display_name.as_deref(), body.color.as_deref()).await.map_err(profile_error)?;
    respond(StatusCode::OK, &[], None, Some(profile_body(&profile)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    current_password: String,
    new_password: String,
    tenant: Option<String>,
}

async fn change_password(
    State(state): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PasswordBody>,
) -> Result<Response, ApiError> {
    let me = me(&state, &Method::POST, &headers).await?;
    let identity = identity(&state)?;
    let service = state.passwords().ok_or(ApiError::unavailable())?;
    if body.current_password.len() > MAX_PASSWORD_BYTES || body.new_password.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let tenant = match selected(&state, uri.path(), &headers)? {
        Some(tenant) => tenant,
        None => super::account_routes::resolve_tenant(&state, body.tenant.as_deref()).await?,
    };
    let now = std::time::Instant::now();
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    let record = credentials::find_for_principal(&mut transaction, me.principal).await.map_err(storage_unavailable)?;
    transaction.rollback().await.map_err(storage_unavailable)?;
    // An OIDC-only person has no password to change.
    let record = record.ok_or(ApiError::not_found())?;
    if let Some(wait) = state.login_throttle().blocked(tenant, &record.login, now) {
        return Err(ApiError::rate_limited(wait.as_secs() + 1));
    }
    let verdict = service.verify(record.password_hash.clone(), body.current_password.clone()).await.map_err(|_| ApiError::unavailable())?;
    let usable = record.state == CredentialState::Active && record.locked_for_secs == 0;
    if !(usable && matches!(verdict, Verification::Valid { .. })) {
        if usable {
            let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
            credentials::record_failure(&mut transaction, me.principal, state.lockout()).await.map_err(storage_unavailable)?;
            credentials::record_login_audit(&mut transaction, me.principal, false, "wrong_current_password").await.map_err(storage_unavailable)?;
            transaction.commit().await.map_err(storage_unavailable)?;
        }
        state.login_throttle().failed(tenant, &record.login, now);
        return Err(ApiError::forbidden());
    }
    let new_hash = hash_password(service, body.new_password, Some(record.login.clone())).await?;
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(storage_unavailable)?;
    credentials::set_password(&mut transaction, me.principal, &new_hash, PasswordChange::Change, Some(me.principal))
        .await
        .map_err(storage_unavailable)?;
    transaction.commit().await.map_err(storage_unavailable)?;
    state.login_throttle().succeeded(tenant, &record.login);
    // Every session of the person ends (this browser's included), then this browser gets a fresh one.
    let revoked = sessions::revoke_all_for_principal(state.pool(), me.principal, RevokeReason::Rotated).await.map_err(session_error)?;
    let created = sessions::create_session(state.pool(), me.principal, identity.limits()).await.map_err(session_error)?;
    respond(StatusCode::OK, &[session_cookie(identity, &created)?], None, Some(json!({ "revoked_sessions": revoked })))
}

async fn list_sessions(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let me = me(&state, &Method::GET, &headers).await?;
    let sessions = profiles::list_sessions(state.pool(), me.principal).await.map_err(profile_error)?;
    let items: Vec<Value> = sessions
        .iter()
        .map(|session| {
            json!({
                "id": session.id.to_string(),
                "created_at": session.created_at,
                "last_seen_at": session.last_seen_at,
                "idle_expires_at": session.idle_expires_at,
                "absolute_expires_at": session.absolute_expires_at,
                "current": session.id == me.session_id,
            })
        })
        .collect();
    respond(StatusCode::OK, &[], None, Some(json!({ "items": items })))
}

async fn revoke_session(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Result<Response, ApiError> {
    let me = me(&state, &Method::DELETE, &headers).await?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    if !profiles::revoke_session(state.pool(), me.principal, id).await.map_err(profile_error)? {
        return Err(ApiError::not_found());
    }
    let cookies = if id == me.session_id && cookie(&headers, COOKIE_NAME).is_some() { vec![clear_cookie()] } else { Vec::new() };
    respond(StatusCode::NO_CONTENT, &cookies, None, None)
}

async fn revoke_others(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let me = me(&state, &Method::POST, &headers).await?;
    let revoked = profiles::revoke_other_sessions(state.pool(), me.principal, me.session_id).await.map_err(profile_error)?;
    respond(StatusCode::OK, &[], None, Some(json!({ "revoked": revoked })))
}
