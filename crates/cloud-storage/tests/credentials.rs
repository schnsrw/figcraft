//! Local credentials against the dev PostgreSQL (S6-032; fixture prefix e6, skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Every test makes its own random tenant and principals, so
//! reruns and parallel runs pass and nothing shared is touched.

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::credentials::{
    CredentialError, CredentialState, LockoutPolicy, PasswordChange, create_credential, find_by_login, find_for_principal, record_failure,
    record_success, set_password, set_state, upgrade_hash,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const HASH_A: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2g";
const HASH_B: &str = "$argon2id$v=19$m=65536,t=3,p=1$c29tZXNhbHRzb21lc2FsdA$b3RoZXJoYXNob3RoZXJoYXNob3RoZXJoYXNob3RoZXJo";

async fn admin_pool() -> Result<PgPool, Error> {
    let file = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE")?;
    let url = std::fs::read_to_string(file)?;
    Ok(PgPoolOptions::new().max_connections(2).connect(url.trim_end()).await?)
}

async fn pool() -> Result<Option<PgPool>, Error> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; credentials probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(8).connect(url.trim_end()).await?))
}

async fn tenant(admin: &PgPool) -> Result<TenantId, Error> {
    let id = Uuid::new_v4();
    let slug = format!("e6-{}", &id.simple().to_string()[..12]);
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'e6 credentials', 'test', 'local')")
        .bind(id.to_string())
        .bind(slug)
        .execute(admin)
        .await?;
    Ok(id.to_string().parse()?)
}

async fn principal(admin: &PgPool) -> Result<PrincipalId, Error> {
    let id = PrincipalId::random();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e6 person')").bind(id.to_string()).execute(admin).await?;
    Ok(id)
}

#[tokio::test]
async fn logins_are_unique_per_tenant_after_normalisation() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let admin = admin_pool().await?;
    let (a, b) = (tenant(&admin).await?, tenant(&admin).await?);
    let (ada, grace, ada_b) = (principal(&admin).await?, principal(&admin).await?, principal(&admin).await?);

    let mut tx = begin_tenant_transaction(&pool, a).await.map_err(|e| format!("{e:?}"))?;
    create_credential(&mut tx, ada, "  Ada.Lovelace@Example.com ", HASH_A, Some(ada)).await?;
    // The same login in another case or Unicode form is the same account.
    assert_eq!(create_credential(&mut tx, grace, "ADA.LOVELACE@example.com", HASH_A, None).await, Err(CredentialError::Conflict));
    tx.rollback().await?;

    let mut tx = begin_tenant_transaction(&pool, a).await.map_err(|e| format!("{e:?}"))?;
    create_credential(&mut tx, ada, "Jose\u{301}", HASH_A, None).await?;
    tx.commit().await?;
    let mut tx = begin_tenant_transaction(&pool, a).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(create_credential(&mut tx, grace, "JOS\u{c9}", HASH_A, None).await, Err(CredentialError::Conflict));
    tx.rollback().await?;
    // A principal has one credential per tenant.
    let mut tx = begin_tenant_transaction(&pool, a).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(create_credential(&mut tx, ada, "someone-else", HASH_A, None).await, Err(CredentialError::Conflict));
    tx.rollback().await?;
    // Bad input is refused before the database.
    let mut tx = begin_tenant_transaction(&pool, a).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(create_credential(&mut tx, grace, "x y", HASH_A, None).await, Err(CredentialError::InvalidLogin));
    assert_eq!(create_credential(&mut tx, grace, "grace", "not-a-phc", None).await, Err(CredentialError::InvalidHash));
    tx.rollback().await?;

    // Another tenant: the same login is free there, and a's accounts are invisible.
    let mut tx = begin_tenant_transaction(&pool, b).await.map_err(|e| format!("{e:?}"))?;
    assert!(find_by_login(&mut tx, "jos\u{e9}").await?.is_none());
    assert!(find_for_principal(&mut tx, ada).await?.is_none());
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM local_credentials").fetch_one(tx.connection()).await?;
    assert_eq!(visible, 0);
    create_credential(&mut tx, ada_b, "jose\u{301}", HASH_B, None).await?;
    tx.commit().await?;

    let mut tx = begin_tenant_transaction(&pool, a).await.map_err(|e| format!("{e:?}"))?;
    let found = find_by_login(&mut tx, "JOSE\u{301}").await?.ok_or("missing")?;
    assert_eq!(found.principal_id, ada);
    assert_eq!(found.password_hash, HASH_A);
    assert_eq!(found.state, CredentialState::Active);
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM local_credentials").fetch_one(tx.connection()).await?;
    assert_eq!(visible, 1);
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn lockout_counts_locks_does_not_extend_and_resets() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let admin = admin_pool().await?;
    let t = tenant(&admin).await?;
    let ada = principal(&admin).await?;
    let policy = LockoutPolicy { threshold: 3, base_secs: 4, max_secs: 10 };

    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    create_credential(&mut tx, ada, "ada", HASH_A, None).await?;
    assert_eq!(record_failure(&mut tx, ada, policy).await?, 0);
    assert_eq!(record_failure(&mut tx, ada, policy).await?, 0);
    let locked = record_failure(&mut tx, ada, policy).await?;
    assert!((1..=4).contains(&locked), "{locked}");
    tx.commit().await?;

    // While locked, more failures neither count nor extend.
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    for _ in 0..5 {
        assert!(record_failure(&mut tx, ada, policy).await? <= 4);
    }
    let record = find_for_principal(&mut tx, ada).await?.ok_or("missing")?;
    assert_eq!(record.failed_attempts, 3);
    assert!((1..=4).contains(&record.locked_for_secs));
    tx.commit().await?;

    // Once the lock lapses the next failure doubles it (3 + 1 = 4th failure: 4 << 1 = 8 s).
    sqlx::query("UPDATE local_credentials SET locked_until = now() - interval '1 second' WHERE principal_id = $1::uuid")
        .bind(ada.to_string())
        .execute(&admin)
        .await?;
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    let second = record_failure(&mut tx, ada, policy).await?;
    assert!((5..=8).contains(&second), "{second}");
    tx.commit().await?;
    // ... and the cap holds.
    sqlx::query("UPDATE local_credentials SET locked_until = now() - interval '1 second' WHERE principal_id = $1::uuid")
        .bind(ada.to_string())
        .execute(&admin)
        .await?;
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    let third = record_failure(&mut tx, ada, policy).await?;
    assert!((7..=10).contains(&third), "{third}");
    tx.commit().await?;

    // Success clears everything.
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    record_success(&mut tx, ada).await?;
    let record = find_for_principal(&mut tx, ada).await?.ok_or("missing")?;
    assert_eq!((record.failed_attempts, record.locked_for_secs), (0, 0));
    assert_eq!(record_failure(&mut tx, ada, LockoutPolicy { threshold: 0, ..policy }).await, Err(CredentialError::InvalidPolicy));
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_failures_are_all_counted_until_the_lock() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let admin = admin_pool().await?;
    let t = tenant(&admin).await?;
    let ada = principal(&admin).await?;
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    create_credential(&mut tx, ada, "ada", HASH_A, None).await?;
    tx.commit().await?;
    let policy = LockoutPolicy::default();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
            let result = record_failure(&mut tx, ada, policy).await.map_err(|e| format!("{e:?}"))?;
            tx.commit().await.map_err(|e| e.to_string())?;
            Ok::<i64, String>(result)
        }));
    }
    for task in tasks {
        task.await??;
    }
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    let record = find_for_principal(&mut tx, ada).await?.ok_or("missing")?;
    // Five counted failures lock the account; the other three arrived locked and were not counted.
    assert_eq!(record.failed_attempts, 5);
    assert!(record.locked_for_secs > 0);
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn password_changes_state_and_rehash_are_audited_and_guarded() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let admin = admin_pool().await?;
    let t = tenant(&admin).await?;
    let (ada, boss) = (principal(&admin).await?, principal(&admin).await?);
    let mut tx = begin_tenant_transaction(&pool, t).await.map_err(|e| format!("{e:?}"))?;
    create_credential(&mut tx, ada, "ada", HASH_A, Some(boss)).await?;
    let policy = LockoutPolicy { threshold: 1, base_secs: 60, max_secs: 60 };
    assert!(record_failure(&mut tx, ada, policy).await? > 0);
    // A new password clears the lock.
    set_password(&mut tx, ada, HASH_B, PasswordChange::Reset, Some(boss)).await?;
    let record = find_for_principal(&mut tx, ada).await?.ok_or("missing")?;
    assert_eq!((record.password_hash.as_str(), record.failed_attempts, record.locked_for_secs), (HASH_B, 0, 0));
    // Rehash applies only while the stored hash is the one that was verified.
    let upgraded = format!("{HASH_B}x");
    assert!(!upgrade_hash(&mut tx, ada, HASH_A, &upgraded).await?);
    assert!(upgrade_hash(&mut tx, ada, HASH_B, &upgraded).await?);
    assert!(!upgrade_hash(&mut tx, ada, HASH_B, &upgraded).await?);
    assert_eq!(set_password(&mut tx, boss, HASH_B, PasswordChange::Change, None).await, Err(CredentialError::NotFound));
    assert!(set_state(&mut tx, ada, CredentialState::Disabled, boss).await?);
    assert!(!set_state(&mut tx, ada, CredentialState::Disabled, boss).await?);
    assert_eq!(find_for_principal(&mut tx, ada).await?.ok_or("missing")?.state, CredentialState::Disabled);
    assert!(set_state(&mut tx, ada, CredentialState::Active, boss).await?);
    tx.commit().await?;

    let mut actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE tenant_id = $1::uuid AND action LIKE 'credential.%' ")
        .bind(t.to_string())
        .fetch_all(&admin)
        .await?;
    actions.sort();
    assert_eq!(actions, ["credential.create", "credential.disable", "credential.enable", "credential.password_reset"]);
    // The audit trail never carries the login or a hash.
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND metadata::text ~* 'argon2|ada'")
        .bind(t.to_string())
        .fetch_one(&admin)
        .await?;
    assert_eq!(leaked, 0);
    Ok(())
}
