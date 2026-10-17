//! Durable document event encoding and checksum format.
//!
//! A `document_events` row stores one batch the collaboration server accepted. Its `payload` is
//! that batch's existing collaboration wire form, [`figcraft_collab::message::encode`] of an
//! [`Accepted`] (the wire-version byte, then postcard), so replay hands the relay the same value
//! the in-memory server journaled and broadcast, with no second document format. The filesystem
//! server's `Journal::Accepted` log entries carry the same `Accepted` value, which keeps the S2-021
//! migration a re-encode rather than a translation.
//!
//! Only accepted batches are events: they alone advance the document sequence. Client-id issue and
//! batch refusals are session state, not ordered document history.
//!
//! `checksum` is SHA-256 over a domain-separated frame:
//!
//! ```text
//! "figcraft.cloud.document-event.v1\0"
//! tenant_id (16 bytes) || document_id (16 bytes) || seq (i64, big-endian)
//! payload length (u64, big-endian) || payload
//! ```
//!
//! Binding the tenant, document and sequence means a payload copied to another row, document or
//! tenant fails verification even though its bytes are intact. The fencing token and actor are
//! provenance checked by the append transaction (S2-015), not part of the event's content.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_collab::Accepted;
use figcraft_collab::message;
use sha2::{Digest, Sha256};

use crate::TenantTransaction;
use crate::attribution::Via;

/// Largest stored payload, matching the `document_events.payload` `CHECK` constraint. It is
/// smaller than the collaboration protocol's message limit, so the relay must refuse a larger
/// batch before confirming it rather than discover the limit at commit.
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Length of [`event_checksum`], matching the `document_events.checksum` `CHECK` constraint.
pub const EVENT_CHECKSUM_BYTES: usize = 32;

const CHECKSUM_DOMAIN: &[u8] = b"figcraft.cloud.document-event.v1\0";

/// Fixed categories for event encoding and verification failures. Decoder details are not exposed,
/// because they can quote stored document content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCodecError {
    /// The batch could not be encoded.
    Encode,
    /// The payload is empty or larger than [`MAX_EVENT_PAYLOAD_BYTES`].
    PayloadSize,
    /// The sequence number is not in `1..=i64::MAX`.
    Sequence,
    /// The stored checksum does not match the row's identity and payload.
    Checksum,
    /// The payload is not a valid encoded batch.
    Decode,
    /// The payload decodes, but names a different sequence number than its row.
    SequenceMismatch,
}

impl std::fmt::Display for EventCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "document event codec error: {self:?}")
    }
}

impl std::error::Error for EventCodecError {}

/// One accepted batch ready to insert as a `document_events` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedEvent {
    pub seq: i64,
    pub payload: Vec<u8>,
    pub checksum: [u8; EVENT_CHECKSUM_BYTES],
}

/// Encode `batch` for storage under `tenant_id` and `document_id`.
pub fn encode_event(tenant_id: TenantId, document_id: CloudDocumentId, batch: &Accepted) -> Result<EncodedEvent, EventCodecError> {
    let seq = stored_seq(batch.seq)?;
    let payload = message::encode(batch).map_err(|_| EventCodecError::Encode)?;
    check_payload_size(&payload)?;
    let checksum = event_checksum(tenant_id, document_id, seq, &payload);
    Ok(EncodedEvent { seq, payload, checksum })
}

/// Verify a stored row and decode its batch.
///
/// The checksum is checked before the payload is decoded, so corrupted or relocated bytes never
/// reach the postcard decoder.
pub fn decode_event(
    tenant_id: TenantId,
    document_id: CloudDocumentId,
    seq: i64,
    payload: &[u8],
    checksum: &[u8],
) -> Result<Accepted, EventCodecError> {
    if seq <= 0 {
        return Err(EventCodecError::Sequence);
    }
    check_payload_size(payload)?;
    if checksum != event_checksum(tenant_id, document_id, seq, payload).as_slice() {
        return Err(EventCodecError::Checksum);
    }
    let batch = message::decode::<Accepted>(payload, MAX_EVENT_PAYLOAD_BYTES).map_err(|_| EventCodecError::Decode)?;
    if i64::try_from(batch.seq).ok() != Some(seq) {
        return Err(EventCodecError::SequenceMismatch);
    }
    Ok(batch)
}

/// The checksum of an event row, in the frame described in the module documentation.
#[must_use]
pub fn event_checksum(tenant_id: TenantId, document_id: CloudDocumentId, seq: i64, payload: &[u8]) -> [u8; EVENT_CHECKSUM_BYTES] {
    let length = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    Sha256::new()
        .chain_update(CHECKSUM_DOMAIN)
        .chain_update(tenant_id.as_uuid().as_bytes())
        .chain_update(document_id.as_uuid().as_bytes())
        .chain_update(seq.to_be_bytes())
        .chain_update(length.to_be_bytes())
        .chain_update(payload)
        .finalize()
        .into()
}

fn stored_seq(seq: u64) -> Result<i64, EventCodecError> {
    i64::try_from(seq).ok().filter(|seq| *seq > 0).ok_or(EventCodecError::Sequence)
}

fn check_payload_size(payload: &[u8]) -> Result<(), EventCodecError> {
    if payload.is_empty() || payload.len() > MAX_EVENT_PAYLOAD_BYTES {
        return Err(EventCodecError::PayloadSize);
    }
    Ok(())
}

/// Most batches one append may write, keeping a single transaction's work bounded.
pub const MAX_APPEND_BATCHES: usize = 1024;

/// Fixed, non-leaking categories for event repository failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventRepositoryError {
    /// The request itself is unusable: no batches, too many, or a batch that cannot be encoded.
    InvalidInput,
    /// No such document in this tenant.
    NotFound,
    /// The document exists but is not `active`, so it cannot accept edits.
    InvalidState,
    /// The batches do not continue the document's committed sequence. `current` is the committed
    /// sequence the caller must reconcile from.
    SequenceConflict {
        current: i64,
    },
    /// The caller's fencing token is not the document's current one: another sequencer claimed
    /// the document since (or the token was never issued). The caller must stop writing.
    StaleFence,
    /// A stored event failed verification on read: its checksum, encoding or inner sequence is
    /// wrong, or the committed log has a gap. Replay stops rather than skip history.
    Corrupt,
    Conflict,
    Constraint,
    /// The database could not be reached or the connection was lost mid-call: an I/O or
    /// protocol failure, a pool timeout or closed pool, a connection-exception SQLSTATE (class
    /// `08`) or a server shutdown/termination (`57P01`, `57P02`, `57P03`).
    ///
    /// **Ambiguous for a commit.** If this is the error from (or in place of) `COMMIT`, the
    /// transaction may or may not be durable: a lost acknowledgement looks exactly like a lost
    /// connection. Before any statement of a transaction completes it is safe to retry on a fresh
    /// connection. After the append ran, retry the same batches: the retry either lands (nothing
    /// was durable) or fails with [`EventRepositoryError::SequenceConflict`], and then
    /// [`read_events`] at the batch's sequence tells whether the durable event is the caller's own
    /// batch or a rival's (see `tests/failure_injection.rs`).
    Unavailable,
    /// The server cancelled a statement: `statement_timeout` or a cancel request (`57014`
    /// `query_canceled`) or `lock_timeout` / `NOWAIT` (`55P03` `lock_not_available`). The
    /// transaction is aborted and nothing of it can commit, so rolling back and retrying the whole
    /// transaction is safe (with backoff: the contention that caused it may persist).
    Timeout,
    /// The transaction lost a serialization race: `40001` `serialization_failure` or `40P01`
    /// `deadlock_detected`. The transaction is aborted and nothing of it can commit; rolling back
    /// and retrying the whole transaction is safe.
    SerializationFailure,
    /// Any other database failure. Not known to be transient, so not safe to retry blindly.
    Database,
}

impl EventRepositoryError {
    /// Whether retrying the whole transaction on a fresh connection is known to be safe: the
    /// failed transaction was aborted, so none of it can be durable. `Unavailable` is not
    /// included because a connection lost around `COMMIT` leaves the outcome unknown; see its
    /// documentation for how to resolve it.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::SerializationFailure)
    }

    /// Whether the transaction's outcome may be unknown (a connection lost around `COMMIT`).
    #[must_use]
    pub const fn is_ambiguous(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

impl std::fmt::Display for EventRepositoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "document event repository error: {self:?}")
    }
}

impl std::error::Error for EventRepositoryError {}

fn event_database_error(error: sqlx::Error) -> EventRepositoryError {
    match &error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => EventRepositoryError::Unavailable,
        _ => match error.as_database_error().and_then(|database| database.code()) {
            Some(code) => event_sqlstate_error(&code),
            None => EventRepositoryError::Database,
        },
    }
}

/// Map a SQLSTATE to a repository error. Only codes whose meaning is unambiguous are mapped; the
/// rest stay [`EventRepositoryError::Database`].
fn event_sqlstate_error(code: &str) -> EventRepositoryError {
    match code {
        "23505" => EventRepositoryError::Conflict,
        "23503" | "23514" => EventRepositoryError::Constraint,
        "57014" | "55P03" => EventRepositoryError::Timeout,
        "40001" | "40P01" => EventRepositoryError::SerializationFailure,
        "57P01" | "57P02" | "57P03" => EventRepositoryError::Unavailable,
        code if code.len() == 5 && code.starts_with("08") => EventRepositoryError::Unavailable,
        _ => EventRepositoryError::Database,
    }
}

/// A sequencer's claim on a document: the fencing token its appends must carry and the committed
/// sequence it resumes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequencerClaim {
    pub fencing_token: i64,
    pub current_seq: i64,
}

/// Take over as a document's sequencer by advancing its fencing token.
///
/// Every earlier token becomes stale at commit, so a previous owner's later appends fail with
/// `StaleFence` even if it still believes it holds the lease. The distributed lease that decides
/// *who* may claim is S5 work; this is the database half of the fence.
pub async fn claim_sequencer(transaction: &mut TenantTransaction<'_>, document_id: CloudDocumentId) -> Result<SequencerClaim, EventRepositoryError> {
    let state: Option<(String,)> = sqlx::query_as("SELECT state FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(event_database_error)?;
    match state {
        None => return Err(EventRepositoryError::NotFound),
        Some((state,)) if state != "active" => return Err(EventRepositoryError::InvalidState),
        Some(_) => {}
    }
    let (fencing_token, current_seq): (i64, i64) = sqlx::query_as(
        "UPDATE documents SET fencing_token = fencing_token + 1, updated_at = now() \
         WHERE tenant_id = $1::uuid AND id = $2::uuid \
         RETURNING fencing_token, current_seq",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(event_database_error)?;
    Ok(SequencerClaim { fencing_token, current_seq })
}

/// The committed sequence range one append wrote, inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Appended {
    pub first_seq: i64,
    pub last_seq: i64,
}

/// Append accepted batches to a document's durable event log and advance its sequence.
///
/// The document row is locked (`FOR UPDATE`) for the rest of the caller's transaction, so
/// concurrent appends to one document serialize. `batches` must continue the committed sequence
/// exactly (`current_seq + 1`, `+ 2`, ...); anything else is `SequenceConflict` carrying the
/// committed sequence, and nothing is written. `fencing_token` must be the document's current
/// token from [`claim_sequencer`]; a superseded or never-issued token is `StaleFence`, checked
/// under the same row lock so a takeover cannot interleave. The caller confirms the batches only
/// after committing the transaction.
pub async fn append_events(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    fencing_token: i64,
    actor: PrincipalId,
    batches: &[Accepted],
) -> Result<Appended, EventRepositoryError> {
    append_events_via(transaction, document_id, fencing_token, actor, None, batches).await
}

/// [`append_events`] that also records how `actor` held access (S6-039): `via` is written to the
/// rows' `via_kind`/`via_id`. `None` records "unknown".
pub async fn append_events_via(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    fencing_token: i64,
    actor: PrincipalId,
    via: Option<Via>,
    batches: &[Accepted],
) -> Result<Appended, EventRepositoryError> {
    if batches.is_empty() || batches.len() > MAX_APPEND_BATCHES {
        return Err(EventRepositoryError::InvalidInput);
    }
    let tenant_id = transaction.tenant_id();
    let encoded = batches
        .iter()
        .map(|batch| encode_event(tenant_id, document_id, batch))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| EventRepositoryError::InvalidInput)?;

    let row: Option<(i64, i64, String)> =
        sqlx::query_as("SELECT current_seq, fencing_token, state FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
            .bind(tenant_id.to_string())
            .bind(document_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(event_database_error)?;
    let Some((current, current_token, state)) = row else {
        return Err(EventRepositoryError::NotFound);
    };
    if state != "active" {
        return Err(EventRepositoryError::InvalidState);
    }
    if fencing_token != current_token || fencing_token <= 0 {
        return Err(EventRepositoryError::StaleFence);
    }
    let consecutive = encoded.iter().zip(1_i64..).all(|(event, offset)| current.checked_add(offset) == Some(event.seq));
    let (Some(first), Some(last)) = (encoded.first(), encoded.last()) else {
        return Err(EventRepositoryError::InvalidInput);
    };
    if !consecutive {
        return Err(EventRepositoryError::SequenceConflict { current });
    }
    let appended = Appended { first_seq: first.seq, last_seq: last.seq };

    let seqs: Vec<i64> = encoded.iter().map(|event| event.seq).collect();
    let checksums: Vec<Vec<u8>> = encoded.iter().map(|event| event.checksum.to_vec()).collect();
    let payloads: Vec<Vec<u8>> = encoded.into_iter().map(|event| event.payload).collect();
    sqlx::query(
        "INSERT INTO document_events (tenant_id, document_id, seq, fencing_token, payload, checksum, actor_principal_id, via_kind, via_id) \
         SELECT $1::uuid, $2::uuid, event.seq, $3, event.payload, event.checksum, $4::uuid, $8, $9::uuid \
         FROM UNNEST($5::bigint[], $6::bytea[], $7::bytea[]) AS event (seq, payload, checksum)",
    )
    .bind(tenant_id.to_string())
    .bind(document_id.to_string())
    .bind(fencing_token)
    .bind(actor.to_string())
    .bind(seqs)
    .bind(payloads)
    .bind(checksums)
    .bind(via.map(Via::kind))
    .bind(via.and_then(Via::id).map(|id| id.to_string()))
    .execute(transaction.connection())
    .await
    .map_err(event_database_error)?;
    sqlx::query("UPDATE documents SET current_seq = $3, updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant_id.to_string())
        .bind(document_id.to_string())
        .bind(appended.last_seq)
        .execute(transaction.connection())
        .await
        .map_err(event_database_error)?;
    Ok(appended)
}

/// Most events one replay page returns, whatever the caller asks for.
pub const MAX_REPLAY_EVENTS: usize = 1000;

/// Largest payload total one replay page returns, whatever the caller asks for. A page always
/// carries at least one event when one is due, so a single event up to
/// [`MAX_EVENT_PAYLOAD_BYTES`] can exceed a smaller budget.
pub const MAX_REPLAY_BYTES: usize = 32 * 1024 * 1024;

/// One committed event read back from the log, verified and decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayedEvent {
    pub seq: i64,
    pub fencing_token: i64,
    pub actor: PrincipalId,
    pub batch: Accepted,
}

/// One page of replay. `events` are consecutive from the requested sequence; `current_seq` is the
/// document's committed sequence when the page was read.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayPage {
    pub events: Vec<ReplayedEvent>,
    pub current_seq: i64,
    after_seq: i64,
}

impl ReplayPage {
    /// The sequence to pass as `after_seq` for the next page: the last returned event, or the
    /// requested sequence when the page is empty.
    #[must_use]
    pub fn next_after_seq(&self) -> i64 {
        self.events.last().map_or(self.after_seq, |event| event.seq)
    }

    /// Whether committed events remain after this page. Callers loop until this is false.
    #[must_use]
    pub fn has_more(&self) -> bool {
        self.next_after_seq() < self.current_seq
    }
}

/// `seq`, `fencing_token`, `actor_principal_id` (as text), `payload`, `checksum`.
type StoredEventRow = (i64, i64, String, Vec<u8>, Vec<u8>);

/// Read a document's committed events with `seq > after_seq`, in order, as one bounded page.
///
/// The page holds at most `max_events` (clamped to [`MAX_REPLAY_EVENTS`]; zero is
/// `InvalidInput`) and at most `max_bytes` of payload (clamped to [`MAX_REPLAY_BYTES`]), except
/// that it always holds the next event when one exists, so a caller looping on
/// [`ReplayPage::next_after_seq`] always progresses. Both caps are applied in SQL: event sizes are
/// summed from `octet_length` before any payload is fetched, so an oversized page is never
/// transferred. Reads stop at the document's `current_seq`, which an append advances in the
/// same transaction as its events, so only committed history is replayed.
///
/// Every row is verified with [`decode_event`] (checksum before decode) and the page must
/// continue `after_seq` without gaps; a failure is `Corrupt` and returns no events. A negative
/// `after_seq` is `InvalidInput`; one beyond the committed sequence is `SequenceConflict`
/// carrying it, because the caller claims history the log does not have.
pub async fn read_events(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    after_seq: i64,
    max_events: usize,
    max_bytes: usize,
) -> Result<ReplayPage, EventRepositoryError> {
    if after_seq < 0 || max_events == 0 {
        return Err(EventRepositoryError::InvalidInput);
    }
    let limit = i64::try_from(max_events.min(MAX_REPLAY_EVENTS)).map_err(|_| EventRepositoryError::InvalidInput)?;
    let budget = i64::try_from(max_bytes.min(MAX_REPLAY_BYTES)).map_err(|_| EventRepositoryError::InvalidInput)?;
    let tenant_id = transaction.tenant_id();

    let current: Option<(i64,)> = sqlx::query_as("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant_id.to_string())
        .bind(document_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(event_database_error)?;
    let Some((current_seq,)) = current else {
        return Err(EventRepositoryError::NotFound);
    };
    if after_seq > current_seq {
        return Err(EventRepositoryError::SequenceConflict { current: current_seq });
    }
    if after_seq == current_seq {
        return Ok(ReplayPage { events: Vec::new(), current_seq, after_seq });
    }

    // `page` picks the next rows by sequence and size only; payloads are joined in afterwards for
    // the prefix that fits the budget (and always the first row).
    let rows: Vec<StoredEventRow> = sqlx::query_as(
        "WITH page AS ( \
             SELECT seq, octet_length(payload)::bigint AS size FROM document_events \
             WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq > $3 AND seq <= $4 \
             ORDER BY seq LIMIT $5 \
         ), sized AS ( \
             SELECT seq, sum(size) OVER (ORDER BY seq) AS running, row_number() OVER (ORDER BY seq) AS position FROM page \
         ) \
         SELECT event.seq, event.fencing_token, event.actor_principal_id::text, event.payload, event.checksum \
         FROM sized JOIN document_events event \
           ON event.tenant_id = $1::uuid AND event.document_id = $2::uuid AND event.seq = sized.seq \
         WHERE sized.running <= $6 OR sized.position = 1 \
         ORDER BY event.seq",
    )
    .bind(tenant_id.to_string())
    .bind(document_id.to_string())
    .bind(after_seq)
    .bind(current_seq)
    .bind(limit)
    .bind(budget)
    .fetch_all(transaction.connection())
    .await
    .map_err(event_database_error)?;

    // Events up to `current_seq` were committed with it, so an empty page here is missing history.
    if rows.is_empty() || rows.len() > max_events.min(MAX_REPLAY_EVENTS) {
        return Err(EventRepositoryError::Corrupt);
    }
    let mut events = Vec::with_capacity(rows.len());
    let mut expected = after_seq;
    for (seq, fencing_token, actor, payload, checksum) in rows {
        expected = expected.checked_add(1).ok_or(EventRepositoryError::Corrupt)?;
        if seq != expected {
            return Err(EventRepositoryError::Corrupt);
        }
        let batch = decode_event(tenant_id, document_id, seq, &payload, &checksum).map_err(|_| EventRepositoryError::Corrupt)?;
        let actor = actor.parse().map_err(|_| EventRepositoryError::Corrupt)?;
        events.push(ReplayedEvent { seq, fencing_token, actor, batch });
    }
    Ok(ReplayPage { events, current_seq, after_seq })
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_collab::{Change, Json, NodeId, Prop};

    fn ids() -> (TenantId, CloudDocumentId) {
        ("10000000-0000-4000-8000-000000000001".parse().unwrap(), "20000000-0000-4000-8000-000000000002".parse().unwrap())
    }

    fn batch(seq: u64) -> Accepted {
        Accepted {
            seq,
            author: 3,
            batch_id: 7,
            changes: vec![
                Change::Set { id: NodeId((4 << 32) | 9), prop: Prop::Name, value: Json::of("Café ☕").unwrap() },
                Change::Delete { id: NodeId(5) },
            ],
        }
    }

    #[test]
    fn round_trips_an_accepted_batch_in_the_collaboration_wire_form() {
        let (tenant, document) = ids();
        let encoded = encode_event(tenant, document, &batch(42)).unwrap();
        assert_eq!(encoded.seq, 42);
        assert_eq!(encoded.payload, message::encode(&batch(42)).unwrap());
        assert_eq!(encoded.payload.first(), Some(&message::WIRE_VERSION));
        assert_eq!(decode_event(tenant, document, 42, &encoded.payload, &encoded.checksum), Ok(batch(42)));
    }

    #[test]
    fn checksum_format_is_pinned() {
        let (tenant, document) = ids();
        let mut frame = Vec::new();
        frame.extend_from_slice(b"figcraft.cloud.document-event.v1\0");
        frame.extend_from_slice(&[0x10, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        frame.extend_from_slice(&[0x20, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 2]);
        frame.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 9]);
        frame.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(b"abc");
        let expected: [u8; 32] = Sha256::digest(&frame).into();
        assert_eq!(event_checksum(tenant, document, 9, b"abc"), expected);
    }

    #[test]
    fn checksum_binds_tenant_document_sequence_and_payload() {
        let (tenant, document) = ids();
        let encoded = encode_event(tenant, document, &batch(5)).unwrap();
        let other_tenant: TenantId = "10000000-0000-4000-8000-000000000009".parse().unwrap();
        let other_document: CloudDocumentId = "20000000-0000-4000-8000-000000000009".parse().unwrap();
        let (payload, checksum) = (&encoded.payload, &encoded.checksum);
        assert_eq!(decode_event(other_tenant, document, 5, payload, checksum), Err(EventCodecError::Checksum));
        assert_eq!(decode_event(tenant, other_document, 5, payload, checksum), Err(EventCodecError::Checksum));
        assert_eq!(decode_event(tenant, document, 6, payload, checksum), Err(EventCodecError::Checksum));
        let mut flipped = payload.clone();
        if let Some(last) = flipped.last_mut() {
            *last ^= 1;
        }
        assert_eq!(decode_event(tenant, document, 5, &flipped, checksum), Err(EventCodecError::Checksum));
        assert_eq!(decode_event(tenant, document, 5, payload, &checksum[..31]), Err(EventCodecError::Checksum));
    }

    #[test]
    fn rejects_sequences_the_table_cannot_hold() {
        let (tenant, document) = ids();
        assert_eq!(encode_event(tenant, document, &batch(0)), Err(EventCodecError::Sequence));
        assert_eq!(encode_event(tenant, document, &batch(u64::MAX)), Err(EventCodecError::Sequence));
        assert_eq!(encode_event(tenant, document, &batch(i64::MAX as u64)).map(|event| event.seq), Ok(i64::MAX));
        assert_eq!(decode_event(tenant, document, 0, b"x", &[0; 32]), Err(EventCodecError::Sequence));
        assert_eq!(decode_event(tenant, document, -1, b"x", &[0; 32]), Err(EventCodecError::Sequence));
    }

    #[test]
    fn rejects_payloads_outside_the_stored_size_range() {
        let (tenant, document) = ids();
        let huge = Accepted {
            changes: vec![Change::Set { id: NodeId(1), prop: Prop::Name, value: Json::of(&"x".repeat(MAX_EVENT_PAYLOAD_BYTES)).unwrap() }],
            ..batch(1)
        };
        assert_eq!(encode_event(tenant, document, &huge), Err(EventCodecError::PayloadSize));
        assert_eq!(decode_event(tenant, document, 1, &[], &event_checksum(tenant, document, 1, &[])), Err(EventCodecError::PayloadSize));
    }

    #[test]
    fn rejects_checksummed_garbage_and_a_mismatched_inner_sequence() {
        let (tenant, document) = ids();
        for junk in [&[0u8][..], &[message::WIRE_VERSION], &[message::WIRE_VERSION, 0xff, 0xff, 0xff], &[9, 1, 2, 3]] {
            assert_eq!(decode_event(tenant, document, 1, junk, &event_checksum(tenant, document, 1, junk)), Err(EventCodecError::Decode));
        }
        let payload = message::encode(&batch(8)).unwrap();
        assert_eq!(
            decode_event(tenant, document, 9, &payload, &event_checksum(tenant, document, 9, &payload)),
            Err(EventCodecError::SequenceMismatch)
        );
    }

    #[test]
    fn truncated_and_mutated_payloads_with_valid_checksums_never_decode_or_panic() {
        let (tenant, document) = ids();
        let payload = message::encode(&batch(3)).unwrap();
        for end in 1..payload.len() {
            let prefix = payload.get(..end).unwrap();
            assert!(decode_event(tenant, document, 3, prefix, &event_checksum(tenant, document, 3, prefix)).is_err());
        }
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..2_000 {
            let mut mutated = payload.clone();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let index = usize::try_from(state % payload.len() as u64).unwrap();
            if let Some(byte) = mutated.get_mut(index) {
                *byte = byte.wrapping_add(u8::try_from(state >> 56).unwrap() | 1);
            }
            let _ = decode_event(tenant, document, 3, &mutated, &event_checksum(tenant, document, 3, &mutated));
        }
    }

    #[test]
    fn replay_page_reports_where_to_resume_and_whether_more_remains() {
        let actor: PrincipalId = "10000000-0000-4000-8000-000000000003".parse().unwrap();
        let empty = ReplayPage { events: Vec::new(), current_seq: 7, after_seq: 7 };
        assert_eq!((empty.next_after_seq(), empty.has_more()), (7, false));
        let event = |seq: i64| ReplayedEvent { seq, fencing_token: 1, actor, batch: batch(u64::try_from(seq).unwrap()) };
        let partial = ReplayPage { events: vec![event(3), event(4)], current_seq: 7, after_seq: 2 };
        assert_eq!((partial.next_after_seq(), partial.has_more()), (4, true));
        let last = ReplayPage { events: vec![event(5), event(6), event(7)], current_seq: 7, after_seq: 4 };
        assert_eq!((last.next_after_seq(), last.has_more()), (7, false));
    }

    #[test]
    fn sqlstates_map_to_precise_retry_categories() {
        use super::{EventRepositoryError as E, event_sqlstate_error as map};
        assert_eq!(map("23505"), E::Conflict);
        assert_eq!(map("23503"), E::Constraint);
        assert_eq!(map("23514"), E::Constraint);
        assert_eq!(map("57014"), E::Timeout);
        assert_eq!(map("55P03"), E::Timeout);
        assert_eq!(map("40001"), E::SerializationFailure);
        assert_eq!(map("40P01"), E::SerializationFailure);
        for code in ["57P01", "57P02", "57P03", "08000", "08003", "08006", "08P01"] {
            assert_eq!(map(code), E::Unavailable, "{code}");
        }
        for code in ["", "08", "0800", "080000", "42P01", "XX000", "22P02", "57000"] {
            assert_eq!(map(code), E::Database, "{code:?}");
        }
    }

    #[test]
    fn transport_failures_are_unavailable_and_other_driver_errors_are_database() {
        use super::{EventRepositoryError as E, event_database_error as map};
        assert_eq!(map(sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset))), E::Unavailable);
        assert_eq!(map(sqlx::Error::Protocol("unexpected eof".to_owned())), E::Unavailable);
        assert_eq!(map(sqlx::Error::PoolTimedOut), E::Unavailable);
        assert_eq!(map(sqlx::Error::PoolClosed), E::Unavailable);
        assert_eq!(map(sqlx::Error::WorkerCrashed), E::Unavailable);
        assert_eq!(map(sqlx::Error::RowNotFound), E::Database);
        assert_eq!(map(sqlx::Error::ColumnNotFound("x".to_owned())), E::Database);
    }

    #[test]
    fn only_aborted_transactions_are_blindly_retryable_and_lost_connections_are_ambiguous() {
        use super::EventRepositoryError as E;
        let all = [
            E::InvalidInput,
            E::NotFound,
            E::InvalidState,
            E::SequenceConflict { current: 1 },
            E::StaleFence,
            E::Corrupt,
            E::Conflict,
            E::Constraint,
            E::Unavailable,
            E::Timeout,
            E::SerializationFailure,
            E::Database,
        ];
        let retryable: Vec<E> = all.into_iter().filter(|error| error.is_retryable()).collect();
        let ambiguous: Vec<E> = all.into_iter().filter(|error| error.is_ambiguous()).collect();
        assert_eq!(retryable, vec![E::Timeout, E::SerializationFailure]);
        assert_eq!(ambiguous, vec![E::Unavailable]);
    }
}
