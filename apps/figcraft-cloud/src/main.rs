//! Process entry point for FigCraft's cloud application.

mod config;

use config::{CloudConfig, CloudRole};
use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Debug, Eq, PartialEq)]
enum Arguments {
    Help,
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
    println!("Usage: figcraft-cloud [--role <all|api|relay|worker>]\n\nDefaults to --role all.");
}

fn main() -> ExitCode {
    match parse_arguments(env::args_os().skip(1)) {
        Ok(Arguments::Help) => {
            usage();
            ExitCode::SUCCESS
        }
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
            eprintln!("figcraft-cloud: role '{}' selected, but the runtime is not implemented yet", config.role.as_str());
            ExitCode::FAILURE
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
}
