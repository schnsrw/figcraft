//! Service accounts against the dev PostgreSQL (fixture prefix c3; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Fixture tenants are created idempotently; every test makes
//! fresh accounts, so reruns pass.

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::service_accounts::{
    MAX_ACTIVE_CREDENTIALS, MAX_CREDENTIAL_LIFETIME_SECS, MAX_ROTATION_OVERLAP_SECS, SECRET_PREFIX, Scope, ServiceAccount, ServiceAccountError,
    authenticate_credential, create_service_account, issue_credential, revoke_credential, rotate_credential, set_service_account_enabled,
};
use figcraft_cloud_storage::sessions::session_token_hash;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Fixtures and raw inspection queries run as the administrator: row-level security (S4-014)
/// hides every tenant row from the runtime role outside a tenant transaction. Repository calls
/// still run as the runtime role.
async fn admin_pool() -> Result<sqlx::PgPool, sqlx::Error> {
    let file = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE")
        .map_err(|_| sqlx::Error::Configuration("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE is unset".into()))?;
    let url = std::fs::read_to_string(file)?;
    sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(url.trim_end()).await
}

const TENANT: &str = "c3a00000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "c3a00000-0000-4000-8000-000000000002";
const ADMIN: &str = "c3a10000-0000-4000-8000-000000000001";
const DAY: i64 = 24 * 60 * 60;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

async fn pool() -> Result<Option<PgPool>, Error> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; service account probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?;
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'c3 service account admin') ON CONFLICT DO NOTHING")
        .bind(ADMIN)
        .execute(&admin_pool().await?)
        .await?;
    for (tenant, slug) in [(TENANT, "c3-service-accounts"), (OTHER_TENANT, "c3-service-accounts-other")] {
        sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'c3 service accounts', 'test', 'local') ON CONFLICT DO NOTHING")
            .bind(tenant)
            .bind(slug)
            .execute(&admin_pool().await?)
            .await?;
    }
    Ok(Some(pool))
}

fn admin() -> Result<PrincipalId, Error> {
    Ok(ADMIN.parse()?)
}

async fn account(pool: &PgPool, tenant: &str, scopes: &[Scope]) -> Result<ServiceAccount, Error> {
    let mut transaction = begin_tenant_transaction(pool, tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let account = create_service_account(&mut transaction, "c3 ci exporter", scopes, admin()?).await?;
    transaction.commit().await?;
    Ok(account)
}

async fn issue(pool: &PgPool, tenant: &str, account: Uuid, lifetime: i64) -> Result<Result<(Uuid, String), ServiceAccountError>, Error> {
    let mut transaction = begin_tenant_transaction(pool, tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let result = issue_credential(&mut transaction, account, lifetime, admin()?).await;
    transaction.commit().await?;
    Ok(result.map(|issued| (issued.id, issued.secret)))
}

#[tokio::test]
async fn credentials_authenticate_to_tenant_principal_and_scopes() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let created = account(&pool, TENANT, &[Scope::JobsRead, Scope::DocumentsRead, Scope::JobsRead]).await?;
    let (credential, secret) = issue(&pool, TENANT, created.id, 30 * DAY).await??;
    assert!(secret.starts_with(SECRET_PREFIX));
    let auth = authenticate_credential(&pool, &secret).await?;
    assert_eq!(auth.tenant_id, TENANT.parse::<TenantId>()?);
    assert_eq!(auth.principal_id, created.principal_id);
    assert_eq!(auth.service_account_id, created.id);
    assert_eq!(auth.credential_id, credential);
    assert_eq!(auth.scopes, vec![Scope::DocumentsRead, Scope::JobsRead]);

    // The principal is a service principal; only the hash is stored; last_used_at set.
    let kind: String = sqlx::query_scalar("SELECT kind FROM principals WHERE id = $1::uuid")
        .bind(created.principal_id.to_string())
        .fetch_one(&admin_pool().await?)
        .await?;
    assert_eq!(kind, "service");
    let (hash, used): (Vec<u8>, bool) =
        sqlx::query_as("SELECT secret_hash, last_used_at IS NOT NULL FROM service_account_credentials WHERE id = $1::uuid")
            .bind(credential.to_string())
            .fetch_one(&admin_pool().await?)
            .await?;
    assert_eq!(hash, session_token_hash(&secret).to_vec());
    assert!(used);
    // last_used_at writes are bounded: a second use inside the interval does not move it.
    sqlx::query("UPDATE service_account_credentials SET last_used_at = now() - interval '10 seconds' WHERE id = $1::uuid")
        .bind(credential.to_string())
        .execute(&admin_pool().await?)
        .await?;
    authenticate_credential(&pool, &secret).await?;
    let age: f64 = sqlx::query_scalar("SELECT extract(epoch FROM now() - last_used_at)::float8 FROM service_account_credentials WHERE id = $1::uuid")
        .bind(credential.to_string())
        .fetch_one(&admin_pool().await?)
        .await?;
    assert!(age >= 9.0, "last_used_at rewritten inside the interval");

    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND resource_id = $2::uuid")
        .bind(TENANT)
        .bind(created.id.to_string())
        .fetch_one(&admin_pool().await?)
        .await?;
    assert_eq!(audited, 2, "create + issue audited");
    Ok(())
}

#[tokio::test]
async fn invalid_inputs_and_caps_are_enforced() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    for name in ["", "   ", &"n".repeat(256), "bad\nname"] {
        assert_eq!(create_service_account(&mut transaction, name, &[Scope::JobsRead], admin()?).await, Err(ServiceAccountError::InvalidInput));
    }
    assert_eq!(create_service_account(&mut transaction, "c3", &[], admin()?).await, Err(ServiceAccountError::InvalidInput));
    transaction.rollback().await?;

    let created = account(&pool, TENANT, &[Scope::ExportsCreate]).await?;
    for lifetime in [0, -1, MAX_CREDENTIAL_LIFETIME_SECS + 1, i64::MIN, i64::MAX] {
        assert_eq!(issue(&pool, TENANT, created.id, lifetime).await?, Err(ServiceAccountError::InvalidInput));
    }
    // Unknown account, and an account of another tenant.
    assert_eq!(issue(&pool, TENANT, Uuid::new_v4(), DAY).await?, Err(ServiceAccountError::NotFound));
    assert_eq!(issue(&pool, OTHER_TENANT, created.id, DAY).await?, Err(ServiceAccountError::NotFound));

    let mut live = Vec::new();
    for _ in 0..MAX_ACTIVE_CREDENTIALS {
        live.push(issue(&pool, TENANT, created.id, MAX_CREDENTIAL_LIFETIME_SECS).await??);
    }
    assert_eq!(issue(&pool, TENANT, created.id, DAY).await?, Err(ServiceAccountError::TooManyCredentials));
    // Revoking one frees a slot.
    let (first, _) = live.first().cloned().ok_or("no credential")?;
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(revoke_credential(&mut transaction, created.id, first, admin()?).await?);
    assert!(!revoke_credential(&mut transaction, created.id, first, admin()?).await?);
    transaction.commit().await?;
    assert!(issue(&pool, TENANT, created.id, DAY).await?.is_ok());
    Ok(())
}

#[tokio::test]
async fn revoked_expired_disabled_and_junk_credentials_are_refused_alike() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let created = account(&pool, TENANT, &[Scope::DocumentsWrite]).await?;
    let (revoked_id, revoked) = issue(&pool, TENANT, created.id, DAY).await??;
    let (expired_id, expired) = issue(&pool, TENANT, created.id, DAY).await??;
    let (_, live) = issue(&pool, TENANT, created.id, DAY).await??;

    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(revoke_credential(&mut transaction, created.id, revoked_id, admin()?).await?);
    transaction.commit().await?;
    sqlx::query(
        "UPDATE service_account_credentials SET created_at = now() - interval '2 days', expires_at = now() - interval '1 second' WHERE id = $1::uuid",
    )
    .bind(expired_id.to_string())
    .execute(&admin_pool().await?)
    .await?;
    let token = live.strip_prefix(SECRET_PREFIX).ok_or("prefix")?.to_owned();
    for bad in [revoked.as_str(), expired.as_str(), "", SECRET_PREFIX, &token, &format!("fcsb_{token}"), &format!("{live} "), &"x".repeat(4096)] {
        assert_eq!(authenticate_credential(&pool, bad).await, Err(ServiceAccountError::Refused), "{bad:?}");
    }
    assert!(authenticate_credential(&pool, &live).await.is_ok());

    // Disabled account: refused, cannot issue, and restored on re-enable.
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(set_service_account_enabled(&mut transaction, created.id, false, admin()?).await?);
    assert!(!set_service_account_enabled(&mut transaction, created.id, false, admin()?).await?);
    transaction.commit().await?;
    assert_eq!(authenticate_credential(&pool, &live).await, Err(ServiceAccountError::Refused));
    assert_eq!(issue(&pool, TENANT, created.id, DAY).await?, Err(ServiceAccountError::NotFound));
    // Another tenant cannot re-enable it.
    let mut transaction = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(!set_service_account_enabled(&mut transaction, created.id, true, admin()?).await?);
    transaction.rollback().await?;
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(set_service_account_enabled(&mut transaction, created.id, true, admin()?).await?);
    transaction.commit().await?;
    assert!(authenticate_credential(&pool, &live).await.is_ok());

    // Suspended service principal: refused.
    sqlx::query("UPDATE principals SET state = 'suspended' WHERE id = $1::uuid")
        .bind(created.principal_id.to_string())
        .execute(&admin_pool().await?)
        .await?;
    assert_eq!(authenticate_credential(&pool, &live).await, Err(ServiceAccountError::Refused));

    // Stored scopes no longer on the allowlist are dropped; none left means refused.
    let other = account(&pool, TENANT, &[Scope::JobsRead]).await?;
    let (_, secret) = issue(&pool, TENANT, other.id, DAY).await??;
    sqlx::query("UPDATE service_accounts SET scopes = ARRAY['jobs:read', 'legacy:admin'] WHERE id = $1::uuid")
        .bind(other.id.to_string())
        .execute(&admin_pool().await?)
        .await?;
    assert_eq!(authenticate_credential(&pool, &secret).await?.scopes, vec![Scope::JobsRead]);
    sqlx::query("UPDATE service_accounts SET scopes = ARRAY['legacy:admin'] WHERE id = $1::uuid")
        .bind(other.id.to_string())
        .execute(&admin_pool().await?)
        .await?;
    assert_eq!(authenticate_credential(&pool, &secret).await, Err(ServiceAccountError::Refused));
    Ok(())
}

#[tokio::test]
async fn rotation_issues_a_replacement_and_keeps_the_old_one_for_the_overlap() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let created = account(&pool, TENANT, &[Scope::DocumentsRead]).await?;
    let (old_id, old) = issue(&pool, TENANT, created.id, 90 * DAY).await??;

    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(
        rotate_credential(&mut transaction, created.id, old_id, DAY, MAX_ROTATION_OVERLAP_SECS + 1, admin()?).await,
        Err(ServiceAccountError::InvalidInput)
    );
    assert_eq!(rotate_credential(&mut transaction, created.id, old_id, DAY, -1, admin()?).await, Err(ServiceAccountError::InvalidInput));
    assert_eq!(rotate_credential(&mut transaction, created.id, Uuid::new_v4(), DAY, 60, admin()?).await, Err(ServiceAccountError::NotFound));
    transaction.rollback().await?;

    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let next = rotate_credential(&mut transaction, created.id, old_id, 90 * DAY, 3600, admin()?).await?;
    transaction.commit().await?;
    // Both work during the overlap; the old one now expires within an hour.
    assert_eq!(authenticate_credential(&pool, &old).await?.credential_id, old_id);
    assert_eq!(authenticate_credential(&pool, &next.secret).await?.credential_id, next.id);
    let remaining: f64 =
        sqlx::query_scalar("SELECT extract(epoch FROM expires_at - now())::float8 FROM service_account_credentials WHERE id = $1::uuid")
            .bind(old_id.to_string())
            .fetch_one(&admin_pool().await?)
            .await?;
    assert!(remaining <= 3600.5 && remaining > 3500.0, "{remaining}");
    // After the overlap (backdated) only the new one works.
    sqlx::query(
        "UPDATE service_account_credentials SET created_at = now() - interval '2 days', expires_at = now() - interval '1 second' WHERE id = $1::uuid",
    )
    .bind(old_id.to_string())
    .execute(&admin_pool().await?)
    .await?;
    assert_eq!(authenticate_credential(&pool, &old).await, Err(ServiceAccountError::Refused));
    // The expired old credential cannot be rotated again.
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(rotate_credential(&mut transaction, created.id, old_id, DAY, 60, admin()?).await, Err(ServiceAccountError::NotFound));
    // Overlap zero revokes the old credential at once (the chain continues).
    let last = rotate_credential(&mut transaction, created.id, next.id, DAY, 0, admin()?).await?;
    transaction.commit().await?;
    assert_eq!(authenticate_credential(&pool, &next.secret).await, Err(ServiceAccountError::Refused));
    assert_eq!(authenticate_credential(&pool, &last.secret).await?.credential_id, last.id);

    // Rotation is allowed at the cap (the rotated credential is not counted).
    let capped = account(&pool, TENANT, &[Scope::JobsRead]).await?;
    let mut ids = Vec::new();
    for _ in 0..MAX_ACTIVE_CREDENTIALS {
        ids.push(issue(&pool, TENANT, capped.id, DAY).await??.0);
    }
    let first = *ids.first().ok_or("none")?;
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(rotate_credential(&mut transaction, capped.id, first, DAY, 60, admin()?).await.is_ok());
    transaction.commit().await?;
    Ok(())
}
