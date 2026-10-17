//! S5 PostgreSQL lease races; set FIGCRAFT_TEST_DATABASE_URL_FILE to a migrated runtime URL.

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::claim_sequencer;
use figcraft_cloud_storage::room_leases::{AcquireRoom, RoomLeaseError, acquire_room, current_room, release_room, renew_room};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; room lease probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(5).connect(url.trim_end()).await?))
}

async fn fixture(pool: &PgPool) -> TestResult<(TenantId, CloudDocumentId)> {
    let tenant = TenantId::random();
    let workspace = WorkspaceId::random();
    let project = ProjectId::random();
    let document = CloudDocumentId::random();
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'lease probe', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("lease-{}", tenant.as_uuid().simple()))
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'lease probe')")
        .bind(workspace.to_string())
        .bind(tenant.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'lease probe')")
        .bind(project.to_string())
        .bind(tenant.to_string())
        .bind(workspace.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'lease probe')",
    )
    .bind(document.to_string())
    .bind(tenant.to_string())
    .bind(workspace.to_string())
    .bind(project.to_string())
    .execute(tx.connection())
    .await?;
    tx.commit().await?;
    Ok((tenant, document))
}

async fn claim(pool: &PgPool, tenant: TenantId, document: CloudDocumentId, owner: Uuid) -> TestResult<AcquireRoom> {
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let result = acquire_room(&mut tx, document, owner, "wss://relay.example/ws/v1/collab", 30).await?;
    tx.commit().await?;
    Ok(result)
}

#[tokio::test]
async fn lease_claim_renew_expire_and_stale_release() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let (tenant, document) = fixture(&pool).await?;
    let first_owner = Uuid::new_v4();
    let second_owner = Uuid::new_v4();
    let first = match claim(&pool, tenant, document, first_owner).await? {
        AcquireRoom::Acquired(lease) => lease,
        AcquireRoom::Held(_) => return Err("new document was already owned".into()),
    };
    assert_eq!(first.fencing_token, 1);
    assert_eq!(claim(&pool, tenant, document, second_owner).await?, AcquireRoom::Held(first.clone()));

    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert!(!renew_room(&mut tx, document, second_owner, first.fencing_token, 30).await?);
    assert!(!release_room(&mut tx, document, second_owner, first.fencing_token).await?);
    assert!(renew_room(&mut tx, document, first_owner, first.fencing_token, 30).await?);
    tx.commit().await?;

    // The database clock and an explicit expiry simulate a paused owner without sleeping.
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("UPDATE room_leases SET expires_at = clock_timestamp() - interval '1 second' WHERE tenant_id = $1::uuid AND document_id = $2::uuid")
        .bind(tenant.to_string())
        .bind(document.to_string())
        .execute(tx.connection())
        .await?;
    assert!(current_room(&mut tx, document).await?.is_none());
    assert!(!renew_room(&mut tx, document, first_owner, first.fencing_token, 30).await?);
    tx.commit().await?;

    let second = match claim(&pool, tenant, document, second_owner).await? {
        AcquireRoom::Acquired(lease) => lease,
        AcquireRoom::Held(_) => return Err("expired room was not reclaimed".into()),
    };
    assert!(second.fencing_token > first.fencing_token);
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert!(!release_room(&mut tx, document, first_owner, first.fencing_token).await?);
    assert_eq!(current_room(&mut tx, document).await?, Some(second.clone()));
    assert!(release_room(&mut tx, document, second_owner, second.fencing_token).await?);
    tx.commit().await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn simultaneous_claims_have_one_winner_and_tenant_scope() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let (tenant, document) = fixture(&pool).await?;
    let owner_a = Uuid::new_v4();
    let owner_b = Uuid::new_v4();
    let (a, b) = tokio::join!(claim(&pool, tenant, document, owner_a), claim(&pool, tenant, document, owner_b));
    let a = a?;
    let b = b?;
    assert!(matches!((&a, &b), (AcquireRoom::Acquired(_), AcquireRoom::Held(_)) | (AcquireRoom::Held(_), AcquireRoom::Acquired(_))));
    let (winner, other) = match (a, b) {
        (AcquireRoom::Acquired(winner), AcquireRoom::Held(other)) | (AcquireRoom::Held(other), AcquireRoom::Acquired(winner)) => (winner, other),
        _ => return Err("simultaneous acquisitions did not serialize".into()),
    };
    assert_eq!(winner, other);
    assert_eq!(winner.fencing_token, 1);

    let mut foreign = begin_tenant_transaction(&pool, TenantId::random()).await.map_err(|error| format!("{error:?}"))?;
    assert!(current_room(&mut foreign, document).await?.is_none());
    assert_eq!(acquire_room(&mut foreign, document, Uuid::new_v4(), "wss://relay.example/ws", 30).await, Err(RoomLeaseError::NotFound));
    foreign.rollback().await?;

    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(acquire_room(&mut tx, document, Uuid::new_v4(), "wss://relay.example/ws?secret=x", 30).await, Err(RoomLeaseError::InvalidInput));
    assert_eq!(acquire_room(&mut tx, document, Uuid::new_v4(), "wss://relay.example/ws", 121).await, Err(RoomLeaseError::InvalidInput));
    tx.rollback().await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_superseded_fence_invalidates_an_unexpired_lease() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let (tenant, document) = fixture(&pool).await?;
    let owner = Uuid::new_v4();
    let first = match claim(&pool, tenant, document, owner).await? {
        AcquireRoom::Acquired(lease) => lease,
        AcquireRoom::Held(_) => return Err("new document was already owned".into()),
    };
    // A legacy sequencer claim can still advance the token during the migration to leased
    // rooms. Neither lookup nor renewal may treat the stale lease as live afterward.
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let advanced = claim_sequencer(&mut tx, document).await?;
    tx.commit().await?;
    assert!(advanced.fencing_token > first.fencing_token);

    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert!(current_room(&mut tx, document).await?.is_none());
    assert!(!renew_room(&mut tx, document, owner, first.fencing_token, 30).await?);
    tx.commit().await?;
    let takeover = claim(&pool, tenant, document, Uuid::new_v4()).await?;
    assert!(matches!(takeover, AcquireRoom::Acquired(_)));
    pool.close().await;
    Ok(())
}
