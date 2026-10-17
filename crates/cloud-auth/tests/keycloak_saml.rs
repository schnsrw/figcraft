//! S4-025: enterprise SAML federation through the Keycloak identity broker, end to end.
//!
//! Skipped unless `FIGCRAFT_TEST_KEYCLOAK_URL` is set (and `FIGCRAFT_TEST_KEYCLOAK_SETUP`, the
//! JSON the setup script writes, plus the dev database files). The topology and the setup
//! script are in `docs/saas/saml-federation.md`: one disposable Keycloak with realm `corp-idp`
//! (plays the customer's SAML IdP, user `alice`) and realm `figcraft-broker` (the OIDC issuer
//! FigCraft trusts, with SAML identity provider `corp-saml` and a browser flow that redirects
//! to it).
//!
//! The test drives the real `OidcProvider` (discover, begin_login, complete_login) and acts as
//! the browser with a cookie-keeping `reqwest` client (explicit rustls/ring configuration, no
//! redirects followed automatically, bounded bodies and steps): broker → SAML AuthnRequest
//! (redirect binding) → `corp-idp` login form → SAML Response (POST binding auto-submit form)
//! → broker ACS → first-broker-login → callback with `code` and `state`. It then checks SSO,
//! logout, broker key rotation (unknown `kid` → one JWKS refresh) and revocation at the IdP,
//! at the broker and at FigCraft. One sequential scenario: the steps share Keycloak state.
//! Principals it creates are named `e4-alice Corp` under the container's unique issuer.
//!
//! The test also configures the broker client (G4, S4-025): a protocol mapper emits the brokered
//! user's `groups` attribute (imported from the SAML `memberOf` attribute) as an ID-token claim,
//! and Keycloak's back-channel logout URL points at a listener the test runs on loopback
//! (reached from the container as `FIGCRAFT_TEST_KEYCLOAK_BACKCHANNEL_HOST`, default
//! `host.docker.internal`). The listener hands each logout token to
//! `OidcProvider::backchannel_logout`, as `POST /api/v1/auth/backchannel-logout` does.

use std::sync::Arc;
use std::time::Duration;

use figcraft_cloud_auth::oidc::{LoginOutcome, OidcConfig, OidcError, OidcProvider, logout_token_from_form};
use figcraft_cloud_storage::sessions::{self, SessionLimits};
use serde_json::{Value, json};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type BoxError = Box<dyn std::error::Error>;

const MAX_BODY_BYTES: usize = 512 * 1024;
const MAX_STEPS: usize = 20;
const ROTATED_KEY_NAME: &str = "e4-rotated-rsa";
const IDP_ALIAS: &str = "corp-saml";
const GROUPS_MAPPER: &str = "e4-groups";

struct Setup {
    keycloak_url: String,
    admin_password: String,
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect_url: String,
    idp_realm: String,
    broker_realm: String,
    username: String,
    password: String,
}

fn setup() -> Result<Option<Setup>, BoxError> {
    let Ok(keycloak_url) = std::env::var("FIGCRAFT_TEST_KEYCLOAK_URL") else {
        eprintln!("FIGCRAFT_TEST_KEYCLOAK_URL is unset; Keycloak SAML federation probe skipped");
        return Ok(None);
    };
    let path = std::env::var("FIGCRAFT_TEST_KEYCLOAK_SETUP")?;
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned).ok_or(format!("setup field {name}"));
    if field("keycloak_url")? != keycloak_url.trim_end_matches('/') {
        return Err("FIGCRAFT_TEST_KEYCLOAK_URL does not match the setup file".into());
    }
    Ok(Some(Setup {
        keycloak_url: field("keycloak_url")?,
        admin_password: field("admin_password")?,
        issuer: field("issuer")?,
        client_id: field("client_id")?,
        client_secret: field("client_secret")?,
        redirect_url: field("redirect_url")?,
        idp_realm: field("idp_realm")?,
        broker_realm: field("broker_realm")?,
        username: field("username")?,
        password: field("password")?,
    }))
}

async fn pool_from(variable: &str) -> Result<PgPool, BoxError> {
    let url = std::fs::read_to_string(std::env::var(variable)?)?;
    Ok(PgPoolOptions::new().max_connections(2).connect(url.trim_end()).await?)
}

fn http_client() -> Result<reqwest::Client, BoxError> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()?)
}

async fn read_body(mut response: reqwest::Response) -> Result<String, BoxError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err("response body too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn form_body(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

// ---- Keycloak admin REST API ------------------------------------------------------------------

struct Admin<'a> {
    http: reqwest::Client,
    setup: &'a Setup,
}

impl Admin<'_> {
    /// A fresh admin token per call (the default admin token lifetime is 60 s).
    async fn token(&self) -> Result<String, BoxError> {
        let body = form_body(&[
            ("grant_type".into(), "password".into()),
            ("client_id".into(), "admin-cli".into()),
            ("username".into(), "admin".into()),
            ("password".into(), self.setup.admin_password.clone()),
        ]);
        let response = self
            .http
            .post(format!("{}/realms/master/protocol/openid-connect/token", self.setup.keycloak_url))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        let value: Value = serde_json::from_str(&read_body(response).await?)?;
        value.get("access_token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "no admin token".into())
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, BoxError> {
        let token = self.token().await?;
        let mut request = self.http.request(method.clone(), format!("{}/admin/realms{path}", self.setup.keycloak_url)).bearer_auth(token);
        if let Some(body) = body {
            request = request.header("content-type", "application/json").body(serde_json::to_vec(&body)?);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = read_body(response).await?;
        if !status.is_success() {
            return Err(format!("admin {method} {path}: {status}").into());
        }
        if text.is_empty() { Ok(Value::Null) } else { Ok(serde_json::from_str(&text)?) }
    }

    async fn get(&self, path: &str) -> Result<Value, BoxError> {
        self.call(reqwest::Method::GET, path, None).await
    }

    async fn user_id(&self, realm: &str, username: &str) -> Result<String, BoxError> {
        let users = self.get(&format!("/{realm}/users?username={username}&exact=true")).await?;
        users.get(0).and_then(|user| user.get("id")).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("no user {username}").into())
    }

    async fn broker_users(&self) -> Result<Vec<String>, BoxError> {
        let users = self.get(&format!("/{}/users?briefRepresentation=true&max=50", self.setup.broker_realm)).await?;
        Ok(users.as_array().map(|users| users.iter().filter_map(|user| user.get("id")?.as_str().map(str::to_owned)).collect()).unwrap_or_default())
    }

    async fn set_enabled(&self, realm: &str, user_id: &str, enabled: bool) -> Result<(), BoxError> {
        self.call(reqwest::Method::PUT, &format!("/{realm}/users/{user_id}"), Some(json!({"enabled": enabled}))).await.map(|_| ())
    }

    async fn logout_user(&self, realm: &str, user_id: &str) -> Result<(), BoxError> {
        self.call(reqwest::Method::POST, &format!("/{realm}/users/{user_id}/logout"), None).await.map(|_| ())
    }

    async fn rotated_key_components(&self, realm: &str) -> Result<Vec<String>, BoxError> {
        let components = self.get(&format!("/{realm}/components?type=org.keycloak.keys.KeyProvider&name={ROTATED_KEY_NAME}")).await?;
        Ok(components
            .as_array()
            .map(|items| items.iter().filter_map(|item| item.get("id")?.as_str().map(str::to_owned)).collect())
            .unwrap_or_default())
    }

    async fn remove_rotated_keys(&self, realm: &str) -> Result<(), BoxError> {
        for component in self.rotated_key_components(realm).await? {
            self.call(reqwest::Method::DELETE, &format!("/{realm}/components/{component}"), None).await?;
        }
        Ok(())
    }

    /// Rotate `realm`'s signing key: a new, higher-priority generated RS256 key becomes active.
    async fn rotate_key(&self, realm: &str) -> Result<(), BoxError> {
        let realm_id = self.get(&format!("/{realm}")).await?.get("id").and_then(Value::as_str).map(str::to_owned).ok_or("no realm id")?;
        let component = json!({
            "name": ROTATED_KEY_NAME, "providerId": "rsa-generated", "providerType": "org.keycloak.keys.KeyProvider", "parentId": realm_id,
            "config": {"priority": ["1000"], "algorithm": ["RS256"], "keySize": ["2048"], "active": ["true"], "enabled": ["true"]},
        });
        self.call(reqwest::Method::POST, &format!("/{realm}/components"), Some(component)).await.map(|_| ())
    }

    async fn client_path(&self) -> Result<String, BoxError> {
        let clients = self.get(&format!("/{}/clients?clientId={}", self.setup.broker_realm, self.setup.client_id)).await?;
        let id = clients.get(0).and_then(|client| client.get("id")).and_then(Value::as_str).ok_or("no broker client")?;
        Ok(format!("/{}/clients/{id}", self.setup.broker_realm))
    }

    /// Send back-channel logout for the FigCraft client to `backchannel_url` (session id
    /// required), and emit the user's `groups` attribute as a multivalued ID-token claim.
    async fn configure_client(&self, backchannel_url: &str) -> Result<(), BoxError> {
        let path = self.client_path().await?;
        let mut client = self.get(&path).await?;
        let object = client.as_object_mut().ok_or("client is not an object")?;
        object.insert("frontchannelLogout".to_owned(), Value::Bool(false));
        let attributes = object.entry("attributes").or_insert_with(|| json!({}));
        let attributes = attributes.as_object_mut().ok_or("client attributes are not an object")?;
        attributes.insert("backchannel.logout.url".to_owned(), Value::String(backchannel_url.to_owned()));
        attributes.insert("backchannel.logout.session.required".to_owned(), Value::String("true".to_owned()));
        attributes.insert("backchannel.logout.revoke.offline.tokens".to_owned(), Value::String("false".to_owned()));
        self.call(reqwest::Method::PUT, &path, Some(client)).await?;
        let mappers = self.get(&format!("{path}/protocol-mappers/models")).await?;
        let present =
            mappers.as_array().is_some_and(|items| items.iter().any(|item| item.get("name").and_then(Value::as_str) == Some(GROUPS_MAPPER)));
        if !present {
            let mapper = json!({
                "name": GROUPS_MAPPER, "protocol": "openid-connect", "protocolMapper": "oidc-usermodel-attribute-mapper",
                "config": {
                    "user.attribute": "groups", "claim.name": "groups", "jsonType.label": "String", "multivalued": "true",
                    "id.token.claim": "true", "access.token.claim": "false", "userinfo.token.claim": "false",
                },
            });
            self.call(reqwest::Method::POST, &format!("{path}/protocol-mappers/models"), Some(mapper)).await?;
        }
        Ok(())
    }

    /// The active RS256 key's id and certificate.
    async fn active_rs256(&self, realm: &str) -> Result<(String, String), BoxError> {
        let keys = self.get(&format!("/{realm}/keys")).await?;
        let kid = keys.pointer("/active/RS256").and_then(Value::as_str).ok_or("no active RS256 key")?;
        let certificate = keys
            .get("keys")
            .and_then(Value::as_array)
            .and_then(|keys| keys.iter().find(|key| key.get("kid").and_then(Value::as_str) == Some(kid)))
            .and_then(|key| key.get("certificate")?.as_str())
            .ok_or("no certificate for the active key")?;
        Ok((kid.to_owned(), certificate.to_owned()))
    }

    /// Point the broker's SAML identity provider at `certificate` for assertion signatures.
    async fn set_idp_signing_certificate(&self, certificate: &str) -> Result<(), BoxError> {
        let path = format!("/{}/identity-provider/instances/{IDP_ALIAS}", self.setup.broker_realm);
        let mut provider = self.get(&path).await?;
        let config = provider.get_mut("config").and_then(Value::as_object_mut).ok_or("identity provider without config")?;
        config.insert("signingCertificate".to_owned(), Value::String(certificate.to_owned()));
        self.call(reqwest::Method::PUT, &path, Some(provider)).await.map(|_| ())
    }
}

// ---- the browser ------------------------------------------------------------------------------

/// A minimal cookie jar: (path, name) → value; the Secure attribute is ignored because the
/// disposable broker runs on plain-HTTP loopback.
#[derive(Default)]
struct Jar {
    cookies: Vec<(String, String, String)>,
}

impl Jar {
    fn store(&mut self, headers: &reqwest::header::HeaderMap) {
        for header in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(line) = header.to_str() else { continue };
            let mut parts = line.split(';').map(str::trim);
            let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else { continue };
            let mut path = "/".to_owned();
            let mut expired = false;
            for attribute in parts {
                let (key, val) = attribute.split_once('=').unwrap_or((attribute, ""));
                if key.eq_ignore_ascii_case("path") && val.starts_with('/') {
                    path = val.to_owned();
                } else if (key.eq_ignore_ascii_case("max-age") && val.trim().parse::<i64>().is_ok_and(|age| age <= 0))
                    || (key.eq_ignore_ascii_case("expires") && val.contains("1970"))
                {
                    expired = true;
                }
            }
            self.cookies.retain(|(p, n, _)| !(p == &path && n == name));
            if !expired && !value.is_empty() && self.cookies.len() < 64 {
                self.cookies.push((path, name.to_owned(), value.to_owned()));
            }
        }
    }

    fn header_for(&self, url: &url::Url) -> Option<String> {
        let path = url.path();
        let matching: Vec<String> = self
            .cookies
            .iter()
            .filter(|(p, _, _)| path.starts_with(p.trim_end_matches('/')) || p == "/")
            .map(|(_, name, value)| format!("{name}={value}"))
            .collect();
        (!matching.is_empty()).then(|| matching.join("; "))
    }
}

fn html_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        out.push_str(rest.get(..start).unwrap_or_default());
        let tail = rest.get(start..).unwrap_or_default();
        let Some(end) = tail.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = tail.get(1..).unwrap_or_default();
            continue;
        };
        let entity = tail.get(1..end).unwrap_or_default();
        let decoded = match entity {
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = tail.get(end + 1..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let tail = tag.get(start..)?;
    let end = tail.find('"')?;
    Some(html_unescape(tail.get(..end)?))
}

/// The first form's action and its named inputs.
fn first_form(body: &str) -> Option<(String, Vec<(String, String)>)> {
    let start = body.find("<form")?;
    let form = body.get(start..)?;
    let open_end = form.find('>')?;
    let action = attribute(form.get(..open_end)?, "action")?;
    let close = form.find("</form>").unwrap_or(form.len());
    let mut inputs = Vec::new();
    let mut section = form.get(..close)?;
    while let Some(at) = section.find("<input") {
        let tag_tail = section.get(at..)?;
        let tag_end = tag_tail.find('>')?;
        let tag = tag_tail.get(..tag_end)?;
        if let Some(name) = attribute(tag, "name") {
            inputs.push((name, attribute(tag, "value").unwrap_or_default()));
        }
        section = tag_tail.get(tag_end..)?;
    }
    Some((action, inputs))
}

#[derive(Debug)]
enum End {
    Callback {
        code: String,
        state: String,
    },
    /// The flow stopped on a page that is not a login or SAML form (an error page).
    Stopped {
        status: u16,
        page: String,
    },
}

#[derive(Debug)]
struct Flow {
    end: End,
    saw_password_form: bool,
    saw_saml: bool,
}

struct Browser<'a> {
    http: reqwest::Client,
    jar: Jar,
    setup: &'a Setup,
}

impl<'a> Browser<'a> {
    fn new(setup: &'a Setup) -> Result<Self, BoxError> {
        Ok(Self { http: http_client()?, jar: Jar::default(), setup })
    }

    /// Follow `authorization_url` through the broker and the SAML IdP to the callback.
    async fn run(&mut self, authorization_url: &str) -> Result<Flow, BoxError> {
        let origin = url::Url::parse(&self.setup.keycloak_url)?.origin();
        let callback = url::Url::parse(&self.setup.redirect_url)?;
        let mut url = url::Url::parse(authorization_url)?;
        let mut post: Option<String> = None;
        let (mut saw_password_form, mut saw_saml) = (false, false);
        for _ in 0..MAX_STEPS {
            if url.origin() != origin {
                return Err(format!("left the broker origin: {}", url.origin().ascii_serialization()).into());
            }
            let mut request = match post.take() {
                Some(body) => self.http.post(url.clone()).header("content-type", "application/x-www-form-urlencoded").body(body),
                None => self.http.get(url.clone()),
            };
            if let Some(cookie) = self.jar.header_for(&url) {
                request = request.header(reqwest::header::COOKIE, cookie);
            }
            let response = request.send().await?;
            self.jar.store(response.headers());
            let status = response.status();
            if status.is_redirection() {
                let location =
                    response.headers().get(reqwest::header::LOCATION).and_then(|value| value.to_str().ok()).ok_or("redirect without location")?;
                let next = url.join(location)?;
                if next.origin() == callback.origin() && next.path() == callback.path() {
                    let query: std::collections::HashMap<String, String> = next.query_pairs().into_owned().collect();
                    let end = match (query.get("code"), query.get("state")) {
                        (Some(code), Some(state)) => End::Callback { code: code.clone(), state: state.clone() },
                        _ => End::Stopped { status: status.as_u16(), page: format!("callback error {:?}", query.get("error")) },
                    };
                    return Ok(Flow { end, saw_password_form, saw_saml });
                }
                url = next;
                continue;
            }
            let body = read_body(response).await?;
            let form = first_form(&body);
            if body.contains("name=\"SAMLResponse\"") || body.contains("name=\"SAMLRequest\"") {
                let (action, inputs) = form.ok_or("SAML page without a form")?;
                saw_saml = true;
                url = url.join(&action)?;
                post = Some(form_body(&inputs));
            } else if body.contains("id=\"kc-form-login\"") && !saw_password_form {
                let (action, _) = form.ok_or("login page without a form")?;
                saw_password_form = true;
                url = url.join(&action)?;
                post = Some(form_body(&[
                    ("username".into(), self.setup.username.clone()),
                    ("password".into(), self.setup.password.clone()),
                    ("credentialId".into(), String::new()),
                ]));
            } else {
                let page = body
                    .find("data-page-id=\"")
                    .and_then(|at| body.get(at + 14..))
                    .and_then(|tail| tail.split('"').next())
                    .unwrap_or("unknown")
                    .to_owned();
                let feedback = body
                    .find("kc-feedback-text")
                    .and_then(|at| body.get(at..))
                    .and_then(|tail| tail.split('>').nth(1))
                    .and_then(|text| text.split('<').next())
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                return Ok(Flow { end: End::Stopped { status: status.as_u16(), page: format!("{page}: {feedback}") }, saw_password_form, saw_saml });
            }
        }
        Err("too many steps".into())
    }
}

async fn login(
    provider: &OidcProvider,
    pool: &PgPool,
    browser: &mut Browser<'_>,
) -> Result<(Flow, Option<Result<LoginOutcome, OidcError>>), BoxError> {
    let begin = provider.begin_login(pool, Some("/files")).await?;
    let flow = browser.run(&begin.authorization_url).await?;
    match &flow.end {
        End::Callback { .. } => eprintln!("flow: callback (password form: {}, saml: {})", flow.saw_password_form, flow.saw_saml),
        End::Stopped { status, page } => {
            eprintln!("flow: stopped with {status} on {page} (password form: {}, saml: {})", flow.saw_password_form, flow.saw_saml);
        }
    }
    let outcome = match &flow.end {
        End::Callback { code, state } => Some(provider.complete_login(pool, code, state, Some(&begin.state_cookie)).await),
        End::Stopped { .. } => None,
    };
    Ok((flow, outcome))
}

type LogoutResult = (String, Result<u64, OidcError>);

/// A loopback listener standing in for `POST /api/v1/auth/backchannel-logout`: every logout
/// token Keycloak sends goes through `OidcProvider::backchannel_logout`; the token and result are
/// reported on the channel. Returns the URL the container uses and the channel.
async fn start_backchannel(
    provider: Arc<OidcProvider>,
    pool: PgPool,
) -> Result<(String, tokio::sync::mpsc::UnboundedReceiver<LogoutResult>), BoxError> {
    let bind = std::env::var("FIGCRAFT_TEST_KEYCLOAK_BACKCHANNEL_BIND").unwrap_or_else(|_| "127.0.0.1:0".to_owned());
    let host = std::env::var("FIGCRAFT_TEST_KEYCLOAK_BACKCHANNEL_HOST").unwrap_or_else(|_| "host.docker.internal".to_owned());
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let url = format!("http://{host}:{}/api/v1/auth/backchannel-logout", listener.local_addr()?.port());
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let handler = move |body: axum::body::Bytes| {
        let (provider, pool, sender) = (Arc::clone(&provider), pool.clone(), sender.clone());
        async move {
            let Some(token) = logout_token_from_form(&body) else {
                return axum::http::StatusCode::BAD_REQUEST;
            };
            let result = provider.backchannel_logout(&pool, &token).await;
            let status = if result.is_ok() { axum::http::StatusCode::OK } else { axum::http::StatusCode::BAD_REQUEST };
            let _ = sender.send((token, result));
            status
        }
    };
    let router = axum::Router::new().route("/api/v1/auth/backchannel-logout", axum::routing::post(handler));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((url, receiver))
}

async fn jwks_kids(http: &reqwest::Client, issuer: &str) -> Result<Vec<String>, BoxError> {
    let jwks: Value = serde_json::from_str(&read_body(http.get(format!("{issuer}/protocol/openid-connect/certs")).send().await?).await?)?;
    Ok(jwks
        .get("keys")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(|key| key.get("kid")?.as_str().map(str::to_owned)).collect())
        .unwrap_or_default())
}

#[tokio::test]
async fn saml_federation_through_the_keycloak_broker() -> TestResult {
    let Some(setup) = setup()? else {
        return Ok(());
    };
    let pool = pool_from("FIGCRAFT_TEST_DATABASE_URL_FILE").await?;
    let admin_pool = pool_from("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE").await?;
    let admin = Admin { http: http_client()?, setup: &setup };
    let (idp, broker) = (setup.idp_realm.as_str(), setup.broker_realm.as_str());

    // Reset for reruns: alice enabled at the IdP with no session, no brokered users, no
    // rotated key left over.
    let alice = admin.user_id(idp, &setup.username).await?;
    admin.set_enabled(idp, &alice, true).await?;
    admin.call(reqwest::Method::PUT, &format!("/{idp}/users/{alice}"), Some(json!({"firstName": "e4-alice", "lastName": "Corp"}))).await?;
    admin.logout_user(idp, &alice).await?;
    for user in admin.broker_users().await? {
        admin.call(reqwest::Method::DELETE, &format!("/{broker}/users/{user}"), None).await?;
    }
    admin.remove_rotated_keys(broker).await?;
    admin.remove_rotated_keys(idp).await?;
    admin.set_idp_signing_certificate(&admin.active_rs256(idp).await?.1).await?;

    let config = OidcConfig {
        allow_insecure_loopback: true,
        ..OidcConfig::new(&setup.issuer, &setup.client_id, Some(&setup.client_secret), &setup.redirect_url)
    };
    let provider = Arc::new(OidcProvider::discover(config.clone()).await?);
    assert_eq!(provider.issuer(), setup.issuer);
    let (backchannel_url, mut logouts) = start_backchannel(Arc::clone(&provider), pool.clone()).await?;
    admin.configure_client(&backchannel_url).await?;

    // 1. First login: broker → SAML IdP password form → SAML POST → broker creates the user →
    //    FigCraft creates the principal.
    let mut browser = Browser::new(&setup)?;
    let (flow, outcome) = login(&provider, &pool, &mut browser).await?;
    assert!(flow.saw_password_form && flow.saw_saml, "{flow:?}");
    let first = outcome.ok_or(format!("first login stopped: {flow:?}"))??;
    assert!(first.new_principal);
    assert_eq!(first.return_to.as_deref(), Some("/files"));

    // The subject is the broker user, linked to corp-idp's persistent NameID.
    let broker_users = admin.broker_users().await?;
    assert_eq!(broker_users.len(), 1, "{broker_users:?}");
    let broker_user = broker_users.first().cloned().ok_or("no broker user")?;
    let links = admin.get(&format!("/{broker}/users/{broker_user}/federated-identity")).await?;
    let link = links.get(0).ok_or("no federated identity")?;
    assert_eq!(link.get("identityProvider").and_then(Value::as_str), Some("corp-saml"));
    assert!(link.get("userId").and_then(Value::as_str).is_some_and(|name_id| name_id.starts_with("G-")), "persistent NameID: {link}");
    let (subject, name, metadata): (String, String, Value) = sqlx::query_as(
        "SELECT l.subject, p.display_name, l.verified_metadata FROM identity_links l JOIN principals p ON p.id = l.principal_id \
         WHERE l.issuer = $1 AND l.principal_id = $2::uuid",
    )
    .bind(&setup.issuer)
    .bind(first.principal_id.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(subject, broker_user);
    assert_eq!(name, "e4-alice Corp");
    assert_eq!(metadata.get("email").and_then(Value::as_str), Some("alice@corp.example"));
    assert_eq!(metadata.get("email_verified").and_then(Value::as_bool), Some(true));
    // The SAML `memberOf` attribute reaches FigCraft as the bounded `groups` claim (recorded,
    // never granted from).
    assert_eq!(metadata.get("groups"), Some(&json!(["designers"])), "{metadata}");
    assert_eq!(metadata.get("groups_truncated"), None);

    // 2. SSO: the broker session answers without the IdP; same principal, no duplicate.
    let (flow, outcome) = login(&provider, &pool, &mut browser).await?;
    assert!(!flow.saw_password_form && !flow.saw_saml, "{flow:?}");
    let second = outcome.ok_or("SSO login stopped")??;
    assert_eq!((second.principal_id, second.new_principal), (first.principal_id, false));

    // 3. Logout. A FigCraft session ends with sessions::logout.
    let limits = SessionLimits::default();
    let session = sessions::create_session(&pool, first.principal_id, limits).await?;
    assert!(sessions::resolve_session(&pool, &session.token, limits).await.is_ok());
    assert!(sessions::logout(&pool, &session.token).await?);
    assert!(sessions::resolve_session(&pool, &session.token, limits).await.is_err());
    //    Ending only the broker session re-runs SAML silently on the IdP session …
    admin.logout_user(broker, &broker_user).await?;
    let (flow, outcome) = login(&provider, &pool, &mut browser).await?;
    assert!(flow.saw_saml && !flow.saw_password_form, "{flow:?}");
    assert_eq!(outcome.ok_or("login after broker logout stopped")??.principal_id, first.principal_id);
    //    … and ending the IdP session too brings back the IdP password form.
    admin.logout_user(broker, &broker_user).await?;
    admin.logout_user(idp, &alice).await?;
    let (flow, outcome) = login(&provider, &pool, &mut browser).await?;
    assert!(flow.saw_saml && flow.saw_password_form, "{flow:?}");
    assert_eq!(outcome.ok_or("login after full logout stopped")??.principal_id, first.principal_id);

    //    Back-channel logout: ending the broker session makes Keycloak POST a logout token to
    //    FigCraft, which ends every FigCraft session of the principal. The same token again is
    //    a replay and is refused.
    while logouts.try_recv().is_ok() {}
    let (web, desktop) =
        (sessions::create_session(&pool, first.principal_id, limits).await?, sessions::create_session(&pool, first.principal_id, limits).await?);
    assert!(sessions::resolve_session(&pool, &web.token, limits).await.is_ok());
    admin.logout_user(broker, &broker_user).await?;
    let (logout_token, result) = tokio::time::timeout(Duration::from_secs(15), logouts.recv()).await?.ok_or("back-channel listener stopped")?;
    assert!(matches!(result, Ok(revoked) if revoked >= 2), "{result:?}");
    assert!(sessions::resolve_session(&pool, &web.token, limits).await.is_err());
    assert!(sessions::resolve_session(&pool, &desktop.token, limits).await.is_err());
    assert_eq!(provider.backchannel_logout(&pool, &logout_token).await, Err(OidcError::InvalidLogoutToken));
    let (reason,): (String,) = sqlx::query_as("SELECT revoke_reason FROM browser_sessions WHERE token_hash = $1")
        .bind(sessions::session_token_hash(&web.token).to_vec())
        .fetch_one(&pool)
        .await?;
    assert_eq!(reason, "logout");
    eprintln!("back-channel logout: {result:?}");

    // 4. Broker key rotation: a new, higher-priority RS256 key signs the next ID token. Its kid
    //    is not in the JWKS the provider cached at discovery, so it refreshes once and accepts.
    let before = jwks_kids(&admin.http, &setup.issuer).await?;
    let (old_kid, _) = admin.active_rs256(broker).await?;
    admin.rotate_key(broker).await?;
    let (new_kid, _) = admin.active_rs256(broker).await?;
    assert_ne!(new_kid, old_kid);
    assert!(!before.contains(&new_kid));
    let (flow, outcome) = login(&provider, &pool, &mut Browser::new(&setup)?).await?;
    assert_eq!(outcome.ok_or(format!("login after rotation stopped: {flow:?}"))??.principal_id, first.principal_id);

    //    SAML signing-key rotation at the IdP: assertions signed with the new key are refused
    //    by the broker (signature validation on) until the broker trusts the new certificate.
    let (_, old_certificate) = admin.active_rs256(idp).await?;
    admin.rotate_key(idp).await?;
    let (_, new_certificate) = admin.active_rs256(idp).await?;
    assert_ne!(new_certificate, old_certificate);
    let (flow, outcome) = login(&provider, &pool, &mut Browser::new(&setup)?).await?;
    assert!(outcome.is_none() && flow.saw_saml && flow.saw_password_form, "{flow:?}");
    admin.set_idp_signing_certificate(&new_certificate).await?;
    let (flow, outcome) = login(&provider, &pool, &mut Browser::new(&setup)?).await?;
    assert_eq!(outcome.ok_or(format!("login after SAML key rotation stopped: {flow:?}"))??.principal_id, first.principal_id);

    // 5. Revocation at the IdP: disabled alice cannot log in; no callback, no FigCraft login.
    admin.logout_user(broker, &broker_user).await?;
    admin.set_enabled(idp, &alice, false).await?;
    let (flow, outcome) = login(&provider, &pool, &mut Browser::new(&setup)?).await?;
    assert!(outcome.is_none() && flow.saw_password_form, "{flow:?}");
    assert!(matches!(&flow.end, End::Stopped { page, .. } if page.to_ascii_lowercase().contains("disabled")), "{flow:?}");
    admin.set_enabled(idp, &alice, true).await?;

    //    Revocation at the broker: the IdP succeeds but the disabled broker user is refused.
    admin.set_enabled(broker, &broker_user, false).await?;
    let (flow, outcome) = login(&provider, &pool, &mut Browser::new(&setup)?).await?;
    assert!(outcome.is_none() && flow.saw_saml, "{flow:?}");
    admin.set_enabled(broker, &broker_user, true).await?;

    //    Revocation at FigCraft: the broker succeeds but a suspended principal is refused.
    sqlx::query("UPDATE principals SET state = 'suspended' WHERE id = $1::uuid").bind(first.principal_id.to_string()).execute(&admin_pool).await?;
    let (flow, outcome) = login(&provider, &pool, &mut Browser::new(&setup)?).await?;
    assert_eq!(outcome.ok_or(format!("suspended login stopped at the broker: {flow:?}"))?, Err(OidcError::PrincipalInactive));

    // Leave the realms without the rotated keys for the next run.
    admin.remove_rotated_keys(broker).await?;
    admin.remove_rotated_keys(idp).await?;
    admin.set_idp_signing_certificate(&old_certificate).await?;
    Ok(())
}
