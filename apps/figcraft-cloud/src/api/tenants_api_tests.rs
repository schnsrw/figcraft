//! Router tests of the tenant, member, team and invitation APIs (S4-019, S4-020) against the dev
//! PostgreSQL (fixture prefix d2; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE). Every run
//! creates fresh d2-prefixed tenants, principals and slugs, so reruns pass single-threaded.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use figcraft_cloud_domain::{DeploymentMode, PrincipalId, Role, TenantContext, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::add_tenant_member;
use figcraft_cloud_storage::metadata::create_workspace;
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tower::ServiceExt as _;

use crate::api::auth::{AuthFuture, AuthRejection, AuthRequest, Authenticated, Authenticator, CredentialKind, SessionInfo};
use crate::api::context::ContextSettings;
use crate::api::{ApiConfig, AppState, api_router};
use crate::config::CloudRole;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

fn d2() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xd2;
    Uuid::from_bytes(bytes)
}

fn suffix() -> String {
    d2().simple().to_string()[2..14].to_owned()
}

/// `x-test-caller: <principal> <tenant> <session|service|share>`.
struct HeaderAuthenticator;

impl Authenticator for HeaderAuthenticator {
    fn authenticate<'a>(&'a self, request: &'a AuthRequest<'a>) -> AuthFuture<'a> {
        let header = request.headers.get("x-test-caller").and_then(|value| value.to_str().ok()).map(str::to_owned);
        Box::pin(async move {
            let header = header.ok_or(AuthRejection::Unauthenticated)?;
            let mut parts = header.split(' ');
            let principal: PrincipalId = parts.next().and_then(|part| part.parse().ok()).ok_or(AuthRejection::Unauthenticated)?;
            let tenant: TenantId = parts.next().and_then(|part| part.parse().ok()).ok_or(AuthRejection::Unauthenticated)?;
            let kind = match parts.next() {
                Some("service") => CredentialKind::ServiceAccount,
                Some("share") => CredentialKind::ShareLink,
                _ => CredentialKind::BrowserSession,
            };
            Ok(Authenticated {
                principal,
                tenant: TenantContext::new(tenant, principal, DeploymentMode::MultiTenant, Role::Viewer, []),
                session: SessionInfo { kind, id: Uuid::new_v4().to_string() },
                grant: crate::api::auth::Grant::Member,
            })
        })
    }
}

async fn pool() -> Fallible<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; tenant API probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

fn app(pool: &PgPool, deployment: DeploymentMode) -> Router {
    let objects = figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".to_string(),
        region: "us-east-1".to_string(),
        bucket: "figcraft-test".to_string(),
        access_key: "FCtestkey".to_string(),
        secret_key: "FCtestsecret".to_string(),
        path_style: true,
    })
    .unwrap();
    let config = ApiConfig { deployment, ..ApiConfig::default() };
    let state = AppState::new(pool.clone(), objects, config).with_authenticator(Arc::new(HeaderAuthenticator));
    crate::health::app(CloudRole::Api, true, Some(api_router(state)), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(10), 1 << 20))
}

#[derive(Clone, Copy, Debug)]
struct Caller {
    principal: PrincipalId,
    tenant: TenantId,
    kind: &'static str,
}

impl Caller {
    fn as_kind(self, kind: &'static str) -> Self {
        Self { kind, ..self }
    }
}

async fn call(app: &Router, caller: Option<Caller>, method: Method, path: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(caller) = caller {
        request = request.header("x-test-caller", format!("{} {} {}", caller.principal, caller.tenant, caller.kind));
    }
    let request = match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
}

fn code(body: &serde_json::Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

async fn principal(pool: &PgPool, name: &str) -> Fallible<PrincipalId> {
    let id = d2();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', $2)")
        .bind(id.to_string())
        .bind(name)
        .execute(pool)
        .await?;
    Ok(PrincipalId::try_from(id).map_err(|_| "nil")?)
}

/// A tenant with an owner, an admin, an editor and a viewer.
struct World {
    tenant: TenantId,
    owner: Caller,
    admin: Caller,
    editor: Caller,
    viewer: Caller,
}

async fn world(pool: &PgPool) -> Fallible<World> {
    let tenant = TenantId::try_from(d2()).map_err(|_| "nil")?;
    let owner = principal(pool, "d2 Owner").await?;
    let admin = principal(pool, "d2 Admin").await?;
    let editor = principal(pool, "d2 Editor").await?;
    let viewer = principal(pool, "d2 Viewer").await?;
    let slug = format!("d2-{}", suffix());
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    create_tenant_with_owner(&mut transaction, NewTenant { slug: &slug, name: "d2 tenant", plan_id: "team", home_region: "local" }, owner).await?;
    add_tenant_member(&mut transaction, admin, Role::TenantAdmin).await.map_err(|error| format!("{error:?}"))?;
    add_tenant_member(&mut transaction, editor, Role::Editor).await.map_err(|error| format!("{error:?}"))?;
    add_tenant_member(&mut transaction, viewer, Role::Viewer).await.map_err(|error| format!("{error:?}"))?;
    transaction.commit().await?;
    let caller = |principal| Caller { principal, tenant, kind: "session" };
    Ok(World { tenant, owner: caller(owner), admin: caller(admin), editor: caller(editor), viewer: caller(viewer) })
}

async fn workspace(pool: &PgPool, tenant: TenantId) -> Fallible<WorkspaceId> {
    let id = WorkspaceId::try_from(d2()).map_err(|_| "nil")?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    create_workspace(&mut transaction, id, "d2 workspace", None).await.map_err(|error| format!("{error:?}"))?;
    transaction.commit().await?;
    Ok(id)
}

async fn audit_count(pool: &PgPool, tenant: TenantId, action: &str) -> Fallible<i64> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let count = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = $2")
        .bind(tenant.to_string())
        .bind(action)
        .fetch_one(transaction.connection())
        .await?;
    Ok(count)
}

#[tokio::test]
async fn tenant_routes_follow_the_role_matrix_and_disclosure_policy() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::SingleTenant);
    let world = world(&pool).await?;
    let other = world_outsider(&pool).await?;
    let path = format!("/api/v1/tenants/{}", world.tenant);

    // View: every member; another tenant's member and malformed ids are 404; share links 403;
    // no credential 401.
    for caller in [world.owner, world.admin, world.editor, world.viewer] {
        let (status, body) = call(&app, Some(caller), Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{caller:?} {body}");
        assert_eq!(body["id"], world.tenant.to_string());
    }
    assert_eq!(call(&app, Some(other), Method::GET, &path, None).await.0, StatusCode::NOT_FOUND);
    // Even with a credential naming the tenant, a non-member sees 404.
    let stranger = Caller { tenant: world.tenant, ..other };
    assert_eq!(call(&app, Some(stranger), Method::GET, &path, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(world.owner), Method::GET, "/api/v1/tenants/not-a-uuid", None).await.0, StatusCode::NOT_FOUND);
    let (status, body) = call(&app, Some(world.owner.as_kind("share")), Method::GET, &path, None).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    assert_eq!(call(&app, None, Method::GET, &path, None).await.0, StatusCode::UNAUTHORIZED);

    // Rename: owners and admins; editors and viewers 403.
    for caller in [world.editor, world.viewer] {
        let (status, _) = call(&app, Some(caller), Method::PATCH, &path, Some(serde_json::json!({"name": "nope"}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?}");
    }
    let (status, body) = call(&app, Some(world.admin), Method::PATCH, &path, Some(serde_json::json!({"name": "d2 renamed"}))).await;
    assert_eq!((status, body["name"].as_str()), (StatusCode::OK, Some("d2 renamed")), "{body}");
    assert_eq!(audit_count(&pool, world.tenant, "tenant.rename").await?, 1);
    let (status, _) = call(&app, Some(world.owner), Method::PATCH, &path, Some(serde_json::json!({"name": "x", "colour": "red"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Suspend, then everything but reactivation answers 409; an editor cannot reactivate.
    let (status, body) = call(&app, Some(world.owner), Method::PATCH, &path, Some(serde_json::json!({"state": "suspended", "reason": "d2"}))).await;
    assert_eq!((status, body["state"].as_str()), (StatusCode::OK, Some("suspended")), "{body}");
    let (status, body) = call(&app, Some(world.viewer), Method::GET, &path, None).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "resource_inactive"));
    let reactivate = serde_json::json!({"state": "active"});
    assert_eq!(call(&app, Some(world.editor), Method::PATCH, &path, Some(reactivate.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Some(stranger), Method::PATCH, &path, Some(reactivate.clone())).await.0, StatusCode::NOT_FOUND);
    let (status, body) = call(&app, Some(world.admin), Method::PATCH, &path, Some(reactivate)).await;
    assert_eq!((status, body["state"].as_str()), (StatusCode::OK, Some("active")), "{body}");

    // Creating tenants: refused in single-tenant mode.
    let create = serde_json::json!({"slug": format!("d2-{}", suffix()), "name": "d2 new"});
    assert_eq!(call(&app, Some(world.owner), Method::POST, "/api/v1/tenants", Some(create)).await.0, StatusCode::FORBIDDEN);
    Ok(())
}

/// A caller who owns a different tenant.
async fn world_outsider(pool: &PgPool) -> Fallible<Caller> {
    Ok(world(pool).await?.owner)
}

#[tokio::test]
async fn tenants_are_created_in_multi_tenant_mode_and_listed_with_cursors() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::MultiTenant);
    let world = world(&pool).await?;
    let mut created = Vec::new();
    for _ in 0..2 {
        let create = serde_json::json!({"slug": format!("d2-{}", suffix()), "name": "d2 self-service"});
        let (status, body) = call(&app, Some(world.viewer), Method::POST, "/api/v1/tenants", Some(create)).await;
        assert_eq!((status, body["role"].as_str()), (StatusCode::CREATED, Some("tenant_owner")), "{body}");
        created.push(body["id"].as_str().unwrap().to_owned());
    }
    // Service accounts and share links cannot own tenants; bad slugs are 400; duplicates 409.
    let create = serde_json::json!({"slug": format!("d2-{}", suffix()), "name": "d2"});
    assert_eq!(
        call(&app, Some(world.viewer.as_kind("service")), Method::POST, "/api/v1/tenants", Some(create.clone())).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(call(&app, Some(world.viewer.as_kind("share")), Method::POST, "/api/v1/tenants", Some(create)).await.0, StatusCode::FORBIDDEN);
    let bad = serde_json::json!({"slug": "Bad Slug", "name": "d2"});
    assert_eq!(call(&app, Some(world.viewer), Method::POST, "/api/v1/tenants", Some(bad)).await.0, StatusCode::BAD_REQUEST);

    // The viewer now belongs to three tenants; page through them one at a time.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(cursor) => format!("/api/v1/tenants?limit=1&cursor={cursor}"),
            None => "/api/v1/tenants?limit=1".to_owned(),
        };
        let (status, body) = call(&app, Some(world.viewer), Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        seen.push(items[0]["id"].as_str().unwrap().to_owned());
        match body["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        assert!(seen.len() <= 3);
    }
    let mut expected = created.clone();
    expected.push(world.tenant.to_string());
    expected.sort();
    assert_eq!(seen, expected);
    // Another principal cannot replay the viewer's cursor (the principal is in the filter).
    let (_, first) = call(&app, Some(world.viewer), Method::GET, "/api/v1/tenants?limit=1", None).await;
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, body) = call(&app, Some(world.editor), Method::GET, &format!("/api/v1/tenants?limit=1&cursor={cursor}"), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_cursor"));
    Ok(())
}

#[tokio::test]
async fn members_are_managed_with_owner_protection_and_bounded_search() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::SingleTenant);
    let world = world(&pool).await?;
    let base = format!("/api/v1/tenants/{}/members", world.tenant);

    // Listing: every member; search by display-name prefix.
    let (status, body) = call(&app, Some(world.viewer), Method::GET, &base, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 4);
    let (_, body) = call(&app, Some(world.viewer), Method::GET, &format!("{base}?q=d2%20Ad"), None).await;
    let names: Vec<&str> = body["items"].as_array().unwrap().iter().filter_map(|item| item["display_name"].as_str()).collect();
    assert_eq!(names, ["d2 Admin"]);
    // A literal `%` does not act as a wildcard.
    let (_, body) = call(&app, Some(world.viewer), Method::GET, &format!("{base}?q=%25"), None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0, "{body}");
    let (status, body) = call(&app, Some(world.viewer), Method::GET, &format!("{base}?q={}", "a".repeat(65)), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_request"));
    // Paging by two, in display-name order.
    let (_, page1) = call(&app, Some(world.viewer), Method::GET, &format!("{base}?limit=2"), None).await;
    let cursor = page1["next_cursor"].as_str().unwrap().to_owned();
    let (_, page2) = call(&app, Some(world.viewer), Method::GET, &format!("{base}?limit=2&cursor={cursor}"), None).await;
    let names: Vec<&str> =
        [&page1, &page2].iter().flat_map(|page| page["items"].as_array().unwrap().iter().filter_map(|item| item["display_name"].as_str())).collect();
    assert_eq!(names, ["d2 Admin", "d2 Editor", "d2 Owner", "d2 Viewer"]);
    assert!(page2["next_cursor"].is_null());
    // The members cursor does not open another filter or another endpoint.
    let (status, body) = call(&app, Some(world.viewer), Method::GET, &format!("{base}?limit=2&q=d2&cursor={cursor}"), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_cursor"));
    let teams = format!("/api/v1/tenants/{}/teams?cursor={cursor}", world.tenant);
    assert_eq!(code(&call(&app, Some(world.viewer), Method::GET, &teams, None).await.1), "invalid_cursor");

    // Adding: admins add non-owners; only owners add owners; editors nothing.
    let newcomer = principal(&pool, "d2 Newcomer").await?;
    let add = |role: &str| serde_json::json!({"principal_id": newcomer.to_string(), "role": role});
    assert_eq!(call(&app, Some(world.editor), Method::POST, &base, Some(add("viewer"))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Some(world.admin), Method::POST, &base, Some(add("tenant_owner"))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Some(world.admin), Method::POST, &base, Some(add("workspace_admin"))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&app, Some(world.admin), Method::POST, &base, Some(add("editor"))).await.0, StatusCode::CREATED);
    assert_eq!(call(&app, Some(world.admin), Method::POST, &base, Some(add("editor"))).await.0, StatusCode::CONFLICT);
    assert_eq!(audit_count(&pool, world.tenant, "member.add").await?, 1);

    // Role changes: the last owner cannot be demoted; admins cannot touch owners.
    let owner_path = format!("{base}/{}", world.owner.principal);
    assert_eq!(call(&app, Some(world.admin), Method::PATCH, &owner_path, Some(serde_json::json!({"role": "viewer"}))).await.0, StatusCode::FORBIDDEN);
    let (status, body) = call(&app, Some(world.owner), Method::PATCH, &owner_path, Some(serde_json::json!({"role": "viewer"}))).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "conflict"));
    assert_eq!(call(&app, Some(world.owner), Method::DELETE, &owner_path, None).await.0, StatusCode::CONFLICT);
    let newcomer_path = format!("{base}/{newcomer}");
    assert_eq!(
        call(&app, Some(world.owner), Method::PATCH, &newcomer_path, Some(serde_json::json!({"role": "tenant_owner"}))).await.0,
        StatusCode::NO_CONTENT
    );
    // With a second owner the first can step down; an admin still cannot remove an owner.
    assert_eq!(call(&app, Some(world.admin), Method::DELETE, &newcomer_path, None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        call(&app, Some(world.owner), Method::PATCH, &owner_path, Some(serde_json::json!({"role": "tenant_admin"}))).await.0,
        StatusCode::NO_CONTENT
    );

    // Suspension and removal of a non-owner by an admin; unknown members are 404.
    let editor_path = format!("{base}/{}", world.editor.principal);
    assert_eq!(
        call(&app, Some(world.admin), Method::PATCH, &editor_path, Some(serde_json::json!({"state": "suspended"}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(call(&app, Some(world.editor), Method::GET, &base, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(world.admin), Method::DELETE, &editor_path, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.admin), Method::DELETE, &editor_path, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(world.admin), Method::DELETE, &format!("{base}/{}", d2()), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(audit_count(&pool, world.tenant, "member.remove").await?, 1);
    Ok(())
}

#[tokio::test]
async fn sensitive_permissions_need_an_owner_and_the_grant_policy() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::SingleTenant);
    let world = world(&pool).await?;
    let member = |caller: Caller| format!("/api/v1/tenants/{}/members/{}/permissions", world.tenant, caller.principal);

    // Admins can neither grant nor list.
    assert_eq!(call(&app, Some(world.admin), Method::PUT, &format!("{}/provider_keys", member(world.admin)), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Some(world.admin), Method::GET, &member(world.admin), None).await.0, StatusCode::FORBIDDEN);
    // Policy: provider keys to an admin yes; billing to an admin no; anything to an editor no.
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &format!("{}/provider_keys", member(world.admin)), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &format!("{}/billing", member(world.admin)), None).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &format!("{}/support", member(world.editor)), None).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &format!("{}/root", member(world.admin)), None).await.0, StatusCode::NOT_FOUND);
    // An owner may grant themselves billing (granting needs no sensitive grant).
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &format!("{}/billing", member(world.owner)), None).await.0, StatusCode::NO_CONTENT);
    let (status, body) = call(&app, Some(world.owner), Method::GET, &member(world.admin), None).await;
    assert_eq!((status, body["permissions"].clone()), (StatusCode::OK, serde_json::json!(["provider_keys"])));
    assert_eq!(
        call(&app, Some(world.owner), Method::DELETE, &format!("{}/provider_keys", member(world.admin)), None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(call(&app, Some(world.owner), Method::DELETE, &format!("{}/provider_keys", member(world.admin)), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(audit_count(&pool, world.tenant, "member.permission_grant").await?, 2);
    assert_eq!(audit_count(&pool, world.tenant, "member.permission_revoke").await?, 1);
    Ok(())
}

#[tokio::test]
async fn teams_grants_and_search_follow_the_matrix() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::SingleTenant);
    let world = world(&pool).await?;
    let teams = format!("/api/v1/tenants/{}/teams", world.tenant);
    let create = |name: &str| serde_json::json!({"slug": format!("d2-{}", suffix()), "name": name});

    // Creating: owners and admins only.
    assert_eq!(call(&app, Some(world.editor), Method::POST, &teams, Some(create("x"))).await.0, StatusCode::FORBIDDEN);
    let mut ids = Vec::new();
    for name in ["100% Design", "100x Design", "a_b", "axb"] {
        let (status, body) = call(&app, Some(world.admin), Method::POST, &teams, Some(create(name))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.push(body["id"].as_str().unwrap().to_owned());
    }
    // Search escapes `%` and `_`.
    let names = |body: &serde_json::Value| -> Vec<String> {
        body["items"].as_array().unwrap().iter().filter_map(|item| item["name"].as_str().map(str::to_owned)).collect()
    };
    assert_eq!(names(&call(&app, Some(world.viewer), Method::GET, &format!("{teams}?q=100%25"), None).await.1), ["100% Design"]);
    assert_eq!(names(&call(&app, Some(world.viewer), Method::GET, &format!("{teams}?q=a_"), None).await.1), ["a_b"]);
    assert_eq!(names(&call(&app, Some(world.viewer), Method::GET, &format!("{teams}?q=100"), None).await.1), ["100% Design", "100x Design"]);
    // Keyset paging through a search, then a tampered cursor.
    let (_, page1) = call(&app, Some(world.viewer), Method::GET, &format!("{teams}?q=100&limit=1"), None).await;
    let cursor = page1["next_cursor"].as_str().unwrap().to_owned();
    let (_, page2) = call(&app, Some(world.viewer), Method::GET, &format!("{teams}?q=100&limit=1&cursor={cursor}"), None).await;
    assert_eq!((names(&page1), names(&page2)), (vec!["100% Design".to_owned()], vec!["100x Design".to_owned()]));
    let mut tampered = cursor.clone().into_bytes();
    tampered[10] = if tampered[10] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered)?;
    let (status, body) = call(&app, Some(world.viewer), Method::GET, &format!("{teams}?q=100&limit=1&cursor={tampered}"), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_cursor"));
    // Page size clamp: 0 → 1, huge → at most 100.
    assert_eq!(call(&app, Some(world.viewer), Method::GET, &format!("{teams}?limit=0"), None).await.1["items"].as_array().unwrap().len(), 1);
    assert_eq!(call(&app, Some(world.viewer), Method::GET, &format!("{teams}?limit=100000"), None).await.1["items"].as_array().unwrap().len(), 4);

    // A team the viewer is not on is invisible; add them and they can view but not manage.
    let team = format!("{teams}/{}", ids[0]);
    assert_eq!(call(&app, Some(world.viewer), Method::GET, &team, None).await.0, StatusCode::NOT_FOUND);
    let members = format!("{team}/members");
    let add = |caller: Caller, role: &str| serde_json::json!({"principal_id": caller.principal.to_string(), "role": role});
    assert_eq!(call(&app, Some(world.admin), Method::POST, &members, Some(add(world.viewer, "viewer"))).await.0, StatusCode::CREATED);
    assert_eq!(call(&app, Some(world.admin), Method::POST, &members, Some(add(world.editor, "team_manager"))).await.0, StatusCode::CREATED);
    assert_eq!(call(&app, Some(world.viewer), Method::GET, &team, None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Some(world.viewer), Method::PATCH, &team, Some(serde_json::json!({"name": "no"}))).await.0, StatusCode::FORBIDDEN);
    let (status, body) = call(&app, Some(world.editor), Method::PATCH, &team, Some(serde_json::json!({"name": "100% Design Ops"}))).await;
    assert_eq!((status, body["name"].as_str()), (StatusCode::OK, Some("100% Design Ops")));
    let (_, body) = call(&app, Some(world.viewer), Method::GET, &format!("{members}?limit=1"), None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(body["next_cursor"].is_string());
    // The team manager changes roles but cannot remove the last manager (the admin who created
    // the team is the other manager).
    let viewer_member = format!("{members}/{}", world.viewer.principal);
    assert_eq!(
        call(&app, Some(world.editor), Method::PATCH, &viewer_member, Some(serde_json::json!({"role": "editor"}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(call(&app, Some(world.editor), Method::DELETE, &viewer_member, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.editor), Method::DELETE, &viewer_member, None).await.0, StatusCode::NOT_FOUND);

    // Workspace grants need the workspace too: the team manager has no workspace role (404);
    // the owner (a workspace admin everywhere) may grant and revoke.
    let ws = workspace(&pool, world.tenant).await?;
    let grant = format!("{team}/grants/workspaces/{ws}");
    let role = Some(serde_json::json!({"role": "editor"}));
    assert_eq!(call(&app, Some(world.editor), Method::PUT, &grant, role.clone()).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &grant, Some(serde_json::json!({"role": "owner"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &grant, role).await.0, StatusCode::NO_CONTENT);
    // Through the grant the team manager (team role cap: grant's role) now sees the workspace,
    // but still cannot manage its members.
    assert_eq!(call(&app, Some(world.editor), Method::DELETE, &grant, None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Some(world.owner), Method::DELETE, &grant, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.owner), Method::DELETE, &grant, None).await.0, StatusCode::NOT_FOUND);
    let owned = format!("{team}/owned/workspaces/{ws}");
    assert_eq!(call(&app, Some(world.owner), Method::PUT, &owned, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.owner), Method::DELETE, &owned, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.owner), Method::DELETE, &owned, None).await.0, StatusCode::NOT_FOUND);

    // Archive hides the team.
    assert_eq!(call(&app, Some(world.admin), Method::DELETE, &team, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.editor), Method::GET, &team, None).await.0, StatusCode::NOT_FOUND);

    // Cross tenant: another tenant's owner sees 404 for this team through either tenant path.
    let other = world_outsider(&pool).await?;
    let other_team = format!("/api/v1/tenants/{}/teams/{}", other.tenant, ids[1]);
    assert_eq!(call(&app, Some(other), Method::GET, &other_team, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(other), Method::GET, &format!("{teams}/{}", ids[1]), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&app, Some(other), Method::POST, &format!("{teams}/{}/members", ids[1]), Some(add(other, "viewer"))).await.0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// A g1-prefixed principal (fixture prefix g1: names and slugs start with "g1").
async fn g1_principal(pool: &PgPool, name: &str) -> Fallible<PrincipalId> {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0x91;
    let id = Uuid::from_bytes(bytes);
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', $2)")
        .bind(id.to_string())
        .bind(format!("g1 {name}"))
        .execute(pool)
        .await?;
    Ok(PrincipalId::try_from(id).map_err(|_| "nil")?)
}

#[tokio::test]
async fn team_member_search_workspace_grant_listing_and_resource_audit() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::SingleTenant);
    let tenant = TenantId::try_from(Uuid::new_v4()).map_err(|_| "nil")?;
    let owner = g1_principal(&pool, "Owner").await?;
    let viewer = g1_principal(&pool, "Viewer").await?;
    let mut people = Vec::new();
    for name in ["Ann%", "Ann_b", "Annx", "Bob"] {
        people.push((name, g1_principal(&pool, name).await?));
    }
    let slug = format!("g1-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    create_tenant_with_owner(&mut transaction, NewTenant { slug: &slug, name: "g1 tenant", plan_id: "team", home_region: "local" }, owner).await?;
    add_tenant_member(&mut transaction, viewer, Role::Viewer).await.map_err(|error| format!("{error:?}"))?;
    for (_, person) in &people {
        add_tenant_member(&mut transaction, *person, Role::Viewer).await.map_err(|error| format!("{error:?}"))?;
    }
    transaction.commit().await?;
    let owner = Caller { principal: owner, tenant, kind: "session" };
    let viewer = Caller { principal: viewer, tenant, kind: "session" };

    let teams = format!("/api/v1/tenants/{tenant}/teams");
    let (status, body) = call(&app, Some(owner), Method::POST, &teams, Some(serde_json::json!({"slug": slug, "name": "g1 team"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let team_id = body["id"].as_str().unwrap().to_owned();
    let members = format!("{teams}/{team_id}/members");
    for (_, person) in &people {
        let add = serde_json::json!({"principal_id": person.to_string(), "role": "viewer"});
        assert_eq!(call(&app, Some(owner), Method::POST, &members, Some(add)).await.0, StatusCode::CREATED);
    }
    let names = |body: &serde_json::Value| -> Vec<String> {
        body["items"].as_array().unwrap().iter().filter_map(|item| item["display_name"].as_str().map(str::to_owned)).collect()
    };
    let search = |q: &str| format!("{members}?q={q}");

    // Prefix search on the display name; `%` and `_` are literal; byte order (COLLATE "C").
    assert_eq!(names(&call(&app, Some(owner), Method::GET, &search("g1%20Ann%25"), None).await.1), ["g1 Ann%"]);
    assert_eq!(names(&call(&app, Some(owner), Method::GET, &search("g1%20Ann_"), None).await.1), ["g1 Ann_b"]);
    assert_eq!(names(&call(&app, Some(owner), Method::GET, &search("g1%20Ann"), None).await.1), ["g1 Ann%", "g1 Ann_b", "g1 Annx"]);
    assert_eq!(names(&call(&app, Some(owner), Method::GET, &search("zz"), None).await.1), Vec::<String>::new());
    // The creator is a team manager too: everyone, ordered by name.
    let (_, all) = call(&app, Some(owner), Method::GET, &members, None).await;
    assert_eq!(names(&all), ["g1 Ann%", "g1 Ann_b", "g1 Annx", "g1 Bob", "g1 Owner"]);
    assert_eq!(all["items"][4]["role"], "team_manager");
    assert_eq!(all["items"][0]["principal_id"], people[0].1.to_string());

    // Keyset paging through a search.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let path = match &cursor {
            Some(cursor) => format!("{}&limit=1&cursor={cursor}", search("g1%20Ann")),
            None => format!("{}&limit=1", search("g1%20Ann")),
        };
        let (status, page) = call(&app, Some(owner), Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        seen.extend(names(&page));
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, ["g1 Ann%", "g1 Ann_b", "g1 Annx"]);
    // A cursor is bound to the search term, the caller and the team.
    let (_, first) = call(&app, Some(owner), Method::GET, &format!("{}&limit=1", search("g1%20Ann")), None).await;
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let (status, body) = call(&app, Some(owner), Method::GET, &format!("{}&limit=1&cursor={cursor}", search("g1%20A")), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_cursor"));
    let ann = Caller { principal: people[0].1, tenant, kind: "session" };
    let (status, body) = call(&app, Some(ann), Method::GET, &format!("{}&limit=1&cursor={cursor}", search("g1%20Ann")), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_cursor"));
    // Bounded: a 65-character term and a control character are 400.
    let (status, body) = call(&app, Some(owner), Method::GET, &search(&"a".repeat(65)), None).await;
    assert_eq!((status, code(&body)), (StatusCode::BAD_REQUEST, "invalid_request"));
    assert_eq!(call(&app, Some(owner), Method::GET, &search("a%07"), None).await.0, StatusCode::BAD_REQUEST);
    // Team members may search; a tenant viewer outside the team cannot see it (404).
    assert_eq!(names(&call(&app, Some(ann), Method::GET, &search("g1%20B"), None).await.1), ["g1 Bob"]);
    assert_eq!(call(&app, Some(viewer), Method::GET, &search("g1"), None).await.0, StatusCode::NOT_FOUND);

    // Workspace team grants are listed per workspace (viewers of the workspace may list them).
    let (status, body) = call(&app, Some(owner), Method::POST, "/api/v1/workspaces", Some(serde_json::json!({"name": "g1 ws"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ws = body["id"].as_str().unwrap().to_owned();
    let grants = format!("/api/v1/workspaces/{ws}/team-grants");
    assert_eq!(call(&app, Some(owner), Method::GET, &grants, None).await.1["items"], serde_json::json!([]));
    let grant = format!("{teams}/{team_id}/grants/workspaces/{ws}");
    assert_eq!(call(&app, Some(owner), Method::PUT, &grant, Some(serde_json::json!({"role": "commenter"}))).await.0, StatusCode::NO_CONTENT);
    let (status, body) = call(&app, Some(owner), Method::GET, &grants, None).await;
    assert_eq!((status, body["items"].clone()), (StatusCode::OK, serde_json::json!([{"team_id": team_id, "role": "commenter"}])));
    // Ann sees the workspace through the team grant and may list its grants; the tenant viewer
    // outside the team holds no role on it (404); share links 403.
    assert_eq!(call(&app, Some(ann), Method::GET, &grants, None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Some(viewer), Method::GET, &grants, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(owner.as_kind("share")), Method::GET, &grants, None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        call(&app, Some(owner), Method::GET, &format!("/api/v1/workspaces/{}/team-grants", Uuid::new_v4()), None).await.0,
        StatusCode::NOT_FOUND
    );

    // Resource mutations are audited in the same transaction.
    assert_eq!(audit_count(&pool, tenant, "workspace.create").await?, 1);
    let workspace = format!("/api/v1/workspaces/{ws}");
    assert_eq!(call(&app, Some(owner), Method::PATCH, &workspace, Some(serde_json::json!({"name": "g1 ws 2"}))).await.0, StatusCode::OK);
    assert_eq!(audit_count(&pool, tenant, "workspace.rename").await?, 1);
    let (status, project) = call(&app, Some(owner), Method::POST, &format!("{workspace}/projects"), Some(serde_json::json!({"name": "g1 p"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let project = project["id"].as_str().unwrap().to_owned();
    let (status, document) =
        call(&app, Some(owner), Method::POST, &format!("/api/v1/projects/{project}/documents"), Some(serde_json::json!({"name": "g1 d"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let document = format!("/api/v1/documents/{}", document["id"].as_str().unwrap());
    let doc_grant = format!("{document}/team-grants/{team_id}");
    assert_eq!(call(&app, Some(owner), Method::PUT, &doc_grant, Some(serde_json::json!({"role": "editor"}))).await.0, StatusCode::OK);
    assert_eq!(call(&app, Some(owner), Method::DELETE, &doc_grant, None).await.0, StatusCode::NO_CONTENT);
    let member = format!("{workspace}/members/{}", viewer.principal);
    assert_eq!(call(&app, Some(owner), Method::PUT, &member, Some(serde_json::json!({"role": "viewer"}))).await.0, StatusCode::OK);
    assert_eq!(call(&app, Some(viewer), Method::GET, &workspace, None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Some(owner), Method::DELETE, &member, None).await.0, StatusCode::NO_CONTENT);
    // Access is lost on the next request.
    assert_eq!(call(&app, Some(viewer), Method::GET, &workspace, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Some(owner), Method::POST, &format!("{document}/archive"), None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Some(owner), Method::POST, &format!("/api/v1/projects/{project}/archive"), None).await.0, StatusCode::OK);
    for action in [
        "project.create",
        "document.create",
        "document.team_grant.set",
        "document.team_grant.remove",
        "workspace.member.set",
        "workspace.member.remove",
        "document.archive",
        "project.archive",
    ] {
        assert_eq!(audit_count(&pool, tenant, action).await?, 1, "{action}");
    }
    // A refused mutation records nothing.
    assert_eq!(call(&app, Some(ann), Method::PATCH, &workspace, Some(serde_json::json!({"name": "no"}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(audit_count(&pool, tenant, "workspace.rename").await?, 1);
    let actor: Option<String> = {
        let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        sqlx::query_scalar("SELECT actor_principal_id::text FROM audit_events WHERE tenant_id = $1::uuid AND action = 'workspace.rename'")
            .bind(tenant.to_string())
            .fetch_one(transaction.connection())
            .await?
    };
    assert_eq!(actor, Some(owner.principal.to_string()));
    Ok(())
}

#[tokio::test]
async fn invitations_are_created_once_listed_revoked_and_accepted() -> Fallible<()> {
    let Some(pool) = pool().await? else { return Ok(()) };
    let app = app(&pool, DeploymentMode::SingleTenant);
    let world = world(&pool).await?;
    let teams = format!("/api/v1/tenants/{}/teams", world.tenant);
    let (_, team) =
        call(&app, Some(world.owner), Method::POST, &teams, Some(serde_json::json!({"slug": format!("d2-{}", suffix()), "name": "Invites"}))).await;
    let team = format!("{teams}/{}", team["id"].as_str().unwrap());
    let manager = serde_json::json!({"principal_id": world.editor.principal.to_string(), "role": "team_manager"});
    assert_eq!(call(&app, Some(world.owner), Method::POST, &format!("{team}/members"), Some(manager)).await.0, StatusCode::CREATED);
    let invite = serde_json::json!({"email": "d2-invitee@example.com", "role": "editor"});

    // A team manager without member.manage cannot invite into the tenant; viewers see 404.
    assert_eq!(call(&app, Some(world.editor), Method::POST, &format!("{team}/invitations"), Some(invite.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&app, Some(world.viewer), Method::POST, &format!("{team}/invitations"), Some(invite.clone())).await.0, StatusCode::NOT_FOUND);
    let (status, created) = call(&app, Some(world.admin), Method::POST, &format!("{team}/invitations"), Some(invite.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    let (_, second) = call(&app, Some(world.admin), Method::POST, &format!("{team}/invitations"), Some(invite)).await;

    // Listing never shows the token; the team manager may list the team's invitations.
    let (status, body) = call(&app, Some(world.editor), Method::GET, &format!("{team}/invitations"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(!body.to_string().contains(&token));
    assert_eq!(
        call(&app, Some(world.editor), Method::GET, &format!("/api/v1/tenants/{}/invitations", world.tenant), None).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, Some(world.admin), Method::GET, &format!("/api/v1/tenants/{}/invitations?limit=1", world.tenant), None).await.1["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Revoke the second; accept the first as a new person (a service account cannot accept).
    let second = format!("{team}/invitations/{}", second["id"].as_str().unwrap());
    assert_eq!(call(&app, Some(world.editor), Method::DELETE, &second, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Some(world.editor), Method::DELETE, &second, None).await.0, StatusCode::NOT_FOUND);
    let invitee = Caller { principal: principal(&pool, "d2 Invitee").await?, tenant: world.tenant, kind: "session" };
    let accept = serde_json::json!({"token": token});
    assert_eq!(
        call(&app, Some(invitee.as_kind("service")), Method::POST, "/api/v1/invitations/accept", Some(accept.clone())).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = call(&app, Some(invitee), Method::POST, "/api/v1/invitations/accept", Some(accept.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["tenant_id"].as_str(), body["team_role"].as_str(), body["joined_tenant"].as_bool()),
        (Some(world.tenant.to_string().as_str()), Some("editor"), Some(true))
    );
    // Used, junk and oversized tokens are all the same 404.
    for token in [token.as_str(), "junk", &"a".repeat(4096)] {
        let (status, body) = call(&app, Some(invitee), Method::POST, "/api/v1/invitations/accept", Some(serde_json::json!({"token": token}))).await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    }
    // The invitee is now a viewer of the tenant and an editor on the team.
    assert_eq!(call(&app, Some(invitee), Method::GET, &team, None).await.0, StatusCode::OK);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// S4-019 / S4-017: member removal and suspension through the API close the member's sessions
// (fixture prefix e1; real sessions through the production CredentialAuthenticator).

mod revocation {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use figcraft_cloud_domain::{DeploymentMode, PrincipalId, Role, TenantId, Uuid};
    use figcraft_cloud_storage::begin_tenant_transaction;
    use figcraft_cloud_storage::memberships::add_tenant_member;
    use figcraft_cloud_storage::sessions::{SessionLimits, create_session, resolve_session};
    use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    use super::{Fallible, pool};
    use crate::api::authenticator::{IdentityConfig, TENANT_HEADER};
    use crate::api::context::ContextSettings;
    use crate::api::{ApiConfig, AppState, api_router};
    use crate::config::CloudRole;

    const ORIGIN: &str = "https://figcraft.e1.test";

    fn e1() -> Uuid {
        let mut bytes = *Uuid::new_v4().as_bytes();
        bytes[0] = 0xe1;
        Uuid::from_bytes(bytes)
    }

    fn app(pool: &PgPool) -> Router {
        let objects = figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
            endpoint: "http://127.0.0.1:9".to_string(),
            region: "us-east-1".to_string(),
            bucket: "figcraft-test".to_string(),
            access_key: "FCtestkey".to_string(),
            secret_key: "FCtestsecret".to_string(),
            path_style: true,
        })
        .unwrap();
        let identity =
            Arc::new(IdentityConfig::new(DeploymentMode::MultiTenant, None, Some(ORIGIN), Some(b"e1-tenant-revocation-signing-key")).unwrap());
        let config = ApiConfig { deployment: DeploymentMode::MultiTenant, ..ApiConfig::default() };
        let state = AppState::new(pool.clone(), objects, config).with_identity(identity);
        crate::health::app(CloudRole::Api, true, Some(api_router(state)), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(10), 1 << 20))
    }

    async fn send(app: &Router, token: &str, tenant: TenantId, method: Method, path: &str, body: Option<serde_json::Value>) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", format!("__Host-figcraft_session={token}"))
            .header(TENANT_HEADER, tenant.to_string())
            .header("origin", ORIGIN);
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let _ = to_bytes(response.into_body(), 1 << 20).await;
        status
    }

    async fn principal(pool: &PgPool) -> Fallible<PrincipalId> {
        let id = e1();
        sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e1 member')")
            .bind(id.to_string())
            .execute(pool)
            .await?;
        Ok(PrincipalId::try_from(id).map_err(|_| "nil")?)
    }

    async fn tenant(pool: &PgPool, owner: PrincipalId, members: &[PrincipalId]) -> Fallible<TenantId> {
        let tenant = TenantId::try_from(e1()).map_err(|_| "nil")?;
        let slug = format!("e1-{}", &e1().simple().to_string()[2..14]);
        let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        create_tenant_with_owner(&mut transaction, NewTenant { slug: &slug, name: "e1 tenant", plan_id: "team", home_region: "local" }, owner)
            .await?;
        for member in members {
            add_tenant_member(&mut transaction, *member, Role::Editor).await.map_err(|error| format!("{error:?}"))?;
        }
        transaction.commit().await?;
        Ok(tenant)
    }

    async fn live(pool: &PgPool, token: &str) -> bool {
        resolve_session(pool, token, SessionLimits::default()).await.is_ok()
    }

    #[tokio::test]
    async fn removed_and_suspended_members_are_refused_and_signed_out() -> Fallible<()> {
        let Some(pool) = pool().await? else { return Ok(()) };
        let app = app(&pool);
        let owner = principal(&pool).await?;
        let removed = principal(&pool).await?;
        let suspended = principal(&pool).await?;
        let elsewhere = principal(&pool).await?;
        let home = tenant(&pool, owner, &[removed, suspended, elsewhere]).await?;
        // `elsewhere` is also a member of a second active tenant.
        let other_owner = principal(&pool).await?;
        let second = tenant(&pool, other_owner, &[elsewhere]).await?;
        let session = |principal| {
            let pool = pool.clone();
            async move { create_session(&pool, principal, SessionLimits::default()).await.unwrap().token }
        };
        let owner_token = session(owner).await;
        let removed_tokens = [session(removed).await, session(removed).await];
        let suspended_token = session(suspended).await;
        let elsewhere_token = session(elsewhere).await;
        let tenant_path = format!("/api/v1/tenants/{home}");
        for token in removed_tokens.iter().chain([&suspended_token, &elsewhere_token]) {
            assert_eq!(send(&app, token, home, Method::GET, &tenant_path, None).await, StatusCode::OK);
        }

        // Removed: the next request is refused and every session of theirs is closed.
        let status = send(&app, &owner_token, home, Method::DELETE, &format!("{tenant_path}/members/{removed}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        for token in &removed_tokens {
            assert_eq!(send(&app, token, home, Method::GET, &tenant_path, None).await, StatusCode::UNAUTHORIZED, "session closed");
            assert!(!live(&pool, token).await, "the removed member's sessions are revoked");
        }

        // Suspended: the same.
        let body = serde_json::json!({"state": "suspended"});
        let status = send(&app, &owner_token, home, Method::PATCH, &format!("{tenant_path}/members/{suspended}"), Some(body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(send(&app, &suspended_token, home, Method::GET, &tenant_path, None).await, StatusCode::UNAUTHORIZED);
        assert!(!live(&pool, &suspended_token).await);

        // Removed here but still trusted by another active tenant: refused here, signed in there.
        let status = send(&app, &owner_token, home, Method::DELETE, &format!("{tenant_path}/members/{elsewhere}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(send(&app, &elsewhere_token, home, Method::GET, &tenant_path, None).await, StatusCode::FORBIDDEN);
        assert!(live(&pool, &elsewhere_token).await, "a member of another tenant keeps their sessions");
        assert_eq!(send(&app, &elsewhere_token, second, Method::GET, &format!("/api/v1/tenants/{second}"), None).await, StatusCode::OK);

        // The owner who made the changes is untouched.
        assert!(live(&pool, &owner_token).await);
        assert_eq!(send(&app, &owner_token, home, Method::GET, &tenant_path, None).await, StatusCode::OK);
        Ok(())
    }
}
