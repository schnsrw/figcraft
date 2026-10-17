//! `codegraph bench`: measured numbers, not estimates of agent savings.
//!
//! 1. Indexing: a full build and incremental updates in a throwaway `git worktree` of HEAD (the
//!    real index is not touched).
//! 2. Retrieval: the same questions answered by `git grep` (what an agent runs without the graph)
//!    and by the graph, comparing latency and the bytes each returns (tokens ≈ bytes / 4).
//! 3. Accuracy: syntax name matches and `git grep -w` lines scored against SCIP references
//!    (rust-analyzer) on the same symbols, when the real index has semantic data.

use crate::db;
use crate::indexer;
use crate::query::{self, Ctx, Page};
use anyhow::{Context, Result, bail};
use rusqlite::params;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

fn median_ms<F: FnMut() -> Result<usize>>(runs: usize, mut f: F) -> Result<(f64, usize)> {
    let mut times = Vec::with_capacity(runs);
    let mut size = 0;
    for _ in 0..runs {
        let t = Instant::now();
        size = f()?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok((times.get(times.len() / 2).copied().unwrap_or(0.0), size))
}

fn git_grep(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(root).arg("grep").args(args).output().context("git grep")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn tok(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

fn dir_size(p: &Path) -> u64 {
    ["db", "db-wal"].iter().filter_map(|e| std::fs::metadata(p.with_extension(e)).ok()).map(|m| m.len()).sum()
}

pub fn run(root: &Path, symbols: &[String]) -> Result<String> {
    let mut md = String::new();
    let head = crate::inventory::head_rev(root).unwrap_or_default();
    writeln!(
        md,
        "# codegraph benchmark\n\nRepository HEAD `{}`; machine-local wall-clock times (median of 5 where repeated).\n",
        head.chars().take(12).collect::<String>()
    )?;
    indexing(root, &mut md)?;
    let db = db::open_read(root).context("the retrieval benchmark uses the real index: run `codegraph index` first")?;
    let ctx = Ctx { db: &db, root };
    let chosen = choose(&ctx, symbols)?;
    if chosen.is_empty() {
        bail!("no symbols to benchmark");
    }
    retrieval(&ctx, &chosen, &mut md)?;
    accuracy(&ctx, &chosen, &mut md)?;
    Ok(md)
}

fn indexing(root: &Path, md: &mut String) -> Result<()> {
    let wt = db::index_dir(root).join("bench-worktree");
    let _ = Command::new("git").arg("-C").arg(root).args(["worktree", "remove", "--force"]).arg(&wt).output();
    let ok = Command::new("git").arg("-C").arg(root).args(["worktree", "add", "-q", "--detach"]).arg(&wt).arg("HEAD").status()?;
    if !ok.success() {
        bail!("git worktree add failed");
    }
    let result = (|| -> Result<()> {
        let full = indexer::update(&wt, true)?;
        let size = dir_size(&db::index_path(&wt));
        let noop = indexer::update(&wt, false)?;
        let rs: Vec<String> = {
            let d = db::open_read(&wt)?;
            let mut st = d.conn.prepare("SELECT path FROM files WHERE lang = 'rust' ORDER BY lines DESC LIMIT 10")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            v
        };
        let touch = |paths: &[String]| -> Result<()> {
            for p in paths {
                let f = wt.join(p);
                let mut t = std::fs::read_to_string(&f)?;
                t.push_str("\n// codegraph bench edit\n");
                std::fs::write(&f, t)?;
            }
            Ok(())
        };
        touch(rs.get(..1).unwrap_or(&[]))?;
        let one = indexer::update(&wt, false)?;
        touch(&rs)?;
        let ten = indexer::update(&wt, false)?;
        if let Some(p) = rs.first() {
            std::fs::remove_file(wt.join(p))?;
        }
        let del = indexer::update(&wt, false)?;
        writeln!(md, "## Indexing (fresh worktree of HEAD, syntax layer)\n")?;
        writeln!(md, "| Run | Files parsed | Time |\n|---|---:|---:|")?;
        writeln!(
            md,
            "| Initial full index ({} files, {} symbols, {} refs) | {} | {} ms |",
            full.files, full.symbols_written, full.refs_written, full.parsed, full.ms
        )?;
        writeln!(md, "| No changes | {} | {} ms |", noop.parsed, noop.ms)?;
        writeln!(md, "| 1 file edited (largest file) | {} | {} ms |", one.parsed, one.ms)?;
        writeln!(md, "| 10 files edited (largest files) | {} | {} ms |", ten.parsed, ten.ms)?;
        writeln!(md, "| 1 file deleted | {} parsed, {} deleted | {} ms |", del.parsed, del.deleted, del.ms)?;
        writeln!(md, "\nDatabase after the initial index: {:.1} MiB (syntax only).\n", size as f64 / 1_048_576.0)?;
        Ok(())
    })();
    let _ = Command::new("git").arg("-C").arg(root).args(["worktree", "remove", "--force"]).arg(&wt).output();
    result
}

struct Chosen {
    name: String,
    sid: String,
    path: String,
    scip: Option<i64>,
}

/// Given symbols, or a deterministic spread of non-test functions, methods and types with unique names.
fn choose(ctx: &Ctx, names: &[String]) -> Result<Vec<Chosen>> {
    let conn = &ctx.db.conn;
    let mut out = Vec::new();
    if !names.is_empty() {
        for n in names {
            if let Some(s) = ctx.resolve(n)?.into_iter().next() {
                out.push(Chosen { name: s.name, sid: s.sid, path: s.path, scip: s.scip });
            }
        }
        return Ok(out);
    }
    let mut st = conn.prepare(
        "SELECT s.name, s.sid, f.path, s.scip FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.is_test = 0 AND s.kind IN ('fn','method','struct','enum','trait') AND length(s.name) >= 6
           AND f.lang = 'rust' AND f.path NOT LIKE 'tools/%' AND (SELECT count(*) FROM symbols o WHERE o.name = s.name) = 1
           AND (SELECT count(*) FROM refs r WHERE r.name = s.name AND r.provenance = 'syntax') >= 2
         ORDER BY (s.id * 2654435761) % 1000003 LIMIT 12",
    )?;
    let v = st.query_map([], |r| Ok(Chosen { name: r.get(0)?, sid: r.get(1)?, path: r.get(2)?, scip: r.get(3)? }))?.collect::<Result<Vec<_>, _>>()?;
    out.extend(v);
    Ok(out)
}

fn retrieval(ctx: &Ctx, chosen: &[Chosen], md: &mut String) -> Result<()> {
    writeln!(md, "## Retrieval: git grep vs graph ({} symbols)\n", chosen.len())?;
    writeln!(
        md,
        "Each row is the same question asked both ways. Bytes are what the agent would read back; \
         tokens are estimated at 4 bytes per token. This measures tool output only, not end-to-end \
         agent token use.\n"
    )?;
    writeln!(md, "| Task | grep: median ms | grep: tokens (median) | graph: median ms | graph: tokens (median) |\n|---|---:|---:|---:|---:|")?;
    let page = Page::new(Some(20), None);
    type Row = (Vec<f64>, Vec<usize>, Vec<f64>, Vec<usize>);
    let mut rows: Vec<(&str, Row)> = vec![
        ("Find the definition", Default::default()),
        ("Find callers / uses", Default::default()),
        ("Understand a symbol (grep + read the defining file vs symbol context)", Default::default()),
        ("Change impact of the defining file", Default::default()),
    ];
    for c in chosen {
        let word = c.name.as_str();
        let defpat = format!("(fn|struct|enum|trait) {word}([^A-Za-z0-9_]|$)");
        let (g0, gs0) = median_ms(5, || Ok(git_grep(ctx.root, &["-n", "-E", &defpat])?.len()))?;
        let (q0, qs0) = median_ms(5, || Ok(query::render(&ctx.find_symbol(word, None, &Page::new(Some(5), None))?).len()))?;
        let (g1, gs1) = median_ms(5, || Ok(git_grep(ctx.root, &["-n", "-w", word])?.len()))?;
        let (q1, qs1) = median_ms(5, || Ok(query::render(&ctx.find_callers(&c.sid, false, &page)?).len()))?;
        let file_bytes = std::fs::metadata(ctx.root.join(&c.path)).map(|m| m.len() as usize).unwrap_or(0);
        let (q2, qs2) = median_ms(5, || Ok(query::render(&ctx.get_symbol_context(&c.sid, 40)?).len()))?;
        let module = c.path.rsplit('/').next().and_then(|f| f.split('.').next()).unwrap_or(word).to_string();
        let (g3, gs3) = median_ms(5, || Ok(git_grep(ctx.root, &["-n", "-w", &module])?.len()))?;
        let (q3, qs3) = median_ms(5, || Ok(query::render(&ctx.get_change_impact(std::slice::from_ref(&c.path), &page)?).len()))?;
        let push = |r: &mut Row, g: f64, gs: usize, q: f64, qs: usize| {
            r.0.push(g);
            r.1.push(tok(gs));
            r.2.push(q);
            r.3.push(tok(qs));
        };
        if let Some((_, r)) = rows.get_mut(0) {
            push(r, g0, gs0, q0, qs0);
        }
        if let Some((_, r)) = rows.get_mut(1) {
            push(r, g1, gs1, q1, qs1);
        }
        if let Some((_, r)) = rows.get_mut(2) {
            push(r, g0, gs0 + file_bytes, q2, qs2);
        }
        if let Some((_, r)) = rows.get_mut(3) {
            push(r, g3, gs3, q3, qs3);
        }
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let medu = |v: &mut Vec<usize>| {
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0)
    };
    for (name, (mut a, mut b, mut c, mut d)) in rows {
        writeln!(md, "| {name} | {:.1} | {} | {:.1} | {} |", med(&mut a), medu(&mut b), med(&mut c), medu(&mut d))?;
    }
    let (tq, ts) = median_ms(5, || {
        let words: Vec<String> = chosen.iter().take(3).map(|c| crate::syntax::words(&c.name)).collect();
        Ok(ctx.get_task_context(&words.join(" "), 2000)?.text.len())
    })?;
    writeln!(md, "\n`get_task_context` (2,000-token budget): median {tq:.1} ms, {} tokens returned.\n", tok(ts))?;
    writeln!(md, "Symbols: {}\n", chosen.iter().map(|c| format!("`{}`", c.name)).collect::<Vec<_>>().join(", "))?;
    Ok(())
}

fn accuracy(ctx: &Ctx, chosen: &[Chosen], md: &mut String) -> Result<()> {
    let conn = &ctx.db.conn;
    let semantic: Vec<&Chosen> = chosen.iter().filter(|c| c.scip.is_some()).collect();
    writeln!(md, "## Reference accuracy against SCIP (rust-analyzer)\n")?;
    if semantic.is_empty() {
        writeln!(md, "Skipped: the index has no SCIP data for these symbols (run `codegraph semantic`).\n")?;
        return Ok(());
    }
    let (mut tp_s, mut n_s, mut tp_g, mut n_g, mut truth) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for c in &semantic {
        let Some(scip) = c.scip else { continue };
        // Truth: SCIP reference lines in semantically current files, minus `use` lines (the syntax
        // layer records imports separately, not as references).
        let mut st = conn.prepare(
            "SELECT f.path, r.line FROM refs r JOIN files f ON f.id = r.file_id WHERE r.target = ?1 AND r.provenance = 'scip'
             AND NOT EXISTS (SELECT 1 FROM imports i WHERE i.file_id = r.file_id AND i.line = r.line)",
        )?;
        let s: HashSet<(String, i64)> = st.query_map(params![scip], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        let mut st = conn.prepare(
            "SELECT f.path, r.line FROM refs r JOIN files f ON f.id = r.file_id WHERE r.name = ?1 AND r.provenance = 'syntax'
             AND f.semantic_hash = f.hash",
        )?;
        let x: HashSet<(String, i64)> = st.query_map(params![c.name], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        let current: HashSet<String> = {
            let mut st = conn.prepare("SELECT path FROM files WHERE semantic_hash = hash")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            v
        };
        let defs: HashSet<(String, i64)> = {
            let mut st = conn.prepare("SELECT f.path, s.name_line FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.name = ?1")?;
            let v = st.query_map([&c.name], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            v
        };
        let g: HashSet<(String, i64)> = git_grep(ctx.root, &["-n", "-w", &c.name, "--", "*.rs"])?
            .lines()
            .filter_map(|l| {
                let mut it = l.splitn(3, ':');
                Some((it.next()?.to_string(), it.next()?.parse().ok()?))
            })
            .filter(|(p, l)| current.contains(p) && !defs.contains(&(p.clone(), *l)))
            .filter(|(p, l)| {
                conn.query_row(
                    "SELECT count(*) = 0 FROM imports i JOIN files f ON f.id = i.file_id WHERE f.path = ?1 AND i.line = ?2",
                    params![p, l],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(true)
            })
            .collect();
        truth += s.len();
        tp_s += x.intersection(&s).count();
        n_s += x.len();
        tp_g += g.intersection(&s).count();
        n_g += g.len();
    }
    let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    writeln!(md, "Pooled over {} symbols with SCIP data; {} SCIP reference lines are the reference set.\n", semantic.len(), truth)?;
    writeln!(md, "| Method | Lines returned | Precision | Recall |\n|---|---:|---:|---:|")?;
    writeln!(md, "| SCIP (graph, resolved) | {truth} | 100% (reference) | 100% (reference) |")?;
    writeln!(md, "| Syntax name matches (graph without SCIP) | {n_s} | {:.1}% | {:.1}% |", pct(tp_s, n_s), pct(tp_s, truth))?;
    writeln!(md, "| `git grep -w name` | {n_g} | {:.1}% | {:.1}% |", pct(tp_g, n_g), pct(tp_g, truth))?;
    let (defs_matched, defs_total): (i64, i64) = conn.query_row(
        "SELECT coalesce(sum(s.scip IS NOT NULL), 0), count(*) FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE f.semantic_hash = f.hash AND s.kind NOT IN ('impl')",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    writeln!(
        md,
        "\nSyntax definitions with a matching SCIP definition (same name and line), in SCIP-covered files: {defs_matched} of {defs_total} ({:.1}%).\n",
        pct(usize::try_from(defs_matched).unwrap_or(0), usize::try_from(defs_total).unwrap_or(0))
    )?;
    Ok(())
}
