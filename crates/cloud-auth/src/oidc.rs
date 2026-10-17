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
//! Every failure is a fixed [`OidcError`] category; no token, code, state, nonce or verifier is
//! ever included in an error or `Debug` output.

use std::time::{Duration, Instant};

use base64::Engine as _;
use figcraft_cloud_domain::{ErrorCode, PrincipalId};
use figcraft_cloud_storage::oidc_logins::{self, NewLoginAttempt, OidcLoginError};
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreIdToken, CoreIdTokenClaims, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreProviderMetadata,
};
use openidconnect::{
    AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, SignatureVerificationError, TokenResponse as _,
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
    /// The metadata store failed.
    Storage,
}

impl OidcError {
    /// The public error code for an HTTP response.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::InvalidReturnTo => ErrorCode::InvalidRequest,
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
    /// Bounded verified claims (`email`, `email_verified`, `name`, `preferred_username`).
    pub metadata: serde_json::Value,
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
        let identity = match id_token.claims(&self.verifier(keys), &nonce) {
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
        self.check_not_before(&raw)?;
        Ok(identity)
    }

    /// `openidconnect` does not check `nbf`; the signature is verified by now, so read it from
    /// the payload directly.
    fn check_not_before(&self, raw: &str) -> Result<(), OidcError> {
        let payload = raw.split('.').nth(1).ok_or(OidcError::InvalidIdToken)?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).map_err(|_| OidcError::InvalidIdToken)?;
        let claims: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| OidcError::InvalidIdToken)?;
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
}
