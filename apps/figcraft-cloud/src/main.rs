//! Process entry point for FigCraft's cloud application.

mod api;
mod config;
mod health;
mod logging;
mod relay;

use config::{CloudConfig, CloudRole, Secret};
use figcraft_cloud_domain::CloudDocumentId;
use figcraft_cloud_storage::share_links::ShareRole;
use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

const MIN_SUPPORTED_SCHEMA_VERSION: i64 = 20261017000007;
const MAX_SUPPORTED_SCHEMA_VERSION: i64 = 20261018000002;

#[derive(Debug, Eq, PartialEq)]
enum Arguments {
    Help,
    CheckReady,
    Migrate,
    InitObjectStore,
    ProvisionDatabaseRoles,
    PrintBootstrapToken,
    CreateShareLink { document: Option<CloudDocumentId>, role: ShareRole },
    Role(Option<CloudRole>),
}

fn parse_arguments(args: impl IntoIterator<Item = OsString>) -> Result<Arguments, &'static str> {
    let mut args = args.into_iter();
    let mut role = None;

    while let Some(argument) = args.next() {
        let argument = argument.into_string().map_err(|_| "arguments must be valid UTF-8")?;
        if argument == "--help" || argument == "-h" {
            return Ok(Arguments::Help);
        }
        if argument == "--check-ready" {
            return if role.is_none() && args.next().is_none() {
                Ok(Arguments::CheckReady)
            } else {
                Err("--check-ready cannot be combined with other arguments")
            };
        }
        if argument == "--migrate" {
            return if role.is_none() && args.next().is_none() {
                Ok(Arguments::Migrate)
            } else {
                Err("--migrate cannot be combined with other arguments")
            };
        }
        if argument == "--init-object-store" {
            return if role.is_none() && args.next().is_none() {
                Ok(Arguments::InitObjectStore)
            } else {
                Err("--init-object-store cannot be combined with other arguments")
            };
        }
        if argument == "--provision-db-roles" {
            return if role.is_none() && args.next().is_none() {
                Ok(Arguments::ProvisionDatabaseRoles)
            } else {
                Err("--provision-db-roles cannot be combined with other arguments")
            };
        }
        if argument == "--print-bootstrap-token" {
            return if role.is_none() && args.next().is_none() {
                Ok(Arguments::PrintBootstrapToken)
            } else {
                Err("--print-bootstrap-token cannot be combined with other arguments")
            };
        }
        if argument == "--create-share-link" {
            return if role.is_none() { parse_share_link(args) } else { Err("--create-share-link cannot be combined with --role") };
        }
        let value = if argument == "--role" {
            args.next().ok_or("--role requires one of: all, api, relay, worker")?.into_string().map_err(|_| "arguments must be valid UTF-8")?
        } else if let Some(value) = argument.strip_prefix("--role=") {
            value.to_string()
        } else {
            return Err("unknown argument");
        };
        if role.is_some() {
            return Err("--role may be specified only once");
        }
        role = Some(CloudRole::parse(&value).ok_or("invalid role; expected all, api, relay or worker")?);
    }

    Ok(Arguments::Role(role))
}

/// The options after `--create-share-link`: `--document <uuid>` and `--role editor|viewer`, each
/// at most once.
fn parse_share_link(args: impl Iterator<Item = OsString>) -> Result<Arguments, &'static str> {
    let mut args = args;
    let (mut document, mut role) = (None, None);
    while let Some(argument) = args.next() {
        let argument = argument.into_string().map_err(|_| "arguments must be valid UTF-8")?;
        let mut value = || args.next().and_then(|value| value.into_string().ok()).ok_or("--document and --role each need a value");
        match argument.as_str() {
            "--document" if document.is_none() => document = Some(value()?.parse().map_err(|_| "--document must be a document UUID")?),
            "--role" if role.is_none() => role = Some(ShareRole::parse(&value()?).ok_or("--role must be editor or viewer")?),
            _ => return Err("--create-share-link takes only --document <uuid> and --role <editor|viewer>, each once"),
        }
    }
    Ok(Arguments::CreateShareLink { document, role: role.unwrap_or(ShareRole::Editor) })
}

fn run_create_share_link(document: Option<CloudDocumentId>, role: ShareRole) -> ExitCode {
    let environment: Vec<_> = env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();
    let settings = environment.iter().map(|(key, value)| (key.as_str(), value.as_str()));
    let Ok(config) = CloudConfig::parse(settings) else {
        eprintln!("figcraft-cloud: invalid cloud configuration");
        return ExitCode::from(2);
    };
    match relay::create_link(&config, document, role) {
        Ok(url) => {
            println!("{url}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("figcraft-cloud: share link not created: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `--print-bootstrap-token` (S4-003): in single-tenant mode, issue a one-time bootstrap token for
/// `FIGCRAFT_SINGLE_TENANT_ID` (creating the tenant row on a fresh database) and print exactly one
/// line, the token, on stdout. Each run replaces the previous token. Refused once bootstrap is
/// completed (or the tenant already has an active owner) and in multi-tenant mode. The token is
/// written nowhere else: errors go to stderr without it.
fn run_print_bootstrap_token() -> ExitCode {
    let environment: Vec<_> = env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();
    let settings = environment.iter().map(|(key, value)| (key.as_str(), value.as_str()));
    let Ok(config) = CloudConfig::parse(settings) else {
        eprintln!("figcraft-cloud: invalid cloud configuration");
        return ExitCode::from(2);
    };
    if config.deployment != figcraft_cloud_domain::DeploymentMode::SingleTenant {
        eprintln!("figcraft-cloud: --print-bootstrap-token is for single-tenant deployments only");
        return ExitCode::from(2);
    }
    // The same default the API's bootstrap gate and the relay use when it is unset.
    let Some(tenant) = config.single_tenant_id().or_else(|| "00000000-0000-0000-0000-000000000001".parse().ok()) else {
        eprintln!("figcraft-cloud: FIGCRAFT_SINGLE_TENANT_ID is required for --print-bootstrap-token");
        return ExitCode::from(2);
    };
    let Some(database_url) = config.database_url() else {
        eprintln!("figcraft-cloud: DATABASE_URL or DATABASE_URL_FILE is required for --print-bootstrap-token");
        return ExitCode::from(2);
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        eprintln!("figcraft-cloud: unable to initialize the bootstrap runtime");
        return ExitCode::FAILURE;
    };
    let outcome = runtime.block_on(async {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(|_| "database connection failed")?;
        tokio::time::timeout(std::time::Duration::from_secs(30), issue_bootstrap_token(&pool, tenant)).await.map_err(|_| "the database timed out")?
    });
    match outcome {
        Ok(token) => {
            println!("{token}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("figcraft-cloud: no bootstrap token issued: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Make sure `tenant` exists, then issue its bootstrap token ([`figcraft_cloud_storage::bootstrap::ensure_bootstrap`],
/// default lifetime). Refused when bootstrap is already completed.
async fn issue_bootstrap_token(pool: &sqlx::PgPool, tenant: figcraft_cloud_domain::TenantId) -> Result<String, &'static str> {
    use figcraft_cloud_storage::bootstrap::{BootstrapStatus, DEFAULT_BOOTSTRAP_LIFETIME_SECS, ensure_bootstrap};
    const DATABASE: &str = "the database refused the bootstrap";
    let mut transaction = figcraft_cloud_storage::begin_tenant_transaction(pool, tenant).await.map_err(|_| DATABASE)?;
    let slug = format!("tenant-{}", &tenant.as_uuid().simple().to_string()[..12]);
    sqlx::query(
        "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'FigCraft', 'self-hosted', 'local') \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(tenant.to_string())
    .bind(slug)
    .execute(transaction.connection())
    .await
    .map_err(|_| DATABASE)?;
    let status = ensure_bootstrap(&mut transaction, DEFAULT_BOOTSTRAP_LIFETIME_SECS).await.map_err(|_| DATABASE)?;
    let BootstrapStatus::Issued { token, .. } = status else {
        let _ = transaction.rollback().await;
        return Err("bootstrap is already completed (the tenant has an owner)");
    };
    transaction.commit().await.map_err(|_| DATABASE)?;
    Ok(token)
}

fn usage() {
    println!(
        "Usage: figcraft-cloud [--role <all|api|relay|worker>]\n       figcraft-cloud --check-ready\n       figcraft-cloud --migrate\n       figcraft-cloud --init-object-store\n       figcraft-cloud --provision-db-roles\n       figcraft-cloud --print-bootstrap-token\n       figcraft-cloud --create-share-link [--document <uuid>] [--role editor|viewer]\n\nDefaults to --role all."
    );
}

fn run_role_provisioning() -> ExitCode {
    let read_secret = |key| -> Result<Secret, &'static str> {
        let path = env::var(key).map_err(|_| "database role provisioning requires mounted secret files")?;
        Secret::from_file(&path)
    };
    let (Ok(admin_url), Ok(migrator_password), Ok(runtime_password)) =
        (read_secret("DATABASE_ADMIN_URL_FILE"), read_secret("DATABASE_MIGRATOR_PASSWORD_FILE"), read_secret("DATABASE_RUNTIME_PASSWORD_FILE"))
    else {
        eprintln!("figcraft-cloud: database role provisioning requires readable mounted secrets");
        return ExitCode::from(2);
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("figcraft-cloud: unable to initialize database role provisioning runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(figcraft_cloud_storage::provision_database_roles(
        admin_url.as_str(),
        migrator_password.as_str(),
        runtime_password.as_str(),
    )) {
        Ok(()) => {
            println!("figcraft-cloud: database roles provisioned");
            ExitCode::SUCCESS
        }
        Err(figcraft_cloud_storage::ProvisionError::Connect) => {
            eprintln!("figcraft-cloud: database administrator connection failed");
            ExitCode::FAILURE
        }
        Err(figcraft_cloud_storage::ProvisionError::RequiresAdministrator) => {
            eprintln!("figcraft-cloud: database role provisioning requires an administrator");
            ExitCode::FAILURE
        }
        Err(figcraft_cloud_storage::ProvisionError::Apply) => {
            eprintln!("figcraft-cloud: database role provisioning failed");
            ExitCode::FAILURE
        }
    }
}

fn run_migrations() -> ExitCode {
    let environment: Vec<_> = env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();
    let settings = environment.iter().map(|(key, value)| (key.as_str(), value.as_str()));
    let Ok(config) = CloudConfig::parse(settings) else {
        eprintln!("figcraft-cloud: invalid cloud configuration");
        return ExitCode::from(2);
    };
    let Some(database_url) = config.database_url() else {
        eprintln!("figcraft-cloud: DATABASE_URL or DATABASE_URL_FILE is required for migration");
        return ExitCode::from(2);
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("figcraft-cloud: unable to initialize migration runtime");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(figcraft_cloud_storage::migrate_database(database_url)) {
        Ok(()) => {
            println!("figcraft-cloud: migrations applied");
            ExitCode::SUCCESS
        }
        Err(figcraft_cloud_storage::MigrationError::Connect) => {
            eprintln!("figcraft-cloud: database connection failed");
            ExitCode::FAILURE
        }
        Err(figcraft_cloud_storage::MigrationError::Apply) => {
            eprintln!("figcraft-cloud: database migration failed");
            ExitCode::FAILURE
        }
    }
}

/// Create the configured bucket if missing, enable and verify versioning, and apply the lifecycle
/// rules. Explicit only: normal startup never changes the bucket.
fn run_object_store_init() -> ExitCode {
    use figcraft_cloud_storage::bucket_admin::{self, BucketInitError, BucketOutcome, LifecycleOutcome, VersioningOutcome};

    let environment: Vec<_> = env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();
    let settings = environment.iter().map(|(key, value)| (key.as_str(), value.as_str()));
    let Ok(config) = CloudConfig::parse(settings) else {
        eprintln!("figcraft-cloud: invalid cloud configuration");
        return ExitCode::from(2);
    };
    let Some(store) = config.object_store_settings().and_then(|settings| figcraft_cloud_storage::objects::ObjectStore::new(&settings).ok()) else {
        eprintln!("figcraft-cloud: S3_ENDPOINT, S3_BUCKET, S3_ACCESS_KEY and S3_SECRET_KEY must be set and valid for object store initialization");
        return ExitCode::from(2);
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        eprintln!("figcraft-cloud: unable to initialize object store runtime");
        return ExitCode::FAILURE;
    };
    match runtime.block_on(bucket_admin::initialize_bucket(&store, &bucket_admin::LifecyclePolicy::default())) {
        Ok(report) => {
            let bucket = match report.bucket {
                BucketOutcome::Created => "created",
                BucketOutcome::AlreadyExisted => "already existed",
            };
            let versioning = match report.versioning {
                VersioningOutcome::Enabled { .. } => "enabled and verified",
                VersioningOutcome::AlreadyEnabled => "already enabled",
            };
            let lifecycle = match report.lifecycle {
                LifecycleOutcome::Applied => "applied and verified",
                LifecycleOutcome::Replaced => "replaced and verified",
                LifecycleOutcome::AlreadyCorrect => "already correct",
                LifecycleOutcome::Unsupported => "unsupported by this store",
            };
            println!("figcraft-cloud: object store bucket {bucket}; versioning {versioning}; lifecycle {lifecycle}");
            if report.lifecycle == LifecycleOutcome::Unsupported {
                eprintln!("figcraft-cloud: warning: staging uploads expire only through the application sweep on this store");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            let message = match error {
                BucketInitError::InvalidPolicy => "the lifecycle policy is invalid",
                BucketInitError::Unreachable => "the object store is unreachable",
                BucketInitError::Unauthorized => "the object store refused the credentials or permission",
                BucketInitError::BucketOwnedElsewhere => "the bucket name belongs to another account",
                BucketInitError::BucketMissing => "the bucket disappeared during initialization",
                BucketInitError::VersioningNotEnabled => "bucket versioning did not read back as enabled",
                BucketInitError::ForeignLifecycleRules => "the bucket has lifecycle rules FigCraft did not write; remove or rename them first",
                BucketInitError::LifecycleRejected => "the object store rejected the lifecycle configuration",
                BucketInitError::LifecycleNotApplied => "the lifecycle configuration did not read back as written",
                BucketInitError::MalformedResponse => "the object store returned an unexpected response body",
                BucketInitError::Unexpected => "the object store returned an unexpected response",
            };
            eprintln!("figcraft-cloud: object store initialization failed: {message}");
            ExitCode::FAILURE
        }
    }
}

fn schema_is_ready(database_url: Option<&str>) -> bool {
    let Some(database_url) = database_url else {
        return false;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return false;
    };
    runtime.block_on(figcraft_cloud_storage::check_schema_version(database_url, MIN_SUPPORTED_SCHEMA_VERSION, MAX_SUPPORTED_SCHEMA_VERSION)).is_ok()
}

/// One signed `HeadBucket` against the configured store; unconfigured or unusable settings are
/// not ready.
fn object_store_is_ready(settings: Option<figcraft_cloud_storage::objects::ObjectStoreSettings>) -> bool {
    let Some(store) = settings.and_then(|settings| figcraft_cloud_storage::objects::ObjectStore::new(&settings).ok()) else {
        return false;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return false;
    };
    runtime.block_on(store.check_connectivity()).is_ok()
}

fn main() -> ExitCode {
    match parse_arguments(env::args_os().skip(1)) {
        Ok(Arguments::Help) => {
            usage();
            ExitCode::SUCCESS
        }
        Ok(Arguments::CheckReady) => match health::probe_ready() {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) | Err(_) => ExitCode::FAILURE,
        },
        Ok(Arguments::Migrate) => run_migrations(),
        Ok(Arguments::InitObjectStore) => run_object_store_init(),
        Ok(Arguments::ProvisionDatabaseRoles) => run_role_provisioning(),
        Ok(Arguments::PrintBootstrapToken) => run_print_bootstrap_token(),
        Ok(Arguments::CreateShareLink { document, role }) => run_create_share_link(document, role),
        Ok(Arguments::Role(cli_role)) => {
            // The long-running roles log to stderr, filtered by RUST_LOG (the CLI commands don't).
            logging::init();
            let environment: Vec<_> = env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();
            let settings = environment.iter().map(|(key, value)| (key.as_str(), value.as_str()));
            let Ok(mut config) = CloudConfig::parse(settings) else {
                eprintln!("figcraft-cloud: invalid cloud configuration");
                return ExitCode::from(2);
            };
            if let Some(role) = cli_role {
                config.role = role;
            }
            let schema_ready = schema_is_ready(config.database_url());
            if !schema_ready {
                eprintln!("figcraft-cloud: database schema is unavailable or unsupported; readiness will fail");
            }
            let objects_ready = object_store_is_ready(config.object_store_settings());
            if !objects_ready {
                eprintln!("figcraft-cloud: object store is unconfigured, unreachable or refused the bucket check; readiness will fail");
            }
            let ready = schema_ready && objects_ready;
            // Every role serves the collaboration relay for now (the core slice runs one process).
            let relay = if ready {
                match relay::start(&config) {
                    Ok(relay) => {
                        eprintln!("figcraft-cloud: collaboration relay listening on {}", relay.hosting.addr());
                        Some(relay)
                    }
                    Err(error) => {
                        eprintln!("figcraft-cloud: collaboration relay not started: {error}");
                        None
                    }
                }
            } else {
                eprintln!("figcraft-cloud: collaboration relay not started: dependencies are not ready");
                None
            };
            let drain_handle = relay.as_ref().map(|relay| relay.hosting.drain_handle());
            let drain = health::Drain {
                period: config.drain_period(),
                hook: Box::new(move || {
                    if let Some(handle) = &drain_handle {
                        handle.drain();
                    }
                }),
            };
            match health::serve(&config, ready, drain) {
                Ok(()) => {
                    // Told to stop: now the relay commits what's open, snapshots, releases its room
                    // leases and tells connected clients to reconnect elsewhere.
                    if let Some(mut relay) = relay {
                        eprintln!("figcraft-cloud: stopping the collaboration relay");
                        relay.hosting.stop();
                    }
                    ExitCode::SUCCESS
                }
                Err(_) => {
                    eprintln!("figcraft-cloud: unable to start the health endpoint server");
                    ExitCode::FAILURE
                }
            }
        }
        Err(error) => {
            eprintln!("figcraft-cloud: {error}");
            usage();
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Arguments, CloudRole, parse_arguments};

    fn arguments(values: &[&str]) -> Result<Arguments, &'static str> {
        parse_arguments(values.iter().map(OsString::from))
    }

    #[test]
    fn leaves_role_to_configuration_by_default() {
        assert_eq!(arguments(&[]), Ok(Arguments::Role(None)));
    }

    #[test]
    fn accepts_each_cli_role_in_both_forms() {
        for (name, role) in [("all", CloudRole::All), ("api", CloudRole::Api), ("relay", CloudRole::Relay), ("worker", CloudRole::Worker)] {
            assert_eq!(arguments(&["--role", name]), Ok(Arguments::Role(Some(role))));
            let inline = format!("--role={name}");
            assert_eq!(arguments(&[&inline]), Ok(Arguments::Role(Some(role))));
        }
    }

    #[test]
    fn rejects_missing_invalid_duplicate_and_unknown_arguments() {
        assert!(arguments(&["--role"]).is_err());
        assert!(arguments(&["--role", "admin"]).is_err());
        assert!(arguments(&["--role", "api", "--role", "worker"]).is_err());
        assert!(arguments(&["--port", "8080"]).is_err());
    }

    #[test]
    fn help_is_recognized() {
        assert_eq!(arguments(&["--help"]), Ok(Arguments::Help));
        assert_eq!(arguments(&["-h"]), Ok(Arguments::Help));
    }

    #[test]
    fn readiness_probe_is_standalone() {
        assert_eq!(arguments(&["--check-ready"]), Ok(Arguments::CheckReady));
        assert!(arguments(&["--check-ready", "--role", "api"]).is_err());
        assert!(arguments(&["--role", "api", "--check-ready"]).is_err());
    }

    #[test]
    fn migration_command_is_standalone() {
        assert_eq!(arguments(&["--migrate"]), Ok(Arguments::Migrate));
        assert!(arguments(&["--migrate", "--role", "api"]).is_err());
        assert!(arguments(&["--role", "api", "--migrate"]).is_err());
    }

    #[test]
    fn object_store_initialization_command_is_standalone() {
        assert_eq!(arguments(&["--init-object-store"]), Ok(Arguments::InitObjectStore));
        assert!(arguments(&["--init-object-store", "--role", "api"]).is_err());
        assert!(arguments(&["--role", "api", "--init-object-store"]).is_err());
        assert!(arguments(&["--init-object-store", "--migrate"]).is_err());
    }

    #[test]
    fn share_link_command_takes_a_document_and_a_role() {
        use super::ShareRole;
        assert_eq!(arguments(&["--create-share-link"]), Ok(Arguments::CreateShareLink { document: None, role: ShareRole::Editor }));
        let id = "b1300000-0000-4000-8000-000000000001";
        assert_eq!(
            arguments(&["--create-share-link", "--document", id, "--role", "viewer"]),
            Ok(Arguments::CreateShareLink { document: Some(id.parse().unwrap()), role: ShareRole::Viewer })
        );
        assert!(arguments(&["--create-share-link", "--role", "owner"]).is_err());
        assert!(arguments(&["--create-share-link", "--document", "nope"]).is_err());
        assert!(arguments(&["--create-share-link", "--document"]).is_err());
        assert!(arguments(&["--create-share-link", "--role", "viewer", "--role", "editor"]).is_err());
        assert!(arguments(&["--role", "api", "--create-share-link"]).is_err());
        assert!(arguments(&["--create-share-link", "--migrate"]).is_err());
    }

    #[test]
    fn bootstrap_token_command_is_standalone() {
        assert_eq!(arguments(&["--print-bootstrap-token"]), Ok(Arguments::PrintBootstrapToken));
        assert!(arguments(&["--print-bootstrap-token", "--role", "api"]).is_err());
        assert!(arguments(&["--role", "api", "--print-bootstrap-token"]).is_err());
        assert!(arguments(&["--print-bootstrap-token", "--migrate"]).is_err());
    }

    /// Against the dev PostgreSQL (fixture prefix e1; skipped without
    /// FIGCRAFT_TEST_DATABASE_URL_FILE): a fresh tenant is created and gets a token, a second run
    /// supersedes the first, and once bootstrap completes the command refuses.
    #[tokio::test]
    async fn bootstrap_tokens_are_issued_until_bootstrap_completes() -> Result<(), Box<dyn std::error::Error>> {
        let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; bootstrap token probe skipped");
            return Ok(());
        };
        let url = std::fs::read_to_string(file)?;
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(url.trim_end()).await?;
        let mut bytes = *figcraft_cloud_domain::Uuid::new_v4().as_bytes();
        bytes[0] = 0xe1;
        let tenant = figcraft_cloud_domain::TenantId::try_from(figcraft_cloud_domain::Uuid::from_bytes(bytes)).map_err(|_| "nil")?;
        let first = super::issue_bootstrap_token(&pool, tenant).await?;
        assert_eq!(first.len(), 43);
        let second = super::issue_bootstrap_token(&pool, tenant).await?;
        assert_ne!(first, second);
        bytes[1] ^= 0xff;
        let owner = figcraft_cloud_domain::PrincipalId::try_from(figcraft_cloud_domain::Uuid::from_bytes(bytes)).map_err(|_| "nil")?;
        sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e1 bootstrap owner')")
            .bind(owner.to_string())
            .execute(&pool)
            .await?;
        let mut transaction = figcraft_cloud_storage::begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        // The superseded token no longer completes bootstrap; the latest one does.
        assert!(figcraft_cloud_storage::bootstrap::complete_bootstrap(&mut transaction, &first, owner).await.is_err());
        transaction.rollback().await?;
        let mut transaction = figcraft_cloud_storage::begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        figcraft_cloud_storage::bootstrap::complete_bootstrap(&mut transaction, &second, owner).await?;
        transaction.commit().await?;
        assert!(super::issue_bootstrap_token(&pool, tenant).await.is_err(), "refused once bootstrap is completed");
        Ok(())
    }

    #[test]
    fn role_provisioning_command_is_standalone() {
        assert_eq!(arguments(&["--provision-db-roles"]), Ok(Arguments::ProvisionDatabaseRoles));
        assert!(arguments(&["--provision-db-roles", "--role", "api"]).is_err());
        assert!(arguments(&["--role", "api", "--provision-db-roles"]).is_err());
    }
}
