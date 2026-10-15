use figcraft_cloud_domain::{PrincipalId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::{
    PageSize, RepositoryError, create_project, create_tenant, create_workspace, get_project, get_tenant, get_workspace, list_projects,
    list_tenants_for_principal, list_workspaces,
};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn repositories_scope_reads_and_bound_keyset_pages() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL metadata repository probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let tenant_a: TenantId = "a0000000-0000-4000-8000-000000000001".parse()?;
    let tenant_b: TenantId = "b0000000-0000-4000-8000-000000000002".parse()?;
    let tenant_d: TenantId = "d0000000-0000-4000-8000-000000000004".parse()?;
    let principal: PrincipalId = "c0000000-0000-4000-8000-000000000001".parse()?;
    let workspace_a1: WorkspaceId = "a1000000-0000-4000-8000-000000000001".parse()?;
    let workspace_a2: WorkspaceId = "a1000000-0000-4000-8000-000000000002".parse()?;
    let workspace_b1: WorkspaceId = "b1000000-0000-4000-8000-000000000001".parse()?;
    let project_a1: ProjectId = "a2000000-0000-4000-8000-000000000001".parse()?;
    let project_b1: ProjectId = "b2000000-0000-4000-8000-000000000001".parse()?;
    let size_one = PageSize::new(1)?;
    let size_max = PageSize::new(100)?;

    let first_tenants = list_tenants_for_principal(&pool, principal, None, size_one).await?;
    assert_eq!(first_tenants.items.len(), 1);
    assert_eq!(first_tenants.items[0].id, tenant_a);
    let second_tenants = list_tenants_for_principal(&pool, principal, first_tenants.next_cursor, size_one).await?;
    assert_eq!(second_tenants.items.len(), 1);
    assert_eq!(second_tenants.items[0].id, tenant_b);
    assert_eq!(second_tenants.next_cursor, None);
    assert_eq!(list_tenants_for_principal(&pool, PrincipalId::random(), None, size_max).await?.items.len(), 0);

    let mut tenant_transaction = begin_tenant_transaction(&pool, tenant_d).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let created_tenant = create_tenant(&mut tenant_transaction, "repo-d", "Repository D", "test", "local").await?;
    assert_eq!(created_tenant.id, tenant_d);
    assert_eq!(get_tenant(&mut tenant_transaction).await?, Some(created_tenant));
    tenant_transaction.rollback().await?;

    let mut duplicate = begin_tenant_transaction(&pool, tenant_d).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(create_tenant(&mut duplicate, "repo-a", "Duplicate slug", "test", "local").await, Err(RepositoryError::Conflict));
    duplicate.rollback().await?;

    let mut scoped = begin_tenant_transaction(&pool, tenant_a).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(get_tenant(&mut scoped).await?.map(|record| record.id), Some(tenant_a));
    assert_eq!(get_workspace(&mut scoped, workspace_b1).await?, None);
    assert_eq!(get_project(&mut scoped, workspace_b1, project_b1).await?, None);
    assert_eq!(get_project(&mut scoped, workspace_a2, project_a1).await?, None);
    assert_eq!(get_project(&mut scoped, workspace_a1, project_a1).await?.map(|record| record.id), Some(project_a1));

    let new_workspace = WorkspaceId::random();
    let workspace = create_workspace(&mut scoped, new_workspace, "New workspace", None).await?;
    assert_eq!(workspace.tenant_id, tenant_a);
    assert_eq!(get_workspace(&mut scoped, new_workspace).await?, Some(workspace));
    let new_project = ProjectId::random();
    let project = create_project(&mut scoped, new_project, new_workspace, "New project", None).await?;
    assert_eq!(project.tenant_id, tenant_a);
    assert_eq!(get_project(&mut scoped, new_workspace, new_project).await?, Some(project));

    let mut workspace_cursor = None;
    let mut workspace_ids = Vec::new();
    loop {
        let page = list_workspaces(&mut scoped, workspace_cursor, size_max).await?;
        assert!(page.items.len() <= 100);
        assert!(page.items.iter().all(|item| item.tenant_id == tenant_a));
        workspace_ids.extend(page.items.into_iter().map(|item| item.id));
        match page.next_cursor {
            Some(cursor) => workspace_cursor = Some(cursor),
            None => break,
        }
    }
    assert_eq!(workspace_ids.len(), 108);
    assert!(workspace_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!workspace_ids.contains(&workspace_b1));

    let mut project_cursor = None;
    let mut project_ids = Vec::new();
    loop {
        let page = list_projects(&mut scoped, workspace_a1, project_cursor, size_max).await?;
        assert!(page.items.len() <= 100);
        assert!(page.items.iter().all(|item| item.tenant_id == tenant_a && item.workspace_id == workspace_a1));
        project_ids.extend(page.items.into_iter().map(|item| item.id));
        match page.next_cursor {
            Some(cursor) => project_cursor = Some(cursor),
            None => break,
        }
    }
    assert_eq!(project_ids.len(), 106);
    assert!(project_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!project_ids.contains(&project_b1));
    scoped.rollback().await?;

    let mut wrong_parent = begin_tenant_transaction(&pool, tenant_a).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(create_project(&mut wrong_parent, ProjectId::random(), workspace_b1, "Wrong tenant", None).await, Err(RepositoryError::Constraint));
    wrong_parent.rollback().await?;

    assert_eq!(PageSize::new(0), Err(RepositoryError::InvalidPageSize));
    assert_eq!(PageSize::new(101), Err(RepositoryError::InvalidPageSize));
    pool.close().await;
    Ok(())
}
