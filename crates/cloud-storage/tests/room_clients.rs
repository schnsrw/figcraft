//! S5-008 PostgreSQL room clients; set FIGCRAFT_TEST_DATABASE_URL_FILE to a migrated runtime URL.

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::room_clients::{MAX_RECORD_BATCH, MAX_REMEMBERED, RoomClientsError, load_and_trim, record_issued};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; room clients probe skipped");
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
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'clients probe', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("clients-{}", tenant.as_uuid().simple()))
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'clients probe')")
        .bind(workspace.to_string())
        .bind(tenant.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'clients probe')")
        .bind(project.to_string())
        .bind(tenant.to_string())
        .bind(workspace.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'clients probe')",
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

async fn record(pool: &PgPool, tenant: TenantId, document: CloudDocumentId, ids: &[u32]) -> TestResult<Result<(), RoomClientsError>> {
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let result = record_issued(&mut tx, document, ids).await;
    tx.commit().await?;
    Ok(result)
}

async fn load(pool: &PgPool, tenant: TenantId, document: CloudDocumentId) -> TestResult<Vec<u32>> {
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let ids = load_and_trim(&mut tx, document).await?;
    tx.commit().await?;
    Ok(ids)
}

#[tokio::test]
async fn issued_ids_are_recorded_once_in_order_and_isolated_by_tenant() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let (tenant, document) = fixture(&pool).await?;
    let (other_tenant, other_document) = fixture(&pool).await?;
    assert_eq!(load(&pool, tenant, document).await?, Vec::<u32>::new());
    record(&pool, tenant, document, &[5, 3, 4]).await??;
    // Recording again, with overlap, keeps one row per id.
    record(&pool, tenant, document, &[4, 6, u32::MAX, 0]).await??;
    record(&pool, tenant, document, &[]).await??;
    assert_eq!(load(&pool, tenant, document).await?, vec![0, 3, 4, 5, 6, u32::MAX]);
    // Another tenant's document is separate, and cannot be reached through this one.
    record(&pool, other_tenant, other_document, &[9]).await??;
    assert_eq!(load(&pool, other_tenant, other_document).await?, vec![9]);
    assert_eq!(load(&pool, tenant, other_document).await?, Vec::<u32>::new(), "row-level security: tenant A sees nothing of B's document");
    assert_eq!(load(&pool, other_tenant, document).await?, Vec::<u32>::new());
    // One call records a bounded number of ids.
    let too_many: Vec<u32> = (0..u32::try_from(MAX_RECORD_BATCH)? + 1).collect();
    assert_eq!(record(&pool, tenant, document, &too_many).await?, Err(RoomClientsError::TooMany));
    Ok(())
}

#[tokio::test]
async fn only_the_newest_remembered_ids_are_kept() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let (tenant, document) = fixture(&pool).await?;
    let total = u32::try_from(MAX_REMEMBERED)? + 100;
    let mut next = 1u32;
    while next <= total {
        let end = (next + u32::try_from(MAX_RECORD_BATCH)? - 1).min(total);
        let ids: Vec<u32> = (next..=end).collect();
        record(&pool, tenant, document, &ids).await??;
        next = end + 1;
    }
    let kept = load(&pool, tenant, document).await?;
    assert_eq!(kept.len(), MAX_REMEMBERED);
    assert_eq!((kept.first().copied(), kept.last().copied()), (Some(101), Some(total)), "the oldest 100 were dropped");
    Ok(())
}
