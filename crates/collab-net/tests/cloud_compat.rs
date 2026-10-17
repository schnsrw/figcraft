//! S5-019: what the cloud relay added keeps the self-hosted paths working. A desktop-hosted
//! session ([`Host`]) still takes the app's own client (from a `figcraft://join?host=…` link and
//! from the same session's `ws://` URL), a browser-style client (an `Origin` header, the web
//! build's request path) and an older client that announces no extra `client_version` words; a
//! client of another protocol is refused by name. The three link shapes round-trip through
//! [`Invite`], and a cloud relay link (`ws(s)://…/ws/v1/collab?file=<uuid>&token=<secret>`) is never
//! read as a desktop link. The matrix: `docs/saas/session-compatibility.md`.
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used, clippy::unreachable)]

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use figcraft_collab::message::{self, Access, ClientMsg, HOSTING, Hello, PROTOCOL, SIGNALS, ServerMsg, WIRE_VERSION};
use figcraft_collab::{Change, Client, Json, Limits, Prop};
use figcraft_collab_net::NetLink;
use figcraft_collab_net::guest::GuestConfig;
use figcraft_collab_net::host::{Host, HostConfig};
use figcraft_collab_net::{Endpoint, Inbox, Invite, NetLimits};
use figcraft_doc::Document;
use figcraft_engine::Session;
use figcraft_engine::live::{Event, Status};
use serde_json::json;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

const FILE: &str = "0123456789abcdef";
const TOKEN: &str = "AbCdEfGhIjKlMnOpQrStUv";
const WAIT: Duration = Duration::from_secs(20);

fn limits() -> NetLimits {
    NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }
}

/// A desktop-hosted session on a free loopback port, and the host window's inbox.
fn host() -> (Host, Inbox) {
    let inbox = Inbox::new(Arc::new(|| {}));
    let mut doc = Document::new("Desktop file");
    doc.repair();
    let cfg = HostConfig {
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        any_port_if_taken: false,
        file: FILE.into(),
        token: TOKEN.into(),
        doc,
        name: "Hana".into(),
        limits: limits(),
        core: Limits::default(),
    };
    (Host::start(cfg, inbox.clone()).unwrap(), inbox)
}

/// Wait until the host's inbox has an accepted batch whose changes mention `text`.
fn host_sees(inbox: &Inbox, seen: &mut Vec<Event>, text: &str) {
    let start = Instant::now();
    loop {
        seen.extend(inbox.drain());
        if seen.iter().any(|e| matches!(e, Event::Sync(ServerMsg::Changes(a)) if format!("{:?}", a.changes).contains(text))) {
            return;
        }
        assert!(start.elapsed() < WAIT, "the host never saw {text}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn quick(mut c: GuestConfig) -> GuestConfig {
    c.backoff = Duration::from_millis(20);
    c.max_backoff = Duration::from_millis(200);
    c.attempts = 100;
    c.limits.hello_timeout = Duration::from_secs(5);
    c
}

/// The desktop app's engine session with the network link (the same client the cloud relay and
/// `figcraft-server` sessions use).
fn app(name: &str) -> Session {
    let mut s = Session::new();
    s.live.install(Box::new(NetLink::new(Arc::new(|| {})).with_guest_config(quick)));
    s.execute("pref.userName", &json!({ "name": name })).unwrap();
    s
}

fn live(s: &Session) -> bool {
    s.live().status == Status::Live && s.live().syncing()
}

fn pump(s: &mut Session, what: &str, f: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    loop {
        s.live_poll();
        s.live_sync();
        if f(s) {
            return;
        }
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}: {}", s.live_summary());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A browser-style client: the handshake carries an `Origin`, the request path is the invite's
/// endpoint, and it speaks the protocol with the collaboration core's [`Client`].
struct Browser(WebSocket<TcpStream>);

impl Browser {
    fn connect(invite: &Invite) -> Browser {
        let addr: SocketAddr = invite.server.host.parse().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut request = invite.endpoint().into_client_request().unwrap();
        request.headers_mut().insert("Origin", HeaderValue::from_static("https://figcraft.example"));
        request.headers_mut().insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (compat probe)"));
        Browser(tungstenite::client::client(request, stream).unwrap().0)
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

    fn welcome(&mut self) -> ServerMsg {
        loop {
            match self.next() {
                Some(w @ ServerMsg::Welcome { .. }) => return w,
                Some(ServerMsg::Refused { reason }) => panic!("refused: {reason}"),
                Some(_) => {}
                None => panic!("closed before a welcome"),
            }
        }
    }

    fn refusal(&mut self) -> Option<String> {
        let mut why = None;
        while let Some(m) = self.next() {
            if let ServerMsg::Refused { reason } = m {
                why = Some(reason);
            }
        }
        why
    }
}

fn hello(client_version: &str, protocol: u32) -> ClientMsg {
    ClientMsg::Hello(Hello {
        protocol,
        file: FILE.into(),
        token: TOKEN.into(),
        name: "Browser".into(),
        color: String::new(),
        client_version: client_version.into(),
        resume: None,
    })
}

/// Rename the page from a raw client; its confirmation (the seq) and what else arrived meanwhile.
fn rename(b: &mut Browser, client: &Client, me: u32, base_seq: u64, name: &str) -> (u64, Vec<ServerMsg>) {
    let page = client.document().first_page().unwrap();
    b.send(&ClientMsg::Changes { base_seq, batch_id: 1, changes: vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }] });
    let mut other = Vec::new();
    loop {
        match b.next().expect("closed before the confirmation") {
            ServerMsg::Changes(a) if a.author == me && a.batch_id == 1 => return (a.seq, other),
            ServerMsg::Reject { reason, .. } => panic!("rejected: {reason}"),
            m => other.push(m),
        }
    }
}

#[test]
fn the_desktop_app_joins_a_desktop_host_from_both_link_shapes() {
    let (mut host, inbox) = host();
    let invite = Invite::new(host.addr().to_string(), FILE, TOKEN);
    assert!(invite.link().starts_with("figcraft://join?host=127.0.0.1:"), "a desktop link names host=");
    let mut seen = Vec::new();
    for (link, name) in [(invite.link(), "Link page"), (invite.url(), "Url page")] {
        let mut s = app("Bo");
        s.execute("collab.join", &json!({ "link": link })).unwrap();
        pump(&mut s, "the join", live);
        assert_eq!(s.live_summary()["access"], "edit");
        let made = s.execute("shape.rectangle", &json!({ "x": 0, "y": 0, "width": 40, "height": 40 })).unwrap();
        s.execute("node.set", &json!({ "ids": [made["id"]], "props": { "name": name } })).unwrap();
        pump(&mut s, "the rename sent", |_| true);
        host_sees(&inbox, &mut seen, name);
        s.execute("collab.leave", &json!({})).unwrap();
    }
    host.stop();
}

#[test]
fn a_browser_style_client_joins_a_desktop_host_and_edits() {
    let (mut host, inbox) = host();
    let invite = Invite::parse(&Invite::new(host.addr().to_string(), FILE, TOKEN).url()).unwrap();
    let mut browser = Browser::connect(&invite);
    browser.send(&hello(&message::client_version(), PROTOCOL));
    let welcome = browser.welcome();
    let ServerMsg::Welcome { client_id, seq, access, ref peers, .. } = welcome else { unreachable!() };
    assert_eq!(access, Access::Edit);
    assert!(peers.iter().any(|p| p.name == "Hana"), "the host is in its own session");
    let client = Client::join(welcome.clone()).unwrap();
    let (confirmed, _) = rename(&mut browser, &client, client_id, seq, "From the browser");
    assert_eq!(confirmed, seq + 1);
    let mut seen = Vec::new();
    host_sees(&inbox, &mut seen, "From the browser");
    host.stop();
}

#[test]
fn an_older_client_without_capability_words_still_joins_and_gets_no_new_messages() {
    let (mut host, _inbox) = host();
    let invite = Invite::new(host.addr().to_string(), FILE, TOKEN);
    let mut old = Browser::connect(&invite);
    // A build from before signals and hosting: its version alone.
    old.send(&hello("0.1.0", PROTOCOL));
    let welcome = old.welcome();
    let ServerMsg::Welcome { client_id, seq, .. } = welcome else { unreachable!() };
    let client = Client::join(welcome.clone()).unwrap();
    let (_, other) = rename(&mut old, &client, client_id, seq, "Old build");
    assert!(
        !other.iter().any(|m| matches!(m, ServerMsg::Signal { .. } | ServerMsg::Hosting(_) | ServerMsg::Reported { .. })),
        "messages an old client can't read are never sent to it: {other:?}"
    );
    // This build announces both words, so it does get them.
    let words = message::client_version();
    assert!(words.split_whitespace().any(|w| w == SIGNALS) && words.split_whitespace().any(|w| w == HOSTING));
    host.stop();
}

#[test]
fn another_protocol_or_wire_version_is_refused_not_misread() {
    let (mut host, _inbox) = host();
    let invite = Invite::new(host.addr().to_string(), FILE, TOKEN);
    let mut future = Browser::connect(&invite);
    future.send(&hello(&message::client_version(), PROTOCOL + 1));
    let why = future.refusal().unwrap_or_default();
    assert!(why.contains(&format!("protocol {}", PROTOCOL + 1)) && why.contains(&PROTOCOL.to_string()), "{why}");
    // A frame of another wire version is never decoded as this one: the connection ends.
    let mut other_wire = Browser::connect(&invite);
    let mut bytes = message::encode(&hello(&message::client_version(), PROTOCOL)).unwrap();
    assert_eq!(bytes[0], WIRE_VERSION);
    bytes[0] = WIRE_VERSION.wrapping_add(1);
    other_wire.0.send(Message::Binary(Bytes::from(bytes))).unwrap();
    while let Some(m) = other_wire.next() {
        assert!(!matches!(m, ServerMsg::Welcome { .. }), "welcomed on another wire version");
    }
    host.stop();
}

const CLOUD_FILE: &str = "0b0e7f5c-6a51-4c8e-9a43-0d2f1e3c4b5a";
/// Share-link secrets are URL-safe base64 (43 characters for 32 bytes).
const CLOUD_SECRET: &str = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCdE";

#[test]
fn all_three_link_shapes_round_trip() {
    // 1. Desktop: figcraft://join?host=…
    let desktop = Invite::parse("figcraft://join?host=192.168.1.20:47900&file=0123456789abcdef&token=AbCdEfGhIjKlMnOpQrStUv").unwrap();
    assert_eq!(desktop.server, Endpoint::plain("192.168.1.20:47900"));
    assert_eq!(desktop.link(), "figcraft://join?host=192.168.1.20:47900&file=0123456789abcdef&token=AbCdEfGhIjKlMnOpQrStUv");
    assert_eq!(Invite::parse(&desktop.url()).unwrap(), desktop);
    // 2. figcraft-server behind a proxy: figcraft://join?server=wss://…/path/
    let server =
        Invite::parse("figcraft://join?server=wss://files.example.org/figcraft/&file=0123456789abcdef&token=AbCdEfGhIjKlMnOpQrStUv").unwrap();
    assert_eq!(server.server, Endpoint { host: "files.example.org:443".into(), path: "/figcraft/".into(), secure: true });
    assert_eq!(Invite::parse(&server.link()).unwrap(), server);
    assert_eq!(Invite::parse(&server.url()).unwrap(), server);
    // 3. Cloud relay: ws(s)://host/ws/v1/collab?file=<uuid>&token=<secret>
    for (link, secure, host) in [
        (format!("wss://figcraft.example/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}"), true, "figcraft.example:443"),
        (format!("ws://127.0.0.1:8080/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}"), false, "127.0.0.1:8080"),
    ] {
        let cloud = Invite::parse(&link).unwrap();
        assert_eq!(cloud.server, Endpoint { host: host.into(), path: "/ws/v1/collab/".into(), secure });
        assert_eq!((cloud.file.as_str(), cloud.token.as_str()), (CLOUD_FILE, CLOUD_SECRET));
        assert_eq!(Invite::parse(&cloud.url()).unwrap(), cloud);
        assert_eq!(Invite::parse(&cloud.link()).unwrap(), cloud);
        // The secret never goes in the request: the endpoint is all the client connects to.
        assert!(!cloud.endpoint().contains(CLOUD_SECRET) && !cloud.endpoint().contains('?'));
    }
}

#[test]
fn a_cloud_link_is_never_mistaken_for_a_desktop_link() {
    for link in [
        format!("ws://127.0.0.1:8080/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}"),
        format!("wss://figcraft.example/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}"),
        format!("wss://figcraft.example:8443/ws/v1/collab/?file={CLOUD_FILE}&token={CLOUD_SECRET}"),
    ] {
        let cloud = Invite::parse(&link).unwrap();
        // Re-shared as a figcraft:// link it names the whole endpoint (server=…), never host=…, so
        // the relay path and TLS survive; a desktop host answers at / over plain ws:// only.
        assert!(cloud.link().starts_with("figcraft://join?server=ws"), "{}", cloud.link());
        assert!(!cloud.link().contains("host="));
        assert_ne!(cloud.server, Endpoint::plain(cloud.server.host.clone()));
        assert_eq!(cloud.server.display(), cloud.server.url(), "shown as a URL, not as host:port");
        assert!(cloud.server.path.starts_with("/ws/v1/collab"));
    }
    // A desktop link keeps its short form, and its endpoint is `/`.
    let desktop = Invite::new("192.168.1.20:47900", FILE, TOKEN);
    assert!(desktop.link().contains("host=192.168.1.20:47900"));
    assert_eq!(desktop.endpoint(), "ws://192.168.1.20:47900/");
    // Join codes stay a separate shape: a cloud link is never read as one.
    let (_, code) = Invite::parse_join(&format!("ws://127.0.0.1:8080/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}")).unwrap();
    assert_eq!(code, None);
    // Broken cloud-shaped links are refused without echoing the secret.
    for bad in [
        format!("ws://127.0.0.1:8080/ws/v1/collab?file={CLOUD_FILE}&token=short"),
        format!("http://127.0.0.1:8080/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}"),
        format!("ws://127.0.0.1:8080/ws/v1/collab?token={CLOUD_SECRET}"),
        format!("ws://127.0.0.1:0/ws/v1/collab?file={CLOUD_FILE}&token={CLOUD_SECRET}"),
    ] {
        let e = Invite::parse(&bad).unwrap_err();
        assert!(!e.contains(CLOUD_SECRET) && !e.contains(CLOUD_FILE), "{e}");
    }
}
