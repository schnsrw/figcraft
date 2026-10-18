//! Built-in account routes through the full middleware stack against the dev PostgreSQL (S6-034;
//! fixture prefix e7, skipped without `FIGCRAFT_TEST_DATABASE_URL_FILE` and
//! `FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE`). Every test makes fresh tenants, principals and
//! invitations, so reruns and parallel runs pass.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use figcraft_cloud_auth::password::HashParams;
use figcraft_cloud_domain::{DeploymentMode, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::account_settings::{AccountSettings, set_settings};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::bootstrap::{BootstrapStatus, ensure_bootstrap};
use figcraft_cloud_storage::invitations::create_invitation;
use figcraft_cloud_storage::teams::{self, TeamRole};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

use super::super::auth::Authenticated;
use super::super::authenticator::IdentityConfig;
use super::super::context::ContextSettings;
use super::super::passwords::PasswordService;
use super::super::rate_limit::RateLimitConfig;
use super::super::{ApiConfig, AppState, api_router_with};
use crate::config::CloudRole;

const ORIGIN: &str = "https://figcraft.test";
const GOOD: &str = "a perfectly good phrase 42";
const COOKIE: &str = "__Host-figcraft_session";

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

fn suffix() -> String {
    Uuid::new_v4().simple().to_string().chars().skip(20).collect()
}

fn e7(kind: &str) -> String {
    format!("e7{kind}0000-0000-4000-8000-{}", suffix())
}

async fn pools() -> Result<Option<(PgPool, PgPool)>, Error> {
    let (Ok(runtime), Ok(admin)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE")) else {
        eprintln!("FIGCRAFT_TEST_*_DATABASE_URL_FILE is unset; account route test skipped");
        return Ok(None);
    };
    let runtime = std::fs::read_to_string(runtime)?;
    let admin = std::fs::read_to_string(admin)?;
    let runtime = sqlx::postgres::PgPoolOptions::new().max_connections(8).connect(runtime.trim_end()).await?;
    let admin = sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(admin.trim_end()).await?;
    Ok(Some((runtime, admin)))
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

fn identity(deployment: DeploymentMode, single_tenant: Option<TenantId>) -> Arc<IdentityConfig> {
    Arc::new(IdentityConfig::new(deployment, single_tenant, Some(ORIGIN), Some(b"e7-test-signing-key-0123456789")).expect("identity"))
}

async fn whoami(caller: Authenticated) -> Json<Value> {
    Json(json!({
        "principal": caller.principal.to_string(),
        "tenant": caller.tenant.tenant_id().to_string(),
        "role": figcraft_cloud_storage::memberships::role_str(caller.tenant.role()),
    }))
}

/// The router: the auth routes under test, the account-settings resource routes and `whoami`.
fn app(state: AppState) -> Router {
    let auth = super::super::auth_routes::routes().merge(super::auth_routes());
    let auth = auth.merge(super::super::accounts_contract::contract_routes()).merge(super::super::admin_users_routes::auth_routes());
    let resources = Router::new()
        .route("/tenants/{tenant}/whoami", get(whoami))
        .merge(super::resource_routes())
        .merge(super::super::me_routes::self_service_routes())
        .merge(super::super::admin_users_routes::resource_routes())
        .merge(super::super::tenants_api::routes())
        .merge(super::super::history_api::history_routes());
    let api = api_router_with(state, auth, resources);
    crate::health::app(CloudRole::Api, true, Some(api), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(20), 1 << 20))
}

fn state(pool: PgPool, identity: Arc<IdentityConfig>) -> AppState {
    state_with(pool, identity, HashParams { memory_kib: 8 * 1024, passes: 1, lanes: 1 }, 1000)
}

/// [`state`] with chosen hash parameters and per-address burst (a fresh app is a fresh "replica":
/// its in-memory throttle starts empty while the database lockout persists).
fn state_with(pool: PgPool, identity: Arc<IdentityConfig>, params: HashParams, burst: u32) -> AppState {
    let config = ApiConfig { auth_rate_limit: RateLimitConfig { burst, ..RateLimitConfig::default() }, ..ApiConfig::default() };
    let passwords = PasswordService::new(params, 4).expect("passwords");
    AppState::new(pool, objects(), config).with_identity(identity).with_passwords(passwords)
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

    fn session(&self) -> Option<String> {
        self.headers.get_all(header::SET_COOKIE).iter().filter_map(|value| value.to_str().ok()).find_map(|line| {
            let value = line.strip_prefix(&format!("{COOKIE}="))?;
            Some(format!("{COOKIE}={}", value.split(';').next().unwrap_or("")))
        })
    }
}

async fn call(app: &Router, method: Method, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Reply {
    let peer: SocketAddr = "192.0.2.50:4000".parse().expect("peer");
    let mut request = Request::builder().method(method).uri(path).extension(ConnectInfo(peer));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = match body {
        Some(body) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())).expect("request"),
        None => request.body(Body::empty()).expect("request"),
    };
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Reply { status, headers, body }
}

async fn post(app: &Router, path: &str, body: Value) -> Reply {
    call(app, Method::POST, path, &[("origin", ORIGIN)], Some(body)).await
}

struct World {
    pool: PgPool,
    admin: PgPool,
    tenant: TenantId,
    owner: PrincipalId,
    team: Uuid,
}

async fn world() -> Result<Option<World>, Error> {
    let Some((pool, admin)) = pools().await? else { return Ok(None) };
    let owner: PrincipalId = e7("a1").parse()?;
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e7 owner')")
        .bind(owner.to_string())
        .execute(&admin)
        .await?;
    let tenant: TenantId = e7("b1").parse()?;
    let mut t = begin_tenant_transaction(&pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    let slug = format!("e7-{}", suffix());
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name: "e7 accounts", plan_id: "team", home_region: "local" }, owner)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let team = Uuid::parse_str(&e7("c1"))?;
    teams::create_team(&mut t, team, &format!("e7-t-{}", suffix()), "e7 team", owner).await.map_err(|e| format!("{e:?}"))?;
    t.commit().await?;
    Ok(Some(World { pool, admin, tenant, owner, team }))
}

impl World {
    async fn invite(&self, email: &str) -> Result<String, Error> {
        let mut t = begin_tenant_transaction(&self.pool, self.tenant).await.map_err(|e| format!("{e:?}"))?;
        let created = create_invitation(&mut t, self.team, email, TeamRole::Editor, self.owner, None).await.map_err(|e| format!("{e:?}"))?;
        t.commit().await?;
        Ok(created.token)
    }

    async fn owner_cookie(&self) -> Result<String, Error> {
        let token =
            figcraft_cloud_storage::sessions::create_session(&self.pool, self.owner, Default::default()).await.map_err(|e| format!("{e:?}"))?.token;
        Ok(format!("{COOKIE}={token}"))
    }

    async fn settings(&self, settings: AccountSettings) -> Result<(), Error> {
        let mut t = begin_tenant_transaction(&self.pool, self.tenant).await.map_err(|e| format!("{e:?}"))?;
        set_settings(&mut t, settings, self.owner).await.map_err(|e| format!("{e:?}"))?;
        t.commit().await?;
        Ok(())
    }

    async fn count(&self, sql: &'static str, bind: &str) -> Result<i64, Error> {
        Ok(sqlx::query_scalar(sql).bind(bind.to_string()).fetch_one(&self.admin).await?)
    }
}

fn email() -> String {
    format!("new.{}@example.test", suffix())
}

#[tokio::test]
async fn invitation_signup_creates_the_account_membership_and_session_atomically() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let address = email();
    let token = w.invite(&address).await?;
    let path = "/api/v1/auth/password/signup";

    // CSRF: same-origin only.
    let body = json!({"invitation": token, "display_name": "Ada Lovelace", "password": GOOD});
    assert_eq!(call(&app, Method::POST, path, &[], Some(body.clone())).await.status, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Method::POST, path, &[("origin", "https://evil.test")], Some(body.clone())).await.status, StatusCode::FORBIDDEN);
    // Naming both doors (an invitation and a tenant) is a mistake.
    assert_eq!(
        post(&app, path, json!({"invitation": token, "tenant": w.tenant.to_string(), "display_name": "A", "password": GOOD})).await.status,
        StatusCode::BAD_REQUEST
    );
    // A password that breaks the policy is explained and creates nothing.
    let reply = post(&app, path, json!({"invitation": token, "display_name": "Ada", "password": "short"})).await;
    assert_eq!((reply.status, reply.body["error"]["reason"].as_str()), (StatusCode::BAD_REQUEST, Some("password_policy")), "{:?}", reply.body);
    assert!(reply.body["error"]["message"].as_str().is_some_and(|m| m.contains("12")));
    assert_eq!(w.count("SELECT count(*) FROM local_credentials WHERE tenant_id = $1::uuid", &w.tenant.to_string()).await?, 0);
    // A bad display name likewise.
    assert_eq!(post(&app, path, json!({"invitation": token, "display_name": "  ", "password": GOOD})).await.status, StatusCode::BAD_REQUEST);

    let reply = post(&app, path, body.clone()).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    let principal = reply.body["principal_id"].as_str().ok_or("principal")?.to_string();
    assert_eq!(reply.body["tenant_id"], w.tenant.to_string());
    let cookie = reply.session().ok_or("no session cookie")?;

    // The new person is signed in as a member (editor) of the tenant, the level the invited team role implies.
    let reply = call(&app, Method::GET, &format!("/api/v1/tenants/{}/whoami", w.tenant), &[("cookie", &cookie)], None).await;
    assert_eq!(
        (reply.status, reply.body["role"].as_str(), reply.body["principal"].as_str()),
        (StatusCode::OK, Some("editor"), Some(principal.as_str()))
    );
    assert_eq!(w.count("SELECT count(*) FROM team_members WHERE principal_id = $1::uuid AND state = 'active'", &principal).await?, 1);
    // The login is the invited address; only an Argon2id hash is stored.
    let (login, hash): (String, String) = sqlx::query_as("SELECT login, password_hash FROM local_credentials WHERE principal_id = $1::uuid")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(login, address);
    assert!(hash.starts_with("$argon2id$v=19$") && !hash.contains(GOOD));
    // Audited, and nothing sensitive in the trail.
    let mut actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE tenant_id = $1::uuid AND actor_principal_id = $2::uuid")
        .bind(w.tenant.to_string())
        .bind(&principal)
        .fetch_all(&w.admin)
        .await?;
    actions.sort();
    assert_eq!(actions, ["account.register", "credential.create", "invitation.accept"]);
    let leaked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND metadata::text ~* 'argon2|perfectly|example.test'")
            .bind(w.tenant.to_string())
            .fetch_one(&w.admin)
            .await?;
    assert_eq!(leaked, 0);

    // The token was single use.
    let again = post(&app, path, json!({"invitation": token, "display_name": "Mallory", "password": GOOD})).await;
    assert_eq!((again.status, again.code()), (StatusCode::NOT_FOUND, "not_found"));
    Ok(())
}

#[tokio::test]
async fn invitation_misuse_is_one_fixed_refusal() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let path = "/api/v1/auth/password/signup";
    let attempt = |token: String| json!({"invitation": token, "display_name": "Eve", "password": GOOD});

    let expired = w.invite(&email()).await?;
    sqlx::query("UPDATE team_invitations SET created_at = now() - interval '2 minutes', expires_at = now() - interval '1 minute' WHERE tenant_id = $1::uuid AND token_hash = $2")
        .bind(w.tenant.to_string())
        .bind(figcraft_cloud_storage::invitations::invitation_token_hash(&expired).to_vec())
        .execute(&w.admin)
        .await?;
    let revoked = w.invite(&email()).await?;
    sqlx::query("UPDATE team_invitations SET revoked_at = now() WHERE tenant_id = $1::uuid AND token_hash = $2")
        .bind(w.tenant.to_string())
        .bind(figcraft_cloud_storage::invitations::invitation_token_hash(&revoked).to_vec())
        .execute(&w.admin)
        .await?;
    let archived_team = w.invite(&email()).await?;
    sqlx::query("UPDATE teams SET state = 'archived' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(w.tenant.to_string())
        .bind(w.team.to_string())
        .execute(&w.admin)
        .await?;
    for token in [expired, revoked, archived_team, "A".repeat(43), "not a token".to_string(), String::new(), "A".repeat(500)] {
        let reply = post(&app, path, attempt(token.clone())).await;
        let want = if token.is_empty() || token.len() > 128 { StatusCode::BAD_REQUEST } else { StatusCode::NOT_FOUND };
        assert_eq!(reply.status, want, "{token}");
    }
    assert_eq!(w.count("SELECT count(*) FROM local_credentials WHERE tenant_id = $1::uuid", &w.tenant.to_string()).await?, 0);

    // A token of one tenant never creates anything in another: the credential is the token's tenant's.
    let other = world().await?.ok_or("world")?;
    sqlx::query("UPDATE teams SET state = 'active' WHERE tenant_id = $1::uuid").bind(w.tenant.to_string()).execute(&w.admin).await?;
    let token = w.invite(&email()).await?;
    let reply = post(&app, path, attempt(token)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let principal = reply.body["principal_id"].as_str().ok_or("principal")?;
    let elsewhere: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_members WHERE principal_id = $1::uuid AND tenant_id = $2::uuid")
        .bind(principal)
        .bind(other.tenant.to_string())
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(elsewhere, 0);

    // A tenant that turned passwords off refuses the sign-up and keeps the invitation usable.
    w.settings(AccountSettings { password_enabled: false, ..AccountSettings::default() }).await?;
    let token = w.invite(&email()).await?;
    let reply = post(&app, path, attempt(token.clone())).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"));
    w.settings(AccountSettings::default()).await?;
    assert_eq!(post(&app, path, attempt(token)).await.status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn concurrent_signups_with_one_invitation_create_exactly_one_account() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let token = w.invite(&email()).await?;
    let mut tasks = Vec::new();
    for n in 0..6 {
        let (app, token) = (app.clone(), token.clone());
        tasks.push(tokio::spawn(async move {
            post(&app, "/api/v1/auth/password/signup", json!({"invitation": token, "display_name": format!("Racer {n}"), "password": GOOD}))
                .await
                .status
        }));
    }
    let mut created = 0;
    for task in tasks {
        match task.await? {
            StatusCode::CREATED => created += 1,
            StatusCode::NOT_FOUND => {}
            other => return Err(format!("unexpected {other}").into()),
        }
    }
    assert_eq!(created, 1);
    assert_eq!(w.count("SELECT count(*) FROM local_credentials WHERE tenant_id = $1::uuid", &w.tenant.to_string()).await?, 1);
    // The losers left no stray principals behind.
    let strays: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM principals WHERE display_name LIKE 'Racer %' AND created_at > now() - interval '5 minutes' AND id NOT IN (SELECT principal_id FROM local_credentials)",
    )
    .fetch_one(&w.admin)
    .await?;
    assert_eq!(strays, 0);
    Ok(())
}

#[tokio::test]
async fn open_registration_is_off_by_default_and_admin_controlled() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let path = "/api/v1/auth/password/signup";
    let tenant = w.tenant.to_string();
    let open = |login: &str| json!({"tenant": tenant, "login": login, "display_name": "Open Person", "password": GOOD});

    // Off by default: refused before any hashing, nothing created.
    assert_eq!(post(&app, path, open("open@example.test")).await.status, StatusCode::FORBIDDEN);
    let reply = call(&app, Method::GET, &format!("/api/v1/auth/methods?tenant={tenant}"), &[], None).await;
    assert_eq!(
        (reply.status, reply.body["password"].as_bool(), reply.body["open_registration"].as_bool()),
        (StatusCode::OK, Some(true), Some(false))
    );

    // Only a tenant administrator may change the settings; an ordinary member may only read.
    let owner = w.owner_cookie().await?;
    let settings_path = format!("/api/v1/tenants/{tenant}/account-settings");
    let reply = call(&app, Method::GET, &settings_path, &[("cookie", &owner)], None).await;
    assert_eq!((reply.status, reply.body["open_registration"].as_bool()), (StatusCode::OK, Some(false)));
    let wanted = json!({"password_enabled": true, "oidc_enabled": true, "open_registration": true, "require_totp": false});
    let viewer = {
        let token = w.invite(&email()).await?;
        post(&app, "/api/v1/auth/password/signup", json!({"invitation": token, "display_name": "Viewer", "password": GOOD}))
            .await
            .session()
            .ok_or("viewer cookie")?
    };
    let reply = call(&app, Method::PUT, &settings_path, &[("cookie", &viewer), ("origin", ORIGIN)], Some(wanted.clone())).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Method::PUT, &settings_path, &[("cookie", &owner)], Some(wanted.clone())).await.status, StatusCode::FORBIDDEN, "CSRF");
    // Invalid combinations are refused.
    for bad in [
        json!({"password_enabled": false, "oidc_enabled": false, "open_registration": false, "require_totp": false}),
        json!({"password_enabled": false, "oidc_enabled": true, "open_registration": true, "require_totp": false}),
    ] {
        assert_eq!(
            call(&app, Method::PUT, &settings_path, &[("cookie", &owner), ("origin", ORIGIN)], Some(bad)).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    let reply = call(&app, Method::PUT, &settings_path, &[("cookie", &owner), ("origin", ORIGIN)], Some(wanted)).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = 'tenant.account_settings'")
        .bind(&tenant)
        .fetch_one(&w.admin)
        .await?;
    assert!(audited >= 1);

    // Now anyone may register: a viewer with no team; a taken login is a conflict; a policy failure explains itself.
    let reply = post(&app, path, open("Open@Example.test")).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    let cookie = reply.session().ok_or("cookie")?;
    let reply = call(&app, Method::GET, &format!("/api/v1/tenants/{tenant}/whoami"), &[("cookie", &cookie)], None).await;
    assert_eq!(reply.body["role"], "viewer");
    assert_eq!(post(&app, path, open("open@example.test")).await.status, StatusCode::CONFLICT);
    let reply = post(&app, path, json!({"tenant": tenant, "login": "other@example.test", "display_name": "O", "password": "password1234"})).await;
    assert_eq!((reply.status, reply.body["error"]["reason"].as_str()), (StatusCode::BAD_REQUEST, Some("password_policy")));
    // The login may not be the password.
    let reply = post(&app, path, json!({"tenant": tenant, "login": "same-as-password!", "display_name": "O", "password": "same-as-password!"})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    // A tenant that does not exist offers no registration.
    assert_eq!(
        post(&app, path, json!({"tenant": Uuid::new_v4().to_string(), "login": "x@y.zz", "display_name": "O", "password": GOOD})).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(call(&app, Method::GET, "/api/v1/auth/methods?tenant=no-such-slug", &[], None).await.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_self_hoster_bootstraps_with_a_password_and_no_identity_provider() -> TestResult {
    let Some((pool, admin)) = pools().await? else { return Ok(()) };
    let tenant: TenantId = e7("b2").parse()?;
    let mut t = begin_tenant_transaction(&pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    figcraft_cloud_storage::metadata::create_tenant(&mut t, &format!("e7-boot-{}", suffix()), "e7 bootstrap", "team", "local")
        .await
        .map_err(|e| format!("{e:?}"))?;
    let BootstrapStatus::Issued { token, .. } = ensure_bootstrap(&mut t, 3600).await.map_err(|e| format!("{e:?}"))? else {
        return Err("expected a token".into());
    };
    t.commit().await?;
    let app = app(state(pool.clone(), identity(DeploymentMode::SingleTenant, Some(tenant))));
    let path = "/api/v1/auth/password/bootstrap";
    let body = |token: &str, login: &str| json!({"token": token, "login": login, "display_name": "Site Owner", "password": GOOD});

    // While bootstrap is pending the sign-in methods are readable (no provider is configured).
    let reply = call(&app, Method::GET, "/api/v1/auth/methods", &[], None).await;
    assert_eq!((reply.status, reply.body["password"].as_bool(), reply.body["oidc"].as_bool()), (StatusCode::OK, Some(true), Some(false)));
    assert_eq!(reply.body["bootstrap_required"], true, "the sign-in screen can tell first-run from sign-in");
    // CSRF, a wrong token and a weak password create nothing.
    assert_eq!(call(&app, Method::POST, path, &[], Some(body(&token, "owner@example.test"))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(post(&app, path, body(&"B".repeat(43), "owner@example.test")).await.status, StatusCode::FORBIDDEN);
    let reply = post(&app, path, json!({"token": token, "login": "owner@example.test", "display_name": "O", "password": "short"})).await;
    assert_eq!(reply.body["error"]["reason"].as_str(), Some("password_policy"));
    let credentials: i64 =
        sqlx::query_scalar("SELECT count(*) FROM local_credentials WHERE tenant_id = $1::uuid").bind(tenant.to_string()).fetch_one(&admin).await?;
    assert_eq!(credentials, 0, "a refused bootstrap leaves no account behind");

    let reply = post(&app, path, body(&token, "Owner@Example.test")).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["role"], "tenant_owner");
    let cookie = reply.session().ok_or("cookie")?;
    let reply = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &cookie)], None).await;
    assert_eq!((reply.body["tenant"]["role"].as_str(), reply.body["bootstrap_required"].as_bool()), (Some("tenant_owner"), Some(false)));
    let reply = call(&app, Method::GET, "/api/v1/auth/methods", &[], None).await;
    assert_eq!(reply.body["bootstrap_required"], false);
    // The token worked once.
    assert_eq!(post(&app, path, body(&token, "second@example.test")).await.status, StatusCode::FORBIDDEN);
    let credentials: i64 =
        sqlx::query_scalar("SELECT count(*) FROM local_credentials WHERE tenant_id = $1::uuid").bind(tenant.to_string()).fetch_one(&admin).await?;
    assert_eq!(credentials, 1);
    // A multi-tenant deployment has no bootstrap route.
    let multi = self::app(state(pool, identity(DeploymentMode::MultiTenant, None)));
    assert_eq!(post(&multi, path, body(&token, "third@example.test")).await.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn the_contract_is_served_unauthenticated_and_matches_the_checked_in_document() -> TestResult {
    let Some((pool, _)) = pools().await? else { return Ok(()) };
    let app = app(state(pool, identity(DeploymentMode::MultiTenant, None)));
    let peer: SocketAddr = "192.0.2.50:4000".parse()?;
    let request = Request::builder().uri("/api/v1/auth/openapi.json").extension(ConnectInfo(peer)).body(Body::empty())?;
    let response = app.oneshot(request).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 22).await?;
    assert_eq!(bytes, super::super::accounts_contract::OPENAPI_JSON.as_bytes());
    Ok(())
}

// ------------------------------------------------------------------------------------ S6-035

const CHEAP: HashParams = HashParams { memory_kib: 8 * 1024, passes: 1, lanes: 1 };

/// A password account in `w`'s tenant, made through the invitation route; returns its login and
/// principal.
async fn account(w: &World, app: &Router) -> Result<(String, String), Error> {
    let login = email();
    let token = w.invite(&login).await?;
    let reply = post(app, "/api/v1/auth/password/signup", json!({"invitation": token, "display_name": "Login Person", "password": GOOD})).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    Ok((login, reply.body["principal_id"].as_str().ok_or("principal")?.to_string()))
}

fn login_body(w: &World, login: &str, password: &str) -> Value {
    json!({"tenant": w.tenant.to_string(), "login": login, "password": password})
}

const LOGIN: &str = "/api/v1/auth/password/login";

#[tokio::test]
async fn password_login_signs_in_rotates_the_session_and_is_audited() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let id = identity(DeploymentMode::MultiTenant, None);
    let app = app(state(w.pool.clone(), id));
    let (login, principal) = account(&w, &app).await?;

    // CSRF.
    assert_eq!(call(&app, Method::POST, LOGIN, &[], Some(login_body(&w, &login, GOOD))).await.status, StatusCode::FORBIDDEN);
    // A browser that already holds a session: it is revoked by the sign-in, the new cookie works.
    let old = w.owner_cookie().await?;
    let reply = call(&app, Method::POST, LOGIN, &[("origin", ORIGIN), ("cookie", &old)], Some(login_body(&w, &login.to_uppercase(), GOOD))).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["principal_id"], principal);
    let set_cookie =
        reply.headers.get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find(|v| v.starts_with(COOKIE)).ok_or("cookie")?.to_string();
    assert!(set_cookie.contains("Secure") && set_cookie.contains("HttpOnly") && set_cookie.contains("Path=/"), "{set_cookie}");
    let fresh = reply.session().ok_or("cookie")?;
    let whoami = format!("/api/v1/tenants/{}/whoami", w.tenant);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &old)], None).await.status, StatusCode::UNAUTHORIZED, "the previous session is gone");
    let reply = call(&app, Method::GET, &whoami, &[("cookie", &fresh)], None).await;
    assert_eq!((reply.status, reply.body["principal"].as_str()), (StatusCode::OK, Some(principal.as_str())));
    // Signing in again gives another distinct session.
    let again = post(&app, LOGIN, login_body(&w, &login, GOOD)).await;
    assert_ne!(again.session(), Some(fresh.clone()));
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = 'auth.password_login' AND outcome = 'succeeded' AND actor_principal_id = $2::uuid",
    )
    .bind(w.tenant.to_string())
    .bind(&principal)
    .fetch_one(&w.admin)
    .await?;
    assert_eq!(audited, 2);
    Ok(())
}

#[tokio::test]
async fn unknown_and_known_accounts_are_indistinguishable() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, _) = account(&w, &app).await?;
    let strip = |mut reply: Reply| {
        reply.body["error"]["request_id"] = Value::Null;
        (reply.status, reply.body, reply.headers.get(header::SET_COOKIE).is_some())
    };
    let wrong = strip(post(&app, LOGIN, login_body(&w, &login, "not the right password")).await);
    let unknown = strip(post(&app, LOGIN, login_body(&w, "nobody@example.test", "not the right password")).await);
    let malformed = strip(post(&app, LOGIN, login_body(&w, "no body \u{0} here", "not the right password")).await);
    let other_tenant = strip(post(&app, LOGIN, json!({"tenant": Uuid::new_v4().to_string(), "login": login, "password": GOOD})).await);
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong, unknown);
    assert_eq!(wrong, malformed);
    assert_eq!(wrong, other_tenant);
    // Timing: every path does one Argon2 computation (loose bound; the unit test in password.rs is tighter).
    let time = |login: String, tenant: String| {
        let app = app.clone();
        async move {
            let start = std::time::Instant::now();
            for _ in 0..3 {
                post(&app, LOGIN, json!({"tenant": tenant, "login": login, "password": "not the right password"})).await;
            }
            start.elapsed()
        }
    };
    let known_time = time(login.clone(), w.tenant.to_string()).await;
    let unknown_time = time("ghost@example.test".to_string(), w.tenant.to_string()).await;
    let (low, high) = if known_time < unknown_time { (known_time, unknown_time) } else { (unknown_time, known_time) };
    assert!(high.as_secs_f64() < low.as_secs_f64() * 4.0 + 0.2, "{known_time:?} vs {unknown_time:?}");
    Ok(())
}

#[tokio::test]
async fn failures_lock_the_account_across_replicas_without_revealing_it() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let id = identity(DeploymentMode::MultiTenant, None);
    let replica_a = app(state(w.pool.clone(), id.clone()));
    let replica_b = app(state(w.pool.clone(), id.clone()));
    let replica_c = app(state(w.pool.clone(), id));
    let (login, principal) = account(&w, &replica_a).await?;

    // Five wrong passwords: five 401s, then the per-login backoff answers 429 on this replica for
    // a known and an unknown login alike.
    for _ in 0..5 {
        assert_eq!(post(&replica_a, LOGIN, login_body(&w, &login, "wrong wrong wrong 1")).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(post(&replica_a, LOGIN, login_body(&w, "ghost@example.test", "wrong wrong wrong 1")).await.status, StatusCode::UNAUTHORIZED);
    }
    for who in [login.as_str(), "ghost@example.test"] {
        let reply = post(&replica_a, LOGIN, login_body(&w, who, GOOD)).await;
        assert_eq!((reply.status, reply.code()), (StatusCode::TOO_MANY_REQUESTS, "rate_limited"), "{who}");
        assert!(reply.headers.get(header::RETRY_AFTER).is_some());
    }
    // Another replica has an empty table, but the database lock holds: even the right password is
    // refused with the same 401 a wrong one gets.
    let locked = post(&replica_b, LOGIN, login_body(&w, &login, GOOD)).await;
    assert_eq!((locked.status, locked.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    assert!(locked.session().is_none());
    let (attempts, locked_for): (i32, bool) =
        sqlx::query_as("SELECT failed_attempts, locked_until > now() FROM local_credentials WHERE principal_id = $1::uuid")
            .bind(&principal)
            .fetch_one(&w.admin)
            .await?;
    assert_eq!((attempts, locked_for), (5, true), "attempts while locked are not counted");
    // When the lock lapses the right password works and clears the counters.
    sqlx::query("UPDATE local_credentials SET locked_until = now() - interval '1 second' WHERE principal_id = $1::uuid")
        .bind(&principal)
        .execute(&w.admin)
        .await?;
    assert_eq!(post(&replica_c, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    let attempts: i32 = sqlx::query_scalar("SELECT failed_attempts FROM local_credentials WHERE principal_id = $1::uuid")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(attempts, 0);
    Ok(())
}

#[tokio::test]
async fn the_per_address_bucket_limits_attempts_whatever_the_login() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state_with(w.pool.clone(), identity(DeploymentMode::MultiTenant, None), CHEAP, 3));
    let mut statuses = Vec::new();
    for n in 0..5 {
        statuses.push(post(&app, LOGIN, login_body(&w, &format!("guess{n}@example.test"), "wrong wrong wrong 1")).await.status);
    }
    assert_eq!(statuses, [401, 401, 401, 429, 429].map(|code| StatusCode::from_u16(code).expect("status")));
    Ok(())
}

#[tokio::test]
async fn disabled_suspended_and_sso_only_accounts_are_refused() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, principal) = account(&w, &app).await?;
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);

    // A disabled credential is refused with the generic 401, even with the right password.
    sqlx::query("UPDATE local_credentials SET state = 'disabled' WHERE principal_id = $1::uuid").bind(&principal).execute(&w.admin).await?;
    let reply = post(&app, LOGIN, login_body(&w, &login, GOOD)).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::UNAUTHORIZED, "unauthenticated"));
    sqlx::query("UPDATE local_credentials SET state = 'active' WHERE principal_id = $1::uuid").bind(&principal).execute(&w.admin).await?;
    // A suspended member likewise.
    sqlx::query("UPDATE tenant_members SET state = 'suspended' WHERE principal_id = $1::uuid").bind(&principal).execute(&w.admin).await?;
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::UNAUTHORIZED);
    sqlx::query("UPDATE tenant_members SET state = 'active' WHERE principal_id = $1::uuid").bind(&principal).execute(&w.admin).await?;
    // Neither counted as password guesses.
    let attempts: i32 = sqlx::query_scalar("SELECT failed_attempts FROM local_credentials WHERE principal_id = $1::uuid")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(attempts, 0);
    // An SSO-only tenant refuses password sign-in outright, and says so on the methods endpoint.
    w.settings(AccountSettings { password_enabled: false, ..AccountSettings::default() }).await?;
    let reply = post(&app, LOGIN, login_body(&w, &login, GOOD)).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"));
    let methods = call(&app, Method::GET, &format!("/api/v1/auth/methods?tenant={}", w.tenant), &[], None).await;
    assert_eq!(methods.body["password"], false);
    w.settings(AccountSettings::default()).await?;
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    // Another tenant's login with the same name has no standing here.
    let other = world().await?.ok_or("world")?;
    assert_eq!(post(&app, LOGIN, login_body(&other, &login, GOOD)).await.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn a_weaker_stored_hash_is_upgraded_at_sign_in() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let id = identity(DeploymentMode::MultiTenant, None);
    let old = app(state_with(w.pool.clone(), id.clone(), CHEAP, 1000));
    let (login, principal) = account(&w, &old).await?;
    let before: String =
        sqlx::query_scalar("SELECT password_hash FROM local_credentials WHERE principal_id = $1::uuid").bind(&principal).fetch_one(&w.admin).await?;
    assert!(before.contains("m=8192,t=1,p=1"), "{before}");
    let stronger = app(state_with(w.pool.clone(), id, HashParams { memory_kib: 16 * 1024, passes: 2, lanes: 1 }, 1000));
    assert_eq!(post(&stronger, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    let after: String =
        sqlx::query_scalar("SELECT password_hash FROM local_credentials WHERE principal_id = $1::uuid").bind(&principal).fetch_one(&w.admin).await?;
    assert!(after.contains("m=16384,t=2,p=1") && after != before, "{after}");
    // The upgraded hash verifies, and so does the old replica (it accepts stronger hashes).
    assert_eq!(post(&stronger, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    assert_eq!(post(&old, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    assert_eq!(post(&stronger, LOGIN, login_body(&w, &login, "still the wrong one 1")).await.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

// ------------------------------------------------------------------------------------ S6-036

async fn sign_in(app: &Router, w: &World, login: &str, password: &str) -> Result<String, Error> {
    post(app, LOGIN, login_body(w, login, password)).await.session().ok_or_else(|| "sign-in failed".into())
}

fn authed(cookie: &str) -> [(&str, &str); 2] {
    [("cookie", cookie), ("origin", ORIGIN)]
}

#[tokio::test]
async fn profile_shows_a_stable_colour_and_accepts_only_valid_changes() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, principal) = account(&w, &app).await?;
    let cookie = sign_in(&app, &w, &login, GOOD).await?;

    assert_eq!(call(&app, Method::GET, "/api/v1/me", &[], None).await.status, StatusCode::UNAUTHORIZED);
    let me = call(&app, Method::GET, "/api/v1/me", &[("cookie", &cookie), ("x-figcraft-tenant", &w.tenant.to_string())], None).await;
    assert_eq!(me.status, StatusCode::OK, "{:?}", me.body);
    assert_eq!(
        (me.body["display_name"].as_str(), me.body["color_chosen"].as_bool(), me.body["login"].as_str()),
        (Some("Login Person"), Some(false), Some(login.as_str()))
    );
    let default_color = me.body["color"].as_str().ok_or("color")?.to_string();
    let palette: Vec<String> = me.body["palette"].as_array().ok_or("palette")?.iter().filter_map(|c| c.as_str().map(str::to_string)).collect();
    assert_eq!(palette.len(), 12);
    assert!(palette.contains(&default_color));
    // The default is stable.
    let again = call(&app, Method::GET, "/api/v1/me", &[("cookie", &cookie)], None).await;
    assert_eq!(again.body["color"], default_color);

    // CSRF and validation.
    assert_eq!(
        call(&app, Method::PATCH, "/api/v1/me", &[("cookie", &cookie)], Some(json!({"display_name": "X"}))).await.status,
        StatusCode::FORBIDDEN
    );
    for bad in [
        json!({}),
        json!({"color": "#123456"}),
        json!({"color": "red"}),
        json!({"display_name": ""}),
        json!({"display_name": "x".repeat(65)}),
        json!({"display_name": "tab\there"}),
        json!({"nope": 1}),
    ] {
        let reply = call(&app, Method::PATCH, "/api/v1/me", &authed(&cookie), Some(bad.clone())).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let chosen = palette.iter().find(|c| **c != default_color).ok_or("another colour")?.clone();
    let reply = call(&app, Method::PATCH, "/api/v1/me", &authed(&cookie), Some(json!({"display_name": "  Ada L. ", "color": chosen}))).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(
        (reply.body["display_name"].as_str(), reply.body["color"].as_str(), reply.body["color_chosen"].as_bool()),
        (Some("Ada L."), Some(chosen.as_str()), Some(true))
    );
    let stored: (String, Option<String>) =
        sqlx::query_as("SELECT display_name, color FROM principals WHERE id = $1::uuid").bind(&principal).fetch_one(&w.admin).await?;
    assert_eq!(stored, ("Ada L.".to_string(), Some(chosen)));
    Ok(())
}

#[tokio::test]
async fn changing_the_password_needs_the_current_one_and_ends_every_other_session() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, principal) = account(&w, &app).await?;
    let here = sign_in(&app, &w, &login, GOOD).await?;
    let elsewhere = sign_in(&app, &w, &login, GOOD).await?;
    let tenant = w.tenant.to_string();
    let path = "/api/v1/me/password";
    let send = |cookie: String, body: Value| {
        let app = app.clone();
        let tenant = tenant.clone();
        async move { call(&app, Method::POST, path, &[("cookie", &cookie), ("origin", ORIGIN), ("x-figcraft-tenant", &tenant)], Some(body)).await }
    };
    let new_password = "a brand new passphrase 77";

    // A weak new password is explained and changes nothing; so does a wrong current password.
    let reply = send(here.clone(), json!({"current_password": GOOD, "new_password": "short"})).await;
    assert_eq!((reply.status, reply.body["error"]["reason"].as_str()), (StatusCode::BAD_REQUEST, Some("password_policy")));
    let reply = send(here.clone(), json!({"current_password": "not my password 123", "new_password": new_password})).await;
    assert_eq!((reply.status, reply.code()), (StatusCode::FORBIDDEN, "forbidden"));
    let whoami = format!("/api/v1/tenants/{tenant}/whoami");
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &here)], None).await.status, StatusCode::OK);
    let failed: i32 = sqlx::query_scalar("SELECT failed_attempts FROM local_credentials WHERE principal_id = $1::uuid")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(failed, 1, "a wrong current password counts as a failed guess");
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK, "the old password still works");
    // CSRF.
    let reply = call(&app, Method::POST, path, &[("cookie", &here)], Some(json!({"current_password": GOOD, "new_password": new_password}))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    let reply = send(here.clone(), json!({"current_password": GOOD, "new_password": new_password})).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert!(reply.body["revoked_sessions"].as_u64().is_some_and(|n| n >= 2));
    let fresh = reply.session().ok_or("a fresh cookie")?;
    // Both earlier sessions are dead, the fresh one lives.
    for dead in [&here, &elsewhere] {
        assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", dead)], None).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &fresh)], None).await.status, StatusCode::OK);
    // The old password is gone, the new works, and the counters are clear.
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, new_password)).await.status, StatusCode::OK);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = 'credential.password_change' AND actor_principal_id = $2::uuid",
    )
    .bind(&tenant)
    .bind(&principal)
    .fetch_one(&w.admin)
    .await?;
    assert_eq!(audited, 1);
    Ok(())
}

#[tokio::test]
async fn guessing_the_current_password_is_throttled_like_signing_in() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, _) = account(&w, &app).await?;
    let cookie = sign_in(&app, &w, &login, GOOD).await?;
    let tenant = w.tenant.to_string();
    let mut last = StatusCode::OK;
    for _ in 0..7 {
        last = call(
            &app,
            Method::POST,
            "/api/v1/me/password",
            &[("cookie", &cookie), ("origin", ORIGIN), ("x-figcraft-tenant", &tenant)],
            Some(json!({"current_password": "guess guess guess 1", "new_password": "whatever new phrase 5"})),
        )
        .await
        .status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    // Even the right current password waits while backed off.
    let reply = call(
        &app,
        Method::POST,
        "/api/v1/me/password",
        &[("cookie", &cookie), ("origin", ORIGIN), ("x-figcraft-tenant", &tenant)],
        Some(json!({"current_password": GOOD, "new_password": "whatever new phrase 5"})),
    )
    .await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    Ok(())
}

#[tokio::test]
async fn people_list_and_revoke_only_their_own_sessions() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, _) = account(&w, &app).await?;
    let (other_login, _) = account(&w, &app).await?;
    let (a, b, c) = (sign_in(&app, &w, &login, GOOD).await?, sign_in(&app, &w, &login, GOOD).await?, sign_in(&app, &w, &login, GOOD).await?);
    let strangers = sign_in(&app, &w, &other_login, GOOD).await?;

    let listed = call(&app, Method::GET, "/api/v1/me/sessions", &[("cookie", &b)], None).await;
    assert_eq!(listed.status, StatusCode::OK);
    let items = listed.body["items"].as_array().ok_or("items")?;
    assert!(items.len() >= 3);
    assert_eq!(items.iter().filter(|item| item["current"] == true).count(), 1);
    assert!(items.iter().all(|item| item.get("token").is_none() && item["id"].as_str().is_some()));
    let a_view = call(&app, Method::GET, "/api/v1/me/sessions", &[("cookie", &a)], None).await;
    let a_id = a_view.body["items"]
        .as_array()
        .ok_or("items")?
        .iter()
        .find(|item| item["current"] == true)
        .and_then(|item| item["id"].as_str())
        .ok_or("a id")?
        .to_string();
    // Another person's session id is a plain 404; so is a malformed one.
    let theirs = call(&app, Method::GET, "/api/v1/me/sessions", &[("cookie", &strangers)], None).await;
    let theirs_id = theirs.body["items"][0]["id"].as_str().ok_or("their id")?.to_string();
    assert_eq!(call(&app, Method::DELETE, &format!("/api/v1/me/sessions/{theirs_id}"), &authed(&b), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Method::DELETE, "/api/v1/me/sessions/not-an-id", &authed(&b), None).await.status, StatusCode::NOT_FOUND);
    let whoami = format!("/api/v1/tenants/{}/whoami", w.tenant);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &strangers)], None).await.status, StatusCode::OK, "their session is untouched");
    // CSRF.
    assert_eq!(call(&app, Method::DELETE, &format!("/api/v1/me/sessions/{a_id}"), &[("cookie", &b)], None).await.status, StatusCode::FORBIDDEN);
    // Revoking one ends exactly that one.
    let reply = call(&app, Method::DELETE, &format!("/api/v1/me/sessions/{a_id}"), &authed(&b), None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let alive = [&a, &b, &c];
    let mut dead = 0;
    for cookie in alive {
        if call(&app, Method::GET, &whoami, &[("cookie", cookie)], None).await.status == StatusCode::UNAUTHORIZED {
            dead += 1;
        }
    }
    assert_eq!(dead, 1);
    // Revoke-others keeps only the caller's.
    let reply = call(&app, Method::POST, "/api/v1/me/sessions/revoke-others", &authed(&b), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &b)], None).await.status, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &c)], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &strangers)], None).await.status, StatusCode::OK);
    // Revoking the current one signs this browser out.
    let listed = call(&app, Method::GET, "/api/v1/me/sessions", &[("cookie", &b)], None).await;
    let current = listed.body["items"]
        .as_array()
        .ok_or("items")?
        .iter()
        .find(|item| item["current"] == true)
        .and_then(|item| item["id"].as_str())
        .ok_or("current")?
        .to_string();
    let reply = call(&app, Method::DELETE, &format!("/api/v1/me/sessions/{current}"), &authed(&b), None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(reply.headers.get(header::SET_COOKIE).is_some_and(|value| value.to_str().is_ok_and(|v| v.contains("Max-Age=0"))));
    assert_eq!(call(&app, Method::GET, "/api/v1/me/sessions", &[("cookie", &b)], None).await.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

// ------------------------------------------------------------------------------------ S6-037

fn users_path(w: &World) -> String {
    format!("/api/v1/tenants/{}/users", w.tenant)
}

async fn as_owner(app: &Router, w: &World, method: Method, path: &str, body: Option<Value>) -> Result<Reply, Error> {
    let cookie = w.owner_cookie().await?;
    Ok(call(app, method, path, &[("cookie", &cookie), ("origin", ORIGIN)], body).await)
}

async fn create(app: &Router, w: &World, login: &str, role: &str) -> Result<Reply, Error> {
    as_owner(app, w, Method::POST, &users_path(w), Some(json!({"login": login, "display_name": "Made By Admin", "role": role}))).await
}

const RESET: &str = "/api/v1/auth/password/reset";

#[tokio::test]
async fn an_administrator_creates_an_account_and_the_person_sets_their_password_once() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let login = email();

    // Only people with member.manage may create accounts; CSRF applies.
    let (viewer_login, _) = account(&w, &app).await?;
    let viewer = sign_in(&app, &w, &viewer_login, GOOD).await?;
    let body = json!({"login": login, "display_name": "Made By Admin"});
    assert_eq!(call(&app, Method::POST, &users_path(&w), &authed(&viewer), Some(body.clone())).await.status, StatusCode::FORBIDDEN);
    let owner = w.owner_cookie().await?;
    assert_eq!(call(&app, Method::POST, &users_path(&w), &[("cookie", &owner)], Some(body.clone())).await.status, StatusCode::FORBIDDEN, "CSRF");
    assert_eq!(create(&app, &w, "x y", "viewer").await?.status, StatusCode::BAD_REQUEST);
    assert_eq!(create(&app, &w, &login, "emperor").await?.status, StatusCode::BAD_REQUEST);

    let reply = create(&app, &w, &login, "editor").await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    let token = reply.body["setup_token"].as_str().ok_or("token")?.to_string();
    let principal = reply.body["principal_id"].as_str().ok_or("principal")?.to_string();
    assert_eq!((token.len(), reply.body["role"].as_str()), (43, Some("editor")));
    assert_eq!(create(&app, &w, &login, "viewer").await?.status, StatusCode::CONFLICT);
    // Until they choose a password nobody can sign in as them.
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::UNAUTHORIZED);
    // Only a hash of the token is stored, and the audit trail never carries it.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM password_resets WHERE principal_id = $1::uuid AND token_hash = $2")
        .bind(&principal)
        .bind(figcraft_cloud_storage::sessions::session_token_hash(&token).to_vec())
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(stored, 1);
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND metadata::text LIKE '%' || $2 || '%'")
        .bind(w.tenant.to_string())
        .bind(&token)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(leaked, 0);

    // A weak password is explained and does not burn the link; CSRF and junk tokens are refused.
    let weak = post(&app, RESET, json!({"token": token, "new_password": "short"})).await;
    assert_eq!((weak.status, weak.body["error"]["reason"].as_str()), (StatusCode::BAD_REQUEST, Some("password_policy")));
    assert_eq!(call(&app, Method::POST, RESET, &[], Some(json!({"token": token, "new_password": GOOD}))).await.status, StatusCode::FORBIDDEN);
    for junk in ["", "A".repeat(43).as_str(), "not a token", "A".repeat(500).as_str()] {
        assert_eq!(post(&app, RESET, json!({"token": junk, "new_password": GOOD})).await.status, StatusCode::NOT_FOUND, "{junk}");
    }
    // The person's first password; the link is single use even under concurrency.
    let mut tasks = Vec::new();
    for n in 0..6 {
        let (app, token) = (app.clone(), token.clone());
        tasks.push(tokio::spawn(async move { (n, post(&app, RESET, json!({"token": token, "new_password": format!("{GOOD} {n}")})).await.status) }));
    }
    let mut winners = Vec::new();
    for task in tasks {
        match task.await? {
            (n, StatusCode::OK) => winners.push(n),
            (_, StatusCode::NOT_FOUND) => {}
            (_, other) => return Err(format!("unexpected {other}").into()),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one use of the link succeeds");
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM password_resets WHERE principal_id = $1::uuid AND used_at IS NOT NULL")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(used, 1);
    // The winner's password is the one that works.
    let loser = (winners[0] + 1) % 6;
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, &format!("{GOOD} {loser}"))).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, &format!("{GOOD} {}", winners[0]))).await.status, StatusCode::OK);

    // The new account is listed with its role.
    let listed = as_owner(&app, &w, Method::GET, &users_path(&w), None).await?;
    let items = listed.body["items"].as_array().ok_or("items")?;
    let mine = items.iter().find(|item| item["principal_id"] == principal.as_str()).ok_or("listed")?;
    assert_eq!((mine["role"].as_str(), mine["state"].as_str(), mine["member_state"].as_str()), (Some("editor"), Some("active"), Some("active")));
    Ok(())
}

#[tokio::test]
async fn reset_links_expire_in_an_hour_replace_each_other_and_end_sessions() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, principal) = account(&w, &app).await?;
    let session = sign_in(&app, &w, &login, GOOD).await?;
    // The person is locked out by their own guesses.
    sqlx::query("UPDATE local_credentials SET failed_attempts = 5, locked_until = now() + interval '10 minutes' WHERE principal_id = $1::uuid")
        .bind(&principal)
        .execute(&w.admin)
        .await?;
    let path = format!("{}/{principal}/reset-link", users_path(&w));
    let first = as_owner(&app, &w, Method::POST, &path, Some(json!({}))).await?;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
    assert!(first.body["expires_in_secs"].as_i64().is_some_and(|secs| secs <= 3600));
    let lifetime: f64 = sqlx::query_scalar("SELECT extract(epoch FROM expires_at - created_at)::float8 FROM password_resets WHERE principal_id = $1::uuid ORDER BY created_at DESC LIMIT 1")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert!(lifetime <= 3600.0);
    let first_token = first.body["reset_token"].as_str().ok_or("token")?.to_string();
    // A newer link replaces the older one.
    let second = as_owner(&app, &w, Method::POST, &path, Some(json!({}))).await?;
    let second_token = second.body["reset_token"].as_str().ok_or("token")?.to_string();
    assert_ne!(first_token, second_token);
    assert_eq!(post(&app, RESET, json!({"token": first_token, "new_password": GOOD})).await.status, StatusCode::NOT_FOUND);
    // An expired link is refused.
    let third = as_owner(&app, &w, Method::POST, &path, Some(json!({}))).await?;
    let third_token = third.body["reset_token"].as_str().ok_or("token")?.to_string();
    sqlx::query("UPDATE password_resets SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE token_hash = $1")
        .bind(figcraft_cloud_storage::sessions::session_token_hash(&third_token).to_vec())
        .execute(&w.admin)
        .await?;
    assert_eq!(post(&app, RESET, json!({"token": third_token, "new_password": GOOD})).await.status, StatusCode::NOT_FOUND);
    // The live one works: sessions end, the lock clears, the old password stops working.
    let fresh = as_owner(&app, &w, Method::POST, &path, Some(json!({}))).await?;
    let token = fresh.body["reset_token"].as_str().ok_or("token")?.to_string();
    let new_password = "my replacement passphrase 31";
    let reply = post(&app, RESET, json!({"token": token, "new_password": new_password})).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let whoami = format!("/api/v1/tenants/{}/whoami", w.tenant);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &session)], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, new_password)).await.status, StatusCode::OK);
    assert_eq!(post(&app, RESET, json!({"token": token, "new_password": GOOD})).await.status, StatusCode::NOT_FOUND, "single use");
    let mut actions: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT action FROM audit_events WHERE tenant_id = $1::uuid AND action LIKE 'credential.reset%'")
            .bind(w.tenant.to_string())
            .fetch_all(&w.admin)
            .await?;
    actions.sort();
    assert_eq!(actions, ["credential.reset_issue", "credential.reset_used"]);
    // A link for a disabled account is refused, and none can be issued.
    let disabled_path = format!("{}/{principal}/disable", users_path(&w));
    let link = as_owner(&app, &w, Method::POST, &path, Some(json!({}))).await?;
    assert_eq!(as_owner(&app, &w, Method::POST, &disabled_path, None).await?.status, StatusCode::NO_CONTENT);
    let token = link.body["reset_token"].as_str().ok_or("token")?;
    assert_eq!(post(&app, RESET, json!({"token": token, "new_password": GOOD})).await.status, StatusCode::NOT_FOUND);
    assert_eq!(as_owner(&app, &w, Method::POST, &path, Some(json!({}))).await?.status, StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn disabling_ends_live_sessions_at_once_and_enabling_restores_access() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, principal) = account(&w, &app).await?;
    let one = sign_in(&app, &w, &login, GOOD).await?;
    let two = sign_in(&app, &w, &login, GOOD).await?;
    let whoami = format!("/api/v1/tenants/{}/whoami", w.tenant);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &one)], None).await.status, StatusCode::OK);
    let base = format!("{}/{principal}", users_path(&w));

    // Not for ordinary members, not for strangers' tenants.
    let (other_login, _) = account(&w, &app).await?;
    let other = sign_in(&app, &w, &other_login, GOOD).await?;
    assert_eq!(call(&app, Method::POST, &format!("{base}/disable"), &authed(&other), None).await.status, StatusCode::FORBIDDEN);
    let elsewhere = world().await?.ok_or("world")?;
    let foreign = elsewhere.owner_cookie().await?;
    let foreign_app = self::app(state(elsewhere.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    assert_eq!(
        call(&foreign_app, Method::POST, &format!("{}/{principal}/disable", users_path(&elsewhere)), &authed(&foreign), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &one)], None).await.status, StatusCode::OK, "nothing changed");

    assert_eq!(as_owner(&app, &w, Method::POST, &format!("{base}/disable"), None).await?.status, StatusCode::NO_CONTENT);
    // Both live sessions are refused at once, the password no longer signs in, and the listing says so.
    for cookie in [&one, &two] {
        assert!(matches!(
            call(&app, Method::GET, &whoami, &[("cookie", cookie)], None).await.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ));
    }
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::UNAUTHORIZED);
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM browser_sessions WHERE principal_id = $1::uuid AND revoked_at IS NULL")
        .bind(&principal)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(live, 0, "the sessions were revoked, not just refused");
    let listed = as_owner(&app, &w, Method::GET, &users_path(&w), None).await?;
    let mine =
        listed.body["items"].as_array().ok_or("items")?.iter().find(|item| item["principal_id"] == principal.as_str()).ok_or("listed")?.clone();
    assert_eq!((mine["state"].as_str(), mine["member_state"].as_str()), (Some("disabled"), Some("suspended")));
    // Disabling again is harmless; enabling restores sign-in.
    assert_eq!(as_owner(&app, &w, Method::POST, &format!("{base}/disable"), None).await?.status, StatusCode::NO_CONTENT);
    assert_eq!(as_owner(&app, &w, Method::POST, &format!("{base}/enable"), None).await?.status, StatusCode::NO_CONTENT);
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    // Audited.
    let mut actions: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT action FROM audit_events WHERE tenant_id = $1::uuid AND action IN ('credential.disable', 'credential.enable')",
    )
    .bind(w.tenant.to_string())
    .fetch_all(&w.admin)
    .await?;
    actions.sort();
    assert_eq!(actions, ["credential.disable", "credential.enable"]);

    // An administrator may not touch an owner; the last owner cannot be disabled even by themselves.
    let (admin_login, admin_id) = account(&w, &app).await?;
    sqlx::query("UPDATE tenant_members SET role = 'tenant_admin' WHERE principal_id = $1::uuid").bind(&admin_id).execute(&w.admin).await?;
    let admin_cookie = sign_in(&app, &w, &admin_login, GOOD).await?;
    let owner_path = format!("{}/{}/disable", users_path(&w), w.owner);
    assert_eq!(
        call(&app, Method::POST, &owner_path, &authed(&admin_cookie), None).await.status,
        StatusCode::NOT_FOUND,
        "the owner has no password account here"
    );
    // Give the owner a password account, then check the two protections.
    let owner_hash = "$argon2id$v=19$m=8192,t=1,p=1$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo";
    sqlx::query("INSERT INTO local_credentials (tenant_id, principal_id, login, password_hash) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(w.tenant.to_string())
        .bind(w.owner.to_string())
        .bind(email())
        .bind(owner_hash)
        .execute(&w.admin)
        .await?;
    assert_eq!(call(&app, Method::POST, &owner_path, &authed(&admin_cookie), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(as_owner(&app, &w, Method::POST, &owner_path, None).await?.status, StatusCode::CONFLICT, "the last active owner stays");
    Ok(())
}

#[tokio::test]
async fn an_administrator_can_end_every_session_of_a_person() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let (login, principal) = account(&w, &app).await?;
    let (one, two) = (sign_in(&app, &w, &login, GOOD).await?, sign_in(&app, &w, &login, GOOD).await?);
    let path = format!("{}/{principal}/revoke-sessions", users_path(&w));
    let (viewer_login, _) = account(&w, &app).await?;
    let viewer = sign_in(&app, &w, &viewer_login, GOOD).await?;
    assert_eq!(call(&app, Method::POST, &path, &authed(&viewer), None).await.status, StatusCode::FORBIDDEN);
    let reply = as_owner(&app, &w, Method::POST, &path, None).await?;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body["revoked"].as_u64().is_some_and(|n| n >= 2));
    let whoami = format!("/api/v1/tenants/{}/whoami", w.tenant);
    for cookie in [&one, &two] {
        assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", cookie)], None).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &viewer)], None).await.status, StatusCode::OK);
    // They can sign in again (sessions are not a ban).
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, GOOD)).await.status, StatusCode::OK);
    Ok(())
}

// ------------------------------------------------------------------------------- TS-013 / TS-014

fn invitations_path(w: &World) -> String {
    format!("/api/v1/tenants/{}/teams/{}/invitations", w.tenant, w.team)
}

async fn link_invitation(app: &Router, w: &World, body: Value) -> Result<String, Error> {
    let reply = as_owner(app, w, Method::POST, &invitations_path(w), Some(body)).await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    Ok(reply.body["token"].as_str().ok_or("token")?.to_string())
}

#[tokio::test]
async fn invitations_need_no_email_and_the_invitee_chooses_a_login_name() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let path = "/api/v1/auth/password/signup";
    let handle = format!("carol{}", suffix());

    // A link-only invitation: no email at all.
    let token = link_invitation(&app, &w, json!({"role": "editor"})).await?;
    let listed = as_owner(&app, &w, Method::GET, &invitations_path(&w), None).await?;
    assert!(listed.body["items"].as_array().ok_or("items")?.iter().any(|item| item["email"].is_null()), "{:?}", listed.body);
    // Without a login name there is nothing to sign in with.
    assert_eq!(post(&app, path, json!({"invitation": token, "display_name": "Carol", "password": GOOD})).await.status, StatusCode::BAD_REQUEST);
    // A login that is not acceptable creates nothing and keeps the link usable.
    assert_eq!(
        post(&app, path, json!({"invitation": token, "login": "no good", "display_name": "Carol", "password": GOOD})).await.status,
        StatusCode::BAD_REQUEST
    );
    let reply = post(&app, path, json!({"invitation": token, "login": handle.to_uppercase(), "display_name": "Carol", "password": GOOD})).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);
    let cookie = reply.session().ok_or("cookie")?;
    let whoami = format!("/api/v1/tenants/{}/whoami", w.tenant);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &cookie)], None).await.status, StatusCode::OK);
    let stored: String = sqlx::query_scalar("SELECT login FROM local_credentials WHERE tenant_id = $1::uuid AND login = $2")
        .bind(w.tenant.to_string())
        .bind(&handle)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(stored, handle);
    assert_eq!(post(&app, LOGIN, login_body(&w, &handle, GOOD)).await.status, StatusCode::OK);
    // Used once; and the same login name is taken, named so the UI can say so.
    assert_eq!(
        post(&app, path, json!({"invitation": token, "login": "other-name", "display_name": "Mallory", "password": GOOD})).await.status,
        StatusCode::NOT_FOUND
    );
    let second = link_invitation(&app, &w, json!({"role": "viewer"})).await?;
    let reply = post(&app, path, json!({"invitation": second, "login": handle, "display_name": "Copycat", "password": GOOD})).await;
    assert_eq!((reply.status, reply.code(), reply.body["error"]["reason"].as_str()), (StatusCode::CONFLICT, "conflict", Some("login_taken")));
    // The refused attempt left the link usable.
    let reply = post(&app, path, json!({"invitation": second, "login": format!("{handle}2"), "display_name": "Copycat", "password": GOOD})).await;
    assert_eq!(reply.status, StatusCode::CREATED);

    // An invitation that notes an address: the person may still choose another login name, or omit it.
    let noted = link_invitation(&app, &w, json!({"role": "viewer", "email": "dave@example.test"})).await?;
    let reply = post(&app, path, json!({"invitation": noted, "display_name": "Dave", "password": GOOD})).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let login: String = sqlx::query_scalar("SELECT login FROM local_credentials WHERE principal_id = $1::uuid")
        .bind(reply.body["principal_id"].as_str().ok_or("p")?)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(login, "dave@example.test");
    // A malformed noted address is still refused.
    assert_eq!(
        as_owner(&app, &w, Method::POST, &invitations_path(&w), Some(json!({"role": "viewer", "email": "nope"}))).await?.status,
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn an_initial_password_is_temporary_and_the_session_can_only_change_it() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let tenant = w.tenant.to_string();
    let temp = "temporary phrase typed by admin 5";

    // Typed and generated together is a mistake; a weak typed password is explained.
    let both = as_owner(
        &app,
        &w,
        Method::POST,
        &users_path(&w),
        Some(json!({"login": "both-ways", "display_name": "X", "initial_password": temp, "generate_password": true})),
    )
    .await?;
    assert_eq!(both.status, StatusCode::BAD_REQUEST);
    let weak =
        as_owner(&app, &w, Method::POST, &users_path(&w), Some(json!({"login": "weak-one", "display_name": "X", "initial_password": "short"})))
            .await?;
    assert_eq!((weak.status, weak.body["error"]["reason"].as_str()), (StatusCode::BAD_REQUEST, Some("password_policy")));
    assert_eq!(w.count("SELECT count(*) FROM local_credentials WHERE tenant_id = $1::uuid", &tenant).await?, 0, "nothing was created");

    let login = format!("erin{}", suffix());
    let made =
        as_owner(&app, &w, Method::POST, &users_path(&w), Some(json!({"login": login, "display_name": "Erin", "initial_password": temp}))).await?;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(made.body["must_change_password"], true);
    assert!(made.body.get("setup_token").is_none() && made.body.get("generated_password").is_none(), "a typed password is never echoed");
    let listed = as_owner(&app, &w, Method::GET, &users_path(&w), None).await?;
    assert!(
        listed.body["items"].as_array().ok_or("items")?.iter().any(|item| item["login"] == login.as_str() && item["must_change_password"] == true)
    );

    // Signing in reports it; the session works only for changing the password.
    let reply = post(&app, LOGIN, login_body(&w, &login, temp)).await;
    assert_eq!((reply.status, reply.body["must_change_password"].as_bool()), (StatusCode::OK, Some(true)));
    let restricted = reply.session().ok_or("cookie")?;
    let whoami = format!("/api/v1/tenants/{tenant}/whoami");
    let blocked = call(&app, Method::GET, &whoami, &[("cookie", &restricted)], None).await;
    assert_eq!((blocked.status, blocked.body["error"]["reason"].as_str()), (StatusCode::FORBIDDEN, Some("password_change_required")));
    let sessions = call(&app, Method::GET, "/api/v1/me/sessions", &[("cookie", &restricted)], None).await;
    assert_eq!((sessions.status, sessions.body["error"]["reason"].as_str()), (StatusCode::FORBIDDEN, Some("password_change_required")));
    assert_eq!(call(&app, Method::GET, "/api/v1/me", &[("cookie", &restricted), ("x-figcraft-tenant", &tenant)], None).await.status, StatusCode::OK);
    let session = call(&app, Method::GET, "/api/v1/auth/session", &[("cookie", &restricted)], None).await;
    assert_eq!(session.body["password_change_required"], true);
    // The temporary password cannot be kept as the new one's twin: a weak new password is refused ...
    let send = |cookie: String, body: Value| {
        let (app, tenant) = (app.clone(), tenant.clone());
        async move {
            call(&app, Method::POST, "/api/v1/me/password", &[("cookie", &cookie), ("origin", ORIGIN), ("x-figcraft-tenant", &tenant)], Some(body))
                .await
        }
    };
    assert_eq!(send(restricted.clone(), json!({"current_password": temp, "new_password": "short"})).await.status, StatusCode::BAD_REQUEST);
    // ... and choosing a real one frees the account.
    let mine = "my own private passphrase 88";
    let changed = send(restricted.clone(), json!({"current_password": temp, "new_password": mine})).await;
    assert_eq!(changed.status, StatusCode::OK, "{:?}", changed.body);
    let free = changed.session().ok_or("fresh cookie")?;
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &free)], None).await.status, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &restricted)], None).await.status, StatusCode::UNAUTHORIZED);
    let reply = post(&app, LOGIN, login_body(&w, &login, mine)).await;
    assert_eq!((reply.status, reply.body["must_change_password"].as_bool()), (StatusCode::OK, Some(false)));
    assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &reply.session().ok_or("c")?)], None).await.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn generated_passwords_are_shown_once_and_set_by_administrators_below_them() -> TestResult {
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let tenant = w.tenant.to_string();
    let made = as_owner(
        &app,
        &w,
        Method::POST,
        &users_path(&w),
        Some(json!({"login": format!("frank{}", suffix()), "display_name": "Frank", "role": "editor", "generate_password": true})),
    )
    .await?;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    let generated = made.body["generated_password"].as_str().ok_or("generated")?.to_string();
    let frank = made.body["principal_id"].as_str().ok_or("principal")?.to_string();
    let login = made.body["login"].as_str().ok_or("login")?.to_string();
    let shape: Vec<usize> = generated.split('-').map(str::len).collect();
    assert_eq!(shape, [5, 5, 5, 5], "{generated}");
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, &generated)).await.body["must_change_password"], true);
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND metadata::text LIKE '%' || $2 || '%'")
        .bind(&tenant)
        .bind(&generated)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(leaked, 0);

    // An administrator (not an owner) may set passwords for people below, not for peers, owners or themselves.
    let (admin_login, admin_id) = account(&w, &app).await?;
    sqlx::query("UPDATE tenant_members SET role = 'tenant_admin' WHERE principal_id = $1::uuid").bind(&admin_id).execute(&w.admin).await?;
    let (peer_login, peer_id) = account(&w, &app).await?;
    sqlx::query("UPDATE tenant_members SET role = 'tenant_admin' WHERE principal_id = $1::uuid").bind(&peer_id).execute(&w.admin).await?;
    let _ = peer_login;
    let hash = "$argon2id$v=19$m=8192,t=1,p=1$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo";
    sqlx::query("INSERT INTO local_credentials (tenant_id, principal_id, login, password_hash) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(&tenant)
        .bind(w.owner.to_string())
        .bind(email())
        .bind(hash)
        .execute(&w.admin)
        .await?;
    let admin = sign_in(&app, &w, &admin_login, GOOD).await?;
    let set = |cookie: String, who: String, body: Value| {
        let (app, tenant) = (app.clone(), tenant.clone());
        async move {
            call(
                &app,
                Method::POST,
                &format!("/api/v1/tenants/{tenant}/users/{who}/password"),
                &[("cookie", &cookie), ("origin", ORIGIN)],
                Some(body),
            )
            .await
        }
    };
    for (who, label) in [(admin_id.clone(), "themselves"), (peer_id.clone(), "a peer"), (w.owner.to_string(), "the owner")] {
        let reply = set(admin.clone(), who, json!({"generate": true})).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{label}");
    }
    // Bad requests: both, neither, weak.
    for bad in [json!({}), json!({"password": GOOD, "generate": true}), json!({"password": "short"})] {
        assert_eq!(set(admin.clone(), frank.clone(), bad.clone()).await.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    // A plain member cannot; CSRF is enforced.
    let (viewer_login, _) = account(&w, &app).await?;
    let viewer = sign_in(&app, &w, &viewer_login, GOOD).await?;
    assert_eq!(set(viewer, frank.clone(), json!({"generate": true})).await.status, StatusCode::FORBIDDEN);
    let frank_session = sign_in(&app, &w, &login, &generated).await.ok();
    let reply =
        call(&app, Method::POST, &format!("/api/v1/tenants/{tenant}/users/{frank}/password"), &[("cookie", &admin)], Some(json!({"generate": true})))
            .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "CSRF");

    // The set works: a new password, shown once, temporary, other sessions gone, audited without the value.
    let reply = set(admin.clone(), frank.clone(), json!({"password": "admin chose this phrase 12"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["must_change_password"], true);
    assert!(reply.body.get("generated_password").is_none());
    if let Some(cookie) = frank_session {
        let whoami = format!("/api/v1/tenants/{tenant}/whoami");
        assert_eq!(call(&app, Method::GET, &whoami, &[("cookie", &cookie)], None).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, &generated)).await.status, StatusCode::UNAUTHORIZED, "the old password is gone");
    assert_eq!(post(&app, LOGIN, login_body(&w, &login, "admin chose this phrase 12")).await.body["must_change_password"], true);
    let again = set(admin.clone(), frank.clone(), json!({"generate": true})).await;
    let second = again.body["generated_password"].as_str().ok_or("generated")?.to_string();
    assert_ne!(second, generated);
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = 'credential.password_admin_set' AND actor_principal_id = $2::uuid AND metadata::text NOT LIKE '%' || $3 || '%'")
        .bind(&tenant)
        .bind(&admin_id)
        .bind(&second)
        .fetch_one(&w.admin)
        .await?;
    assert_eq!(audited, 2);
    // A disabled account gets none.
    assert_eq!(as_owner(&app, &w, Method::POST, &format!("{}/{frank}/disable", users_path(&w)), None).await?.status, StatusCode::NO_CONTENT);
    assert_eq!(set(admin.clone(), frank, json!({"generate": true})).await.status, StatusCode::CONFLICT);
    // The owner may set a password for an administrator.
    let owner_set =
        as_owner(&app, &w, Method::POST, &format!("/api/v1/tenants/{tenant}/users/{admin_id}/password"), Some(json!({"generate": true}))).await?;
    assert_eq!(owner_set.status, StatusCode::OK);
    Ok(())
}

// ------------------------------------------------------------------------------------ S6-040

struct Filed {
    document: figcraft_cloud_domain::CloudDocumentId,
    workspace: figcraft_cloud_domain::WorkspaceId,
}

/// A workspace, project and document in `w`'s tenant, owned by the tenant owner.
async fn filed(w: &World) -> Result<Filed, Error> {
    use figcraft_cloud_domain::{CloudDocumentId, ProjectId, WorkspaceId};
    use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
    let (workspace, project, document): (WorkspaceId, ProjectId, CloudDocumentId) = (e7("c2").parse()?, e7("c3").parse()?, e7("c4").parse()?);
    let mut t = begin_tenant_transaction(&w.pool, w.tenant).await.map_err(|e| format!("{e:?}"))?;
    create_workspace(&mut t, workspace, "e7 ws", None).await.map_err(|e| format!("{e:?}"))?;
    create_project(&mut t, project, workspace, "e7 project", None).await.map_err(|e| format!("{e:?}"))?;
    create_document(&mut t, document, workspace, project, "e7 doc").await.map_err(|e| format!("{e:?}"))?;
    t.commit().await?;
    Ok(Filed { document, workspace })
}

/// Commit one batch as `actor` through `via`, `ago` seconds in the past.
async fn commit_event(
    w: &World,
    f: &Filed,
    actor: PrincipalId,
    via: Option<figcraft_cloud_storage::attribution::Via>,
    ago: i64,
) -> Result<i64, Error> {
    use figcraft_cloud_storage::events::{append_events_via, claim_sequencer};
    use figcraft_collab::{Accepted, Change, NodeId};
    let mut t = begin_tenant_transaction(&w.pool, w.tenant).await.map_err(|e| format!("{e:?}"))?;
    let claim = claim_sequencer(&mut t, f.document).await.map_err(|e| format!("{e:?}"))?;
    let seq = claim.current_seq + 1;
    let batch = Accepted {
        seq: u64::try_from(seq)?,
        author: 1,
        batch_id: u64::try_from(seq)?,
        changes: vec![Change::Delete { id: NodeId(u64::try_from(seq)? * 10) }],
    };
    append_events_via(&mut t, f.document, claim.fencing_token, actor, via, &[batch]).await.map_err(|e| format!("{e:?}"))?;
    t.commit().await?;
    sqlx::query("UPDATE document_events SET created_at = now() - make_interval(secs => $3::bigint) WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq = $4")
        .bind(w.tenant.to_string())
        .bind(f.document.to_string())
        .bind(ago)
        .bind(seq)
        .execute(&w.admin)
        .await?;
    Ok(seq)
}

#[tokio::test]
async fn history_groups_activity_by_person_and_access_and_keeps_checkpoints_apart() -> TestResult {
    use figcraft_cloud_storage::attribution::Via;
    use figcraft_cloud_storage::memberships::set_workspace_member;
    let Some(w) = world().await? else { return Ok(()) };
    let app = app(state(w.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let f = filed(&w).await?;
    let (ben_login, ben) = account(&w, &app).await?;
    let ben: PrincipalId = ben.parse()?;
    let (viewer_login, viewer) = account(&w, &app).await?;
    let viewer: PrincipalId = viewer.parse()?;
    let (outsider_login, _) = account(&w, &app).await?;
    {
        let mut t = begin_tenant_transaction(&w.pool, w.tenant).await.map_err(|e| format!("{e:?}"))?;
        set_workspace_member(&mut t, f.workspace, viewer, figcraft_cloud_domain::Role::Viewer).await.map_err(|e| format!("{e:?}"))?;
        t.commit().await?;
    }
    sqlx::query("UPDATE teams SET name = 'Design' WHERE id = $1::uuid").bind(w.team.to_string()).execute(&w.admin).await?;
    let _ = (ben_login, viewer_login.clone());

    // ada = the owner (directly), ben through the Design team, then a labelled share link.
    let link = {
        let mut t = begin_tenant_transaction(&w.pool, w.tenant).await.map_err(|e| format!("{e:?}"))?;
        let link = figcraft_cloud_storage::share_links::create_share_link(
            &mut t,
            f.document,
            figcraft_cloud_storage::share_links::ShareRole::Editor,
            Some("Design review"),
            w.owner,
            None,
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
        t.commit().await?;
        link
    };
    commit_event(&w, &f, w.owner, Some(Via::Member), 2000).await?; // 1
    commit_event(&w, &f, w.owner, Some(Via::Member), 1990).await?; // 2
    commit_event(&w, &f, ben, Some(Via::Team(w.team)), 1500).await?; // 3
    commit_event(&w, &f, ben, Some(Via::Team(w.team)), 1490).await?; // 4
    commit_event(&w, &f, w.owner, Some(Via::Member), 600).await?; // 5 (a long pause after 2)
    commit_event(&w, &f, w.owner, Some(Via::ShareLink(link.id)), 100).await?; // 6: same person, other access
    commit_event(&w, &f, w.owner, None, 50).await?; // 7: unknown access

    let base = format!("/api/v1/documents/{}/history", f.document);
    let tenant = w.tenant.to_string();
    let owner = w.owner_cookie().await?;
    let get = |cookie: String, path: String| {
        let (app, tenant) = (app.clone(), tenant.clone());
        async move { call(&app, Method::GET, &path, &[("cookie", &cookie), ("x-figcraft-tenant", &tenant)], None).await }
    };
    let reply = get(owner.clone(), format!("{base}/activity")).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let items = reply.body["items"].as_array().ok_or("items")?;
    let shape: Vec<(i64, i64, String)> = items
        .iter()
        .map(|i| (i["from_seq"].as_i64().unwrap_or(-1), i["to_seq"].as_i64().unwrap_or(-1), i["label"].as_str().unwrap_or("").to_string()))
        .collect();
    assert_eq!(shape.len(), 5, "{shape:?}");
    assert_eq!((shape[0].0, shape[0].2.as_str()), (7, "e7 owner"), "unknown access reads as the person alone");
    assert_eq!((shape[1].0, shape[1].2.as_str()), (6, "e7 owner, through a share link (Design review)"));
    assert_eq!((shape[2].0, shape[2].1), (5, 5));
    assert_eq!((shape[3].0, shape[3].1, shape[3].2.as_str()), (3, 4, "Login Person, through the Design team"));
    assert_eq!((shape[4].0, shape[4].1), (1, 2));
    assert_eq!(items[3]["batches"], 2);
    assert_eq!(items[3]["via"]["kind"], "team");
    assert_eq!(items[3]["via"]["name"], "Design");
    assert_eq!(items[3]["person"]["principal_id"], ben.to_string());
    assert!(items[3]["person"]["color"].as_str().is_some_and(|c| c.starts_with('#')));
    assert!(items[3]["started_at"].as_i64() <= items[3]["ended_at"].as_i64());

    // Paging: one run per page walks the same list without gaps or repeats.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let path = match &cursor {
            Some(cursor) => format!("{base}/activity?limit=1&cursor={cursor}"),
            None => format!("{base}/activity?limit=1"),
        };
        let page = get(owner.clone(), path).await;
        assert_eq!(page.status, StatusCode::OK);
        for item in page.body["items"].as_array().ok_or("items")? {
            seen.push((item["from_seq"].as_i64().unwrap_or(-1), item["to_seq"].as_i64().unwrap_or(-1)));
        }
        cursor = page.body["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, shape.iter().map(|s| (s.0, s.1)).collect::<Vec<_>>());
    assert_eq!(get(owner.clone(), format!("{base}/activity?cursor=garbage")).await.status, StatusCode::BAD_REQUEST);

    // Checkpoints are a different list, and the summary names the last editor.
    let reply = get(owner.clone(), format!("{base}/checkpoints")).await;
    assert_eq!((reply.status, reply.body["items"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)));
    let reply = get(owner.clone(), format!("{base}/summary")).await;
    assert_eq!(reply.body["current_seq"], 7);
    assert_eq!(reply.body["last_edit"]["seq"], 7);

    // Who may read it: a document viewer, not an outsider, not another workspace, not a stranger.
    let viewer_cookie = sign_in(&app, &w, &viewer_login, GOOD).await?;
    assert_eq!(get(viewer_cookie, format!("{base}/activity")).await.status, StatusCode::OK);
    let outsider_cookie = sign_in(&app, &w, &outsider_login, GOOD).await?;
    assert_eq!(get(outsider_cookie, format!("{base}/activity")).await.status, StatusCode::NOT_FOUND);
    let elsewhere = world().await?.ok_or("world")?;
    let foreign = elsewhere.owner_cookie().await?;
    let foreign_app = self::app(state(elsewhere.pool.clone(), identity(DeploymentMode::MultiTenant, None)));
    let reply = call(
        &foreign_app,
        Method::GET,
        &format!("{base}/activity"),
        &[("cookie", &foreign), ("x-figcraft-tenant", &elsewhere.tenant.to_string())],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Method::GET, &format!("{base}/activity"), &[], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(get(owner, "/api/v1/documents/not-an-id/history/activity".to_string()).await.status, StatusCode::NOT_FOUND);
    Ok(())
}
