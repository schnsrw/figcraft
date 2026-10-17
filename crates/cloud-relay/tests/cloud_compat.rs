//! S5-019: the app's own network client (an engine session with the network link, the same code
//! that joins a desktop host or a `figcraft-server`) joins the cloud relay from a cloud invite
//! (`ws://…/ws/v1/collab?file=<uuid>&token=<secret>`) and from that link re-shared as
//! `figcraft://join?server=…`; edits converge and are committed; the relay restarts and the clients
//! reconnect and catch up; a viewer link is read-only. Runs against the dev PostgreSQL and S3
//! (fixture prefix f2; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR).
//! The matrix: `docs/saas/session-compatibility.md`.
#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::read_events;
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link};
use figcraft_collab_net::guest::GuestConfig;
use figcraft_collab_net::hub::Hosting;
use figcraft_collab_net::{Invite, NetLimits, NetLink};
use figcraft_engine::Session;
use figcraft_engine::live::Status;
use serde_json::{Value, json};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "f2000000-0000-4000-8000-000000000041";
const ACTOR: &str = "f2000000-0000-4000-8000-000000000049";
const WORKSPACE: &str = "f2100000-0000-4000-8000-000000000041";
const PROJECT: &str = "f2200000-0000-4000-8000-000000000041";
const SLUG: &str = "f2-compat";
const WAIT: Duration = Duration::from_secs(30);

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
}

impl Fixture {
    fn new() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
        else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; cloud compatibility probe skipped");
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
            let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
            let statements = [
                "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'user', 'compat probe') ON CONFLICT DO NOTHING",
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $5, 'compat probe', 'test', 'local') ON CONFLICT DO NOTHING",
                "INSERT INTO workspaces (id, tenant_id, name) VALUES ($3::uuid, $1::uuid, 'compat probe') ON CONFLICT DO NOTHING",
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($4::uuid, $1::uuid, $3::uuid, 'compat probe') ON CONFLICT DO NOTHING",
            ];
            for statement in statements {
                sqlx::query(statement).bind(TENANT).bind(ACTOR).bind(WORKSPACE).bind(PROJECT).bind(SLUG).execute(transaction.connection()).await?;
            }
            transaction.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(pool)
        })?;
        Ok(Some(Self { runtime, pool, database_url, objects }))
    }

    fn tenant(&self) -> TenantId {
        TENANT.parse().unwrap()
    }

    fn document(&self) -> CloudDocumentId {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let id = CloudDocumentId::random();
            let workspace: WorkspaceId = WORKSPACE.parse().unwrap();
            let project: ProjectId = PROJECT.parse().unwrap();
            create_document(&mut transaction, id, workspace, project, "compat probe doc").await.unwrap();
            transaction.commit().await.unwrap();
            id
        })
    }

    fn secret(&self, document: CloudDocumentId, role: ShareRole) -> String {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let link = create_share_link(&mut transaction, document, role, None, ACTOR.parse().unwrap(), None).await.unwrap();
            transaction.commit().await.unwrap();
            link.secret
        })
    }

    /// The committed sequence.
    fn committed(&self, document: CloudDocumentId) -> i64 {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant()).await.unwrap();
            let page = read_events(&mut transaction, document, 0, 100, 1 << 20).await.unwrap();
            transaction.rollback().await.unwrap();
            page.current_seq
        })
    }

    /// The relay on `addr` (port 0: any free one; a fixed port may take a moment to come free).
    fn serve(&self, addr: SocketAddr) -> Hosting {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        let start = Instant::now();
        let listener = loop {
            match TcpListener::bind(addr) {
                Ok(l) => break l,
                Err(e) if start.elapsed() < Duration::from_secs(10) => {
                    eprintln!("waiting for {addr}: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("{e}"),
            }
        };
        let backend = CloudBackend::connect(&config).unwrap();
        Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap()
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

fn join(link: &str, name: &str) -> Session {
    let mut s = session(name);
    s.execute("collab.join", &json!({ "link": link })).unwrap();
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

fn id_of(v: &Value) -> figcraft_doc::NodeId {
    figcraft_doc::NodeId(v["id"].as_u64().unwrap())
}

#[test]
fn the_app_client_joins_the_cloud_relay_from_a_cloud_invite() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let document = fixture.document();
    let editor = fixture.secret(document, ShareRole::Editor);
    let viewer = fixture.secret(document, ShareRole::Viewer);
    let mut hosting = fixture.serve(SocketAddr::from(([127, 0, 0, 1], 0)));
    let addr = hosting.addr();

    // The invite the cloud API hands out, and the same session re-shared as a figcraft:// link.
    let cloud = format!("ws://{addr}/ws/v1/collab?file={document}&token={editor}");
    let invite = Invite::parse(&cloud)?;
    assert_eq!(invite.server.path, "/ws/v1/collab/");
    assert!(invite.link().starts_with("figcraft://join?server=ws://"), "a cloud link never takes the desktop host= form");
    let mut ada = join(&cloud, "Ada");
    let mut bo = join(&invite.link(), "Bo");
    pump(&mut [&mut ada, &mut bo], "both in", |s| s.iter().all(|x| live(x)) && s[0].live().peers.len() == 1);
    assert_eq!(ada.live_summary()["access"], "edit");
    assert!(same(ada.doc().unwrap(), bo.doc().unwrap()));

    // Edits converge, and what was confirmed is committed.
    let rect = id_of(&ada.execute("shape.rectangle", &json!({ "x": 10, "y": 10, "width": 40, "height": 40 }))?);
    pump(&mut [&mut ada, &mut bo], "Ada's rectangle at Bo", |s| s[1].doc().unwrap().contains(rect));
    bo.execute("node.set", &json!({ "ids": [rect.0], "props": { "name": "Cloud card" } }))?;
    pump(&mut [&mut ada, &mut bo], "Bo's rename at Ada", |s| s[0].doc().unwrap().node(rect).is_some_and(|n| n.name == "Cloud card"));
    let committed = fixture.committed(document);
    assert!(committed >= 2, "both batches committed before they were confirmed: {committed}");

    // The relay restarts on the same address: the clients reconnect and catch up, and what Bo
    // made meanwhile goes through.
    hosting.stop();
    drop(hosting);
    pump(&mut [&mut ada, &mut bo], "the drop", |s| s.iter().all(|x| matches!(x.live().status, Status::Reconnecting { .. })));
    let offline = id_of(&bo.execute("shape.ellipse", &json!({ "x": 100, "y": 10, "width": 30, "height": 30 }))?);
    let mut hosting = fixture.serve(addr);
    pump(&mut [&mut ada, &mut bo], "both back with everything", |s| {
        s.iter().all(|x| live(x) && x.doc().unwrap().contains(offline) && x.doc().unwrap().node(rect).is_some_and(|n| n.name == "Cloud card"))
    });
    assert!(same(ada.doc().unwrap(), bo.doc().unwrap()));
    assert!(fixture.committed(document) > committed);

    // A viewer link joins read-only through the same client.
    let mut cy = join(&format!("ws://{addr}/ws/v1/collab?file={document}&token={viewer}"), "Cy");
    pump(&mut [&mut ada, &mut bo, &mut cy], "Cy in", |s| live(s[2]) && s[2].doc().unwrap().contains(offline));
    assert_eq!(cy.live_summary()["access"], "view");
    assert!(cy.execute("shape.rectangle", &json!({ "x": 0, "y": 0, "width": 5, "height": 5 })).is_err());
    drop((ada, bo, cy));
    hosting.stop();
    Ok(())
}
