//! Signed webhook delivery with retries, terminal failure and manual replay (S6-014).
//!
//! # Wire format
//!
//! Every delivery is `POST <endpoint url>` with the stored JSON body and these headers:
//!
//! | Header | Value |
//! |---|---|
//! | `X-FigCraft-Event-Id` | the event's UUID; constant across retries and replays |
//! | `X-FigCraft-Event-Type` | e.g. `document.created` |
//! | `X-FigCraft-Delivery-Id` | this delivery's UUID |
//! | `X-FigCraft-Timestamp` | Unix seconds at signing time |
//! | `X-FigCraft-Signature` | `v1=<hex>` where hex is HMAC-SHA256(secret, `timestamp + "." + body`); during a secret rotation overlap one `v1=` item per live secret, comma separated |
//!
//! A receiver recomputes the MAC over the raw body with its secret, compares in constant time with
//! any `v1` item, and rejects a timestamp more than [`SIGNATURE_TOLERANCE_SECS`] from its clock
//! (the replay window); [`verify_signature`] is that check. Receivers deduplicate on the event id:
//! delivery is at least once.
//!
//! # Retry schedule
//!
//! Success is any `2xx`. Anything else (another status, a timeout, a refused connection, a
//! blocked destination) fails the attempt. A failed attempt is retried after
//! [`RETRY_DELAYS_SECS`] (10 s, 30 s, 2 min, 10 min, 30 min, 2 h, 6 h); after
//! [`DEFAULT_MAX_ATTEMPTS`] attempts the delivery is `failed` for good (terminal) until someone
//! replays it ([`replay_delivery`]), which resets it to a fresh `pending` delivery of the same
//! event with the same body and event id. Redirects are never followed.
//!
//! # Destination safety
//!
//! The destination host is resolved here and every address is checked against
//! [`crate::webhooks::is_non_public_ip`] (unless the policy allows private networks); the request
//! then connects to exactly the vetted addresses, so a name that changes its answer between the
//! check and the connection (DNS rebinding) cannot reach an internal address.
//!
//! Responses are read for their status only; no response body is stored or logged. Nothing here
//! logs a secret, a signature or a body.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{TenantId, Uuid};
use ring::hmac;
use sqlx::PgPool;
use url::{Host, Url};

use crate::secret_box::SecretKey;
use crate::webhooks::{DestinationPolicy, WebhookError, is_non_public_ip, signing_secrets};
use crate::{TenantTransaction, begin_tenant_transaction};

pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_ATTEMPTS: i32 = 8;
/// Delay before attempt `n + 2` after attempt `n + 1` failed.
pub const RETRY_DELAYS_SECS: [i64; 7] = [10, 30, 120, 600, 1_800, 7_200, 21_600];
/// How far a receiver should let the signed timestamp drift from its own clock.
pub const SIGNATURE_TOLERANCE_SECS: i64 = 300;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A claimed delivery that is not finished within this is claimable again.
pub const LEASE_SECS: i32 = 60;
pub const MAX_LISTED_DELIVERIES: i64 = 50;
pub const MAX_LISTED_ATTEMPTS: i64 = 20;
const USER_AGENT: &str = "FigCraft-Webhooks/1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryError {
    InvalidInput,
    NotFound,
    Database,
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "webhook delivery error: {self:?}")
    }
}

impl std::error::Error for DeliveryError {}

fn database_error(error: sqlx::Error) -> DeliveryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503") => DeliveryError::NotFound,
        Some("23514" | "22001") => DeliveryError::InvalidInput,
        _ => DeliveryError::Database,
    }
}

// ---- signing ----

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok()).collect()
}

fn signed_message(timestamp: i64, body: &[u8]) -> Vec<u8> {
    let mut message = timestamp.to_string().into_bytes();
    message.push(b'.');
    message.extend_from_slice(body);
    message
}

/// The `X-FigCraft-Signature` value for `body` signed at `timestamp` with every secret in
/// `secrets` (one `v1=` item each).
#[must_use]
pub fn signature_header(secrets: &[Vec<u8>], timestamp: i64, body: &[u8]) -> String {
    let message = signed_message(timestamp, body);
    secrets
        .iter()
        .map(|secret| format!("v1={}", to_hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), &message).as_ref())))
        .collect::<Vec<_>>()
        .join(",")
}

/// Receiver-side check: does `header` hold a `v1` MAC of `timestamp.body` under `secret`, and is
/// `timestamp` within `tolerance_secs` of `now`? Constant-time comparison.
#[must_use]
pub fn verify_signature(secret: &[u8], header: &str, timestamp: i64, body: &[u8], now: i64, tolerance_secs: i64) -> bool {
    if (now - timestamp).abs() > tolerance_secs {
        return false;
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let message = signed_message(timestamp, body);
    header
        .split(',')
        .filter_map(|item| item.trim().strip_prefix("v1="))
        .filter_map(from_hex)
        .any(|candidate| hmac::verify(&key, &message, &candidate).is_ok())
}

// ---- records ----

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryState {
    Pending,
    Succeeded,
    Failed,
}

impl DeliveryState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryRecord {
    pub id: Uuid,
    pub endpoint_id: Uuid,
    pub event_id: Uuid,
    pub event_type: String,
    pub state: DeliveryState,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_status: Option<i16>,
    pub last_error: Option<String>,
    pub next_attempt_at: i64,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptRecord {
    pub attempt: i32,
    pub started_at: i64,
    pub duration_ms: i32,
    pub http_status: Option<i16>,
    pub error: Option<String>,
}

type AttemptRow = (i32, i64, i32, Option<i16>, Option<String>);
type WorkRow = (String, Vec<u8>, String, String, i32, i32, String);
type DeliveryRow = (String, String, String, String, String, i32, i32, Option<i16>, Option<String>, i64, i64, Option<i64>);

macro_rules! columns {
    () => {
        concat!(
            "d.id::text, d.endpoint_id::text, d.event_id::text, d.event_type, d.state, d.attempts, d.max_attempts, d.last_status, d.last_error, ",
            "floor(extract(epoch FROM d.next_attempt_at))::bigint, floor(extract(epoch FROM d.created_at))::bigint, ",
            "floor(extract(epoch FROM d.completed_at))::bigint"
        )
    };
}

fn delivery(row: DeliveryRow) -> Result<DeliveryRecord, DeliveryError> {
    let (id, endpoint_id, event_id, event_type, state, attempts, max_attempts, last_status, last_error, next_attempt_at, created_at, completed_at) =
        row;
    Ok(DeliveryRecord {
        id: id.parse().map_err(|_| DeliveryError::Database)?,
        endpoint_id: endpoint_id.parse().map_err(|_| DeliveryError::Database)?,
        event_id: event_id.parse().map_err(|_| DeliveryError::Database)?,
        event_type,
        state: DeliveryState::parse(&state).ok_or(DeliveryError::Database)?,
        attempts,
        max_attempts,
        last_status,
        last_error,
        next_attempt_at,
        created_at,
        completed_at,
    })
}

// ---- repository ----

/// Queue `body` (a JSON object, at most [`MAX_BODY_BYTES`]) for an endpoint. Returns the new
/// delivery's id, or `None` when this endpoint already has a delivery for `event_id` (publishing is
/// idempotent per endpoint).
pub async fn enqueue_delivery(
    transaction: &mut TenantTransaction<'_>,
    endpoint_id: Uuid,
    event_id: Uuid,
    event_type: &str,
    body: &[u8],
    max_attempts: i32,
) -> Result<Option<Uuid>, DeliveryError> {
    if body.len() < 2 || body.len() > MAX_BODY_BYTES || body.first() != Some(&b'{') || event_type.is_empty() || !(1..=20).contains(&max_attempts) {
        return Err(DeliveryError::InvalidInput);
    }
    let id = Uuid::new_v4();
    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO webhook_deliveries (tenant_id, id, endpoint_id, event_id, event_type, body, max_attempts) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5, $6, $7) ON CONFLICT (tenant_id, endpoint_id, event_id) DO NOTHING RETURNING id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(endpoint_id.to_string())
    .bind(event_id.to_string())
    .bind(event_type)
    .bind(body)
    .bind(max_attempts)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(inserted.map(|_| id))
}

/// An endpoint's deliveries, newest first, at most [`MAX_LISTED_DELIVERIES`]; `state` filters.
pub async fn list_deliveries(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    endpoint_id: Uuid,
    state: Option<DeliveryState>,
) -> Result<Vec<DeliveryRecord>, DeliveryError> {
    let rows: Vec<DeliveryRow> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM webhook_deliveries d JOIN webhook_endpoints e ON e.tenant_id = d.tenant_id AND e.id = d.endpoint_id ",
        "WHERE d.tenant_id = $1::uuid AND e.system_id = $2 AND d.endpoint_id = $3::uuid AND ($4::text IS NULL OR d.state = $4) ",
        "ORDER BY d.created_at DESC, d.id LIMIT 50"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(endpoint_id.to_string())
    .bind(state.map(DeliveryState::as_str))
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(delivery).collect()
}

pub async fn get_delivery(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    endpoint_id: Uuid,
    delivery_id: Uuid,
) -> Result<Option<DeliveryRecord>, DeliveryError> {
    let row: Option<DeliveryRow> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM webhook_deliveries d JOIN webhook_endpoints e ON e.tenant_id = d.tenant_id AND e.id = d.endpoint_id ",
        "WHERE d.tenant_id = $1::uuid AND e.system_id = $2 AND d.endpoint_id = $3::uuid AND d.id = $4::uuid"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(endpoint_id.to_string())
    .bind(delivery_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(delivery).transpose()
}

/// A delivery's attempt log, newest first, at most [`MAX_LISTED_ATTEMPTS`].
pub async fn list_attempts(transaction: &mut TenantTransaction<'_>, delivery_id: Uuid) -> Result<Vec<AttemptRecord>, DeliveryError> {
    let rows: Vec<AttemptRow> = sqlx::query_as(
        "SELECT attempt, floor(extract(epoch FROM started_at))::bigint, duration_ms, http_status, error FROM webhook_delivery_attempts \
         WHERE tenant_id = $1::uuid AND delivery_id = $2::uuid ORDER BY attempt DESC LIMIT 20",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(delivery_id.to_string())
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(rows
        .into_iter()
        .map(|(attempt, started_at, duration_ms, http_status, error)| AttemptRecord { attempt, started_at, duration_ms, http_status, error })
        .collect())
}

/// Replay a finished (`succeeded` or `failed`) delivery: it becomes a fresh `pending` delivery of
/// the same event, same body and same event id, due now. A `pending` delivery cannot be replayed
/// (`false`), nor can one that does not exist (`NotFound`).
pub async fn replay_delivery(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    endpoint_id: Uuid,
    delivery_id: Uuid,
) -> Result<bool, DeliveryError> {
    let current = get_delivery(transaction, system_id, endpoint_id, delivery_id).await?.ok_or(DeliveryError::NotFound)?;
    if current.state == DeliveryState::Pending {
        return Ok(false);
    }
    let result = sqlx::query(
        "UPDATE webhook_deliveries SET state = 'pending', attempts = 0, next_attempt_at = clock_timestamp(), lease_expires_at = NULL, \
         last_status = NULL, last_error = NULL, completed_at = NULL \
         WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('succeeded', 'failed')",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(delivery_id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(result.rows_affected() == 1)
}

// ---- the worker ----

/// Claim up to `limit` due deliveries across tenants (lease [`LEASE_SECS`], attempt counted).
pub async fn claim_due(pool: &PgPool, limit: i32) -> Result<Vec<(TenantId, Uuid)>, DeliveryError> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT o_tenant_id, o_delivery_id FROM figcraft_claim_webhook_deliveries($1, $2)")
        .bind(limit)
        .bind(LEASE_SECS)
        .fetch_all(pool)
        .await
        .map_err(|_| DeliveryError::Database)?;
    rows.into_iter()
        .map(|(tenant, delivery)| Ok((tenant.parse().map_err(|_| DeliveryError::Database)?, delivery.parse().map_err(|_| DeliveryError::Database)?)))
        .collect()
}

/// What one processed delivery came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Delivered {
        status: u16,
    },
    /// The attempt failed and will be retried.
    Retry {
        error: &'static str,
        status: Option<u16>,
    },
    /// The attempt failed and no attempts are left.
    Failed {
        error: &'static str,
        status: Option<u16>,
    },
    /// Nothing to do (already finished, or the endpoint is gone).
    Skipped,
}

struct Work {
    url: String,
    body: Vec<u8>,
    event_id: Uuid,
    event_type: String,
    attempts: i32,
    max_attempts: i32,
    secrets: Option<Vec<Vec<u8>>>,
}

async fn load_work(pool: &PgPool, key: &SecretKey, tenant: TenantId, delivery_id: Uuid) -> Result<Option<Work>, DeliveryError> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| DeliveryError::Database)?;
    let row: Option<WorkRow> = sqlx::query_as(
        "SELECT e.url, d.body, d.event_id::text, d.event_type, d.attempts, d.max_attempts, d.endpoint_id::text \
         FROM webhook_deliveries d JOIN webhook_endpoints e ON e.tenant_id = d.tenant_id AND e.id = d.endpoint_id \
         WHERE d.tenant_id = $1::uuid AND d.id = $2::uuid AND d.state = 'pending'",
    )
    .bind(tenant.to_string())
    .bind(delivery_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some((url, body, event_id, event_type, attempts, max_attempts, endpoint_id)) = row else {
        let _ = transaction.rollback().await;
        return Ok(None);
    };
    let endpoint_id: Uuid = endpoint_id.parse().map_err(|_| DeliveryError::Database)?;
    let secrets = match signing_secrets(&mut transaction, key, endpoint_id).await {
        Ok(secrets) => Some(secrets),
        Err(WebhookError::SecretUnavailable | WebhookError::NotFound) => None,
        Err(_) => return Err(DeliveryError::Database),
    };
    let _ = transaction.rollback().await;
    Ok(Some(Work { url, body, event_id: event_id.parse().map_err(|_| DeliveryError::Database)?, event_type, attempts, max_attempts, secrets }))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok()).unwrap_or(0)
}

fn tls_client(addresses: &[SocketAddr], domain: Option<&str>, request_timeout: Duration) -> Result<reqwest::Client, &'static str> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| "request_failure")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut builder = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(request_timeout)
        .user_agent(USER_AGENT);
    if let Some(domain) = domain {
        // Connect only to the addresses that were vetted, whatever DNS says a moment later.
        builder = builder.resolve_to_addrs(domain, addresses);
    }
    builder.build().map_err(|_| "request_failure")
}

/// Resolve and vet the destination, then POST. `Ok(status)` for any HTTP answer; `Err(category)`
/// for transport problems and blocked destinations.
async fn post(
    policy: DestinationPolicy,
    url: &str,
    headers: &[(&'static str, String)],
    body: Vec<u8>,
    request_timeout: Duration,
) -> Result<u16, &'static str> {
    let parsed = Url::parse(url).map_err(|_| "request_failure")?;
    let port = parsed.port_or_known_default().ok_or("request_failure")?;
    let (addresses, domain): (Vec<SocketAddr>, Option<String>) = match parsed.host().ok_or("request_failure")? {
        Host::Ipv4(ip) => (vec![SocketAddr::new(IpAddr::V4(ip), port)], None),
        Host::Ipv6(ip) => (vec![SocketAddr::new(IpAddr::V6(ip), port)], None),
        Host::Domain(name) => {
            let lookup = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((name, port))).await.map_err(|_| "dns_failure")?;
            (lookup.map_err(|_| "dns_failure")?.collect(), Some(name.to_owned()))
        }
    };
    if addresses.is_empty() {
        return Err("dns_failure");
    }
    if !policy.allow_private_networks && addresses.iter().any(|address| is_non_public_ip(address.ip())) {
        return Err("blocked_destination");
    }
    if parsed.scheme() == "http" && !policy.allow_private_networks {
        return Err("blocked_destination");
    }
    let client = tls_client(&addresses, domain.as_deref(), request_timeout)?;
    let mut request = client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    match request.send().await {
        Ok(response) => Ok(response.status().as_u16()),
        Err(error) if error.is_timeout() => Err("timeout"),
        Err(error) if error.is_connect() => Err("connect_failure"),
        Err(_) => Err("request_failure"),
    }
}

/// Deliver one claimed delivery: one HTTP attempt, then record it and schedule the retry or the
/// terminal failure.
pub async fn process_claimed(
    pool: &PgPool,
    key: &SecretKey,
    policy: DestinationPolicy,
    tenant: TenantId,
    delivery_id: Uuid,
) -> Result<Outcome, DeliveryError> {
    process_claimed_with(pool, key, policy, tenant, delivery_id, REQUEST_TIMEOUT).await
}

/// [`process_claimed`] with an explicit whole-request timeout (tests use a short one).
pub async fn process_claimed_with(
    pool: &PgPool,
    key: &SecretKey,
    policy: DestinationPolicy,
    tenant: TenantId,
    delivery_id: Uuid,
    request_timeout: Duration,
) -> Result<Outcome, DeliveryError> {
    let Some(work) = load_work(pool, key, tenant, delivery_id).await? else {
        return Ok(Outcome::Skipped);
    };
    let started = Instant::now();
    let result = match &work.secrets {
        None => Err("secret_unavailable"),
        Some(secrets) => {
            let timestamp = now_unix();
            let headers = [
                ("x-figcraft-event-id", work.event_id.to_string()),
                ("x-figcraft-event-type", work.event_type.clone()),
                ("x-figcraft-delivery-id", delivery_id.to_string()),
                ("x-figcraft-timestamp", timestamp.to_string()),
                ("x-figcraft-signature", signature_header(secrets, timestamp, &work.body)),
            ];
            post(policy, &work.url, &headers, work.body.clone(), request_timeout).await
        }
    };
    let duration_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
    let (status, error): (Option<u16>, Option<&'static str>) = match result {
        Ok(status) if (200..300).contains(&status) => (Some(status), None),
        Ok(status) => (Some(status), Some("http_status")),
        Err(category) => (None, Some(category)),
    };
    let exhausted = work.attempts >= work.max_attempts;
    let delay = RETRY_DELAYS_SECS
        .get(usize::try_from(work.attempts.saturating_sub(1)).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or(RETRY_DELAYS_SECS[RETRY_DELAYS_SECS.len() - 1]);
    let new_state = match (error, exhausted) {
        (None, _) => "succeeded",
        (Some(_), true) => "failed",
        (Some(_), false) => "pending",
    };
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| DeliveryError::Database)?;
    sqlx::query(
        "INSERT INTO webhook_delivery_attempts (tenant_id, delivery_id, attempt, duration_ms, http_status, error) \
         VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(tenant.to_string())
    .bind(delivery_id.to_string())
    .bind(work.attempts.max(1))
    .bind(duration_ms)
    .bind(status.and_then(|status| i16::try_from(status).ok()))
    .bind(error)
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    let updated = sqlx::query(
        "UPDATE webhook_deliveries SET state = $3, last_status = $4, last_error = $5, lease_expires_at = NULL, \
         next_attempt_at = CASE WHEN $3 = 'pending' THEN clock_timestamp() + $6::bigint * interval '1 second' ELSE next_attempt_at END, \
         completed_at = CASE WHEN $3 = 'pending' THEN NULL ELSE clock_timestamp() END \
         WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'pending' AND attempts = $7",
    )
    .bind(tenant.to_string())
    .bind(delivery_id.to_string())
    .bind(new_state)
    .bind(status.and_then(|status| i16::try_from(status).ok()))
    .bind(error)
    .bind(delay)
    .bind(work.attempts)
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    transaction.commit().await.map_err(|_| DeliveryError::Database)?;
    if updated.rows_affected() != 1 {
        // Replayed or re-claimed while the request was in flight: the newer attempt owns the row.
        return Ok(Outcome::Skipped);
    }
    Ok(match (error, exhausted) {
        (None, _) => Outcome::Delivered { status: status.unwrap_or(200) },
        (Some(error), true) => Outcome::Failed { error, status },
        (Some(error), false) => Outcome::Retry { error, status },
    })
}

/// Claim and process one batch; returns how many deliveries were processed.
pub async fn run_once(pool: &PgPool, key: &SecretKey, policy: DestinationPolicy, limit: i32) -> Result<usize, DeliveryError> {
    let claimed = claim_due(pool, limit).await?;
    let count = claimed.len();
    for (tenant, delivery) in claimed {
        // One bad delivery must not stop the batch; its lease expires and it is retried.
        let _ = process_claimed(pool, key, policy, tenant, delivery).await;
    }
    Ok(count)
}

/// The dispatcher loop for the worker role: process due deliveries until `stop` is set, sleeping
/// a second when idle. Errors are swallowed per batch (the database may be restarting); the next
/// pass tries again.
pub async fn run_dispatcher(pool: PgPool, key: Arc<SecretKey>, policy: DestinationPolicy, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let processed = run_once(&pool, &key, policy, 20).await.unwrap_or(0);
        if processed == 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SIGNATURE_TOLERANCE_SECS, from_hex, signature_header, to_hex, verify_signature};

    #[test]
    fn signatures_cover_the_timestamp_and_the_exact_body() {
        let secret = b"whsec_example".to_vec();
        let header = signature_header(std::slice::from_ref(&secret), 1_800_000_000, b"{\"a\":1}");
        assert!(header.starts_with("v1="));
        assert!(verify_signature(&secret, &header, 1_800_000_000, b"{\"a\":1}", 1_800_000_010, SIGNATURE_TOLERANCE_SECS));
        // Any change to body, timestamp or secret fails.
        assert!(!verify_signature(&secret, &header, 1_800_000_000, b"{\"a\":2}", 1_800_000_010, SIGNATURE_TOLERANCE_SECS));
        assert!(!verify_signature(&secret, &header, 1_800_000_001, b"{\"a\":1}", 1_800_000_010, SIGNATURE_TOLERANCE_SECS));
        assert!(!verify_signature(b"other", &header, 1_800_000_000, b"{\"a\":1}", 1_800_000_010, SIGNATURE_TOLERANCE_SECS));
        // The replay window.
        assert!(verify_signature(&secret, &header, 1_800_000_000, b"{\"a\":1}", 1_800_000_000 + SIGNATURE_TOLERANCE_SECS, SIGNATURE_TOLERANCE_SECS));
        assert!(!verify_signature(
            &secret,
            &header,
            1_800_000_000,
            b"{\"a\":1}",
            1_800_000_000 + SIGNATURE_TOLERANCE_SECS + 1,
            SIGNATURE_TOLERANCE_SECS
        ));
        assert!(!verify_signature(
            &secret,
            &header,
            1_800_000_000,
            b"{\"a\":1}",
            1_800_000_000 - SIGNATURE_TOLERANCE_SECS - 1,
            SIGNATURE_TOLERANCE_SECS
        ));
        // Garbage headers.
        for bad in ["", "v1=", "v1=zz", "v2=00", "v1=00,v1=11", "sha256=abc"] {
            assert!(!verify_signature(&secret, bad, 1_800_000_000, b"{\"a\":1}", 1_800_000_000, SIGNATURE_TOLERANCE_SECS), "{bad}");
        }
    }

    #[test]
    fn a_rotation_overlap_signs_with_both_secrets_and_either_verifies() {
        let new = b"whsec_new".to_vec();
        let old = b"whsec_old".to_vec();
        let header = signature_header(&[new.clone(), old.clone()], 100, b"{}");
        assert_eq!(header.matches("v1=").count(), 2);
        assert!(verify_signature(&new, &header, 100, b"{}", 100, 300));
        assert!(verify_signature(&old, &header, 100, b"{}", 100, 300));
        assert!(!verify_signature(b"whsec_third", &header, 100, b"{}", 100, 300));
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        assert_eq!(to_hex(&[0, 15, 255]), "000fff");
        assert_eq!(from_hex("000fff"), Some(vec![0, 15, 255]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("é1"), None);
    }
}
