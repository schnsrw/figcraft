//! S3-020: a database backup plus a bucket version manifest restores a document exactly as it was.
//!
//! The probe builds a document in a scratch database, takes a logical backup of every application
//! table (the in-test equivalent of `pg_dump --data-only`, since the test cannot shell out) and
//! records the bucket's object versions under the document's prefix. It then mutates both sides
//! (more events, an overwritten object, a deleted row), restores the backup into a second freshly
//! migrated scratch database and proves that the restored metadata reopens the document at the
//! backed-up sequence, that the backed-up object version still holds the recorded bytes in the
//! versioned bucket, and that nothing written after the backup survives the restore.
//!
//! Needs `FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE` (an administrator URL that may create databases)
//! and `FIGCRAFT_TEST_S3_DIR` (a versioned `figcraft` bucket). Fixture prefix `ae`.

use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::events::{EventRepositoryError, MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS, append_events, claim_sequencer, read_events};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_tenant, create_workspace, get_document, get_project};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, ObjectState, Verification, get_object, keys, register_object, verify_object};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::snapshots::{register_snapshot, set_current_snapshot, verify_snapshot};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction, check_schema_version, migrate_database};
use figcraft_collab::{Accepted, Change, NodeId};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

type Probe<T = ()> = Result<T, Box<dyn std::error::Error>>;

const TENANT: &str = "ae000000-0000-4000-8000-000000000001";
const ACTOR: &str = "ae000000-0000-4000-8000-000000000009";
const WORKSPACE: &str = "ae100000-0000-4000-8000-000000000001";
const PROJECT: &str = "ae200000-0000-4000-8000-000000000001";
/// A project that exists at backup time and is deleted afterwards; the restore must bring it back.
const DOOMED_PROJECT: &str = "ae200000-0000-4000-8000-000000000002";
/// A document created only after the backup; the restore must not contain it.
const LATE_DOCUMENT: &str = "ae300000-0000-4000-8000-000000000002";

/// Committed before the backup, and after it.
const BACKED_UP_EVENTS: u64 = 3;
const LATE_EVENTS: u64 = 2;

const DROP_DATABASES: [&str; 2] =
    ["DROP DATABASE IF EXISTS figcraft_backup_source_ae WITH (FORCE)", "DROP DATABASE IF EXISTS figcraft_backup_restore_ae WITH (FORCE)"];
const CREATE_SOURCE: &str = "CREATE DATABASE figcraft_backup_source_ae";
const CREATE_RESTORE: &str = "CREATE DATABASE figcraft_backup_restore_ae";

/// Most rows one table may contribute to the backup; the fixture is tiny, so hitting this is a bug.
const MAX_ROWS_PER_TABLE: i64 = 10_000;
/// Most object versions the manifest accepts under the document prefix (one list page).
const MAX_VERSIONS: usize = 1000;
const MAX_LISTING_BYTES: usize = 1024 * 1024;
const TTL: Duration = Duration::from_secs(60);

/// One table's logical backup step: its name, a bounded export as a JSON array of rows, and the
/// matching load. SQLx rejects SQL built at runtime, so every statement is a literal.
struct Table {
    name: &'static str,
    export: &'static str,
    load: &'static str,
}

macro_rules! table {
    ($name:literal) => {
        Table {
            name: $name,
            export: concat!(
                "SELECT count(*)::bigint, COALESCE(json_agg(row_to_json(t)), '[]'::json)::text FROM (SELECT * FROM public.",
                $name,
                " LIMIT 10001) t"
            ),
            load: concat!("INSERT INTO public.", $name, " SELECT * FROM json_populate_recordset(NULL::public.", $name, ", $1::json)"),
        }
    };
}

/// Every application table, parents before children so each load satisfies its foreign keys.
/// `_sqlx_migrations` is not data: the restore target is migrated by the binary instead, and the
/// test asserts both ends report the same schema version. `documents.current_snapshot_id` and
/// `document_snapshots` reference each other, so documents load without the pointer and get it
/// back once the snapshots exist ([`RELINK_CURRENT_SNAPSHOTS`]).
fn tables() -> [Table; 40] {
    [
        table!("principals"),
        table!("identity_links"),
        table!("tenants"),
        table!("tenant_members"),
        table!("teams"),
        table!("team_members"),
        table!("workspaces"),
        table!("workspace_members"),
        table!("workspace_team_grants"),
        table!("projects"),
        Table {
            name: "documents",
            export: table!("documents").export,
            load: concat!(
                "INSERT INTO public.documents SELECT * FROM json_populate_recordset(NULL::public.documents, ",
                "(SELECT COALESCE(json_agg(e::jsonb - 'current_snapshot_id'), '[]'::json) FROM json_array_elements($1::json) e))"
            ),
        },
        table!("document_members"),
        table!("document_team_grants"),
        table!("document_events"),
        table!("objects"),
        table!("document_snapshots"),
        table!("jobs"),
        table!("idempotency_keys"),
        table!("audit_events"),
        table!("external_mappings"),
        table!("object_references"),
        table!("upload_reservations"),
        table!("libraries"),
        table!("library_team_grants"),
        table!("library_versions"),
        table!("library_usages"),
        table!("plugins"),
        table!("plugin_versions"),
        table!("exports"),
        table!("document_thumbnails"),
        // S3 follow-ups, the core slice and S4, each after every table it references.
        table!("retention_holds"),
        table!("share_links"),
        table!("oidc_login_attempts"),
        table!("browser_sessions"),
        table!("tenant_bootstrap"),
        table!("service_accounts"),
        table!("service_account_credentials"),
        table!("team_invitations"),
        table!("member_permissions"),
        table!("support_access_grants"),
    ]
}

const RELINK_CURRENT_SNAPSHOTS: &str = concat!(
    "UPDATE public.documents d SET current_snapshot_id = b.current_snapshot_id ",
    "FROM json_populate_recordset(NULL::public.documents, $1::json) b ",
    "WHERE d.tenant_id = b.tenant_id AND d.id = b.id AND b.current_snapshot_id IS NOT NULL"
);

const APPLICATION_TABLES: &str =
    "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations' ORDER BY tablename";

/// A logical backup: each table's rows as a JSON array, in load order.
struct DatabaseBackup {
    rows: Vec<(&'static str, String)>,
}

/// One entry of the bucket version manifest taken with the backup.
#[derive(Clone, Debug, Eq, PartialEq)]
struct VersionEntry {
    key: String,
    version_id: String,
    etag: String,
    size: u64,
    is_latest: bool,
}

/// Raw signed bucket access the shipped API deliberately lacks: version listing, versioned reads
/// and version deletes.
struct Raw {
    bucket: Bucket,
    credentials: Credentials,
    client: reqwest::Client,
}

impl Raw {
    fn new(settings: &ObjectStoreSettings) -> Probe<Self> {
        let bucket = Bucket::new(settings.endpoint.parse()?, UrlStyle::Path, settings.bucket.clone(), settings.region.clone())?;
        // `reqwest::Client::new()` panics under `rustls-no-provider`.
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let client = reqwest::Client::builder().tls_backend_preconfigured(tls).timeout(Duration::from_secs(30)).build()?;
        Ok(Self { bucket, credentials: Credentials::new(settings.access_key.clone(), settings.secret_key.clone()), client })
    }

    /// `ListObjectVersions` under `prefix` (one bounded page; a truncated listing is an error).
    async fn list_versions(&self, prefix: &str) -> Probe<Vec<VersionEntry>> {
        let url = rusty_s3::signing::sign(
            &jiff::Timestamp::now(),
            rusty_s3::Method::Get,
            self.bucket.base_url().clone(),
            self.credentials.key(),
            self.credentials.secret(),
            self.credentials.token(),
            self.bucket.region(),
            TTL.as_secs(),
            // `sign` merges this with its own sorted parameters, so it must already be sorted.
            [("prefix", prefix), ("versions", "")].into_iter(),
            std::iter::empty(),
        );
        let response = self.client.get(url.as_str()).send().await?;
        let status = response.status().as_u16();
        let body = response.bytes().await?;
        if status != 200 || body.len() > MAX_LISTING_BYTES {
            return Err(format!("version listing answered {status} with {} bytes", body.len()).into());
        }
        parse_versions(std::str::from_utf8(&body)?)
    }

    async fn get_version(&self, key: &str, version: &str) -> Probe<Vec<u8>> {
        let mut action = self.bucket.get_object(Some(&self.credentials), key);
        action.query_mut().insert("versionId", version.to_string());
        let response = self.client.get(action.sign(TTL).as_str()).send().await?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!("versioned read answered {status}").into());
        }
        Ok(response.bytes().await?.to_vec())
    }

    async fn delete_version(&self, key: &str, version: &str) -> Probe<u16> {
        let mut action = self.bucket.delete_object(Some(&self.credentials), key);
        action.query_mut().insert("versionId", version.to_string());
        Ok(self.client.delete(action.sign(TTL).as_str()).send().await?.status().as_u16())
    }
}

/// Parse a `ListVersionsResult`. Delete markers carry no bytes and are listed with size 0 and an
/// empty ETag so cleanup can remove them too.
fn parse_versions(xml: &str) -> Probe<Vec<VersionEntry>> {
    let document = roxmltree::Document::parse(xml)?;
    let root = document.root_element();
    let text = |node: roxmltree::Node<'_, '_>, name: &str| -> String {
        node.children().find(|child| child.has_tag_name(name)).and_then(|child| child.text()).unwrap_or_default().to_string()
    };
    if text(root, "IsTruncated") == "true" {
        return Err("version listing is truncated; the manifest must be complete".into());
    }
    let mut entries = Vec::new();
    for node in root.children().filter(|node| node.has_tag_name("Version") || node.has_tag_name("DeleteMarker")) {
        if entries.len() >= MAX_VERSIONS {
            return Err("too many object versions".into());
        }
        entries.push(VersionEntry {
            key: text(node, "Key"),
            version_id: text(node, "VersionId"),
            etag: text(node, "ETag").trim_matches('"').to_string(),
            size: text(node, "Size").parse().unwrap_or(0),
            is_latest: text(node, "IsLatest") == "true",
        });
    }
    Ok(entries)
}

fn database_url(admin_url: &str, database: &str) -> Probe<String> {
    let (server, _) = admin_url.rsplit_once('/').ok_or("database URL has no path")?;
    Ok(format!("{server}/{database}"))
}

fn store_settings(directory: &str) -> Probe<ObjectStoreSettings> {
    let read = |name: &str| -> Probe<String> { Ok(std::fs::read_to_string(std::path::Path::new(directory).join(name))?.trim_end().to_string()) };
    Ok(ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    })
}

fn batch(seq: u64) -> Accepted {
    Accepted { seq, author: 1, batch_id: seq, changes: vec![Change::Delete { id: NodeId(seq * 10) }] }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

async fn begin(pool: &PgPool) -> Probe<TenantTransaction<'_>> {
    Ok(begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?)
}

async fn append(pool: &PgPool, document: CloudDocumentId, count: u64) -> Probe {
    let mut writer = begin(pool).await?;
    let claim = claim_sequencer(&mut writer, document).await?;
    let first = u64::try_from(claim.current_seq)? + 1;
    let batches: Vec<Accepted> = (first..first + count).map(batch).collect();
    append_events(&mut writer, document, claim.fencing_token, ACTOR.parse()?, &batches).await?;
    writer.commit().await?;
    Ok(())
}

/// Everything the backup captured that the assertions compare against.
struct Captured {
    document: CloudDocumentId,
    object_id: Uuid,
    object_key: String,
    snapshot_id: Uuid,
    snapshot_bytes: Vec<u8>,
    fencing_token: i64,
    backup: DatabaseBackup,
    manifest: Vec<VersionEntry>,
}

#[tokio::test]
async fn database_backup_and_bucket_versions_restore_a_document_to_the_backup_point() -> Probe {
    let (Ok(admin_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
    else {
        eprintln!("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; backup/restore probe skipped");
        return Ok(());
    };
    let admin_url = std::fs::read_to_string(admin_url_file)?.trim_end().to_string();
    let settings = store_settings(&s3_directory)?;
    let store = ObjectStore::new(&settings)?;
    let raw = Raw::new(&settings)?;
    let admin = PgPoolOptions::new().max_connections(1).connect(&admin_url).await?;
    for statement in DROP_DATABASES {
        sqlx::query(statement).execute(&admin).await?;
    }

    // A fresh document id per run keeps this run's bucket prefix free of any earlier run's versions.
    let document = CloudDocumentId::random();
    let prefix = format!("tenants/{TENANT}/documents/{document}/");
    let outcome = probe(&admin, &admin_url, &store, &raw, document).await;

    // Cleanup runs whatever happened: every version and delete marker under the prefix, then the
    // scratch databases.
    let mut cleanup: Probe = Ok(());
    match raw.list_versions(&prefix).await {
        Ok(versions) => {
            for version in versions {
                let status = raw.delete_version(&version.key, &version.version_id).await?;
                if !matches!(status, 200 | 204) {
                    cleanup = Err(format!("version delete answered {status}").into());
                }
            }
        }
        Err(error) => cleanup = Err(error),
    }
    for statement in DROP_DATABASES {
        sqlx::query(statement).execute(&admin).await?;
    }
    admin.close().await;
    outcome?;
    cleanup?;
    assert!(raw.list_versions(&prefix).await?.is_empty(), "cleanup leaves no versions behind");
    Ok(())
}

async fn probe(admin: &PgPool, admin_url: &str, store: &ObjectStore, raw: &Raw, document: CloudDocumentId) -> Probe {
    sqlx::query(CREATE_SOURCE).execute(admin).await?;
    let source_url = database_url(admin_url, "figcraft_backup_source_ae")?;
    migrate_database(&source_url).await.map_err(|error| format!("migrate source: {error:?}"))?;
    let source = PgPoolOptions::new().max_connections(2).connect(&source_url).await?;
    let captured = build_and_back_up(&source, store, raw, document).await?;
    mutate(&source, store, &captured).await?;
    let source_version = check_schema_version(&source_url, 0, i64::MAX).await.map_err(|error| format!("source version: {error:?}"))?;
    source.close().await;

    sqlx::query(CREATE_RESTORE).execute(admin).await?;
    let restore_url = database_url(admin_url, "figcraft_backup_restore_ae")?;
    migrate_database(&restore_url).await.map_err(|error| format!("migrate restore: {error:?}"))?;
    // A logical backup restores only into the schema version it was taken from.
    let restore_version = check_schema_version(&restore_url, source_version, source_version).await;
    assert_eq!(restore_version, Ok(source_version), "restore target must be migrated to the backup's schema version");
    let restored = PgPoolOptions::new().max_connections(2).connect(&restore_url).await?;
    let result = async {
        restore(&restored, &captured.backup).await?;
        assert_restored(&restored, store, raw, &captured).await
    }
    .await;
    restored.close().await;
    result
}

/// Step 1 and 2: build the document, then back up the database and the bucket version manifest.
async fn build_and_back_up(pool: &PgPool, store: &ObjectStore, raw: &Raw, document: CloudDocumentId) -> Probe<Captured> {
    let tenant: TenantId = TENANT.parse()?;
    let workspace: WorkspaceId = WORKSPACE.parse()?;
    let project: ProjectId = PROJECT.parse()?;

    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'Backup restore probe')")
        .bind(ACTOR)
        .execute(pool)
        .await?;
    let mut scoped = begin(pool).await?;
    create_tenant(&mut scoped, "backup-ae", "Backup AE", "test", "local").await?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, 'tenant_owner', 'active')")
        .bind(TENANT)
        .bind(ACTOR)
        .execute(scoped.connection())
        .await?;
    create_workspace(&mut scoped, workspace, "Backup workspace", None).await?;
    create_project(&mut scoped, project, workspace, "Backup project", None).await?;
    create_project(&mut scoped, DOOMED_PROJECT.parse()?, workspace, "Deleted after backup", None).await?;
    create_document(&mut scoped, document, workspace, project, "Backup document").await?;
    scoped.commit().await?;
    append(pool, document, BACKED_UP_EVENTS).await?;

    // A snapshot object at the backed-up sequence, verified against the bucket, and its snapshot row.
    let snapshot_bytes = format!("figcraft backup probe snapshot {}", Uuid::new_v4()).into_bytes();
    let digest = sha256(&snapshot_bytes);
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Snapshot,
        object_key: keys::snapshot(tenant, document, BACKED_UP_EVENTS, &digest),
        sha256: digest,
        size_bytes: i64::try_from(snapshot_bytes.len())?,
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    };
    let snapshot_id = Uuid::new_v4();
    let mut scoped = begin(pool).await?;
    register_object(&mut scoped, store.bucket_name(), &object).await?;
    store.put_object(&object.object_key, snapshot_bytes.clone(), &object.media_type).await?;
    assert!(matches!(verify_object(&mut scoped, store, object.id).await?, Some(Verification::Verified(_))));
    register_snapshot(&mut scoped, snapshot_id, document, i64::try_from(BACKED_UP_EVENTS)?, object.id, &digest, object.size_bytes).await?;
    verify_snapshot(&mut scoped, snapshot_id, document).await?.ok_or("snapshot should exist")?;
    set_current_snapshot(&mut scoped, document, snapshot_id).await?;
    scoped.commit().await?;

    // The backup covers every application table; a new table that this list misses fails here
    // rather than silently going unrestored.
    let present: Vec<String> = sqlx::query_scalar(APPLICATION_TABLES).fetch_all(pool).await?;
    let mut listed: Vec<String> = tables().iter().map(|table| table.name.to_string()).collect();
    listed.sort();
    assert_eq!(present, listed, "the backup table list must match the schema");

    // One repeatable-read transaction, so every table is exported from the same instant (what
    // `pg_dump` does with its snapshot).
    let mut export = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *export).await?;
    let mut rows = Vec::new();
    for table in tables() {
        let (count, json): (i64, String) = sqlx::query_as(table.export).fetch_one(&mut *export).await?;
        if count > MAX_ROWS_PER_TABLE {
            return Err(format!("{} has more rows than the bounded probe backup takes", table.name).into());
        }
        rows.push((table.name, json));
    }
    let fencing_token: i64 =
        sqlx::query_scalar("SELECT fencing_token FROM documents WHERE id = $1::uuid").bind(document.to_string()).fetch_one(&mut *export).await?;
    export.commit().await?;

    // The bucket half of the backup: the versions that were current when the database was dumped.
    let manifest = raw.list_versions(&format!("tenants/{TENANT}/documents/{document}/")).await?;
    let [entry] = manifest.as_slice() else {
        return Err(format!("expected one object version, found {}", manifest.len()).into());
    };
    assert_eq!((entry.key.as_str(), entry.size, entry.is_latest), (object.object_key.as_str(), u64::try_from(snapshot_bytes.len())?, true));
    assert!(!entry.version_id.is_empty() && entry.version_id != "null", "the bucket must be versioned");
    assert!(!entry.etag.is_empty());

    Ok(Captured {
        document,
        object_id: object.id,
        object_key: object.object_key,
        snapshot_id,
        snapshot_bytes,
        fencing_token,
        backup: DatabaseBackup { rows },
        manifest,
    })
}

/// Step 3: change both halves after the backup.
async fn mutate(pool: &PgPool, store: &ObjectStore, captured: &Captured) -> Probe {
    append(pool, captured.document, LATE_EVENTS).await?;
    // Overwriting the key makes a new current version; the backed-up one becomes noncurrent.
    store.put_object(&captured.object_key, b"overwritten after the backup".to_vec(), "application/octet-stream").await?;
    let mut scoped = begin(pool).await?;
    let deleted = sqlx::query("DELETE FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(DOOMED_PROJECT)
        .execute(scoped.connection())
        .await?;
    assert_eq!(deleted.rows_affected(), 1);
    create_document(&mut scoped, LATE_DOCUMENT.parse()?, WORKSPACE.parse()?, PROJECT.parse()?, "Created after backup").await?;
    scoped.commit().await?;
    Ok(())
}

/// Step 4: load the logical backup into the freshly migrated database in one transaction.
async fn restore(pool: &PgPool, backup: &DatabaseBackup) -> Probe {
    let mut load = pool.begin().await?;
    for (table, (name, json)) in tables().iter().zip(&backup.rows) {
        assert_eq!(table.name, *name);
        sqlx::query(table.load).bind(json).execute(&mut *load).await?;
    }
    let documents = backup.rows.iter().find(|(name, _)| *name == "documents").map(|(_, json)| json.as_str()).ok_or("documents missing")?;
    sqlx::query(RELINK_CURRENT_SNAPSHOTS).bind(documents).execute(&mut *load).await?;
    load.commit().await?;
    Ok(())
}

/// Step 5: the restored database reopens the document exactly at the backup point.
async fn assert_restored(pool: &PgPool, store: &ObjectStore, raw: &Raw, captured: &Captured) -> Probe {
    let document = captured.document;
    let backed_up_seq = i64::try_from(BACKED_UP_EVENTS)?;
    let mut scoped = begin(pool).await?;

    // Events replay to the backed-up sequence and no further; the late events are gone.
    let page = read_events(&mut scoped, document, 0, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await?;
    assert_eq!(page.current_seq, backed_up_seq);
    assert!(!page.has_more());
    assert_eq!(page.events.iter().map(|event| event.batch.clone()).collect::<Vec<_>>(), (1..=BACKED_UP_EVENTS).map(batch).collect::<Vec<_>>());
    let actor: PrincipalId = ACTOR.parse()?;
    assert!(page.events.iter().all(|event| event.actor == actor));
    assert_eq!(
        read_events(&mut scoped, document, backed_up_seq + 1, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await,
        Err(EventRepositoryError::SequenceConflict { current: backed_up_seq })
    );

    // The deleted row is back and the late document never existed.
    assert!(get_project(&mut scoped, WORKSPACE.parse()?, DOOMED_PROJECT.parse()?).await?.is_some());
    assert_eq!(get_document(&mut scoped, LATE_DOCUMENT.parse()?).await?, None);
    assert!(get_document(&mut scoped, document).await?.is_some());
    let current: Option<String> = sqlx::query_scalar("SELECT current_snapshot_id::text FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(document.to_string())
        .fetch_one(scoped.connection())
        .await?;
    assert_eq!(current, Some(captured.snapshot_id.to_string()), "the current snapshot pointer is restored");

    // The object row survives verified, and the backed-up version still holds exactly its bytes.
    let object = get_object(&mut scoped, captured.object_id).await?.ok_or("restored object row")?;
    assert_eq!(object.state, ObjectState::Verified);
    assert_eq!(object.object_key, captured.object_key);
    let current_bytes = store.get_object(&object.object_key, 1024).await?;
    assert_ne!(sha256(&current_bytes), object.sha256, "the current version is the post-backup overwrite");
    let entry = captured.manifest.first().ok_or("manifest entry")?;
    let backed_up = raw.get_version(&entry.key, &entry.version_id).await?;
    assert_eq!(backed_up, captured.snapshot_bytes);
    assert_eq!(sha256(&backed_up), object.sha256, "the backed-up version verifies against the restored row");
    assert_eq!(i64::try_from(backed_up.len())?, object.size_bytes);
    // The manifest version is still listed, now noncurrent.
    let versions = raw.list_versions(&format!("tenants/{TENANT}/documents/{document}/")).await?;
    assert!(versions.iter().any(|version| version.version_id == entry.version_id && version.etag == entry.etag && !version.is_latest));

    // Promote the backed-up bytes back to current, the bucket half of the restore; reads by key
    // then match the restored metadata again.
    store.put_object(&object.object_key, backed_up, &object.media_type).await?;
    assert_eq!(sha256(&store.get_object(&object.object_key, 1024).await?), object.sha256);
    scoped.rollback().await?;

    // The document reopens for writing: the restored fencing token is superseded by a new claim
    // and the next append continues the backed-up sequence.
    let mut writer = begin(pool).await?;
    let claim = claim_sequencer(&mut writer, document).await?;
    assert_eq!((claim.current_seq, claim.fencing_token), (backed_up_seq, captured.fencing_token + 1));
    let next = append_events(&mut writer, document, claim.fencing_token, actor, &[batch(BACKED_UP_EVENTS + 1)]).await?;
    assert_eq!((next.first_seq, next.last_seq), (backed_up_seq + 1, backed_up_seq + 1));
    writer.rollback().await?;
    Ok(())
}

/// The version listing parser never panics on junk and refuses truncated listings.
#[test]
fn version_listing_parser_is_bounded_and_total() {
    let listing = concat!(
        r#"<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>false</IsTruncated>"#,
        r#"<Version><Key>k</Key><VersionId>v1</VersionId><IsLatest>true</IsLatest><ETag>"e"</ETag><Size>4</Size></Version>"#,
        "<DeleteMarker><Key>k</Key><VersionId>v0</VersionId><IsLatest>false</IsLatest></DeleteMarker></ListVersionsResult>"
    );
    let parsed = parse_versions(listing).ok();
    assert_eq!(
        parsed,
        Some(vec![
            VersionEntry { key: "k".into(), version_id: "v1".into(), etag: "e".into(), size: 4, is_latest: true },
            VersionEntry { key: "k".into(), version_id: "v0".into(), etag: String::new(), size: 0, is_latest: false },
        ])
    );
    assert!(parse_versions("<ListVersionsResult><IsTruncated>true</IsTruncated></ListVersionsResult>").is_err());
    for junk in ["", "<", "<a>", "<ListVersionsResult><Version><Size>-1</Size></Version>", "\u{0}\u{ffff}"] {
        let _ = parse_versions(junk);
    }
}
