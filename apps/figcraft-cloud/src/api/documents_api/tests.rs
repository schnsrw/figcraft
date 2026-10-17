//! S4-019 / S4-021: the per-route permission matrix, the relay action matrix, cursors and the
//! collaboration link (pure), and the routes end to end against the dev PostgreSQL (fixture
//! prefix d3; skipped without `FIGCRAFT_TEST_DATABASE_URL_FILE`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use figcraft_cloud_auth::authz::{Action, Resource, Scope, decide};
use figcraft_cloud_domain::{CloudDocumentId, DeploymentMode, PrincipalId, ResourceState, Role, TenantContext, TenantId};
use figcraft_cloud_relay::admission::RelayAccess;
use figcraft_cloud_storage::effective_roles::{ResourceAccess, TenantAccess};
use tower::ServiceExt as _;

use super::admission::relay_access;
use super::{CURSORS, CursorCodec as _, Kind, ROUTES, check, collab_urls, subject};
use crate::api::auth::{AuthFuture, AuthRejection, AuthRequest, Authenticated, Authenticator, CredentialKind, SessionInfo};
use crate::api::context::ContextSettings;
use crate::api::{ApiConfig, AppState, api_router};
use crate::config::CloudRole;

// ---------------------------------------------------------------------------------------------
// The expected matrix, written independently of `authz::role_allows`.

/// Effective resource roles: workspace admin (also what tenant owners/admins act as), editor,
/// commenter, viewer.
const RESOURCE_ROLES: [(char, Role); 4] = [('W', Role::WorkspaceAdmin), ('E', Role::Editor), ('C', Role::Commenter), ('V', Role::Viewer)];
/// Tenant roles for the tenant-scope route.
const TENANT_ROLES: [(char, Role); 5] =
    [('O', Role::TenantOwner), ('A', Role::TenantAdmin), ('E', Role::Editor), ('C', Role::Commenter), ('V', Role::Viewer)];

/// Who may use each route on an active resource (letters as above).
const EXPECTED: &[(&str, &str, &str)] = &[
    ("GET", "/workspaces", "WECV"),
    ("POST", "/workspaces", "OA"),
    ("GET", "/workspaces/{workspace_id}", "WECV"),
    ("PATCH", "/workspaces/{workspace_id}", "W"),
    ("DELETE", "/workspaces/{workspace_id}", "W"),
    ("POST", "/workspaces/{workspace_id}/archive", "W"),
    ("POST", "/workspaces/{workspace_id}/restore", "W"),
    ("GET", "/workspaces/{workspace_id}/members", "WECV"),
    ("PUT", "/workspaces/{workspace_id}/members/{principal_id}", "W"),
    ("DELETE", "/workspaces/{workspace_id}/members/{principal_id}", "W"),
    ("GET", "/workspaces/{workspace_id}/projects", "WECV"),
    ("POST", "/workspaces/{workspace_id}/projects", "WE"),
    ("GET", "/projects/{project_id}", "WECV"),
    ("PATCH", "/projects/{project_id}", "WE"),
    ("DELETE", "/projects/{project_id}", "W"),
    ("POST", "/projects/{project_id}/archive", "W"),
    ("POST", "/projects/{project_id}/restore", "W"),
    ("GET", "/projects/{project_id}/documents", "WECV"),
    ("POST", "/projects/{project_id}/documents", "WE"),
    ("GET", "/documents/{document_id}", "WECV"),
    ("PATCH", "/documents/{document_id}", "WE"),
    ("DELETE", "/documents/{document_id}", "W"),
    ("POST", "/documents/{document_id}/archive", "W"),
    ("POST", "/documents/{document_id}/restore", "W"),
    ("GET", "/documents/{document_id}/members", "WECV"),
    ("PUT", "/documents/{document_id}/members/{principal_id}", "WE"),
    ("DELETE", "/documents/{document_id}/members/{principal_id}", "WE"),
    ("GET", "/documents/{document_id}/team-grants", "WECV"),
    ("PUT", "/documents/{document_id}/team-grants/{team_id}", "WE"),
    ("DELETE", "/documents/{document_id}/team-grants/{team_id}", "WE"),
    ("GET", "/documents/{document_id}/share-links", "WE"),
    ("POST", "/documents/{document_id}/share-links", "WE"),
    ("DELETE", "/documents/{document_id}/share-links/{link_id}", "WE"),
    ("GET", "/documents/{document_id}/collab-link", "WECV"),
];

fn tenant_access(tenant_role: Option<Role>) -> TenantAccess {
    TenantAccess { principal_active: true, tenant_state: ResourceState::Active, tenant_role, permissions: Vec::new() }
}

fn access(state: ResourceState, role: Option<Role>) -> ResourceAccess {
    ResourceAccess { tenant: tenant_access(Some(Role::Editor)), state, role, sources: Vec::new() }
}

fn kind(scope: Scope) -> Kind {
    match scope {
        Scope::Workspace => Kind::Workspace,
        Scope::Project => Kind::Project,
        Scope::Document => Kind::Document,
        Scope::Tenant | Scope::Team => panic!("no resource kind for {scope:?}"),
    }
}

#[test]
fn every_route_has_exactly_one_expected_row() {
    let routes: BTreeSet<(&str, &str)> = ROUTES.iter().map(|route| (route.method, route.path)).collect();
    assert_eq!(routes.len(), ROUTES.len(), "duplicate route");
    let expected: BTreeSet<(&str, &str)> = EXPECTED.iter().map(|(method, path, _)| (*method, *path)).collect();
    assert_eq!(expected.len(), EXPECTED.len(), "duplicate expected row");
    assert_eq!(routes, expected);
}

#[test]
fn every_route_and_role_matches_the_expected_matrix_with_the_disclosure_policy() {
    for route in ROUTES {
        let allowed = EXPECTED.iter().find(|(method, path, _)| *method == route.method && *path == route.path).map(|row| row.2).unwrap();
        if route.action.scope() == Scope::Tenant {
            for (letter, role) in TENANT_ROLES {
                let decision = decide(&subject(&tenant_access(Some(role))), &Resource::Tenant, route.action);
                assert_eq!(decision.allow, allowed.contains(letter), "{} {} as {letter}", route.method, route.path);
            }
            continue;
        }
        let kind = kind(route.action.scope());
        for (letter, role) in RESOURCE_ROLES {
            let result = check(Some(access(ResourceState::Active, Some(role))), kind, route.action);
            let expected = if allowed.contains(letter) {
                Ok(())
            } else {
                // Every role can see an active resource it holds a role on: 403, never 404.
                Err(StatusCode::FORBIDDEN)
            };
            assert_eq!(result.map(|_| ()).map_err(|error| error.status()), expected, "{} {} as {letter}", route.method, route.path);
        }
        // No role, missing, or another tenant's (resolves to nothing): 404 for every route.
        assert_eq!(
            check(Some(access(ResourceState::Active, None)), kind, route.action).map(|_| ()).map_err(|e| e.status()),
            Err(StatusCode::NOT_FOUND)
        );
        assert_eq!(check(None, kind, route.action).map(|_| ()).map_err(|e| e.status()), Err(StatusCode::NOT_FOUND));
        // Inactive principal or tenant: nothing is visible.
        let mut inactive = access(ResourceState::Active, Some(Role::WorkspaceAdmin));
        inactive.tenant.tenant_state = ResourceState::Suspended;
        assert_eq!(check(Some(inactive), kind, route.action).map(|_| ()).map_err(|e| e.status()), Err(StatusCode::NOT_FOUND));
    }
}

#[test]
fn lifecycle_states_limit_routes_without_leaking_soft_deleted_resources() {
    let status = |state, role, action| check(Some(access(state, Some(role))), Kind::Document, action).map(|_| ()).map_err(|e| e.status());
    // Archived: still readable, restorable by admins, not editable or joinable.
    assert_eq!(status(ResourceState::Archived, Role::Viewer, Action::DocumentView), Ok(()));
    assert_eq!(status(ResourceState::Archived, Role::WorkspaceAdmin, Action::DocumentRestore), Ok(()));
    assert_eq!(status(ResourceState::Archived, Role::Editor, Action::DocumentRename), Err(StatusCode::FORBIDDEN));
    assert_eq!(status(ResourceState::Archived, Role::Editor, Action::DocumentJoinRealtime), Err(StatusCode::FORBIDDEN));
    // Soft-deleted: invisible except to whoever may restore it.
    assert_eq!(status(ResourceState::Deleting, Role::WorkspaceAdmin, Action::DocumentView), Err(StatusCode::NOT_FOUND));
    assert_eq!(status(ResourceState::Deleting, Role::WorkspaceAdmin, Action::DocumentRestore), Ok(()));
    assert_eq!(status(ResourceState::Deleting, Role::Editor, Action::DocumentRestore), Err(StatusCode::NOT_FOUND));
}

#[test]
fn relay_action_matrix_admits_edit_view_or_refuses_by_document_role() {
    let cases = [
        (ResourceState::Active, Some(Role::WorkspaceAdmin), RelayAccess::Edit),
        (ResourceState::Active, Some(Role::Editor), RelayAccess::Edit),
        (ResourceState::Active, Some(Role::Commenter), RelayAccess::View),
        (ResourceState::Active, Some(Role::Viewer), RelayAccess::View),
        (ResourceState::Active, Some(Role::TeamManager), RelayAccess::Refused),
        (ResourceState::Active, None, RelayAccess::Refused),
        (ResourceState::Archived, Some(Role::WorkspaceAdmin), RelayAccess::Refused),
        (ResourceState::Deleting, Some(Role::Editor), RelayAccess::Refused),
        (ResourceState::Suspended, Some(Role::Editor), RelayAccess::Refused),
    ];
    for (state, role, expected) in cases {
        assert_eq!(relay_access(Some(&access(state, role))), expected, "{state:?} {role:?}");
    }
    assert_eq!(relay_access(None), RelayAccess::Refused);
    let mut not_member = access(ResourceState::Active, Some(Role::Editor));
    not_member.tenant.tenant_role = None;
    assert_eq!(relay_access(Some(&not_member)), RelayAccess::Refused);
    let mut inactive = access(ResourceState::Active, Some(Role::Editor));
    inactive.tenant.principal_active = false;
    assert_eq!(relay_access(Some(&inactive)), RelayAccess::Refused);
}

#[test]
fn cursors_round_trip_and_reject_other_lists_and_junk() {
    let id = "d3000000-0000-4000-8000-0000000000ff";
    let cursor = CURSORS.encode("doc", id);
    assert!(!cursor.contains(id), "cursors are opaque");
    assert_eq!(CURSORS.decode("doc", &cursor).as_deref(), Some(id));
    assert_eq!(CURSORS.decode("ws", &cursor), None);
    for junk in ["", "!!!", "ZG9j", &"a".repeat(200), "ZG9jLg"] {
        assert_eq!(CURSORS.decode("doc", junk), None, "{junk}");
    }
}

#[test]
fn collab_urls_name_the_relay_and_the_editor() {
    let document: CloudDocumentId = "d3000000-0000-4000-8000-0000000000dd".parse().unwrap();
    let (invite, editor) = collab_urls("https://figcraft.example/", document, "SECRET").unwrap();
    assert_eq!(invite, format!("wss://figcraft.example/ws/v1/collab?file={document}&token=SECRET"));
    assert!(editor.starts_with("https://figcraft.example/edit/#join=wss%3A%2F%2Ffigcraft.example%2Fws%2Fv1%2Fcollab"), "{editor}");
    let (invite, _) = collab_urls("http://127.0.0.1:8080", document, "S").unwrap();
    assert!(invite.starts_with("ws://127.0.0.1:8080/ws/v1/collab?file="));
    assert!(collab_urls("ftp://x", document, "S").is_err());
}

// ---------------------------------------------------------------------------------------------
// Router-level checks (no database needed).

/// `Authorization: Bearer <principal uuid>` in [`TENANT`] (or `Bearer other:<uuid>` in
/// [`OTHER_TENANT`], `Bearer link:<uuid>` as a share-link credential).
struct BearerPrincipal;

const TENANT: &str = "d3000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "d3000000-0000-4000-8000-000000000002";

impl Authenticator for BearerPrincipal {
    fn authenticate<'a>(&'a self, request: &'a AuthRequest<'a>) -> AuthFuture<'a> {
        let header = request.headers.get("authorization").and_then(|value| value.to_str().ok()).map(str::to_string);
        Box::pin(async move {
            let token = header.as_deref().and_then(|value| value.strip_prefix("Bearer ")).ok_or(AuthRejection::Unauthenticated)?;
            let (tenant, kind, principal) = if let Some(rest) = token.strip_prefix("other:") {
                (OTHER_TENANT, CredentialKind::BrowserSession, rest)
            } else if let Some(rest) = token.strip_prefix("link:") {
                (TENANT, CredentialKind::ShareLink, rest)
            } else {
                (TENANT, CredentialKind::BrowserSession, token)
            };
            let principal: PrincipalId = principal.parse().map_err(|_| AuthRejection::Unauthenticated)?;
            let tenant: TenantId = tenant.parse().map_err(|_| AuthRejection::Unavailable)?;
            Ok(Authenticated {
                principal,
                tenant: TenantContext::new(tenant, principal, DeploymentMode::MultiTenant, Role::Editor, []),
                session: SessionInfo { kind, id: "d3000000-0000-4000-8000-0000000000ee".to_string() },
                grant: crate::api::auth::Grant::Member,
            })
        })
    }
}

fn app(pool: sqlx::PgPool) -> Router {
    let objects = figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".to_string(),
        region: "us-east-1".to_string(),
        bucket: "figcraft-test".to_string(),
        access_key: "FCtestkey".to_string(),
        secret_key: "FCtestsecret".to_string(),
        path_style: true,
    })
    .unwrap();
    let state = AppState::new(pool, objects, ApiConfig::default()).with_authenticator(Arc::new(BearerPrincipal));
    crate::health::app(CloudRole::Api, true, Some(api_router(state)), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(10), 1 << 20))
}

fn lazy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").unwrap()
}

async fn send(app: &Router, method: &str, path: &str, bearer: Option<&str>, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(Method::from_bytes(method.as_bytes()).unwrap()).uri(format!("/api/v1{path}"));
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
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

fn concrete(path: &str, id: &str) -> String {
    ["{workspace_id}", "{project_id}", "{document_id}", "{principal_id}", "{team_id}", "{link_id}"]
        .iter()
        .fold(path.to_string(), |path, param| path.replace(param, id))
}

#[tokio::test]
async fn every_listed_route_is_registered_and_requires_authentication() {
    let app = app(lazy_pool());
    let id = "d3000000-0000-4000-8000-0000000000f1";
    for route in ROUTES {
        let (status, body) = send(&app, route.method, &concrete(route.path, id), None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{} {}", route.method, route.path);
        assert_eq!(body["error"]["code"], "unauthenticated");
    }
    // Every other method on these paths is not routed at all.
    let paths: BTreeSet<&str> = ROUTES.iter().map(|route| route.path).collect();
    for path in paths {
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            if ROUTES.iter().any(|route| route.path == path && route.method == method) {
                continue;
            }
            let (status, _) = send(&app, method, &concrete(path, id), None, None).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
        }
    }
}

#[tokio::test]
async fn share_link_credentials_cannot_use_the_management_api() {
    let app = app(lazy_pool());
    let (status, body) = send(&app, "GET", "/workspaces", Some("link:d3000000-0000-4000-8000-0000000000a1"), None).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")));
}

// ---------------------------------------------------------------------------------------------
// End to end against PostgreSQL.

const ADMIN: &str = "d3a00000-0000-4000-8000-000000000001";
const EDITOR: &str = "d3a00000-0000-4000-8000-000000000002";
const VIEWER: &str = "d3a00000-0000-4000-8000-000000000003";
const OUTSIDER: &str = "d3a00000-0000-4000-8000-000000000004";
const OTHER_ADMIN: &str = "d3a00000-0000-4000-8000-000000000005";
const WORKSPACE: &str = "d3b00000-0000-4000-8000-000000000001";
const PROJECT: &str = "d3c00000-0000-4000-8000-000000000001";

type TestResult = Result<(), Box<dyn std::error::Error>>;
/// One request: method, path, body, and each caller with the status they must get.
type Case<'a> = (&'a str, String, Option<serde_json::Value>, Vec<(&'a str, StatusCode)>);

async fn database() -> Result<Option<sqlx::PgPool>, Box<dyn std::error::Error>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; documents API probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?;
    let members: [(&str, &[(&str, &str)]); 2] = [
        (TENANT, &[(ADMIN, "tenant_admin"), (EDITOR, "editor"), (VIEWER, "editor"), (OUTSIDER, "editor")]),
        (OTHER_TENANT, &[(OTHER_ADMIN, "tenant_admin")]),
    ];
    for (tenant, people) in members {
        let mut transaction = figcraft_cloud_storage::begin_tenant_transaction(&pool, tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
        sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'd3 documents api', 'test', 'local') ON CONFLICT DO NOTHING")
            .bind(tenant)
            .bind(format!("d3-{tenant}"))
            .execute(transaction.connection())
            .await?;
        for (principal, role) in people {
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd3 probe') ON CONFLICT DO NOTHING")
                .bind(principal)
                .execute(transaction.connection())
                .await?;
            sqlx::query(
                "INSERT INTO tenant_members (tenant_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, $3, 'active') \
                 ON CONFLICT (tenant_id, principal_id) DO UPDATE SET role = EXCLUDED.role, state = 'active'",
            )
            .bind(tenant)
            .bind(principal)
            .bind(role)
            .execute(transaction.connection())
            .await?;
        }
        transaction.commit().await?;
    }
    let mut transaction = figcraft_cloud_storage::begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    for statement in [
        "INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $3::uuid, 'd3 workspace') ON CONFLICT (id) DO UPDATE SET state = 'active'",
        "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($2::uuid, $3::uuid, $1::uuid, 'd3 project') ON CONFLICT (id) DO UPDATE SET state = 'active'",
        "DELETE FROM workspace_members WHERE tenant_id = $3::uuid AND workspace_id = $1::uuid",
    ] {
        sqlx::query(statement).bind(WORKSPACE).bind(PROJECT).bind(TENANT).execute(transaction.connection()).await?;
    }
    sqlx::query("INSERT INTO workspace_members (tenant_id, workspace_id, principal_id, role) VALUES ($1::uuid, $2::uuid, $3::uuid, 'editor')")
        .bind(TENANT)
        .bind(WORKSPACE)
        .bind(EDITOR)
        .execute(transaction.connection())
        .await?;
    transaction.commit().await?;
    Ok(Some(pool))
}

/// A fresh document in the fixture project with `VIEWER` as a direct viewer.
async fn fresh_document(app: &Router) -> Result<String, Box<dyn std::error::Error>> {
    let (status, body) =
        send(app, "POST", &format!("/projects/{PROJECT}/documents"), Some(EDITOR), Some(serde_json::json!({"name": "d3 doc"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let document = body["id"].as_str().ok_or("no id")?.to_string();
    let (status, body) =
        send(app, "PUT", &format!("/documents/{document}/members/{VIEWER}"), Some(EDITOR), Some(serde_json::json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(document)
}

#[tokio::test]
async fn document_routes_follow_the_matrix_end_to_end() -> TestResult {
    let Some(pool) = database().await? else {
        return Ok(());
    };
    let app = app(pool.clone());
    let other = format!("other:{OTHER_ADMIN}");
    let missing_team = "d3e00000-0000-4000-8000-0000000000ff";
    let document = fresh_document(&app).await?;
    let doc = |suffix: &str| format!("/documents/{document}{suffix}");
    let name = Some(serde_json::json!({"name": "renamed"}));
    let viewer_role = Some(serde_json::json!({"role": "viewer"}));
    // (method, path, body, [(caller, expected status)]).
    let cases: Vec<Case<'_>> = vec![
        (
            "GET",
            doc(""),
            None,
            vec![
                (ADMIN, StatusCode::OK),
                (EDITOR, StatusCode::OK),
                (VIEWER, StatusCode::OK),
                (OUTSIDER, StatusCode::NOT_FOUND),
                (other.as_str(), StatusCode::NOT_FOUND),
            ],
        ),
        (
            "PATCH",
            doc(""),
            name.clone(),
            vec![
                (VIEWER, StatusCode::FORBIDDEN),
                (OUTSIDER, StatusCode::NOT_FOUND),
                (other.as_str(), StatusCode::NOT_FOUND),
                (EDITOR, StatusCode::OK),
            ],
        ),
        ("GET", doc("/members"), None, vec![(VIEWER, StatusCode::OK), (OUTSIDER, StatusCode::NOT_FOUND), (other.as_str(), StatusCode::NOT_FOUND)]),
        (
            "PUT",
            doc(&format!("/members/{OUTSIDER}")),
            viewer_role.clone(),
            vec![(VIEWER, StatusCode::FORBIDDEN), (other.as_str(), StatusCode::NOT_FOUND), (EDITOR, StatusCode::OK)],
        ),
        // OUTSIDER is now a direct viewer of this document.
        ("GET", doc(""), None, vec![(OUTSIDER, StatusCode::OK)]),
        (
            "DELETE",
            doc(&format!("/members/{OUTSIDER}")),
            None,
            vec![(OUTSIDER, StatusCode::FORBIDDEN), (EDITOR, StatusCode::NO_CONTENT), (EDITOR, StatusCode::NOT_FOUND)],
        ),
        ("GET", doc(""), None, vec![(OUTSIDER, StatusCode::NOT_FOUND)]),
        ("GET", doc("/team-grants"), None, vec![(VIEWER, StatusCode::OK), (OUTSIDER, StatusCode::NOT_FOUND)]),
        (
            "PUT",
            doc(&format!("/team-grants/{missing_team}")),
            viewer_role.clone(),
            vec![(VIEWER, StatusCode::FORBIDDEN), (EDITOR, StatusCode::NOT_FOUND)],
        ),
        ("GET", doc("/share-links"), None, vec![(VIEWER, StatusCode::FORBIDDEN), (OUTSIDER, StatusCode::NOT_FOUND), (EDITOR, StatusCode::OK)]),
        ("POST", doc("/share-links"), viewer_role.clone(), vec![(VIEWER, StatusCode::FORBIDDEN), (other.as_str(), StatusCode::NOT_FOUND)]),
        ("GET", doc("/collab-link"), None, vec![(OUTSIDER, StatusCode::NOT_FOUND), (other.as_str(), StatusCode::NOT_FOUND)]),
        (
            "POST",
            doc("/archive"),
            None,
            vec![(VIEWER, StatusCode::FORBIDDEN), (EDITOR, StatusCode::FORBIDDEN), (OUTSIDER, StatusCode::NOT_FOUND), (ADMIN, StatusCode::OK)],
        ),
        // Archived: readable, not joinable.
        ("GET", doc(""), None, vec![(VIEWER, StatusCode::OK)]),
        ("GET", doc("/collab-link"), None, vec![(EDITOR, StatusCode::FORBIDDEN)]),
        ("POST", doc("/restore"), None, vec![(EDITOR, StatusCode::FORBIDDEN), (ADMIN, StatusCode::OK)]),
        ("DELETE", doc(""), None, vec![(EDITOR, StatusCode::FORBIDDEN), (ADMIN, StatusCode::OK), (ADMIN, StatusCode::NOT_FOUND)]),
        // Soft-deleted: invisible except to whoever may restore it.
        ("GET", doc(""), None, vec![(ADMIN, StatusCode::NOT_FOUND), (EDITOR, StatusCode::NOT_FOUND)]),
        ("POST", doc("/restore"), None, vec![(EDITOR, StatusCode::NOT_FOUND), (ADMIN, StatusCode::OK)]),
    ];
    for (method, path, body, callers) in cases {
        for (caller, expected) in callers {
            let (status, response) = send(&app, method, &path, Some(caller), body.clone()).await;
            assert_eq!(status, expected, "{method} {path} as {caller}: {response}");
            if !status.is_success() {
                assert!(response["error"]["code"].is_string(), "{response}");
            }
        }
    }
    let (_, renamed) = send(&app, "GET", &doc(""), Some(VIEWER), None).await;
    assert_eq!((renamed["name"].as_str(), renamed["role"].as_str(), renamed["can_edit"].as_bool()), (Some("renamed"), Some("viewer"), Some(false)));
    Ok(())
}

/// The live link `secret` names in [`TENANT`] (document, role), looked up by hash inside a tenant
/// transaction so the check does not depend on the cross-tenant exchange path.
async fn live_link(pool: &sqlx::PgPool, secret: &str) -> Result<Option<(String, String)>, Box<dyn std::error::Error>> {
    let mut transaction = figcraft_cloud_storage::begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT document_id::text, role FROM share_links \
         WHERE tenant_id = $1::uuid AND token_hash = $2 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(TENANT)
    .bind(figcraft_cloud_storage::share_links::share_token_hash(secret).to_vec())
    .fetch_optional(transaction.connection())
    .await?;
    Ok(row)
}

#[tokio::test]
async fn share_links_and_collab_links_mint_live_secrets_for_the_callers_role() -> TestResult {
    let Some(pool) = database().await? else {
        return Ok(());
    };
    let app = app(pool.clone());
    let document = fresh_document(&app).await?;
    let id: CloudDocumentId = document.parse()?;
    // A viewer gets a read-only invite; an editor an editing one; both resolve to this document.
    for (caller, access, role) in [(VIEWER, "view", "viewer"), (EDITOR, "edit", "editor")] {
        let (status, body) = send(&app, "GET", &format!("/documents/{document}/collab-link"), Some(caller), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["access"], access);
        let invite = body["invite"].as_str().ok_or("no invite")?;
        let prefix = format!("ws://127.0.0.1:8080/ws/v1/collab?file={document}&token=");
        assert!(invite.starts_with(&prefix), "{invite}");
        let secret = &invite[prefix.len()..];
        assert_eq!(live_link(&pool, secret).await?, Some((id.to_string(), role.to_string())));
        assert!(body["editor_url"].as_str().is_some_and(|url| url.contains("/edit/#join=")));
    }
    // Create returns the secret once; listing never shows it; revoke kills it.
    let (status, created) =
        send(&app, "POST", &format!("/documents/{document}/share-links"), Some(EDITOR), Some(serde_json::json!({"role": "editor", "label": "d3"})))
            .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret = created["secret"].as_str().ok_or("no secret")?.to_string();
    let link_id = created["id"].as_str().ok_or("no id")?.to_string();
    let (status, listed) = send(&app, "GET", &format!("/documents/{document}/share-links"), Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listed.to_string().contains(&secret));
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(3));
    // A link of another document cannot be revoked through this one.
    let other_document = fresh_document(&app).await?;
    let (status, _) = send(&app, "DELETE", &format!("/documents/{other_document}/share-links/{link_id}"), Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "DELETE", &format!("/documents/{document}/share-links/{link_id}"), Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(live_link(&pool, &secret).await?, None);
    Ok(())
}

#[tokio::test]
async fn workspace_and_project_routes_follow_the_matrix_end_to_end() -> TestResult {
    let Some(pool) = database().await? else {
        return Ok(());
    };
    let app = app(pool);
    let other = format!("other:{OTHER_ADMIN}");
    // Creating a workspace is a tenant-admin action (403 for members: no resource to hide).
    let (status, _) = send(&app, "POST", "/workspaces", Some(EDITOR), Some(serde_json::json!({"name": "d3 nope"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, created) = send(&app, "POST", "/workspaces", Some(ADMIN), Some(serde_json::json!({"name": "  d3 created  "}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "d3 created");
    let created_id = created["id"].as_str().ok_or("no id")?.to_string();
    let (status, _) = send(&app, "POST", "/workspaces", Some(ADMIN), Some(serde_json::json!({"name": " "}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The editor sees only the workspace they belong to; others see nothing of it.
    let (status, listed) = send(&app, "GET", "/workspaces?limit=100", Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let ids: Vec<&str> = listed["items"].as_array().ok_or("no items")?.iter().filter_map(|item| item["id"].as_str()).collect();
    assert_eq!(ids, vec![WORKSPACE]);
    for caller in [OUTSIDER, VIEWER, other.as_str()] {
        let (_, listed) = send(&app, "GET", "/workspaces?limit=100", Some(caller), None).await;
        assert!(!listed.to_string().contains(WORKSPACE), "{caller}: {listed}");
        let (status, _) = send(&app, "GET", &format!("/workspaces/{WORKSPACE}"), Some(caller), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{caller}");
        let (status, _) = send(&app, "GET", &format!("/projects/{PROJECT}"), Some(caller), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{caller}");
    }
    let (status, _) = send(&app, "GET", "/workspaces?cursor=junk", Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, "GET", "/workspaces?limit=101", Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Editors read and create inside; only admins rename, archive, manage members.
    let (status, _) = send(&app, "PATCH", &format!("/workspaces/{WORKSPACE}"), Some(EDITOR), Some(serde_json::json!({"name": "x"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) =
        send(&app, "PUT", &format!("/workspaces/{WORKSPACE}/members/{OUTSIDER}"), Some(EDITOR), Some(serde_json::json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, members) = send(&app, "GET", &format!("/workspaces/{WORKSPACE}/members"), Some(EDITOR), None).await;
    assert_eq!((status, members["items"][0]["principal_id"].as_str()), (StatusCode::OK, Some(EDITOR)));
    let (status, projects) = send(&app, "GET", &format!("/workspaces/{WORKSPACE}/projects"), Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(projects.to_string().contains(PROJECT));
    let (status, project) =
        send(&app, "POST", &format!("/workspaces/{WORKSPACE}/projects"), Some(EDITOR), Some(serde_json::json!({"name": "d3 p"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    let project_id = project["id"].as_str().ok_or("no id")?.to_string();
    let (status, _) = send(&app, "PATCH", &format!("/projects/{project_id}"), Some(EDITOR), Some(serde_json::json!({"name": "d3 p2"}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, "POST", &format!("/projects/{project_id}/archive"), Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for (method, suffix, expected) in [("POST", "/archive", "archived"), ("POST", "/restore", "active"), ("DELETE", "", "deleting")] {
        let (status, body) = send(&app, method, &format!("/projects/{project_id}{suffix}"), Some(ADMIN), None).await;
        assert_eq!((status, body["state"].as_str()), (StatusCode::OK, Some(expected)), "{method} {suffix}");
    }
    // The admin manages members and the new workspace's lifecycle.
    let (status, _) =
        send(&app, "PUT", &format!("/workspaces/{created_id}/members/{OUTSIDER}"), Some(ADMIN), Some(serde_json::json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, "GET", &format!("/workspaces/{created_id}"), Some(OUTSIDER), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) =
        send(&app, "PUT", &format!("/workspaces/{created_id}/members/{OTHER_ADMIN}"), Some(ADMIN), Some(serde_json::json!({"role": "viewer"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "another tenant's principal is not a member here");
    let (status, _) = send(&app, "DELETE", &format!("/workspaces/{created_id}/members/{OUTSIDER}"), Some(ADMIN), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) =
        send(&app, "PATCH", &format!("/workspaces/{created_id}"), Some(ADMIN), Some(serde_json::json!({"name": "d3 renamed"}))).await;
    assert_eq!((status, body["name"].as_str()), (StatusCode::OK, Some("d3 renamed")));
    for (method, suffix, expected) in
        [("POST", "/archive", "archived"), ("POST", "/restore", "active"), ("DELETE", "", "deleting"), ("POST", "/restore", "active")]
    {
        let (status, body) = send(&app, method, &format!("/workspaces/{created_id}{suffix}"), Some(ADMIN), None).await;
        assert_eq!((status, body["state"].as_str()), (StatusCode::OK, Some(expected)), "{method} {suffix}");
    }
    // Another tenant's admin cannot touch it.
    let (status, _) = send(&app, "POST", &format!("/workspaces/{created_id}/archive"), Some(other.as_str()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn document_lists_page_with_opaque_cursors() -> TestResult {
    let Some(pool) = database().await? else {
        return Ok(());
    };
    let app = app(pool);
    let (status, project) =
        send(&app, "POST", &format!("/workspaces/{WORKSPACE}/projects"), Some(EDITOR), Some(serde_json::json!({"name": "d3 paging"}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let project = project["id"].as_str().ok_or("no id")?.to_string();
    let mut created = BTreeSet::new();
    for index in 0..5 {
        let (_, body) =
            send(&app, "POST", &format!("/projects/{project}/documents"), Some(EDITOR), Some(serde_json::json!({"name": format!("d3 {index}")})))
                .await;
        created.insert(body["id"].as_str().ok_or("no id")?.to_string());
    }
    let mut seen = BTreeSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let path = match &cursor {
            Some(cursor) => format!("/projects/{project}/documents?limit=2&cursor={cursor}"),
            None => format!("/projects/{project}/documents?limit=2"),
        };
        let (status, page) = send(&app, "GET", &path, Some(EDITOR), None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for item in page["items"].as_array().ok_or("no items")? {
            assert!(seen.insert(item["id"].as_str().ok_or("no id")?.to_string()));
        }
        cursor = page["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, created);
    // A workspace cursor is not a document cursor.
    let ws_cursor = CURSORS.encode("ws", WORKSPACE);
    let (status, _) = send(&app, "GET", &format!("/projects/{project}/documents?cursor={ws_cursor}"), Some(EDITOR), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}
