//! S5-012: the real `figcraft-cloud` process told to stop (SIGTERM) drains: it stops being ready,
//! turns new relay connections away with the restarting code, keeps the sessions it has until the
//! drain period is over, then commits, releases its room leases, tells connected clients to
//! reconnect and exits cleanly. Against the dev PostgreSQL and S3 (fixture tenant d7; skipped
//! without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR, or when ports 8081/8082 are
//! taken: the process binds them).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::room_leases::current_room;
use figcraft_collab::message::{self, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Json, Prop};
use sqlx::postgres::PgPoolOptions;
use tungstenite::protocol::frame::coding::CloseCode;

mod common;
use common::*;

const DRAIN_SECONDS: u64 = 3;

#[test]
fn a_stopping_process_drains_then_stops_cleanly() {
    let Some(stack) = stack(DRAIN_SECONDS) else { return };
    let (file, token) = create_link(&stack);
    let document: CloudDocumentId = file.parse().unwrap();
    let mut relay =
        Killed(binary().envs(stack.envs.clone()).arg("--role").arg("relay").stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap());
    wait_for("the process to be ready", Duration::from_secs(60), || get("/readyz").is_some_and(|(status, _)| status.contains("200")));

    // A session in the room, with one confirmed edit.
    let mut inside = Raw::hello(&file, &token);
    let Ok(ServerMsg::Welcome { client_id, seq, catch_up: message::CatchUp::Snapshot(changes), .. }) =
        inside.until(|m| matches!(m, ServerMsg::Welcome { .. }))
    else {
        panic!("no welcome")
    };
    let page = changes
        .iter()
        .find_map(|change| match change {
            Change::Create { id, parent: None, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    inside.send(&ClientMsg::Changes {
        base_seq: seq,
        batch_id: 1,
        changes: vec![Change::Set { id: page, prop: Prop::Name, value: Json::of("before the drain").unwrap() }],
    });
    inside.until(|m| matches!(m, ServerMsg::Changes(a) if a.author == client_id && a.batch_id == 1)).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let pool = runtime.block_on(PgPoolOptions::new().max_connections(1).connect(&stack.database_url)).unwrap();
    let tenant: TenantId = TENANT.parse().unwrap();
    let lease_held = |document: CloudDocumentId| {
        runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&pool, tenant).await.unwrap();
            let lease = current_room(&mut transaction, document).await.unwrap();
            transaction.rollback().await.unwrap();
            lease.is_some()
        })
    };
    assert!(lease_held(document), "the open room is leased");

    terminate(&relay.0);
    let told = Instant::now();
    // Not ready from now on, and says why.
    wait_for("readiness to turn to draining", Duration::from_secs(2), || {
        get("/readyz").is_some_and(|(status, body)| status.contains("503") && body.contains("draining"))
    });
    // New relay connections are turned away with the restarting code, not refused.
    let mut late = Raw::hello(&file, &token);
    match late.until(|m| matches!(m, ServerMsg::Welcome { .. } | ServerMsg::Refused { .. })) {
        Err(Some((CloseCode::Restart, _))) => {}
        other => panic!("a hello during the drain should be sent away to retry, got {other:?}"),
    }
    // The session that was in carries on, and the room is still leased.
    inside.send(&ClientMsg::Ping);
    inside.until(|m| matches!(m, ServerMsg::Pong)).unwrap();
    assert!(lease_held(document), "the lease is held until the relay stops");

    // After the drain period the relay stops: connected clients are told to reconnect...
    match inside.until(|_| false) {
        Err(Some((CloseCode::Restart, _))) => {}
        other => panic!("expected the restarting close, got {other:?}"),
    }
    assert!(told.elapsed() >= Duration::from_secs(DRAIN_SECONDS), "the drain period was honoured: {:?}", told.elapsed());
    // ...the process exits cleanly, having released the room's lease.
    wait_for("the process to exit", Duration::from_secs(20), || relay.0.try_wait().unwrap().is_some());
    let status = relay.0.wait().unwrap();
    assert!(status.success(), "clean exit, got {status:?}");
    assert!(!lease_held(document), "the room's lease was released on the way out");
    // The relay's own log lines (not just the process's messages) reach stderr, filtered by RUST_LOG.
    let mut log = String::new();
    relay.0.stderr.take().unwrap().read_to_string(&mut log).unwrap();
    assert!(log.contains(&format!("figcraft_cloud_relay::collab_backend: document {file} opened at seq")), "relay log lines are written:\n{log}");
    assert!(log.contains("stopping the collaboration relay"), "{log}");
}
