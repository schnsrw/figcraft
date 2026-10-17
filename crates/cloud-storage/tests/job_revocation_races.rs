//! S4-023: an export job racing the revocation of its requester's `document.export`, against the
//! dev PostgreSQL and object store (fixture prefix e3, random tails per run; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR).
//!
//! The worker and the revoker each run on a pool of their own with exactly one connection, so
//! their backend pids are known, on a multi-thread runtime. The order is forced, never slept
//! into: one side holds its transaction open with its writes (or its recheck locks) done, the
//! other is started and the test waits, through `pg_stat_activity`, until it is blocked on a lock;
//! then the first side commits. Both orders are tested for each way of losing access:
//!
//! * **revoke first** (the revocation is written but not committed when the worker commits): the
//!   worker's recheck (`recheck_before_commit`, inside `complete_export`) blocks on the decision
//!   rows the revocation holds, then sees the revocation, so `complete_export` answers
//!   `AuthorizationRevoked` before any byte is written or registered. The worker rolls back and
//!   fails the export with `authorization_revoked`; no result object is registered, referenced or
//!   attached.
//! * **job first** (the worker has rechecked and holds the decision rows `FOR SHARE`): the
//!   revocation waits for the job's commit, so the result is committed strictly before the revoke;
//!   the next recheck refuses.
//!
//! Either way no job result is ever committed after a revocation that committed first.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::exports::{
    ExportError, ExportFormat, ExportState, complete_export, fail_export, get_export, request_export, start_export,
};
use figcraft_cloud_storage::job_authorization::{
    AUTHORIZATION_REVOKED, JobAuthorization, JobAuthorizationError, JobPermission, recheck_before_commit,
};
use figcraft_cloud_storage::memberships::{
    add_tenant_member, remove_document_member, remove_tenant_member, remove_workspace_member, set_document_member, set_workspace_member,
    set_workspace_team_grant, suspend_tenant_member,
};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::teams::{TeamRole, add_team_member, create_team, remove_team_member};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::task::JoinHandle;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

/// Upper bound on any single wait in these tests.
const BOUND: Duration = Duration::from_secs(20);

/// A fresh e3-prefixed UUID; `kind` keeps the families apart.
fn e3(kind: u8) -> Uuid {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("e3{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

fn database_url() -> TestResult<Option<String>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; job revocation race probe skipped");
        return Ok(None);
    };
    Ok(Some(std::fs::read_to_string(file)?.trim_end().to_string()))
}

fn object_store() -> TestResult<Option<ObjectStore>> {
    let Ok(directory) = std::env::var("FIGCRAFT_TEST_S3_DIR") else {
        eprintln!("FIGCRAFT_TEST_S3_DIR is unset; job revocation race probe skipped");
        return Ok(None);
    };
    let read =
        |name: &str| -> TestResult<String> { Ok(std::fs::read_to_string(std::path::Path::new(&directory).join(name))?.trim_end().to_string()) };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    Ok(Some(ObjectStore::new(&settings)?))
}

async fn shared_pool(url: &str) -> TestResult<PgPool> {
    Ok(PgPoolOptions::new().max_connections(4).acquire_timeout(BOUND).connect(url).await?)
}

/// A racer's own single-connection pool and that connection's backend pid.
async fn racer_pool(url: &str) -> TestResult<(PgPool, i32)> {
    let pool = PgPoolOptions::new().max_connections(1).min_connections(1).acquire_timeout(BOUND).connect(url).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&pool).await?;
    Ok((pool, pid))
}

async fn tx(pool: &PgPool, tenant: TenantId) -> TestResult<TenantTransaction<'_>> {
    begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

/// Wait until backend `pid` is waiting on a lock.
async fn wait_until_blocked(pool: &PgPool, pid: i32) -> TestResult {
    let deadline = Instant::now() + BOUND;
    loop {
        let waiting: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock'").bind(pid).fetch_one(pool).await?;
        if waiting == 1 {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("backend {pid} never blocked on a lock").into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn join<T>(handle: JoinHandle<TestResult<T>>) -> TestResult<T> {
    timeout(BOUND, handle).await??
}

fn fail<E: std::fmt::Debug>(error: E) -> BoxError {
    format!("{error:?}").into()
}

/// A valid 1x1 PNG unique to this run (a `tEXt` chunk carries a random id).
fn unique_png() -> Vec<u8> {
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc.finalize().to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    chunk(&mut png, b"tEXt", format!("Comment\0e3 export race {}", Uuid::new_v4()).as_bytes());
    chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// A tenant with an owner, a workspace, a project, a document, and a team granted `viewer` on the
/// workspace.
struct World {
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    document: CloudDocumentId,
    team: Uuid,
}

async fn principal(pool: &PgPool) -> TestResult<PrincipalId> {
    let id = e3(0x01);
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e3 job race')")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(id.to_string().parse()?)
}

async fn world(pool: &PgPool) -> TestResult<World> {
    let tenant: TenantId = e3(0x00).to_string().parse()?;
    let owner = principal(pool).await?;
    let mut t = tx(pool, tenant).await?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'e3 job races', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("e3-jobs-{}", &tenant.to_string()[24..]))
        .execute(t.connection())
        .await?;
    add_tenant_member(&mut t, owner, Role::TenantOwner).await.map_err(fail)?;
    let workspace: WorkspaceId = e3(0x10).to_string().parse()?;
    let project: ProjectId = e3(0x20).to_string().parse()?;
    let document: CloudDocumentId = e3(0x30).to_string().parse()?;
    create_workspace(&mut t, workspace, "e3 workspace", None).await.map_err(fail)?;
    create_project(&mut t, project, workspace, "e3 project", None).await.map_err(fail)?;
    create_document(&mut t, document, workspace, project, "e3 document").await.map_err(fail)?;
    let team = e3(0x40);
    create_team(&mut t, team, &format!("e3-{}", &team.to_string()[24..]), "e3 export team", owner).await.map_err(fail)?;
    set_workspace_team_grant(&mut t, workspace, team, Role::Viewer).await.map_err(fail)?;
    t.commit().await?;
    Ok(World { tenant, owner, workspace, document, team })
}

/// How the requester holds, and then loses, `document.export`.
#[derive(Clone, Copy, Debug)]
enum Loss {
    /// A direct workspace membership is deleted.
    WorkspaceMemberRemoved,
    /// A direct document membership is deleted.
    DocumentMemberRemoved,
    /// The team whose workspace grant gave access drops them.
    TeamMemberRemoved,
    /// They are removed from the tenant.
    TenantMemberRemoved,
    /// Their tenant membership is suspended.
    TenantMemberSuspended,
}

const LOSSES: [Loss; 5] =
    [Loss::WorkspaceMemberRemoved, Loss::DocumentMemberRemoved, Loss::TeamMemberRemoved, Loss::TenantMemberRemoved, Loss::TenantMemberSuspended];

/// A tenant member who may export the document through `loss`'s path.
async fn requester(pool: &PgPool, world: &World, loss: Loss) -> TestResult<PrincipalId> {
    let person = principal(pool).await?;
    let mut t = tx(pool, world.tenant).await?;
    add_tenant_member(&mut t, person, Role::Editor).await.map_err(fail)?;
    match loss {
        Loss::DocumentMemberRemoved => set_document_member(&mut t, world.document, person, Role::Viewer).await.map_err(fail)?,
        Loss::TeamMemberRemoved => add_team_member(&mut t, world.team, person, TeamRole::Viewer, world.owner).await.map_err(fail)?,
        Loss::WorkspaceMemberRemoved | Loss::TenantMemberRemoved | Loss::TenantMemberSuspended => {
            set_workspace_member(&mut t, world.workspace, person, Role::Viewer).await.map_err(fail)?;
        }
    }
    t.commit().await?;
    Ok(person)
}

/// Apply `loss` to `person` inside `t` (not committed).
async fn revoke(t: &mut TenantTransaction<'_>, world: &World, person: PrincipalId, loss: Loss) -> TestResult {
    match loss {
        Loss::WorkspaceMemberRemoved => assert!(remove_workspace_member(t, world.workspace, person).await.map_err(fail)?),
        Loss::DocumentMemberRemoved => assert!(remove_document_member(t, world.document, person).await.map_err(fail)?),
        Loss::TeamMemberRemoved => assert!(remove_team_member(t, world.team, person, world.owner).await.map_err(fail)?),
        Loss::TenantMemberRemoved => remove_tenant_member(t, person).await.map_err(fail)?,
        Loss::TenantMemberSuspended => suspend_tenant_member(t, person).await.map_err(fail)?,
    }
    Ok(())
}

/// An export requested (enqueued) by `person` and picked up by a worker (`running`).
async fn running_export(pool: &PgPool, world: &World, person: PrincipalId) -> TestResult<Uuid> {
    let mut t = tx(pool, world.tenant).await?;
    let export = request_export(&mut t, world.document, person, ExportFormat::Png, &serde_json::json!({}), 0).await.map_err(fail)?;
    start_export(&mut t, export.id).await.map_err(fail)?;
    t.commit().await?;
    Ok(export.id)
}

/// The export's job (state, error code), and how many objects are registered under or referenced
/// by the export.
async fn export_footprint(pool: &PgPool, world: &World, export: Uuid) -> TestResult<((String, Option<String>), i64, i64)> {
    let mut t = tx(pool, world.tenant).await?;
    let job: (String, Option<String>) = sqlx::query_as(
        "SELECT j.state, j.error_code FROM jobs j JOIN exports e ON e.tenant_id = j.tenant_id AND e.job_id = j.id WHERE e.tenant_id = $1::uuid AND e.id = $2::uuid",
    )
    .bind(world.tenant.to_string())
    .bind(export.to_string())
    .fetch_one(t.connection())
    .await?;
    let objects: i64 = sqlx::query_scalar("SELECT count(*) FROM objects WHERE tenant_id = $1::uuid AND starts_with(object_key, $2)")
        .bind(world.tenant.to_string())
        .bind(format!("tenants/{}/exports/{}/", world.tenant, export))
        .fetch_one(t.connection())
        .await?;
    let references: i64 =
        sqlx::query_scalar("SELECT count(*) FROM object_references WHERE tenant_id = $1::uuid AND owner_kind = 'export' AND owner_id = $2::uuid")
            .bind(world.tenant.to_string())
            .bind(export.to_string())
            .fetch_one(t.connection())
            .await?;
    t.rollback().await?;
    Ok((job, objects, references))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_committing_while_the_export_commits_fails_it_with_nothing_attached() -> TestResult {
    let (Some(url), Some(store)) = (database_url()?, object_store()?) else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let world = world(&pool).await?;
    for loss in LOSSES {
        let person = requester(&pool, &world, loss).await?;
        let export = running_export(&pool, &world, person).await?;

        // The revocation is written, not committed: it holds the decision rows it changes.
        let (revoker, _) = racer_pool(&url).await?;
        let mut revoking = tx(&revoker, world.tenant).await?;
        revoke(&mut revoking, &world, person, loss).await?;

        // The worker commits the result now: its recheck blocks on the revocation's locks.
        let (worker, worker_pid) = racer_pool(&url).await?;
        let worker_store = store.clone();
        let tenant = world.tenant;
        let working = tokio::spawn(async move {
            let mut t = begin_tenant_transaction(&worker, tenant).await.map_err(fail)?;
            let completed = complete_export(&mut t, &worker_store, export, unique_png(), "e3.png", "e3 worker").await.map(|done| done.export.id);
            if completed.is_ok() {
                t.commit().await?;
            } else {
                t.rollback().await?;
            }
            Ok::<_, BoxError>(completed)
        });
        wait_until_blocked(&pool, worker_pid).await?;
        revoking.commit().await?;

        // The recheck saw the committed revocation: refused before anything was written.
        assert_eq!(join(working).await?, Err(ExportError::AuthorizationRevoked), "{loss:?}");
        let mut t = tx(&pool, world.tenant).await?;
        let failed = fail_export(&mut t, export, AUTHORIZATION_REVOKED).await.map_err(fail)?;
        t.commit().await?;
        assert_eq!(
            (failed.state, failed.error_code.as_deref(), failed.result_object_id),
            (ExportState::Failed, Some(AUTHORIZATION_REVOKED), None),
            "{loss:?}"
        );
        let (job, objects, references) = export_footprint(&pool, &world, export).await?;
        assert_eq!(job, ("failed".to_string(), Some(AUTHORIZATION_REVOKED.to_string())), "{loss:?}");
        assert_eq!((objects, references), (0, 0), "{loss:?}: no result object is registered or referenced");
        // A retried completion cannot attach a result to the failed export either.
        let mut t = tx(&pool, world.tenant).await?;
        assert_eq!(
            complete_export(&mut t, &store, export, unique_png(), "e3.png", "e3 worker").await.map(|_| ()),
            Err(ExportError::InvalidState),
            "{loss:?}"
        );
        t.rollback().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_arriving_after_the_recheck_waits_for_the_export_commit() -> TestResult {
    let (Some(url), Some(store)) = (database_url()?, object_store()?) else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let world = world(&pool).await?;
    for loss in LOSSES {
        let person = requester(&pool, &world, loss).await?;
        let export = running_export(&pool, &world, person).await?;

        // The worker has rechecked and written its result, not yet committed.
        let (worker, _) = racer_pool(&url).await?;
        let mut working = tx(&worker, world.tenant).await?;
        let done = complete_export(&mut working, &store, export, unique_png(), "e3.png", "e3 worker").await.map_err(fail)?;
        assert_eq!(done.export.state, ExportState::Succeeded);

        // The revocation can't commit in between: it blocks on the rows the recheck holds.
        let (revoker, revoker_pid) = racer_pool(&url).await?;
        let (tenant, owner, workspace, document, team) = (world.tenant, world.owner, world.workspace, world.document, world.team);
        let revoking = tokio::spawn(async move {
            let world = World { tenant, owner, workspace, document, team };
            let mut t = begin_tenant_transaction(&revoker, tenant).await.map_err(fail)?;
            revoke(&mut t, &world, person, loss).await?;
            t.commit().await?;
            Ok::<_, BoxError>(Instant::now())
        });
        wait_until_blocked(&pool, revoker_pid).await?;
        working.commit().await?;
        let committed = Instant::now();
        let revoked = join(revoking).await?;
        assert!(revoked >= committed, "{loss:?}: the revocation committed before the job");

        // The result is attached (it committed first) ...
        let mut t = tx(&pool, world.tenant).await?;
        let record = get_export(&mut t, export).await.map_err(fail)?.ok_or("export vanished")?;
        t.rollback().await?;
        assert_eq!((record.state, record.result_object_id), (ExportState::Succeeded, Some(done.object.id)), "{loss:?}");
        // ... and the next job for this requester is refused at its recheck.
        let job = JobAuthorization { requested_by: person, document: world.document, permission: JobPermission::DocumentExport };
        let mut t = tx(&pool, world.tenant).await?;
        assert_eq!(recheck_before_commit(&mut t, &job).await, Err(JobAuthorizationError::Revoked), "{loss:?}");
        t.rollback().await?;
    }
    Ok(())
}
