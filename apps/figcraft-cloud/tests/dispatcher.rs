//! S6-014 wiring: the real process starts the webhook delivery dispatcher in the `worker` and
//! `all` roles (given a database and the signing key its secrets are sealed with), says so when it
//! can't, and stays out of the `api` role. Against the dev PostgreSQL and S3 (skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR, or when ports 8081/8082 are taken).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

mod common;
use common::*;

const KEY: &str = "dispatcher-probe-signing-key-0123456789abcdef";

/// Run the process in `role` until it is ready, stop it with SIGTERM and return what it said on
/// stderr.
fn stderr_of(stack: &Stack, role: &str, with_key: bool) -> String {
    let mut command = binary();
    command.envs(stack.envs.clone());
    if with_key {
        command.env("SESSION_SIGNING_KEY", KEY);
    }
    let mut process = Killed(command.arg("--role").arg(role).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap());
    wait_for("the process to be ready", Duration::from_secs(60), || get("/readyz").is_some_and(|(status, _)| status.contains("200")));
    terminate(&process.0);
    wait_for("the process to exit", Duration::from_secs(20), || process.0.try_wait().unwrap().is_some());
    let mut log = String::new();
    process.0.stderr.take().unwrap().read_to_string(&mut log).unwrap();
    log
}

#[test]
fn the_dispatcher_runs_in_the_worker_and_all_roles_and_says_when_it_cannot() {
    let Some(stack) = stack(0) else { return };
    let started = "webhook delivery dispatcher started";
    assert!(stderr_of(&stack, "worker", true).contains(started), "the worker role delivers webhooks");
    assert!(stderr_of(&stack, "all", true).contains(started), "so does a process that runs every role");
    let api = stderr_of(&stack, "api", true);
    assert!(!api.contains("dispatcher"), "the api role doesn't deliver: {api}");
    let keyless = stderr_of(&stack, "worker", false);
    assert!(keyless.contains("dispatcher not started: it needs DATABASE_URL and SESSION_SIGNING_KEY"), "{keyless}");
    assert!(!keyless.contains(started));
}
