//! The hub's housekeeping tick ([`Backend::tick`]): called on its own schedule with every open
//! file's server and client count, and the empty files a backend names are offered to `idle`.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use figcraft_collab::message::{self, Access, ServerMsg};
use figcraft_collab::{Client, Limits, Server};
use figcraft_doc::Document;
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

use super::{Admission, Backend, Closing, Hosting};
use crate::net::NetLimits;

#[derive(Default)]
struct Seen {
    ticks: AtomicUsize,
    /// (file, clients) per tick, last 50.
    rooms: Mutex<Vec<(String, usize)>>,
    opens: AtomicUsize,
    idles: AtomicUsize,
}

/// Keeps an emptied room warm: `idle` says no the first time (the last client just left) and yes
/// when the tick offers the file again.
struct Warm(Arc<Seen>, Mutex<Vec<String>>);

impl Backend for Warm {
    fn admit(&mut self, _file: &str, _token: &str) -> Result<Admission, String> {
        Ok(Admission { access: Access::Edit, link: None })
    }

    fn open(&mut self, _file: &str) -> Result<Server, String> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        let mut doc = Document::new("File");
        doc.repair();
        Ok(Server::new(doc, "", Limits::default()))
    }

    fn idle(&mut self, file: &str, _server: &mut Server) -> bool {
        self.0.idles.fetch_add(1, Ordering::SeqCst);
        let mut waiting = self.1.lock().unwrap();
        if waiting.iter().any(|f| f == file) {
            waiting.retain(|f| f != file);
            true
        } else {
            waiting.push(file.to_string());
            false
        }
    }

    fn tick_every(&self) -> Option<Duration> {
        Some(Duration::from_millis(50))
    }

    fn tick(&mut self, open: Vec<(&str, &mut Server, usize)>) -> Vec<String> {
        self.0.ticks.fetch_add(1, Ordering::SeqCst);
        let mut rooms = self.0.rooms.lock().unwrap();
        rooms.clear();
        rooms.extend(open.iter().map(|(f, _, n)| ((*f).to_string(), *n)));
        // Offer the empty ones to idle (what a backend with a keep-warm period does when it is over).
        let waiting = self.1.lock().unwrap().clone();
        open.iter().filter(|(f, _, n)| *n == 0 && waiting.iter().any(|w| w == f)).map(|(f, _, _)| (*f).to_string()).collect()
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
        raw.0.send(Message::Binary(Bytes::from(message::encode(&Client::hello(file, "t", "Raw", "")).unwrap()))).unwrap();
        raw
    }

    fn welcomed(&mut self) {
        loop {
            match self.0.read() {
                Ok(Message::Binary(b)) => {
                    if matches!(message::decode::<ServerMsg>(&b, 64 << 20).unwrap(), ServerMsg::Welcome { .. }) {
                        return;
                    }
                }
                other => panic!("no welcome: {other:?}"),
            }
        }
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_tick_runs_on_its_schedule_and_closes_the_empty_files_a_backend_names() {
    let seen = Arc::new(Seen::default());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut hosting = Hosting::serve(listener, Warm(seen.clone(), Mutex::default()), NetLimits::default()).unwrap();
    let addr = hosting.addr();

    // Ticks come with no traffic at all.
    wait_for("some ticks", || seen.ticks.load(Ordering::SeqCst) >= 5);

    let mut client = Raw::hello(addr, "doc");
    client.welcomed();
    wait_for("the tick to see the client", || seen.rooms.lock().unwrap().contains(&("doc".to_string(), 1)));
    // With someone in, the room is never offered to idle.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(seen.idles.load(Ordering::SeqCst), 0);

    // The last client leaves: idle says keep it (once), the next tick names it, idle says close.
    drop(client);
    wait_for("the room to be offered to idle twice", || seen.idles.load(Ordering::SeqCst) >= 2);
    wait_for("the room to be gone", || seen.rooms.lock().unwrap().is_empty());
    assert_eq!(seen.opens.load(Ordering::SeqCst), 1);
    // A later client opens it afresh.
    let mut again = Raw::hello(addr, "doc");
    again.welcomed();
    assert_eq!(seen.opens.load(Ordering::SeqCst), 2);
    drop(again);
    hosting.stop();
}
