//! S6-001 host registry against PostgreSQL; set FIGCRAFT_TEST_DATABASE_URL_FILE to a migrated
//! runtime URL. Every fixture tenant is random, so runs are independent and idempotent.

use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::host_systems::{
    HostRole, HostSystemError, KeyAlgorithm, MAX_LIVE_KEYS, NewHostKey, NewHostSystem, add_host_key, find_host_by_issuer, get_host_system,
    list_host_keys, list_host_systems, register_host_system, retire_host_key, set_host_enabled, update_host_policy,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; host registry probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

async fn tenant(pool: &PgPool) -> TestResult<TenantId> {
    let tenant = TenantId::random();
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'host probe', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("host-{}", tenant.as_uuid().simple()))
        .execute(tx.connection())
        .await?;
    tx.commit().await?;
    Ok(tenant)
}

fn host(issuer: &str) -> NewHostSystem {
    NewHostSystem {
        system_id: "host-a".to_owned(),
        issuer: issuer.to_owned(),
        audiences: vec!["figcraft".to_owned()],
        allowed_origins: vec!["https://host.example".to_owned()],
        max_role: HostRole::Editor,
        allow_insecure_localhost: false,
    }
}

fn key(kid: &str) -> NewHostKey {
    NewHostKey { kid: kid.to_owned(), alg: KeyAlgorithm::EdDsa, public_key: vec![7; 32], not_before: 0, expires_at: None }
}

fn issuer() -> String {
    format!("https://issuer-{}.example", Uuid::new_v4().simple())
}

#[tokio::test]
async fn registers_reads_updates_and_disables_a_host() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant = tenant(&pool).await?;
    let issuer = issuer();
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    let created = register_host_system(&mut tx, &host(&issuer)).await?;
    assert_eq!(created.tenant_id, tenant);
    assert!(created.enabled);
    assert_eq!(created.max_role, HostRole::Editor);
    assert_eq!(get_host_system(&mut tx, "host-a").await?, Some(created.clone()));
    assert_eq!(get_host_system(&mut tx, "absent").await?, None);
    assert_eq!(list_host_systems(&mut tx).await?, vec![created]);
    tx.commit().await?;

    // The issuer resolves while enabled and only then.
    let located = find_host_by_issuer(&pool, &issuer).await?.ok_or("issuer must resolve")?;
    assert_eq!((located.tenant_id, located.system_id.as_str()), (tenant, "host-a"));
    assert_eq!(find_host_by_issuer(&pool, "https://nobody.example").await?, None);

    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert!(
        update_host_policy(&mut tx, "host-a", &["a".to_owned(), "b".to_owned()], &["https://other.example".to_owned()], HostRole::Viewer, false)
            .await?
    );
    assert!(!update_host_policy(&mut tx, "absent", &["a".to_owned()], &[], HostRole::Viewer, false).await?);
    let updated = get_host_system(&mut tx, "host-a").await?.ok_or("host")?;
    assert_eq!((updated.audiences.len(), updated.max_role), (2, HostRole::Viewer));
    assert!(set_host_enabled(&mut tx, "host-a", false).await?);
    assert!(!set_host_enabled(&mut tx, "absent", false).await?);
    tx.commit().await?;
    assert_eq!(find_host_by_issuer(&pool, &issuer).await?, None, "a disabled host must not resolve");
    Ok(())
}

#[tokio::test]
async fn rejects_invalid_and_conflicting_registrations() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant = tenant(&pool).await?;
    let other = self::tenant(&pool).await?;
    let issuer = issuer();
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    for bad in [
        NewHostSystem { system_id: " ".into(), ..host(&issuer) },
        NewHostSystem { system_id: "x".repeat(129), ..host(&issuer) },
        NewHostSystem { issuer: String::new(), ..host(&issuer) },
        NewHostSystem { issuer: "i".repeat(257), ..host(&issuer) },
        NewHostSystem { audiences: vec![], ..host(&issuer) },
        NewHostSystem { audiences: vec!["a".into(); 9], ..host(&issuer) },
        NewHostSystem { allowed_origins: vec!["http://host.example".into()], ..host(&issuer) },
        NewHostSystem { allowed_origins: vec!["https://*.example".into()], ..host(&issuer) },
        NewHostSystem { allowed_origins: vec!["https://host.example/".into()], ..host(&issuer) },
        NewHostSystem { allowed_origins: vec!["https://a.example".to_owned(); 17], ..host(&issuer) },
        NewHostSystem { allowed_origins: vec!["http://localhost:3000".into()], ..host(&issuer) },
    ] {
        assert_eq!(register_host_system(&mut tx, &bad).await.err(), Some(HostSystemError::InvalidInput));
    }
    // Loopback http is accepted only in development mode.
    let dev = NewHostSystem {
        system_id: "dev".into(),
        issuer: self::issuer(),
        allowed_origins: vec!["http://localhost:3000".into()],
        allow_insecure_localhost: true,
        ..host(&issuer)
    };
    register_host_system(&mut tx, &dev).await?;
    register_host_system(&mut tx, &host(&issuer)).await?;
    tx.commit().await?;

    // Duplicate system id in the tenant and a second tenant claiming the same issuer both conflict.
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(
        register_host_system(&mut tx, &NewHostSystem { issuer: self::issuer(), ..host(&issuer) }).await.err(),
        Some(HostSystemError::Conflict)
    );
    tx.rollback().await?;
    let mut tx = begin_tenant_transaction(&pool, other).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(register_host_system(&mut tx, &host(&issuer)).await.err(), Some(HostSystemError::Conflict));
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn row_level_security_isolates_hosts_and_keys() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant_a = tenant(&pool).await?;
    let tenant_b = tenant(&pool).await?;
    let mut tx = begin_tenant_transaction(&pool, tenant_a).await.map_err(|error| format!("{error:?}"))?;
    register_host_system(&mut tx, &host(&issuer())).await?;
    add_host_key(&mut tx, "host-a", &key("k1")).await?;
    tx.commit().await?;

    let mut other = begin_tenant_transaction(&pool, tenant_b).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(get_host_system(&mut other, "host-a").await?, None);
    assert!(list_host_systems(&mut other).await?.is_empty());
    assert!(list_host_keys(&mut other, "host-a").await?.is_empty());
    assert_eq!(add_host_key(&mut other, "host-a", &key("k2")).await.err(), Some(HostSystemError::NotFound));
    assert!(!set_host_enabled(&mut other, "host-a", false).await?);
    assert!(!retire_host_key(&mut other, "host-a", "k1", 10).await?);
    // No tenant context at all sees nothing, and cannot insert.
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM host_systems").fetch_one(&pool).await?;
    assert_eq!(visible, 0);
    assert!(
        sqlx::query("INSERT INTO host_systems (tenant_id, system_id, issuer, audiences, allowed_origins, max_role) VALUES ($1::uuid, 'x', 'https://rls.example', ARRAY['a'], ARRAY[]::text[], 'viewer')")
            .bind(tenant_a.to_string())
            .execute(&pool)
            .await
            .is_err()
    );
    other.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn keys_rotate_with_overlap_retire_and_are_bounded() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let tenant = tenant(&pool).await?;
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    register_host_system(&mut tx, &host(&issuer())).await?;
    add_host_key(&mut tx, "host-a", &NewHostKey { expires_at: Some(2_000), ..key("old") }).await?;
    add_host_key(&mut tx, "host-a", &NewHostKey { not_before: 1_000, ..key("new") }).await?;
    let es = NewHostKey { alg: KeyAlgorithm::Es256, public_key: [vec![4], vec![9; 64]].concat(), ..key("es") };
    add_host_key(&mut tx, "host-a", &es).await?;
    assert_eq!(add_host_key(&mut tx, "host-a", &key("old")).await.err(), Some(HostSystemError::Conflict));
    tx.rollback().await?;

    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    register_host_system(&mut tx, &host(&issuer())).await?;
    add_host_key(&mut tx, "host-a", &NewHostKey { expires_at: Some(2_000), ..key("old") }).await?;
    add_host_key(&mut tx, "host-a", &NewHostKey { not_before: 1_000, ..key("new") }).await?;
    let keys = list_host_keys(&mut tx, "host-a").await?;
    assert_eq!(keys.len(), 2);
    let old = keys.iter().find(|k| k.kid == "old").ok_or("old")?;
    let new = keys.iter().find(|k| k.kid == "new").ok_or("new")?;
    // Overlap window 1000..2000: both verify; after it only the new key does.
    assert!(old.usable_at(1_500) && new.usable_at(1_500));
    assert!(!old.usable_at(2_000) && new.usable_at(2_000));
    assert!(!new.usable_at(999));
    assert!(retire_host_key(&mut tx, "host-a", "new", 1_600).await?);
    assert!(!retire_host_key(&mut tx, "host-a", "new", 1_700).await?, "retiring twice changes nothing");
    let keys = list_host_keys(&mut tx, "host-a").await?;
    let new = keys.iter().find(|k| k.kid == "new").ok_or("new")?;
    assert!(new.usable_at(1_599) && !new.usable_at(1_600));

    // Invalid keys.
    for bad in [
        NewHostKey { public_key: vec![1; 31], ..key("short") },
        NewHostKey { alg: KeyAlgorithm::Es256, public_key: vec![3; 65], ..key("not-uncompressed") },
        NewHostKey { alg: KeyAlgorithm::Es256, public_key: vec![4; 32], ..key("wrong-length") },
        NewHostKey { expires_at: Some(5), not_before: 5, ..key("empty-window") },
        NewHostKey { not_before: -1, ..key("negative") },
        key(""),
        key(&"k".repeat(129)),
    ] {
        assert_eq!(add_host_key(&mut tx, "host-a", &bad).await.err(), Some(HostSystemError::InvalidInput));
    }
    // The live-key cap counts only unretired keys: one is retired, one live ("old") exists.
    for index in 0..(MAX_LIVE_KEYS - 1) {
        add_host_key(&mut tx, "host-a", &key(&format!("fill-{index}"))).await?;
    }
    assert_eq!(add_host_key(&mut tx, "host-a", &key("one-too-many")).await.err(), Some(HostSystemError::TooManyKeys));
    tx.rollback().await?;
    Ok(())
}
