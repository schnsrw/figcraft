//! The production [`Authenticator`] (S4-002, S4-004, S4-005 HTTP wiring) and the identity
//! settings the auth routes share with it.
//!
//! A request is identified by the first credential that resolves, in this order:
//!
//! 1. the browser session cookie `__Host-figcraft_session` (`sessions::resolve_session` with the
//!    [`SessionPolicy`] limits);
//! 2. a service-account bearer credential, `Authorization: Bearer fcsa_…`
//!    (`service_accounts::authenticate_credential`; any other `Authorization` value is refused);
//! 3. the share-session cookie `__Host-figcraft_share` minted by `POST /api/v1/auth/share/exchange`:
//!    an HMAC-SHA256-signed `(link, tenant, document, role, expiry)` tuple, checked in constant
//!    time (`ring::hmac::verify`), unexpired, and re-checked against storage on every request
//!    (`share_links::dead_share_links`), so revoking the link ends the session at once.
//!
//! A browser session's tenant comes from the request: in single-tenant mode it is
//! `FIGCRAFT_SINGLE_TENANT_ID`; otherwise the `X-Figcraft-Tenant` header or a
//! `/tenants/{tenant}/…` path (both, if present, must agree). The principal must be an *active*
//! member of an *active* tenant (`effective_roles::resolve_tenant_access`); anything else is a
//! uniform `403` so tenant existence cannot be probed. A service account is bound to its own
//! tenant, and a share session to its own document: it is refused (`403`) on any route that does
//! not name that document.
//!
//! CSRF: cookie-authenticated requests with an unsafe method (anything but GET, HEAD, OPTIONS)
//! must carry an `Origin` equal to the `FIGCRAFT_PUBLIC_URL` origin, or, without `Origin`,
//! `Sec-Fetch-Site: same-origin`. Neither present fails closed. Bearer requests are exempt (a
//! browser never attaches them by itself).
//!
//! Nothing here logs or echoes a credential; every refusal is one of the fixed
//! [`AuthRejection`]s.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, header};
use base64::Engine as _;
use figcraft_cloud_auth::session_policy::{COOKIE_NAME, SessionPolicy};
use figcraft_cloud_domain::{CloudDocumentId, DeploymentMode, Permission, PrincipalId, Role, TenantContext, TenantId, Uuid};
use figcraft_cloud_storage::access_guard::{self, AccessGuardError, HeldCredential, MembershipRequirement};
use figcraft_cloud_storage::effective_roles::{TenantAccess, resolve_tenant_access};
use figcraft_cloud_storage::memberships::MemberPermission;
use figcraft_cloud_storage::service_accounts::{self, AuthenticatedServiceAccount, SECRET_PREFIX, ServiceAccountError};
use figcraft_cloud_storage::sessions::{self, ResolvedSession, SessionError, SessionLimits};
use figcraft_cloud_storage::share_links::{self, ShareGrant, ShareRole};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use ring::hmac;
use sqlx::PgPool;

use super::AppState;
use super::auth::{AuthFuture, AuthRejection, AuthRequest, Authenticated, Authenticator, CredentialKind, Grant, SessionInfo};
use super::error::ApiError;

/// Header naming the tenant a browser session acts in (multi-tenant and embedded deployments).
pub(crate) const TENANT_HEADER: &str = "x-figcraft-tenant";
/// The share-session cookie.
pub(crate) const SHARE_COOKIE: &str = "__Host-figcraft_share";
/// The OIDC login-state cookie (set by `GET /auth/login`, consumed by `GET /auth/callback`).
pub(crate) const OIDC_STATE_COOKIE: &str = "__Host-figcraft_oidc";
/// Longest total of all `Cookie` headers scanned.
pub(crate) const MAX_COOKIE_BYTES: usize = 8 * 1024;
/// Longest cookie value accepted (the share cookie is about 200 characters).
pub(crate) const MAX_COOKIE_VALUE_CHARS: usize = 512;
/// Longest `Authorization` header value examined.
const MAX_AUTHORIZATION_CHARS: usize = 256;
/// Label the share-session MAC key is derived under.
const SHARE_KEY_LABEL: &[u8] = b"figcraft share-session v1";

/// Identity settings shared by the authenticator and the `/api/v1/auth` routes.
pub(crate) struct IdentityConfig {
    deployment: DeploymentMode,
    single_tenant: Option<TenantId>,
    public_origin: Option<String>,
    policy: SessionPolicy,
    share_key: hmac::Key,
    /// Seals secrets at rest (webhook signing secrets); derived from `SESSION_SIGNING_KEY`, so
    /// `None` without one (a random per-process key could not reopen what it sealed).
    secret_key: Option<figcraft_cloud_storage::secret_box::SecretKey>,
    bootstrap_done: AtomicBool,
}

impl std::fmt::Debug for IdentityConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdentityConfig")
            .field("deployment", &self.deployment)
            .field("single_tenant", &self.single_tenant)
            .field("public_origin", &self.public_origin)
            .field("share_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl IdentityConfig {
    /// Settings for `deployment`. Single-tenant deployments need `single_tenant`.
    /// `public_url` (`FIGCRAFT_PUBLIC_URL`) gives the origin CSRF checks compare against; without
    /// it only `Sec-Fetch-Site: same-origin` passes. `signing_key` (`SESSION_SIGNING_KEY`) keys
    /// the share-session MAC so every replica accepts the same cookies; without it a random
    /// per-process key is used.
    pub(crate) fn new(
        deployment: DeploymentMode,
        single_tenant: Option<TenantId>,
        public_url: Option<&str>,
        signing_key: Option<&[u8]>,
    ) -> Result<Self, &'static str> {
        if deployment == DeploymentMode::SingleTenant && single_tenant.is_none() {
            return Err("single-tenant deployments need FIGCRAFT_SINGLE_TENANT_ID");
        }
        let public_origin = public_url.map(|url| origin_of(url).ok_or("FIGCRAFT_PUBLIC_URL is not an http(s) origin")).transpose()?;
        let share_key = match signing_key {
            Some(secret) if secret.len() >= 16 => {
                let derived = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), SHARE_KEY_LABEL);
                hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref())
            }
            Some(_) => return Err("SESSION_SIGNING_KEY must be at least 16 bytes"),
            None => hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new()).map_err(|_| "no secure random numbers")?,
        };
        Ok(Self {
            deployment,
            single_tenant: if deployment == DeploymentMode::SingleTenant { single_tenant } else { None },
            public_origin,
            policy: SessionPolicy::default(),
            share_key,
            secret_key: signing_key.and_then(|secret| figcraft_cloud_storage::secret_box::SecretKey::derive(secret).ok()),
            bootstrap_done: AtomicBool::new(false),
        })
    }

    /// The key webhook secrets are sealed with, when `SESSION_SIGNING_KEY` is configured.
    pub(crate) const fn secret_key(&self) -> Option<&figcraft_cloud_storage::secret_box::SecretKey> {
        self.secret_key.as_ref()
    }

    /// Replace the session policy.
    #[must_use]
    pub(crate) fn with_policy(mut self, policy: SessionPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub(crate) const fn deployment(&self) -> DeploymentMode {
        self.deployment
    }

    pub(crate) const fn policy(&self) -> &SessionPolicy {
        &self.policy
    }

    /// The repository limits for [`Self::policy`].
    pub(crate) fn limits(&self) -> SessionLimits {
        let (idle_secs, absolute_secs, touch_interval_secs) = self.policy.storage_secs();
        SessionLimits { idle_secs, absolute_secs, touch_interval_secs }
    }

    /// The tenant that needs bootstrap gating: the single tenant, in single-tenant mode only.
    pub(crate) const fn bootstrap_tenant(&self) -> Option<TenantId> {
        self.single_tenant
    }

    /// Whether bootstrap was already seen completed (it never becomes required again).
    pub(crate) fn bootstrap_known_done(&self) -> bool {
        self.bootstrap_done.load(Ordering::Acquire)
    }

    pub(crate) fn mark_bootstrap_done(&self) {
        self.bootstrap_done.store(true, Ordering::Release);
    }

    /// Whether an unsafe cookie-authenticated request came from our own origin.
    pub(crate) fn same_origin(&self, headers: &HeaderMap) -> bool {
        same_origin_request(headers, self.public_origin.as_deref())
    }

    /// The signed share-session cookie value for `grant`.
    pub(crate) fn share_cookie_value(&self, grant: &ShareGrant) -> String {
        let payload = format!(
            "v1.{}.{}.{}.{}.{}.{}",
            grant.link_id,
            grant.tenant_id,
            grant.document_id,
            share_role_tag(grant.role),
            grant.created_by,
            grant.session_expires_at
        );
        let tag = hmac::sign(&self.share_key, payload.as_bytes());
        format!("{payload}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag.as_ref()))
    }

    /// Verify a share-session cookie value: MAC first (constant time), then its fields.
    pub(crate) fn open_share_cookie(&self, value: &str) -> Option<ShareSession> {
        if value.len() > MAX_COOKIE_VALUE_CHARS {
            return None;
        }
        let (payload, tag) = value.rsplit_once('.')?;
        let tag = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(tag).ok()?;
        hmac::verify(&self.share_key, payload.as_bytes(), &tag).ok()?;
        let mut parts = payload.split('.');
        let (version, link, tenant, document, role, creator, expires) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if version != "v1" || parts.next().is_some() {
            return None;
        }
        Some(ShareSession {
            link_id: link.parse().ok()?,
            tenant_id: tenant.parse().ok()?,
            document_id: document.parse().ok()?,
            role: match role {
                "editor" => ShareRole::Editor,
                "viewer" => ShareRole::Viewer,
                _ => return None,
            },
            created_by: creator.parse().ok()?,
            expires_at: expires.parse().ok()?,
        })
    }
}

const fn share_role_tag(role: ShareRole) -> &'static str {
    match role {
        ShareRole::Editor => "editor",
        ShareRole::Viewer => "viewer",
    }
}

/// A verified share-session cookie.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShareSession {
    pub(crate) link_id: Uuid,
    pub(crate) tenant_id: TenantId,
    pub(crate) document_id: CloudDocumentId,
    pub(crate) role: ShareRole,
    /// Whom actions through the link are attributed to.
    pub(crate) created_by: PrincipalId,
    /// Unix seconds.
    pub(crate) expires_at: i64,
}

/// `scheme://host[:port]` of an http(s) URL, lowercased, default ports dropped.
pub(crate) fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') || !authority.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let authority = match (scheme.as_str(), authority.rsplit_once(':')) {
        ("https", Some((host, "443"))) | ("http", Some((host, "80"))) => host.to_string(),
        _ => authority,
    };
    Some(format!("{scheme}://{authority}"))
}

/// The CSRF rule: `Origin` must equal `public_origin`; without `Origin`, `Sec-Fetch-Site` must be
/// `same-origin`. Neither present (or no configured origin with an `Origin` header) fails closed.
pub(crate) fn same_origin_request(headers: &HeaderMap, public_origin: Option<&str>) -> bool {
    if let Some(origin) = headers.get(header::ORIGIN) {
        return match (origin.to_str(), public_origin) {
            (Ok(origin), Some(expected)) => origin.eq_ignore_ascii_case(expected),
            _ => false,
        };
    }
    headers.get("sec-fetch-site").is_some_and(|value| value.as_bytes() == b"same-origin")
}

/// GET, HEAD and OPTIONS change nothing; everything else needs the CSRF check.
pub(crate) fn is_unsafe(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The value of cookie `name` across every `Cookie` header: `None` when absent, duplicated
/// (cookie tossing), oversized, or not URL-safe text.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut total = 0usize;
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        total = total.saturating_add(value.len());
        if total > MAX_COOKIE_BYTES {
            return None;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if key.trim() != name {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(value.trim());
        }
    }
    found.filter(|value| {
        !value.is_empty()
            && value.len() <= MAX_COOKIE_VALUE_CHARS
            && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}

/// `Set-Cookie` for a `__Host-` cookie (`Secure; HttpOnly; Path=/`, no `Domain`). `value` must
/// already be URL-safe text (all values set here are base64url or UUIDs and digits).
pub(crate) fn host_cookie(name: &str, value: &str, max_age_secs: i64, same_site: &str) -> String {
    format!("{name}={value}; Max-Age={}; Path=/; Secure; HttpOnly; SameSite={same_site}", max_age_secs.max(0))
}

/// `Set-Cookie` clearing `name`.
pub(crate) fn clear_host_cookie(name: &str, same_site: &str) -> String {
    format!("{name}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite={same_site}")
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// Who presented which credential, before any tenant is chosen.
#[derive(Clone, Debug)]
pub(crate) enum Caller {
    Browser(ResolvedSession),
    ServiceAccount(AuthenticatedServiceAccount),
    Share(ShareSession),
}

/// The tenant a request selects (header or `/tenants/{id}` path), `Err` when they disagree or the
/// header is not a tenant id. Single-tenant deployments always act in their one tenant.
pub(crate) fn selected_tenant(identity: &IdentityConfig, path: &str, headers: &HeaderMap) -> Result<Option<TenantId>, AuthRejection> {
    let from_header = match headers.get(TENANT_HEADER) {
        None => None,
        Some(value) => Some(value.to_str().ok().and_then(|value| value.trim().parse::<TenantId>().ok()).ok_or(AuthRejection::Forbidden)?),
    };
    let from_path = path_resource(path, "tenants").and_then(|segment| segment.parse::<TenantId>().ok());
    let requested = match (from_header, from_path) {
        (Some(a), Some(b)) if a != b => return Err(AuthRejection::Forbidden),
        (Some(id), _) | (None, Some(id)) => Some(id),
        (None, None) => None,
    };
    match identity.single_tenant {
        Some(single) if requested.is_some_and(|id| id != single) => Err(AuthRejection::Forbidden),
        Some(single) => Ok(Some(single)),
        None => Ok(requested),
    }
}

/// The segment after the first `/{collection}/` in `path`, if any.
fn path_resource<'a>(path: &'a str, collection: &str) -> Option<&'a str> {
    let mut segments = path.split('/').take(32);
    while let Some(segment) = segments.next() {
        if segment == collection {
            return segments.next().filter(|next| !next.is_empty());
        }
    }
    None
}

/// The tenant context for an active member, or `Forbidden` for anyone else.
fn member_context(
    identity: &IdentityConfig,
    tenant: TenantId,
    principal: PrincipalId,
    access: Option<TenantAccess>,
) -> Result<TenantContext, AuthRejection> {
    let access = access.ok_or(AuthRejection::Forbidden)?;
    let role = access.tenant_role.ok_or(AuthRejection::Forbidden)?;
    let permissions = access.permissions.iter().map(|permission| match permission {
        MemberPermission::Billing => Permission::ManageBilling,
        MemberPermission::ProviderKeys => Permission::ManageProviderKeys,
        MemberPermission::Support => Permission::UseSupportAccess,
        MemberPermission::Export => Permission::ExportAudit,
        MemberPermission::Deletion => Permission::DeleteTenant,
    });
    Ok(TenantContext::new(tenant, principal, identity.deployment, role, permissions))
}

/// The production authenticator: session cookie, service-account bearer, share session.
#[derive(Clone)]
pub(crate) struct CredentialAuthenticator {
    pool: PgPool,
    identity: Arc<IdentityConfig>,
}

impl CredentialAuthenticator {
    pub(crate) const fn new(pool: PgPool, identity: Arc<IdentityConfig>) -> Self {
        Self { pool, identity }
    }

    /// Resolve the credential on a request (see the module docs for the order and CSRF rule).
    pub(crate) async fn identify(&self, method: &Method, headers: &HeaderMap) -> Result<Caller, AuthRejection> {
        if let Some(token) = cookie(headers, COOKIE_NAME) {
            match sessions::resolve_session(&self.pool, token, self.identity.limits()).await {
                Ok(session) => {
                    if is_unsafe(method) && !self.identity.same_origin(headers) {
                        return Err(AuthRejection::Forbidden);
                    }
                    return Ok(Caller::Browser(session));
                }
                Err(SessionError::Database) => return Err(AuthRejection::Unavailable),
                Err(_) => {}
            }
        }
        if let Some(authorization) = headers.get(header::AUTHORIZATION) {
            let secret = authorization
                .to_str()
                .ok()
                .filter(|value| value.len() <= MAX_AUTHORIZATION_CHARS)
                .and_then(|value| value.strip_prefix("Bearer "))
                .filter(|secret| secret.starts_with(SECRET_PREFIX))
                .ok_or(AuthRejection::Unauthenticated)?;
            return match service_accounts::authenticate_credential(&self.pool, secret).await {
                Ok(account) => Ok(Caller::ServiceAccount(account)),
                Err(ServiceAccountError::Database) => Err(AuthRejection::Unavailable),
                Err(_) => Err(AuthRejection::Unauthenticated),
            };
        }
        if let Some(share) = cookie(headers, SHARE_COOKIE).and_then(|value| self.identity.open_share_cookie(value))
            && share.expires_at > now_unix()
        {
            let dead =
                share_links::dead_share_links(&self.pool, &[(share.tenant_id, share.link_id)]).await.map_err(|_| AuthRejection::Unavailable)?;
            if dead.is_empty() {
                if is_unsafe(method) && !self.identity.same_origin(headers) {
                    return Err(AuthRejection::Forbidden);
                }
                return Ok(Caller::Share(share));
            }
        }
        Err(AuthRejection::Unauthenticated)
    }

    /// The tenant context of an active member of `tenant`.
    pub(crate) async fn member(&self, tenant: TenantId, principal: PrincipalId) -> Result<TenantContext, AuthRejection> {
        let mut transaction = begin_tenant_transaction(&self.pool, tenant).await.map_err(|_| AuthRejection::Unavailable)?;
        let access = resolve_tenant_access(&mut transaction, principal).await.map_err(|_| AuthRejection::Unavailable)?;
        let _ = transaction.rollback().await;
        member_context(&self.identity, tenant, principal, access)
    }

    async fn authenticated(&self, request: &AuthRequest<'_>) -> Result<Authenticated, AuthRejection> {
        let caller = self.identify(request.method, request.headers).await?;
        let selected = selected_tenant(&self.identity, request.path, request.headers)?;
        match caller {
            Caller::Browser(session) => {
                let tenant = selected.ok_or(AuthRejection::Forbidden)?;
                let context = self.member(tenant, session.principal_id).await?;
                Ok(Authenticated {
                    principal: session.principal_id,
                    tenant: context,
                    session: SessionInfo { kind: CredentialKind::BrowserSession, id: session.id.to_string() },
                    grant: Grant::Member,
                    mutating: false,
                })
            }
            Caller::ServiceAccount(account) => {
                if selected.is_some_and(|tenant| tenant != account.tenant_id) {
                    return Err(AuthRejection::Forbidden);
                }
                Ok(Authenticated {
                    principal: account.principal_id,
                    tenant: TenantContext::new(account.tenant_id, account.principal_id, self.identity.deployment, Role::Viewer, []),
                    session: SessionInfo { kind: CredentialKind::ServiceAccount, id: account.credential_id.to_string() },
                    grant: Grant::ServiceAccount { scopes: account.scopes },
                    mutating: false,
                })
            }
            Caller::Share(share) => {
                if selected.is_some_and(|tenant| tenant != share.tenant_id) {
                    return Err(AuthRejection::Forbidden);
                }
                let own = share.document_id.to_string();
                if path_resource(request.path, "documents") != Some(own.as_str()) {
                    return Err(AuthRejection::Forbidden);
                }
                // Attribution only: actions through the link are credited to its creator.
                let creator = share.created_by;
                let role = match share.role {
                    ShareRole::Editor => Role::Editor,
                    ShareRole::Viewer => Role::Viewer,
                };
                Ok(Authenticated {
                    principal: creator,
                    tenant: TenantContext::new(share.tenant_id, creator, self.identity.deployment, role, []),
                    session: SessionInfo { kind: CredentialKind::ShareLink, id: share.link_id.to_string() },
                    grant: Grant::Share { document_id: share.document_id, role: share.role, expires_at: share.expires_at },
                    mutating: false,
                })
            }
        }
    }
}

impl Authenticator for CredentialAuthenticator {
    fn authenticate<'a>(&'a self, request: &'a AuthRequest<'a>) -> AuthFuture<'a> {
        Box::pin(self.authenticated(request))
    }
}

/// S4-023: the one path every mutating handler takes to hold the caller's access inside the
/// transaction that performs its mutation (`tenants_api::open`/`open_suspended`/`create_tenant`
/// and `documents_api::begin` call it right after opening the tenant transaction, before their
/// authorization reads and writes). It re-reads, `FOR SHARE`, the principal, the session or
/// credential row and (for `membership` = active member) the caller's `tenant_members` row
/// (`access_guard::hold_caller_access`), so a revocation that committed after the request was
/// authenticated refuses it (`401` for a dead credential, `403` for a lost membership), and one
/// that has not committed yet waits for this transaction to end. A concurrent revocation that
/// deadlocks against the request aborts one of them (`503` here); neither half-commits.
///
/// Reads (GET/HEAD/OPTIONS) are not held: they change nothing, and holding share locks on the
/// session row for every read would make sliding-expiry touches wait. Only the production
/// credential authenticator (installed with an identity configuration) is held: the test
/// authenticators carry synthetic credential ids that have no rows.
pub(crate) async fn hold_caller_access(
    state: &AppState,
    transaction: &mut TenantTransaction<'_>,
    caller: &Authenticated,
    membership: MembershipRequirement,
) -> Result<(), ApiError> {
    if !caller.mutating || state.identity().is_none() {
        return Ok(());
    }
    let id: Uuid = caller.session.id.parse().map_err(|_| ApiError::unauthenticated())?;
    #[cfg(test)]
    guard_pause::reached(&caller.session.id).await;
    let (credential, membership) = match caller.session.kind {
        CredentialKind::BrowserSession => (HeldCredential::BrowserSession(id), membership),
        // Service principals and share sessions are not tenant members: their credential row is
        // what binds them to the tenant.
        CredentialKind::ServiceAccount => (HeldCredential::ServiceAccountCredential(id), MembershipRequirement::None),
        CredentialKind::ShareLink => (HeldCredential::ShareLink(id), MembershipRequirement::None),
    };
    access_guard::hold_caller_access(transaction, caller.principal, credential, membership).await.map_err(|error| match error {
        AccessGuardError::Revoked => ApiError::unauthenticated(),
        AccessGuardError::NotMember => ApiError::forbidden(),
        AccessGuardError::Database => ApiError::unavailable(),
    })
}

/// Test-only pause point just before the guard's reads (`revocation_race_tests`): a test arms it
/// for one credential id, the request stops there with its transaction open, the test commits a
/// revocation, then releases the request. Requests with any other credential id never stop.
#[cfg(test)]
pub(crate) mod guard_pause {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use tokio::sync::Notify;

    /// `reached` is signalled when the request arrives; it then waits for `release`.
    #[derive(Clone, Default)]
    pub(crate) struct Pause {
        pub(crate) reached: Arc<Notify>,
        pub(crate) release: Arc<Notify>,
    }

    fn armed() -> &'static Mutex<HashMap<String, Pause>> {
        static ARMED: OnceLock<Mutex<HashMap<String, Pause>>> = OnceLock::new();
        ARMED.get_or_init(Mutex::default)
    }

    /// Stop the next mutating request authenticated with `credential_id` before its guard.
    pub(crate) fn arm(credential_id: &str) -> Pause {
        let pause = Pause::default();
        if let Ok(mut armed) = armed().lock() {
            armed.insert(credential_id.to_string(), pause.clone());
        }
        pause
    }

    pub(super) async fn reached(credential_id: &str) {
        let pause = armed().lock().ok().and_then(|mut armed| armed.remove(credential_id));
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
    }
}

/// A signed-in principal with no tenant chosen (e.g. listing the caller's tenants): a browser
/// session or a service account. Share sessions are refused (`403`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SignedIn {
    pub(crate) principal: PrincipalId,
    pub(crate) session: SessionInfo,
}

impl FromRequestParts<AppState> for SignedIn {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let authenticator = state.credential_authenticator().ok_or(ApiError::unauthenticated())?;
        match authenticator.identify(&parts.method, &parts.headers).await.map_err(ApiError::from)? {
            Caller::Browser(session) => Ok(Self {
                principal: session.principal_id,
                session: SessionInfo { kind: CredentialKind::BrowserSession, id: session.id.to_string() },
            }),
            Caller::ServiceAccount(account) => Ok(Self {
                principal: account.principal_id,
                session: SessionInfo { kind: CredentialKind::ServiceAccount, id: account.credential_id.to_string() },
            }),
            Caller::Share(_) => Err(ApiError::forbidden()),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn identity(deployment: DeploymentMode) -> IdentityConfig {
        let tenant = "d1000000-0000-4000-8000-000000000001".parse().ok();
        IdentityConfig::new(deployment, tenant, Some("https://Design.Example.com:443/app"), Some(b"0123456789abcdef0123")).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn origins_are_normalized_and_non_http_urls_rejected() {
        assert_eq!(origin_of("https://Design.Example.com:443/app").as_deref(), Some("https://design.example.com"));
        assert_eq!(origin_of("http://127.0.0.1:8080").as_deref(), Some("http://127.0.0.1:8080"));
        for bad in ["ftp://x", "https://", "https://user@host", "design.example.com", ""] {
            assert_eq!(origin_of(bad), None, "{bad}");
        }
        assert!(IdentityConfig::new(DeploymentMode::MultiTenant, None, Some("javascript:alert(1)"), None).is_err());
        assert!(IdentityConfig::new(DeploymentMode::SingleTenant, None, None, None).is_err());
        assert!(IdentityConfig::new(DeploymentMode::MultiTenant, None, None, Some(b"short")).is_err());
    }

    #[test]
    fn csrf_requires_our_origin_or_same_origin_fetch_metadata() {
        let identity = identity(DeploymentMode::MultiTenant);
        assert!(identity.same_origin(&headers(&[("origin", "https://design.example.com")])));
        assert!(identity.same_origin(&headers(&[("sec-fetch-site", "same-origin")])));
        for refused in [
            headers(&[("origin", "https://evil.example")]),
            headers(&[("origin", "null")]),
            headers(&[("origin", "https://evil.example"), ("sec-fetch-site", "same-origin")]),
            headers(&[("sec-fetch-site", "cross-site")]),
            headers(&[("sec-fetch-site", "same-site")]),
            headers(&[]),
        ] {
            assert!(!identity.same_origin(&refused), "{refused:?}");
        }
        let unconfigured = IdentityConfig::new(DeploymentMode::MultiTenant, None, None, None).unwrap();
        assert!(!unconfigured.same_origin(&headers(&[("origin", "https://design.example.com")])));
        assert!(is_unsafe(&Method::POST) && is_unsafe(&Method::DELETE) && !is_unsafe(&Method::GET) && !is_unsafe(&Method::HEAD));
    }

    #[test]
    fn cookies_are_found_once_across_headers_and_duplicates_refused() {
        let map = headers(&[("cookie", "a=1; __Host-figcraft_share=abc.def"), ("cookie", "b=2")]);
        assert_eq!(cookie(&map, SHARE_COOKIE), Some("abc.def"));
        let tossed = headers(&[("cookie", "__Host-figcraft_share=abc"), ("cookie", "__Host-figcraft_share=xyz")]);
        assert_eq!(cookie(&tossed, SHARE_COOKIE), None);
        assert_eq!(cookie(&headers(&[("cookie", "__Host-figcraft_share=a\"b")]), SHARE_COOKIE), None);
        assert_eq!(cookie(&headers(&[("cookie", "__Host-figcraft_share=")]), SHARE_COOKIE), None);
        let huge = format!("x={}", "y".repeat(MAX_COOKIE_BYTES));
        assert_eq!(cookie(&headers(&[("cookie", &huge), ("cookie", "__Host-figcraft_share=abc")]), SHARE_COOKIE), None);
    }

    #[test]
    fn share_cookies_round_trip_and_any_tampering_is_refused() {
        let identity = identity(DeploymentMode::MultiTenant);
        let grant = ShareGrant {
            link_id: "d1000000-0000-4000-8000-0000000000aa".parse().unwrap(),
            tenant_id: "d1000000-0000-4000-8000-0000000000bb".parse().unwrap(),
            document_id: "d1000000-0000-4000-8000-0000000000cc".parse().unwrap(),
            role: ShareRole::Viewer,
            created_by: "d1000000-0000-4000-8000-0000000000dd".parse().unwrap(),
            link_expires_at: None,
            session_expires_at: 1_900_000_000,
        };
        let value = identity.share_cookie_value(&grant);
        assert!(value.len() <= MAX_COOKIE_VALUE_CHARS);
        let opened = identity.open_share_cookie(&value).unwrap();
        assert_eq!(
            (opened.link_id, opened.tenant_id, opened.document_id, opened.role, opened.expires_at),
            (grant.link_id, grant.tenant_id, grant.document_id, grant.role, 1_900_000_000)
        );
        // Upgrading the role, extending the expiry or swapping the document breaks the MAC.
        for forged in
            [value.replace(".viewer.", ".editor."), value.replace("1900000000", "1990000000"), value.replace("0000000000cc", "0000000000ee")]
        {
            assert_ne!(forged, value);
            assert_eq!(identity.open_share_cookie(&forged), None);
        }
        let other = IdentityConfig::new(DeploymentMode::MultiTenant, None, None, Some(b"another-signing-key")).unwrap();
        assert_eq!(other.open_share_cookie(&value), None);
        for junk in ["", ".", "v1", "a.b.c.d.e.f.g", &"x".repeat(10_000)] {
            assert_eq!(identity.open_share_cookie(junk), None);
        }
    }

    #[test]
    fn tenant_selection_agrees_or_refuses() {
        let multi = identity(DeploymentMode::MultiTenant);
        let a = "d1000000-0000-4000-8000-00000000000a";
        let b = "d1000000-0000-4000-8000-00000000000b";
        assert_eq!(selected_tenant(&multi, "/documents/x", &headers(&[])), Ok(None));
        assert_eq!(selected_tenant(&multi, "/x", &headers(&[(TENANT_HEADER, a)])).map(|t| t.map(|t| t.to_string())), Ok(Some(a.to_string())));
        assert_eq!(selected_tenant(&multi, &format!("/tenants/{a}/teams"), &headers(&[])).map(|t| t.map(|t| t.to_string())), Ok(Some(a.to_string())));
        assert_eq!(selected_tenant(&multi, &format!("/tenants/{a}"), &headers(&[(TENANT_HEADER, b)])), Err(AuthRejection::Forbidden));
        assert_eq!(selected_tenant(&multi, "/x", &headers(&[(TENANT_HEADER, "not-a-tenant")])), Err(AuthRejection::Forbidden));
        let single = identity(DeploymentMode::SingleTenant);
        assert_eq!(
            selected_tenant(&single, "/x", &headers(&[])).map(|t| t.map(|t| t.to_string())),
            Ok(Some("d1000000-0000-4000-8000-000000000001".to_string()))
        );
        assert_eq!(selected_tenant(&single, "/x", &headers(&[(TENANT_HEADER, a)])), Err(AuthRejection::Forbidden));
        assert_eq!(path_resource("/documents/abc/versions", "documents"), Some("abc"));
        assert_eq!(path_resource("/documents/", "documents"), None);
    }
}
