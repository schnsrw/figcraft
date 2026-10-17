//! The platform API under `/platform/v1` (S6-045, S6-046).
//!
//! This is the operators' surface: platform admins sign in here, create and suspend workspaces,
//! assign plans and read the platform audit log. It is a separate realm from `/api/v1`:
//!
//! * its own authenticator: the cookie [`PLATFORM_COOKIE`] (a different name and path from the
//!   workspace session cookie, `SameSite=Strict`, 15 minutes idle and 4 hours absolute by default)
//!   resolves only against `platform_sessions`; a workspace session or service-account credential
//!   is never looked at, and a platform cookie is never looked at by `/api/v1`;
//! * its own database access ([`figcraft_cloud_storage::platform`]): tables a workspace
//!   transaction cannot read;
//! * its own rate limits (stricter for sign-in), error envelope shared with the rest of the API;
//! * its own OpenAPI document, `GET /platform/v1/openapi.json`.
//!
//! Sign-in is two steps: password (Argon2id, through [`PasswordHasher`]) then a TOTP code. Until
//! the second step passes, the session can only enrol or verify the second factor. Every outcome,
//! including refusals, is appended to the platform audit log.

pub(crate) mod admins;
pub(crate) mod session;
pub(crate) mod totp;
pub(crate) mod workspaces;

use std::net::SocketAddr;
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts, MatchedPath, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use figcraft_cloud_domain::{ErrorCode, TenantId, Uuid};
use figcraft_cloud_storage::platform::{
    PlatformActor, PlatformAudit, PlatformError, PlatformOutcome, begin_platform_transaction, record_platform_audit,
};
use figcraft_cloud_storage::platform_admins::{self, PlatformSessionLimits};

use crate::api::authenticator::{cookie, is_unsafe};
use crate::api::context::RequestContext;
use crate::api::error::ApiError;
use crate::api::rate_limit::{Decision, RateLimitConfig, RateLimiter, retry_after_seconds};
use crate::api::{AppState, client_ip};

/// The platform session cookie. Its name and `Path` differ from every workspace cookie.
pub(crate) const PLATFORM_COOKIE: &str = "__Secure-figcraft-platform";
/// Where the cookie is sent: the platform API only.
pub(crate) const COOKIE_PATH: &str = "/platform/v1";
/// The checked-in OpenAPI document.
pub(crate) const OPENAPI_JSON: &str = include_str!("../../openapi/platform-api-v1.json");
/// Consecutive failed sign-ins before an admin is locked.
pub(crate) const LOCK_AFTER_FAILURES: i32 = 5;
/// How long a locked admin stays locked.
pub(crate) const LOCK_SECS: i64 = 15 * 60;
/// A sign-in password longer than this is refused before any hashing.
pub(crate) const MAX_SIGN_IN_PASSWORD_BYTES: usize = 1024;

/// Platform-wide runtime pieces held by [`AppState`].
pub(crate) struct PlatformRuntime {
    /// Sign-in, bootstrap and second-factor attempts per client.
    pub(crate) public_limiter: RateLimiter,
    /// Authenticated calls per client.
    pub(crate) api_limiter: RateLimiter,
    pub(crate) limits: PlatformSessionLimits,
}

impl PlatformRuntime {
    pub(crate) fn new() -> Self {
        Self {
            public_limiter: RateLimiter::new(RateLimitConfig { burst: 5, refill_every: std::time::Duration::from_secs(12), max_keys: 10_000 }),
            api_limiter: RateLimiter::new(RateLimitConfig { burst: 120, refill_every: std::time::Duration::from_millis(500), max_keys: 10_000 }),
            limits: PlatformSessionLimits::default(),
        }
    }
}

/// `Set-Cookie` value carrying a platform session token.
pub(crate) fn session_cookie(token: &str, max_age_secs: i64) -> String {
    format!("{PLATFORM_COOKIE}={token}; Max-Age={}; Path={COOKIE_PATH}; Secure; HttpOnly; SameSite=Strict", max_age_secs.max(0))
}

pub(crate) fn clear_session_cookie() -> String {
    format!("{PLATFORM_COOKIE}=; Max-Age=0; Path={COOKIE_PATH}; Secure; HttpOnly; SameSite=Strict")
}

/// Add a `Set-Cookie` to a response.
pub(crate) fn with_cookie(mut response: Response, value: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(crate) fn api_error(error: PlatformError) -> ApiError {
    match error {
        PlatformError::Invalid => ApiError::new(ErrorCode::InvalidRequest),
        PlatformError::NotFound => ApiError::not_found(),
        PlatformError::Conflict => ApiError::new(ErrorCode::Conflict),
        PlatformError::Refused => ApiError::forbidden(),
        PlatformError::Database => ApiError::unavailable(),
        PlatformError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

/// The shared password service, or `503` when passwords are not configured.
pub(crate) fn passwords(state: &AppState) -> Result<&crate::api::passwords::PasswordService, ApiError> {
    state.passwords().ok_or(ApiError::unavailable())
}

/// A hashing failure as an API error: a policy violation is the caller's `400`, the rest `503`.
pub(crate) fn service_error(error: crate::api::passwords::ServiceError) -> ApiError {
    match error {
        crate::api::passwords::ServiceError::Policy(_) => ApiError::new(ErrorCode::InvalidRequest),
        crate::api::passwords::ServiceError::Unavailable => ApiError::unavailable(),
    }
}

/// Append an audit event in its own platform transaction (used for refusals and for changes whose
/// own transaction already ended).
pub(crate) async fn audit(
    state: &AppState,
    context: &RequestContext,
    actor: PlatformActor,
    action: &str,
    resource_kind: &str,
    resource_id: Option<Uuid>,
    tenant: Option<TenantId>,
    outcome: PlatformOutcome,
    metadata: &serde_json::Value,
) {
    let request_id = context.request_id.to_string();
    let record = async {
        let mut tx = begin_platform_transaction(state.pool()).await?;
        record_platform_audit(
            &mut tx,
            &PlatformAudit { actor, action, resource_kind, resource_id, tenant_id: tenant, outcome, request_id: Some(&request_id), metadata },
        )
        .await?;
        tx.commit().await
    };
    if record.await.is_err() {
        eprintln!("figcraft-cloud: a platform audit event could not be recorded");
    }
}

/// A caller whose session passed the password step, with or without the second factor.
#[derive(Clone, Debug)]
pub(crate) struct PartialSession {
    pub(crate) session_id: Uuid,
    pub(crate) admin_id: Uuid,
    pub(crate) login: String,
    pub(crate) second_factor_done: bool,
}

impl FromRequestParts<AppState> for PartialSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let identity = state.identity().ok_or(ApiError::unavailable())?;
        let token = cookie(&parts.headers, PLATFORM_COOKIE).ok_or(ApiError::unauthenticated())?;
        if is_unsafe(&parts.method) && !identity.same_origin(&parts.headers) {
            return Err(ApiError::forbidden());
        }
        let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
        let resolved = platform_admins::resolve_session(&mut tx, token, state.platform().limits).await;
        tx.commit().await.map_err(api_error)?;
        match resolved {
            Ok(session) => {
                Ok(Self { session_id: session.id, admin_id: session.admin_id, login: session.login, second_factor_done: session.second_factor_done })
            }
            Err(PlatformError::Refused) => Err(ApiError::unauthenticated()),
            Err(error) => Err(api_error(error)),
        }
    }
}

/// A fully authenticated platform caller (password and second factor done).
#[derive(Clone, Debug)]
pub(crate) struct PlatformCaller {
    pub(crate) actor: PlatformActor,
    pub(crate) admin_id: Uuid,
}

impl FromRequestParts<AppState> for PlatformCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let session = PartialSession::from_request_parts(parts, state).await?;
        if !session.second_factor_done {
            return Err(ApiError::unauthenticated());
        }
        Ok(Self { actor: PlatformActor::Admin(session.admin_id), admin_id: session.admin_id })
    }
}

async fn limit_public(State(state): State<AppState>, request: Request, next: Next) -> Response {
    limit(&state.platform().public_limiter, &state, request, next).await
}

async fn limit_api(State(state): State<AppState>, request: Request, next: Next) -> Response {
    limit(&state.platform().api_limiter, &state, request, next).await
}

async fn limit(limiter: &RateLimiter, state: &AppState, request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|info| info.0.ip());
    let client = client_ip::resolve(peer, request.headers(), &state.config().trusted_proxies);
    let route = request.extensions().get::<MatchedPath>().map_or("/platform/v1/*", MatchedPath::as_str);
    match limiter.check_at(client, route, Instant::now()) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after } => ApiError::rate_limited(retry_after_seconds(retry_after)).into_response(),
    }
}

async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI_JSON).into_response()
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

/// The `/platform/v1` router (full paths).
pub(crate) fn router(state: AppState) -> Router {
    let public: Router<AppState> = Router::new()
        .route("/bootstrap", post(session::bootstrap))
        .route("/session", post(session::sign_in))
        .route("/session/totp", post(session::verify_totp))
        .route("/session/totp/enrol", post(session::enrol_totp))
        .layer(from_fn_with_state(state.clone(), limit_public));
    let authenticated: Router<AppState> = Router::new()
        .route("/session", get(session::current).delete(session::sign_out))
        .route("/admins", get(admins::list).post(admins::create))
        .route("/admins/{id}/disable", post(admins::disable))
        .route("/admins/{id}/enable", post(admins::enable))
        .merge(workspaces::routes())
        .route("/audit", get(workspaces::audit_log))
        .layer(from_fn_with_state(state.clone(), limit_api));
    let api = Router::new().route("/openapi.json", get(openapi)).merge(public).merge(authenticated).fallback(not_found);
    Router::new().nest("/platform/v1", api).with_state(state)
}

#[cfg(test)]
mod tests;
