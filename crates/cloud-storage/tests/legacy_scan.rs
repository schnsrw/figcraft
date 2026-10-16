//! Filesystem-server data directory scanner probes (S2-021). No database is needed: each test
//! builds a synthetic data directory in the documented on-disk format under a unique temporary
//! folder, scans it, and removes it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use figcraft_cloud_storage::legacy_scan::{
    DocumentStatus, MigrationReport, ProblemCode, REPORT_FORMAT_VERSION, ScanLimits, Severity, scan_data_directory,
};
use figcraft_collab::{Accepted, Change, Journal, NodeId, SessionState, message};
use sha2::{Digest, Sha256};

/// A temporary data directory, removed when dropped.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("figcraft-legacy-scan-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("files")).unwrap();
        Self(dir)
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let path = self.0.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A complete, healthy file folder whose snapshot is at `seq` and whose log adds `extra`.
    fn document(&self, id: &str, generation: u64, seq: u64, log_entries: &[Journal]) {
        self.write(&format!("files/{id}/meta.json"), br#"{"name":"Synthetic board","created":10,"updated":20,"seq":7,"opened":30}"#);
        self.write(
            &format!("files/{id}/links.json"),
            br#"[{"id":"l1","access":"edit","label":"Team","created":11,"hash":"00"},
                 {"id":"l2","access":"view","label":"","created":12,"hash":"11","code":{"hash":"aa","token":"bb"}}]"#,
        );
        self.write(&format!("files/{id}/snapshot.bin"), &snapshot(generation, seq));
        self.write(&format!("files/{id}/log.bin"), &log(generation, log_entries));
    }

    fn scan(&self) -> MigrationReport {
        scan_data_directory(&self.0, 1_760_000_000, &ScanLimits::default())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn accepted(seq: u64) -> Journal {
    Journal::Accepted(Accepted { seq, author: 2, batch_id: seq, changes: vec![Change::Delete { id: NodeId(9) }] })
}

fn snapshot(generation: u64, seq: u64) -> Vec<u8> {
    let state = SessionState {
        session: 77,
        snapshot: vec![Change::Delete { id: NodeId(1) }],
        seq,
        next_client: 3,
        issued: BTreeMap::new(),
        deleted: Default::default(),
        removed: Default::default(),
        log: Vec::new(),
    };
    let body = message::encode(&state).unwrap();
    let mut out = b"FCSN\x01".to_vec();
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn log(generation: u64, entries: &[Journal]) -> Vec<u8> {
    let mut out = b"FCLG\x01".to_vec();
    out.extend_from_slice(&generation.to_le_bytes());
    for e in entries {
        let body = message::encode(e).unwrap();
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
    }
    out
}

fn codes(report: &MigrationReport, document: &str) -> Vec<ProblemCode> {
    report.problems.iter().filter(|p| p.document.as_deref() == Some(document)).map(|p| p.code).collect()
}

/// Every file's bytes and modification time, to prove a scan changed nothing.
fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                out.insert(e.path(), (fs::read(e.path()).unwrap(), e.metadata().unwrap().modified().unwrap()));
            }
        }
    }
    out
}

#[test]
fn a_healthy_directory_is_inventoried_and_migratable() {
    let f = Fixture::new("healthy");
    f.document("doc-a", 2, 4, &[Journal::Issued { client: 2 }, accepted(5), accepted(6), Journal::Refused { client: 2, batch_id: 1 }]);
    f.write("LOCK", b"");
    f.write("server.key", b"never-read-secret-value\n");
    f.write("blocked.json", br#"["192.0.2.1","192.0.2.2"]"#);
    f.write("reports.json", b"[]");
    let report = f.scan();
    assert_eq!(report.format_version, REPORT_FORMAT_VERSION);
    assert_eq!(report.scanned_at, 1_760_000_000);
    assert!(report.complete && report.migratable, "{:?}", report.problems);
    assert!(report.server.lock_present && report.server.key_present && !report.server.runtime_file_present);
    assert_eq!((report.server.blocked_addresses, report.server.abuse_reports), (Some(2), Some(0)));
    let doc = &report.documents[0];
    assert_eq!((doc.id.as_str(), doc.status), ("doc-a", DocumentStatus::Ready));
    let meta = doc.meta.as_ref().unwrap();
    assert_eq!((meta.name.as_str(), meta.seq, meta.published_without_key), ("Synthetic board", 7, false));
    let links = doc.links.as_ref().unwrap();
    assert_eq!((links.edit, links.view, links.with_join_code), (1, 1, 1));
    let snap = doc.snapshot.as_ref().unwrap();
    assert_eq!((snap.generation, snap.session_seq, snap.decoded), (Some(2), Some(4), true));
    let log = doc.log.as_ref().unwrap();
    assert_eq!((log.matches_snapshot, log.issued, log.accepted, log.refused, log.replayable), (Some(true), 1, 2, 1, 2));
    assert_eq!(doc.last_accepted_seq, Some(6));
    assert_eq!(report.totals.replayable_batches, 2);
    assert_eq!((report.totals.documents, report.totals.ready), (1, 1));

    // The report carries no secrets or addresses and round-trips through JSON.
    let json = serde_json::to_string(&report).unwrap();
    for secret in ["never-read-secret-value", "192.0.2.1", "\"aa\"", "\"hash\""] {
        assert!(!json.contains(secret), "{secret} leaked into the report");
    }
    assert_eq!(serde_json::from_str::<MigrationReport>(&json).unwrap(), report);
}

#[test]
fn damage_is_classified_per_document_and_blocks_only_what_it_should() {
    let f = Fixture::new("damage");
    // Torn log tail: the server drops it, so the document is degraded, not blocked.
    f.document("torn", 1, 0, &[accepted(1), accepted(2)]);
    let path = f.0.join("files/torn/log.bin");
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 2]).unwrap();
    // Corrupt snapshot checksum: nothing to migrate from, blocked.
    f.document("badsnap", 1, 0, &[]);
    let path = f.0.join("files/badsnap/snapshot.bin");
    let mut bytes = fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&path, bytes).unwrap();
    // A log from before an interrupted compaction is ignored by the server: info only.
    f.document("stale", 3, 9, &[]);
    f.write("files/stale/log.bin", &log(2, &[accepted(1)]));
    // A sequence that jumps: the server stops replaying at the jump.
    f.document("gap", 1, 4, &[accepted(5), accepted(8)]);
    // Missing links and unreadable meta block; unknown files and leftovers are reported.
    f.document("broken", 1, 0, &[]);
    fs::remove_file(f.0.join("files/broken/links.json")).unwrap();
    f.write("files/broken/meta.json", b"{not json");
    f.write("files/broken/notes.txt", b"?");
    f.write("files/broken/snapshot.bin.tmp", b"partial");
    f.write("files/bad.id!/meta.json", b"{}");
    f.write("stray.bin", b"x");

    let report = f.scan();
    assert!(report.complete);
    assert!(!report.migratable);
    let status = |id: &str| report.documents.iter().find(|d| d.id == id).unwrap().status;
    assert_eq!(status("torn"), DocumentStatus::Degraded);
    assert_eq!(codes(&report, "torn"), [ProblemCode::LogTruncatedRecord]);
    let torn = report.documents.iter().find(|d| d.id == "torn").unwrap();
    assert_eq!(torn.last_accepted_seq, Some(1));
    assert_eq!(status("badsnap"), DocumentStatus::Blocked);
    assert_eq!(codes(&report, "badsnap"), [ProblemCode::SnapshotChecksumMismatch]);
    assert_eq!(status("stale"), DocumentStatus::Ready);
    assert_eq!(codes(&report, "stale"), [ProblemCode::LogStaleGeneration]);
    assert_eq!(report.documents.iter().find(|d| d.id == "stale").unwrap().last_accepted_seq, Some(9));
    assert_eq!(status("gap"), DocumentStatus::Degraded);
    assert_eq!(codes(&report, "gap"), [ProblemCode::LogSequenceBreak]);
    assert_eq!(report.documents.iter().find(|d| d.id == "gap").unwrap().last_accepted_seq, Some(5));
    assert_eq!(status("broken"), DocumentStatus::Blocked);
    let mut broken = codes(&report, "broken");
    broken.sort();
    assert_eq!(broken, [ProblemCode::UnknownEntry, ProblemCode::LeftoverTempFile, ProblemCode::MetaUnreadable, ProblemCode::LinksMissing]);
    let root_codes: Vec<_> = report.problems.iter().filter(|p| p.document.is_none()).map(|p| (p.code, p.path.as_str())).collect();
    assert!(root_codes.contains(&(ProblemCode::InvalidDocumentId, "files/bad.id!")));
    assert!(root_codes.contains(&(ProblemCode::UnknownEntry, "stray.bin")));
    assert_eq!(report.totals.blocked, 2);
    assert!(report.problems.iter().all(|p| p.severity == p.code.severity()));
    assert_eq!(report.totals.blocking_problems, report.problems.iter().filter(|p| p.severity == Severity::Blocking).count() as u64);
}

#[test]
fn scanning_changes_nothing_on_disk() {
    let f = Fixture::new("readonly");
    f.document("doc", 1, 0, &[accepted(1)]);
    f.write("files/doc/log.bin.tmp", b"left by a crash");
    f.write("plugins/index.json.tmp", b"[");
    f.write("server.key.tmp", b"x");
    let before = tree(&f.0);
    let report = f.scan();
    assert_eq!(report.problems.iter().filter(|p| p.code == ProblemCode::LeftoverTempFile).count(), 3);
    assert_eq!(tree(&f.0), before);
}

#[cfg(unix)]
#[test]
fn symlinks_are_reported_and_never_followed() {
    let f = Fixture::new("links");
    let outside = Fixture::new("outside");
    outside.document("secret", 1, 0, &[]);
    f.document("doc", 1, 0, &[]);
    std::os::unix::fs::symlink(outside.0.join("files/secret"), f.0.join("files/escape")).unwrap();
    fs::remove_file(f.0.join("files/doc/snapshot.bin")).unwrap();
    std::os::unix::fs::symlink(outside.0.join("files/secret/snapshot.bin"), f.0.join("files/doc/snapshot.bin")).unwrap();
    std::os::unix::fs::symlink(&outside.0, f.0.join("plugins")).unwrap();
    let report = f.scan();
    assert_eq!(report.documents.len(), 1);
    assert!(report.plugins.is_none());
    let links: Vec<_> = report.problems.iter().filter(|p| p.code == ProblemCode::SymlinkSkipped).map(|p| p.path.as_str()).collect();
    assert_eq!(links, ["plugins", "files/doc/snapshot.bin", "files/escape"]);
    let doc = &report.documents[0];
    assert!(doc.snapshot.is_none());
    assert_eq!(doc.status, DocumentStatus::Blocked);
}

#[test]
fn plugins_are_checked_by_size_and_hash() {
    let f = Fixture::new("plugins");
    let bundle = b"synthetic bundle bytes";
    let sha: String = Sha256::digest(bundle).iter().map(|b| format!("{b:02x}")).collect();
    let missing = "a".repeat(64);
    f.write(&format!("plugins/{sha}.bin"), bundle);
    let index = format!(
        r#"[{{"id":"demo.one","version":"1.0.0","bytes":{},"sha256":"{sha}"}},
            {{"id":"demo.two","version":"2.0.0","bytes":5,"sha256":"{missing}"}},
            {{"id":"demo.bad","version":"1.0.0","bytes":5,"sha256":"../../etc/passwd"}}]"#,
        bundle.len()
    );
    f.write("plugins/index.json", index.as_bytes());
    let report = f.scan();
    let plugins = report.plugins.as_ref().unwrap();
    assert_eq!(plugins.entries.len(), 2);
    assert!(plugins.entries[0].bundle_valid);
    assert!(!plugins.entries[1].bundle_present);
    let codes: Vec<_> = report.problems.iter().map(|p| p.code).collect();
    assert!(codes.contains(&ProblemCode::PluginBundleMissing) && codes.contains(&ProblemCode::PluginEntryInvalid));
    assert_eq!(report.totals.plugin_bytes, bundle.len() as u64);

    // A bundle whose bytes changed keeps its size but fails the hash.
    f.write(&format!("plugins/{sha}.bin"), b"synthetic bundle BYTES");
    let report = f.scan();
    assert!(!report.plugins.as_ref().unwrap().entries[0].bundle_valid);
    assert!(report.problems.iter().any(|p| p.code == ProblemCode::PluginBundleMismatch));
}

#[test]
fn caps_mark_the_report_incomplete() {
    let f = Fixture::new("caps");
    for i in 0..3 {
        f.document(&format!("d{i}"), 1, 0, &[accepted(1), accepted(2), accepted(3)]);
    }
    f.write("files/d0/snapshot.bin", &[0u8; 4096]);
    let limits = ScanLimits { max_documents: 2, max_log_entries: 2, max_snapshot_bytes: 1024, max_problems: 3, ..ScanLimits::default() };
    let report = scan_data_directory(&f.0, 0, &limits);
    assert_eq!(report.documents.len(), 2);
    assert!(!report.complete && !report.migratable);
    assert!(report.totals.problems_dropped > 0);
    assert_eq!(report.problems.last().map(|p| p.code), Some(ProblemCode::ProblemLimitReached));
    let first: Vec<_> = report.problems.iter().take(3).map(|p| p.code).collect();
    assert_eq!(first, [ProblemCode::FileTooLarge, ProblemCode::LogEntryLimitReached, ProblemCode::LogEntryLimitReached]);
}

#[test]
fn a_missing_or_non_directory_root_is_blocking() {
    let f = Fixture::new("root");
    let report = scan_data_directory(&f.0.join("nope"), 0, &ScanLimits::default());
    assert_eq!(report.problems[0].code, ProblemCode::RootUnreadable);
    assert!(!report.migratable);
    f.write("file", b"x");
    let report = scan_data_directory(&f.0.join("file"), 0, &ScanLimits::default());
    assert_eq!(report.problems[0].code, ProblemCode::RootNotDirectory);
}

/// Never-crash sweep at directory level: every single-byte corruption of a document's files is
/// scanned without a panic and never yields a ready document with a damaged snapshot.
#[test]
fn corrupted_document_files_never_panic() {
    let f = Fixture::new("sweep");
    f.document("doc", 1, 2, &[accepted(3), accepted(4)]);
    for name in ["snapshot.bin", "log.bin", "meta.json", "links.json"] {
        let path = f.0.join("files/doc").join(name);
        let good = fs::read(&path).unwrap();
        for i in (0..good.len()).step_by(3) {
            let mut bytes = good.clone();
            bytes[i] ^= 0xa5;
            fs::write(&path, &bytes).unwrap();
            let report = f.scan();
            let doc = &report.documents[0];
            if doc.snapshot.as_ref().is_some_and(|s| !s.decoded) {
                assert_eq!(doc.status, DocumentStatus::Blocked);
            }
        }
        fs::write(&path, &good).unwrap();
    }
}
