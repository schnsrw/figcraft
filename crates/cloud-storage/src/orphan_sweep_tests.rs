//! Orphan-key sweep against PostgreSQL and a versioned bucket (fixture
//! `deploy/saas/orphan-sweep-preflight.sql`, tenant 1). These live inside the crate because a key
//! written now cannot be backdated in the store: the sweep runs with its clock shifted two days
//! ahead through the private [`collect_orphans_at`], while the minimum age itself stays at its
//! floor. Skipped unless `FIGCRAFT_TEST_DATABASE_URL_FILE` and `FIGCRAFT_TEST_S3_DIR` are set.

use super::*;
use crate::object_metadata::{NewObject, ObjectKind, keys, register_object};
use crate::objects::ObjectStoreSettings;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;

const TENANT: &str = "b0000000-0000-4000-8000-000000000001";
const PRINCIPAL: &str = "b0000000-0000-4000-8000-000000000009";
const SHIFT: Duration = Duration::from_secs(2 * 24 * 60 * 60);

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

async fn probe() -> TestResult<Option<(PgPool, ObjectStore)>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; orphan sweep probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let read =
        |name: &str| -> TestResult<String> { Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string()) };
    let store = ObjectStore::new(&ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    })?;
    Ok(Some((pool, store)))
}

fn tenant() -> TenantId {
    TENANT.parse().unwrap()
}

async fn versions(store: &ObjectStore, key: &str) -> TestResult<usize> {
    Ok(list_version_entries(store, key).await.map_err(|failure| format!("{failure:?}"))?.ok_or("store lists no versions")?.len())
}

async fn sweep(pool: &PgPool, store: &ObjectStore, dry_run: bool, elapsed: Duration) -> TestResult<OrphanReport> {
    let options = OrphanOptions { dry_run, limit: MAX_GC_BATCH, min_age: Duration::ZERO, max_keys: MAX_ORPHAN_KEYS };
    Ok(collect_orphans_at(pool, store, tenant(), options, elapsed).await?)
}

fn named(report: &OrphanReport, key: &str) -> bool {
    report.orphans.iter().any(|orphan| orphan.object_key == key)
        || report.removed.iter().any(|removed| removed == key)
        || report.kept_young_versions.iter().any(|kept| kept == key)
        || report.failed.iter().any(|(failed, _)| failed == key)
}

#[tokio::test]
async fn orphan_keys_are_swept_after_the_minimum_age_and_protected_keys_are_not() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let run = Uuid::new_v4();
    // An orphan with two versions behind a delete marker, as a rolled-back upload retried twice
    // and then deleted plainly would leave.
    let orphan = keys::asset(tenant(), &<[u8; 32]>::from(Sha256::digest(format!("orphan {run}"))));
    store.put_object(&orphan, b"orphan v1".to_vec(), "application/octet-stream").await?;
    store.put_object(&orphan, b"orphan v2".to_vec(), "application/octet-stream").await?;
    // A live reservation's staging key with bytes uploaded to it.
    let reservation = Uuid::new_v4();
    let staging = keys::staging(tenant(), reservation);
    let mut scoped = begin(&pool, tenant()).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query(concat!(
        "INSERT INTO upload_reservations (id, tenant_id, principal_id, kind, staging_key, expected_sha256, expected_size, media_type, expires_at) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, 'asset', $4, $5, 7, 'image/png', now() + interval '10 minutes')"
    ))
    .bind(reservation.to_string())
    .bind(TENANT)
    .bind(PRINCIPAL)
    .bind(&staging)
    .bind(vec![7_u8; 32])
    .execute(scoped.connection())
    .await?;
    // A registered key: its row names it, so it is never an orphan.
    let registered_bytes = format!("registered {run}").into_bytes();
    let registered_sha: [u8; 32] = Sha256::digest(&registered_bytes).into();
    let registered = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Asset,
        object_key: keys::asset(tenant(), &registered_sha),
        sha256: registered_sha,
        size_bytes: i64::try_from(registered_bytes.len())?,
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    };
    register_object(&mut scoped, "figcraft", &registered).await?;
    scoped.commit().await?;
    store.put_object(&staging, b"staged".to_vec(), "image/png").await?;
    store.put_object(&registered.object_key, registered_bytes, "application/octet-stream").await?;
    assert_eq!(versions(&store, &orphan).await?, 2);

    // Fresh keys are too young: the minimum age is never lowered, and nothing is touched.
    let fresh = sweep(&pool, &store, false, Duration::ZERO).await?;
    assert_eq!(fresh.min_age, MIN_COLLECTION_AGE);
    assert!(!named(&fresh, &orphan) && fresh.too_young >= 1);
    assert_eq!(versions(&store, &orphan).await?, 2);

    // A dry run two days later lists the orphan and changes nothing.
    let dry = sweep(&pool, &store, true, SHIFT).await?;
    assert!(dry.dry_run && dry.listing_complete);
    assert!(dry.orphans.iter().any(|candidate| candidate.object_key == orphan && !candidate.deleted_row));
    assert!(dry.removed.is_empty() && dry.versions_removed == 0);
    assert!(dry.protected.contains(&(staging.clone(), OrphanProtection::UploadReservation)));
    assert!(!named(&dry, &staging) && !named(&dry, &registered.object_key));
    assert!(dry.registered >= 1);
    assert_eq!(versions(&store, &orphan).await?, 2);

    // A real sweep removes every version of the orphan and leaves the protected keys alone.
    let real = sweep(&pool, &store, false, SHIFT).await?;
    assert!(real.removed.contains(&orphan), "{real:?}");
    assert!(real.versions_removed >= 2);
    assert_eq!(versions(&store, &orphan).await?, 0);
    assert!(!named(&real, &staging) && !named(&real, &registered.object_key));
    assert_eq!(versions(&store, &staging).await?, 1);
    assert_eq!(versions(&store, &registered.object_key).await?, 1);

    // A rerun has nothing of ours left to remove.
    let rerun = sweep(&pool, &store, false, SHIFT).await?;
    assert!(!named(&rerun, &orphan));

    // Once the reservation is closed its staged bytes are an orphan like any other.
    let mut scoped = begin(&pool, tenant()).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("UPDATE upload_reservations SET state = 'expired' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(reservation.to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;
    let closed = sweep(&pool, &store, false, SHIFT).await?;
    assert!(closed.removed.contains(&staging), "{closed:?}");
    assert_eq!(versions(&store, &staging).await?, 0);

    // Clean up the registered key's bytes; its row goes with the fixture cleanup.
    remove_all_versions(&store, &registered.object_key).await.map_err(|failure| format!("{failure:?}"))?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn bad_orphan_options_are_refused_before_any_io() {
    let store = ObjectStore::new(&ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".into(),
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: "FCaccess".into(),
        secret_key: "secret-value-123".into(),
        path_style: true,
    })
    .unwrap();
    let pool = PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").unwrap();
    for (limit, max_keys) in [(0, 1), (MAX_GC_BATCH + 1, 1), (1, 0), (1, MAX_ORPHAN_KEYS + 1)] {
        let options = OrphanOptions { dry_run: true, limit, min_age: MIN_COLLECTION_AGE, max_keys };
        assert_eq!(collect_orphans(&pool, &store, tenant(), options).await, Err(GcError::InvalidOptions));
    }
    assert!(OrphanOptions::default().dry_run);
    assert_eq!(remove_old_versions(&store, "a/../b", 0).await, Err(GcFailure::UnsignableKey));
}

#[test]
fn listing_timestamps_parse_and_junk_does_not() {
    assert_eq!(parse_timestamp("2026-10-17T10:00:00.000Z"), Some(1_792_231_200));
    for junk in ["", "yesterday", "2026-10-17", "2026-13-01T00:00:00Z", &"9".repeat(65)] {
        assert_eq!(parse_timestamp(junk), None, "{junk}");
    }
    let body = format!(
        concat!(
            r#"<?xml version="1.0"?><ListVersionsResult><Version><Key>k</Key><VersionId>v1</VersionId>"#,
            "<LastModified>2026-10-17T10:00:00.000Z</LastModified></Version>",
            "<DeleteMarker><Key>k</Key><VersionId>m1</VersionId><LastModified>junk</LastModified></DeleteMarker>{}</ListVersionsResult>"
        ),
        ""
    );
    assert_eq!(
        parse_version_entries(body.as_bytes(), "k"),
        Ok(vec![VersionEntry { id: "v1".into(), last_modified: Some(1_792_231_200) }, VersionEntry { id: "m1".into(), last_modified: None }])
    );
}
