//! Dry-run migration plan for a scanned filesystem-server data directory (S3-017).
//!
//! [`plan_migration`] turns a [`MigrationReport`] (S2-021) and an operator-chosen [`PlanTarget`]
//! into a [`MigrationPlan`]: for every importable legacy document, the PostgreSQL rows the import
//! would create (document, external mapping, snapshot, event range) and the S3 objects it would
//! upload; the documents it would exclude and why; the libraries and plugins it would defer; and
//! totals with estimated bytes. The format is documented in
//! `docs/saas/filesystem-migration-report.md` ("Dry-run migration plan").
//!
//! The planner is pure over its inputs: it reads no files, clock, randomness or database and
//! writes nothing. The same report and target always give the same plan, byte for byte once
//! serialized, because every identifier is derived from the tenant and the legacy identifiers
//! (see [`derived_id`]) and every list is sorted. A rerun of the import can therefore find the
//! rows and objects an earlier attempt created. Every count is capped; a report deserialized from
//! hostile JSON yields exclusions, deferrals and blockers, never a panic.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, Uuid, WorkspaceId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::legacy_scan::{DocumentReport, DocumentStatus, LibraryFile, MigrationReport, PluginSummary, ProblemCode, REPORT_FORMAT_VERSION, Severity};
use crate::object_metadata::{ObjectKind, keys};
use crate::objects::valid_key;

/// Version of the [`MigrationPlan`] layout. Bumped on any change a consumer must notice, including
/// any change to identifier derivation; adding an optional field or a new reason code is not one.
pub const PLAN_FORMAT_VERSION: u32 = 1;
/// The `external_mappings.system_id` recorded for every imported legacy document.
pub const LEGACY_SYSTEM_ID: &str = "figcraft-server";
/// Most documents planned. Past this the plan is blocked with [`PlanBlocker::DocumentLimitReached`].
pub const MAX_PLAN_DOCUMENTS: usize = 100_000;
/// Most library files planned per document; later ones are deferred.
pub const MAX_PLAN_LIBRARIES_PER_DOCUMENT: usize = 256;
/// Most plugin index entries planned; later ones are deferred.
pub const MAX_PLAN_PLUGINS: usize = 10_000;
/// `snapshot.bin` header (`FCSN`, format byte, generation, CRC-32). The object holds the body.
pub const SNAPSHOT_HEADER_BYTES: u64 = 17;
/// Longest document name the `documents.name` constraint accepts (characters).
const MAX_NAME_CHARS: usize = 255;
/// Longest legacy plugin id carried into a plan (bytes).
const MAX_PLUGIN_ID_BYTES: usize = 256;
/// Domain separator for [`derived_id`]; changing it changes every identifier.
const ID_NAMESPACE: &[u8] = b"figcraft-migration-plan/v1";

const SNAPSHOT_MEDIA_TYPE: &str = crate::validation::MEDIA_SNAPSHOT;
const LIBRARY_MEDIA_TYPE: &str = crate::validation::MEDIA_FIGCRAFT;
const PLUGIN_MEDIA_TYPE: &str = crate::validation::MEDIA_PLUGIN;

/// Where the import would put the data, chosen by the operator.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanTarget {
    pub tenant_id: TenantId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    /// The principal recorded as the actor of imported events.
    pub principal_id: PrincipalId,
    pub bucket: String,
}

/// Why no plan could be made. Nothing about the input is echoed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    /// The report's `format_version` is not one this planner reads.
    UnsupportedReportFormat,
    /// The target bucket is not a valid S3 bucket name.
    InvalidBucket,
    /// A derived identifier was nil (never expected; reported instead of panicking).
    IdDerivation,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "migration plan error: {self:?}")
    }
}

impl std::error::Error for PlanError {}

/// A reason the import must not run this plan as it stands.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanBlocker {
    /// A scan cap was reached: the report does not list everything.
    ReportIncomplete,
    /// A blocking problem not tied to one document (an unreadable directory, for example).
    BlockingServerProblem,
    /// Some documents are excluded; the import needs the operator's explicit exclusion list.
    ExclusionApprovalRequired,
    /// The report lists more than [`MAX_PLAN_DOCUMENTS`] documents.
    DocumentLimitReached,
}

/// Why a legacy document is left out of the import.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// The scanner marked the document `blocked`; `problems` lists its blocking codes.
    Blocked,
    /// The folder name is not a server document id (1–64 ASCII letters, digits, `-`, `_`).
    InvalidLegacyId,
    /// More than one report entry has this id; all of them are excluded.
    DuplicateLegacyId,
    /// No decoded snapshot with a sequence, or a snapshot without a body.
    SnapshotUnavailable,
    /// `last_accepted_seq` does not equal the snapshot sequence plus the replayable batches, or a
    /// sequence or size does not fit PostgreSQL `BIGINT`.
    SequenceInconsistent,
    /// Metadata is missing although the document is not marked blocked.
    MetadataUnavailable,
}

/// Something the import changes on the way in, or that the operator should review.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanWarning {
    /// The legacy name was empty after trimming; the legacy id is used as the name.
    NameReplaced,
    /// The legacy name was longer than 255 characters and is cut at a character boundary.
    NameTruncated,
    /// The log does not continue the snapshot; none of its batches are imported.
    LogIgnored,
    /// The log stops early (damage or a sequence break); later batches are not imported.
    LogTruncated,
    /// Some of the document's library files are deferred (see `deferred`).
    LibrariesDeferred,
}

/// Why a library file or plugin is not uploaded by this plan.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferralReason {
    /// `library.bin` has no version; the versioned key layout needs one.
    LibraryUnversioned,
    /// The file name is neither `library.bin` nor `library-<n>.bin`.
    LibraryNameInvalid,
    /// The file is empty (objects must hold at least one byte).
    Empty,
    /// The plugin bundle is missing or does not match the index.
    PluginBundleInvalid,
    /// The plugin version cannot be a key segment (see `keys::plugin_version_valid`).
    PluginVersionInvalid,
    /// The plugin id is empty or too long.
    PluginIdInvalid,
    /// The plugin digest is not 64 lowercase hexadecimal characters.
    PluginDigestInvalid,
    /// Another index entry has the same id and version.
    PluginDuplicate,
    /// A size does not fit PostgreSQL `BIGINT`.
    SizeOutOfRange,
    /// A plan cap was reached.
    LimitReached,
}

/// What a planned object holds, as the `objects.kind` value.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedObjectKind {
    Snapshot,
    Library,
    Plugin,
}

impl PlannedObjectKind {
    #[must_use]
    pub const fn object_kind(self) -> ObjectKind {
        match self {
            Self::Snapshot => ObjectKind::Snapshot,
            Self::Library => ObjectKind::Library,
            Self::Plugin => ObjectKind::Plugin,
        }
    }
}

/// One object the import would upload and register `pending`, then verify.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedObject {
    pub id: Uuid,
    pub kind: PlannedObjectKind,
    pub bucket: String,
    /// The legacy file, relative to the scanned root.
    pub source: String,
    /// The key up to the digest. The full key is `key_prefix` + SHA-256 hex + `key_suffix`.
    pub key_prefix: String,
    pub key_suffix: String,
    /// The full key, when the scanner knows the digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_key: Option<String>,
    /// Lowercase hex SHA-256, when the scanner knows it; otherwise the import computes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub size_bytes: u64,
    pub media_type: String,
}

/// The `documents` row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedDocumentRow {
    pub id: CloudDocumentId,
    pub name: String,
    pub state: String,
    pub current_seq: u64,
}

/// The `external_mappings` row tying the cloud document to its legacy id.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedMapping {
    pub system_id: String,
    pub resource_kind: String,
    pub external_id: String,
    pub internal_id: Uuid,
}

/// The `document_snapshots` row and its object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedSnapshot {
    pub id: Uuid,
    pub seq: u64,
    pub object: PlannedObject,
}

/// The `document_events` rows: `count` batches with sequences `first_seq..=last_seq`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedEvents {
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    pub actor_principal_id: PrincipalId,
    /// Bytes of the log the events come from: an upper bound on their payload bytes.
    pub source_log_bytes: u64,
}

/// One legacy document the import would create.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DocumentPlan {
    pub legacy_id: String,
    pub status: DocumentStatus,
    /// The scanner's warning codes for this document, sorted and deduplicated.
    pub problems: Vec<ProblemCode>,
    /// What the planner changes or wants reviewed, sorted.
    pub warnings: Vec<PlanWarning>,
    pub document: PlannedDocumentRow,
    pub external_mapping: PlannedMapping,
    pub snapshot: PlannedSnapshot,
    pub events: PlannedEvents,
    pub libraries: Vec<PlannedObject>,
    /// Links recreated from `links.json` (token hashes carried over by the import).
    pub links: u64,
}

/// One legacy document the import would leave out.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExcludedDocument {
    pub legacy_id: String,
    pub reason: ExclusionReason,
    /// The scanner's blocking codes for this document, sorted and deduplicated.
    pub problems: Vec<ProblemCode>,
}

/// A library file or plugin the plan does not upload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeferredItem {
    pub kind: PlannedObjectKind,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_document: Option<String>,
    pub reason: DeferralReason,
    pub bytes: u64,
}

/// A plugin bundle the import would upload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PluginPlan {
    pub legacy_id: String,
    pub version: String,
    pub plugin_id: Uuid,
    pub object: PlannedObject,
}

/// Sums over the plan.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanTotals {
    pub documents: u64,
    pub excluded: u64,
    pub deferred: u64,
    pub degraded: u64,
    pub events: u64,
    pub links: u64,
    pub plugins: u64,
    /// Objects uploaded: snapshots, libraries and plugins.
    pub objects: u64,
    pub snapshot_bytes: u64,
    pub library_bytes: u64,
    pub plugin_bytes: u64,
    /// Sum of every planned object's size.
    pub upload_bytes: u64,
    /// Sum of the source logs of planned events (an upper bound on event payload bytes).
    pub event_bytes_max: u64,
    /// `upload_bytes + event_bytes_max`.
    pub estimated_bytes: u64,
}

/// The whole dry-run plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub format_version: u32,
    pub report_format_version: u32,
    pub report_scanned_at: u64,
    pub target: PlanTarget,
    /// No blocker: the import may run this plan as it stands.
    pub executable: bool,
    pub blockers: Vec<PlanBlocker>,
    pub totals: PlanTotals,
    /// Sorted by legacy id.
    pub documents: Vec<DocumentPlan>,
    /// Sorted by legacy id, then reason.
    pub excluded: Vec<ExcludedDocument>,
    /// Sorted by legacy id, version.
    pub plugins: Vec<PluginPlan>,
    /// Sorted by source.
    pub deferred: Vec<DeferredItem>,
}

/// A deterministic identifier for one migrated resource.
///
/// The `uuid` crate's v5 feature (SHA-1) is not enabled in this workspace, so the identifier is
/// the first 16 bytes of SHA-256 over [`ID_NAMESPACE`], the tenant and each part (each
/// length-prefixed, so no two part lists collide), with the RFC 9562 version set to 8 (custom)
/// and the variant to RFC 4122. The tenant is included because `documents.id` and `objects.id`
/// are global primary keys: importing one legacy server into two tenants must not collide.
#[must_use]
pub fn derived_id(tenant: TenantId, parts: &[&str]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(ID_NAMESPACE);
    let tenant = tenant.to_string();
    for part in std::iter::once(tenant.as_str()).chain(parts.iter().copied()) {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// The filesystem server's document id rule.
fn valid_legacy_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// S3 bucket naming, as `objects::ObjectStore` enforces it.
fn valid_bucket(bucket: &str) -> bool {
    let bytes = bucket.as_bytes();
    let edge = |byte: Option<&u8>| byte.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    (3..=63).contains(&bytes.len())
        && edge(bytes.first())
        && edge(bytes.last())
        && bytes.iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'.' || *byte == b'-')
        && !bucket.contains("..")
}

fn fits_bigint(value: u64) -> bool {
    i64::try_from(value).is_ok()
}

fn parse_hex_digest(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)) {
        return None;
    }
    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let text = std::str::from_utf8(pair).ok()?;
        *slot = u8::from_str_radix(text, 16).ok()?;
    }
    Some(out)
}

/// The `documents.name` the import writes, and what changed.
fn document_name(legacy: &str, legacy_id: &str, warnings: &mut BTreeSet<PlanWarning>) -> String {
    let trimmed = legacy.trim();
    if trimmed.is_empty() {
        warnings.insert(PlanWarning::NameReplaced);
        return legacy_id.to_owned();
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        warnings.insert(PlanWarning::NameTruncated);
        // The cut can leave trailing whitespace, which `btrim` would not count; trim it too.
        return trimmed.chars().take(MAX_NAME_CHARS).collect::<String>().trim_end().to_owned();
    }
    trimmed.to_owned()
}

/// The library file's version: `Some(Some(n))` for `library-<n>.bin`, `Some(None)` for
/// `library.bin`, `None` for anything else.
fn library_version(name: &str) -> Option<Option<u64>> {
    if name == "library.bin" {
        return Some(None);
    }
    let digits = name.strip_prefix("library-")?.strip_suffix(".bin")?;
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().map(Some)
}

struct Planner<'a> {
    target: &'a PlanTarget,
    /// Per document: (warning-or-info codes, blocking codes) from `report.problems`.
    problems: BTreeMap<&'a str, (BTreeSet<ProblemCode>, BTreeSet<ProblemCode>)>,
    documents: Vec<DocumentPlan>,
    excluded: Vec<ExcludedDocument>,
    plugins: Vec<PluginPlan>,
    deferred: Vec<DeferredItem>,
}

/// Build the dry-run plan for importing `report` into `target`. Pure: see the module docs.
pub fn plan_migration(report: &MigrationReport, target: &PlanTarget) -> Result<MigrationPlan, PlanError> {
    if report.format_version != REPORT_FORMAT_VERSION {
        return Err(PlanError::UnsupportedReportFormat);
    }
    if !valid_bucket(&target.bucket) {
        return Err(PlanError::InvalidBucket);
    }
    let mut blockers = BTreeSet::new();
    if !report.complete {
        blockers.insert(PlanBlocker::ReportIncomplete);
    }
    let mut problems: BTreeMap<&str, (BTreeSet<ProblemCode>, BTreeSet<ProblemCode>)> = BTreeMap::new();
    for problem in &report.problems {
        // The code's own severity is authoritative; a hand-edited `severity` field is ignored.
        let blocking = problem.code.severity() == Severity::Blocking;
        match problem.document.as_deref() {
            Some(document) => {
                let entry = problems.entry(document).or_default();
                if blocking {
                    entry.1.insert(problem.code)
                } else {
                    entry.0.insert(problem.code)
                };
            }
            None if blocking => {
                blockers.insert(PlanBlocker::BlockingServerProblem);
            }
            None => {}
        }
    }
    let mut planner = Planner { target, problems, documents: Vec::new(), excluded: Vec::new(), plugins: Vec::new(), deferred: Vec::new() };

    if report.documents.len() > MAX_PLAN_DOCUMENTS {
        blockers.insert(PlanBlocker::DocumentLimitReached);
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for document in report.documents.iter().take(MAX_PLAN_DOCUMENTS) {
        *seen.entry(document.id.as_str()).or_default() += 1;
    }
    let mut duplicates_noted = BTreeSet::new();
    for document in report.documents.iter().take(MAX_PLAN_DOCUMENTS) {
        if seen.get(document.id.as_str()).is_some_and(|count| *count > 1) {
            if duplicates_noted.insert(document.id.as_str()) {
                planner.exclude(document, ExclusionReason::DuplicateLegacyId);
            }
            continue;
        }
        planner.document(document)?;
    }
    if let Some(inventory) = &report.plugins {
        planner.plugins_from(&inventory.entries)?;
    }

    let Planner { mut documents, mut excluded, mut plugins, mut deferred, .. } = planner;
    documents.sort_by(|a, b| a.legacy_id.cmp(&b.legacy_id));
    excluded.sort_by(|a, b| (&a.legacy_id, a.reason).cmp(&(&b.legacy_id, b.reason)));
    plugins.sort_by(|a, b| (&a.legacy_id, &a.version).cmp(&(&b.legacy_id, &b.version)));
    deferred.sort_by(|a, b| (&a.source, a.reason, &a.legacy_document).cmp(&(&b.source, b.reason, &b.legacy_document)));
    if !excluded.is_empty() {
        blockers.insert(PlanBlocker::ExclusionApprovalRequired);
    }
    let totals = totals(&documents, &excluded, &plugins, &deferred);
    let blockers: Vec<PlanBlocker> = blockers.into_iter().collect();
    Ok(MigrationPlan {
        format_version: PLAN_FORMAT_VERSION,
        report_format_version: report.format_version,
        report_scanned_at: report.scanned_at,
        target: target.clone(),
        executable: blockers.is_empty(),
        blockers,
        totals,
        documents,
        excluded,
        plugins,
        deferred,
    })
}

fn totals(documents: &[DocumentPlan], excluded: &[ExcludedDocument], plugins: &[PluginPlan], deferred: &[DeferredItem]) -> PlanTotals {
    let mut t = PlanTotals {
        documents: documents.len() as u64,
        excluded: excluded.len() as u64,
        deferred: deferred.len() as u64,
        plugins: plugins.len() as u64,
        ..PlanTotals::default()
    };
    for document in documents {
        if document.status == DocumentStatus::Degraded {
            t.degraded += 1;
        }
        t.events = t.events.saturating_add(document.events.count);
        t.event_bytes_max = t.event_bytes_max.saturating_add(document.events.source_log_bytes);
        t.links = t.links.saturating_add(document.links);
        t.objects = t.objects.saturating_add(1).saturating_add(document.libraries.len() as u64);
        t.snapshot_bytes = t.snapshot_bytes.saturating_add(document.snapshot.object.size_bytes);
        for library in &document.libraries {
            t.library_bytes = t.library_bytes.saturating_add(library.size_bytes);
        }
    }
    for plugin in plugins {
        t.objects = t.objects.saturating_add(1);
        t.plugin_bytes = t.plugin_bytes.saturating_add(plugin.object.size_bytes);
    }
    t.upload_bytes = t.snapshot_bytes.saturating_add(t.library_bytes).saturating_add(t.plugin_bytes);
    t.estimated_bytes = t.upload_bytes.saturating_add(t.event_bytes_max);
    t
}

impl Planner<'_> {
    fn tenant(&self) -> TenantId {
        self.target.tenant_id
    }

    fn exclude(&mut self, document: &DocumentReport, reason: ExclusionReason) {
        let problems = self.problems.get(document.id.as_str()).map(|(_, blocking)| blocking.iter().copied().collect()).unwrap_or_default();
        // An invalid id may be arbitrarily long in a hand-made report; keep the plan bounded.
        let legacy_id = if valid_legacy_id(&document.id) { document.id.clone() } else { document.id.chars().take(64).collect() };
        self.excluded.push(ExcludedDocument { legacy_id, reason, problems });
    }

    fn document(&mut self, document: &DocumentReport) -> Result<(), PlanError> {
        if !valid_legacy_id(&document.id) {
            self.exclude(document, ExclusionReason::InvalidLegacyId);
            return Ok(());
        }
        if document.status == DocumentStatus::Blocked || self.problems.get(document.id.as_str()).is_some_and(|(_, blocking)| !blocking.is_empty()) {
            self.exclude(document, ExclusionReason::Blocked);
            return Ok(());
        }
        let Some(meta) = &document.meta else {
            self.exclude(document, ExclusionReason::MetadataUnavailable);
            return Ok(());
        };
        let Some((snapshot_bytes, base_seq)) = document
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.decoded && snapshot.checksum_valid)
            .and_then(|snapshot| Some((snapshot.bytes.checked_sub(SNAPSHOT_HEADER_BYTES).filter(|body| *body > 0)?, snapshot.session_seq?)))
        else {
            self.exclude(document, ExclusionReason::SnapshotUnavailable);
            return Ok(());
        };

        let mut warnings = BTreeSet::new();
        let continuing_log = document.log.as_ref().filter(|log| log.matches_snapshot == Some(true));
        if document.log.as_ref().is_some_and(|log| log.matches_snapshot == Some(false)) {
            warnings.insert(PlanWarning::LogIgnored);
        }
        if continuing_log.is_some_and(|log| log.damage.is_some() || log.sequence_break_at.is_some()) {
            warnings.insert(PlanWarning::LogTruncated);
        }
        let count = continuing_log.map_or(0, |log| log.replayable);
        let source_log_bytes = continuing_log.filter(|_| count > 0).map_or(0, |log| log.bytes);
        let last_seq = document.last_accepted_seq.unwrap_or(base_seq);
        if base_seq.checked_add(count) != Some(last_seq) || !fits_bigint(last_seq) || !fits_bigint(snapshot_bytes) {
            self.exclude(document, ExclusionReason::SequenceInconsistent);
            return Ok(());
        }

        let tenant = self.tenant();
        let id = &document.id;
        let document_id = CloudDocumentId::try_from(derived_id(tenant, &["document", id])).map_err(|_| PlanError::IdDerivation)?;
        let seq_text = base_seq.to_string();
        let snapshot_object = PlannedObject {
            id: derived_id(tenant, &["snapshot_object", id, &seq_text]),
            kind: PlannedObjectKind::Snapshot,
            bucket: self.target.bucket.clone(),
            source: format!("files/{id}/snapshot.bin"),
            key_prefix: format!("tenants/{tenant}/documents/{document_id}/snapshots/{base_seq}-"),
            key_suffix: ".bin".to_owned(),
            object_key: None,
            sha256: None,
            size_bytes: snapshot_bytes,
            media_type: SNAPSHOT_MEDIA_TYPE.to_owned(),
        };
        let libraries = self.libraries(id, &document.libraries, &mut warnings);
        let name = document_name(&meta.name, id, &mut warnings);
        let problems = self
            .problems
            .get(id.as_str())
            .map(|(other, _)| other.iter().copied().filter(|code| code.severity() == Severity::Warning).collect())
            .unwrap_or_default();
        self.documents.push(DocumentPlan {
            legacy_id: id.clone(),
            status: document.status,
            problems,
            warnings: warnings.into_iter().collect(),
            document: PlannedDocumentRow { id: document_id, name, state: "active".to_owned(), current_seq: last_seq },
            external_mapping: PlannedMapping {
                system_id: LEGACY_SYSTEM_ID.to_owned(),
                resource_kind: "document".to_owned(),
                external_id: id.clone(),
                internal_id: document_id.as_uuid(),
            },
            snapshot: PlannedSnapshot { id: derived_id(tenant, &["document_snapshot", id, &seq_text]), seq: base_seq, object: snapshot_object },
            events: PlannedEvents {
                count,
                first_seq: (count > 0).then(|| base_seq.saturating_add(1)),
                last_seq: (count > 0).then_some(last_seq),
                actor_principal_id: self.target.principal_id,
                source_log_bytes,
            },
            libraries,
            links: document.links.as_ref().map_or(0, |links| links.links.len() as u64),
        });
        Ok(())
    }

    fn libraries(&mut self, id: &str, files: &[LibraryFile], warnings: &mut BTreeSet<PlanWarning>) -> Vec<PlannedObject> {
        let tenant = self.tenant();
        let library_id = derived_id(tenant, &["library", id]);
        let mut planned = Vec::new();
        let mut seen = BTreeSet::new();
        for (index, file) in files.iter().enumerate() {
            let version = library_version(&file.name);
            let source = match version {
                Some(_) => format!("files/{id}/{}", file.name),
                None => format!("files/{id}/library-file-{index}"),
            };
            let reason = match version {
                _ if index >= MAX_PLAN_LIBRARIES_PER_DOCUMENT => Some(DeferralReason::LimitReached),
                None => Some(DeferralReason::LibraryNameInvalid),
                Some(None) => Some(DeferralReason::LibraryUnversioned),
                Some(Some(version)) if !seen.insert(version) => Some(DeferralReason::LibraryNameInvalid),
                Some(Some(_)) if file.bytes == 0 => Some(DeferralReason::Empty),
                Some(Some(_)) if !fits_bigint(file.bytes) => Some(DeferralReason::SizeOutOfRange),
                Some(Some(_)) => None,
            };
            match (reason, version) {
                (None, Some(Some(version))) => planned.push(PlannedObject {
                    id: derived_id(tenant, &["library_object", id, &version.to_string()]),
                    kind: PlannedObjectKind::Library,
                    bucket: self.target.bucket.clone(),
                    source,
                    key_prefix: format!("tenants/{tenant}/libraries/{library_id}/{version}-"),
                    key_suffix: ".figcraft".to_owned(),
                    object_key: None,
                    sha256: None,
                    size_bytes: file.bytes,
                    media_type: LIBRARY_MEDIA_TYPE.to_owned(),
                }),
                (reason, _) => {
                    warnings.insert(PlanWarning::LibrariesDeferred);
                    self.deferred.push(DeferredItem {
                        kind: PlannedObjectKind::Library,
                        source,
                        legacy_document: Some(id.to_owned()),
                        reason: reason.unwrap_or(DeferralReason::LibraryNameInvalid),
                        bytes: file.bytes,
                    });
                }
            }
        }
        planned.sort_by(|a, b| a.source.cmp(&b.source));
        planned
    }

    fn plugins_from(&mut self, entries: &[PluginSummary]) -> Result<(), PlanError> {
        let tenant = self.tenant();
        let mut seen = BTreeSet::new();
        for (index, entry) in entries.iter().enumerate() {
            let digest = parse_hex_digest(&entry.sha256);
            let source = match digest {
                Some(_) => format!("plugins/{}.bin", entry.sha256),
                None => format!("plugins/index.json#{index}"),
            };
            let version = keys::plugin_version_valid(&entry.version).then_some(entry.version.as_str());
            let id_valid = (1..=MAX_PLUGIN_ID_BYTES).contains(&entry.id.len());
            let reason = if index >= MAX_PLAN_PLUGINS {
                Some(DeferralReason::LimitReached)
            } else if !id_valid {
                Some(DeferralReason::PluginIdInvalid)
            } else if digest.is_none() {
                Some(DeferralReason::PluginDigestInvalid)
            } else if !entry.bundle_present || !entry.bundle_valid {
                Some(DeferralReason::PluginBundleInvalid)
            } else if entry.bytes == 0 {
                Some(DeferralReason::Empty)
            } else if !fits_bigint(entry.bytes) {
                Some(DeferralReason::SizeOutOfRange)
            } else if version.is_none() {
                Some(DeferralReason::PluginVersionInvalid)
            } else if !seen.insert((entry.id.as_str(), version)) {
                Some(DeferralReason::PluginDuplicate)
            } else {
                None
            };
            match (reason, digest, version) {
                (None, Some(digest), Some(version)) => {
                    let plugin_id = derived_id(tenant, &["plugin", &entry.id]);
                    let Some(key) = keys::plugin(tenant, plugin_id, version, &digest) else {
                        return Err(PlanError::IdDerivation);
                    };
                    if !valid_key(&key) {
                        return Err(PlanError::IdDerivation);
                    }
                    let sha = entry.sha256.clone();
                    self.plugins.push(PluginPlan {
                        legacy_id: entry.id.clone(),
                        version: entry.version.clone(),
                        plugin_id,
                        object: PlannedObject {
                            id: derived_id(tenant, &["plugin_object", &entry.id, &entry.version, &sha]),
                            kind: PlannedObjectKind::Plugin,
                            bucket: self.target.bucket.clone(),
                            source,
                            key_prefix: format!("tenants/{tenant}/plugins/{plugin_id}/{version}-"),
                            key_suffix: ".bin".to_owned(),
                            object_key: Some(key),
                            sha256: Some(sha),
                            size_bytes: entry.bytes,
                            media_type: PLUGIN_MEDIA_TYPE.to_owned(),
                        },
                    });
                }
                (reason, _, _) => self.deferred.push(DeferredItem {
                    kind: PlannedObjectKind::Plugin,
                    source,
                    legacy_document: None,
                    reason: reason.unwrap_or(DeferralReason::PluginBundleInvalid),
                    bytes: entry.bytes,
                }),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        "11111111-1111-4111-8111-111111111111".parse().unwrap()
    }

    #[test]
    fn derived_ids_are_stable_versioned_and_separated() {
        let id = derived_id(tenant(), &["document", "doc-a"]);
        assert_eq!(id, derived_id(tenant(), &["document", "doc-a"]));
        assert_eq!(id.get_version_num(), 8);
        assert_eq!(id.as_bytes()[8] & 0xc0, 0x80, "RFC 4122 variant");
        assert_ne!(id, derived_id(tenant(), &["document", "doc-b"]));
        assert_ne!(id, derived_id("22222222-2222-4222-8222-222222222222".parse().unwrap(), &["document", "doc-a"]));
        // Length prefixes keep ("ab", "c") and ("a", "bc") apart.
        assert_ne!(derived_id(tenant(), &["ab", "c"]), derived_id(tenant(), &["a", "bc"]));
        assert!(!id.is_nil());
    }

    #[test]
    fn names_are_trimmed_replaced_and_truncated_at_char_boundaries() {
        let mut w = BTreeSet::new();
        assert_eq!(document_name("  Board  ", "d", &mut w), "Board");
        assert!(w.is_empty());
        assert_eq!(document_name(" \t ", "doc-x", &mut w), "doc-x");
        assert!(w.contains(&PlanWarning::NameReplaced));
        let long = "é".repeat(300);
        let name = document_name(&long, "d", &mut w);
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert!(w.contains(&PlanWarning::NameTruncated));
    }

    #[test]
    fn library_names_and_digests_parse_strictly() {
        assert_eq!(library_version("library.bin"), Some(None));
        assert_eq!(library_version("library-7.bin"), Some(Some(7)));
        for bad in ["library-.bin", "library-x.bin", "library-1.bin.tmp", "library-99999999999999999999999.bin", "lib.bin", ""] {
            assert_eq!(library_version(bad), None, "{bad}");
        }
        assert_eq!(parse_hex_digest(&"ab".repeat(32)), Some([0xab; 32]));
        for bad in ["AB".repeat(32), "ab".repeat(31), "zz".repeat(32), "é".repeat(32), String::new()] {
            assert_eq!(parse_hex_digest(&bad), None);
        }
    }

    #[test]
    fn junk_strings_never_panic_the_validators() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..20_000 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let len = (state >> 58) as usize;
            let text: String = (0..len).map(|i| char::from_u32(((state >> (i % 48)) as u32) % 0x3000).unwrap_or('x')).collect();
            let _ = valid_legacy_id(&text);
            let _ = valid_bucket(&text);
            let _ = library_version(&text);
            let _ = parse_hex_digest(&text);
            let mut w = BTreeSet::new();
            let name = document_name(&text, "fallback", &mut w);
            assert!((1..=MAX_NAME_CHARS).contains(&name.chars().count()));
        }
    }
}
