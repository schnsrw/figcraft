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
    oidc_issuer: Option<String>,
    mounted_secrets: BTreeSet<&'static str>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            role: CloudRole::All,
            deployment: DeploymentMode::SingleTenant,
            secrets: BTreeMap::new(),
            environment: Environment::Development,
            public_url: None,
            oidc_issuer: None,
            mounted_secrets: BTreeSet::new(),
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
    fn from_file(path: &str) -> Result<Self, &'static str> {
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
                "OIDC_ISSUER" => config.oidc_issuer = Some(value.to_string()),
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

        config.validate_production()?;
        Ok(config)
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
        "FIGCRAFT_SINGLE_TENANT_ID",
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
    fn development_environment_keeps_loopback_configuration_allowed() {
        assert!(CloudConfig::parse([("FIGCRAFT_ENV", "development"), ("FIGCRAFT_PUBLIC_URL", "http://127.0.0.1:8080"),]).is_ok());
    }
}
