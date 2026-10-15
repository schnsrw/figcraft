use std::collections::BTreeSet;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CloudConfig {
    pub(super) role: CloudRole,
    pub(super) deployment: DeploymentMode,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self { role: CloudRole::All, deployment: DeploymentMode::SingleTenant }
    }
}

impl CloudConfig {
    /// Parse cloud-specific settings. Unrelated process environment is intentionally ignored.
    /// Values for settings whose implementation is pending are validated but never retained.
    pub(super) fn parse<'a>(settings: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self, &'static str> {
        let mut config = Self::default();
        let mut seen = BTreeSet::new();

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
                _ => {}
            }
        }

        Ok(config)
    }
}

fn is_cloud_setting(key: &str) -> bool {
    const PREFIXES: &[&str] = &["FIGCRAFT_", "DATABASE_", "REDIS_", "S3_", "OIDC_", "SESSION_", "EMBED_", "OTEL_"];

    is_known_setting(key) || PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

fn is_known_setting(key: &str) -> bool {
    const KNOWN: &[&str] = &[
        "FIGCRAFT_ROLE",
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
    }

    #[test]
    fn ignores_unrelated_process_environment() {
        assert_eq!(CloudConfig::parse([("PATH", "/usr/bin"), ("RUST_LOG", "info")]), Ok(CloudConfig::default()));
    }
}
