//! A backend that opens a file away from the hub's thread ([`Backend::begin_open`]): a slow
//! restore holds only the hellos for that file, never other rooms.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use figcraft_collab::message::{self, ServerMsg};
use figcraft_collab::{Client, Limits, Server};
use figcraft_doc::Document;
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

use super::{Admission, Backend, Closing, Hosting, Wake};
use crate::net::NetLimits;
use message::Access;

const SLOW: Duration = Duration::from_millis(1500);

type Done = Arc<Mutex<Vec<(String, Result<Server, String>)>>>;
type Log = Arc<Mutex<Vec<String>>>;

/// Files named `slow*` open on a background thread after `SLOW` (`slow-bad` fails); the rest open
/// on the hub's thread at once. Counts how often each path ran.
struct Opens {
    done: Done,
    wake: Option<Wake>,
    background: Log,
    inline: Log,
}

fn session() -> Server {
    let mut doc = Document::new("File");
    doc.repair();
    Server::new(doc, "", Limits::default())
}

impl Backend for Opens {
    fn admit(&mut self, _file: &str, _token: &str) -> Result<Admission, String> {
        Ok(Admission { access: Access::Edit, link: None })
    }

    fn begin_open(&mut self, file: &str) -> Result<bool, String> {
        if file == "refused" {
            return Err("not today".into());
        }
        if !file.starts_with("slow") {
            return Ok(false);
        }
        self.background.lock().unwrap().push(file.to_string());
        let (done, wake, file) = (self.done.clone(), self.wake.clone(), file.to_string());
        std::thread::spawn(move || {
            std::thread::sleep(SLOW);
            let result = if file == "slow-bad" { Err("could not restore".to_string()) } else { Ok(session()) };
            done.lock().unwrap().push((file, result));
            if let Some(w) = wake {
                w.wake();
            }
        });
        Ok(true)
    }

    fn opened(&mut self) -> Vec<(String, Result<Server, String>)> {
        std::mem::take(&mut *self.done.lock().unwrap())
    }

    fn attach(&mut self, wake: Wake) {
        self.wake = Some(wake);
    }

    fn open(&mut self, file: &str) -> Result<Server, String> {
        self.inline.lock().unwrap().push(file.to_string());
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
        let (mut ws, _) = tungstenite::client::client(format!("ws://{addr}/"), s).unwrap();
        ws.send(Message::Binary(Bytes::from(message::encode(&Client::hello(file, "t", "Raw", "")).unwrap()))).unwrap();
        Raw(ws)
    }

    /// Wait for the welcome (true) or the end of the connection with the refusal (false).
    fn welcomed(&mut self) -> Result<(), String> {
        loop {
            match self.0.read() {
                Ok(Message::Binary(b)) => match message::decode(&b, 64 << 20).unwrap() {
                    ServerMsg::Welcome { .. } => return Ok(()),
                    ServerMsg::Refused { reason } => return Err(reason),
                    _ => {}
                },
                Ok(Message::Close(_)) | Err(_) => return Err("closed".into()),
                Ok(_) => {}
            }
        }
    }
}

fn serve() -> (Hosting, SocketAddr, Log, Log) {
    let background = Log::default();
    let inline = Log::default();
    let backend = Opens { done: Done::default(), wake: None, background: background.clone(), inline: inline.clone() };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hosting = Hosting::serve(listener, backend, NetLimits::default()).unwrap();
    let addr = hosting.addr();
    (hosting, addr, background, inline)
}

#[test]
fn a_slow_open_does_not_hold_other_rooms() {
    let (mut hosting, addr, background, inline) = serve();
    let t = Instant::now();
    let mut slow = Raw::hello(addr, "slow-a");
    std::thread::sleep(Duration::from_millis(100));
    let mut fast = Raw::hello(addr, "fast");
    assert_eq!(fast.welcomed(), Ok(()));
    assert!(t.elapsed() < SLOW, "another room was served while the slow one was still opening: {:?}", t.elapsed());
    assert_eq!(slow.welcomed(), Ok(()));
    assert!(t.elapsed() >= SLOW, "the slow room opened only when its open finished");
    assert_eq!(*background.lock().unwrap(), vec!["slow-a".to_string()]);
    assert_eq!(*inline.lock().unwrap(), vec!["fast".to_string()], "backends without background opens keep the old path");
    hosting.stop();
}

#[test]
fn hellos_for_a_file_that_is_opening_share_one_open() {
    let (mut hosting, addr, background, _) = serve();
    let mut a = Raw::hello(addr, "slow-shared");
    let mut b = Raw::hello(addr, "slow-shared");
    let mut c = Raw::hello(addr, "slow-shared");
    for r in [&mut a, &mut b, &mut c] {
        assert_eq!(r.welcomed(), Ok(()));
    }
    assert_eq!(background.lock().unwrap().len(), 1, "one open for three hellos");
    hosting.stop();
}

#[test]
fn a_failed_background_open_refuses_everyone_waiting_for_it() {
    let (mut hosting, addr, _, _) = serve();
    let mut a = Raw::hello(addr, "slow-bad");
    let mut b = Raw::hello(addr, "slow-bad");
    assert_eq!(a.welcomed(), Err("could not restore".into()));
    assert_eq!(b.welcomed(), Err("could not restore".into()));
    // The file can be tried again.
    let mut again = Raw::hello(addr, "slow-bad");
    assert_eq!(again.welcomed(), Err("could not restore".into()));
    hosting.stop();
}

#[test]
fn begin_open_can_refuse_at_once() {
    let (mut hosting, addr, _, _) = serve();
    assert_eq!(Raw::hello(addr, "refused").welcomed(), Err("not today".into()));
    hosting.stop();
}
