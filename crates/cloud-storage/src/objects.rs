//! S3-compatible object storage client.
//!
//! Requests are signed with SigV4 by `rusty-s3`, which only builds presigned URLs and performs no
//! I/O, and sent with a `reqwest` client over Rustls with the `ring` provider and the Mozilla root
//! store compiled in (no OpenSSL, no OS certificate store, no `aws-lc-rs`). Redirects and proxies
//! are disabled so a signed request only ever reaches the configured endpoint, and every request
//! has connect and total timeouts. See `docs/saas/object-store-client-decision.md`.
//!
//! Credentials stay in this module: they are never formatted, and errors are fixed categories that
//! cannot echo an endpoint response, a URL or a signature.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

/// Connection timeout for one request to the object store.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Total timeout for one small control request (connectivity checks, metadata).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a signature this client sends for itself stays valid.
const SELF_SIGNED_TTL: Duration = Duration::from_secs(60);
const MAX_SETTING_BYTES: usize = 2048;
/// Longest object key this client signs (S3 allows 1024 bytes).
pub const MAX_KEY_BYTES: usize = 1024;
/// Longest validity of a presigned request handed to a client.
pub const MAX_PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);
/// Largest body this client uploads or downloads in one request; larger objects need multipart
/// transfer, which is not implemented.
pub const MAX_SINGLE_TRANSFER_BYTES: u64 = 256 * 1024 * 1024;

/// Fixed, non-leaking categories for object-store failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectStoreError {
    /// The endpoint, region, bucket or credentials are unusable.
    InvalidConfig,
    /// The HTTP client could not be built.
    Client,
    /// The endpoint could not be reached in time.
    Unreachable,
    /// The endpoint refused the credentials or signature.
    Unauthorized,
    /// The configured bucket does not exist.
    BucketMissing,
    /// The object key is not one this client will sign.
    InvalidKey,
    /// The object does not exist.
    NotFound,
    /// The object is larger than the caller's bound.
    TooLarge,
    /// Any other response.
    Unexpected,
}

impl fmt::Display for ObjectStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "object store error: {self:?}")
    }
}

impl std::error::Error for ObjectStoreError {}

/// Object-store settings, from the cloud configuration (`S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET`,
/// `S3_ACCESS_KEY`, `S3_SECRET_KEY`, `S3_FORCE_PATH_STYLE`).
#[derive(Clone, Eq, PartialEq)]
pub struct ObjectStoreSettings {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Path-style URLs (`endpoint/bucket/key`), required by SeaweedFS and most self-hosted stores.
    pub path_style: bool,
}

impl fmt::Debug for ObjectStoreSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObjectStoreSettings")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("credentials", &"[REDACTED]")
            .field("path_style", &self.path_style)
            .finish()
    }
}

/// A configured S3-compatible bucket. Cloning shares one connection pool.
#[derive(Clone)]
pub struct ObjectStore {
    pub(crate) client: reqwest::Client,
    pub(crate) bucket: Arc<Bucket>,
    pub(crate) credentials: Arc<Credentials>,
}

impl fmt::Debug for ObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("ObjectStore").field("bucket", &self.bucket.name()).finish_non_exhaustive()
    }
}

impl ObjectStore {
    /// Validate `settings` and build the client. Nothing is sent.
    pub fn new(settings: &ObjectStoreSettings) -> Result<Self, ObjectStoreError> {
        let endpoint = parse_endpoint(&settings.endpoint)?;
        if !valid_region(&settings.region)
            || !valid_bucket(&settings.bucket)
            || !valid_credential(&settings.access_key)
            || !valid_credential(&settings.secret_key)
        {
            return Err(ObjectStoreError::InvalidConfig);
        }
        let style = if settings.path_style { UrlStyle::Path } else { UrlStyle::VirtualHost };
        let bucket = Bucket::new(endpoint, style, settings.bucket.clone(), settings.region.clone()).map_err(|_| ObjectStoreError::InvalidConfig)?;
        let credentials = Credentials::new(settings.access_key.clone(), settings.secret_key.clone());
        Ok(Self { client: http_client()?, bucket: Arc::new(bucket), credentials: Arc::new(credentials) })
    }

    #[must_use]
    pub fn bucket_name(&self) -> &str {
        self.bucket.name()
    }

    /// Prove the endpoint is reachable, accepts these credentials and has the bucket, with one
    /// signed `HeadBucket` request.
    pub async fn check_connectivity(&self) -> Result<(), ObjectStoreError> {
        let url = self.bucket.head_bucket(Some(&self.credentials)).sign(SELF_SIGNED_TTL);
        let response = self.client.head(url.as_str()).timeout(REQUEST_TIMEOUT).send().await.map_err(|_| ObjectStoreError::Unreachable)?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            401 | 403 => Err(ObjectStoreError::Unauthorized),
            404 => Err(ObjectStoreError::BucketMissing),
            _ => Err(ObjectStoreError::Unexpected),
        }
    }
}

impl ObjectStore {
    /// Upload `body` to `key`, replacing nothing the caller has not already decided to replace:
    /// immutability is enforced by the object metadata repository, not by the store.
    pub async fn put_object(&self, key: &str, body: Vec<u8>, media_type: &str) -> Result<(), ObjectStoreError> {
        let url = self.signed_object_url(key, |bucket, credentials| bucket.put_object(Some(credentials), key).sign(SELF_SIGNED_TTL))?;
        if u64::try_from(body.len()).map_or(true, |length| length > MAX_SINGLE_TRANSFER_BYTES) {
            return Err(ObjectStoreError::TooLarge);
        }
        if media_type.is_empty() || media_type.len() > 255 || !media_type.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ObjectStoreError::InvalidConfig);
        }
        let response = self
            .client
            .put(url.as_str())
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .timeout(transfer_timeout(body.len()))
            .body(body)
            .send()
            .await
            .map_err(|_| ObjectStoreError::Unreachable)?;
        status_result(response.status().as_u16())
    }

    /// Download `key`, refusing an object larger than `max_bytes` before and while reading it.
    pub async fn get_object(&self, key: &str, max_bytes: u64) -> Result<Vec<u8>, ObjectStoreError> {
        let url = self.signed_object_url(key, |bucket, credentials| bucket.get_object(Some(credentials), key).sign(SELF_SIGNED_TTL))?;
        let max_bytes = max_bytes.min(MAX_SINGLE_TRANSFER_BYTES);
        let mut response = self
            .client
            .get(url.as_str())
            .timeout(transfer_timeout(usize::try_from(max_bytes).unwrap_or(usize::MAX)))
            .send()
            .await
            .map_err(|_| ObjectStoreError::Unreachable)?;
        status_result(response.status().as_u16())?;
        if response.content_length().is_some_and(|length| length > max_bytes) {
            return Err(ObjectStoreError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ObjectStoreError::Unreachable)? {
            let total = u64::try_from(body.len().saturating_add(chunk.len())).unwrap_or(u64::MAX);
            if total > max_bytes {
                return Err(ObjectStoreError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// A presigned `PUT` for exactly `key`, valid for `ttl` (1 second to [`MAX_PRESIGN_TTL`]).
    /// Callers must have reserved the key and authorized the uploader first.
    pub fn presign_put(&self, key: &str, ttl: Duration) -> Result<String, ObjectStoreError> {
        let ttl = presign_ttl(ttl)?;
        Ok(self.signed_object_url(key, |bucket, credentials| bucket.put_object(Some(credentials), key).sign(ttl))?.into())
    }

    /// A presigned `GET` for exactly `key`, valid for `ttl` (1 second to [`MAX_PRESIGN_TTL`]).
    /// Callers must have authorized the download first.
    pub fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, ObjectStoreError> {
        let ttl = presign_ttl(ttl)?;
        Ok(self.signed_object_url(key, |bucket, credentials| bucket.get_object(Some(credentials), key).sign(ttl))?.into())
    }

    /// Delete `key`. Deleting a missing key succeeds, as S3 does.
    pub async fn delete_object(&self, key: &str) -> Result<(), ObjectStoreError> {
        let url = self.signed_object_url(key, |bucket, credentials| bucket.delete_object(Some(credentials), key).sign(SELF_SIGNED_TTL))?;
        let response = self.client.delete(url.as_str()).timeout(REQUEST_TIMEOUT).send().await.map_err(|_| ObjectStoreError::Unreachable)?;
        status_result(response.status().as_u16())
    }

    /// Validate `key` and sign. `rusty-s3` unwraps the object URL internally, so the URL is built
    /// fallibly first from the same key; with a valid key the internal unwrap cannot fail.
    fn signed_object_url(&self, key: &str, sign: impl FnOnce(&Bucket, &Credentials) -> url::Url) -> Result<url::Url, ObjectStoreError> {
        if !valid_key(key) || self.bucket.object_url(key).is_err() {
            return Err(ObjectStoreError::InvalidKey);
        }
        Ok(sign(&self.bucket, &self.credentials))
    }
}

/// Object keys FigCraft writes: 1–1024 bytes of lowercase letters, digits, `/`, `.`, `-` and `_`,
/// with no empty, `.` or `..` segment and no leading or trailing `/`.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    (1..=MAX_KEY_BYTES).contains(&key.len())
        && key.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'/' | b'.' | b'-' | b'_'))
        && key.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn presign_ttl(ttl: Duration) -> Result<Duration, ObjectStoreError> {
    if ttl < Duration::from_secs(1) || ttl > MAX_PRESIGN_TTL {
        return Err(ObjectStoreError::InvalidConfig);
    }
    Ok(Duration::from_secs(ttl.as_secs()))
}

fn status_result(status: u16) -> Result<(), ObjectStoreError> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => Err(ObjectStoreError::Unauthorized),
        404 => Err(ObjectStoreError::NotFound),
        _ => Err(ObjectStoreError::Unexpected),
    }
}

/// The request timeout plus one second per MiB transferred, so large bodies on slow links finish.
fn transfer_timeout(bytes: usize) -> Duration {
    REQUEST_TIMEOUT.saturating_add(Duration::from_secs(u64::try_from(bytes / (1024 * 1024)).unwrap_or(u64::MAX)))
}

fn http_client() -> Result<reqwest::Client, ObjectStoreError> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| ObjectStoreError::Client)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| ObjectStoreError::Client)
}

/// An `http(s)` endpoint with a host, no credentials, query or fragment, and at most a `/` path.
fn parse_endpoint(endpoint: &str) -> Result<url::Url, ObjectStoreError> {
    if endpoint.len() > MAX_SETTING_BYTES {
        return Err(ObjectStoreError::InvalidConfig);
    }
    let url = url::Url::parse(endpoint).map_err(|_| ObjectStoreError::InvalidConfig)?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/";
    if plain { Ok(url) } else { Err(ObjectStoreError::InvalidConfig) }
}

fn valid_region(region: &str) -> bool {
    (1..=64).contains(&region.len()) && region.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// S3 bucket naming: 3–63 lowercase letters, digits, dots and hyphens, starting and ending with a
/// letter or digit.
fn valid_bucket(bucket: &str) -> bool {
    let bytes = bucket.as_bytes();
    let edge = |byte: Option<&u8>| byte.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    (3..=63).contains(&bytes.len())
        && edge(bytes.first())
        && edge(bytes.last())
        && bytes.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'.' || *byte == b'-')
        && !bucket.contains("..")
}

fn valid_credential(value: &str) -> bool {
    (1..=MAX_SETTING_BYTES).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ObjectStoreSettings {
        ObjectStoreSettings {
            endpoint: "http://seaweedfs:8333".into(),
            region: "us-east-1".into(),
            bucket: "figcraft".into(),
            access_key: "FCaccess".into(),
            secret_key: "secret-value-123".into(),
            path_style: true,
        }
    }

    #[test]
    fn builds_from_valid_settings_without_sending_anything() {
        let store = ObjectStore::new(&settings()).unwrap();
        assert_eq!(store.bucket_name(), "figcraft");
        let virtual_host =
            ObjectStore::new(&ObjectStoreSettings { endpoint: "https://s3.example.com/".into(), path_style: false, ..settings() }).unwrap();
        assert_eq!(virtual_host.bucket.base_url().as_str(), "https://figcraft.s3.example.com/");
        assert_eq!(store.bucket.base_url().as_str(), "http://seaweedfs:8333/figcraft/");
    }

    #[test]
    fn rejects_unsafe_or_malformed_endpoints() {
        for endpoint in [
            "",
            "seaweedfs:8333",
            "ftp://seaweedfs",
            "http://user:pass@seaweedfs:8333",
            "http://key@seaweedfs:8333",
            "http://seaweedfs:8333/prefix",
            "http://seaweedfs:8333/?x=1",
            "http://seaweedfs:8333/#f",
            "http://",
            &format!("http://{}", "a".repeat(MAX_SETTING_BYTES)),
        ] {
            assert_eq!(
                ObjectStore::new(&ObjectStoreSettings { endpoint: endpoint.into(), ..settings() }).map(|_| ()),
                Err(ObjectStoreError::InvalidConfig),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn rejects_invalid_buckets_regions_and_credentials() {
        for bucket in ["", "ab", "Figcraft", "-figcraft", "figcraft-", "fig..craft", "fig_craft", &"a".repeat(64)] {
            assert!(ObjectStore::new(&ObjectStoreSettings { bucket: bucket.into(), ..settings() }).is_err(), "{bucket}");
        }
        for region in ["", "US-EAST-1", "us east", &"a".repeat(65)] {
            assert!(ObjectStore::new(&ObjectStoreSettings { region: region.into(), ..settings() }).is_err(), "{region}");
        }
        for secret in ["", "with space", "new\nline", "é"] {
            assert!(ObjectStore::new(&ObjectStoreSettings { secret_key: secret.into(), ..settings() }).is_err());
            assert!(ObjectStore::new(&ObjectStoreSettings { access_key: secret.into(), ..settings() }).is_err());
        }
    }

    #[test]
    fn never_formats_credentials() {
        let settings = settings();
        let shown = format!("{settings:?} {:?}", ObjectStore::new(&settings).unwrap());
        assert!(!shown.contains("secret-value-123") && !shown.contains("FCaccess"), "{shown}");
    }

    #[test]
    fn signs_only_well_formed_keys() {
        for key in ["tenants/a/assets/ab12", "tenants/1-2/snapshots/3-ab.bin", "a", "a_b.c-d"] {
            assert!(valid_key(key), "{key}");
        }
        for key in ["", "/a", "a/", "a//b", "a/./b", "a/../b", "..", "A", "a b", "a?b", "a#b", "a%2f", "é", &"a".repeat(MAX_KEY_BYTES + 1)] {
            assert!(!valid_key(key), "{key}");
        }
    }

    #[tokio::test]
    async fn refuses_bad_keys_and_bodies_before_sending() {
        let store = ObjectStore::new(&ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings() }).unwrap();
        assert_eq!(store.get_object("../x", 1).await, Err(ObjectStoreError::InvalidKey));
        assert_eq!(store.put_object("a/../b", vec![1], "image/png").await, Err(ObjectStoreError::InvalidKey));
        assert_eq!(store.delete_object("a//b").await, Err(ObjectStoreError::InvalidKey));
        assert_eq!(store.put_object("a/b", vec![1], "").await, Err(ObjectStoreError::InvalidConfig));
        assert_eq!(store.put_object("a/b", vec![1], "image/png").await, Err(ObjectStoreError::Unreachable));
    }

    #[test]
    fn presigned_requests_name_one_key_and_a_bounded_expiry() {
        let store = ObjectStore::new(&settings()).unwrap();
        let get = store.presign_get("tenants/a/assets/ab", Duration::from_secs(300)).unwrap();
        assert!(get.starts_with("http://seaweedfs:8333/figcraft/tenants/a/assets/ab?"), "{get}");
        assert!(get.contains("X-Amz-Expires=300") && get.contains("X-Amz-Signature="), "{get}");
        assert!(!get.contains("secret-value-123"));
        assert!(store.presign_put("tenants/a/staging/x", MAX_PRESIGN_TTL).unwrap().contains("X-Amz-Expires=900"));
        assert_eq!(store.presign_get("a/../b", Duration::from_secs(60)), Err(ObjectStoreError::InvalidKey));
        assert_eq!(store.presign_put("a/b", MAX_PRESIGN_TTL + Duration::from_secs(1)), Err(ObjectStoreError::InvalidConfig));
        assert_eq!(store.presign_get("a/b", Duration::from_millis(500)), Err(ObjectStoreError::InvalidConfig));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_a_fixed_category() {
        let store = ObjectStore::new(&ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings() }).unwrap();
        assert_eq!(store.check_connectivity().await, Err(ObjectStoreError::Unreachable));
    }
}
