//! S4-023: live relay connections racing the revocation of their access, over real WebSockets
//! against the dev PostgreSQL and object store (fixture prefix e3, random tails per run; skipped
//! without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR).
//!
//! # The contract (bounded window)
//!
//! The relay does not re-authorize each batch: the [`RevocationSweeper`] re-checks every watched
//! member admission and share link once per sweep interval and has the hub close the dead ones.
//! So for a revocation that commits at `T`:
//!
//! * the connection is closed (told [`STOPPED`]) by `T + interval + one sweep round trip` (the
//!   sweep's database check plus its managing request to the hub, each bounded by the backend
//!   timeout); this file allows [`ROUND_TRIP`] for the round trip;
//! * a batch the client *sends* after that point is never confirmed (the connection is gone), and
//!   a fresh hello with the same credential is refused at once;
//! * a batch sent before `T` (or inside the window) may still be confirmed, even when its append
//!   commits after `T`: the in-flight case is forced here with a gate transaction holding the
//!   document row, so the batch's append is blocked (seen in `pg_stat_activity`) while the
//!   revocation commits, and is released afterwards.
//!
//! Each case uses its own document, so every committed event in it came from the client under
//! test: at most one of them (the batch in flight when the connection closed) is unconfirmed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED, RevocationSweeper};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::{
    add_tenant_member, remove_document_member, remove_tenant_member, set_document_member, set_workspace_member,
};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::sessions::{SessionLimits, create_session, logout};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, revoke_share_link};
use figcraft_collab::message::{self, Access, CatchUp, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::{Hosting, STOPPED};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

type TestResult = Result<(), Box<dyn std::error::Error>>;
/// Longest any single read or wait in these tests.
const WAIT: Duration = Duration::from_secs(20);
/// The sweep interval the relay runs with here.
const INTERVAL: Duration = Duration::from_millis(200);
/// Allowance for one sweep round trip (database check plus the managing request to the hub).
const ROUND_TRIP: Duration = Duration::from_secs(2);

fn e3(kind: u8) -> Uuid {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("e3{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

/// How the client under test is admitted, and how it loses access.
#[derive(Clone, Copy, Debug)]
enum Revocation {
    /// A member (workspace editor) removed from the tenant.
    TenantMemberRemoved,
    /// A member whose only access is a direct document membership, which is removed.
    DocumentMemberRemoved,
    /// A member whose session is signed out.
    SessionLoggedOut,
    /// An editor share link, revoked.
    ShareLinkRevoked,
}

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    project: ProjectId,
}

impl Fixture {
    fn new() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
        else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; relay revocation race probe skipped");
            return Ok(None);
        };
        let database_url = std::fs::read_to_string(database_url_file)?.trim_end().to_string();
        let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
            Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string())
        };
        let objects = ObjectStoreSettings {
            endpoint: read("s3_endpoint")?,
            region: "us-east-1".into(),
            bucket: "figcraft".into(),
            access_key: read("s3_access_key")?,
            secret_key: read("s3_secret_key")?,
            path_style: true,
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let tenant: TenantId = e3(0x00).to_string().parse()?;
        let workspace: WorkspaceId = e3(0x10).to_string().parse()?;
        let project: ProjectId = e3(0x20).to_string().parse()?;
        let owner: PrincipalId = e3(0x01).to_string().parse()?;
        let pool = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(4).acquire_timeout(WAIT).connect(&database_url).await?;
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e3 relay owner')")
                .bind(owner.to_string())
                .execute(&pool)
                .await?;
            let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
            sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'e3 relay races', 'test', 'local')")
                .bind(tenant.to_string())
                .bind(format!("e3-relay-{}", &tenant.to_string()[24..]))
                .execute(transaction.connection())
                .await?;
            add_tenant_member(&mut transaction, owner, Role::TenantOwner).await.map_err(|error| format!("{error:?}"))?;
            create_workspace(&mut transaction, workspace, "e3 relay", None).await.map_err(|error| format!("{error:?}"))?;
            create_project(&mut transaction, project, workspace, "e3 relay", None).await.map_err(|error| format!("{error:?}"))?;
            transaction.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(pool)
        })?;
        Ok(Some(Self { runtime, pool, database_url, objects, tenant, owner, workspace, project }))
    }

    fn document(&self) -> CloudDocumentId {
        let document: CloudDocumentId = e3(0x30).to_string().parse().unwrap();
        let (workspace, project) = (self.workspace, self.project);
        self.change(async move |tx| create_document(tx, document, workspace, project, "e3 relay doc").await.map(|_| ()).unwrap());
        document
    }

    /// A tenant member (tenant role `editor`) with editor access to `document` (through the
    /// workspace, or only through a document membership), and a fresh session's token.
    fn member(&self, document: CloudDocumentId, document_only: bool) -> (PrincipalId, String) {
        let principal: PrincipalId = e3(0x01).to_string().parse().unwrap();
        let workspace = self.workspace;
        self.runtime.block_on(async {
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e3 relay member')")
                .bind(principal.to_string())
                .execute(&self.pool)
                .await
                .unwrap();
        });
        self.change(async move |tx| {
            add_tenant_member(tx, principal, Role::Editor).await.unwrap();
            if document_only {
                set_document_member(tx, document, principal, Role::Editor).await.unwrap();
            } else {
                set_workspace_member(tx, workspace, principal, Role::Editor).await.unwrap();
            }
        });
        let session = self.runtime.block_on(create_session(&self.pool, principal, SessionLimits::default())).unwrap();
        (principal, session.token)
    }

    fn change(&self, change: impl AsyncFnOnce(&mut figcraft_cloud_storage::TenantTransaction<'_>)) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            change(&mut transaction).await;
            transaction.commit().await.unwrap();
        });
    }

    fn config(&self) -> CloudBackendConfig {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config
    }

    fn serve_swept(&self) -> (Hosting, RevocationSweeper) {
        let backend = CloudBackend::connect(&self.config()).unwrap();
        let watch = backend.revocation_watch();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let hosting = Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap();
        let sweeper = RevocationSweeper::start(&self.config(), watch, hosting.addr(), INTERVAL).unwrap();
        (hosting, sweeper)
    }

    /// Committed events in `document`.
    fn events(&self, document: CloudDocumentId) -> i64 {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            let count = sqlx::query_scalar("SELECT count(*) FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid")
                .bind(self.tenant.to_string())
                .bind(document.to_string())
                .fetch_one(transaction.connection())
                .await
                .unwrap();
            transaction.rollback().await.unwrap();
            count
        })
    }
}

struct Raw(WebSocket<TcpStream>);

/// What the hub said next about our batch.
#[derive(Debug, PartialEq)]
enum Outcome {
    Confirmed(u64),
    /// The connection ended; `true` when the hub closed it as revoked.
    Closed(bool),
}

impl Raw {
    fn join(addr: SocketAddr, file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "e3 probe", ""));
        raw
    }

    fn send(&mut self, msg: &ClientMsg) -> bool {
        self.0.send(Message::Binary(Bytes::from(message::encode(msg).unwrap()))).is_ok()
    }

    fn welcome(&mut self) -> (u32, u64, Access, NodeId) {
        loop {
            match self.0.read() {
                Ok(Message::Binary(bytes)) => match message::decode(&bytes, 64 << 20).unwrap() {
                    ServerMsg::Welcome { client_id, seq, access, catch_up: CatchUp::Snapshot(changes), .. } => {
                        let page = changes
                            .iter()
                            .find_map(|change| match change {
                                Change::Create { id, parent: None, .. } => Some(*id),
                                _ => None,
                            })
                            .unwrap();
                        return (client_id, seq, access, page);
                    }
                    other => panic!("expected a welcome with a snapshot, got {other:?}"),
                },
                Ok(_) => {}
                Err(error) => panic!("no welcome: {error}"),
            }
        }
    }

    fn rename(&mut self, base_seq: u64, batch_id: u64, page: NodeId, name: &str) -> bool {
        let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
        self.send(&ClientMsg::Changes { base_seq, batch_id, changes })
    }

    /// Read until this client's batch is confirmed or the connection ends.
    fn outcome(&mut self, me: u32, batch_id: u64) -> Outcome {
        loop {
            match self.0.read() {
                Ok(Message::Binary(bytes)) => match message::decode(&bytes, 64 << 20).unwrap() {
                    ServerMsg::Changes(accepted) if accepted.author == me && accepted.batch_id == batch_id => {
                        return Outcome::Confirmed(accepted.seq);
                    }
                    ServerMsg::Reject { reason, .. } => panic!("batch refused: {reason}"),
                    _ => {}
                },
                Ok(Message::Close(frame)) => return Outcome::Closed(frame.is_some_and(|frame| frame.reason.as_str() == STOPPED)),
                Ok(_) => {}
                Err(_) => return Outcome::Closed(false),
            }
        }
    }

    fn refusal(&mut self) -> Option<String> {
        let mut why = None;
        while let Ok(msg) = self.0.read() {
            if let Message::Binary(bytes) = msg
                && let Ok(ServerMsg::Refused { reason }) = message::decode(&bytes, 64 << 20)
            {
                why = Some(reason);
            }
        }
        why
    }
}

/// Wait until some backend is blocked behind `gate_pid`.
fn wait_blocked_behind(fixture: &Fixture, gate_pid: i32) {
    fixture.runtime.block_on(async {
        let deadline = Instant::now() + WAIT;
        loop {
            let blocked: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))")
                .bind(gate_pid)
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
            if blocked > 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the batch's append never blocked on the gate");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
}

/// Commits the revocation.
type Revoke = Box<dyn FnOnce(&Fixture)>;

fn race(fixture: &Fixture, revocation: Revocation) {
    let document = fixture.document();
    let file = document.to_string();
    let (hello, revoke): (String, Revoke) = match revocation {
        Revocation::TenantMemberRemoved | Revocation::DocumentMemberRemoved | Revocation::SessionLoggedOut => {
            let document_only = matches!(revocation, Revocation::DocumentMemberRemoved);
            let (principal, token) = fixture.member(document, document_only);
            let hello = format!("member.{}.{}", fixture.tenant, token);
            let revoke: Revoke = match revocation {
                Revocation::TenantMemberRemoved => {
                    Box::new(move |f: &Fixture| f.change(async move |tx| remove_tenant_member(tx, principal).await.unwrap()))
                }
                Revocation::DocumentMemberRemoved => {
                    Box::new(move |f: &Fixture| f.change(async move |tx| assert!(remove_document_member(tx, document, principal).await.unwrap())))
                }
                _ => Box::new(move |f: &Fixture| assert_eq!(f.runtime.block_on(logout(&f.pool, &token)), Ok(true))),
            };
            (hello, revoke)
        }
        Revocation::ShareLinkRevoked => {
            let owner = fixture.owner;
            let link = fixture.runtime.block_on(async {
                let mut transaction = begin_tenant_transaction(&fixture.pool, fixture.tenant).await.unwrap();
                let link = create_share_link(&mut transaction, document, ShareRole::Editor, None, owner, None).await.unwrap();
                transaction.commit().await.unwrap();
                link
            });
            let id = link.id;
            (link.secret, Box::new(move |f: &Fixture| f.change(async move |tx| assert!(revoke_share_link(tx, id).await.unwrap()))))
        }
    };

    let (mut hosting, sweeper) = fixture.serve_swept();
    let addr = hosting.addr();
    let mut client = Raw::join(addr, &file, &hello);
    let (me, mut seq, access, page) = client.welcome();
    assert_eq!(access, Access::Edit, "{revocation:?}");
    let mut batch = 1;
    assert!(client.rename(seq, batch, page, "before"));
    let Outcome::Confirmed(confirmed) = client.outcome(me, batch) else { panic!("{revocation:?}: the first batch wasn't confirmed") };
    seq = confirmed;
    let mut confirmations = 1;
    // Live access is left alone across several sweeps.
    std::thread::sleep(INTERVAL * 3);
    batch += 1;
    assert!(client.rename(seq, batch, page, "still live"));
    let Outcome::Confirmed(confirmed) = client.outcome(me, batch) else { panic!("{revocation:?}: a live client was disconnected") };
    seq = confirmed;
    confirmations += 1;

    // In flight: the gate holds the document row, so the next batch's append blocks; the
    // revocation commits while it is blocked; then the append is released.
    let (gate, gate_pid) = fixture.runtime.block_on(async {
        let mut gate = begin_tenant_transaction(&fixture.pool, fixture.tenant).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(gate.connection()).await.unwrap();
        sqlx::query("SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
            .bind(fixture.tenant.to_string())
            .bind(document.to_string())
            .execute(gate.connection())
            .await
            .unwrap();
        (gate, pid)
    });
    batch += 1;
    assert!(client.rename(seq, batch, page, "in flight"));
    wait_blocked_behind(fixture, gate_pid);
    revoke(fixture);
    let revoked_at = Instant::now();
    fixture.runtime.block_on(gate.rollback()).unwrap();
    // The batch sent before the revocation may be confirmed although its append committed after
    // it: that is inside the documented window.
    let mut closed = None;
    match client.outcome(me, batch) {
        Outcome::Confirmed(confirmed) => {
            seq = confirmed;
            confirmations += 1;
        }
        Outcome::Closed(stopped) => closed = Some((Instant::now(), stopped)),
    }

    // Keep editing as fast as the relay confirms until the connection is closed.
    let window = INTERVAL + ROUND_TRIP;
    let mut sent_after_window_confirmed = 0;
    while closed.is_none() {
        assert!(revoked_at.elapsed() < WAIT, "{revocation:?}: the connection was never closed");
        batch += 1;
        let sent = Instant::now();
        if !client.rename(seq, batch, page, &format!("after revoke {batch}")) {
            closed = Some((Instant::now(), false));
            break;
        }
        match client.outcome(me, batch) {
            Outcome::Confirmed(confirmed) => {
                seq = confirmed;
                confirmations += 1;
                if sent.duration_since(revoked_at) > window {
                    sent_after_window_confirmed += 1;
                }
            }
            Outcome::Closed(stopped) => closed = Some((Instant::now(), stopped)),
        }
    }
    let (closed_at, stopped) = closed.unwrap();
    assert!(stopped, "{revocation:?}: the hub closes the connection as revoked");
    let took = closed_at.duration_since(revoked_at);
    assert!(took <= window, "{revocation:?}: closed {took:?} after the revoke committed (window {window:?})");
    assert_eq!(sent_after_window_confirmed, 0, "{revocation:?}: a batch sent after the window was confirmed");
    // Every committed event was confirmed, except at most the one in flight at the close.
    let committed = fixture.events(document);
    assert!((confirmations..=confirmations + 1).contains(&committed), "{revocation:?}: {committed} events, {confirmations} confirmed");
    // The same credential is refused at once.
    assert_eq!(Raw::join(addr, &file, &hello).refusal().as_deref(), Some(REFUSED), "{revocation:?}");
    eprintln!("{revocation:?}: closed {took:?} after the revoke, {confirmations} confirmed, {committed} committed");
    drop(client);
    drop(sweeper);
    hosting.stop();
}

#[test]
fn a_member_removed_from_the_tenant_is_closed_within_one_sweep() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    race(&fixture, Revocation::TenantMemberRemoved);
    Ok(())
}

#[test]
fn a_member_removed_from_the_document_is_closed_within_one_sweep() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    race(&fixture, Revocation::DocumentMemberRemoved);
    Ok(())
}

#[test]
fn a_member_whose_session_is_signed_out_is_closed_within_one_sweep() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    race(&fixture, Revocation::SessionLoggedOut);
    Ok(())
}

#[test]
fn a_revoked_share_link_is_closed_within_one_sweep() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    race(&fixture, Revocation::ShareLinkRevoked);
    Ok(())
}
