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

#[tokio::test]
async fn presigned_requests_upload_and_download_exactly_one_key() -> Result<(), Box<dyn std::error::Error>> {
    let Some(settings) = settings()? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let key = format!("tenants/a0000000-0000-4000-8000-000000000001/staging/{}", figcraft_cloud_domain::Uuid::new_v4());
    let body = b"presigned probe".to_vec();
    // `reqwest::Client::new()` panics under `rustls-no-provider`: every client needs an explicit
    // provider, as `ObjectStore` gives its own.
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let client = reqwest::Client::builder().tls_backend_preconfigured(tls).build()?;
    let ttl = std::time::Duration::from_secs(60);

    let put = store.presign_put(&key, ttl)?;
    assert!(client.put(&put).body(body.clone()).send().await?.status().is_success());
    let get = store.presign_get(&key, ttl)?;
    assert_eq!(client.get(&get).send().await?.bytes().await?.as_ref(), body.as_slice());

    // The signature covers the method and key: it cannot be reused for another key or method,
    // and altering it is refused.
    let other_key = get.replacen(&key, &format!("{key}-other"), 1);
    assert!(!client.get(&other_key).send().await?.status().is_success());
    assert!(!client.delete(&get).send().await?.status().is_success());
    let tampered = get.replacen("X-Amz-Signature=", "X-Amz-Signature=0", 1);
    assert!(!client.get(&tampered).send().await?.status().is_success());
    assert_eq!(store.get_object(&key, 1024).await?, body);
    store.delete_object(&key).await?;
    Ok(())
}
