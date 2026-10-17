//! Single-tenant bootstrap against the dev PostgreSQL (fixture prefix c3; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Every test uses a fresh tenant, so reruns pass.

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::bootstrap::{
    BootstrapError, BootstrapStatus, DEFAULT_BOOTSTRAP_LIFETIME_SECS, MAX_BOOTSTRAP_LIFETIME_SECS, bootstrap_required, complete_bootstrap,
    ensure_bootstrap,
};
use figcraft_cloud_storage::sessions::session_token_hash;
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; bootstrap probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

fn suffix() -> String {
    Uuid::new_v4().simple().to_string().chars().skip(20).collect()
}

async fn tenant(pool: &PgPool) -> Result<TenantId, Box<dyn std::error::Error>> {
    let suffix = suffix();
    let id: TenantId = format!("c3b00000-0000-4000-8000-{suffix}").parse()?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'c3 bootstrap', 'test', 'local')")
        .bind(id.to_string())
        .bind(format!("c3-boot-{suffix}"))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn user(pool: &PgPool, kind: &str, state: &str) -> Result<PrincipalId, Box<dyn std::error::Error>> {
    let id: PrincipalId = format!("c3b10000-0000-4000-8000-{}", suffix()).parse()?;
    sqlx::query("INSERT INTO principals (id, kind, display_name, state) VALUES ($1::uuid, $2, 'c3 bootstrap user', $3)")
        .bind(id.to_string())
        .bind(kind)
        .bind(state)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn issue(pool: &PgPool, tenant: TenantId, lifetime: i64) -> Result<Result<BootstrapStatus, BootstrapError>, Box<dyn std::error::Error>> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let status = ensure_bootstrap(&mut transaction, lifetime).await;
    transaction.commit().await?;
    Ok(status)
}

async fn complete(
    pool: &PgPool,
    tenant: TenantId,
    token: &str,
    principal: PrincipalId,
) -> Result<Result<(), BootstrapError>, Box<dyn std::error::Error>> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let result = complete_bootstrap(&mut transaction, token, principal).await;
    if result.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    Ok(result)
}

fn token(status: Result<BootstrapStatus, BootstrapError>) -> Result<String, Box<dyn std::error::Error>> {
    match status? {
        BootstrapStatus::Issued { token, .. } => Ok(token),
        BootstrapStatus::Completed => Err("expected a token".into()),
    }
}

#[tokio::test]
async fn bootstrap_makes_the_first_user_owner_once() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant = tenant(&pool).await?;
    let admin = user(&pool, "user", "active").await?;
    assert!(bootstrap_required(&pool, tenant).await?);
    let token = token(issue(&pool, tenant, DEFAULT_BOOTSTRAP_LIFETIME_SECS).await?)?;
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM tenant_bootstrap WHERE tenant_id = $1::uuid").bind(tenant.to_string()).fetch_one(&pool).await?;
    assert_eq!(stored, session_token_hash(&token).to_vec());
    assert!(bootstrap_required(&pool, tenant).await?);

    complete(&pool, tenant, &token, admin).await??;
    assert!(!bootstrap_required(&pool, tenant).await?);
    let (role, state): (String, String) =
        sqlx::query_as("SELECT role, state FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
            .bind(tenant.to_string())
            .bind(admin.to_string())
            .fetch_one(&pool)
            .await?;
    assert_eq!((role.as_str(), state.as_str()), ("tenant_owner", "active"));
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = 'tenant.bootstrap.complete' AND actor_principal_id = $2::uuid",
    )
    .bind(tenant.to_string())
    .bind(admin.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(audited, 1);

    // One-time: the token is spent and no new one is issued.
    let second = user(&pool, "user", "active").await?;
    assert_eq!(complete(&pool, tenant, &token, second).await?, Err(BootstrapError::Refused));
    assert_eq!(issue(&pool, tenant, DEFAULT_BOOTSTRAP_LIFETIME_SECS).await?, Ok(BootstrapStatus::Completed));
    Ok(())
}

#[tokio::test]
async fn wrong_expired_superseded_and_junk_tokens_are_refused() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant = tenant(&pool).await?;
    let admin = user(&pool, "user", "active").await?;
    for lifetime in [0, -1, MAX_BOOTSTRAP_LIFETIME_SECS + 1, i64::MIN, i64::MAX] {
        assert_eq!(issue(&pool, tenant, lifetime).await?, Err(BootstrapError::InvalidInput));
    }
    let old = token(issue(&pool, tenant, DEFAULT_BOOTSTRAP_LIFETIME_SECS).await?)?;
    let new = token(issue(&pool, tenant, DEFAULT_BOOTSTRAP_LIFETIME_SECS).await?)?;
    assert_ne!(old, new);
    assert_eq!(complete(&pool, tenant, &old, admin).await?, Err(BootstrapError::Refused), "superseded token still works");
    for junk in ["", "short", &"x".repeat(4096), &format!("{new} "), &"A".repeat(43)] {
        assert_eq!(complete(&pool, tenant, junk, admin).await?, Err(BootstrapError::Refused), "{junk:?}");
    }
    // Another tenant's token does not work here.
    let other = self::tenant(&pool).await?;
    let foreign = token(issue(&pool, other, DEFAULT_BOOTSTRAP_LIFETIME_SECS).await?)?;
    assert_eq!(complete(&pool, tenant, &foreign, admin).await?, Err(BootstrapError::Refused));

    // Service, pending and unknown principals cannot become owner.
    let service = user(&pool, "service", "active").await?;
    let pending = user(&pool, "user", "pending").await?;
    for principal in [service, pending, PrincipalId::random()] {
        assert_eq!(complete(&pool, tenant, &new, principal).await?, Err(BootstrapError::InvalidPrincipal));
    }

    // Expired (backdated).
    sqlx::query(
        "UPDATE tenant_bootstrap SET created_at = now() - interval '2 days', expires_at = now() - interval '1 second' WHERE tenant_id = $1::uuid",
    )
    .bind(tenant.to_string())
    .execute(&pool)
    .await?;
    assert_eq!(complete(&pool, tenant, &new, admin).await?, Err(BootstrapError::Refused));
    assert!(bootstrap_required(&pool, tenant).await?);
    // Reissuing after expiry works.
    let fresh = token(issue(&pool, tenant, 60).await?)?;
    complete(&pool, tenant, &fresh, admin).await??;
    assert!(!bootstrap_required(&pool, tenant).await?);
    Ok(())
}

#[tokio::test]
async fn an_existing_owner_means_no_bootstrap() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant = tenant(&pool).await?;
    let owner = user(&pool, "user", "active").await?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, 'tenant_owner')")
        .bind(tenant.to_string())
        .bind(owner.to_string())
        .execute(&pool)
        .await?;
    assert!(!bootstrap_required(&pool, tenant).await?);
    assert_eq!(issue(&pool, tenant, 60).await?, Ok(BootstrapStatus::Completed));
    // A suspended owner does not count.
    sqlx::query("UPDATE principals SET state = 'suspended' WHERE id = $1::uuid").bind(owner.to_string()).execute(&pool).await?;
    assert!(bootstrap_required(&pool, tenant).await?);
    // Unknown tenant.
    let unknown: TenantId = format!("c3b20000-0000-4000-8000-{}", suffix()).parse()?;
    assert_eq!(issue(&pool, unknown, 60).await?, Err(BootstrapError::Constraint));
    Ok(())
}
