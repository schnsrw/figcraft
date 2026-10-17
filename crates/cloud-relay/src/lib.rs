//! Distributed collaboration relay for FigCraft Cloud.
//!
//! [`DurableRoom`] adapts the in-memory collaboration [`Server`] to cloud durability: a batch the
//! server accepts is appended to PostgreSQL under the room's fencing token and committed before
//! any confirmation or broadcast for it leaves the room. If the commit fails, the outgoing
//! messages are dropped and the room is poisoned: its in-memory state may hold a batch nobody
//! will hear of, so the transport must close every connection and the room is reopened from
//! durable state, where clients reconcile on reconnect. This matches the filesystem server's
//! rule (`figcraft-collab-net`'s `Backend::flush` before answers) without its transport.
//!
//! Client-id issue and refusals stay session state; only accepted batches are durable events.
//!
//! [`DurableRoom::recover`] restores the room from the document's latest verified snapshot (S3-008,
//! `figcraft_cloud_storage::document_snapshots`) and replays only the events after it.

pub mod collab_backend;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::document_snapshots::{SnapshotStoreError, restore_latest_snapshot_with};
use figcraft_cloud_storage::events::{EventRepositoryError, MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS, append_events, claim_sequencer, read_events};
use figcraft_cloud_storage::objects::ObjectStore;
use figcraft_cloud_storage::snapshots::SnapshotRecord;
use figcraft_collab::message::{Access, ClientMsg, Hello, ServerMsg};
use figcraft_collab::{Document, Journal, Limits, Outgoing, Server};
use sqlx::PgPool;

/// Fixed, non-leaking categories for room failures. Every one of them except `Join` leaves the
/// room poisoned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoomError {
    /// The room failed an earlier commit and must be reopened from durable state.
    Poisoned,
    /// A tenant-scoped transaction could not be started.
    Begin,
    /// The append was refused or failed.
    Append(EventRepositoryError),
    /// The append succeeded but the commit failed; the batch is not confirmed.
    Commit,
    /// Claiming the document or reading its log failed.
    Open(EventRepositoryError),
    /// A committed batch does not apply over the replayed state, or the replayed state does not
    /// reach the claimed sequence; the room is not served.
    Replay,
    /// Reading the document's snapshots failed in a way that says nothing about their bytes (the
    /// store or database is unavailable); no older snapshot is substituted.
    Snapshot(SnapshotStoreError),
}

impl std::fmt::Display for RoomError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "collaboration room error: {self:?}")
    }
}

impl std::error::Error for RoomError {}

/// One document's collaboration session whose accepted batches are durable before they are
/// confirmed.
pub struct DurableRoom {
    pool: PgPool,
    tenant_id: TenantId,
    document_id: CloudDocumentId,
    fencing_token: i64,
    server: Server,
    poisoned: bool,
    /// The snapshot (id, sequence) the room was restored from, if any.
    snapshot: Option<(Uuid, i64)>,
}

/// The newest verified snapshot of `document_id` at or below `claimed_seq` that re-verifies and
/// restores a server.
async fn restore_server(
    pool: &PgPool,
    store: &ObjectStore,
    tenant_id: TenantId,
    document_id: CloudDocumentId,
    claimed_seq: i64,
    token: &str,
    limits: Limits,
) -> Result<Option<(SnapshotRecord, Server)>, RoomError> {
    let mut transaction = begin_tenant_transaction(pool, tenant_id).await.map_err(|_| RoomError::Begin)?;
    let restore = restore_latest_snapshot_with(&mut transaction, store, document_id, |state| {
        let within = i64::try_from(state.seq).is_ok_and(|seq| seq <= claimed_seq);
        if within { Server::restore(state, token, limits).ok() } else { None }
    })
    .await
    .map_err(RoomError::Snapshot)?;
    transaction.rollback().await.map_err(|_| RoomError::Begin)?;
    Ok(restore.restored)
}

impl DurableRoom {
    /// Take over `document_id` as its sequencer and rebuild its session from the committed log.
    ///
    /// The claim commits first, so every earlier owner is fenced before this room reads; then the
    /// log is replayed page by page over `base` (the document at sequence 0) up to the sequence
    /// the claim resumed from. A batch that does not apply is `Replay`: the room is not served from
    /// uncertain state. [`recover`](Self::recover) starts from the latest verified snapshot
    /// instead; this path reads no objects and needs the whole log.
    pub async fn open(
        pool: PgPool,
        tenant_id: TenantId,
        document_id: CloudDocumentId,
        base: Document,
        token: &str,
        limits: Limits,
    ) -> Result<Self, RoomError> {
        Self::open_from(pool, None, tenant_id, document_id, base, token, limits).await
    }

    /// [`open`](Self::open), starting from the document's latest verified snapshot in `store`
    /// (`docs/saas/architecture.md`, "Room recovery").
    ///
    /// After the claim, verified snapshots at or below the claimed sequence are tried newest
    /// first: each object is read again and must match its recorded size and SHA-256, decode, and
    /// restore a [`Server`] ([`Server::restore`]); one that fails is passed over for the preceding
    /// one. Only the events after the restored snapshot's sequence are replayed. With no snapshot
    /// (or none usable) the room replays the whole log over `base`, which is the snapshot at
    /// sequence 0: it reproduces the committed state exactly or fails with `Replay` (a log that no
    /// longer reaches back to sequence 1), and a room that fails is never served. A store outage is
    /// `Snapshot(Store(..))` rather than a silent fallback.
    pub async fn recover(
        pool: PgPool,
        store: &ObjectStore,
        tenant_id: TenantId,
        document_id: CloudDocumentId,
        base: Document,
        token: &str,
        limits: Limits,
    ) -> Result<Self, RoomError> {
        Self::open_from(pool, Some(store), tenant_id, document_id, base, token, limits).await
    }

    async fn open_from(
        pool: PgPool,
        store: Option<&ObjectStore>,
        tenant_id: TenantId,
        document_id: CloudDocumentId,
        base: Document,
        token: &str,
        limits: Limits,
    ) -> Result<Self, RoomError> {
        let mut transaction = begin_tenant_transaction(&pool, tenant_id).await.map_err(|_| RoomError::Begin)?;
        let claim = claim_sequencer(&mut transaction, document_id).await.map_err(RoomError::Open)?;
        transaction.commit().await.map_err(|_| RoomError::Commit)?;

        let restored = match store {
            Some(store) => restore_server(&pool, store, tenant_id, document_id, claim.current_seq, token, limits).await?,
            None => None,
        };
        let (mut server, snapshot, mut after) = match restored {
            Some((record, server)) => (server, Some((record.id, record.seq)), record.seq),
            None => (Server::new(base, token, limits), None, 0),
        };
        while after < claim.current_seq {
            let mut transaction = begin_tenant_transaction(&pool, tenant_id).await.map_err(|_| RoomError::Begin)?;
            let page = read_events(&mut transaction, document_id, after, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await.map_err(RoomError::Open)?;
            transaction.rollback().await.map_err(|_| RoomError::Begin)?;
            if page.events.is_empty() {
                return Err(RoomError::Open(EventRepositoryError::Corrupt));
            }
            after = page.next_after_seq();
            for event in page.events.into_iter().take_while(|event| event.seq <= claim.current_seq) {
                server.replay(Journal::Accepted(event.batch)).map_err(|_| RoomError::Replay)?;
            }
        }
        if i64::try_from(server.seq()).ok() != Some(claim.current_seq) {
            return Err(RoomError::Replay);
        }
        let mut room = Self::from_parts(pool, tenant_id, document_id, claim.fencing_token, server);
        room.snapshot = snapshot;
        Ok(room)
    }

    /// A room over `server`, which must already hold the document's committed state through its
    /// current sequence, writing with `fencing_token` from
    /// [`claim_sequencer`](figcraft_cloud_storage::events::claim_sequencer).
    #[must_use]
    pub fn from_parts(pool: PgPool, tenant_id: TenantId, document_id: CloudDocumentId, fencing_token: i64, mut server: Server) -> Self {
        server.journaling(true);
        let _ = server.take_journal();
        DurableRoom { pool, tenant_id, document_id, fencing_token, server, poisoned: false, snapshot: None }
    }

    /// The snapshot (id, sequence) [`recover`](Self::recover) restored the room from; `None` when
    /// it replayed from the base document or the room was built another way.
    #[must_use]
    pub const fn restored_snapshot(&self) -> Option<(Uuid, i64)> {
        self.snapshot
    }

    #[must_use]
    pub fn server(&self) -> &Server {
        &self.server
    }

    #[must_use]
    pub const fn fencing_token(&self) -> i64 {
        self.fencing_token
    }

    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Admit a client. Joining changes only session state, so nothing is committed.
    pub fn join(&mut self, hello: Hello, access: Access) -> Result<Result<(u32, Vec<Outgoing>), ServerMsg>, RoomError> {
        if self.poisoned {
            return Err(RoomError::Poisoned);
        }
        let joined = self.server.join_as(hello, access);
        let _ = self.server.take_journal();
        Ok(joined)
    }

    /// A client left.
    pub fn leave(&mut self, client: u32) -> Vec<Outgoing> {
        let out = self.server.leave(client);
        let _ = self.server.take_journal();
        out
    }

    /// Handle a message from `from`, authenticated as `actor`. What the server accepted is
    /// committed before the returned messages may be delivered; on any error nothing is returned
    /// and the room is poisoned.
    pub async fn receive(&mut self, from: u32, actor: PrincipalId, msg: ClientMsg) -> Result<Vec<Outgoing>, RoomError> {
        if self.poisoned {
            return Err(RoomError::Poisoned);
        }
        let out = self.server.receive(from, msg);
        let accepted: Vec<_> = self
            .server
            .take_journal()
            .into_iter()
            .filter_map(|entry| match entry {
                Journal::Accepted(batch) => Some(batch),
                Journal::Issued { .. } | Journal::Refused { .. } => None,
            })
            .collect();
        if accepted.is_empty() {
            return Ok(out);
        }
        match self.commit(actor, &accepted).await {
            Ok(()) => Ok(out),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    async fn commit(&self, actor: PrincipalId, accepted: &[figcraft_collab::Accepted]) -> Result<(), RoomError> {
        let mut transaction = begin_tenant_transaction(&self.pool, self.tenant_id).await.map_err(|_| RoomError::Begin)?;
        append_events(&mut transaction, self.document_id, self.fencing_token, actor, accepted).await.map_err(RoomError::Append)?;
        transaction.commit().await.map_err(|_| RoomError::Commit)
    }
}
