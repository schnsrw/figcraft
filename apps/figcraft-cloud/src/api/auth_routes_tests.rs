//! Router tests of `/api/v1/auth` and the production authenticator through the full middleware
//! stack (fixture prefix d1). Database-backed tests use the dev PostgreSQL and are skipped without
//! `FIGCRAFT_TEST_DATABASE_URL_FILE`; every run makes fresh tenants and principals, so reruns pass.
//!
//! The OIDC test runs an in-test provider on loopback (discovery, JWKS, token endpoint) that
//! signs ES256 ID tokens with a key generated in-test and checks the PKCE challenge, following
//! `crates/cloud-auth/tests/oidc_flow.rs`; the test acts as the browser at the provider.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Form, Path, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use figcraft_cloud_auth::oidc::{OidcConfig, OidcProvider};
use figcraft_cloud_domain::{CloudDocumentId, DeploymentMode, PrincipalId, ProjectId, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::service_accounts::{Scope, create_service_account, issue_credential, revoke_credential};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, revoke_share_link};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};

use sqlx::PgPool;
use tower::ServiceExt as _;

use super::super::auth::{Authenticated, Grant};
use super::super::authenticator::{IdentityConfig, TENANT_HEADER};
use super::super::context::ContextSettings;
use super::super::error::ApiError;
use super::super::rate_limit::RateLimitConfig;
use super::super::{ApiConfig, AppState, api_router_with};
use crate::config::CloudRole;

const ORIGIN: &str = "https://figcraft.test";
const CLIENT_ID: &str = "figcraft-b1-test";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

type TestResult = Result<(), Box<dyn std::error::Error>>;

// ---------------------------------------------------------------- fixtures

async fn pool() -> Option<PgPool> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; auth route DB test skipped");
        return None;
    };
    let url = std::fs::read_to_string(file).ok()?;
    Some(sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await.unwrap())
}

fn unreachable_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").unwrap()
}

fn objects() -> figcraft_cloud_storage::objects::ObjectStore {
    figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".to_string(),
        region: "us-east-1".to_string(),
        bucket: "figcraft-test".to_string(),
        access_key: "FCtestkey".to_string(),
        secret_key: "FCtestsecret".to_string(),
        path_style: true,
    })
    .unwrap()
}

fn suffix() -> String {
    Uuid::new_v4().simple().to_string().chars().skip(20).collect()
}

fn d1(kind: &str) -> String {
    format!("d1{kind}0000-0000-4000-8000-{}", suffix())
}

async fn user(pool: &PgPool) -> PrincipalId {
    let id: PrincipalId = d1("a1").parse().unwrap();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd1 auth user')")
        .bind(id.to_string())
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn tenant_owned_by(pool: &PgPool, owner: PrincipalId) -> TenantId {
    let tenant: TenantId = d1("b1").parse().unwrap();
    let slug = format!("d1-auth-{}", suffix());
    let mut transaction = begin_tenant_transaction(pool, tenant).await.unwrap();
    create_tenant_with_owner(&mut transaction, NewTenant { slug: &slug, name: "d1 auth tenant", plan_id: "team", home_region: "local" }, owner)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    tenant
}

async fn bare_tenant(pool: &PgPool) -> TenantId {
    let tenant: TenantId = d1("b2").parse().unwrap();
    let mut transaction = begin_tenant_transaction(pool, tenant).await.unwrap();
    figcraft_cloud_storage::metadata::create_tenant(&mut transaction, &format!("d1-boot-{}", suffix()), "d1 bootstrap", "team", "local")
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    tenant
}

async fn document(pool: &PgPool, tenant: TenantId) -> CloudDocumentId {
    let workspace: WorkspaceId = d1("c1").parse().unwrap();
    let project: ProjectId = d1("c2").parse().unwrap();
    let document: CloudDocumentId = d1("c3").parse().unwrap();
    let mut transaction = begin_tenant_transaction(pool, tenant).await.unwrap();
    figcraft_cloud_storage::metadata::create_workspace(&mut transaction, workspace, "d1 ws", None).await.unwrap();
    figcraft_cloud_storage::metadata::create_project(&mut transaction, project, workspace, "d1 project", None).await.unwrap();
    figcraft_cloud_storage::metadata::create_document(&mut transaction, document, workspace, project, "d1 doc").await.unwrap();
    transaction.commit().await.unwrap();
    document
}

async fn session_for(pool: &PgPool, principal: PrincipalId) -> String {
    figcraft_cloud_storage::sessions::create_session(pool, principal, figcraft_cloud_storage::sessions::SessionLimits::default()).await.unwrap().token
}

// ---------------------------------------------------------------- app and requests

fn identity(deployment: DeploymentMode, single_tenant: Option<TenantId>) -> Arc<IdentityConfig> {
    Arc::new(IdentityConfig::new(deployment, single_tenant, Some(ORIGIN), Some(b"d1-test-signing-key-0123456789")).unwrap())
}

async fn whoami(caller: Authenticated) -> Json<Value> {
    let (kind, scopes, document) = match &caller.grant {
        Grant::Member => ("member", Vec::new(), None),
        Grant::ServiceAccount { scopes } => ("service_account", scopes.iter().map(|scope| scope.as_str()).collect(), None),
        Grant::Share { document_id, .. } => ("share", Vec::new(), Some(document_id.to_string())),
        Grant::Embed { document_id, .. } => ("embed", Vec::new(), Some(document_id.to_string())),
    };
    Json(json!({
        "principal": caller.principal.to_string(),
        "tenant": caller.tenant.tenant_id().to_string(),
        "role": figcraft_cloud_storage::memberships::role_str(caller.tenant.role()),
        "grant": kind,
        "scopes": scopes,
        "document": document,
    }))
}

async fn open_document(caller: Authenticated, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let id: CloudDocumentId = id.parse().map_err(|_| ApiError::not_found())?;
    caller.require_document(id)?;
    caller.require_scope(Scope::DocumentsRead)?;
    Ok(whoami(caller).await)
}

fn app(state: AppState) -> Router {
    let resources =
        Router::new().route("/tenants/{tenant}/whoami", get(whoami).post(whoami)).route("/documents/{id}", get(open_document).post(open_document));
    let api = api_router_with(state, super::routes(), resources);
    crate::health::app(CloudRole::Api, true, Some(api), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(20), 1 << 20))
}

/// [`app`], for tests whose local `app` shadows it.
fn app_for_replica(state: AppState) -> Router {
    app(state)
}

fn state(pool: PgPool, identity: Arc<IdentityConfig>) -> AppState {
    AppState::new(pool, objects(), ApiConfig::default()).with_identity(identity)
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }

    /// The value set for cookie `name` (and the whole `Set-Cookie` line).
    fn cookie(&self, name: &str) -> Option<(String, String)> {
        self.headers.get_all(header::SET_COOKIE).iter().filter_map(|value| value.to_str().ok()).find_map(|line| {
            let value = line.strip_prefix(&format!("{name}="))?;
            Some((value.split(';').next().unwrap_or("").to_string(), line.to_string()))
        })
    }
}

async fn call(app: &Router, method: Method, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Reply {
    let peer: SocketAddr = "192.0.2.50:4000".parse().unwrap();
    let mut request = Request::builder().method(method).uri(path).extension(ConnectInfo(peer));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = match body {
        Some(body) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Reply { status, headers, body }
}

fn session_cookie(token: &str) -> String {
    format!("__Host-figcraft_session={token}")
}

// ---------------------------------------------------------------- mock OIDC provider (ES256)

fn ec_key() -> &'static EcdsaKeyPair {
    static KEY: OnceLock<EcdsaKeyPair> = OnceLock::new();
    KEY.get_or_init(|| {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    })
}

struct CodeGrant {
    nonce: String,
    challenge: String,
    subject: String,
}

struct Mock {
    issuer: String,
    grants: Mutex<HashMap<String, CodeGrant>>,
}

fn id_token(mock: &Mock, grant: &CodeGrant) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let header = json!({"alg": "ES256", "kid": "ec-1", "typ": "JWT"});
    let claims =
        json!({"iss": mock.issuer, "sub": grant.subject, "aud": CLIENT_ID, "iat": now, "exp": now + 300, "nonce": grant.nonce, "name": "D1 Probe"});
    let input = format!("{}.{}", B64.encode(header.to_string()), B64.encode(claims.to_string()));
    let signature = ec_key().sign(&ring::rand::SystemRandom::new(), input.as_bytes()).unwrap();
    format!("{input}.{}", B64.encode(signature.as_ref()))
}

async fn start_mock() -> Arc<Mock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let mock = Arc::new(Mock { issuer, grants: Mutex::new(HashMap::new()) });
    let discovery = |State(mock): State<Arc<Mock>>| async move {
        Json(json!({
            "issuer": mock.issuer,
            "authorization_endpoint": format!("{}/authorize", mock.issuer),
            "token_endpoint": format!("{}/token", mock.issuer),
            "jwks_uri": format!("{}/jwks", mock.issuer),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["ES256"],
            "code_challenge_methods_supported": ["S256"],
        }))
    };
    let jwks = || async {
        let point = ec_key().public_key().as_ref();
        let (x, y) = (point.get(1..33).unwrap_or_default(), point.get(33..65).unwrap_or_default());
        Json(json!({"keys": [{"kty": "EC", "use": "sig", "alg": "ES256", "kid": "ec-1", "crv": "P-256", "x": B64.encode(x), "y": B64.encode(y)}]}))
    };
    let token = |State(mock): State<Arc<Mock>>, Form(form): Form<HashMap<String, String>>| async move {
        let refused = (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
        let (Some(code), Some(verifier)) = (form.get("code"), form.get("code_verifier")) else {
            return refused;
        };
        let Some(grant) = mock.grants.lock().unwrap().remove(code) else {
            return refused;
        };
        if B64.encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes())) != grant.challenge {
            return refused;
        }
        let id_token = id_token(&mock, &grant);
        (StatusCode::OK, Json(json!({"access_token": "d1-access", "token_type": "Bearer", "expires_in": 300, "id_token": id_token})))
    };
    let router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", axum::routing::post(token))
        .with_state(Arc::clone(&mock));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    mock
}

fn query_of(url: &str) -> HashMap<String, String> {
    url.split_once('?')
        .map(|(_, query)| query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- tests without a database

#[tokio::test]
async fn auth_routes_without_identity_or_oidc_are_unavailable() {
    let bare = app(AppState::new(unreachable_pool(), objects(), ApiConfig::default()));
    for path in ["/api/v1/auth/login", "/api/v1/auth/session"] {
        let reply = call(&bare, Method::GET, path, &[], None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"), "{path}");
    }
    // Identity but no OIDC provider: login is 503; session without a credential is 401 before
    // any lookup (no database is reachable here).
    let no_oidc = app(state(unreachable_pool(), identity(DeploymentMode::MultiTenant, None)));
    assert_eq!(call(&no_oidc, Method::GET, "/api/v1/auth/login", &[], None).await.status, StatusCode::SERVICE_UNAVAILABLE);
    let reply = call(&no_oidc, Method::GET, "/api/v1/auth/session", &[], None).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    // Malformed bearer values never reach storage.
    for value in ["Basic abc", "Bearer ", "Bearer fc_sa_wrong", "bearer fcsa_x"] {
        let reply = call(&no_oidc, Method::GET, "/api/v1/tenants/x/whoami", &[("authorization", value)], None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{value}");
    }
    // Every POST is same-origin only, before its body is looked at.
    for path in ["/api/v1/auth/logout", "/api/v1/auth/share/exchange"] {
        let reply = call(&no_oidc, Method::POST, path, &[("origin", "https://evil.example")], Some(json!({"token": "x"}))).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"), "{path}");
    }
    // Bootstrap completion does not exist in multi-tenant deployments.
    let reply = call(&no_oidc, Method::POST, "/api/v1/auth/bootstrap/complete", &[("origin", ORIGIN)], Some(json!({"token": "x"}))).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

fn logout_token(issuer: &str, audience: &str, subject: &str, jti: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let header = json!({"alg": "ES256", "kid": "ec-1", "typ": "logout+jwt"});
    let claims = json!({
        "iss": issuer, "aud": audience, "sub": subject, "sid": "d1-provider-session", "iat": now, "exp": now + 120, "jti": jti,
        "events": {"http://schemas.openid.net/event/backchannel-logout": {}},
    });
    let input = format!("{}.{}", B64.encode(header.to_string()), B64.encode(claims.to_string()));
    let signature = ec_key().sign(&ring::rand::SystemRandom::new(), input.as_bytes()).unwrap();
    format!("{input}.{}", B64.encode(signature.as_ref()))
}

/// `POST /api/v1/auth/backchannel-logout` with a form body (no Origin, no cookies: the
/// provider calls it server to server).
async fn post_logout_token(app: &Router, content_type: &str, body: String) -> StatusCode {
    let peer: SocketAddr = "192.0.2.51:4000".parse().unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/backchannel-logout")
        .extension(ConnectInfo(peer))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn backchannel_logout_ends_every_session_of_the_identity_and_reveals_nothing() -> TestResult {
    // Without a provider the route is unavailable, before the body is read.
    let no_oidc = app(state(unreachable_pool(), identity(DeploymentMode::MultiTenant, None)));
    assert_eq!(post_logout_token(&no_oidc, "application/x-www-form-urlencoded", "logout_token=x".into()).await, StatusCode::SERVICE_UNAVAILABLE);
    let Some(pool) = pool().await else {
        return Ok(());
    };
    let mock = start_mock().await;
    let config =
        OidcConfig { allow_insecure_loopback: true, ..OidcConfig::new(&mock.issuer, CLIENT_ID, None, "https://figcraft.test/api/v1/auth/callback") };
    let provider = OidcProvider::discover(config).await?;
    let issuer = provider.issuer().to_string();
    let app = app(state(pool.clone(), identity(DeploymentMode::MultiTenant, None)).with_oidc(Arc::new(provider)));

    let subject = format!("d1-bcl-{}", Uuid::new_v4());
    let resolved = figcraft_cloud_storage::oidc_logins::resolve_identity(&pool, &issuer, &subject, "D1 Back-channel", &json!({})).await?;
    let (first, second) = (session_for(&pool, resolved.principal_id).await, session_for(&pool, resolved.principal_id).await);
    let bystander = session_for(&pool, user(&pool).await).await;
    let alive = |token: String| {
        let app = app.clone();
        async move { call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &session_cookie(&token))], None).await.status == StatusCode::OK }
    };
    assert!(alive(first.clone()).await && alive(second.clone()).await);
    let form = |token: &str| format!("logout_token={token}");

    // Refused: not a form, no token, wrong audience, wrong issuer, unsigned.
    let good = logout_token(&issuer, CLIENT_ID, &subject, &format!("jti-{}", Uuid::new_v4()));
    assert_eq!(post_logout_token(&app, "application/json", form(&good)).await, StatusCode::BAD_REQUEST);
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded", "other=1".into()).await, StatusCode::BAD_REQUEST);
    let wrong_aud = logout_token(&issuer, "someone-else", &subject, &format!("jti-{}", Uuid::new_v4()));
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded", form(&wrong_aud)).await, StatusCode::BAD_REQUEST);
    let wrong_iss = logout_token("https://evil.example", CLIENT_ID, &subject, &format!("jti-{}", Uuid::new_v4()));
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded", form(&wrong_iss)).await, StatusCode::BAD_REQUEST);
    assert!(alive(first.clone()).await && alive(second.clone()).await, "refused tokens revoke nothing");

    // Accepted: both sessions of the identity end; another principal's session does not.
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded; charset=utf-8", form(&good)).await, StatusCode::OK);
    assert!(!alive(first.clone()).await && !alive(second.clone()).await);
    assert!(alive(bystander).await);
    let (reason,): (String,) = sqlx::query_as("SELECT revoke_reason FROM browser_sessions WHERE token_hash = $1")
        .bind(figcraft_cloud_storage::sessions::session_token_hash(&first).to_vec())
        .fetch_one(&pool)
        .await?;
    assert_eq!(reason, "logout");

    // The same token again is a replay: refused.
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded", form(&good)).await, StatusCode::BAD_REQUEST);
    // A valid token for an identity with no sessions, or no link at all: the same 200.
    let again = logout_token(&issuer, CLIENT_ID, &subject, &format!("jti-{}", Uuid::new_v4()));
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded", form(&again)).await, StatusCode::OK);
    let unknown = logout_token(&issuer, CLIENT_ID, &format!("d1-nobody-{}", Uuid::new_v4()), &format!("jti-{}", Uuid::new_v4()));
    assert_eq!(post_logout_token(&app, "application/x-www-form-urlencoded", form(&unknown)).await, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn rate_limit_still_applies_to_the_auth_routes() {
    let config =
        ApiConfig { auth_rate_limit: RateLimitConfig { burst: 2, refill_every: Duration::from_secs(60), max_keys: 100 }, ..ApiConfig::default() };
    let limited = app(AppState::new(unreachable_pool(), objects(), config).with_identity(identity(DeploymentMode::MultiTenant, None)));
    for _ in 0..2 {
        assert_eq!(call(&limited, Method::GET, "/api/v1/auth/login", &[], None).await.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    let reply = call(&limited, Method::GET, "/api/v1/auth/login", &[], None).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    assert!(reply.headers.contains_key(header::RETRY_AFTER));
    // Resource routes are not limited by the auth bucket.
    assert_eq!(call(&limited, Method::GET, "/api/v1/tenants/x/whoami", &[], None).await.status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------- database-backed tests

#[tokio::test]
async fn oidc_login_callback_session_logout_end_to_end() -> TestResult {
    let Some(pool) = pool().await else {
        return Ok(());
    };
    let mock = start_mock().await;
    let config =
        OidcConfig { allow_insecure_loopback: true, ..OidcConfig::new(&mock.issuer, CLIENT_ID, None, "https://figcraft.test/api/v1/auth/callback") };
    let provider = OidcProvider::discover(config).await?;
    let app = app(state(pool.clone(), identity(DeploymentMode::MultiTenant, None)).with_oidc(Arc::new(provider)));

    // A return_to off our origin is refused before anything is stored.
    let reply = call(&app, Method::GET, "/api/v1/auth/login?return_to=//evil.example/x", &[], None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let reply = call(&app, Method::GET, "/api/v1/auth/login?return_to=/files/recent", &[], None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let location = reply.headers.get(header::LOCATION).ok_or("no location")?.to_str()?.to_string();
    assert!(location.starts_with(&format!("{}/authorize?", mock.issuer)));
    let (state_cookie, line) = reply.cookie("__Host-figcraft_oidc").ok_or("no state cookie")?;
    assert!(line.contains("Max-Age=600") && line.contains("SameSite=Lax") && line.contains("Secure") && line.contains("HttpOnly"), "{line}");
    let query = query_of(&location);
    assert_eq!(query.get("state"), Some(&state_cookie));
    assert_eq!(query.get("code_challenge_method").map(String::as_str), Some("S256"));
    let code = format!("code-{}", Uuid::new_v4());
    let subject = format!("d1-{}", Uuid::new_v4());
    mock.grants.lock().map_err(|_| "poisoned")?.insert(
        code.clone(),
        CodeGrant {
            nonce: query.get("nonce").cloned().ok_or("nonce")?,
            challenge: query.get("code_challenge").cloned().ok_or("challenge")?,
            subject,
        },
    );

    // The state cookie must match the state parameter.
    let callback = format!("/api/v1/auth/callback?code={code}&state={state_cookie}");
    let wrong = format!("__Host-figcraft_oidc={}", "A".repeat(43));
    assert_eq!(call(&app, Method::GET, &callback, &[("cookie", &wrong)], None).await.status, StatusCode::UNAUTHORIZED);

    let oidc_cookie = format!("__Host-figcraft_oidc={state_cookie}");
    let reply = call(&app, Method::GET, &callback, &[("cookie", &oidc_cookie)], None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{:?}", reply.body);
    assert_eq!(reply.headers.get(header::LOCATION).ok_or("no location")?, "/files/recent");
    let (token, line) = reply.cookie("__Host-figcraft_session").ok_or("no session cookie")?;
    assert!(line.contains("Secure") && line.contains("HttpOnly") && line.contains("SameSite=Lax") && line.contains("Path=/"), "{line}");
    assert!(!line.contains("Domain"));
    assert!(reply.cookie("__Host-figcraft_oidc").is_some_and(|(value, _)| value.is_empty()));
    // The state works once.
    assert_eq!(call(&app, Method::GET, &callback, &[("cookie", &oidc_cookie)], None).await.status, StatusCode::UNAUTHORIZED);

    // Who am I: signed in, no tenant selected yet, then as owner of a new tenant.
    let cookie = session_cookie(&token);
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["credential"], "browser_session");
    assert_eq!(reply.body["tenant"], Value::Null);
    assert!(!reply.body.to_string().contains(&token), "the session reply must not echo the token");
    let principal: PrincipalId = reply.body["principal_id"].as_str().ok_or("principal")?.parse()?;
    let tenant = tenant_owned_by(&pool, principal).await;
    let tenant_text = tenant.to_string();
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie), (TENANT_HEADER, &tenant_text)], None).await;
    assert_eq!(reply.body["tenant"]["role"], "tenant_owner");
    let reply = call(&app, Method::GET, &format!("/api/v1/tenants/{tenant}/whoami"), &[("cookie", &cookie)], None).await;
    assert_eq!((reply.status, reply.body["grant"].as_str()), (StatusCode::OK, Some("member")));

    // A second login from the same browser revokes the old session (rotation on login).
    let reply = call(&app, Method::GET, "/api/v1/auth/login", &[], None).await;
    let location = reply.headers.get(header::LOCATION).ok_or("no location")?.to_str()?.to_string();
    let query = query_of(&location);
    let code = format!("code-{}", Uuid::new_v4());
    mock.grants.lock().map_err(|_| "poisoned")?.insert(
        code.clone(),
        CodeGrant {
            nonce: query.get("nonce").cloned().ok_or("nonce")?,
            challenge: query.get("code_challenge").cloned().ok_or("challenge")?,
            subject: format!("d1-{}", Uuid::new_v4()),
        },
    );
    let state2 = query.get("state").cloned().ok_or("state")?;
    let both = format!("__Host-figcraft_oidc={state2}; {cookie}");
    let reply = call(&app, Method::GET, &format!("/api/v1/auth/callback?code={code}&state={state2}"), &[("cookie", &both)], None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers.get(header::LOCATION).ok_or("no location")?, "/");
    let (second, _) = reply.cookie("__Host-figcraft_session").ok_or("no session cookie")?;
    assert_ne!(second, token);
    assert_eq!(call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await.status, StatusCode::UNAUTHORIZED);

    // Logout: cross-site refused, same-origin revokes and clears; the token is dead afterwards.
    let second_cookie = session_cookie(&second);
    assert_eq!(call(&app, Method::POST, "/api/v1/auth/logout", &[("cookie", &second_cookie)], None).await.status, StatusCode::FORBIDDEN);
    let reply = call(&app, Method::POST, "/api/v1/auth/logout", &[("cookie", &second_cookie), ("origin", ORIGIN)], None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(reply.cookie("__Host-figcraft_session").is_some_and(|(value, line)| value.is_empty() && line.contains("Max-Age=0")));
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &second_cookie)], None).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    Ok(())
}

#[tokio::test]
async fn cookie_requests_need_membership_and_same_origin_for_unsafe_methods() -> TestResult {
    let Some(pool) = pool().await else {
        return Ok(());
    };
    let app = app(state(pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let owner = user(&pool).await;
    let tenant = tenant_owned_by(&pool, owner).await;
    let cookie = session_cookie(&session_for(&pool, owner).await);
    let path = format!("/api/v1/tenants/{tenant}/whoami");

    assert_eq!(call(&app, Method::GET, &path, &[("cookie", &cookie)], None).await.status, StatusCode::OK);
    for headers in [vec![], vec![("origin", "https://evil.example")], vec![("origin", "null")], vec![("sec-fetch-site", "cross-site")]] {
        let mut all = vec![("cookie", cookie.as_str())];
        all.extend(headers.iter().copied());
        let reply = call(&app, Method::POST, &path, &all, None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"), "{headers:?}");
    }
    assert_eq!(call(&app, Method::POST, &path, &[("cookie", &cookie), ("origin", ORIGIN)], None).await.status, StatusCode::OK);
    assert_eq!(call(&app, Method::POST, &path, &[("cookie", &cookie), ("sec-fetch-site", "same-origin")], None).await.status, StatusCode::OK);

    // Another tenant, an unknown tenant and a disagreeing header all look the same: 403.
    let stranger = user(&pool).await;
    let other = tenant_owned_by(&pool, stranger).await;
    let unknown: TenantId = d1("ee").parse()?;
    for tenant_path in [format!("/api/v1/tenants/{other}/whoami"), format!("/api/v1/tenants/{unknown}/whoami")] {
        let reply = call(&app, Method::GET, &tenant_path, &[("cookie", &cookie)], None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"));
    }
    let other_text = other.to_string();
    assert_eq!(call(&app, Method::GET, &path, &[("cookie", &cookie), (TENANT_HEADER, &other_text)], None).await.status, StatusCode::FORBIDDEN);
    // No credential and a junk cookie are both 401.
    assert_eq!(call(&app, Method::GET, &path, &[], None).await.status, StatusCode::UNAUTHORIZED);
    let junk = session_cookie(&"x".repeat(43));
    assert_eq!(call(&app, Method::GET, &path, &[("cookie", &junk)], None).await.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn service_account_bearer_credentials_carry_their_scopes_and_tenant() -> TestResult {
    let Some(pool) = pool().await else {
        return Ok(());
    };
    let app = app(state(pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let owner = user(&pool).await;
    let tenant = tenant_owned_by(&pool, owner).await;
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let account = create_service_account(&mut transaction, "d1 robot", &[Scope::JobsRead], owner).await?;
    let credential = issue_credential(&mut transaction, account.id, 3600, owner).await?;
    transaction.commit().await?;
    let bearer = format!("Bearer {}", credential.secret);

    let path = format!("/api/v1/tenants/{tenant}/whoami");
    let reply = call(&app, Method::GET, &path, &[("authorization", &bearer)], None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!((reply.body["grant"].as_str(), reply.body["scopes"].clone()), (Some("service_account"), json!(["jobs:read"])));
    assert_eq!(reply.body["principal"], account.principal_id.to_string());
    // Bearer requests are not CSRF-checked (browsers never attach them on their own).
    assert_eq!(call(&app, Method::POST, &path, &[("authorization", &bearer)], None).await.status, StatusCode::OK);
    // Bound to its own tenant; scopes are enforced by the handler.
    let other = tenant_owned_by(&pool, owner).await;
    assert_eq!(
        call(&app, Method::GET, &format!("/api/v1/tenants/{other}/whoami"), &[("authorization", &bearer)], None).await.status,
        StatusCode::FORBIDDEN
    );
    let document = format!("/api/v1/documents/{}", d1("c3"));
    assert_eq!(call(&app, Method::GET, &document, &[("authorization", &bearer)], None).await.status, StatusCode::FORBIDDEN);
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("authorization", &bearer)], None).await;
    assert_eq!((reply.body["credential"].as_str(), reply.body["scopes"].clone()), (Some("service_account"), json!(["jobs:read"])));
    assert!(!reply.body.to_string().contains(&credential.secret));

    // Revocation ends it at once.
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert!(revoke_credential(&mut transaction, account.id, credential.id, owner).await?);
    transaction.commit().await?;
    assert_eq!(call(&app, Method::GET, &path, &[("authorization", &bearer)], None).await.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn share_exchange_mints_a_document_bound_session_that_revocation_ends() -> TestResult {
    let Some(pool) = pool().await else {
        return Ok(());
    };
    let app = app(state(pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let owner = user(&pool).await;
    let tenant = tenant_owned_by(&pool, owner).await;
    let doc = document(&pool, tenant).await;
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let link = create_share_link(&mut transaction, doc, ShareRole::Viewer, Some("d1"), owner, None).await?;
    transaction.commit().await?;

    let exchange = "/api/v1/auth/share/exchange";
    let body = json!({"token": link.secret});
    assert_eq!(call(&app, Method::POST, exchange, &[], Some(body.clone())).await.status, StatusCode::FORBIDDEN);
    let reply = call(&app, Method::POST, exchange, &[("origin", ORIGIN)], Some(json!({"token": "A".repeat(43)}))).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    let reply = call(&app, Method::POST, exchange, &[("origin", ORIGIN)], Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!((reply.body["document_id"].as_str(), reply.body["role"].as_str()), (Some(doc.to_string().as_str()), Some("viewer")));
    assert!(!reply.body.to_string().contains(&link.secret));
    let (share, line) = reply.cookie("__Host-figcraft_share").ok_or("no share cookie")?;
    assert!(line.contains("SameSite=Strict") && line.contains("HttpOnly") && line.contains("Secure"), "{line}");
    let max_age: i64 = line.split("Max-Age=").nth(1).and_then(|rest| rest.split(';').next()).ok_or("max-age")?.parse()?;
    assert!((3500..=3600).contains(&max_age), "{max_age}");
    assert!(!share.contains(&link.secret));

    let cookie = format!("__Host-figcraft_share={share}");
    let own = format!("/api/v1/documents/{doc}");
    let reply = call(&app, Method::GET, &own, &[("cookie", &cookie)], None).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!((reply.body["grant"].as_str(), reply.body["role"].as_str()), (Some("share"), Some("viewer")));
    assert_eq!(reply.body["principal"], owner.to_string(), "attributed to the link's creator");
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await;
    assert_eq!((reply.body["credential"].as_str(), reply.body["document"]["id"].as_str()), (Some("share_link"), Some(doc.to_string().as_str())));
    // Any other document or a tenant route is refused outright.
    for path in [format!("/api/v1/documents/{}", d1("c3")), format!("/api/v1/tenants/{tenant}/whoami")] {
        let reply = call(&app, Method::GET, &path, &[("cookie", &cookie)], None).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"), "{path}");
    }
    // Unsafe methods need the same origin.
    assert_eq!(call(&app, Method::POST, &own, &[("cookie", &cookie)], None).await.status, StatusCode::FORBIDDEN);
    // A forged cookie (role upgraded) is not a credential.
    let forged = cookie.replace(".viewer.", ".editor.");
    assert_eq!(call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &forged)], None).await.status, StatusCode::UNAUTHORIZED);

    // Revoking the link ends the session on the next request.
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert!(revoke_share_link(&mut transaction, link.id).await?);
    transaction.commit().await?;
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn bootstrap_required_gates_every_other_route_until_completed() -> TestResult {
    let Some(pool) = pool().await else {
        return Ok(());
    };
    let tenant = bare_tenant(&pool).await;
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let figcraft_cloud_storage::bootstrap::BootstrapStatus::Issued { token, .. } =
        figcraft_cloud_storage::bootstrap::ensure_bootstrap(&mut transaction, 3600).await?
    else {
        return Err("expected a bootstrap token".into());
    };
    transaction.commit().await?;
    let app = app(state(pool.clone(), identity(DeploymentMode::SingleTenant, Some(tenant))));
    let admin = user(&pool).await;
    let cookie = session_cookie(&session_for(&pool, admin).await);

    for (method, path) in [
        (Method::GET, format!("/api/v1/tenants/{tenant}/whoami")),
        (Method::GET, "/api/v1/no-such-route".to_string()),
        (Method::POST, "/api/v1/auth/share/exchange".to_string()),
    ] {
        let reply = call(&app, method, &path, &[("cookie", &cookie), ("origin", ORIGIN)], Some(json!({"token": "x"}))).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "bootstrap_required"), "{path}");
        assert!(reply.headers.get("x-request-id").is_some_and(|id| reply.body["error"]["request_id"] == id.to_str().unwrap_or("")));
    }
    // The bootstrap flow itself is served.
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await;
    assert_eq!((reply.status, reply.body["bootstrap_required"].as_bool()), (StatusCode::OK, Some(true)));
    assert_eq!(reply.body["tenant"], Value::Null);
    let complete = "/api/v1/auth/bootstrap/complete";
    assert_eq!(call(&app, Method::POST, complete, &[("cookie", &cookie)], Some(json!({"token": token}))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Method::POST, complete, &[("origin", ORIGIN)], Some(json!({"token": token}))).await.status, StatusCode::UNAUTHORIZED);
    let reply = call(&app, Method::POST, complete, &[("cookie", &cookie), ("origin", ORIGIN)], Some(json!({"token": "B".repeat(43)}))).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"));
    let reply = call(&app, Method::POST, complete, &[("cookie", &cookie), ("origin", ORIGIN)], Some(json!({"token": token}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["role"], "tenant_owner");
    // The session was rotated: the old cookie is dead, the new one is the owner's.
    let (rotated, _) = reply.cookie("__Host-figcraft_session").ok_or("no rotated cookie")?;
    assert_eq!(call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await.status, StatusCode::UNAUTHORIZED);
    let owner_cookie = session_cookie(&rotated);
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &owner_cookie)], None).await;
    assert_eq!((reply.body["tenant"]["role"].as_str(), reply.body["bootstrap_required"].as_bool()), (Some("tenant_owner"), Some(false)));
    let reply = call(&app, Method::GET, &format!("/api/v1/tenants/{tenant}/whoami"), &[("cookie", &owner_cookie)], None).await;
    assert_eq!((reply.status, reply.body["role"].as_str()), (StatusCode::OK, Some("tenant_owner")));
    // The token works once; and a fresh app (another replica) sees bootstrap as done.
    let reply = call(&app, Method::POST, complete, &[("cookie", &owner_cookie), ("origin", ORIGIN)], Some(json!({"token": token}))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let replica = app_for_replica(state(pool.clone(), identity(DeploymentMode::SingleTenant, Some(tenant))));
    assert_eq!(call(&replica, Method::GET, "/api/v1/no-such-route", &[], None).await.status, StatusCode::NOT_FOUND);
    Ok(())
}
