//! Repeatable filesystem-server to PostgreSQL/S3 import with per-resource checkpoints (S3-018),
//! and read-only verification of the result before cutover (S3-019).
//!
//! [`import_plan`] executes a [`MigrationPlan`] (S3-017) against a stopped legacy data directory.
//! For each planned document, [`import_document`] re-reads `snapshot.bin` and `log.bin` read-only
//! and bounded, checks they still match the plan, and then runs four stages, each idempotent and
//! detectable from the rows an earlier attempt left behind:
//!
//! 1. **Document**: the `documents` row with the plan's deterministic id, created at the snapshot
//!    sequence (`current_seq` = snapshot seq). An existing row with that id is resumed.
//! 2. **Mapping**: the `external_mappings` row (`figcraft-server`, `document`, legacy id).
//! 3. **Snapshot**: the snapshot body (the `SessionState` wire bytes) uploaded content-addressed
//!    (`keys::snapshot`), its `objects` row registered and verified against the stored bytes, the
//!    `document_snapshots` row registered and verified, an object reference from that snapshot,
//!    and `current_snapshot_id` pointed at it.
//! 4. **Events**: the log's replayable accepted batches after the snapshot sequence, appended
//!    with `append_events` in bounded chunks. Only batches after the document's committed
//!    `current_seq` are appended, so a rerun after a crash continues and never duplicates.
//!
//! A rerun of a completed import writes nothing (no fencing-token claim, no upload). The import
//! never writes, renames or locks anything under the legacy root. Errors are fixed categories;
//! no path, driver or store message is carried.
//!
//! Libraries, links and plugins are not imported yet: [`verify_import`] lists them as not imported,
//! which blocks cutover (see `docs/saas/filesystem-migration-report.md`, "Import and cutover
//! verification").

use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use figcraft_cloud_domain::{CloudDocumentId, ResourceState, Uuid};
use figcraft_collab::message;
use figcraft_collab::{Accepted, Journal, SessionState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::events::{EventRepositoryError, MAX_APPEND_BATCHES, append_events, claim_sequencer};
use crate::legacy_scan::ScanLimits;
use crate::migration_plan::{DocumentPlan, LEGACY_SYSTEM_ID, MigrationPlan, PLAN_FORMAT_VERSION, PlanBlocker};
use crate::object_metadata::{
    NewObject, ObjectKind, ObjectRepositoryError, ObjectState, Verification, get_object, keys, register_object, verify_object,
};
use crate::object_references::{OwnerKind, add_reference};
use crate::objects::{ObjectStore, ObjectStoreError, valid_key};
use crate::{TenantTransaction, begin_tenant_transaction};

/// `producer_version` recorded on objects this import writes.
pub const PRODUCER_VERSION: &str = "figcraft-legacy-import/1";
/// Most batches appended in one transaction.
pub const MAX_IMPORT_CHUNK_BATCHES: usize = 256;
/// Most legacy record bytes appended in one transaction (a chunk always holds at least one batch).
pub const MAX_IMPORT_CHUNK_BYTES: u64 = 8 << 20;

const SNAP_MAGIC: &[u8; 4] = b"FCSN";
const LOG_MAGIC: &[u8; 4] = b"FCLG";
const DISK_FORMAT: u8 = 1;
const SNAP_HEADER: usize = 17;
const LOG_HEADER: usize = 13;
const RECORD_HEADER: usize = 8;

/// Fixed, non-leaking categories for import failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportError {
    /// The plan has a blocker other than unapproved exclusions, or exclusions were not approved.
    PlanNotExecutable,
    /// The plan's format is not one this importer understands.
    UnsupportedPlanFormat,
    /// The plan targets another bucket than the store.
    BucketMismatch,
    /// A plan entry is internally inconsistent (hand-edited or corrupt plan).
    InvalidPlanEntry,
    /// A legacy file could not be opened or read, is a link, or is larger than the limits.
    LegacyUnreadable,
    /// The legacy files no longer match the plan (sizes, sequences, damage): rescan and replan.
    LegacyChanged,
    /// Existing cloud rows contradict the plan (another workspace/project, a mapping to another
    /// id, a sequence beyond the legacy log, a non-active document). Needs an operator.
    Diverged,
    /// The stored snapshot object was rejected on verification.
    SnapshotRejected,
    /// A sequencer claimed the document while the import was appending.
    StaleFence,
    Database,
    Store(ObjectStoreError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "legacy import error: {self:?}")
    }
}

impl std::error::Error for ImportError {}

fn db<E>(_: E) -> ImportError {
    ImportError::Database
}

fn object_error(error: ObjectRepositoryError) -> ImportError {
    match error {
        ObjectRepositoryError::Store(store) => ImportError::Store(store),
        ObjectRepositoryError::InvalidState => ImportError::SnapshotRejected,
        _ => ImportError::Database,
    }
}

fn event_error(error: EventRepositoryError) -> ImportError {
    match error {
        EventRepositoryError::StaleFence => ImportError::StaleFence,
        EventRepositoryError::InvalidState | EventRepositoryError::SequenceConflict { .. } => ImportError::Diverged,
        _ => ImportError::Database,
    }
}

/// One import stage, in order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ImportStage {
    Document,
    Mapping,
    Snapshot,
    Events,
}

/// Operator choices for one run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportOptions {
    /// Proceed although the plan excludes documents (the operator reviewed `excluded`).
    pub approve_exclusions: bool,
    /// Stop each document after this stage (staged imports; crash-resume rehearsal).
    pub stop_after: Option<ImportStage>,
}

/// How one document's run went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportStatus {
    /// This run created the document row.
    Created,
    /// The document existed and this run wrote something more.
    Resumed,
    /// Everything was already in place; nothing was written.
    AlreadyComplete,
}

/// The per-document result of [`import_document`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportOutcome {
    pub legacy_id: String,
    pub document_id: CloudDocumentId,
    pub status: ImportStatus,
    pub document_created: bool,
    pub mapping_created: bool,
    /// Snapshot bytes were uploaded in this run.
    pub snapshot_uploaded: bool,
    /// The snapshot row (and current-snapshot pointer) was written in this run.
    pub snapshot_registered: bool,
    pub events_appended: u64,
    /// Planned events an earlier run had already appended.
    pub events_already_present: u64,
    /// The last stage run, when [`ImportOptions::stop_after`] stopped early.
    pub stopped_after: Option<ImportStage>,
}

/// One planned document's result inside [`PlanImport`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentImport {
    pub legacy_id: String,
    pub result: Result<ImportOutcome, ImportError>,
}

/// The result of [`import_plan`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanImport {
    pub documents: Vec<DocumentImport>,
    /// Legacy ids the plan excluded, which were not touched.
    pub skipped: Vec<String>,
}

impl PlanImport {
    /// Whether every planned document imported.
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        self.documents.iter().all(|document| document.result.is_ok())
    }
}

/// Check a plan can run against `store` with `options`.
fn check_plan(plan: &MigrationPlan, store: &ObjectStore, options: &ImportOptions) -> Result<(), ImportError> {
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(ImportError::UnsupportedPlanFormat);
    }
    if plan.target.bucket != store.bucket_name() {
        return Err(ImportError::BucketMismatch);
    }
    let unapproved = plan.blockers.iter().any(|blocker| *blocker != PlanBlocker::ExclusionApprovalRequired || !options.approve_exclusions);
    if unapproved {
        return Err(ImportError::PlanNotExecutable);
    }
    Ok(())
}

/// Import every planned document of `plan` from the legacy data directory `root`.
///
/// Excluded documents are never touched (and are listed in `skipped`); a failing document does
/// not stop the others. Run it again after any failure: completed stages are skipped.
pub async fn import_plan(
    pool: &PgPool,
    store: &ObjectStore,
    plan: &MigrationPlan,
    root: &Path,
    limits: &ScanLimits,
    options: &ImportOptions,
) -> Result<PlanImport, ImportError> {
    check_plan(plan, store, options)?;
    let mut documents = Vec::with_capacity(plan.documents.len());
    for entry in &plan.documents {
        let result = import_document(pool, store, plan, entry, root, limits, options).await;
        documents.push(DocumentImport { legacy_id: entry.legacy_id.clone(), result });
    }
    Ok(PlanImport { documents, skipped: plan.excluded.iter().map(|excluded| excluded.legacy_id.clone()).collect() })
}

/// The legacy snapshot body and what its header and decoding say.
pub(crate) struct LegacySnapshot {
    pub(crate) generation: u64,
    pub(crate) seq: u64,
    pub(crate) body: Vec<u8>,
}

/// One replayable accepted batch and its record's body length.
pub(crate) struct LegacyBatch {
    pub(crate) batch: Accepted,
    pub(crate) record_bytes: u64,
}

/// What an import of `entry` must write, re-read from the legacy folder and checked against it.
pub(crate) struct LegacyDocument {
    pub(crate) snapshot: LegacySnapshot,
    pub(crate) sha256: [u8; 32],
    pub(crate) object_key: String,
    pub(crate) batches: Vec<LegacyBatch>,
}

/// The server's document id rule; re-checked so a hand-edited plan cannot escape `files/`.
fn valid_legacy_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn document_dir(root: &Path, legacy_id: &str) -> Result<PathBuf, ImportError> {
    if !valid_legacy_id(legacy_id) {
        return Err(ImportError::InvalidPlanEntry);
    }
    Ok(root.join("files").join(legacy_id))
}

/// Read a regular file (never a link) of at most `max` bytes; `Ok(None)` when it is absent.
fn read_bounded(path: &Path, max: u64) -> Result<Option<Vec<u8>>, ImportError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => return Err(ImportError::LegacyUnreadable),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ImportError::LegacyUnreadable),
    }
    let file = File::open(path).map_err(|_| ImportError::LegacyUnreadable)?;
    match file.metadata() {
        Ok(meta) if meta.is_file() && meta.len() <= max => {}
        _ => return Err(ImportError::LegacyUnreadable),
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes).map_err(|_| ImportError::LegacyUnreadable)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > max) {
        return Err(ImportError::LegacyUnreadable);
    }
    Ok(Some(bytes))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..at.checked_add(4)?).and_then(|slice| slice.try_into().ok()).map(u32::from_le_bytes)
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    bytes.get(at..at.checked_add(8)?).and_then(|slice| slice.try_into().ok()).map(u64::from_le_bytes)
}

/// Parse `snapshot.bin` bytes: header, CRC-32, then the `SessionState` body. `None` when damaged.
pub(crate) fn parse_snapshot(bytes: &[u8]) -> Option<LegacySnapshot> {
    if bytes.get(..4) != Some(SNAP_MAGIC.as_slice()) || bytes.get(4) != Some(&DISK_FORMAT) {
        return None;
    }
    let generation = u64_at(bytes, 5)?;
    let sum = u32_at(bytes, 13)?;
    let body = bytes.get(SNAP_HEADER..)?;
    if body.is_empty() || crc32fast::hash(body) != sum {
        return None;
    }
    let state = message::decode::<SessionState>(body, body.len()).ok()?;
    Some(LegacySnapshot { generation, seq: state.seq, body: body.to_vec() })
}

/// The replayable accepted batches of `log.bin` bytes, as the server's replay picks them: only a
/// log of the snapshot's generation counts; reading stops at the first damaged record or after
/// `max_entries` records; batches at or below the reached sequence are skipped and the first that
/// does not continue it ends the replay.
pub(crate) fn parse_log(bytes: &[u8], generation: u64, base_seq: u64, max_entries: usize) -> Vec<LegacyBatch> {
    let mut out = Vec::new();
    if bytes.get(..4) != Some(LOG_MAGIC.as_slice()) || bytes.get(4) != Some(&DISK_FORMAT) || u64_at(bytes, 5) != Some(generation) {
        return out;
    }
    let mut at = LOG_HEADER;
    let mut reached = base_seq;
    let mut records = 0_usize;
    while records < max_entries {
        let (Some(len), Some(sum)) = (u32_at(bytes, at), u32_at(bytes, at.saturating_add(4))) else { break };
        let Some(start) = at.checked_add(RECORD_HEADER) else { break };
        let Some(end) = usize::try_from(len).ok().and_then(|len| start.checked_add(len)) else { break };
        let Some(body) = bytes.get(start..end) else { break };
        if crc32fast::hash(body) != sum {
            break;
        }
        let Ok(entry) = message::decode::<Journal>(body, body.len()) else { break };
        records += 1;
        at = end;
        if let Journal::Accepted(batch) = entry {
            if batch.seq <= reached {
                continue;
            }
            if reached.checked_add(1) != Some(batch.seq) {
                break;
            }
            reached = batch.seq;
            out.push(LegacyBatch { batch, record_bytes: u64::from(len) });
        }
    }
    out
}

/// Re-read `entry`'s legacy folder and check it against the plan.
pub(crate) fn read_legacy(plan: &MigrationPlan, entry: &DocumentPlan, root: &Path, limits: &ScanLimits) -> Result<LegacyDocument, ImportError> {
    let dir = document_dir(root, &entry.legacy_id)?;
    let bytes = read_bounded(&dir.join("snapshot.bin"), limits.max_snapshot_bytes)?.ok_or(ImportError::LegacyUnreadable)?;
    let snapshot = parse_snapshot(&bytes).ok_or(ImportError::LegacyChanged)?;
    drop(bytes);
    if snapshot.seq != entry.snapshot.seq || u64::try_from(snapshot.body.len()).ok() != Some(entry.snapshot.object.size_bytes) {
        return Err(ImportError::LegacyChanged);
    }
    let batches = if entry.events.count == 0 {
        Vec::new()
    } else {
        let log = read_bounded(&dir.join("log.bin"), limits.max_log_bytes)?.ok_or(ImportError::LegacyChanged)?;
        parse_log(&log, snapshot.generation, snapshot.seq, limits.max_log_entries)
    };
    let last = batches.last().map_or(snapshot.seq, |batch| batch.batch.seq);
    if batches.len() as u64 != entry.events.count || last != entry.document.current_seq {
        return Err(ImportError::LegacyChanged);
    }
    let sha256: [u8; 32] = Sha256::digest(&snapshot.body).into();
    let object_key = keys::snapshot(plan.target.tenant_id, entry.document.id, snapshot.seq, &sha256);
    let planned = &entry.snapshot.object;
    if !valid_key(&object_key) || !object_key.starts_with(&planned.key_prefix) || !object_key.ends_with(&planned.key_suffix) {
        return Err(ImportError::InvalidPlanEntry);
    }
    Ok(LegacyDocument { snapshot, sha256, object_key, batches })
}

/// `current_seq` and `current_snapshot_id` of a document, when it exists.
pub(crate) async fn document_progress(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
) -> Result<Option<(i64, Option<String>)>, ImportError> {
    sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT current_seq, current_snapshot_id::text FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(db)
}

/// The state of a snapshot row, when it exists.
pub(crate) async fn snapshot_state(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    snapshot_id: Uuid,
) -> Result<Option<(i64, String, String)>, ImportError> {
    sqlx::query_as::<_, (i64, String, String)>(
        "SELECT seq, object_id::text, state FROM document_snapshots WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(snapshot_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(db)
}

async fn begin<'a>(pool: &'a PgPool, plan: &MigrationPlan) -> Result<TenantTransaction<'a>, ImportError> {
    begin_tenant_transaction(pool, plan.target.tenant_id).await.map_err(db)
}

fn seq_i64(seq: u64) -> Result<i64, ImportError> {
    i64::try_from(seq).map_err(|_| ImportError::InvalidPlanEntry)
}

/// Import one planned document; see the module docs for the stages and their checkpoints.
pub async fn import_document(
    pool: &PgPool,
    store: &ObjectStore,
    plan: &MigrationPlan,
    entry: &DocumentPlan,
    root: &Path,
    limits: &ScanLimits,
    options: &ImportOptions,
) -> Result<ImportOutcome, ImportError> {
    if plan.target.bucket != store.bucket_name() {
        return Err(ImportError::BucketMismatch);
    }
    let document_id = entry.document.id;
    if entry.external_mapping.internal_id != document_id.as_uuid() || entry.external_mapping.external_id != entry.legacy_id {
        return Err(ImportError::InvalidPlanEntry);
    }
    let base_seq = seq_i64(entry.snapshot.seq)?;
    let last_seq = seq_i64(entry.document.current_seq)?;
    let legacy = read_legacy(plan, entry, root, limits)?;
    let mut outcome = ImportOutcome {
        legacy_id: entry.legacy_id.clone(),
        document_id,
        status: ImportStatus::AlreadyComplete,
        document_created: false,
        mapping_created: false,
        snapshot_uploaded: false,
        snapshot_registered: false,
        events_appended: 0,
        events_already_present: 0,
        stopped_after: None,
    };
    let stop = |stage: ImportStage, outcome: &mut ImportOutcome| {
        let stopping = options.stop_after == Some(stage);
        if stopping {
            outcome.stopped_after = Some(stage);
        }
        stopping
    };

    // Stage 1: the document row, at the snapshot sequence.
    let mut tx = begin(pool, plan).await?;
    match crate::metadata::get_document(&mut tx, document_id).await.map_err(db)? {
        Some(existing) => {
            if existing.workspace_id != plan.target.workspace_id
                || existing.project_id != plan.target.project_id
                || existing.state != ResourceState::Active
            {
                return Err(ImportError::Diverged);
            }
            tx.rollback().await.map_err(db)?;
        }
        None => {
            crate::metadata::create_document(&mut tx, document_id, plan.target.workspace_id, plan.target.project_id, &entry.document.name)
                .await
                .map_err(|error| if error == crate::metadata::RepositoryError::Conflict { ImportError::Diverged } else { ImportError::Database })?;
            sqlx::query("UPDATE documents SET current_seq = $3 WHERE tenant_id = $1::uuid AND id = $2::uuid AND current_seq = 0")
                .bind(tx.tenant_id().to_string())
                .bind(document_id.to_string())
                .bind(base_seq)
                .execute(tx.connection())
                .await
                .map_err(db)?;
            tx.commit().await.map_err(db)?;
            outcome.document_created = true;
        }
    }
    if stop(ImportStage::Document, &mut outcome) {
        return Ok(finish(outcome));
    }

    // Stage 2: the external mapping.
    let mapping = &entry.external_mapping;
    let mut tx = begin(pool, plan).await?;
    match crate::external_mappings::get_external_mapping(&mut tx, LEGACY_SYSTEM_ID, &mapping.resource_kind, &mapping.external_id).await.map_err(db)? {
        Some(existing) if existing.internal_id == document_id.as_uuid() => tx.rollback().await.map_err(db)?,
        Some(_) => return Err(ImportError::Diverged),
        None => {
            crate::external_mappings::create_external_mapping(
                &mut tx,
                LEGACY_SYSTEM_ID,
                &mapping.resource_kind,
                &mapping.external_id,
                document_id.as_uuid(),
            )
            .await
            .map_err(db)?;
            tx.commit().await.map_err(db)?;
            outcome.mapping_created = true;
        }
    }
    if stop(ImportStage::Mapping, &mut outcome) {
        return Ok(finish(outcome));
    }

    // Stage 3: the snapshot object and row.
    import_snapshot(pool, store, plan, entry, &legacy, &mut outcome).await?;
    if stop(ImportStage::Snapshot, &mut outcome) {
        return Ok(finish(outcome));
    }

    // Stage 4: events after the committed sequence.
    let mut tx = begin(pool, plan).await?;
    let (current, _) = document_progress(&mut tx, document_id).await?.ok_or(ImportError::Diverged)?;
    tx.rollback().await.map_err(db)?;
    if current < base_seq || current > last_seq {
        return Err(ImportError::Diverged);
    }
    outcome.events_already_present = u64::try_from(current - base_seq).unwrap_or(0);
    let pending: Vec<&LegacyBatch> = legacy.batches.iter().filter(|batch| i64::try_from(batch.batch.seq).is_ok_and(|seq| seq > current)).collect();
    if !pending.is_empty() {
        let mut tx = begin(pool, plan).await?;
        let claim = claim_sequencer(&mut tx, document_id).await.map_err(event_error)?;
        tx.commit().await.map_err(db)?;
        if claim.current_seq != current {
            return Err(ImportError::Diverged);
        }
        let limit = MAX_IMPORT_CHUNK_BATCHES.min(MAX_APPEND_BATCHES);
        let mut rest = pending.as_slice();
        while !rest.is_empty() {
            let mut take = 0_usize;
            let mut bytes = 0_u64;
            for batch in rest.iter().take(limit) {
                if take > 0 && bytes.saturating_add(batch.record_bytes) > MAX_IMPORT_CHUNK_BYTES {
                    break;
                }
                bytes = bytes.saturating_add(batch.record_bytes);
                take += 1;
            }
            let (chunk, tail) = rest.split_at(take.max(1).min(rest.len()));
            let batches: Vec<Accepted> = chunk.iter().map(|batch| batch.batch.clone()).collect();
            let mut tx = begin(pool, plan).await?;
            append_events(&mut tx, document_id, claim.fencing_token, entry.events.actor_principal_id, &batches).await.map_err(event_error)?;
            tx.commit().await.map_err(db)?;
            outcome.events_appended = outcome.events_appended.saturating_add(batches.len() as u64);
            rest = tail;
        }
    }
    stop(ImportStage::Events, &mut outcome);
    Ok(finish(outcome))
}

fn finish(mut outcome: ImportOutcome) -> ImportOutcome {
    let wrote = outcome.mapping_created || outcome.snapshot_uploaded || outcome.snapshot_registered || outcome.events_appended > 0;
    outcome.status = if outcome.document_created {
        ImportStatus::Created
    } else if wrote {
        ImportStatus::Resumed
    } else {
        ImportStatus::AlreadyComplete
    };
    outcome
}

async fn import_snapshot(
    pool: &PgPool,
    store: &ObjectStore,
    plan: &MigrationPlan,
    entry: &DocumentPlan,
    legacy: &LegacyDocument,
    outcome: &mut ImportOutcome,
) -> Result<(), ImportError> {
    let document_id = entry.document.id;
    let object_id = entry.snapshot.object.id;
    let snapshot_id = entry.snapshot.id;
    let seq = seq_i64(legacy.snapshot.seq)?;
    let size = i64::try_from(legacy.snapshot.body.len()).map_err(|_| ImportError::InvalidPlanEntry)?;

    let mut tx = begin(pool, plan).await?;
    let object = get_object(&mut tx, object_id).await.map_err(object_error)?;
    if object
        .as_ref()
        .is_some_and(|existing| existing.sha256 != legacy.sha256 || existing.object_key != legacy.object_key || existing.size_bytes != size)
    {
        return Err(ImportError::LegacyChanged);
    }
    let snapshot = snapshot_state(&mut tx, document_id, snapshot_id).await?;
    let (_, current_snapshot) = document_progress(&mut tx, document_id).await?.ok_or(ImportError::Diverged)?;
    let complete = object.as_ref().is_some_and(|object| object.state == ObjectState::Verified)
        && snapshot.as_ref().is_some_and(|(_, _, state)| state == "verified")
        && current_snapshot.as_deref() == Some(snapshot_id.to_string().as_str());
    if complete {
        tx.rollback().await.map_err(db)?;
        return Ok(());
    }

    match object.as_ref().map(|object| object.state) {
        None | Some(ObjectState::Pending) => {
            // Content-addressed, so a repeated upload after a crash writes identical bytes.
            store
                .put_object(&legacy.object_key, legacy.snapshot.body.clone(), &entry.snapshot.object.media_type)
                .await
                .map_err(ImportError::Store)?;
            outcome.snapshot_uploaded = true;
        }
        Some(ObjectState::Verified) => {}
        Some(_) => return Err(ImportError::SnapshotRejected),
    }
    if object.is_none() {
        let new = NewObject {
            id: object_id,
            kind: ObjectKind::Snapshot,
            object_key: legacy.object_key.clone(),
            sha256: legacy.sha256,
            size_bytes: size,
            media_type: entry.snapshot.object.media_type.clone(),
            producer_version: PRODUCER_VERSION.to_owned(),
        };
        register_object(&mut tx, store.bucket_name(), &new).await.map_err(object_error)?;
    }
    match verify_object(&mut tx, store, object_id).await.map_err(object_error)? {
        Some(Verification::Verified(_)) => {}
        Some(Verification::Rejected(..)) => {
            // Keep the rejection on record so the operator sees it.
            tx.commit().await.map_err(db)?;
            return Err(ImportError::SnapshotRejected);
        }
        None => return Err(ImportError::Database),
    }
    match &snapshot {
        Some((row_seq, row_object, _)) if *row_seq != seq || *row_object != object_id.to_string() => return Err(ImportError::Diverged),
        Some(_) => {}
        None => {
            crate::snapshots::register_snapshot(&mut tx, snapshot_id, document_id, seq, object_id, &legacy.sha256, size).await.map_err(db)?;
        }
    }
    crate::snapshots::verify_snapshot(&mut tx, snapshot_id, document_id).await.map_err(|error| match error {
        crate::snapshots::RepositoryError::InvalidState => ImportError::SnapshotRejected,
        _ => ImportError::Database,
    })?;
    add_reference(&mut tx, object_id, OwnerKind::DocumentSnapshot, snapshot_id).await.map_err(db)?;
    crate::snapshots::set_current_snapshot(&mut tx, document_id, snapshot_id).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    outcome.snapshot_registered = true;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Cutover verification (S3-019)
// ---------------------------------------------------------------------------------------------

/// Why one imported document failed verification. Serialized names never change meaning.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyCode {
    /// The legacy files could not be read (missing, a link, too large).
    LegacyUnreadable,
    /// The legacy files no longer match the plan.
    LegacyChanged,
    DocumentMissing,
    /// The document is in another workspace or project, or not active.
    DocumentMismatch,
    MappingMissing,
    /// The mapping points at another internal id.
    MappingMismatch,
    SnapshotRowMissing,
    /// The snapshot row is not verified, or records another sequence or object.
    SnapshotNotVerified,
    /// `current_snapshot_id` is not the imported snapshot.
    CurrentSnapshotMismatch,
    ObjectMissing,
    /// The object row is not verified, or records another key, size or digest.
    ObjectNotVerified,
    /// The stored bytes are missing or do not hash to the object row.
    ObjectBytesMismatch,
    /// The stored bytes differ from the legacy snapshot body.
    SnapshotContentMismatch,
    /// The store failed in a way that says nothing about the bytes.
    StoreUnavailable,
    /// `documents.current_seq` is not the plan's last sequence.
    SequenceMismatch,
    /// The event rows after the snapshot do not number `events.count` or span its range.
    EventCountMismatch,
    /// Replay of the stored events failed verification (checksum, decoding or a gap).
    ReplayCorrupt,
    /// Replay succeeded but yields other batches than the legacy log.
    ReplayMismatch,
}

/// One planned document's verification.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DocumentVerification {
    pub legacy_id: String,
    pub document_id: CloudDocumentId,
    pub passed: bool,
    pub failures: Vec<VerifyCode>,
    /// Event rows found after the snapshot sequence.
    pub events: u64,
}

/// What a not-imported item is.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotImportedKind {
    /// A document the plan excluded.
    ExcludedDocument,
    /// A document's share links (`links.json`).
    Links,
    /// A planned library file (library import is later work).
    Library,
    /// A planned plugin bundle (plugin import is later work).
    Plugin,
    /// A library file or plugin the plan deferred.
    Deferred,
}

/// Legacy data the import did not carry over. Any entry blocks cutover.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NotImported {
    pub kind: NotImportedKind,
    /// Legacy path.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_document: Option<String>,
    pub count: u64,
}

/// The result of [`verify_import`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportVerification {
    pub documents: Vec<DocumentVerification>,
    pub not_imported: Vec<NotImported>,
    /// Every planned document passed and nothing was left behind.
    pub cutover_ready: bool,
}

/// Re-read PostgreSQL, the object store and the legacy directory, and check every planned
/// document of `plan` was imported exactly. Read-only: nothing is written anywhere (object and
/// snapshot verification states are read, never changed); the caller should roll back.
pub async fn verify_import(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    plan: &MigrationPlan,
    root: &Path,
    limits: &ScanLimits,
) -> Result<ImportVerification, ImportError> {
    if plan.target.tenant_id != transaction.tenant_id() {
        return Err(ImportError::InvalidPlanEntry);
    }
    if plan.target.bucket != store.bucket_name() {
        return Err(ImportError::BucketMismatch);
    }
    let mut documents = Vec::with_capacity(plan.documents.len());
    let mut not_imported = Vec::new();
    for entry in &plan.documents {
        documents.push(verify_document(transaction, store, plan, entry, root, limits).await?);
        if entry.links > 0 {
            not_imported.push(NotImported {
                kind: NotImportedKind::Links,
                source: format!("files/{}/links.json", entry.legacy_id),
                legacy_document: Some(entry.legacy_id.clone()),
                count: entry.links,
            });
        }
        for library in &entry.libraries {
            not_imported.push(NotImported {
                kind: NotImportedKind::Library,
                source: library.source.clone(),
                legacy_document: Some(entry.legacy_id.clone()),
                count: 1,
            });
        }
    }
    for excluded in &plan.excluded {
        not_imported.push(NotImported {
            kind: NotImportedKind::ExcludedDocument,
            source: format!("files/{}", excluded.legacy_id),
            legacy_document: Some(excluded.legacy_id.clone()),
            count: 1,
        });
    }
    for plugin in &plan.plugins {
        not_imported.push(NotImported { kind: NotImportedKind::Plugin, source: plugin.object.source.clone(), legacy_document: None, count: 1 });
    }
    for deferred in &plan.deferred {
        not_imported.push(NotImported {
            kind: NotImportedKind::Deferred,
            source: deferred.source.clone(),
            legacy_document: deferred.legacy_document.clone(),
            count: 1,
        });
    }
    let cutover_ready = not_imported.is_empty() && documents.iter().all(|document| document.passed);
    Ok(ImportVerification { documents, not_imported, cutover_ready })
}

async fn verify_document(
    tx: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    plan: &MigrationPlan,
    entry: &DocumentPlan,
    root: &Path,
    limits: &ScanLimits,
) -> Result<DocumentVerification, ImportError> {
    let document_id = entry.document.id;
    let mut failures = std::collections::BTreeSet::new();
    let base_seq = seq_i64(entry.snapshot.seq)?;
    let last_seq = seq_i64(entry.document.current_seq)?;
    let legacy = match read_legacy(plan, entry, root, limits) {
        Ok(legacy) => Some(legacy),
        Err(ImportError::LegacyUnreadable) => {
            failures.insert(VerifyCode::LegacyUnreadable);
            None
        }
        Err(_) => {
            failures.insert(VerifyCode::LegacyChanged);
            None
        }
    };
    let result = |failures: std::collections::BTreeSet<VerifyCode>, events: u64| {
        let failures: Vec<VerifyCode> = failures.into_iter().collect();
        DocumentVerification { legacy_id: entry.legacy_id.clone(), document_id, passed: failures.is_empty(), failures, events }
    };

    // Document and mapping.
    let Some(document) = crate::metadata::get_document(tx, document_id).await.map_err(db)? else {
        failures.insert(VerifyCode::DocumentMissing);
        return Ok(result(failures, 0));
    };
    if document.workspace_id != plan.target.workspace_id || document.project_id != plan.target.project_id || document.state != ResourceState::Active {
        failures.insert(VerifyCode::DocumentMismatch);
    }
    let mapping = &entry.external_mapping;
    match crate::external_mappings::get_external_mapping(tx, LEGACY_SYSTEM_ID, &mapping.resource_kind, &mapping.external_id).await.map_err(db)? {
        Some(found) if found.internal_id == document_id.as_uuid() => {}
        Some(_) => {
            failures.insert(VerifyCode::MappingMismatch);
        }
        None => {
            failures.insert(VerifyCode::MappingMissing);
        }
    }
    let (current_seq, current_snapshot) = document_progress(tx, document_id).await?.unwrap_or((-1, None));
    if current_seq != last_seq {
        failures.insert(VerifyCode::SequenceMismatch);
    }

    // Snapshot row, object row and stored bytes.
    let snapshot_id = entry.snapshot.id;
    let object_id = entry.snapshot.object.id;
    match snapshot_state(tx, document_id, snapshot_id).await? {
        Some((seq, object, state)) if seq == base_seq && object == object_id.to_string() && state == "verified" => {}
        Some(_) => {
            failures.insert(VerifyCode::SnapshotNotVerified);
        }
        None => {
            failures.insert(VerifyCode::SnapshotRowMissing);
        }
    }
    if current_snapshot.as_deref() != Some(snapshot_id.to_string().as_str()) {
        failures.insert(VerifyCode::CurrentSnapshotMismatch);
    }
    match get_object(tx, object_id).await.map_err(object_error)? {
        None => {
            failures.insert(VerifyCode::ObjectMissing);
        }
        Some(object) => {
            let planned = &entry.snapshot.object;
            let shape_ok = object.kind == ObjectKind::Snapshot
                && object.bucket == store.bucket_name()
                && object.object_key.starts_with(&planned.key_prefix)
                && u64::try_from(object.size_bytes).ok() == Some(planned.size_bytes)
                && legacy.as_ref().is_none_or(|legacy| legacy.object_key == object.object_key && legacy.sha256 == object.sha256);
            if object.state != ObjectState::Verified || !shape_ok {
                failures.insert(VerifyCode::ObjectNotVerified);
            }
            match store.get_object(&object.object_key, planned.size_bytes).await {
                Ok(bytes) => {
                    if <[u8; 32]>::from(Sha256::digest(&bytes)) != object.sha256 {
                        failures.insert(VerifyCode::ObjectBytesMismatch);
                    }
                    if legacy.as_ref().is_some_and(|legacy| legacy.snapshot.body != bytes) {
                        failures.insert(VerifyCode::SnapshotContentMismatch);
                    }
                }
                Err(ObjectStoreError::NotFound | ObjectStoreError::TooLarge) => {
                    failures.insert(VerifyCode::ObjectBytesMismatch);
                }
                Err(_) => {
                    failures.insert(VerifyCode::StoreUnavailable);
                }
            }
        }
    }

    // Event rows and their replay.
    let (count, first, last): (i64, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT count(*), min(seq), max(seq) FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq > $3")
            .bind(tx.tenant_id().to_string())
            .bind(document_id.to_string())
            .bind(base_seq)
            .fetch_one(tx.connection())
            .await
            .map_err(db)?;
    let expected = entry.events.count;
    let range_ok = if expected == 0 { first.is_none() } else { first == base_seq.checked_add(1) && last == Some(last_seq) };
    if u64::try_from(count).ok() != Some(expected) || !range_ok {
        failures.insert(VerifyCode::EventCountMismatch);
    }
    if current_seq >= base_seq {
        let mut replayed: Vec<Accepted> = Vec::new();
        let mut after = base_seq;
        let mut corrupt = false;
        // Bounded by the planned count: one page past it is enough to prove a mismatch.
        while after < current_seq && replayed.len() as u64 <= expected {
            match crate::events::read_events(tx, document_id, after, crate::events::MAX_REPLAY_EVENTS, crate::events::MAX_REPLAY_BYTES).await {
                Ok(page) => {
                    let next = page.next_after_seq();
                    replayed.extend(page.events.into_iter().map(|event| event.batch));
                    if next <= after {
                        break;
                    }
                    after = next;
                }
                Err(EventRepositoryError::Corrupt | EventRepositoryError::SequenceConflict { .. }) => {
                    corrupt = true;
                    break;
                }
                Err(_) => return Err(ImportError::Database),
            }
        }
        if corrupt {
            failures.insert(VerifyCode::ReplayCorrupt);
        } else if let Some(legacy) = &legacy {
            let same = replayed.len() == legacy.batches.len() && replayed.iter().zip(&legacy.batches).all(|(stored, source)| *stored == source.batch);
            if !same {
                failures.insert(VerifyCode::ReplayMismatch);
            }
        }
    }
    Ok(result(failures, u64::try_from(count).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_collab::{Change, NodeId};

    fn record(entry: &Journal) -> Vec<u8> {
        let body = message::encode(entry).unwrap_or_default();
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn accepted(seq: u64) -> Journal {
        Journal::Accepted(Accepted { seq, author: 1, batch_id: seq, changes: vec![Change::Delete { id: NodeId(seq) }] })
    }

    fn log(generation: u64, entries: &[Journal]) -> Vec<u8> {
        let mut out = b"FCLG\x01".to_vec();
        out.extend_from_slice(&generation.to_le_bytes());
        for entry in entries {
            out.extend_from_slice(&record(entry));
        }
        out
    }

    fn seqs(batches: &[LegacyBatch]) -> Vec<u64> {
        batches.iter().map(|batch| batch.batch.seq).collect()
    }

    #[test]
    fn log_replay_follows_the_server_rules() {
        let entries = [accepted(3), Journal::Issued { client: 2 }, accepted(4), accepted(5), accepted(7), accepted(8)];
        assert_eq!(seqs(&parse_log(&log(2, &entries), 2, 3, 100)), [4, 5]);
        assert!(parse_log(&log(1, &entries), 2, 3, 100).is_empty(), "a stale generation is ignored");
        assert_eq!(seqs(&parse_log(&log(2, &entries), 2, 3, 3)), [4], "the entry cap stops reading");
    }

    /// Never-crash sweep over truncated, bit-flipped and random log and snapshot bytes.
    #[test]
    fn junk_sweep_never_panics() {
        let good = log(2, &[accepted(1), accepted(2), accepted(3)]);
        for n in 0..=good.len() {
            let batches = parse_log(good.get(..n).unwrap_or_default(), 2, 0, 100);
            assert!(batches.len() <= 3);
            let _ = parse_snapshot(good.get(..n).unwrap_or_default());
        }
        for i in 0..good.len() {
            let mut bytes = good.clone();
            if let Some(byte) = bytes.get_mut(i) {
                *byte ^= 0x41;
            }
            assert!(parse_log(&bytes, 2, 0, 100).len() <= 3);
            let _ = parse_snapshot(&bytes);
        }
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..500 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut bytes: Vec<u8> = (0..(state % 128)).map(|i| (state >> (i % 56)) as u8).collect();
            if bytes.len() > 13 && state.is_multiple_of(2) {
                bytes[..5].copy_from_slice(b"FCLG\x01");
                bytes[5..13].copy_from_slice(&2_u64.to_le_bytes());
            }
            let _ = parse_log(&bytes, 2, 0, 100);
            let _ = parse_snapshot(&bytes);
        }
    }

    #[test]
    fn hostile_legacy_ids_never_leave_the_files_directory() {
        for id in ["..", "a/b", "", "é", &"x".repeat(65)] {
            assert_eq!(document_dir(Path::new("/srv"), id).err(), Some(ImportError::InvalidPlanEntry));
        }
        assert!(document_dir(Path::new("/srv"), "doc-1_A").is_ok());
    }
}
