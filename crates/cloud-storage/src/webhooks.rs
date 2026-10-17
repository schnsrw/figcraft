//! Webhook endpoints and their encrypted signing secrets (S6-013).
//!
//! A host registers HTTPS endpoints for its events. Each endpoint has a signing secret generated
//! here (256 random bits), returned to the caller exactly once and stored only sealed by
//! [`crate::secret_box`]. Rotation keeps the previous secret valid for a bounded overlap
//! ([`ROTATION_OVERLAP_SECS`]) so receivers can switch without losing deliveries. Listings and
//! reads never include a secret. Every query takes its tenant from `TenantTransaction`.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use figcraft_cloud_domain::{TenantId, Uuid};
use ring::rand::{SecureRandom, SystemRandom};
use url::{Host, Url};

use crate::TenantTransaction;
use crate::secret_box::SecretKey;

pub const MAX_URL_BYTES: usize = 2048;
pub const MAX_EVENT_TYPES: usize = 32;
pub const MAX_ENDPOINTS_PER_HOST: i64 = 10;
pub const MAX_LISTED_ENDPOINTS: i64 = 100;
/// How long the previous secret keeps signing after a rotation.
pub const ROTATION_OVERLAP_SECS: i64 = 24 * 60 * 60;
/// Prefix every signing secret starts with.
pub const SECRET_PREFIX: &str = "whsec_";

/// The event types an endpoint may subscribe to. An empty subscription means all of them.
pub const EVENT_TYPES: &[&str] =
    &["document.created", "document.updated", "document.archived", "document.restored", "document.deleted", "export.completed", "export.failed"];

/// Fixed, non-leaking categories for webhook repository failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookError {
    InvalidInput,
    /// The host or endpoint does not exist in this tenant.
    NotFound,
    TooMany,
    /// A stored secret does not open (wrong deployment key or tampering).
    SecretUnavailable,
    Database,
}

impl fmt::Display for WebhookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "webhook error: {self:?}")
    }
}

impl std::error::Error for WebhookError {}

fn database_error(error: sqlx::Error) -> WebhookError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503") => WebhookError::NotFound,
        Some("23514" | "22001") => WebhookError::InvalidInput,
        _ => WebhookError::Database,
    }
}

/// Where deliveries may go. Production refuses private and loopback destinations (server-side
/// request forgery); development and tests may allow them, and then also plain `http`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DestinationPolicy {
    pub allow_private_networks: bool,
}

impl DestinationPolicy {
    pub const PUBLIC_ONLY: Self = Self { allow_private_networks: false };
}

/// Whether `ip` is a destination webhooks must never reach in production: loopback, private,
/// link-local (including cloud metadata), carrier-grade NAT, unique-local, multicast, unspecified,
/// documentation and other non-global ranges.
#[must_use]
pub fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => non_public_v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => non_public_v4(mapped),
            None => non_public_v6(ip),
        },
    }
}

fn non_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240
}

fn non_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first == 0x2001 && ip.segments()[1] == 0x0db8)
        || (first & 0xffc0) == 0xfec0
}

/// Validate an endpoint URL: at most [`MAX_URL_BYTES`], `https` (plain `http` only when the policy
/// allows private networks), a host, no credentials or fragment, and no loopback, private or
/// internal destination named by an IP literal or an obviously internal name. Hostnames are
/// resolved and re-checked again when a delivery connects.
#[must_use]
pub fn valid_endpoint_url(url: &str, policy: DestinationPolicy) -> bool {
    if url.is_empty() || url.len() > MAX_URL_BYTES || url.chars().any(|c| c.is_control() || c == ' ') {
        return false;
    }
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let scheme_ok = parsed.scheme() == "https" || (parsed.scheme() == "http" && policy.allow_private_networks);
    if !scheme_ok || !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return false;
    }
    match parsed.host() {
        None => false,
        Some(Host::Ipv4(ip)) => policy.allow_private_networks || !is_non_public_ip(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => policy.allow_private_networks || !is_non_public_ip(IpAddr::V6(ip)),
        Some(Host::Domain(name)) => {
            let lower = name.to_ascii_lowercase();
            let internal = lower == "localhost"
                || lower.ends_with(".localhost")
                || lower.ends_with(".local")
                || lower.ends_with(".internal")
                || lower.ends_with(".lan")
                || !lower.contains('.');
            policy.allow_private_networks || !internal
        }
    }
}

/// Whether `events` is a valid subscription: known event types, no duplicates, at most
/// [`MAX_EVENT_TYPES`]. Empty means all events.
#[must_use]
pub fn valid_event_types(events: &[String]) -> bool {
    events.len() <= MAX_EVENT_TYPES
        && events.iter().all(|event| EVENT_TYPES.contains(&event.as_str()))
        && events.iter().enumerate().all(|(index, event)| !events[..index].contains(event))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookEndpoint {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub system_id: String,
    pub url: String,
    pub event_types: Vec<String>,
    pub enabled: bool,
    /// Whether a rotated-out secret is still being signed with.
    pub previous_secret_active: bool,
}

/// A new secret in the clear. Shown to the caller once; never stored or logged.
pub struct NewSecret(String);

impl NewSecret {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for NewSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NewSecret(redacted)")
    }
}

fn generate_secret() -> Result<String, WebhookError> {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).map_err(|_| WebhookError::Database)?;
    Ok(format!("{SECRET_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn aad(tenant: TenantId, endpoint: Uuid) -> Vec<u8> {
    format!("webhook-secret|{tenant}|{endpoint}").into_bytes()
}

type Row = (String, String, String, String, Vec<String>, bool, bool);
/// The selected columns, as a literal so every query stays a compile-time string.
macro_rules! columns {
    () => {
        "id::text, tenant_id::text, system_id, url, event_types, enabled, (previous_sealed IS NOT NULL AND previous_expires_at > clock_timestamp())"
    };
}

fn endpoint(row: Row) -> Result<WebhookEndpoint, WebhookError> {
    let (id, tenant_id, system_id, url, event_types, enabled, previous_secret_active) = row;
    Ok(WebhookEndpoint {
        id: id.parse().map_err(|_| WebhookError::Database)?,
        tenant_id: tenant_id.parse().map_err(|_| WebhookError::Database)?,
        system_id,
        url,
        event_types,
        enabled,
        previous_secret_active,
    })
}

/// Register an endpoint for a host. Returns it with its signing secret, to be shown once.
pub async fn create_webhook_endpoint(
    transaction: &mut TenantTransaction<'_>,
    key: &SecretKey,
    policy: DestinationPolicy,
    system_id: &str,
    url: &str,
    event_types: &[String],
) -> Result<(WebhookEndpoint, NewSecret), WebhookError> {
    if !valid_endpoint_url(url, policy) || !valid_event_types(event_types) {
        return Err(WebhookError::InvalidInput);
    }
    // Lock the host row so concurrent creations cannot both pass the cap.
    let locked: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM host_systems WHERE tenant_id = $1::uuid AND system_id = $2 FOR UPDATE")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    locked.ok_or(WebhookError::NotFound)?;
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_endpoints WHERE tenant_id = $1::uuid AND system_id = $2")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .fetch_one(transaction.connection())
        .await
        .map_err(database_error)?;
    if held >= MAX_ENDPOINTS_PER_HOST {
        return Err(WebhookError::TooMany);
    }
    let id = Uuid::new_v4();
    let secret = generate_secret()?;
    let sealed = key.seal(&aad(transaction.tenant_id(), id), secret.as_bytes()).map_err(|_| WebhookError::SecretUnavailable)?;
    let row: Row = sqlx::query_as(concat!(
        "INSERT INTO webhook_endpoints (tenant_id, id, system_id, url, event_types, secret_sealed, secret_version) ",
        "VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, 1) RETURNING ",
        columns!()
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(system_id)
    .bind(url)
    .bind(event_types)
    .bind(&sealed)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok((endpoint(row)?, NewSecret(secret)))
}

/// One host's endpoints, oldest first, at most [`MAX_LISTED_ENDPOINTS`]. No secrets.
pub async fn list_webhook_endpoints(transaction: &mut TenantTransaction<'_>, system_id: &str) -> Result<Vec<WebhookEndpoint>, WebhookError> {
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM webhook_endpoints WHERE tenant_id = $1::uuid AND system_id = $2 ORDER BY created_at, id LIMIT 100"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(endpoint).collect()
}

pub async fn get_webhook_endpoint(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    id: Uuid,
) -> Result<Option<WebhookEndpoint>, WebhookError> {
    let row: Option<Row> =
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM webhook_endpoints WHERE tenant_id = $1::uuid AND system_id = $2 AND id = $3::uuid"))
            .bind(transaction.tenant_id().to_string())
            .bind(system_id)
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    row.map(endpoint).transpose()
}

/// Change an endpoint's URL, subscription and/or enabled flag. `None` leaves a field alone.
pub async fn update_webhook_endpoint(
    transaction: &mut TenantTransaction<'_>,
    policy: DestinationPolicy,
    system_id: &str,
    id: Uuid,
    url: Option<&str>,
    event_types: Option<&[String]>,
    enabled: Option<bool>,
) -> Result<Option<WebhookEndpoint>, WebhookError> {
    if url.is_some_and(|url| !valid_endpoint_url(url, policy)) || event_types.is_some_and(|events| !valid_event_types(events)) {
        return Err(WebhookError::InvalidInput);
    }
    let row: Option<Row> = sqlx::query_as(concat!(
        "UPDATE webhook_endpoints SET url = COALESCE($4, url), event_types = COALESCE($5, event_types), ",
        "enabled = COALESCE($6, enabled), updated_at = clock_timestamp() ",
        "WHERE tenant_id = $1::uuid AND system_id = $2 AND id = $3::uuid RETURNING ",
        columns!()
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(id.to_string())
    .bind(url)
    .bind(event_types)
    .bind(enabled)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(endpoint).transpose()
}

pub async fn delete_webhook_endpoint(transaction: &mut TenantTransaction<'_>, system_id: &str, id: Uuid) -> Result<bool, WebhookError> {
    let result = sqlx::query("DELETE FROM webhook_endpoints WHERE tenant_id = $1::uuid AND system_id = $2 AND id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .bind(id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(result.rows_affected() == 1)
}

/// Replace the signing secret. The old one keeps signing for [`ROTATION_OVERLAP_SECS`]. Returns
/// the new secret in the clear, once; `None` when the endpoint does not exist.
pub async fn rotate_webhook_secret(
    transaction: &mut TenantTransaction<'_>,
    key: &SecretKey,
    system_id: &str,
    id: Uuid,
) -> Result<Option<NewSecret>, WebhookError> {
    let secret = generate_secret()?;
    let sealed = key.seal(&aad(transaction.tenant_id(), id), secret.as_bytes()).map_err(|_| WebhookError::SecretUnavailable)?;
    let result = sqlx::query(
        "UPDATE webhook_endpoints SET previous_sealed = secret_sealed, \
         previous_expires_at = clock_timestamp() + $4::bigint * interval '1 second', secret_sealed = $5, updated_at = clock_timestamp() \
         WHERE tenant_id = $1::uuid AND system_id = $2 AND id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(id.to_string())
    .bind(ROTATION_OVERLAP_SECS)
    .bind(&sealed)
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok((result.rows_affected() == 1).then_some(NewSecret(secret)))
}

/// The secrets a delivery signs with: the current one, plus the previous one while its overlap
/// lasts. Opened here, for the delivery worker only; never returned by an API.
pub async fn signing_secrets(transaction: &mut TenantTransaction<'_>, key: &SecretKey, id: Uuid) -> Result<Vec<Vec<u8>>, WebhookError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT secret_sealed, CASE WHEN previous_expires_at > clock_timestamp() THEN previous_sealed END \
         FROM webhook_endpoints WHERE tenant_id = $1::uuid AND id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let (current, previous) = row.ok_or(WebhookError::NotFound)?;
    let context = aad(transaction.tenant_id(), id);
    let mut secrets = vec![key.open(&context, &current).map_err(|_| WebhookError::SecretUnavailable)?];
    if let Some(previous) = previous {
        secrets.push(key.open(&context, &previous).map_err(|_| WebhookError::SecretUnavailable)?);
    }
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::{DestinationPolicy, is_non_public_ip, valid_endpoint_url, valid_event_types};

    const PUBLIC: DestinationPolicy = DestinationPolicy::PUBLIC_ONLY;
    const DEV: DestinationPolicy = DestinationPolicy { allow_private_networks: true };

    #[test]
    fn public_https_urls_pass_and_internal_destinations_do_not() {
        for good in ["https://hooks.example.com/figcraft", "https://hooks.example.com:8443/a?b=c", "https://203.0.114.9/x"] {
            assert!(valid_endpoint_url(good, PUBLIC), "{good}");
        }
        for bad in [
            "http://hooks.example.com/x",
            "https://user:pw@hooks.example.com/x",
            "https://hooks.example.com/x#frag",
            "https://localhost/x",
            "https://app.localhost/x",
            "https://printer.local/x",
            "https://db.internal/x",
            "https://intranet/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://172.16.0.1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/x",
            "https://0.0.0.0/x",
            "https://[fd00::1]/x",
            "https://[fe80::1]/x",
            "https://[::ffff:10.0.0.1]/x",
            "https://[::ffff:169.254.169.254]/x",
            "ftp://hooks.example.com/x",
            "javascript:alert(1)",
            "",
            "https://",
            "not a url",
        ] {
            assert!(!valid_endpoint_url(bad, PUBLIC), "{bad}");
        }
        assert!(!valid_endpoint_url(&format!("https://hooks.example.com/{}", "a".repeat(2100)), PUBLIC));
        assert!(!valid_endpoint_url("https://hooks.example.com/a\nb", PUBLIC));
        // Development may point at loopback and use plain http; credentials are still refused.
        assert!(valid_endpoint_url("http://127.0.0.1:9000/hook", DEV));
        assert!(valid_endpoint_url("http://localhost:9000/hook", DEV));
        assert!(!valid_endpoint_url("http://user@127.0.0.1/hook", DEV));
        assert!(!valid_endpoint_url("ftp://127.0.0.1/hook", DEV));
    }

    #[test]
    fn non_public_ranges_are_recognised() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.1.1",
            "100.127.0.1",
            "198.19.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_non_public_ip(ip.parse().expect("ip")), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(!is_non_public_ip(ip.parse().expect("ip")), "{ip}");
        }
    }

    #[test]
    fn event_subscriptions_are_known_unique_and_bounded() {
        assert!(valid_event_types(&[]));
        assert!(valid_event_types(&["document.created".into(), "export.completed".into()]));
        assert!(!valid_event_types(&["document.created".into(), "document.created".into()]));
        assert!(!valid_event_types(&["document.exploded".into()]));
        assert!(!valid_event_types(&["*".into()]));
    }
}
