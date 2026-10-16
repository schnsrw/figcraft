use figcraft_cloud_domain::{CloudDocumentId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::snapshots::{RepositoryError, register_snapshot, reject_snapshot, set_current_snapshot, verify_snapshot};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn snapshot_repository_registers_and_transitions_snapshot_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL snapshot repository probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let tenant: TenantId = "93000000-0000-4000-8000-000000000001".parse()?;
    let document_one: CloudDocumentId = "93300000-0000-4000-8000-000000000001".parse()?;
    let document_two: CloudDocumentId = "93300000-0000-4000-8000-000000000002".parse()?;
    let checksum = [0x11_u8; 32];

    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;

    // register: success, starts pending, carries through the values it was given.
    let snapshot_one = Uuid::new_v4();
    let object_one = Uuid::new_v4();
    let registered = register_snapshot(&mut scoped, snapshot_one, document_one, 1, object_one, &checksum, 1024).await?;
    assert_eq!(registered.id, snapshot_one);
    assert_eq!(registered.tenant_id, tenant);
    assert_eq!(registered.document_id, document_one);
    assert_eq!(registered.seq, 1);
    assert_eq!(registered.object_id, object_one);
    assert_eq!(registered.size_bytes, 1024);
    assert_eq!(registered.state, "pending");

    // verify: pending -> verified, then idempotent on a second call.
    let verified = verify_snapshot(&mut scoped, snapshot_one, document_one).await?.ok_or("snapshot_one should exist")?;
    assert_eq!(verified.state, "verified");
    assert_eq!(verify_snapshot(&mut scoped, snapshot_one, document_one).await?, Some(verified.clone()));

    // verify: a nonexistent snapshot id resolves to None, not an error.
    assert_eq!(verify_snapshot(&mut scoped, Uuid::new_v4(), document_one).await?, None);

    // A second snapshot on the same document: reject it, confirm rejection is idempotent, and that
    // verifying it afterward is rejected as an invalid-state transition (a rejected snapshot cannot
    // be un-rejected by verifying it).
    let snapshot_two = Uuid::new_v4();
    let object_two = Uuid::new_v4();
    let registered_two = register_snapshot(&mut scoped, snapshot_two, document_one, 2, object_two, &checksum, 2048).await?;
    assert_eq!(registered_two.state, "pending");
    let rejected = reject_snapshot(&mut scoped, snapshot_two, document_one).await?.ok_or("snapshot_two should exist")?;
    assert_eq!(rejected.state, "rejected");
    assert_eq!(reject_snapshot(&mut scoped, snapshot_two, document_one).await?, Some(rejected.clone()));
    assert_eq!(verify_snapshot(&mut scoped, snapshot_two, document_one).await, Err(RepositoryError::InvalidState));

    // set_current_snapshot: a snapshot that belongs to this exact document succeeds.
    set_current_snapshot(&mut scoped, document_one, snapshot_one).await?;

    scoped.rollback().await?;

    // register: an unknown document_id is rejected by the foreign key as Constraint. A constraint
    // violation aborts its Postgres transaction, so this gets its own short-lived transaction
    // rather than sharing one with other assertions.
    let mut wrong_document = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(
        register_snapshot(&mut wrong_document, Uuid::new_v4(), CloudDocumentId::random(), 1, Uuid::new_v4(), &checksum, 1).await,
        Err(RepositoryError::Constraint)
    );
    wrong_document.rollback().await?;

    // set_current_snapshot: a snapshot that exists but belongs to a *different* document is
    // rejected by the composite foreign key on `documents.current_snapshot_id`, not silently
    // accepted. The insert and the violating update share one transaction (a row inserted earlier
    // in a transaction is visible to later statements in that same transaction without a commit);
    // the violation still aborts the whole transaction, so it gets its own short-lived one.
    let mut cross_document = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let snapshot_on_document_two = Uuid::new_v4();
    register_snapshot(&mut cross_document, snapshot_on_document_two, document_two, 1, Uuid::new_v4(), &checksum, 1).await?;
    assert_eq!(set_current_snapshot(&mut cross_document, document_one, snapshot_on_document_two).await, Err(RepositoryError::Constraint));
    cross_document.rollback().await?;

    pool.close().await;
    Ok(())
}
