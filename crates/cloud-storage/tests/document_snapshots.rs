use figcraft_cloud_domain::{CloudDocumentId, ProjectId, WorkspaceId};
use figcraft_cloud_storage::document_snapshots::{
    Pointer, SkipReason, SkippedSnapshot, SnapshotStoreError, StoredSnapshot, restore_latest_snapshot, restore_latest_snapshot_with, store_snapshot,
};
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::object_metadata::{ObjectKind, ObjectState, get_object};
use figcraft_cloud_storage::object_references::{OwnerKind, ReferenceOwner, list_references};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::validation::MEDIA_SNAPSHOT;
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::{Document, Limits, Server, SessionState};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "a6000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a6000000-0000-4000-8000-000000000002";
const WORKSPACE: &str = "a6100000-0000-4000-8000-000000000001";
const PROJECT: &str = "a6200000-0000-4000-8000-000000000001";
const PRODUCER: &str = "figcraft-cloud test";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; document snapshot probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string())
    };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    Ok(Some((pool, settings)))
}

async fn scoped<'a>(pool: &'a PgPool, tenant: &str) -> Result<TenantTransaction<'a>, Box<dyn std::error::Error>> {
    Ok(begin_tenant_transaction(pool, tenant.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?)
}

/// A fresh document per run, so reruns never meet an earlier run's pointer or snapshots.
async fn new_document(pool: &PgPool) -> Result<CloudDocumentId, Box<dyn std::error::Error>> {
    let mut transaction = scoped(pool, TENANT).await?;
    let id = CloudDocumentId::random();
    let workspace: WorkspaceId = WORKSPACE.parse()?;
    let project: ProjectId = PROJECT.parse()?;
    create_document(&mut transaction, id, workspace, project, "Snapshot probe").await?;
    transaction.commit().await?;
    Ok(id)
}

/// Advance the document's committed sequence as appends would (the counter only grows).
async fn advance(pool: &PgPool, document: CloudDocumentId, seq: i64) -> Result<(), Box<dyn std::error::Error>> {
    let mut transaction = scoped(pool, TENANT).await?;
    sqlx::query("UPDATE documents SET current_seq = $3 WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(document.to_string())
        .bind(seq)
        .execute(transaction.connection())
        .await?;
    transaction.commit().await?;
    Ok(())
}

async fn pointer(pool: &PgPool, document: CloudDocumentId) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut transaction = scoped(pool, TENANT).await?;
    let id: Option<String> = sqlx::query_scalar("SELECT current_snapshot_id::text FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(document.to_string())
        .fetch_one(transaction.connection())
        .await?;
    transaction.rollback().await?;
    Ok(id)
}

/// A distinct session state at `seq` (the document name makes each one's bytes unique).
fn state(seq: u64) -> Result<SessionState, Box<dyn std::error::Error>> {
    let mut state = Server::new(Document::new(format!("Snapshot at {seq} {}", CloudDocumentId::random())), "", Limits::default()).state()?;
    state.seq = seq;
    Ok(state)
}

async fn store(
    pool: &PgPool,
    objects: &ObjectStore,
    document: CloudDocumentId,
    state: &SessionState,
) -> Result<Result<StoredSnapshot, SnapshotStoreError>, Box<dyn std::error::Error>> {
    let mut transaction = scoped(pool, TENANT).await?;
    let stored = store_snapshot(&mut transaction, objects, document, state, PRODUCER).await;
    if stored.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    Ok(stored)
}

/// Flip one byte of a stored object in place: the size stays, the checksum no longer matches.
async fn corrupt(objects: &ObjectStore, key: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = objects.get_object(key, 1 << 30).await?;
    let last = bytes.last_mut().ok_or("empty object")?;
    *last ^= 0x5a;
    objects.put_object(key, bytes, MEDIA_SNAPSHOT).await?;
    Ok(())
}

#[tokio::test]
async fn snapshots_are_stored_verified_and_restored_newest_first() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let objects = ObjectStore::new(&settings)?;
    let document = new_document(&pool).await?;
    let mut keys = Vec::new();

    // A snapshot past the committed log is refused before anything is uploaded.
    let two = state(2)?;
    assert_eq!(store(&pool, &objects, document, &two).await?, Err(SnapshotStoreError::AheadOfLog));
    advance(&pool, document, 5).await?;

    // Stored: an immutable, verified snapshot object under the content-addressed key, a verified
    // snapshot row referencing it, and the pointer moved.
    let at_two = store(&pool, &objects, document, &two).await??;
    keys.push(at_two.object.object_key.clone());
    assert_eq!((at_two.snapshot.seq, at_two.snapshot.state.as_str(), at_two.pointer, at_two.reused_object), (2, "verified", Pointer::Moved, false));
    assert_eq!(
        (at_two.object.kind, at_two.object.state, at_two.object.media_type.as_str()),
        (ObjectKind::Snapshot, ObjectState::Verified, MEDIA_SNAPSHOT)
    );
    assert!(at_two.object.object_key.starts_with(&format!("tenants/{TENANT}/documents/{document}/snapshots/2-")));
    assert_eq!(at_two.snapshot.object_id, at_two.object.id);
    let mut transaction = scoped(&pool, TENANT).await?;
    assert_eq!(
        list_references(&mut transaction, at_two.object.id, 10).await?,
        vec![ReferenceOwner::Owner(OwnerKind::DocumentSnapshot, at_two.snapshot.id)]
    );
    transaction.rollback().await?;
    assert_eq!(pointer(&pool, document).await?, Some(at_two.snapshot.id.to_string()));

    // The pointer moves forward, never backwards.
    let four = state(4)?;
    let at_four = store(&pool, &objects, document, &four).await??;
    keys.push(at_four.object.object_key.clone());
    assert_eq!(at_four.pointer, Pointer::Moved);
    let three = state(3)?;
    let at_three = store(&pool, &objects, document, &three).await??;
    keys.push(at_three.object.object_key.clone());
    assert_eq!(at_three.pointer, Pointer::Kept);
    assert_eq!(pointer(&pool, document).await?, Some(at_four.snapshot.id.to_string()));

    // Storing the same state again reuses the object and the snapshot row.
    let again = store(&pool, &objects, document, &four).await??;
    assert_eq!(
        (again.snapshot.id, again.object.id, again.reused_object, again.pointer),
        (at_four.snapshot.id, at_four.object.id, true, Pointer::Moved)
    );

    // The newest verified snapshot is restored, its bytes checked again on read.
    let mut transaction = scoped(&pool, TENANT).await?;
    let restore = restore_latest_snapshot(&mut transaction, &objects, document).await?;
    transaction.rollback().await?;
    assert_eq!(restore.restored.as_ref().map(|(record, state)| (record.id, state)), Some((at_four.snapshot.id, &four)));
    assert!(restore.skipped.is_empty());

    // A corrupted object is never used: the restore falls back to the preceding snapshot, then
    // past a missing one to the one before.
    corrupt(&objects, &at_four.object.object_key).await?;
    let mut transaction = scoped(&pool, TENANT).await?;
    let restore = restore_latest_snapshot(&mut transaction, &objects, document).await?;
    transaction.rollback().await?;
    assert_eq!(restore.restored.as_ref().map(|(record, state)| (record.id, state)), Some((at_three.snapshot.id, &three)));
    assert_eq!(restore.skipped, vec![SkippedSnapshot { id: at_four.snapshot.id, seq: 4, reason: SkipReason::ChecksumMismatch }]);
    objects.delete_object(&at_three.object.object_key).await?;
    let mut transaction = scoped(&pool, TENANT).await?;
    let restore = restore_latest_snapshot(&mut transaction, &objects, document).await?;
    transaction.rollback().await?;
    assert_eq!(restore.restored.as_ref().map(|(record, state)| (record.id, state)), Some((at_two.snapshot.id, &two)));
    assert_eq!(
        restore.skipped.iter().map(|skipped| (skipped.seq, skipped.reason)).collect::<Vec<_>>(),
        vec![(4, SkipReason::ChecksumMismatch), (3, SkipReason::Missing)]
    );

    // A caller that refuses every state gets nothing, with every candidate reported.
    let mut transaction = scoped(&pool, TENANT).await?;
    let refused = restore_latest_snapshot_with(&mut transaction, &objects, document, |_| None::<()>).await?;
    transaction.rollback().await?;
    assert_eq!(refused.restored, None);
    assert_eq!(refused.skipped.last().map(|skipped| (skipped.seq, skipped.reason)), Some((2, SkipReason::Refused)));

    // Storing the corrupted state again rewrites its content-addressed bytes and checks them, so
    // the newest snapshot is usable again.
    let repaired = store(&pool, &objects, document, &four).await??;
    assert!(repaired.reused_object);
    let mut transaction = scoped(&pool, TENANT).await?;
    let restore = restore_latest_snapshot(&mut transaction, &objects, document).await?;
    transaction.rollback().await?;
    assert_eq!(restore.restored.map(|(record, _)| record.seq), Some(4));

    // Another tenant sees none of it and cannot store against the document.
    let mut other = scoped(&pool, OTHER_TENANT).await?;
    let restore = restore_latest_snapshot(&mut other, &objects, document).await?;
    assert_eq!((restore.restored, restore.skipped), (None, Vec::new()));
    assert_eq!(get_object(&mut other, at_four.object.id).await?, None);
    assert_eq!(store_snapshot(&mut other, &objects, document, &state(1)?, PRODUCER).await.map(|_| ()), Err(SnapshotStoreError::NotFound));
    other.rollback().await?;

    // Bad input is refused without touching the store.
    let mut transaction = scoped(&pool, TENANT).await?;
    assert_eq!(store_snapshot(&mut transaction, &objects, document, &two, "").await.map(|_| ()), Err(SnapshotStoreError::InvalidInput));
    assert_eq!(
        store_snapshot(&mut transaction, &objects, document, &state(u64::MAX)?, PRODUCER).await.map(|_| ()),
        Err(SnapshotStoreError::InvalidInput)
    );
    assert_eq!(
        store_snapshot(&mut transaction, &objects, CloudDocumentId::random(), &two, PRODUCER).await.map(|_| ()),
        Err(SnapshotStoreError::NotFound)
    );
    transaction.rollback().await?;

    for key in keys {
        objects.delete_object(&key).await?;
    }
    pool.close().await;
    Ok(())
}
