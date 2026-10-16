//! Dry-run migration planner probes (S3-017). No database or object store is needed: reports are
//! built in memory (and once by scanning a synthetic data directory), planned, and checked.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use figcraft_cloud_storage::legacy_scan::{
    DocumentReport, DocumentStatus, LibraryFile, LibraryMetaSummary, LinkSummary, LinksSummary, LogDamage, LogSummary, MetaSummary, MigrationReport,
    PluginInventory, PluginSummary, Problem, ProblemCode, REPORT_FORMAT_VERSION, ReportTotals, ScanLimits, ServerFiles, SnapshotSummary,
    scan_data_directory,
};
use figcraft_cloud_storage::migration_plan::{
    DeferralReason, ExclusionReason, LEGACY_SYSTEM_ID, MigrationPlan, PLAN_FORMAT_VERSION, PlanBlocker, PlanError, PlanTarget, PlanWarning,
    SNAPSHOT_HEADER_BYTES, derived_id, plan_migration,
};
use figcraft_cloud_storage::objects::valid_key;
use figcraft_collab::message::Access;
use figcraft_collab::{Accepted, Change, Journal, NodeId, SessionState, message};

fn target() -> PlanTarget {
    PlanTarget {
        tenant_id: "5d17a000-0000-4000-8000-000000000001".parse().unwrap(),
        workspace_id: "5d17a000-0000-4000-8000-000000000002".parse().unwrap(),
        project_id: "5d17a000-0000-4000-8000-000000000003".parse().unwrap(),
        principal_id: "5d17a000-0000-4000-8000-000000000004".parse().unwrap(),
        bucket: "figcraft".to_owned(),
    }
}

fn problem(code: ProblemCode, path: &str, document: Option<&str>) -> Problem {
    Problem { code, severity: code.severity(), path: path.to_owned(), document: document.map(str::to_owned), offset: None }
}

fn meta(name: &str) -> MetaSummary {
    MetaSummary {
        name: name.to_owned(),
        created: 10,
        updated: 20,
        opened: 30,
        seq: 7,
        published_without_key: false,
        origin_address_recorded: false,
        library: None,
    }
}

fn snapshot(bytes: u64, seq: u64) -> SnapshotSummary {
    SnapshotSummary {
        bytes,
        generation: Some(2),
        checksum_valid: true,
        decoded: true,
        session_seq: Some(seq),
        changes: Some(1),
        retained_batches: Some(0),
        issued_clients: Some(0),
        fault: None,
    }
}

fn log(bytes: u64, replayable: u64, first: u64) -> LogSummary {
    LogSummary {
        bytes,
        generation: Some(2),
        matches_snapshot: Some(true),
        records: replayable,
        accepted: replayable,
        replayable,
        first_accepted_seq: (replayable > 0).then_some(first),
        last_accepted_seq: (replayable > 0).then(|| first.saturating_add(replayable).saturating_sub(1)),
        last_replayable_seq: Some((first.saturating_add(replayable)).saturating_sub(1)),
        ..LogSummary::default()
    }
}

fn document(id: &str, status: DocumentStatus, seq: u64, replayable: u64) -> DocumentReport {
    DocumentReport {
        id: id.to_owned(),
        path: format!("files/{id}"),
        status,
        meta: Some(meta("Synthetic board")),
        links: Some(LinksSummary {
            edit: 1,
            view: 1,
            with_join_code: 0,
            links: vec![
                LinkSummary { id: "l1".into(), access: Access::Edit, label: "Team".into(), created: 11, has_join_code: false },
                LinkSummary { id: "l2".into(), access: Access::View, label: String::new(), created: 12, has_join_code: false },
            ],
        }),
        snapshot: Some(snapshot(117, seq)),
        log: Some(log(13 + 40 * replayable, replayable, seq.saturating_add(1))),
        last_accepted_seq: Some(seq.saturating_add(replayable)),
        libraries: Vec::new(),
        usage_reports: None,
        bytes: 400,
    }
}

fn report(documents: Vec<DocumentReport>, problems: Vec<Problem>, plugins: Option<PluginInventory>) -> MigrationReport {
    MigrationReport {
        format_version: REPORT_FORMAT_VERSION,
        scanned_at: 1_760_000_000,
        root: "/srv/figcraft-data".to_owned(),
        complete: true,
        migratable: !problems.iter().any(|p| p.code.severity() == figcraft_cloud_storage::legacy_scan::Severity::Blocking),
        totals: ReportTotals::default(),
        server: ServerFiles::default(),
        documents,
        plugins,
        problems,
    }
}

/// A ready document, a degraded one with a torn log and two libraries, a blocked one and plugins.
fn mixed_report() -> MigrationReport {
    let ready = document("doc-a", DocumentStatus::Ready, 4, 2);
    let mut degraded = document("doc-b", DocumentStatus::Degraded, 9, 1);
    if let Some(log) = degraded.log.as_mut() {
        log.damage = Some(LogDamage { code: ProblemCode::LogTruncatedRecord, offset: 53, trailing_bytes: 5 });
    }
    degraded.meta = Some(MetaSummary { library: Some(LibraryMetaSummary { id: "lib".into(), name: "Kit".into(), version: 2 }), ..meta("   ") });
    degraded.libraries = vec![
        LibraryFile { name: "library-2.bin".into(), version: Some(2), bytes: 900, parsed: false },
        LibraryFile { name: "library.bin".into(), version: None, bytes: 800, parsed: false },
    ];
    let mut blocked = document("doc-c", DocumentStatus::Blocked, 1, 0);
    blocked.snapshot = Some(SnapshotSummary {
        checksum_valid: false,
        decoded: false,
        session_seq: None,
        fault: Some(ProblemCode::SnapshotChecksumMismatch),
        ..snapshot(50, 0)
    });
    let plugins = PluginInventory {
        index_present: true,
        entries: vec![
            PluginSummary { id: "grid".into(), version: "3".into(), bytes: 64, sha256: "ab".repeat(32), bundle_present: true, bundle_valid: true },
            PluginSummary {
                id: "notes".into(),
                // Build metadata is not part of a key segment, so this version is deferred.
                version: "1.2.0+build".into(),
                bytes: 70,
                sha256: "cd".repeat(32),
                bundle_present: true,
                bundle_valid: true,
            },
            PluginSummary { id: "lost".into(), version: "1".into(), bytes: 80, sha256: "ef".repeat(32), bundle_present: false, bundle_valid: false },
        ],
    };
    report(
        vec![ready, degraded, blocked],
        vec![
            problem(ProblemCode::LogTruncatedRecord, "files/doc-b/log.bin", Some("doc-b")),
            problem(ProblemCode::LibraryMissing, "files/doc-b/library-3.bin", Some("doc-b")),
            problem(ProblemCode::LeftoverTempFile, "files/doc-b/meta.json.tmp", Some("doc-b")),
            problem(ProblemCode::SnapshotChecksumMismatch, "files/doc-c/snapshot.bin", Some("doc-c")),
            problem(ProblemCode::PluginBundleMissing, "plugins/efef.bin", None),
        ],
        Some(plugins),
    )
}

#[test]
fn plans_are_identical_across_runs_and_ids_are_derived_from_legacy_ids() {
    let report = mixed_report();
    let first = plan_migration(&report, &target()).unwrap();
    let second = plan_migration(&report.clone(), &target()).unwrap();
    assert_eq!(first, second);
    assert_eq!(serde_json::to_string(&first).unwrap(), serde_json::to_string(&second).unwrap());
    let doc = &first.documents[0];
    assert_eq!(doc.document.id.as_uuid(), derived_id(target().tenant_id, &["document", "doc-a"]));
    assert_eq!(doc.external_mapping.internal_id, doc.document.id.as_uuid());
    assert_eq!(
        (doc.external_mapping.system_id.as_str(), doc.external_mapping.resource_kind.as_str(), doc.external_mapping.external_id.as_str()),
        (LEGACY_SYSTEM_ID, "document", "doc-a")
    );
    // Document order in the report does not matter.
    let mut shuffled = report.clone();
    shuffled.documents.reverse();
    shuffled.plugins.as_mut().unwrap().entries.reverse();
    assert_eq!(plan_migration(&shuffled, &target()).unwrap(), first);
    // Another tenant gets other ids.
    let mut other = target();
    other.tenant_id = "5d17a000-0000-4000-8000-0000000000ff".parse().unwrap();
    let other_plan = plan_migration(&report, &other).unwrap();
    assert_ne!(other_plan.documents[0].document.id, doc.document.id);
    assert!(other_plan.documents[0].snapshot.object.key_prefix.starts_with("tenants/5d17a000-0000-4000-8000-0000000000ff/"));
}

#[test]
fn blocked_documents_are_excluded_with_their_problem_codes() {
    let plan = plan_migration(&mixed_report(), &target()).unwrap();
    assert!(plan.documents.iter().all(|d| d.legacy_id != "doc-c"));
    assert_eq!(plan.excluded.len(), 1);
    let excluded = &plan.excluded[0];
    assert_eq!((excluded.legacy_id.as_str(), excluded.reason), ("doc-c", ExclusionReason::Blocked));
    assert_eq!(excluded.problems, vec![ProblemCode::SnapshotChecksumMismatch]);
    assert!(!plan.executable);
    assert_eq!(plan.blockers, vec![PlanBlocker::ExclusionApprovalRequired]);

    // A status claiming ready does not override a blocking problem recorded for the document.
    let mut lying = mixed_report();
    lying.documents[2].status = DocumentStatus::Ready;
    let plan = plan_migration(&lying, &target()).unwrap();
    assert_eq!(plan.excluded[0].reason, ExclusionReason::Blocked);
}

#[test]
fn degraded_documents_are_included_with_warnings() {
    let plan = plan_migration(&mixed_report(), &target()).unwrap();
    let doc = plan.documents.iter().find(|d| d.legacy_id == "doc-b").unwrap();
    assert_eq!(doc.status, DocumentStatus::Degraded);
    assert_eq!(doc.problems, vec![ProblemCode::LogTruncatedRecord, ProblemCode::LibraryMissing]);
    assert_eq!(doc.warnings, vec![PlanWarning::NameReplaced, PlanWarning::LogTruncated, PlanWarning::LibrariesDeferred]);
    assert_eq!(doc.document.name, "doc-b");
    assert_eq!((doc.events.count, doc.events.first_seq, doc.events.last_seq, doc.document.current_seq), (1, Some(10), Some(10), 10));
    assert_eq!(doc.libraries.len(), 1);
    assert_eq!(doc.libraries[0].source, "files/doc-b/library-2.bin");
    let deferred = plan.deferred.iter().find(|d| d.source == "files/doc-b/library.bin").unwrap();
    assert_eq!(deferred.reason, DeferralReason::LibraryUnversioned);
}

#[test]
fn incomplete_or_unsupported_reports_and_bad_buckets_are_refused_or_blocked() {
    let mut future = mixed_report();
    future.format_version = REPORT_FORMAT_VERSION + 1;
    assert_eq!(plan_migration(&future, &target()), Err(PlanError::UnsupportedReportFormat));
    for bucket in ["", "ab", "Upper", "a..b", "-edge", "x".repeat(64).as_str(), "figcraft/x"] {
        let mut t = target();
        t.bucket = bucket.to_owned();
        assert_eq!(plan_migration(&mixed_report(), &t), Err(PlanError::InvalidBucket), "{bucket}");
    }
    let mut incomplete =
        report(vec![document("doc-a", DocumentStatus::Ready, 0, 0)], vec![problem(ProblemCode::DirectoryUnreadable, "files", None)], None);
    incomplete.complete = false;
    let plan = plan_migration(&incomplete, &target()).unwrap();
    assert_eq!(plan.blockers, vec![PlanBlocker::ReportIncomplete, PlanBlocker::BlockingServerProblem]);
    assert!(!plan.executable);
    let clean = plan_migration(&report(vec![document("doc-a", DocumentStatus::Ready, 0, 0)], Vec::new(), None), &target()).unwrap();
    assert!(clean.executable && clean.blockers.is_empty());
    assert_eq!((clean.documents[0].events.count, clean.documents[0].events.first_seq), (0, None));
}

#[test]
fn inconsistent_and_malformed_documents_are_excluded() {
    let mut wrong_seq = document("seq-off", DocumentStatus::Ready, 4, 2);
    wrong_seq.last_accepted_seq = Some(9);
    let mut huge = document("huge", DocumentStatus::Ready, u64::MAX - 1, 1);
    huge.last_accepted_seq = Some(u64::MAX);
    let mut no_snapshot = document("no-snap", DocumentStatus::Ready, 0, 0);
    no_snapshot.snapshot = None;
    let mut tiny = document("tiny", DocumentStatus::Ready, 0, 0);
    tiny.snapshot = Some(snapshot(SNAPSHOT_HEADER_BYTES, 0));
    let mut no_meta = document("no-meta", DocumentStatus::Ready, 0, 0);
    no_meta.meta = None;
    let bad_id = document("../escape", DocumentStatus::Ready, 0, 0);
    let dup = document("twin", DocumentStatus::Ready, 0, 0);
    let plan =
        plan_migration(&report(vec![wrong_seq, huge, no_snapshot, tiny, no_meta, bad_id, dup.clone(), dup], Vec::new(), None), &target()).unwrap();
    assert!(plan.documents.is_empty());
    let reasons: BTreeMap<&str, ExclusionReason> = plan.excluded.iter().map(|e| (e.legacy_id.as_str(), e.reason)).collect();
    assert_eq!(reasons["seq-off"], ExclusionReason::SequenceInconsistent);
    assert_eq!(reasons["huge"], ExclusionReason::SequenceInconsistent);
    assert_eq!(reasons["no-snap"], ExclusionReason::SnapshotUnavailable);
    assert_eq!(reasons["tiny"], ExclusionReason::SnapshotUnavailable);
    assert_eq!(reasons["no-meta"], ExclusionReason::MetadataUnavailable);
    assert_eq!(reasons["../escape"], ExclusionReason::InvalidLegacyId);
    assert_eq!(reasons["twin"], ExclusionReason::DuplicateLegacyId);
    assert_eq!(plan.excluded.len(), 7);
}

#[test]
fn stale_logs_are_ignored_and_plugins_are_planned_or_deferred() {
    let mut stale = document("stale", DocumentStatus::Ready, 6, 0);
    stale.log = Some(LogSummary { matches_snapshot: Some(false), accepted: 3, ..LogSummary::default() });
    stale.last_accepted_seq = Some(6);
    let plan = plan_migration(&report(vec![stale], Vec::new(), None), &target()).unwrap();
    assert_eq!(plan.documents[0].warnings, vec![PlanWarning::LogIgnored]);
    assert_eq!((plan.documents[0].events.count, plan.documents[0].events.source_log_bytes), (0, 0));

    let plan = plan_migration(&mixed_report(), &target()).unwrap();
    assert_eq!(plan.plugins.len(), 1);
    let grid = &plan.plugins[0];
    let key = grid.object.object_key.as_deref().unwrap();
    assert!(valid_key(key) && key.ends_with(&format!("/3-{}.bin", "ab".repeat(32))), "{key}");
    let reasons: BTreeSet<DeferralReason> = plan.deferred.iter().filter(|d| d.legacy_document.is_none()).map(|d| d.reason).collect();
    assert_eq!(reasons, BTreeSet::from([DeferralReason::PluginVersionInvalid, DeferralReason::PluginBundleInvalid]));
}

/// The JSON layout is part of the format: pin it, ids included.
#[test]
fn plan_json_format_is_pinned() {
    let mut doc = document("doc-a", DocumentStatus::Ready, 4, 2);
    doc.libraries = vec![LibraryFile { name: "library-1.bin".into(), version: Some(1), bytes: 30, parsed: false }];
    let plugins = PluginInventory {
        index_present: true,
        entries: vec![PluginSummary {
            id: "grid".into(),
            version: "3".into(),
            bytes: 64,
            sha256: "ab".repeat(32),
            bundle_present: true,
            bundle_valid: true,
        }],
    };
    let plan = plan_migration(&report(vec![doc], Vec::new(), Some(plugins)), &target()).unwrap();
    let json = serde_json::to_string_pretty(&plan).unwrap();
    let expected = include_str!("migration_plan_golden.json");
    assert_eq!(json.trim_end(), expected.trim_end(), "plan JSON changed; bump PLAN_FORMAT_VERSION if a consumer must notice\n{json}");
    let back: MigrationPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(back, plan);
    assert_eq!(back.format_version, PLAN_FORMAT_VERSION);
}

#[test]
fn a_scanned_directory_plans_its_replayable_events() {
    let dir = std::env::temp_dir().join(format!("figcraft-migration-plan-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    write_document(&dir, "doc-a", 3, &[accepted(4), accepted(5), accepted(7)]);
    let scanned = scan_data_directory(&dir, 1_760_000_000, &ScanLimits::default());
    let before = tree(&dir);
    let plan = plan_migration(&scanned, &target()).unwrap();
    assert_eq!(tree(&dir), before);
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(plan.documents.len(), 1, "{:?}", plan.excluded);
    let doc = &plan.documents[0];
    assert_eq!(doc.status, DocumentStatus::Degraded);
    assert_eq!(doc.problems, vec![ProblemCode::LogSequenceBreak]);
    assert_eq!((doc.events.count, doc.events.first_seq, doc.events.last_seq), (2, Some(4), Some(5)));
    assert_eq!(doc.snapshot.object.size_bytes, scanned.documents[0].snapshot.as_ref().unwrap().bytes - SNAPSHOT_HEADER_BYTES);
    assert_eq!(plan.totals.events, scanned.totals.replayable_batches);
    assert_eq!(doc.links, 1);
}

/// Pseudo-random reports, including hostile field values, never panic the planner, and every plan
/// keeps its invariants.
#[test]
fn sweep_over_synthetic_reports_keeps_plan_invariants() {
    let mut rng = 0x5eed_u64;
    let mut next = move |bound: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % bound.max(1)
    };
    let statuses = [DocumentStatus::Ready, DocumentStatus::Degraded, DocumentStatus::Blocked];
    let codes = [
        ProblemCode::LogTruncatedRecord,
        ProblemCode::SnapshotTruncated,
        ProblemCode::UnknownEntry,
        ProblemCode::LeftoverTempFile,
        ProblemCode::MetaMissing,
    ];
    let ids = ["a", "b", "c", "doc-1", "doc_2", "", "bad id", "é", "../x", &"z".repeat(65)];
    for round in 0..2_000 {
        let mut documents = Vec::new();
        let mut problems = Vec::new();
        for _ in 0..next(8) {
            let id = ids[next(ids.len() as u64) as usize];
            let seq = [0, 1, 5, u64::MAX - 1, u64::MAX, i64::MAX as u64][next(6) as usize];
            let replayable = next(4);
            let mut d = document(id, statuses[next(3) as usize], seq.saturating_sub(replayable), replayable);
            if next(4) == 0 {
                d.last_accepted_seq = Some(next(20));
            }
            if next(5) == 0 {
                d.snapshot = Some(snapshot(next(40), next(10)));
            }
            if next(6) == 0 {
                d.meta = Some(meta(&"x ".repeat(next(300) as usize)));
            }
            if next(4) == 0 {
                d.log.as_mut().unwrap().matches_snapshot = [None, Some(false), Some(true)][next(3) as usize];
            }
            for _ in 0..next(4) {
                let name = ["library.bin", "library-1.bin", "library-1.bin", "library-x.bin", "other"][next(5) as usize];
                d.libraries.push(LibraryFile { name: name.into(), version: None, bytes: [0, 5, u64::MAX][next(3) as usize], parsed: false });
            }
            if next(3) == 0 {
                problems.push(problem(codes[next(5) as usize], "files/x", Some(id)));
            }
            documents.push(d);
        }
        let entries = (0..next(5))
            .map(|_| PluginSummary {
                id: ["p", "", "q"][next(3) as usize].into(),
                version: ["1", "01", "+1", "1.0", ""][next(5) as usize].into(),
                bytes: [0, 9, u64::MAX][next(3) as usize],
                sha256: ["ab".repeat(32), "AB".repeat(32), "a".into()][next(3) as usize].clone(),
                bundle_present: next(2) == 0,
                bundle_valid: next(2) == 0,
            })
            .collect();
        let mut r = report(documents, problems, Some(PluginInventory { index_present: true, entries }));
        r.complete = next(5) != 0;
        let plan = plan_migration(&r, &target()).unwrap();
        assert_eq!(plan, plan_migration(&r, &target()).unwrap(), "round {round}");

        let mut object_ids = BTreeSet::new();
        let mut row_ids = BTreeSet::new();
        for d in &plan.documents {
            assert!(row_ids.insert(d.document.id.as_uuid()) && row_ids.insert(d.snapshot.id), "round {round}");
            assert!((1..=255).contains(&d.document.name.chars().count()));
            assert!(d.document.current_seq <= i64::MAX as u64);
            assert_eq!(d.snapshot.seq + d.events.count, d.document.current_seq);
            for o in std::iter::once(&d.snapshot.object).chain(&d.libraries) {
                assert!(object_ids.insert(o.id), "round {round}");
                assert!(o.size_bytes > 0 && o.size_bytes <= i64::MAX as u64);
                assert!(valid_key(&format!("{}{}{}", o.key_prefix, "0".repeat(64), o.key_suffix)), "{}", o.key_prefix);
            }
        }
        for p in &plan.plugins {
            assert!(object_ids.insert(p.object.id) && valid_key(p.object.object_key.as_deref().unwrap()));
        }
        let input = r.documents.len() as u64;
        assert!(plan.totals.documents + plan.totals.excluded <= input);
        assert_eq!(plan.totals.objects as usize, object_ids.len());
        assert_eq!(plan.executable, plan.blockers.is_empty());
        assert_eq!(plan.blockers.contains(&PlanBlocker::ReportIncomplete), !r.complete);
    }
}

#[test]
fn document_cap_blocks_the_plan_without_unbounded_work() {
    let documents: Vec<DocumentReport> = (0..100_001).map(|i| document(&format!("d{i}"), DocumentStatus::Ready, 0, 0)).collect();
    let plan = plan_migration(&report(documents, Vec::new(), None), &target()).unwrap();
    assert_eq!(plan.documents.len(), 100_000);
    assert!(plan.blockers.contains(&PlanBlocker::DocumentLimitReached) && !plan.executable);
}

fn accepted(seq: u64) -> Journal {
    Journal::Accepted(Accepted { seq, author: 2, batch_id: seq, changes: vec![Change::Delete { id: NodeId(9) }] })
}

fn write_document(root: &std::path::Path, id: &str, seq: u64, entries: &[Journal]) {
    let folder = root.join("files").join(id);
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("meta.json"), br#"{"name":"Synthetic board","created":10,"updated":20,"seq":7,"opened":30}"#).unwrap();
    fs::write(folder.join("links.json"), br#"[{"id":"l1","access":"edit","label":"Team","created":11,"hash":"00"}]"#).unwrap();
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
    let mut snap = b"FCSN\x01".to_vec();
    snap.extend_from_slice(&2_u64.to_le_bytes());
    snap.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    snap.extend_from_slice(&body);
    fs::write(folder.join("snapshot.bin"), snap).unwrap();
    let mut log = b"FCLG\x01".to_vec();
    log.extend_from_slice(&2_u64.to_le_bytes());
    for e in entries {
        let body = message::encode(e).unwrap();
        log.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        log.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        log.extend_from_slice(&body);
    }
    fs::write(folder.join("log.bin"), log).unwrap();
}

fn tree(root: &std::path::Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), fs::read(e.path()).unwrap());
            }
        }
    }
    out
}
