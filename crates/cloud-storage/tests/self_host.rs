//! TS-001 one-command setup against PostgreSQL; skipped cleanly without
//! FIGCRAFT_TEST_DATABASE_URL_FILE. The pure parts (slugs, generated passwords) are unit-tested in
//! `self_host.rs` without a database.

use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::platform::begin_platform_transaction;
use figcraft_cloud_storage::self_host::{PlatformAdminSpec, SelfHostSpec, SetupError, setup_self_hosted};
use figcraft_cloud_storage::{begin_tenant_transaction, credentials};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const HASH: &str = "$argon2id$v=19$m=8192,t=1,p=1$c29tZXNhbHQ$aGFzaGhhc2hoYXNoaGFzaGhhc2g";

static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; self-host setup probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

fn suffix() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_owned()
}

#[tokio::test]
async fn setup_creates_the_workspace_and_owner_once_and_refuses_a_second_run() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant = TenantId::random();
    let slug = format!("sh-{}", suffix());
    let login = format!("owner-{}@team.example", suffix());
    let spec = SelfHostSpec {
        tenant,
        slug: &slug,
        name: "Team Space",
        owner_login: &login,
        owner_name: "Ada",
        owner_password_hash: HASH,
        platform_admin: None,
    };

    let outcome = setup_self_hosted(&pool, &spec).await?;
    assert_eq!((outcome.workspace_created, outcome.platform_admin_created), (true, false));
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    let (state, plan, name): (String, String, String) = sqlx::query_as("SELECT state, plan_id, name FROM tenants").fetch_one(tx.connection()).await?;
    assert_eq!((state.as_str(), plan.as_str(), name.as_str()), ("active", "self_hosted", "Team Space"));
    let owners: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_members WHERE role = 'tenant_owner' AND state = 'active'").fetch_one(tx.connection()).await?;
    assert_eq!(owners, 1);
    let record = credentials::find_by_login(&mut tx, &login).await.map_err(|e| format!("{e:?}"))?.ok_or("the owner's password account")?;
    assert_eq!(record.password_hash, HASH);
    let audit: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events").fetch_all(tx.connection()).await?;
    assert!(audit.contains(&"tenant.create".to_owned()));
    assert!(!format!("{audit:?}").contains(HASH), "no secret in the audit trail");
    let _ = tx.rollback().await;

    // Running it again changes nothing and says so.
    assert_eq!(setup_self_hosted(&pool, &spec).await.err(), Some(SetupError::AlreadySetUp));
    // Another workspace cannot take the same address.
    let other = SelfHostSpec { tenant: TenantId::random(), owner_login: "someone@team.example", ..spec };
    assert_eq!(setup_self_hosted(&pool, &other).await.err(), Some(SetupError::Conflict));
    Ok(())
}

#[tokio::test]
async fn setup_validates_names_and_completes_an_ownerless_workspace() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    fn base<'a>(tenant: TenantId, slug: &'a str, name: &'a str, login: &'a str) -> SelfHostSpec<'a> {
        SelfHostSpec { tenant, slug, name, owner_login: login, owner_name: "Ada", owner_password_hash: HASH, platform_admin: None }
    }
    let tenant = TenantId::random();
    let fresh_slug = format!("sh-{}", suffix());
    assert_eq!(setup_self_hosted(&pool, &base(tenant, "Bad Slug", "Name", "a@team.example")).await.err(), Some(SetupError::Invalid));
    assert_eq!(setup_self_hosted(&pool, &base(tenant, "admin", "Name", "a@team.example")).await.err(), Some(SetupError::Invalid), "reserved");
    assert_eq!(setup_self_hosted(&pool, &base(tenant, "fine-slug-x", "  ", "a@team.example")).await.err(), Some(SetupError::Invalid));
    assert_eq!(setup_self_hosted(&pool, &base(tenant, &fresh_slug, "Name", "no")).await.err(), Some(SetupError::Invalid), "login");

    // A tenant row left by an earlier bootstrap without an owner is completed, not duplicated.
    let slug = format!("sh-{}", suffix());
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'placeholder', 'self-hosted', 'local')")
        .bind(tenant.to_string())
        .bind(format!("tenant-{}", suffix()))
        .execute(tx.connection())
        .await?;
    tx.commit().await.map_err(|e| format!("{e:?}"))?;
    let login = format!("late-{}@team.example", suffix());
    let outcome = setup_self_hosted(&pool, &base(tenant, &slug, "Completed", &login)).await?;
    assert!(outcome.workspace_created);
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    let (name, found_slug): (String, String) = sqlx::query_as("SELECT name, slug FROM tenants").fetch_one(tx.connection()).await?;
    assert_eq!((name.as_str(), found_slug.as_str()), ("Completed", slug.as_str()));
    let _ = tx.rollback().await;
    Ok(())
}

#[tokio::test]
async fn setup_creates_the_platform_admin_only_when_none_exists() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    let mut tx = begin_platform_transaction(&pool).await?;
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM platform_admins").fetch_one(tx.connection()).await?;
    let _ = tx.rollback().await;
    if existing > 0 {
        eprintln!("platform admins already exist in this database; the first-admin branch is not exercised");
        return Ok(());
    }
    let login = format!("root-{}@team.example", suffix());
    let tenant = TenantId::random();
    let slug = format!("sh-{}", suffix());
    let owner = format!("own-{}@team.example", suffix());
    let spec = SelfHostSpec {
        tenant,
        slug: &slug,
        name: "With Admin",
        owner_login: &owner,
        owner_name: "Ada",
        owner_password_hash: HASH,
        platform_admin: Some(PlatformAdminSpec { login: &login, display_name: "Ada", password_hash: HASH }),
    };
    let outcome = setup_self_hosted(&pool, &spec).await?;
    assert_eq!((outcome.workspace_created, outcome.platform_admin_created), (true, true));
    // A second run finds both and does nothing.
    assert_eq!(setup_self_hosted(&pool, &spec).await.err(), Some(SetupError::AlreadySetUp));
    let mut tx = begin_platform_transaction(&pool).await?;
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM platform_audit_events WHERE action LIKE 'platform.setup.%' AND metadata::text NOT LIKE '%argon2%'")
            .fetch_all(tx.connection())
            .await?;
    assert!(actions.contains(&"platform.setup.admin".to_owned()) && actions.contains(&"platform.setup.workspace".to_owned()));
    sqlx::query("DELETE FROM platform_admins WHERE login = $1").bind(&login).execute(tx.connection()).await?;
    tx.commit().await?;
    Ok(())
}
