//! S3-016: the database-to-bucket reconciliation report against PostgreSQL and SeaweedFS.
//!
//! Each discrepancy is planted under the fixture tenant's prefix with unique per-run content:
//! raw puts of orphan and staging keys, a verified row without bytes, a verified row whose bytes
//! are overwritten with a different length, a key whose only row is `deleted`, stuck `pending` and
//! `deleting` rows, and expired and open reservations. The test asserts each classification,
//! proves the report changes neither rows nor the bucket listing and that its transaction refuses
//! writes, and checks that caps mark the report incomplete without inventing findings. Leftovers
//! from an interrupted earlier run are removed first, so reruns see only this run's plants.

use std::collections::BTreeSet;
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, TenantId, Uuid};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, Verification, keys, register_object, verify_object};
use figcraft_cloud_storage::object_references::{OwnerKind, add_reference, place_retention_hold};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::reconciliation::{
    Cap, OrphanReason, REPORT_FORMAT_VERSION, ReconciliationError, ReconciliationLimits, ReconciliationReport, RowState, Severity, StagingReason,
    reconcile_tenant,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const TENANT: &str = "ac000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "ac000000-0000-4000-8000-000000000002";
const PRINCIPAL: &str = "ac000000-0000-4000-8000-000000000009";
const BUCKET: &str = "figcraft";
const MEDIA: &str = "image/png";
const HOUR: i64 = 60 * 60;

struct Probe {
    pool: PgPool,
    store: ObjectStore,
    settings: ObjectStoreSettings,
}

async fn probe() -> TestResult<Option<Probe>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; reconciliation probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
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
    Ok(Some(Probe { pool, store, settings }))
}

async fn begin<'a>(pool: &'a PgPool, tenant: &str) -> TestResult<TenantTransaction<'a>> {
    let tenant: TenantId = tenant.parse()?;
    begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

fn unique(label: &str) -> Vec<u8> {
    format!("figcraft reconciliation probe {label} {}", Uuid::new_v4()).into_bytes()
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn tenant() -> TestResult<TenantId> {
    Ok(TENANT.parse()?)
}

/// An independent listing of a prefix (key → size) through `rusty-s3`'s own parser, so the
/// before/after comparison does not trust the code under test.
async fn raw_listing(settings: &ObjectStoreSettings, prefix: &str) -> TestResult<Vec<(String, u64)>> {
    let bucket = Bucket::new(settings.endpoint.parse()?, UrlStyle::Path, settings.bucket.clone(), settings.region.clone())?;
    let credentials = Credentials::new(settings.access_key.clone(), settings.secret_key.clone());
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let client = reqwest::Client::builder().tls_backend_preconfigured(tls).build()?;
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..1000 {
        let mut action = ListObjectsV2::new(&bucket, Some(&credentials));
        action.with_prefix(prefix);
        if let Some(token) = &token {
            action.with_continuation_token(token.clone());
        }
        let body = client.get(action.sign(Duration::from_secs(60)).as_str()).send().await?.error_for_status()?.text().await?;
        let parsed = ListObjectsV2::parse_response(&body).map_err(|error| format!("listing: {error:?}"))?;
        keys.extend(parsed.contents.into_iter().map(|content| (content.key, content.size)));
        match parsed.next_continuation_token {
            Some(next) => token = Some(next),
            None => {
                keys.sort();
                return Ok(keys);
            }
        }
    }
    Err("listing never ended".into())
}

type RowSnapshot = Vec<(String, String, i64, String, Option<String>, Option<String>)>;

async fn row_snapshot(pool: &PgPool) -> TestResult<(RowSnapshot, Vec<(String, String)>, i64)> {
    let rows: RowSnapshot = sqlx::query_as(concat!(
        "SELECT id::text, state, size_bytes, created_at::text, verified_at::text, deleted_at::text FROM objects ",
        "WHERE tenant_id = $1::uuid ORDER BY id"
    ))
    .bind(TENANT)
    .fetch_all(pool)
    .await?;
    let reservations: Vec<(String, String)> =
        sqlx::query_as("SELECT id::text, state FROM upload_reservations WHERE tenant_id = $1::uuid ORDER BY id").bind(TENANT).fetch_all(pool).await?;
    let references: i64 =
        sqlx::query_scalar("SELECT count(*) FROM object_references WHERE tenant_id = $1::uuid").bind(TENANT).fetch_one(pool).await?;
    Ok((rows, reservations, references))
}

/// Remove an interrupted earlier run's rows and bytes for both fixture tenants.
async fn clear(probe: &Probe) -> TestResult {
    for tenant in [TENANT, OTHER_TENANT] {
        for statement in [
            "DELETE FROM upload_reservations WHERE tenant_id = $1::uuid",
            "DELETE FROM object_references WHERE tenant_id = $1::uuid",
            "DELETE FROM objects WHERE tenant_id = $1::uuid",
        ] {
            sqlx::query(statement).bind(tenant).execute(&probe.pool).await?;
        }
        for (key, _) in raw_listing(&probe.settings, &format!("tenants/{tenant}/")).await? {
            probe.store.delete_object(&key).await?;
        }
    }
    Ok(())
}

async fn register(transaction: &mut TenantTransaction<'_>, key: &str, bytes: &[u8], kind: ObjectKind) -> TestResult<Uuid> {
    let object = NewObject {
        id: Uuid::new_v4(),
        kind,
        object_key: key.to_string(),
        sha256: digest(bytes),
        size_bytes: i64::try_from(bytes.len())?,
        media_type: MEDIA.into(),
        producer_version: "figcraft-cloud test".into(),
    };
    register_object(transaction, BUCKET, &object).await?;
    Ok(object.id)
}

/// Force a row's state and ages (seconds before now) with plain SQL.
async fn force(pool: &PgPool, id: Uuid, state: &str, created_age: i64, deleted_age: Option<i64>) -> TestResult {
    sqlx::query(concat!(
        "UPDATE objects SET state = $2, created_at = now() - $3::bigint * interval '1 second', ",
        "verified_at = CASE WHEN $2 = 'verified' THEN now() - $3::bigint * interval '1 second' END, ",
        "deleted_at = CASE WHEN $4::bigint IS NULL THEN NULL ELSE now() - $4::bigint * interval '1 second' END ",
        "WHERE tenant_id = $1::uuid AND id = $5::uuid"
    ))
    .bind(TENANT)
    .bind(state)
    .bind(created_age)
    .bind(deleted_age)
    .bind(id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a reservation whose staging key is planted in the bucket; `expires_in` may be negative.
async fn reservation(pool: &PgPool, state: &str, expires_in: i64) -> TestResult<(Uuid, String)> {
    let id = Uuid::new_v4();
    let key = keys::staging(tenant()?, id);
    sqlx::query(concat!(
        "INSERT INTO upload_reservations (id, tenant_id, principal_id, kind, staging_key, expected_sha256, expected_size, media_type, ",
        "state, created_at, expires_at) VALUES ($1::uuid, $2::uuid, $3::uuid, 'asset', $4, $5, 4, 'image/png', $6, ",
        "now() - interval '2 hours', now() + $7::bigint * interval '1 second')"
    ))
    .bind(id.to_string())
    .bind(TENANT)
    .bind(PRINCIPAL)
    .bind(&key)
    .bind([7_u8; 32].as_slice())
    .bind(state)
    .bind(expires_in)
    .execute(pool)
    .await?;
    Ok((id, key))
}

struct Plants {
    healthy: Uuid,
    held: Uuid,
    missing: Uuid,
    mismatched: Uuid,
    mismatched_sizes: (i64, u64),
    stuck_pending: Uuid,
    stuck_deleting: Uuid,
    rejected_without_bytes: Uuid,
    orphan: String,
    deleted_only: String,
    unreserved_staging: String,
    expired_staging: (Uuid, String),
    closed_staging: (Uuid, String),
    open_staging: String,
    other_tenant_orphan: String,
    keys: Vec<String>,
}

async fn plant(probe: &Probe) -> TestResult<Plants> {
    let store = &probe.store;
    let pool = &probe.pool;
    let tenant = tenant()?;
    let mut keys_written = Vec::new();

    // A verified object with bytes and a reference: no finding at all.
    let healthy_bytes = unique("healthy");
    let healthy_key = keys::asset(tenant, &digest(&healthy_bytes));
    put(store, &mut keys_written, &healthy_key, healthy_bytes.clone()).await?;
    // A verified object kept only by a retention hold: not collectable.
    let held_bytes = unique("held");
    let held_key = keys::asset(tenant, &digest(&held_bytes));
    put(store, &mut keys_written, &held_key, held_bytes.clone()).await?;
    // A verified object whose bytes are then replaced by a longer body.
    let mismatched_bytes = unique("mismatch");
    let mismatched_key = keys::asset(tenant, &digest(&mismatched_bytes));
    put(store, &mut keys_written, &mismatched_key, mismatched_bytes.clone()).await?;

    let mut scoped = begin(pool, TENANT).await?;
    let healthy = register(&mut scoped, &healthy_key, &healthy_bytes, ObjectKind::Asset).await?;
    let held = register(&mut scoped, &held_key, &held_bytes, ObjectKind::Asset).await?;
    let mismatched = register(&mut scoped, &mismatched_key, &mismatched_bytes, ObjectKind::Asset).await?;
    for id in [healthy, held, mismatched] {
        assert!(matches!(verify_object(&mut scoped, store, id).await?, Some(Verification::Verified(_))));
    }
    add_reference(&mut scoped, healthy, OwnerKind::Asset, Uuid::new_v4()).await?;
    place_retention_hold(&mut scoped, held, Uuid::new_v4()).await?;

    // A row with no bytes, forced verified: critical.
    let missing_bytes = unique("missing");
    let missing = register(&mut scoped, &keys::asset(tenant, &digest(&missing_bytes)), &missing_bytes, ObjectKind::Asset).await?;
    // Stuck rows: pending two hours (no bytes) and deleting for two hours (bytes still present).
    let pending_bytes = unique("pending");
    let stuck_pending =
        register(&mut scoped, &keys::snapshot(tenant, CloudDocumentId::random(), 3, &digest(&pending_bytes)), &pending_bytes, ObjectKind::Snapshot)
            .await?;
    let deleting_bytes = unique("deleting");
    let deleting_key = keys::thumbnail(tenant, CloudDocumentId::random(), 9);
    let stuck_deleting = register(&mut scoped, &deleting_key, &deleting_bytes, ObjectKind::Thumbnail).await?;
    // A rejected row without bytes: expected, not reported missing; collectable.
    let rejected_bytes = unique("rejected");
    let rejected_without_bytes = register(&mut scoped, &keys::asset(tenant, &digest(&rejected_bytes)), &rejected_bytes, ObjectKind::Asset).await?;
    // A key whose only row is deleted.
    let deleted_bytes = unique("deleted");
    let deleted_only = keys::asset(tenant, &digest(&deleted_bytes));
    let deleted_row = register(&mut scoped, &deleted_only, &deleted_bytes, ObjectKind::Asset).await?;
    scoped.commit().await?;

    force(pool, missing, "verified", HOUR, None).await?;
    force(pool, stuck_pending, "pending", 2 * HOUR, None).await?;
    force(pool, stuck_deleting, "deleting", 3 * HOUR, Some(2 * HOUR)).await?;
    force(pool, rejected_without_bytes, "rejected", HOUR, None).await?;
    force(pool, deleted_row, "deleted", 3 * HOUR, Some(2 * HOUR)).await?;
    put(store, &mut keys_written, &deleting_key, deleting_bytes.clone()).await?;
    put(store, &mut keys_written, &deleted_only, deleted_bytes).await?;

    let longer = [mismatched_bytes.as_slice(), b" extra"].concat();
    let mismatched_sizes = (i64::try_from(mismatched_bytes.len())?, u64::try_from(longer.len())?);
    put(store, &mut keys_written, &mismatched_key, longer).await?;

    // Raw orphans: an asset key with no row, and the other tenant's key (never in this report).
    let orphan_bytes = unique("orphan");
    let orphan = keys::asset(tenant, &digest(&orphan_bytes));
    put(store, &mut keys_written, &orphan, orphan_bytes).await?;
    let other_bytes = unique("other");
    let other_tenant_orphan = keys::asset(OTHER_TENANT.parse()?, &digest(&other_bytes));
    put(store, &mut keys_written, &other_tenant_orphan, other_bytes).await?;

    // Staging: no reservation, an expired one, a promoted one and an open one.
    let unreserved_staging = keys::staging(tenant, Uuid::new_v4());
    put(store, &mut keys_written, &unreserved_staging, unique("unreserved")).await?;
    let expired_staging = reservation(pool, "reserved", -HOUR).await?;
    put(store, &mut keys_written, &expired_staging.1, unique("expired")).await?;
    let closed_staging = reservation(pool, "rejected", HOUR).await?;
    put(store, &mut keys_written, &closed_staging.1, unique("closed")).await?;
    let open_staging = reservation(pool, "reserved", 600).await?.1;
    put(store, &mut keys_written, &open_staging, unique("open")).await?;

    Ok(Plants {
        healthy,
        held,
        missing,
        mismatched,
        mismatched_sizes,
        stuck_pending,
        stuck_deleting,
        rejected_without_bytes,
        orphan,
        deleted_only,
        unreserved_staging,
        expired_staging,
        closed_staging,
        open_staging,
        other_tenant_orphan,
        keys: keys_written,
    })
}

async fn put(store: &ObjectStore, written: &mut Vec<String>, key: &str, bytes: Vec<u8>) -> TestResult {
    store.put_object(key, bytes, MEDIA).await?;
    written.push(key.to_string());
    Ok(())
}

async fn report(probe: &Probe, limits: &ReconciliationLimits) -> TestResult<ReconciliationReport> {
    let mut scoped = begin(&probe.pool, TENANT).await?;
    let report = reconcile_tenant(&mut scoped, &probe.store, limits).await?;
    // Committing proves nothing was written, not merely rolled back.
    scoped.commit().await?;
    Ok(report)
}

#[tokio::test]
async fn reconciliation_classifies_every_planted_discrepancy_and_never_mutates() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    clear(&probe).await?;
    let plants = plant(&probe).await?;
    let prefix = format!("tenants/{TENANT}/");
    let rows_before = row_snapshot(&probe.pool).await?;
    let listing_before = raw_listing(&probe.settings, &prefix).await?;

    let report = report(&probe, &ReconciliationLimits::default()).await?;
    assert_eq!(report.format_version, REPORT_FORMAT_VERSION);
    assert_eq!((report.tenant_id, report.bucket.as_str(), report.prefix.as_str()), (tenant()?, BUCKET, prefix.as_str()));
    assert!(report.complete && report.caps_reached.is_empty(), "{report:?}");
    assert!(!report.clean());

    // Missing: the verified row is critical; the stuck pending row without bytes is informational.
    let missing: Vec<_> = report.missing_objects.iter().map(|finding| (finding.object_id, finding.state, finding.severity)).collect();
    assert_eq!(missing.iter().copied().collect::<BTreeSet<_>>().len(), 2, "{missing:?}");
    assert!(missing.contains(&(plants.missing, RowState::Verified, Severity::Critical)), "{missing:?}");
    assert!(missing.contains(&(plants.stuck_pending, RowState::Pending, Severity::Info)), "{missing:?}");

    // Size mismatch: recorded vs listed bytes, critical because the row is verified.
    assert_eq!(report.size_mismatches.len(), 1, "{:?}", report.size_mismatches);
    let mismatch = &report.size_mismatches[0];
    assert_eq!((mismatch.object_id, mismatch.severity, mismatch.state), (plants.mismatched, Severity::Critical, RowState::Verified));
    assert_eq!((mismatch.recorded_bytes, mismatch.stored_bytes), plants.mismatched_sizes);

    // Orphans: the raw key has no row; the deleted-only key's row is deleted. The other tenant's
    // key never appears.
    let orphans: BTreeSet<_> = report.orphan_keys.iter().map(|finding| (finding.object_key.clone(), finding.reason)).collect();
    let expected: BTreeSet<_> =
        [(plants.orphan.clone(), OrphanReason::NoRow), (plants.deleted_only.clone(), OrphanReason::DeletedRowOnly)].into_iter().collect();
    assert_eq!(orphans, expected);
    assert!(report.orphan_keys.iter().all(|finding| finding.severity == Severity::Warning && !finding.last_modified.is_empty()));
    assert!(!format!("{report:?}").contains(&plants.other_tenant_orphan));

    // Staging: unreserved, expired and closed reservations are stale; the open one is in flight.
    let staging: BTreeSet<_> =
        report.stale_staging_keys.iter().map(|finding| (finding.object_key.clone(), finding.reason, finding.reservation_id)).collect();
    let expected: BTreeSet<_> = [
        (plants.unreserved_staging.clone(), StagingReason::NoReservation, None),
        (plants.expired_staging.1.clone(), StagingReason::ReservationExpired, Some(plants.expired_staging.0)),
        (plants.closed_staging.1.clone(), StagingReason::ReservationClosed, Some(plants.closed_staging.0)),
    ]
    .into_iter()
    .collect();
    assert_eq!(staging, expected);
    assert!(!report.stale_staging_keys.iter().any(|finding| finding.object_key == plants.open_staging));
    assert_eq!(report.summary.open_staging_keys, 1);

    // Stuck: pending for two hours (no bytes) and deleting for two hours (bytes still listed).
    let stuck: BTreeSet<_> = report.stuck_objects.iter().map(|finding| (finding.object_id, finding.state, finding.in_bucket)).collect();
    let expected: BTreeSet<_> =
        [(plants.stuck_pending, RowState::Pending, Some(false)), (plants.stuck_deleting, RowState::Deleting, Some(true))].into_iter().collect();
    assert_eq!(stuck, expected);
    assert!(report.stuck_objects.iter().all(|finding| finding.age_seconds >= 2 * HOUR - 60));

    // Collectable: rows without references or holds, informational; none is old enough yet.
    let collectable: BTreeSet<_> = report.collectable_objects.iter().map(|finding| finding.object_id).collect();
    let expected: BTreeSet<_> = [plants.missing, plants.mismatched, plants.stuck_pending, plants.rejected_without_bytes].into_iter().collect();
    assert_eq!(collectable, expected);
    assert!(!collectable.contains(&plants.healthy) && !collectable.contains(&plants.held));
    assert!(report.collectable_objects.iter().all(|finding| finding.severity == Severity::Info && !finding.eligible));

    // Counts and totals.
    assert_eq!(report.counts.missing_objects, 2);
    assert_eq!(report.counts.orphan_keys, 2);
    assert_eq!(report.counts.stale_staging_keys, 3);
    assert_eq!(report.counts.size_mismatches, 1);
    assert_eq!(report.counts.stuck_objects, 2);
    assert_eq!(report.counts.collectable_objects, 4);
    assert_eq!(report.summary.critical, 2);
    assert_eq!(report.summary.warnings, 2 + 3 + 2);
    assert_eq!(report.summary.info, 1 + 4);
    assert_eq!(report.summary.rows_scanned, 8);
    assert_eq!(report.summary.keys_listed, u64::try_from(listing_before.len())?);
    assert_eq!(report.summary.listed_bytes, listing_before.iter().map(|(_, size)| size).sum::<u64>());
    assert_eq!((report.summary.recent_rows, report.summary.unchecked_rows, report.summary.unchecked_keys), (0, 0, 0));
    let asset = report.kinds.iter().find(|kind| kind.kind == "asset").ok_or("asset totals")?;
    // healthy, held, missing, mismatched, rejected (the deleted row is not live).
    assert_eq!((asset.live_rows, asset.verified_rows, asset.stored_keys), (5, 4, 3));
    let thumbnail = report.kinds.iter().find(|kind| kind.kind == "thumbnail").ok_or("thumbnail totals")?;
    assert_eq!((thumbnail.live_rows, thumbnail.stored_keys), (1, 1));
    assert_eq!(report.kinds.len(), 6);

    // The report serializes and reads back.
    let json = serde_json::to_string(&report)?;
    assert_eq!(serde_json::from_str::<ReconciliationReport>(&json)?, report);
    assert!(json.contains("\"severity\":\"critical\"") && json.contains("\"reason\":\"deleted_row_only\""));

    // Nothing changed: rows, reservations, references and the bucket listing.
    assert_eq!(row_snapshot(&probe.pool).await?, rows_before);
    assert_eq!(raw_listing(&probe.settings, &prefix).await?, listing_before);

    // The transaction is read-only afterwards: the database refuses a write.
    let mut scoped = begin(&probe.pool, TENANT).await?;
    reconcile_tenant(&mut scoped, &probe.store, &ReconciliationLimits::default()).await?;
    let write = sqlx::query("UPDATE objects SET state = 'rejected' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(plants.healthy.to_string())
        .execute(scoped.connection())
        .await;
    assert!(write.is_err(), "a write after the report must be refused");
    scoped.rollback().await?;

    // Invalid limits are refused before anything is sent.
    let mut scoped = begin(&probe.pool, TENANT).await?;
    assert_eq!(
        reconcile_tenant(&mut scoped, &probe.store, &ReconciliationLimits { page_size: 0, ..ReconciliationLimits::default() }).await,
        Err(ReconciliationError::InvalidLimits)
    );
    scoped.rollback().await?;

    caps_mark_the_report_incomplete_without_false_findings(&probe, &plants, &listing_before).await?;

    assert_eq!(row_snapshot(&probe.pool).await?, rows_before);
    assert_eq!(raw_listing(&probe.settings, &prefix).await?, listing_before);
    for key in &plants.keys {
        probe.store.delete_object(key).await?;
    }
    clear(&probe).await?;
    Ok(())
}

async fn caps_mark_the_report_incomplete_without_false_findings(probe: &Probe, plants: &Plants, listing: &[(String, u64)]) -> TestResult {
    let defaults = ReconciliationLimits::default();
    let planted_orphans: BTreeSet<&str> = [plants.orphan.as_str(), plants.deleted_only.as_str()].into_iter().collect();

    // Small pages walk the continuation tokens and give the same findings.
    let paged = report(probe, &ReconciliationLimits { page_size: 2, ..defaults }).await?;
    assert!(paged.complete);
    assert!(paged.summary.pages >= u64::try_from(listing.len().div_ceil(2))?);
    assert_eq!(paged.counts, report(probe, &defaults).await?.counts);

    // Key cap: the listing stops early, and rows past its last key are unchecked, never missing.
    let keys = report(probe, &ReconciliationLimits { page_size: 3, max_keys: 3, ..defaults }).await?;
    assert!(!keys.complete && keys.caps_reached.contains(&Cap::Keys), "{:?}", keys.caps_reached);
    assert_eq!(keys.summary.keys_listed, 3);
    let horizon = listing.get(2).map(|(key, _)| key.as_str()).ok_or("listing too short")?;
    assert!(keys.missing_objects.iter().all(|finding| finding.object_key.as_str() <= horizon));
    assert!(keys.missing_objects.iter().all(|finding| [plants.missing, plants.stuck_pending].contains(&finding.object_id)));
    assert!(keys.summary.unchecked_rows > 0);

    // Page cap.
    let pages = report(probe, &ReconciliationLimits { page_size: 1, max_pages: 2, ..defaults }).await?;
    assert!(!pages.complete && pages.caps_reached == vec![Cap::Pages], "{:?}", pages.caps_reached);
    assert_eq!(pages.summary.pages, 2);

    // Row cap: keys past the last scanned row are unchecked, never orphans.
    let rows = report(probe, &ReconciliationLimits { max_rows: 2, ..defaults }).await?;
    assert!(!rows.complete && rows.caps_reached.contains(&Cap::Rows));
    assert_eq!(rows.summary.rows_scanned, 2);
    assert!(rows.summary.unchecked_keys > 0);
    assert!(rows.orphan_keys.iter().all(|finding| planted_orphans.contains(finding.object_key.as_str())));

    // Finding cap: lists stop at one entry, counts stay exact.
    let findings = report(probe, &ReconciliationLimits { max_findings: 1, ..defaults }).await?;
    assert!(!findings.complete && findings.caps_reached == vec![Cap::Findings]);
    assert_eq!(findings.counts.stale_staging_keys, 3);
    assert_eq!(findings.stale_staging_keys.len(), 1);
    assert_eq!(findings.collectable_objects.len(), 1);
    assert_eq!(findings.counts.collectable_objects, 4);
    Ok(())
}
