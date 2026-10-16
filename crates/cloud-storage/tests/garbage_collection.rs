//! S3-015: garbage collection against PostgreSQL and a versioned S3-compatible bucket.
//!
//! Each test owns one fixture tenant (`deploy/saas/garbage-collection-preflight.sql`), so a pass
//! only ever sees that test's objects. Objects get unique per-run keys and bytes, are backdated
//! with plain SQL where age matters, and every key's versions are counted with an independent,
//! hand-signed `ListObjectVersions` request rather than the code under test. Each test ends by
//! collecting what it created, so the shared bucket keeps nothing from a passing run.

use std::collections::HashMap;
use std::time::Duration;

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::garbage_collection::{GcFailure, GcOptions, GcReport, MAX_GC_BATCH, Protection, collect_garbage};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, ObjectState, get_object, keys, register_object};
use figcraft_cloud_storage::object_references::{
    DeletionStart, MIN_COLLECTION_AGE, OwnerKind, add_reference, begin_object_deletion, place_retention_hold, release_retention_hold,
    remove_reference,
};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreError, ObjectStoreSettings};
use figcraft_cloud_storage::uploads::{ReservationState, get_reservation, reserve_upload};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use rusty_s3::{Bucket, Credentials, Method, UrlStyle};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const AGE_TENANT: &str = "ab000000-0000-4000-8000-000000000001";
/// The fixture principal that places retention holds.
const PLACER: &str = "ab000000-0000-4000-8000-000000000009";
const POINTER_TENANT: &str = "ab000000-0000-4000-8000-000000000002";
const EXPORT_TENANT: &str = "ab000000-0000-4000-8000-000000000003";
const CRASH_TENANT: &str = "ab000000-0000-4000-8000-000000000004";
const STAGING_TENANT: &str = "ab000000-0000-4000-8000-000000000005";
const BYSTANDER_TENANT: &str = "ab000000-0000-4000-8000-000000000006";
const PRINCIPAL: &str = "ab000000-0000-4000-8000-000000000009";
const POINTER_DOCUMENT: &str = "ab000000-0000-4000-8000-000000000023";
const POINTER_LIBRARY: &str = "ab000000-0000-4000-8000-000000000024";
const POINTER_PLUGIN: &str = "ab000000-0000-4000-8000-000000000025";
const EXPORT_DOCUMENT: &str = "ab000000-0000-4000-8000-000000000033";
const BUCKET: &str = "figcraft";
const OLD: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const TTL: Duration = Duration::from_secs(60);

struct Probe {
    pool: PgPool,
    settings: ObjectStoreSettings,
    store: ObjectStore,
    raw: Raw,
}

async fn probe() -> TestResult<Option<Probe>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; garbage collection probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?;
    let read =
        |name: &str| -> TestResult<String> { Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string()) };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: BUCKET.into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    let store = ObjectStore::new(&settings)?;
    let raw = Raw::new(&settings)?;
    Ok(Some(Probe { pool, settings, store, raw }))
}

/// Independent signed access: counts a key's versions and delete markers.
struct Raw {
    bucket: Bucket,
    credentials: Credentials,
    client: reqwest::Client,
}

impl Raw {
    fn new(settings: &ObjectStoreSettings) -> TestResult<Self> {
        let bucket = Bucket::new(settings.endpoint.parse()?, UrlStyle::Path, settings.bucket.clone(), settings.region.clone())?;
        Ok(Self { bucket, credentials: Credentials::new(settings.access_key.clone(), settings.secret_key.clone()), client: client()? })
    }

    /// How many versions and delete markers exactly `key` has.
    async fn versions(&self, key: &str) -> TestResult<usize> {
        let url = rusty_s3::signing::sign(
            &jiff::Timestamp::now(),
            Method::Get,
            self.bucket.base_url().clone(),
            self.credentials.key(),
            self.credentials.secret(),
            None,
            self.bucket.region(),
            TTL.as_secs(),
            [("max-keys", "1000"), ("prefix", key), ("versions", "")].into_iter(),
            std::iter::empty(),
        );
        let response = self.client.get(url.as_str()).send().await?;
        assert_eq!(response.status().as_u16(), 200, "ListObjectVersions should succeed");
        let body = response.text().await?;
        assert!(body.contains("<ListVersionsResult"), "{body}");
        Ok(body.matches(&format!("<Key>{key}</Key>")).count())
    }
}

/// `reqwest::Client::new()` panics under `rustls-no-provider`, so the provider is explicit.
fn client() -> TestResult<reqwest::Client> {
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    Ok(reqwest::Client::builder().tls_backend_preconfigured(tls).build()?)
}

async fn begin<'a>(pool: &'a PgPool, tenant: &str) -> TestResult<TenantTransaction<'a>> {
    let tenant: TenantId = tenant.parse()?;
    Ok(begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?)
}

/// An object in `tenant`: a row in `state` created `age` ago, with `versions` uploads of fresh bytes
/// under its unique key (none: a row with no bytes at all).
struct Made {
    id: Uuid,
    key: String,
    sha256: [u8; 32],
}

async fn object(probe: &Probe, tenant: &str, kind: ObjectKind, state: &str, age: Duration, versions: usize) -> TestResult<Made> {
    let label = Uuid::new_v4();
    let bytes = |version: usize| format!("figcraft gc probe {label} v{version}").into_bytes();
    let sha256: [u8; 32] = Sha256::digest(bytes(0)).into();
    let mut scoped = begin(&probe.pool, tenant).await?;
    let object = NewObject {
        id: Uuid::new_v4(),
        kind,
        object_key: keys::asset(scoped.tenant_id(), &sha256),
        sha256,
        size_bytes: i64::try_from(bytes(0).len())?,
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    };
    register_object(&mut scoped, BUCKET, &object).await?;
    sqlx::query(concat!(
        "UPDATE objects SET state = $3, created_at = now() - $4::bigint * interval '1 second', ",
        "verified_at = CASE WHEN $3 = 'verified' THEN now() END, ",
        "deleted_at = CASE WHEN $3 IN ('deleting', 'deleted') THEN now() END ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid"
    ))
    .bind(scoped.tenant_id().to_string())
    .bind(object.id.to_string())
    .bind(state)
    .bind(i64::try_from(age.as_secs())?)
    .execute(scoped.connection())
    .await?;
    scoped.commit().await?;
    for version in 0..versions {
        probe.store.put_object(&object.object_key, bytes(version), "application/octet-stream").await?;
    }
    Ok(Made { id: object.id, key: object.object_key, sha256 })
}

async fn state(probe: &Probe, tenant: &str, id: Uuid) -> TestResult<Option<ObjectState>> {
    let mut scoped = begin(&probe.pool, tenant).await?;
    let state = get_object(&mut scoped, id).await?.map(|object| object.state);
    scoped.rollback().await?;
    Ok(state)
}

async fn backdate(probe: &Probe, tenant: &str, id: Uuid) -> TestResult {
    let mut scoped = begin(&probe.pool, tenant).await?;
    sqlx::query("UPDATE objects SET created_at = now() - interval '2 days' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant)
        .bind(id.to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;
    Ok(())
}

async fn run(probe: &Probe, tenant: &str, dry_run: bool) -> TestResult<GcReport> {
    run_with(probe, &probe.store, tenant, GcOptions { dry_run, limit: MAX_GC_BATCH, min_age: MIN_COLLECTION_AGE }).await
}

async fn run_with(probe: &Probe, store: &ObjectStore, tenant: &str, options: GcOptions) -> TestResult<GcReport> {
    Ok(collect_garbage(&probe.pool, store, tenant.parse()?, options).await?)
}

fn ids(objects: &[figcraft_cloud_storage::object_references::CollectableObject]) -> Vec<Uuid> {
    objects.iter().map(|object| object.id).collect()
}

fn protected(report: &GcReport) -> HashMap<Uuid, Protection> {
    report.protected.iter().copied().collect()
}

/// Nothing in the report names `made`.
fn untouched(report: &GcReport, made: &Made) -> bool {
    !ids(&report.candidates).contains(&made.id)
        && !ids(&report.resumed).contains(&made.id)
        && !report.deleted.contains(&made.id)
        && !report.failed.iter().any(|(id, _)| *id == made.id)
}

#[tokio::test]
async fn age_references_and_holds_protect_and_a_dry_run_changes_nothing() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let fresh = object(&probe, AGE_TENANT, ObjectKind::Asset, "verified", Duration::ZERO, 1).await?;
    let referenced = object(&probe, AGE_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?;
    let held = object(&probe, AGE_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?;
    // Two versions behind a delete marker: a plain DELETE would leave all three entries.
    let layered = object(&probe, AGE_TENANT, ObjectKind::Asset, "verified", OLD, 2).await?;
    probe.store.delete_object(&layered.key).await?;
    let rejected = object(&probe, AGE_TENANT, ObjectKind::Asset, "rejected", OLD, 1).await?;
    let unwritten = object(&probe, AGE_TENANT, ObjectKind::Asset, "pending", OLD, 0).await?;
    let bystander = object(&probe, BYSTANDER_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?;
    let (document, hold) = (Uuid::new_v4(), Uuid::new_v4());
    let mut scoped = begin(&probe.pool, AGE_TENANT).await?;
    add_reference(&mut scoped, referenced.id, OwnerKind::Document, document).await?;
    place_retention_hold(&mut scoped, held.id, hold, PLACER.parse()?, "S3 probe hold").await?;
    scoped.commit().await?;
    assert_eq!(probe.raw.versions(&layered.key).await?, 3);

    // A dry run lists exactly the old unreferenced objects and changes nothing.
    let dry = run(&probe, AGE_TENANT, true).await?;
    assert!(dry.dry_run);
    for made in [&layered, &rejected, &unwritten] {
        assert!(ids(&dry.candidates).contains(&made.id));
    }
    for made in [&fresh, &referenced, &held, &bystander] {
        assert!(untouched(&dry, made));
    }
    assert!(dry.deleted.is_empty() && dry.failed.is_empty() && dry.versions_removed == 0);
    assert_eq!(state(&probe, AGE_TENANT, layered.id).await?, Some(ObjectState::Verified));
    assert_eq!(state(&probe, AGE_TENANT, rejected.id).await?, Some(ObjectState::Rejected));
    assert_eq!(state(&probe, AGE_TENANT, unwritten.id).await?, Some(ObjectState::Pending));
    assert_eq!(probe.raw.versions(&layered.key).await?, 3);

    // The minimum age is never lowered, and a longer one protects the two-day-old objects.
    let zero = run_with(&probe, &probe.store, AGE_TENANT, GcOptions { dry_run: true, limit: 10, min_age: Duration::ZERO }).await?;
    assert_eq!(zero.min_age, MIN_COLLECTION_AGE);
    assert!(untouched(&zero, &fresh));
    let longer = run_with(&probe, &probe.store, AGE_TENANT, GcOptions { dry_run: true, limit: 10, min_age: OLD * 2 }).await?;
    assert_eq!(longer.min_age, OLD * 2);
    for made in [&layered, &rejected, &unwritten] {
        assert!(untouched(&longer, made));
    }

    // A real pass honours its limit; the next one finishes the batch.
    let first = run_with(&probe, &probe.store, AGE_TENANT, GcOptions { dry_run: false, limit: 1, min_age: MIN_COLLECTION_AGE }).await?;
    assert_eq!((first.candidates.len(), first.deleted.len()), (1, 1));
    let second = run(&probe, AGE_TENANT, false).await?;
    assert_eq!(second.deleted.len(), 2);
    for made in [&layered, &rejected, &unwritten] {
        assert!(first.deleted.contains(&made.id) || second.deleted.contains(&made.id));
        assert_eq!(state(&probe, AGE_TENANT, made.id).await?, Some(ObjectState::Deleted));
        assert_eq!(probe.raw.versions(&made.key).await?, 0, "every version and delete marker is gone");
    }
    assert!(first.versions_removed + second.versions_removed >= 4);
    let mut scoped = begin(&probe.pool, AGE_TENANT).await?;
    let deleted_at_kept: bool =
        sqlx::query_scalar("SELECT bool_and(deleted_at IS NOT NULL) FROM objects WHERE tenant_id = $1::uuid AND id = ANY ($2::text[]::uuid[])")
            .bind(AGE_TENANT)
            .bind(vec![layered.id.to_string(), rejected.id.to_string(), unwritten.id.to_string()])
            .fetch_one(scoped.connection())
            .await?;
    scoped.rollback().await?;
    assert!(deleted_at_kept);
    for (made, expected) in [(&fresh, ObjectState::Verified), (&referenced, ObjectState::Verified), (&held, ObjectState::Verified)] {
        assert_eq!(state(&probe, AGE_TENANT, made.id).await?, Some(expected));
        assert_eq!(probe.raw.versions(&made.key).await?, 1);
    }
    assert_eq!(state(&probe, BYSTANDER_TENANT, bystander.id).await?, Some(ObjectState::Verified));
    assert_eq!(probe.raw.versions(&bystander.key).await?, 1);

    // A rerun has nothing left to do.
    let rerun = run(&probe, AGE_TENANT, false).await?;
    assert!(rerun.deleted.is_empty() && rerun.candidates.is_empty() && rerun.resumed.is_empty());

    // Releasing the reference and hold and ageing the fresh object makes all three collectable.
    let mut scoped = begin(&probe.pool, AGE_TENANT).await?;
    remove_reference(&mut scoped, referenced.id, OwnerKind::Document, document).await?;
    release_retention_hold(&mut scoped, held.id, hold).await?;
    scoped.commit().await?;
    backdate(&probe, AGE_TENANT, fresh.id).await?;
    let released = run(&probe, AGE_TENANT, false).await?;
    for made in [&fresh, &referenced, &held] {
        assert!(released.deleted.contains(&made.id));
        assert_eq!(probe.raw.versions(&made.key).await?, 0);
    }
    let bystander_pass = run(&probe, BYSTANDER_TENANT, false).await?;
    assert!(bystander_pass.deleted.contains(&bystander.id));
    assert_eq!(probe.raw.versions(&bystander.key).await?, 0);
    Ok(())
}

#[tokio::test]
async fn pointer_rows_jobs_and_reservations_protect_objects_without_references() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let mut made = HashMap::new();
    for name in ["current", "snapshot", "library", "plugin", "export", "thumbnail", "reservation", "job_id", "job_key", "control"] {
        made.insert(name, object(&probe, POINTER_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?);
    }
    let id = |name: &str| made.get(name).map(|made| made.id.to_string()).unwrap_or_default();
    let run_id = Uuid::new_v4();
    let seq = i64::from(u16::from_be_bytes([run_id.as_bytes()[0], run_id.as_bytes()[1]]));
    let (current_snapshot, other_snapshot, job, reservation) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let digest = vec![0xab_u8; 32];
    let mut scoped = begin(&probe.pool, POINTER_TENANT).await?;
    for (snapshot, name) in [(current_snapshot, "current"), (other_snapshot, "snapshot")] {
        sqlx::query(concat!(
            "INSERT INTO document_snapshots (id, tenant_id, document_id, seq, object_id, checksum, size_bytes) ",
            "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5::uuid, $6, 1)"
        ))
        .bind(snapshot.to_string())
        .bind(POINTER_TENANT)
        .bind(POINTER_DOCUMENT)
        .bind(seq)
        .bind(id(name))
        .bind(&digest)
        .execute(scoped.connection())
        .await?;
    }
    sqlx::query("UPDATE documents SET current_snapshot_id = $3::uuid WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(POINTER_TENANT)
        .bind(POINTER_DOCUMENT)
        .bind(current_snapshot.to_string())
        .execute(scoped.connection())
        .await?;
    sqlx::query(concat!(
        "INSERT INTO library_versions (id, tenant_id, library_id, version, object_id, sha256, published_by) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5::uuid, $6, $7::uuid)"
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(POINTER_TENANT)
    .bind(POINTER_LIBRARY)
    .bind(seq + 1)
    .bind(id("library"))
    .bind(&digest)
    .bind(PRINCIPAL)
    .execute(scoped.connection())
    .await?;
    sqlx::query(concat!(
        "INSERT INTO plugin_versions (id, tenant_id, plugin_id, version, object_id, sha256, manifest) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5::uuid, $6, '{}'::jsonb)"
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(POINTER_TENANT)
    .bind(POINTER_PLUGIN)
    .bind(format!("1.0.{seq}"))
    .bind(id("plugin"))
    .bind(&digest)
    .execute(scoped.connection())
    .await?;
    sqlx::query(concat!(
        "INSERT INTO exports (id, tenant_id, document_id, requested_by, requested_seq, format, result_object_id, state, completed_at) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 0, 'png', $5::uuid, 'succeeded', now())"
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(POINTER_TENANT)
    .bind(POINTER_DOCUMENT)
    .bind(PRINCIPAL)
    .bind(id("export"))
    .execute(scoped.connection())
    .await?;
    sqlx::query("INSERT INTO document_thumbnails (tenant_id, document_id, seq, object_id) VALUES ($1::uuid, $2::uuid, $3, $4::uuid)")
        .bind(POINTER_TENANT)
        .bind(POINTER_DOCUMENT)
        .bind(seq)
        .bind(id("thumbnail"))
        .execute(scoped.connection())
        .await?;
    // A live reservation for the same kind and digest would complete onto the object.
    let reserved = made.get("reservation").ok_or("reservation object")?;
    sqlx::query(concat!(
        "INSERT INTO upload_reservations (id, tenant_id, principal_id, kind, staging_key, expected_sha256, expected_size, media_type, expires_at) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, 'asset', $4, $5, 1, 'image/png', now() + interval '10 minutes')"
    ))
    .bind(reservation.to_string())
    .bind(POINTER_TENANT)
    .bind(PRINCIPAL)
    .bind(keys::staging(POINTER_TENANT.parse()?, reservation))
    .bind(reserved.sha256.as_slice())
    .execute(scoped.connection())
    .await?;
    let key_named = made.get("job_key").ok_or("job_key object")?;
    sqlx::query("INSERT INTO jobs (id, tenant_id, kind, input) VALUES ($1::uuid, $2::uuid, 'probe', $3::jsonb)")
        .bind(job.to_string())
        .bind(POINTER_TENANT)
        .bind(serde_json::json!({ "object": id("job_id"), "nested": { "keys": [key_named.key] } }).to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;

    let expected = [
        ("current", Protection::CurrentSnapshot),
        ("snapshot", Protection::Snapshot),
        ("library", Protection::LibraryVersion),
        ("plugin", Protection::PluginVersion),
        ("export", Protection::ExportResult),
        ("thumbnail", Protection::Thumbnail),
        ("reservation", Protection::UploadReservation),
        ("job_id", Protection::ActiveJob),
        ("job_key", Protection::ActiveJob),
    ];
    let control = made.get("control").ok_or("control object")?;
    for dry_run in [true, false] {
        let report = run(&probe, POINTER_TENANT, dry_run).await?;
        let protected = protected(&report);
        for (name, protection) in expected {
            let object = made.get(name).ok_or("object")?;
            assert_eq!(protected.get(&object.id), Some(&protection), "{name}");
            assert!(untouched(&report, object), "{name}");
            assert_eq!(state(&probe, POINTER_TENANT, object.id).await?, Some(ObjectState::Verified));
            assert_eq!(probe.raw.versions(&object.key).await?, 1);
        }
        assert!(ids(&report.candidates).contains(&control.id));
        assert_eq!(report.deleted.contains(&control.id), !dry_run);
    }
    assert_eq!(probe.raw.versions(&control.key).await?, 0);

    // Once nothing names them any more, they are collected.
    let mut scoped = begin(&probe.pool, POINTER_TENANT).await?;
    for statement in [
        "UPDATE documents SET current_snapshot_id = NULL WHERE tenant_id = $1::uuid",
        "DELETE FROM document_snapshots WHERE tenant_id = $1::uuid",
        "DELETE FROM library_versions WHERE tenant_id = $1::uuid",
        "DELETE FROM plugin_versions WHERE tenant_id = $1::uuid",
        "DELETE FROM exports WHERE tenant_id = $1::uuid",
        "DELETE FROM document_thumbnails WHERE tenant_id = $1::uuid",
        "UPDATE upload_reservations SET state = 'expired' WHERE tenant_id = $1::uuid",
        "UPDATE jobs SET state = 'succeeded' WHERE tenant_id = $1::uuid",
    ] {
        sqlx::query(statement).bind(POINTER_TENANT).execute(scoped.connection()).await?;
    }
    scoped.commit().await?;
    let released = run(&probe, POINTER_TENANT, false).await?;
    for (name, _) in expected {
        let object = made.get(name).ok_or("object")?;
        assert!(released.deleted.contains(&object.id), "{name}");
        assert_eq!(probe.raw.versions(&object.key).await?, 0);
    }
    Ok(())
}

#[tokio::test]
async fn an_active_export_pauses_snapshot_and_asset_collection() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let asset = object(&probe, EXPORT_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?;
    let thumbnail = object(&probe, EXPORT_TENANT, ObjectKind::Thumbnail, "verified", OLD, 1).await?;
    let export = Uuid::new_v4();
    let mut scoped = begin(&probe.pool, EXPORT_TENANT).await?;
    sqlx::query(concat!(
        "INSERT INTO exports (id, tenant_id, document_id, requested_by, requested_seq, format) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 0, 'pdf')"
    ))
    .bind(export.to_string())
    .bind(EXPORT_TENANT)
    .bind(EXPORT_DOCUMENT)
    .bind(PRINCIPAL)
    .execute(scoped.connection())
    .await?;
    scoped.commit().await?;

    let report = run(&probe, EXPORT_TENANT, false).await?;
    assert_eq!(protected(&report).get(&asset.id), Some(&Protection::ActiveExport));
    assert!(report.deleted.contains(&thumbnail.id));
    assert_eq!(probe.raw.versions(&asset.key).await?, 1);
    assert_eq!(probe.raw.versions(&thumbnail.key).await?, 0);

    let mut scoped = begin(&probe.pool, EXPORT_TENANT).await?;
    sqlx::query("UPDATE exports SET state = 'failed', error_code = 'probe', completed_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(EXPORT_TENANT)
        .bind(export.to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;
    let finished = run(&probe, EXPORT_TENANT, false).await?;
    assert!(finished.deleted.contains(&asset.id));
    assert_eq!(probe.raw.versions(&asset.key).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_crash_between_steps_leaves_deleting_rows_that_a_rerun_finishes() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    // Crashed after marking: still `deleting` with every version in place.
    let marked = object(&probe, CRASH_TENANT, ObjectKind::Asset, "verified", OLD, 2).await?;
    // Crashed part-way through removing bytes: a plain delete added a marker over two versions.
    let partial = object(&probe, CRASH_TENANT, ObjectKind::Asset, "verified", OLD, 2).await?;
    // Marked, but a queued job names it again: its bytes are kept for an operator.
    let renamed = object(&probe, CRASH_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?;
    let fresh_candidate = object(&probe, CRASH_TENANT, ObjectKind::Asset, "verified", OLD, 1).await?;
    let mut scoped = begin(&probe.pool, CRASH_TENANT).await?;
    for made in [&marked, &partial, &renamed] {
        assert!(matches!(begin_object_deletion(&mut scoped, made.id).await?, DeletionStart::Marked(_)));
    }
    let job = Uuid::new_v4();
    sqlx::query("INSERT INTO jobs (id, tenant_id, kind, input) VALUES ($1::uuid, $2::uuid, 'probe', $3::jsonb)")
        .bind(job.to_string())
        .bind(CRASH_TENANT)
        .bind(serde_json::json!({ "object_id": renamed.id.to_string() }).to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;
    probe.store.delete_object(&partial.key).await?;
    assert_eq!(probe.raw.versions(&partial.key).await?, 3);

    let dry = run(&probe, CRASH_TENANT, true).await?;
    for made in [&marked, &partial, &renamed] {
        assert!(ids(&dry.resumed).contains(&made.id));
        assert_eq!(state(&probe, CRASH_TENANT, made.id).await?, Some(ObjectState::Deleting));
    }
    assert!(ids(&dry.candidates).contains(&fresh_candidate.id));
    assert_eq!(probe.raw.versions(&marked.key).await?, 2);

    // The store fails mid-pass: every object it touched stays `deleting` with its bytes.
    let unreachable = ObjectStore::new(&ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..probe.settings.clone() })?;
    let broken = run_with(&probe, &unreachable, CRASH_TENANT, GcOptions { dry_run: false, limit: MAX_GC_BATCH, min_age: MIN_COLLECTION_AGE }).await?;
    let failed: HashMap<Uuid, GcFailure> = broken.failed.iter().copied().collect();
    for made in [&marked, &partial, &fresh_candidate] {
        assert_eq!(failed.get(&made.id), Some(&GcFailure::Store(ObjectStoreError::Unreachable)));
        assert_eq!(state(&probe, CRASH_TENANT, made.id).await?, Some(ObjectState::Deleting));
    }
    assert_eq!(failed.get(&renamed.id), Some(&GcFailure::Protected(Protection::ActiveJob)));
    assert!(broken.deleted.is_empty());
    assert_eq!(probe.raw.versions(&fresh_candidate.key).await?, 1);

    // The rerun finishes them all.
    let rerun = run(&probe, CRASH_TENANT, false).await?;
    for made in [&marked, &partial, &fresh_candidate] {
        assert!(ids(&rerun.resumed).contains(&made.id));
        assert!(rerun.deleted.contains(&made.id));
        assert_eq!(state(&probe, CRASH_TENANT, made.id).await?, Some(ObjectState::Deleted));
        assert_eq!(probe.raw.versions(&made.key).await?, 0);
    }
    assert_eq!(state(&probe, CRASH_TENANT, renamed.id).await?, Some(ObjectState::Deleting));
    assert_eq!(probe.raw.versions(&renamed.key).await?, 1);

    // Once the job is done, the protected `deleting` object is finished too; then nothing is left.
    let mut scoped = begin(&probe.pool, CRASH_TENANT).await?;
    sqlx::query("UPDATE jobs SET state = 'succeeded' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(CRASH_TENANT)
        .bind(job.to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;
    let finished = run(&probe, CRASH_TENANT, false).await?;
    assert!(finished.deleted.contains(&renamed.id));
    assert_eq!(probe.raw.versions(&renamed.key).await?, 0);
    let idle = run(&probe, CRASH_TENANT, false).await?;
    assert!(idle.resumed.is_empty() && idle.candidates.is_empty() && idle.deleted.is_empty() && idle.failed.is_empty());
    Ok(())
}

#[tokio::test]
async fn expired_staging_uploads_are_swept_with_every_version() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let bytes = format!("figcraft gc staging probe {}", Uuid::new_v4()).into_bytes();
    let mut scoped = begin(&probe.pool, STAGING_TENANT).await?;
    let reserved = reserve_upload(
        &mut scoped,
        &probe.store,
        principal,
        ObjectKind::Asset,
        Sha256::digest(&bytes).into(),
        i64::try_from(bytes.len())?,
        "image/png",
        Duration::from_secs(120),
    )
    .await?;
    scoped.commit().await?;
    let client = client()?;
    for _ in 0..2 {
        let mut request = client.put(&reserved.url).body(bytes.clone());
        for (name, value) in &reserved.headers {
            request = request.header(*name, *value);
        }
        assert!(request.send().await?.status().is_success());
    }
    let key = reserved.reservation.staging_key.clone();
    assert_eq!(probe.raw.versions(&key).await?, 2);
    let mut scoped = begin(&probe.pool, STAGING_TENANT).await?;
    sqlx::query("UPDATE upload_reservations SET expires_at = created_at + interval '1 microsecond' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(STAGING_TENANT)
        .bind(reserved.reservation.id.to_string())
        .execute(scoped.connection())
        .await?;
    scoped.commit().await?;

    let reservation_state = |probe: &Probe| {
        let id = reserved.reservation.id;
        let pool = probe.pool.clone();
        async move {
            let mut scoped = begin(&pool, STAGING_TENANT).await?;
            let state = get_reservation(&mut scoped, id).await?.map(|reservation| reservation.state);
            scoped.rollback().await?;
            TestResult::Ok(state)
        }
    };
    let dry = run(&probe, STAGING_TENANT, true).await?;
    assert!(dry.staging_keys.contains(&key));
    assert_eq!(reservation_state(&probe).await?, Some(ReservationState::Reserved));
    assert_eq!(probe.raw.versions(&key).await?, 2);

    let swept = run(&probe, STAGING_TENANT, false).await?;
    assert!(swept.staging_keys.contains(&key));
    assert!(swept.staging_failed.is_empty());
    assert!(swept.versions_removed >= 2);
    assert_eq!(reservation_state(&probe).await?, Some(ReservationState::Expired));
    assert_eq!(probe.raw.versions(&key).await?, 0);
    assert!(!run(&probe, STAGING_TENANT, false).await?.staging_keys.contains(&key));
    Ok(())
}
