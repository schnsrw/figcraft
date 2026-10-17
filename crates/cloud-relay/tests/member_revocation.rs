//! S4-017: signed-in members in the relay lose their live connection when their access goes
//! (fixture prefix d5, random tails per run; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE and
//! FIGCRAFT_TEST_S3_DIR). A member removed from the tenant, one removed from the team whose grant
//! let them in, and one downgraded from editor are each disconnected within a bounded time by the
//! revocation sweep; their next hello and API request are refused, and a member who kept access
//! keeps editing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED, RevocationSweeper};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::{add_tenant_member, remove_tenant_member, set_workspace_member, set_workspace_team_grant};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::revocation::{AccessRevocation, RevocationError, apply_revocation, authorize_session_in_tenant};
use figcraft_cloud_storage::sessions::{SessionLimits, create_session};
use figcraft_cloud_storage::teams::{TeamRole, add_team_member, create_team, remove_team_member};
use figcraft_collab::message::{self, Access, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::{Hosting, STOPPED};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const WAIT: Duration = Duration::from_secs(20);

fn fresh(kind: u8) -> Uuid {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("d5{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    document: CloudDocumentId,
}

impl Fixture {
    fn new() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
        else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; member revocation probe skipped");
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
        let tenant: TenantId = fresh(0x00).to_string().parse()?;
        let workspace: WorkspaceId = fresh(0x10).to_string().parse()?;
        let project: ProjectId = fresh(0x20).to_string().parse()?;
        let document: CloudDocumentId = fresh(0x30).to_string().parse()?;
        let owner: PrincipalId = fresh(0x01).to_string().parse()?;
        let pool = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd5 relay owner')")
                .bind(owner.to_string())
                .execute(&pool)
                .await?;
            let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
            sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'd5 relay', 'test', 'local')")
                .bind(tenant.to_string())
                .bind(format!("d5-relay-{}", &tenant.to_string()[24..]))
                .execute(transaction.connection())
                .await?;
            add_tenant_member(&mut transaction, owner, Role::TenantOwner).await.map_err(|error| format!("{error:?}"))?;
            create_workspace(&mut transaction, workspace, "d5 relay", None).await.map_err(|error| format!("{error:?}"))?;
            create_project(&mut transaction, project, workspace, "d5 relay", None).await.map_err(|error| format!("{error:?}"))?;
            create_document(&mut transaction, document, workspace, project, "d5 relay doc").await.map_err(|error| format!("{error:?}"))?;
            transaction.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(pool)
        })?;
        Ok(Some(Self { runtime, pool, database_url, objects, tenant, owner, workspace, document }))
    }

    /// A tenant member (`editor` tenant role) with `workspace_role` on the workspace, and a member
    /// hello token for a fresh session of theirs.
    fn member(&self, workspace_role: Option<Role>) -> (PrincipalId, String, String) {
        self.runtime.block_on(async {
            let principal: PrincipalId = fresh(0x01).to_string().parse().unwrap();
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd5 relay member')")
                .bind(principal.to_string())
                .execute(&self.pool)
                .await
                .unwrap();
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            add_tenant_member(&mut transaction, principal, Role::Editor).await.unwrap();
            if let Some(role) = workspace_role {
                set_workspace_member(&mut transaction, self.workspace, principal, role).await.unwrap();
            }
            transaction.commit().await.unwrap();
            let session = create_session(&self.pool, principal, SessionLimits::default()).await.unwrap();
            let hello = format!("member.{}.{}", self.tenant, session.token);
            (principal, session.token, hello)
        })
    }

    fn config(&self) -> CloudBackendConfig {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config
    }

    fn serve_swept(&self, interval: Duration) -> (Hosting, RevocationSweeper) {
        let backend = CloudBackend::connect(&self.config()).unwrap();
        let watch = backend.revocation_watch();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let hosting = Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap();
        let sweeper = RevocationSweeper::start(&self.config(), watch, hosting.addr(), interval).unwrap();
        (hosting, sweeper)
    }

    fn change(&self, change: impl AsyncFnOnce(&mut figcraft_cloud_storage::TenantTransaction<'_>)) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            change(&mut transaction).await;
            transaction.commit().await.unwrap();
        });
    }

    fn api_refuses(&self, token: &str) -> bool {
        self.runtime.block_on(authorize_session_in_tenant(&self.pool, token, SessionLimits::default(), self.tenant)).map(|_| ())
            == Err(RevocationError::Refused)
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

    /// The welcome: client id, sequence, access and the page.
    fn welcome(&mut self) -> (u32, u64, Access, NodeId) {
        match self.next() {
            Some(ServerMsg::Welcome { client_id, seq, access, catch_up: figcraft_collab::message::CatchUp::Snapshot(changes), .. }) => {
                let page = changes
                    .iter()
                    .find_map(|change| match change {
                        Change::Create { id, parent: None, .. } => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                (client_id, seq, access, page)
            }
            other => panic!("expected a welcome with a snapshot, got {other:?}"),
        }
    }

    fn rename(&mut self, base_seq: u64, batch_id: u64, page: NodeId, name: &str) {
        let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
        self.send(&ClientMsg::Changes { base_seq, batch_id, changes });
    }

    /// Read until this client's batch is confirmed (its sequence); `None` if the connection ends.
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

/// Read until the connection closes: whether the hub closed it as revoked.
fn stopped(raw: &mut Raw) -> bool {
    loop {
        match raw.0.read() {
            Ok(Message::Close(frame)) => return frame.is_some_and(|frame| frame.reason.as_str() == STOPPED),
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

fn within(started: Instant, what: &str) {
    assert!(started.elapsed() < Duration::from_secs(5), "{what} disconnected within a bounded time: {:?}", started.elapsed());
}

#[test]
fn removed_members_are_disconnected_and_refused_while_others_keep_editing() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let file = fixture.document.to_string();
    let (direct, direct_token, direct_hello) = fixture.member(Some(Role::Editor));
    let (teamed, teamed_token, teamed_hello) = fixture.member(None);
    let (_, downgraded_token, downgraded_hello) = fixture.member(Some(Role::Editor));
    let (_, kept_token, kept_hello) = fixture.member(Some(Role::Editor));
    let (_, _, outsider_hello) = fixture.member(None);
    let team = fresh(0x40);
    let owner = fixture.owner;
    let workspace = fixture.workspace;
    fixture.change(async move |tx| {
        create_team(tx, team, &format!("d5-relay-{}", &team.to_string()[24..]), "d5 relay team", owner).await.unwrap();
        add_team_member(tx, team, teamed, TeamRole::Editor, owner).await.unwrap();
        set_workspace_team_grant(tx, workspace, team, Role::Editor).await.unwrap();
    });

    let (mut hosting, sweeper) = fixture.serve_swept(Duration::from_millis(100));
    let addr = hosting.addr();
    let mut ada = Raw::join(addr, &file, &direct_hello);
    let (_, _, access, _) = ada.welcome();
    assert_eq!(access, Access::Edit);
    let mut ben = Raw::join(addr, &file, &teamed_hello);
    assert_eq!(ben.welcome().2, Access::Edit);
    let mut cat = Raw::join(addr, &file, &downgraded_hello);
    assert_eq!(cat.welcome().2, Access::Edit);
    let mut kim = Raw::join(addr, &file, &kept_hello);
    let (kim_id, mut seq, access, page) = kim.welcome();
    assert_eq!(access, Access::Edit);
    // A member with no role on the document, or a bad session, is refused like a bad link.
    assert_eq!(Raw::join(addr, &file, &outsider_hello).refusal().as_deref(), Some(REFUSED));
    let forged = format!("member.{}.{}", fixture.tenant, "A".repeat(43));
    assert_eq!(Raw::join(addr, &file, &forged).refusal().as_deref(), Some(REFUSED));
    // Live members are left alone across several sweeps.
    std::thread::sleep(Duration::from_millis(400));
    kim.rename(seq, 1, page, "Still editing");
    seq = kim.confirmation(kim_id, 1).unwrap();

    // Removed from the tenant: disconnected, the next hello and the next API request refused.
    assert!(!fixture.api_refuses(&direct_token));
    fixture.change(async move |tx| remove_tenant_member(tx, direct).await.unwrap());
    let revoked_at = Instant::now();
    assert!(stopped(&mut ada), "a member removed from the tenant is told access stopped");
    within(revoked_at, "a removed tenant member");
    assert!(fixture.api_refuses(&direct_token), "the removed member's next API request is refused");
    let pool = fixture.pool.clone();
    fixture.runtime.block_on(apply_revocation(&pool, AccessRevocation::TenantMemberRemoved { principal: direct }))?;
    assert_eq!(Raw::join(addr, &file, &direct_hello).refusal().as_deref(), Some(REFUSED));

    // Removed from the team whose grant let them in.
    fixture.change(async move |tx| assert!(remove_team_member(tx, team, teamed, owner).await.unwrap()));
    let revoked_at = Instant::now();
    assert!(stopped(&mut ben), "a member removed from the granting team is disconnected");
    within(revoked_at, "a removed team member");
    assert_eq!(Raw::join(addr, &file, &teamed_hello).refusal().as_deref(), Some(REFUSED));
    assert!(!fixture.api_refuses(&teamed_token), "still a tenant member: tenant-level requests work");

    // Downgraded from editor to viewer: disconnected, and rejoins with view access only.
    let downgraded = fixture.runtime.block_on(async {
        figcraft_cloud_storage::sessions::resolve_session(&pool, &downgraded_token, SessionLimits::default()).await.unwrap().principal_id
    });
    fixture.change(async move |tx| set_workspace_member(tx, workspace, downgraded, Role::Viewer).await.unwrap());
    let revoked_at = Instant::now();
    assert!(stopped(&mut cat), "a downgraded editor is disconnected");
    within(revoked_at, "a downgraded editor");
    let mut cat = Raw::join(addr, &file, &downgraded_hello);
    assert_eq!(cat.welcome().2, Access::View);

    // The member who kept access was never interrupted and keeps editing.
    kim.rename(seq, 2, page, "Kept editing");
    assert!(kim.confirmation(kim_id, 2).is_some());
    assert!(!fixture.api_refuses(&kept_token));
    drop(cat);
    drop(kim);
    drop(sweeper);
    hosting.stop();
    Ok(())
}
