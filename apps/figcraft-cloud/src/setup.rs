//! `figcraft-cloud --setup`: the one command that turns a fresh install into something a team can
//! sign in to (TS-001, S6-051).
//!
//! ```text
//! figcraft-cloud --setup --workspace-name "Acme Design" --owner-login ada@acme.example \
//!     --owner-name "Ada Lovelace" [--password-file -|PATH] [--slug acme] \
//!     [--platform-login root@acme.example] [--no-platform-admin]
//! ```
//!
//! It creates the workspace, its owner (a password account: no identity provider) and the platform
//! admin who installed the system, whatever of that is missing, and prints where to sign in. The
//! password is never an argument: it comes from `--password-file` (`-` is standard input) or, when
//! none is given, is generated and printed once on standard output. Nothing secret reaches a log or
//! an audit event.

use std::ffi::OsString;
use std::io::Read as _;
use std::process::ExitCode;

use figcraft_cloud_auth::password::{PasswordError, PasswordHasher};
use figcraft_cloud_domain::TenantId;
use figcraft_cloud_storage::self_host::{PlatformAdminSpec, SelfHostSpec, SetupError, generate_password, setup_self_hosted, slug_for};

use crate::config::CloudConfig;

/// The workspace id single-tenant installs use when `FIGCRAFT_SINGLE_TENANT_ID` is not set (the
/// same default the API and the relay use).
const DEFAULT_TENANT: &str = "00000000-0000-0000-0000-000000000001";
/// A password file is read up to this size.
const MAX_PASSWORD_FILE_BYTES: u64 = 1024;

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SetupArgs {
    pub(crate) workspace_name: Option<String>,
    pub(crate) slug: Option<String>,
    pub(crate) owner_login: Option<String>,
    pub(crate) owner_name: Option<String>,
    pub(crate) password_file: Option<String>,
    pub(crate) platform_login: Option<String>,
    pub(crate) no_platform_admin: bool,
}

/// The options after `--setup`, each at most once.
pub(crate) fn parse(args: impl Iterator<Item = OsString>) -> Result<SetupArgs, &'static str> {
    let mut args = args;
    let mut parsed = SetupArgs::default();
    while let Some(argument) = args.next() {
        let argument = argument.into_string().map_err(|_| "arguments must be valid UTF-8")?;
        let mut value = || {
            args.next()
                .and_then(|value| value.into_string().ok())
                .filter(|value| !value.is_empty())
                .ok_or("every --setup option except --no-platform-admin needs a value")
        };
        let slot = match argument.as_str() {
            "--workspace-name" => &mut parsed.workspace_name,
            "--slug" => &mut parsed.slug,
            "--owner-login" => &mut parsed.owner_login,
            "--owner-name" => &mut parsed.owner_name,
            "--password-file" => &mut parsed.password_file,
            "--platform-login" => &mut parsed.platform_login,
            "--no-platform-admin" => {
                if parsed.no_platform_admin {
                    return Err("--setup options may be given only once");
                }
                parsed.no_platform_admin = true;
                continue;
            }
            _ => {
                return Err(
                    "--setup takes --workspace-name, --owner-login, --owner-name, --slug, --password-file, --platform-login and --no-platform-admin",
                );
            }
        };
        if slot.is_some() {
            return Err("--setup options may be given only once");
        }
        *slot = Some(value()?);
    }
    if parsed.no_platform_admin && parsed.platform_login.is_some() {
        return Err("--platform-login cannot be combined with --no-platform-admin");
    }
    if parsed.workspace_name.is_none() || parsed.owner_login.is_none() {
        return Err("--setup needs --workspace-name and --owner-login");
    }
    Ok(parsed)
}

fn read_password(source: &str) -> Result<String, &'static str> {
    let mut text = String::new();
    let read = if source == "-" {
        std::io::stdin().take(MAX_PASSWORD_FILE_BYTES).read_to_string(&mut text)
    } else {
        std::fs::File::open(source).and_then(|file| file.take(MAX_PASSWORD_FILE_BYTES).read_to_string(&mut text))
    };
    read.map_err(|_| "the password file could not be read")?;
    // One line; the trailing newline is not part of the password.
    let line = text.lines().next().unwrap_or("").to_owned();
    if line.is_empty() { Err("the password file is empty") } else { Ok(line) }
}

pub(crate) fn run(args: &SetupArgs) -> ExitCode {
    let environment: Vec<_> = std::env::vars_os().filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))).collect();
    let settings = environment.iter().map(|(key, value)| (key.as_str(), value.as_str()));
    let Ok(config) = CloudConfig::parse(settings) else {
        eprintln!("figcraft-cloud: invalid cloud configuration");
        return ExitCode::from(2);
    };
    let Some(database_url) = config.database_url() else {
        eprintln!("figcraft-cloud: DATABASE_URL or DATABASE_URL_FILE is required for --setup");
        return ExitCode::from(2);
    };
    let (Some(name), Some(login)) = (args.workspace_name.as_deref(), args.owner_login.as_deref()) else {
        return ExitCode::from(2);
    };
    let Some(slug) = args.slug.clone().or_else(|| slug_for(name)) else {
        eprintln!("figcraft-cloud: no usable workspace address could be made from the name; pass --slug");
        return ExitCode::from(2);
    };
    let (password, generated) = match args.password_file.as_deref() {
        Some(source) => match read_password(source) {
            Ok(password) => (password, false),
            Err(error) => {
                eprintln!("figcraft-cloud: {error}");
                return ExitCode::from(2);
            }
        },
        None => match generate_password() {
            Some(password) => (password, true),
            None => {
                eprintln!("figcraft-cloud: no secure random numbers are available");
                return ExitCode::FAILURE;
            }
        },
    };
    let owner_name = args.owner_name.as_deref().unwrap_or(login);
    let hasher = match PasswordHasher::new(config.password_params()) {
        Ok(hasher) => hasher,
        Err(_) => {
            eprintln!("figcraft-cloud: the password hashing parameters are invalid");
            return ExitCode::from(2);
        }
    };
    let hash = match hasher.hash(&password, Some(login)) {
        Ok(hash) => hash,
        Err(PasswordError::Policy(violation)) => {
            eprintln!("figcraft-cloud: that password is not acceptable: {}", violation.message());
            return ExitCode::from(2);
        }
        Err(_) => {
            eprintln!("figcraft-cloud: the password could not be hashed");
            return ExitCode::FAILURE;
        }
    };
    let platform_login = args.platform_login.as_deref().unwrap_or(login);
    let spec = SelfHostSpec {
        tenant: config.single_tenant_id().or_else(|| DEFAULT_TENANT.parse::<TenantId>().ok()).unwrap_or_else(TenantId::random),
        slug: &slug,
        name,
        owner_login: login,
        owner_name,
        owner_password_hash: &hash,
        // The installer is also the platform admin; their password hash is the same string.
        platform_admin: (!args.no_platform_admin).then_some(PlatformAdminSpec {
            login: platform_login,
            display_name: owner_name,
            password_hash: &hash,
        }),
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        eprintln!("figcraft-cloud: unable to initialize the setup runtime");
        return ExitCode::FAILURE;
    };
    let outcome = runtime.block_on(async {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(|_| "the database connection failed")?;
        match tokio::time::timeout(std::time::Duration::from_secs(60), setup_self_hosted(&pool, &spec)).await {
            Err(_) => Err("the database timed out"),
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(SetupError::AlreadySetUp)) => Err("this installation is already set up (a workspace owner and a platform admin exist)"),
            Ok(Err(SetupError::Conflict)) => Err("that address or login is already in use"),
            Ok(Err(SetupError::Invalid)) => Err("a name, address or login was not acceptable"),
            Ok(Err(SetupError::Database)) => Err("the database refused the setup (is the schema migrated?)"),
        }
    });
    match outcome {
        Ok(outcome) => {
            let base = config.public_url().unwrap_or("http://localhost:8080").trim_end_matches('/').to_owned();
            println!("FigCraft is set up.");
            println!("Open: {base}/");
            if outcome.workspace_created {
                println!("Workspace: {name}  (address: {slug})");
                println!("Owner sign-in: {login}");
                if generated {
                    println!("Password (shown once; change it after signing in): {password}");
                }
            }
            if outcome.platform_admin_created {
                println!(
                    "Platform admin: {platform_login}  (operator API at {base}/platform/v1; the authenticator app is enrolled at first sign-in)"
                );
                if generated && !outcome.workspace_created {
                    println!("Password (shown once): {password}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("figcraft-cloud: setup failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(values: &[&str]) -> Result<SetupArgs, &'static str> {
        parse(values.iter().map(OsString::from))
    }

    #[test]
    fn setup_options_are_collected_and_required_ones_enforced() {
        let args = parsed(&["--workspace-name", "Acme Design", "--owner-login", "ada@acme.example", "--owner-name", "Ada", "--password-file", "-"])
            .expect("parsed");
        assert_eq!(args.workspace_name.as_deref(), Some("Acme Design"));
        assert_eq!(args.password_file.as_deref(), Some("-"));
        assert!(!args.no_platform_admin);
        assert!(parsed(&["--owner-login", "a@b.c"]).is_err(), "a workspace name is required");
        assert!(parsed(&["--workspace-name", "X"]).is_err(), "an owner login is required");
    }

    #[test]
    fn setup_rejects_repeats_unknown_options_and_conflicts() {
        let base = ["--workspace-name", "X", "--owner-login", "a@b.c"];
        let with = |extra: &[&str]| parsed(&[&base[..], extra].concat());
        assert!(with(&["--workspace-name", "Y"]).is_err());
        assert!(with(&["--password", "secret"]).is_err(), "a password is never an argument");
        assert!(with(&["--slug"]).is_err());
        assert!(with(&["--slug", ""]).is_err());
        assert!(with(&["--no-platform-admin", "--no-platform-admin"]).is_err());
        assert!(with(&["--no-platform-admin", "--platform-login", "r@o.o"]).is_err());
        assert!(with(&["--no-platform-admin"]).is_ok());
    }

    #[test]
    fn the_password_file_is_one_line_and_bounded() {
        let dir = std::env::temp_dir().join(format!("figcraft-setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pw");
        std::fs::write(&path, "first line\nsecond\n").expect("write");
        assert_eq!(read_password(path.to_str().expect("utf8")).as_deref(), Ok("first line"));
        std::fs::write(&path, "\n").expect("write");
        assert!(read_password(path.to_str().expect("utf8")).is_err());
        assert!(read_password(dir.join("missing").to_str().expect("utf8")).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
