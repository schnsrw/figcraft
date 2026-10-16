use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::object_metadata::{
    NewObject, ObjectKind, ObjectRepositoryError, ObjectState, Rejection, Verification, get_object, keys, register_object, verify_object,
};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreError, ObjectStoreSettings};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "a1000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a1000000-0000-4000-8000-000000000002";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; object repository probe skipped");
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

/// Bytes unique to this run, so reruns never collide on content-addressed keys.
fn unique_bytes(label: &str) -> Vec<u8> {
    format!("figcraft object probe {label} {}", Uuid::new_v4()).into_bytes()
}

fn new_object(tenant: TenantId, bytes: &[u8]) -> NewObject {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Asset,
        object_key: keys::asset(tenant, &sha256),
        sha256,
        size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    }
}

#[tokio::test]
async fn objects_become_verified_only_when_the_stored_bytes_match() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;

    // Matching bytes verify; verifying again is a no-op.
    let good = unique_bytes("good");
    let object = new_object(tenant, &good);
    let registered = register_object(&mut scoped, store.bucket_name(), &object).await?;
    assert_eq!(registered.state, ObjectState::Pending);
    store.put_object(&object.object_key, good.clone(), &object.media_type).await?;
    let Some(Verification::Verified(verified)) = verify_object(&mut scoped, &store, object.id).await? else {
        return Err("matching bytes should verify".into());
    };
    assert_eq!(verified.state, ObjectState::Verified);
    assert!(matches!(verify_object(&mut scoped, &store, object.id).await?, Some(Verification::Verified(_))));
    assert_eq!(get_object(&mut scoped, object.id).await?.map(|object| object.state), Some(ObjectState::Verified));

    // Bytes that do not hash to the record, are a different size, or are missing are rejected,
    // and a rejected object cannot be verified later even once correct bytes appear.
    let claimed = unique_bytes("claimed");
    let tampered = new_object(tenant, &claimed);
    register_object(&mut scoped, store.bucket_name(), &tampered).await?;
    let mut altered = claimed.clone();
    if let Some(first) = altered.first_mut() {
        *first ^= 1;
    }
    store.put_object(&tampered.object_key, altered, &tampered.media_type).await?;
    assert!(matches!(verify_object(&mut scoped, &store, tampered.id).await?, Some(Verification::Rejected(_, Rejection::ChecksumMismatch))));
    store.put_object(&tampered.object_key, claimed, &tampered.media_type).await?;
    assert_eq!(verify_object(&mut scoped, &store, tampered.id).await, Err(ObjectRepositoryError::InvalidState));

    let short = unique_bytes("short");
    let resized = NewObject { size_bytes: i64::try_from(short.len() + 1)?, ..new_object(tenant, &short) };
    register_object(&mut scoped, store.bucket_name(), &resized).await?;
    store.put_object(&resized.object_key, short, &resized.media_type).await?;
    assert!(matches!(verify_object(&mut scoped, &store, resized.id).await?, Some(Verification::Rejected(_, Rejection::SizeMismatch))));

    let long = unique_bytes("long");
    let truncated = NewObject { size_bytes: i64::try_from(long.len() - 1)?, ..new_object(tenant, &long) };
    register_object(&mut scoped, store.bucket_name(), &truncated).await?;
    store.put_object(&truncated.object_key, long, &truncated.media_type).await?;
    assert!(matches!(verify_object(&mut scoped, &store, truncated.id).await?, Some(Verification::Rejected(_, Rejection::SizeMismatch))));

    let never_uploaded = new_object(tenant, &unique_bytes("missing"));
    register_object(&mut scoped, store.bucket_name(), &never_uploaded).await?;
    assert!(matches!(verify_object(&mut scoped, &store, never_uploaded.id).await?, Some(Verification::Rejected(_, Rejection::Missing))));
    assert_eq!(verify_object(&mut scoped, &store, Uuid::new_v4()).await?, None);

    // A store failure that says nothing about the bytes leaves the row pending for a retry.
    let retry = new_object(tenant, &unique_bytes("retry"));
    register_object(&mut scoped, store.bucket_name(), &retry).await?;
    let unreachable = ObjectStore::new(&ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings.clone() })?;
    assert_eq!(verify_object(&mut scoped, &unreachable, retry.id).await, Err(ObjectRepositoryError::Store(ObjectStoreError::Unreachable)));
    assert_eq!(get_object(&mut scoped, retry.id).await?.map(|object| object.state), Some(ObjectState::Pending));
    scoped.rollback().await?;

    // A second live row for one key, and a key under another tenant's prefix, are refused by the
    // table. Each violation aborts its transaction, so each gets its own.
    let mut duplicate = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let first = new_object(tenant, &unique_bytes("duplicate"));
    register_object(&mut duplicate, store.bucket_name(), &first).await?;
    let second = NewObject { id: Uuid::new_v4(), ..first.clone() };
    assert_eq!(register_object(&mut duplicate, store.bucket_name(), &second).await, Err(ObjectRepositoryError::Conflict));
    duplicate.rollback().await?;
    let mut foreign = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let other: TenantId = OTHER_TENANT.parse()?;
    assert_eq!(
        register_object(&mut foreign, store.bucket_name(), &new_object(other, &unique_bytes("foreign"))).await,
        Err(ObjectRepositoryError::Constraint)
    );
    foreign.rollback().await?;

    // Another tenant cannot read this tenant's object.
    let mut visible = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let committed = new_object(tenant, &unique_bytes("committed"));
    register_object(&mut visible, store.bucket_name(), &committed).await?;
    visible.commit().await?;
    let mut other_scope = begin_tenant_transaction(&pool, other).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(get_object(&mut other_scope, committed.id).await?, None);
    other_scope.rollback().await?;

    pool.close().await;
    Ok(())
}
