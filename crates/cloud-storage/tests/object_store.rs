use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreError, ObjectStoreSettings};

/// `FIGCRAFT_TEST_S3_DIR` holds `s3_endpoint`, `s3_access_key` and `s3_secret_key` for a disposable
/// S3-compatible store with a `figcraft` bucket (path style, `us-east-1`).
fn settings() -> Result<Option<ObjectStoreSettings>, Box<dyn std::error::Error>> {
    let Ok(directory) = std::env::var("FIGCRAFT_TEST_S3_DIR") else {
        eprintln!("FIGCRAFT_TEST_S3_DIR is unset; S3-compatible object store probe skipped");
        return Ok(None);
    };
    let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::fs::read_to_string(std::path::Path::new(&directory).join(name))?.trim_end().to_string())
    };
    Ok(Some(ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    }))
}

#[tokio::test]
async fn connectivity_check_distinguishes_reachability_credentials_and_bucket() -> Result<(), Box<dyn std::error::Error>> {
    let Some(settings) = settings()? else {
        return Ok(());
    };
    ObjectStore::new(&settings)?.check_connectivity().await?;

    let wrong_secret = ObjectStoreSettings { secret_key: format!("{}x", settings.secret_key), ..settings.clone() };
    assert_eq!(ObjectStore::new(&wrong_secret)?.check_connectivity().await, Err(ObjectStoreError::Unauthorized));
    let wrong_key = ObjectStoreSettings { access_key: "FCnobody".into(), ..settings.clone() };
    assert_eq!(ObjectStore::new(&wrong_key)?.check_connectivity().await, Err(ObjectStoreError::Unauthorized));
    let missing_bucket = ObjectStoreSettings { bucket: "figcraft-missing".into(), ..settings.clone() };
    assert_eq!(ObjectStore::new(&missing_bucket)?.check_connectivity().await, Err(ObjectStoreError::BucketMissing));
    let closed_port = ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings.clone() };
    assert_eq!(ObjectStore::new(&closed_port)?.check_connectivity().await, Err(ObjectStoreError::Unreachable));
    Ok(())
}
