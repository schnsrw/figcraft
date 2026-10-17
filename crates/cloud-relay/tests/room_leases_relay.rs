//! Room leases in the relay against the dev PostgreSQL and S3 (S5-003/004, fixture prefix d5;
//! skipped without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR): two relay replicas
//! on one database, each room served by exactly one of them, the other refusing it; two replicas
//! owning different rooms at once (S5-020); a released room taken over at once; and a room whose
//! lease was taken away closing without confirming anything more.
//!
//! The tests are synchronous: the backend owns its own runtime, and fixtures use another one.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, LeaseConfig, OWNED_ELSEWHERE};
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

const TENANT: &str = "d5000000-0000-4000-8000-000000000051";
const ACTOR: &str = "d5000000-0000-4000-8000-000000000059";
const WORKSPACE: &str = "d5100000-0000-4000-8000-000000000051";
const PROJECT: &str = "d5200000-0000-4000-8000-000000000051";
const WAIT: Duration = Duration::from_secs(20);
const ENDPOINT_A: &str = "ws://relay-a.internal:8082/ws/v1/collab";
const ENDPOINT_B: &str = "ws://relay-b.internal:8082/ws/v1/collab";

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
}

impl Fixture {
    fn new() -> Option<Self> {
        let (Ok(url_file), Ok(s3)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; room lease relay probe skipped");
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
                "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'user', 'lease probe') ON CONFLICT DO NOTHING",
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, 'd5-leases', 'lease probe', 'test', 'local') ON CONFLICT DO NOTHING",
                "INSERT INTO workspaces (id, tenant_id, name) VALUES ($3::uuid, $1::uuid, 'lease probe') ON CONFLICT DO NOTHING",
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($4::uuid, $1::uuid, $3::uuid, 'lease probe') ON CONFLICT DO NOTHING",
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

    /// A new document and an editor link to it: its id and the link secret.
    fn document(&self) -> (CloudDocumentId, String) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let id = CloudDocumentId::random();
            create_document(
                &mut transaction,
                id,
                WORKSPACE.parse::<WorkspaceId>().unwrap(),
                PROJECT.parse::<ProjectId>().unwrap(),
                "lease probe doc",
            )
            .await
            .unwrap();
            let link = create_share_link(&mut transaction, id, ShareRole::Editor, None, ACTOR.parse().unwrap(), None).await.unwrap();
            transaction.commit().await.unwrap();
            (id, link.secret)
        })
    }

    /// A relay replica advertising `endpoint`, renewing every `renew` with a lease of `lifetime`.
    fn replica(&self, endpoint: &str, renew: Duration, lifetime: Duration) -> Hosting {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config.leases = Some(LeaseConfig { endpoint: endpoint.into(), lifetime, renew_every: renew });
        let backend = CloudBackend::connect(&config).unwrap();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
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

    fn committed_seq(&self, document: CloudDocumentId) -> i64 {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let seq = sqlx::query_scalar("SELECT current_seq FROM documents WHERE id = $1::uuid")
                .bind(document.to_string())
                .fetch_one(transaction.connection())
                .await
                .unwrap();
            transaction.rollback().await.unwrap();
            seq
        })
    }

    /// Another relay taking the room over: a new owner and a higher fence, in the lease and the
    /// document, as a successful acquisition after an expiry would leave them.
    fn take_over(&self, document: CloudDocumentId) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            sqlx::query("UPDATE documents SET fencing_token = fencing_token + 1 WHERE id = $1::uuid")
                .bind(document.to_string())
                .execute(transaction.connection())
                .await
                .unwrap();
            sqlx::query("UPDATE room_leases SET owner_id = gen_random_uuid(), fencing_token = fencing_token + 1 WHERE document_id = $1::uuid")
                .bind(document.to_string())
                .execute(transaction.connection())
                .await
                .unwrap();
            transaction.commit().await.unwrap();
        });
    }
}

struct Raw(WebSocket<TcpStream>);

impl Raw {
    fn join(addr: SocketAddr, file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "d5 probe", ""));
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

    /// The welcome: client id, sequence and the page to edit; or the refusal's reason.
    fn welcome(&mut self) -> Result<(u32, u64, NodeId), String> {
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
                    return Ok((client_id, seq, page));
                }
                Some(ServerMsg::Refused { reason }) => return Err(reason),
                Some(_) => {}
                None => return Err("closed".into()),
            }
        }
    }

    fn rename(&mut self, base_seq: u64, batch_id: u64, page: NodeId, name: &str) {
        let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
        self.send(&ClientMsg::Changes { base_seq, batch_id, changes });
    }

    /// Read until this client's batch is confirmed (its sequence), or the connection ends.
    fn confirmation(&mut self, me: u32, batch_id: u64) -> Option<u64> {
        loop {
            match self.next()? {
                ServerMsg::Changes(accepted) if accepted.author == me && accepted.batch_id == batch_id => return Some(accepted.seq),
                ServerMsg::Reject { reason, .. } => panic!("batch refused: {reason}"),
                _ => {}
            }
        }
    }

    /// Read until the connection ends.
    fn until_closed(&mut self) {
        while self.next().is_some() {}
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
fn one_replica_owns_a_room_and_the_other_refuses_it_until_it_is_released() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let file = document.to_string();
    let (mut a, mut b) = (
        fx.replica(ENDPOINT_A, Duration::from_secs(5), Duration::from_secs(15)),
        fx.replica(ENDPOINT_B, Duration::from_secs(5), Duration::from_secs(15)),
    );

    let mut on_a = Raw::join(a.addr(), &file, &secret);
    let (me, seq, page) = on_a.welcome().unwrap();
    let lease = fx.lease(document).expect("the room is leased once it is open");
    assert_eq!(lease.endpoint, ENDPOINT_A);
    on_a.rename(seq, 1, page, "from a");
    assert!(on_a.confirmation(me, 1).is_some());

    // The other replica must not serve the room, and says why.
    assert_eq!(Raw::join(b.addr(), &file, &secret).welcome(), Err(OWNED_ELSEWHERE.to_string()));
    assert_eq!(
        fx.lease(document).map(|l| (l.endpoint, l.fencing_token)),
        Some((ENDPOINT_A.to_string(), lease.fencing_token)),
        "the refused open changed nothing"
    );

    // Everyone leaves A: the room closes and its lease is released at once, no waiting for expiry.
    drop(on_a);
    wait_for("the lease to be released", || fx.lease(document).is_none());
    let mut on_b = Raw::join(b.addr(), &file, &secret);
    let (me_b, seq_b, page_b) = on_b.welcome().unwrap();
    let taken = fx.lease(document).expect("B holds the room now");
    assert_eq!(taken.endpoint, ENDPOINT_B);
    assert!(taken.fencing_token > lease.fencing_token, "a new owner is fenced above the old one");
    assert_eq!(seq_b, 1, "B restored what A committed");
    on_b.rename(seq_b, 1, page_b, "from b");
    assert_eq!(on_b.confirmation(me_b, 1), Some(2));
    drop(on_b);
    a.stop();
    b.stop();
}

#[test]
fn two_replicas_own_different_rooms_at_the_same_time() {
    let Some(fx) = Fixture::new() else { return };
    let ((doc_a, secret_a), (doc_b, secret_b)) = (fx.document(), fx.document());
    let (mut a, mut b) = (
        fx.replica(ENDPOINT_A, Duration::from_secs(5), Duration::from_secs(15)),
        fx.replica(ENDPOINT_B, Duration::from_secs(5), Duration::from_secs(15)),
    );

    let mut ca = Raw::join(a.addr(), &doc_a.to_string(), &secret_a);
    let mut cb = Raw::join(b.addr(), &doc_b.to_string(), &secret_b);
    let ((me_a, seq_a, page_a), (me_b, seq_b, page_b)) = (ca.welcome().unwrap(), cb.welcome().unwrap());
    for round in 1..=5u64 {
        ca.rename(seq_a + round - 1, round, page_a, &format!("a{round}"));
        cb.rename(seq_b + round - 1, round, page_b, &format!("b{round}"));
        assert_eq!(ca.confirmation(me_a, round), Some(round));
        assert_eq!(cb.confirmation(me_b, round), Some(round));
    }
    assert_eq!(fx.lease(doc_a).map(|l| l.endpoint), Some(ENDPOINT_A.to_string()));
    assert_eq!(fx.lease(doc_b).map(|l| l.endpoint), Some(ENDPOINT_B.to_string()));
    // Each replica refuses the other's room.
    assert_eq!(Raw::join(b.addr(), &doc_a.to_string(), &secret_a).welcome(), Err(OWNED_ELSEWHERE.to_string()));
    assert_eq!(Raw::join(a.addr(), &doc_b.to_string(), &secret_b).welcome(), Err(OWNED_ELSEWHERE.to_string()));
    assert_eq!((fx.committed_seq(doc_a), fx.committed_seq(doc_b)), (5, 5));
    drop((ca, cb));
    a.stop();
    b.stop();
}

#[test]
fn a_room_whose_lease_was_taken_away_closes_and_confirms_nothing_more() {
    let Some(fx) = Fixture::new() else { return };
    let (document, secret) = fx.document();
    let file = document.to_string();
    // Renewed every 200 ms so the loss is noticed quickly.
    let mut a = fx.replica(ENDPOINT_A, Duration::from_millis(200), Duration::from_secs(15));
    let mut client = Raw::join(a.addr(), &file, &secret);
    let (me, seq, page) = client.welcome().unwrap();
    client.rename(seq, 1, page, "before");
    assert_eq!(client.confirmation(me, 1), Some(1));

    fx.take_over(document);
    // The renewer finds the lease is no longer this relay's and the hub closes the room: the
    // client's connection ends without its next edit being confirmed.
    let started = Instant::now();
    client.rename(1, 2, page, "after the takeover");
    assert_eq!(client.confirmation(me, 2), None, "an edit after losing the lease is never confirmed");
    client.until_closed();
    assert!(started.elapsed() < Duration::from_secs(5), "closed promptly: {:?}", started.elapsed());
    assert_eq!(fx.committed_seq(document), 1, "nothing was committed after the takeover");
    // The new owner's lease row is untouched by the old relay's cleanup.
    assert!(fx.lease(document).is_some());
    a.stop();
    assert!(fx.lease(document).is_some(), "a stale owner releases only its own lease");
}

#[test]
fn simultaneous_hellos_to_two_replicas_leave_exactly_one_owner() {
    let Some(fx) = Fixture::new() else { return };
    for round in 0..3 {
        let (document, secret) = fx.document();
        let file = document.to_string();
        let (mut a, mut b) = (
            fx.replica(ENDPOINT_A, Duration::from_secs(5), Duration::from_secs(15)),
            fx.replica(ENDPOINT_B, Duration::from_secs(5), Duration::from_secs(15)),
        );
        let addrs = [a.addr(), b.addr()];
        let start = std::sync::Barrier::new(8);
        // Eight clients, four per replica, all saying hello at the same moment.
        let results: Vec<(usize, Result<(), String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let (start, file, secret) = (&start, &file, &secret);
                    scope.spawn(move || {
                        start.wait();
                        let mut client = Raw::join(addrs[i % 2], file, secret);
                        let outcome = client.welcome().map(|_| ());
                        std::thread::sleep(Duration::from_millis(300));
                        (i % 2, outcome)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winner = fx.lease(document).expect("someone owns the room");
        let winning_replica = usize::from(winner.endpoint == ENDPOINT_B);
        for (replica, outcome) in &results {
            if *replica == winning_replica {
                assert_eq!(outcome, &Ok(()), "round {round}: clients of the owning replica are served");
            } else {
                assert_eq!(outcome, &Err(OWNED_ELSEWHERE.to_string()), "round {round}: the other replica refuses");
            }
        }
        // The fence was claimed once for the single winner: events written under it commit.
        assert!(winner.fencing_token >= 1);
        a.stop();
        b.stop();
    }
}
