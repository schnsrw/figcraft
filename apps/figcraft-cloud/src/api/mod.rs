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

pub(crate) mod account_routes;
pub(crate) mod accounts_contract;
pub(crate) mod auth;
pub(crate) mod auth_routes;
pub(crate) mod authenticator;
pub(crate) mod client_ip;
pub(crate) mod context;
pub(crate) mod cursor;
pub(crate) mod documents_api;
pub(crate) mod error;
pub(crate) mod host_api;
pub(crate) mod host_events;
pub(crate) mod login_throttle;
pub(crate) mod me_routes;
pub(crate) mod me_api;
pub(crate) mod passwords;
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
    /// Embed-token exchanges per issuer (S6-012).
    embed_limiter: Arc<RateLimiter>,
    /// Where webhook deliveries may go (S6-013); public destinations only unless a test or
    /// development setup says otherwise.
    webhook_policy: figcraft_cloud_storage::webhooks::DestinationPolicy,
    /// Identity settings (sessions, CSRF origin, share-session key, bootstrap gate); `None`
    /// leaves `/api/v1/auth` answering `503`.
    identity: Option<Arc<IdentityConfig>>,
    /// The OIDC provider, when `OIDC_ISSUER`, `OIDC_CLIENT_ID` and `OIDC_REDIRECT_URL` are set.
    oidc: Option<Arc<OidcProvider>>,
    /// The realtime-ticket key (derived from `SESSION_SIGNING_KEY`); `None` leaves
    /// `POST /documents/{id}/realtime-ticket` answering `503` and collab links on share links.
    realtime: Option<Arc<figcraft_cloud_storage::realtime_tickets::TicketKey>>,
    /// Password hashing for built-in accounts (S6-033); `None` leaves password routes answering `503`.
    passwords: Option<passwords::PasswordService>,
    /// Failed-sign-in lockout policy and the per-login backoff table (S6-035).
    lockout: figcraft_cloud_storage::credentials::LockoutPolicy,
    login_throttle: Arc<login_throttle::LoginThrottle>,
    /// The platform realm's limiters and session limits (S6-045).
    platform: Arc<crate::platform_api::PlatformRuntime>,
}

impl AppState {
    /// State with the deny-all authenticator.
    pub(crate) fn new(pool: PgPool, objects: ObjectStore, config: ApiConfig) -> Self {
        let auth_limiter = Arc::new(RateLimiter::new(config.auth_rate_limit));
        Self {
            pool,
            objects,
            config: Arc::new(config),
            authenticator: Arc::new(DenyAll),
            auth_limiter,
            embed_limiter: Arc::new(RateLimiter::new(host_api::EMBED_LIMIT)),
            webhook_policy: figcraft_cloud_storage::webhooks::DestinationPolicy::PUBLIC_ONLY,
            identity: None,
            oidc: None,
            realtime: None,
            passwords: None,
            lockout: figcraft_cloud_storage::credentials::LockoutPolicy::default(),
            login_throttle: Arc::new(login_throttle::LoginThrottle::new(figcraft_cloud_storage::credentials::LockoutPolicy::default())),
            platform: Arc::new(crate::platform_api::PlatformRuntime::new()),
        }
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

    /// Enable built-in password accounts.
    pub(crate) fn with_passwords(mut self, service: passwords::PasswordService) -> Self {
        self.passwords = Some(service);
        self
    }

    /// Use `policy` for sign-in lockout and backoff.
    pub(crate) fn with_lockout(mut self, policy: figcraft_cloud_storage::credentials::LockoutPolicy) -> Self {
        self.lockout = policy;
        self.login_throttle = Arc::new(login_throttle::LoginThrottle::new(policy));
        self
    }

    pub(crate) const fn lockout(&self) -> figcraft_cloud_storage::credentials::LockoutPolicy {
        self.lockout
    }

    pub(crate) fn login_throttle(&self) -> &login_throttle::LoginThrottle {
        &self.login_throttle
    }

    /// Replace the platform realm's runtime (tests use generous limits).
    #[cfg(test)]
    pub(crate) fn with_platform_runtime(mut self, runtime: crate::platform_api::PlatformRuntime) -> Self {
        self.platform = Arc::new(runtime);
        self
    }

    pub(crate) fn platform(&self) -> &crate::platform_api::PlatformRuntime {
        &self.platform
    }

    pub(crate) fn passwords(&self) -> Option<&passwords::PasswordService> {
        self.passwords.as_ref()
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

    /// Allow private and loopback webhook destinations (development and tests only).
    #[must_use]
    pub(crate) fn with_webhook_policy(mut self, policy: figcraft_cloud_storage::webhooks::DestinationPolicy) -> Self {
        self.webhook_policy = policy;
        self
    }

    pub(crate) const fn webhook_policy(&self) -> figcraft_cloud_storage::webhooks::DestinationPolicy {
        self.webhook_policy
    }

    /// Replace the per-issuer embed-exchange limit (tests use a tiny, non-refilling one).
    #[must_use]
    pub(crate) fn with_embed_limit(mut self, config: RateLimitConfig) -> Self {
        self.embed_limiter = Arc::new(RateLimiter::new(config));
        self
    }

    pub(crate) fn embed_limiter(&self) -> &RateLimiter {
        &self.embed_limiter
    }
}

/// The `/api/v1` router (full paths) with every module's routes.
pub(crate) fn api_router(state: AppState) -> Router {
    // S4 modules merge their routers here, e.g. `.merge(oidc::routes())`.
    let auth: Router<AppState> = Router::new()
        .merge(auth_routes::routes())
        .merge(account_routes::auth_routes())
        .merge(accounts_contract::routes())
        .merge(host_api::auth_router());
    let resources: Router<AppState> = Router::new()
        .merge(tenants_api::routes())
        .merge(account_routes::resource_routes())
        .merge(me_routes::routes())
        .merge(me_api::me_routes())
        .merge(documents_api::routes())
        .merge(host_api::router());
    // The platform realm (`/platform/v1`) is a separate router with its own authenticator.
    let platform = crate::platform_api::router(state.clone()).layer(DefaultBodyLimit::max(state.config().max_body_bytes));
    api_router_with(state, auth, resources).merge(platform)
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
mod host_api_tests;
#[cfg(test)]
mod revocation_race_tests;
#[cfg(test)]
mod route_matrix_tests;
#[cfg(test)]
mod tests;
