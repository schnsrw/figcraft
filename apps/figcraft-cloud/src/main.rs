//! Process entry point for FigCraft's cloud application.

mod config;
mod health;

use config::{CloudConfig, CloudRole, Secret};
use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

const MIN_SUPPORTED_SCHEMA_VERSION: i64 = 20261017000003;
const MAX_SUPPORTED_SCHEMA_VERSION: i64 = 20261017000003;

#[derive(Debug, Eq, PartialEq)]
enum Arguments {
    Help,
    CheckReady,
    Migrate,
    InitObjectStore,
    ProvisionDatabaseRoles,
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

fn usage() {
    println!(
        "Usage: figcraft-cloud [--role <all|api|relay|worker>]\n       figcraft-cloud --check-ready\n       figcraft-cloud --migrate\n       figcraft-cloud --init-object-store\n       figcraft-cloud --provision-db-roles\n\nDefaults to --role all."
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
        Ok(Arguments::Role(cli_role)) => {
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
            match health::serve(config.role, ready) {
                Ok(()) => ExitCode::SUCCESS,
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
    fn role_provisioning_command_is_standalone() {
        assert_eq!(arguments(&["--provision-db-roles"]), Ok(Arguments::ProvisionDatabaseRoles));
        assert!(arguments(&["--provision-db-roles", "--role", "api"]).is_err());
        assert!(arguments(&["--role", "api", "--provision-db-roles"]).is_err());
    }
}
