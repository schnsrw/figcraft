use std::time::Duration;

use figcraft_cloud_domain::Uuid;
use figcraft_cloud_storage::bucket_admin::{
    BucketInitError, BucketOutcome, LifecycleOutcome, LifecyclePolicy, LifecycleState, RuleFilter, STAGING_TAG_KEY, STAGING_TAG_VALUE,
    VersioningOutcome, VersioningStatus, initialize_bucket, lifecycle_state, staging_tagging_header, versioning_status,
};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreError, ObjectStoreSettings};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

type Probe = Result<(), Box<dyn std::error::Error>>;

/// `FIGCRAFT_TEST_S3_DIR` holds `s3_endpoint`, `s3_access_key` and `s3_secret_key` for a disposable
/// S3-compatible store (path style, `us-east-1`). Every run uses fresh `figcraft-init-<random>`
/// buckets and deletes them, so reruns never see an earlier run's state.
fn settings(bucket: &str) -> Result<Option<ObjectStoreSettings>, Box<dyn std::error::Error>> {
    let Ok(directory) = std::env::var("FIGCRAFT_TEST_S3_DIR") else {
        eprintln!("FIGCRAFT_TEST_S3_DIR is unset; bucket initialization probe skipped");
        return Ok(None);
    };
    let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::fs::read_to_string(std::path::Path::new(&directory).join(name))?.trim_end().to_string())
    };
    Ok(Some(ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: bucket.into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    }))
}

fn fresh_bucket() -> String {
    format!("figcraft-init-{}", &Uuid::new_v4().simple().to_string()[..16])
}

/// Raw signed access for fixture setup and cleanup that the shipped API deliberately lacks.
struct Raw {
    bucket: Bucket,
    credentials: Credentials,
    client: reqwest::Client,
}

impl Raw {
    fn new(settings: &ObjectStoreSettings) -> Result<Self, Box<dyn std::error::Error>> {
        let bucket = Bucket::new(settings.endpoint.parse()?, UrlStyle::Path, settings.bucket.clone(), settings.region.clone())?;
        // `reqwest::Client::new()` panics under `rustls-no-provider`.
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let client = reqwest::Client::builder().tls_backend_preconfigured(tls).build()?;
        Ok(Self { bucket, credentials: Credentials::new(settings.access_key.clone(), settings.secret_key.clone()), client })
    }

    async fn put_lifecycle(&self, body: &str) -> Result<u16, Box<dyn std::error::Error>> {
        let mut action = self.bucket.create_bucket(&self.credentials);
        action.query_mut().insert("lifecycle", "");
        let url = action.sign(TTL);
        Ok(self.client.put(url.as_str()).body(body.to_string()).send().await?.status().as_u16())
    }

    async fn delete_lifecycle(&self) -> Result<u16, Box<dyn std::error::Error>> {
        let mut action = self.bucket.delete_bucket(&self.credentials);
        action.query_mut().insert("lifecycle", "");
        Ok(self.client.delete(action.sign(TTL).as_str()).send().await?.status().as_u16())
    }

    async fn delete_version(&self, key: &str, version: &str) -> Result<u16, Box<dyn std::error::Error>> {
        let mut action = self.bucket.delete_object(Some(&self.credentials), key);
        action.query_mut().insert("versionId", version.to_string());
        Ok(self.client.delete(action.sign(TTL).as_str()).send().await?.status().as_u16())
    }

    /// Delete the (empty) bucket. Missing buckets are fine.
    async fn delete_bucket(&self) -> Result<(), Box<dyn std::error::Error>> {
        let url = self.bucket.delete_bucket(&self.credentials).sign(TTL);
        let status = self.client.delete(url.as_str()).send().await?.status().as_u16();
        assert!(matches!(status, 204 | 404), "bucket delete answered {status}");
        Ok(())
    }
}

const TTL: Duration = Duration::from_secs(60);

#[tokio::test]
async fn initializes_a_fresh_bucket_once_and_reruns_as_a_no_op() -> Probe {
    let Some(settings) = settings(&fresh_bucket())? else {
        return Ok(());
    };
    let raw = Raw::new(&settings)?;
    let result = fresh_bucket_probe(&settings, &raw).await;
    raw.delete_lifecycle().await?;
    raw.delete_bucket().await?;
    assert_eq!(ObjectStore::new(&settings)?.check_connectivity().await, Err(ObjectStoreError::BucketMissing));
    result
}

async fn fresh_bucket_probe(settings: &ObjectStoreSettings, raw: &Raw) -> Probe {
    let store = ObjectStore::new(settings)?;
    let policy = LifecyclePolicy::default();
    assert_eq!(store.check_connectivity().await, Err(ObjectStoreError::BucketMissing));
    assert_eq!(versioning_status(&store).await, Err(BucketInitError::BucketMissing));

    let first = initialize_bucket(&store, &policy).await?;
    assert_eq!(first.bucket, BucketOutcome::Created);
    assert_eq!(first.versioning, VersioningOutcome::Enabled { previous: VersioningStatus::Unversioned });
    assert_eq!(first.lifecycle, LifecycleOutcome::Applied);
    assert!(first.changed());
    store.check_connectivity().await?;
    assert_eq!(versioning_status(&store).await?, VersioningStatus::Enabled);
    let LifecycleState::Rules(rules) = lifecycle_state(&store).await? else {
        return Err("lifecycle configuration should read back".into());
    };
    assert_eq!(rules, policy.rules());
    assert!(rules.iter().any(
        |rule| rule.filter == RuleFilter::Tag { key: STAGING_TAG_KEY.into(), value: STAGING_TAG_VALUE.into() } && rule.expiration_days == Some(2)
    ));
    assert!(rules.iter().any(|rule| rule.abort_incomplete_multipart_days == Some(2)));
    assert!(rules.iter().any(|rule| rule.noncurrent_expiration_days == Some(30)));

    for _ in 0..2 {
        let rerun = initialize_bucket(&store, &policy).await?;
        assert_eq!(rerun.bucket, BucketOutcome::AlreadyExisted);
        assert_eq!(rerun.versioning, VersioningOutcome::AlreadyEnabled);
        assert_eq!(rerun.lifecycle, LifecycleOutcome::AlreadyCorrect);
        assert!(!rerun.changed());
    }

    // A changed policy replaces FigCraft's own rules; restoring it replaces them again.
    let longer = LifecyclePolicy { staging_expiration_days: 3, ..policy };
    assert_eq!(initialize_bucket(&store, &longer).await?.lifecycle, LifecycleOutcome::Replaced);
    assert_eq!(lifecycle_state(&store).await?, LifecycleState::Rules(longer.rules()));
    assert_eq!(initialize_bucket(&store, &policy).await?.lifecycle, LifecycleOutcome::Replaced);

    // Rules FigCraft did not write are never replaced silently.
    let foreign = concat!(
        r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        "<Rule><ID>ops-logs</ID><Filter><Prefix>logs/</Prefix></Filter><Status>Enabled</Status>",
        "<Expiration><Days>7</Days></Expiration></Rule></LifecycleConfiguration>"
    );
    assert_eq!(raw.put_lifecycle(foreign).await?, 200);
    assert_eq!(initialize_bucket(&store, &policy).await, Err(BucketInitError::ForeignLifecycleRules));
    let LifecycleState::Rules(kept) = lifecycle_state(&store).await? else {
        return Err("foreign lifecycle configuration should remain".into());
    };
    assert_eq!(kept.iter().map(|rule| rule.id.as_str()).collect::<Vec<_>>(), ["ops-logs"]);
    assert!(matches!(raw.delete_lifecycle().await?, 200 | 204));
    assert_eq!(lifecycle_state(&store).await?, LifecycleState::Absent);
    assert_eq!(initialize_bucket(&store, &policy).await?.lifecycle, LifecycleOutcome::Applied);

    staging_tag_probe(&store, raw).await
}

/// The staging lifecycle rule filters on a tag, so a presigned staging `PUT` must carry it: signing
/// `x-amz-tagging` makes the store refuse the upload without exactly that header.
async fn staging_tag_probe(store: &ObjectStore, raw: &Raw) -> Probe {
    let key = format!("tenants/a0000000-0000-4000-8000-000000000014/staging/{}", Uuid::new_v4());
    let (header, value) = staging_tagging_header();
    let mut put = raw.bucket.put_object(Some(&raw.credentials), &key);
    put.headers_mut().insert(header, value);
    let url = put.sign(TTL);

    assert!(!raw.client.put(url.as_str()).body("untagged").send().await?.status().is_success());
    assert!(!raw.client.put(url.as_str()).header(header, "figcraft-staging=false").body("retagged").send().await?.status().is_success());
    let response = raw.client.put(url.as_str()).header(header, value).body("tagged").send().await?;
    assert!(response.status().is_success());
    let version = response.headers().get("x-amz-version-id").and_then(|version| version.to_str().ok()).map(str::to_string);

    let mut tagging = raw.bucket.get_object(Some(&raw.credentials), &key);
    tagging.query_mut().insert("tagging", "");
    let tags = raw.client.get(tagging.sign(TTL).as_str()).send().await?.text().await?;
    assert!(tags.contains(&format!("<Key>{STAGING_TAG_KEY}</Key><Value>{STAGING_TAG_VALUE}</Value>")), "{tags}");
    assert_eq!(store.get_object(&key, 64).await?, b"tagged");

    // Remove the version itself (a plain delete would leave it behind a delete marker).
    let version = version.ok_or("a versioned bucket should return a version id")?;
    assert!(matches!(raw.delete_version(&key, &version).await?, 200 | 204));
    assert_eq!(store.get_object(&key, 64).await, Err(ObjectStoreError::NotFound));
    Ok(())
}

#[tokio::test]
async fn concurrent_initializations_both_succeed() -> Probe {
    let Some(settings) = settings(&fresh_bucket())? else {
        return Ok(());
    };
    let raw = Raw::new(&settings)?;
    let (left, right) = (ObjectStore::new(&settings)?, ObjectStore::new(&settings)?);
    let policy = LifecyclePolicy::default();
    let (first, second) = tokio::join!(initialize_bucket(&left, &policy), initialize_bucket(&right, &policy));
    let outcome = (|| -> Probe {
        let (first, second) = (first?, second?);
        let created = [first.bucket, second.bucket].iter().filter(|outcome| **outcome == BucketOutcome::Created).count();
        assert!(created <= 1, "{first:?} {second:?}");
        for report in [first, second] {
            assert_ne!(report.lifecycle, LifecycleOutcome::Unsupported);
        }
        Ok(())
    })();
    let settled = async {
        assert_eq!(versioning_status(&left).await?, VersioningStatus::Enabled);
        assert_eq!(lifecycle_state(&left).await?, LifecycleState::Rules(policy.rules()));
        assert!(!initialize_bucket(&left, &policy).await?.changed());
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    raw.delete_lifecycle().await?;
    raw.delete_bucket().await?;
    outcome?;
    settled
}

#[tokio::test]
async fn failures_are_fixed_categories_and_change_nothing() -> Probe {
    let bucket = fresh_bucket();
    let Some(settings) = settings(&bucket)? else {
        return Ok(());
    };
    let policy = LifecyclePolicy::default();
    let wrong_secret = ObjectStoreSettings { secret_key: format!("{}x", settings.secret_key), ..settings.clone() };
    assert_eq!(initialize_bucket(&ObjectStore::new(&wrong_secret)?, &policy).await, Err(BucketInitError::Unauthorized));
    let closed_port = ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings.clone() };
    assert_eq!(initialize_bucket(&ObjectStore::new(&closed_port)?, &policy).await, Err(BucketInitError::Unreachable));
    let invalid = LifecyclePolicy { noncurrent_version_expiration_days: 0, ..policy };
    assert_eq!(initialize_bucket(&ObjectStore::new(&settings)?, &invalid).await, Err(BucketInitError::InvalidPolicy));
    // None of the refused runs created the bucket.
    assert_eq!(ObjectStore::new(&settings)?.check_connectivity().await, Err(ObjectStoreError::BucketMissing));
    Ok(())
}
