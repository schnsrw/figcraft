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
    let auth = auth.merge(super::super::accounts_contract::routes());
    let resources = Router::new().route("/tenants/{tenant}/whoami", get(whoami)).merge(super::resource_routes());
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
    // A login in the body is a mistake for invitations; so is naming both doors.
    assert_eq!(
        post(&app, path, json!({"invitation": token, "login": "x@y.zz", "display_name": "A", "password": GOOD})).await.status,
        StatusCode::BAD_REQUEST
    );
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

    // The new person is signed in as a viewer of the tenant, member of the invited team.
    let reply = call(&app, Method::GET, &format!("/api/v1/tenants/{}/whoami", w.tenant), &[("cookie", &cookie)], None).await;
    assert_eq!(
        (reply.status, reply.body["role"].as_str(), reply.body["principal"].as_str()),
        (StatusCode::OK, Some("viewer"), Some(principal.as_str()))
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
    assert!(high.as_secs_f64() < low.as_secs_f64() * 3.0 + 0.05, "{known_time:?} vs {unknown_time:?}");
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
