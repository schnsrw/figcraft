//! Who is calling: the [`Authenticated`] extractor and the pluggable [`Authenticator`].
//!
//! A handler that takes `Authenticated` runs only for a caller the configured authenticator
//! accepted; every other request gets `401 unauthenticated` (or `403`/`503` as the authenticator
//! decides) before the handler sees it. The default authenticator denies everything, so a route
//! is never reachable anonymously by accident. Session-cookie, service-account and share-link
//! authenticators (S4-002/004/005) implement [`Authenticator`] and are installed with
//! [`super::AppState::with_authenticator`]; the production one is
//! [`super::authenticator::CredentialAuthenticator`], installed by
//! [`super::AppState::with_identity`].
//!
//! [`Authenticated::grant`] says what the credential itself limits the caller to. Handlers
//! authorize members with `figcraft_cloud_auth::authz::decide`, and must additionally call
//! [`Authenticated::require_scope`] (service accounts) or [`Authenticated::require_document`]
//! (share sessions) before acting.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use figcraft_cloud_domain::{CloudDocumentId, ErrorCode, PrincipalId, TenantContext};
use figcraft_cloud_storage::service_accounts::Scope;
use figcraft_cloud_storage::share_links::ShareRole;

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

/// What the credential limits the caller to, on top of role-based authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Grant {
    /// An active tenant member signed in through a browser session: the tenant role in
    /// [`Authenticated::tenant`] and the resource roles apply, nothing more.
    Member,
    /// A service account: only these allowlisted scopes (sorted, deduplicated, never empty). Its
    /// `tenant` role is a `viewer` placeholder: service principals are not tenant members.
    ServiceAccount { scopes: Vec<Scope> },
    /// A share session: this one document, at most this role. Its principal is the link's
    /// creator (attribution only); the authenticator refuses it on every other route.
    Share { document_id: CloudDocumentId, role: ShareRole, expires_at: i64 },
}

/// An authenticated caller in one tenant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Authenticated {
    pub(crate) principal: PrincipalId,
    pub(crate) tenant: TenantContext,
    pub(crate) session: SessionInfo,
    pub(crate) grant: Grant,
    /// Whether the request's method can change state (anything but GET, HEAD, OPTIONS). Set by
    /// the extractor from the request, whatever the authenticator put here; mutating requests
    /// hold their access for the whole transaction
    /// ([`super::authenticator::hold_caller_access`]).
    pub(crate) mutating: bool,
}

impl Authenticated {
    /// `403` unless the credential carries `scope`. Browser members are not scope-limited; a
    /// share session implies `documents:read`, plus `documents:write` for an editor link (its
    /// document binding is [`Self::require_document`]).
    pub(crate) fn require_scope(&self, scope: Scope) -> Result<(), ApiError> {
        let allowed = match &self.grant {
            Grant::Member => true,
            Grant::ServiceAccount { scopes } => scopes.contains(&scope),
            Grant::Share { role, .. } => scope == Scope::DocumentsRead || (scope == Scope::DocumentsWrite && *role == ShareRole::Editor),
        };
        if allowed { Ok(()) } else { Err(ApiError::forbidden()) }
    }

    /// `404` when a share session is used on a document other than its own (the caller cannot
    /// see any other document); members and service accounts pass to the normal checks.
    pub(crate) fn require_document(&self, document_id: CloudDocumentId) -> Result<(), ApiError> {
        match &self.grant {
            Grant::Share { document_id: own, .. } if *own != document_id => Err(ApiError::not_found()),
            _ => Ok(()),
        }
    }
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
        let mut authenticated = state.authenticator().authenticate(&request).await.map_err(ApiError::from)?;
        authenticated.mutating = super::authenticator::is_unsafe(&parts.method);
        parts.extensions.insert(authenticated.clone());
        Ok(authenticated)
    }
}
