use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::external_mappings::{RepositoryError, create_external_mapping, get_external_mapping};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn external_mapping_repository_scopes_reads_and_enforces_natural_key_uniqueness() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL external mapping repository probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let tenant: TenantId = "90000000-0000-4000-8000-000000000001".parse()?;
    let internal_id = Uuid::new_v4();

    // create: success and tenant scoping. This is committed (not rolled back) because the
    // cross-tenant check below must observe it from an independent transaction, which transaction
    // isolation alone would otherwise make vacuously pass with no mapping ever created.
    // `deploy/saas/external-mapping-repository-cleanup.sql` removes any mapping left under this
    // tenant after the test run.
    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let created = create_external_mapping(&mut scoped, "host-example", "document", "ext-doc-1", internal_id).await?;
    assert_eq!(created.tenant_id, tenant);
    assert_eq!(created.system_id, "host-example");
    assert_eq!(created.resource_kind, "document");
    assert_eq!(created.external_id, "ext-doc-1");
    assert_eq!(created.internal_id, internal_id);
    assert_eq!(get_external_mapping(&mut scoped, "host-example", "document", "ext-doc-1").await?, Some(created.clone()));
    assert_eq!(get_external_mapping(&mut scoped, "host-example", "document", "missing-id").await?, None);
    scoped.commit().await?;

    // create: a duplicate (tenant_id, system_id, resource_kind, external_id) natural key is
    // rejected as Conflict. A constraint violation aborts its Postgres transaction, so this check
    // gets its own short-lived transaction rather than sharing one with other assertions.
    let mut duplicate = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(
        create_external_mapping(&mut duplicate, "host-example", "document", "ext-doc-1", Uuid::new_v4()).await,
        Err(RepositoryError::Conflict)
    );
    duplicate.rollback().await?;

    // get: the mapping committed under `tenant` is invisible to a transaction scoped to a
    // different tenant.
    let mut other_tenant = begin_tenant_transaction(&pool, TenantId::random()).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(get_external_mapping(&mut other_tenant, "host-example", "document", "ext-doc-1").await?, None);
    other_tenant.rollback().await?;

    // get: the mapping remains visible under its own tenant after commit.
    let mut reread = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(get_external_mapping(&mut reread, "host-example", "document", "ext-doc-1").await?, Some(created));
    reread.rollback().await?;

    pool.close().await;
    Ok(())
}
