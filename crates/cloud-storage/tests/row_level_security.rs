//! S4-014 row-level security against the dev PostgreSQL (fixture prefix d4; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Runs as the runtime role. Fixtures are created idempotently
//! inside tenant transactions, so reruns pass.

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::PageSize;
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, dead_share_links, exchange_share_token, resolve_share_link};
use figcraft_cloud_storage::tenants::list_tenants_for_principal;
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT_A: &str = "d4000000-0000-4000-8000-0000000000a1";
const TENANT_B: &str = "d4000000-0000-4000-8000-0000000000b1";
const PRINCIPAL: &str = "d4000000-0000-4000-8000-000000000011";
const WORKSPACE_A: &str = "d4100000-0000-4000-8000-0000000000a1";
const PROJECT_A: &str = "d4200000-0000-4000-8000-0000000000a1";
const DOCUMENT_A: &str = "d4300000-0000-4000-8000-0000000000a1";
const WORKSPACE_B: &str = "d4100000-0000-4000-8000-0000000000b1";

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; row-level security probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    // One connection, so every check below reuses the connection the tenant transactions ran on.
    let pool = PgPoolOptions::new().max_connections(1).connect(database_url.trim_end()).await?;
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd4 rls probe') ON CONFLICT DO NOTHING")
        .bind(PRINCIPAL)
        .execute(&pool)
        .await?;
    for (tenant, slug, workspace) in [(TENANT_A, "d4-rls-a", WORKSPACE_A), (TENANT_B, "d4-rls-b", WORKSPACE_B)] {
        let mut transaction = begin_tenant_transaction(&pool, tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
        let connection = transaction.connection();
        sqlx::query(
            "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'd4 rls', 'test', 'local') ON CONFLICT DO NOTHING",
        )
        .bind(tenant)
        .bind(slug)
        .execute(&mut *connection)
        .await?;
        sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, 'editor') ON CONFLICT DO NOTHING")
            .bind(tenant)
            .bind(PRINCIPAL)
            .execute(&mut *connection)
            .await?;
        sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'd4 workspace') ON CONFLICT DO NOTHING")
            .bind(workspace)
            .bind(tenant)
            .execute(&mut *connection)
            .await?;
        transaction.commit().await?;
    }
    let mut transaction = begin_tenant_transaction(&pool, TENANT_A.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let connection = transaction.connection();
    sqlx::query(
        "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'd4 project') ON CONFLICT DO NOTHING",
    )
    .bind(PROJECT_A)
    .bind(TENANT_A)
    .bind(WORKSPACE_A)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'd4 document') \
         ON CONFLICT DO NOTHING",
    )
    .bind(DOCUMENT_A)
    .bind(TENANT_A)
    .bind(WORKSPACE_A)
    .bind(PROJECT_A)
    .execute(&mut *connection)
    .await?;
    transaction.commit().await?;
    Ok(Some(pool))
}

/// Every tenant-owned table carrying forced row-level security, from the catalog. The platform
/// realm's tables (`platform_*`, S6-044) have their own scope and their own test
/// (`platform_realm.rs`); `platform_audit_events.tenant_id` only names the workspace an event is about.
async fn protected_tables(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'p') \
         AND c.relrowsecurity AND c.relforcerowsecurity AND c.relname NOT LIKE 'platform\\_%' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
}

fn is_rls_violation(error: &sqlx::Error) -> bool {
    error.as_database_error().and_then(|error| error.code()).is_some_and(|code| code == "42501")
}

#[tokio::test]
async fn tenant_tables_hide_every_row_outside_a_tenant_transaction_and_other_tenants_inside_one() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tables = protected_tables(&pool).await?;
    // Every tenant-owned table: a tenant_id column, plus tenants itself.
    let expected: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r', 'p') \
         AND c.relname <> 'tenant_context_probe' AND c.relname NOT LIKE 'platform\\_%' \
         AND (c.relname = 'tenants' OR EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attname = 'tenant_id' AND NOT a.attisdropped)) \
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await?;
    assert!(expected.len() >= 36, "tenant-owned tables: {expected:?}");
    assert_eq!(tables, expected, "every tenant-owned table, and only those, has forced row-level security");
    for global in ["principals", "identity_links", "browser_sessions", "oidc_login_attempts", "_sqlx_migrations"] {
        assert!(!tables.iter().any(|table| table == global), "{global} is global by design");
    }

    // Outside a tenant transaction: zero rows everywhere, although the fixtures exist.
    for table in &tables {
        let sql = format!("SELECT count(*) FROM public.\"{table}\"");
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).fetch_one(&pool).await?;
        assert_eq!(count, 0, "{table} leaked rows without a tenant context");
    }

    // Inside tenant A: only tenant A rows, in every tenant table.
    let tenant_a: TenantId = TENANT_A.parse()?;
    let mut transaction = begin_tenant_transaction(&pool, tenant_a).await.map_err(|error| format!("{error:?}"))?;
    let visible: Vec<String> = sqlx::query_scalar("SELECT id::text FROM tenants ORDER BY id").fetch_all(transaction.connection()).await?;
    assert_eq!(visible, [TENANT_A.to_owned()]);
    let workspaces: Vec<String> = sqlx::query_scalar("SELECT id::text FROM workspaces WHERE id IN ($1::uuid, $2::uuid)")
        .bind(WORKSPACE_A)
        .bind(WORKSPACE_B)
        .fetch_all(transaction.connection())
        .await?;
    assert_eq!(workspaces, [WORKSPACE_A.to_owned()]);
    for table in tables.iter().filter(|table| *table != "tenants") {
        let sql = format!("SELECT count(*) FROM public.\"{table}\" WHERE tenant_id <> $1::uuid");
        let foreign: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(TENANT_A).fetch_one(transaction.connection()).await?;
        assert_eq!(foreign, 0, "{table} shows another tenant's rows inside tenant A");
    }
    // Setting the definer flag does not open anything to the runtime role.
    sqlx::query("SELECT set_config('figcraft.definer_lookup', 'on', true)").execute(transaction.connection()).await?;
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants").fetch_one(transaction.connection()).await?;
    assert_eq!(still, 1);
    transaction.rollback().await?;

    // Inside tenant B: only tenant B.
    let mut transaction = begin_tenant_transaction(&pool, TENANT_B.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let visible: Vec<String> = sqlx::query_scalar("SELECT id::text FROM tenants ORDER BY id").fetch_all(transaction.connection()).await?;
    assert_eq!(visible, [TENANT_B.to_owned()]);
    let documents: i64 =
        sqlx::query_scalar("SELECT count(*) FROM documents WHERE id = $1::uuid").bind(DOCUMENT_A).fetch_one(transaction.connection()).await?;
    assert_eq!(documents, 0, "tenant A's document is invisible to tenant B");
    transaction.commit().await?;

    // The same pooled connection is back to seeing nothing once the transaction ended.
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants").fetch_one(&pool).await?;
    assert_eq!(after, 0);
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn writes_are_refused_without_a_tenant_context_and_for_another_tenant() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    // No context: an insert fails WITH CHECK and an update or delete touches nothing.
    let refused = sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'd4 no context')")
        .bind(Uuid::new_v4().to_string())
        .bind(TENANT_A)
        .execute(&pool)
        .await;
    assert!(refused.as_ref().is_err_and(is_rls_violation), "{refused:?}");
    let renamed = sqlx::query("UPDATE workspaces SET name = 'd4 hijacked' WHERE id = $1::uuid").bind(WORKSPACE_A).execute(&pool).await?;
    assert_eq!(renamed.rows_affected(), 0);

    // Tenant A's context: a row naming tenant B is refused, and B's rows cannot be changed.
    let mut transaction = begin_tenant_transaction(&pool, TENANT_A.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let cross = sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'd4 cross tenant')")
        .bind(Uuid::new_v4().to_string())
        .bind(TENANT_B)
        .execute(transaction.connection())
        .await;
    assert!(cross.as_ref().is_err_and(is_rls_violation), "{cross:?}");
    transaction.rollback().await?;
    let mut transaction = begin_tenant_transaction(&pool, TENANT_A.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let moved =
        sqlx::query("UPDATE workspaces SET name = 'd4 hijacked' WHERE id = $1::uuid").bind(WORKSPACE_B).execute(transaction.connection()).await?;
    assert_eq!(moved.rows_affected(), 0, "tenant A cannot update tenant B's workspace");
    // Moving one of A's own rows to B is a WITH CHECK violation too.
    let rehome = sqlx::query("UPDATE workspaces SET tenant_id = $2::uuid WHERE id = $1::uuid")
        .bind(WORKSPACE_A)
        .bind(TENANT_B)
        .execute(transaction.connection())
        .await;
    assert!(rehome.is_err(), "{rehome:?}");
    transaction.rollback().await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn pre_tenant_lookups_go_through_the_definer_functions_only() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    // A principal in two tenants lists both before choosing one.
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let page = list_tenants_for_principal(&pool, principal, None, PageSize::new(10).map_err(|error| format!("{error:?}"))?)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let mut listed: Vec<String> = page.items.iter().map(|item| item.tenant.id.to_string()).collect();
    listed.sort();
    assert_eq!(listed, [TENANT_A.to_owned(), TENANT_B.to_owned()]);

    // A share link resolves by its secret alone, and the dead-link check works across tenants.
    let mut transaction = begin_tenant_transaction(&pool, TENANT_A.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let created = create_share_link(&mut transaction, DOCUMENT_A.parse()?, ShareRole::Viewer, Some("d4 rls"), principal, Some(3600))
        .await
        .map_err(|error| format!("{error:?}"))?;
    transaction.commit().await?;
    let resolved = resolve_share_link(&pool, &created.secret).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(resolved.tenant_id.to_string(), TENANT_A);
    assert_eq!(resolved.document_id.to_string(), DOCUMENT_A);
    let grant = exchange_share_token(&pool, &created.secret).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(grant.link_id, created.id);
    let tenant_a: TenantId = TENANT_A.parse()?;
    let tenant_b: TenantId = TENANT_B.parse()?;
    let dead = dead_share_links(&pool, &[(tenant_a, created.id), (tenant_b, created.id)]).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(dead, [(tenant_b, created.id)], "the live link is alive only in its own tenant");

    // The functions return nothing for an unknown hash, and refuse an oversized batch.
    let unknown: Option<String> = sqlx::query_scalar("SELECT figcraft_invitation_tenant($1)").bind(vec![0_u8; 32]).fetch_one(&pool).await?;
    assert_eq!(unknown, None);
    let none: Vec<String> = sqlx::query_scalar("SELECT id FROM figcraft_live_share_link($1, 60)").bind(vec![0_u8; 32]).fetch_all(&pool).await?;
    assert!(none.is_empty());
    let oversized: Vec<String> = (0..1025).map(|_| Uuid::new_v4().to_string()).collect();
    let refused =
        sqlx::query("SELECT * FROM figcraft_dead_share_links($1::text[]::uuid[], $1::text[]::uuid[])").bind(&oversized).fetch_all(&pool).await;
    assert!(refused.is_err());

    // The runtime role cannot see the rows the functions read, nor redefine the functions.
    let direct: i64 = sqlx::query_scalar("SELECT count(*) FROM share_links").fetch_one(&pool).await?;
    assert_eq!(direct, 0);
    let redefine =
        sqlx::query("CREATE OR REPLACE FUNCTION public.figcraft_invitation_tenant(p BYTEA) RETURNS TEXT LANGUAGE sql AS $$ SELECT NULL::text $$")
            .execute(&pool)
            .await;
    assert!(redefine.is_err());
    pool.close().await;
    Ok(())
}
