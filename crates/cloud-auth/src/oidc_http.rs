//! The HTTP client the OIDC flow uses for discovery, JWKS and token requests.
//!
//! `openidconnect` is built without its own HTTP backends; this adapter sends its requests with
//! the workspace `reqwest` over rustls/ring (an explicit TLS configuration: `reqwest` here has no
//! default provider and panics without one). Every request is checked against the endpoint policy
//! (HTTPS, or plain HTTP to a loopback host only when the configuration allows it for tests),
//! redirects are never followed (an issuer cannot bounce the client elsewhere), proxies are not
//! used, and response bodies are capped.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use openidconnect::{HttpRequest, HttpResponse};

/// Largest response body accepted from the provider.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Fixed, non-leaking HTTP failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OidcHttpError {
    /// The URL is not allowed by the endpoint policy.
    Refused,
    /// The request could not be built.
    Request,
    /// The request failed in transit.
    Transport,
    /// The response body exceeds [`MAX_RESPONSE_BYTES`].
    TooLarge,
    /// The client could not be configured.
    Client,
}

impl std::fmt::Display for OidcHttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "oidc http error: {self:?}")
    }
}

impl std::error::Error for OidcHttpError {}

/// True when `url` may be used as a provider endpoint: `https` with a host and no credentials,
/// or, only when `allow_insecure_loopback`, `http` to `localhost` or a loopback address.
#[must_use]
pub fn endpoint_allowed(url: &url::Url, allow_insecure_loopback: bool) -> bool {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    let Some(host) = url.host() else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" if allow_insecure_loopback => match host {
            url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(address) => address.is_loopback(),
            url::Host::Ipv6(address) => address.is_loopback(),
        },
        _ => false,
    }
}

/// The policy-checking client.
#[derive(Clone)]
pub struct OidcHttpClient {
    client: reqwest::Client,
    allow_insecure_loopback: bool,
}

impl OidcHttpClient {
    /// A client trusting the webpki roots.
    pub fn new(allow_insecure_loopback: bool) -> Result<Self, OidcHttpError> {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| OidcHttpError::Client)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| OidcHttpError::Client)?;
        Ok(Self { client, allow_insecure_loopback })
    }

    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, OidcHttpError> {
        let url = url::Url::parse(&request.uri().to_string()).map_err(|_| OidcHttpError::Refused)?;
        if !endpoint_allowed(&url, self.allow_insecure_loopback) {
            return Err(OidcHttpError::Refused);
        }
        let request = reqwest::Request::try_from(request).map_err(|_| OidcHttpError::Request)?;
        let mut response = self.client.execute(request).await.map_err(|_| OidcHttpError::Transport)?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| OidcHttpError::Transport)? {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(OidcHttpError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let mut built = openidconnect::http::Response::builder().status(status).body(body).map_err(|_| OidcHttpError::Transport)?;
        *built.headers_mut() = headers;
        Ok(built)
    }
}

impl<'c> openidconnect::AsyncHttpClient<'c> for OidcHttpClient {
    type Error = OidcHttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, OidcHttpError>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(self.send(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(url: &str, loopback: bool) -> bool {
        url::Url::parse(url).is_ok_and(|url| endpoint_allowed(&url, loopback))
    }

    #[test]
    fn production_requires_https() {
        assert!(allowed("https://login.example.com/", false));
        assert!(!allowed("http://login.example.com/", false));
        assert!(!allowed("http://127.0.0.1:8080/", false));
        assert!(!allowed("ftp://login.example.com/", false));
        assert!(!allowed("https://user:pw@login.example.com/", false));
    }

    #[test]
    fn plain_http_only_reaches_loopback_when_allowed() {
        assert!(allowed("http://127.0.0.1:8080/", true));
        assert!(allowed("http://[::1]:8080/", true));
        assert!(allowed("http://localhost:8080/", true));
        assert!(!allowed("http://10.0.0.1/", true));
        assert!(!allowed("http://login.example.com/", true));
        assert!(!allowed("http://localhost.example.com/", true));
    }
}
