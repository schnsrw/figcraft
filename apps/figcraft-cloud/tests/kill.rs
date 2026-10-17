//! S5-022 and S5-023: the real `figcraft-cloud` process killed with SIGKILL (no graceful stop, no
//! lease release) against the dev PostgreSQL and S3 (fixture tenant d7; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR, or when ports 8081/8082 are taken).
//!
//! Before the commit: an edit whose commit cannot finish (the document row is held locked from
//! outside) is never confirmed, and killing the relay then leaves nothing of it in the database
//! or in what the next process serves. After the commit: an edit that was confirmed is in the
//! database and comes back to a client that reconnects to the restarted process, which takes the
//! killed process's still-live room lease over at once.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Stdio;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_collab::message::{self, CatchUp, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Json, NodeId, Prop};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::Message;

mod common;
use common::*;

fn start(stack: &Stack) -> Killed {
    let process = binary().envs(stack.envs.clone()).arg("--role").arg("relay").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let process = Killed(process);
    wait_for("the process to be ready", Duration::from_secs(60), || get("/readyz").is_some_and(|(status, _)| status.contains("200")));
    process
}

/// Join and read the welcome: client id, sequence and the page to edit.
fn join(file: &str, token: &str) -> (Raw, u32, u64, NodeId) {
    let mut raw = Raw::hello(file, token);
    let Ok(ServerMsg::Welcome { client_id, seq, catch_up: CatchUp::Snapshot(changes), .. }) = raw.until(|m| matches!(m, ServerMsg::Welcome { .. }))
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
    (raw, client_id, seq, page)
}

fn rename(raw: &mut Raw, base_seq: u64, batch_id: u64, page: NodeId, name: &str) {
    let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
    raw.send(&ClientMsg::Changes { base_seq, batch_id, changes });
}

fn confirmed(raw: &mut Raw, me: u32, batch_id: u64) -> u64 {
    match raw.until(|m| matches!(m, ServerMsg::Changes(a) if a.author == me && a.batch_id == batch_id)) {
        Ok(ServerMsg::Changes(accepted)) => accepted.seq,
        other => panic!("batch {batch_id} was not confirmed: {other:?}"),
    }
}

struct Db {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    tenant: TenantId,
}

impl Db {
    fn new(stack: &Stack) -> Db {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let pool = runtime.block_on(PgPoolOptions::new().max_connections(2).connect(&stack.database_url)).unwrap();
        Db { runtime, pool, tenant: TENANT.parse().unwrap() }
    }

    /// The document's committed sequence.
    fn committed_seq(&self, document: CloudDocumentId) -> i64 {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            let seq = sqlx::query_scalar("SELECT current_seq FROM documents WHERE id = $1::uuid")
                .bind(document.to_string())
                .fetch_one(transaction.connection())
                .await
                .unwrap();
            transaction.rollback().await.unwrap();
            seq
        })
    }

    /// The number of events stored for the document.
    fn events(&self, document: CloudDocumentId) -> i64 {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            let n = sqlx::query_scalar("SELECT count(*) FROM document_events WHERE document_id = $1::uuid")
                .bind(document.to_string())
                .fetch_one(transaction.connection())
                .await
                .unwrap();
            transaction.rollback().await.unwrap();
            n
        })
    }

    /// Hold the document's row locked, as a slow or stuck commit would: the relay's append (which
    /// locks the same row) cannot finish until [`Held::release`]. A helper thread owns the
    /// transaction for as long as it is held.
    fn lock_document(&self, stack: &Stack, document: CloudDocumentId) -> Held {
        let (url, tenant, doc) = (stack.database_url.clone(), self.tenant, document.to_string());
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
                let mut transaction = begin_tenant_transaction(&pool, tenant).await.unwrap();
                sqlx::query("SELECT id FROM documents WHERE id = $1::uuid FOR UPDATE").bind(doc).fetch_one(transaction.connection()).await.unwrap();
                locked_tx.send(()).unwrap();
                let _ = tokio::task::spawn_blocking(move || release_rx.recv()).await;
                transaction.rollback().await.unwrap();
            });
        });
        locked_rx.recv_timeout(Duration::from_secs(10)).expect("the document row was locked");
        Held { release: release_tx, thread }
    }
}

/// A held row lock.
struct Held {
    release: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl Held {
    fn release(self) {
        let _ = self.release.send(());
        self.thread.join().unwrap();
    }
}

#[test]
fn a_killed_relay_never_confirmed_an_uncommitted_edit_and_loses_no_confirmed_one() {
    let Some(stack) = stack(1) else { return };
    let db = Db::new(&stack);

    // ---- S5-022: killed before the commit. ----
    let (file, token) = create_link(&stack);
    let document: CloudDocumentId = file.parse().unwrap();
    let mut first = start(&stack);
    let (mut client, me, seq, page) = join(&file, &token);
    rename(&mut client, seq, 1, page, "committed edit");
    assert_eq!(confirmed(&mut client, me, 1), 1);
    assert_eq!((db.committed_seq(document), db.events(document)), (1, 1));

    // The commit of the next edit cannot finish: its document row is held from outside.
    let held = db.lock_document(&stack, document);
    rename(&mut client, 1, 2, page, "never committed");
    client.0.get_ref().set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
    let window = Instant::now();
    while window.elapsed() < Duration::from_millis(1800) {
        match client.0.read() {
            Ok(Message::Binary(bytes)) => {
                if let Ok(ServerMsg::Changes(accepted)) = message::decode::<ServerMsg>(&bytes, 64 << 20) {
                    assert!(!(accepted.author == me && accepted.batch_id == 2), "an edit whose commit is still pending was confirmed");
                }
            }
            Ok(_) => {}
            Err(_) => break, // read timeout: nothing more arrived
        }
    }
    // The relay dies with the edit accepted in memory but not committed.
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    held.release();
    assert_eq!((db.committed_seq(document), db.events(document)), (1, 1), "nothing of the unconfirmed edit is durable");
    assert!(client.until(|_| false).is_err(), "the client's connection ended with the process");

    // The restarted process serves exactly the committed state: the client reconnects and finds
    // sequence 1, so it knows to send the edit again.
    let mut second = start(&stack);
    let (mut again, me2, seq2, page2) = join(&file, &token);
    assert_eq!(seq2, 1);

    // ---- S5-023: killed after the commit. ----
    rename(&mut again, seq2, 1, page2, "resent edit");
    assert_eq!(confirmed(&mut again, me2, 1), 2);
    rename(&mut again, 2, 2, page2, "one more");
    assert_eq!(confirmed(&mut again, me2, 2), 3);
    // Confirmed means committed: it is in the database already, whatever happens to the process.
    assert_eq!((db.committed_seq(document), db.events(document)), (3, 3));
    second.0.kill().unwrap();
    second.0.wait().unwrap();
    assert!(again.until(|_| false).is_err());

    // The restarted process (same address: it takes over the dead one's still-live room lease at
    // once instead of waiting for it to expire) restores every confirmed edit.
    let started = Instant::now();
    let _third = start(&stack);
    let (mut after, me3, seq3, page3) = join(&file, &token);
    assert!(started.elapsed() < Duration::from_secs(30), "reconnected at once: {:?}", started.elapsed());
    assert_eq!(seq3, 3, "all confirmed edits came back");
    rename(&mut after, seq3, 1, page3, "after the second crash");
    assert_eq!(confirmed(&mut after, me3, 1), 4, "and the room carries on from there");
    assert_eq!((db.committed_seq(document), db.events(document)), (4, 4));
}
