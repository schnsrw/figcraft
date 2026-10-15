use figcraft_cloud_domain::TenantId;
use figcraft_cloud_storage::begin_tenant_transaction;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn tenant_context_is_transaction_local_on_a_reused_connection() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL tenant-context integration probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let database_url = database_url.trim_end();
    let pool = PgPoolOptions::new().max_connections(1).connect(database_url).await?;
    let tenant_a: TenantId = "a0000000-0000-4000-8000-000000000001".parse()?;
    let tenant_b: TenantId = "b0000000-0000-4000-8000-000000000002".parse()?;
    let initial_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&pool).await?;
    assert!(visible_probe_rows(&pool).await?.is_empty());

    let mut first = begin_tenant_transaction(&pool, tenant_a).await.map_err(|error| format!("tenant transaction setup failed: {error:?}"))?;
    let first_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(first.connection()).await?;
    assert_eq!(first_backend, initial_backend);
    assert_eq!(tenant_setting(&mut first).await?, Some(tenant_a.to_string()));
    let first_rows: Vec<String> =
        sqlx::query_scalar("SELECT label FROM public.tenant_context_probe ORDER BY label").fetch_all(first.connection()).await?;
    assert_eq!(first_rows, ["tenant-a".to_owned()]);
    first.commit().await?;
    assert!(visible_probe_rows(&pool).await?.is_empty());

    let mut second = begin_tenant_transaction(&pool, tenant_b).await.map_err(|error| format!("tenant transaction setup failed: {error:?}"))?;
    let second_backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(second.connection()).await?;
    assert_eq!(second_backend, initial_backend);
    assert_eq!(tenant_setting(&mut second).await?, Some(tenant_b.to_string()));
    let second_rows: Vec<String> =
        sqlx::query_scalar("SELECT label FROM public.tenant_context_probe ORDER BY label").fetch_all(second.connection()).await?;
    assert_eq!(second_rows, ["tenant-b".to_owned()]);
    second.rollback().await?;
    assert!(visible_probe_rows(&pool).await?.is_empty());

    let mut dropped = begin_tenant_transaction(&pool, tenant_a).await.map_err(|error| format!("tenant transaction setup failed: {error:?}"))?;
    assert_eq!(tenant_setting(&mut dropped).await?, Some(tenant_a.to_string()));
    drop(dropped);
    assert!(visible_probe_rows(&pool).await?.is_empty());

    pool.close().await;
    Ok(())
}

async fn visible_probe_rows(pool: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT label FROM public.tenant_context_probe ORDER BY label").fetch_all(pool).await
}

async fn tenant_setting(transaction: &mut figcraft_cloud_storage::TenantTransaction<'_>) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT NULLIF(current_setting('figcraft.tenant_id', true), '')").fetch_one(transaction.connection()).await
}
