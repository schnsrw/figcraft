//! Process entry point for FigCraft's cloud application.

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloudRole {
    All,
    Api,
    Relay,
    Worker,
}

impl CloudRole {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "api" => Some(Self::Api),
            "relay" => Some(Self::Relay),
            "worker" => Some(Self::Worker),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Api => "api",
            Self::Relay => "relay",
            Self::Worker => "worker",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Arguments {
    Help,
    Role(CloudRole),
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

    Ok(Arguments::Role(role.unwrap_or(CloudRole::All)))
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
        Ok(Arguments::Role(role)) => {
            eprintln!("figcraft-cloud: role '{}' selected, but the runtime is not implemented yet", role.as_str());
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
    fn defaults_to_all_role() {
        assert_eq!(arguments(&[]), Ok(Arguments::Role(CloudRole::All)));
    }

    #[test]
    fn accepts_each_role_in_both_forms() {
        for (name, role) in [("all", CloudRole::All), ("api", CloudRole::Api), ("relay", CloudRole::Relay), ("worker", CloudRole::Worker)] {
            assert_eq!(arguments(&["--role", name]), Ok(Arguments::Role(role)));
            let inline = format!("--role={name}");
            assert_eq!(arguments(&[&inline]), Ok(Arguments::Role(role)));
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
