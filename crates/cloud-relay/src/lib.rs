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

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::{EventRepositoryError, MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS, append_events, claim_sequencer, read_events};
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
    /// A committed batch does not apply over the replayed state; the room is not served.
    Replay,
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
}

impl DurableRoom {
    /// Take over `document_id` as its sequencer and rebuild its session from the committed log.
    ///
    /// The claim commits first, so every earlier owner is fenced before this room reads; then the
    /// log is replayed page by page over `base` (the document at sequence 0) up to the sequence
    /// the claim resumed from. A batch that does not apply is `Replay`: the room is not served from
    /// uncertain state. Restoring from a snapshot instead of `base` arrives with S3 object storage.
    pub async fn open(
        pool: PgPool,
        tenant_id: TenantId,
        document_id: CloudDocumentId,
        base: Document,
        token: &str,
        limits: Limits,
    ) -> Result<Self, RoomError> {
        let mut transaction = begin_tenant_transaction(&pool, tenant_id).await.map_err(|_| RoomError::Begin)?;
        let claim = claim_sequencer(&mut transaction, document_id).await.map_err(RoomError::Open)?;
        transaction.commit().await.map_err(|_| RoomError::Commit)?;

        let mut server = Server::new(base, token, limits);
        let mut after = 0;
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
        Ok(Self::from_parts(pool, tenant_id, document_id, claim.fencing_token, server))
    }

    /// A room over `server`, which must already hold the document's committed state through its
    /// current sequence, writing with `fencing_token` from
    /// [`claim_sequencer`](figcraft_cloud_storage::events::claim_sequencer).
    #[must_use]
    pub fn from_parts(pool: PgPool, tenant_id: TenantId, document_id: CloudDocumentId, fencing_token: i64, mut server: Server) -> Self {
        server.journaling(true);
        let _ = server.take_journal();
        DurableRoom { pool, tenant_id, document_id, fencing_token, server, poisoned: false }
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
