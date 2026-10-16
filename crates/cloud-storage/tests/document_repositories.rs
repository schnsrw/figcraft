use figcraft_cloud_domain::{CloudDocumentId, ProjectId, ResourceState, TenantId, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::{
    RepositoryError, archive_document, create_document, get_document, rename_document, restore_document, soft_delete_document,
};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn document_repositories_drive_lifecycle_transitions_and_scope_reads() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL document repository probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let tenant_e: TenantId = "e0000000-0000-4000-8000-000000000001".parse()?;
    let tenant_f: TenantId = "f0000000-0000-4000-8000-000000000001".parse()?;
    let workspace_e1: WorkspaceId = "e1000000-0000-4000-8000-000000000001".parse()?;
    let workspace_f1: WorkspaceId = "f1000000-0000-4000-8000-000000000001".parse()?;
    let project_e1: ProjectId = "e2000000-0000-4000-8000-000000000001".parse()?;
    let doc_one: CloudDocumentId = "e3000000-0000-4000-8000-000000000001".parse()?;
    let doc_two: CloudDocumentId = "e3000000-0000-4000-8000-000000000002".parse()?;
    let doc_three: CloudDocumentId = "e3000000-0000-4000-8000-000000000003".parse()?;
    let doc_four: CloudDocumentId = "e3000000-0000-4000-8000-000000000004".parse()?;

    // A constraint violation aborts its Postgres transaction, so the wrong-parent check gets its
    // own short-lived transaction rather than sharing one with later successful calls.
    let mut wrong_parent = begin_tenant_transaction(&pool, tenant_e).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(
        create_document(&mut wrong_parent, CloudDocumentId::random(), workspace_f1, project_e1, "Wrong parent").await,
        Err(RepositoryError::Constraint)
    );
    wrong_parent.rollback().await?;

    let mut scoped = begin_tenant_transaction(&pool, tenant_e).await.map_err(|error| format!("begin tenant: {error:?}"))?;

    // create: success and read scoping.
    let new_document = CloudDocumentId::random();
    let created = create_document(&mut scoped, new_document, workspace_e1, project_e1, "New document").await?;
    assert_eq!(created.id, new_document);
    assert_eq!(created.tenant_id, tenant_e);
    assert_eq!(created.state, ResourceState::Active);
    assert_eq!(get_document(&mut scoped, new_document).await?, Some(created));
    assert_eq!(get_document(&mut scoped, CloudDocumentId::random()).await?, None);

    // doc_one: active -> renamed -> archived -> archived (idempotent) -> renamed while archived
    // -> active (restored) -> active (idempotent) -> deleting (soft-deleted) -> deleting
    // (idempotent) -> rejected rename/archive while deleting -> active (restored again).
    let renamed = rename_document(&mut scoped, doc_one, "Renamed one").await?.ok_or("doc_one should exist")?;
    assert_eq!(renamed.name, "Renamed one");
    assert_eq!(renamed.state, ResourceState::Active);

    let archived = archive_document(&mut scoped, doc_one).await?.ok_or("doc_one should exist")?;
    assert_eq!(archived.state, ResourceState::Archived);
    assert_eq!(archive_document(&mut scoped, doc_one).await?, Some(archived.clone()));

    let renamed_while_archived = rename_document(&mut scoped, doc_one, "Renamed again").await?.ok_or("doc_one should exist")?;
    assert_eq!(renamed_while_archived.name, "Renamed again");
    assert_eq!(renamed_while_archived.state, ResourceState::Archived);

    let restored = restore_document(&mut scoped, doc_one).await?.ok_or("doc_one should exist")?;
    assert_eq!(restored.state, ResourceState::Active);
    assert_eq!(restore_document(&mut scoped, doc_one).await?, Some(restored));

    let deleting = soft_delete_document(&mut scoped, doc_one).await?.ok_or("doc_one should exist")?;
    assert_eq!(deleting.state, ResourceState::Deleting);
    assert_eq!(soft_delete_document(&mut scoped, doc_one).await?, Some(deleting));
    assert_eq!(rename_document(&mut scoped, doc_one, "Should not apply").await, Err(RepositoryError::InvalidState));
    assert_eq!(archive_document(&mut scoped, doc_one).await, Err(RepositoryError::InvalidState));
    let restored_again = restore_document(&mut scoped, doc_one).await?.ok_or("doc_one should exist")?;
    assert_eq!(restored_again.state, ResourceState::Active);

    // doc_two: seeded archived; restore returns it to active directly.
    let doc_two_restored = restore_document(&mut scoped, doc_two).await?.ok_or("doc_two should exist")?;
    assert_eq!(doc_two_restored.state, ResourceState::Active);

    // doc_three: seeded mid grace-period; soft-delete is idempotent, restore clears it.
    let doc_three_deleting = soft_delete_document(&mut scoped, doc_three).await?.ok_or("doc_three should exist")?;
    assert_eq!(doc_three_deleting.state, ResourceState::Deleting);
    let doc_three_restored = restore_document(&mut scoped, doc_three).await?.ok_or("doc_three should exist")?;
    assert_eq!(doc_three_restored.state, ResourceState::Active);

    // doc_four: seeded in the terminal 'deleted' state; no transition may move it.
    assert_eq!(rename_document(&mut scoped, doc_four, "Should not apply").await, Err(RepositoryError::InvalidState));
    assert_eq!(archive_document(&mut scoped, doc_four).await, Err(RepositoryError::InvalidState));
    assert_eq!(restore_document(&mut scoped, doc_four).await, Err(RepositoryError::InvalidState));
    assert_eq!(soft_delete_document(&mut scoped, doc_four).await, Err(RepositoryError::InvalidState));

    // Missing documents resolve to None rather than an error for every transition.
    let missing = CloudDocumentId::random();
    assert_eq!(rename_document(&mut scoped, missing, "Ghost").await?, None);
    assert_eq!(archive_document(&mut scoped, missing).await?, None);
    assert_eq!(restore_document(&mut scoped, missing).await?, None);
    assert_eq!(soft_delete_document(&mut scoped, missing).await?, None);

    scoped.rollback().await?;

    // A document created under tenant_e is invisible to a transaction scoped to tenant_f.
    let mut other_tenant = begin_tenant_transaction(&pool, tenant_f).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(get_document(&mut other_tenant, doc_one).await?, None);
    other_tenant.rollback().await?;

    pool.close().await;
    Ok(())
}
