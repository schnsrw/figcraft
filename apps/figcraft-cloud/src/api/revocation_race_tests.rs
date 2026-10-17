//! S4-023: HTTP requests authenticated with a browser session racing the session's logout,
//! "sign out everywhere" (`revoke_all_for_principal`) and the caller's removal from the tenant,
//! through the production authenticator and the full middleware stack, against the dev
//! PostgreSQL (fixture prefix e3; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE). Every run
//! creates fresh e3-prefixed tenants and principals, so reruns pass single-threaded.
//!
//! The mutation is `PATCH /api/v1/documents/{id}` (rename), which a workspace editor may do.
//!
//! The contract: a request either commits before the revocation commits, or it is refused and
//! changes nothing. Never is a mutation committed after a revocation that committed first.
//!
//! * Requests that *start* after the revocation's commit returned are always refused: the
//!   authenticator re-resolves the session and the tenant membership on every request (tested
//!   in both a sequential and a concurrent hammer form; these pass).
//! * A request that was authenticated and authorized *before* the revocation committed, and has
//!   not committed yet, must either hold the revocation off until it commits or see it and be
//!   refused. This is forced deterministically: a gate transaction holds the document row, so the
//!   rename's UPDATE blocks (seen through `pg_blocking_pids`) after its authorization reads; the
//!   revocation is started on a connection of its own, and the test waits until it has either
//!   committed or is itself blocked on a lock; then the gate is released. These tests are
//!   `#[ignore]`d: they fail today, because the handler's transaction takes no lock on the
//!   session row or the tenant membership it authorized with (see the ignore reason).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use figcraft_cloud_domain::{CloudDocumentId, DeploymentMode, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::{add_tenant_member, remove_tenant_member, set_workspace_member};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::sessions::{RevokeReason, SessionLimits, create_session, logout, revoke_all_for_principal};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use serde_json::{Value, json};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tower::ServiceExt as _;

use super::authenticator::{IdentityConfig, TENANT_HEADER};
use super::context::ContextSettings;
use super::{ApiConfig, AppState, api_router};
use crate::config::CloudRole;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const ORIGIN: &str = "https://figcraft-e3.test";
/// Upper bound on any single wait.
const BOUND: Duration = Duration::from_secs(20);

fn e3(kind: u8) -> Uuid {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("e3{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

fn database_url() -> Option<String> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; HTTP revocation race probe skipped");
        return None;
    };
    Some(std::fs::read_to_string(file).ok()?.trim_end().to_string())
}

async fn racer_pool(url: &str) -> TestResult<(PgPool, i32)> {
    let pool = PgPoolOptions::new().max_connections(1).min_connections(1).acquire_timeout(BOUND).connect(url).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&pool).await?;
    Ok((pool, pid))
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

fn app(pool: &PgPool) -> Router {
    let identity = Arc::new(IdentityConfig::new(DeploymentMode::MultiTenant, None, Some(ORIGIN), Some(b"e3-test-signing-key-0123456789")).unwrap());
    let config = ApiConfig { deployment: DeploymentMode::MultiTenant, ..ApiConfig::default() };
    let state = AppState::new(pool.clone(), objects(), config).with_identity(identity);
    crate::health::app(CloudRole::Api, true, Some(api_router(state)), ContextSettings::new(CloudRole::Api, 64, BOUND, 1 << 20))
}

/// A tenant (owned by someone else) where `member` is a tenant editor and workspace editor, with
/// a document and a live session for the member.
struct World {
    tenant: TenantId,
    member: PrincipalId,
    document: CloudDocumentId,
    token: String,
}

async fn principal(pool: &PgPool) -> TestResult<PrincipalId> {
    let id = e3(0x01);
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e3 http race')")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(id.to_string().parse()?)
}

async fn world(pool: &PgPool) -> TestResult<World> {
    let owner = principal(pool).await?;
    let member = principal(pool).await?;
    let tenant: TenantId = e3(0x00).to_string().parse()?;
    let workspace: WorkspaceId = e3(0x10).to_string().parse()?;
    let project: ProjectId = e3(0x20).to_string().parse()?;
    let document: CloudDocumentId = e3(0x30).to_string().parse()?;
    let slug = format!("e3-http-{}", &tenant.to_string()[24..]);
    let mut t = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name: "e3 http races", plan_id: "team", home_region: "local" }, owner)
        .await
        .map_err(|error| format!("{error:?}"))?;
    add_tenant_member(&mut t, member, Role::Editor).await.map_err(|error| format!("{error:?}"))?;
    create_workspace(&mut t, workspace, "e3 ws", None).await.map_err(|error| format!("{error:?}"))?;
    create_project(&mut t, project, workspace, "e3 project", None).await.map_err(|error| format!("{error:?}"))?;
    create_document(&mut t, document, workspace, project, "e3 original").await.map_err(|error| format!("{error:?}"))?;
    set_workspace_member(&mut t, workspace, member, Role::Editor).await.map_err(|error| format!("{error:?}"))?;
    t.commit().await?;
    let token = create_session(pool, member, SessionLimits::default()).await.map_err(|error| format!("{error:?}"))?.token;
    Ok(World { tenant, member, document, token })
}

/// Rename the document as the member; the status and the error code, if any.
async fn rename(app: &Router, world: &World, name: &str) -> (StatusCode, String) {
    let peer: SocketAddr = "192.0.2.53:4000".parse().unwrap();
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/api/v1/documents/{}", world.document))
        .extension(ConnectInfo(peer))
        .header(header::COOKIE, format!("__Host-figcraft_session={}", world.token))
        .header(TENANT_HEADER, world.tenant.to_string())
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "name": name }).to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body["error"]["code"].as_str().unwrap_or("").to_string())
}

fn refused(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND)
}

async fn document_name(pool: &PgPool, world: &World) -> TestResult<String> {
    let mut t = begin_tenant_transaction(pool, world.tenant).await.map_err(|error| format!("{error:?}"))?;
    let name: String = sqlx::query_scalar("SELECT name FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(world.tenant.to_string())
        .bind(world.document.to_string())
        .fetch_one(t.connection())
        .await?;
    t.rollback().await?;
    Ok(name)
}

#[derive(Clone, Copy, Debug)]
enum Revocation {
    Logout,
    RevokeAll,
    TenantMemberRemoved,
}

const REVOCATIONS: [Revocation; 3] = [Revocation::Logout, Revocation::RevokeAll, Revocation::TenantMemberRemoved];

/// Apply `revocation` on `pool` and commit.
async fn revoke(pool: &PgPool, tenant: TenantId, member: PrincipalId, token: &str, revocation: Revocation) -> TestResult {
    match revocation {
        Revocation::Logout => assert_eq!(logout(pool, token).await, Ok(true)),
        Revocation::RevokeAll => assert_eq!(revoke_all_for_principal(pool, member, RevokeReason::Operator).await, Ok(1)),
        Revocation::TenantMemberRemoved => {
            let mut t = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
            remove_tenant_member(&mut t, member).await.map_err(|error| format!("{error:?}"))?;
            t.commit().await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_starting_after_the_revoke_commits_are_refused_and_change_nothing() -> TestResult {
    let Some(url) = database_url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new().max_connections(8).acquire_timeout(BOUND).connect(&url).await?;
    let app = app(&pool);
    for revocation in REVOCATIONS {
        let world = world(&pool).await?;
        // Committed before the revoke: it stands.
        assert_eq!(rename(&app, &world, "before").await.0, StatusCode::OK, "{revocation:?}");
        revoke(&pool, world.tenant, world.member, &world.token, revocation).await?;
        let (status, code) = rename(&app, &world, "after").await;
        assert!(refused(status), "{revocation:?}: {status} {code}");
        assert_eq!(document_name(&pool, &world).await?, "before", "{revocation:?}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_straddling_the_revoke_are_refused_once_it_has_committed() -> TestResult {
    let Some(url) = database_url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new().max_connections(12).acquire_timeout(BOUND).connect(&url).await?;
    let app = app(&pool);
    for revocation in REVOCATIONS {
        let world = Arc::new(world(&pool).await?);
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for racer in 0..4 {
            let (app, world, stop) = (app.clone(), Arc::clone(&world), Arc::clone(&stop));
            handles.push(tokio::spawn(async move {
                let mut outcomes = Vec::new();
                let mut attempt = 0u32;
                while !stop.load(Ordering::SeqCst) && outcomes.len() < 2_000 {
                    attempt += 1;
                    let started = Instant::now();
                    let (status, code) = rename(&app, &world, &format!("racer {racer} attempt {attempt}")).await;
                    assert!(status == StatusCode::OK || refused(status), "unexpected {status} {code}");
                    outcomes.push((started, status == StatusCode::OK));
                }
                outcomes
            }));
        }
        // Let the racers get going, then revoke; once the revoke has returned, every request that
        // starts is refused.
        let warmup = Instant::now();
        while warmup.elapsed() < Duration::from_millis(150) {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        revoke(&pool, world.tenant, world.member, &world.token, revocation).await?;
        let committed = Instant::now();
        while committed.elapsed() < Duration::from_millis(250) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.store(true, Ordering::SeqCst);
        let (mut before, mut after) = (0, 0);
        for handle in handles {
            for (started, succeeded) in tokio::time::timeout(BOUND, handle).await?? {
                if started > committed {
                    after += 1;
                    assert!(!succeeded, "{revocation:?}: a request that started after the revoke committed succeeded");
                } else if succeeded {
                    before += 1;
                }
            }
        }
        assert!(before > 0 && after > 0, "{revocation:?}: the race didn't straddle the commit ({before} before, {after} after)");
    }
    Ok(())
}

/// Wait until some backend is blocked behind `gate_pid`.
async fn wait_blocked_behind(pool: &PgPool, gate_pid: i32) -> TestResult {
    let deadline = Instant::now() + BOUND;
    loop {
        let blocked: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))").bind(gate_pid).fetch_one(pool).await?;
        if blocked > 0 {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("the request never blocked on the gate".into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// See the module docs: a request paused after its authorization, before its commit, while the
/// revocation commits or is held off.
async fn paused_request_race(revocation: Revocation) -> TestResult {
    let Some(url) = database_url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new().max_connections(8).acquire_timeout(BOUND).connect(&url).await?;
    let app = app(&pool);
    let world = Arc::new(world(&pool).await?);

    // The gate holds the document row: the rename's UPDATE will wait on it.
    let mut gate = begin_tenant_transaction(&pool, world.tenant).await.map_err(|error| format!("{error:?}"))?;
    let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(gate.connection()).await?;
    sqlx::query("SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
        .bind(world.tenant.to_string())
        .bind(world.document.to_string())
        .execute(gate.connection())
        .await?;
    let requesting = {
        let (app, world) = (app.clone(), Arc::clone(&world));
        tokio::spawn(async move { rename(&app, &world, "paused").await })
    };
    wait_blocked_behind(&pool, gate_pid).await?;

    // The revocation, on its own connection: it either commits now or is held off by a lock.
    let (revoker, revoker_pid) = racer_pool(&url).await?;
    let revoking = {
        let world = Arc::clone(&world);
        tokio::spawn(async move { revoke(&revoker, world.tenant, world.member, &world.token, revocation).await })
    };
    let deadline = Instant::now() + BOUND;
    let revoke_committed_first = loop {
        if revoking.is_finished() {
            break true;
        }
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock'")
            .bind(revoker_pid)
            .fetch_one(&pool)
            .await?;
        if waiting == 1 {
            break false;
        }
        if Instant::now() > deadline {
            return Err("the revocation neither committed nor blocked".into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    gate.rollback().await?;
    let (status, code) = tokio::time::timeout(BOUND, requesting).await??;
    tokio::time::timeout(BOUND, revoking).await???;
    let name = document_name(&pool, &world).await?;
    if revoke_committed_first {
        assert!(
            refused(status) && name == "e3 original",
            "{revocation:?}: the revocation committed first, yet the paused request answered {status} {code} and the document is named {name:?}"
        );
    } else {
        // The request held the revocation off, so it committed first and stands.
        assert_eq!((status, name.as_str()), (StatusCode::OK, "paused"), "{revocation:?}");
    }
    // Either way the next request is refused.
    assert!(refused(rename(&app, &world, "later").await.0), "{revocation:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "known race: a request authorized before a logout commits its mutation after it (the handler transaction does not lock the session row)"]
async fn a_request_paused_before_commit_never_commits_after_a_logout() -> TestResult {
    paused_request_race(Revocation::Logout).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "known race: a request authorized before revoke_all_for_principal commits its mutation after it (the handler transaction does not lock the session row)"]
async fn a_request_paused_before_commit_never_commits_after_revoke_all() -> TestResult {
    paused_request_race(Revocation::RevokeAll).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "known race: a request authorized before the caller's tenant removal commits its mutation after it (the handler reads tenant_members without FOR SHARE)"]
async fn a_request_paused_before_commit_never_commits_after_tenant_member_removal() -> TestResult {
    paused_request_race(Revocation::TenantMemberRemoved).await
}
