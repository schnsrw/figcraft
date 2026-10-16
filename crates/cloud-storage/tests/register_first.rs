//! Register-first: the crash window between uploading bytes and committing the caller's
//! transaction leaves a `pending` row, never unnamed bytes, and garbage collection retires it.
//!
//! Fixture: `deploy/saas/orphan-sweep-preflight.sql`, tenant 2 (a document and a library). Rows
//! are backdated with plain SQL past the minimum collection age; key versions are counted with an
//! independent, hand-signed `ListObjectVersions` request rather than the code under test.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::document_snapshots::store_snapshot;
use figcraft_cloud_storage::garbage_collection::{GcOptions, MAX_GC_BATCH, Settled, collect_garbage};
use figcraft_cloud_storage::libraries::publish_library_version;
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, ObjectState, Rejection, get_object, keys, register_object};
use figcraft_cloud_storage::object_references::MIN_COLLECTION_AGE;
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::{Document, Limits, Server, SessionState};
use rusty_s3::{Bucket, Credentials, Method, UrlStyle};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const TENANT: &str = "b0000000-0000-4000-8000-000000000002";
const PRINCIPAL: &str = "b0000000-0000-4000-8000-000000000009";
const DOCUMENT: &str = "b0000000-0000-4000-8000-000000000023";
const LIBRARY: &str = "b0000000-0000-4000-8000-000000000024";
const BUCKET: &str = "figcraft";

struct Probe {
    pool: PgPool,
    store: ObjectStore,
    bucket: Bucket,
    credentials: Credentials,
    client: reqwest::Client,
}

async fn probe() -> TestResult<Option<Probe>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; register-first probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    // One connection for the caller's transaction and one for the pre-registration.
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
    let bucket = Bucket::new(settings.endpoint.parse()?, UrlStyle::Path, settings.bucket.clone(), settings.region.clone())?;
    let credentials = Credentials::new(settings.access_key.clone(), settings.secret_key.clone());
    // `reqwest::Client::new()` panics under `rustls-no-provider`, so the provider is explicit.
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let client = reqwest::Client::builder().tls_backend_preconfigured(tls).build()?;
    Ok(Some(Probe { pool, store, bucket, credentials, client }))
}

impl Probe {
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
            60,
            [("max-keys", "1000"), ("prefix", key), ("versions", "")].into_iter(),
            std::iter::empty(),
        );
        let response = self.client.get(url.as_str()).send().await?;
        assert_eq!(response.status().as_u16(), 200, "ListObjectVersions should succeed");
        let body = response.text().await?;
        Ok(body.matches(&format!("<Key>{key}</Key>")).count())
    }

    async fn begin(&self) -> TestResult<TenantTransaction<'_>> {
        Ok(begin_tenant_transaction(&self.pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?)
    }

    async fn state(&self, id: Uuid) -> TestResult<Option<ObjectState>> {
        let mut scoped = self.begin().await?;
        let state = get_object(&mut scoped, id).await?.map(|object| object.state);
        scoped.rollback().await?;
        Ok(state)
    }

    async fn backdate(&self, id: Uuid) -> TestResult {
        let mut scoped = self.begin().await?;
        sqlx::query("UPDATE objects SET created_at = now() - interval '2 days' WHERE tenant_id = $1::uuid AND id = $2::uuid")
            .bind(TENANT)
            .bind(id.to_string())
            .execute(scoped.connection())
            .await?;
        scoped.commit().await?;
        Ok(())
    }

    /// The live row naming `key`, if any.
    async fn row_for(&self, key: &str) -> TestResult<Option<(Uuid, String)>> {
        let mut scoped = self.begin().await?;
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT id::text, state FROM objects WHERE tenant_id = $1::uuid AND object_key = $2 AND state <> 'deleted'")
                .bind(TENANT)
                .bind(key)
                .fetch_optional(scoped.connection())
                .await?;
        scoped.rollback().await?;
        Ok(row.map(|(id, state)| id.parse().map(|id| (id, state))).transpose()?)
    }
}

fn state(seq: u64) -> TestResult<SessionState> {
    let mut state = Server::new(Document::new(format!("Register first {}", Uuid::new_v4())), "", Limits::default()).state()?;
    state.seq = seq;
    Ok(state)
}

fn archive(label: &str) -> TestResult<Vec<u8>> {
    use std::io::Write;
    let mut buffer = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut buffer);
    let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("document.json", deflated)?;
    let body = format!(r#"{{"format":"figcraft","version":1,"pages":[{{"name":"{label} {}","children":[]}}]}}"#, Uuid::new_v4());
    zip.write_all(body.as_bytes())?;
    zip.start_file("library/manifest.json", deflated)?;
    zip.write_all(br#"{"id":"lib","name":"Library"}"#)?;
    zip.finish()?;
    Ok(buffer.into_inner())
}

async fn collect(probe: &Probe) -> TestResult<figcraft_cloud_storage::garbage_collection::GcReport> {
    let tenant: TenantId = TENANT.parse()?;
    Ok(collect_garbage(&probe.pool, &probe.store, tenant, GcOptions { dry_run: false, limit: MAX_GC_BATCH, min_age: MIN_COLLECTION_AGE }).await?)
}

#[tokio::test]
async fn a_rolled_back_snapshot_store_leaves_a_pending_row_that_collection_retires() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let document: CloudDocumentId = DOCUMENT.parse()?;
    let snapshot = state(7)?;

    // The crash window, twice: pre-registered and committed, uploaded, then the caller's linking
    // transaction rolls back. The second attempt reuses the pending row and adds a version.
    let mut key = String::new();
    for attempt in 0..2 {
        let mut transaction = probe.begin().await?;
        let stored = store_snapshot(&probe.pool, &mut transaction, &probe.store, document, &snapshot, "figcraft-cloud test").await?;
        assert_eq!(stored.reused_object, attempt == 1);
        key = stored.object.object_key;
        transaction.rollback().await?;
    }
    let (id, row_state) = probe.row_for(&key).await?.ok_or("the pre-registered row survives the rollback")?;
    assert_eq!(row_state, "pending");
    assert_eq!(probe.versions(&key).await?, 2);

    // Young, it is left alone.
    let young = collect(&probe).await?;
    assert!(!young.deleted.contains(&id) && !young.settled.iter().any(|(settled, _)| *settled == id));
    assert_eq!(probe.state(id).await?, Some(ObjectState::Pending));

    // Past the minimum age it is settled (the bytes are intact) and, unreferenced, collected with
    // every version.
    probe.backdate(id).await?;
    let report = collect(&probe).await?;
    assert!(report.settled.contains(&(id, Some(Settled::Verified))), "{report:?}");
    assert!(report.deleted.contains(&id), "{report:?}");
    assert_eq!(probe.state(id).await?, Some(ObjectState::Deleted));
    assert_eq!(probe.versions(&key).await?, 0);

    // A committed store of a fresh state still links and verifies as before.
    let mut transaction = probe.begin().await?;
    let stored = store_snapshot(&probe.pool, &mut transaction, &probe.store, document, &state(8)?, "figcraft-cloud test").await?;
    transaction.commit().await?;
    assert_eq!(stored.object.state, ObjectState::Verified);
    assert!(!stored.reused_object);
    let kept = collect(&probe).await?;
    assert!(!kept.deleted.contains(&stored.object.id));
    assert_eq!(probe.versions(&stored.object.object_key).await?, 1);
    probe.store.delete_object(&stored.object.object_key).await?;
    probe.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_pending_row_whose_bytes_never_arrived_is_rejected_then_collected() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let label = Uuid::new_v4();
    let sha256: [u8; 32] = Sha256::digest(format!("never uploaded {label}")).into();
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Asset,
        object_key: keys::asset(TENANT.parse()?, &sha256),
        sha256,
        size_bytes: 16,
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    };
    let mut scoped = probe.begin().await?;
    register_object(&mut scoped, BUCKET, &object).await?;
    scoped.commit().await?;
    probe.backdate(object.id).await?;

    // A dry run lists it for settling and changes nothing.
    let tenant: TenantId = TENANT.parse()?;
    let dry =
        collect_garbage(&probe.pool, &probe.store, tenant, GcOptions { dry_run: true, limit: MAX_GC_BATCH, min_age: MIN_COLLECTION_AGE }).await?;
    assert!(dry.settled.contains(&(object.id, None)));
    assert_eq!(probe.state(object.id).await?, Some(ObjectState::Pending));

    let report = collect(&probe).await?;
    assert!(report.settled.contains(&(object.id, Some(Settled::Rejected(Rejection::Missing)))), "{report:?}");
    assert!(report.deleted.contains(&object.id));
    assert_eq!(probe.state(object.id).await?, Some(ObjectState::Deleted));
    probe.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_rolled_back_publish_is_reused_by_its_retry() -> TestResult {
    let Some(probe) = probe().await? else {
        return Ok(());
    };
    let library: Uuid = LIBRARY.parse()?;
    let publisher: PrincipalId = PRINCIPAL.parse()?;
    let bytes = archive("register first")?;

    let mut transaction = probe.begin().await?;
    let first = publish_library_version(&probe.pool, &mut transaction, &probe.store, library, publisher, bytes.clone()).await?;
    transaction.rollback().await?;
    let (id, row_state) = probe.row_for(&first.object.object_key).await?.ok_or("the pre-registered row survives the rollback")?;
    assert_eq!((id, row_state.as_str()), (first.object.id, "pending"));
    assert_eq!(probe.versions(&first.object.object_key).await?, 1);

    // The retry takes the same version number and key and reuses the pending row.
    let mut transaction = probe.begin().await?;
    let retried = publish_library_version(&probe.pool, &mut transaction, &probe.store, library, publisher, bytes).await?;
    transaction.commit().await?;
    assert_eq!((retried.object.id, retried.version.version), (first.object.id, first.version.version));
    assert_eq!(retried.object.state, ObjectState::Verified);
    assert_eq!(probe.state(id).await?, Some(ObjectState::Verified));

    // Referenced by its version row, it survives collection even when old.
    probe.backdate(id).await?;
    let report = collect(&probe).await?;
    assert!(!report.deleted.contains(&id));
    assert_eq!(probe.versions(&retried.object.object_key).await?, 2);
    probe.store.delete_object(&retried.object.object_key).await?;
    probe.pool.close().await;
    Ok(())
}
