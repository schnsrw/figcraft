//! Semantic layer: imports a SCIP index (rust-analyzer `scip`, or `scip-typescript`) on top of the
//! syntax graph.
//!
//! SCIP data is only attached to files whose content hash still equals the hash the SCIP run saw;
//! a file that changes later loses its SCIP rows on re-parse (`semantic_hash` goes NULL), so the
//! index never presents resolved references for code that SCIP did not see. Only references to
//! symbols defined inside the repository are kept (references into std and dependencies are not
//! what an agent navigates by, and they would dominate the database).

use crate::db::{self, Db};
use crate::indexer;
use anyhow::{Context, Result, bail};
use protobuf::Message;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use scip::types::{Index, SymbolRole};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Default, Serialize)]
pub struct SemanticStats {
    pub documents: usize,
    pub files_attached: usize,
    pub files_skipped_changed: usize,
    pub definitions_matched: usize,
    pub refs_written: usize,
    pub tool: String,
    pub ms: u128,
}

/// File path -> content hash, as indexed now.
pub fn hashes(db: &Db) -> Result<HashMap<String, String>> {
    let mut st = db.conn.prepare("SELECT path, hash FROM files")?;
    let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<HashMap<_, _>, _>>()?;
    Ok(v)
}

/// Runs rust-analyzer's SCIP export over the repository into `out`.
pub fn run_rust_analyzer(root: &Path, out: &Path) -> Result<()> {
    // rust-analyzer prints a line per range it could not map; keep that out of the way and show
    // only the tail when the run fails.
    let out = Command::new("rust-analyzer")
        .arg("scip")
        .arg(root)
        .arg("--output")
        .arg(out)
        .current_dir(root)
        .output()
        .context("running `rust-analyzer scip` (install it with `rustup component add rust-analyzer`)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(15).collect();
        bail!("rust-analyzer scip failed ({}):\n{}", out.status, tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    Ok(())
}

/// The full semantic pass: sync the syntax index, run SCIP (unless a ready file is given), sync
/// again, and import SCIP data for every file that did not change in between.
pub fn semantic(root: &Path, scip_file: Option<&Path>) -> Result<SemanticStats> {
    let t0 = Instant::now();
    indexer::update(root, false)?;
    let before = {
        let db = db::open_read(root)?;
        hashes(&db)?
    };
    let tmp;
    let mut seen = before;
    let path = match scip_file {
        Some(p) => {
            // A SCIP file made elsewhere cannot say which content it saw: trust it only for files
            // not modified after it was written.
            let made = std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            let db = db::open_read(root)?;
            let mut st = db.conn.prepare("SELECT path FROM files WHERE mtime_ns > ?1")?;
            let newer: Vec<String> = st.query_map([made], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for n in newer {
                seen.remove(&n);
            }
            p
        }
        None => {
            tmp = db::index_dir(root).join("rust-analyzer.scip");
            run_rust_analyzer(root, &tmp)?;
            tmp.as_path()
        }
    };
    indexer::update(root, false)?;
    let mut db = db::open_write(root, false)?;
    let mut stats = import(&mut db, path, &seen)?;
    stats.ms = t0.elapsed().as_millis();
    Ok(stats)
}

fn descriptor_name(symbol: &str, cache: &mut HashMap<String, Option<String>>) -> Option<String> {
    if let Some(v) = cache.get(symbol) {
        return v.clone();
    }
    let name = scip::symbol::parse_symbol(symbol).ok().and_then(|s| s.descriptors.last().map(|d| d.name.clone())).filter(|n| !n.is_empty());
    cache.insert(symbol.to_string(), name.clone());
    name
}

/// Imports `scip_path`, attaching data to files whose current hash equals `seen_hashes[path]`
/// (the content the SCIP run saw).
pub fn import(db: &mut Db, scip_path: &Path, seen_hashes: &HashMap<String, String>) -> Result<SemanticStats> {
    let bytes = std::fs::read(scip_path).with_context(|| format!("reading {}", scip_path.display()))?;
    let index = Index::parse_from_bytes(&bytes).context("parsing the SCIP index")?;
    drop(bytes);
    let mut stats = SemanticStats { documents: index.documents.len(), ..SemanticStats::default() };
    if let Some(m) = index.metadata.as_ref()
        && let Some(t) = m.tool_info.as_ref()
    {
        stats.tool = format!("{} {}", t.name, t.version);
    }
    let def_role = SymbolRole::Definition as i32;

    // Symbols defined inside the repository: the only reference targets worth keeping.
    let mut local_defs: HashSet<&str> = HashSet::new();
    for d in &index.documents {
        for o in &d.occurrences {
            if o.symbol_roles & def_role != 0 && !o.symbol.starts_with("local ") {
                local_defs.insert(o.symbol.as_str());
            }
        }
    }

    let tx = db.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut name_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut sym_ids: HashMap<String, i64> = HashMap::new();
    for doc in &index.documents {
        let path = doc.relative_path.trim_start_matches("./").to_string();
        let row: Option<(i64, String)> =
            tx.query_row("SELECT id, hash FROM files WHERE path=?1", [&path], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((file_id, hash)) = row else { continue };
        if seen_hashes.get(&path) != Some(&hash) {
            stats.files_skipped_changed += 1;
            continue;
        }
        tx.execute("DELETE FROM refs WHERE file_id=?1 AND provenance='scip'", [file_id])?;
        tx.execute("UPDATE symbols SET scip=NULL WHERE file_id=?1", [file_id])?;
        // (id, name, name_line, name_col, start_line, end_line)
        let syms: Vec<(i64, String, i64, i64, i64, i64)> = {
            let mut st = tx.prepare("SELECT id, name, name_line, name_col, start_line, end_line FROM symbols WHERE file_id=?1")?;
            let v = st
                .query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            v
        };
        let enclosing = |line: i64| -> Option<i64> { syms.iter().filter(|s| s.4 <= line && line <= s.5).min_by_key(|s| s.5 - s.4).map(|s| s.0) };
        let mut ins = tx.prepare(
            "INSERT INTO refs(file_id, line, col, name, kind, enclosing_id, target, provenance)
             VALUES(?1, ?2, ?3, ?4, 'ref', ?5, ?6, 'scip')",
        )?;
        let mut set_scip = tx.prepare("UPDATE symbols SET scip=?2 WHERE id=?1")?;
        for o in &doc.occurrences {
            if o.symbol.is_empty() || o.symbol.starts_with("local ") {
                continue;
            }
            let (Some(l), Some(c)) = (o.range.first(), o.range.get(1)) else { continue };
            let line = i64::from(*l) + 1;
            let col = i64::from(*c);
            let is_def = o.symbol_roles & def_role != 0;
            if !is_def && !local_defs.contains(o.symbol.as_str()) {
                continue;
            }
            let Some(name) = descriptor_name(&o.symbol, &mut name_cache) else { continue };
            let sym_id = match sym_ids.get(&o.symbol) {
                Some(id) => *id,
                None => {
                    tx.execute("INSERT OR IGNORE INTO scip_syms(symbol) VALUES(?1)", [&o.symbol])?;
                    let id: i64 = tx.query_row("SELECT id FROM scip_syms WHERE symbol=?1", [&o.symbol], |r| r.get(0))?;
                    sym_ids.insert(o.symbol.clone(), id);
                    id
                }
            };
            if is_def {
                let best = syms.iter().filter(|s| s.2 == line && s.1 == name).min_by_key(|s| (s.3 - col).abs()).map(|s| s.0);
                if let Some(id) = best {
                    set_scip.execute(params![id, sym_id])?;
                    stats.definitions_matched += 1;
                }
            } else {
                ins.execute(params![file_id, line, col, name, enclosing(line), sym_id])?;
                stats.refs_written += 1;
            }
        }
        // A SCIP reference at a syntax call site is a call.
        tx.execute(
            "UPDATE refs SET kind='call' WHERE file_id=?1 AND provenance='scip' AND EXISTS (
               SELECT 1 FROM refs s WHERE s.file_id=refs.file_id AND s.provenance='syntax'
               AND s.line=refs.line AND s.name=refs.name AND s.kind IN ('call','method'))",
            [file_id],
        )?;
        tx.execute("UPDATE files SET semantic_hash=hash WHERE id=?1", [file_id])?;
        stats.files_attached += 1;
    }
    let rev: String = tx.query_row("SELECT value FROM meta WHERE key='indexed_rev'", [], |r| r.get(0)).unwrap_or_default();
    for (k, v) in [("semantic_rev", rev), ("semantic_at", indexer::now_secs().to_string()), ("semantic_tool", stats.tool.clone())] {
        tx.execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![k, v])?;
    }
    tx.commit()?;
    Ok(stats)
}
