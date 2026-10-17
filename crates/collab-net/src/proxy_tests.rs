//! A hub that passes clients on to the server owning their file's room ([`OpenFailure::Elsewhere`]):
//! messages go both ways, an unreachable owner refuses with the reason, and what the owner ends
//! ends the client's connection.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use figcraft_collab::message::{self, Access, ClientMsg, ServerMsg};
use figcraft_collab::{Client, Limits, Server};
use figcraft_doc::Document;
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

use super::{Admission, Backend, Closing, Hosting, OpenFailure, Wake};
use crate::net::NetLimits;

const REASON: &str = "this document is open on another server: try again in a moment";

type Log = Arc<Mutex<Vec<String>>>;
type Done = Arc<Mutex<Vec<(String, Result<Server, OpenFailure>)>>>;

fn session() -> Server {
    let mut doc = Document::new("File");
    doc.repair();
    Server::new(doc, "", Limits::default())
}

/// A server that opens every file itself (the owner's side), noting which.
struct Owner(Log);

impl Backend for Owner {
    fn admit(&mut self, _file: &str, _token: &str) -> Result<Admission, String> {
        Ok(Admission { access: Access::Edit, link: None })
    }

    fn open(&mut self, file: &str) -> Result<Server, String> {
        self.0.lock().unwrap().push(file.to_string());
        Ok(session())
    }

    fn closing(&self) -> Closing {
        Closing::Restarting
    }
}

/// The front's backend: every file belongs to `owner` (`None`: to nobody reachable).
struct Front {
    owner: Option<String>,
    done: Done,
    wake: Option<Wake>,
    opened_here: Log,
}

impl Backend for Front {
    fn admit(&mut self, _file: &str, _token: &str) -> Result<Admission, String> {
        Ok(Admission { access: Access::Edit, link: None })
    }

    fn begin_open(&mut self, file: &str) -> Result<bool, String> {
        let failure = OpenFailure::Elsewhere { endpoint: self.owner.clone().unwrap_or_else(|| "ws://127.0.0.1:1/".into()), reason: REASON.into() };
        self.done.lock().unwrap().push((file.to_string(), Err(failure)));
        if let Some(w) = &self.wake {
            w.wake();
        }
        Ok(true)
    }

    fn opened(&mut self) -> Vec<(String, Result<Server, OpenFailure>)> {
        std::mem::take(&mut *self.done.lock().unwrap())
    }

    fn attach(&mut self, wake: Wake) {
        self.wake = Some(wake);
    }

    fn open(&mut self, file: &str) -> Result<Server, String> {
        self.opened_here.lock().unwrap().push(file.to_string());
        Ok(session())
    }

    fn closing(&self) -> Closing {
        Closing::Restarting
    }
}

struct Raw(WebSocket<TcpStream>);

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

    fn next(&mut self) -> Option<ServerMsg> {
        loop {
            match self.0.read() {
                Ok(Message::Binary(b)) => return Some(message::decode(&b, 64 << 20).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    fn until(&mut self, what: &str, f: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        loop {
            let m = self.next().unwrap_or_else(|| panic!("the connection ended before {what}"));
            if f(&m) {
                return m;
            }
        }
    }
}

fn serve<B: Backend>(backend: B) -> (Hosting, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hosting = Hosting::serve(listener, backend, NetLimits::default()).unwrap();
    let addr = hosting.addr();
    (hosting, addr)
}

fn front(owner: Option<String>) -> (Hosting, SocketAddr, Log) {
    let opened_here = Log::default();
    let (hosting, addr) = serve(Front { owner, done: Arc::default(), wake: None, opened_here: opened_here.clone() });
    (hosting, addr, opened_here)
}

#[test]
fn a_client_is_passed_on_to_the_owner_and_messages_go_both_ways() {
    let owner_log = Log::default();
    let (mut owner, owner_addr) = serve(Owner(owner_log.clone()));
    let (mut front, front_addr, opened_here) = front(Some(format!("ws://{owner_addr}/ws/v1/collab")));

    let mut a = Raw::hello(front_addr, "doc-1");
    a.until("the welcome", |m| matches!(m, ServerMsg::Welcome { .. }));
    // The owner opened the room, not the front.
    assert_eq!(*owner_log.lock().unwrap(), vec!["doc-1".to_string()]);
    assert!(opened_here.lock().unwrap().is_empty());
    // A message up and its answer down.
    a.send(&ClientMsg::Ping);
    a.until("a pong", |m| matches!(m, ServerMsg::Pong));

    // A second client, straight to the owner, and the proxied one see each other.
    let mut direct = Raw::hello(owner_addr, "doc-1");
    direct.until("the welcome", |m| matches!(m, ServerMsg::Welcome { .. }));
    a.until("the direct client joining", |m| matches!(m, ServerMsg::PeerJoined(p) if p.name == "Raw"));
    assert_eq!(owner_log.lock().unwrap().len(), 1, "one room on the owner for both");

    // Leaving through the proxy ends it on the owner too.
    drop(a);
    direct.until("the proxied client leaving", |m| matches!(m, ServerMsg::PeerLeft { .. }));
    drop(direct);
    front.stop();
    owner.stop();
}

#[test]
fn an_unreachable_owner_refuses_with_the_reason() {
    let (mut front, front_addr, _) = front(None);
    let mut a = Raw::hello(front_addr, "doc-2");
    match a.next() {
        Some(ServerMsg::Refused { reason }) => assert_eq!(reason, REASON),
        other => panic!("expected the refusal, got {other:?}"),
    }
    front.stop();
}

#[test]
fn when_the_owner_stops_the_proxied_client_is_told_to_reconnect() {
    let (mut owner, owner_addr) = serve(Owner(Log::default()));
    let (mut front, front_addr, _) = front(Some(format!("ws://{owner_addr}/ws/v1/collab")));
    let mut a = Raw::hello(front_addr, "doc-3");
    a.until("the welcome", |m| matches!(m, ServerMsg::Welcome { .. }));
    owner.stop();
    // The owner's shutdown reaches the client as its own: the connection ends promptly (not at
    // the read timeout).
    let started = std::time::Instant::now();
    while a.next().is_some() {}
    assert!(started.elapsed() < Duration::from_secs(5), "the client was told at once: {:?}", started.elapsed());
    front.stop();
}
