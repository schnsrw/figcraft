//! S4-013, S4-017 and S4-018 against the dev PostgreSQL (fixture prefix d5; every run creates its
//! own rows with random tails, so reruns never collide; `deploy/saas/identity-revocation-cleanup.sql`
//! removes them). Skipped without FIGCRAFT_TEST_DATABASE_URL_FILE (and, for the export test,
//! FIGCRAFT_TEST_S3_DIR).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::exports::{
    ExportError, ExportFormat, ExportState, complete_export, fail_export, get_export, request_export, start_export,
};
use figcraft_cloud_storage::job_authorization::{
    AUTHORIZATION_REVOKED, JobAuthorization, JobAuthorizationError, JobPermission, fail_job_authorization_revoked, recheck_before_commit,
};
use figcraft_cloud_storage::memberships::{
    add_tenant_member, remove_tenant_member, remove_workspace_member, set_workspace_member, set_workspace_team_grant, suspend_tenant_member,
};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::revocation::{
    AccessRevocation, RevocationError, admit_member, apply_revocation, authorize_session_in_tenant, dead_member_admissions,
};
use figcraft_cloud_storage::sessions::{SessionError, SessionLimits, create_session, resolve_session};
use figcraft_cloud_storage::support_access::{
    MAX_GRANT_SECS, SupportAccessError, SupportScope, active_support_access, grant_support_access, list_active_support_grants, revoke_support_access,
    use_support_access,
};
use figcraft_cloud_storage::teams::{TeamRole, add_team_member, create_team, remove_team_member};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; identity revocation probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

/// A fresh d5-prefixed UUID; `kind` keeps the families apart.
fn fresh(kind: u8) -> Uuid {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("d5{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

async fn principal(pool: &PgPool, name: &str) -> PrincipalId {
    let id = fresh(0x01);
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', $2)")
        .bind(id.to_string())
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id.to_string().parse().unwrap()
}

/// A tenant with an owner, a workspace, a project and a document.
struct World {
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    document: CloudDocumentId,
}

async fn new_world(pool: &PgPool) -> World {
    let tenant: TenantId = fresh(0x00).to_string().parse().unwrap();
    let owner = principal(pool, "d5 owner").await;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.unwrap();
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'd5 probe', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("d5-{}", &tenant.to_string()[24..]))
        .execute(transaction.connection())
        .await
        .unwrap();
    add_tenant_member(&mut transaction, owner, Role::TenantOwner).await.unwrap();
    let workspace: WorkspaceId = fresh(0x10).to_string().parse().unwrap();
    let project: ProjectId = fresh(0x20).to_string().parse().unwrap();
    let document: CloudDocumentId = fresh(0x30).to_string().parse().unwrap();
    create_workspace(&mut transaction, workspace, "d5 workspace", None).await.unwrap();
    create_project(&mut transaction, project, workspace, "d5 project", None).await.unwrap();
    create_document(&mut transaction, document, workspace, project, "d5 document").await.unwrap();
    transaction.commit().await.unwrap();
    World { tenant, owner, workspace, document }
}

/// A new tenant member (tenant role `editor`) holding `workspace_role` on the world's workspace.
async fn member(pool: &PgPool, world: &World, workspace_role: Option<Role>) -> PrincipalId {
    let principal = principal(pool, "d5 member").await;
    let mut transaction = begin_tenant_transaction(pool, world.tenant).await.unwrap();
    add_tenant_member(&mut transaction, principal, Role::Editor).await.unwrap();
    if let Some(role) = workspace_role {
        set_workspace_member(&mut transaction, world.workspace, principal, role).await.unwrap();
    }
    transaction.commit().await.unwrap();
    principal
}

async fn audit_rows(pool: &PgPool, tenant: TenantId, action: &str) -> Vec<String> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.unwrap();
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT metadata::text FROM audit_events WHERE tenant_id = $1::uuid AND action = $2 ORDER BY created_at, id")
            .bind(tenant.to_string())
            .bind(action)
            .fetch_all(transaction.connection())
            .await
            .unwrap();
    transaction.rollback().await.unwrap();
    rows
}

// ---- S4-018 ----

#[tokio::test]
async fn support_grants_are_bounded_audited_expire_and_revoke() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = new_world(&pool).await;
    let other = new_world(&pool).await;
    let operator = principal(&pool, "d5 operator").await;
    let admin = world.owner;

    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    for (reason, secs, to, expected) in [
        ("", 3600, operator, SupportAccessError::InvalidReason),
        ("   ", 3600, operator, SupportAccessError::InvalidReason),
        (&*"r".repeat(501), 3600, operator, SupportAccessError::InvalidReason),
        ("bad\u{1}reason", 3600, operator, SupportAccessError::InvalidReason),
        ("ticket", 0, operator, SupportAccessError::InvalidExpiry),
        ("ticket", MAX_GRANT_SECS + 1, operator, SupportAccessError::InvalidExpiry),
        ("ticket", 3600, admin, SupportAccessError::InvalidGrantee),
        ("ticket", 3600, PrincipalId::random(), SupportAccessError::InvalidGrantee),
    ] {
        assert_eq!(grant_support_access(&mut tx, to, admin, reason, SupportScope::Read, secs).await, Err(expected), "{reason:?} {secs}");
    }
    assert_eq!(active_support_access(&mut tx, operator).await?, None);
    let grant = grant_support_access(&mut tx, operator, admin, "  ticket 4711: broken export  ", SupportScope::Read, MAX_GRANT_SECS).await?;
    assert_eq!(grant.reason, "ticket 4711: broken export");
    assert_eq!(grant.expires_at - grant.created_at, i64::try_from(MAX_GRANT_SECS)?);
    tx.commit().await?;

    // Live: visible, listed, usable (and the use is audited with the grant's reason).
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    let access = active_support_access(&mut tx, operator).await?.unwrap();
    assert_eq!((access.grant_id, access.scope, access.tenant_role()), (grant.id, SupportScope::Read, Role::Viewer));
    assert_eq!(list_active_support_grants(&mut tx).await?.iter().map(|g| g.id).collect::<Vec<_>>(), vec![grant.id]);
    assert_eq!(use_support_access(&mut tx, operator, "Document.View", "document").await, Err(SupportAccessError::InvalidLabel));
    assert_eq!(use_support_access(&mut tx, operator, "document.view", "document").await?.map(|a| a.grant_id), Some(grant.id));
    assert_eq!(use_support_access(&mut tx, admin, "document.view", "document").await?, None);
    tx.commit().await?;
    // Another tenant knows nothing of it.
    let mut tx = begin_tenant_transaction(&pool, other.tenant).await.unwrap();
    assert_eq!(active_support_access(&mut tx, operator).await?, None);
    assert!(list_active_support_grants(&mut tx).await?.is_empty());
    assert_eq!(revoke_support_access(&mut tx, grant.id, other.owner, "not yours").await, Err(SupportAccessError::NotFound));
    tx.rollback().await?;

    let grants = audit_rows(&pool, world.tenant, "support.grant").await;
    assert_eq!(grants.len(), 1);
    assert!(grants[0].contains("ticket 4711: broken export") && grants[0].contains("\"scope\": \"read\""), "{grants:?}");
    let uses = audit_rows(&pool, world.tenant, "support.use").await;
    assert_eq!(uses.len(), 1);
    assert!(uses[0].contains("ticket 4711") && uses[0].contains("document.view"), "{uses:?}");

    // Expiry: a grant whose time is up opens nothing and is not listed; nothing is audited.
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    let write = grant_support_access(&mut tx, operator, admin, "short write window", SupportScope::Write, 60).await?;
    assert_eq!(active_support_access(&mut tx, operator).await?.map(|a| (a.grant_id, a.resource_role())), Some((write.id, Role::Editor)));
    sqlx::query(
        "UPDATE support_access_grants SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE id = $1::uuid",
    )
    .bind(write.id.to_string())
    .execute(tx.connection())
    .await?;
    // The 7-day bound is the table's too.
    let too_long =
        sqlx::query("UPDATE support_access_grants SET expires_at = created_at + interval '8 days' WHERE id = $1::uuid").bind(write.id.to_string());
    let mut savepoint = sqlx::Connection::begin(tx.connection()).await?;
    assert!(too_long.execute(&mut *savepoint).await.is_err());
    savepoint.rollback().await?;
    assert_eq!(active_support_access(&mut tx, operator).await?.map(|a| a.grant_id), Some(grant.id));
    assert_eq!(revoke_support_access(&mut tx, write.id, admin, "expired anyway").await, Err(SupportAccessError::NotFound));
    tx.commit().await?;

    // Revoke: audited with both reasons; afterwards nothing opens and a second revoke is NotFound.
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    assert_eq!(revoke_support_access(&mut tx, grant.id, admin, "").await, Err(SupportAccessError::InvalidReason));
    let revoked = revoke_support_access(&mut tx, grant.id, admin, "issue resolved").await?;
    assert!(revoked.revoked_at.is_some());
    assert_eq!(active_support_access(&mut tx, operator).await?, None);
    assert_eq!(use_support_access(&mut tx, operator, "document.view", "document").await?, None);
    assert!(list_active_support_grants(&mut tx).await?.is_empty());
    assert_eq!(revoke_support_access(&mut tx, grant.id, admin, "again").await, Err(SupportAccessError::NotFound));
    tx.commit().await?;
    let revokes = audit_rows(&pool, world.tenant, "support.revoke").await;
    assert_eq!(revokes.len(), 1);
    assert!(revokes[0].contains("issue resolved") && revokes[0].contains("ticket 4711"), "{revokes:?}");
    assert_eq!(audit_rows(&pool, world.tenant, "support.use").await.len(), 1);
    Ok(())
}

// ---- S4-017 (sessions and member admissions) ----

#[tokio::test]
async fn losing_a_tenant_refuses_the_next_request_and_the_last_tenant_signs_out() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let first = new_world(&pool).await;
    let second = new_world(&pool).await;
    let person = member(&pool, &first, Some(Role::Editor)).await;
    let mut tx = begin_tenant_transaction(&pool, second.tenant).await.unwrap();
    add_tenant_member(&mut tx, person, Role::Viewer).await?;
    tx.commit().await?;
    let limits = SessionLimits::default();
    let session = create_session(&pool, person, limits).await?;
    assert_eq!(authorize_session_in_tenant(&pool, &session.token, limits, first.tenant).await?.0.principal_id, person);
    assert!(authorize_session_in_tenant(&pool, &session.token, limits, second.tenant).await.is_ok());

    // Removed from the first tenant: that tenant refuses the very next request, the session lives
    // on for the second.
    let mut tx = begin_tenant_transaction(&pool, first.tenant).await.unwrap();
    remove_tenant_member(&mut tx, person).await?;
    tx.commit().await?;
    assert_eq!(apply_revocation(&pool, AccessRevocation::TenantMemberRemoved { principal: person }).await?.sessions_revoked, 0);
    assert_eq!(authorize_session_in_tenant(&pool, &session.token, limits, first.tenant).await.map(|_| ()), Err(RevocationError::Refused));
    assert!(authorize_session_in_tenant(&pool, &session.token, limits, second.tenant).await.is_ok());

    // Suspended in the last tenant: every session is revoked.
    let mut tx = begin_tenant_transaction(&pool, second.tenant).await.unwrap();
    suspend_tenant_member(&mut tx, person).await?;
    tx.commit().await?;
    assert_eq!(apply_revocation(&pool, AccessRevocation::TenantMemberSuspended { principal: person }).await?.sessions_revoked, 1);
    assert_eq!(resolve_session(&pool, &session.token, limits).await, Err(SessionError::Refused));
    assert_eq!(authorize_session_in_tenant(&pool, &session.token, limits, second.tenant).await.map(|_| ()), Err(RevocationError::Refused));
    // Team, grant and tenant revocations touch no session.
    assert_eq!(apply_revocation(&pool, AccessRevocation::GrantRevoked).await?.sessions_revoked, 0);
    Ok(())
}

#[tokio::test]
async fn member_admissions_die_with_their_session_role_or_team() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = new_world(&pool).await;
    let limits = SessionLimits::default();
    let direct = member(&pool, &world, Some(Role::Editor)).await;
    let teamed = member(&pool, &world, None).await;
    let outsider = member(&pool, &world, None).await;
    let team = fresh(0x40);
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    create_team(&mut tx, team, &format!("d5-{}", &team.to_string()[24..]), "d5 team", world.owner).await?;
    add_team_member(&mut tx, team, teamed, TeamRole::Editor, world.owner).await?;
    set_workspace_team_grant(&mut tx, world.workspace, team, Role::Editor).await?;
    tx.commit().await?;
    let direct_session = create_session(&pool, direct, limits).await?;
    let teamed_session = create_session(&pool, teamed, limits).await?;
    let outsider_session = create_session(&pool, outsider, limits).await?;

    let direct_edit = admit_member(&pool, world.tenant, &direct_session.token, world.document, limits).await?;
    let teamed_edit = admit_member(&pool, world.tenant, &teamed_session.token, world.document, limits).await?;
    assert!(direct_edit.edit && teamed_edit.edit);
    assert_eq!((direct_edit.principal, direct_edit.session), (direct, direct_session.id));
    // No role on the document, a bad token, or the wrong tenant: refused alike.
    for (tenant, token) in [
        (world.tenant, outsider_session.token.as_str()),
        (world.tenant, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        (TenantId::random(), direct_session.token.as_str()),
    ] {
        assert_eq!(admit_member(&pool, tenant, token, world.document, limits).await.map(|_| ()), Err(RevocationError::Refused));
    }
    assert!(dead_member_admissions(&pool, &[direct_edit, teamed_edit]).await?.is_empty());

    // Downgrading an editor kills the edit admission (not a view one); removal kills both.
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    set_workspace_member(&mut tx, world.workspace, direct, Role::Viewer).await?;
    tx.commit().await?;
    let direct_view = admit_member(&pool, world.tenant, &direct_session.token, world.document, limits).await?;
    assert!(!direct_view.edit);
    assert_eq!(dead_member_admissions(&pool, &[direct_edit, direct_view, teamed_edit]).await?, vec![direct_edit]);
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    assert!(remove_workspace_member(&mut tx, world.workspace, direct).await?);
    assert!(remove_team_member(&mut tx, team, teamed, world.owner).await?);
    tx.commit().await?;
    let mut dead = dead_member_admissions(&pool, &[direct_view, teamed_edit]).await?;
    dead.sort_by_key(|admission| admission.session);
    let mut expected = vec![direct_view, teamed_edit];
    expected.sort_by_key(|admission| admission.session);
    assert_eq!(dead, expected);

    // A revoked session is dead even while the role still holds.
    let survivor = member(&pool, &world, Some(Role::Viewer)).await;
    let session = create_session(&pool, survivor, limits).await?;
    let admission = admit_member(&pool, world.tenant, &session.token, world.document, limits).await?;
    assert!(dead_member_admissions(&pool, &[admission]).await?.is_empty());
    figcraft_cloud_storage::sessions::logout(&pool, &session.token).await?;
    assert_eq!(dead_member_admissions(&pool, &[admission]).await?, vec![admission]);
    Ok(())
}

// ---- S4-013 ----

async fn object_store() -> Result<Option<ObjectStore>, Box<dyn std::error::Error>> {
    let Ok(directory) = std::env::var("FIGCRAFT_TEST_S3_DIR") else {
        eprintln!("FIGCRAFT_TEST_S3_DIR is unset; export recheck probe skipped");
        return Ok(None);
    };
    let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::fs::read_to_string(std::path::Path::new(&directory).join(name))?.trim_end().to_string())
    };
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
    chunk(&mut png, b"tEXt", format!("Comment\0d5 export probe {}", Uuid::new_v4()).as_bytes());
    chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
    chunk(&mut png, b"IEND", &[]);
    png
}

async fn queued_export(pool: &PgPool, world: &World, principal: PrincipalId) -> Uuid {
    let mut tx = begin_tenant_transaction(pool, world.tenant).await.unwrap();
    let export = request_export(&mut tx, world.document, principal, ExportFormat::Png, &serde_json::json!({}), 0).await.unwrap();
    start_export(&mut tx, export.id).await.unwrap();
    tx.commit().await.unwrap();
    export.id
}

async fn job_state(pool: &PgPool, tenant: TenantId, job: Uuid) -> (String, Option<String>) {
    let mut tx = begin_tenant_transaction(pool, tenant).await.unwrap();
    let row = sqlx::query_as("SELECT state, error_code FROM jobs WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant.to_string())
        .bind(job.to_string())
        .fetch_one(tx.connection())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    row
}

#[tokio::test]
async fn an_export_whose_requester_lost_access_is_refused_at_commit_and_failed() -> TestResult {
    let (Some(pool), Some(store)) = (pool().await?, object_store().await?) else {
        return Ok(());
    };
    let world = new_world(&pool).await;
    let kept = member(&pool, &world, Some(Role::Viewer)).await;
    let lost = member(&pool, &world, Some(Role::Viewer)).await;
    let teamed = member(&pool, &world, None).await;
    let team = fresh(0x41);
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    create_team(&mut tx, team, &format!("d5-{}", &team.to_string()[24..]), "d5 export team", world.owner).await?;
    add_team_member(&mut tx, team, teamed, TeamRole::Viewer, world.owner).await?;
    set_workspace_team_grant(&mut tx, world.workspace, team, Role::Viewer).await?;
    tx.commit().await?;
    let kept_export = queued_export(&pool, &world, kept).await;
    let lost_export = queued_export(&pool, &world, lost).await;
    let teamed_export = queued_export(&pool, &world, teamed).await;

    // Access is lost after the exports were requested and started: a direct membership is
    // removed, and the other requester leaves the team whose grant gave them access.
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    assert!(remove_workspace_member(&mut tx, world.workspace, lost).await?);
    assert!(remove_team_member(&mut tx, team, teamed, world.owner).await?);
    tx.commit().await?;

    for export in [lost_export, teamed_export] {
        let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
        assert_eq!(
            complete_export(&mut tx, &store, export, unique_png(), "a.png", "d5 worker").await.map(|_| ()),
            Err(ExportError::AuthorizationRevoked)
        );
        tx.rollback().await?;
        let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
        let failed = fail_export(&mut tx, export, AUTHORIZATION_REVOKED).await?;
        tx.commit().await?;
        assert_eq!((failed.state, failed.error_code.as_deref(), failed.result_object_id), (ExportState::Failed, Some(AUTHORIZATION_REVOKED), None));
        assert_eq!(job_state(&pool, world.tenant, failed.job_id.unwrap()).await, ("failed".into(), Some(AUTHORIZATION_REVOKED.into())));
    }
    // The requester who kept access completes normally.
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    let done = complete_export(&mut tx, &store, kept_export, unique_png(), "a.png", "d5 worker").await?;
    tx.commit().await?;
    assert_eq!(done.export.state, ExportState::Succeeded);
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    assert_eq!(get_export(&mut tx, kept_export).await?.map(|e| e.state), Some(ExportState::Succeeded));
    tx.rollback().await?;

    // Removing the requester from the tenant revokes too (tenant membership is part of the check).
    let gone = member(&pool, &world, Some(Role::Editor)).await;
    let gone_export = queued_export(&pool, &world, gone).await;
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    remove_tenant_member(&mut tx, gone).await?;
    tx.commit().await?;
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    assert_eq!(
        complete_export(&mut tx, &store, gone_export, unique_png(), "a.png", "d5 worker").await.map(|_| ()),
        Err(ExportError::AuthorizationRevoked)
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn a_revocation_racing_the_recheck_waits_for_the_commit() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = new_world(&pool).await;
    let requester = member(&pool, &world, Some(Role::Editor)).await;
    let job = JobAuthorization { requested_by: requester, document: world.document, permission: JobPermission::DocumentEdit };

    // The job's commit transaction rechecks (and holds the decision rows) ...
    let mut commit = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    recheck_before_commit(&mut commit, &job).await?;
    // ... so a concurrent revocation cannot commit in between: it waits.
    let revoker = {
        let pool = pool.clone();
        let (tenant, workspace) = (world.tenant, world.workspace);
        tokio::spawn(async move {
            let mut tx = begin_tenant_transaction(&pool, tenant).await.unwrap();
            let removed = remove_workspace_member(&mut tx, workspace, requester).await.unwrap();
            tx.commit().await.unwrap();
            removed
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!revoker.is_finished(), "the revocation must wait for the job's commit");
    commit.commit().await?;
    assert!(tokio::time::timeout(Duration::from_secs(10), revoker).await??);

    // The next recheck sees the revocation.
    let mut again = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    assert_eq!(recheck_before_commit(&mut again, &job).await, Err(JobAuthorizationError::Revoked));
    again.rollback().await?;

    // Any job kind can be failed with the code.
    let job_id = Uuid::new_v4();
    let mut tx = begin_tenant_transaction(&pool, world.tenant).await.unwrap();
    sqlx::query("INSERT INTO jobs (id, tenant_id, kind, input) VALUES ($1::uuid, $2::uuid, 'd5.probe', '{}'::jsonb)")
        .bind(job_id.to_string())
        .bind(world.tenant.to_string())
        .execute(tx.connection())
        .await?;
    assert!(fail_job_authorization_revoked(&mut tx, job_id).await?);
    assert!(!fail_job_authorization_revoked(&mut tx, job_id).await?);
    tx.commit().await?;
    assert_eq!(job_state(&pool, world.tenant, job_id).await, ("failed".into(), Some(AUTHORIZATION_REVOKED.into())));
    Ok(())
}
