//! `/api/v1/auth` (S4-001, S4-002, S4-003, S4-005 HTTP wiring), rate limited with the rest of
//! the auth router, plus the bootstrap gate every `/api/v1` route passes through.
//!
//! | Route | What it does |
//! |---|---|
//! | `GET login?return_to=/path` | OIDC `begin_login`; sets `__Host-figcraft_oidc` (state, `SameSite=Lax`, 10 min) and redirects (303) to the provider |
//! | `GET callback?code&state` | `complete_login` against the state cookie; revokes any session the browser already had, creates a fresh one (rotation on login), sets `__Host-figcraft_session`, clears the state cookie, redirects (303) to the validated `return_to` or `/` |
//! | `POST logout` | revokes the session, clears the session and share cookies (`204`, idempotent) |
//! | `POST bootstrap/complete {"token"}` | single-tenant only: the signed-in user completes bootstrap and becomes `tenant_owner`; the session is rotated (privilege change) |
//! | `POST share/exchange {"token"}` | exchanges a share token for a share-session cookie bound to its document, living at most 1 h and never past the link's expiry |
//! | `POST backchannel-logout` (form `logout_token`) | OIDC Back-Channel Logout 1.0 from the provider (server to server, no cookies, no same-origin check): validates the logout token and revokes every session of the principal linked to `(issuer, sub)`. `200` whether or not any session existed, `400` for any invalid, replayed or `sub`-less token, `503` when no provider is configured or the store is down (the provider retries) |
//! | `GET session` | who am I: credential kind, principal, tenant role and grants, scopes or shared document; never a secret. Rotates a browser session past the rotation interval |
//!
//! Every `POST` here except `backchannel-logout` must pass the same-origin check (`Origin` or `Sec-Fetch-Site`), session or
//! not, so a cross-site page cannot log a victim out, into a share session, or into bootstrap.
//!
//! While a single-tenant deployment still needs bootstrap, [`bootstrap_gate`] answers every other
//! `/api/v1` route with a fixed `403 bootstrap_required`; only login, callback, logout,
//! back-channel logout, session and bootstrap completion are served.

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use figcraft_cloud_auth::oidc::{LOGIN_ATTEMPT_LIFETIME_SECS, MAX_LOGOUT_FORM_BYTES, OidcError, logout_token_from_form, return_to_allowed};
use figcraft_cloud_auth::session_policy::{COOKIE_NAME, RotationTrigger, clear_cookie};
use figcraft_cloud_domain::{DeploymentMode, ErrorCode, PrincipalId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::bootstrap::{self, BootstrapError};
use figcraft_cloud_storage::memberships::role_str;
use figcraft_cloud_storage::sessions::{self, CreatedSession, SessionError};
use figcraft_cloud_storage::share_links::{self, ShareLinkError};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::auth::AuthRejection;
use super::authenticator::{
    Caller, CredentialAuthenticator, IdentityConfig, OIDC_STATE_COOKIE, SHARE_COOKIE, clear_host_cookie, cookie, host_cookie, now_unix,
    selected_tenant,
};
use super::context::RequestContext;
use super::error::{ApiError, ApiJson};

/// Longest bootstrap or share token accepted in a body (both are 43 characters).
const MAX_TOKEN_CHARS: usize = 128;

/// The auth routes, relative to `/api/v1/auth`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login))
        .route("/callback", get(callback))
        .route("/logout", post(logout))
        .route("/backchannel-logout", post(backchannel_logout))
        .route("/bootstrap/complete", post(complete_bootstrap))
        .route("/share/exchange", post(exchange_share))
        .route("/session", get(session))
}

fn identity(state: &AppState) -> Result<&IdentityConfig, ApiError> {
    state.identity().ok_or(ApiError::unavailable())
}

fn authenticator(state: &AppState) -> Result<CredentialAuthenticator, ApiError> {
    state.credential_authenticator().ok_or(ApiError::unavailable())
}

fn header_value(value: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|_| ApiError::new(ErrorCode::Internal))
}

/// A response with `status`, `Set-Cookie` values, `Cache-Control: no-store` and an optional
/// `Location`.
fn respond(status: StatusCode, cookies: &[String], location: Option<&str>, body: Option<Value>) -> Result<Response, ApiError> {
    let mut response = match body {
        Some(body) => (status, axum::Json(body)).into_response(),
        None => (status, Body::empty()).into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for value in cookies {
        headers.append(header::SET_COOKIE, header_value(value)?);
    }
    if let Some(location) = location {
        headers.insert(header::LOCATION, header_value(location)?);
    }
    Ok(response)
}

fn require_same_origin(identity: &IdentityConfig, headers: &HeaderMap) -> Result<(), ApiError> {
    if identity.same_origin(headers) { Ok(()) } else { Err(ApiError::forbidden()) }
}

fn session_cookie(identity: &IdentityConfig, created: &CreatedSession) -> Result<String, ApiError> {
    identity.policy().set_cookie(&created.token, created.absolute_remaining_secs).ok_or(ApiError::new(ErrorCode::Internal))
}

fn session_error(error: SessionError) -> ApiError {
    match error {
        SessionError::Database | SessionError::Random => ApiError::unavailable(),
        SessionError::InactivePrincipal => ApiError::new(ErrorCode::ResourceInactive),
        SessionError::InvalidInput | SessionError::Refused => ApiError::unauthenticated(),
    }
}

/// Revoke the session `token` names. A token that opens nothing is already as good as revoked;
/// only an unreachable store fails the request (so a revocation is never silently skipped).
async fn revoke_best_effort(state: &AppState, token: &str) -> Result<(), ApiError> {
    match sessions::logout(state.pool(), token).await {
        Err(SessionError::Database) => Err(ApiError::unavailable()),
        Ok(_) | Err(_) => Ok(()),
    }
}

#[derive(Deserialize)]
struct LoginQuery {
    return_to: Option<String>,
}

async fn login(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Result<Response, ApiError> {
    identity(&state)?;
    let provider = state.oidc().ok_or(ApiError::unavailable())?;
    if query.return_to.as_deref().is_some_and(|path| !return_to_allowed(path)) {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let begin = provider.begin_login(state.pool(), query.return_to.as_deref()).await.map_err(|error| ApiError::new(error.code()))?;
    let max_age = i64::try_from(LOGIN_ATTEMPT_LIFETIME_SECS).unwrap_or(600);
    respond(StatusCode::SEE_OTHER, &[host_cookie(OIDC_STATE_COOKIE, &begin.state_cookie, max_age, "Lax")], Some(&begin.authorization_url), None)
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(State(state): State<AppState>, headers: HeaderMap, Query(query): Query<CallbackQuery>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    let provider = state.oidc().ok_or(ApiError::unavailable())?;
    let (Some(code), Some(login_state)) = (query.code.as_deref(), query.state.as_deref()) else {
        return Err(ApiError::unauthenticated());
    };
    let outcome = provider
        .complete_login(state.pool(), code, login_state, cookie(&headers, OIDC_STATE_COOKIE))
        .await
        .map_err(|error| ApiError::new(error.code()))?;
    // Rotation on login: whatever session this browser held before is revoked, never upgraded.
    if let Some(previous) = cookie(&headers, COOKIE_NAME) {
        revoke_best_effort(&state, previous).await?;
    }
    let created = sessions::create_session(state.pool(), outcome.principal_id, identity.limits()).await.map_err(session_error)?;
    let target = outcome.return_to.filter(|path| return_to_allowed(path)).unwrap_or_else(|| "/".to_string());
    respond(StatusCode::SEE_OTHER, &[session_cookie(identity, &created)?, clear_host_cookie(OIDC_STATE_COOKIE, "Lax")], Some(&target), None)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    require_same_origin(identity, &headers)?;
    if let Some(token) = cookie(&headers, COOKIE_NAME) {
        revoke_best_effort(&state, token).await?;
    }
    respond(StatusCode::NO_CONTENT, &[clear_cookie(), clear_host_cookie(SHARE_COOKIE, "Strict")], None, None)
}

/// `POST backchannel-logout`: the provider's server-to-server logout notice. The answer never
/// depends on whether a session existed (`200` either way).
async fn backchannel_logout(State(state): State<AppState>, request: Request) -> Result<Response, ApiError> {
    identity(&state)?;
    let provider = state.oidc().ok_or(ApiError::unavailable())?;
    let form = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/x-www-form-urlencoded"));
    if !form {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let body = axum::body::to_bytes(request.into_body(), MAX_LOGOUT_FORM_BYTES).await.map_err(|_| ApiError::new(ErrorCode::InvalidRequest))?;
    let token = logout_token_from_form(&body).ok_or(ApiError::new(ErrorCode::InvalidRequest))?;
    match provider.backchannel_logout(state.pool(), &token).await {
        Ok(_) => respond(StatusCode::OK, &[], None, None),
        Err(OidcError::Storage | OidcError::Discovery) => Err(ApiError::unavailable()),
        Err(_) => Err(ApiError::new(ErrorCode::InvalidRequest)),
    }
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

fn bounded_token(body: &TokenBody) -> Result<&str, ApiError> {
    if body.token.is_empty() || body.token.len() > MAX_TOKEN_CHARS { Err(ApiError::new(ErrorCode::InvalidRequest)) } else { Ok(&body.token) }
}

/// The browser session on this request (bootstrap needs a signed-in user, not a service account
/// or share session).
async fn browser_principal(state: &AppState, headers: &HeaderMap) -> Result<(PrincipalId, String), ApiError> {
    match authenticator(state)?.identify(&Method::POST, headers).await.map_err(ApiError::from)? {
        Caller::Browser(session) => {
            let token = cookie(headers, COOKIE_NAME).ok_or(ApiError::unauthenticated())?;
            Ok((session.principal_id, token.to_string()))
        }
        Caller::ServiceAccount(_) | Caller::Share(_) => Err(ApiError::forbidden()),
    }
}

async fn complete_bootstrap(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<TokenBody>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    let tenant = identity.bootstrap_tenant().ok_or(ApiError::not_found())?;
    require_same_origin(identity, &headers)?;
    let token = bounded_token(&body)?;
    let (principal, session_token) = browser_principal(&state, &headers).await?;
    let mut transaction = begin_tenant_transaction(state.pool(), tenant).await.map_err(|_| ApiError::unavailable())?;
    match bootstrap::complete_bootstrap(&mut transaction, token, principal).await {
        Ok(()) => transaction.commit().await.map_err(|_| ApiError::unavailable())?,
        Err(error) => {
            let _ = transaction.rollback().await;
            return Err(match error {
                BootstrapError::Database | BootstrapError::Random => ApiError::unavailable(),
                BootstrapError::Refused | BootstrapError::InvalidPrincipal | BootstrapError::Constraint | BootstrapError::InvalidInput => {
                    ApiError::forbidden()
                }
            });
        }
    }
    identity.mark_bootstrap_done();
    // A privilege change: the session that just became owner is replaced.
    let rotated = sessions::rotate_session(state.pool(), &session_token, identity.limits()).await.map_err(session_error)?;
    let body = json!({ "tenant_id": tenant.to_string(), "principal_id": principal.to_string(), "role": "tenant_owner" });
    respond(StatusCode::OK, &[session_cookie(identity, &rotated)?], None, Some(body))
}

async fn exchange_share(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<TokenBody>) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    require_same_origin(identity, &headers)?;
    let token = bounded_token(&body)?;
    let grant = share_links::exchange_share_token(state.pool(), token).await.map_err(|error| match error {
        ShareLinkError::Database => ApiError::unavailable(),
        _ => ApiError::unauthenticated(),
    })?;
    let max_age = grant.session_expires_at.saturating_sub(now_unix()).clamp(0, 3600);
    let cookie = host_cookie(SHARE_COOKIE, &identity.share_cookie_value(&grant), max_age, "Strict");
    let body = json!({
        "tenant_id": grant.tenant_id.to_string(),
        "document_id": grant.document_id.to_string(),
        "role": grant.role.as_str(),
        "expires_at": grant.session_expires_at,
    });
    respond(StatusCode::OK, &[cookie], None, Some(body))
}

async fn session(State(state): State<AppState>, method: Method, uri: Uri, headers: HeaderMap) -> Result<Response, ApiError> {
    let identity = identity(&state)?;
    let authenticator = authenticator(&state)?;
    let caller = authenticator.identify(&method, &headers).await.map_err(ApiError::from)?;
    let bootstrap_required = match identity.bootstrap_tenant() {
        Some(tenant) if !identity.bootstrap_known_done() => gate_check(&state, identity, tenant).await?,
        _ => false,
    };
    let mut cookies = Vec::new();
    let body = match caller {
        Caller::Browser(session) => {
            let tenant = match selected_tenant(identity, uri.path(), &headers).map_err(ApiError::from)? {
                Some(tenant) => match authenticator.member(tenant, session.principal_id).await {
                    Ok(context) => json!({
                        "id": tenant.to_string(),
                        "role": role_str(context.role()),
                        "permissions": context.permissions().iter().filter_map(|permission| serde_json::to_value(permission).ok()).collect::<Vec<_>>(),
                    }),
                    Err(AuthRejection::Unavailable) => return Err(ApiError::unavailable()),
                    Err(_) => Value::Null,
                },
                None => Value::Null,
            };
            if identity.policy().rotation_due(session.age_secs, None) == Some(RotationTrigger::Age)
                && let Some(token) = cookie(&headers, COOKIE_NAME)
            {
                let rotated = sessions::rotate_session(state.pool(), token, identity.limits()).await.map_err(session_error)?;
                cookies.push(session_cookie(identity, &rotated)?);
            }
            json!({
                "credential": "browser_session",
                "principal_id": session.principal_id.to_string(),
                "tenant": tenant,
                "session_expires_in": session.absolute_remaining_secs,
            })
        }
        Caller::ServiceAccount(account) => json!({
            "credential": "service_account",
            "principal_id": account.principal_id.to_string(),
            "tenant": { "id": account.tenant_id.to_string(), "role": Value::Null, "permissions": [] },
            "service_account_id": account.service_account_id.to_string(),
            "scopes": account.scopes.iter().map(|scope| scope.as_str()).collect::<Vec<_>>(),
        }),
        Caller::Share(share) => json!({
            "credential": "share_link",
            "principal_id": Value::Null,
            "tenant": { "id": share.tenant_id.to_string(), "role": Value::Null, "permissions": [] },
            "document": { "id": share.document_id.to_string(), "role": share.role.as_str(), "expires_at": share.expires_at },
        }),
    };
    let mut body = body;
    body["bootstrap_required"] = Value::Bool(bootstrap_required);
    body["deployment"] = Value::String(
        match identity.deployment() {
            DeploymentMode::SingleTenant => "single",
            DeploymentMode::MultiTenant => "multi",
            DeploymentMode::Embedded => "embedded",
        }
        .to_string(),
    );
    respond(StatusCode::OK, &cookies, None, Some(body))
}

/// Whether bootstrap is still required (remembering a completed one).
async fn gate_check(state: &AppState, identity: &IdentityConfig, tenant: TenantId) -> Result<bool, ApiError> {
    let required = bootstrap::bootstrap_required(state.pool(), tenant).await.map_err(|_| ApiError::unavailable())?;
    if !required {
        identity.mark_bootstrap_done();
    }
    Ok(required)
}

/// `/api/v1` paths served while bootstrap is required.
fn bootstrap_exempt(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/auth/login"
            | "/api/v1/auth/callback"
            | "/api/v1/auth/logout"
            | "/api/v1/auth/backchannel-logout"
            | "/api/v1/auth/session"
            | "/api/v1/auth/bootstrap/complete"
    )
}

/// The fixed body of a refusal while bootstrap is required (not an [`ErrorCode`]: it is a
/// deployment state, not a request error, and only this gate produces it).
pub(crate) fn bootstrap_required_body(request_id: &str) -> String {
    json!({"error": {"code": "bootstrap_required", "message": "This deployment must complete bootstrap first.", "request_id": request_id}})
        .to_string()
}

/// Middleware over every `/api/v1` route: while the single tenant needs bootstrap, only the
/// bootstrap flow is served and everything else is a fixed `403 bootstrap_required`.
pub(crate) async fn bootstrap_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(identity) = state.identity() else {
        return next.run(request).await;
    };
    let Some(tenant) = identity.bootstrap_tenant() else {
        return next.run(request).await;
    };
    if identity.bootstrap_known_done() || bootstrap_exempt(request.uri().path()) {
        return next.run(request).await;
    }
    match gate_check(&state, identity, tenant).await {
        Ok(false) => next.run(request).await,
        Ok(true) => {
            let request_id = request.extensions().get::<RequestContext>().map(|context| context.request_id.to_string()).unwrap_or_default();
            let mut response = (StatusCode::FORBIDDEN, bootstrap_required_body(&request_id)).into_response();
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
#[path = "auth_routes_tests.rs"]
mod tests;
