//! S5-011 against the dev PostgreSQL and S3 (fixture prefix d8; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR): an emptied room stays warm (leased,
//! no new restore for a client that comes back) and then closes, saved and released; a busy room
//! is snapshotted on a schedule (by accepted batches and by time) while it stays open.
//!
//! The tests are synchronous: the backend owns its own runtime, and fixtures use another one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, LeaseConfig};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::room_leases::{RoomLease, current_room};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link};
use figcraft_collab::message::{self, CatchUp, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::Hosting;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

const TENANT: &str = "d8000000-0000-4000-8000-000000000081";
const ACTOR: &str = "d8000000-0000-4000-8000-000000000089";
const WORKSPACE: &str = "d8100000-0000-4000-8000-000000000081";
const PROJECT: &str = "d8200000-0000-4000-8000-000000000081";
const WAIT: Duration = Duration::from_secs(20);

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
}

/// What a replica is configured with beyond the defaults.
struct Housekeeping {
    idle_close_after: Duration,
    snapshot_every: Duration,
    snapshot_after_events: u64,
}

impl Fixture {
    fn new() -> Option<Self> {
        let (Ok(url_file), Ok(s3)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; room housekeeping probe skipped");
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
                "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'user', 'housekeeping probe') ON CONFLICT DO NOTHING",
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, 'd8-housekeeping', 'housekeeping probe', 'test', 'local') ON CONFLICT DO NOTHING",
                "INSERT INTO workspaces (id, tenant_id, name) VALUES ($3::uuid, $1::uuid, 'housekeeping probe') ON CONFLICT DO NOTHING",
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($4::uuid, $1::uuid, $3::uuid, 'housekeeping probe') ON CONFLICT DO NOTHING",
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

    /// A relay with leases (so a warm room is visible as a held lease) and `settings`, ticking
    /// every 100 ms.
    fn relay(&self, settings: &Housekeeping) -> Hosting {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config.leases = Some(LeaseConfig::new(format!("ws://{}/ws/v1/collab", listener.local_addr().unwrap())));
        config.idle_close_after = settings.idle_close_after;
        config.snapshot_every = settings.snapshot_every;
        config.snapshot_after_events = settings.snapshot_after_events;
        config.housekeeping_every = Duration::from_millis(100);
        let backend = CloudBackend::connect(&config).unwrap();
        Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap()
    }

    fn lease(&self, document: CloudDocumentId) -> Option<RoomLease> {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let lease = current_room(&mut transaction, document).await.unwrap();
            transaction.rollback().await.unwrap();
            lease
        })
    }

    fn snapshot_seqs(&self, document: CloudDocumentId) -> Vec<i64> {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let seqs = sqlx::query_scalar("SELECT seq FROM document_snapshots WHERE document_id = $1::uuid ORDER BY seq")
                .bind(document.to_string())
                .fetch_all(transaction.connection())
                .await
                .unwrap();
            transaction.rollback().await.unwrap();
            seqs
        })
    }
}

struct Raw(WebSocket<TcpStream>);

impl Raw {
    fn join(addr: SocketAddr, file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "d8 probe", ""));
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

    /// The welcome: client id, sequence and the page to edit.
    fn welcome(&mut self) -> (u32, u64, NodeId) {
        loop {
            match self.next() {
                Some(ServerMsg::Welcome { client_id, seq, catch_up: CatchUp::Snapshot(changes), .. }) => {
                    let page = changes
                        .iter()
                        .find_map(|change| match change {
                            Change::Create { id, parent: None, .. } => Some(*id),
                            _ => None,
                        })
                        .unwrap();
                    return (client_id, seq, page);
                }
                Some(ServerMsg::Refused { reason }) => panic!("refused: {reason}"),
                Some(_) => {}
                None => panic!("closed before the welcome"),
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

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_emptied_room_stays_warm_for_a_returning_client_then_closes_saved_and_released() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let file = document.to_string();
    let mut relay = fx.relay(&Housekeeping {
        idle_close_after: Duration::from_millis(2500),
        snapshot_every: Duration::from_secs(3600),
        snapshot_after_events: 1_000_000,
    });

    let mut first = Raw::join(relay.addr(), &file, &secret);
    let (me, seq, page) = first.welcome();
    assert_eq!(first.rename(me, seq, 1, page, "edited"), 1);
    let lease = fx.lease(document).expect("the open room is leased");
    drop(first);

    // Well inside the keep-warm period the room is still open and leased.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(fx.lease(document).map(|l| l.fencing_token), Some(lease.fencing_token), "still the same lease and fence");
    // A client that comes back finds it warm: same fence (it was not restored and re-fenced), the
    // committed edit is there, and it can edit on.
    let mut back = Raw::join(relay.addr(), &file, &secret);
    let (me_back, seq_back, page_back) = back.welcome();
    assert_eq!(seq_back, 1);
    assert_eq!(fx.lease(document).map(|l| l.fencing_token), Some(lease.fencing_token), "no new restore: the fence did not move");
    assert_eq!(back.rename(me_back, seq_back, 1, page_back, "edited again"), 2);
    drop(back);

    // Empty again: after the period the room closes, is saved and its lease released.
    let emptied = Instant::now();
    wait_for("the warm room to close", || fx.lease(document).is_none());
    assert!(emptied.elapsed() >= Duration::from_millis(2000), "it was kept for the period: {:?}", emptied.elapsed());
    assert!(fx.snapshot_seqs(document).contains(&2), "the closing room stored a snapshot of its committed state: {:?}", fx.snapshot_seqs(document));
    // And a client after that opens it afresh under a new, higher fence.
    let mut later = Raw::join(relay.addr(), &file, &secret);
    let (_, seq_later, _) = later.welcome();
    assert_eq!(seq_later, 2);
    assert!(fx.lease(document).unwrap().fencing_token > lease.fencing_token);
    drop(later);
    relay.stop();
}

#[test]
fn a_busy_room_is_snapshotted_after_enough_batches_while_it_stays_open() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let mut relay =
        fx.relay(&Housekeeping { idle_close_after: Duration::from_secs(60), snapshot_every: Duration::from_secs(3600), snapshot_after_events: 3 });
    let mut client = Raw::join(relay.addr(), &document.to_string(), &secret);
    let (me, seq, page) = client.welcome();
    assert_eq!(fx.snapshot_seqs(document), vec![0], "a new document starts with its first state stored");

    let mut seq = seq;
    for n in 1..=7u64 {
        seq = client.rename(me, seq, n, page, &format!("edit {n}"));
    }
    wait_for("a scheduled snapshot", || fx.snapshot_seqs(document).iter().any(|s| *s >= 3));
    let first = fx.snapshot_seqs(document).into_iter().max().unwrap();
    assert!((3..=7).contains(&first), "snapshotted a committed sequence: {first}");

    // It carries on: more edits, another snapshot, the client never left.
    for n in 8..=12u64 {
        seq = client.rename(me, seq, n, page, &format!("edit {n}"));
    }
    wait_for("a second scheduled snapshot", || fx.snapshot_seqs(document).into_iter().max().unwrap() > first);
    assert!(fx.lease(document).is_some(), "the room was open the whole time");
    drop(client);
    relay.stop();
}

#[test]
fn a_room_with_few_changes_is_still_snapshotted_by_time() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let mut relay = fx.relay(&Housekeeping {
        idle_close_after: Duration::from_secs(60),
        snapshot_every: Duration::from_secs(1),
        snapshot_after_events: 1_000_000,
    });
    let mut client = Raw::join(relay.addr(), &document.to_string(), &secret);
    let (me, seq, page) = client.welcome();
    client.rename(me, seq, 1, page, "one edit");
    wait_for("the time-based snapshot", || fx.snapshot_seqs(document).contains(&1));
    // Nothing changed since: no further snapshots pile up.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(fx.snapshot_seqs(document), vec![0, 1]);
    drop(client);
    relay.stop();
}
