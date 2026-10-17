//! Who is calling: the [`Authenticated`] extractor and the pluggable [`Authenticator`].
//!
//! A handler that takes `Authenticated` runs only for a caller the configured authenticator
//! accepted; every other request gets `401 unauthenticated` (or `403`/`503` as the authenticator
//! decides) before the handler sees it. The default authenticator denies everything, so a route
//! is never reachable anonymously by accident. Session-cookie, service-account and share-link
//! authenticators (S4-002/004/005) implement [`Authenticator`] and are installed with
//! [`super::AppState::with_authenticator`].

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use figcraft_cloud_domain::{ErrorCode, PrincipalId, TenantContext};

use super::AppState;
use super::client_ip;
use super::error::ApiError;

/// How the caller proved who they are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialKind {
    /// A browser session cookie (`browser_sessions`).
    BrowserSession,
    /// A service-account bearer credential (`service_account_credentials`).
    ServiceAccount,
    /// A document share link (`share_links`).
    ShareLink,
}

/// The credential behind this request: its kind and its row identifier (never the secret).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionInfo {
    pub(crate) kind: CredentialKind,
    /// The canonical UUID of the session, credential or link row.
    pub(crate) id: String,
}

/// An authenticated caller in one tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Authenticated {
    pub(crate) principal: PrincipalId,
    pub(crate) tenant: TenantContext,
    pub(crate) session: SessionInfo,
}

/// Why a request was not authenticated. Each maps to one fixed public error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthRejection {
    /// No credential, or an invalid, expired or revoked one: `401`.
    Unauthenticated,
    /// A valid credential that may not use this surface (e.g. a share link on a tenant API): `403`.
    Forbidden,
    /// The principal or tenant is suspended: `409 resource_inactive`.
    Inactive,
    /// The credential store could not be reached: `503`.
    Unavailable,
}

impl From<AuthRejection> for ApiError {
    fn from(rejection: AuthRejection) -> Self {
        match rejection {
            AuthRejection::Unauthenticated => ApiError::unauthenticated(),
            AuthRejection::Forbidden => ApiError::forbidden(),
            AuthRejection::Inactive => ApiError::new(ErrorCode::ResourceInactive),
            AuthRejection::Unavailable => ApiError::unavailable(),
        }
    }
}

/// What an authenticator may look at. The query string is deliberately absent: credentials in
/// URLs are not accepted on API routes.
#[derive(Debug)]
pub(crate) struct AuthRequest<'a> {
    pub(crate) method: &'a Method,
    pub(crate) path: &'a str,
    pub(crate) headers: &'a HeaderMap,
    /// The client address after trusted-proxy resolution.
    pub(crate) client_ip: Option<IpAddr>,
}

/// The future an [`Authenticator`] returns.
pub(crate) type AuthFuture<'a> = Pin<Box<dyn Future<Output = Result<Authenticated, AuthRejection>> + Send + 'a>>;

/// Resolves the caller of a request. Implementations must not log credentials, must compare
/// secrets by hash (or in constant time) and must bound their own work.
pub(crate) trait Authenticator: Send + Sync + 'static {
    fn authenticate<'a>(&'a self, request: &'a AuthRequest<'a>) -> AuthFuture<'a>;
}

/// The default: nobody is authenticated.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DenyAll;

impl Authenticator for DenyAll {
    fn authenticate<'a>(&'a self, _request: &'a AuthRequest<'a>) -> AuthFuture<'a> {
        Box::pin(async { Err(AuthRejection::Unauthenticated) })
    }
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        if let Some(existing) = parts.extensions.get::<Self>() {
            return Ok(existing.clone());
        }
        let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|info| info.0.ip());
        let request = AuthRequest {
            method: &parts.method,
            path: parts.uri.path(),
            headers: &parts.headers,
            client_ip: client_ip::resolve(peer, &parts.headers, &state.config().trusted_proxies),
        };
        let authenticated = state.authenticator().authenticate(&request).await.map_err(ApiError::from)?;
        parts.extensions.insert(authenticated.clone());
        Ok(authenticated)
    }
}
