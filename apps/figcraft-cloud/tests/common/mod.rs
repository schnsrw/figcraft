//! Shared by the process-level tests: the dev stack's settings, the real `figcraft-cloud` binary,
//! the relay's fixed ports, a raw collaboration client and a guard that kills what a test started.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use figcraft_collab::Client;
use figcraft_collab::message::{self, ClientMsg, ServerMsg};
use tungstenite::protocol::WebSocket;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Bytes, Message};

pub const TENANT: &str = "d7000000-0000-4000-8000-000000000071";
pub const HEALTH: ([u8; 4], u16) = ([127, 0, 0, 1], 8081);
pub const RELAY: ([u8; 4], u16) = ([127, 0, 0, 1], 8082);

pub struct Stack {
    pub envs: Vec<(String, String)>,
    pub database_url: String,
}

pub fn stack(drain_seconds: u64) -> Option<Stack> {
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
        ("FIGCRAFT_DRAIN_SECONDS".to_string(), drain_seconds.to_string()),
    ];
    Some(Stack { envs, database_url })
}

/// The binary with only the settings the test gives it: the process refuses unknown `FIGCRAFT_`
/// variables, and the test harness's own `FIGCRAFT_TEST_*` ones are such.
pub fn binary() -> Command {
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
pub fn create_link(stack: &Stack) -> (String, String) {
    let out = binary().envs(stack.envs.clone()).arg("--create-share-link").output().unwrap();
    assert!(out.status.success(), "creating the link failed: {}", String::from_utf8_lossy(&out.stderr));
    let url = String::from_utf8(out.stdout).unwrap();
    let invite = percent_decode(url.trim().split("#join=").nth(1).expect("an editor URL with a join fragment"));
    let query = invite.split('?').nth(1).expect("the invite has a query");
    let value = |key: &str| query.split('&').find_map(|pair| pair.strip_prefix(&format!("{key}="))).unwrap().to_string();
    (value("file"), value("token"))
}

/// `GET path` on the health port: the status line and body.
pub fn get(path: &str) -> Option<(String, String)> {
    let mut stream = TcpStream::connect_timeout(&SocketAddr::from(HEALTH), Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let status = text.lines().next()?.to_string();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

pub fn wait_for(what: &str, limit: Duration, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub struct Raw(pub WebSocket<TcpStream>);

/// What ended a read.
pub type Ended = Option<(CloseCode, String)>;

impl Raw {
    pub fn hello(file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(SocketAddr::from(RELAY)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let (ws, _) = tungstenite::client::client("ws://127.0.0.1:8082/ws/v1/collab", stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "drain probe", ""));
        raw
    }

    /// Like [`hello`](Self::hello), asking to resume as `resume` (a client of an earlier connection).
    pub fn hello_resuming(file: &str, token: &str, resume: message::Resume) -> Raw {
        let stream = TcpStream::connect(SocketAddr::from(RELAY)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let (ws, _) = tungstenite::client::client("ws://127.0.0.1:8082/ws/v1/collab", stream).unwrap();
        let mut raw = Raw(ws);
        let mut hello = Client::hello(file, token, "Raw", "");
        if let ClientMsg::Hello(h) = &mut hello {
            h.resume = Some(resume);
        }
        raw.send(&hello);
        raw
    }

    pub fn send(&mut self, msg: &ClientMsg) {
        self.0.send(Message::Binary(Bytes::from(message::encode(msg).unwrap()))).unwrap();
    }

    pub fn until(&mut self, want: impl Fn(&ServerMsg) -> bool) -> Result<ServerMsg, Ended> {
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

/// Kills the process when the test ends any way at all, so a failed assertion can't leave a
/// relay holding the ports for the next run.
pub struct Killed(pub Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn terminate(child: &Child) {
    let status = Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
    assert!(status.success());
}
