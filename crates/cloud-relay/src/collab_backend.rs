//! The collaboration hub's backend for FigCraft Cloud (core slice): the same protocol
//! `figcraft-server` speaks (`figcraft_collab_net::hub`), with documents in PostgreSQL and S3.
//!
//! - **Joining.** A hello's `file` is the cloud document's UUID and its `token` a share-link secret
//!   ([`figcraft_cloud_storage::share_links`]). The link must be live and name that document; an
//!   editor link joins with [`Access::Edit`], a viewer link with [`Access::View`]. Every refusal
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
//! The hub calls the backend on its own thread and the backend is synchronous, so the backend owns
//! a small tokio runtime and blocks on each storage call, every one bounded by a timeout.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::document_snapshots::store_snapshot;
use figcraft_cloud_storage::events::{MAX_APPEND_BATCHES, append_events};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::share_links::{ShareRole, resolve_share_link};
use figcraft_collab::message::Access;
use figcraft_collab::{Accepted, Document, Journal, Limits, Server};
use figcraft_collab_net::hub::{Admission, Backend, Closing};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::DurableRoom;

/// What every refused hello is told (never why: unknown, revoked, expired and wrong-file links
/// read the same).
pub const REFUSED: &str = "this link isn't valid (it may have been revoked or have expired)";
/// What a hello is told when the document can't be served right now.
pub const UNAVAILABLE: &str = "this document can't be opened right now: try again in a moment";
/// The producer recorded with the snapshots this backend stores.
pub const PRODUCER: &str = concat!("figcraft-cloud-relay ", env!("CARGO_PKG_VERSION"));
/// The name of [`base_document`] (fixed: the document's own name is metadata, not content).
pub const BASE_NAME: &str = "Untitled";
/// Admissions remembered for documents that aren't open yet.
const MAX_ADMITTED: usize = 4096;

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
}

impl CloudBackendConfig {
    #[must_use]
    pub fn new(database_url: impl Into<String>, object_store: ObjectStoreSettings) -> Self {
        Self {
            database_url: database_url.into(),
            object_store,
            limits: Limits::default(),
            timeout: Duration::from_secs(15),
            max_connections: 8,
            max_open: 1024,
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

/// Who may open a document, from the link a hello was admitted with.
#[derive(Clone, Copy, Debug)]
struct Admitted {
    tenant: TenantId,
    /// Whom accepted batches are recorded as made by (the link's creator until people sign in).
    actor: PrincipalId,
}

/// One open document.
struct OpenDoc {
    tenant: TenantId,
    document: CloudDocumentId,
    actor: PrincipalId,
    fencing_token: i64,
    /// Accepted batches not yet committed.
    pending: Vec<Accepted>,
    /// The sequence of the latest snapshot stored (or restored).
    snapshot_seq: Option<u64>,
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
}

impl CloudBackend {
    /// Connect to the database (lazily: the first storage call opens connections) and the object
    /// store.
    pub fn connect(config: &CloudBackendConfig) -> Result<Self, CloudBackendError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
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
        })
    }

    /// Documents open now.
    #[must_use]
    pub fn open_documents(&self) -> usize {
        self.open.len()
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
        let (pool, tenant, document, token, actor) = (&self.pool, doc.tenant, doc.document, doc.fencing_token, doc.actor);
        let committed = self.block(async move {
            let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| "begin")?;
            for chunk in pending.chunks(MAX_APPEND_BATCHES) {
                append_events(&mut transaction, document, token, actor, chunk).await.map_err(|_| "append")?;
            }
            transaction.commit().await.map_err(|_| "commit")
        });
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
        let stored = self.block(async move {
            let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| ())?;
            store_snapshot(&mut transaction, store, document, &state, PRODUCER).await.map_err(|_| ())?;
            transaction.commit().await.map_err(|_| ())
        });
        if matches!(stored, Some(Ok(()))) {
            doc.snapshot_seq = Some(server.seq());
        } else {
            log::warn!("document {file}: snapshot at seq {} couldn't be stored (its events still hold it)", server.seq());
        }
    }

    /// Recover `document` as its new sequencer.
    fn recover(&self, file: &str, admitted: Admitted, document: CloudDocumentId) -> Result<(Server, OpenDoc), String> {
        let (pool, store, limits) = (self.pool.clone(), &self.store, self.limits);
        let recovered = self.block(async move { DurableRoom::recover(pool, store, admitted.tenant, document, base_document(), "", limits).await });
        let room = match recovered {
            Some(Ok(room)) => room,
            Some(Err(error)) => {
                log::error!("document {file} couldn't be recovered: {error}");
                return Err(UNAVAILABLE.into());
            }
            None => {
                log::error!("document {file}: recovery timed out");
                return Err(UNAVAILABLE.into());
            }
        };
        let restored = room.restored_snapshot();
        let state = room.server().state().map_err(|_| UNAVAILABLE.to_string())?;
        let mut server = Server::restore(state, "", self.limits).map_err(|_| UNAVAILABLE.to_string())?;
        server.journaling(true);
        let _ = server.take_journal();
        let snapshot_seq = restored.and_then(|(_, seq)| u64::try_from(seq).ok());
        let mut doc = OpenDoc {
            tenant: admitted.tenant,
            document,
            actor: admitted.actor,
            fencing_token: room.fencing_token(),
            pending: Vec::new(),
            snapshot_seq,
        };
        if snapshot_seq.is_none() && server.seq() == 0 {
            // A new document: its first state becomes durable before anyone edits it.
            self.snapshot(file, &mut doc, &server);
        }
        Ok((server, doc))
    }
}

impl Backend for CloudBackend {
    fn admit(&mut self, file: &str, token: &str) -> Result<Admission, String> {
        let Ok(document) = file.parse::<CloudDocumentId>() else {
            return Err(REFUSED.into());
        };
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
        if self.admitted.len() >= MAX_ADMITTED && !self.admitted.contains_key(file) {
            let open = &self.open;
            self.admitted.retain(|file, _| open.contains_key(file));
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
        if self.open.len() >= self.max_open && !self.open.contains_key(file) {
            log::warn!("document {file} refused: {} documents are open", self.open.len());
            return Err(UNAVAILABLE.into());
        }
        let (server, doc) = self.recover(file, admitted, document)?;
        log::info!("document {file} opened at seq {} (fencing token {})", server.seq(), doc.fencing_token);
        self.open.insert(file.to_string(), doc);
        Ok(server)
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
        match self.open.get_mut(file) {
            Some(doc) => doc.pending.extend(accepted),
            // Not ours to keep: it must not be confirmed.
            None if !self.failed.iter().any(|f| f == file) => self.failed.push(file.to_string()),
            None => {}
        }
    }

    fn flush(&mut self) -> Vec<String> {
        let mut failed = std::mem::take(&mut self.failed);
        let files: Vec<String> = self.open.iter().filter(|(_, doc)| !doc.pending.is_empty()).map(|(file, _)| file.clone()).collect();
        for file in files {
            let Some(mut doc) = self.open.remove(&file) else { continue };
            if self.commit(&file, &mut doc) {
                self.open.insert(file, doc);
            } else {
                failed.push(file);
            }
        }
        failed
    }

    fn idle(&mut self, file: &str, server: &mut Server) -> bool {
        self.changed(file, server);
        if let Some(mut doc) = self.open.remove(file) {
            // Batches of this burst not flushed yet: everyone has left, so nobody was told of
            // them; keep them if possible, and snapshot only state that is committed.
            if self.commit(file, &mut doc) {
                self.snapshot(file, &mut doc, server);
            }
        }
        self.failed.retain(|f| f != file);
        self.admitted.remove(file);
        true
    }

    fn stop(&mut self, open: Vec<(&str, &mut Server)>) {
        for (file, server) in open {
            self.changed(file, server);
            if let Some(mut doc) = self.open.remove(file)
                && self.commit(file, &mut doc)
            {
                self.snapshot(file, &mut doc, server);
            }
        }
        self.failed.clear();
    }

    fn closing(&self) -> Closing {
        Closing::Restarting
    }
}
