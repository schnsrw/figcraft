//! S5-012: the real `figcraft-cloud` process told to stop (SIGTERM) drains: it stops being ready,
//! turns new relay connections away with the restarting code, keeps the sessions it has until the
//! drain period is over, then commits, releases its room leases, tells connected clients to
//! reconnect and exits cleanly. Against the dev PostgreSQL and S3 (fixture tenant d7; skipped
//! without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR, or when ports 8081/8082 are
//! taken: the process binds them).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::room_leases::current_room;
use figcraft_collab::message::{self, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, Prop};
use sqlx::postgres::PgPoolOptions;
use tungstenite::protocol::WebSocket;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Bytes, Message};

const TENANT: &str = "d7000000-0000-4000-8000-000000000071";
const HEALTH: ([u8; 4], u16) = ([127, 0, 0, 1], 8081);
const RELAY: ([u8; 4], u16) = ([127, 0, 0, 1], 8082);
const DRAIN_SECONDS: u64 = 3;

struct Stack {
    envs: Vec<(String, String)>,
    database_url: String,
}

fn stack() -> Option<Stack> {
    let (Ok(url_file), Ok(s3)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; drain probe skipped");
        return None;
    };
    for port in [HEALTH.1, RELAY.1] {
        if TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_err() {
            eprintln!("port {port} is taken; drain probe skipped");
            return None;
        }
    }
    let read = |name: &str| std::fs::read_to_string(Path::new(&s3).join(name)).unwrap().trim_end().to_string();
    let database_url = std::fs::read_to_string(&url_file).unwrap().trim_end().to_string();
    let envs = vec![
        ("DATABASE_URL_FILE".to_string(), url_file),
        ("S3_ENDPOINT".to_string(), read("s3_endpoint")),
        ("S3_REGION".to_string(), "us-east-1".to_string()),
        ("S3_BUCKET".to_string(), "figcraft".to_string()),
        ("S3_ACCESS_KEY_FILE".to_string(), Path::new(&s3).join("s3_access_key").display().to_string()),
        ("S3_SECRET_KEY_FILE".to_string(), Path::new(&s3).join("s3_secret_key").display().to_string()),
        ("S3_FORCE_PATH_STYLE".to_string(), "true".to_string()),
        ("FIGCRAFT_ENV".to_string(), "development".to_string()),
        ("FIGCRAFT_SINGLE_TENANT_ID".to_string(), TENANT.to_string()),
        ("FIGCRAFT_PUBLIC_URL".to_string(), "http://127.0.0.1:8080".to_string()),
        ("FIGCRAFT_RELAY_ADVERTISE_URL".to_string(), "ws://127.0.0.1:8082/ws/v1/collab".to_string()),
        ("FIGCRAFT_DRAIN_SECONDS".to_string(), DRAIN_SECONDS.to_string()),
    ];
    Some(Stack { envs, database_url })
}

/// The binary with only the settings the test gives it: the process refuses unknown `FIGCRAFT_`
/// variables, and the test harness's own `FIGCRAFT_TEST_*` ones are such.
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_figcraft-cloud"));
    for key in ["FIGCRAFT_TEST_DATABASE_URL_FILE", "FIGCRAFT_TEST_S3_DIR", "FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE"] {
        command.env_remove(key);
    }
    command
}

/// `%XX` decoding of the invite inside the editor URL's fragment.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap()
}

/// A new document and editor link through the binary's own CLI: (document, token).
fn create_link(stack: &Stack) -> (String, String) {
    let out = binary().envs(stack.envs.clone()).arg("--create-share-link").output().unwrap();
    assert!(out.status.success(), "creating the link failed: {}", String::from_utf8_lossy(&out.stderr));
    let url = String::from_utf8(out.stdout).unwrap();
    let invite = percent_decode(url.trim().split("#join=").nth(1).expect("an editor URL with a join fragment"));
    let query = invite.split('?').nth(1).expect("the invite has a query");
    let value = |key: &str| query.split('&').find_map(|pair| pair.strip_prefix(&format!("{key}="))).unwrap().to_string();
    (value("file"), value("token"))
}

/// `GET path` on the health port: the status line and body.
fn get(path: &str) -> Option<(String, String)> {
    let mut stream = TcpStream::connect_timeout(&SocketAddr::from(HEALTH), Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let status = text.lines().next()?.to_string();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

fn wait_for(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Raw(WebSocket<TcpStream>);

/// What ended a read.
type Ended = Option<(CloseCode, String)>;

impl Raw {
    fn hello(file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(SocketAddr::from(RELAY)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let (ws, _) = tungstenite::client::client("ws://127.0.0.1:8082/ws/v1/collab", stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "drain probe", ""));
        raw
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.0.send(Message::Binary(Bytes::from(message::encode(msg).unwrap()))).unwrap();
    }

    fn until(&mut self, want: impl Fn(&ServerMsg) -> bool) -> Result<ServerMsg, Ended> {
        loop {
            match self.0.read() {
                Ok(Message::Binary(b)) => {
                    let msg = message::decode(&b, 64 << 20).unwrap();
                    if want(&msg) {
                        return Ok(msg);
                    }
                }
                Ok(Message::Close(frame)) => return Err(frame.map(|f| (f.code, f.reason.to_string()))),
                Ok(_) => {}
                Err(_) => return Err(None),
            }
        }
    }
}

fn terminate(child: &Child) {
    let status = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
    assert!(status.success());
}

#[test]
fn a_stopping_process_drains_then_stops_cleanly() {
    let Some(stack) = stack() else { return };
    let (file, token) = create_link(&stack);
    let document: CloudDocumentId = file.parse().unwrap();
    let mut relay = binary().envs(stack.envs.clone()).arg("--role").arg("relay").stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
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

    terminate(&relay);
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
    wait_for("the process to exit", Duration::from_secs(20), || relay.try_wait().unwrap().is_some());
    let status = relay.wait().unwrap();
    assert!(status.success(), "clean exit, got {status:?}");
    assert!(!lease_held(document), "the room's lease was released on the way out");
}
