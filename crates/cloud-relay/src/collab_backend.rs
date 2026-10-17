//! The collaboration hub's backend for FigCraft Cloud (core slice): the same protocol
//! `figcraft-server` speaks (`figcraft_collab_net::hub`), with documents in PostgreSQL and S3.
//!
//! - **Joining.** A hello's `file` is the cloud document's UUID and its `token` a share-link secret
//!   ([`figcraft_cloud_storage::share_links`]). The link must be live and name that document; an
//!   editor link joins with [`Access::Edit`], a viewer link with [`Access::View`]. A signed-in
//!   member joins with a `member.<tenant>.<session token>` token
//!   ([`figcraft_cloud_storage::revocation::admit_member`]): the session must be live and the
//!   member's effective role on the document at least `viewer` (`editor` to edit). A browser
//!   joins with a short-lived realtime ticket instead
//!   ([`figcraft_cloud_storage::realtime_tickets`], minted by the API, verified with
//!   [`CloudBackendConfig::ticket_key`]): MAC, expiry and document must check out and the session
//!   and role it names must still hold; the ticket's access decides edit or view. Every refusal
//!   reads the same.
//! - **Opening.** A document that isn't open is recovered like [`DurableRoom::recover`]: the
//!   sequencer is claimed (fencing every earlier owner), the latest verified snapshot is restored
//!   and the events after it replayed. A document with no history starts from a fixed base
//!   ([`base_document`], identical on every open, whatever the document is called) and that state
//!   is stored as its snapshot at sequence 0 straight away, so later opens never depend on the base.
//! - **Durability.** Accepted batches wait per document ([`Backend::changed`]) until the hub's
//!   [`Backend::flush`], which appends them under the document's fencing token and commits before
//!   the hub sends anyone the confirmations. A document whose commit fails is closed: nobody hears
//!   of its batches and everyone in it reconnects to its durable state.
//! - **Snapshots.** When everyone has left a document, and when the hub stops, its state is stored
//!   as a verified snapshot (best effort: the event log already holds every confirmed batch).
//!
//! - **Revocation (S4-017).** Every link a hello was admitted with is watched
//!   ([`RevocationWatch`]) while its document is open. A [`RevocationSweeper`] re-checks the
//!   watched links every interval ([`dead_share_links`]: revoked, expired, deleted, or on a
//!   document or in a tenant that is no longer active) and asks the hub, over a managing connection with a key
//!   only this process knows, to revoke each dead one: whoever joined with it is disconnected
//!   (told [`STOPPED`](figcraft_collab_net::hub::STOPPED)) and a new hello with it is refused at
//!   [`Backend::admit`]. The backend re-checks the link before acting, so the key can't disconnect
//!   anyone whose link still works. A revoked link, archived document or suspended tenant therefore closes its
//!   sessions within one interval plus one database round trip. Member admissions are watched the
//!   same way, with the admitted principal, session and access recorded: the sweep re-resolves
//!   each member's effective role ([`dead_member_admissions`]) and disconnects a member whose
//!   session was revoked, who was removed from the tenant, a team or a grant, or whose editor
//!   access was downgraded. Each member connection's revocation key is `session:<session id>`.
//!
//! The hub calls the backend on its own thread and the backend is synchronous, so the backend owns
//! a small tokio runtime and blocks on each storage call, every one bounded by a timeout.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::document_snapshots::store_snapshot;
use figcraft_cloud_storage::events::{MAX_APPEND_BATCHES, append_events};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::quarantine::{QuarantineError, quarantine_reason};
use figcraft_cloud_storage::realtime_tickets::{TicketKey, looks_like_ticket, verify as verify_ticket};
use figcraft_cloud_storage::revocation::{
    MAX_CHECKED_MEMBERS, MemberAdmission, RevocationError, admit_member, dead_member_admissions, parse_member_token,
};
use figcraft_cloud_storage::room_leases::{AcquireRoom, RoomLease, acquire_or_reclaim_room, release_room, renew_room};
use figcraft_cloud_storage::sessions::SessionLimits;
use figcraft_cloud_storage::share_links::{MAX_CHECKED_LINKS, ShareRole, dead_share_links, resolve_share_link};
use figcraft_collab::message::{Access, Admin, AdminReply};
use figcraft_collab::{Accepted, Document, Journal, Limits, Server};
use figcraft_collab_net::hub::{Admission, Backend, Closing, Handled, OpenFailure, OpenFile, Wake};
use figcraft_collab_net::{Endpoint, NetLimits};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::DurableRoom;

/// What every refused hello is told (never why: unknown, revoked, expired and wrong-file links
/// read the same).
pub const REFUSED: &str = "this link isn't valid (it may have been revoked or have expired)";
/// What a hello is told when the document can't be served right now.
pub const UNAVAILABLE: &str = "this document can't be opened right now: try again in a moment";
/// Default time between snapshots of a room that keeps changing.
pub const DEFAULT_SNAPSHOT_EVERY: Duration = Duration::from_secs(300);
/// Default number of accepted batches after which a room is snapshotted.
pub const DEFAULT_SNAPSHOT_AFTER_EVENTS: u64 = 1000;
/// What a hello is told when another relay replica owns the document's room right now.
pub const OWNED_ELSEWHERE: &str = "this document is open on another server: try again in a moment";
/// Default lifetime of a room lease. A relay that dies leaves its rooms unavailable for at most
/// this long; the fencing token stops its late writes either way.
pub const DEFAULT_LEASE_LIFETIME: Duration = Duration::from_secs(15);
/// Default interval between lease renewals (a third of the lifetime).
pub const DEFAULT_LEASE_RENEWAL: Duration = Duration::from_secs(5);
/// The producer recorded with the snapshots this backend stores.
pub const PRODUCER: &str = concat!("figcraft-cloud-relay ", env!("CARGO_PKG_VERSION"));
/// The name of [`base_document`] (fixed: the document's own name is metadata, not content).
pub const BASE_NAME: &str = "Untitled";
/// The initial cloud room limit: one page plus 20,000 design layers. Recovery of this size
/// meets the S5 target in the release load harness; the editor core keeps its larger limit.
pub const DEFAULT_MAX_CLOUD_NODES: usize = 20_001;
/// Admissions remembered for documents that aren't open yet.
const MAX_ADMITTED: usize = 4096;
/// Links watched for revocation in one document; a hello with one more is refused as
/// [`UNAVAILABLE`] rather than admitted unwatched.
pub const MAX_WATCHED_LINKS_PER_FILE: usize = 256;
/// How often a [`RevocationSweeper`] re-checks the watched links by default.
pub const REVOCATION_INTERVAL: Duration = Duration::from_secs(5);
/// The shortest sweep interval accepted.
const MIN_REVOCATION_INTERVAL: Duration = Duration::from_millis(50);
/// Revocations sent to the hub on one managing connection.
const REVOKES_PER_REQUEST: usize = 64;
/// The revocation key prefix of a member connection (followed by its session id).
pub const MEMBER_LINK_PREFIX: &str = "session:";
/// What a managing connection without this process's sweep key is told.
const NOT_MANAGED: &str = "this relay is managed by FigCraft Cloud: there is nothing to manage here";

/// Settings for a [`CloudBackend`].
#[derive(Clone, Debug)]
pub struct CloudBackendConfig {
    /// The runtime role's database URL.
    pub database_url: String,
    pub object_store: ObjectStoreSettings,
    /// The collaboration core's limits for every document.
    pub limits: Limits,
    /// Longest any one storage call may take.
    pub timeout: Duration,
    /// Database connections the backend keeps.
    pub max_connections: u32,
    /// Documents open at once; past it new documents are refused.
    pub max_open: usize,
    /// The key realtime tickets are verified with (derived from `SESSION_SIGNING_KEY`, the same
    /// one the API mints with). `None`: ticket hellos are refused like an unknown link.
    pub ticket_key: Option<TicketKey>,
    /// Room leases (S5-003/004): `None` serves every room without one, as a single relay does.
    pub leases: Option<LeaseConfig>,
    /// How long a room stays open (leased, in memory) after its last client left before it is
    /// saved and closed: a client that comes back in that time finds it warm instead of
    /// restoring it. Zero closes it at once.
    pub idle_close_after: Duration,
    /// A room with changes since its last snapshot is snapshotted at least this often.
    pub snapshot_every: Duration,
    /// ... and after this many accepted batches since it, whichever comes first, so a crash
    /// replays a bounded number of events.
    pub snapshot_after_events: u64,
    /// How often the hub calls the backend's housekeeping (scheduled snapshots, idle closes).
    pub housekeeping_every: Duration,
}

/// Room leases for a relay replica.
#[derive(Clone, Debug)]
pub struct LeaseConfig {
    /// A WebSocket URL (`ws://` or `wss://`, no credentials) where other relays and the gateway
    /// reach this replica; stored in the lease so a client can be sent to the room's owner.
    pub endpoint: String,
    /// How long a lease lasts without a renewal (5 to 120 s).
    pub lifetime: Duration,
    /// How often held leases are renewed; well under `lifetime`.
    pub renew_every: Duration,
}

impl LeaseConfig {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self { endpoint: endpoint.into(), lifetime: DEFAULT_LEASE_LIFETIME, renew_every: DEFAULT_LEASE_RENEWAL }
    }

    fn lifetime_seconds(&self) -> i32 {
        i32::try_from(self.lifetime.as_secs()).unwrap_or(i32::MAX)
    }
}

impl CloudBackendConfig {
    #[must_use]
    pub fn new(database_url: impl Into<String>, object_store: ObjectStoreSettings) -> Self {
        Self {
            database_url: database_url.into(),
            object_store,
            limits: Limits { max_nodes: DEFAULT_MAX_CLOUD_NODES, ..Limits::default() },
            timeout: Duration::from_secs(15),
            max_connections: 8,
            max_open: 1024,
            ticket_key: None,
            leases: None,
            idle_close_after: Duration::ZERO,
            snapshot_every: DEFAULT_SNAPSHOT_EVERY,
            snapshot_after_events: DEFAULT_SNAPSHOT_AFTER_EVENTS,
            housekeeping_every: Duration::from_secs(1),
        }
    }
}

/// Why a [`CloudBackend`] couldn't start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloudBackendError {
    Runtime,
    ObjectStore,
    Database,
}

impl std::fmt::Display for CloudBackendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "cloud collaboration backend error: {self:?}")
    }
}

impl std::error::Error for CloudBackendError {}

/// The document every cloud document's history starts from.
#[must_use]
pub fn base_document() -> Document {
    Document::new(BASE_NAME)
}

/// One watched file: its tenant, the share links and the member sessions admitted to it.
struct WatchedFile {
    tenant: TenantId,
    links: HashSet<Uuid>,
    members: HashMap<Uuid, MemberAdmission>,
}

impl WatchedFile {
    fn new(tenant: TenantId) -> Self {
        Self { tenant, links: HashSet::new(), members: HashMap::new() }
    }

    fn len(&self) -> usize {
        self.links.len() + self.members.len()
    }
}

type WatchedFiles = HashMap<String, WatchedFile>;

fn member_link(session: Uuid) -> String {
    format!("{MEMBER_LINK_PREFIX}{session}")
}

/// The links admitted to each open document, shared between a [`CloudBackend`] and its
/// [`RevocationSweeper`], and the key the sweeper manages the hub with (never printed).
#[derive(Clone)]
pub struct RevocationWatch {
    key: Arc<String>,
    files: Arc<Mutex<WatchedFiles>>,
}

impl std::fmt::Debug for RevocationWatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RevocationWatch").field("key", &"[REDACTED]").field("links", &self.len()).finish()
    }
}

impl RevocationWatch {
    fn new() -> Self {
        // Two v4 UUIDs: 244 bits from the operating system's generator.
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        Self { key: Arc::new(key), files: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn lock(&self) -> MutexGuard<'_, WatchedFiles> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Links watched now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().values().map(WatchedFile::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The watched entry for `file` in `tenant` (a file id names one document in one tenant; a
    /// different tenant starts over rather than mixing tenants).
    fn entry<'a>(files: &'a mut WatchedFiles, file: &str, tenant: TenantId) -> &'a mut WatchedFile {
        let watched = files.entry(file.to_string()).or_insert_with(|| WatchedFile::new(tenant));
        if watched.tenant != tenant {
            *watched = WatchedFile::new(tenant);
        }
        watched
    }

    /// Watch `link` for `file`; false when the file already has
    /// [`MAX_WATCHED_LINKS_PER_FILE`] other links and members watched.
    fn watch(&self, file: &str, tenant: TenantId, link: Uuid) -> bool {
        let mut files = self.lock();
        let watched = Self::entry(&mut files, file, tenant);
        if watched.links.contains(&link) {
            return true;
        }
        if watched.len() >= MAX_WATCHED_LINKS_PER_FILE {
            return false;
        }
        watched.links.insert(link);
        true
    }

    /// Watch a member admission for `file` (keyed by session); false past the per-file cap.
    fn watch_member(&self, file: &str, admission: MemberAdmission) -> bool {
        let mut files = self.lock();
        let watched = Self::entry(&mut files, file, admission.tenant);
        let full = watched.len() >= MAX_WATCHED_LINKS_PER_FILE;
        match watched.members.get_mut(&admission.session) {
            // One revocation key per session: keep the widest access admitted, so a downgrade
            // still disconnects the connection that joined with edit access.
            Some(existing) => existing.edit |= admission.edit,
            None if full => return false,
            None => {
                watched.members.insert(admission.session, admission);
            }
        }
        true
    }

    fn forget(&self, file: &str) {
        self.lock().remove(file);
    }

    fn unwatch(&self, file: &str, link: Uuid) {
        let mut files = self.lock();
        if let Some(watched) = files.get_mut(file) {
            watched.links.remove(&link);
            if watched.len() == 0 {
                files.remove(file);
            }
        }
    }

    fn unwatch_member(&self, file: &str, session: Uuid) {
        let mut files = self.lock();
        if let Some(watched) = files.get_mut(file) {
            watched.members.remove(&session);
            if watched.len() == 0 {
                files.remove(file);
            }
        }
    }

    fn member(&self, file: &str, session: Uuid) -> Option<MemberAdmission> {
        self.lock().get(file).and_then(|watched| watched.members.get(&session).copied())
    }

    /// Every watched `(file, member admission)`.
    fn members(&self) -> Vec<(String, MemberAdmission)> {
        self.lock().iter().flat_map(|(file, watched)| watched.members.values().map(|admission| (file.clone(), *admission))).collect()
    }

    fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.lock().retain(|file, _| keep(file));
    }

    fn tenant(&self, file: &str) -> Option<TenantId> {
        self.lock().get(file).map(|watched| watched.tenant)
    }

    /// Every watched `(file, tenant, link)`.
    fn links(&self) -> Vec<(String, TenantId, Uuid)> {
        self.lock().iter().flat_map(|(file, watched)| watched.links.iter().map(|link| (file.clone(), watched.tenant, *link))).collect()
    }

    /// Whether `key` is the sweep key (compared in constant time).
    fn opens(&self, key: &str) -> bool {
        let (a, b) = (self.key.as_bytes(), key.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
    }
}

/// Who may open a document, from the link or session a hello was admitted with.
#[derive(Clone, Copy, Debug)]
struct Admitted {
    tenant: TenantId,
    /// Whom accepted batches are recorded as made by: the signed-in member, or a share link's
    /// creator.
    actor: PrincipalId,
}

/// One open document.
struct OpenDoc {
    tenant: TenantId,
    document: CloudDocumentId,
    fencing_token: i64,
    /// The trusted principal recorded when each client id joined this room.
    actors: HashMap<u32, PrincipalId>,
    /// Accepted batches not yet committed.
    pending: Vec<(PrincipalId, Accepted)>,
    /// The sequence of the latest snapshot stored (or restored).
    snapshot_seq: Option<u64>,
    /// Whether this relay holds the room's lease (under `fencing_token`).
    leased: bool,
    /// A scheduled snapshot is being uploaded.
    snapshotting: bool,
    /// When the last snapshot was started (or the room opened), for the schedule.
    last_snapshot: Instant,
}

/// What the lease renewer keeps for a held lease.
struct HeldLease {
    tenant: TenantId,
    document: CloudDocumentId,
    fencing_token: i64,
    /// When the lease was last confirmed live; past the lifetime it is treated as lost.
    confirmed: Instant,
}

/// The leases this relay holds, shared with the renewer task.
#[derive(Default)]
struct Leases {
    held: Mutex<HashMap<String, HeldLease>>,
    /// Files whose lease was lost; the next flush closes them.
    lost: Mutex<Vec<String>>,
}

impl Leases {
    fn held(&self) -> MutexGuard<'_, HashMap<String, HeldLease>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lost(&self) -> MutexGuard<'_, Vec<String>> {
        self.lost.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The lease a restore takes before restoring.
#[derive(Clone)]
struct LeaseParams {
    owner: Uuid,
    endpoint: String,
    lifetime_seconds: i32,
}

/// A [`Backend`] serving cloud documents (see the module docs).
pub struct CloudBackend {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    store: ObjectStore,
    limits: Limits,
    timeout: Duration,
    max_open: usize,
    admitted: HashMap<String, Admitted>,
    open: HashMap<String, OpenDoc>,
    /// Files whose changes couldn't be kept outside a flush; reported by the next flush.
    failed: Vec<String>,
    watch: RevocationWatch,
    ticket_key: Option<TicketKey>,
    /// How restores and the lease renewer running on the runtime tell the hub (set by the hub).
    wake: Arc<Mutex<Option<Wake>>>,
    /// Restores finished on the runtime, not yet taken by the hub.
    finished: Finished,
    /// Files being restored on the runtime (they count against `max_open`).
    opening: HashSet<String>,
    /// This process's identity in room leases (new each start).
    owner: Uuid,
    lease: Option<LeaseConfig>,
    leases: Arc<Leases>,
    idle_close_after: Duration,
    snapshot_every: Duration,
    snapshot_after_events: u64,
    housekeeping_every: Duration,
    /// When each file's last client left, while it is kept warm.
    idle_since: HashMap<String, Instant>,
    /// Scheduled snapshot uploads that finished: (file, sequence, stored).
    snapshots_done: Arc<Mutex<Vec<(String, u64, bool)>>>,
}

type Finished = Arc<Mutex<Vec<(String, Result<(Server, OpenDoc), OpenFailure>)>>>;

impl CloudBackend {
    /// Connect to the database (lazily: the first storage call opens connections) and the object
    /// store.
    pub fn connect(config: &CloudBackendConfig) -> Result<Self, CloudBackendError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Restores replay events on a worker while commits and admissions need others.
            .worker_threads(4)
            .thread_name("cloud-relay-io")
            .enable_all()
            .build()
            .map_err(|_| CloudBackendError::Runtime)?;
        let store = ObjectStore::new(&config.object_store).map_err(|_| CloudBackendError::ObjectStore)?;
        let pool = {
            let _entered = runtime.enter();
            PgPoolOptions::new()
                .max_connections(config.max_connections.max(1))
                .acquire_timeout(config.timeout)
                .connect_lazy(&config.database_url)
                .map_err(|_| CloudBackendError::Database)?
        };
        let wake: Arc<Mutex<Option<Wake>>> = Arc::default();
        let leases: Arc<Leases> = Arc::default();
        let owner = Uuid::new_v4();
        if let Some(lease) = config.leases.clone() {
            runtime.spawn(renew_leases(pool.clone(), config.timeout, lease, owner, leases.clone(), wake.clone()));
        }
        Ok(Self {
            runtime,
            pool,
            store,
            limits: config.limits,
            timeout: config.timeout,
            max_open: config.max_open.max(1),
            admitted: HashMap::new(),
            open: HashMap::new(),
            failed: Vec::new(),
            watch: RevocationWatch::new(),
            ticket_key: config.ticket_key.clone(),
            wake,
            finished: Finished::default(),
            opening: HashSet::new(),
            owner,
            lease: config.leases.clone(),
            leases,
            idle_close_after: config.idle_close_after,
            snapshot_every: config.snapshot_every,
            snapshot_after_events: config.snapshot_after_events.max(1),
            housekeeping_every: config.housekeeping_every.max(Duration::from_millis(10)),
            idle_since: HashMap::new(),
            snapshots_done: Arc::default(),
        })
    }

    /// The links this backend admitted, for a [`RevocationSweeper`] (start it once the hub
    /// serves this backend).
    #[must_use]
    pub fn revocation_watch(&self) -> RevocationWatch {
        self.watch.clone()
    }

    /// Documents open now.
    #[must_use]
    pub fn open_documents(&self) -> usize {
        self.open.len()
    }

    /// Documents open or being restored now.
    fn open_or_opening(&self) -> usize {
        self.open.len() + self.opening.len()
    }

    /// The lease a restore takes first (`None`: leases are off).
    fn lease_params(&self) -> Option<LeaseParams> {
        self.lease.as_ref().map(|lease| LeaseParams {
            owner: self.owner,
            endpoint: lease.endpoint.clone(),
            lifetime_seconds: lease.lifetime_seconds(),
        })
    }

    /// Start renewing the lease of a document that just opened.
    fn hold(&self, file: &str, doc: &OpenDoc) {
        if doc.leased {
            self.leases.held().insert(
                file.to_string(),
                HeldLease { tenant: doc.tenant, document: doc.document, fencing_token: doc.fencing_token, confirmed: Instant::now() },
            );
        }
    }

    /// Stop renewing and release the lease of a document this relay is closing.
    fn release(&self, file: &str, doc: &OpenDoc) {
        if !doc.leased {
            return;
        }
        self.leases.held().remove(file);
        let (pool, timeout, owner) = (&self.pool, self.timeout, self.owner);
        self.runtime.block_on(release_lease(pool, timeout, file, doc.tenant, doc.document, owner, doc.fencing_token));
    }

    /// Run `future` on the backend's runtime, giving up after the timeout (`None`).
    fn block<T>(&self, future: impl Future<Output = T>) -> Option<T> {
        let timeout = self.timeout;
        self.runtime.block_on(async move { tokio::time::timeout(timeout, future).await.ok() })
    }

    /// Commit `doc`'s pending batches; false when they couldn't be (they are dropped).
    fn commit(&self, file: &str, doc: &mut OpenDoc) -> bool {
        if doc.pending.is_empty() {
            return true;
        }
        let pending = std::mem::take(&mut doc.pending);
        let committed = self.block(commit_batches(&self.pool, doc.tenant, doc.document, doc.fencing_token, pending));
        Self::commit_outcome(file, committed)
    }

    fn commit_outcome(file: &str, committed: Option<Result<(), &'static str>>) -> bool {
        match committed {
            Some(Ok(())) => true,
            Some(Err(stage)) => {
                log::error!("document {file}: accepted batches couldn't be committed ({stage}); closing it");
                false
            }
            None => {
                log::error!("document {file}: committing accepted batches timed out; closing it");
                false
            }
        }
    }

    /// Commit every given document's pending batches at once, each in its own transaction on the
    /// runtime, and wait for all of them (each bounded by the storage timeout): one slow room
    /// doesn't make the others wait their turn. The documents whose commit failed.
    fn commit_all(&mut self, files: Vec<String>) -> Vec<String> {
        let mut jobs = Vec::new();
        for file in files {
            let Some(doc) = self.open.get_mut(&file) else { continue };
            if doc.pending.is_empty() {
                continue;
            }
            let pending = std::mem::take(&mut doc.pending);
            let (pool, tenant, document, token, timeout) = (self.pool.clone(), doc.tenant, doc.document, doc.fencing_token, self.timeout);
            let handle =
                self.runtime.spawn(async move { tokio::time::timeout(timeout, commit_batches(&pool, tenant, document, token, pending)).await.ok() });
            jobs.push((file, handle));
        }
        let mut failed = Vec::new();
        for (file, handle) in jobs {
            let outcome = self.runtime.block_on(handle).unwrap_or(None);
            if !Self::commit_outcome(&file, outcome) {
                if let Some(doc) = self.open.remove(&file) {
                    self.release(&file, &doc);
                }
                failed.push(file);
            }
        }
        failed
    }

    /// Store `server`'s state as `doc`'s snapshot unless one at its sequence exists.
    fn snapshot(&self, file: &str, doc: &mut OpenDoc, server: &Server) {
        if doc.snapshot_seq == Some(server.seq()) {
            return;
        }
        let Ok(state) = server.state() else {
            log::error!("document {file}: its state couldn't be captured for a snapshot");
            return;
        };
        let (pool, store, tenant, document) = (&self.pool, &self.store, doc.tenant, doc.document);
        let stored = self.block(put_snapshot(pool, store, tenant, document, &state));
        if matches!(stored, Some(Ok(()))) {
            doc.snapshot_seq = Some(server.seq());
        } else {
            log::warn!("document {file}: snapshot at seq {} couldn't be stored (its events still hold it)", server.seq());
        }
    }

    /// Recover `document` as its new sequencer, on the calling (hub) thread.
    fn recover(&self, file: &str, admitted: Admitted, document: CloudDocumentId) -> Result<(Server, OpenDoc), String> {
        let (pool, store, limits, timeout) = (self.pool.clone(), self.store.clone(), self.limits, self.timeout);
        let lease = self.lease_params();
        self.runtime.block_on(recover_document(pool, store, limits, timeout, file.to_string(), admitted, document, lease)).map_err(|failure| {
            match failure {
                OpenFailure::Refused(reason) | OpenFailure::Elsewhere { reason, .. } => reason,
            }
        })
    }
}

/// What taking a room's lease came to.
enum Leased {
    Claimed(figcraft_cloud_storage::events::SequencerClaim),
    /// Another owner holds a live lease.
    Held(RoomLease),
}

/// Lease `document` for this relay and claim its fence, in one committed transaction. A room owned
/// by another replica (live lease) comes back as [`OpenFailure::Elsewhere`] with that replica's
/// endpoint, so the hub can pass the client on; a quarantined, missing or archived document is
/// [`UNAVAILABLE`] before it is fenced.
async fn acquire_lease(
    pool: &PgPool,
    timeout: Duration,
    file: &str,
    tenant: TenantId,
    document: CloudDocumentId,
    params: &LeaseParams,
) -> Result<figcraft_cloud_storage::events::SequencerClaim, OpenFailure> {
    let attempt = async {
        let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| "begin")?;
        match quarantine_reason(&mut transaction, document).await {
            Ok(None) | Err(QuarantineError::NotFound) => {}
            Ok(Some(_)) => return Err("quarantined"),
            Err(_) => return Err("quarantine check"),
        }
        // A live lease at this replica's own address is its earlier process (one process per
        // address): reclaimed at once, the fence moving past that process's.
        let (acquired, claim) = acquire_or_reclaim_room(&mut transaction, document, params.owner, &params.endpoint, params.lifetime_seconds)
            .await
            .map_err(|_| "acquire")?;
        match (acquired, claim) {
            (AcquireRoom::Acquired(_), Some(claim)) => {
                transaction.commit().await.map_err(|_| "commit")?;
                Ok(Leased::Claimed(claim))
            }
            (AcquireRoom::Held(lease), _) | (AcquireRoom::Acquired(lease), None) => Ok(Leased::Held(lease)),
        }
    };
    match tokio::time::timeout(timeout, attempt).await {
        Ok(Ok(Leased::Claimed(claim))) => Ok(claim),
        // Held at this replica's own address by this very process (it already has the room, which
        // is open): never pass a client on to itself.
        Ok(Ok(Leased::Held(lease))) if lease.endpoint == params.endpoint => {
            log::warn!("document {file}: its room lease at this relay's own address is held by this process");
            Err(OpenFailure::Refused(OWNED_ELSEWHERE.into()))
        }
        Ok(Ok(Leased::Held(lease))) => {
            log::info!("document {file}: its room is owned by the relay replica at {}", lease.endpoint);
            Err(OpenFailure::Elsewhere { endpoint: lease.endpoint, reason: OWNED_ELSEWHERE.into() })
        }
        Ok(Err(stage)) => {
            log::error!("document {file}: its room lease couldn't be taken ({stage})");
            Err(OpenFailure::Refused(UNAVAILABLE.into()))
        }
        Err(_) => {
            log::error!("document {file}: taking its room lease timed out");
            Err(OpenFailure::Refused(UNAVAILABLE.into()))
        }
    }
}

/// Release this relay's exact lease (a failure only means it expires on its own).
async fn release_lease(pool: &PgPool, timeout: Duration, file: &str, tenant: TenantId, document: CloudDocumentId, owner: Uuid, fencing_token: i64) {
    let attempt = async {
        let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| ())?;
        release_room(&mut transaction, document, owner, fencing_token).await.map_err(|_| ())?;
        transaction.commit().await.map_err(|_| ())
    };
    if !matches!(tokio::time::timeout(timeout, attempt).await, Ok(Ok(()))) {
        log::warn!("document {file}: its room lease couldn't be released (it expires on its own)");
    }
}

/// Renew every held lease each `config.renew_every`. A lease the database says is no longer this
/// relay's (taken over, expired, or the fence moved), or one that couldn't be confirmed for its
/// whole lifetime, is lost: the file is queued for the hub to close at its next flush.
async fn renew_leases(pool: PgPool, timeout: Duration, config: LeaseConfig, owner: Uuid, leases: Arc<Leases>, wake: Arc<Mutex<Option<Wake>>>) {
    let mut tick = tokio::time::interval(config.renew_every.max(Duration::from_millis(100)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let snapshot: Vec<(String, TenantId, CloudDocumentId, i64, Instant)> =
            leases.held().iter().map(|(f, h)| (f.clone(), h.tenant, h.document, h.fencing_token, h.confirmed)).collect();
        let mut lost = Vec::new();
        for (file, tenant, document, token, confirmed) in snapshot {
            // Stamped before the request: the database's expiry counts from when it ran, so the
            // relay's own deadline is never later than the lease's.
            let asked = Instant::now();
            let attempt = async {
                let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|_| ())?;
                let renewed = renew_room(&mut transaction, document, owner, token, config.lifetime_seconds()).await.map_err(|_| ())?;
                transaction.commit().await.map_err(|_| ())?;
                Ok::<bool, ()>(renewed)
            };
            match tokio::time::timeout(timeout, attempt).await {
                Ok(Ok(true)) => {
                    if let Some(held) = leases.held().get_mut(&file) {
                        held.confirmed = asked;
                    }
                }
                Ok(Ok(false)) => {
                    log::warn!("document {file}: its room lease is no longer this relay's; closing the room");
                    lost.push(file);
                }
                // The database couldn't be asked: keep serving until the lease would have expired.
                Ok(Err(())) | Err(_) if confirmed.elapsed() >= config.lifetime => {
                    log::warn!("document {file}: its room lease couldn't be renewed for {:?}; closing the room", config.lifetime);
                    lost.push(file);
                }
                Ok(Err(())) | Err(_) => log::warn!("document {file}: a room lease renewal failed; will retry"),
            }
        }
        if !lost.is_empty() {
            {
                let mut held = leases.held();
                for file in &lost {
                    held.remove(file);
                }
            }
            leases.lost().extend(lost);
            let wake = wake.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(wake) = wake {
                wake.wake();
            }
        }
    }
}

/// Append `pending` (accepted batches, in order) to `document`'s log under `fencing_token` and
/// commit, all in one tenant transaction: the batches are durable or none of them is.
async fn commit_batches(
    pool: &PgPool,
    tenant: TenantId,
    document: CloudDocumentId,
    fencing_token: i64,
    pending: Vec<(PrincipalId, Accepted)>,
) -> Result<(), &'static str> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| "begin")?;
    let mut actor = None;
    let mut chunk = Vec::new();
    for (batch_actor, batch) in pending {
        if actor != Some(batch_actor) || chunk.len() == MAX_APPEND_BATCHES {
            if let Some(previous) = actor {
                append_events(&mut transaction, document, fencing_token, previous, &chunk).await.map_err(|_| "append")?;
                chunk.clear();
            }
            actor = Some(batch_actor);
        }
        chunk.push(batch);
    }
    if let Some(last) = actor {
        append_events(&mut transaction, document, fencing_token, last, &chunk).await.map_err(|_| "append")?;
    }
    transaction.commit().await.map_err(|_| "commit")
}

/// Store `state` as the document's snapshot in one tenant transaction.
async fn put_snapshot(
    pool: &PgPool,
    store: &ObjectStore,
    tenant: TenantId,
    document: CloudDocumentId,
    state: &figcraft_collab::SessionState,
) -> Result<(), ()> {
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| ())?;
    store_snapshot(pool, &mut transaction, store, document, state, PRODUCER).await.map_err(|_| ())?;
    transaction.commit().await.map_err(|_| ())
}

/// Restore `document` (latest verified snapshot plus later events) and claim it as its new
/// sequencer. Runs on the runtime, on the hub's thread or away from it
/// ([`Backend::begin_open`]); every storage step is bounded by `timeout`.
#[allow(clippy::too_many_arguments)]
async fn recover_document(
    pool: PgPool,
    store: ObjectStore,
    limits: Limits,
    timeout: Duration,
    file: String,
    admitted: Admitted,
    document: CloudDocumentId,
    lease: Option<LeaseParams>,
) -> Result<(Server, OpenDoc), OpenFailure> {
    // With leases the room is leased (and its fence claimed) first, in one transaction: a room
    // another replica owns is never restored here, and the restore runs under that fence.
    let claim = match &lease {
        Some(params) => Some(acquire_lease(&pool, timeout, &file, admitted.tenant, document, params).await?),
        None => None,
    };
    let recovered = tokio::time::timeout(timeout, async {
        match claim {
            Some(claim) => DurableRoom::recover_claimed(pool.clone(), &store, admitted.tenant, document, claim, base_document(), "", limits).await,
            None => DurableRoom::recover(pool.clone(), &store, admitted.tenant, document, base_document(), "", limits).await,
        }
    })
    .await;
    let room = match recovered {
        Ok(Ok(room)) => room,
        outcome => {
            match outcome {
                Ok(Err(error)) => log::error!("document {file} couldn't be recovered: {error}"),
                _ => log::error!("document {file}: recovery timed out"),
            }
            // Don't hold the room for the lease's lifetime when it can't be served.
            if let (Some(params), Some(claim)) = (&lease, claim) {
                release_lease(&pool, timeout, &file, admitted.tenant, document, params.owner, claim.fencing_token).await;
            }
            return Err(UNAVAILABLE.to_string().into());
        }
    };
    let restored = room.restored_snapshot();
    if room.server().document().node_count() > limits.max_nodes {
        log::warn!("document {file}: recovered document exceeds the cloud node limit");
        return Err(UNAVAILABLE.to_string().into());
    }
    let fencing_token = room.fencing_token();
    let server = room.into_server();
    let snapshot_seq = restored.and_then(|(_, seq)| u64::try_from(seq).ok());
    let mut doc = OpenDoc {
        tenant: admitted.tenant,
        document,
        fencing_token,
        actors: HashMap::new(),
        pending: Vec::new(),
        snapshot_seq,
        leased: lease.is_some(),
        snapshotting: false,
        last_snapshot: Instant::now(),
    };
    if snapshot_seq.is_none() && server.seq() == 0 {
        // A new document: its first state becomes durable before anyone edits it.
        match server.state() {
            Ok(state) => match tokio::time::timeout(timeout, put_snapshot(&pool, &store, admitted.tenant, document, &state)).await {
                Ok(Ok(())) => doc.snapshot_seq = Some(server.seq()),
                _ => log::warn!("document {file}: snapshot at seq {} couldn't be stored (its events still hold it)", server.seq()),
            },
            Err(_) => log::error!("document {file}: its state couldn't be captured for a snapshot"),
        }
    }
    Ok((server, doc))
}

impl CloudBackend {
    /// Forget admissions of documents that aren't open when the admitted map is full.
    fn make_room(&mut self, file: &str) {
        if self.admitted.len() >= MAX_ADMITTED && !self.admitted.contains_key(file) {
            let open = &self.open;
            self.admitted.retain(|file, _| open.contains_key(file));
            self.watch.retain(|file| open.contains_key(file));
        }
    }

    /// Admit a signed-in member (a `member.<tenant>.<session>` hello token).
    fn admit_member(&mut self, file: &str, tenant: TenantId, session: &str, document: CloudDocumentId) -> Result<Admission, String> {
        let pool = &self.pool;
        let admission = match self.block(admit_member(pool, tenant, session, document, SessionLimits::default())) {
            Some(Ok(admission)) => admission,
            Some(Err(RevocationError::Refused)) => return Err(REFUSED.into()),
            Some(Err(_)) | None => {
                log::error!("document {file}: a member couldn't be checked (storage unavailable)");
                return Err(UNAVAILABLE.into());
            }
        };
        self.make_room(file);
        if !self.watch.watch_member(file, admission) {
            log::warn!("document {file}: too many links and members in use to watch another; hello refused");
            return Err(UNAVAILABLE.into());
        }
        self.admitted.insert(file.to_string(), Admitted { tenant: admission.tenant, actor: admission.principal });
        let access = if admission.edit { Access::Edit } else { Access::View };
        Ok(Admission { access, link: Some(member_link(admission.session)) })
    }
}

impl CloudBackend {
    /// Admit a realtime ticket (`figcraft_cloud_storage::realtime_tickets`): the MAC, expiry and
    /// document must check out, and the session and effective role it names must still hold
    /// (`dead_member_admissions`), so a ticket never outlives a revocation. The member is then
    /// watched like any signed-in member and recorded as the batch actor.
    fn admit_ticket(&mut self, file: &str, token: &str, document: CloudDocumentId) -> Result<Admission, String> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
        let Some(ticket) = self.ticket_key.as_ref().and_then(|key| verify_ticket(key, token, document, now).ok()) else {
            return Err(REFUSED.into());
        };
        let admission = MemberAdmission { tenant: ticket.tenant, document, principal: ticket.principal, session: ticket.session, edit: ticket.edit };
        let pool = &self.pool;
        match self.block(dead_member_admissions(pool, &[admission])) {
            Some(Ok(dead)) if dead.is_empty() => {}
            Some(Ok(_)) => return Err(REFUSED.into()),
            Some(Err(_)) | None => {
                log::error!("document {file}: a realtime ticket couldn't be checked (storage unavailable)");
                return Err(UNAVAILABLE.into());
            }
        }
        self.make_room(file);
        if !self.watch.watch_member(file, admission) {
            log::warn!("document {file}: too many links and members in use to watch another; hello refused");
            return Err(UNAVAILABLE.into());
        }
        self.admitted.insert(file.to_string(), Admitted { tenant: admission.tenant, actor: admission.principal });
        let access = if admission.edit { Access::Edit } else { Access::View };
        Ok(Admission { access, link: Some(member_link(admission.session)) })
    }
}

impl Backend for CloudBackend {
    fn admit(&mut self, file: &str, token: &str) -> Result<Admission, String> {
        let Ok(document) = file.parse::<CloudDocumentId>() else {
            return Err(REFUSED.into());
        };
        if let Some((tenant, session)) = parse_member_token(token) {
            return self.admit_member(file, tenant, session, document);
        }
        if looks_like_ticket(token) {
            return self.admit_ticket(file, token, document);
        }
        let pool = &self.pool;
        let link = match self.block(resolve_share_link(pool, token)) {
            Some(Ok(link)) => link,
            Some(Err(figcraft_cloud_storage::share_links::ShareLinkError::Refused)) => return Err(REFUSED.into()),
            Some(Err(_)) | None => {
                log::error!("document {file}: a share link couldn't be checked (storage unavailable)");
                return Err(UNAVAILABLE.into());
            }
        };
        if link.document_id != document {
            return Err(REFUSED.into());
        }
        self.make_room(file);
        if !self.watch.watch(file, link.tenant_id, link.id) {
            log::warn!("document {file}: too many share links in use to watch another; hello refused");
            return Err(UNAVAILABLE.into());
        }
        self.admitted.insert(file.to_string(), Admitted { tenant: link.tenant_id, actor: link.created_by });
        let access = match link.role {
            ShareRole::Editor => Access::Edit,
            ShareRole::Viewer => Access::View,
        };
        Ok(Admission { access, link: Some(link.id.to_string()) })
    }

    fn open(&mut self, file: &str) -> Result<Server, String> {
        let (Ok(document), Some(admitted)) = (file.parse::<CloudDocumentId>(), self.admitted.get(file).copied()) else {
            return Err(REFUSED.into());
        };
        if self.open_or_opening() >= self.max_open && !self.open.contains_key(file) {
            log::warn!("document {file} refused: {} documents are open", self.open_or_opening());
            return Err(UNAVAILABLE.into());
        }
        let (server, doc) = self.recover(file, admitted, document)?;
        log::info!("document {file} opened at seq {} (fencing token {})", server.seq(), doc.fencing_token);
        self.hold(file, &doc);
        self.open.insert(file.to_string(), doc);
        Ok(server)
    }

    /// Restores run on the runtime, so one slow room never holds the hub's other rooms.
    fn begin_open(&mut self, file: &str) -> Result<bool, String> {
        let wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let Some(wake) = wake else { return Ok(false) };
        let (Ok(document), Some(admitted)) = (file.parse::<CloudDocumentId>(), self.admitted.get(file).copied()) else {
            return Err(REFUSED.into());
        };
        if self.open_or_opening() >= self.max_open && !self.open.contains_key(file) {
            log::warn!("document {file} refused: {} documents are open", self.open_or_opening());
            return Err(UNAVAILABLE.into());
        }
        if !self.opening.insert(file.to_string()) {
            return Ok(true);
        }
        let (pool, store, limits, timeout) = (self.pool.clone(), self.store.clone(), self.limits, self.timeout);
        let (finished, file, lease) = (self.finished.clone(), file.to_string(), self.lease_params());
        self.runtime.spawn(async move {
            let result = recover_document(pool, store, limits, timeout, file.clone(), admitted, document, lease).await;
            if let Ok(mut done) = finished.lock() {
                done.push((file, result));
            }
            wake.wake();
        });
        Ok(true)
    }

    fn opened(&mut self) -> Vec<(String, Result<Server, OpenFailure>)> {
        let done = self.finished.lock().map(|mut done| std::mem::take(&mut *done)).unwrap_or_default();
        done.into_iter()
            .map(|(file, result)| {
                self.opening.remove(&file);
                let result = result.map(|(server, doc)| {
                    log::info!("document {file} opened at seq {} (fencing token {})", server.seq(), doc.fencing_token);
                    self.hold(&file, &doc);
                    self.open.insert(file.clone(), doc);
                    server
                });
                (file, result)
            })
            .collect()
    }

    fn attach(&mut self, wake: Wake) {
        *self.wake.lock().unwrap_or_else(PoisonError::into_inner) = Some(wake);
    }

    fn joined(&mut self, file: &str, id: u32) {
        self.idle_since.remove(file);
        if let (Some(admitted), Some(doc)) = (self.admitted.get(file), self.open.get_mut(file)) {
            doc.actors.insert(id, admitted.actor);
        }
    }

    fn left(&mut self, file: &str, id: u32) {
        if let Some(doc) = self.open.get_mut(file) {
            doc.actors.remove(&id);
        }
    }

    fn changed(&mut self, file: &str, server: &mut Server) {
        let accepted: Vec<Accepted> = server
            .take_journal()
            .into_iter()
            .filter_map(|entry| match entry {
                Journal::Accepted(batch) => Some(batch),
                Journal::Issued { .. } | Journal::Refused { .. } => None,
            })
            .collect();
        if accepted.is_empty() {
            return;
        }
        let attributed = self.open.get(file).and_then(|doc| {
            accepted.into_iter().map(|batch| doc.actors.get(&batch.author).copied().map(|actor| (actor, batch))).collect::<Option<Vec<_>>>()
        });
        match attributed {
            Some(batches) => {
                if let Some(doc) = self.open.get_mut(file) {
                    doc.pending.extend(batches);
                }
            }
            // An accepted batch without a trusted admission identity cannot be confirmed.
            None => {
                if let Some(doc) = self.open.remove(file) {
                    self.release(file, &doc);
                }
                if !self.failed.iter().any(|failed| failed == file) {
                    self.failed.push(file.to_string());
                }
            }
        }
    }

    fn flush(&mut self) -> Vec<String> {
        let mut failed = std::mem::take(&mut self.failed);
        // A room whose lease was lost: what it accepted this burst is never committed or
        // confirmed, and everyone in it reconnects (to the new owner).
        let lost = std::mem::take(&mut *self.leases.lost());
        for file in lost {
            if let Some(mut doc) = self.open.remove(&file) {
                doc.pending.clear();
                // Not released: it is no longer this relay's lease to release.
                doc.leased = false;
            }
            if !failed.contains(&file) {
                failed.push(file);
            }
        }
        let files: Vec<String> = self.open.iter().filter(|(_, doc)| !doc.pending.is_empty()).map(|(file, _)| file.clone()).collect();
        failed.extend(self.commit_all(files));
        for file in &failed {
            self.idle_since.remove(file);
        }
        failed
    }

    fn idle(&mut self, file: &str, server: &mut Server) -> bool {
        self.changed(file, server);
        // Kept warm for `idle_close_after` after the last client left: this call is the leave
        // (note the time, stay open); a later one from the housekeeping tick finds it over.
        if !self.idle_close_after.is_zero() {
            match self.idle_since.get(file) {
                Some(since) if since.elapsed() >= self.idle_close_after => {}
                Some(_) => return false,
                None => {
                    self.idle_since.insert(file.to_string(), Instant::now());
                    return false;
                }
            }
        }
        self.idle_since.remove(file);
        if let Some(mut doc) = self.open.remove(file) {
            // Batches of this burst not flushed yet: everyone has left, so nobody was told of
            // them; keep them if possible, and snapshot only state that is committed.
            if self.commit(file, &mut doc) {
                self.snapshot(file, &mut doc, server);
            }
            self.release(file, &doc);
        }
        self.failed.retain(|f| f != file);
        self.admitted.remove(file);
        self.watch.forget(file);
        true
    }

    fn tick_every(&self) -> Option<Duration> {
        Some(self.housekeeping_every)
    }

    /// Scheduled snapshots of rooms that keep changing, and the close of rooms that were empty
    /// for `idle_close_after`. Runs right after a flush, so each server holds exactly what was
    /// committed: its state can be snapshotted without a batch that might still fail.
    fn tick(&mut self, open: Vec<(&str, &mut Server, usize)>) -> Vec<String> {
        let finished = std::mem::take(&mut *self.snapshots_done.lock().unwrap_or_else(PoisonError::into_inner));
        for (file, seq, stored) in finished {
            if let Some(doc) = self.open.get_mut(&file) {
                doc.snapshotting = false;
                if stored {
                    doc.snapshot_seq = Some(doc.snapshot_seq.map_or(seq, |held| held.max(seq)));
                }
            }
        }
        let mut close = Vec::new();
        for (file, server, clients) in open {
            if clients == 0 {
                if self.idle_since.get(file).is_some_and(|since| since.elapsed() >= self.idle_close_after) {
                    close.push(file.to_string());
                }
                continue;
            }
            let Some(doc) = self.open.get_mut(file) else { continue };
            if doc.snapshotting || !doc.pending.is_empty() || self.failed.iter().any(|failed| failed == file) {
                continue;
            }
            let held = doc.snapshot_seq.unwrap_or(0);
            let behind = server.seq().saturating_sub(held);
            if behind == 0 || (behind < self.snapshot_after_events && doc.last_snapshot.elapsed() < self.snapshot_every) {
                continue;
            }
            let Ok(state) = server.state() else {
                log::error!("document {file}: its state couldn't be captured for a scheduled snapshot");
                continue;
            };
            doc.snapshotting = true;
            doc.last_snapshot = Instant::now();
            let (pool, store, timeout, tenant, document) = (self.pool.clone(), self.store.clone(), self.timeout, doc.tenant, doc.document);
            let (done, name, seq) = (self.snapshots_done.clone(), file.to_string(), server.seq());
            self.runtime.spawn(async move {
                let stored = matches!(tokio::time::timeout(timeout, put_snapshot(&pool, &store, tenant, document, &state)).await, Ok(Ok(())));
                if !stored {
                    log::warn!("document {name}: the scheduled snapshot at seq {seq} couldn't be stored (its events still hold it)");
                }
                if let Ok(mut finished) = done.lock() {
                    finished.push((name, seq, stored));
                }
            });
        }
        close
    }

    fn stop(&mut self, open: Vec<(&str, &mut Server)>) {
        for (file, server) in open {
            self.changed(file, server);
            if let Some(mut doc) = self.open.remove(file) {
                if self.commit(file, &mut doc) {
                    self.snapshot(file, &mut doc, server);
                }
                self.release(file, &doc);
            }
        }
        self.failed.clear();
        self.watch.retain(|_| false);
    }

    fn manage(&mut self, key: &str) -> Result<(), String> {
        if self.watch.opens(key) { Ok(()) } else { Err(NOT_MANAGED.into()) }
    }

    /// Only revocations, and only of links (or member sessions) that really no longer work
    /// (checked again here).
    fn admin(&mut self, req: Admin, _open: &dyn Fn(&str) -> Option<OpenFile>) -> Handled {
        let failed = |reason: &str| Handled::from(AdminReply::Failed { reason: reason.into() });
        let Admin::Revoke { file, link } = req else {
            return failed("nothing to manage here");
        };
        if let Some(session) = link.strip_prefix(MEMBER_LINK_PREFIX) {
            let Some(admission) = session.parse::<Uuid>().ok().and_then(|session| self.watch.member(&file, session)) else {
                return failed("no such member in use");
            };
            let pool = &self.pool;
            return match self.block(dead_member_admissions(pool, &[admission])) {
                Some(Ok(dead)) if !dead.is_empty() => {
                    self.watch.unwatch_member(&file, admission.session);
                    log::info!("document {file}: a member lost access (session {}); disconnecting them", admission.session);
                    Handled { reply: AdminReply::Done, revoked: Some((file, link)), removed: None, blocked: None }
                }
                Some(Ok(_)) => failed("that member still has access"),
                Some(Err(_)) | None => failed("the member couldn't be checked"),
            };
        }
        let (Ok(id), Some(tenant)) = (link.parse::<Uuid>(), self.watch.tenant(&file)) else {
            return failed("no such link in use");
        };
        let pool = &self.pool;
        match self.block(dead_share_links(pool, &[(tenant, id)])) {
            Some(Ok(dead)) if !dead.is_empty() => {
                self.watch.unwatch(&file, id);
                log::info!("document {file}: share link {id} no longer works; disconnecting whoever joined with it");
                Handled { reply: AdminReply::Done, revoked: Some((file, link)), removed: None, blocked: None }
            }
            Some(Ok(_)) => failed("that link still works"),
            Some(Err(_)) | None => failed("the link couldn't be checked"),
        }
    }

    fn closing(&self) -> Closing {
        Closing::Restarting
    }
}

/// Re-checks a backend's watched links every interval and has the hub disconnect whoever joined
/// with one that no longer works (see the module docs). Stops when dropped.
pub struct RevocationSweeper {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RevocationSweeper {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RevocationSweeper").field("running", &self.thread.is_some()).finish()
    }
}

impl RevocationSweeper {
    /// Sweep `watch` (from [`CloudBackend::revocation_watch`]) every `interval` (at least 50 ms),
    /// with a database connection of its own, revoking through the hub listening at `hub` (an
    /// unspecified address means this host's loopback).
    pub fn start(config: &CloudBackendConfig, watch: RevocationWatch, hub: SocketAddr, interval: Duration) -> Result<Self, CloudBackendError> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_| CloudBackendError::Runtime)?;
        let timeout = config.timeout;
        let pool = {
            let _entered = runtime.enter();
            PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(timeout)
                .connect_lazy(&config.database_url)
                .map_err(|_| CloudBackendError::Database)?
        };
        let mut hub = hub;
        if hub.ip().is_unspecified() {
            hub.set_ip(if hub.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
        }
        let endpoint = Endpoint { host: hub.to_string(), path: "/".into(), secure: false };
        let interval = interval.max(MIN_REVOCATION_INTERVAL);
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("cloud-relay-revocation".into())
            .spawn(move || {
                // Stops on a message or when the sweeper (the sender) is dropped.
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                    sweep(&runtime, &pool, &watch, &endpoint, timeout);
                }
            })
            .map_err(|_| CloudBackendError::Runtime)?;
        Ok(Self { stop: Some(stop), thread: Some(thread) })
    }
}

impl Drop for RevocationSweeper {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One sweep: find the watched links that no longer work and revoke them through the hub.
fn sweep(runtime: &tokio::runtime::Runtime, pool: &PgPool, watch: &RevocationWatch, hub: &Endpoint, timeout: Duration) {
    let watched = watch.links();
    let mut revokes = Vec::new();
    for chunk in watched.chunks(MAX_CHECKED_LINKS) {
        let pairs: Vec<(TenantId, Uuid)> = chunk.iter().map(|(_, tenant, link)| (*tenant, *link)).collect();
        let dead = match runtime.block_on(async { tokio::time::timeout(timeout, dead_share_links(pool, &pairs)).await }) {
            Ok(Ok(dead)) => dead,
            Ok(Err(error)) => {
                log::warn!("revocation sweep: share links couldn't be checked ({error}); retrying next sweep");
                return;
            }
            Err(_) => {
                log::warn!("revocation sweep: checking share links timed out; retrying next sweep");
                return;
            }
        };
        let dead: HashSet<(TenantId, Uuid)> = dead.into_iter().collect();
        revokes.extend(
            chunk
                .iter()
                .filter(|(_, tenant, link)| dead.contains(&(*tenant, *link)))
                .map(|(file, _, link)| Admin::Revoke { file: file.clone(), link: link.to_string() }),
        );
    }
    let members = watch.members();
    for chunk in members.chunks(MAX_CHECKED_MEMBERS) {
        let admissions: Vec<MemberAdmission> = chunk.iter().map(|(_, admission)| *admission).collect();
        let dead = match runtime.block_on(async { tokio::time::timeout(timeout, dead_member_admissions(pool, &admissions)).await }) {
            Ok(Ok(dead)) => dead,
            Ok(Err(error)) => {
                log::warn!("revocation sweep: members couldn't be checked ({error}); retrying next sweep");
                break;
            }
            Err(_) => {
                log::warn!("revocation sweep: checking members timed out; retrying next sweep");
                break;
            }
        };
        let dead: HashSet<MemberAdmission> = dead.into_iter().collect();
        revokes.extend(
            chunk
                .iter()
                .filter(|(_, admission)| dead.contains(admission))
                .map(|(file, admission)| Admin::Revoke { file: file.clone(), link: member_link(admission.session) }),
        );
    }
    let limits = NetLimits::default();
    for requests in revokes.chunks(REVOKES_PER_REQUEST) {
        match figcraft_collab_net::manage::request(hub, &watch.key, requests.to_vec(), &limits, timeout) {
            Ok(replies) => {
                let done = replies.iter().filter(|reply| matches!(reply, AdminReply::Done)).count();
                log::info!("revocation sweep: {done} of {} dead share links and members disconnected", requests.len());
            }
            Err(_) => log::warn!("revocation sweep: the hub couldn't be reached; retrying next sweep"),
        }
    }
}
