//! S4-019: the relay admits a hello whose token is a realtime ticket, through the real hub
//! (fixture prefix e1, random tails per run; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE and
//! FIGCRAFT_TEST_S3_DIR). An editor's ticket joins with edit access and its batches are recorded
//! as made by that principal; a viewer's ticket joins read-only; a ticket for another document, an
//! expired one, a forged one, one under a revoked session, or any ticket on a relay without the
//! key is refused like a bad link; share links keep working beside tickets; and a ticket-admitted
//! member removed from the tenant is disconnected by the revocation sweep.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED, RevocationSweeper};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::{add_tenant_member, remove_tenant_member, set_workspace_member};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::realtime_tickets::{MAX_TICKET_SECS, RealtimeTicket, TicketKey, expiry, mint};
use figcraft_cloud_storage::sessions::{SessionLimits, create_session, logout};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link};
use figcraft_collab::message::{self, Access, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::{Hosting, STOPPED};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const WAIT: Duration = Duration::from_secs(20);
const SECRET: &[u8] = b"e1-relay-ticket-signing-secret";

fn fresh(kind: u8) -> Uuid {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("e1{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

struct Member {
    principal: PrincipalId,
    session: Uuid,
    token: String,
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
    other_document: CloudDocumentId,
}

impl Fixture {
    fn new() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
        else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; realtime ticket probe skipped");
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
        let other_document: CloudDocumentId = fresh(0x31).to_string().parse()?;
        let owner: PrincipalId = fresh(0x01).to_string().parse()?;
        let pool = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e1 relay owner')")
                .bind(owner.to_string())
                .execute(&pool)
                .await?;
            let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
            sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'e1 relay', 'test', 'local')")
                .bind(tenant.to_string())
                .bind(format!("e1-relay-{}", &tenant.to_string()[24..]))
                .execute(transaction.connection())
                .await?;
            add_tenant_member(&mut transaction, owner, Role::TenantOwner).await.map_err(|error| format!("{error:?}"))?;
            create_workspace(&mut transaction, workspace, "e1 relay", None).await.map_err(|error| format!("{error:?}"))?;
            create_project(&mut transaction, project, workspace, "e1 relay", None).await.map_err(|error| format!("{error:?}"))?;
            create_document(&mut transaction, document, workspace, project, "e1 relay doc").await.map_err(|error| format!("{error:?}"))?;
            create_document(&mut transaction, other_document, workspace, project, "e1 other doc").await.map_err(|error| format!("{error:?}"))?;
            transaction.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(pool)
        })?;
        Ok(Some(Self { runtime, pool, database_url, objects, tenant, owner, workspace, document, other_document }))
    }

    /// A tenant member with `workspace_role` on the workspace and a fresh browser session.
    fn member(&self, workspace_role: Role) -> Member {
        self.runtime.block_on(async {
            let principal: PrincipalId = fresh(0x01).to_string().parse().unwrap();
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'e1 relay member')")
                .bind(principal.to_string())
                .execute(&self.pool)
                .await
                .unwrap();
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            add_tenant_member(&mut transaction, principal, Role::Editor).await.unwrap();
            set_workspace_member(&mut transaction, self.workspace, principal, workspace_role).await.unwrap();
            transaction.commit().await.unwrap();
            let session = create_session(&self.pool, principal, SessionLimits::default()).await.unwrap();
            Member { principal, session: session.id, token: session.token }
        })
    }

    fn ticket(&self, member: &Member, document: CloudDocumentId, edit: bool, expires_at: u64) -> String {
        let key = TicketKey::derive(SECRET).unwrap();
        mint(&key, &RealtimeTicket { tenant: self.tenant, document, principal: member.principal, session: member.session, edit, expires_at })
    }

    fn config(&self, keyed: bool) -> CloudBackendConfig {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config.ticket_key = keyed.then(|| TicketKey::derive(SECRET).unwrap());
        config
    }

    fn serve(&self, keyed: bool) -> (Hosting, RevocationSweeper) {
        let backend = CloudBackend::connect(&self.config(keyed)).unwrap();
        let watch = backend.revocation_watch();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let hosting = Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap();
        let sweeper = RevocationSweeper::start(&self.config(keyed), watch, hosting.addr(), Duration::from_millis(100)).unwrap();
        (hosting, sweeper)
    }

    fn actors(&self) -> Vec<String> {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            let rows: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT actor_principal_id::text FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid",
            )
            .bind(self.tenant.to_string())
            .bind(self.document.to_string())
            .fetch_all(transaction.connection())
            .await
            .unwrap();
            transaction.rollback().await.unwrap();
            rows
        })
    }

    fn attributions(&self) -> Vec<(i64, String)> {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.unwrap();
            let rows = sqlx::query_as(
                "SELECT seq, actor_principal_id::text FROM document_events \
                 WHERE tenant_id = $1::uuid AND document_id = $2::uuid ORDER BY seq",
            )
            .bind(self.tenant.to_string())
            .bind(self.document.to_string())
            .fetch_all(transaction.connection())
            .await
            .unwrap();
            transaction.rollback().await.unwrap();
            rows
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
        raw.send(&Client::hello(file, token, "e1 probe", ""));
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
fn tickets_admit_by_their_access_and_share_links_keep_working() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let file = fixture.document.to_string();
    let editor = fixture.member(Role::Editor);
    let second_editor = fixture.member(Role::Editor);
    let viewer = fixture.member(Role::Viewer);
    let removed = fixture.member(Role::Editor);
    let logged_out = fixture.member(Role::Editor);
    let expires = expiry(now(), MAX_TICKET_SECS);

    let (mut hosting, sweeper) = fixture.serve(true);
    let addr = hosting.addr();

    // An editor's ticket joins with edit access; its batches are recorded as the editor's.
    let mut ada = Raw::join(addr, &file, &fixture.ticket(&editor, fixture.document, true, expires));
    let (ada_id, seq, access, page) = ada.welcome();
    assert_eq!(access, Access::Edit);
    ada.rename(seq, 1, page, "Ticketed edit");
    assert!(ada.confirmation(ada_id, 1).is_some());
    assert_eq!(fixture.actors(), vec![editor.principal.to_string()], "the ticket's principal is the batch actor");

    // The room stays open while a second editor joins. Each committed event must carry the
    // principal whose ticket admitted its author, including when the first editor edits again.
    let mut ellis = Raw::join(addr, &file, &fixture.ticket(&second_editor, fixture.document, true, expires));
    let (ellis_id, second_seq, second_access, second_page) = ellis.welcome();
    assert_eq!(second_access, Access::Edit);
    ellis.rename(second_seq, 1, second_page, "Second editor");
    let third_seq = ellis.confirmation(ellis_id, 1).ok_or("second editor was not confirmed")?;
    ada.rename(third_seq, 2, page, "First editor again");
    assert!(ada.confirmation(ada_id, 2).is_some());
    assert_eq!(
        fixture.attributions(),
        vec![(1, editor.principal.to_string()), (2, second_editor.principal.to_string()), (3, editor.principal.to_string()),],
    );

    // A viewer's ticket joins read-only; a viewer cannot gain edit by holding an edit ticket
    // (forged for the test with the right key: the relay re-checks the role and refuses).
    let mut ben = Raw::join(addr, &file, &fixture.ticket(&viewer, fixture.document, false, expires));
    assert_eq!(ben.welcome().2, Access::View);
    assert_eq!(Raw::join(addr, &file, &fixture.ticket(&viewer, fixture.document, true, expires)).refusal().as_deref(), Some(REFUSED));

    // Another document's ticket, an expired one and a tampered one read like a bad link.
    let other = fixture.ticket(&editor, fixture.other_document, true, expires);
    assert_eq!(Raw::join(addr, &file, &other).refusal().as_deref(), Some(REFUSED));
    let expired = fixture.ticket(&editor, fixture.document, true, now().saturating_sub(1));
    assert_eq!(Raw::join(addr, &file, &expired).refusal().as_deref(), Some(REFUSED));
    let mut tampered = fixture.ticket(&editor, fixture.document, true, expires).into_bytes();
    let last = tampered.len() - 1;
    tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
    assert_eq!(Raw::join(addr, &file, &String::from_utf8(tampered)?).refusal().as_deref(), Some(REFUSED));

    // A ticket minted under a session that has since been logged out is refused.
    let pool = fixture.pool.clone();
    let ticket = fixture.ticket(&logged_out, fixture.document, true, expires);
    assert!(fixture.runtime.block_on(logout(&pool, &logged_out.token))?);
    assert_eq!(Raw::join(addr, &file, &ticket).refusal().as_deref(), Some(REFUSED));

    // Share links still admit beside tickets.
    let link = fixture.runtime.block_on(async {
        let mut transaction = begin_tenant_transaction(&pool, fixture.tenant).await.unwrap();
        let link = create_share_link(&mut transaction, fixture.document, ShareRole::Viewer, Some("e1"), fixture.owner, None).await.unwrap();
        transaction.commit().await.unwrap();
        link
    });
    let mut cat = Raw::join(addr, &file, &link.secret);
    assert_eq!(cat.welcome().2, Access::View);

    // A ticket-admitted member removed from the tenant is disconnected by the sweep.
    let mut dan = Raw::join(addr, &file, &fixture.ticket(&removed, fixture.document, true, expires));
    assert_eq!(dan.welcome().2, Access::Edit);
    let principal = removed.principal;
    fixture.runtime.block_on(async {
        let mut transaction = begin_tenant_transaction(&pool, fixture.tenant).await.unwrap();
        remove_tenant_member(&mut transaction, principal).await.unwrap();
        transaction.commit().await.unwrap();
    });
    let revoked_at = Instant::now();
    assert!(stopped(&mut dan), "a removed member admitted by ticket is told access stopped");
    assert!(revoked_at.elapsed() < Duration::from_secs(5), "{:?}", revoked_at.elapsed());
    assert_eq!(Raw::join(addr, &file, &fixture.ticket(&removed, fixture.document, true, expires)).refusal().as_deref(), Some(REFUSED));

    drop((ada, ben, cat));
    drop(sweeper);
    hosting.stop();

    // A relay without the ticket key refuses every ticket.
    let (mut unkeyed, sweeper) = fixture.serve(false);
    let ticket = fixture.ticket(&editor, fixture.document, true, expiry(now(), MAX_TICKET_SECS));
    assert_eq!(Raw::join(unkeyed.addr(), &file, &ticket).refusal().as_deref(), Some(REFUSED));
    drop(sweeper);
    unkeyed.stop();
    Ok(())
}
