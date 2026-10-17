//! The cloud collaboration backend end to end against the dev PostgreSQL and S3 (fixture prefix
//! b1; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR): share-link
//! admission, a client joining through the real hub over loopback, durability before
//! confirmation, recovery after a restart, and a fenced commit closing the document unconfirmed;
//! and (fixture prefix c6, S4-017) a revoked link or archived document disconnecting a live client
//! within a bounded time, with a new hello refused.
//!
//! The tests are synchronous: the backend owns its own runtime, and fixtures use another one.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED, RevocationSweeper};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::{claim_sequencer, read_events};
use figcraft_cloud_storage::metadata::{archive_document, create_document};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, revoke_share_link};
use figcraft_collab::message::{self, Access, CatchUp, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::{Backend, Hosting, STOPPED};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

/// One test group's fixture rows (each group keeps to its own UUID prefix).
struct Ids {
    tenant: &'static str,
    actor: &'static str,
    workspace: &'static str,
    project: &'static str,
    slug: &'static str,
}

const B1: Ids = Ids {
    tenant: "b1000000-0000-4000-8000-000000000021",
    actor: "b1000000-0000-4000-8000-000000000029",
    workspace: "b1100000-0000-4000-8000-000000000021",
    project: "b1200000-0000-4000-8000-000000000021",
    slug: "b1-relay",
};
/// S4-017 revocation (fixture prefix c6).
const C6: Ids = Ids {
    tenant: "c6000000-0000-4000-8000-000000000031",
    actor: "c6000000-0000-4000-8000-000000000039",
    workspace: "c6100000-0000-4000-8000-000000000031",
    project: "c6200000-0000-4000-8000-000000000031",
    slug: "c6-relay",
};
const WAIT: Duration = Duration::from_secs(20);

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    ids: &'static Ids,
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
}

impl Fixture {
    fn new() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        Self::with(&B1)
    }

    fn with(ids: &'static Ids) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
        else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; cloud collaboration backend probe skipped");
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
        let pool = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            let mut transaction = begin_tenant_transaction(&pool, ids.tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
            let statements = [
                "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'user', 'relay probe') ON CONFLICT DO NOTHING",
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $5, 'relay probe', 'test', 'local') ON CONFLICT DO NOTHING",
                "INSERT INTO workspaces (id, tenant_id, name) VALUES ($3::uuid, $1::uuid, 'relay probe') ON CONFLICT DO NOTHING",
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($4::uuid, $1::uuid, $3::uuid, 'relay probe') ON CONFLICT DO NOTHING",
            ];
            for statement in statements {
                sqlx::query(statement)
                    .bind(ids.tenant)
                    .bind(ids.actor)
                    .bind(ids.workspace)
                    .bind(ids.project)
                    .bind(ids.slug)
                    .execute(transaction.connection()).await?;
            }
            transaction.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(pool)
        })?;
        Ok(Some(Self { ids, runtime, pool, database_url, objects }))
    }

    fn tenant(&self) -> TenantId {
        self.ids.tenant.parse().unwrap()
    }

    fn document(&self) -> CloudDocumentId {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let id = CloudDocumentId::random();
            let workspace: WorkspaceId = self.ids.workspace.parse().unwrap();
            let project: ProjectId = self.ids.project.parse().unwrap();
            create_document(&mut transaction, id, workspace, project, "relay probe doc").await.unwrap();
            transaction.commit().await.unwrap();
            id
        })
    }

    /// A link to `document`: its id and secret.
    fn link(&self, document: CloudDocumentId, role: ShareRole, expires_in: Option<u64>) -> (figcraft_cloud_domain::Uuid, String) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let link = create_share_link(&mut transaction, document, role, None, self.ids.actor.parse().unwrap(), expires_in).await.unwrap();
            transaction.commit().await.unwrap();
            (link.id, link.secret)
        })
    }

    fn config(&self) -> CloudBackendConfig {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config
    }

    fn backend(&self) -> CloudBackend {
        CloudBackend::connect(&self.config()).unwrap()
    }

    /// A hub with a revocation sweeper re-checking its links every `interval`.
    fn serve_swept(&self, interval: Duration) -> (Hosting, RevocationSweeper) {
        let backend = self.backend();
        let watch = backend.revocation_watch();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let hosting = Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap();
        let sweeper = RevocationSweeper::start(&self.config(), watch, hosting.addr(), interval).unwrap();
        (hosting, sweeper)
    }

    fn serve(&self) -> Hosting {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        Hosting::serve(listener, self.backend(), NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap()
    }

    /// The committed sequence and the events' sequences.
    fn events(&self, document: CloudDocumentId) -> (i64, Vec<i64>) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let page = read_events(&mut transaction, document, 0, 100, 1 << 20).await.unwrap();
            transaction.rollback().await.unwrap();
            (page.current_seq, page.events.iter().map(|event| event.seq).collect())
        })
    }

    fn snapshot_seqs(&self, document: CloudDocumentId) -> Vec<i64> {
        self.runtime.block_on(async {
            sqlx::query_scalar("SELECT seq FROM document_snapshots WHERE document_id = $1::uuid ORDER BY seq")
                .bind(document.to_string())
                .fetch_all(&self.pool)
                .await
                .unwrap()
        })
    }
}

/// A raw protocol client over loopback (the path is the gateway's, which the hub ignores).
struct Raw(WebSocket<TcpStream>);

impl Raw {
    fn join(addr: SocketAddr, file: &str, token: &str) -> Raw {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(&Client::hello(file, token, "b1 probe", ""));
        raw
    }

    fn send(&mut self, msg: &ClientMsg) {
        self.0.send(Message::Binary(Bytes::from(message::encode(msg).unwrap()))).unwrap();
    }

    /// The next server message; `None` once the connection closed.
    fn next(&mut self) -> Option<ServerMsg> {
        loop {
            match self.0.read() {
                Ok(Message::Binary(bytes)) => return Some(message::decode(&bytes, 64 << 20).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// The welcome: client id, sequence, access and the page to edit.
    fn welcome(&mut self) -> (u32, u64, Access, NodeId, String) {
        match self.next() {
            Some(ServerMsg::Welcome { client_id, seq, access, catch_up: CatchUp::Snapshot(changes), .. }) => {
                let page = changes
                    .iter()
                    .find_map(|change| match change {
                        Change::Create { id, parent: None, .. } => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                (client_id, seq, access, page, serde_json::to_string(&changes).unwrap())
            }
            other => panic!("expected a welcome with a snapshot, got {other:?}"),
        }
    }

    fn rename(&mut self, base_seq: u64, batch_id: u64, page: NodeId, name: &str) {
        let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
        self.send(&ClientMsg::Changes { base_seq, batch_id, changes });
    }

    /// Read until this client's batch `batch_id` is confirmed (its sequence), or the connection
    /// ends (`None`).
    fn confirmation(&mut self, me: u32, batch_id: u64) -> Option<u64> {
        loop {
            match self.next()? {
                ServerMsg::Changes(accepted) if accepted.author == me && accepted.batch_id == batch_id => return Some(accepted.seq),
                ServerMsg::Reject { reason, .. } => panic!("batch refused: {reason}"),
                _ => {}
            }
        }
    }

    fn refusal(&mut self) -> Option<String> {
        let mut why = None;
        while let Some(msg) = self.next() {
            if let ServerMsg::Refused { reason } = msg {
                why = Some(reason);
            }
        }
        why
    }
}

#[test]
fn share_links_admit_with_their_role_and_refuse_everything_else_alike() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let document = fixture.document();
    let other = fixture.document();
    let (_, editor) = fixture.link(document, ShareRole::Editor, None);
    let (_, viewer) = fixture.link(document, ShareRole::Viewer, None);
    let (_, elsewhere) = fixture.link(other, ShareRole::Editor, None);
    let (revoked_id, revoked) = fixture.link(document, ShareRole::Editor, None);
    let (expired_id, expired) = fixture.link(document, ShareRole::Editor, Some(3600));
    fixture.runtime.block_on(async {
        let mut transaction = begin_tenant_transaction(&fixture.pool, fixture.tenant()).await.unwrap();
        assert!(revoke_share_link(&mut transaction, revoked_id).await.unwrap());
        transaction.commit().await.unwrap();
        sqlx::query("UPDATE share_links SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE id = $1::uuid")
            .bind(expired_id.to_string())
            .execute(&fixture.pool)
            .await
            .unwrap();
    });

    let mut backend = fixture.backend();
    let file = document.to_string();
    assert_eq!(backend.admit(&file, &editor)?.access, Access::Edit);
    assert_eq!(backend.admit(&file, &viewer)?.access, Access::View);
    for token in [revoked.as_str(), expired.as_str(), elsewhere.as_str(), "", "not-a-share-secret"] {
        assert_eq!(backend.admit(&file, token), Err(REFUSED.to_string()));
    }
    assert_eq!(backend.admit("not-a-document", &editor), Err(REFUSED.to_string()));
    assert_eq!(backend.admit(&other.to_string(), &editor), Err(REFUSED.to_string()));
    // Nothing was opened by admitting.
    assert_eq!(backend.open_documents(), 0);
    Ok(())
}

#[test]
fn confirmed_batches_are_durable_and_survive_a_restart() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let document = fixture.document();
    let file = document.to_string();
    let (_, editor) = fixture.link(document, ShareRole::Editor, None);
    let (_, viewer) = fixture.link(document, ShareRole::Viewer, None);

    let mut hosting = fixture.serve();
    let mut ada = Raw::join(hosting.addr(), &file, &editor);
    let (me, seq, access, page, _) = ada.welcome();
    assert_eq!((seq, access), (0, Access::Edit));
    // The new document's first state was stored before anyone edited it.
    assert_eq!(fixture.snapshot_seqs(document), vec![0]);

    ada.rename(seq, 1, page, "Durable one");
    assert_eq!(ada.confirmation(me, 1), Some(1));
    // Confirmed means committed: the event is in the log by the time the confirmation arrived.
    assert_eq!(fixture.events(document), (1, vec![1]));
    ada.rename(1, 2, page, "Durable two");
    assert_eq!(ada.confirmation(me, 2), Some(2));
    assert_eq!(fixture.events(document), (2, vec![1, 2]));

    // A viewer joins the same document and sees the edits; its own edits are refused.
    let mut bea = Raw::join(hosting.addr(), &file, &viewer);
    let (_, seq, access, _, snapshot) = bea.welcome();
    assert_eq!((seq, access), (2, Access::View));
    assert!(snapshot.contains("Durable two"));
    drop(bea);

    // The cloud process restarts: the hub stops (storing a snapshot) and a new one opens the
    // document from PostgreSQL and S3.
    drop(ada);
    hosting.stop();
    drop(hosting);
    let snapshots = fixture.snapshot_seqs(document);
    assert_eq!(snapshots.last(), Some(&2), "a snapshot at the last committed seq: {snapshots:?}");

    let mut hosting = fixture.serve();
    let mut cleo = Raw::join(hosting.addr(), &file, &editor);
    let (me, seq, _, page, snapshot) = cleo.welcome();
    assert_eq!(seq, 2);
    assert!(snapshot.contains("Durable two"));
    cleo.rename(seq, 1, page, "After restart");
    assert_eq!(cleo.confirmation(me, 1), Some(3));
    assert_eq!(fixture.events(document), (3, vec![1, 2, 3]));
    drop(cleo);
    hosting.stop();
    Ok(())
}

#[test]
fn a_fenced_commit_closes_the_document_without_confirming() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let document = fixture.document();
    let file = document.to_string();
    let (_, editor) = fixture.link(document, ShareRole::Editor, None);

    let mut hosting = fixture.serve();
    let mut ada = Raw::join(hosting.addr(), &file, &editor);
    let (me, seq, _, page, _) = ada.welcome();

    // Another sequencer takes the document over: this relay's token is stale from now on.
    fixture.runtime.block_on(async {
        let mut transaction = begin_tenant_transaction(&fixture.pool, fixture.tenant()).await.unwrap();
        claim_sequencer(&mut transaction, document).await.unwrap();
        transaction.commit().await.unwrap();
    });

    ada.rename(seq, 1, page, "Never confirmed");
    // The connection closes (everyone reconnects) and the batch is never confirmed.
    assert_eq!(ada.confirmation(me, 1), None);
    assert_eq!(fixture.events(document), (0, vec![]));

    // Reconnecting opens the document again from what is durable.
    let mut again = Raw::join(hosting.addr(), &file, &editor);
    let (_, seq, _, _, snapshot) = again.welcome();
    assert_eq!(seq, 0);
    assert!(!snapshot.contains("Never confirmed"));
    drop(again);
    hosting.stop();

    // A refused hello reads the same as any other.
    let mut hosting = fixture.serve();
    let mut stranger = Raw::join(hosting.addr(), &file, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    assert_eq!(stranger.refusal().as_deref(), Some(REFUSED));
    hosting.stop();
    Ok(())
}

/// Read until the connection closes: whether the hub closed it saying the link this client joined
/// with stopped working.
fn stopped(raw: &mut Raw) -> bool {
    loop {
        match raw.0.read() {
            Ok(Message::Close(frame)) => return frame.is_some_and(|frame| frame.reason.as_str() == STOPPED),
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

#[test]
fn a_revoked_link_disconnects_its_live_clients_and_refuses_new_hellos() -> TestResult {
    let Some(fixture) = Fixture::with(&C6)? else {
        return Ok(());
    };
    let document = fixture.document();
    let file = document.to_string();
    let (revoked_id, revoked) = fixture.link(document, ShareRole::Editor, None);
    let (_, kept) = fixture.link(document, ShareRole::Viewer, None);

    let (mut hosting, sweeper) = fixture.serve_swept(Duration::from_millis(100));
    let mut ada = Raw::join(hosting.addr(), &file, &revoked);
    let (me, seq, access, page, _) = ada.welcome();
    assert_eq!(access, Access::Edit);
    let mut bob = Raw::join(hosting.addr(), &file, &kept);
    let _ = bob.welcome();
    // Live links are left alone across several sweeps.
    std::thread::sleep(Duration::from_millis(400));
    ada.rename(seq, 1, page, "Still editing");
    assert!(ada.confirmation(me, 1).is_some());

    fixture.runtime.block_on(async {
        let mut transaction = begin_tenant_transaction(&fixture.pool, fixture.tenant()).await.unwrap();
        assert!(revoke_share_link(&mut transaction, revoked_id).await.unwrap());
        transaction.commit().await.unwrap();
    });
    let revoked_at = std::time::Instant::now();
    assert!(stopped(&mut ada), "the revoked link's client is told sharing stopped");
    assert!(revoked_at.elapsed() < Duration::from_secs(5), "disconnected within a bounded time: {:?}", revoked_at.elapsed());

    // A new hello with the revoked link is refused like any other; the kept link still joins.
    let mut again = Raw::join(hosting.addr(), &file, &revoked);
    assert_eq!(again.refusal().as_deref(), Some(REFUSED));
    let mut carol = Raw::join(hosting.addr(), &file, &kept);
    assert_eq!(carol.welcome().2, Access::View);

    // Archiving the document closes the remaining sessions the same way.
    fixture.runtime.block_on(async {
        let mut transaction = begin_tenant_transaction(&fixture.pool, fixture.tenant()).await.unwrap();
        assert!(archive_document(&mut transaction, document).await.unwrap().is_some());
        transaction.commit().await.unwrap();
    });
    assert!(stopped(&mut bob), "an archived document's clients are disconnected");
    assert!(stopped(&mut carol), "an archived document's clients are disconnected");
    let mut late = Raw::join(hosting.addr(), &file, &kept);
    assert_eq!(late.refusal().as_deref(), Some(REFUSED));
    drop(sweeper);
    hosting.stop();
    Ok(())
}
