//! End-to-end tests on throwaway Git repositories: discovery, references, incremental updates,
//! deletes, renames, branch switches, merges, stale edges, limits, concurrency and corruption.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "tests")]

use codegraph::query::{Ctx, Page, Response};
use codegraph::{db, indexer, scip_import, tasks};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        let r = Repo { _dir: dir, root };
        r.write("Cargo.toml", "[package]\nname = \"alpha-core\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n");
        r.write("src/lib.rs", "pub mod shapes;\npub mod util;\n\npub use shapes::Shape;\n");
        r.write(
            "src/shapes.rs",
            "use crate::util::scale;\n\n/// Anything with an area.\npub trait Shape {\n    fn area(&self) -> f64;\n}\n\npub struct Square(pub f64);\n\nimpl Shape for Square {\n    fn area(&self) -> f64 {\n        scale(self.0 * self.0)\n    }\n}\n\npub fn total(shapes: &[Square]) -> f64 {\n    shapes.iter().map(|s| s.area()).sum()\n}\n",
        );
        r.write("src/util.rs", "/// Scales a value.\npub fn scale(x: f64) -> f64 {\n    x * 2.0\n}\n");
        r.write(
            "tests/shapes.rs",
            "use alpha_core::shapes::{total, Square};\n\n#[test]\nfn totals() {\n    assert_eq!(total(&[Square(1.0)]), 2.0);\n}\n",
        );
        r.write("web/app.ts", "import { helper } from './helper';\nexport class Widget implements Drawable { draw() { return helper(); } }\n");
        r.write("web/helper.ts", "export function helper(): number { return 1; }\n");
        r.write("target/debug/junk.rs", "pub fn should_not_be_indexed() {}\n");
        r.write(".gitignore", "/target/\n");
        r.commit("init");
        r
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        // Make mtime-based change detection see every write, even within the same tick.
        bump_mtime(&p);
    }

    fn commit(&self, msg: &str) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "-m", msg]);
    }

    fn index(&self) -> indexer::Stats {
        indexer::update(&self.root, false).unwrap()
    }

    fn ask<T>(&self, f: impl FnOnce(&Ctx) -> T) -> T {
        let db = db::open_read(&self.root).unwrap();
        let ctx = Ctx { db: &db, root: &self.root };
        f(&ctx)
    }
}

fn bump_mtime(p: &Path) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TICK: AtomicU64 = AtomicU64::new(1);
    let t = std::time::SystemTime::now() + std::time::Duration::from_secs(TICK.fetch_add(1, Ordering::SeqCst));
    let f = std::fs::File::options().write(true).open(p).unwrap();
    f.set_modified(t).unwrap();
}

fn names(r: &Response) -> Vec<String> {
    r.results.iter().map(|x| x.name.clone()).collect()
}

fn page() -> Page {
    Page::new(Some(50), None)
}

#[test]
fn discovers_symbols_packages_and_skips_ignored_files() {
    let r = Repo::new();
    let s = r.index();
    assert_eq!(s.parsed, 6, "{s:?}");
    r.ask(|c| {
        let found = c.find_symbol("Square", None, &page()).unwrap();
        assert_eq!(found.results.first().map(|x| x.name.as_str()), Some("alpha_core::shapes::Square"));
        let m = c.find_symbol("area", Some("method"), &page()).unwrap();
        assert!(names(&m).contains(&"alpha_core::shapes::Square::area".to_string()), "{:?}", names(&m));
        assert!(c.find_symbol("should_not_be_indexed", None, &page()).unwrap().results.is_empty());
        // Full text finds words from docs and identifiers.
        let fts = c.find_symbol("anything with an area", None, &page()).unwrap();
        assert!(names(&fts).contains(&"alpha_core::shapes::Shape".to_string()), "{:?}", names(&fts));
        let ts = c.find_symbol("Widget", None, &page()).unwrap();
        assert_eq!(ts.results.first().map(|x| x.kind.as_str()), Some("class"));
        assert!(!ts.freshness.stale);
    });
}

#[test]
fn cross_file_references_callers_imports_and_impls() {
    let r = Repo::new();
    r.index();
    r.ask(|c| {
        let refs = c.find_references("scale", false, &page()).unwrap();
        assert!(refs.results.iter().any(|x| x.path == "src/shapes.rs" && x.provenance == "syntax"), "{refs:?}");
        let callers = c.find_callers("alpha_core::util::scale", false, &page()).unwrap();
        assert_eq!(names(&callers), vec!["alpha_core::shapes::Square::area".to_string()]);
        let callers = c.find_callers("total", false, &page()).unwrap();
        assert!(names(&callers).iter().any(|n| n.ends_with("totals")), "{:?}", names(&callers));
        let deps = c.find_dependencies("src/shapes.rs", &page()).unwrap();
        assert!(deps.results.iter().any(|x| x.rel == "imports" && x.path == "src/util.rs"), "{deps:?}");
        assert!(deps.results.iter().any(|x| x.rel == "imported_by" && x.path == "tests/shapes.rs"), "{deps:?}");
        let pkg = c.find_dependencies("alpha-core", &page()).unwrap();
        assert!(pkg.results.iter().any(|x| x.rel == "depends_on" && x.name == "serde"), "{pkg:?}");
        let ts = c.find_dependencies("web/app.ts", &page()).unwrap();
        assert!(ts.results.iter().any(|x| x.rel == "imports" && x.path == "web/helper.ts"), "{ts:?}");
        let impls = c.find_implementations("Shape", &page()).unwrap();
        assert_eq!(impls.results.first().map(|x| x.name.as_str()), Some("Square"));
        let ctx = c.get_symbol_context("Square::area", 20).unwrap();
        assert!(ctx.text.contains("scale(self.0 * self.0)"), "{}", ctx.text);
        assert!(ctx.text.contains("alpha_core::util::scale"), "{}", ctx.text);
        let impact = c.get_change_impact(&["src/util.rs".to_string()], &page()).unwrap();
        assert!(impact.results.iter().any(|x| x.path == "src/shapes.rs"), "{impact:?}");
        let tc = c.get_task_context("scale the area of shapes", 800).unwrap();
        assert!(tc.text.contains("fn scale"), "{}", tc.text);
        assert!(tc.text.len() <= 800 * 4);
    });
}

#[test]
fn incremental_update_reparses_only_changed_files_and_drops_stale_edges() {
    let r = Repo::new();
    r.index();
    let s = r.index();
    assert_eq!((s.parsed, s.deleted), (0, 0), "a second run with no changes parses nothing: {s:?}");
    // `area` stops calling `scale`: the old edge must disappear.
    r.write(
        "src/shapes.rs",
        "/// Anything with an area.\npub trait Shape {\n    fn area(&self) -> f64;\n}\n\npub struct Square(pub f64);\n\nimpl Shape for Square {\n    fn area(&self) -> f64 {\n        self.0 * self.0\n    }\n}\n",
    );
    r.ask(|c| {
        let stale = c.find_symbol("Square", None, &page()).unwrap();
        assert!(stale.freshness.stale && stale.freshness.changed_files == 1, "{:?}", stale.freshness);
    });
    let s = r.index();
    assert_eq!(s.parsed, 1, "{s:?}");
    r.ask(|c| {
        assert!(c.find_callers("alpha_core::util::scale", false, &page()).unwrap().results.is_empty());
        assert!(c.find_symbol("total", Some("fn"), &page()).unwrap().results.iter().all(|x| x.path != "src/shapes.rs"));
        assert!(!c.find_symbol("Square", None, &page()).unwrap().freshness.stale);
    });
    // Touching a file without changing it costs a hash, not a parse.
    bump_mtime(&r.root.join("src/util.rs"));
    let s = r.index();
    assert_eq!(s.parsed, 0, "{s:?}");
}

#[test]
fn deletion_and_rename_are_reflected() {
    let r = Repo::new();
    r.index();
    git(&r.root, &["mv", "src/util.rs", "src/math.rs"]);
    r.write("src/lib.rs", "pub mod shapes;\npub mod math;\n");
    r.write("src/shapes.rs", "use crate::math::scale;\npub fn twice(x: f64) -> f64 { scale(x) }\n");
    std::fs::remove_file(r.root.join("tests/shapes.rs")).unwrap();
    let s = r.index();
    assert_eq!(s.deleted, 2, "util.rs (renamed away) and tests/shapes.rs: {s:?}");
    r.ask(|c| {
        // Exact matches rank first; full-text matches (bodies mentioning `scale`) follow.
        let found = c.find_symbol("scale", Some("fn"), &page()).unwrap();
        assert_eq!(names(&found).first().map(String::as_str), Some("alpha_core::math::scale"));
        assert!(!names(&found).iter().any(|n| n.contains("util")), "{:?}", names(&found));
        assert!(c.find_symbol("totals", None, &page()).unwrap().results.is_empty());
        let deps = c.find_dependencies("src/shapes.rs", &page()).unwrap();
        assert!(deps.results.iter().any(|x| x.path == "src/math.rs"), "{deps:?}");
    });
}

#[test]
fn branch_switch_and_merge_update_incrementally() {
    let r = Repo::new();
    r.index();
    git(&r.root, &["checkout", "-q", "-b", "feature"]);
    r.write("src/circle.rs", "pub struct Circle(pub f64);\npub fn circle_area(c: &Circle) -> f64 { crate::util::scale(c.0) }\n");
    r.write("src/lib.rs", "pub mod shapes;\npub mod util;\npub mod circle;\n");
    r.commit("circle");
    let s = r.index();
    assert_eq!(s.parsed, 2, "{s:?}");
    git(&r.root, &["checkout", "-q", "main"]);
    // Checkout restores files; mtimes may or may not move, content hashes decide.
    let s = r.index();
    assert_eq!(s.deleted, 1, "{s:?}");
    r.ask(|c| assert!(c.find_symbol("Circle", None, &page()).unwrap().results.is_empty()));
    // Upstream-style merge brings the branch back: only the merged files are parsed.
    r.write("src/util.rs", "/// Scales a value.\npub fn scale(x: f64) -> f64 {\n    x * 3.0\n}\n");
    r.commit("main change");
    git(&r.root, &["merge", "-q", "--no-edit", "feature"]);
    for p in ["src/circle.rs", "src/lib.rs"] {
        bump_mtime(&r.root.join(p));
    }
    let s = r.index();
    assert!(s.parsed <= 3 && !s.full, "merge re-parses only touched files: {s:?}");
    r.ask(|c| {
        let callers = c.find_callers("alpha_core::util::scale", false, &page()).unwrap();
        assert!(names(&callers).contains(&"alpha_core::circle::circle_area".to_string()), "{:?}", names(&callers));
    });
}

#[test]
fn package_rename_reparses_everything_once() {
    let r = Repo::new();
    r.index();
    r.write("Cargo.toml", "[package]\nname = \"beta-core\"\nversion = \"0.1.0\"\n");
    let s = r.index();
    assert!(s.full, "{s:?}");
    r.ask(|c| assert_eq!(names(&c.find_symbol("Square", None, &page()).unwrap()).first().map(String::as_str), Some("beta_core::shapes::Square")));
    assert!(!r.index().full);
}

#[test]
fn retrieval_limits_and_pagination() {
    let r = Repo::new();
    let mut body = String::new();
    for i in 0..300 {
        body.push_str(&format!("pub fn item_{i}() {{ crate::util::scale(1.0); }}\n"));
    }
    r.write("src/many.rs", &body);
    r.index();
    r.ask(|c| {
        let p = Page::new(Some(10_000), None);
        assert_eq!(p.limit, 200);
        let all = c.find_symbol("item", Some("fn"), &Page::new(Some(25), Some(0))).unwrap();
        assert_eq!(all.results.len(), 25);
        assert_eq!(all.next_offset, Some(25));
        let callers = c.find_callers("alpha_core::util::scale", false, &Page::new(Some(10), Some(295))).unwrap();
        assert_eq!(callers.total, 301);
        assert_eq!(callers.results.len(), 6);
        assert_eq!(callers.next_offset, None);
        let tc = c.get_task_context("item scale", 200).unwrap();
        assert!(tc.text.len() <= 200 * 4, "{}", tc.text.len());
    });
}

#[test]
fn ambiguous_and_missing_symbols_are_explained() {
    let r = Repo::new();
    r.write("src/other.rs", "pub fn scale() {}\n");
    r.index();
    r.ask(|c| {
        let amb = c.find_references("scale", false, &page()).unwrap();
        assert_eq!(amb.op, "ambiguous");
        assert_eq!(amb.total, 2);
        let all = c.find_references("scale", true, &page()).unwrap();
        assert_eq!(all.op, "find_references");
        let missing = c.find_callers("no_such_thing", false, &page()).unwrap();
        assert_eq!(missing.op, "not_found");
    });
}

#[test]
fn concurrent_indexing_and_querying() {
    let r = Repo::new();
    r.index();
    let root = r.root.clone();
    std::thread::scope(|s| {
        let writers: Vec<_> = (0..3)
            .map(|i| {
                let root = root.clone();
                s.spawn(move || {
                    for j in 0..5 {
                        let p = root.join(format!("src/gen_{i}.rs"));
                        std::fs::write(&p, format!("pub fn generated_{i}_{j}() {{}}\n")).unwrap();
                        bump_mtime(&p);
                        indexer::update(&root, false).unwrap();
                    }
                })
            })
            .collect();
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let root = root.clone();
                s.spawn(move || {
                    for _ in 0..20 {
                        let db = db::open_read(&root).unwrap();
                        let c = Ctx { db: &db, root: &root };
                        // Every read sees a complete generation: Square and its impl together.
                        let sq = c.find_symbol("Square", None, &page()).unwrap();
                        assert!(!sq.results.is_empty());
                        let im = c.find_implementations("Shape", &page()).unwrap();
                        assert_eq!(im.results.len(), 1);
                    }
                })
            })
            .collect();
        for h in writers.into_iter().chain(readers) {
            h.join().unwrap();
        }
    });
    r.ask(|c| {
        for i in 0..3 {
            let n = names(&c.find_symbol(&format!("generated_{i}_4"), None, &page()).unwrap());
            assert_eq!(n.first().map(String::as_str), Some(format!("alpha_core::gen_{i}::generated_{i}_4").as_str()), "{n:?}");
            assert!(!n.iter().any(|x| x.ends_with("_3")), "earlier generations are gone: {n:?}");
        }
    });
}

#[test]
fn corrupt_index_is_reported_and_rebuilt() {
    let r = Repo::new();
    r.index();
    let path = db::index_path(&r.root);
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    std::fs::write(&path, b"this is not a sqlite database at all, just garbage bytes......").unwrap();
    let err = match db::open_read(&r.root) {
        Ok(_) => panic!("a garbage file must not open"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("corrupt") && err.contains("--rebuild"), "{err}");
    let s = r.index();
    assert_eq!(s.parsed, 6, "{s:?}");
    r.ask(|c| assert!(!c.find_symbol("Square", None, &page()).unwrap().results.is_empty()));
    // Missing index: a clear instruction, not a crash.
    let fresh = Repo::new();
    let err = db::open_read(&fresh.root).err().map(|e| format!("{e:#}")).unwrap_or_default();
    assert!(err.contains("codegraph index"), "{err}");
}

#[test]
fn task_records_are_separate_and_reference_the_graph() {
    let r = Repo::new();
    r.index();
    let c = tasks::open(&r.root).unwrap();
    tasks::create(&c, "S5-003", "Room leases in PostgreSQL").unwrap();
    tasks::add(&c, "S5-003", "symbol", "lease math", Some("alpha_core::util::scale")).unwrap();
    tasks::add(&c, "S5-003", "todo", "write the drain test", None).unwrap();
    tasks::add(&c, "S5-003", "symbol", "gone", Some("alpha_core::nothing")).unwrap();
    assert!(tasks::add(&c, "S5-003", "essay", "x", None).is_err());
    let t = tasks::get(&c, &r.root, "S5-003").unwrap().unwrap();
    let text = tasks::render(&t);
    assert!(text.contains("src/util.rs:2-4"), "{text}");
    assert!(text.contains("not in the current index"), "{text}");
    // Rebuilding the code index keeps task records.
    indexer::update(&r.root, true).unwrap();
    assert_eq!(tasks::list(&c, false).unwrap().len(), 1);
}

#[test]
fn scip_import_resolves_references_and_is_dropped_when_files_change() {
    use protobuf::Message;
    use scip::types::{Document, Index, Occurrence, SymbolRole};
    let r = Repo::new();
    r.index();
    let scale = "rust-analyzer cargo alpha-core 0.1.0 util/scale().";
    let occ = |line: i32, col: i32, len: i32, sym: &str, def: bool| {
        let mut o = Occurrence::new();
        o.range = vec![line, col, col + len];
        o.symbol = sym.to_string();
        if def {
            o.symbol_roles = SymbolRole::Definition as i32;
        }
        o
    };
    let mut util = Document::new();
    util.relative_path = "src/util.rs".into();
    util.occurrences = vec![occ(1, 7, 5, scale, true)];
    let mut shapes = Document::new();
    shapes.relative_path = "src/shapes.rs".into();
    shapes.occurrences = vec![occ(0, 16, 5, scale, false), occ(11, 8, 5, scale, false)];
    let mut index = Index::new();
    index.documents = vec![util, shapes];
    let scip_path = r.root.join(".codegraph/test.scip");
    std::fs::write(&scip_path, index.write_to_bytes().unwrap()).unwrap();
    let seen = {
        let db = db::open_read(&r.root).unwrap();
        scip_import::hashes(&db).unwrap()
    };
    let mut db = db::open_write(&r.root, false).unwrap();
    let st = scip_import::import(&mut db, &scip_path, &seen).unwrap();
    drop(db);
    assert_eq!((st.files_attached, st.definitions_matched, st.refs_written), (2, 1, 2), "{st:?}");
    r.ask(|c| {
        let callers = c.find_callers("alpha_core::util::scale", false, &page()).unwrap();
        assert_eq!(callers.results.first().map(|x| x.provenance.as_str()), Some("scip"), "{callers:?}");
        assert_eq!(names(&callers), vec!["alpha_core::shapes::Square::area".to_string()]);
        assert!(callers.notes.iter().any(|n| n.contains("resolved by SCIP")));
    });
    // shapes.rs changes: its SCIP rows go, its new call shows up as an unresolved name match,
    // and util.rs keeps its semantic identity.
    r.write("src/shapes.rs", "use crate::util::scale;\npub fn double(x: f64) -> f64 {\n    scale(x)\n}\n");
    r.index();
    r.ask(|c| {
        let callers = c.find_callers("alpha_core::util::scale", false, &page()).unwrap();
        assert_eq!(names(&callers), vec!["alpha_core::shapes::double".to_string()]);
        assert_eq!(callers.results.first().map(|x| x.provenance.as_str()), Some("syntax"));
        assert!(callers.notes.iter().any(|n| n.contains("unresolved")), "{:?}", callers.notes);
    });
}
