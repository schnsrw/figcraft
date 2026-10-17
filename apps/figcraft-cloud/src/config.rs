use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;

use figcraft_cloud_domain::DeploymentMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloudRole {
    All,
    Api,
    Relay,
    Worker,
}

impl CloudRole {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "api" => Some(Self::Api),
            "relay" => Some(Self::Relay),
            "worker" => Some(Self::Worker),
            _ => None,
        }
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Api => "api",
            Self::Relay => "relay",
            Self::Worker => "worker",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CloudConfig {
    pub(super) role: CloudRole,
    pub(super) deployment: DeploymentMode,
    pub(super) secrets: BTreeMap<&'static str, Secret>,
    environment: Environment,
    public_url: Option<String>,
    /// `FIGCRAFT_RELAY_ADVERTISE_URL`: where other relays and the gateway reach this replica.
    relay_advertise_url: Option<String>,
    /// `FIGCRAFT_DRAIN_SECONDS`: how long a stopping process stays up, not ready, before it stops.
    drain_seconds: u64,
    room_idle_seconds: u64,
    snapshot_interval_seconds: u64,
    snapshot_after_events: u64,
    relay_quotas: RelayQuotas,
    oidc_issuer: Option<String>,
    /// `OIDC_CLIENT_ID` and `OIDC_REDIRECT_URL` (S4 auth wiring): login is enabled only when
    /// these and `OIDC_ISSUER` are all set.
    oidc_client_id: Option<String>,
    oidc_redirect_url: Option<String>,
    /// `FIGCRAFT_SINGLE_TENANT_ID` (validated UUID): the tenant of a single-tenant deployment.
    single_tenant_id: Option<figcraft_cloud_domain::TenantId>,
    s3_endpoint: Option<String>,
    s3_region: Option<String>,
    s3_bucket: Option<String>,
    s3_force_path_style: bool,
    mounted_secrets: BTreeSet<&'static str>,
    /// Peers whose `X-Forwarded-For` is believed (`FIGCRAFT_TRUSTED_PROXY_CIDRS`; default none).
    pub(super) trusted_proxies: Vec<crate::api::client_ip::Cidr>,
    /// Argon2id cost for built-in accounts (`FIGCRAFT_PASSWORD_MEMORY_KIB`, `_PASSES`, `_LANES`).
    password_params: figcraft_cloud_auth::password::HashParams,
    /// Password hashes computed at once (`FIGCRAFT_PASSWORD_CONCURRENCY`).
    password_concurrency: usize,
    /// Sign-in lockout (`FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD`, `_BASE_SECONDS`, `_MAX_SECONDS`).
    login_lockout: figcraft_cloud_storage::credentials::LockoutPolicy,
    /// Document images (TS-009): `FIGCRAFT_IMAGE_MAX_BYTES`, `FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES`,
    /// `FIGCRAFT_IMAGE_UPLOAD_SECONDS`.
    images: crate::api::images_api::ImageSettings,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            role: CloudRole::All,
            deployment: DeploymentMode::SingleTenant,
            secrets: BTreeMap::new(),
            environment: Environment::Development,
            public_url: None,
            relay_advertise_url: None,
            drain_seconds: DEFAULT_DRAIN_SECONDS,
            room_idle_seconds: DEFAULT_ROOM_IDLE_SECONDS,
            snapshot_interval_seconds: DEFAULT_SNAPSHOT_INTERVAL_SECONDS,
            snapshot_after_events: DEFAULT_SNAPSHOT_AFTER_EVENTS,
            relay_quotas: RelayQuotas::default(),
            oidc_issuer: None,
            oidc_client_id: None,
            oidc_redirect_url: None,
            single_tenant_id: None,
            s3_endpoint: None,
            s3_region: None,
            s3_bucket: None,
            s3_force_path_style: false,
            mounted_secrets: BTreeSet::new(),
            trusted_proxies: Vec::new(),
            password_params: figcraft_cloud_auth::password::HashParams::default(),
            password_concurrency: 4,
            login_lockout: figcraft_cloud_storage::credentials::LockoutPolicy::default(),
            images: crate::api::images_api::ImageSettings::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Environment {
    Development,
    Production,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Secret(String);

impl Secret {
    pub(super) fn from_file(path: &str) -> Result<Self, &'static str> {
        const MAX_SECRET_FILE_BYTES: usize = 64 * 1024;
        let contents = fs::read(path).map_err(|_| "unable to read configured secret file")?;
        if contents.len() > MAX_SECRET_FILE_BYTES {
            return Err("configured secret file exceeds the size limit");
        }
        let value = String::from_utf8(contents).map_err(|_| "configured secret file is not UTF-8")?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.trim().is_empty() {
            return Err("configured secret file is empty");
        }
        Ok(Self(value.to_string()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl CloudConfig {
    /// Parse cloud-specific settings. Unrelated process environment is intentionally ignored.
    /// Secret file contents are held only in a redacted wrapper.
    pub(super) fn parse<'a>(settings: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self, &'static str> {
        let mut config = Self::default();
        let mut seen = BTreeSet::new();
        let mut seen_secrets = BTreeSet::new();

        for (key, value) in settings {
            if !is_cloud_setting(key) {
                continue;
            }
            if !is_known_setting(key) {
                return Err("unknown cloud setting");
            }
            if !seen.insert(key) {
                return Err("duplicate cloud setting");
            }
            if value.trim().is_empty() {
                return Err("cloud settings may not be empty");
            }

            match key {
                "FIGCRAFT_ROLE" => {
                    config.role = CloudRole::parse(value).ok_or("invalid FIGCRAFT_ROLE; expected all, api, relay or worker")?;
                }
                "FIGCRAFT_DEPLOYMENT" => {
                    config.deployment = match value {
                        "single" => DeploymentMode::SingleTenant,
                        "multi" => DeploymentMode::MultiTenant,
                        "embedded" => DeploymentMode::Embedded,
                        _ => {
                            return Err("invalid FIGCRAFT_DEPLOYMENT; expected single, multi or embedded");
                        }
                    };
                }
                "FIGCRAFT_ENV" => {
                    config.environment = match value {
                        "development" => Environment::Development,
                        "production" => Environment::Production,
                        _ => return Err("invalid FIGCRAFT_ENV; expected development or production"),
                    };
                }
                "FIGCRAFT_PUBLIC_URL" => config.public_url = Some(value.to_string()),
                "FIGCRAFT_RELAY_MAX_ROOMS" => {
                    config.relay_quotas.max_rooms =
                        bounded(value, 1, 100_000, "FIGCRAFT_RELAY_MAX_ROOMS must be a whole number from 1 to 100000")? as usize
                }
                "FIGCRAFT_RELAY_MAX_CONNECTIONS" => {
                    config.relay_quotas.max_connections =
                        bounded(value, 1, 100_000, "FIGCRAFT_RELAY_MAX_CONNECTIONS must be a whole number from 1 to 100000")? as usize;
                }
                "FIGCRAFT_RELAY_MAX_PEERS_PER_ROOM" => {
                    config.relay_quotas.max_peers_per_room =
                        bounded(value, 1, 10_000, "FIGCRAFT_RELAY_MAX_PEERS_PER_ROOM must be a whole number from 1 to 10000")? as usize;
                }
                "FIGCRAFT_RELAY_MESSAGES_PER_SECOND" => {
                    config.relay_quotas.messages_per_second =
                        bounded(value, 1, 10_000, "FIGCRAFT_RELAY_MESSAGES_PER_SECOND must be a whole number from 1 to 10000")?;
                }
                "FIGCRAFT_RELAY_MAX_MESSAGE_BYTES" => {
                    config.relay_quotas.max_message_bytes =
                        bounded(value, 1 << 10, 64 << 20, "FIGCRAFT_RELAY_MAX_MESSAGE_BYTES must be a whole number of bytes from 1024 to 67108864")?
                            as usize;
                }
                "FIGCRAFT_RELAY_MAX_QUEUED_BYTES" => {
                    config.relay_quotas.max_queued_bytes = bounded(
                        value,
                        1 << 20,
                        1 << 30,
                        "FIGCRAFT_RELAY_MAX_QUEUED_BYTES must be a whole number of bytes from 1048576 to 1073741824",
                    )? as usize;
                }
                "FIGCRAFT_ROOM_IDLE_SECONDS" => {
                    config.room_idle_seconds = bounded(value, 0, 3600, "FIGCRAFT_ROOM_IDLE_SECONDS must be a whole number of seconds from 0 to 3600")?
                }
                "FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS" => {
                    config.snapshot_interval_seconds =
                        bounded(value, 10, 86_400, "FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS must be a whole number of seconds from 10 to 86400")?;
                }
                "FIGCRAFT_SNAPSHOT_AFTER_EVENTS" => {
                    config.snapshot_after_events =
                        bounded(value, 1, 1_000_000, "FIGCRAFT_SNAPSHOT_AFTER_EVENTS must be a whole number from 1 to 1000000")?;
                }
                "FIGCRAFT_DRAIN_SECONDS" => {
                    config.drain_seconds = match value.parse::<u64>() {
                        Ok(seconds) if seconds <= MAX_DRAIN_SECONDS => seconds,
                        _ => return Err("FIGCRAFT_DRAIN_SECONDS must be a whole number of seconds from 0 to 120"),
                    };
                }
                "FIGCRAFT_RELAY_ADVERTISE_URL" => {
                    if !is_relay_advertise_url(value) {
                        return Err("FIGCRAFT_RELAY_ADVERTISE_URL must be a ws:// or wss:// URL without credentials, query or fragment");
                    }
                    config.relay_advertise_url = Some(value.to_string());
                }
                "FIGCRAFT_TRUSTED_PROXY_CIDRS" => config.trusted_proxies = crate::api::client_ip::parse_cidrs(value)?,
                "FIGCRAFT_PASSWORD_MEMORY_KIB" => {
                    config.password_params.memory_kib =
                        bounded(value, 8192, 1_048_576, "FIGCRAFT_PASSWORD_MEMORY_KIB must be a whole number of KiB from 8192 to 1048576")? as u32;
                }
                "FIGCRAFT_PASSWORD_PASSES" => {
                    config.password_params.passes = bounded(value, 1, 10, "FIGCRAFT_PASSWORD_PASSES must be a whole number from 1 to 10")? as u32;
                }
                "FIGCRAFT_PASSWORD_LANES" => {
                    config.password_params.lanes = bounded(value, 1, 8, "FIGCRAFT_PASSWORD_LANES must be a whole number from 1 to 8")? as u32;
                }
                "FIGCRAFT_PASSWORD_CONCURRENCY" => {
                    config.password_concurrency =
                        bounded(value, 1, 64, "FIGCRAFT_PASSWORD_CONCURRENCY must be a whole number from 1 to 64")? as usize;
                }
                "FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD" => {
                    config.login_lockout.threshold =
                        bounded(value, 1, 100, "FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD must be a whole number from 1 to 100")? as u32;
                }
                "FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS" => {
                    config.login_lockout.base_secs =
                        bounded(value, 1, 86_400, "FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS must be a whole number of seconds from 1 to 86400")? as i64;
                }
                "FIGCRAFT_IMAGE_MAX_BYTES" => {
                    config.images.max_bytes =
                        bounded(value, 1 << 20, 256 << 20, "FIGCRAFT_IMAGE_MAX_BYTES must be a whole number of bytes from 1048576 to 268435456")?;
                }
                "FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES" => {
                    config.images.tenant_quota_bytes =
                        bounded(value, 0, 1 << 50, "FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES must be a whole number of bytes (0: no quota)")?;
                }
                "FIGCRAFT_IMAGE_UPLOAD_SECONDS" => {
                    config.images.upload_seconds =
                        bounded(value, 10, 3600, "FIGCRAFT_IMAGE_UPLOAD_SECONDS must be a whole number of seconds from 10 to 3600")?;
                }
                "FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS" => {
                    config.login_lockout.max_secs =
                        bounded(value, 1, 86_400, "FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS must be a whole number of seconds from 1 to 86400")? as i64;
                }
                "OIDC_ISSUER" => config.oidc_issuer = Some(value.to_string()),
                "OIDC_CLIENT_ID" => config.oidc_client_id = Some(value.to_string()),
                "OIDC_REDIRECT_URL" => config.oidc_redirect_url = Some(value.to_string()),
                "FIGCRAFT_SINGLE_TENANT_ID" => {
                    config.single_tenant_id = Some(value.parse().map_err(|_| "FIGCRAFT_SINGLE_TENANT_ID must be a UUID")?);
                }
                "S3_ENDPOINT" => config.s3_endpoint = Some(value.to_string()),
                "S3_REGION" => config.s3_region = Some(value.to_string()),
                "S3_BUCKET" => config.s3_bucket = Some(value.to_string()),
                "S3_FORCE_PATH_STYLE" => {
                    config.s3_force_path_style = match value {
                        "true" => true,
                        "false" => false,
                        _ => return Err("invalid S3_FORCE_PATH_STYLE; expected true or false"),
                    };
                }
                _ if let Some((name, from_file)) = secret_setting(key) => {
                    if !seen_secrets.insert(name) {
                        return Err("secret may be configured only once");
                    }
                    let secret = if from_file { Secret::from_file(value)? } else { Secret(value.to_string()) };
                    config.secrets.insert(name, secret);
                    if from_file {
                        config.mounted_secrets.insert(name);
                    }
                }
                _ => {}
            }
        }

        if !config.login_lockout.is_valid() {
            return Err("the sign-in lockout base seconds must not exceed its maximum seconds");
        }
        config.validate_production()?;
        Ok(config)
    }

    /// The document image limits (`FIGCRAFT_IMAGE_*`).
    pub(super) const fn image_settings(&self) -> crate::api::images_api::ImageSettings {
        self.images
    }

    /// `FIGCRAFT_PUBLIC_URL`, when configured.
    pub(super) fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    /// The relay's quotas (`FIGCRAFT_RELAY_MAX_*`, `FIGCRAFT_RELAY_MESSAGES_PER_SECOND`).
    pub(super) fn relay_quotas(&self) -> &RelayQuotas {
        &self.relay_quotas
    }

    /// How long an emptied room stays open before it is saved and closed.
    pub(super) fn room_idle(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.room_idle_seconds)
    }

    /// The most a changing room goes without a snapshot.
    pub(super) fn snapshot_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.snapshot_interval_seconds)
    }

    /// Accepted batches after which a room is snapshotted.
    pub(super) fn snapshot_after_events(&self) -> u64 {
        self.snapshot_after_events
    }

    /// How long a stopping process drains: not ready, turning new connections away, before it stops.
    pub(super) fn drain_period(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.drain_seconds)
    }

    /// `FIGCRAFT_RELAY_ADVERTISE_URL`, when configured: it turns on room leases for this relay.
    pub(super) fn relay_advertise_url(&self) -> Option<&str> {
        self.relay_advertise_url.as_deref()
    }

    /// The Argon2id cost for new password hashes.
    pub(super) const fn password_params(&self) -> figcraft_cloud_auth::password::HashParams {
        self.password_params
    }

    /// When and for how long failing sign-ins lock an account.
    pub(super) const fn login_lockout(&self) -> figcraft_cloud_storage::credentials::LockoutPolicy {
        self.login_lockout
    }

    /// How many password hashes may be computed at once.
    pub(super) const fn password_concurrency(&self) -> usize {
        self.password_concurrency
    }

    /// `FIGCRAFT_SINGLE_TENANT_ID`, when configured.
    pub(super) const fn single_tenant_id(&self) -> Option<figcraft_cloud_domain::TenantId> {
        self.single_tenant_id
    }

    /// `(issuer, client_id, client_secret, redirect_url)` when OIDC login is fully configured.
    /// The client secret is optional (public clients use PKCE alone).
    pub(super) fn oidc_settings(&self) -> Option<(&str, &str, Option<&str>, &str)> {
        Some((
            self.oidc_issuer.as_deref()?,
            self.oidc_client_id.as_deref()?,
            self.secrets.get("OIDC_CLIENT_SECRET").map(Secret::as_str),
            self.oidc_redirect_url.as_deref()?,
        ))
    }

    /// `SESSION_SIGNING_KEY`, when configured (keys the share-session cookie MAC).
    pub(super) fn session_signing_key(&self) -> Option<&str> {
        self.secrets.get("SESSION_SIGNING_KEY").map(Secret::as_str)
    }

    pub(super) fn database_url(&self) -> Option<&str> {
        self.secrets.get("DATABASE_URL").map(|secret| secret.0.as_str())
    }

    /// Object-store settings when every part is configured. Values are validated when the client
    /// is built, not here.
    pub(super) fn object_store_settings(&self) -> Option<figcraft_cloud_storage::objects::ObjectStoreSettings> {
        Some(figcraft_cloud_storage::objects::ObjectStoreSettings {
            endpoint: self.s3_endpoint.clone()?,
            region: self.s3_region.clone().unwrap_or_else(|| "us-east-1".to_string()),
            bucket: self.s3_bucket.clone()?,
            access_key: self.secrets.get("S3_ACCESS_KEY")?.as_str().to_string(),
            secret_key: self.secrets.get("S3_SECRET_KEY")?.as_str().to_string(),
            path_style: self.s3_force_path_style,
        })
    }

    fn validate_production(&self) -> Result<(), &'static str> {
        if self.environment != Environment::Production {
            return Ok(());
        }
        let public_url = self.public_url.as_deref().ok_or("production requires FIGCRAFT_PUBLIC_URL")?;
        if !is_public_https_url(public_url) {
            return Err("production requires a public HTTPS FIGCRAFT_PUBLIC_URL");
        }
        let issuer = self.oidc_issuer.as_deref().ok_or("production requires OIDC_ISSUER")?;
        if !is_public_https_url(issuer) {
            return Err("production requires a public HTTPS OIDC_ISSUER");
        }
        const REQUIRED_MOUNTED_SECRETS: &[&str] =
            &["DATABASE_URL", "REDIS_URL", "S3_ACCESS_KEY", "S3_SECRET_KEY", "OIDC_CLIENT_SECRET", "SESSION_SIGNING_KEY"];
        if REQUIRED_MOUNTED_SECRETS.iter().any(|name| !self.mounted_secrets.contains(name)) {
            return Err("production requires mounted secret files for all core credentials");
        }
        Ok(())
    }
}

fn is_public_https_url(value: &str) -> bool {
    let Some(authority) = value.strip_prefix("https://").map(|rest| rest.split(['/', '?', '#']).next().unwrap_or("")) else {
        return false;
    };
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or("").trim_matches(['[', ']']);
    !host.is_empty()
        && !host.eq_ignore_ascii_case("localhost")
        && !host.ends_with(".localhost")
        && !host.ends_with(".invalid")
        && host != "127.0.0.1"
        && host != "0.0.0.0"
        && host != "::1"
}

fn secret_setting(key: &str) -> Option<(&'static str, bool)> {
    match key {
        "DATABASE_URL" => Some(("DATABASE_URL", false)),
        "DATABASE_URL_FILE" => Some(("DATABASE_URL", true)),
        "REDIS_URL" => Some(("REDIS_URL", false)),
        "REDIS_URL_FILE" => Some(("REDIS_URL", true)),
        "S3_ACCESS_KEY" => Some(("S3_ACCESS_KEY", false)),
        "S3_ACCESS_KEY_FILE" => Some(("S3_ACCESS_KEY", true)),
        "S3_SECRET_KEY" => Some(("S3_SECRET_KEY", false)),
        "S3_SECRET_KEY_FILE" => Some(("S3_SECRET_KEY", true)),
        "OIDC_CLIENT_SECRET" => Some(("OIDC_CLIENT_SECRET", false)),
        "OIDC_CLIENT_SECRET_FILE" => Some(("OIDC_CLIENT_SECRET", true)),
        "SESSION_SIGNING_KEY" => Some(("SESSION_SIGNING_KEY", false)),
        "SESSION_SIGNING_KEY_FILE" => Some(("SESSION_SIGNING_KEY", true)),
        _ => None,
    }
}

/// How long a stopping process drains by default: a few health-check intervals' worth is the
/// operator's to set; this is enough for a gateway that checks every second or two.
const DEFAULT_DRAIN_SECONDS: u64 = 5;
const MAX_DRAIN_SECONDS: u64 = 120;
/// An emptied room stays open this long, so a reconnecting client finds it warm.
const DEFAULT_ROOM_IDLE_SECONDS: u64 = 30;
const DEFAULT_SNAPSHOT_INTERVAL_SECONDS: u64 = 300;
const DEFAULT_SNAPSHOT_AFTER_EVENTS: u64 = 1000;

/// The relay's quotas: every one is bounded, and the defaults are what the relay used before they
/// were configurable (S5-017).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RelayQuotas {
    /// Documents open at once on this replica; past it a new document is refused (and retried
    /// elsewhere or later).
    pub max_rooms: usize,
    /// Connections at once on this replica, all rooms together.
    pub max_connections: usize,
    /// Clients in one room; the next hello is refused as a full session.
    pub max_peers_per_room: usize,
    /// Messages per second one connection may send (a burst of four times it is allowed); past it
    /// the connection is dropped.
    pub messages_per_second: u64,
    /// The largest message a client may send.
    pub max_message_bytes: usize,
    /// Outbound bytes queued for one connection; a reader further behind than this is dropped.
    pub max_queued_bytes: usize,
}

impl Default for RelayQuotas {
    fn default() -> Self {
        Self {
            max_rooms: 1024,
            max_connections: 1024,
            max_peers_per_room: 256,
            messages_per_second: 100,
            max_message_bytes: 64 << 20,
            max_queued_bytes: 160 << 20,
        }
    }
}

/// `value` as a whole number within `min..=max`, or `error`.
fn bounded(value: &str, min: u64, max: u64, error: &'static str) -> Result<u64, &'static str> {
    match value.parse::<u64>() {
        Ok(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(error),
    }
}

/// A room lease's endpoint: a `ws://` or `wss://` URL with a host, no credentials, query or fragment.
fn is_relay_advertise_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("wss://").or_else(|| value.strip_prefix("ws://")) else { return false };
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty() && !host.contains('@') && !value.contains(['?', '#', ' '])
}

fn is_cloud_setting(key: &str) -> bool {
    const PREFIXES: &[&str] = &["FIGCRAFT_", "DATABASE_", "REDIS_", "S3_", "OIDC_", "SESSION_", "EMBED_", "OTEL_"];

    is_known_setting(key) || PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

fn is_known_setting(key: &str) -> bool {
    const KNOWN: &[&str] = &[
        "FIGCRAFT_ROLE",
        "FIGCRAFT_ENV",
        "FIGCRAFT_DEPLOYMENT",
        "FIGCRAFT_PUBLIC_URL",
        "FIGCRAFT_RELAY_ADVERTISE_URL",
        "FIGCRAFT_DRAIN_SECONDS",
        "FIGCRAFT_RELAY_MAX_ROOMS",
        "FIGCRAFT_RELAY_MAX_CONNECTIONS",
        "FIGCRAFT_RELAY_MAX_PEERS_PER_ROOM",
        "FIGCRAFT_RELAY_MESSAGES_PER_SECOND",
        "FIGCRAFT_RELAY_MAX_MESSAGE_BYTES",
        "FIGCRAFT_RELAY_MAX_QUEUED_BYTES",
        "FIGCRAFT_ROOM_IDLE_SECONDS",
        "FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS",
        "FIGCRAFT_SNAPSHOT_AFTER_EVENTS",
        "FIGCRAFT_SINGLE_TENANT_ID",
        "FIGCRAFT_TRUSTED_PROXY_CIDRS",
        "FIGCRAFT_PASSWORD_MEMORY_KIB",
        "FIGCRAFT_PASSWORD_PASSES",
        "FIGCRAFT_PASSWORD_LANES",
        "FIGCRAFT_PASSWORD_CONCURRENCY",
        "FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD",
        "FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS",
        "FIGCRAFT_IMAGE_MAX_BYTES",
        "FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES",
        "FIGCRAFT_IMAGE_UPLOAD_SECONDS",
        "FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS",
        "DATABASE_URL",
        "DATABASE_URL_FILE",
        "REDIS_URL",
        "REDIS_URL_FILE",
        "S3_ENDPOINT",
        "S3_REGION",
        "S3_BUCKET",
        "S3_ACCESS_KEY",
        "S3_ACCESS_KEY_FILE",
        "S3_SECRET_KEY",
        "S3_SECRET_KEY_FILE",
        "S3_FORCE_PATH_STYLE",
        "OIDC_ISSUER",
        "OIDC_CLIENT_ID",
        "OIDC_REDIRECT_URL",
        "OIDC_CLIENT_SECRET",
        "OIDC_CLIENT_SECRET_FILE",
        "OIDC_AUDIENCE",
        "SESSION_SIGNING_KEY",
        "SESSION_SIGNING_KEY_FILE",
        "EMBED_ISSUER_CONFIG_FILE",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ];
    KNOWN.contains(&key)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use figcraft_cloud_domain::DeploymentMode;

    use super::{CloudConfig, CloudRole};

    #[test]
    fn defaults_are_local_single_tenant_and_all_roles() {
        assert_eq!(CloudConfig::parse([]), Ok(CloudConfig::default()));
    }

    #[test]
    fn relay_advertise_url_is_off_by_default_and_must_be_a_plain_websocket_url() {
        assert_eq!(CloudConfig::default().relay_advertise_url(), None);
        let ok = |url| CloudConfig::parse([("FIGCRAFT_RELAY_ADVERTISE_URL", url)]).map(|c| c.relay_advertise_url().map(str::to_string));
        assert_eq!(ok("ws://relay-1.internal:8082/ws/v1/collab"), Ok(Some("ws://relay-1.internal:8082/ws/v1/collab".to_string())));
        assert_eq!(ok("wss://relay.example.com/ws/v1/collab"), Ok(Some("wss://relay.example.com/ws/v1/collab".to_string())));
        for bad in [
            "http://relay:8082",
            "ws://",
            "ws:///path",
            "ws://user:pass@relay:8082/x",
            "ws://relay:8082/x?token=1",
            "ws://relay:8082/x#frag",
            "relay:8082",
            "ws://re lay/x",
        ] {
            assert!(ok(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn image_limits_have_defaults_and_bounds() {
        use crate::api::images_api::ImageSettings;
        assert_eq!(CloudConfig::default().image_settings(), ImageSettings { max_bytes: 100 << 20, tenant_quota_bytes: 0, upload_seconds: 300 });
        let images = |pairs: &[(&str, &str)]| CloudConfig::parse(pairs.iter().copied()).map(|c| c.image_settings());
        let set = images(&[
            ("FIGCRAFT_IMAGE_MAX_BYTES", "33554432"),
            ("FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES", "10737418240"),
            ("FIGCRAFT_IMAGE_UPLOAD_SECONDS", "120"),
        ]);
        assert_eq!(set, Ok(ImageSettings { max_bytes: 32 << 20, tenant_quota_bytes: 10 << 30, upload_seconds: 120 }));
        for (key, bad) in [
            ("FIGCRAFT_IMAGE_MAX_BYTES", "0"),
            ("FIGCRAFT_IMAGE_MAX_BYTES", "1048575"),
            ("FIGCRAFT_IMAGE_MAX_BYTES", "268435457"),
            ("FIGCRAFT_IMAGE_MAX_BYTES", "big"),
            ("FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES", "-1"),
            ("FIGCRAFT_IMAGE_UPLOAD_SECONDS", "9"),
            ("FIGCRAFT_IMAGE_UPLOAD_SECONDS", "3601"),
        ] {
            assert!(images(&[(key, bad)]).is_err(), "{key}={bad}");
        }
    }

    #[test]
    fn relay_quotas_have_defaults_and_bounds() {
        use super::RelayQuotas;
        assert_eq!(CloudConfig::default().relay_quotas(), &RelayQuotas::default());
        let parse = |k: &str, v: &str| CloudConfig::parse([(k, v)]);
        let q = |k: &str, v: &str| parse(k, v).map(|c| c.relay_quotas().clone());
        assert_eq!(q("FIGCRAFT_RELAY_MAX_ROOMS", "50").map(|q| q.max_rooms), Ok(50));
        assert_eq!(q("FIGCRAFT_RELAY_MAX_CONNECTIONS", "200").map(|q| q.max_connections), Ok(200));
        assert_eq!(q("FIGCRAFT_RELAY_MAX_PEERS_PER_ROOM", "8").map(|q| q.max_peers_per_room), Ok(8));
        assert_eq!(q("FIGCRAFT_RELAY_MESSAGES_PER_SECOND", "30").map(|q| q.messages_per_second), Ok(30));
        assert_eq!(q("FIGCRAFT_RELAY_MAX_MESSAGE_BYTES", "1048576").map(|q| q.max_message_bytes), Ok(1 << 20));
        assert_eq!(q("FIGCRAFT_RELAY_MAX_QUEUED_BYTES", "33554432").map(|q| q.max_queued_bytes), Ok(32 << 20));
        for (key, bad) in [
            ("FIGCRAFT_RELAY_MAX_ROOMS", "0"),
            ("FIGCRAFT_RELAY_MAX_ROOMS", "100001"),
            ("FIGCRAFT_RELAY_MAX_CONNECTIONS", "0"),
            ("FIGCRAFT_RELAY_MAX_PEERS_PER_ROOM", "10001"),
            ("FIGCRAFT_RELAY_MESSAGES_PER_SECOND", "0"),
            ("FIGCRAFT_RELAY_MESSAGES_PER_SECOND", "10001"),
            ("FIGCRAFT_RELAY_MAX_MESSAGE_BYTES", "1023"),
            ("FIGCRAFT_RELAY_MAX_MESSAGE_BYTES", "67108865"),
            ("FIGCRAFT_RELAY_MAX_QUEUED_BYTES", "1048575"),
            ("FIGCRAFT_RELAY_MAX_QUEUED_BYTES", "lots"),
        ] {
            assert!(parse(key, bad).is_err(), "{key}={bad} must be refused");
        }
    }

    #[test]
    fn room_idle_and_snapshot_schedule_have_defaults_and_bounds() {
        use std::time::Duration;
        let c = CloudConfig::default();
        assert_eq!((c.room_idle(), c.snapshot_interval(), c.snapshot_after_events()), (Duration::from_secs(30), Duration::from_secs(300), 1000));
        let parse = |k: &str, v: &str| CloudConfig::parse([(k, v)]);
        assert_eq!(parse("FIGCRAFT_ROOM_IDLE_SECONDS", "0").map(|c| c.room_idle()), Ok(Duration::ZERO));
        assert_eq!(parse("FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS", "60").map(|c| c.snapshot_interval()), Ok(Duration::from_secs(60)));
        assert_eq!(parse("FIGCRAFT_SNAPSHOT_AFTER_EVENTS", "50").map(|c| c.snapshot_after_events()), Ok(50));
        for (key, bad) in [
            ("FIGCRAFT_ROOM_IDLE_SECONDS", "3601"),
            ("FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS", "9"),
            ("FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS", "86401"),
            ("FIGCRAFT_SNAPSHOT_AFTER_EVENTS", "0"),
            ("FIGCRAFT_SNAPSHOT_AFTER_EVENTS", "1000001"),
            ("FIGCRAFT_SNAPSHOT_AFTER_EVENTS", "lots"),
        ] {
            assert!(parse(key, bad).is_err(), "{key}={bad} must be refused");
        }
    }

    #[test]
    fn the_drain_period_defaults_to_five_seconds_and_is_bounded() {
        use std::time::Duration;
        assert_eq!(CloudConfig::default().drain_period(), Duration::from_secs(5));
        let period = |v| CloudConfig::parse([("FIGCRAFT_DRAIN_SECONDS", v)]).map(|c| c.drain_period());
        assert_eq!(period("0"), Ok(Duration::ZERO));
        assert_eq!(period("120"), Ok(Duration::from_secs(120)));
        for bad in ["121", "-1", "5s", "1.5", "many"] {
            assert!(period(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn password_settings_are_bounded() {
        use figcraft_cloud_auth::password::HashParams;
        assert_eq!(CloudConfig::default().password_params(), HashParams::default());
        let config = CloudConfig::parse([
            ("FIGCRAFT_PASSWORD_MEMORY_KIB", "65536"),
            ("FIGCRAFT_PASSWORD_PASSES", "3"),
            ("FIGCRAFT_PASSWORD_LANES", "2"),
            ("FIGCRAFT_PASSWORD_CONCURRENCY", "8"),
        ])
        .unwrap();
        assert_eq!(config.password_params(), HashParams { memory_kib: 65536, passes: 3, lanes: 2 });
        assert_eq!(config.password_concurrency(), 8);
        for (key, bad) in [
            ("FIGCRAFT_PASSWORD_MEMORY_KIB", "1024"),
            ("FIGCRAFT_PASSWORD_MEMORY_KIB", "2097152"),
            ("FIGCRAFT_PASSWORD_PASSES", "0"),
            ("FIGCRAFT_PASSWORD_PASSES", "11"),
            ("FIGCRAFT_PASSWORD_LANES", "9"),
            ("FIGCRAFT_PASSWORD_CONCURRENCY", "0"),
            ("FIGCRAFT_PASSWORD_CONCURRENCY", "65"),
            ("FIGCRAFT_PASSWORD_PASSES", "two"),
        ] {
            assert!(CloudConfig::parse([(key, bad)]).is_err(), "{key}={bad}");
        }
    }

    #[test]
    fn lockout_settings_are_bounded_and_consistent() {
        use figcraft_cloud_storage::credentials::LockoutPolicy;
        assert_eq!(CloudConfig::default().login_lockout(), LockoutPolicy::default());
        let config = CloudConfig::parse([
            ("FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD", "3"),
            ("FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS", "10"),
            ("FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS", "600"),
        ])
        .unwrap();
        assert_eq!(config.login_lockout(), LockoutPolicy { threshold: 3, base_secs: 10, max_secs: 600 });
        for (key, bad) in [
            ("FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD", "0"),
            ("FIGCRAFT_LOGIN_LOCKOUT_THRESHOLD", "101"),
            ("FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS", "0"),
            ("FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS", "86401"),
            ("FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS", "soon"),
        ] {
            assert!(CloudConfig::parse([(key, bad)]).is_err(), "{key}={bad}");
        }
        // The first lock may not be longer than the longest.
        assert!(CloudConfig::parse([("FIGCRAFT_LOGIN_LOCKOUT_BASE_SECONDS", "1000"), ("FIGCRAFT_LOGIN_LOCKOUT_MAX_SECONDS", "10")]).is_err());
    }

    #[test]
    fn trusted_proxies_default_to_none_and_must_be_cidrs() {
        assert!(CloudConfig::default().trusted_proxies.is_empty());
        let config = CloudConfig::parse([("FIGCRAFT_TRUSTED_PROXY_CIDRS", "10.0.0.0/8, fd00::/8")]).unwrap();
        assert_eq!(config.trusted_proxies.len(), 2);
        assert!(CloudConfig::parse([("FIGCRAFT_TRUSTED_PROXY_CIDRS", "10.0.0.0/8,gateway")]).is_err());
    }

    #[test]
    fn object_store_settings_need_endpoint_bucket_and_both_keys() {
        let full = [
            ("S3_ENDPOINT", "http://seaweedfs:8333"),
            ("S3_BUCKET", "figcraft"),
            ("S3_ACCESS_KEY", "FCkey"),
            ("S3_SECRET_KEY", "secret"),
            ("S3_FORCE_PATH_STYLE", "true"),
        ];
        let settings = CloudConfig::parse(full).unwrap().object_store_settings().unwrap();
        assert_eq!(
            (settings.endpoint.as_str(), settings.region.as_str(), settings.bucket.as_str()),
            ("http://seaweedfs:8333", "us-east-1", "figcraft")
        );
        assert!(settings.path_style);
        for missing in 0..4 {
            let partial: Vec<_> = full.iter().enumerate().filter(|(index, _)| *index != missing).map(|(_, setting)| *setting).collect();
            assert_eq!(CloudConfig::parse(partial).unwrap().object_store_settings(), None);
        }
        assert!(CloudConfig::parse([("S3_FORCE_PATH_STYLE", "yes")]).is_err());
        assert!(!CloudConfig::parse([("S3_FORCE_PATH_STYLE", "false")]).unwrap().s3_force_path_style);
    }

    #[test]
    fn parses_role_and_deployment_modes() {
        for (role, parsed_role) in [("all", CloudRole::All), ("api", CloudRole::Api), ("relay", CloudRole::Relay), ("worker", CloudRole::Worker)] {
            assert_eq!(CloudConfig::parse([("FIGCRAFT_ROLE", role)]).map(|config| config.role), Ok(parsed_role));
        }
        for (mode, parsed_mode) in
            [("single", DeploymentMode::SingleTenant), ("multi", DeploymentMode::MultiTenant), ("embedded", DeploymentMode::Embedded)]
        {
            assert_eq!(CloudConfig::parse([("FIGCRAFT_DEPLOYMENT", mode)]).map(|config| config.deployment), Ok(parsed_mode));
        }
    }

    #[test]
    fn rejects_unknown_namespaced_settings_without_echoing_them() {
        let error = CloudConfig::parse([("FIGCRAFT_UNRECOGNIZED_SECRET", "private-value")]);
        assert_eq!(error, Err("unknown cloud setting"));
    }

    #[test]
    fn rejects_invalid_empty_and_duplicate_settings() {
        assert!(CloudConfig::parse([("FIGCRAFT_ROLE", "operator")]).is_err());
        assert!(CloudConfig::parse([("FIGCRAFT_DEPLOYMENT", "local")]).is_err());
        assert!(CloudConfig::parse([("S3_BUCKET", " ")]).is_err());
        assert!(CloudConfig::parse([("FIGCRAFT_ROLE", "api"), ("FIGCRAFT_ROLE", "worker")]).is_err());
        assert!(CloudConfig::parse([("DATABASE_URL", "inline"), ("DATABASE_URL_FILE", "/missing")]).is_err());
    }

    #[test]
    fn ignores_unrelated_process_environment() {
        assert_eq!(CloudConfig::parse([("PATH", "/usr/bin"), ("RUST_LOG", "info")]), Ok(CloudConfig::default()));
    }

    #[test]
    fn loads_secret_files_and_redacts_debug_output() {
        static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("figcraft-cloud-secret-{}-{sequence}", std::process::id()));
        let result = (|| {
            fs::write(&path, "very-private-value\n").map_err(|_| "test setup failed")?;
            let path_text = path.to_str().ok_or("test path is not UTF-8")?;
            let config = CloudConfig::parse([("OIDC_CLIENT_SECRET_FILE", path_text)])?;
            let debug = format!("{config:?}");
            assert!(debug.contains("[REDACTED]"));
            assert!(!debug.contains("very-private-value"));
            assert_eq!(config.secrets.get("OIDC_CLIENT_SECRET"), Some(&super::Secret("very-private-value".to_string())));
            Ok::<(), &'static str>(())
        })();
        let _ = fs::remove_file(path);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn secret_file_errors_are_non_echoing() {
        let path = "/no/such/figcraft-cloud-secret-file";
        assert_eq!(CloudConfig::parse([("DATABASE_URL_FILE", path)]), Err("unable to read configured secret file"));
    }

    #[test]
    fn production_rejects_missing_or_development_urls_and_inline_secrets() {
        assert_eq!(CloudConfig::parse([("FIGCRAFT_ENV", "production")]), Err("production requires FIGCRAFT_PUBLIC_URL"));
        assert_eq!(
            CloudConfig::parse([
                ("FIGCRAFT_ENV", "production"),
                ("FIGCRAFT_PUBLIC_URL", "http://127.0.0.1:8080"),
                ("OIDC_ISSUER", "https://identity.example.com"),
            ]),
            Err("production requires a public HTTPS FIGCRAFT_PUBLIC_URL")
        );
        assert_eq!(
            CloudConfig::parse([
                ("FIGCRAFT_ENV", "production"),
                ("FIGCRAFT_PUBLIC_URL", "https://design.example.com"),
                ("OIDC_ISSUER", "https://identity.invalid"),
            ]),
            Err("production requires a public HTTPS OIDC_ISSUER")
        );
        assert_eq!(
            CloudConfig::parse([
                ("FIGCRAFT_ENV", "production"),
                ("FIGCRAFT_PUBLIC_URL", "https://design.example.com"),
                ("OIDC_ISSUER", "https://identity.example.com"),
                ("DATABASE_URL", "postgres://developer:developer@localhost/db"),
            ]),
            Err("production requires mounted secret files for all core credentials")
        );
    }

    #[test]
    fn production_accepts_https_origins_and_mounted_core_secrets() {
        static NEXT_FILE: AtomicUsize = AtomicUsize::new(100);
        let sequence = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("figcraft-cloud-prod-secret-{}-{sequence}", std::process::id()));
        let result = (|| {
            fs::write(&path, "mounted-secret").map_err(|_| "test setup failed")?;
            let path_text = path.to_str().ok_or("test path is not UTF-8")?;
            let mut settings = vec![
                ("FIGCRAFT_ENV", "production"),
                ("FIGCRAFT_PUBLIC_URL", "https://design.example.com"),
                ("OIDC_ISSUER", "https://identity.example.com"),
            ];
            settings.extend([
                ("DATABASE_URL_FILE", path_text),
                ("REDIS_URL_FILE", path_text),
                ("S3_ACCESS_KEY_FILE", path_text),
                ("S3_SECRET_KEY_FILE", path_text),
                ("OIDC_CLIENT_SECRET_FILE", path_text),
                ("SESSION_SIGNING_KEY_FILE", path_text),
            ]);
            let config = CloudConfig::parse(settings)?;
            assert_eq!(config.environment, super::Environment::Production);
            Ok::<(), &'static str>(())
        })();
        let _ = fs::remove_file(path);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn oidc_login_settings_need_issuer_client_and_redirect() {
        let full = [
            ("OIDC_ISSUER", "https://id.example.com"),
            ("OIDC_CLIENT_ID", "figcraft"),
            ("OIDC_REDIRECT_URL", "https://d.example.com/api/v1/auth/callback"),
        ];
        let config = CloudConfig::parse(full).unwrap();
        assert_eq!(config.oidc_settings(), Some(("https://id.example.com", "figcraft", None, "https://d.example.com/api/v1/auth/callback")));
        for missing in 0..3 {
            let partial: Vec<_> = full.iter().enumerate().filter(|(index, _)| *index != missing).map(|(_, setting)| *setting).collect();
            assert_eq!(CloudConfig::parse(partial).unwrap().oidc_settings(), None);
        }
        assert!(CloudConfig::parse([("FIGCRAFT_SINGLE_TENANT_ID", "tenant-one")]).is_err());
        let tenant = CloudConfig::parse([("FIGCRAFT_SINGLE_TENANT_ID", "00000000-0000-0000-0000-000000000001")]).unwrap();
        assert_eq!(tenant.single_tenant_id().map(|id| id.to_string()).as_deref(), Some("00000000-0000-0000-0000-000000000001"));
    }

    #[test]
    fn development_environment_keeps_loopback_configuration_allowed() {
        assert!(CloudConfig::parse([("FIGCRAFT_ENV", "development"), ("FIGCRAFT_PUBLIC_URL", "http://127.0.0.1:8080"),]).is_ok());
    }
}
