//! A draining hub takes no new connections (each is told the server is restarting, so it retries
//! elsewhere) while the sessions it has carry on until it stops.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use figcraft_collab::message::{self, Access, ClientMsg, ServerMsg};
use figcraft_collab::{Client, Limits, Server};
use figcraft_doc::Document;
use tungstenite::protocol::WebSocket;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Bytes, Message};

use super::{Admission, Backend, Closing, Hosting, RESTARTING};
use crate::net::NetLimits;

struct Plain;

impl Backend for Plain {
    fn admit(&mut self, _file: &str, _token: &str) -> Result<Admission, String> {
        Ok(Admission { access: Access::Edit, link: None })
    }

    fn open(&mut self, _file: &str) -> Result<Server, String> {
        let mut doc = Document::new("File");
        doc.repair();
        Ok(Server::new(doc, "", Limits::default()))
    }

    fn closing(&self) -> Closing {
        Closing::Restarting
    }
}

struct Raw(WebSocket<TcpStream>);

/// What ended a read: the close code and reason, or nothing (a plain drop).
type Ended = Option<(CloseCode, String)>;

impl Raw {
    fn hello(addr: SocketAddr, file: &str) -> Raw {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/"), s).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, "token", "Raw", ""));
        raw
    }

    fn send(&mut self, m: &ClientMsg) {
        self.0.send(Message::Binary(Bytes::from(message::encode(m).unwrap()))).unwrap();
    }

    /// Read until `want` (Ok) or the connection ends (Err: its close frame).
    fn until(&mut self, want: impl Fn(&ServerMsg) -> bool) -> Result<ServerMsg, Ended> {
        loop {
            match self.0.read() {
                Ok(Message::Binary(b)) => {
                    let m = message::decode(&b, 64 << 20).unwrap();
                    if want(&m) {
                        return Ok(m);
                    }
                }
                Ok(Message::Close(frame)) => return Err(frame.map(|f| (f.code, f.reason.to_string()))),
                Ok(_) => {}
                Err(_) => return Err(None),
            }
        }
    }
}

fn serve() -> (Hosting, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hosting = Hosting::serve(listener, Plain, NetLimits::default()).unwrap();
    let addr = hosting.addr();
    (hosting, addr)
}

#[test]
fn a_draining_hub_turns_new_connections_away_and_keeps_its_sessions() {
    let (mut hosting, addr) = serve();
    let mut inside = Raw::hello(addr, "doc");
    inside.until(|m| matches!(m, ServerMsg::Welcome { .. })).unwrap();

    hosting.drain();
    // A new hello is closed at once with the restarting code and no refusal (a refusal is final
    // for a client; this one retries).
    let started = Instant::now();
    let mut late = Raw::hello(addr, "doc");
    let ended = late.until(|m| matches!(m, ServerMsg::Welcome { .. } | ServerMsg::Refused { .. }));
    match ended {
        Err(Some((CloseCode::Restart, why))) => assert_eq!(why, RESTARTING),
        other => panic!("expected the restarting close, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(5), "turned away at once: {:?}", started.elapsed());

    // The session that was in carries on.
    inside.send(&ClientMsg::Ping);
    inside.until(|m| matches!(m, ServerMsg::Pong)).unwrap();

    // Stopping tells it to reconnect.
    hosting.stop();
    match inside.until(|_| false) {
        Err(Some((CloseCode::Restart, _))) => {}
        other => panic!("expected the restarting close on stop, got {other:?}"),
    }
}

#[test]
fn a_hub_that_is_not_draining_takes_connections_as_before() {
    let (mut hosting, addr) = serve();
    let mut a = Raw::hello(addr, "doc");
    a.until(|m| matches!(m, ServerMsg::Welcome { .. })).unwrap();
    let mut b = Raw::hello(addr, "doc");
    b.until(|m| matches!(m, ServerMsg::Welcome { .. })).unwrap();
    hosting.stop();
}
