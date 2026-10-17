//! `/platform/v1` through the full middleware stack against the dev PostgreSQL (skipped without
//! `FIGCRAFT_TEST_DATABASE_URL_FILE`). Admin rows are global, so every test uses unique logins and
//! removes them.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use figcraft_cloud_auth::password::HashParams;
use figcraft_cloud_domain::{DeploymentMode, Uuid};
use figcraft_cloud_storage::platform::begin_platform_transaction;
use figcraft_cloud_storage::platform_admins::create_admin;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

use super::{PLATFORM_COOKIE, PlatformRuntime, totp};
use crate::api::authenticator::{IdentityConfig, now_unix};
use crate::api::context::ContextSettings;
use crate::api::passwords::PasswordService;
use crate::api::{ApiConfig, AppState};
use crate::config::CloudRole;

const ORIGIN: &str = "https://ops.figcraft.test";
const PASSWORD: &str = "violet glacier sunrise ledger 42";

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Option<PgPool> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; platform API probe skipped");
        return None;
    };
    let url = std::fs::read_to_string(file).ok()?;
    sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await.ok()
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
    .expect("object store")
}

fn generous() -> PlatformRuntime {
    let mut runtime = PlatformRuntime::new();
    let config = crate::api::rate_limit::RateLimitConfig { burst: 10_000, refill_every: Duration::from_millis(1), max_keys: 100 };
    runtime.public_limiter = crate::api::rate_limit::RateLimiter::new(config);
    runtime.api_limiter = crate::api::rate_limit::RateLimiter::new(config);
    runtime
}

fn state(pool: PgPool, runtime: PlatformRuntime) -> AppState {
    let identity =
        IdentityConfig::new(DeploymentMode::MultiTenant, None, Some(ORIGIN), Some(b"p1-platform-test-signing-key-0123")).expect("identity");
    let passwords = PasswordService::new(HashParams { memory_kib: 8 * 1024, passes: 1, lanes: 1 }, 2).expect("passwords");
    AppState::new(pool, objects(), ApiConfig::default())
        .with_identity(std::sync::Arc::new(identity))
        .with_passwords(passwords)
        .with_platform_runtime(runtime)
}

fn app(state: AppState) -> Router {
    let api = crate::api::api_router(state);
    crate::health::app(CloudRole::Api, true, Some(api), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(20), 1 << 20))
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

    fn set_cookie(&self) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|line| line.starts_with(&format!("{PLATFORM_COOKIE}=")))
            .map(str::to_owned)
    }

    /// The `name=value` pair to send back.
    fn cookie_pair(&self) -> Option<String> {
        self.set_cookie().map(|line| line.split(';').next().unwrap_or("").to_owned())
    }
}

static NEXT_PEER: AtomicU16 = AtomicU16::new(1);

async fn call(app: &Router, method: Method, path: &str, cookie: Option<&str>, body: Option<Value>) -> Reply {
    send(app, method, path, cookie, true, body).await
}

async fn send(app: &Router, method: Method, path: &str, cookie: Option<&str>, with_origin: bool, body: Option<Value>) -> Reply {
    let peer: SocketAddr = format!("192.0.2.{}:4000", NEXT_PEER.fetch_add(1, Ordering::Relaxed) % 250 + 1).parse().expect("peer");
    let mut request = Request::builder().method(method).uri(path).extension(ConnectInfo(peer));
    if with_origin {
        request = request.header(header::ORIGIN, ORIGIN);
    }
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let request = match body {
        Some(body) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).expect("request"),
        None => request.body(Body::empty()).expect("request"),
    };
    let response = app.clone().oneshot(request).await.expect("response");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply { status, headers, body }
}

fn base32_decode(text: &str) -> Vec<u8> {
    let (mut buffer, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for c in text.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => continue,
        };
        buffer = (buffer << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("byte"));
        }
    }
    out
}

fn code_for(secret: &[u8], offset: i64) -> String {
    let step = now_unix() / totp::STEP_SECS + offset;
    format!("{:06}", totp::code_at(secret, u64::try_from(step).expect("step")))
}

/// A new admin with [`PASSWORD`], created straight in storage.
async fn admin(pool: &PgPool, state: &AppState, prefix: &str) -> (Uuid, String) {
    let login = format!("{prefix}-{}@ops.example", &Uuid::new_v4().simple().to_string()[..12]);
    let hash = state.passwords().expect("passwords").hash(PASSWORD.to_owned(), Some(login.clone())).await.expect("hash");
    let mut tx = begin_platform_transaction(pool).await.expect("tx");
    let created = create_admin(&mut tx, None, &login, "P1 Operator", &hash).await.expect("admin");
    tx.commit().await.expect("commit");
    (created.id, login)
}

async fn cleanup(pool: &PgPool, ids: &[Uuid]) {
    let mut tx = begin_platform_transaction(pool).await.expect("tx");
    for id in ids {
        sqlx::query("DELETE FROM platform_admins WHERE id = $1::uuid").bind(id.to_string()).execute(tx.connection()).await.expect("delete");
    }
    tx.commit().await.expect("commit");
}

/// Sign in with the password and the second factor. `secret` is `None` on the first sign-in (the
/// admin enrols); `offset` picks the TOTP step so successive sign-ins use increasing steps.
async fn sign_in(app: &Router, login: &str, secret: Option<Vec<u8>>, offset: i64) -> (String, Vec<u8>) {
    let reply = call(app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": PASSWORD}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let cookie = reply.cookie_pair().expect("session cookie");
    let secret = match secret {
        Some(secret) => {
            assert_eq!(reply.body["next"], "totp");
            secret
        }
        None => {
            assert_eq!(reply.body["next"], "totp_enrol");
            let enrol = call(app, Method::POST, "/platform/v1/session/totp/enrol", Some(&cookie), None).await;
            assert_eq!(enrol.status, StatusCode::OK, "{:?}", enrol.body);
            assert!(enrol.body["otpauth_uri"].as_str().expect("uri").starts_with("otpauth://totp/"));
            base32_decode(enrol.body["secret"].as_str().expect("secret"))
        }
    };
    let verified = call(app, Method::POST, "/platform/v1/session/totp", Some(&cookie), Some(json!({"code": code_for(&secret, offset)}))).await;
    assert_eq!(verified.status, StatusCode::OK, "{:?}", verified.body);
    (cookie, secret)
}

#[tokio::test]
async fn an_admin_signs_in_with_password_then_totp_and_signs_out() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let state = state(pool.clone(), generous());
    let app = app(state.clone());
    let (id, login) = admin(&pool, &state, "flow").await;

    // Wrong password and unknown login are the same fixed 401.
    let wrong = call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": "not the password at all"}))).await;
    let unknown = call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": "nobody@ops.example", "password": PASSWORD}))).await;
    assert_eq!((wrong.status, wrong.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    assert_eq!((unknown.status, unknown.code()), (wrong.status, wrong.code()));
    assert_eq!(wrong.body["error"]["message"], unknown.body["error"]["message"]);

    let reply = call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": PASSWORD}))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!((reply.body["second_factor_done"].clone(), reply.body["next"].clone()), (json!(false), json!("totp_enrol")));
    let line = reply.set_cookie().expect("cookie");
    for part in ["Path=/platform/v1", "HttpOnly", "Secure", "SameSite=Strict", "Max-Age=14400"] {
        assert!(line.contains(part), "{line}");
    }
    assert!(!line.contains("Domain"));
    let cookie = reply.cookie_pair().expect("pair");

    // Before the second factor the session opens nothing but the second-factor steps.
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&cookie), None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, Method::GET, "/platform/v1/workspaces", Some(&cookie), None).await.status, StatusCode::UNAUTHORIZED);
    let me = call(&app, Method::GET, "/platform/v1/session", Some(&cookie), None).await;
    assert_eq!((me.status, me.body["next"].clone()), (StatusCode::OK, json!("totp_enrol")));
    assert_eq!(
        call(&app, Method::POST, "/platform/v1/session/totp", Some(&cookie), Some(json!({"code": "123456"}))).await.code(),
        "conflict",
        "no secret yet"
    );

    let enrol = call(&app, Method::POST, "/platform/v1/session/totp/enrol", Some(&cookie), None).await;
    assert_eq!(enrol.status, StatusCode::OK);
    let secret = base32_decode(enrol.body["secret"].as_str().ok_or("secret")?);
    assert_eq!(secret.len(), totp::SECRET_BYTES);
    let bad = call(&app, Method::POST, "/platform/v1/session/totp", Some(&cookie), Some(json!({"code": "000000"}))).await;
    assert_eq!((bad.status, bad.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    let good = call(&app, Method::POST, "/platform/v1/session/totp", Some(&cookie), Some(json!({"code": code_for(&secret, -1)}))).await;
    assert_eq!(good.status, StatusCode::OK);
    // The same code is single use.
    let replay = call(&app, Method::POST, "/platform/v1/session/totp", Some(&cookie), Some(json!({"code": code_for(&secret, -1)}))).await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);

    let admins = call(&app, Method::GET, "/platform/v1/admins", Some(&cookie), None).await;
    assert_eq!(admins.status, StatusCode::OK);
    assert!(admins.body["items"].as_array().ok_or("items")?.iter().any(|a| a["login"] == json!(login) && a["totp_enrolled"] == json!(true)));
    let me = call(&app, Method::GET, "/platform/v1/session", Some(&cookie), None).await;
    assert_eq!((me.body["second_factor_done"].clone(), me.body["next"].clone()), (json!(true), json!("none")));
    // Enrolment cannot be redone once confirmed.
    assert_eq!(call(&app, Method::POST, "/platform/v1/session/totp/enrol", Some(&cookie), None).await.status, StatusCode::CONFLICT);

    // A later sign-in goes straight to the code and needs a newer step.
    let (second, _) = sign_in(&app, &login, Some(secret.clone()), 0).await;
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&second), None).await.status, StatusCode::OK);

    let out = call(&app, Method::DELETE, "/platform/v1/session", Some(&cookie), None).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(out.set_cookie().expect("cleared").contains("Max-Age=0"));
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&cookie), None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&second), None).await.status, StatusCode::OK, "other sessions stay");

    // Every outcome is in the platform audit log, denials included, with no secrets.
    let mut tx = begin_platform_transaction(&pool).await?;
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT action, outcome, metadata::text FROM platform_audit_events WHERE actor_id = $1::uuid ORDER BY created_at, id")
            .bind(id.to_string())
            .fetch_all(tx.connection())
            .await?;
    let _ = tx.rollback().await;
    for (action, outcome) in [
        ("platform.login.password", "denied"),
        ("platform.login.password", "succeeded"),
        ("platform.totp.enrol", "succeeded"),
        ("platform.login.totp", "denied"),
        ("platform.login.totp", "succeeded"),
        ("platform.logout", "succeeded"),
    ] {
        assert!(rows.iter().any(|row| row.0 == action && row.1 == outcome), "missing {action} {outcome}: {rows:?}");
    }
    assert!(rows.iter().all(|row| !row.2.contains(PASSWORD) && !row.2.contains("secret")));
    cleanup(&pool, &[id]).await;
    Ok(())
}

#[tokio::test]
async fn state_changing_platform_requests_need_the_same_origin() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let state = state(pool.clone(), generous());
    let app = app(state.clone());
    let (id, login) = admin(&pool, &state, "csrf").await;
    let (cookie, _) = sign_in(&app, &login, None, -1).await;
    let body = || Some(json!({"login": "x@ops.example", "display_name": "X", "password": PASSWORD}));
    let foreign = send(&app, Method::POST, "/platform/v1/admins", Some(&cookie), false, body()).await;
    assert_eq!((foreign.status, foreign.code()), (StatusCode::FORBIDDEN, "forbidden"));
    assert_eq!(send(&app, Method::GET, "/platform/v1/admins", Some(&cookie), false, None).await.status, StatusCode::OK, "reads need no origin");
    cleanup(&pool, &[id]).await;
    Ok(())
}

#[tokio::test]
async fn a_workspace_session_cannot_reach_the_platform_and_a_platform_session_cannot_reach_a_workspace() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let state = state(pool.clone(), generous());
    let app = app(state.clone());
    let (id, login) = admin(&pool, &state, "realm").await;
    let (platform_cookie, _) = sign_in(&app, &login, None, -1).await;
    let platform_token = platform_cookie.split_once('=').ok_or("pair")?.1.to_owned();

    // A genuine workspace session of a genuine principal.
    let principal = Uuid::new_v4();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'p1 realm probe')")
        .bind(principal.to_string())
        .execute(&pool)
        .await?;
    let principal: figcraft_cloud_domain::PrincipalId = principal.try_into().map_err(|_| "nil")?;
    let session = figcraft_cloud_storage::sessions::create_session(&pool, principal, figcraft_cloud_storage::sessions::SessionLimits::default())
        .await
        .map_err(|e| format!("{e:?}"))?;
    for cookie in [
        format!("__Host-figcraft_session={}", session.token),
        format!("{PLATFORM_COOKIE}={}", session.token),
        format!("__Host-figcraft_session={platform_token}; {PLATFORM_COOKIE}={}", session.token),
    ] {
        for path in ["/platform/v1/admins", "/platform/v1/workspaces", "/platform/v1/audit", "/platform/v1/session"] {
            let reply = call(&app, Method::GET, path, Some(&cookie), None).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path} with a workspace credential");
        }
    }
    // The reverse: the platform token presented as a workspace session, in either cookie.
    for cookie in [format!("__Host-figcraft_session={platform_token}"), format!("{PLATFORM_COOKIE}={platform_token}")] {
        let reply = call(&app, Method::GET, "/api/v1/tenants", Some(&cookie), None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "workspace API with a platform credential");
    }
    // A service-account style bearer is not a platform credential either.
    let reply = {
        let peer: SocketAddr = "192.0.2.77:4000".parse()?;
        let request = Request::builder()
            .uri("/platform/v1/admins")
            .header(header::AUTHORIZATION, "Bearer fcsa_not-a-real-credential")
            .extension(ConnectInfo(peer))
            .body(Body::empty())?;
        app.clone().oneshot(request).await?.status()
    };
    assert_eq!(reply, StatusCode::UNAUTHORIZED);
    cleanup(&pool, &[id]).await;
    Ok(())
}

#[tokio::test]
async fn repeated_wrong_passwords_lock_the_admin_even_for_the_right_one() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let state = state(pool.clone(), generous());
    let app = app(state.clone());
    let (id, login) = admin(&pool, &state, "lock").await;
    for _ in 0..super::LOCK_AFTER_FAILURES {
        let reply =
            call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": "definitely the wrong one"}))).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let reply = call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": PASSWORD}))).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"), "locked");
    assert!(reply.cookie_pair().is_none());
    // Lifting the lock (as time would) lets the right password in again.
    let mut tx = begin_platform_transaction(&pool).await?;
    sqlx::query("UPDATE platform_admins SET locked_until = now() - interval '1 second' WHERE id = $1::uuid")
        .bind(id.to_string())
        .execute(tx.connection())
        .await?;
    tx.commit().await?;
    let reply = call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": PASSWORD}))).await;
    assert_eq!(reply.status, StatusCode::OK);
    cleanup(&pool, &[id]).await;
    Ok(())
}

#[tokio::test]
async fn sign_in_attempts_are_rate_limited_per_client() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let app = app(state(pool.clone(), PlatformRuntime::new()));
    let peer: SocketAddr = "198.51.100.9:4000".parse()?;
    let mut last = StatusCode::OK;
    for attempt in 0..8 {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/platform/v1/session")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, ORIGIN)
            .extension(ConnectInfo(peer))
            .body(Body::from(json!({"login": "nobody@ops.example", "password": PASSWORD}).to_string()))?;
        let response = app.clone().oneshot(request).await?;
        last = response.status();
        if attempt < 5 {
            assert_eq!(last, StatusCode::UNAUTHORIZED, "attempt {attempt}");
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    Ok(())
}

#[tokio::test]
async fn admins_are_created_disabled_and_enabled_by_other_admins() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let state = state(pool.clone(), generous());
    let app = app(state.clone());
    let (root_id, root_login) = admin(&pool, &state, "root").await;
    let (root, _) = sign_in(&app, &root_login, None, -1).await;

    let login = format!("made-{}@ops.example", &Uuid::new_v4().simple().to_string()[..12]);
    let weak =
        call(&app, Method::POST, "/platform/v1/admins", Some(&root), Some(json!({"login": login, "display_name": "Made", "password": "short"})))
            .await;
    assert_eq!((weak.status, weak.code()), (StatusCode::BAD_REQUEST, "invalid_request"));
    let created =
        call(&app, Method::POST, "/platform/v1/admins", Some(&root), Some(json!({"login": login, "display_name": "Made", "password": PASSWORD})))
            .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["state"], "active");
    assert!(created.body.get("password_hash").is_none() && created.body["has_password"] == json!(true));
    let made: Uuid = created.body["id"].as_str().ok_or("id")?.parse()?;
    let duplicate =
        call(&app, Method::POST, "/platform/v1/admins", Some(&root), Some(json!({"login": login, "display_name": "Made", "password": PASSWORD})))
            .await;
    assert_eq!((duplicate.status, duplicate.code()), (StatusCode::CONFLICT, "conflict"));

    let (made_cookie, secret) = sign_in(&app, &login, None, -1).await;
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&made_cookie), None).await.status, StatusCode::OK);

    // Disabling ends their sessions and sign-in; nobody can disable themselves.
    let own = call(&app, Method::POST, &format!("/platform/v1/admins/{root_id}/disable"), Some(&root), None).await;
    assert_eq!(own.status, StatusCode::FORBIDDEN);
    let disabled = call(&app, Method::POST, &format!("/platform/v1/admins/{made}/disable"), Some(&root), None).await;
    assert_eq!((disabled.status, disabled.body["state"].clone()), (StatusCode::OK, json!("disabled")));
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&made_cookie), None).await.status, StatusCode::UNAUTHORIZED);
    let refused = call(&app, Method::POST, "/platform/v1/session", None, Some(json!({"login": login, "password": PASSWORD}))).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    let enabled = call(&app, Method::POST, &format!("/platform/v1/admins/{made}/enable"), Some(&root), None).await;
    assert_eq!(enabled.body["state"], "active");
    let (again, _) = sign_in(&app, &login, Some(secret), 1).await;
    assert_eq!(call(&app, Method::GET, "/platform/v1/admins", Some(&again), None).await.status, StatusCode::OK);
    assert_eq!(
        call(&app, Method::POST, &format!("/platform/v1/admins/{}/disable", Uuid::new_v4()), Some(&root), None).await.status,
        StatusCode::NOT_FOUND
    );

    // The audit log names who did what.
    let audit = call(&app, Method::GET, "/platform/v1/audit?action_prefix=platform.admin&limit=50", Some(&root), None).await;
    assert_eq!(audit.status, StatusCode::OK);
    let items = audit.body["items"].as_array().ok_or("items")?;
    assert!(items.iter().any(|e| e["action"] == "platform.admin.create"
        && e["actor_id"] == json!(root_id.to_string())
        && e["resource_id"] == json!(made.to_string())));
    assert!(items.iter().any(|e| e["action"] == "platform.admin.disable" && e["resource_id"] == json!(made.to_string())));
    cleanup(&pool, &[made, root_id]).await;
    Ok(())
}

#[tokio::test]
async fn bootstrap_refuses_a_wrong_token_and_a_weak_password() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let app = app(state(pool.clone(), generous()));
    let weak = call(
        &app,
        Method::POST,
        "/platform/v1/bootstrap",
        None,
        Some(json!({"token": "x", "login": "boot@ops.example", "display_name": "Boot", "password": "short"})),
    )
    .await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);
    let wrong = call(
        &app,
        Method::POST,
        "/platform/v1/bootstrap",
        None,
        Some(json!({"token": "A".repeat(43), "login": "boot@ops.example", "display_name": "Boot", "password": PASSWORD})),
    )
    .await;
    assert_eq!((wrong.status, wrong.code()), (StatusCode::FORBIDDEN, "forbidden"));
    let unknown_field = call(
        &app,
        Method::POST,
        "/platform/v1/bootstrap",
        None,
        Some(json!({"token": "x", "login": "b@o.example", "display_name": "B", "password": PASSWORD, "admin": true})),
    )
    .await;
    assert_eq!(unknown_field.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn every_documented_operation_is_routed_and_the_document_is_served() -> TestResult {
    let Some(pool) = pool().await else { return Ok(()) };
    let app = app(state(pool.clone(), generous()));
    let served = call(&app, Method::GET, "/platform/v1/openapi.json", None, None).await;
    assert_eq!(served.status, StatusCode::OK);
    let doc: Value = serde_json::from_str(super::OPENAPI_JSON)?;
    assert_eq!(served.body, doc);
    assert_eq!(doc["openapi"], "3.1.0");
    let mut seen = 0;
    let mut ids = std::collections::BTreeSet::new();
    for (path, item) in doc["paths"].as_object().ok_or("paths")? {
        for (method, operation) in item.as_object().ok_or("item")? {
            assert!(ids.insert(operation["operationId"].as_str().ok_or("operationId")?.to_owned()), "operation ids are unique");
            let path = format!("/platform/v1{}", path.replace("{id}", &Uuid::new_v4().to_string()));
            let body = operation.get("requestBody").map(|_| json!({}));
            let reply = call(&app, method.to_uppercase().parse()?, &path, None, body).await;
            assert!(
                reply.status != StatusCode::NOT_FOUND && reply.status != StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is documented but not routed"
            );
            if !operation["security"].is_array() {
                assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {path} must need a platform session");
            }
            seen += 1;
        }
    }
    assert!(seen >= 20, "{seen}");
    Ok(())
}
