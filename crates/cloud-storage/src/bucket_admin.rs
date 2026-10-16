//! Idempotent bucket initialization: existence, versioning and lifecycle rules.
//!
//! `rusty-s3` has no actions for bucket versioning or lifecycle, so this module signs those
//! sub-resource requests itself with `rusty_s3::signing::sign` over the store's validated base URL
//! and sends them with the store's own client. Request bodies are fixed templates filled only with
//! validated numbers; response bodies are read up to [`MAX_RESPONSE_BYTES`] and parsed with
//! `roxmltree` under a node limit, without DTDs.
//!
//! [`initialize_bucket`] is safe to run repeatedly and concurrently: each step reads the current
//! state first, changes only what differs, and reads the result back. It never deletes anything
//! and refuses to replace lifecycle rules FigCraft did not write.
//!
//! Staging expiry: staging keys are `tenants/<tenant>/staging/<reservation>` (fixed by the S3-002
//! constraint) and S3 lifecycle prefix filters cannot wildcard the tenant segment, so the staging
//! rule filters on the object tag [`STAGING_TAG_KEY`]` = `[`STAGING_TAG_VALUE`]. Staging uploads
//! carry it by signing the [`staging_tagging_header`] into their presigned `PUT`, so an uploader
//! cannot drop it. The application's reservation expiry sweep stays authoritative; the rule is a
//! backstop for objects the sweep never sees. On a versioned bucket expiry adds a delete marker,
//! and the noncurrent-version rule then removes the bytes.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use md5::{Digest as _, Md5};
use rusty_s3::Method;

use crate::objects::{ObjectStore, REQUEST_TIMEOUT};

/// Largest response body read from a bucket administration request.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// Most XML nodes parsed from one response.
const MAX_XML_NODES: u32 = 8192;
/// Most lifecycle rules read from one configuration (the S3 limit).
pub const MAX_LIFECYCLE_RULES: usize = 1000;
/// Longest lifecycle period this module writes, in days.
pub const MAX_LIFECYCLE_DAYS: u32 = 36_500;
/// How long a signature this module sends for itself stays valid.
const SELF_SIGNED_TTL: Duration = Duration::from_secs(60);
const S3_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

/// The object tag that marks a staging upload for lifecycle expiry.
pub const STAGING_TAG_KEY: &str = "figcraft-staging";
pub const STAGING_TAG_VALUE: &str = "true";
/// Lifecycle rules whose ID starts with this prefix belong to FigCraft and may be replaced.
pub const RULE_ID_PREFIX: &str = "figcraft-";
pub const STAGING_RULE_ID: &str = "figcraft-staging-expiry";
pub const MULTIPART_RULE_ID: &str = "figcraft-abort-incomplete-multipart";
pub const NONCURRENT_RULE_ID: &str = "figcraft-noncurrent-expiry";

/// The `x-amz-tagging` header (name, value) a presigned staging `PUT` must sign so the upload
/// carries [`STAGING_TAG_KEY`].
#[must_use]
pub const fn staging_tagging_header() -> (&'static str, &'static str) {
    ("x-amz-tagging", "figcraft-staging=true")
}

/// Fixed, non-leaking categories for bucket initialization failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BucketInitError {
    /// The lifecycle policy has a period outside 1..=[`MAX_LIFECYCLE_DAYS`].
    InvalidPolicy,
    /// The endpoint could not be reached in time.
    Unreachable,
    /// The endpoint refused the credentials, signature or permission.
    Unauthorized,
    /// The bucket name is taken by another account.
    BucketOwnedElsewhere,
    /// The bucket disappeared while it was being configured.
    BucketMissing,
    /// Versioning did not read back as enabled after it was enabled.
    VersioningNotEnabled,
    /// The bucket has lifecycle rules FigCraft did not write; they are never replaced silently.
    ForeignLifecycleRules,
    /// The store refused the lifecycle configuration.
    LifecycleRejected,
    /// The lifecycle configuration read back differs from the one written.
    LifecycleNotApplied,
    /// A response was too large or not the expected XML.
    MalformedResponse,
    /// Any other response.
    Unexpected,
}

impl fmt::Display for BucketInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "bucket initialization error: {self:?}")
    }
}

impl std::error::Error for BucketInitError {}

/// Lifecycle periods, in days.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecyclePolicy {
    /// Expire current versions of objects tagged [`STAGING_TAG_KEY`].
    pub staging_expiration_days: u32,
    /// Abort multipart uploads not completed within this period.
    pub abort_incomplete_multipart_days: u32,
    /// Remove versions this long after they became noncurrent (overwritten or deleted).
    pub noncurrent_version_expiration_days: u32,
}

impl Default for LifecyclePolicy {
    /// Staging reservations live at most 15 minutes, so two days leaves the expiry sweep ample
    /// time; 30 noncurrent days keeps deleted bytes restorable for a month of backups.
    fn default() -> Self {
        Self { staging_expiration_days: 2, abort_incomplete_multipart_days: 2, noncurrent_version_expiration_days: 30 }
    }
}

impl LifecyclePolicy {
    fn validate(&self) -> Result<(), BucketInitError> {
        let valid = |days: u32| (1..=MAX_LIFECYCLE_DAYS).contains(&days);
        if valid(self.staging_expiration_days) && valid(self.abort_incomplete_multipart_days) && valid(self.noncurrent_version_expiration_days) {
            Ok(())
        } else {
            Err(BucketInitError::InvalidPolicy)
        }
    }

    /// The rules this policy writes, sorted by ID.
    #[must_use]
    pub fn rules(&self) -> Vec<LifecycleRule> {
        let rule = |id: &str, filter| LifecycleRule {
            id: id.to_string(),
            enabled: true,
            filter,
            expiration_days: None,
            noncurrent_expiration_days: None,
            abort_incomplete_multipart_days: None,
            other_settings: false,
        };
        let mut rules = vec![
            LifecycleRule {
                abort_incomplete_multipart_days: Some(self.abort_incomplete_multipart_days),
                ..rule(MULTIPART_RULE_ID, RuleFilter::Prefix(String::new()))
            },
            LifecycleRule {
                noncurrent_expiration_days: Some(self.noncurrent_version_expiration_days),
                ..rule(NONCURRENT_RULE_ID, RuleFilter::Prefix(String::new()))
            },
            LifecycleRule {
                expiration_days: Some(self.staging_expiration_days),
                ..rule(STAGING_RULE_ID, RuleFilter::Tag { key: STAGING_TAG_KEY.to_string(), value: STAGING_TAG_VALUE.to_string() })
            },
        ];
        rules.sort_by(|left, right| left.id.cmp(&right.id));
        rules
    }

    /// The `PutBucketLifecycleConfiguration` body. Only validated numbers are interpolated.
    fn to_xml(self) -> String {
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<LifecycleConfiguration xmlns="{ns}">"#,
                "<Rule><ID>{multipart_id}</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status>",
                "<AbortIncompleteMultipartUpload><DaysAfterInitiation>{multipart}</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule>",
                "<Rule><ID>{noncurrent_id}</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status>",
                "<NoncurrentVersionExpiration><NoncurrentDays>{noncurrent}</NoncurrentDays></NoncurrentVersionExpiration></Rule>",
                "<Rule><ID>{staging_id}</ID><Filter><Tag><Key>{tag_key}</Key><Value>{tag_value}</Value></Tag></Filter>",
                "<Status>Enabled</Status><Expiration><Days>{staging}</Days></Expiration></Rule>",
                "</LifecycleConfiguration>"
            ),
            ns = S3_NAMESPACE,
            multipart_id = MULTIPART_RULE_ID,
            multipart = self.abort_incomplete_multipart_days,
            noncurrent_id = NONCURRENT_RULE_ID,
            noncurrent = self.noncurrent_version_expiration_days,
            staging_id = STAGING_RULE_ID,
            tag_key = STAGING_TAG_KEY,
            tag_value = STAGING_TAG_VALUE,
            staging = self.staging_expiration_days,
        )
    }
}

/// Which objects a lifecycle rule applies to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuleFilter {
    /// Keys starting with this prefix (empty: every key).
    Prefix(String),
    /// Objects carrying exactly this tag.
    Tag { key: String, value: String },
    /// Any other filter (`And`, object size, …); never written by FigCraft.
    Other,
}

/// The parts of one lifecycle rule FigCraft writes and compares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleRule {
    pub id: String,
    pub enabled: bool,
    pub filter: RuleFilter,
    pub expiration_days: Option<u32>,
    pub noncurrent_expiration_days: Option<u32>,
    pub abort_incomplete_multipart_days: Option<u32>,
    /// The rule has settings FigCraft never writes (transitions, dates, version counts, …).
    pub other_settings: bool,
}

/// A bucket's versioning state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersioningStatus {
    /// Versioning was never enabled.
    Unversioned,
    Enabled,
    Suspended,
}

/// A bucket's lifecycle configuration as read back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    /// The store does not implement bucket lifecycle configuration.
    Unsupported,
    /// No configuration is set.
    Absent,
    /// The configured rules, sorted by ID.
    Rules(Vec<LifecycleRule>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BucketOutcome {
    Created,
    AlreadyExisted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersioningOutcome {
    /// Versioning was enabled from this previous state and verified.
    Enabled {
        previous: VersioningStatus,
    },
    AlreadyEnabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleOutcome {
    /// The rules were written to a bucket without a lifecycle configuration and verified.
    Applied,
    /// Older or different FigCraft rules were replaced and the result verified.
    Replaced,
    AlreadyCorrect,
    /// The store does not implement bucket lifecycle configuration; the application expiry sweep
    /// is the only staging expiry.
    Unsupported,
}

/// What [`initialize_bucket`] created, changed or found already correct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BucketInitReport {
    pub bucket: BucketOutcome,
    pub versioning: VersioningOutcome,
    pub lifecycle: LifecycleOutcome,
}

impl BucketInitReport {
    /// Whether this run changed anything.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.bucket == BucketOutcome::Created
            || self.versioning != VersioningOutcome::AlreadyEnabled
            || matches!(self.lifecycle, LifecycleOutcome::Applied | LifecycleOutcome::Replaced)
    }
}

/// Create the store's bucket if missing, enable and verify versioning, and apply and verify the
/// lifecycle `policy`. Safe to run repeatedly and concurrently with the same policy.
pub async fn initialize_bucket(store: &ObjectStore, policy: &LifecyclePolicy) -> Result<BucketInitReport, BucketInitError> {
    policy.validate()?;
    let bucket = ensure_bucket(store).await?;
    let versioning = ensure_versioning(store).await?;
    let lifecycle = ensure_lifecycle(store, policy).await?;
    Ok(BucketInitReport { bucket, versioning, lifecycle })
}

/// Read the bucket's versioning state.
pub async fn versioning_status(store: &ObjectStore) -> Result<VersioningStatus, BucketInitError> {
    let response = send(store, Method::Get, Some("versioning"), None).await?;
    match response.status {
        200 => parse_versioning(&response.body),
        _ => Err(failure(&response)),
    }
}

/// Read the bucket's lifecycle configuration.
pub async fn lifecycle_state(store: &ObjectStore) -> Result<LifecycleState, BucketInitError> {
    let response = send(store, Method::Get, Some("lifecycle"), None).await?;
    match response.status {
        200 => parse_lifecycle(&response.body).map(LifecycleState::Rules),
        404 if response.code().as_deref() == Some("NoSuchLifecycleConfiguration") => Ok(LifecycleState::Absent),
        _ if unsupported(&response) => Ok(LifecycleState::Unsupported),
        _ => Err(failure(&response)),
    }
}

async fn ensure_bucket(store: &ObjectStore) -> Result<BucketOutcome, BucketInitError> {
    let head = send(store, Method::Head, None, None).await?;
    match head.status {
        200..=299 => return Ok(BucketOutcome::AlreadyExisted),
        404 => {}
        // A HEAD has no body, so a refused signature and a bucket owned by another account look
        // the same; the credentials are the far likelier cause.
        _ => return Err(failure(&head)),
    }
    // Outside us-east-1, S3 requires the region as a location constraint. Regions are validated
    // as lowercase letters, digits and hyphens when the store is built.
    let region = store.bucket.region();
    let body = (region != "us-east-1").then(|| {
        format!(r#"<CreateBucketConfiguration xmlns="{S3_NAMESPACE}"><LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"#)
    });
    let response = send(store, Method::Put, None, body.map(String::into_bytes)).await?;
    match response.status {
        200..=299 => Ok(BucketOutcome::Created),
        // `BucketAlreadyOwnedByYou` (a concurrent or earlier create), `BucketAlreadyExists` or
        // `OperationAborted`. These credentials were just accepted, so whether the bucket is ours
        // is decided by whether they can now see it.
        409 => match send(store, Method::Head, None, None).await?.status {
            200..=299 => Ok(BucketOutcome::AlreadyExisted),
            403 => Err(BucketInitError::BucketOwnedElsewhere),
            _ => Err(BucketInitError::Unexpected),
        },
        _ => Err(failure(&response)),
    }
}

async fn ensure_versioning(store: &ObjectStore) -> Result<VersioningOutcome, BucketInitError> {
    let previous = versioning_status(store).await?;
    if previous == VersioningStatus::Enabled {
        return Ok(VersioningOutcome::AlreadyEnabled);
    }
    let body = format!(r#"<VersioningConfiguration xmlns="{S3_NAMESPACE}"><Status>Enabled</Status></VersioningConfiguration>"#);
    let response = send(store, Method::Put, Some("versioning"), Some(body.into_bytes())).await?;
    if !(200..=299).contains(&response.status) {
        return Err(failure(&response));
    }
    match versioning_status(store).await? {
        VersioningStatus::Enabled => Ok(VersioningOutcome::Enabled { previous }),
        _ => Err(BucketInitError::VersioningNotEnabled),
    }
}

async fn ensure_lifecycle(store: &ObjectStore, policy: &LifecyclePolicy) -> Result<LifecycleOutcome, BucketInitError> {
    let desired = policy.rules();
    let had_rules = match lifecycle_state(store).await? {
        LifecycleState::Unsupported => return Ok(LifecycleOutcome::Unsupported),
        LifecycleState::Absent => false,
        LifecycleState::Rules(rules) => {
            if rules.iter().any(|rule| !rule.id.starts_with(RULE_ID_PREFIX)) {
                return Err(BucketInitError::ForeignLifecycleRules);
            }
            if rules == desired {
                return Ok(LifecycleOutcome::AlreadyCorrect);
            }
            !rules.is_empty()
        }
    };
    let response = send(store, Method::Put, Some("lifecycle"), Some(policy.to_xml().into_bytes())).await?;
    match response.status {
        200..=299 => {}
        _ if unsupported(&response) => return Ok(LifecycleOutcome::Unsupported),
        400 => return Err(BucketInitError::LifecycleRejected),
        _ => return Err(failure(&response)),
    }
    match lifecycle_state(store).await? {
        LifecycleState::Rules(rules) if rules == desired => Ok(if had_rules { LifecycleOutcome::Replaced } else { LifecycleOutcome::Applied }),
        _ => Err(BucketInitError::LifecycleNotApplied),
    }
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

impl Response {
    /// The S3 error `<Code>`, when the body is a well-formed error document.
    fn code(&self) -> Option<String> {
        error_code(&self.body)
    }
}

fn unsupported(response: &Response) -> bool {
    response.status == 501 || response.code().as_deref() == Some("NotImplemented")
}

fn failure(response: &Response) -> BucketInitError {
    match (response.status, response.code().as_deref()) {
        (401 | 403, _) => BucketInitError::Unauthorized,
        (404, Some("NoSuchBucket")) => BucketInitError::BucketMissing,
        _ => BucketInitError::Unexpected,
    }
}

/// Sign and send one bucket-level request (`subresource` is a query flag such as `versioning`).
/// A body is sent with a signed `Content-MD5`, which S3 requires for lifecycle configuration.
async fn send(store: &ObjectStore, method: Method, subresource: Option<&'static str>, body: Option<Vec<u8>>) -> Result<Response, BucketInitError> {
    let content_md5 = body.as_deref().map(|body| base64::engine::general_purpose::STANDARD.encode(Md5::digest(body)));
    let headers: Vec<(&str, &str)> = content_md5.as_deref().map(|digest| ("content-md5", digest)).into_iter().collect();
    // The base URL was validated as an `http(s)` URL with a host when the store was built, which
    // is everything `sign` requires.
    let url = rusty_s3::signing::sign(
        &jiff::Timestamp::now(),
        method,
        store.bucket.base_url().clone(),
        store.credentials.key(),
        store.credentials.secret(),
        store.credentials.token(),
        store.bucket.region(),
        SELF_SIGNED_TTL.as_secs(),
        subresource.map(|name| (name, "")).into_iter(),
        headers.iter().copied(),
    );
    let http_method = match method {
        Method::Head => reqwest::Method::HEAD,
        Method::Get => reqwest::Method::GET,
        Method::Put => reqwest::Method::PUT,
        Method::Post => reqwest::Method::POST,
        Method::Delete => reqwest::Method::DELETE,
    };
    let mut request = store.client.request(http_method, url.as_str()).timeout(REQUEST_TIMEOUT);
    for (name, value) in &headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request.header(reqwest::header::CONTENT_TYPE, "application/xml").body(body);
    }
    let mut response = request.send().await.map_err(|_| BucketInitError::Unreachable)?;
    let status = response.status().as_u16();
    if response.content_length().is_some_and(|length| length > MAX_RESPONSE_BYTES as u64) {
        return Err(BucketInitError::MalformedResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BucketInitError::Unreachable)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(BucketInitError::MalformedResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response { status, body })
}

fn parse_document(body: &[u8]) -> Result<roxmltree::Document<'_>, BucketInitError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(BucketInitError::MalformedResponse);
    }
    let text = std::str::from_utf8(body).map_err(|_| BucketInitError::MalformedResponse)?;
    let options = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: MAX_XML_NODES, ..roxmltree::ParsingOptions::default() };
    roxmltree::Document::parse_with_options(text, options).map_err(|_| BucketInitError::MalformedResponse)
}

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, name: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|child| child.is_element() && child.tag_name().name() == name)
}

fn text<'a>(node: roxmltree::Node<'a, '_>) -> &'a str {
    node.text().unwrap_or("").trim()
}

/// A day count: 1..=[`MAX_LIFECYCLE_DAYS`] ASCII digits, nothing else.
fn parse_days(node: Option<roxmltree::Node<'_, '_>>) -> Result<u32, BucketInitError> {
    let value = node.map(text).ok_or(BucketInitError::MalformedResponse)?;
    if value.is_empty() || value.len() > 10 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BucketInitError::MalformedResponse);
    }
    value.parse().map_err(|_| BucketInitError::MalformedResponse)
}

/// The S3 error `<Code>` of an error document: 1–64 ASCII letters and digits, or nothing.
fn error_code(body: &[u8]) -> Option<String> {
    let document = parse_document(body).ok()?;
    let root = document.root_element();
    if root.tag_name().name() != "Error" {
        return None;
    }
    let code = text(child(root, "Code")?);
    let valid = (1..=64).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_alphanumeric());
    valid.then(|| code.to_string())
}

fn parse_versioning(body: &[u8]) -> Result<VersioningStatus, BucketInitError> {
    let document = parse_document(body)?;
    let root = document.root_element();
    if root.tag_name().name() != "VersioningConfiguration" {
        return Err(BucketInitError::MalformedResponse);
    }
    match child(root, "Status").map(text) {
        None | Some("") => Ok(VersioningStatus::Unversioned),
        Some("Enabled") => Ok(VersioningStatus::Enabled),
        Some("Suspended") => Ok(VersioningStatus::Suspended),
        Some(_) => Err(BucketInitError::MalformedResponse),
    }
}

/// Parse a `LifecycleConfiguration` into rules sorted by ID.
fn parse_lifecycle(body: &[u8]) -> Result<Vec<LifecycleRule>, BucketInitError> {
    let document = parse_document(body)?;
    let root = document.root_element();
    if root.tag_name().name() != "LifecycleConfiguration" {
        return Err(BucketInitError::MalformedResponse);
    }
    let mut rules = Vec::new();
    for node in root.children().filter(|node| node.is_element() && node.tag_name().name() == "Rule") {
        if rules.len() >= MAX_LIFECYCLE_RULES {
            return Err(BucketInitError::MalformedResponse);
        }
        rules.push(parse_rule(node)?);
    }
    rules.sort_by(|left: &LifecycleRule, right| left.id.cmp(&right.id));
    Ok(rules)
}

fn parse_rule(node: roxmltree::Node<'_, '_>) -> Result<LifecycleRule, BucketInitError> {
    let mut rule = LifecycleRule {
        id: String::new(),
        enabled: false,
        filter: RuleFilter::Prefix(String::new()),
        expiration_days: None,
        noncurrent_expiration_days: None,
        abort_incomplete_multipart_days: None,
        other_settings: false,
    };
    for element in node.children().filter(roxmltree::Node::is_element) {
        match element.tag_name().name() {
            "ID" => rule.id = bounded_text(element)?,
            "Status" => rule.enabled = text(element) == "Enabled",
            // The legacy rule-level prefix.
            "Prefix" => rule.filter = RuleFilter::Prefix(bounded_text(element)?),
            "Filter" => rule.filter = parse_filter(element)?,
            "Expiration" => {
                for setting in element.children().filter(roxmltree::Node::is_element) {
                    match setting.tag_name().name() {
                        "Days" => rule.expiration_days = Some(parse_days(Some(setting))?),
                        _ => rule.other_settings = true,
                    }
                }
            }
            "NoncurrentVersionExpiration" => {
                for setting in element.children().filter(roxmltree::Node::is_element) {
                    match setting.tag_name().name() {
                        "NoncurrentDays" => rule.noncurrent_expiration_days = Some(parse_days(Some(setting))?),
                        _ => rule.other_settings = true,
                    }
                }
            }
            "AbortIncompleteMultipartUpload" => {
                rule.abort_incomplete_multipart_days = Some(parse_days(child(element, "DaysAfterInitiation"))?);
            }
            _ => rule.other_settings = true,
        }
    }
    Ok(rule)
}

fn parse_filter(node: roxmltree::Node<'_, '_>) -> Result<RuleFilter, BucketInitError> {
    let mut elements = node.children().filter(roxmltree::Node::is_element);
    let (Some(only), None) = (elements.next(), elements.next()) else {
        // An empty filter applies to every key; several conditions are not a FigCraft filter.
        return Ok(if node.children().any(|child| child.is_element()) { RuleFilter::Other } else { RuleFilter::Prefix(String::new()) });
    };
    match only.tag_name().name() {
        "Prefix" => Ok(RuleFilter::Prefix(bounded_text(only)?)),
        "Tag" => Ok(RuleFilter::Tag {
            key: child(only, "Key").map(bounded_text).transpose()?.unwrap_or_default(),
            value: child(only, "Value").map(bounded_text).transpose()?.unwrap_or_default(),
        }),
        _ => Ok(RuleFilter::Other),
    }
}

/// Element text of at most 1024 bytes (the longest S3 key or tag), as an owned string.
fn bounded_text(node: roxmltree::Node<'_, '_>) -> Result<String, BucketInitError> {
    let value = text(node);
    if value.len() > 1024 { Err(BucketInitError::MalformedResponse) } else { Ok(value.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_policy_parses_back_to_its_own_rules() {
        let policy = LifecyclePolicy::default();
        assert_eq!(parse_lifecycle(policy.to_xml().as_bytes()).unwrap(), policy.rules());
        let ids: Vec<_> = policy.rules().into_iter().map(|rule| rule.id).collect();
        assert_eq!(ids, [MULTIPART_RULE_ID, NONCURRENT_RULE_ID, STAGING_RULE_ID]);
        let staging = &policy.rules()[2];
        assert_eq!(staging.filter, RuleFilter::Tag { key: STAGING_TAG_KEY.into(), value: STAGING_TAG_VALUE.into() });
        assert_eq!(staging.expiration_days, Some(2));
        assert_eq!(staging_tagging_header().1, format!("{STAGING_TAG_KEY}={STAGING_TAG_VALUE}"));
    }

    #[test]
    fn policy_periods_are_bounded() {
        assert!(LifecyclePolicy::default().validate().is_ok());
        for days in [0, MAX_LIFECYCLE_DAYS + 1] {
            for policy in [
                LifecyclePolicy { staging_expiration_days: days, ..LifecyclePolicy::default() },
                LifecyclePolicy { abort_incomplete_multipart_days: days, ..LifecyclePolicy::default() },
                LifecyclePolicy { noncurrent_version_expiration_days: days, ..LifecyclePolicy::default() },
            ] {
                assert_eq!(policy.validate(), Err(BucketInitError::InvalidPolicy));
            }
        }
    }

    #[test]
    fn parses_versioning_states() {
        let ns = r#"xmlns="http://s3.amazonaws.com/doc/2006-03-01/""#;
        assert_eq!(parse_versioning(format!("<VersioningConfiguration {ns}/>").as_bytes()), Ok(VersioningStatus::Unversioned));
        assert_eq!(
            parse_versioning(format!("<VersioningConfiguration {ns}><Status>Enabled</Status></VersioningConfiguration>").as_bytes()),
            Ok(VersioningStatus::Enabled)
        );
        assert_eq!(
            parse_versioning(b"<VersioningConfiguration><Status>Suspended</Status><MfaDelete>Disabled</MfaDelete></VersioningConfiguration>"),
            Ok(VersioningStatus::Suspended)
        );
        for junk in [&b""[..], b"<Other/>", b"<VersioningConfiguration><Status>On</Status></VersioningConfiguration>", b"\xff"] {
            assert_eq!(parse_versioning(junk), Err(BucketInitError::MalformedResponse));
        }
    }

    #[test]
    fn foreign_and_unusual_rules_are_recognized() {
        let body = concat!(
            "<LifecycleConfiguration>",
            "<Rule><ID>ops-archive</ID><Prefix>logs/</Prefix><Status>Disabled</Status><Transition><Days>30</Days></Transition></Rule>",
            "<Rule><ID>figcraft-old</ID><Filter><And><Prefix>a</Prefix><Tag><Key>k</Key><Value>v</Value></Tag></And></Filter>",
            "<Status>Enabled</Status><Expiration><Date>2030-01-01T00:00:00Z</Date></Expiration></Rule>",
            "</LifecycleConfiguration>"
        );
        let rules = parse_lifecycle(body.as_bytes()).unwrap();
        assert_eq!(rules[0].id, "figcraft-old");
        assert_eq!(rules[0].filter, RuleFilter::Other);
        assert!(rules[0].other_settings && rules[0].expiration_days.is_none());
        assert_eq!(rules[1].filter, RuleFilter::Prefix("logs/".into()));
        assert!(!rules[1].enabled && rules[1].other_settings);
    }

    #[test]
    fn error_codes_are_read_only_from_error_documents() {
        let body = b"<?xml version=\"1.0\"?><Error><Code>NoSuchLifecycleConfiguration</Code><Message>x</Message></Error>";
        assert_eq!(error_code(body).as_deref(), Some("NoSuchLifecycleConfiguration"));
        assert_eq!(error_code(b"<Error><Code>Not&amp;Code</Code></Error>"), None);
        assert_eq!(error_code(b"<Error><Code>a b</Code></Error>"), None);
        assert_eq!(error_code(b"<Other><Code>X</Code></Other>"), None);
        assert_eq!(error_code(b""), None);
    }

    #[test]
    fn oversized_and_hostile_documents_are_refused() {
        let big = format!("<LifecycleConfiguration>{}</LifecycleConfiguration>", " ".repeat(MAX_RESPONSE_BYTES));
        assert_eq!(parse_lifecycle(big.as_bytes()), Err(BucketInitError::MalformedResponse));
        let many = format!("<LifecycleConfiguration>{}</LifecycleConfiguration>", "<Rule><ID>figcraft-x</ID></Rule>".repeat(MAX_LIFECYCLE_RULES + 1));
        assert_eq!(parse_lifecycle(many.as_bytes()), Err(BucketInitError::MalformedResponse));
        let dtd = r#"<!DOCTYPE a [<!ENTITY e "x">]><LifecycleConfiguration>&e;</LifecycleConfiguration>"#;
        assert_eq!(parse_lifecycle(dtd.as_bytes()), Err(BucketInitError::MalformedResponse));
        for days in ["", "-1", "1.5", "99999999999", "x"] {
            let body = format!(
                "<LifecycleConfiguration><Rule><ID>figcraft-a</ID><Expiration><Days>{days}</Days></Expiration></Rule></LifecycleConfiguration>"
            );
            assert_eq!(parse_lifecycle(body.as_bytes()), Err(BucketInitError::MalformedResponse), "{days}");
        }
        let long_id = format!("<LifecycleConfiguration><Rule><ID>{}</ID></Rule></LifecycleConfiguration>", "a".repeat(1025));
        assert_eq!(parse_lifecycle(long_id.as_bytes()), Err(BucketInitError::MalformedResponse));
    }

    /// Never-crash sweep: every parser accepts arbitrary bytes, including truncations and byte
    /// flips of valid documents, and answers with a value or a fixed error.
    #[test]
    fn junk_input_sweep_never_panics() {
        let valid = [
            LifecyclePolicy::default().to_xml(),
            "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>".to_string(),
            "<Error><Code>NoSuchBucket</Code></Error>".to_string(),
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for document in &valid {
            let bytes = document.as_bytes();
            for end in 0..=bytes.len() {
                let prefix = &bytes[..end];
                let _ = (parse_lifecycle(prefix), parse_versioning(prefix), error_code(prefix));
            }
            for _ in 0..2000 {
                let mut mutated = bytes.to_vec();
                let flips = 1 + (next() % 4) as usize;
                for _ in 0..flips {
                    let index = (next() as usize) % mutated.len();
                    mutated[index] = next() as u8;
                }
                let _ = (parse_lifecycle(&mutated), parse_versioning(&mutated), error_code(&mutated));
            }
        }
        for _ in 0..2000 {
            let length = (next() % 256) as usize;
            let junk: Vec<u8> = (0..length).map(|_| next() as u8).collect();
            let _ = (parse_lifecycle(&junk), parse_versioning(&junk), error_code(&junk));
        }
    }
}
