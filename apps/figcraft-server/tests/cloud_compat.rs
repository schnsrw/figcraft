//! S5-019: a `figcraft-server` (in-process, loopback, a temporary data directory) still serves its
//! sessions to the app's network client exactly as before the cloud relay: a file published with
//! the server key is joined from its `figcraft://join?host=…` link, from the same session as a
//! `ws://` URL and as a `figcraft://join?server=ws://…` link; edits converge; the server restarts
//! mid-session and everyone catches up without losing a confirmed edit. The matrix:
//! `docs/saas/session-compatibility.md`.
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use figcraft_collab_net::guest::GuestConfig;
use figcraft_collab_net::{Endpoint, Invite, NetLink};
use figcraft_engine::Session;
use figcraft_engine::live::{Published, Status};
use figcraft_server::config::Config;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join("figcraft-server-tests").join(format!("cloud-compat-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn quick(mut c: GuestConfig) -> GuestConfig {
    c.backoff = Duration::from_millis(20);
    c.max_backoff = Duration::from_millis(200);
    c.attempts = 100;
    c.limits.hello_timeout = Duration::from_secs(5);
    c
}

fn session(name: &str) -> Session {
    let mut s = Session::new();
    s.live.install(Box::new(NetLink::new(Arc::new(|| {})).with_guest_config(quick)));
    s.execute("pref.userName", &json!({ "name": name })).unwrap();
    s
}

fn live(s: &Session) -> bool {
    s.live().status == Status::Live && s.live().syncing()
}

fn pump(sessions: &mut [&mut Session], what: &str, f: impl Fn(&[&mut Session]) -> bool) {
    let start = Instant::now();
    loop {
        for s in sessions.iter_mut() {
            s.live_poll();
            s.live_sync();
        }
        if f(sessions) {
            return;
        }
        let states: Vec<String> = sessions.iter().map(|s| s.live_summary().to_string()).collect();
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}: {states:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn same(a: &figcraft_doc::Document, b: &figcraft_doc::Document) -> bool {
    figcraft_collab::diff(&figcraft_collab::Replica::new(a.clone()), b).is_ok_and(|d| d.changes.is_empty())
}

fn key(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(figcraft_server::store::KEY)).unwrap().trim().to_string()
}

fn publish(a: &mut Session, dir: &Path, addr: SocketAddr) -> Published {
    a.execute("collab.publish", &json!({ "server": format!("ws://{addr}/?key={}", key(dir)), "name": "Team file" })).unwrap();
    pump(&mut [a], "the publish", |s| live(s[0]) && s[0].live().published.is_some());
    a.live().published.clone().unwrap()
}

fn join(link: &str, name: &str) -> Session {
    let mut s = session(name);
    s.execute("collab.join", &json!({ "link": link })).unwrap();
    s
}

fn id_of(v: &Value) -> figcraft_doc::NodeId {
    figcraft_doc::NodeId(v["id"].as_u64().unwrap())
}

fn start_again(cfg: &Config) -> figcraft_server::Running {
    let start = Instant::now();
    loop {
        match figcraft_server::start(cfg) {
            Ok(s) => return s,
            Err(e) if start.elapsed() < Duration::from_secs(10) && e.contains("can't listen") => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn a_figcraft_server_session_joins_from_every_link_shape_and_survives_a_restart() {
    let dir = TempDir::new();
    let mut cfg = Config { listen: "127.0.0.1:0".into(), data: dir.0.clone(), ..Config::default() };
    let mut server = figcraft_server::start(&cfg).unwrap();
    let addr = server.addr();
    let mut ada = session("Ada");
    ada.execute("shape.rectangle", &json!({ "x": 0, "y": 0, "width": 80, "height": 80 })).unwrap();
    let p = publish(&mut ada, &dir.0, addr);

    // The server's links keep their short desktop form (a plain ws:// endpoint at /).
    assert!(p.edit.starts_with(&format!("figcraft://join?host={addr}&file=")), "{}", p.edit);
    let edit = Invite::parse(&p.edit).unwrap();
    assert_eq!(edit.server, Endpoint::plain(addr.to_string()));
    // The same session as a ws:// URL, and as a server= link.
    let as_url = edit.url();
    let as_server = format!("figcraft://join?server=ws://{addr}/&file={}&token={}", edit.file, edit.token);
    assert_eq!(Invite::parse(&as_url).unwrap(), edit);
    assert_eq!(Invite::parse(&as_server).unwrap(), edit);

    let mut bo = join(&p.edit, "Bo");
    let mut cy = join(&as_url, "Cy");
    let mut di = join(&as_server, "Di");
    pump(&mut [&mut ada, &mut bo, &mut cy, &mut di], "everyone in", |s| s.iter().all(|x| live(x)) && s[0].live().peers.len() == 3);
    for s in [&bo, &cy, &di] {
        assert_eq!(s.live_summary()["access"], "edit");
        assert!(same(ada.doc().unwrap(), s.doc().unwrap()));
    }

    // Edits from each converge.
    let rect = id_of(&bo.execute("shape.rectangle", &json!({ "x": 100, "y": 0, "width": 40, "height": 40 })).unwrap());
    pump(&mut [&mut ada, &mut bo, &mut cy, &mut di], "Bo's rectangle everywhere", |s| s.iter().all(|x| x.doc().unwrap().contains(rect)));
    cy.execute("node.set", &json!({ "ids": [rect.0], "props": { "name": "From the URL" } })).unwrap();
    pump(&mut [&mut ada, &mut bo, &mut cy, &mut di], "Cy's rename everywhere", |s| {
        s.iter().all(|x| x.doc().unwrap().node(rect).is_some_and(|n| n.name == "From the URL"))
    });

    // A restart mid-session: everyone reconnects and catches up, nobody loses a confirmed edit.
    let me = (ada.live().info.as_ref().unwrap().me, cy.live().info.as_ref().unwrap().me);
    server.stop();
    drop(server);
    pump(&mut [&mut ada, &mut bo, &mut cy, &mut di], "the drop", |s| s.iter().all(|x| matches!(x.live().status, Status::Reconnecting { .. })));
    let offline = id_of(&di.execute("shape.ellipse", &json!({ "x": 200, "y": 0, "width": 30, "height": 30 })).unwrap());
    cfg.listen = addr.to_string();
    let mut server = start_again(&cfg);
    pump(&mut [&mut ada, &mut bo, &mut cy, &mut di], "everyone back with everything", |s| {
        s.iter().all(|x| live(x) && x.doc().unwrap().contains(offline) && x.doc().unwrap().node(rect).is_some_and(|n| n.name == "From the URL"))
    });
    assert_eq!((ada.live().info.as_ref().unwrap().me, cy.live().info.as_ref().unwrap().me), me, "resumed as themselves");
    assert!(
        same(ada.doc().unwrap(), bo.doc().unwrap()) && same(ada.doc().unwrap(), cy.doc().unwrap()) && same(ada.doc().unwrap(), di.doc().unwrap())
    );
    server.stop();
}
