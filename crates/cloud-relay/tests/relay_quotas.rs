//! S5-017 against the dev PostgreSQL and S3 (fixture prefix d9; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR): the relay's quotas hold through the
//! real hub: rooms open at once, clients in one room, messages per second, message size.
//!
//! The tests are synchronous: the backend owns its own runtime, and fixtures use another one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, UNAVAILABLE};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link};
use figcraft_collab::message::{self, CatchUp, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::Hosting;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

const TENANT: &str = "d9000000-0000-4000-8000-000000000081";
const ACTOR: &str = "d9000000-0000-4000-8000-000000000089";
const WORKSPACE: &str = "d9100000-0000-4000-8000-000000000081";
const PROJECT: &str = "d9200000-0000-4000-8000-000000000081";
const WAIT: Duration = Duration::from_secs(20);

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
}

/// The quotas a replica is configured with.
struct Quotas {
    max_rooms: usize,
    max_peers: usize,
    rate: f64,
    max_message_bytes: usize,
}

impl Default for Quotas {
    fn default() -> Self {
        Self { max_rooms: 1024, max_peers: 256, rate: 100.0, max_message_bytes: 64 << 20 }
    }
}

impl Fixture {
    fn new() -> Option<Self> {
        let (Ok(url_file), Ok(s3)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; room quota probe skipped");
            return None;
        };
        let database_url = std::fs::read_to_string(url_file).unwrap().trim_end().to_string();
        let read = |name: &str| std::fs::read_to_string(std::path::Path::new(&s3).join(name)).unwrap().trim_end().to_string();
        let objects = ObjectStoreSettings {
            endpoint: read("s3_endpoint"),
            region: "us-east-1".into(),
            bucket: "figcraft".into(),
            access_key: read("s3_access_key"),
            secret_key: read("s3_secret_key"),
            path_style: true,
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let pool = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await.unwrap();
            let mut transaction = begin_tenant_transaction(&pool, TENANT.parse().unwrap()).await.unwrap();
            for statement in [
                "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'user', 'quota probe') ON CONFLICT DO NOTHING",
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, 'd9-quotas', 'quota probe', 'test', 'local') ON CONFLICT DO NOTHING",
                "INSERT INTO workspaces (id, tenant_id, name) VALUES ($3::uuid, $1::uuid, 'quota probe') ON CONFLICT DO NOTHING",
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($4::uuid, $1::uuid, $3::uuid, 'quota probe') ON CONFLICT DO NOTHING",
            ] {
                sqlx::query(statement)
                    .bind(TENANT)
                    .bind(ACTOR)
                    .bind(WORKSPACE)
                    .bind(PROJECT)
                    .execute(transaction.connection())
                    .await
                    .unwrap();
            }
            transaction.commit().await.unwrap();
            pool
        });
        Some(Self { runtime, pool, database_url, objects })
    }

    fn tenant(&self) -> TenantId {
        TENANT.parse().unwrap()
    }

    fn document(&self) -> (CloudDocumentId, String) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let id = CloudDocumentId::random();
            create_document(
                &mut transaction,
                id,
                WORKSPACE.parse::<WorkspaceId>().unwrap(),
                PROJECT.parse::<ProjectId>().unwrap(),
                "housekeeping doc",
            )
            .await
            .unwrap();
            let link = create_share_link(&mut transaction, id, ShareRole::Editor, None, ACTOR.parse().unwrap(), None).await.unwrap();
            transaction.commit().await.unwrap();
            (id, link.secret)
        })
    }

    /// A relay with `quotas`, set the way the cloud app sets them.
    fn relay(&self, quotas: &Quotas) -> Hosting {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config.max_open = quotas.max_rooms;
        config.limits.max_peers = quotas.max_peers;
        config.limits.max_message_bytes = quotas.max_message_bytes;
        let backend = CloudBackend::connect(&config).unwrap();
        let limits = NetLimits {
            hello_timeout: Duration::from_secs(5),
            rate: quotas.rate,
            burst: quotas.rate * 4.0,
            max_client_message: quotas.max_message_bytes,
            ..NetLimits::default()
        };
        Hosting::serve(listener, backend, limits).unwrap()
    }
}

struct Raw(WebSocket<TcpStream>);

impl Raw {
    fn join(addr: SocketAddr, file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "d9 probe", ""));
        raw
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.0.send(Message::Binary(Bytes::from(message::encode(msg).unwrap()))).unwrap();
    }

    fn next(&mut self) -> Option<ServerMsg> {
        loop {
            match self.0.read() {
                Ok(Message::Binary(bytes)) => return Some(message::decode(&bytes, 64 << 20).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// Rename the page, wait for the confirmation: the new sequence.
    fn rename(&mut self, me: u32, base_seq: u64, batch_id: u64, page: NodeId, name: &str) -> u64 {
        let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
        self.send(&ClientMsg::Changes { base_seq, batch_id, changes });
        loop {
            match self.next().expect("connection closed before the confirmation") {
                ServerMsg::Changes(accepted) if accepted.author == me && accepted.batch_id == batch_id => return accepted.seq,
                ServerMsg::Reject { reason, .. } => panic!("batch refused: {reason}"),
                _ => {}
            }
        }
    }
}

/// The welcome (`Ok`) or the refusal's reason (`Err`).
fn joined(raw: &mut Raw) -> Result<(u32, u64, NodeId), String> {
    loop {
        match raw.next() {
            Some(ServerMsg::Welcome { client_id, seq, catch_up: CatchUp::Snapshot(changes), .. }) => {
                let page = changes
                    .iter()
                    .find_map(|change| match change {
                        Change::Create { id, parent: None, .. } => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                return Ok((client_id, seq, page));
            }
            Some(ServerMsg::Refused { reason }) => return Err(reason),
            Some(_) => {}
            None => return Err("closed".into()),
        }
    }
}

#[test]
fn the_room_quota_refuses_a_new_document_and_frees_a_slot_when_a_room_closes() {
    let Some(fx) = Fixture::new() else { return };
    let ((a, secret_a), (b, secret_b), (c, secret_c)) = (fx.document(), fx.document(), fx.document());
    let mut relay = fx.relay(&Quotas { max_rooms: 2, ..Quotas::default() });

    let mut ca = Raw::join(relay.addr(), &a.to_string(), &secret_a);
    let mut cb = Raw::join(relay.addr(), &b.to_string(), &secret_b);
    assert!(joined(&mut ca).is_ok() && joined(&mut cb).is_ok());
    // A third document is refused, with the same message as any unavailable document; the open
    // ones are unaffected, and another client can still join an open room.
    assert_eq!(joined(&mut Raw::join(relay.addr(), &c.to_string(), &secret_c)), Err(UNAVAILABLE.to_string()));
    assert!(joined(&mut Raw::join(relay.addr(), &a.to_string(), &secret_a)).is_ok(), "a room already open takes more clients");
    // When a room closes its slot is free again.
    drop(ca);
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    loop {
        if joined(&mut Raw::join(relay.addr(), &c.to_string(), &secret_c)).is_ok() {
            break;
        }
        assert!(started.elapsed() < WAIT, "a freed room slot was never reusable");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(cb);
    relay.stop();
}

#[test]
fn the_peer_quota_refuses_the_next_client_of_a_full_room_until_one_leaves() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let file = document.to_string();
    let mut relay = fx.relay(&Quotas { max_peers: 2, ..Quotas::default() });
    let (mut one, mut two) = (Raw::join(relay.addr(), &file, &secret), Raw::join(relay.addr(), &file, &secret));
    assert!(joined(&mut one).is_ok() && joined(&mut two).is_ok());
    assert_eq!(joined(&mut Raw::join(relay.addr(), &file, &secret)), Err("the session is full".to_string()));
    drop(two);
    let started = Instant::now();
    loop {
        if joined(&mut Raw::join(relay.addr(), &file, &secret)).is_ok() {
            break;
        }
        assert!(started.elapsed() < WAIT, "a freed seat was never reusable");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(one);
    relay.stop();
}

#[test]
fn the_rate_quota_drops_a_connection_that_floods_and_leaves_the_others_alone() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let file = document.to_string();
    let mut relay = fx.relay(&Quotas { rate: 5.0, ..Quotas::default() });
    let (mut flooder, mut calm) = (Raw::join(relay.addr(), &file, &secret), Raw::join(relay.addr(), &file, &secret));
    assert!(joined(&mut flooder).is_ok());
    let (me, seq, page) = joined(&mut calm).unwrap();
    // Far more messages than the burst (4 x 5) in one go: the hub drops the connection.
    for _ in 0..200 {
        if flooder.0.send(Message::Binary(Bytes::from(message::encode(&ClientMsg::Ping).unwrap()))).is_err() {
            break;
        }
    }
    let started = Instant::now();
    while flooder.next().is_some() {}
    assert!(started.elapsed() < Duration::from_secs(5), "the flooding connection was ended promptly: {:?}", started.elapsed());
    // The well-behaved client in the same room is not affected: it can still edit.
    assert_eq!(calm.rename(me, seq, 1, page, "still fine"), 1);
    drop(calm);
    relay.stop();
}

#[test]
fn the_message_size_quota_ends_a_connection_that_sends_too_much_and_allows_small_messages() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let file = document.to_string();
    let mut relay = fx.relay(&Quotas { max_message_bytes: 4096, ..Quotas::default() });
    let mut client = Raw::join(relay.addr(), &file, &secret);
    let (me, seq, page) = joined(&mut client).unwrap();
    assert_eq!(client.rename(me, seq, 1, page, "small"), 1, "a small message is fine");
    // A message past the cap (a name of 64 KiB inside one batch): the connection is ended, not the relay.
    let big = "x".repeat(64 << 10);
    let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(&big).unwrap() }];
    let _ = client.0.send(Message::Binary(Bytes::from(message::encode(&ClientMsg::Changes { base_seq: 1, batch_id: 2, changes }).unwrap())));
    let started = Instant::now();
    loop {
        match client.next() {
            Some(ServerMsg::Changes(accepted)) if accepted.batch_id == 2 => panic!("an oversized batch was accepted"),
            Some(_) => {}
            None => break,
        }
    }
    assert!(started.elapsed() < Duration::from_secs(5), "ended promptly: {:?}", started.elapsed());
    // The relay is fine: a new client joins and sees only the small edit.
    let mut next = Raw::join(relay.addr(), &file, &secret);
    let (_, seq_next, _) = joined(&mut next).unwrap();
    assert_eq!(seq_next, 1);
    drop(next);
    relay.stop();
}
