//! Initial and incremental indexing.
//!
//! One run: take the write lock (`BEGIN IMMEDIATE`), compare the Git-visible source files with
//! the index (size + mtime, then content hash), re-parse only what changed, drop what disappeared,
//! and commit everything as one new generation. Renames are a delete plus an add; branch switches
//! and merges are just more changed files. A changed set of packages (renamed crate, new manifest)
//! changes module paths, so it re-parses every file.

use crate::db::{self, Db};
use crate::inventory::{self, Lang, Package, SourceFile};
use crate::syntax::{self, Facts};
use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{TransactionBehavior, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub generation: i64,
    pub files: usize,
    pub parsed: usize,
    pub added: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped_generated: usize,
    pub full: bool,
    pub symbols_written: usize,
    pub refs_written: usize,
    pub ms: u128,
}

struct Existing {
    id: i64,
    hash: String,
    size: i64,
    mtime: i64,
}

struct Parsed {
    file: SourceFile,
    hash: String,
    module: String,
    is_test: bool,
    facts: Facts,
}

pub fn update(root: &Path, rebuild: bool) -> Result<Stats> {
    let t0 = Instant::now();
    let mut db = db::open_write(root, rebuild)?;
    let stats = update_db(&mut db, root, t0)?;
    Ok(stats)
}

fn update_db(db: &mut Db, root: &Path, t0: Instant) -> Result<Stats> {
    let head = inventory::head_rev(root).unwrap_or_default();
    // Hold the write lock from the start: a second indexer waits here, then finds nothing to do.
    let tx = db.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let all = inventory::git_files(root)?;
    let pkgs = inventory::packages(root, &all);
    let fp = inventory::packages_fingerprint(&pkgs);
    let old_fp: Option<String> = tx.query_row("SELECT value FROM meta WHERE key='packages_fp'", [], |r| r.get(0)).ok();
    let full = old_fp.as_deref() != Some(fp.as_str());
    let sources = inventory::source_files(root, &all);

    let mut existing: HashMap<String, Existing> = HashMap::new();
    {
        let mut st = tx.prepare("SELECT id, path, hash, size, mtime_ns FROM files")?;
        let rows =
            st.query_map([], |r| Ok((r.get::<_, String>(1)?, Existing { id: r.get(0)?, hash: r.get(2)?, size: r.get(3)?, mtime: r.get(4)? })))?;
        for row in rows {
            let (p, e) = row?;
            existing.insert(p, e);
        }
    }

    let mut stats = Stats { files: sources.len(), full, ..Stats::default() };
    let mut seen: HashSet<String> = HashSet::new();
    let mut touched: Vec<(i64, u64, i64)> = Vec::new();
    let mut to_parse: Vec<(SourceFile, String, String)> = Vec::new();
    for f in sources {
        let old = existing.get(&f.path);
        if !full
            && let Some(e) = old
            && e.size == i64::try_from(f.size).unwrap_or(-1)
            && e.mtime == f.mtime_ns
        {
            seen.insert(f.path.clone());
            stats.unchanged += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(root.join(&f.path)) else { continue };
        let Ok(text) = String::from_utf8(bytes) else { continue };
        if inventory::is_generated(&text) {
            stats.skipped_generated += 1;
            continue;
        }
        seen.insert(f.path.clone());
        let h = inventory::hash(text.as_bytes());
        if !full
            && let Some(e) = old
            && e.hash == h
        {
            touched.push((e.id, f.size, f.mtime_ns));
            stats.unchanged += 1;
            continue;
        }
        to_parse.push((f, h, text));
    }
    let deleted: Vec<(String, i64)> = existing.iter().filter(|(p, _)| !seen.contains(*p)).map(|(p, e)| (p.clone(), e.id)).collect();

    let parsed: Vec<Parsed> = to_parse
        .into_par_iter()
        .map(|(file, hash, text)| {
            let pkg = inventory::owning_package(&pkgs, &file.path, file.lang);
            let module = inventory::module_path(pkg, &file.path, file.lang);
            let is_test = inventory::is_test_path(&file.path);
            let facts = syntax::extract(file.lang, &module, is_test, &text);
            Parsed { file, hash, module, is_test, facts }
        })
        .collect();

    let generation: i64 = tx.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='generation'", [], |r| r.get(0)).unwrap_or(0) + 1;

    // Packages are small: replace them wholesale.
    tx.execute("DELETE FROM package_deps", [])?;
    tx.execute("DELETE FROM packages", [])?;
    let mut pkg_ids: HashMap<String, i64> = HashMap::new();
    for p in &pkgs {
        tx.execute(
            "INSERT INTO packages(name, ident, lang, manifest, root) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![p.name, p.ident, p.lang, p.manifest, p.root],
        )?;
        let id = tx.last_insert_rowid();
        pkg_ids.insert(p.name.clone(), id);
        for (dep, kind) in &p.deps {
            tx.execute("INSERT INTO package_deps(package_id, dep, kind) VALUES(?1, ?2, ?3)", params![id, dep, kind])?;
        }
    }

    for (_, id) in &deleted {
        delete_file_rows(&tx, *id, true)?;
    }
    stats.deleted = deleted.len();

    for (id, size, mtime) in &touched {
        tx.execute("UPDATE files SET size=?2, mtime_ns=?3 WHERE id=?1", params![id, i64::try_from(*size).unwrap_or(i64::MAX), mtime])?;
    }

    for p in &parsed {
        let lang = p.file.lang.as_str();
        let size = i64::try_from(p.file.size).unwrap_or(i64::MAX);
        let lines = i64::from(p.facts.lines);
        let (file_id, carried) = match existing.get(&p.file.path) {
            Some(e) => {
                let carried = scip_by_sid(&tx, e.id)?;
                delete_file_rows(&tx, e.id, false)?;
                tx.execute(
                    "UPDATE files SET lang=?2, module=?3, is_test=?4, hash=?5, size=?6, mtime_ns=?7, lines=?8,
                     generation=?9, semantic_hash=NULL WHERE id=?1",
                    params![e.id, lang, p.module, p.is_test, p.hash, size, p.file.mtime_ns, lines, generation],
                )?;
                (e.id, carried)
            }
            None => {
                tx.execute(
                    "INSERT INTO files(path, lang, module, is_test, hash, size, mtime_ns, lines, generation)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![p.file.path, lang, p.module, p.is_test, p.hash, size, p.file.mtime_ns, lines, generation],
                )?;
                stats.added += 1;
                (tx.last_insert_rowid(), HashMap::new())
            }
        };
        let (s, r) = write_facts(&tx, file_id, &p.file.path, p.file.lang, &p.facts, &carried)?;
        stats.symbols_written += s;
        stats.refs_written += r;
    }
    stats.parsed = parsed.len();

    // Package ownership can change for any file when manifests move; recompute it all (cheap).
    {
        let rows: Vec<(i64, String, String)> = {
            let mut st = tx.prepare("SELECT id, path, lang FROM files")?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            v
        };
        let mut st = tx.prepare("UPDATE files SET package_id=?2 WHERE id=?1")?;
        for (id, path, lang) in rows {
            let l = if lang == "rust" { Lang::Rust } else { Lang::Ts };
            let pid = owning(&pkgs, &path, l).and_then(|p| pkg_ids.get(&p.name).copied());
            st.execute(params![id, pid])?;
        }
    }

    let changed = full || !parsed.is_empty() || !deleted.is_empty();
    let set = |k: &str, v: &str| {
        tx.execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![k, v])
    };
    if changed {
        set("generation", &generation.to_string())?;
    }
    set("packages_fp", &fp)?;
    set("indexed_rev", &head)?;
    set("indexed_at", &now_secs().to_string())?;
    tx.commit().context("committing the index generation")?;

    stats.generation = if changed { generation } else { generation - 1 };
    stats.ms = t0.elapsed().as_millis();
    Ok(stats)
}

fn owning<'a>(pkgs: &'a [Package], path: &str, lang: Lang) -> Option<&'a Package> {
    inventory::owning_package(pkgs, path, lang)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// SCIP ids of a file's symbols by stable id, carried over a re-parse: a definition that keeps its
/// qualified name keeps its semantic identity, so references from unchanged files still resolve.
fn scip_by_sid(tx: &rusqlite::Transaction, file_id: i64) -> Result<HashMap<String, i64>> {
    let mut st = tx.prepare("SELECT sid, scip FROM symbols WHERE file_id=?1 AND scip IS NOT NULL")?;
    let v = st.query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<HashMap<_, _>, _>>()?;
    Ok(v)
}

/// Removes a file's derived rows (and the file itself when `and_file`).
pub fn delete_file_rows(tx: &rusqlite::Connection, file_id: i64, and_file: bool) -> Result<()> {
    tx.execute("DELETE FROM symbols_fts WHERE rowid IN (SELECT id FROM symbols WHERE file_id=?1)", [file_id])?;
    for t in ["symbols", "refs", "imports", "impls"] {
        tx.execute(&format!("DELETE FROM {t} WHERE file_id=?1"), [file_id])?;
    }
    if and_file {
        tx.execute("DELETE FROM files WHERE id=?1", [file_id])?;
    }
    Ok(())
}

fn write_facts(
    tx: &rusqlite::Transaction,
    file_id: i64,
    path: &str,
    lang: Lang,
    f: &Facts,
    carried: &HashMap<String, i64>,
) -> Result<(usize, usize)> {
    let mut ids: Vec<i64> = Vec::with_capacity(f.symbols.len());
    let mut sid_counts: HashMap<String, u32> = HashMap::new();
    let mut ins = tx.prepare(
        "INSERT INTO symbols(file_id, sid, name, qualname, kind, parent_id, start_line, end_line, name_line, name_col,
         sig, doc, is_test, scip) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?;
    let mut fts = tx.prepare("INSERT INTO symbols_fts(rowid, name, words, qualname, sig, doc, path, body) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
    let path_words = syntax::words(path);
    for s in &f.symbols {
        let base = format!("{}:{}", lang.as_str(), s.qualname);
        let n = sid_counts.entry(base.clone()).or_insert(0);
        *n += 1;
        let sid = if *n == 1 { base } else { format!("{base}#{n}") };
        let parent = s.parent.and_then(|i| ids.get(i).copied());
        let scip = carried.get(&sid);
        ins.execute(params![
            file_id,
            sid,
            s.name,
            s.qualname,
            s.kind,
            parent,
            s.start_line,
            s.end_line,
            s.name_line,
            s.name_col,
            s.sig,
            s.doc,
            s.is_test,
            scip
        ])?;
        let id = tx.last_insert_rowid();
        ids.push(id);
        let w = format!("{} {}", syntax::words(&s.name), syntax::words(&s.qualname));
        fts.execute(params![id, s.name, w, s.qualname, s.sig, s.doc, path_words, s.body])?;
    }
    let mut rs = tx.prepare(
        "INSERT INTO refs(file_id, line, col, name, kind, enclosing_id, target, provenance) VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL, 'syntax')",
    )?;
    for r in &f.refs {
        let enc = r.enclosing.and_then(|i| ids.get(i).copied());
        rs.execute(params![file_id, r.line, r.col, r.name, r.kind, enc])?;
    }
    let mut is = tx.prepare("INSERT INTO imports(file_id, line, path) VALUES(?1, ?2, ?3)")?;
    for i in &f.imports {
        is.execute(params![file_id, i.line, i.path])?;
    }
    let mut im = tx.prepare("INSERT INTO impls(file_id, symbol_id, trait_name, trait_path, type_name, line) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")?;
    for i in &f.impls {
        if let Some(sym) = ids.get(i.symbol) {
            im.execute(params![file_id, sym, i.trait_name, i.trait_path, i.type_name, i.line])?;
        }
    }
    Ok((f.symbols.len(), f.refs.len()))
}
