//! S4-021: the relay action matrix, through the real hub with `CloudBackend` against the dev
//! services (fixture prefix g2, random tails per run; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR).
//!
//! Every admission kind (share links, realtime tickets for each document role, and the credentials
//! that must not work) is driven through every relay action a participant can take (join, the
//! welcome's document state, a property-change batch, presence, a cursor-chat signal and a
//! reconnect with catch-up). What happened is compared with [`EXPECTED`], a table written from the
//! access policy (an editor link or an editor's ticket edits; a viewer link, a viewer's ticket and
//! a commenter's ticket only watch, chat and point; nothing else gets in), not from the code.
//! A watching editor checks that nothing a refused participant sends ever reaches the room.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED, RevocationSweeper};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::{add_tenant_member, remove_tenant_member, set_workspace_member};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::realtime_tickets::{MAX_TICKET_SECS, RealtimeTicket, TicketKey, expiry, mint};
use figcraft_cloud_storage::sessions::{SessionLimits, create_session};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, revoke_share_link};
use figcraft_collab::message::{self, Access, CatchUp, ClientMsg, Hello, Presence, Resume, ServerMsg, Signal};
use figcraft_collab::{Change, Client, Json, NodeId, Prop, VIEW_ONLY};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::Hosting;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const WAIT: Duration = Duration::from_secs(15);
const SECRET: &[u8] = b"g2-relay-action-matrix-signing-secret";

/// What a participant tried.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Join,
    WelcomeState,
    Batch,
    Presence,
    Signal,
    CatchUp,
}

const ACTIONS: [Action; 6] = [Action::Join, Action::WelcomeState, Action::Batch, Action::Presence, Action::Signal, Action::CatchUp];

/// What happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// Joined with edit access.
    Edit,
    /// Joined read-only.
    View,
    Allowed,
    Refused,
}

/// How a participant tries to get in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    EditorLink,
    ViewerLink,
    EditorTicket,
    ViewerTicket,
    /// A commenter's realtime ticket (the API mints commenters view tickets).
    CommenterTicket,
    /// An edit ticket for a viewer (correctly signed): the relay re-checks the role.
    ViewerWithEditTicket,
    /// An edit ticket for a commenter (correctly signed).
    CommenterWithEditTicket,
    RevokedLink,
    ExpiredTicket,
    /// A member of another tenant with a ticket naming that tenant and this document.
    OtherTenantTicket,
    /// A member of another tenant with a ticket claiming this tenant.
    OtherTenantMemberClaimingThisTenant,
    /// A ticket of a member removed from the tenant before joining.
    RemovedMemberTicket,
}

use Outcome::{Allowed as Y, Edit as E, Refused as N, View as V};

/// The independently written expectation, in [`ACTIONS`] order. A refused join refuses everything
/// after it: there is no session to act in.
const EXPECTED: &[(Kind, [Outcome; 6])] = &[
    (Kind::EditorLink, [E, Y, Y, Y, Y, Y]),
    (Kind::ViewerLink, [V, Y, N, Y, Y, Y]),
    (Kind::EditorTicket, [E, Y, Y, Y, Y, Y]),
    (Kind::ViewerTicket, [V, Y, N, Y, Y, Y]),
    (Kind::CommenterTicket, [V, Y, N, Y, Y, Y]),
    (Kind::ViewerWithEditTicket, [N, N, N, N, N, N]),
    (Kind::CommenterWithEditTicket, [N, N, N, N, N, N]),
    (Kind::RevokedLink, [N, N, N, N, N, N]),
    (Kind::ExpiredTicket, [N, N, N, N, N, N]),
    (Kind::OtherTenantTicket, [N, N, N, N, N, N]),
    (Kind::OtherTenantMemberClaimingThisTenant, [N, N, N, N, N, N]),
    (Kind::RemovedMemberTicket, [N, N, N, N, N, N]),
];

fn id<T: std::str::FromStr>(kind: u8) -> T
where
    T::Err: std::fmt::Debug,
{
    // "g2" isn't hex: the g2 fixtures start 0x92 (and carry "g2" in every slug and name).
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) & 0xFFFF_FFFF_FFFF;
    format!("92{kind:02x}0000-0000-4000-8000-{tail:012x}").parse().unwrap()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

struct Member {
    tenant: TenantId,
    principal: PrincipalId,
    session: Uuid,
}

struct Space {
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    document: CloudDocumentId,
}

struct Fixture {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    database_url: String,
    objects: ObjectStoreSettings,
    home: Space,
    other: Space,
}

async fn space(pool: &PgPool, label: &str) -> Result<Space, Box<dyn std::error::Error>> {
    let tenant: TenantId = id(0x00);
    let workspace: WorkspaceId = id(0x10);
    let project: ProjectId = id(0x20);
    let document: CloudDocumentId = id(0x30);
    let owner: PrincipalId = id(0x01);
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'g2 matrix owner')")
        .bind(owner.to_string())
        .execute(pool)
        .await?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'g2 matrix', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("g2-matrix-{label}-{}", &tenant.to_string()[24..]))
        .execute(transaction.connection())
        .await?;
    add_tenant_member(&mut transaction, owner, Role::TenantOwner).await.map_err(|error| format!("{error:?}"))?;
    create_workspace(&mut transaction, workspace, "g2 matrix", None).await.map_err(|error| format!("{error:?}"))?;
    create_project(&mut transaction, project, workspace, "g2 matrix", None).await.map_err(|error| format!("{error:?}"))?;
    create_document(&mut transaction, document, workspace, project, "g2 matrix doc").await.map_err(|error| format!("{error:?}"))?;
    transaction.commit().await?;
    Ok(Space { tenant, owner, workspace, document })
}

impl Fixture {
    fn new() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR"))
        else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; relay action matrix skipped");
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
        let (pool, home, other) = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            let home = space(&pool, "home").await?;
            let other = space(&pool, "other").await?;
            Ok::<_, Box<dyn std::error::Error>>((pool, home, other))
        })?;
        Ok(Some(Self { runtime, pool, database_url, objects, home, other }))
    }

    /// A member of `space`'s tenant with `workspace_role` on its workspace and a live session.
    fn member(&self, space: &Space, workspace_role: Role) -> Member {
        self.runtime.block_on(async {
            let principal: PrincipalId = id(0x02);
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'g2 matrix member')")
                .bind(principal.to_string())
                .execute(&self.pool)
                .await
                .unwrap();
            let mut transaction = begin_tenant_transaction(&self.pool, space.tenant).await.unwrap();
            add_tenant_member(&mut transaction, principal, Role::Editor).await.unwrap();
            set_workspace_member(&mut transaction, space.workspace, principal, workspace_role).await.unwrap();
            transaction.commit().await.unwrap();
            let session = create_session(&self.pool, principal, SessionLimits::default()).await.unwrap();
            Member { tenant: space.tenant, principal, session: session.id }
        })
    }

    fn remove(&self, member: &Member) {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, member.tenant).await.unwrap();
            remove_tenant_member(&mut transaction, member.principal).await.unwrap();
            transaction.commit().await.unwrap();
        });
    }

    fn ticket(&self, tenant: TenantId, member: &Member, document: CloudDocumentId, edit: bool, expires_at: u64) -> String {
        let key = TicketKey::derive(SECRET).unwrap();
        mint(&key, &RealtimeTicket { tenant, document, principal: member.principal, session: member.session, edit, expires_at })
    }

    fn link(&self, role: ShareRole, revoked: bool) -> String {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.home.tenant).await.unwrap();
            let link = create_share_link(&mut transaction, self.home.document, role, Some("g2"), self.home.owner, None).await.unwrap();
            if revoked {
                assert!(revoke_share_link(&mut transaction, link.id).await.unwrap());
            }
            transaction.commit().await.unwrap();
            link.secret
        })
    }

    fn token(&self, kind: Kind) -> String {
        let home = &self.home;
        let expires = expiry(now(), MAX_TICKET_SECS);
        match kind {
            Kind::EditorLink => self.link(ShareRole::Editor, false),
            Kind::ViewerLink => self.link(ShareRole::Viewer, false),
            Kind::RevokedLink => self.link(ShareRole::Editor, true),
            Kind::EditorTicket => self.ticket(home.tenant, &self.member(home, Role::Editor), home.document, true, expires),
            Kind::ViewerTicket => self.ticket(home.tenant, &self.member(home, Role::Viewer), home.document, false, expires),
            Kind::CommenterTicket => self.ticket(home.tenant, &self.member(home, Role::Commenter), home.document, false, expires),
            Kind::ViewerWithEditTicket => self.ticket(home.tenant, &self.member(home, Role::Viewer), home.document, true, expires),
            Kind::CommenterWithEditTicket => self.ticket(home.tenant, &self.member(home, Role::Commenter), home.document, true, expires),
            Kind::ExpiredTicket => self.ticket(home.tenant, &self.member(home, Role::Editor), home.document, true, now().saturating_sub(1)),
            Kind::OtherTenantTicket => {
                let stranger = self.member(&self.other, Role::Editor);
                self.ticket(self.other.tenant, &stranger, home.document, true, expires)
            }
            Kind::OtherTenantMemberClaimingThisTenant => {
                let stranger = self.member(&self.other, Role::Editor);
                self.ticket(home.tenant, &stranger, home.document, true, expires)
            }
            Kind::RemovedMemberTicket => {
                let gone = self.member(home, Role::Editor);
                let ticket = self.ticket(home.tenant, &gone, home.document, true, expires);
                self.remove(&gone);
                ticket
            }
        }
    }

    fn config(&self) -> CloudBackendConfig {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 2;
        config.ticket_key = Some(TicketKey::derive(SECRET).unwrap());
        config
    }

    fn serve(&self) -> (Hosting, RevocationSweeper) {
        let backend = CloudBackend::connect(&self.config()).unwrap();
        let watch = backend.revocation_watch();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let hosting = Hosting::serve(listener, backend, NetLimits { hello_timeout: Duration::from_secs(5), ..NetLimits::default() }).unwrap();
        let sweeper = RevocationSweeper::start(&self.config(), watch, hosting.addr(), Duration::from_millis(200)).unwrap();
        (hosting, sweeper)
    }
}

/// What a welcome said.
struct Welcome {
    session: u64,
    client_id: u32,
    seq: u64,
    access: Access,
    catch_up: CatchUp,
}

impl Welcome {
    fn page(&self) -> Option<NodeId> {
        match &self.catch_up {
            CatchUp::Snapshot(changes) => changes.iter().find_map(|change| match change {
                Change::Create { id, parent: None, .. } => Some(*id),
                _ => None,
            }),
            CatchUp::Delta(_) => None,
        }
    }
}

struct Raw(WebSocket<TcpStream>);

impl Raw {
    fn connect(addr: SocketAddr, hello: &ClientMsg) -> Raw {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).unwrap();
        let mut raw = Raw(ws);
        raw.send(hello);
        raw
    }

    fn join(addr: SocketAddr, file: &str, token: &str) -> Raw {
        Raw::connect(addr, &Client::hello(file, token, "g2 probe", ""))
    }

    fn rejoin(addr: SocketAddr, file: &str, token: &str, resume: Resume) -> Raw {
        let ClientMsg::Hello(hello) = Client::hello(file, token, "g2 probe", "") else { panic!("a hello") };
        Raw::connect(addr, &ClientMsg::Hello(Hello { resume: Some(resume), ..hello }))
    }

    /// Send, ignoring a connection the server already closed.
    fn send(&mut self, msg: &ClientMsg) -> bool {
        self.0.send(Message::Binary(Bytes::from(message::encode(msg).unwrap()))).is_ok()
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

    /// The first answer to a hello: its welcome, or the refusal's reason.
    fn answer(&mut self) -> Result<Welcome, Option<String>> {
        match self.next() {
            Some(ServerMsg::Welcome { session, client_id, seq, access, catch_up, .. }) => Ok(Welcome { session, client_id, seq, access, catch_up }),
            Some(ServerMsg::Refused { reason }) => Err(Some(reason)),
            Some(other) => panic!("expected a welcome or a refusal, got {other:?}"),
            None => Err(None),
        }
    }

    fn rename(&mut self, base_seq: u64, batch_id: u64, page: NodeId, name: &str) -> bool {
        let changes = vec![Change::Set { id: page, prop: Prop::Name, value: Json::of(name).unwrap() }];
        self.send(&ClientMsg::Changes { base_seq, batch_id, changes })
    }

    /// What happened to batch `batch_id` of client `me`: `Ok(seq)` accepted, `Err(reason)` refused.
    fn verdict(&mut self, me: u32, batch_id: u64) -> Result<u64, String> {
        loop {
            match self.next() {
                Some(ServerMsg::Changes(accepted)) if accepted.author == me && accepted.batch_id == batch_id => return Ok(accepted.seq),
                Some(ServerMsg::Reject { batch_id: refused, reason, .. }) if refused == batch_id => return Err(reason),
                Some(_) => {}
                None => return Err("connection closed".into()),
            }
        }
    }

    /// Wait (up to [`WAIT`]) for a message matching `wanted`.
    fn hears(&mut self, wanted: impl Fn(&ServerMsg) -> bool) -> bool {
        while let Some(msg) = self.next() {
            if wanted(&msg) {
                return true;
            }
        }
        false
    }

    /// Ping and read up to the pong: whether anything before it matched `unwanted`. `None` when
    /// the connection is gone.
    fn heard_before_pong(&mut self, unwanted: impl Fn(&ServerMsg) -> bool) -> Option<bool> {
        if !self.send(&ClientMsg::Ping) {
            return None;
        }
        let mut heard = false;
        loop {
            match self.next()? {
                ServerMsg::Pong => return Some(heard),
                msg if unwanted(&msg) => {
                    eprintln!("heard before the pong: {msg:?}");
                    heard = true;
                }
                _ => {}
            }
        }
    }
}

/// The editor in the room who watches what reaches everyone.
struct Watcher {
    raw: Raw,
    id: u32,
    page: NodeId,
    seq: u64,
    batch: u64,
}

impl Watcher {
    fn join(addr: SocketAddr, file: &str, token: &str) -> Watcher {
        let mut raw = Raw::join(addr, file, token);
        let welcome = raw.answer().map_err(|why| format!("{why:?}")).unwrap();
        assert_eq!(welcome.access, Access::Edit);
        let page = welcome.page().unwrap();
        Watcher { raw, id: welcome.client_id, page, seq: welcome.seq, batch: 0 }
    }

    /// An accepted edit of the watcher's: its sequence number.
    fn edit(&mut self, name: &str) -> u64 {
        self.batch += 1;
        assert!(self.raw.rename(self.seq, self.batch, self.page, name));
        let seq = self.raw.verdict(self.id, self.batch).unwrap();
        self.seq = seq;
        seq
    }
}

/// Did `msg` come from someone other than the watcher (a batch, presence, a signal or a join)?
fn from_someone_else(watcher: u32, msg: &ServerMsg) -> bool {
    match msg {
        ServerMsg::Changes(accepted) => accepted.author != watcher,
        ServerMsg::Presence { client_id, .. } | ServerMsg::Signal { client_id, .. } => *client_id != watcher,
        ServerMsg::PeerJoined(peer) => peer.client_id != watcher,
        _ => false,
    }
}

fn presence(page: NodeId) -> ClientMsg {
    ClientMsg::Presence(Presence { cursor: Some((page, 12.0, 34.0)), ..Presence::default() })
}

/// Try every action with `token` and say what happened.
fn probe(addr: SocketAddr, file: &str, token: &str, watcher: &mut Watcher) -> [Outcome; 6] {
    let mut raw = Raw::join(addr, file, token);
    let welcome = match raw.answer() {
        Ok(welcome) => welcome,
        Err(reason) => return refused(addr, file, token, raw, reason, watcher),
    };
    let me = welcome.client_id;
    let mut outcomes = [Outcome::Refused; 6];
    outcomes[0] = match welcome.access {
        Access::Edit => Outcome::Edit,
        Access::View => Outcome::View,
    };
    assert!(watcher.raw.hears(|msg| matches!(msg, ServerMsg::PeerJoined(peer) if peer.client_id == me)), "the room hears {me} join");

    // The document state.
    let page = welcome.page();
    if page.is_some() {
        outcomes[1] = Outcome::Allowed;
    }
    let page = page.unwrap_or(watcher.page);

    // A property-change batch: accepted for everyone, or refused as view-only and heard by nobody.
    let _ = raw.rename(welcome.seq, 1, page, &format!("g2 edit by {me}"));
    match raw.verdict(me, 1) {
        Ok(seq) => {
            assert!(watcher.raw.hears(|msg| matches!(msg, ServerMsg::Changes(accepted) if accepted.author == me)), "the room hears the batch");
            watcher.seq = seq;
            outcomes[2] = Outcome::Allowed;
        }
        Err(reason) => {
            assert_eq!(reason, VIEW_ONLY, "a refused batch is refused as view-only");
            let leaked = watcher.raw.heard_before_pong(|msg| matches!(msg, ServerMsg::Changes(accepted) if accepted.author == me));
            assert_eq!(leaked, Some(false), "a refused batch reaches nobody");
        }
    }

    // Presence (a cursor) and a cursor-chat signal reach the room.
    if raw.send(&presence(page)) && watcher.raw.hears(|msg| matches!(msg, ServerMsg::Presence { client_id, .. } if *client_id == me)) {
        outcomes[3] = Outcome::Allowed;
    }
    if raw.send(&ClientMsg::Signal(Signal::chat("g2 hello")))
        && watcher.raw.hears(|msg| matches!(msg, ServerMsg::Signal { client_id, signal } if *client_id == me && signal.kind == Signal::CHAT))
    {
        outcomes[4] = Outcome::Allowed;
    }

    // Drop, miss a batch, come back: the same id and access, and the missed batch as catch-up.
    let last = watcher.seq;
    drop(raw);
    let missed = watcher.edit(&format!("g2 while {me} was away"));
    let mut back = Raw::rejoin(addr, file, token, Resume { session: welcome.session, client_id: me, seq: last });
    if let Ok(again) = back.answer()
        && again.client_id == me
        && again.access == welcome.access
        && matches!(&again.catch_up, CatchUp::Delta(batches) if batches.iter().any(|batch| batch.seq == missed))
    {
        outcomes[5] = Outcome::Allowed;
        assert!(watcher.raw.hears(|msg| matches!(msg, ServerMsg::PeerJoined(peer) if peer.client_id == me)), "the room hears {me} come back");
    }
    drop(back);
    outcomes
}

/// A refused hello: nothing the connection sends afterwards reaches the room, and a refused
/// credential resuming as the watcher neither gets in nor knocks the watcher out.
fn refused(addr: SocketAddr, file: &str, token: &str, mut raw: Raw, reason: Option<String>, watcher: &mut Watcher) -> [Outcome; 6] {
    assert_eq!(reason.as_deref(), Some(REFUSED), "every refusal reads like a bad link");
    let _ = raw.rename(watcher.seq, 1, watcher.page, "g2 refused edit");
    let _ = raw.send(&presence(watcher.page));
    let _ = raw.send(&ClientMsg::Signal(Signal::chat("g2 refused chat")));
    assert!(raw.next().is_none(), "a refused connection is closed");
    let me = watcher.id;
    assert_eq!(watcher.raw.heard_before_pong(|msg| from_someone_else(me, msg)), Some(false), "nothing from a refused connection reaches the room");

    let mut back = Raw::rejoin(addr, file, token, Resume { session: 0, client_id: watcher.id, seq: watcher.seq });
    assert_eq!(back.answer().err().flatten().as_deref(), Some(REFUSED));
    assert_eq!(watcher.raw.heard_before_pong(|msg| from_someone_else(me, msg)), Some(false), "the watcher is still in, undisturbed");
    [Outcome::Refused; 6]
}

#[test]
fn every_admission_kind_and_relay_action_matches_the_expected_matrix() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    // The table covers every kind exactly once.
    let kinds = [
        Kind::EditorLink,
        Kind::ViewerLink,
        Kind::EditorTicket,
        Kind::ViewerTicket,
        Kind::CommenterTicket,
        Kind::ViewerWithEditTicket,
        Kind::CommenterWithEditTicket,
        Kind::RevokedLink,
        Kind::ExpiredTicket,
        Kind::OtherTenantTicket,
        Kind::OtherTenantMemberClaimingThisTenant,
        Kind::RemovedMemberTicket,
    ];
    for kind in kinds {
        assert_eq!(EXPECTED.iter().filter(|(row, _)| *row == kind).count(), 1, "{kind:?} has one expected row");
    }
    assert_eq!(EXPECTED.len(), kinds.len());

    let (mut hosting, sweeper) = fixture.serve();
    let addr = hosting.addr();
    let file = fixture.home.document.to_string();
    let mut watcher = Watcher::join(addr, &file, &fixture.token(Kind::EditorTicket));

    let mut mismatches = Vec::new();
    for (kind, expected) in EXPECTED {
        let token = fixture.token(*kind);
        let actual = probe(addr, &file, &token, &mut watcher);
        for (index, action) in ACTIONS.iter().enumerate() {
            if actual[index] != expected[index] {
                mismatches.push(format!("{kind:?} x {action:?}: expected {:?}, got {:?}", expected[index], actual[index]));
            }
        }
    }
    assert!(mismatches.is_empty(), "relay action matrix mismatches:\n{}", mismatches.join("\n"));
    assert_eq!(watcher.raw.heard_before_pong(|_| false), Some(false), "the watcher stayed connected throughout");

    drop(watcher);
    drop(sweeper);
    hosting.stop();
    Ok(())
}

/// A participant can't resume as someone else's client id with a different credential: a viewer
/// claiming an editor's id would otherwise close the editor's connection and take over its name
/// in the room.
#[test]
fn a_viewer_cannot_resume_as_an_editors_client_id() -> TestResult {
    let Some(fixture) = Fixture::new()? else {
        return Ok(());
    };
    let (mut hosting, sweeper) = fixture.serve();
    let addr = hosting.addr();
    let file = fixture.home.document.to_string();
    let mut editor = Watcher::join(addr, &file, &fixture.token(Kind::EditorTicket));

    for kind in [Kind::ViewerLink, Kind::ViewerTicket, Kind::CommenterTicket] {
        let token = fixture.token(kind);
        let mut viewer = Raw::join(addr, &file, &token);
        let welcome = viewer.answer().map_err(|why| format!("{why:?}"))?;
        assert_eq!(welcome.access, Access::View);
        let mut hijack = Raw::rejoin(addr, &file, &token, Resume { session: welcome.session, client_id: editor.id, seq: editor.seq });
        let answer = hijack.answer();
        if let Ok(taken) = &answer {
            assert_ne!(taken.client_id, editor.id, "{kind:?}: a viewer doesn't get the editor's client id");
            assert_eq!(taken.access, Access::View);
        }
        assert!(editor.raw.heard_before_pong(|_| false).is_some(), "{kind:?}: the editor is still connected");
        editor.edit(&format!("g2 still editing after {kind:?}"));
    }

    drop(editor);
    drop(sweeper);
    hosting.stop();
    Ok(())
}
