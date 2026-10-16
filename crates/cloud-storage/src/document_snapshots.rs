//! Immutable document snapshots: store a collaboration [`SessionState`] in object storage and
//! restore the newest one that still verifies.
//!
//! [`store_snapshot`] follows `docs/saas/architecture.md` "Snapshot compaction": the state is
//! encoded with the collaboration wire encoding ([`message::encode`]), uploaded under a
//! content-addressed key ([`keys::snapshot`]: sequence plus SHA-256), registered as a `snapshot`
//! object, read back and verified, recorded as a `document_snapshots` row, referenced by that row
//! (`owner_kind = 'document_snapshot'`) and only then offered to the document's
//! `current_snapshot_id` pointer. The pointer moves only to a snapshot whose sequence is at least
//! the current pointer's and at most the document's committed `current_seq`, so it never moves
//! backwards and never claims history the log does not hold.
//!
//! [`restore_latest_snapshot`] follows "Room recovery": verified snapshots are tried newest first;
//! each one's bytes are read again and must match the recorded size and SHA-256, decode within
//! [`MAX_SNAPSHOT_BYTES`] and carry the recorded sequence. One that fails is skipped (and
//! reported) and the preceding one is tried, up to [`MAX_RESTORE_CANDIDATES`]. A store failure
//! that says nothing about the bytes stops the restore instead of silently using an older
//! snapshot. Nothing here marks a failed snapshot: verified rows never change state, and repair
//! or quarantine of a bad object is operator work.

use std::fmt;

use figcraft_cloud_domain::{CloudDocumentId, Uuid};
use figcraft_collab::SessionState;
use figcraft_collab::message;
use sha2::{Digest, Sha256};

use crate::TenantTransaction;
use crate::object_metadata::{
    NewObject, ObjectKind, ObjectRecord, ObjectRepositoryError, ObjectState, Rejection, Verification, get_object, keys, register_object,
    verify_object,
};
use crate::object_references::{ObjectReferenceError, OwnerKind, add_reference};
use crate::objects::{ObjectStore, ObjectStoreError};
use crate::snapshots::{RepositoryError, SnapshotRecord, register_snapshot, set_current_snapshot, verify_snapshot};
use crate::validation::MEDIA_SNAPSHOT;

/// Largest encoded snapshot stored or restored (well under the client's single-transfer cap).
pub const MAX_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;
/// Most verified snapshots one restore tries before giving up, newest first.
pub const MAX_RESTORE_CANDIDATES: i64 = 8;
const MAX_PRODUCER_VERSION_BYTES: usize = 64;

/// Fixed, non-leaking categories for snapshot storage and restore failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotStoreError {
    /// The state cannot be encoded, encodes past [`MAX_SNAPSHOT_BYTES`], its sequence does not fit
    /// the schema, or the producer version is empty or too long.
    InvalidInput,
    /// No such document in the transaction's tenant.
    NotFound,
    /// The snapshot's sequence is past the document's committed sequence.
    AheadOfLog,
    /// A live object already holds the key but cannot be reused (rejected, being deleted, or a
    /// different object), or a concurrent writer won a unique race; the caller may retry later.
    Conflict,
    /// The uploaded bytes failed read-back verification; the object row is now `rejected`.
    Rejected(Rejection),
    /// The object store failed in a way that says nothing about the bytes.
    Store(ObjectStoreError),
    Object(ObjectRepositoryError),
    Snapshot(RepositoryError),
    Reference(ObjectReferenceError),
    Database,
    CorruptRow,
}

impl fmt::Display for SnapshotStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "document snapshot error: {self:?}")
    }
}

impl std::error::Error for SnapshotStoreError {}

fn database_error(error: sqlx::Error) -> SnapshotStoreError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => SnapshotStoreError::Conflict,
        _ => SnapshotStoreError::Database,
    }
}

fn object_error(error: ObjectRepositoryError) -> SnapshotStoreError {
    match error {
        ObjectRepositoryError::Conflict => SnapshotStoreError::Conflict,
        ObjectRepositoryError::Store(error) => SnapshotStoreError::Store(error),
        other => SnapshotStoreError::Object(other),
    }
}

/// What a stored snapshot did to the document's `current_snapshot_id`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pointer {
    /// The pointer now names this snapshot.
    Moved,
    /// The pointer already names a snapshot at a later sequence and was left alone.
    Kept,
}

/// The result of [`store_snapshot`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSnapshot {
    pub snapshot: SnapshotRecord,
    pub object: ObjectRecord,
    pub pointer: Pointer,
    /// An object row for this key already existed (the same state was stored before) and was
    /// reused; its bytes were written and checked again.
    pub reused_object: bool,
}

/// Encode `state` and check it against the snapshot bounds.
fn encode_state(state: &SessionState) -> Result<(Vec<u8>, i64, i64), SnapshotStoreError> {
    let seq = i64::try_from(state.seq).map_err(|_| SnapshotStoreError::InvalidInput)?;
    let bytes = message::encode(state).map_err(|_| SnapshotStoreError::InvalidInput)?;
    let size = u64::try_from(bytes.len()).map_err(|_| SnapshotStoreError::InvalidInput)?;
    if size == 0 || size > MAX_SNAPSHOT_BYTES {
        return Err(SnapshotStoreError::InvalidInput);
    }
    let size = i64::try_from(size).map_err(|_| SnapshotStoreError::InvalidInput)?;
    Ok((bytes, seq, size))
}

/// The document's committed sequence, or `NotFound`. Read without a lock: the sequence only grows,
/// and [`offer_pointer`] checks again under the row lock.
async fn committed_seq(transaction: &mut TenantTransaction<'_>, document_id: CloudDocumentId) -> Result<i64, SnapshotStoreError> {
    let seq: Option<i64> = sqlx::query_scalar("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    seq.ok_or(SnapshotStoreError::NotFound)
}

/// The id of the live (not `deleted`) object row holding `key` in `bucket`, if any.
async fn live_object_for_key(transaction: &mut TenantTransaction<'_>, bucket: &str, key: &str) -> Result<Option<Uuid>, SnapshotStoreError> {
    let id: Option<String> =
        sqlx::query_scalar("SELECT id::text FROM objects WHERE tenant_id = $1::uuid AND bucket = $2 AND object_key = $3 AND state <> 'deleted'")
            .bind(transaction.tenant_id().to_string())
            .bind(bucket)
            .bind(key)
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    id.map(|id| id.parse().map_err(|_| SnapshotStoreError::CorruptRow)).transpose()
}

/// An existing verified snapshot row of `document_id` at `seq` over `object_id`, so storing the
/// same state twice records it once.
async fn existing_snapshot(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    seq: i64,
    object_id: Uuid,
) -> Result<Option<Uuid>, SnapshotStoreError> {
    let id: Option<String> = sqlx::query_scalar(concat!(
        "SELECT id::text FROM document_snapshots ",
        "WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq = $3 AND object_id = $4::uuid AND state = 'verified' ",
        "ORDER BY created_at, id LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(seq)
    .bind(object_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    id.map(|id| id.parse().map_err(|_| SnapshotStoreError::CorruptRow)).transpose()
}

/// Upload the snapshot object under `key` and return its verified record, and whether the row
/// already existed. The bytes are written every time: the key is content-addressed, so writing
/// the same bytes again is harmless and repairs a store that lost or damaged them. A row that was
/// already verified is checked by reading the bytes back here, because [`verify_object`] does not
/// re-read verified objects.
async fn stored_object(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    object: &NewObject,
    bytes: Vec<u8>,
) -> Result<(ObjectRecord, bool), SnapshotStoreError> {
    let existing = match live_object_for_key(transaction, store.bucket_name(), &object.object_key).await? {
        Some(id) => {
            let existing = get_object(transaction, id).await.map_err(object_error)?.ok_or(SnapshotStoreError::Conflict)?;
            let same = existing.kind == ObjectKind::Snapshot && existing.sha256 == object.sha256 && existing.size_bytes == object.size_bytes;
            // A pending row is an earlier attempt that never finished; anything else cannot be
            // reused (rejected or being deleted: operator repair and collection decide).
            if !same || !matches!(existing.state, ObjectState::Verified | ObjectState::Pending) {
                return Err(SnapshotStoreError::Conflict);
            }
            Some(existing)
        }
        None => None,
    };
    let id = match &existing {
        Some(existing) => existing.id,
        None => register_object(transaction, store.bucket_name(), object).await.map_err(object_error)?.id,
    };
    let expected = u64::try_from(object.size_bytes).map_err(|_| SnapshotStoreError::InvalidInput)?;
    store.put_object(&object.object_key, bytes, MEDIA_SNAPSHOT).await.map_err(SnapshotStoreError::Store)?;
    let reused = existing.is_some();
    if let Some(existing) = existing.filter(|existing| existing.state == ObjectState::Verified) {
        let stored = match store.get_object(&object.object_key, expected).await {
            Ok(stored) => stored,
            Err(ObjectStoreError::NotFound) => return Err(SnapshotStoreError::Rejected(Rejection::Missing)),
            Err(ObjectStoreError::TooLarge) => return Err(SnapshotStoreError::Rejected(Rejection::SizeMismatch)),
            Err(error) => return Err(SnapshotStoreError::Store(error)),
        };
        if u64::try_from(stored.len()).ok() != Some(expected) {
            return Err(SnapshotStoreError::Rejected(Rejection::SizeMismatch));
        }
        if <[u8; 32]>::from(Sha256::digest(&stored)) != object.sha256 {
            return Err(SnapshotStoreError::Rejected(Rejection::ChecksumMismatch));
        }
        return Ok((existing, true));
    }
    match verify_object(transaction, store, id).await.map_err(object_error)? {
        Some(Verification::Verified(record)) => Ok((record, reused)),
        Some(Verification::Rejected(_, rejection)) => Err(SnapshotStoreError::Rejected(rejection)),
        None => Err(SnapshotStoreError::Conflict),
    }
}

/// Move `document_id`'s `current_snapshot_id` to `snapshot` at `seq` unless the pointer already
/// names a later snapshot. The document row is locked first, so concurrent offers serialize and
/// the pointer's sequence is read fresh after the lock.
async fn offer_pointer(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    snapshot: Uuid,
    seq: i64,
) -> Result<Pointer, SnapshotStoreError> {
    let row: Option<(i64, Option<String>)> =
        sqlx::query_as("SELECT current_seq, current_snapshot_id::text FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
            .bind(transaction.tenant_id().to_string())
            .bind(document_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    let (current_seq, current) = row.ok_or(SnapshotStoreError::NotFound)?;
    if seq > current_seq {
        return Err(SnapshotStoreError::AheadOfLog);
    }
    if let Some(current) = current {
        let current_snapshot_seq: Option<i64> =
            sqlx::query_scalar("SELECT seq FROM document_snapshots WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid")
                .bind(transaction.tenant_id().to_string())
                .bind(document_id.to_string())
                .bind(current)
                .fetch_optional(transaction.connection())
                .await
                .map_err(database_error)?;
        // The pointer's foreign key guarantees the row exists; a missing one is corrupt state.
        if current_snapshot_seq.ok_or(SnapshotStoreError::CorruptRow)? > seq {
            return Ok(Pointer::Kept);
        }
    }
    set_current_snapshot(transaction, document_id, snapshot).await.map_err(SnapshotStoreError::Snapshot)?;
    Ok(Pointer::Moved)
}

/// Store `state` (a [`Server::state`](figcraft_collab::Server::state) of `document_id` at
/// `state.seq`) as an immutable, verified snapshot and offer it to the document's pointer.
///
/// Everything is written in `transaction`; the caller commits. The object bytes are uploaded
/// before the commit, so a rolled-back attempt leaves bytes without a row under a
/// content-addressed key, which a retry reuses. Storing the same state again reuses the verified
/// object and snapshot row. A state past the document's committed sequence is `AheadOfLog` and
/// nothing is uploaded.
pub async fn store_snapshot(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    document_id: CloudDocumentId,
    state: &SessionState,
    producer_version: &str,
) -> Result<StoredSnapshot, SnapshotStoreError> {
    if producer_version.is_empty() || producer_version.len() > MAX_PRODUCER_VERSION_BYTES {
        return Err(SnapshotStoreError::InvalidInput);
    }
    let (bytes, seq, size_bytes) = encode_state(state)?;
    if seq > committed_seq(transaction, document_id).await? {
        return Err(SnapshotStoreError::AheadOfLog);
    }
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Snapshot,
        object_key: keys::snapshot(transaction.tenant_id(), document_id, state.seq, &sha256),
        sha256,
        size_bytes,
        media_type: MEDIA_SNAPSHOT.to_owned(),
        producer_version: producer_version.to_owned(),
    };
    let (object, reused_object) = stored_object(transaction, store, &object, bytes).await?;

    let snapshot_id = match existing_snapshot(transaction, document_id, seq, object.id).await? {
        Some(id) => id,
        None => {
            let id = Uuid::new_v4();
            register_snapshot(transaction, id, document_id, seq, object.id, &object.sha256, object.size_bytes)
                .await
                .map_err(SnapshotStoreError::Snapshot)?;
            id
        }
    };
    let snapshot =
        verify_snapshot(transaction, snapshot_id, document_id).await.map_err(SnapshotStoreError::Snapshot)?.ok_or(SnapshotStoreError::Conflict)?;
    add_reference(transaction, object.id, OwnerKind::DocumentSnapshot, snapshot.id).await.map_err(SnapshotStoreError::Reference)?;
    let pointer = offer_pointer(transaction, document_id, snapshot.id, seq).await?;
    Ok(StoredSnapshot { snapshot, object, pointer, reused_object })
}

/// Why a verified snapshot was not used by a restore.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    /// The object is gone from the store.
    Missing,
    /// The stored bytes (or the object row) do not have the snapshot's recorded size.
    SizeMismatch,
    /// The stored bytes (or the object row) do not hash to the snapshot's recorded checksum.
    ChecksumMismatch,
    /// The bytes verify but do not decode as a session state.
    Undecodable,
    /// The decoded state is at a different sequence than its row.
    SequenceMismatch,
    /// The caller's check refused the decoded state.
    Refused,
}

/// A verified snapshot a restore passed over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SkippedSnapshot {
    pub id: Uuid,
    pub seq: i64,
    pub reason: SkipReason,
}

/// The result of a restore: the snapshot used and what the caller made of it, if any, and the
/// newer verified snapshots that were passed over (newest first).
#[derive(Clone, Debug, PartialEq)]
pub struct Restore<T> {
    pub restored: Option<(SnapshotRecord, T)>,
    pub skipped: Vec<SkippedSnapshot>,
}

type CandidateRow = (String, String, String, i64, String, i64, String, Vec<u8>, String, Vec<u8>, i64);

struct Candidate {
    record: SnapshotRecord,
    checksum: Vec<u8>,
    object_key: String,
    object_sha256: Vec<u8>,
    object_size: i64,
}

fn candidate(row: CandidateRow) -> Result<Candidate, SnapshotStoreError> {
    let (id, tenant_id, document_id, seq, object_id, size_bytes, state, checksum, object_key, object_sha256, object_size) = row;
    Ok(Candidate {
        record: SnapshotRecord {
            id: id.parse().map_err(|_| SnapshotStoreError::CorruptRow)?,
            tenant_id: tenant_id.parse().map_err(|_| SnapshotStoreError::CorruptRow)?,
            document_id: document_id.parse().map_err(|_| SnapshotStoreError::CorruptRow)?,
            seq,
            object_id: object_id.parse().map_err(|_| SnapshotStoreError::CorruptRow)?,
            size_bytes,
            state,
        },
        checksum,
        object_key,
        object_sha256,
        object_size,
    })
}

/// Check `bytes` against the candidate's recorded size and checksum and decode them.
fn decode_candidate(candidate: &Candidate, bytes: &[u8]) -> Result<SessionState, SkipReason> {
    if u64::try_from(bytes.len()).ok() != u64::try_from(candidate.record.size_bytes).ok() {
        return Err(SkipReason::SizeMismatch);
    }
    if Sha256::digest(bytes).as_slice() != candidate.checksum.as_slice() {
        return Err(SkipReason::ChecksumMismatch);
    }
    let max = usize::try_from(MAX_SNAPSHOT_BYTES).unwrap_or(usize::MAX);
    let state: SessionState = message::decode(bytes, max).map_err(|_| SkipReason::Undecodable)?;
    if i64::try_from(state.seq).ok() != Some(candidate.record.seq) {
        return Err(SkipReason::SequenceMismatch);
    }
    Ok(state)
}

/// Restore the newest verified snapshot of `document_id` that re-verifies and that `accept`
/// takes, falling back to preceding ones (at most [`MAX_RESTORE_CANDIDATES`] in all).
///
/// Only snapshots at or below the document's committed sequence whose object is a verified
/// `snapshot` in `store`'s bucket are candidates. `accept` sees each decoded state and returns
/// `None` to pass it over (for example when [`Server::restore`](figcraft_collab::Server::restore)
/// refuses it). `restored` is `None` when no candidate is usable. A store failure that says
/// nothing about the bytes is `Store`, so an outage never silently selects an older snapshot.
pub async fn restore_latest_snapshot_with<T>(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    document_id: CloudDocumentId,
    mut accept: impl FnMut(SessionState) -> Option<T>,
) -> Result<Restore<T>, SnapshotStoreError> {
    let rows: Vec<CandidateRow> = sqlx::query_as(concat!(
        "SELECT s.id::text, s.tenant_id::text, s.document_id::text, s.seq, s.object_id::text, s.size_bytes, s.state, ",
        "s.checksum, o.object_key, o.sha256, o.size_bytes ",
        "FROM document_snapshots s ",
        "JOIN documents d ON d.tenant_id = s.tenant_id AND d.id = s.document_id ",
        "JOIN objects o ON o.tenant_id = s.tenant_id AND o.id = s.object_id ",
        "WHERE s.tenant_id = $1::uuid AND s.document_id = $2::uuid AND s.state = 'verified' AND s.seq <= d.current_seq ",
        "AND o.kind = 'snapshot' AND o.state = 'verified' AND o.bucket = $3 ",
        "ORDER BY s.seq DESC, s.created_at DESC, s.id DESC LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(store.bucket_name())
    .bind(MAX_RESTORE_CANDIDATES)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;

    let mut skipped = Vec::new();
    for row in rows {
        let candidate = candidate(row)?;
        let skip = |reason| SkippedSnapshot { id: candidate.record.id, seq: candidate.record.seq, reason };
        if candidate.object_size != candidate.record.size_bytes {
            skipped.push(skip(SkipReason::SizeMismatch));
            continue;
        }
        if candidate.object_sha256 != candidate.checksum {
            skipped.push(skip(SkipReason::ChecksumMismatch));
            continue;
        }
        let Ok(expected) = u64::try_from(candidate.record.size_bytes) else {
            skipped.push(skip(SkipReason::SizeMismatch));
            continue;
        };
        if expected > MAX_SNAPSHOT_BYTES {
            skipped.push(skip(SkipReason::SizeMismatch));
            continue;
        }
        let bytes = match store.get_object(&candidate.object_key, expected).await {
            Ok(bytes) => bytes,
            Err(ObjectStoreError::NotFound) => {
                skipped.push(skip(SkipReason::Missing));
                continue;
            }
            Err(ObjectStoreError::TooLarge) => {
                skipped.push(skip(SkipReason::SizeMismatch));
                continue;
            }
            Err(error) => return Err(SnapshotStoreError::Store(error)),
        };
        let state = match decode_candidate(&candidate, &bytes) {
            Ok(state) => state,
            Err(reason) => {
                skipped.push(skip(reason));
                continue;
            }
        };
        match accept(state) {
            Some(value) => return Ok(Restore { restored: Some((candidate.record, value)), skipped }),
            None => skipped.push(skip(SkipReason::Refused)),
        }
    }
    Ok(Restore { restored: None, skipped })
}

/// [`restore_latest_snapshot_with`] returning the decoded [`SessionState`] itself.
pub async fn restore_latest_snapshot(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    document_id: CloudDocumentId,
) -> Result<Restore<SessionState>, SnapshotStoreError> {
    restore_latest_snapshot_with(transaction, store, document_id, Some).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_collab::{Document, Limits, Server};

    fn state() -> SessionState {
        Server::new(Document::new("Snapshot"), "", Limits::default()).state().unwrap()
    }

    fn candidate_for(bytes: &[u8], seq: i64) -> Candidate {
        Candidate {
            record: SnapshotRecord {
                id: Uuid::nil(),
                tenant_id: "a6000000-0000-4000-8000-000000000001".parse().unwrap(),
                document_id: "a6300000-0000-4000-8000-000000000001".parse().unwrap(),
                seq,
                object_id: Uuid::nil(),
                size_bytes: i64::try_from(bytes.len()).unwrap(),
                state: "verified".into(),
            },
            checksum: Sha256::digest(bytes).to_vec(),
            object_key: String::new(),
            object_sha256: Sha256::digest(bytes).to_vec(),
            object_size: i64::try_from(bytes.len()).unwrap(),
        }
    }

    #[test]
    fn encoded_states_round_trip_through_the_candidate_checks() {
        let mut state = state();
        state.seq = 3;
        let (bytes, seq, size) = encode_state(&state).unwrap();
        assert_eq!((seq, usize::try_from(size).unwrap()), (3, bytes.len()));
        assert_eq!(decode_candidate(&candidate_for(&bytes, 3), &bytes), Ok(state));
        assert_eq!(decode_candidate(&candidate_for(&bytes, 4), &bytes), Err(SkipReason::SequenceMismatch));
    }

    #[test]
    fn sequences_past_the_schema_are_invalid() {
        let mut state = state();
        state.seq = u64::MAX;
        assert_eq!(encode_state(&state).map(|_| ()), Err(SnapshotStoreError::InvalidInput));
    }

    #[test]
    fn junk_bytes_never_decode_into_a_restored_state() {
        let (good, _, _) = encode_state(&state()).unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..2000 {
            let mut bytes = match round % 4 {
                0 => good.clone(),
                1 => good.get(..good.len() / 2).unwrap_or_default().to_vec(),
                2 => Vec::new(),
                _ => (0..(next() % 512)).map(|_| u8::try_from(next() & 0xff).unwrap()).collect(),
            };
            if round % 4 == 0 && !bytes.is_empty() {
                let at = usize::try_from(next()).unwrap() % bytes.len();
                bytes[at] ^= u8::try_from(next() % 255 + 1).unwrap();
            }
            // Bytes whose recorded checksum matches them reach the decoder; it must refuse junk
            // without panicking. Bytes recorded under the good checksum never get that far.
            let _ = decode_candidate(&candidate_for(&bytes, 0), &bytes);
            if bytes != good {
                assert!(decode_candidate(&candidate_for(&good, 0), &bytes).is_err());
            }
        }
    }
}
