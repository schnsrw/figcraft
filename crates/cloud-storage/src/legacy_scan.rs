//! Read-only inventory of a filesystem collaboration server's data directory (S2-021).
//!
//! `figcraft-server` keeps every published file as a folder of plain files (see its `store` and
//! `disk` modules). Before any of it is imported into PostgreSQL and object storage, an operator
//! runs [`scan_data_directory`] against a stopped server's directory and reviews the resulting
//! [`MigrationReport`]: what each file holds, what is damaged, and what blocks migration. The
//! report format is documented in `docs/saas/filesystem-migration-report.md`.
//!
//! The scanner reimplements only the parsing an inventory needs, because a library crate must not
//! depend on the server application:
//!
//! ```text
//! snapshot.bin  "FCSN", format 1, generation (u64 LE), CRC-32 of the body (u32 LE),
//!               body = collaboration wire encoding of a SessionState
//! log.bin       "FCLG", format 1, generation (u64 LE), then records of
//!               length (u32 LE), CRC-32 of the body (u32 LE), body = one wire-encoded Journal
//! ```
//!
//! Guarantees: nothing in the scanned tree is written, renamed, locked or deleted (files are only
//! opened for reading, and leftover `.tmp` files the server would remove are reported instead);
//! symbolic links are reported and never followed; only the fixed server layout is descended, so
//! depth is at most two below the root; every file read, directory listing, log entry count and
//! problem list is capped by [`ScanLimits`]; damaged or hostile input becomes a [`Problem`], never
//! a panic. Problem details carry fixed codes and byte offsets only, never decoder messages,
//! secrets (`server.key`, link and owner token hashes) or recorded client addresses.
//!
//! Team library archives and the semantics of replaying journal entries into a session are not
//! verified here (see [`LibraryFile::parsed`]); the import replays them and reports its own errors.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use figcraft_collab::message::{self, Access, PluginEntry};
use figcraft_collab::{Journal, SessionState};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version of the [`MigrationReport`] layout. Bumped on any change a consumer must notice;
/// adding an optional field or a new [`ProblemCode`] is not such a change.
pub const REPORT_FORMAT_VERSION: u32 = 1;

const SNAP_MAGIC: &[u8; 4] = b"FCSN";
const LOG_MAGIC: &[u8; 4] = b"FCLG";
const DISK_FORMAT: u8 = 1;
const SNAP_HEADER: usize = 4 + 1 + 8 + 4;
const LOG_HEADER: usize = 4 + 1 + 8;
const RECORD_HEADER: usize = 8;
/// Longest name or label copied into a report (characters).
const MAX_TEXT: usize = 256;

/// Caps on what one scan reads. Hitting a count cap marks the report incomplete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanLimits {
    /// Most document folders inventoried.
    pub max_documents: usize,
    /// Most entries read from any one directory listing.
    pub max_directory_entries: usize,
    /// Largest `snapshot.bin` read and decoded.
    pub max_snapshot_bytes: u64,
    /// Largest `log.bin` read (streamed; at most one record is held at a time).
    pub max_log_bytes: u64,
    /// Most records read from one log.
    pub max_log_entries: usize,
    /// Largest JSON file read (`meta.json`, `links.json`, `usage.json`, root lists, plugin index).
    pub max_json_bytes: u64,
    /// Largest plugin bundle hashed.
    pub max_plugin_bytes: u64,
    /// Most problems kept; later ones are counted in [`ReportTotals::problems_dropped`].
    pub max_problems: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_documents: 100_000,
            max_directory_entries: 200_000,
            max_snapshot_bytes: 512 << 20,
            max_log_bytes: 512 << 20,
            max_log_entries: 10_000_000,
            max_json_bytes: 64 << 20,
            max_plugin_bytes: 256 << 20,
            max_problems: 100_000,
        }
    }
}

/// How much a problem matters to migration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Recorded for the operator; nothing is lost.
    Info,
    /// Data the filesystem server itself would drop or skip; migration may proceed knowingly.
    Warning,
    /// Migration must not proceed until it is fixed or the document is explicitly excluded.
    Blocking,
}

/// Stable, machine-readable problem identifiers. Serialized names never change meaning.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemCode {
    RootUnreadable,
    RootNotDirectory,
    FilesDirectoryMissing,
    DirectoryUnreadable,
    DirectoryEntryLimitReached,
    DocumentLimitReached,
    ProblemLimitReached,
    InvalidDocumentId,
    SymlinkSkipped,
    UnknownEntry,
    LeftoverTempFile,
    FileTooLarge,
    ReadFailed,
    MetaMissing,
    MetaUnreadable,
    LinksMissing,
    LinksUnreadable,
    SnapshotMissing,
    SnapshotBadHeader,
    SnapshotUnsupportedFormat,
    SnapshotTruncated,
    SnapshotChecksumMismatch,
    SnapshotUndecodable,
    LogMissing,
    LogBadHeader,
    LogUnsupportedFormat,
    LogTruncatedHeader,
    LogStaleGeneration,
    LogTruncatedRecord,
    LogChecksumMismatch,
    LogUndecodableRecord,
    LogSequenceBreak,
    LogEntryLimitReached,
    LibraryMissing,
    UsageUnreadable,
    ServerListUnreadable,
    PluginIndexUnreadable,
    PluginEntryInvalid,
    PluginBundleMissing,
    PluginBundleMismatch,
}

impl ProblemCode {
    /// The fixed severity of this code.
    #[must_use]
    pub const fn severity(self) -> Severity {
        use ProblemCode as P;
        match self {
            P::FilesDirectoryMissing | P::LeftoverTempFile | P::LogStaleGeneration => Severity::Info,
            P::InvalidDocumentId
            | P::SymlinkSkipped
            | P::UnknownEntry
            | P::LogMissing
            | P::LogBadHeader
            | P::LogTruncatedHeader
            | P::LogTruncatedRecord
            | P::LogChecksumMismatch
            | P::LogUndecodableRecord
            | P::LogSequenceBreak
            | P::LibraryMissing
            | P::UsageUnreadable
            | P::ServerListUnreadable
            | P::PluginIndexUnreadable
            | P::PluginEntryInvalid
            | P::PluginBundleMissing
            | P::PluginBundleMismatch => Severity::Warning,
            P::RootUnreadable
            | P::RootNotDirectory
            | P::DirectoryUnreadable
            | P::DirectoryEntryLimitReached
            | P::DocumentLimitReached
            | P::ProblemLimitReached
            | P::FileTooLarge
            | P::ReadFailed
            | P::MetaMissing
            | P::MetaUnreadable
            | P::LinksMissing
            | P::LinksUnreadable
            | P::SnapshotMissing
            | P::SnapshotBadHeader
            | P::SnapshotUnsupportedFormat
            | P::SnapshotTruncated
            | P::SnapshotChecksumMismatch
            | P::SnapshotUndecodable
            | P::LogUnsupportedFormat
            | P::LogEntryLimitReached => Severity::Blocking,
        }
    }
}

/// One finding, located by a `/`-separated path relative to the scanned root (`""`: the root).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub code: ProblemCode,
    pub severity: Severity,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// Byte offset inside the file, for record-level damage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// The whole inventory of one data directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MigrationReport {
    pub format_version: u32,
    /// When the scan ran, seconds since 1970 UTC, as supplied by the caller.
    pub scanned_at: u64,
    pub root: String,
    /// No count cap was reached: every document and record was inventoried.
    pub complete: bool,
    /// Complete and without blocking problems.
    pub migratable: bool,
    pub totals: ReportTotals,
    pub server: ServerFiles,
    pub documents: Vec<DocumentReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginInventory>,
    pub problems: Vec<Problem>,
}

/// Sums over the report.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReportTotals {
    pub documents: u64,
    pub ready: u64,
    pub degraded: u64,
    pub blocked: u64,
    pub snapshot_bytes: u64,
    pub log_bytes: u64,
    pub library_bytes: u64,
    /// Accepted batches the import will replay on top of the snapshots.
    pub replayable_batches: u64,
    pub plugins: u64,
    pub plugin_bytes: u64,
    pub blocking_problems: u64,
    pub warnings: u64,
    pub info: u64,
    pub problems_dropped: u64,
}

/// Server-wide files at the root. `server.key` is noted by presence only and never read.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServerFiles {
    pub lock_present: bool,
    pub runtime_file_present: bool,
    pub key_present: bool,
    /// Entries in `blocked.json` (addresses are not copied).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_addresses: Option<u64>,
    /// Entries in `reports.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abuse_reports: Option<u64>,
}

/// What migration can do with one document.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    Ready,
    Degraded,
    Blocked,
}

/// One `files/<id>` folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocumentReport {
    pub id: String,
    pub path: String,
    pub status: DocumentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MetaSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<LinksSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogSummary>,
    /// The sequence the document reaches after the snapshot and the replayable log entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accepted_seq: Option<u64>,
    pub libraries: Vec<LibraryFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_reports: Option<u64>,
    /// Every regular file's size in the folder, including unknown ones.
    pub bytes: u64,
}

/// `meta.json`, without the owner token hash or recorded address.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetaSummary {
    pub name: String,
    pub created: u64,
    pub updated: u64,
    pub opened: u64,
    pub seq: u64,
    /// Published without the server key (an owner token hash is recorded).
    pub published_without_key: bool,
    pub origin_address_recorded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<LibraryMetaSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibraryMetaSummary {
    pub id: String,
    pub name: String,
    pub version: u64,
}

/// `links.json`, without token or join-code hashes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LinksSummary {
    pub edit: u64,
    pub view: u64,
    pub with_join_code: u64,
    pub links: Vec<LinkSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LinkSummary {
    pub id: String,
    pub access: Access,
    pub label: String,
    pub created: u64,
    pub has_join_code: bool,
}

/// `snapshot.bin`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub checksum_valid: bool,
    pub decoded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_batches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_clients: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<ProblemCode>,
}

/// `log.bin`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogSummary {
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// The log continues the snapshot (otherwise the server ignores it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches_snapshot: Option<bool>,
    pub records: u64,
    pub issued: u64,
    pub accepted: u64,
    pub refused: u64,
    /// Accepted batches at or below the sequence already reached (the server skips them).
    pub already_applied: u64,
    /// Accepted batches that extend the sequence one by one, up to the first break.
    pub replayable: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_accepted_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accepted_seq: Option<u64>,
    /// The sequence after the replayable batches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_replayable_seq: Option<u64>,
    /// Offset of the first accepted batch that skips a sequence number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_break_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub damage: Option<LogDamage>,
}

/// Where and why a log stops being readable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogDamage {
    pub code: ProblemCode,
    pub offset: u64,
    /// Bytes from `offset` to the end of the file, which the server drops.
    pub trailing_bytes: u64,
}

/// A team library archive. Its contents are not parsed (`parsed` is always `false` in format 1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibraryFile {
    pub name: String,
    /// `None` for the unversioned `library.bin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    pub bytes: u64,
    pub parsed: bool,
}

/// `plugins/`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct PluginInventory {
    pub index_present: bool,
    pub entries: Vec<PluginSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PluginSummary {
    pub id: String,
    pub version: String,
    pub bytes: u64,
    pub sha256: String,
    pub bundle_present: bool,
    /// The bundle's size and SHA-256 match the index (the bundle itself is not decoded).
    pub bundle_valid: bool,
}

/// Inventory the data directory at `root` without changing it.
#[must_use]
pub fn scan_data_directory(root: &Path, scanned_at: u64, limits: &ScanLimits) -> MigrationReport {
    let mut scan = Scanner { root, limits, problems: Vec::new(), dropped: 0, complete: true, worst: None };
    let mut report = MigrationReport {
        format_version: REPORT_FORMAT_VERSION,
        scanned_at,
        root: root.display().to_string(),
        complete: true,
        migratable: false,
        totals: ReportTotals::default(),
        server: ServerFiles::default(),
        documents: Vec::new(),
        plugins: None,
        problems: Vec::new(),
    };
    match fs::metadata(root) {
        Ok(m) if m.is_dir() => scan.scan_root(&mut report),
        Ok(_) => scan.note(ProblemCode::RootNotDirectory, "", None, None),
        Err(_) => scan.note(ProblemCode::RootUnreadable, "", None, None),
    }
    scan.finish(report)
}

/// What a directory listing found, without following links.
enum Kind {
    File(u64),
    Dir,
    Link,
    Other,
}

struct Listed {
    name: String,
    kind: Kind,
}

struct Scanner<'a> {
    root: &'a Path,
    limits: &'a ScanLimits,
    problems: Vec<Problem>,
    dropped: u64,
    complete: bool,
    /// Worst severity noted for the document being scanned.
    worst: Option<Severity>,
}

impl Scanner<'_> {
    fn note(&mut self, code: ProblemCode, path: &str, document: Option<&str>, offset: Option<u64>) {
        let severity = code.severity();
        if document.is_some() {
            self.worst = self.worst.max(Some(severity));
        }
        if matches!(code, ProblemCode::DirectoryEntryLimitReached | ProblemCode::DocumentLimitReached | ProblemCode::LogEntryLimitReached) {
            self.complete = false;
        }
        if self.problems.len() >= self.limits.max_problems {
            self.dropped = self.dropped.saturating_add(1);
            self.complete = false;
            return;
        }
        self.problems.push(Problem { code, severity, path: path.to_string(), document: document.map(str::to_string), offset });
    }

    fn finish(mut self, mut report: MigrationReport) -> MigrationReport {
        if self.dropped > 0 {
            // Always recorded, even past the cap, so a truncated list is never mistaken for a full one.
            self.problems.push(Problem {
                code: ProblemCode::ProblemLimitReached,
                severity: Severity::Blocking,
                path: String::new(),
                document: None,
                offset: None,
            });
        }
        let t = &mut report.totals;
        t.problems_dropped = self.dropped;
        for p in &self.problems {
            match p.severity {
                Severity::Info => t.info += 1,
                Severity::Warning => t.warnings += 1,
                Severity::Blocking => t.blocking_problems += 1,
            }
        }
        for d in &report.documents {
            t.documents += 1;
            match d.status {
                DocumentStatus::Ready => t.ready += 1,
                DocumentStatus::Degraded => t.degraded += 1,
                DocumentStatus::Blocked => t.blocked += 1,
            }
            t.snapshot_bytes = t.snapshot_bytes.saturating_add(d.snapshot.as_ref().map_or(0, |s| s.bytes));
            t.log_bytes = t.log_bytes.saturating_add(d.log.as_ref().map_or(0, |l| l.bytes));
            t.library_bytes = t.library_bytes.saturating_add(d.libraries.iter().fold(0u64, |a, l| a.saturating_add(l.bytes)));
            t.replayable_batches =
                t.replayable_batches.saturating_add(d.log.as_ref().filter(|l| l.matches_snapshot == Some(true)).map_or(0, |l| l.replayable));
        }
        if let Some(p) = &report.plugins {
            t.plugins = p.entries.len() as u64;
            t.plugin_bytes = p.entries.iter().filter(|e| e.bundle_present).fold(0u64, |a, e| a.saturating_add(e.bytes));
        }
        report.complete = self.complete;
        report.migratable = self.complete && t.blocking_problems == 0;
        report.problems = self.problems;
        report
    }

    /// The entries of `rel` (relative to the root), sorted by name; `None` when unreadable.
    fn list(&mut self, rel: &str, document: Option<&str>) -> Option<Vec<Listed>> {
        let path = self.root.join(rel);
        let Ok(entries) = fs::read_dir(&path) else {
            self.note(ProblemCode::DirectoryUnreadable, rel, document, None);
            return None;
        };
        let mut out = Vec::new();
        for entry in entries {
            if out.len() >= self.limits.max_directory_entries {
                self.note(ProblemCode::DirectoryEntryLimitReached, rel, document, None);
                break;
            }
            let Ok(entry) = entry else {
                self.note(ProblemCode::DirectoryUnreadable, rel, document, None);
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            // `DirEntry::file_type` and `metadata` describe the entry itself, not a link's target.
            let kind = match entry.file_type() {
                Ok(t) if t.is_symlink() => Kind::Link,
                Ok(t) if t.is_dir() => Kind::Dir,
                Ok(t) if t.is_file() => Kind::File(entry.metadata().map_or(0, |m| m.len())),
                _ => Kind::Other,
            };
            out.push(Listed { name, kind });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Some(out)
    }

    /// Read a regular file of at most `max` bytes, noting why not otherwise.
    fn read(&mut self, rel: &str, max: u64, document: Option<&str>) -> Option<Vec<u8>> {
        let file = self.open(rel, max, document)?;
        let mut bytes = Vec::new();
        match file.take(max).read_to_end(&mut bytes) {
            Ok(_) => Some(bytes),
            Err(_) => {
                self.note(ProblemCode::ReadFailed, rel, document, None);
                None
            }
        }
    }

    /// Open a regular file read-only, refusing links and files over `max` bytes.
    fn open(&mut self, rel: &str, max: u64, document: Option<&str>) -> Option<File> {
        let path = self.root.join(rel);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                self.note(ProblemCode::SymlinkSkipped, rel, document, None);
                return None;
            }
            Ok(m) if m.is_file() => {}
            _ => {
                self.note(ProblemCode::ReadFailed, rel, document, None);
                return None;
            }
        }
        let Ok(file) = File::open(&path) else {
            self.note(ProblemCode::ReadFailed, rel, document, None);
            return None;
        };
        // Check the opened file itself, in case the entry was swapped since the check above.
        match file.metadata() {
            Ok(m) if m.is_file() && m.len() <= max => Some(file),
            Ok(m) if m.is_file() => {
                self.note(ProblemCode::FileTooLarge, rel, document, None);
                None
            }
            _ => {
                self.note(ProblemCode::ReadFailed, rel, document, None);
                None
            }
        }
    }

    fn scan_root(&mut self, report: &mut MigrationReport) {
        let Some(entries) = self.list("", None) else { return };
        let mut files_dir = false;
        let mut plugins_dir = false;
        for e in &entries {
            match (e.name.as_str(), &e.kind) {
                (_, Kind::Link) => self.note(ProblemCode::SymlinkSkipped, &e.name, None, None),
                ("files", Kind::Dir) => files_dir = true,
                ("plugins", Kind::Dir) => plugins_dir = true,
                ("LOCK", Kind::File(_)) => report.server.lock_present = true,
                ("server.json", Kind::File(_)) => report.server.runtime_file_present = true,
                ("server.key", Kind::File(_)) => report.server.key_present = true,
                ("blocked.json", Kind::File(_)) => report.server.blocked_addresses = self.count_json_list(&e.name),
                ("reports.json", Kind::File(_)) => report.server.abuse_reports = self.count_json_list(&e.name),
                (n, Kind::File(_)) if is_temp_of(n, &["server.json", "server.key", "blocked.json", "reports.json"]) => {
                    self.note(ProblemCode::LeftoverTempFile, n, None, None);
                }
                (n, _) => self.note(ProblemCode::UnknownEntry, n, None, None),
            }
        }
        if files_dir {
            self.scan_files(report);
        } else {
            self.note(ProblemCode::FilesDirectoryMissing, "files", None, None);
        }
        if plugins_dir {
            report.plugins = Some(self.scan_plugins());
        }
    }

    fn count_json_list(&mut self, rel: &str) -> Option<u64> {
        let bytes = self.read(rel, self.limits.max_json_bytes, None)?;
        match serde_json::from_slice::<Vec<IgnoredAny>>(&bytes) {
            Ok(list) => Some(list.len() as u64),
            Err(_) => {
                self.note(ProblemCode::ServerListUnreadable, rel, None, None);
                None
            }
        }
    }

    fn scan_files(&mut self, report: &mut MigrationReport) {
        let Some(entries) = self.list("files", None) else { return };
        for e in entries {
            let rel = format!("files/{}", e.name);
            match e.kind {
                Kind::Link => self.note(ProblemCode::SymlinkSkipped, &rel, None, None),
                Kind::Dir if valid_document_id(&e.name) => {
                    if report.documents.len() >= self.limits.max_documents {
                        self.note(ProblemCode::DocumentLimitReached, "files", None, None);
                        break;
                    }
                    let doc = self.scan_document(&e.name, &rel);
                    report.documents.push(doc);
                }
                Kind::Dir => self.note(ProblemCode::InvalidDocumentId, &rel, None, None),
                _ => self.note(ProblemCode::UnknownEntry, &rel, None, None),
            }
        }
    }

    fn scan_document(&mut self, id: &str, rel: &str) -> DocumentReport {
        self.worst = None;
        let mut doc = DocumentReport {
            id: id.to_string(),
            path: rel.to_string(),
            status: DocumentStatus::Ready,
            meta: None,
            links: None,
            snapshot: None,
            log: None,
            last_accepted_seq: None,
            libraries: Vec::new(),
            usage_reports: None,
            bytes: 0,
        };
        let entries = self.list(rel, Some(id)).unwrap_or_default();
        let mut seen = [false; 4]; // meta, links, snapshot, log
        let mut log_len = 0;
        for e in &entries {
            let path = format!("{rel}/{}", e.name);
            let len = match e.kind {
                Kind::File(len) => len,
                Kind::Link => {
                    self.note(ProblemCode::SymlinkSkipped, &path, Some(id), None);
                    continue;
                }
                _ => {
                    self.note(ProblemCode::UnknownEntry, &path, Some(id), None);
                    continue;
                }
            };
            doc.bytes = doc.bytes.saturating_add(len);
            match e.name.as_str() {
                "meta.json" => {
                    seen[0] = true;
                    doc.meta = self.read_meta(&path, id);
                }
                "links.json" => {
                    seen[1] = true;
                    doc.links = self.read_links(&path, id);
                }
                "snapshot.bin" => {
                    seen[2] = true;
                    doc.snapshot = Some(self.read_snapshot(&path, id, len));
                }
                "log.bin" => {
                    seen[3] = true;
                    log_len = len;
                }
                "usage.json" => {
                    let bytes = self.read(&path, self.limits.max_json_bytes, Some(id));
                    match bytes.map(|b| serde_json::from_slice::<Vec<IgnoredAny>>(&b)) {
                        Some(Ok(list)) => doc.usage_reports = Some(list.len() as u64),
                        Some(Err(_)) => self.note(ProblemCode::UsageUnreadable, &path, Some(id), None),
                        None => {}
                    }
                }
                "library.bin" => doc.libraries.push(LibraryFile { name: e.name.clone(), version: None, bytes: len, parsed: false }),
                n if library_version(n).is_some() => {
                    doc.libraries.push(LibraryFile { name: e.name.clone(), version: library_version(n), bytes: len, parsed: false });
                }
                n if is_temp_of(n, &["meta.json", "links.json", "snapshot.bin", "log.bin", "usage.json"]) || is_library_temp(n) => {
                    self.note(ProblemCode::LeftoverTempFile, &path, Some(id), None);
                }
                _ => self.note(ProblemCode::UnknownEntry, &path, Some(id), None),
            }
        }
        for (present, code, name) in [
            (seen[0], ProblemCode::MetaMissing, "meta.json"),
            (seen[1], ProblemCode::LinksMissing, "links.json"),
            (seen[2], ProblemCode::SnapshotMissing, "snapshot.bin"),
            (seen[3], ProblemCode::LogMissing, "log.bin"),
        ] {
            if !present {
                self.note(code, &format!("{rel}/{name}"), Some(id), None);
            }
        }
        let snapshot_seq = doc.snapshot.as_ref().and_then(|s| s.session_seq);
        doc.last_accepted_seq = snapshot_seq;
        if seen[3] {
            let path = format!("{rel}/log.bin");
            let snapshot_generation = doc.snapshot.as_ref().and_then(|s| s.generation);
            if let Some(file) = self.open(&path, self.limits.max_log_bytes, Some(id)) {
                let mut log = inventory_log(io::BufReader::new(file.take(log_len)), log_len, snapshot_seq, self.limits.max_log_entries);
                log.matches_snapshot = match (log.generation, snapshot_generation) {
                    (Some(l), Some(s)) => Some(l == s),
                    _ => None,
                };
                self.note_log(&log, &path, id);
                if log.matches_snapshot == Some(true) && snapshot_seq.is_some() {
                    doc.last_accepted_seq = log.last_replayable_seq.or(snapshot_seq);
                }
                doc.log = Some(log);
            }
        }
        if let Some(lib) = doc.meta.as_ref().and_then(|m| m.library.as_ref()) {
            let version = lib.version;
            if !doc.libraries.iter().any(|l| l.version == Some(version) || l.version.is_none()) {
                self.note(ProblemCode::LibraryMissing, &format!("{rel}/library-{version}.bin"), Some(id), None);
            }
        }
        doc.status = match self.worst {
            None | Some(Severity::Info) => DocumentStatus::Ready,
            Some(Severity::Warning) => DocumentStatus::Degraded,
            Some(Severity::Blocking) => DocumentStatus::Blocked,
        };
        doc
    }

    fn note_log(&mut self, log: &LogSummary, path: &str, id: &str) {
        if log.matches_snapshot == Some(false) {
            // The server ignores a log from before an interrupted compaction: its contents are in
            // the snapshot already, so its damage does not matter either.
            self.note(ProblemCode::LogStaleGeneration, path, Some(id), None);
            return;
        }
        if let Some(d) = log.damage {
            self.note(d.code, path, Some(id), Some(d.offset));
        }
        if let Some(at) = log.sequence_break_at {
            self.note(ProblemCode::LogSequenceBreak, path, Some(id), Some(at));
        }
    }

    fn read_meta(&mut self, path: &str, id: &str) -> Option<MetaSummary> {
        let bytes = self.read(path, self.limits.max_json_bytes.min(1 << 20), Some(id))?;
        let Ok(m) = serde_json::from_slice::<MetaRecord>(&bytes) else {
            self.note(ProblemCode::MetaUnreadable, path, Some(id), None);
            return None;
        };
        Some(MetaSummary {
            name: clip(&m.name),
            created: m.created,
            updated: m.updated,
            opened: m.opened,
            seq: m.seq,
            published_without_key: m.owner.is_some(),
            origin_address_recorded: m.ip.is_some(),
            library: m.library.map(|l| LibraryMetaSummary { id: clip(&l.id), name: clip(&l.name), version: l.version }),
        })
    }

    fn read_links(&mut self, path: &str, id: &str) -> Option<LinksSummary> {
        let bytes = self.read(path, self.limits.max_json_bytes, Some(id))?;
        let Ok(list) = serde_json::from_slice::<Vec<LinkRecord>>(&bytes) else {
            self.note(ProblemCode::LinksUnreadable, path, Some(id), None);
            return None;
        };
        let mut s = LinksSummary::default();
        for l in list {
            match l.access {
                Access::Edit => s.edit += 1,
                Access::View => s.view += 1,
            }
            let has_join_code = l.code.is_some();
            s.with_join_code += u64::from(has_join_code);
            s.links.push(LinkSummary { id: clip(&l.id), access: l.access, label: clip(&l.label), created: l.created, has_join_code });
        }
        Some(s)
    }

    fn read_snapshot(&mut self, path: &str, id: &str, len: u64) -> SnapshotSummary {
        let Some(bytes) = self.read(path, self.limits.max_snapshot_bytes, Some(id)) else {
            return SnapshotSummary { bytes: len, ..SnapshotSummary::default() };
        };
        let s = inventory_snapshot(&bytes);
        if let Some(code) = s.fault {
            self.note(code, path, Some(id), None);
        }
        s
    }

    fn scan_plugins(&mut self) -> PluginInventory {
        let mut inv = PluginInventory::default();
        let Some(entries) = self.list("plugins", None) else { return inv };
        let mut bundles = std::collections::BTreeMap::new();
        for e in &entries {
            let rel = format!("plugins/{}", e.name);
            match (&e.kind, e.name.as_str()) {
                (Kind::Link, _) => self.note(ProblemCode::SymlinkSkipped, &rel, None, None),
                (Kind::File(_), "index.json") => inv.index_present = true,
                (Kind::File(len), n) if n.strip_suffix(".bin").is_some_and(valid_sha) => {
                    bundles.insert(n.to_string(), *len);
                }
                (Kind::File(_), n) if n == "index.json.tmp" || n.strip_suffix(".bin.tmp").is_some_and(valid_sha) => {
                    self.note(ProblemCode::LeftoverTempFile, &rel, None, None);
                }
                _ => self.note(ProblemCode::UnknownEntry, &rel, None, None),
            }
        }
        if !inv.index_present {
            return inv;
        }
        let Some(bytes) = self.read("plugins/index.json", self.limits.max_json_bytes, None) else { return inv };
        let Ok(all) = serde_json::from_slice::<Vec<PluginEntry>>(&bytes) else {
            self.note(ProblemCode::PluginIndexUnreadable, "plugins/index.json", None, None);
            return inv;
        };
        for p in all {
            if !valid_sha(&p.sha256) {
                self.note(ProblemCode::PluginEntryInvalid, "plugins/index.json", None, None);
                continue;
            }
            let name = format!("{}.bin", p.sha256);
            let rel = format!("plugins/{name}");
            let bundle_present = bundles.contains_key(&name);
            let bundle_valid = bundles.get(&name) == Some(&p.bytes) && self.hash_matches(&rel, &p.sha256);
            if !bundle_present {
                self.note(ProblemCode::PluginBundleMissing, &rel, None, None);
            } else if !bundle_valid {
                self.note(ProblemCode::PluginBundleMismatch, &rel, None, None);
            }
            inv.entries.push(PluginSummary {
                id: clip(&p.id),
                version: clip(&p.version),
                bytes: p.bytes,
                sha256: p.sha256,
                bundle_present,
                bundle_valid,
            });
        }
        inv
    }

    fn hash_matches(&mut self, rel: &str, sha: &str) -> bool {
        let Some(mut file) = self.open(rel, self.limits.max_plugin_bytes, None) else { return false };
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(buf.get(..n).unwrap_or_default()),
                Err(_) => return false,
            }
        }
        hex(&hasher.finalize()) == sha
    }
}

/// `meta.json` as the server writes it; unknown fields are ignored.
#[derive(Deserialize)]
struct MetaRecord {
    name: String,
    created: u64,
    updated: u64,
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    library: Option<LibraryRecord>,
    #[serde(default)]
    owner: Option<IgnoredAny>,
    #[serde(default)]
    ip: Option<IgnoredAny>,
    #[serde(default)]
    opened: u64,
}

#[derive(Deserialize)]
struct LibraryRecord {
    id: String,
    name: String,
    version: u64,
}

/// One `links.json` entry as the server writes it; the token hash is not kept.
#[derive(Deserialize)]
struct LinkRecord {
    id: String,
    access: Access,
    label: String,
    created: u64,
    #[serde(default)]
    code: Option<IgnoredAny>,
}

/// Inventory one snapshot file's bytes. Header and checksum are checked before decoding.
#[must_use]
pub fn inventory_snapshot(bytes: &[u8]) -> SnapshotSummary {
    let mut s = SnapshotSummary { bytes: bytes.len() as u64, ..SnapshotSummary::default() };
    if bytes.get(..4) != Some(SNAP_MAGIC.as_slice()) {
        s.fault = Some(ProblemCode::SnapshotBadHeader);
        return s;
    }
    if bytes.get(4) != Some(&DISK_FORMAT) {
        s.fault = Some(ProblemCode::SnapshotUnsupportedFormat);
        return s;
    }
    let (Some(generation), Some(sum), Some(body)) = (u64_at(bytes, 5), u32_at(bytes, 13), bytes.get(SNAP_HEADER..)) else {
        s.fault = Some(ProblemCode::SnapshotTruncated);
        return s;
    };
    s.generation = Some(generation);
    if crc32fast::hash(body) != sum {
        s.fault = Some(ProblemCode::SnapshotChecksumMismatch);
        return s;
    }
    s.checksum_valid = true;
    match message::decode::<SessionState>(body, body.len()) {
        Ok(state) => {
            s.decoded = true;
            s.session_seq = Some(state.seq);
            s.changes = Some(state.snapshot.len() as u64);
            s.retained_batches = Some(state.log.len() as u64);
            s.issued_clients = Some(state.issued.len() as u64);
        }
        Err(_) => s.fault = Some(ProblemCode::SnapshotUndecodable),
    }
    s
}

/// Inventory a log of `len` bytes read from `reader`, holding one record at a time.
///
/// `base_seq` is the snapshot's session sequence when known; replayability is then judged as the
/// server's replay does (batches at or below the reached sequence are skipped, the next must be
/// exactly one more, and the first that is not ends the replay). Reading stops, like the server's,
/// at the first damaged record, and after `max_entries` records.
pub fn inventory_log<R: Read>(mut reader: R, len: u64, base_seq: Option<u64>, max_entries: usize) -> LogSummary {
    let mut s = LogSummary { bytes: len, ..LogSummary::default() };
    let damage = |code, offset: u64| Some(LogDamage { code, offset, trailing_bytes: len.saturating_sub(offset) });
    let mut header = [0u8; LOG_HEADER];
    let Ok(got) = fill(&mut reader, &mut header) else {
        s.damage = damage(ProblemCode::ReadFailed, 0);
        return s;
    };
    let header = header.get(..got).unwrap_or_default();
    if header.get(..4) != Some(LOG_MAGIC.as_slice()) {
        s.damage = damage(ProblemCode::LogBadHeader, 0);
        return s;
    }
    if header.get(4) != Some(&DISK_FORMAT) {
        s.damage = damage(ProblemCode::LogUnsupportedFormat, 0);
        return s;
    }
    let Some(generation) = u64_at(header, 5) else {
        s.damage = damage(ProblemCode::LogTruncatedHeader, 0);
        return s;
    };
    s.generation = Some(generation);
    let mut reached = base_seq;
    let mut at = LOG_HEADER as u64;
    let mut broken = false;
    loop {
        let mut rec = [0u8; RECORD_HEADER];
        match fill(&mut reader, &mut rec) {
            Ok(0) => break,
            Ok(RECORD_HEADER) => {}
            Ok(_) => {
                s.damage = damage(ProblemCode::LogTruncatedRecord, at);
                break;
            }
            Err(_) => {
                s.damage = damage(ProblemCode::ReadFailed, at);
                break;
            }
        }
        if usize::try_from(s.records).map_or(true, |n| n >= max_entries) {
            s.damage = damage(ProblemCode::LogEntryLimitReached, at);
            break;
        }
        let (Some(body_len), Some(sum)) = (u32_at(&rec, 0), u32_at(&rec, 4)) else { break };
        let body_len = u64::from(body_len);
        // The body must fit in what is left of the file: nothing larger is ever allocated.
        let remaining = len.saturating_sub(at.saturating_add(RECORD_HEADER as u64));
        let Some(size) = usize::try_from(body_len).ok().filter(|_| body_len <= remaining) else {
            s.damage = damage(ProblemCode::LogTruncatedRecord, at);
            break;
        };
        let mut body = vec![0u8; size];
        if reader.read_exact(&mut body).is_err() {
            s.damage = damage(ProblemCode::LogTruncatedRecord, at);
            break;
        }
        if crc32fast::hash(&body) != sum {
            s.damage = damage(ProblemCode::LogChecksumMismatch, at);
            break;
        }
        let Ok(entry) = message::decode::<Journal>(&body, body.len()) else {
            s.damage = damage(ProblemCode::LogUndecodableRecord, at);
            break;
        };
        s.records += 1;
        match entry {
            Journal::Issued { .. } => s.issued += 1,
            Journal::Refused { .. } => s.refused += 1,
            Journal::Accepted(acc) => {
                s.accepted += 1;
                s.first_accepted_seq = s.first_accepted_seq.or(Some(acc.seq));
                s.last_accepted_seq = Some(acc.seq);
                if !broken {
                    match reached {
                        Some(r) if acc.seq <= r => s.already_applied += 1,
                        Some(r) if r.checked_add(1) != Some(acc.seq) => {
                            broken = true;
                            s.sequence_break_at = Some(at);
                        }
                        _ => {
                            reached = Some(acc.seq);
                            s.replayable += 1;
                        }
                    }
                }
            }
        }
        at = at.saturating_add(RECORD_HEADER as u64).saturating_add(body_len);
    }
    s.last_replayable_seq = reached;
    s
}

/// Read until `buf` is full or the reader ends; how many bytes were read.
fn fill<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while let Some(rest) = buf.get_mut(got..).filter(|r| !r.is_empty()) {
        match reader.read(rest) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at.checked_add(4)?).and_then(|s| s.try_into().ok()).map(u32::from_le_bytes)
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at.checked_add(8)?).and_then(|s| s.try_into().ok()).map(u64::from_le_bytes)
}

/// A file id the server accepts (letters, digits, `-`, `_`; 1 to 64 bytes).
fn valid_document_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The version in `library-<version>.bin`.
fn library_version(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("library-")?.strip_suffix(".bin")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn is_library_temp(name: &str) -> bool {
    name.strip_suffix(".tmp").is_some_and(|n| n == "library.bin" || library_version(n).is_some())
}

fn is_temp_of(name: &str, known: &[&str]) -> bool {
    name.strip_suffix(".tmp").is_some_and(|n| known.contains(&n))
}

fn clip(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_collab::{Accepted, Change, NodeId};

    fn accepted(seq: u64) -> Journal {
        Journal::Accepted(Accepted { seq, author: 3, batch_id: seq, changes: vec![Change::Delete { id: NodeId(5) }] })
    }

    fn record(entry: &Journal) -> Vec<u8> {
        let body = message::encode(entry).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn log(generation: u64, entries: &[Journal]) -> Vec<u8> {
        let mut out = b"FCLG\x01".to_vec();
        out.extend_from_slice(&generation.to_le_bytes());
        for e in entries {
            out.extend_from_slice(&record(e));
        }
        out
    }

    fn read(bytes: &[u8], base: Option<u64>) -> LogSummary {
        inventory_log(bytes, bytes.len() as u64, base, 1000)
    }

    fn sample() -> Vec<Journal> {
        vec![Journal::Issued { client: 4 }, accepted(5), accepted(6), Journal::Refused { client: 4, batch_id: 9 }, accepted(7)]
    }

    #[test]
    fn counts_entries_by_kind_and_tracks_replayable_sequence() {
        let s = read(&log(2, &sample()), Some(4));
        assert_eq!((s.generation, s.records, s.issued, s.accepted, s.refused), (Some(2), 5, 1, 3, 1));
        assert_eq!((s.replayable, s.already_applied, s.last_replayable_seq), (3, 0, Some(7)));
        assert_eq!((s.first_accepted_seq, s.last_accepted_seq, s.damage), (Some(5), Some(7), None));
    }

    #[test]
    fn already_applied_batches_and_sequence_breaks_follow_server_replay() {
        let s = read(&log(1, &[accepted(3), accepted(4), accepted(5), accepted(7), accepted(8)]), Some(4));
        assert_eq!((s.already_applied, s.replayable, s.last_replayable_seq), (2, 1, Some(5)));
        let break_at = (LOG_HEADER + 3 * record(&accepted(1)).len()) as u64;
        assert_eq!(s.sequence_break_at, Some(break_at));
    }

    #[test]
    fn header_faults_are_classified() {
        assert_eq!(read(b"", None).damage.map(|d| d.code), Some(ProblemCode::LogBadHeader));
        assert_eq!(read(b"FCLG\x02", None).damage.map(|d| d.code), Some(ProblemCode::LogUnsupportedFormat));
        assert_eq!(read(b"FCLG\x01\x00", None).damage.map(|d| d.code), Some(ProblemCode::LogTruncatedHeader));
    }

    #[test]
    fn a_cut_short_tail_is_reported_with_offset_and_trailing_bytes() {
        let full = log(1, &sample());
        let cut = full.get(..full.len() - 3).unwrap();
        let s = read(cut, Some(4));
        let d = s.damage.unwrap();
        assert_eq!(d.code, ProblemCode::LogTruncatedRecord);
        assert_eq!(d.offset + d.trailing_bytes, cut.len() as u64);
        assert_eq!(s.records, 4);
    }

    #[test]
    fn a_flipped_body_byte_is_a_checksum_mismatch() {
        let mut bytes = log(1, &sample());
        let last = bytes.len() - 1;
        bytes[last] ^= 0x40;
        assert_eq!(read(&bytes, None).damage.map(|d| d.code), Some(ProblemCode::LogChecksumMismatch));
    }

    #[test]
    fn checksummed_garbage_is_an_undecodable_record() {
        let mut bytes = log(1, &[]);
        let body = [0xff, 0xff, 0xff];
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        bytes.extend_from_slice(&body);
        assert_eq!(read(&bytes, None).damage.map(|d| d.code), Some(ProblemCode::LogUndecodableRecord));
    }

    #[test]
    fn a_huge_declared_length_is_not_allocated() {
        let mut bytes = log(1, &[]);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(read(&bytes, None).damage.map(|d| d.code), Some(ProblemCode::LogTruncatedRecord));
    }

    #[test]
    fn the_entry_cap_stops_reading() {
        let bytes = log(1, &sample());
        let s = inventory_log(bytes.as_slice(), bytes.len() as u64, None, 2);
        assert_eq!(s.records, 2);
        assert_eq!(s.damage.map(|d| d.code), Some(ProblemCode::LogEntryLimitReached));
    }

    /// Never-crash sweep: every truncation, every single-byte corruption and pseudo-random garbage
    /// over a valid log and snapshot must produce a summary, never a panic, with consistent offsets.
    #[test]
    fn junk_sweep_over_corrupted_logs_and_snapshots_never_panics() {
        let good = log(3, &sample());
        let check = |bytes: &[u8]| {
            let s = read(bytes, Some(4));
            assert!(s.records <= 5);
            if let Some(d) = s.damage {
                assert!(d.offset <= bytes.len() as u64);
                assert_eq!(d.offset + d.trailing_bytes, bytes.len() as u64);
            }
            // A reader that ends early (file shrank after it was measured) is handled too.
            let _ = inventory_log(bytes, bytes.len() as u64 + 17, Some(4), 1000);
        };
        for n in 0..=good.len() {
            check(good.get(..n).unwrap());
        }
        for i in 0..good.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut bytes = good.clone();
                bytes[i] ^= flip;
                check(&bytes);
            }
        }
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let len = (next() % 200) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if next() % 2 == 0 && bytes.len() >= 5 {
                bytes[..5].copy_from_slice(b"FCLG\x01");
            }
            check(&bytes);
            let s = inventory_snapshot(&bytes);
            assert!(s.decoded == (s.fault.is_none()));
        }
        let state = SessionState {
            session: 1,
            snapshot: vec![Change::Delete { id: NodeId(2) }],
            seq: 4,
            next_client: 5,
            issued: Default::default(),
            deleted: Default::default(),
            removed: Default::default(),
            log: Vec::new(),
        };
        let body = message::encode(&state).unwrap();
        let mut snap = b"FCSN\x01".to_vec();
        snap.extend_from_slice(&3u64.to_le_bytes());
        snap.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        snap.extend_from_slice(&body);
        let s = inventory_snapshot(&snap);
        assert_eq!((s.decoded, s.generation, s.session_seq, s.changes), (true, Some(3), Some(4), Some(1)));
        for n in 0..snap.len() {
            assert!(inventory_snapshot(snap.get(..n).unwrap()).fault.is_some());
        }
        for i in 0..snap.len() {
            let mut bytes = snap.clone();
            bytes[i] ^= 0x20;
            let s = inventory_snapshot(&bytes);
            assert_eq!(s.decoded, s.fault.is_none());
        }
    }

    #[test]
    fn problem_codes_have_stable_names() {
        let names: Vec<String> = [ProblemCode::LogTruncatedRecord, ProblemCode::SnapshotChecksumMismatch, ProblemCode::SymlinkSkipped]
            .iter()
            .map(|c| serde_json::to_string(c).unwrap())
            .collect();
        assert_eq!(names, ["\"log_truncated_record\"", "\"snapshot_checksum_mismatch\"", "\"symlink_skipped\""]);
        assert_eq!(serde_json::to_string(&Severity::Blocking).unwrap(), "\"blocking\"");
    }
}
