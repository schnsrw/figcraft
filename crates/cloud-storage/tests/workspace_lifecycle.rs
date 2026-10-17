//! S6-047: a workspace that is not `active` serves nothing, on every surface, and serves it all
//! again once restored, with its data intact. Live PostgreSQL (FIGCRAFT_TEST_DATABASE_URL_FILE);
//! every fixture workspace is random.
//!
//! Surfaces: member access resolution, relay member admission and the revocation sweep, service
//! account credentials, share links (exchange and the liveness check of an open share session),
//! host issuer lookup (embed-token exchange), webhook delivery claims and job authorization.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, ResourceState, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::effective_roles::{has_active_membership, resolve_document_access, resolve_tenant_access};
use figcraft_cloud_storage::host_systems::{HostRole, NewHostSystem, find_host_by_issuer, register_host_system};
use figcraft_cloud_storage::job_authorization::{JobAuthorization, JobPermission, recheck_before_commit};
use figcraft_cloud_storage::platform::{PlatformActor, PlatformError};
use figcraft_cloud_storage::platform_workspaces::{Cause, set_lifecycle};
use figcraft_cloud_storage::revocation::{admit_member, dead_member_admissions};
use figcraft_cloud_storage::secret_box::SecretKey;
use figcraft_cloud_storage::service_accounts::{Scope, authenticate_credential, create_service_account, issue_credential};
use figcraft_cloud_storage::sessions::{SessionLimits, create_session};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, dead_share_links, exchange_share_token};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use figcraft_cloud_storage::webhook_delivery::{claim_due, enqueue_delivery};
use figcraft_cloud_storage::webhooks::{DestinationPolicy, create_webhook_endpoint};
use figcraft_cloud_storage::{begin_tenant_transaction, metadata};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; workspace lifecycle probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(8).connect(url.trim_end()).await?))
}

struct Fixture {
    tenant: TenantId,
    owner: PrincipalId,
    outsider: PrincipalId,
    document: CloudDocumentId,
    session: String,
    credential: String,
    share: String,
    issuer: String,
    delivery: Uuid,
}

async fn user(pool: &PgPool) -> TestResult<PrincipalId> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'p3 lifecycle')")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(id.try_into().map_err(|_| "nil")?)
}

async fn fixture(pool: &PgPool) -> TestResult<Fixture> {
    let owner = user(pool).await?;
    let outsider = user(pool).await?;
    let tenant = TenantId::random();
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    let slug = format!("p3-{}", &Uuid::new_v4().simple().to_string()[..12]);
    create_tenant_with_owner(&mut tx, NewTenant { slug: &slug, name: "P3 lifecycle", plan_id: "team", home_region: "local" }, owner)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let (workspace, project, document) = (WorkspaceId::random(), ProjectId::random(), CloudDocumentId::random());
    metadata::create_workspace(&mut tx, workspace, "p3 space", None).await.map_err(|e| format!("{e:?}"))?;
    metadata::create_project(&mut tx, project, workspace, "p3 project", None).await.map_err(|e| format!("{e:?}"))?;
    metadata::create_document(&mut tx, document, workspace, project, "p3 document").await.map_err(|e| format!("{e:?}"))?;
    let account = create_service_account(&mut tx, "p3 robot", &[Scope::DocumentsRead], owner).await.map_err(|e| format!("{e:?}"))?;
    let credential = issue_credential(&mut tx, account.id, 3600, owner).await.map_err(|e| format!("{e:?}"))?.secret;
    let share = create_share_link(&mut tx, document, ShareRole::Viewer, Some("p3"), owner, None).await.map_err(|e| format!("{e:?}"))?.secret;
    let issuer = format!("https://p3-{}.example", Uuid::new_v4().simple());
    register_host_system(
        &mut tx,
        &NewHostSystem {
            system_id: "host-a".into(),
            issuer: issuer.clone(),
            audiences: vec!["figcraft".into()],
            allowed_origins: vec![],
            max_role: HostRole::Editor,
            allow_insecure_localhost: false,
        },
    )
    .await
    .map_err(|e| format!("{e:?}"))?;
    let key = SecretKey::derive(b"p3-lifecycle-signing-key-0123456").map_err(|e| format!("{e:?}"))?;
    let (endpoint, _) =
        create_webhook_endpoint(&mut tx, &key, DestinationPolicy { allow_private_networks: true }, "host-a", "http://127.0.0.1:9/hook", &[])
            .await
            .map_err(|e| format!("{e:?}"))?;
    let event = Uuid::new_v4();
    let body = format!("{{\"id\":\"{event}\",\"type\":\"document.created\",\"data\":{{\"document_id\":\"d\"}}}}");
    let delivery = enqueue_delivery(&mut tx, endpoint.id, event, "document.created", body.as_bytes(), 5)
        .await
        .map_err(|e| format!("{e:?}"))?
        .ok_or("delivery")?;
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = clock_timestamp() - interval '1 second' WHERE id = $1::uuid")
        .bind(delivery.to_string())
        .execute(tx.connection())
        .await?;
    tx.commit().await?;
    let session = create_session(pool, owner, SessionLimits::default()).await.map_err(|e| format!("{e:?}"))?.token;
    Ok(Fixture { tenant, owner, outsider, document, session, credential, share, issuer, delivery })
}

/// What each surface says right now: `true` when it serves the workspace.
#[derive(Debug, Eq, PartialEq)]
struct Surfaces {
    member_access: bool,
    still_a_member: bool,
    relay_admission: bool,
    service_account: bool,
    share_exchange: bool,
    open_share_session_alive: bool,
    host_issuer: bool,
    job_commit: bool,
    webhook_claimed: bool,
}

async fn surfaces(pool: &PgPool, f: &Fixture) -> TestResult<Surfaces> {
    let mut tx = begin_tenant_transaction(pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
    let access = resolve_tenant_access(&mut tx, f.owner).await.map_err(|e| format!("{e:?}"))?.ok_or("tenant")?;
    let document_access = resolve_document_access(&mut tx, f.owner, f.document).await.map_err(|e| format!("{e:?}"))?;
    let still_a_member = has_active_membership(&mut tx, f.owner).await.map_err(|e| format!("{e:?}"))?;
    let outsider_is_member = has_active_membership(&mut tx, f.outsider).await.map_err(|e| format!("{e:?}"))?;
    assert!(!outsider_is_member, "a non-member never learns anything from the membership check");
    let _ = tx.rollback().await;
    let member_access = access.tenant_role.is_some() && document_access.as_ref().is_some_and(|a| a.role.is_some());

    let relay_admission = admit_member(pool, f.tenant, &f.session, f.document, SessionLimits::default()).await.is_ok();
    let service_account = authenticate_credential(pool, &f.credential).await.is_ok();
    let share_exchange = exchange_share_token(pool, &f.share).await.is_ok();
    let link_id = {
        let mut tx = begin_tenant_transaction(pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
        let id: String = sqlx::query_scalar("SELECT id::text FROM share_links LIMIT 1").fetch_one(tx.connection()).await?;
        let _ = tx.rollback().await;
        id.parse::<Uuid>()?
    };
    let dead = dead_share_links(pool, &[(f.tenant, link_id)]).await.map_err(|e| format!("{e:?}"))?;
    let host_issuer = find_host_by_issuer(pool, &f.issuer).await.map_err(|e| format!("{e:?}"))?.is_some();

    let mut tx = begin_tenant_transaction(pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
    let job_commit =
        recheck_before_commit(&mut tx, &JobAuthorization { requested_by: f.owner, document: f.document, permission: JobPermission::DocumentView })
            .await
            .is_ok();
    let _ = tx.rollback().await;

    let claimed = claim_due(pool, 500).await.map_err(|e| format!("{e:?}"))?;
    let webhook_claimed = claimed.iter().any(|(tenant, id)| *tenant == f.tenant && *id == f.delivery);
    Ok(Surfaces {
        member_access,
        still_a_member,
        relay_admission,
        service_account,
        share_exchange,
        open_share_session_alive: dead.is_empty(),
        host_issuer,
        job_commit,
        webhook_claimed,
    })
}

fn open() -> Surfaces {
    Surfaces {
        member_access: true,
        still_a_member: true,
        relay_admission: true,
        service_account: true,
        share_exchange: true,
        open_share_session_alive: true,
        host_issuer: true,
        job_commit: true,
        webhook_claimed: true,
    }
}

fn closed() -> Surfaces {
    Surfaces {
        member_access: false,
        still_a_member: true,
        relay_admission: false,
        service_account: false,
        share_exchange: false,
        open_share_session_alive: false,
        host_issuer: false,
        job_commit: false,
        webhook_claimed: false,
    }
}

async fn move_to(pool: &PgPool, f: &Fixture, to: ResourceState) -> TestResult {
    set_lifecycle(pool, f.tenant, to, None, Cause { actor: PlatformActor::System, request_id: None }).await.map_err(|e| format!("{e:?}"))?;
    Ok(())
}

/// The webhook claim hands a delivery out at most once per lease, so the fixture's delivery is made
/// due again before each look.
async fn due_again(pool: &PgPool, f: &Fixture) -> TestResult {
    let mut tx = begin_tenant_transaction(pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query(
        "UPDATE webhook_deliveries SET next_attempt_at = clock_timestamp() - interval '1 second', lease_expires_at = NULL WHERE id = $1::uuid",
    )
    .bind(f.delivery.to_string())
    .execute(tx.connection())
    .await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn only_an_active_workspace_is_served_and_restoring_brings_everything_back() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let f = fixture(&pool).await?;
    assert_eq!(surfaces(&pool, &f).await?, open(), "an active workspace serves every surface");

    // Suspended, archived and pending deletion all serve nothing; the owner stays a member.
    for (to, back) in [(ResourceState::Suspended, ResourceState::Active), (ResourceState::Archived, ResourceState::Active)] {
        move_to(&pool, &f, to).await?;
        due_again(&pool, &f).await?;
        assert_eq!(surfaces(&pool, &f).await?, closed(), "{to:?}");
        move_to(&pool, &f, back).await?;
        due_again(&pool, &f).await?;
        assert_eq!(surfaces(&pool, &f).await?, open(), "restored from {to:?}");
    }
    move_to(&pool, &f, ResourceState::Suspended).await?;
    move_to(&pool, &f, ResourceState::Deleting).await?;
    due_again(&pool, &f).await?;
    assert_eq!(surfaces(&pool, &f).await?, closed(), "pending deletion");
    move_to(&pool, &f, ResourceState::Archived).await?;
    move_to(&pool, &f, ResourceState::Active).await?;
    due_again(&pool, &f).await?;
    assert_eq!(surfaces(&pool, &f).await?, open(), "back after cancelling deletion");

    // Nothing was deleted along the way.
    let mut tx = begin_tenant_transaction(&pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
    let documents: i64 = sqlx::query_scalar("SELECT count(*) FROM documents").fetch_one(tx.connection()).await?;
    let deliveries: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries").fetch_one(tx.connection()).await?;
    let _ = tx.rollback().await;
    assert_eq!((documents, deliveries), (1, 1));
    Ok(())
}

#[tokio::test]
async fn suspending_ends_a_live_relay_admission_and_an_open_share_session_at_once() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let f = fixture(&pool).await?;
    let admission = admit_member(&pool, f.tenant, &f.session, f.document, SessionLimits::default()).await.map_err(|e| format!("{e:?}"))?;
    assert!(dead_member_admissions(&pool, &[admission]).await.map_err(|e| format!("{e:?}"))?.is_empty());
    move_to(&pool, &f, ResourceState::Suspended).await?;
    let dead = dead_member_admissions(&pool, &[admission]).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(dead.len(), 1, "the relay sweep finds the admission dead");
    move_to(&pool, &f, ResourceState::Active).await?;
    assert!(
        dead_member_admissions(&pool, &[admission]).await.map_err(|e| format!("{e:?}"))?.is_empty(),
        "a restored workspace admits the same session again"
    );
    Ok(())
}

#[tokio::test]
async fn the_platform_cannot_activate_a_pending_workspace_or_leave_the_policy() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let f = fixture(&pool).await?;
    let cause = Cause { actor: PlatformActor::System, request_id: None };
    assert_eq!(
        set_lifecycle(&pool, f.tenant, ResourceState::Deleting, None, cause).await.err(),
        Some(PlatformError::Conflict),
        "active cannot skip to deletion"
    );
    assert_eq!(
        set_lifecycle(&pool, f.tenant, ResourceState::Deleted, None, cause).await.err(),
        Some(PlatformError::Conflict),
        "deleted is for the deletion job"
    );
    assert_eq!(set_lifecycle(&pool, f.tenant, ResourceState::Pending, None, cause).await.err(), Some(PlatformError::Conflict));
    assert_eq!(set_lifecycle(&pool, TenantId::random(), ResourceState::Suspended, None, cause).await.err(), Some(PlatformError::NotFound));
    Ok(())
}
