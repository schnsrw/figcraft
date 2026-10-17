//! S6-012: the host API and the embed-token exchange over HTTP against the dev PostgreSQL, through
//! the production router with real service-account credentials (skipped without
//! `FIGCRAFT_TEST_DATABASE_URL_FILE`). Every tenant and principal is random, so runs are independent.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use figcraft_cloud_domain::{CloudDocumentId, DeploymentMode, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::host_mappings::{MappingKind, map_external_resource};
use figcraft_cloud_storage::host_systems::{HostRole, KeyAlgorithm, NewHostKey, NewHostSystem, add_host_key, register_host_system, set_host_enabled};
use figcraft_cloud_storage::memberships::{add_tenant_member, set_document_member};
use figcraft_cloud_storage::metadata::{create_project, create_workspace};
use figcraft_cloud_storage::resource_admin::{Transition, transition_project};
use figcraft_cloud_storage::service_accounts::{Scope, create_service_account, issue_credential};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt as _;

use super::authenticator::IdentityConfig;
use super::context::ContextSettings;
use super::host_api::HOST_ROUTES;
use super::rate_limit::RateLimitConfig;
use super::{ApiConfig, AppState, api_router};
use crate::config::CloudRole;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

const ORIGIN: &str = "https://figcraft-s6.test";

fn err<E: std::fmt::Debug>(error: E) -> Box<dyn std::error::Error> {
    format!("{error:?}").into()
}

async fn pool() -> Fallible<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; host API DB test skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(8).connect(url.trim_end()).await?))
}

fn app(pool: PgPool) -> Fallible<Router> {
    app_with(pool, Some(b"s6-host-api-signing-key-0123456"))
}

fn app_with(pool: PgPool, signing_key: Option<&[u8]>) -> Fallible<Router> {
    app_limited(pool, signing_key, super::host_api::EMBED_LIMIT)
}

fn app_limited(pool: PgPool, signing_key: Option<&[u8]>, embed_limit: RateLimitConfig) -> Fallible<Router> {
    let objects = figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".to_string(),
        region: "us-east-1".to_string(),
        bucket: "figcraft-test".to_string(),
        access_key: "FCtestkey".to_string(),
        secret_key: "FCtestsecret".to_string(),
        path_style: true,
    })
    .map_err(err)?;
    let identity = Arc::new(IdentityConfig::new(DeploymentMode::MultiTenant, None, Some(ORIGIN), signing_key).map_err(err)?);
    let config = ApiConfig {
        deployment: DeploymentMode::MultiTenant,
        auth_rate_limit: RateLimitConfig { burst: 1_000_000, refill_every: Duration::from_millis(1), max_keys: 1_000 },
        ..ApiConfig::default()
    };
    let state = AppState::new(pool, objects, config).with_identity(identity).with_embed_limit(embed_limit);
    Ok(crate::health::app(CloudRole::Api, true, Some(api_router(state)), ContextSettings::new(CloudRole::Api, 256, Duration::from_secs(30), 1 << 20)))
}

struct Reply {
    status: u16,
    body: Value,
    headers: axum::http::HeaderMap,
}

async fn send(app: &Router, method: &str, path: &str, bearer: Option<&str>, extra: &[(&str, &str)], body: Option<&Value>) -> Fallible<Reply> {
    let peer: SocketAddr = "192.0.2.90:4000".parse()?;
    let mut request = Request::builder().method(Method::from_bytes(method.as_bytes())?).uri(format!("/api/v1{path}")).extension(ConnectInfo(peer));
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let request = match body {
        Some(body) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string()))?,
        None => request.body(Body::empty())?,
    };
    let response = app.clone().oneshot(request).await?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await?;
    Ok(Reply { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null), headers })
}

async fn principal(pool: &PgPool, name: &str) -> Fallible<PrincipalId> {
    let id = PrincipalId::random();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', $2)")
        .bind(id.to_string())
        .bind(name)
        .execute(pool)
        .await?;
    Ok(id)
}

/// A tenant with one registered host (`host-a`, key `k1`), mapped project `p1` and member `u1`,
/// and service accounts for the four scopes.
struct World {
    tenant: TenantId,
    owner: PrincipalId,
    member: PrincipalId,
    workspace: WorkspaceId,
    project: ProjectId,
    key: Ed25519KeyPair,
    issuer: String,
    all: String,
    read_only: String,
    export_only: String,
}

async fn world(pool: &PgPool) -> Fallible<World> {
    let owner = principal(pool, "s6 owner").await?;
    let member = principal(pool, "s6 member").await?;
    let tenant = TenantId::random();
    let mut t = begin_tenant_transaction(pool, tenant).await.map_err(err)?;
    let slug = format!("s6-{}", &tenant.as_uuid().simple().to_string()[..12]);
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name: "s6 host", plan_id: "team", home_region: "local" }, owner).await.map_err(err)?;
    add_tenant_member(&mut t, member, Role::Editor).await.map_err(err)?;
    let workspace = create_workspace(&mut t, WorkspaceId::random(), "s6 workspace", None).await.map_err(err)?.id;
    let project = create_project(&mut t, ProjectId::random(), workspace, "s6 project", None).await.map_err(err)?.id;
    let rng = SystemRandom::new();
    let key = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "keygen")?.as_ref()).map_err(|_| "pkcs8")?;
    let issuer = format!("https://host-{}.example", Uuid::new_v4().simple());
    register_host_system(
        &mut t,
        &NewHostSystem {
            system_id: "host-a".into(),
            issuer: issuer.clone(),
            audiences: vec!["figcraft".into()],
            allowed_origins: vec!["https://host.example".into()],
            max_role: HostRole::Editor,
            allow_insecure_localhost: false,
        },
    )
    .await
    .map_err(err)?;
    add_host_key(
        &mut t,
        "host-a",
        &NewHostKey { kid: "k1".into(), alg: KeyAlgorithm::EdDsa, public_key: key.public_key().as_ref().to_vec(), not_before: 0, expires_at: None },
    )
    .await
    .map_err(err)?;
    map_external_resource(&mut t, "host-a", MappingKind::Tenant, "t1", tenant.as_uuid()).await.map_err(err)?;
    map_external_resource(&mut t, "host-a", MappingKind::Workspace, "w1", workspace.as_uuid()).await.map_err(err)?;
    map_external_resource(&mut t, "host-a", MappingKind::Project, "p1", project.as_uuid()).await.map_err(err)?;
    map_external_resource(&mut t, "host-a", MappingKind::Principal, "u1", member.as_uuid()).await.map_err(err)?;
    let mut secrets = Vec::new();
    for (name, scopes) in [
        ("all", vec![Scope::DocumentsRead, Scope::DocumentsWrite, Scope::ExportsCreate, Scope::JobsRead, Scope::WebhooksManage]),
        ("read", vec![Scope::DocumentsRead]),
        ("export", vec![Scope::ExportsCreate]),
    ] {
        let account = create_service_account(&mut t, &format!("s6 {name}"), &scopes, owner).await.map_err(err)?;
        secrets.push(issue_credential(&mut t, account.id, 3_600, owner).await.map_err(err)?.secret);
    }
    t.commit().await?;
    let export_only = secrets.pop().ok_or("secret")?;
    let read_only = secrets.pop().ok_or("secret")?;
    let all = secrets.pop().ok_or("secret")?;
    Ok(World { tenant, owner, member, workspace, project, key, issuer, all, read_only, export_only })
}

fn key(value: &str) -> [(&'static str, String); 1] {
    [("idempotency-key", value.to_owned())]
}

fn extra<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(name, value)| (*name, value.as_str())).collect()
}

fn code(reply: &Reply) -> &str {
    reply.body["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn documents_are_created_resolved_archived_restored_and_exported_idempotently() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let world = world(&pool).await?;
    let app = app(pool.clone())?;
    let base = "/host/systems/host-a";
    let create = json!({"external_id": "doc-1", "external_project_id": "p1", "name": "First"});
    let idem = format!("create-{}", Uuid::new_v4().simple());

    // Create, then replay: the same stored response, no second document.
    let first = send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&key(&idem)), Some(&create)).await?;
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.body["external_id"], "doc-1");
    assert_eq!(first.body["state"], "active");
    let document_id = first.body["id"].as_str().ok_or("id")?.to_owned();
    let replay = send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&key(&idem)), Some(&create)).await?;
    assert_eq!((replay.status, &replay.body), (201, &first.body));
    assert_eq!(replay.headers.get("idempotent-replay").and_then(|v| v.to_str().ok()), Some("true"));
    // The same key with another body is a conflict; so is a new key for a mapped external id.
    let other = json!({"external_id": "doc-2", "external_project_id": "p1", "name": "Second"});
    let reuse = send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&key(&idem)), Some(&other)).await?;
    assert_eq!((reuse.status, code(&reuse)), (409, "conflict"));
    let duplicate = send(
        &app,
        "POST",
        &format!("{base}/documents"),
        Some(&world.all),
        &extra(&key(&format!("again-{}", Uuid::new_v4().simple()))),
        Some(&create),
    )
    .await?;
    assert_eq!((duplicate.status, code(&duplicate)), (409, "conflict"));
    // Missing or malformed keys, unknown fields and bad names are 400.
    for headers in [vec![], vec![("idempotency-key", "short".to_owned())], vec![("idempotency-key", "has space in it!".to_owned())]] {
        let bad = send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&headers), Some(&other)).await?;
        assert_eq!(bad.status, 400, "{:?}", headers);
    }
    let unknown = json!({"external_id": "doc-3", "external_project_id": "p1", "name": "x", "extra": 1});
    assert_eq!(
        send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&key("unknown-field-1")), Some(&unknown)).await?.status,
        400
    );
    let blank = json!({"external_id": "doc-3", "external_project_id": "p1", "name": "   "});
    assert_eq!(send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&key("blank-name-0001")), Some(&blank)).await?.status, 400);
    // A project the host has not mapped does not exist for it.
    let unmapped = json!({"external_id": "doc-4", "external_project_id": "nope", "name": "x"});
    assert_eq!(
        send(&app, "POST", &format!("{base}/documents"), Some(&world.all), &extra(&key("unmapped-proj-1")), Some(&unmapped)).await?.status,
        404
    );

    // Resolve.
    let resolved = send(&app, "GET", &format!("{base}/documents/doc-1"), Some(&world.read_only), &[], None).await?;
    assert_eq!((resolved.status, resolved.body["id"].as_str()), (200, Some(document_id.as_str())));
    assert_eq!(send(&app, "GET", &format!("{base}/documents/missing"), Some(&world.read_only), &[], None).await?.status, 404);

    // Archive and restore are idempotent.
    for _ in 0..2 {
        let archived = send(&app, "POST", &format!("{base}/documents/doc-1/archive"), Some(&world.all), &[], None).await?;
        assert_eq!((archived.status, archived.body["state"].as_str()), (200, Some("archived")));
    }
    for _ in 0..2 {
        let restored = send(&app, "POST", &format!("{base}/documents/doc-1/restore"), Some(&world.all), &[], None).await?;
        assert_eq!((restored.status, restored.body["state"].as_str()), (200, Some("active")));
    }
    // Each real change is audited once; repeats are not.
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    let audits: Vec<(String, i64)> =
        sqlx::query_as("SELECT action, count(*) FROM audit_events WHERE action LIKE 'host.document.%' GROUP BY action ORDER BY action")
            .fetch_all(t.connection())
            .await?;
    t.rollback().await?;
    assert_eq!(
        audits,
        vec![("host.document.archive".to_owned(), 1), ("host.document.create".to_owned(), 1), ("host.document.restore".to_owned(), 1)]
    );

    // Export: needs a mapped, active member as the requester; idempotent; visible through the host.
    let request = json!({"format": "png", "external_user_id": "u1"});
    let export_key = format!("export-{}", Uuid::new_v4().simple());
    let accepted =
        send(&app, "POST", &format!("{base}/documents/doc-1/exports"), Some(&world.export_only), &extra(&key(&export_key)), Some(&request)).await?;
    assert_eq!(accepted.status, 202, "{}", accepted.body);
    assert_eq!(accepted.body["state"], "queued");
    let export_id = accepted.body["id"].as_str().ok_or("export id")?.to_owned();
    let again =
        send(&app, "POST", &format!("{base}/documents/doc-1/exports"), Some(&world.export_only), &extra(&key(&export_key)), Some(&request)).await?;
    assert_eq!((again.status, &again.body), (202, &accepted.body));
    let polled = send(&app, "GET", &format!("{base}/exports/{export_id}"), Some(&world.all), &[], None).await?;
    assert_eq!((polled.status, polled.body["id"].as_str()), (200, Some(export_id.as_str())));
    let bad_format = json!({"format": "exe", "external_user_id": "u1"});
    assert_eq!(
        send(&app, "POST", &format!("{base}/documents/doc-1/exports"), Some(&world.export_only), &extra(&key("bad-format-0001")), Some(&bad_format))
            .await?
            .status,
        400
    );
    let stranger = json!({"format": "png", "external_user_id": "nobody"});
    assert_eq!(
        send(&app, "POST", &format!("{base}/documents/doc-1/exports"), Some(&world.export_only), &extra(&key("stranger-0001")), Some(&stranger))
            .await?
            .status,
        404
    );
    assert_eq!(send(&app, "GET", &format!("{base}/exports/{}", Uuid::new_v4()), Some(&world.all), &[], None).await?.status, 404);
    Ok(())
}

#[tokio::test]
async fn scopes_credentials_hosts_and_tenants_are_enforced() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let a = world(&pool).await?;
    let b = world(&pool).await?;
    let app = app(pool.clone())?;
    let base = "/host/systems/host-a";
    let create = json!({"external_id": "doc-1", "external_project_id": "p1", "name": "First"});
    send(&app, "POST", &format!("{base}/documents"), Some(&a.all), &extra(&key("setup-0000001")), Some(&create)).await?;

    // Anonymous is 401 on every authenticated route; a bad bearer too.
    for route in HOST_ROUTES.iter().filter(|route| route.scope.is_some()) {
        let path = route.path.replace("{system_id}", "host-a").replace("{external_id}", "doc-1").replace("{export_id}", &Uuid::new_v4().to_string());
        let body = json!({});
        let reply = send(&app, route.method, &path, None, &[], (route.method == "POST").then_some(&body)).await?;
        assert_eq!(reply.status, 401, "{} {}", route.method, route.path);
        let reply = send(&app, route.method, &path, Some("not-a-credential"), &[], (route.method == "POST").then_some(&body)).await?;
        assert_eq!(reply.status, 401, "{} {}", route.method, route.path);
    }
    // A scope that is not granted is 403 (read-only cannot write, export-only cannot read).
    assert_eq!(
        send(&app, "POST", &format!("{base}/documents"), Some(&a.read_only), &extra(&key("scope-write-001")), Some(&create)).await?.status,
        403
    );
    assert_eq!(send(&app, "POST", &format!("{base}/documents/doc-1/archive"), Some(&a.read_only), &[], None).await?.status, 403);
    assert_eq!(send(&app, "GET", &format!("{base}/documents/doc-1"), Some(&a.export_only), &[], None).await?.status, 403);
    let export = json!({"format": "png", "external_user_id": "u1"});
    assert_eq!(
        send(&app, "POST", &format!("{base}/documents/doc-1/exports"), Some(&a.read_only), &extra(&key("scope-export-01")), Some(&export))
            .await?
            .status,
        403
    );
    assert_eq!(send(&app, "GET", &format!("{base}/exports/{}", Uuid::new_v4()), Some(&a.read_only), &[], None).await?.status, 403);

    // A browser session is not a host credential.
    let session = figcraft_cloud_storage::sessions::create_session(&pool, a.owner, figcraft_cloud_storage::sessions::SessionLimits::default())
        .await
        .map_err(err)?
        .token;
    let reply = send(
        &app,
        "GET",
        &format!("{base}/documents/doc-1"),
        None,
        &[("cookie", &format!("__Host-figcraft_session={session}")), ("x-figcraft-tenant", &a.tenant.to_string())],
        None,
    )
    .await?;
    assert!(matches!(reply.status, 401 | 403), "{}", reply.status);

    // An unregistered system, and another tenant's credential on the same system id, find nothing:
    // B has its own `host-a` with its own mappings, and A's external ids do not exist there.
    assert_eq!(send(&app, "GET", "/host/systems/unknown/documents/doc-1", Some(&a.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "GET", &format!("{base}/documents/doc-1"), Some(&b.all), &[], None).await?.status, 404);
    let created_in_b = send(&app, "POST", &format!("{base}/documents"), Some(&b.all), &extra(&key("tenant-b-doc-001")), Some(&create)).await?;
    assert_eq!(created_in_b.status, 201);
    let a_doc = send(&app, "GET", &format!("{base}/documents/doc-1"), Some(&a.all), &[], None).await?;
    assert_ne!(a_doc.body["id"], created_in_b.body["id"]);
    assert_eq!(a_doc.body["workspace_id"].as_str(), Some(a.workspace.to_string().as_str()));

    // Disabling the host switches the whole API off for it.
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    set_host_enabled(&mut t, "host-a", false).await.map_err(err)?;
    t.commit().await?;
    assert_eq!(send(&app, "GET", &format!("{base}/documents/doc-1"), Some(&a.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "POST", &format!("{base}/documents"), Some(&a.all), &extra(&key("disabled-host-01")), Some(&create)).await?.status, 404);

    // An archived project takes no new documents.
    let mut t = begin_tenant_transaction(&pool, b.tenant).await.map_err(err)?;
    transition_project(&mut t, b.project, Transition::Archive).await.map_err(err)?;
    t.commit().await?;
    let blocked = json!({"external_id": "doc-9", "external_project_id": "p1", "name": "x"});
    let reply = send(&app, "POST", &format!("{base}/documents"), Some(&b.all), &extra(&key("archived-proj-01")), Some(&blocked)).await?;
    assert_eq!((reply.status, code(&reply)), (409, "resource_inactive"));
    Ok(())
}

fn sign(world: &World, jti: &str, mutate: impl FnOnce(&mut Value)) -> String {
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())).unwrap_or(0);
    let mut claims = json!({"iss": world.issuer, "aud": "figcraft", "sub": "u1", "jti": jti, "exp": now + 120, "nbf": now - 5, "iat": now - 5,
        "tenant": "t1", "workspace": "w1", "document": "doc-1", "role": "editor"});
    mutate(&mut claims);
    let enc = |value: &Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let input = format!("{}.{}", enc(&json!({"alg": "EdDSA", "kid": "k1"})), enc(&claims));
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(world.key.sign(input.as_bytes()).as_ref()))
}

#[tokio::test]
async fn the_embed_exchange_trades_a_token_once_for_a_session_and_refuses_everything_else_alike() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let world = world(&pool).await?;
    let app = app(pool.clone())?;
    let create = json!({"external_id": "doc-1", "external_project_id": "p1", "name": "Embedded"});
    let made = send(&app, "POST", "/host/systems/host-a/documents", Some(&world.all), &extra(&key("embed-doc-0001")), Some(&create)).await?;
    assert_eq!(made.status, 201);
    let document = CloudDocumentId::try_from(Uuid::parse_str(made.body["id"].as_str().ok_or("id")?)?).map_err(err)?;
    // The principal needs a grant of its own: the token only narrows what the member already holds.
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    set_document_member(&mut t, document, world.member, Role::Editor).await.map_err(err)?;
    t.commit().await?;
    let same_origin = [("origin", ORIGIN)];

    let token = sign(&world, &Uuid::new_v4().simple().to_string(), |_| {});
    let body = json!({"token": token});
    let ok = send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&body)).await?;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!((ok.body["role"].as_str(), ok.body["document_id"].as_str()), (Some("editor"), Some(document.to_string().as_str())));
    let cookie = ok.headers.get(header::SET_COOKIE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    assert!(
        cookie.starts_with("__Host-figcraft_embed=")
            && cookie.contains("SameSite=None")
            && cookie.contains("Partitioned")
            && cookie.contains("HttpOnly"),
        "{cookie}"
    );
    assert!(!ok.body.to_string().contains(&token));

    // A viewer token narrows the role.
    let viewer = send(
        &app,
        "POST",
        "/auth/embed/exchange",
        None,
        &same_origin,
        Some(&json!({"token": sign(&world, &Uuid::new_v4().simple().to_string(), |c| c["role"] = json!("viewer"))})),
    )
    .await?;
    assert_eq!(viewer.body["role"], "viewer");

    // Replay, forged, expired, wrong audience, unmapped document, role escalation beyond the host
    // maximum, an unknown issuer and garbage are all the same 401 with the same body shape.
    let id = Uuid::new_v4().simple().to_string();
    let used = sign(&world, &id, |_| {});
    assert_eq!(send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": used}))).await?.status, 200);
    let mut cases: Vec<(&str, String)> = vec![
        ("replay", used.clone()),
        ("expired", sign(&world, &Uuid::new_v4().simple().to_string(), |c| c["exp"] = json!(1_000))),
        ("audience", sign(&world, &Uuid::new_v4().simple().to_string(), |c| c["aud"] = json!("other"))),
        ("unmapped document", sign(&world, &Uuid::new_v4().simple().to_string(), |c| c["document"] = json!("nope"))),
        ("unknown issuer", sign(&world, &Uuid::new_v4().simple().to_string(), |c| c["iss"] = json!("https://nobody.example"))),
        ("garbage", "not.a.token".to_owned()),
    ];
    let tampered = sign(&world, &Uuid::new_v4().simple().to_string(), |_| {});
    let mut parts: Vec<String> = tampered.split('.').map(str::to_owned).collect();
    parts[1] = URL_SAFE_NO_PAD.encode(json!({"iss": world.issuer, "aud": "figcraft", "sub": "u1", "jti": "tamper-0000001", "exp": 4_000_000_000_i64, "nbf": 0, "iat": 0, "tenant": "t1", "workspace": "w1", "document": "doc-1", "role": "editor"}).to_string());
    cases.push(("tampered", parts.join(".")));
    let mut shapes = Vec::new();
    for (label, token) in cases {
        let reply = send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": token}))).await?;
        assert_eq!(reply.status, 401, "{label}: {}", reply.body);
        assert!(reply.headers.get(header::SET_COOKIE).is_none(), "{label}");
        shapes.push((reply.body["error"]["code"].clone(), reply.body["error"]["message"].clone()));
    }
    assert!(shapes.windows(2).all(|pair| pair[0] == pair[1]), "refusals must be indistinguishable: {shapes:?}");

    // Cross-origin posts, oversize and malformed bodies never reach the validator.
    let cross = send(&app, "POST", "/auth/embed/exchange", None, &[("origin", "https://evil.example")], Some(&json!({"token": token}))).await?;
    assert_eq!(cross.status, 403);
    assert_eq!(send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": "a".repeat(5_000)}))).await?.status, 400);
    assert_eq!(send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": ""}))).await?.status, 400);
    assert_eq!(send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": "x", "more": 1}))).await?.status, 400);

    // A principal whose own role is lower than the token's: the grant is the ceiling.
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    sqlx::query("UPDATE document_members SET role = 'commenter' WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
        .bind(world.tenant.to_string())
        .bind(world.member.to_string())
        .execute(t.connection())
        .await?;
    t.commit().await?;
    let narrowed = send(
        &app,
        "POST",
        "/auth/embed/exchange",
        None,
        &same_origin,
        Some(&json!({"token": sign(&world, &Uuid::new_v4().simple().to_string(), |_| {})})),
    )
    .await?;
    assert_eq!(narrowed.body["role"], "commenter");

    // Exchanges and denials are audited.
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    let counts: Vec<(String, i64)> =
        sqlx::query_as("SELECT outcome, count(*) FROM audit_events WHERE action = 'embed.exchange' GROUP BY outcome ORDER BY outcome")
            .fetch_all(t.connection())
            .await?;
    t.rollback().await?;
    assert!(
        counts.iter().any(|(outcome, n)| outcome == "succeeded" && *n >= 4) && counts.iter().any(|(outcome, n)| outcome == "denied" && *n >= 3),
        "{counts:?}"
    );
    Ok(())
}

/// The `name=value` pair of the first `Set-Cookie` of a reply.
fn session_cookie(reply: &Reply) -> Fallible<String> {
    let header = reply.headers.get(header::SET_COOKIE).and_then(|value| value.to_str().ok()).ok_or("no cookie")?;
    Ok(header.split(';').next().ok_or("empty cookie")?.to_owned())
}

#[tokio::test]
async fn embedded_sessions_reach_only_their_document_at_most_their_role_and_end_with_the_host() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let world = world(&pool).await?;
    let app = app(pool.clone())?;
    let mut documents = Vec::new();
    for (index, external) in ["doc-1", "doc-2"].into_iter().enumerate() {
        let create = json!({"external_id": external, "external_project_id": "p1", "name": external});
        let made =
            send(&app, "POST", "/host/systems/host-a/documents", Some(&world.all), &extra(&key(&format!("embed-sess-{index:04}"))), Some(&create))
                .await?;
        assert_eq!(made.status, 201);
        let id = CloudDocumentId::try_from(Uuid::parse_str(made.body["id"].as_str().ok_or("id")?)?).map_err(err)?;
        let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
        set_document_member(&mut t, id, world.member, Role::Editor).await.map_err(err)?;
        t.commit().await?;
        documents.push(id);
    }
    let (own, other) = (documents[0], documents[1]);
    let tenant = world.tenant.to_string();
    let same_origin = [("origin", ORIGIN)];
    let exchange = |role: &'static str| {
        let token = sign(&world, &Uuid::new_v4().simple().to_string(), |c| c["role"] = json!(role));
        let app = app.clone();
        async move { send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": token}))).await }
    };

    // A viewer token over a member who holds editor: the embed is held to viewer.
    let viewer = exchange("viewer").await?;
    assert_eq!(viewer.status, 200, "{}", viewer.body);
    let cookie = session_cookie(&viewer)?;
    let headers = [("cookie", cookie.as_str()), ("x-figcraft-tenant", tenant.as_str()), ("origin", ORIGIN)];
    let document = send(&app, "GET", &format!("/documents/{own}"), None, &headers, None).await?;
    assert_eq!(document.status, 200, "{}", document.body);
    assert_eq!(document.body["can_edit"], false, "a viewer embed cannot edit");
    let ticket = send(&app, "POST", &format!("/documents/{own}/realtime-ticket"), None, &headers, None).await?;
    assert_eq!((ticket.status, ticket.body["access"].as_str()), (200, Some("view")));

    // Everything outside the one document and the editor's few routes is refused.
    for (method, path) in [
        ("GET", format!("/documents/{other}")),
        ("POST", format!("/documents/{other}/realtime-ticket")),
        ("GET", format!("/documents/{own}/members")),
        ("GET", format!("/documents/{own}/share-links")),
        ("POST", format!("/documents/{own}/archive")),
        ("PATCH", format!("/documents/{own}")),
        ("GET", "/tenants".to_owned()),
        ("GET", format!("/tenants/{tenant}")),
    ] {
        let body = json!({});
        let reply = send(&app, method, &path, None, &headers, (method != "GET").then_some(&body)).await?;
        assert_eq!(reply.status, 403, "{method} {path}: {}", reply.body);
    }
    // The session is bound to its tenant, whatever the header says.
    let elsewhere = TenantId::random().to_string();
    let wrong_tenant = [("cookie", cookie.as_str()), ("x-figcraft-tenant", elsewhere.as_str())];
    assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, &wrong_tenant, None).await?.status, 403);

    // An editor token over the same member may edit.
    let editor = exchange("editor").await?;
    let editor_cookie = session_cookie(&editor)?;
    let editor_headers = [("cookie", editor_cookie.as_str()), ("x-figcraft-tenant", tenant.as_str()), ("origin", ORIGIN)];
    let ticket = send(&app, "POST", &format!("/documents/{own}/realtime-ticket"), None, &editor_headers, None).await?;
    assert_eq!((ticket.status, ticket.body["access"].as_str()), (200, Some("edit")));

    // Disabling the host ends every session it opened, at once and in the same transaction.
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    assert!(set_host_enabled(&mut t, "host-a", false).await.map_err(err)?);
    t.commit().await?;
    for h in [&headers[..], &editor_headers[..]] {
        assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, h, None).await?.status, 401);
    }
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    assert!(set_host_enabled(&mut t, "host-a", true).await.map_err(err)?);
    t.commit().await?;
    // Enabling again does not bring a revoked session back.
    assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, &headers, None).await?.status, 401);

    // Removing the document mapping ends the sessions on that document, and only those.
    let first = exchange("editor").await?;
    let first_cookie = session_cookie(&first)?;
    let first_headers = [("cookie", first_cookie.as_str()), ("x-figcraft-tenant", tenant.as_str()), ("origin", ORIGIN)];
    assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, &first_headers, None).await?.status, 200);
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    assert!(figcraft_cloud_storage::host_mappings::unmap_external_resource(&mut t, "host-a", MappingKind::Document, "doc-1").await.map_err(err)?);
    t.commit().await?;
    assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, &first_headers, None).await?.status, 401);

    // Removing the principal mapping ends that person's sessions.
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    map_external_resource(&mut t, "host-a", MappingKind::Document, "doc-1", own.as_uuid()).await.map_err(err)?;
    t.commit().await?;
    let second = exchange("viewer").await?;
    let second_cookie = session_cookie(&second)?;
    let second_headers = [("cookie", second_cookie.as_str()), ("x-figcraft-tenant", tenant.as_str()), ("origin", ORIGIN)];
    assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, &second_headers, None).await?.status, 200);
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    assert!(figcraft_cloud_storage::host_mappings::unmap_external_resource(&mut t, "host-a", MappingKind::Principal, "u1").await.map_err(err)?);
    t.commit().await?;
    assert_eq!(send(&app, "GET", &format!("/documents/{own}"), None, &second_headers, None).await?.status, 401);
    Ok(())
}

#[tokio::test]
async fn concurrent_identical_creates_make_exactly_one_document() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let world = world(&pool).await?;
    let app = app(pool.clone())?;
    let create = json!({"external_id": "race-1", "external_project_id": "p1", "name": "Race"});
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let (app, bearer, create) = (app.clone(), world.all.clone(), create.clone());
        tasks.push(tokio::spawn(async move {
            let reply =
                send(&app, "POST", "/host/systems/host-a/documents", Some(&bearer), &[("idempotency-key", "race-key-0000001")], Some(&create)).await;
            reply.map(|reply| (reply.status, reply.body["id"].as_str().map(str::to_owned))).map_err(|error| error.to_string())
        }));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut created = 0;
    for task in tasks {
        let (status, id) = task.await?.map_err(err)?;
        // The loser of the reservation race either waits and replays (201) or is told the first
        // attempt is still running (409); it never makes a second document.
        assert!(matches!(status, 201 | 409), "{status}");
        if status == 201 {
            created += 1;
            ids.insert(id.ok_or("id")?);
        }
    }
    assert!(created >= 1);
    assert_eq!(ids.len(), 1, "one document id across all replays: {ids:?}");
    let mut t = begin_tenant_transaction(&pool, world.tenant).await.map_err(err)?;
    let documents: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE tenant_id = $1::uuid")
        .bind(world.tenant.to_string())
        .fetch_one(t.connection())
        .await?;
    t.rollback().await?;
    assert_eq!(documents, 1);
    Ok(())
}

#[tokio::test]
async fn webhook_endpoints_are_managed_with_secrets_shown_once_and_stored_encrypted() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let a = world(&pool).await?;
    let b = world(&pool).await?;
    let app = app(pool.clone())?;
    let base = "/host/systems/host-a/webhooks";
    let create = json!({"url": "https://hooks.example.com/figcraft/a1b2c3", "event_types": ["document.created", "document.archived"]});

    let made = send(&app, "POST", base, Some(&a.all), &[], Some(&create)).await?;
    assert_eq!(made.status, 201, "{}", made.body);
    let secret = made.body["secret"].as_str().ok_or("secret")?.to_owned();
    assert!(secret.starts_with("whsec_") && secret.len() == 49, "{secret}");
    let id = made.body["id"].as_str().ok_or("id")?.to_owned();
    assert_eq!(made.body["event_types"], json!(["document.created", "document.archived"]));
    assert_eq!(made.body["enabled"], true);

    // No read ever returns the secret again.
    let one = send(&app, "GET", &format!("{base}/{id}"), Some(&a.all), &[], None).await?;
    let list = send(&app, "GET", base, Some(&a.all), &[], None).await?;
    assert_eq!((one.status, list.status), (200, 200));
    assert!(!one.body.to_string().contains(&secret) && !list.body.to_string().contains(&secret));
    assert!(one.body.get("secret").is_none() && list.body["webhooks"][0].get("secret").is_none());
    assert_eq!(list.body["webhooks"].as_array().map(Vec::len), Some(1));

    // At rest the secret is sealed: the column does not contain it, and it opens only with the
    // deployment key and this endpoint's context.
    let endpoint_id: Uuid = id.parse()?;
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT secret_sealed FROM webhook_endpoints WHERE id = $1::uuid").bind(&id).fetch_one(t.connection()).await?;
    assert!(!sealed.windows(secret.len()).any(|window| window == secret.as_bytes()));
    let key = figcraft_cloud_storage::secret_box::SecretKey::derive(b"s6-host-api-signing-key-0123456").map_err(err)?;
    assert_eq!(figcraft_cloud_storage::webhooks::signing_secrets(&mut t, &key, endpoint_id).await.map_err(err)?, vec![secret.clone().into_bytes()]);
    let wrong = figcraft_cloud_storage::secret_box::SecretKey::derive(b"another-signing-key-0123456789").map_err(err)?;
    assert!(figcraft_cloud_storage::webhooks::signing_secrets(&mut t, &wrong, endpoint_id).await.is_err());
    t.rollback().await?;

    // Bad URLs (private, plain http, credentials, internal names, metadata service) and bad
    // subscriptions are refused; nothing is created.
    for url in [
        "http://hooks.example.com/x",
        "https://localhost/x",
        "https://127.0.0.1/x",
        "https://169.254.169.254/x",
        "https://user:pw@hooks.example.com/x",
        "https://intranet/x",
        "not a url",
        "",
    ] {
        let reply = send(&app, "POST", base, Some(&a.all), &[], Some(&json!({"url": url}))).await?;
        assert_eq!(reply.status, 400, "{url}");
    }
    for events in [json!(["document.exploded"]), json!(["document.created", "document.created"]), json!(["*"])] {
        let reply = send(&app, "POST", base, Some(&a.all), &[], Some(&json!({"url": "https://hooks.example.com/x", "event_types": events}))).await?;
        assert_eq!(reply.status, 400, "{events}");
    }
    assert_eq!(send(&app, "POST", base, Some(&a.all), &[], Some(&json!({"url": "https://hooks.example.com/x", "extra": 1}))).await?.status, 400);

    // Update.
    let patched = send(&app, "PATCH", &format!("{base}/{id}"), Some(&a.all), &[], Some(&json!({"enabled": false, "event_types": []}))).await?;
    assert_eq!((patched.status, patched.body["enabled"].clone(), patched.body["event_types"].clone()), (200, json!(false), json!([])));
    assert_eq!(
        send(&app, "PATCH", &format!("{base}/{id}"), Some(&a.all), &[], Some(&json!({"url": "http://insecure.example.com/"}))).await?.status,
        400
    );

    // Rotation: a new secret, the old one still signs during the overlap.
    let rotated = send(&app, "POST", &format!("{base}/{id}/rotate-secret"), Some(&a.all), &[], None).await?;
    assert_eq!(rotated.status, 200);
    let new_secret = rotated.body["secret"].as_str().ok_or("secret")?.to_owned();
    assert_ne!(new_secret, secret);
    assert_eq!(rotated.body["previous_secret_active"], true);
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    let both = figcraft_cloud_storage::webhooks::signing_secrets(&mut t, &key, endpoint_id).await.map_err(err)?;
    assert_eq!(both, vec![new_secret.clone().into_bytes(), secret.clone().into_bytes()]);
    // Once the overlap has passed only the new secret remains.
    sqlx::query("UPDATE webhook_endpoints SET previous_expires_at = clock_timestamp() - interval '1 second' WHERE id = $1::uuid")
        .bind(&id)
        .execute(t.connection())
        .await?;
    assert_eq!(
        figcraft_cloud_storage::webhooks::signing_secrets(&mut t, &key, endpoint_id).await.map_err(err)?,
        vec![new_secret.clone().into_bytes()]
    );
    t.commit().await?;

    // Audit rows carry ids only: no URL, no secret.
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT action, metadata::text FROM audit_events WHERE action LIKE 'host.webhook.%' ORDER BY action")
            .fetch_all(t.connection())
            .await?;
    t.rollback().await?;
    let actions: Vec<&str> = audits.iter().map(|(action, _)| action.as_str()).collect();
    assert_eq!(actions, vec!["host.webhook.create", "host.webhook.rotate_secret", "host.webhook.update"]);
    for (_, metadata) in &audits {
        assert!(!metadata.contains("hooks.example.com") && !metadata.contains("whsec_"), "{metadata}");
    }

    // Scope, tenant isolation, cap, delete.
    assert_eq!(send(&app, "POST", base, Some(&a.read_only), &[], Some(&create)).await?.status, 403);
    assert_eq!(send(&app, "GET", base, Some(&a.read_only), &[], None).await?.status, 403);
    assert_eq!(send(&app, "GET", &format!("{base}/{id}"), Some(&b.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "DELETE", &format!("{base}/{id}"), Some(&b.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "POST", &format!("{base}/{id}/rotate-secret"), Some(&b.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "GET", &format!("{base}/{}", Uuid::new_v4()), Some(&a.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "GET", &format!("{base}/not-a-uuid"), Some(&a.all), &[], None).await?.status, 404);
    for _ in 0..9 {
        assert_eq!(send(&app, "POST", base, Some(&a.all), &[], Some(&create)).await?.status, 201);
    }
    let over = send(&app, "POST", base, Some(&a.all), &[], Some(&create)).await?;
    assert_eq!((over.status, code(&over)), (429, "quota_exceeded"), "{}", over.body);
    assert_eq!(send(&app, "DELETE", &format!("{base}/{id}"), Some(&a.all), &[], None).await?.status, 204);
    assert_eq!(send(&app, "GET", &format!("{base}/{id}"), Some(&a.all), &[], None).await?.status, 404);

    // Without a deployment signing key secrets cannot be sealed: the feature is off, not insecure.
    let keyless = app_with(pool.clone(), None)?;
    assert_eq!(send(&keyless, "POST", base, Some(&b.all), &[], Some(&create)).await?.status, 503);
    Ok(())
}

#[tokio::test]
async fn webhook_deliveries_can_be_listed_inspected_and_replayed_through_the_api() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let a = world(&pool).await?;
    let b = world(&pool).await?;
    let app = app(pool.clone())?;
    let base = "/host/systems/host-a/webhooks";
    let made = send(&app, "POST", base, Some(&a.all), &[], Some(&json!({"url": "https://hooks.example.com/x"}))).await?;
    let endpoint = made.body["id"].as_str().ok_or("id")?.to_owned();
    let endpoint_id: Uuid = endpoint.parse()?;

    // Two deliveries: one failed (after two attempts), one pending.
    let (failed_event, pending_event) = (Uuid::new_v4(), Uuid::new_v4());
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    let failed =
        figcraft_cloud_storage::webhook_delivery::enqueue_delivery(&mut t, endpoint_id, failed_event, "document.created", b"{\"id\":\"x\"}", 2)
            .await
            .map_err(err)?
            .ok_or("enqueue")?;
    let pending =
        figcraft_cloud_storage::webhook_delivery::enqueue_delivery(&mut t, endpoint_id, pending_event, "document.archived", b"{\"id\":\"y\"}", 8)
            .await
            .map_err(err)?
            .ok_or("enqueue")?;
    sqlx::query("UPDATE webhook_deliveries SET state = 'failed', attempts = 2, last_status = 500, last_error = 'http_status', completed_at = clock_timestamp() WHERE id = $1::uuid")
        .bind(failed.to_string())
        .execute(t.connection())
        .await?;
    sqlx::query("INSERT INTO webhook_delivery_attempts (tenant_id, delivery_id, attempt, duration_ms, http_status, error) VALUES ($1::uuid, $2::uuid, 1, 12, 500, 'http_status'), ($1::uuid, $2::uuid, 2, 9, 500, 'http_status')")
        .bind(a.tenant.to_string())
        .bind(failed.to_string())
        .execute(t.connection())
        .await?;
    t.commit().await?;

    let list = send(&app, "GET", &format!("{base}/{endpoint}/deliveries"), Some(&a.all), &[], None).await?;
    assert_eq!(list.status, 200);
    assert_eq!(list.body["deliveries"].as_array().map(Vec::len), Some(2));
    let only_failed = send(&app, "GET", &format!("{base}/{endpoint}/deliveries?state=failed"), Some(&a.all), &[], None).await?;
    assert_eq!(only_failed.body["deliveries"][0]["id"].as_str(), Some(failed.to_string().as_str()));
    assert_eq!(send(&app, "GET", &format!("{base}/{endpoint}/deliveries?state=bogus"), Some(&a.all), &[], None).await?.status, 400);

    let detail = send(&app, "GET", &format!("{base}/{endpoint}/deliveries/{failed}"), Some(&a.all), &[], None).await?;
    assert_eq!(
        (detail.status, detail.body["state"].as_str(), detail.body["event_id"].as_str()),
        (200, Some("failed"), Some(failed_event.to_string().as_str()))
    );
    assert_eq!(detail.body["attempt_log"].as_array().map(Vec::len), Some(2));
    assert_eq!(detail.body["attempt_log"][0]["attempt"], 2);
    // Bodies and secrets are never part of a delivery view.
    assert!(!detail.body.to_string().contains("\"id\\\":\\\"x\\\"") && detail.body.get("body").is_none());

    // Replay: a failed delivery goes back to pending with the same event id; a pending one is 409.
    let replayed = send(&app, "POST", &format!("{base}/{endpoint}/deliveries/{failed}/replay"), Some(&a.all), &[], None).await?;
    assert_eq!((replayed.status, replayed.body["state"].as_str(), replayed.body["attempts"].as_i64()), (202, Some("pending"), Some(0)));
    assert_eq!(replayed.body["event_id"].as_str(), Some(failed_event.to_string().as_str()));
    let again = send(&app, "POST", &format!("{base}/{endpoint}/deliveries/{failed}/replay"), Some(&a.all), &[], None).await?;
    assert_eq!((again.status, code(&again)), (409, "conflict"));
    let pending_replay = send(&app, "POST", &format!("{base}/{endpoint}/deliveries/{pending}/replay"), Some(&a.all), &[], None).await?;
    assert_eq!(pending_replay.status, 409);

    // Scope and isolation.
    assert_eq!(send(&app, "GET", &format!("{base}/{endpoint}/deliveries"), Some(&a.read_only), &[], None).await?.status, 403);
    assert_eq!(send(&app, "POST", &format!("{base}/{endpoint}/deliveries/{failed}/replay"), Some(&a.read_only), &[], None).await?.status, 403);
    assert_eq!(send(&app, "GET", &format!("{base}/{endpoint}/deliveries"), Some(&b.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "GET", &format!("{base}/{endpoint}/deliveries/{failed}"), Some(&b.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "POST", &format!("{base}/{endpoint}/deliveries/{failed}/replay"), Some(&b.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "GET", &format!("{base}/{}/deliveries", Uuid::new_v4()), Some(&a.all), &[], None).await?.status, 404);
    assert_eq!(send(&app, "GET", &format!("{base}/{endpoint}/deliveries/{}", Uuid::new_v4()), Some(&a.all), &[], None).await?.status, 404);

    // Deleting the endpoint removes its deliveries and their attempts.
    assert_eq!(send(&app, "DELETE", &format!("{base}/{endpoint}"), Some(&a.all), &[], None).await?.status, 204);
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    let left: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM webhook_deliveries) + (SELECT count(*) FROM webhook_delivery_attempts)")
        .fetch_one(t.connection())
        .await?;
    t.rollback().await?;
    assert_eq!(left, 0);
    Ok(())
}

#[tokio::test]
async fn document_changes_publish_minimal_events_to_subscribed_endpoints_only() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let a = world(&pool).await?;
    let b = world(&pool).await?;
    let app = app(pool.clone())?;
    let base = "/host/systems/host-a/webhooks";
    let hook = |events: Value| json!({"url": "https://hooks.example.com/x", "event_types": events});
    let all = send(&app, "POST", base, Some(&a.all), &[], Some(&hook(json!([])))).await?.body["id"].as_str().ok_or("id")?.to_owned();
    let all_too = send(&app, "POST", base, Some(&a.all), &[], Some(&hook(json!([])))).await?.body["id"].as_str().ok_or("id")?.to_owned();
    let archived_only =
        send(&app, "POST", base, Some(&a.all), &[], Some(&hook(json!(["document.archived"])))).await?.body["id"].as_str().ok_or("id")?.to_owned();
    let updated_only =
        send(&app, "POST", base, Some(&a.all), &[], Some(&hook(json!(["document.updated"])))).await?.body["id"].as_str().ok_or("id")?.to_owned();
    let off = send(&app, "POST", base, Some(&a.all), &[], Some(&hook(json!([])))).await?.body["id"].as_str().ok_or("id")?.to_owned();
    assert_eq!(send(&app, "PATCH", &format!("{base}/{off}"), Some(&a.all), &[], Some(&json!({"enabled": false}))).await?.status, 200);
    // Tenant B has an endpoint for the same system id; it must see none of A's events.
    let theirs = send(&app, "POST", base, Some(&b.all), &[], Some(&hook(json!([])))).await?.body["id"].as_str().ok_or("id")?.to_owned();

    let count = |endpoint: String, tenant: TenantId, filter: &'static str| {
        let pool = pool.clone();
        async move {
            let mut t = begin_tenant_transaction(&pool, tenant).await.map_err(err)?;
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE endpoint_id = $1::uuid AND ($2 = '' OR event_type = $2)")
                .bind(&endpoint)
                .bind(filter)
                .fetch_one(t.connection())
                .await?;
            t.rollback().await?;
            Ok::<i64, Box<dyn std::error::Error>>(n)
        }
    };

    // Create through the host API: only document.created, to the everything endpoints.
    let create = json!({"external_id": "doc-1", "external_project_id": "p1", "name": "Quarterly secret plans"});
    let made = send(&app, "POST", "/host/systems/host-a/documents", Some(&a.all), &extra(&key("event-create-01")), Some(&create)).await?;
    assert_eq!(made.status, 201);
    let document = made.body["id"].as_str().ok_or("id")?.to_owned();
    assert_eq!((count(all.clone(), a.tenant, "document.created").await?, count(all_too.clone(), a.tenant, "").await?), (1, 1));
    assert_eq!(
        (
            count(archived_only.clone(), a.tenant, "").await?,
            count(updated_only.clone(), a.tenant, "").await?,
            count(off.clone(), a.tenant, "").await?
        ),
        (0, 0, 0)
    );
    // A refused create (duplicate) publishes nothing: the event rolled back with the change.
    let duplicate = send(&app, "POST", "/host/systems/host-a/documents", Some(&a.all), &extra(&key("event-create-02")), Some(&create)).await?;
    assert_eq!(duplicate.status, 409);
    assert_eq!(count(all.clone(), a.tenant, "").await?, 1);
    // Replaying the create (same idempotency key) does not publish again either.
    let replay = send(&app, "POST", "/host/systems/host-a/documents", Some(&a.all), &extra(&key("event-create-01")), Some(&create)).await?;
    assert_eq!(replay.status, 201);
    assert_eq!(count(all.clone(), a.tenant, "").await?, 1);

    // Archive: the everything endpoints and the archive-only endpoint. Repeating it is a no-op.
    for _ in 0..2 {
        assert_eq!(send(&app, "POST", "/host/systems/host-a/documents/doc-1/archive", Some(&a.all), &[], None).await?.status, 200);
    }
    assert_eq!((count(all.clone(), a.tenant, "document.archived").await?, count(archived_only.clone(), a.tenant, "").await?), (1, 1));

    // A person restores and renames it in the browser API: the host hears about it too.
    let session = figcraft_cloud_storage::sessions::create_session(&pool, a.owner, figcraft_cloud_storage::sessions::SessionLimits::default())
        .await
        .map_err(err)?
        .token;
    let cookie = format!("__Host-figcraft_session={session}");
    let tenant_header = a.tenant.to_string();
    let browser = [("cookie", cookie.as_str()), ("x-figcraft-tenant", tenant_header.as_str()), ("origin", ORIGIN)];
    let restored = send(&app, "POST", &format!("/documents/{document}/restore"), None, &browser, None).await?;
    assert_eq!(restored.status, 200, "{}", restored.body);
    let renamed = send(&app, "PATCH", &format!("/documents/{document}"), None, &browser, Some(&json!({"name": "Renamed behind your back"}))).await?;
    assert_eq!(renamed.status, 200, "{}", renamed.body);
    assert_eq!((count(all.clone(), a.tenant, "document.restored").await?, count(all.clone(), a.tenant, "document.updated").await?), (1, 1));
    assert_eq!(count(updated_only.clone(), a.tenant, "").await?, 1);
    assert_eq!(count(archived_only.clone(), a.tenant, "").await?, 1, "no new events for the archive-only endpoint");
    assert_eq!(count(off.clone(), a.tenant, "").await?, 0, "a disabled endpoint receives nothing");
    assert_eq!(count(all.clone(), a.tenant, "").await?, 4);

    // Every body carries ids only: no document name, no matter how the change was made.
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    let bodies: Vec<(String, String)> =
        sqlx::query_as("SELECT event_type, convert_from(body, 'UTF8') FROM webhook_deliveries WHERE endpoint_id = $1::uuid ORDER BY created_at")
            .bind(&all)
            .fetch_all(t.connection())
            .await?;
    let shared: Vec<(String, String)> =
        sqlx::query_as("SELECT event_id::text, endpoint_id::text FROM webhook_deliveries WHERE event_type = 'document.created' ORDER BY endpoint_id")
            .fetch_all(t.connection())
            .await?;
    t.rollback().await?;
    assert_eq!(
        bodies.iter().map(|(event, _)| event.as_str()).collect::<Vec<_>>(),
        vec!["document.created", "document.archived", "document.restored", "document.updated"]
    );
    for (event, body) in &bodies {
        let parsed: Value = serde_json::from_str(body)?;
        let mut top: Vec<&str> = parsed.as_object().ok_or("object")?.keys().map(String::as_str).collect();
        top.sort_unstable();
        assert_eq!(top, ["created_at", "data", "id", "tenant_id", "type"], "{event}");
        let mut data: Vec<&str> = parsed["data"].as_object().ok_or("data")?.keys().map(String::as_str).collect();
        data.sort_unstable();
        assert_eq!(data, ["document_id", "external_document_id", "project_id", "workspace_id"], "{event}");
        assert_eq!(parsed["data"]["document_id"].as_str(), Some(document.as_str()));
        assert_eq!(parsed["data"]["external_document_id"], "doc-1");
        assert!(!body.contains("secret plans") && !body.contains("Renamed"), "no document name in {body}");
    }
    // The same event goes to its endpoints under one event id.
    assert_eq!(shared.len(), 2);
    assert_eq!(shared[0].0, shared[1].0);

    // Tenant B saw nothing; a host that is disabled stops publishing altogether.
    assert_eq!(count(theirs.clone(), b.tenant, "").await?, 0);
    let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
    set_host_enabled(&mut t, "host-a", false).await.map_err(err)?;
    t.commit().await?;
    assert_eq!(send(&app, "POST", &format!("/documents/{document}/archive"), None, &browser, None).await?.status, 200);
    assert_eq!(count(all.clone(), a.tenant, "").await?, 4, "a disabled host gets no events");
    Ok(())
}

#[tokio::test]
async fn the_embed_exchange_is_rate_limited_per_issuer() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let world = world(&pool).await?;
    // A small bucket that never refills in the test, so timing cannot matter.
    let limit = RateLimitConfig { burst: 5, refill_every: Duration::from_secs(3600), max_keys: 16 };
    let app = app_limited(pool.clone(), Some(b"s6-host-api-signing-key-0123456"), limit)?;
    let same_origin = [("origin", ORIGIN)];
    let mut limited = 0;
    for _ in 0..12 {
        let reply = send(&app, "POST", "/auth/embed/exchange", None, &same_origin, Some(&json!({"token": sign(&world, "short", |_| {})}))).await?;
        if reply.status == 429 {
            limited += 1;
            assert!(reply.headers.get(header::RETRY_AFTER).is_some());
            assert_eq!(code(&reply), "rate_limited");
        } else {
            assert_eq!(reply.status, 401);
        }
    }
    assert_eq!(limited, 7, "a burst of 5 is allowed, the other 7 of 12 are throttled");
    // The production limit is a real burst, not a trickle.
    const { assert!(super::host_api::EMBED_LIMIT.burst >= 60) };
    Ok(())
}

#[tokio::test]
async fn the_route_table_matches_the_router() -> Fallible<()> {
    // Every documented route is registered (not 404/405 for an anonymous caller) and nothing the
    // table does not list is: the same lists back the OpenAPI document test.
    let pool = PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none")?;
    let app = app(pool)?;
    for route in HOST_ROUTES {
        let path = route.path.replace("{system_id}", "s").replace("{external_id}", "e").replace("{export_id}", &Uuid::new_v4().to_string());
        let reply =
            send(&app, route.method, &path, None, &[("origin", ORIGIN)], (route.method == "POST").then(|| json!({"token": "x"})).as_ref()).await?;
        assert!(!matches!(reply.status, 404 | 405) || route.path.contains("openapi"), "{} {} -> {}", route.method, route.path, reply.status);
        // Every method the table does not list for this path is not routed.
        for wrong in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            if HOST_ROUTES.iter().any(|other| other.path == route.path && other.method == wrong) {
                continue;
            }
            let reply = send(&app, wrong, &path, None, &[], None).await?;
            assert!(matches!(reply.status, 404 | 405), "{wrong} {} must not exist, got {}", route.path, reply.status);
        }
    }
    Ok(())
}
