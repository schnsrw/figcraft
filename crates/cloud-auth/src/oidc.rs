//! OpenID Connect login (S4-001): discovery, the authorization-code flow with PKCE S256, state and
//! nonce, ID-token validation and the `(issuer, subject)` → principal mapping.
//!
//! * [`OidcProvider::discover`] fetches the issuer's discovery document and JWKS through
//!   [`OidcHttpClient`] (HTTPS only; plain HTTP to loopback only when the configuration allows it,
//!   for tests). The document's issuer must equal the configured one, and its authorization,
//!   token and JWKS endpoints must pass the same policy.
//! * [`OidcProvider::begin_login`] makes a fresh 256-bit state, nonce and PKCE verifier, stores the
//!   state's SHA-256 (with the nonce, verifier, issuer and a validated relative `return_to`) for
//!   [`LOGIN_ATTEMPT_LIFETIME_SECS`], and returns the provider authorization URL plus the state as
//!   the value for a short-lived, `HttpOnly`, `Secure`, `SameSite=Lax` cookie.
//! * [`OidcProvider::complete_login`] requires the callback's `state` to equal the cookie
//!   (constant time; this binds the flow to the browser that started it), consumes the stored
//!   attempt exactly once, redeems the code with the PKCE verifier, validates the ID token
//!   (signature with an RS256/ES256 allowlist — never `none` or HS*, issuer, audience, nonce,
//!   `exp`/`iat`/`nbf` with [`OidcConfig::clock_skew`]), and resolves the principal, creating it on
//!   first login.
//! * An ID token signed with a key id the cached JWKS does not hold triggers one re-discovery
//!   (new keys) and a retry, at most once per [`OidcConfig::key_refresh_min_interval`].
//!
//! * The ID token's `groups` claim is kept, bounded ([`MAX_GROUPS`], [`MAX_GROUP_CHARS`] and the
//!   stored-metadata budget), in `identity_links.verified_metadata.groups` for the later directory
//!   binding work (S4-024 D5/D6). Nothing is granted from it.
//! * [`OidcProvider::backchannel_logout`] (S4-025) implements OpenID Connect Back-Channel Logout
//!   1.0: the logout token is a JWT signed with a key from the same JWKS (RS256/ES256 only, the
//!   same refresh-once rule), `iss` is the issuer, every `aud` is our client id, `events` holds
//!   [`BACKCHANNEL_LOGOUT_EVENT`], `iat` is within [`LOGOUT_TOKEN_MAX_AGE`] (plus skew), there is
//!   no `nonce`, a `jti` is present and has not been seen ([`MAX_REMEMBERED_LOGOUT_TOKENS`],
//!   in memory per process until the token could no longer be fresh), and `sub` is present (a
//!   `sid`-only token is refused: sessions are not tied to provider sessions, so FigCraft needs
//!   the subject). Every session of the principal linked to `(issuer, sub)` is revoked with reason
//!   `logout`; whether any existed is never revealed.
//!
//! Every failure is a fixed [`OidcError`] category; no token, code, state, nonce or verifier is
//! ever included in an error or `Debug` output.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use figcraft_cloud_domain::{ErrorCode, PrincipalId};
use figcraft_cloud_storage::oidc_logins::{self, NewLoginAttempt, OidcLoginError};
use figcraft_cloud_storage::sessions::{self, RevokeReason};
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreIdToken, CoreIdTokenClaims, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJsonWebKeyUse,
    CoreJwsSigningAlgorithm, CoreProviderMetadata,
};
use openidconnect::{
    AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret, CsrfToken, IssuerUrl, JsonWebKey as _, JsonWebKeyAlgorithm, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, SignatureVerificationError, TokenResponse as _,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::{Mutex, RwLock};

pub use crate::oidc_http::{OidcHttpClient, OidcHttpError, endpoint_allowed};

/// How long a started login may take before its state expires.
pub const LOGIN_ATTEMPT_LIFETIME_SECS: u64 = 10 * 60;
/// Random bytes in each state, nonce and PKCE verifier.
pub const SECRET_BYTES: usize = 32;
/// Characters in an encoded state (URL-safe base64 of 32 bytes, no padding).
pub const STATE_CHARS: usize = 43;
/// Longest authorization code accepted from a callback.
pub const MAX_CODE_CHARS: usize = 2048;
/// Longest `return_to` accepted.
pub const MAX_RETURN_TO_CHARS: usize = 2048;
/// Longest raw ID token accepted.
pub const MAX_ID_TOKEN_BYTES: usize = 16 * 1024;
/// Most extra scopes a configuration may request.
pub const MAX_EXTRA_SCOPES: usize = 16;
/// Default tolerance for clock differences with the provider.
pub const DEFAULT_CLOCK_SKEW: Duration = Duration::from_secs(60);
/// Default minimum time between key refreshes triggered by unknown key ids.
pub const DEFAULT_KEY_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Largest clock skew a configuration may set.
pub const MAX_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);

/// Most groups kept from an ID token's `groups` claim.
pub const MAX_GROUPS: usize = 32;
/// Longest group name kept (longer names are dropped, never truncated into another name).
pub const MAX_GROUP_CHARS: usize = 256;
/// The `events` member a logout token must carry.
pub const BACKCHANNEL_LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";
/// Longest raw logout token accepted.
pub const MAX_LOGOUT_TOKEN_BYTES: usize = 16 * 1024;
/// Longest back-channel logout request body accepted.
pub const MAX_LOGOUT_FORM_BYTES: usize = 32 * 1024;
/// How old (by `iat`) a logout token may be, before clock skew.
pub const LOGOUT_TOKEN_MAX_AGE: Duration = Duration::from_secs(2 * 60);
/// Most logout-token ids remembered for replay protection (per process).
pub const MAX_REMEMBERED_LOGOUT_TOKENS: usize = 10_000;

const MAX_JTI_CHARS: usize = 256;
const MAX_SID_CHARS: usize = 256;
const MAX_AUDIENCES: usize = 8;
const MAX_CANDIDATE_KEYS: usize = 8;
const MAX_EMAIL_CHARS: usize = 320;
const MAX_NAME_CHARS: usize = 255;
const FALLBACK_DISPLAY_NAME: &str = "FigCraft user";

/// Fixed, non-leaking categories for OIDC failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OidcError {
    /// The configuration is unusable (URL policy, empty client id, bounds).
    InvalidConfig,
    /// Discovery or key fetching failed, or the provider metadata is not acceptable.
    Discovery,
    /// No secure random numbers are available.
    Random,
    /// `return_to` is not an allowed relative path.
    InvalidReturnTo,
    /// The callback does not continue a live login: missing or mismatched state cookie, unknown,
    /// expired or replayed state. Deliberately one category.
    Refused,
    /// The provider did not redeem the code.
    TokenExchange,
    /// The ID token is missing or fails validation.
    InvalidIdToken,
    /// The identity belongs to a principal that is not active.
    PrincipalInactive,
    /// A back-channel logout token is malformed, fails validation, was already used, or names
    /// no subject. Deliberately one category.
    InvalidLogoutToken,
    /// The metadata store failed.
    Storage,
}

impl OidcError {
    /// The public error code for an HTTP response.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::InvalidReturnTo | Self::InvalidLogoutToken => ErrorCode::InvalidRequest,
            Self::Refused | Self::TokenExchange | Self::InvalidIdToken => ErrorCode::Unauthenticated,
            Self::PrincipalInactive => ErrorCode::Forbidden,
            Self::Discovery => ErrorCode::TemporarilyUnavailable,
            Self::InvalidConfig | Self::Random | Self::Storage => ErrorCode::Internal,
        }
    }
}

impl std::fmt::Display for OidcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "oidc error: {self:?}")
    }
}

impl std::error::Error for OidcError {}

/// One OIDC provider registration.
#[derive(Clone)]
pub struct OidcConfig {
    /// The issuer URL exactly as the provider states it in discovery.
    pub issuer_url: String,
    pub client_id: String,
    /// The confidential client's secret; `None` for a public client (PKCE only).
    pub client_secret: Option<String>,
    /// The callback URL registered with the provider.
    pub redirect_url: String,
    /// Scopes beyond `openid` (for example `email`, `profile`).
    pub extra_scopes: Vec<String>,
    /// Allow plain HTTP to loopback hosts (tests only; production requires HTTPS).
    pub allow_insecure_loopback: bool,
    pub clock_skew: Duration,
    pub key_refresh_min_interval: Duration,
}

impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcConfig")
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "[REDACTED]"))
            .field("redirect_url", &self.redirect_url)
            .field("extra_scopes", &self.extra_scopes)
            .field("allow_insecure_loopback", &self.allow_insecure_loopback)
            .field("clock_skew", &self.clock_skew)
            .field("key_refresh_min_interval", &self.key_refresh_min_interval)
            .finish()
    }
}

impl OidcConfig {
    /// A production configuration with the default skew and refresh interval and the `email`
    /// and `profile` scopes.
    #[must_use]
    pub fn new(issuer_url: &str, client_id: &str, client_secret: Option<&str>, redirect_url: &str) -> Self {
        Self {
            issuer_url: issuer_url.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.map(str::to_owned),
            redirect_url: redirect_url.to_owned(),
            extra_scopes: vec!["email".to_owned(), "profile".to_owned()],
            allow_insecure_loopback: false,
            clock_skew: DEFAULT_CLOCK_SKEW,
            key_refresh_min_interval: DEFAULT_KEY_REFRESH_MIN_INTERVAL,
        }
    }

    fn validate(&self) -> Result<(), OidcError> {
        let url_ok = |value: &str| {
            value.len() <= oidc_logins::MAX_ISSUER_CHARS
                && url::Url::parse(value).is_ok_and(|url| endpoint_allowed(&url, self.allow_insecure_loopback) && url.query().is_none())
        };
        let scope_ok =
            |scope: &String| !scope.is_empty() && scope.len() <= 64 && scope.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
        if !url_ok(&self.issuer_url)
            || !url_ok(&self.redirect_url)
            || self.client_id.is_empty()
            || self.client_id.len() > 512
            || self.client_secret.as_ref().is_some_and(|secret| secret.is_empty() || secret.len() > 1024)
            || self.extra_scopes.len() > MAX_EXTRA_SCOPES
            || !self.extra_scopes.iter().all(scope_ok)
            || self.clock_skew > MAX_CLOCK_SKEW
        {
            return Err(OidcError::InvalidConfig);
        }
        Ok(())
    }
}

/// What starting a login hands to the HTTP layer.
#[derive(Clone, Eq, PartialEq)]
pub struct BeginLogin {
    /// Where to redirect the browser.
    pub authorization_url: String,
    /// The value for the state cookie (same as the `state` query parameter the provider echoes).
    pub state_cookie: String,
}

impl std::fmt::Debug for BeginLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BeginLogin").field("authorization_url", &"[REDACTED]").field("state_cookie", &"[REDACTED]").finish()
    }
}

/// A completed login.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoginOutcome {
    pub principal_id: PrincipalId,
    /// True when this login created the principal.
    pub new_principal: bool,
    /// The validated relative path to continue to, if the login started with one.
    pub return_to: Option<String>,
}

/// The validated identity an ID token asserts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedIdentity {
    pub issuer: String,
    pub subject: String,
    pub display_name: String,
    /// Bounded verified claims (`email`, `email_verified`, `name`, `preferred_username`, and
    /// `groups` with `groups_truncated` when some did not fit).
    pub metadata: serde_json::Value,
}

/// The validated content of a back-channel logout token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogoutClaims {
    pub issuer: String,
    pub subject: Option<String>,
    /// The provider session id (`sid`), when the token names one.
    pub session_id: Option<String>,
    replay_key: [u8; 32],
    /// Unix time after which the token can no longer pass the `iat` check.
    replay_until: i64,
}

/// Logout-token ids seen recently (bounded; entries expire with the token's freshness window).
#[derive(Debug, Default)]
struct ReplayCache {
    seen: HashMap<[u8; 32], i64>,
}

impl ReplayCache {
    /// Record `key` until `until`; refused when it is already recorded or the cache is full of
    /// live entries (fail closed: the provider retries).
    fn insert(&mut self, key: [u8; 32], until: i64, now: i64) -> Result<(), OidcError> {
        if self.seen.get(&key).is_some_and(|expiry| *expiry >= now) {
            return Err(OidcError::InvalidLogoutToken);
        }
        if self.seen.len() >= MAX_REMEMBERED_LOGOUT_TOKENS {
            self.seen.retain(|_, expiry| *expiry >= now);
        }
        if self.seen.len() >= MAX_REMEMBERED_LOGOUT_TOKENS {
            return Err(OidcError::InvalidLogoutToken);
        }
        self.seen.insert(key, until);
        Ok(())
    }

    fn remove(&mut self, key: &[u8; 32]) {
        self.seen.remove(key);
    }
}

/// A discovered provider, ready to start and complete logins. Share one per issuer.
pub struct OidcProvider {
    config: OidcConfig,
    issuer: IssuerUrl,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    redirect_url: RedirectUrl,
    http: OidcHttpClient,
    metadata: RwLock<CoreProviderMetadata>,
    last_key_refresh: Mutex<Option<Instant>>,
    logout_tokens: Mutex<ReplayCache>,
}

impl std::fmt::Debug for OidcProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OidcProvider").field("config", &self.config).finish_non_exhaustive()
    }
}

/// True when `return_to` is an allowed continuation: a same-origin absolute path (`/…`), not a
/// protocol-relative (`//`) or backslash form, printable ASCII only, at most
/// [`MAX_RETURN_TO_CHARS`].
#[must_use]
pub fn return_to_allowed(return_to: &str) -> bool {
    let bytes = return_to.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_RETURN_TO_CHARS || bytes.first() != Some(&b'/') {
        return false;
    }
    if matches!(bytes.get(1), Some(b'/' | b'\\')) || !bytes.iter().all(|b| b.is_ascii_graphic()) || bytes.contains(&b'\\') {
        return false;
    }
    // Resolving against a placeholder origin must stay on that origin.
    url::Url::parse("https://figcraft.invalid/")
        .and_then(|base| base.join(return_to))
        .is_ok_and(|joined| joined.host_str() == Some("figcraft.invalid") && joined.scheme() == "https")
}

fn random_secret() -> Result<String, OidcError> {
    let mut bytes = [0u8; SECRET_BYTES];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut bytes).map_err(|_| OidcError::Random)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn state_well_formed(state: &str) -> bool {
    state.len() == STATE_CHARS && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn state_hash(state: &str) -> [u8; 32] {
    Sha256::digest(state.as_bytes()).into()
}

/// Constant-time equality for equal-length secrets (length is not secret).
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn clean_claim(value: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = value.chars().filter(|c| !c.is_control()).take(max_chars).collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn chrono_duration(duration: Duration) -> chrono::TimeDelta {
    chrono::TimeDelta::from_std(duration).unwrap_or(chrono::TimeDelta::zero())
}

fn storage_error(error: OidcLoginError) -> OidcError {
    match error {
        OidcLoginError::Refused => OidcError::Refused,
        OidcLoginError::PrincipalInactive => OidcError::PrincipalInactive,
        OidcLoginError::InvalidInput => OidcError::InvalidIdToken,
        OidcLoginError::Constraint | OidcLoginError::Database => OidcError::Storage,
    }
}

impl OidcProvider {
    /// Validate `config` and discover the provider (metadata and keys).
    pub async fn discover(config: OidcConfig) -> Result<Self, OidcError> {
        config.validate()?;
        let http = OidcHttpClient::new(config.allow_insecure_loopback).map_err(|_| OidcError::InvalidConfig)?;
        let issuer = IssuerUrl::new(config.issuer_url.clone()).map_err(|_| OidcError::InvalidConfig)?;
        let redirect_url = RedirectUrl::new(config.redirect_url.clone()).map_err(|_| OidcError::InvalidConfig)?;
        let metadata = Self::fetch_metadata(&issuer, &http, config.allow_insecure_loopback).await?;
        Ok(Self {
            client_id: ClientId::new(config.client_id.clone()),
            client_secret: config.client_secret.clone().map(ClientSecret::new),
            config,
            issuer,
            redirect_url,
            http,
            metadata: RwLock::new(metadata),
            last_key_refresh: Mutex::new(None),
            logout_tokens: Mutex::new(ReplayCache::default()),
        })
    }

    async fn fetch_metadata(issuer: &IssuerUrl, http: &OidcHttpClient, allow_insecure_loopback: bool) -> Result<CoreProviderMetadata, OidcError> {
        let metadata = CoreProviderMetadata::discover_async(issuer.clone(), http).await.map_err(|_| OidcError::Discovery)?;
        let allowed = |url: &url::Url| endpoint_allowed(url, allow_insecure_loopback);
        let token_ok = metadata.token_endpoint().is_some_and(|token| allowed(token.url()));
        if !allowed(metadata.authorization_endpoint().url()) || !token_ok || !allowed(metadata.jwks_uri().url()) {
            return Err(OidcError::Discovery);
        }
        Ok(metadata)
    }

    /// The configured issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        self.issuer.as_str()
    }

    /// Start a login: store the attempt and return the authorization URL and state-cookie value.
    pub async fn begin_login(&self, pool: &PgPool, return_to: Option<&str>) -> Result<BeginLogin, OidcError> {
        if return_to.is_some_and(|path| !return_to_allowed(path)) {
            return Err(OidcError::InvalidReturnTo);
        }
        let state = random_secret()?;
        let nonce = random_secret()?;
        let verifier = random_secret()?;
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(verifier.clone()));
        let metadata = self.metadata.read().await.clone();
        let client = CoreClient::from_provider_metadata(metadata, self.client_id.clone(), self.client_secret.clone())
            .set_redirect_uri(self.redirect_url.clone());
        let state_for_url = state.clone();
        let nonce_for_url = nonce.clone();
        let mut request = client
            .authorize_url(CoreAuthenticationFlow::AuthorizationCode, move || CsrfToken::new(state_for_url), move || Nonce::new(nonce_for_url))
            .set_pkce_challenge(challenge);
        for scope in &self.config.extra_scopes {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        let (url, _, _) = request.url();
        oidc_logins::insert_login_attempt(
            pool,
            &NewLoginAttempt {
                state_hash: state_hash(&state),
                issuer: self.issuer.as_str(),
                nonce: &nonce,
                pkce_verifier: &verifier,
                return_to,
                lifetime_secs: LOGIN_ATTEMPT_LIFETIME_SECS,
            },
        )
        .await
        .map_err(storage_error)?;
        Ok(BeginLogin { authorization_url: url.to_string(), state_cookie: state })
    }

    /// Complete a login from the callback's `code` and `state` query parameters and the state
    /// cookie set by [`Self::begin_login`].
    pub async fn complete_login(&self, pool: &PgPool, code: &str, state: &str, state_cookie: Option<&str>) -> Result<LoginOutcome, OidcError> {
        if !state_well_formed(state) || !state_cookie.is_some_and(|cookie| constant_time_eq(cookie.as_bytes(), state.as_bytes())) {
            return Err(OidcError::Refused);
        }
        if code.is_empty() || code.len() > MAX_CODE_CHARS {
            return Err(OidcError::Refused);
        }
        let attempt = oidc_logins::consume_login_attempt(pool, &state_hash(state), self.issuer.as_str()).await.map_err(storage_error)?;
        let id_token = self.redeem_code(code, &attempt.pkce_verifier).await?;
        let identity = self.verify_id_token(&id_token, &attempt.nonce).await?;
        let resolved = oidc_logins::resolve_identity(pool, &identity.issuer, &identity.subject, &identity.display_name, &identity.metadata)
            .await
            .map_err(storage_error)?;
        Ok(LoginOutcome { principal_id: resolved.principal_id, new_principal: resolved.new_principal, return_to: attempt.return_to })
    }

    async fn redeem_code(&self, code: &str, verifier: &str) -> Result<CoreIdToken, OidcError> {
        let metadata = self.metadata.read().await.clone();
        let client = CoreClient::from_provider_metadata(metadata, self.client_id.clone(), self.client_secret.clone())
            .set_redirect_uri(self.redirect_url.clone());
        let response = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| OidcError::TokenExchange)?
            .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_owned()))
            .request_async(&self.http)
            .await
            .map_err(|_| OidcError::TokenExchange)?;
        response.id_token().cloned().ok_or(OidcError::InvalidIdToken)
    }

    fn verifier(&self, keys: CoreJsonWebKeySet) -> CoreIdTokenVerifier<'static> {
        let skew = chrono_duration(self.config.clock_skew);
        let max_age = chrono_duration(Duration::from_secs(LOGIN_ATTEMPT_LIFETIME_SECS)) + skew;
        // A public-client verifier: no client secret, so no HMAC key exists even if the
        // allowlist were widened.
        CoreIdTokenVerifier::new_public_client(self.client_id.clone(), self.issuer.clone(), keys)
            .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256, CoreJwsSigningAlgorithm::EcdsaP256Sha256])
            .require_issuer_match(true)
            .require_audience_match(true)
            .set_other_audience_verifier_fn(|_| false)
            // `exp` is compared with the returned time, so this accepts up to `skew` past expiry.
            .set_time_fn(move || chrono::Utc::now() - skew)
            .set_issue_time_verifier_fn(move |issued| {
                let now = chrono::Utc::now();
                if issued > now + skew {
                    Err("issued in the future".to_owned())
                } else if issued < now - max_age {
                    Err("issued too long ago".to_owned())
                } else {
                    Ok(())
                }
            })
    }

    /// Validate `id_token` against the provider's keys and `expected_nonce`, refreshing the keys
    /// once (rate-limited) when the token names a key id they do not hold.
    pub async fn verify_id_token(&self, id_token: &CoreIdToken, expected_nonce: &str) -> Result<VerifiedIdentity, OidcError> {
        let raw = id_token.to_string();
        if raw.len() > MAX_ID_TOKEN_BYTES {
            return Err(OidcError::InvalidIdToken);
        }
        let nonce = Nonce::new(expected_nonce.to_owned());
        let keys = self.metadata.read().await.jwks().clone();
        let mut identity = match id_token.claims(&self.verifier(keys), &nonce) {
            Ok(claims) => self.identity(claims),
            Err(ClaimsVerificationError::SignatureVerification(SignatureVerificationError::NoMatchingKey)) => {
                if !self.refresh_keys().await? {
                    return Err(OidcError::InvalidIdToken);
                }
                let keys = self.metadata.read().await.jwks().clone();
                let claims = id_token.claims(&self.verifier(keys), &nonce).map_err(|_| OidcError::InvalidIdToken)?;
                self.identity(claims)
            }
            Err(_) => return Err(OidcError::InvalidIdToken),
        }?;
        // `openidconnect` does not check `nbf` or expose `groups`; the signature is verified by
        // now, so read both from the payload directly.
        let payload = raw.split('.').nth(1).ok_or(OidcError::InvalidIdToken)?;
        let claims = decode_json_segment(payload).ok_or(OidcError::InvalidIdToken)?;
        self.check_not_before(&claims)?;
        add_groups(&mut identity.metadata, &claims);
        Ok(identity)
    }

    fn check_not_before(&self, claims: &serde_json::Value) -> Result<(), OidcError> {
        match claims.get("nbf") {
            None => Ok(()),
            Some(nbf) => {
                let nbf = nbf.as_f64().filter(|value| value.is_finite()).ok_or(OidcError::InvalidIdToken)?;
                #[allow(clippy::cast_precision_loss)]
                let latest = (chrono::Utc::now() + chrono_duration(self.config.clock_skew)).timestamp() as f64;
                if nbf > latest { Err(OidcError::InvalidIdToken) } else { Ok(()) }
            }
        }
    }

    fn identity(&self, claims: &CoreIdTokenClaims) -> Result<VerifiedIdentity, OidcError> {
        let subject = claims.subject().as_str();
        if subject.is_empty() || subject.chars().count() > oidc_logins::MAX_SUBJECT_CHARS {
            return Err(OidcError::InvalidIdToken);
        }
        let email = claims.email().and_then(|email| clean_claim(email.as_str(), MAX_EMAIL_CHARS));
        let name = claims.name().and_then(|name| name.get(None)).and_then(|name| clean_claim(name.as_str(), MAX_NAME_CHARS));
        let username = claims.preferred_username().and_then(|username| clean_claim(username.as_str(), MAX_NAME_CHARS));
        let display_name = name.clone().or_else(|| username.clone()).or_else(|| email.clone()).unwrap_or_else(|| FALLBACK_DISPLAY_NAME.to_owned());
        let mut metadata = serde_json::Map::new();
        if let Some(email) = email {
            metadata.insert("email".to_owned(), serde_json::Value::String(email));
        }
        if let Some(verified) = claims.email_verified() {
            metadata.insert("email_verified".to_owned(), serde_json::Value::Bool(verified));
        }
        if let Some(name) = name {
            metadata.insert("name".to_owned(), serde_json::Value::String(name));
        }
        if let Some(username) = username {
            metadata.insert("preferred_username".to_owned(), serde_json::Value::String(username));
        }
        Ok(VerifiedIdentity {
            issuer: self.issuer.as_str().to_owned(),
            subject: subject.to_owned(),
            display_name,
            metadata: serde_json::Value::Object(metadata),
        })
    }

    /// Re-discover the provider for new keys unless a refresh ran within the minimum interval.
    /// Returns whether a refresh happened.
    async fn refresh_keys(&self) -> Result<bool, OidcError> {
        let mut last = self.last_key_refresh.lock().await;
        if last.is_some_and(|at| at.elapsed() < self.config.key_refresh_min_interval) {
            return Ok(false);
        }
        *last = Some(Instant::now());
        let metadata = Self::fetch_metadata(&self.issuer, &self.http, self.config.allow_insecure_loopback).await?;
        *self.metadata.write().await = metadata;
        Ok(true)
    }

    /// Validate a back-channel logout token (signature, claims, freshness, no replay) against
    /// this provider, refreshing the keys once (rate-limited) for an unknown key id. On success
    /// the token's `jti` is remembered, so the same token is refused afterwards.
    pub async fn verify_logout_token(&self, raw: &str) -> Result<LogoutClaims, OidcError> {
        let now = chrono::Utc::now().timestamp();
        let skew = i64::try_from(self.config.clock_skew.as_secs()).unwrap_or(0);
        let keys = self.metadata.read().await.jwks().clone();
        let claims = match validate_logout_token(raw, &keys, self.issuer.as_str(), &self.config.client_id, now, skew) {
            Err(LogoutTokenCheck::NoMatchingKey) => {
                if !self.refresh_keys().await? {
                    return Err(OidcError::InvalidLogoutToken);
                }
                let keys = self.metadata.read().await.jwks().clone();
                validate_logout_token(raw, &keys, self.issuer.as_str(), &self.config.client_id, now, skew)
                    .map_err(|_| OidcError::InvalidLogoutToken)?
            }
            Err(LogoutTokenCheck::Invalid) => return Err(OidcError::InvalidLogoutToken),
            Ok(claims) => claims,
        };
        self.logout_tokens.lock().await.insert(claims.replay_key, claims.replay_until, now)?;
        Ok(claims)
    }

    /// Handle a back-channel logout request: validate `logout_token` and revoke every session of
    /// the principal linked to `(issuer, sub)` with reason `logout`. Returns how many sessions
    /// were revoked (for metrics only; the HTTP answer must not depend on it). A token without
    /// `sub` is refused (and its `jti` forgotten). When the store fails, the `jti` is forgotten
    /// again so the provider's retry is not refused as a replay.
    pub async fn backchannel_logout(&self, pool: &PgPool, logout_token: &str) -> Result<u64, OidcError> {
        let claims = self.verify_logout_token(logout_token).await?;
        let Some(subject) = claims.subject.as_deref() else {
            self.logout_tokens.lock().await.remove(&claims.replay_key);
            return Err(OidcError::InvalidLogoutToken);
        };
        match sessions::revoke_all_for_identity(pool, &claims.issuer, subject, RevokeReason::Logout).await {
            Ok(revoked) => Ok(revoked),
            Err(_) => {
                self.logout_tokens.lock().await.remove(&claims.replay_key);
                Err(OidcError::Storage)
            }
        }
    }
}

/// The `logout_token` of a back-channel logout request body
/// (`application/x-www-form-urlencoded`): `None` unless the body is at most
/// [`MAX_LOGOUT_FORM_BYTES`] and holds exactly one non-empty `logout_token` of at most
/// [`MAX_LOGOUT_TOKEN_BYTES`]. Other parameters are ignored.
#[must_use]
pub fn logout_token_from_form(body: &[u8]) -> Option<String> {
    if body.len() > MAX_LOGOUT_FORM_BYTES {
        return None;
    }
    let mut found: Option<String> = None;
    for (name, value) in url::form_urlencoded::parse(body) {
        if name == "logout_token" {
            if found.is_some() || value.is_empty() || value.len() > MAX_LOGOUT_TOKEN_BYTES {
                return None;
            }
            found = Some(value.into_owned());
        }
    }
    found
}

/// Why a logout token failed the offline checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogoutTokenCheck {
    /// The token names a key id the key set does not hold (worth one key refresh).
    NoMatchingKey,
    Invalid,
}

fn decode_json_segment(segment: &str) -> Option<serde_json::Value> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(segment).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jws_algorithm(name: &str) -> Option<CoreJwsSigningAlgorithm> {
    match name {
        "RS256" => Some(CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256),
        "ES256" => Some(CoreJwsSigningAlgorithm::EcdsaP256Sha256),
        _ => None,
    }
}

/// Verify a compact JWS against `keys`: allowlisted `alg`, no `crit`, a signing key matching
/// `kid` (or, without `kid`, any compatible signing key), at most [`MAX_CANDIDATE_KEYS`] tried.
/// Returns the decoded payload.
fn verify_jws(raw: &str, keys: &CoreJsonWebKeySet) -> Result<serde_json::Value, LogoutTokenCheck> {
    let (signing_input, signature) = raw.rsplit_once('.').ok_or(LogoutTokenCheck::Invalid)?;
    let (header, payload) = signing_input.split_once('.').ok_or(LogoutTokenCheck::Invalid)?;
    if payload.contains('.') {
        return Err(LogoutTokenCheck::Invalid);
    }
    let header = decode_json_segment(header).ok_or(LogoutTokenCheck::Invalid)?;
    if header.get("crit").is_some() {
        return Err(LogoutTokenCheck::Invalid);
    }
    let alg = header.get("alg").and_then(serde_json::Value::as_str).and_then(jws_algorithm).ok_or(LogoutTokenCheck::Invalid)?;
    let kid = match header.get("kid") {
        None => None,
        Some(serde_json::Value::String(kid)) if !kid.is_empty() && kid.len() <= 256 => Some(kid.as_str()),
        Some(_) => return Err(LogoutTokenCheck::Invalid),
    };
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(signature).map_err(|_| LogoutTokenCheck::Invalid)?;
    let candidates: Vec<_> = keys
        .keys()
        .iter()
        .filter(|key| key.key_use().is_none_or(|key_use| *key_use == CoreJsonWebKeyUse::Signature))
        .filter(|key| kid.is_none_or(|kid| key.key_id().is_some_and(|id| id.as_str() == kid)))
        .filter(|key| match key.signing_alg() {
            JsonWebKeyAlgorithm::Algorithm(key_alg) => *key_alg == alg,
            JsonWebKeyAlgorithm::Unspecified => true,
            JsonWebKeyAlgorithm::Unsupported => false,
        })
        .take(MAX_CANDIDATE_KEYS)
        .collect();
    if candidates.is_empty() {
        return Err(if kid.is_some() { LogoutTokenCheck::NoMatchingKey } else { LogoutTokenCheck::Invalid });
    }
    if !candidates.iter().any(|key| key.verify_signature(&alg, signing_input.as_bytes(), &signature).is_ok()) {
        return Err(LogoutTokenCheck::Invalid);
    }
    decode_json_segment(payload).ok_or(LogoutTokenCheck::Invalid)
}

fn bounded_string(value: Option<&serde_json::Value>, max_chars: usize) -> Result<Option<String>, LogoutTokenCheck> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(text)) if !text.is_empty() && text.chars().count() <= max_chars => Ok(Some(text.clone())),
        Some(_) => Err(LogoutTokenCheck::Invalid),
    }
}

fn numeric_date(value: Option<&serde_json::Value>) -> Result<Option<i64>, LogoutTokenCheck> {
    match value {
        None => Ok(None),
        Some(value) => {
            let number = value.as_f64().filter(|number| number.is_finite() && number.abs() < 1e12).ok_or(LogoutTokenCheck::Invalid)?;
            #[allow(clippy::cast_possible_truncation)]
            Ok(Some(number.floor() as i64))
        }
    }
}

/// Every offline check of a back-channel logout token (OIDC Back-Channel Logout 1.0 section
/// 2.6), at Unix time `now` with `skew` seconds of tolerance.
fn validate_logout_token(
    raw: &str,
    keys: &CoreJsonWebKeySet,
    issuer: &str,
    client_id: &str,
    now: i64,
    skew: i64,
) -> Result<LogoutClaims, LogoutTokenCheck> {
    if raw.is_empty() || raw.len() > MAX_LOGOUT_TOKEN_BYTES {
        return Err(LogoutTokenCheck::Invalid);
    }
    let claims = verify_jws(raw, keys)?;
    let invalid = LogoutTokenCheck::Invalid;
    if !claims.is_object() || claims.get("iss").and_then(serde_json::Value::as_str) != Some(issuer) {
        return Err(invalid);
    }
    let audience_ok = match claims.get("aud") {
        Some(serde_json::Value::String(audience)) => audience == client_id,
        Some(serde_json::Value::Array(audiences)) => {
            !audiences.is_empty() && audiences.len() <= MAX_AUDIENCES && audiences.iter().all(|audience| audience.as_str() == Some(client_id))
        }
        _ => false,
    };
    if !audience_ok || claims.get("nonce").is_some() {
        return Err(invalid);
    }
    let max_age = i64::try_from(LOGOUT_TOKEN_MAX_AGE.as_secs()).unwrap_or(120);
    let issued = numeric_date(claims.get("iat"))?.ok_or(invalid)?;
    if issued > now.saturating_add(skew) || issued < now.saturating_sub(max_age).saturating_sub(skew) {
        return Err(invalid);
    }
    if numeric_date(claims.get("exp"))?.is_some_and(|expiry| expiry < now.saturating_sub(skew))
        || numeric_date(claims.get("nbf"))?.is_some_and(|not_before| not_before > now.saturating_add(skew))
    {
        return Err(invalid);
    }
    let event_ok = claims
        .get("events")
        .and_then(serde_json::Value::as_object)
        .and_then(|events| events.get(BACKCHANNEL_LOGOUT_EVENT))
        .is_some_and(serde_json::Value::is_object);
    if !event_ok {
        return Err(invalid);
    }
    let jti = bounded_string(claims.get("jti"), MAX_JTI_CHARS)?.ok_or(invalid)?;
    let subject = bounded_string(claims.get("sub"), oidc_logins::MAX_SUBJECT_CHARS)?;
    let session_id = bounded_string(claims.get("sid"), MAX_SID_CHARS)?;
    if subject.is_none() && session_id.is_none() {
        return Err(invalid);
    }
    let mut hasher = Sha256::new();
    hasher.update(issuer.as_bytes());
    hasher.update([0u8]);
    hasher.update(jti.as_bytes());
    Ok(LogoutClaims {
        issuer: issuer.to_owned(),
        subject,
        session_id,
        replay_key: hasher.finalize().into(),
        replay_until: issued.saturating_add(max_age).saturating_add(skew.saturating_mul(2)),
    })
}

/// Add the ID token's `groups` (an array of strings) to `metadata`: control characters removed,
/// empty and over-long names dropped (never truncated into another name), duplicates removed, at
/// most [`MAX_GROUPS`], and only as many as keep the serialized metadata within the stored
/// budget. `groups_truncated` records that some were dropped. Nothing is granted from these
/// (S4-024 D5/D6).
fn add_groups(metadata: &mut serde_json::Value, claims: &serde_json::Value) {
    let Some(raw_groups) = claims.get("groups").and_then(serde_json::Value::as_array) else {
        return;
    };
    let Some(object) = metadata.as_object_mut() else {
        return;
    };
    let scan_limit = MAX_GROUPS.saturating_mul(8);
    let mut groups: Vec<String> = Vec::new();
    let mut truncated = raw_groups.len() > scan_limit;
    for value in raw_groups.iter().take(scan_limit) {
        let cleaned = value.as_str().filter(|name| name.chars().count() <= MAX_GROUP_CHARS).and_then(|name| clean_claim(name, MAX_GROUP_CHARS));
        match cleaned {
            Some(name) if groups.contains(&name) => {}
            Some(name) if groups.len() < MAX_GROUPS => groups.push(name),
            _ => truncated = true,
        }
    }
    // Leave room for `"groups_truncated":true` so the flag always fits.
    let budget = oidc_logins::MAX_VERIFIED_METADATA_BYTES.saturating_sub(32);
    loop {
        object.insert("groups".to_owned(), serde_json::json!(groups));
        let size = serde_json::to_string(&*object).map_or(usize::MAX, |text| text.len());
        if size <= budget {
            break;
        }
        if groups.pop().is_none() {
            object.remove("groups");
            break;
        }
        truncated = true;
    }
    if truncated {
        object.insert("groups_truncated".to_owned(), serde_json::Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_accepts_only_relative_paths() {
        for ok in ["/", "/files", "/files/abc?tab=1#frag", "/a/b%2Fc"] {
            assert!(return_to_allowed(ok), "{ok}");
        }
        for bad in [
            "",
            "files",
            "//evil.example",
            "/\\evil.example",
            "https://evil.example/",
            "/a\\b",
            "/a b",
            "/\u{7f}",
            "/é",
            "javascript:alert(1)",
            "\t/files",
        ] {
            assert!(!return_to_allowed(bad), "{bad:?}");
        }
        assert!(!return_to_allowed(&format!("/{}", "a".repeat(MAX_RETURN_TO_CHARS))));
    }

    #[test]
    fn secrets_are_43_url_safe_characters_and_distinct() -> Result<(), OidcError> {
        let first = random_secret()?;
        let second = random_secret()?;
        assert!(state_well_formed(&first) && state_well_formed(&second));
        assert_ne!(first, second);
        Ok(())
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn config_requires_https_outside_loopback_tests() {
        let config = OidcConfig::new("https://login.example.com", "client", Some("secret"), "https://app.example.com/auth/callback");
        assert_eq!(config.validate(), Ok(()));
        let plain = OidcConfig::new("http://127.0.0.1:9000", "client", None, "http://127.0.0.1:9001/cb");
        assert_eq!(plain.validate(), Err(OidcError::InvalidConfig));
        let loopback = OidcConfig { allow_insecure_loopback: true, ..plain };
        assert_eq!(loopback.validate(), Ok(()));
        let remote = OidcConfig { issuer_url: "http://login.example.com".into(), ..loopback };
        assert_eq!(remote.validate(), Err(OidcError::InvalidConfig));
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let config = OidcConfig::new("https://login.example.com", "client", Some("top-secret"), "https://app.example.com/cb");
        assert!(!format!("{config:?}").contains("top-secret"));
        let begin = BeginLogin { authorization_url: "https://x/?state=s3cr3t".into(), state_cookie: "s3cr3t".into() };
        assert!(!format!("{begin:?}").contains("s3cr3t"));
    }

    #[test]
    fn claims_are_cleaned_and_bounded() {
        assert_eq!(clean_claim("  Ada\u{0}\n Lovelace ", 255).as_deref(), Some("Ada Lovelace"));
        assert_eq!(clean_claim("\u{1}\u{2}", 255), None);
        assert_eq!(clean_claim(&"x".repeat(400), 255).map(|value| value.len()), Some(255));
    }

    // ---- back-channel logout tokens (in-test ES256 signer) ------------------------------------

    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
    use serde_json::{Value, json};

    const ISSUER: &str = "https://login.example.com/realms/corp";
    const CLIENT: &str = "figcraft-cloud";
    const NOW: i64 = 1_800_000_000;
    const SKEW: i64 = 60;
    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn key_pair() -> EcdsaKeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    }

    fn key_set(key: &EcdsaKeyPair, kid: &str) -> CoreJsonWebKeySet {
        let point = key.public_key().as_ref();
        let (x, y) = (&point[1..33], &point[33..65]);
        serde_json::from_value(json!({"keys": [
            {"kty": "EC", "use": "sig", "alg": "ES256", "kid": kid, "crv": "P-256", "x": B64.encode(x), "y": B64.encode(y)}
        ]}))
        .unwrap()
    }

    fn sign(key: &EcdsaKeyPair, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", B64.encode(header.to_string()), B64.encode(claims.to_string()));
        let signature = key.sign(&ring::rand::SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", B64.encode(signature.as_ref()))
    }

    fn header() -> Value {
        json!({"alg": "ES256", "kid": "k1", "typ": "logout+jwt"})
    }

    fn logout_claims() -> Value {
        json!({
            "iss": ISSUER, "aud": CLIENT, "iat": NOW - 5, "exp": NOW + 115, "jti": "jti-1", "sub": "user-1", "sid": "sid-1",
            "events": {BACKCHANNEL_LOGOUT_EVENT: {}},
        })
    }

    fn check(token: &str, keys: &CoreJsonWebKeySet) -> Result<LogoutClaims, LogoutTokenCheck> {
        validate_logout_token(token, keys, ISSUER, CLIENT, NOW, SKEW)
    }

    #[test]
    fn a_valid_logout_token_is_accepted() {
        let key = key_pair();
        let keys = key_set(&key, "k1");
        let claims = check(&sign(&key, &header(), &logout_claims()), &keys).unwrap();
        assert_eq!((claims.issuer.as_str(), claims.subject.as_deref(), claims.session_id.as_deref()), (ISSUER, Some("user-1"), Some("sid-1")));
        assert_eq!(claims.replay_until, NOW - 5 + 120 + 2 * SKEW);
        // An audience array of only our client id, and a token with `sid` but no `sub`, also pass
        // the offline checks (the handler then refuses a token without `sub`).
        let mut array = logout_claims();
        array["aud"] = json!([CLIENT]);
        assert!(check(&sign(&key, &header(), &array), &keys).is_ok());
        let mut sid_only = logout_claims();
        sid_only.as_object_mut().unwrap().remove("sub");
        assert_eq!(check(&sign(&key, &header(), &sid_only), &keys).unwrap().subject, None);
    }

    #[test]
    fn invalid_logout_tokens_are_refused() {
        let key = key_pair();
        let keys = key_set(&key, "k1");
        let with = |edit: &dyn Fn(&mut serde_json::Map<String, Value>)| {
            let mut claims = logout_claims();
            edit(claims.as_object_mut().unwrap());
            sign(&key, &header(), &claims)
        };
        let cases: Vec<(&str, String)> = vec![
            ("wrong aud", with(&|c| drop(c.insert("aud".into(), json!("someone-else"))))),
            ("extra aud", with(&|c| drop(c.insert("aud".into(), json!([CLIENT, "someone-else"]))))),
            ("empty aud", with(&|c| drop(c.insert("aud".into(), json!([]))))),
            ("missing aud", with(&|c| drop(c.remove("aud")))),
            ("wrong iss", with(&|c| drop(c.insert("iss".into(), json!("https://evil.example"))))),
            ("missing events", with(&|c| drop(c.remove("events")))),
            ("wrong event", with(&|c| drop(c.insert("events".into(), json!({"http://schemas.openid.net/event/other": {}}))))),
            ("event not an object", with(&|c| drop(c.insert("events".into(), json!({BACKCHANNEL_LOGOUT_EVENT: "yes"}))))),
            ("old iat", with(&|c| drop(c.insert("iat".into(), json!(NOW - 120 - SKEW - 1))))),
            ("future iat", with(&|c| drop(c.insert("iat".into(), json!(NOW + SKEW + 1))))),
            ("missing iat", with(&|c| drop(c.remove("iat")))),
            ("string iat", with(&|c| drop(c.insert("iat".into(), json!("now"))))),
            ("expired", with(&|c| drop(c.insert("exp".into(), json!(NOW - SKEW - 1))))),
            ("not yet valid", with(&|c| drop(c.insert("nbf".into(), json!(NOW + SKEW + 1))))),
            ("nonce present", with(&|c| drop(c.insert("nonce".into(), json!("n"))))),
            ("missing jti", with(&|c| drop(c.remove("jti")))),
            ("empty jti", with(&|c| drop(c.insert("jti".into(), json!(""))))),
            ("long jti", with(&|c| drop(c.insert("jti".into(), json!("j".repeat(257)))))),
            (
                "no sub or sid",
                with(&|c| {
                    c.remove("sub");
                    c.remove("sid");
                }),
            ),
            ("numeric sub", with(&|c| drop(c.insert("sub".into(), json!(7))))),
            ("hs256", sign(&key, &json!({"alg": "HS256", "kid": "k1"}), &logout_claims())),
            ("none", format!("{}.{}.", B64.encode(json!({"alg": "none"}).to_string()), B64.encode(logout_claims().to_string()))),
            ("crit header", sign(&key, &json!({"alg": "ES256", "kid": "k1", "crit": ["exp"]}), &logout_claims())),
            ("other key", sign(&key_pair(), &header(), &logout_claims())),
            ("four parts", format!("{}.x", sign(&key, &header(), &logout_claims()))),
            ("garbage", "not-a-jwt".to_owned()),
            ("too long", "a".repeat(MAX_LOGOUT_TOKEN_BYTES + 1)),
        ];
        for (name, token) in cases {
            assert_eq!(check(&token, &keys).err(), Some(LogoutTokenCheck::Invalid), "{name}");
        }
        // A tampered payload under a valid signature.
        let token = sign(&key, &header(), &logout_claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let mut forged_claims = logout_claims();
        forged_claims["sub"] = json!("admin");
        let forged = B64.encode(forged_claims.to_string());
        parts[1] = &forged;
        assert_eq!(check(&parts.join("."), &keys).err(), Some(LogoutTokenCheck::Invalid));
        // An unknown kid is worth one key refresh.
        assert_eq!(check(&sign(&key, &json!({"alg": "ES256", "kid": "k2"}), &logout_claims()), &keys).err(), Some(LogoutTokenCheck::NoMatchingKey));
        // Without a kid, any compatible signing key is tried.
        assert!(check(&sign(&key, &json!({"alg": "ES256"}), &logout_claims()), &keys).is_ok());
    }

    #[test]
    fn a_replayed_jti_is_refused_until_it_expires_and_the_cache_is_bounded() {
        let key = key_pair();
        let keys = key_set(&key, "k1");
        let claims = check(&sign(&key, &header(), &logout_claims()), &keys).unwrap();
        let mut cache = ReplayCache::default();
        assert_eq!(cache.insert(claims.replay_key, claims.replay_until, NOW), Ok(()));
        assert_eq!(cache.insert(claims.replay_key, claims.replay_until, NOW), Err(OidcError::InvalidLogoutToken));
        // The same jti from another issuer is a different key.
        let other = validate_logout_token(
            &sign(&key, &header(), &json!({"iss": "https://other.example", "aud": CLIENT, "iat": NOW, "jti": "jti-1", "sub": "u", "events": {BACKCHANNEL_LOGOUT_EVENT: {}}})),
            &keys,
            "https://other.example",
            CLIENT,
            NOW,
            SKEW,
        )
        .unwrap();
        assert_ne!(other.replay_key, claims.replay_key);
        // Forgotten after a failed revocation, accepted again.
        cache.remove(&claims.replay_key);
        assert_eq!(cache.insert(claims.replay_key, claims.replay_until, NOW), Ok(()));
        // Full of live entries: fail closed; expired entries are pruned to make room.
        let mut full = ReplayCache::default();
        for index in 0..MAX_REMEMBERED_LOGOUT_TOKENS {
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&(index as u64).to_le_bytes());
            full.insert(key, NOW + 10, NOW).unwrap();
        }
        assert_eq!(full.insert([0xff; 32], NOW + 10, NOW), Err(OidcError::InvalidLogoutToken));
        assert_eq!(full.insert([0xff; 32], NOW + 100, NOW + 11), Ok(()));
        assert_eq!(full.seen.len(), 1);
    }

    #[test]
    fn the_logout_form_needs_exactly_one_bounded_token() {
        assert_eq!(logout_token_from_form(b"logout_token=a.b.c").as_deref(), Some("a.b.c"));
        assert_eq!(logout_token_from_form(b"other=1&logout_token=a%2Eb.c").as_deref(), Some("a.b.c"));
        assert_eq!(logout_token_from_form(b""), None);
        assert_eq!(logout_token_from_form(b"logout_token="), None);
        assert_eq!(logout_token_from_form(b"logout_token=a&logout_token=b"), None);
        assert_eq!(logout_token_from_form(format!("logout_token={}", "a".repeat(MAX_LOGOUT_TOKEN_BYTES + 1)).as_bytes()), None);
        assert_eq!(logout_token_from_form(&vec![b'a'; MAX_LOGOUT_FORM_BYTES + 1]), None);
    }

    #[test]
    fn groups_are_kept_bounded_and_never_overflow_the_metadata_budget() {
        let mut metadata = json!({"email": "a@example.com"});
        add_groups(&mut metadata, &json!({"groups": ["/designers", "/designers", " /eng\u{0} ", "", 7, "g".repeat(MAX_GROUP_CHARS + 1)]}));
        assert_eq!(metadata["groups"], json!(["/designers", "/eng"]));
        assert_eq!(metadata["groups_truncated"], json!(true));

        let mut clean = json!({});
        add_groups(&mut clean, &json!({"groups": ["/a", "/b"]}));
        assert_eq!(clean, json!({"groups": ["/a", "/b"]}));

        let mut none = json!({"email": "a@example.com"});
        add_groups(&mut none, &json!({"groups": "not-an-array"}));
        assert_eq!(none, json!({"email": "a@example.com"}));

        let many: Vec<String> = (0..1000).map(|index| format!("/group-{index}")).collect();
        let mut capped = json!({});
        add_groups(&mut capped, &json!({"groups": many}));
        assert_eq!(capped["groups"].as_array().map(Vec::len), Some(MAX_GROUPS));
        assert_eq!(capped["groups_truncated"], json!(true));

        // Large existing claims: groups shrink to fit the stored budget, never over it.
        let long_name = "名".repeat(255);
        let mut tight = json!({"name": long_name, "preferred_username": long_name, "email": "e".repeat(320)});
        let big: Vec<String> = (0..MAX_GROUPS).map(|index| format!("{index:03}{}", "x".repeat(MAX_GROUP_CHARS - 3))).collect();
        add_groups(&mut tight, &json!({"groups": big}));
        assert!(serde_json::to_string(&tight).unwrap().len() <= oidc_logins::MAX_VERIFIED_METADATA_BYTES);
        assert_eq!(tight["groups_truncated"], json!(true));
    }
}
