//! S6-044 platform realm isolation against PostgreSQL; set FIGCRAFT_TEST_DATABASE_URL_FILE to a
//! migrated runtime URL. Fixture slugs are random so runs are independent.

use figcraft_cloud_domain::{ResourceState, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::PageSize;
use figcraft_cloud_storage::platform::{
    AuditFilter, PlatformActor, PlatformAudit, PlatformOutcome, WorkspaceFilter, begin_platform_transaction, get_workspace, list_platform_audit,
    list_workspaces, record_platform_audit,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; platform realm probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

async fn workspace(pool: &PgPool, prefix: &str, state: &str) -> TestResult<TenantId> {
    let tenant = TenantId::random();
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, state, plan_id, home_region) VALUES ($1::uuid, $2, 'platform probe', $3, 'team', 'local')")
        .bind(tenant.to_string())
        .bind(format!("{prefix}-{}", tenant.as_uuid().simple()))
        .bind(state)
        .execute(tx.connection())
        .await?;
    // Something below the workspace the platform must never see.
    sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'secret space')")
        .bind(Uuid::new_v4().to_string())
        .bind(tenant.to_string())
        .execute(tx.connection())
        .await?;
    tx.commit().await?;
    Ok(tenant)
}

fn audit<'a>(action: &'a str, tenant: Option<TenantId>, metadata: &'a serde_json::Value) -> PlatformAudit<'a> {
    PlatformAudit {
        actor: PlatformActor::System,
        action,
        resource_kind: "workspace",
        resource_id: None,
        tenant_id: tenant,
        outcome: PlatformOutcome::Succeeded,
        request_id: None,
        metadata,
    }
}

#[tokio::test]
async fn workspace_transactions_cannot_touch_the_platform_realm() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant = workspace(&pool, "pr-iso", "active").await?;
    let marker = format!("pr.iso.{}", Uuid::new_v4().simple());
    let mut platform = begin_platform_transaction(&pool).await?;
    record_platform_audit(&mut platform, &audit(&marker, Some(tenant), &serde_json::json!({}))).await?;
    platform.commit().await?;

    // Outside any transaction scope, and inside a tenant transaction, the realm is empty.
    let outside: i64 = sqlx::query_scalar("SELECT count(*) FROM platform_audit_events").fetch_one(&pool).await?;
    assert_eq!(outside, 0);
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    for query in [
        "SELECT count(*) FROM platform_admins",
        "SELECT count(*) FROM platform_sessions",
        "SELECT count(*) FROM platform_bootstrap",
        "SELECT count(*) FROM platform_audit_events",
    ] {
        let rows: i64 = sqlx::query_scalar(query).fetch_one(tx.connection()).await?;
        assert_eq!(rows, 0, "{query}");
    }
    // Writes are refused by row-level security.
    let insert = sqlx::query(
        "INSERT INTO platform_audit_events (id, actor_kind, action, resource_kind, outcome) VALUES (gen_random_uuid(), 'system', 'x', 'x', 'succeeded')",
    )
    .execute(tx.connection())
    .await;
    assert!(insert.is_err());
    tx.rollback().await?;
    // The definer lookups refuse a workspace caller.
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let lookup = sqlx::query("SELECT * FROM figcraft_platform_workspaces(NULL, 5, NULL, NULL)").fetch_all(tx.connection()).await;
    assert!(lookup.is_err(), "a workspace caller must not list workspaces");
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn the_platform_sees_workspace_metadata_and_nothing_below_it() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let prefix = format!("pm{}", &Uuid::new_v4().simple().to_string()[..8]);
    let mut ids = Vec::new();
    for state in ["active", "suspended", "active"] {
        ids.push(workspace(&pool, &prefix, state).await?);
    }
    let mut tx = begin_platform_transaction(&pool).await?;
    // No tenant context: the tenant tables are invisible to a platform transaction.
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants").fetch_one(tx.connection()).await?;
    assert_eq!(visible, 0);
    let spaces: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces").fetch_one(tx.connection()).await?;
    assert_eq!(spaces, 0);
    let documents: i64 = sqlx::query_scalar("SELECT count(*) FROM documents").fetch_one(tx.connection()).await?;
    assert_eq!(documents, 0);

    let size = PageSize::new(2).map_err(|error| format!("{error:?}"))?;
    let filter = WorkspaceFilter { state: None, slug_prefix: Some(&prefix) };
    let first = list_workspaces(&mut tx, None, size, filter).await?;
    assert_eq!(first.items.len(), 2);
    let cursor = first.next_cursor.ok_or("a second page must exist")?;
    let second = list_workspaces(&mut tx, Some(cursor), size, filter).await?;
    assert_eq!(second.items.len(), 1);
    assert!(second.next_cursor.is_none());
    let mut found: Vec<_> = first.items.iter().chain(&second.items).map(|w| w.id).collect();
    found.sort();
    ids.sort();
    assert_eq!(found, ids);

    let suspended =
        list_workspaces(&mut tx, None, size, WorkspaceFilter { state: Some(ResourceState::Suspended), slug_prefix: Some(&prefix) }).await?;
    assert_eq!(suspended.items.len(), 1);
    assert_eq!(suspended.items[0].state, ResourceState::Suspended);
    assert_eq!(suspended.items[0].active_members, 0);
    let one = get_workspace(&mut tx, ids[0]).await?.ok_or("workspace must resolve")?;
    assert_eq!((one.plan_id.as_str(), one.home_region.as_str()), ("team", "local"));
    assert!(get_workspace(&mut tx, TenantId::random()).await?.is_none());
    assert!(list_workspaces(&mut tx, None, size, WorkspaceFilter { state: None, slug_prefix: Some("Bad Prefix") }).await.is_err());
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn the_platform_audit_log_is_append_only_and_pages_newest_first() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant = TenantId::random();
    let prefix = format!("pa{}", &Uuid::new_v4().simple().to_string()[..8]);
    let mut tx = begin_platform_transaction(&pool).await?;
    for n in 0..3 {
        record_platform_audit(&mut tx, &audit(&format!("{prefix}.n{n}"), Some(tenant), &serde_json::json!({ "n": n }))).await?;
    }
    record_platform_audit(&mut tx, &audit(&format!("{prefix}.other"), None, &serde_json::json!({}))).await?;
    tx.commit().await?;

    let mut tx = begin_platform_transaction(&pool).await?;
    let size = PageSize::new(2).map_err(|error| format!("{error:?}"))?;
    let filter = AuditFilter { tenant_id: Some(tenant), action_prefix: Some(&prefix) };
    let first = list_platform_audit(&mut tx, None, size, filter).await?;
    assert_eq!(first.items.len(), 2);
    let second = list_platform_audit(&mut tx, first.next_cursor, size, filter).await?;
    assert_eq!(second.items.len(), 1);
    let mut actions: Vec<_> = first.items.iter().chain(&second.items).map(|e| e.action.clone()).collect();
    actions.sort();
    assert_eq!(actions, [format!("{prefix}.n0"), format!("{prefix}.n1"), format!("{prefix}.n2")]);
    assert!(first.items.iter().chain(&second.items).all(|e| e.actor_kind == "system" && e.tenant_id == Some(tenant)));
    assert!(list_platform_audit(&mut tx, None, size, AuditFilter { tenant_id: None, action_prefix: Some("bad prefix!") }).await.is_err());

    // The runtime role may append but never rewrite or delete.
    let update = sqlx::query("UPDATE platform_audit_events SET outcome = 'failed' WHERE action LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(tx.connection())
        .await;
    assert!(update.is_err());
    tx.rollback().await?;
    let mut tx = begin_platform_transaction(&pool).await?;
    let delete = sqlx::query("DELETE FROM platform_audit_events WHERE action LIKE $1").bind(format!("{prefix}%")).execute(tx.connection()).await;
    assert!(delete.is_err());
    tx.rollback().await?;
    Ok(())
}
