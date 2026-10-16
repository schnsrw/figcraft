//! Legacy filesystem-server import probes (S3-018) against the dev PostgreSQL and S3 store.
//!
//! Each test writes a synthetic data directory in the real on-disk format, scans and plans it, and
//! imports it into the `ad…` fixture tenant (`deploy/saas/legacy-import-preflight.sql`). Legacy ids
//! carry a per-run suffix, so derived ids and keys never collide with earlier runs; uploaded
//! snapshot objects are deleted at the end and the cleanup SQL removes the rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use figcraft_cloud_domain::{CloudDocumentId, Uuid};
use figcraft_cloud_storage::events::{MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS, append_events, claim_sequencer, read_events};
use figcraft_cloud_storage::legacy_import::{
    ImportError, ImportOptions, ImportStage, ImportStatus, NotImportedKind, VerifyCode, import_document, import_plan, verify_import,
};
use figcraft_cloud_storage::legacy_scan::{ScanLimits, scan_data_directory};
use figcraft_cloud_storage::migration_plan::{ExclusionReason, MigrationPlan, PlanTarget, plan_migration};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::{Accepted, Change, Journal, NodeId, SessionState, message};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TENANT: &str = "ad000000-0000-4000-8000-000000000001";

fn target() -> PlanTarget {
    PlanTarget {
        tenant_id: TENANT.parse().unwrap(),
        workspace_id: "ad100000-0000-4000-8000-000000000001".parse().unwrap(),
        project_id: "ad200000-0000-4000-8000-000000000001".parse().unwrap(),
        principal_id: "ad000000-0000-4000-8000-000000000009".parse().unwrap(),
        bucket: "figcraft".to_owned(),
    }
}

async fn probe() -> Result<Option<(PgPool, ObjectStore)>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; legacy import probe skipped");
        return Ok(None);
    };
    let database_url = fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        Ok(fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string())
    };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    Ok(Some((pool, ObjectStore::new(&settings)?)))
}

/// A temporary legacy data directory, removed when dropped.
struct Fixture {
    root: PathBuf,
    run: String,
}

impl Fixture {
    fn new() -> Self {
        let run = Uuid::new_v4().simple().to_string()[..12].to_owned();
        let root = std::env::temp_dir().join(format!("figcraft-legacy-import-{run}"));
        fs::create_dir_all(root.join("files")).unwrap();
        Self { root, run }
    }

    fn id(&self, name: &str) -> String {
        format!("ad-{name}-{}", self.run)
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A file folder with a snapshot at `seq` and a log of `entries`; `links` is `links.json`.
    fn document(&self, id: &str, seq: u64, entries: &[Journal], links: Option<&[u8]>) {
        self.write(&format!("files/{id}/meta.json"), br#"{"name":"Synthetic board","created":10,"updated":20,"seq":7,"opened":30}"#);
        if let Some(links) = links {
            self.write(&format!("files/{id}/links.json"), links);
        }
        self.write(&format!("files/{id}/snapshot.bin"), &snapshot(2, seq, id));
        self.write(&format!("files/{id}/log.bin"), &log(2, entries));
    }

    fn plan(&self) -> MigrationPlan {
        let report = scan_data_directory(&self.root, 1_760_000_000, &ScanLimits::default());
        plan_migration(&report, &target()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const LINKS: &[u8] = br#"[{"id":"l1","access":"edit","label":"Team","created":11,"hash":"00"}]"#;

fn accepted(seq: u64) -> Accepted {
    Accepted { seq, author: 2, batch_id: seq, changes: vec![Change::Delete { id: NodeId(seq * 3) }] }
}

/// The snapshot body differs per document (the session id), so content keys differ too.
fn snapshot(generation: u64, seq: u64, id: &str) -> Vec<u8> {
    let state = SessionState {
        session: id.bytes().fold(7_u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b))),
        snapshot: vec![Change::Delete { id: NodeId(1) }],
        seq,
        next_client: 3,
        issued: BTreeMap::new(),
        deleted: Default::default(),
        removed: Default::default(),
        log: Vec::new(),
    };
    let body = message::encode(&state).unwrap();
    let mut out = b"FCSN\x01".to_vec();
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn log(generation: u64, entries: &[Journal]) -> Vec<u8> {
    let mut out = b"FCLG\x01".to_vec();
    out.extend_from_slice(&generation.to_le_bytes());
    for e in entries {
        let body = message::encode(e).unwrap();
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
    }
    out
}

async fn begin(pool: &PgPool) -> Result<TenantTransaction<'_>, Box<dyn std::error::Error>> {
    Ok(begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?)
}

/// Every committed event after `after`, as batches.
async fn replay(pool: &PgPool, document: CloudDocumentId, after: i64) -> Result<Vec<Accepted>, Box<dyn std::error::Error>> {
    let mut tx = begin(pool).await?;
    let page = read_events(&mut tx, document, after, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await?;
    tx.rollback().await?;
    Ok(page.events.into_iter().map(|event| event.batch).collect())
}

async fn delete_snapshots(store: &ObjectStore, plan: &MigrationPlan, pool: &PgPool) -> TestResult {
    let mut tx = begin(pool).await?;
    for entry in &plan.documents {
        if let Some(object) = figcraft_cloud_storage::object_metadata::get_object(&mut tx, entry.snapshot.object.id).await? {
            store.delete_object(&object.object_key).await?;
        }
    }
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn a_plan_imports_once_skips_blocked_documents_and_reruns_as_a_no_op() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let f = Fixture::new();
    let (ready, quiet, blocked) = (f.id("ready"), f.id("quiet"), f.id("blocked"));
    let entries = [
        Journal::Issued { client: 2 },
        Journal::Accepted(accepted(5)),
        Journal::Accepted(accepted(6)),
        Journal::Refused { client: 2, batch_id: 1 },
        Journal::Accepted(accepted(7)),
    ];
    f.document(&ready, 4, &entries, Some(LINKS));
    f.document(&quiet, 2, &[], Some(b"[]"));
    // No links.json: a blocking problem, so the plan excludes it.
    f.document(&blocked, 1, &[Journal::Accepted(accepted(2))], None);
    let plan = f.plan();
    assert_eq!(plan.documents.len(), 2);
    assert_eq!(plan.excluded.iter().map(|e| (e.legacy_id.as_str(), e.reason)).collect::<Vec<_>>(), [(blocked.as_str(), ExclusionReason::Blocked)]);

    let limits = ScanLimits::default();
    // Exclusions need explicit approval.
    let refused = import_plan(&pool, &store, &plan, &f.root, &limits, &ImportOptions::default()).await;
    assert_eq!(refused.err(), Some(ImportError::PlanNotExecutable));

    let options = ImportOptions { approve_exclusions: true, stop_after: None };
    let first = import_plan(&pool, &store, &plan, &f.root, &limits, &options).await?;
    assert!(first.all_succeeded(), "{first:?}");
    assert_eq!(first.skipped, std::slice::from_ref(&blocked));
    let outcomes: BTreeMap<_, _> = first.documents.iter().map(|d| (d.legacy_id.clone(), d.result.clone().unwrap())).collect();
    let r = &outcomes[&ready];
    assert_eq!(
        (r.status, r.document_created, r.mapping_created, r.snapshot_uploaded, r.snapshot_registered),
        (ImportStatus::Created, true, true, true, true)
    );
    assert_eq!((r.events_appended, r.events_already_present), (3, 0));
    let q = &outcomes[&quiet];
    assert_eq!((q.status, q.events_appended), (ImportStatus::Created, 0));

    // The events continue the snapshot sequence and replay to the legacy batches.
    let ready_plan = plan.documents.iter().find(|d| d.legacy_id == ready).unwrap();
    assert_eq!(replay(&pool, ready_plan.document.id, 4).await?, vec![accepted(5), accepted(6), accepted(7)]);
    let mut tx = begin(&pool).await?;
    let mapping = figcraft_cloud_storage::external_mappings::get_external_mapping(&mut tx, "figcraft-server", "document", &ready).await?.unwrap();
    assert_eq!(mapping.internal_id, ready_plan.document.id.as_uuid());
    let object = figcraft_cloud_storage::object_metadata::get_object(&mut tx, ready_plan.snapshot.object.id).await?.unwrap();
    assert_eq!(object.state, figcraft_cloud_storage::object_metadata::ObjectState::Verified);
    assert!(object.object_key.starts_with(&ready_plan.snapshot.object.key_prefix));
    let (token,): (i64,) = sqlx::query_as("SELECT fencing_token FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(ready_plan.document.id.to_string())
        .fetch_one(tx.connection())
        .await?;
    tx.rollback().await?;

    // A rerun writes nothing: no claim, no upload, no events.
    let second = import_plan(&pool, &store, &plan, &f.root, &limits, &options).await?;
    for document in &second.documents {
        let outcome = document.result.clone().unwrap();
        assert_eq!(outcome.status, ImportStatus::AlreadyComplete, "{outcome:?}");
        assert!(!outcome.snapshot_uploaded && outcome.events_appended == 0);
    }
    assert_eq!(second.documents.iter().find(|d| d.legacy_id == ready).unwrap().result.clone().unwrap().events_already_present, 3);
    let mut tx = begin(&pool).await?;
    let (after,): (i64,) = sqlx::query_as("SELECT fencing_token FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(ready_plan.document.id.to_string())
        .fetch_one(tx.connection())
        .await?;
    tx.rollback().await?;
    assert_eq!(after, token);
    assert_eq!(replay(&pool, ready_plan.document.id, 4).await?.len(), 3);

    // S3-019: both documents verify; the links and the excluded document block cutover.
    let mut tx = begin(&pool).await?;
    let verification = verify_import(&mut tx, &store, &plan, &f.root, &limits).await?;
    tx.rollback().await?;
    assert!(verification.documents.iter().all(|d| d.passed), "{verification:?}");
    assert_eq!(verification.documents.iter().find(|d| d.legacy_id == ready).map(|d| d.events), Some(3));
    let left: Vec<_> = verification.not_imported.iter().map(|n| (n.kind, n.legacy_document.clone(), n.count)).collect();
    assert_eq!(left, [(NotImportedKind::Links, Some(ready.clone()), 1), (NotImportedKind::ExcludedDocument, Some(blocked.clone()), 1)]);
    assert!(!verification.cutover_ready);

    delete_snapshots(&store, &plan, &pool).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn an_interrupted_import_resumes_without_duplicating_and_refuses_changed_sources() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let f = Fixture::new();
    let id = f.id("crash");
    let entries: Vec<Journal> = (10..=14).map(|seq| Journal::Accepted(accepted(seq))).collect();
    f.document(&id, 9, &entries, Some(b"[]"));
    let plan = f.plan();
    let entry = plan.documents.first().unwrap();
    let limits = ScanLimits::default();

    // "Crash" after the snapshot stage.
    let staged = ImportOptions { approve_exclusions: false, stop_after: Some(ImportStage::Snapshot) };
    let first = import_document(&pool, &store, &plan, entry, &f.root, &limits, &staged).await?;
    assert_eq!((first.status, first.stopped_after, first.events_appended), (ImportStatus::Created, Some(ImportStage::Snapshot), 0));
    assert!(replay(&pool, entry.document.id, 9).await?.is_empty());

    // A live sequencer (or a crashed earlier attempt) committed part of the range already.
    let mut tx = begin(&pool).await?;
    let claim = claim_sequencer(&mut tx, entry.document.id).await?;
    assert_eq!(claim.current_seq, 9);
    append_events(&mut tx, entry.document.id, claim.fencing_token, target().principal_id, &[accepted(10), accepted(11)]).await?;
    tx.commit().await?;

    let resumed = import_document(&pool, &store, &plan, entry, &f.root, &limits, &ImportOptions::default()).await?;
    assert_eq!(
        (resumed.status, resumed.document_created, resumed.mapping_created, resumed.snapshot_uploaded),
        (ImportStatus::Resumed, false, false, false)
    );
    assert_eq!((resumed.events_appended, resumed.events_already_present), (3, 2));
    assert_eq!(replay(&pool, entry.document.id, 9).await?, (10..=14).map(accepted).collect::<Vec<_>>());
    let again = import_document(&pool, &store, &plan, entry, &f.root, &limits, &ImportOptions::default()).await?;
    assert_eq!(again.status, ImportStatus::AlreadyComplete);

    // A source that changed since the plan is refused before anything is written.
    f.write(&format!("files/{id}/log.bin"), &log(2, &entries[..3]));
    assert_eq!(
        import_document(&pool, &store, &plan, entry, &f.root, &limits, &ImportOptions::default()).await.err(),
        Some(ImportError::LegacyChanged)
    );
    fs::remove_file(f.root.join(format!("files/{id}/snapshot.bin")))?;
    assert_eq!(
        import_document(&pool, &store, &plan, entry, &f.root, &limits, &ImportOptions::default()).await.err(),
        Some(ImportError::LegacyUnreadable)
    );

    delete_snapshots(&store, &plan, &pool).await?;
    pool.close().await;
    Ok(())
}

/// The admin pool, for tampering with append-only rows the runtime role cannot touch.
async fn admin() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE is unset; tamper probe skipped");
        return Ok(None);
    };
    let url = fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(1).connect(url.trim_end()).await?))
}

#[tokio::test]
async fn verification_passes_a_clean_import_and_detects_tampering() -> TestResult {
    let (Some((pool, store)), Some(admin)) = (probe().await?, admin().await?) else {
        return Ok(());
    };
    let f = Fixture::new();
    let id = f.id("verify");
    let entries: Vec<Journal> = (4..=6).map(|seq| Journal::Accepted(accepted(seq))).collect();
    f.document(&id, 3, &entries, Some(b"[]"));
    let plan = f.plan();
    let entry = plan.documents.first().unwrap().clone();
    let limits = ScanLimits::default();
    let verify = || async {
        let mut tx = begin(&pool).await?;
        let verification = verify_import(&mut tx, &store, &plan, &f.root, &limits).await?;
        tx.rollback().await?;
        Ok::<_, Box<dyn std::error::Error>>(verification)
    };

    // Nothing imported yet: every check fails, and verification wrote nothing.
    let before = verify().await?;
    assert_eq!(before.documents[0].failures, [VerifyCode::DocumentMissing]);
    assert!(!before.cutover_ready);

    let options = ImportOptions::default();
    import_plan(&pool, &store, &plan, &f.root, &limits, &options).await?;
    let clean = verify().await?;
    assert!(clean.cutover_ready, "{clean:?}");
    assert!(clean.not_imported.is_empty());

    // An event row removed behind the import's back.
    let deleted = sqlx::query("DELETE FROM public.document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq = $3")
        .bind(TENANT)
        .bind(entry.document.id.to_string())
        .bind(5_i64)
        .execute(&admin)
        .await?;
    assert_eq!(deleted.rows_affected(), 1);
    let gap = verify().await?;
    assert!(!gap.cutover_ready);
    assert_eq!(gap.documents[0].failures, [VerifyCode::EventCountMismatch, VerifyCode::ReplayCorrupt]);

    // Snapshot bytes overwritten in the bucket (same size, so only the digest gives it away).
    let mut tx = begin(&pool).await?;
    let object = figcraft_cloud_storage::object_metadata::get_object(&mut tx, entry.snapshot.object.id).await?.unwrap();
    tx.rollback().await?;
    let mut bytes = store.get_object(&object.object_key, 1 << 20).await?;
    if let Some(first) = bytes.first_mut() {
        *first ^= 0xff;
    }
    store.put_object(&object.object_key, bytes, &object.media_type).await?;
    let tampered = verify().await?;
    let failures = &tampered.documents[0].failures;
    assert!(failures.contains(&VerifyCode::ObjectBytesMismatch) && failures.contains(&VerifyCode::SnapshotContentMismatch), "{failures:?}");

    // A legacy source that changed after the import is reported too.
    f.write(&format!("files/{id}/log.bin"), &log(2, &entries[..1]));
    assert!(verify().await?.documents[0].failures.contains(&VerifyCode::LegacyChanged));

    delete_snapshots(&store, &plan, &pool).await?;
    admin.close().await;
    pool.close().await;
    Ok(())
}

/// A `.figcraft`-shaped library archive, unique to `nonce`.
fn library_archive(nonce: &str) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut z = zip::ZipWriter::new(&mut buf);
    let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    z.start_file("document.json", deflated).unwrap();
    z.write_all(format!(r#"{{"format":"figcraft","version":1,"pages":[{{"name":"{nonce}","children":[]}}]}}"#).as_bytes()).unwrap();
    z.start_file("library/manifest.json", deflated).unwrap();
    z.write_all(br#"{"id":"lib","name":"Library"}"#).unwrap();
    z.finish().unwrap();
    buf.into_inner()
}

/// A plugin bundle in the `FCPLUGB1` layout.
fn plugin_bundle(manifest: &str) -> Vec<u8> {
    let mut out = b"FCPLUGB1".to_vec();
    for part in [manifest.as_bytes(), b"\0asm\x01\0\0\0".as_slice()] {
        out.extend(u32::try_from(part.len()).unwrap().to_le_bytes());
        out.extend(part);
    }
    out.extend(0_u32.to_le_bytes());
    out
}

fn hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove the library and plugin objects a plan imported from the bucket.
async fn delete_resources(store: &ObjectStore, plan: &MigrationPlan, pool: &PgPool) -> TestResult {
    let mut tx = begin(pool).await?;
    for library in plan.documents.iter().flat_map(|d| &d.libraries) {
        if let Some(object) = figcraft_cloud_storage::object_metadata::get_object(&mut tx, library.id).await? {
            store.delete_object(&object.object_key).await?;
        }
    }
    tx.rollback().await?;
    for plugin in &plan.plugins {
        if let Some(key) = &plugin.object.object_key {
            let _ = store.delete_object(key).await;
        }
    }
    Ok(())
}

#[tokio::test]
async fn libraries_and_plugins_import_once_verify_and_detect_tampering() -> TestResult {
    use figcraft_cloud_storage::legacy_import::IMPORT_PUBLISHER;
    use figcraft_cloud_storage::plugin_store::{PluginName, find_plugin};
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let f = Fixture::new();
    let (lib, bad) = (f.id("lib"), f.id("badlib"));
    f.document(&lib, 2, &[Journal::Accepted(accepted(3))], Some(b"[]"));
    // Legacy versions need not be dense: they keep their numbers.
    f.write(&format!("files/{lib}/library-1.bin"), &library_archive(&format!("{lib}-1")));
    f.write(&format!("files/{lib}/library-3.bin"), &library_archive(&format!("{lib}-3")));
    f.document(&bad, 2, &[], Some(b"[]"));
    f.write(&format!("files/{bad}/library-1.bin"), b"not a figcraft archive");
    let plugin_id = f.id("plug");
    let bundle = plugin_bundle(r#"{"id":"org.example.legacy","name":"Legacy","main":"p.wasm","version":"1.0.0","api":1}"#);
    let sha = hex(&bundle);
    f.write(&format!("plugins/{sha}.bin"), &bundle);
    f.write("plugins/index.json", format!(r#"[{{"id":"{plugin_id}","version":"1.0.0","bytes":{},"sha256":"{sha}"}}]"#, bundle.len()).as_bytes());
    let plan = f.plan();
    assert!(plan.executable, "{:?}", plan.blockers);
    assert!(plan.deferred.is_empty(), "{:?}", plan.deferred);
    let lib_plan = plan.documents.iter().find(|d| d.legacy_id == lib).unwrap();
    assert_eq!(lib_plan.libraries.len(), 2);
    assert_eq!(plan.plugins.len(), 1);
    let limits = ScanLimits::default();
    let verify = || async {
        let mut tx = begin(&pool).await?;
        let verification = verify_import(&mut tx, &store, &plan, &f.root, &limits).await?;
        tx.rollback().await?;
        Ok::<_, Box<dyn std::error::Error>>(verification)
    };

    // Before the import every library and the plugin are missing and listed as not imported.
    let before = verify().await?;
    assert!(before.libraries.iter().all(|l| l.failures.contains(&VerifyCode::LibraryMissing) && l.failures.contains(&VerifyCode::VersionMissing)));
    assert_eq!(before.plugins[0].failures, [VerifyCode::PluginMissing, VerifyCode::VersionMissing]);
    assert_eq!(before.not_imported.iter().filter(|n| n.kind == NotImportedKind::Library).count(), 3);
    assert_eq!(before.not_imported.iter().filter(|n| n.kind == NotImportedKind::Plugin).count(), 1);

    let options = ImportOptions::default();
    let first = import_plan(&pool, &store, &plan, &f.root, &limits, &options).await?;
    assert!(first.documents.iter().all(|d| d.result.is_ok()), "{first:?}");
    let results: Vec<_> = first.libraries.iter().map(|l| (l.legacy_document.clone(), l.version, l.result)).collect();
    assert_eq!(
        results,
        [
            (bad.clone(), 1, Err(ImportError::LibraryRejected)),
            (lib.clone(), 1, Ok(ImportStatus::Created)),
            (lib.clone(), 3, Ok(ImportStatus::Created)),
        ]
    );
    assert_eq!(first.plugins.iter().map(|p| p.result).collect::<Vec<_>>(), [Ok(ImportStatus::Created)]);
    assert!(!first.all_succeeded());

    // The library keeps the legacy version numbers; the plugin is published by the importer.
    let mut tx = begin(&pool).await?;
    let library_id = first.libraries[1].library_id;
    let library = figcraft_cloud_storage::libraries::get_library(&mut tx, library_id).await?.unwrap();
    assert_eq!(library.source_document_id, Some(lib_plan.document.id));
    let versions = figcraft_cloud_storage::libraries::list_versions(&mut tx, library_id, None, 10).await?;
    assert_eq!(versions.iter().map(|v| v.version).collect::<Vec<_>>(), [3, 1]);
    let plugin = find_plugin(&mut tx, PluginName { publisher: IMPORT_PUBLISHER, name: &plugin_id }).await?.unwrap();
    assert_eq!(plugin.id, plan.plugins[0].plugin_id);
    let manifest = figcraft_cloud_storage::plugin_store::get_manifest(&mut tx, plugin.id, "1.0.0").await?.unwrap();
    assert_eq!(manifest.manifest["id"], "org.example.legacy");
    tx.rollback().await?;

    // A rerun writes nothing.
    let second = import_plan(&pool, &store, &plan, &f.root, &limits, &options).await?;
    assert!(second.libraries.iter().filter(|l| l.legacy_document == lib).all(|l| l.result == Ok(ImportStatus::AlreadyComplete)), "{second:?}");
    assert_eq!(second.plugins[0].result, Ok(ImportStatus::AlreadyComplete));

    // Imported libraries and the plugin verify and leave not_imported; the rejected one stays.
    let clean = verify().await?;
    assert!(clean.documents.iter().all(|d| d.passed), "{clean:?}");
    assert!(clean.plugins.iter().all(|p| p.passed), "{:?}", clean.plugins);
    let passed: Vec<_> = clean.libraries.iter().map(|l| (l.legacy_id.clone(), l.passed)).collect();
    assert_eq!(passed, [(bad.clone(), false), (lib.clone(), true), (lib.clone(), true)]);
    let left: Vec<_> = clean.not_imported.iter().map(|n| (n.kind, n.legacy_document.clone())).collect();
    assert_eq!(left, [(NotImportedKind::Library, Some(bad.clone()))]);
    assert!(!clean.cutover_ready);

    // A hand-edited plan cannot point a library outside its document folder.
    let mut hostile = plan.clone();
    for document in &mut hostile.documents {
        for library in &mut document.libraries {
            library.source = format!("files/{bad}/../{lib}/library-1.bin");
        }
    }
    let refused = import_plan(&pool, &store, &hostile, &f.root, &limits, &options).await?;
    assert!(refused.libraries.iter().all(|l| l.result == Err(ImportError::InvalidPlanEntry)), "{refused:?}");

    // Library bytes overwritten in the bucket (same size) and a changed legacy plugin bundle.
    let mut tx = begin(&pool).await?;
    let object = figcraft_cloud_storage::object_metadata::get_object(&mut tx, lib_plan.libraries[1].id).await?.unwrap();
    tx.rollback().await?;
    let mut bytes = store.get_object(&object.object_key, 1 << 20).await?;
    if let Some(last) = bytes.last_mut() {
        *last ^= 0xff;
    }
    store.put_object(&object.object_key, bytes, &object.media_type).await?;
    let mut changed = bundle.clone();
    if let Some(last) = changed.last_mut() {
        *last ^= 0x01;
    }
    f.write(&format!("plugins/{sha}.bin"), &changed);
    let tampered = verify().await?;
    let library = tampered.libraries.iter().find(|l| l.legacy_id == lib && l.version == "3").unwrap();
    assert_eq!(library.failures, [VerifyCode::ObjectBytesMismatch, VerifyCode::ContentMismatch]);
    assert_eq!(tampered.plugins[0].failures, [VerifyCode::LegacyChanged]);
    let left = tampered.not_imported.iter().filter(|n| matches!(n.kind, NotImportedKind::Library | NotImportedKind::Plugin)).count();
    assert_eq!(left, 3);
    assert!(!tampered.cutover_ready);

    delete_resources(&store, &plan, &pool).await?;
    delete_snapshots(&store, &plan, &pool).await?;
    pool.close().await;
    Ok(())
}
