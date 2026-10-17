//! The `/api/v1` HTTP API (S4): shared state, the router other modules plug into, and the
//! middleware every route gets.
//!
//! Layering of a request on port 8081, outermost first:
//! 1. [`context::middleware`]: request ID, `traceparent`, in-flight cap, deadline, declared body
//!    size, error envelope, structured request log (all routes, health included).
//! 2. [`axum::extract::DefaultBodyLimit`]: the same body cap for streamed bodies read by extractors.
//! 3. [`auth_routes::bootstrap_gate`]: while a single-tenant deployment needs bootstrap, every
//!    `/api/v1` route but the bootstrap flow answers a fixed `403 bootstrap_required` (S4-003).
//! 4. `/api/v1/auth/*` only: [`rate_limit::auth_middleware`] (S4-006).
//! 5. Handlers, which take [`auth::Authenticated`] to require a caller and return
//!    [`error::ApiError`] on failure, following the disclosure policy in [`error`] (S4-016).
//!
//! # Adding routes
//!
//! Put a module under `api/` exposing `fn routes() -> Router<AppState>` with paths relative to its
//! mount point, then merge it in [`api_router`]: authentication endpoints (login, callback,
//! logout, token exchange) into the `auth` router so they are rate limited; everything else into
//! the `resources` router.

#![allow(dead_code, reason = "S4 modules (OIDC, sessions, service accounts, tenant APIs) build on this foundation")]

pub(crate) mod auth;
pub(crate) mod auth_routes;
pub(crate) mod authenticator;
pub(crate) mod client_ip;
pub(crate) mod context;
pub(crate) mod cursor;
pub(crate) mod documents_api;
pub(crate) mod error;
pub(crate) mod rate_limit;
pub(crate) mod realtime;
pub(crate) mod teams_api;
pub(crate) mod tenants_api;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use figcraft_cloud_domain::DeploymentMode;
use figcraft_cloud_storage::objects::ObjectStore;
use sqlx::PgPool;

use figcraft_cloud_auth::oidc::OidcProvider;

use self::auth::{Authenticator, DenyAll};
use self::authenticator::{CredentialAuthenticator, IdentityConfig};
use self::client_ip::Cidr;
use self::error::ApiError;
use self::rate_limit::{RateLimitConfig, RateLimiter};

/// The default cap on a request body.
pub(crate) const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// API settings derived from the process configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApiConfig {
    pub(crate) deployment: DeploymentMode,
    /// Peers whose `X-Forwarded-For` is believed (the gateway). Empty: none.
    pub(crate) trusted_proxies: Vec<Cidr>,
    pub(crate) auth_rate_limit: RateLimitConfig,
    pub(crate) max_body_bytes: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            deployment: DeploymentMode::SingleTenant,
            trusted_proxies: Vec::new(),
            auth_rate_limit: RateLimitConfig::default(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

/// Everything a handler can reach. Cloning is cheap (shared pointers).
#[derive(Clone)]
pub(crate) struct AppState {
    pool: PgPool,
    objects: ObjectStore,
    config: Arc<ApiConfig>,
    authenticator: Arc<dyn Authenticator>,
    auth_limiter: Arc<RateLimiter>,
    /// Identity settings (sessions, CSRF origin, share-session key, bootstrap gate); `None`
    /// leaves `/api/v1/auth` answering `503`.
    identity: Option<Arc<IdentityConfig>>,
    /// The OIDC provider, when `OIDC_ISSUER`, `OIDC_CLIENT_ID` and `OIDC_REDIRECT_URL` are set.
    oidc: Option<Arc<OidcProvider>>,
    /// The realtime-ticket key (derived from `SESSION_SIGNING_KEY`); `None` leaves
    /// `POST /documents/{id}/realtime-ticket` answering `503` and collab links on share links.
    realtime: Option<Arc<figcraft_cloud_storage::realtime_tickets::TicketKey>>,
}

impl AppState {
    /// State with the deny-all authenticator.
    pub(crate) fn new(pool: PgPool, objects: ObjectStore, config: ApiConfig) -> Self {
        let auth_limiter = Arc::new(RateLimiter::new(config.auth_rate_limit));
        Self { pool, objects, config: Arc::new(config), authenticator: Arc::new(DenyAll), auth_limiter, identity: None, oidc: None, realtime: None }
    }

    /// Install the production identity stack: these settings and the
    /// [`CredentialAuthenticator`] (session cookie, service-account bearer, share session).
    pub(crate) fn with_identity(mut self, identity: Arc<IdentityConfig>) -> Self {
        self.authenticator = Arc::new(CredentialAuthenticator::new(self.pool.clone(), Arc::clone(&identity)));
        self.identity = Some(identity);
        self
    }

    /// Enable OIDC login.
    pub(crate) fn with_oidc(mut self, provider: Arc<OidcProvider>) -> Self {
        self.oidc = Some(provider);
        self
    }

    /// Enable realtime tickets with `key` (the relay must verify with the same key).
    pub(crate) fn with_realtime_tickets(mut self, key: Arc<figcraft_cloud_storage::realtime_tickets::TicketKey>) -> Self {
        self.realtime = Some(key);
        self
    }

    pub(crate) fn realtime_key(&self) -> Option<&figcraft_cloud_storage::realtime_tickets::TicketKey> {
        self.realtime.as_deref()
    }

    pub(crate) fn identity(&self) -> Option<&IdentityConfig> {
        self.identity.as_deref()
    }

    pub(crate) fn oidc(&self) -> Option<&OidcProvider> {
        self.oidc.as_deref()
    }

    /// The production authenticator over this state, when identity is configured.
    pub(crate) fn credential_authenticator(&self) -> Option<CredentialAuthenticator> {
        self.identity.as_ref().map(|identity| CredentialAuthenticator::new(self.pool.clone(), Arc::clone(identity)))
    }

    /// Replace the authenticator (sessions, service accounts, share links).
    pub(crate) fn with_authenticator(mut self, authenticator: Arc<dyn Authenticator>) -> Self {
        self.authenticator = authenticator;
        self
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn objects(&self) -> &ObjectStore {
        &self.objects
    }

    pub(crate) fn config(&self) -> &ApiConfig {
        &self.config
    }

    pub(crate) fn authenticator(&self) -> &dyn Authenticator {
        self.authenticator.as_ref()
    }

    pub(crate) fn auth_limiter(&self) -> &RateLimiter {
        &self.auth_limiter
    }
}

/// The `/api/v1` router (full paths) with every module's routes.
pub(crate) fn api_router(state: AppState) -> Router {
    // S4 modules merge their routers here, e.g. `.merge(oidc::routes())`.
    let auth: Router<AppState> = Router::new().merge(auth_routes::routes());
    let resources: Router<AppState> = Router::new().merge(tenants_api::routes()).merge(documents_api::routes());
    api_router_with(state, auth, resources)
}

/// [`api_router`] with explicit `auth` (mounted at `/api/v1/auth`, rate limited) and `resources`
/// (mounted at `/api/v1`) routers. Unknown paths answer `404 not_found` in the envelope.
pub(crate) fn api_router_with(state: AppState, auth: Router<AppState>, resources: Router<AppState>) -> Router {
    let auth = auth.fallback(not_found).layer(from_fn_with_state(state.clone(), rate_limit::auth_middleware));
    let api = resources.nest("/auth", auth).fallback(not_found);
    Router::new()
        .nest("/api/v1", api)
        .layer(from_fn_with_state(state.clone(), auth_routes::bootstrap_gate))
        .layer(DefaultBodyLimit::max(state.config().max_body_bytes))
        .with_state(state)
}

/// `/api/v1` while dependencies are not ready: every route answers `503`.
pub(crate) fn unavailable_router() -> Router {
    Router::new().nest("/api/v1", Router::new().fallback(unavailable))
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

async fn unavailable() -> ApiError {
    ApiError::unavailable()
}

#[cfg(test)]
mod revocation_race_tests;
#[cfg(test)]
mod route_matrix_tests;
#[cfg(test)]
mod tests;
