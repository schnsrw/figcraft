//! Read-only retrieval over the graph. Every response carries index freshness, is bounded and
//! paginated, and labels each relationship with its provenance: `scip` (resolved by a semantic
//! indexer) or `syntax` (a name match that may refer to another symbol of the same name).

use crate::db::Db;
use crate::inventory;
use crate::syntax;
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;

/// Names too common for name-matching to say anything about impact.
const COMMON_NAMES: &[&str] = &[
    "new",
    "default",
    "from",
    "into",
    "fmt",
    "clone",
    "run",
    "get",
    "set",
    "len",
    "is_empty",
    "iter",
    "next",
    "build",
    "parse",
    "eq",
    "hash",
    "drop",
    "deref",
    "as_ref",
    "to_string",
    "push",
    "insert",
    "remove",
    "contains",
    "map",
    "with",
    "id",
    "name",
    "value",
    "update",
    "apply",
    "init",
    "test",
    "main",
    "write",
    "read",
    "open",
    "close",
    "unwrap",
    "expect",
    "ok",
    "err",
    "map_err",
    "and_then",
    "unwrap_or",
    "collect",
    "as_str",
    "to_owned",
    "begin",
    "commit",
    "query",
    "bind",
    "execute",
    "fetch_one",
    "fetch_optional",
    "fetch_all",
    "send",
    "recv",
    "lock",
    "spawn",
];

#[derive(Debug, Serialize, Clone)]
pub struct Freshness {
    pub generation: i64,
    pub indexed_rev: String,
    pub head_rev: String,
    pub indexed_at: u64,
    /// Source files that differ on disk from the index (added, modified or deleted).
    pub changed_files: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed_sample: Vec<String>,
    pub stale: bool,
    /// SCIP coverage: `none`, or `<current>/<rust files> files (<tool>)`.
    pub semantic: String,
}

#[derive(Debug, Serialize, Default, Clone)]
pub struct Row {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub rel: String,
    pub path: String,
    pub lines: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provenance: String,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub op: String,
    pub query: String,
    pub freshness: Freshness,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub results: Vec<Row>,
    /// Free text (symbol context, task context).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub limit: usize,
    pub offset: usize,
}

impl Page {
    pub fn new(limit: Option<usize>, offset: Option<usize>) -> Page {
        Page { limit: limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), offset: offset.unwrap_or(0) }
    }
}

#[derive(Debug, Clone)]
pub struct Sym {
    pub id: i64,
    pub sid: String,
    pub name: String,
    pub qualname: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub sig: String,
    pub doc: String,
    pub is_test: bool,
    pub scip: Option<i64>,
    pub parent_id: Option<i64>,
    pub path: String,
    pub file_lang: String,
    pub semantic_current: bool,
}

const SYM_COLS: &str = "s.id, s.sid, s.name, s.qualname, s.kind, s.start_line, s.end_line, s.sig, s.doc, s.is_test, s.scip,
    s.parent_id, f.path, f.lang, (f.semantic_hash IS NOT NULL AND f.semantic_hash = f.hash)";

fn sym_from_row(r: &rusqlite::Row) -> rusqlite::Result<Sym> {
    Ok(Sym {
        id: r.get(0)?,
        sid: r.get(1)?,
        name: r.get(2)?,
        qualname: r.get(3)?,
        kind: r.get(4)?,
        start_line: r.get(5)?,
        end_line: r.get(6)?,
        sig: r.get(7)?,
        doc: r.get(8)?,
        is_test: r.get(9)?,
        scip: r.get(10)?,
        parent_id: r.get(11)?,
        path: r.get(12)?,
        file_lang: r.get(13)?,
        semantic_current: r.get(14)?,
    })
}

fn lines(a: i64, b: i64) -> String {
    if a == b { a.to_string() } else { format!("{a}-{b}") }
}

fn sym_row(s: &Sym, rel: &str) -> Row {
    Row {
        rel: rel.to_string(),
        path: s.path.clone(),
        lines: lines(s.start_line, s.end_line),
        kind: s.kind.clone(),
        name: s.qualname.clone(),
        sid: s.sid.clone(),
        detail: s.sig.clone(),
        provenance: String::new(),
    }
}

pub struct Ctx<'a> {
    pub db: &'a Db,
    pub root: &'a Path,
}

impl Ctx<'_> {
    fn conn(&self) -> &rusqlite::Connection {
        &self.db.conn
    }

    // ------------------------------------------------------------------------------ freshness

    pub fn freshness(&self) -> Result<Freshness> {
        let meta = |k: &str| self.db.meta(k).ok().flatten().unwrap_or_default();
        let indexed_rev = meta("indexed_rev");
        let head_rev = inventory::head_rev(self.root).unwrap_or_default();
        let mut indexed: HashMap<String, (i64, i64, String)> = HashMap::new();
        {
            let mut st = self.conn().prepare("SELECT path, size, mtime_ns, hash FROM files")?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))? {
                let (p, v) = r?;
                indexed.insert(p, v);
            }
        }
        let disk = inventory::disk_state(self.root)?;
        let mut changed: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for (p, (size, mtime)) in &disk {
            match indexed.get(p) {
                Some((s, m, _)) if *s == i64::try_from(*size).unwrap_or(-1) && *m == *mtime => {}
                Some((_, _, h)) => {
                    // Metadata moved; only a content change makes the index stale.
                    checked += 1;
                    let same = checked <= 500 && std::fs::read(self.root.join(p)).map(|b| inventory::hash(&b) == *h).unwrap_or(false);
                    if !same {
                        changed.push(p.clone());
                    }
                }
                None => {
                    // New file, unless it is one the indexer deliberately skips (generated).
                    checked += 1;
                    let generated =
                        checked <= 500 && std::fs::read_to_string(self.root.join(p)).map(|t| inventory::is_generated(&t)).unwrap_or(false);
                    if !generated {
                        changed.push(p.clone());
                    }
                }
            }
        }
        for p in indexed.keys() {
            if !disk.contains_key(p) {
                changed.push(p.clone());
            }
        }
        changed.sort();
        let (current, rust_total): (i64, i64) = self.conn().query_row(
            "SELECT coalesce(sum(semantic_hash IS NOT NULL AND semantic_hash = hash), 0), count(*) FROM files WHERE lang='rust'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let semantic = if current == 0 {
            "none (syntax only; run `codegraph semantic` for resolved references)".to_string()
        } else {
            format!("{current}/{rust_total} rust files current ({})", meta("semantic_tool"))
        };
        Ok(Freshness {
            generation: self.db.generation()?,
            indexed_rev: indexed_rev.chars().take(12).collect(),
            head_rev: head_rev.chars().take(12).collect(),
            indexed_at: meta("indexed_at").parse().unwrap_or(0),
            changed_files: changed.len(),
            stale: !changed.is_empty(),
            changed_sample: changed.into_iter().take(5).collect(),
            semantic,
        })
    }

    fn respond(&self, op: &str, query: &str, page: &Page, total: usize, results: Vec<Row>, mut notes: Vec<String>) -> Result<Response> {
        let freshness = self.freshness()?;
        if freshness.stale {
            notes.insert(
                0,
                format!(
                    "index is stale: {} source file(s) changed since generation {} (run `codegraph index`)",
                    freshness.changed_files, freshness.generation
                ),
            );
        }
        let next = page.offset + results.len();
        Ok(Response {
            op: op.to_string(),
            query: query.to_string(),
            freshness,
            total,
            offset: page.offset,
            limit: page.limit,
            next_offset: (next < total).then_some(next),
            notes,
            results,
            text: String::new(),
        })
    }

    fn paginate<T: Clone>(v: &[T], page: &Page) -> Vec<T> {
        v.iter().skip(page.offset).take(page.limit).cloned().collect()
    }

    // ------------------------------------------------------------------------------ resolution

    /// Symbols a query names: a sid (`rust:a::b`), a qualified name or suffix (`Type::method`), or
    /// a bare name. Impl blocks only match by sid or qualified name.
    pub fn resolve(&self, q: &str) -> Result<Vec<Sym>> {
        let q = q.trim();
        if q.is_empty() {
            bail!("empty symbol query");
        }
        let run = |sql: &str, arg: &str| -> Result<Vec<Sym>> {
            let sql = format!(
                "SELECT {SYM_COLS} FROM symbols s JOIN files f ON f.id = s.file_id WHERE {sql}
                 ORDER BY s.is_test, CASE s.kind WHEN 'impl' THEN 2 WHEN 'variant' THEN 1 ELSE 0 END, f.path, s.start_line LIMIT 50"
            );
            let mut st = self.conn().prepare(&sql)?;
            let v = st.query_map([arg], sym_from_row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        };
        if q.starts_with("rust:") || q.starts_with("ts:") {
            return run("s.sid = ?1", q);
        }
        if q.contains("::") || q.contains('/') {
            let exact = run("s.qualname = ?1", q)?;
            if !exact.is_empty() {
                return Ok(exact);
            }
            let suffix = format!("%::{}", q.replace('%', "").replace('_', "\\_"));
            let by_suffix = run("s.qualname LIKE ?1 ESCAPE '\\'", &suffix)?;
            if !by_suffix.is_empty() || !q.contains("::") {
                return Ok(by_suffix);
            }
            // `krate::Name` for an item defined deeper and re-exported: same crate, same name.
            let (first, last) = (q.split("::").next().unwrap_or(""), q.rsplit("::").next().unwrap_or(""));
            let pattern = format!("{}::%::{}", first.replace('_', "\\_"), last.replace('_', "\\_"));
            return run("s.qualname LIKE ?1 ESCAPE '\\' AND s.kind != 'impl'", &pattern);
        }
        let exact = run("s.name = ?1 AND s.kind != 'impl'", q)?;
        if !exact.is_empty() {
            return Ok(exact);
        }
        run("s.name = ?1 COLLATE NOCASE AND s.kind != 'impl'", q)
    }

    /// One symbol, or an explanatory error listing candidates.
    fn resolve_one(&self, q: &str) -> Result<std::result::Result<Sym, Response>> {
        let found = self.resolve(q)?;
        let page = Page::new(Some(10), None);
        match found.len() {
            0 => {
                let sugg = self.symbol_search(q, &page, None)?;
                let mut r = self.respond("resolve", q, &page, sugg.1, sugg.0, vec![format!("no symbol named `{q}`; closest matches:")])?;
                r.op = "not_found".into();
                Ok(Err(r))
            }
            1 => Ok(Ok(found.into_iter().next().unwrap_or_else(unreachable_sym))),
            n => {
                let rows: Vec<Row> = found.iter().take(10).map(|s| sym_row(s, "candidate")).collect();
                let mut r = self.respond(
                    "ambiguous",
                    q,
                    &page,
                    n,
                    rows,
                    vec![format!("`{q}` names {n} symbols; pass a sid or qualified name from the candidates (or all=true)")],
                )?;
                r.op = "ambiguous".into();
                Ok(Err(r))
            }
        }
    }

    fn targets(&self, q: &str, all: bool) -> Result<std::result::Result<Vec<Sym>, Response>> {
        if all {
            let v = self.resolve(q)?;
            if !v.is_empty() {
                return Ok(Ok(v));
            }
        }
        Ok(self.resolve_one(q)?.map(|s| vec![s]))
    }

    // ------------------------------------------------------------------------------ find_symbol

    fn fts_query(q: &str) -> Option<String> {
        const STOP: &[&str] = &[
            "the", "and", "for", "with", "that", "this", "from", "into", "how", "does", "what", "where", "when", "are", "use", "fix", "add", "make",
            "of", "to", "in", "on", "is", "a", "an", "it", "be", "by", "or",
        ];
        let mut seen = HashSet::new();
        let toks: Vec<String> = syntax::words(q)
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 2 && !STOP.contains(t))
            .filter(|t| seen.insert(t.to_string()))
            .take(24)
            .map(|t| format!("\"{t}\""))
            .collect();
        (!toks.is_empty()).then(|| toks.join(" OR "))
    }

    /// Ranked symbol search: exact name, then name prefix, then full text.
    fn symbol_search(&self, q: &str, page: &Page, kind: Option<&str>) -> Result<(Vec<Row>, usize)> {
        let mut ids: Vec<i64> = Vec::new();
        let mut seen = HashSet::new();
        let kind_sql = if kind.is_some() { " AND s.kind = ?2" } else { "" };
        let k = kind.unwrap_or("");
        for sql in [
            format!("SELECT s.id FROM symbols s WHERE s.name = ?1{kind_sql} ORDER BY s.is_test, s.kind = 'impl' LIMIT 400"),
            format!("SELECT s.id FROM symbols s WHERE s.qualname = ?1 OR s.sid = ?1{kind_sql} LIMIT 50"),
            format!("SELECT s.id FROM symbols s WHERE s.name LIKE ?1 || '%' ESCAPE '\\'{kind_sql} ORDER BY length(s.name), s.is_test LIMIT 400"),
        ] {
            let mut st = self.conn().prepare(&sql)?;
            let rows: Vec<i64> = if kind.is_some() {
                st.query_map(params![q, k], |r| r.get(0))?.collect::<Result<_, _>>()?
            } else {
                st.query_map(params![q], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            ids.extend(rows.into_iter().filter(|id| seen.insert(*id)));
        }
        if let Some(fq) = Self::fts_query(q) {
            let sql = format!(
                "SELECT s.id FROM symbols_fts JOIN symbols s ON s.id = symbols_fts.rowid
                 WHERE symbols_fts MATCH ?1{kind_sql}
                 ORDER BY bm25(symbols_fts, 10.0, 6.0, 4.0, 3.0, 2.0, 1.0, 1.0) + s.is_test LIMIT 400"
            );
            let mut st = self.conn().prepare(&sql)?;
            let rows: Vec<i64> = if kind.is_some() {
                st.query_map(params![fq, k], |r| r.get(0))?.collect::<Result<_, _>>()?
            } else {
                st.query_map(params![fq], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            ids.extend(rows.into_iter().filter(|id| seen.insert(*id)));
        }
        let total = ids.len();
        let page_ids = Self::paginate(&ids, page);
        let mut out = Vec::with_capacity(page_ids.len());
        for id in page_ids {
            if let Some(s) = self.sym_by_id(id)? {
                out.push(sym_row(&s, ""));
            }
        }
        Ok((out, total))
    }

    pub fn sym_by_id(&self, id: i64) -> Result<Option<Sym>> {
        let sql = format!("SELECT {SYM_COLS} FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.id = ?1");
        Ok(self.conn().query_row(&sql, [id], sym_from_row).optional()?)
    }

    pub fn find_symbol(&self, q: &str, kind: Option<&str>, page: &Page) -> Result<Response> {
        let (rows, total) = self.symbol_search(q, page, kind)?;
        self.respond("find_symbol", q, page, total, rows, Vec::new())
    }

    // -------------------------------------------------------------------------- references

    fn syntax_kinds(kind: &str) -> &'static [&'static str] {
        match kind {
            "fn" | "method" => &["call", "method", "path"],
            "macro" => &["macro"],
            "const" | "static" | "mod" => &["path"],
            "variant" => &["path", "call"],
            _ => &["type", "path", "call"],
        }
    }

    /// (path, line, col, kind, enclosing qualname, enclosing id, provenance) of a symbol's uses.
    fn uses(&self, t: &Sym, calls_only: bool) -> Result<(Vec<Use>, Vec<String>)> {
        let mut notes = Vec::new();
        let mut out = Vec::new();
        let base = "SELECT f.path, r.line, r.col, r.kind, coalesce(e.qualname, ''), r.enclosing_id, r.provenance, f.is_test
                    FROM refs r JOIN files f ON f.id = r.file_id LEFT JOIN symbols e ON e.id = r.enclosing_id";
        let map = |r: &rusqlite::Row| -> rusqlite::Result<Use> {
            Ok(Use {
                path: r.get(0)?,
                line: r.get(1)?,
                col: r.get(2)?,
                kind: r.get(3)?,
                enclosing: r.get(4)?,
                enclosing_id: r.get(5)?,
                provenance: r.get(6)?,
                is_test: r.get(7)?,
            })
        };
        let sk: Vec<&str> = if calls_only {
            Self::syntax_kinds(&t.kind).iter().copied().filter(|k| *k == "call" || *k == "method").collect()
        } else {
            Self::syntax_kinds(&t.kind).to_vec()
        };
        let kinds_sql = sk.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
        if sk.is_empty() {
            return Ok((out, notes));
        }
        if let Some(scip) = t.scip {
            let extra = if calls_only { " AND r.kind = 'call'" } else { "" };
            let mut st = self.conn().prepare(&format!("{base} WHERE r.target = ?1 AND r.provenance = 'scip'{extra}"))?;
            out.extend(st.query_map([scip], map)?.collect::<Result<Vec<_>, _>>()?);
            let mut st = self.conn().prepare(&format!(
                "{base} WHERE r.name = ?1 AND r.provenance = 'syntax' AND r.kind IN ({kinds_sql}) AND f.lang = ?2
                 AND (f.semantic_hash IS NULL OR f.semantic_hash != f.hash)"
            ))?;
            let unresolved: Vec<Use> = st.query_map(params![t.name, t.file_lang], map)?.collect::<Result<_, _>>()?;
            notes.push(format!(
                "resolved by SCIP{}",
                if unresolved.is_empty() {
                    String::new()
                } else {
                    format!("; {} name match(es) in files without current SCIP data are unresolved", unresolved.len())
                }
            ));
            out.extend(unresolved);
        } else {
            let mut st =
                self.conn().prepare(&format!("{base} WHERE r.name = ?1 AND r.provenance = 'syntax' AND r.kind IN ({kinds_sql}) AND f.lang = ?2"))?;
            out.extend(st.query_map(params![t.name, t.file_lang], map)?.collect::<Result<Vec<_>, _>>()?);
            let others: i64 = self.conn().query_row(
                "SELECT count(*) FROM symbols WHERE name = ?1 AND id != ?2 AND kind != 'impl'",
                params![t.name, t.id],
                |r| r.get(0),
            )?;
            let mut n = "unresolved name matches (syntax)".to_string();
            if others > 0 {
                n.push_str(&format!("; {others} other definition(s) are also named `{}` and may be the real target", t.name));
            }
            if !t.semantic_current {
                n.push_str("; run `codegraph semantic` for resolved references");
            }
            notes.push(n);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        out.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
        Ok((out, notes))
    }

    pub fn find_references(&self, q: &str, all: bool, page: &Page) -> Result<Response> {
        let targets = match self.targets(q, all)? {
            Ok(t) => t,
            Err(r) => return Ok(r),
        };
        let mut uses = Vec::new();
        let mut notes = Vec::new();
        for t in &targets {
            let (u, n) = self.uses(t, false)?;
            uses.extend(u);
            notes.extend(n.into_iter().map(|n| format!("{}: {n}", t.qualname)));
        }
        let rows: Vec<Row> = uses
            .iter()
            .map(|u| Row {
                path: u.path.clone(),
                lines: u.line.to_string(),
                kind: u.kind.clone(),
                name: u.enclosing.clone(),
                provenance: u.provenance.clone(),
                ..Row::default()
            })
            .collect();
        let total = rows.len();
        self.respond("find_references", q, page, total, Self::paginate(&rows, page), notes)
    }

    pub fn find_callers(&self, q: &str, all: bool, page: &Page) -> Result<Response> {
        let targets = match self.targets(q, all)? {
            Ok(t) => t,
            Err(r) => return Ok(r),
        };
        let mut notes = Vec::new();
        // enclosing id (or file for top-level) -> (row, call lines)
        let mut groups: BTreeMap<(String, i64), (Row, Vec<i64>, usize)> = BTreeMap::new();
        for t in &targets {
            let (u, n) = self.uses(t, true)?;
            notes.extend(n.into_iter().map(|n| format!("{}: {n}", t.qualname)));
            for c in u {
                let key = (c.path.clone(), c.enclosing_id.unwrap_or(-1));
                let entry = groups.entry(key).or_insert_with(|| {
                    let caller = c.enclosing_id.and_then(|id| self.sym_by_id(id).ok().flatten());
                    let row = match caller {
                        Some(s) => {
                            let mut r = sym_row(&s, "caller");
                            r.detail = String::new();
                            r
                        }
                        None => Row {
                            rel: "caller".into(),
                            path: c.path.clone(),
                            lines: c.line.to_string(),
                            name: "(top level)".into(),
                            ..Row::default()
                        },
                    };
                    (row, Vec::new(), 0)
                });
                entry.1.push(c.line);
                entry.2 += 1;
                if entry.0.provenance.is_empty() || c.provenance == "syntax" {
                    entry.0.provenance = c.provenance.clone();
                }
            }
        }
        let mut rows: Vec<(Row, usize)> = groups
            .into_values()
            .map(|(mut r, lines, n)| {
                let shown: Vec<String> = lines.iter().take(5).map(|l| l.to_string()).collect();
                r.detail = format!("{n} call(s) at line {}{}", shown.join(","), if lines.len() > 5 { ",…" } else { "" });
                (r, n)
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.path.cmp(&b.0.path)));
        let rows: Vec<Row> = rows.into_iter().map(|(r, _)| r).collect();
        notes.push(
            "calls through trait objects and function pointers are not resolved; calls inside macro arguments are found by token heuristics (syntax)"
                .into(),
        );
        let total = rows.len();
        self.respond("find_callers", q, page, total, Self::paginate(&rows, page), notes)
    }

    // ------------------------------------------------------------------------- dependencies

    fn package_of(&self, q: &str) -> Result<Option<(i64, String, String, String)>> {
        Ok(self
            .conn()
            .query_row("SELECT id, name, ident, manifest FROM packages WHERE name = ?1 OR ident = ?1", [q], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?)
    }

    pub fn find_dependencies(&self, q: &str, page: &Page) -> Result<Response> {
        let mut rows = Vec::new();
        let mut notes = Vec::new();
        if let Some((id, name, _ident, manifest)) = self.package_of(q)? {
            let mut st = self.conn().prepare(
                "SELECT d.dep, d.kind, p.manifest FROM package_deps d LEFT JOIN packages p ON p.name = d.dep
                 WHERE d.package_id = ?1 ORDER BY p.manifest IS NULL, d.kind, d.dep",
            )?;
            for r in st.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))? {
                let (dep, kind, m) = r?;
                rows.push(Row {
                    rel: "depends_on".into(),
                    path: m.clone().unwrap_or_default(),
                    lines: String::new(),
                    kind: kind.clone(),
                    name: dep,
                    detail: if m.is_some() { "workspace package".into() } else { "external".into() },
                    provenance: "manifest".into(),
                    ..Row::default()
                });
            }
            let mut st = self.conn().prepare(
                "SELECT p.name, p.manifest, d.kind FROM package_deps d JOIN packages p ON p.id = d.package_id
                 WHERE d.dep = ?1 ORDER BY p.name",
            )?;
            for r in st.query_map([&name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
                let (n, m, kind) = r?;
                rows.push(Row { rel: "depended_by".into(), path: m, kind, name: n, provenance: "manifest".into(), ..Row::default() });
            }
            notes.push(format!("package {name} ({manifest})"));
        } else {
            let file: Option<(i64, String, String, String)> = self
                .conn()
                .query_row("SELECT id, path, module, lang FROM files WHERE path = ?1 OR module = ?1 ORDER BY path = ?1 DESC LIMIT 1", [q], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            let Some((fid, path, module, lang)) = file else {
                let r = self.respond("find_dependencies", q, page, 0, Vec::new(), vec![format!("no package, file or module named `{q}`")])?;
                return Ok(r);
            };
            let mut st = self.conn().prepare("SELECT line, path FROM imports WHERE file_id = ?1 ORDER BY line")?;
            let imports: Vec<(i64, String)> = st.query_map([fid], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            let mut seen_targets = HashSet::new();
            for (line, ip) in imports {
                let target = self.resolve_import(&path, &module, &lang, &ip)?;
                let detail = match &target {
                    Some(_) => String::new(),
                    None => "external or unresolved".into(),
                };
                if let Some(t) = &target
                    && !seen_targets.insert(t.clone())
                {
                    continue;
                }
                let detail = if detail.is_empty() { format!("line {line}") } else { format!("line {line}, {detail}") };
                rows.push(Row {
                    rel: "imports".into(),
                    path: target.unwrap_or_default(),
                    lines: String::new(),
                    name: ip,
                    detail,
                    provenance: "syntax".into(),
                    ..Row::default()
                });
            }
            // Importers: candidate imports mentioning the last module segment, resolved back.
            let last = if lang == "rust" {
                module.rsplit("::").next().unwrap_or(&module).to_string()
            } else {
                path.rsplit('/').next().and_then(|f| f.split('.').next()).unwrap_or("").to_string()
            };
            let mut st = self.conn().prepare(
                "SELECT f.path, f.module, f.lang, i.line, i.path FROM imports i JOIN files f ON f.id = i.file_id
                 WHERE i.path LIKE '%' || ?1 || '%' AND f.id != ?2 LIMIT 5000",
            )?;
            let cands: Vec<(String, String, String, i64, String)> =
                st.query_map(params![last, fid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
            let mut importers = HashSet::new();
            for (fp, fm, fl, line, ip) in cands {
                if importers.contains(&fp) {
                    continue;
                }
                if self.resolve_import(&fp, &fm, &fl, &ip)?.as_deref() == Some(path.as_str()) {
                    importers.insert(fp.clone());
                    rows.push(Row {
                        rel: "imported_by".into(),
                        path: fp,
                        lines: line.to_string(),
                        name: ip,
                        provenance: "syntax".into(),
                        ..Row::default()
                    });
                }
            }
            notes
                .push(format!("file {path} (module {module}); imports resolved by module path, `#[path]` modules and glob re-exports may be missed"));
        }
        let total = rows.len();
        self.respond("find_dependencies", q, page, total, Self::paginate(&rows, page), notes)
    }

    /// The repository file an import names, if any.
    pub fn resolve_import(&self, from_path: &str, from_module: &str, lang: &str, ip: &str) -> Result<Option<String>> {
        if lang != "rust" {
            if !ip.starts_with('.') {
                return Ok(None);
            }
            let dir = from_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let mut segs: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
            for s in ip.split('/') {
                match s {
                    "." | "" => {}
                    ".." => {
                        segs.pop();
                    }
                    s => segs.push(s),
                }
            }
            let base = segs.join("/");
            let stem = base.trim_end_matches(".js").trim_end_matches(".ts").to_string();
            for cand in [
                base.clone(),
                format!("{stem}.ts"),
                format!("{stem}.tsx"),
                format!("{stem}.js"),
                format!("{stem}/index.ts"),
                format!("{stem}/index.tsx"),
            ] {
                let hit: Option<String> = self.conn().query_row("SELECT path FROM files WHERE path = ?1", [&cand], |r| r.get(0)).optional()?;
                if hit.is_some() {
                    return Ok(hit);
                }
            }
            return Ok(None);
        }
        let mut segs: Vec<String> = ip.split("::").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let Some(first) = segs.first().cloned() else { return Ok(None) };
        let mods: Vec<String> = from_module.split("::").map(str::to_string).collect();
        let crate_ident = mods.first().cloned().unwrap_or_default();
        match first.as_str() {
            "crate" | "$crate" => {
                segs[0] = crate_ident;
            }
            "self" => {
                segs.remove(0);
                let mut m = mods.clone();
                m.extend(segs);
                segs = m;
            }
            "super" => {
                let mut m = mods.clone();
                while segs.first().map(String::as_str) == Some("super") {
                    segs.remove(0);
                    m.pop();
                }
                m.extend(segs);
                segs = m;
            }
            other => {
                let is_pkg: bool =
                    self.conn().query_row("SELECT count(*) > 0 FROM packages WHERE ident = ?1 AND lang = 'rust'", [other], |r| r.get(0))?;
                if !is_pkg {
                    // `use foo::Bar` may name a sibling module of the current crate (2015-style
                    // paths in submodules, or a child module of the file).
                    let mut m = mods.clone();
                    m.extend(segs.clone());
                    for k in (2..=m.len()).rev() {
                        let cand = m.get(..k).map(|s| s.join("::")).unwrap_or_default();
                        let hit: Option<String> = self
                            .conn()
                            .query_row("SELECT path FROM files WHERE module = ?1 AND lang = 'rust' LIMIT 1", [&cand], |r| r.get(0))
                            .optional()?;
                        if hit.is_some() && k > mods.len() {
                            return Ok(hit);
                        }
                    }
                    return Ok(None);
                }
            }
        }
        for k in (1..=segs.len()).rev() {
            let cand = segs.get(..k).map(|s| s.join("::")).unwrap_or_default();
            let hit: Option<String> = self
                .conn()
                .query_row("SELECT path FROM files WHERE module = ?1 AND lang = 'rust' ORDER BY is_test, length(path) LIMIT 1", [&cand], |r| r.get(0))
                .optional()?;
            if hit.is_some() {
                return Ok(hit);
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------------------ implementations

    pub fn find_implementations(&self, q: &str, page: &Page) -> Result<Response> {
        let name = q.split('<').next().unwrap_or(q).rsplit("::").next().unwrap_or(q).trim().to_string();
        let mut rows = Vec::new();
        let sql = "SELECT f.path, i.line, i.trait_path, i.type_name, s.qualname, s.end_line, s.sid
                   FROM impls i JOIN files f ON f.id = i.file_id JOIN symbols s ON s.id = i.symbol_id WHERE ";
        let mut st = self.conn().prepare(&format!("{sql} i.trait_name = ?1 ORDER BY f.is_test, f.path, i.line"))?;
        for r in st.query_map([&name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
            ))
        })? {
            let (path, line, tp, ty, qn, end, sid) = r?;
            rows.push(Row {
                rel: "implemented_by".into(),
                path,
                lines: lines(line, end),
                kind: "impl".into(),
                name: ty,
                sid,
                detail: format!("{tp} ({qn})"),
                provenance: "syntax".into(),
            });
        }
        let mut st = self.conn().prepare(&format!("{sql} i.type_name = ?1 ORDER BY f.path, i.line"))?;
        for r in st.query_map([&name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(5)?, r.get::<_, String>(6)?))
        })? {
            let (path, line, tp, end, sid) = r?;
            rows.push(Row {
                rel: "implements".into(),
                path,
                lines: lines(line, end),
                kind: "impl".into(),
                name: tp,
                sid,
                provenance: "syntax".into(),
                ..Row::default()
            });
        }
        let notes = vec![format!(
            "matched by trait/type name `{name}` (syntax): implementations of another trait with the same name also match; generated impls (derive, macros) are not listed"
        )];
        let total = rows.len();
        self.respond("find_implementations", q, page, total, Self::paginate(&rows, page), notes)
    }

    // -------------------------------------------------------------------------- symbol context

    /// Source lines [a, b] of a file, noting when the file changed since it was indexed.
    pub fn snippet(&self, path: &str, a: i64, b: i64) -> Result<(String, bool)> {
        let Ok(text) = std::fs::read_to_string(self.root.join(path)) else { return Ok((String::new(), true)) };
        let indexed: Option<String> = self.conn().query_row("SELECT hash FROM files WHERE path = ?1", [path], |r| r.get(0)).optional()?;
        let changed = indexed.as_deref() != Some(inventory::hash(text.as_bytes()).as_str());
        let from = usize::try_from(a.max(1) - 1).unwrap_or(0);
        let n = usize::try_from((b - a + 1).max(0)).unwrap_or(0);
        let mut out = String::new();
        for (i, l) in text.lines().enumerate().skip(from).take(n) {
            out.push_str(&format!("{:>5} {}\n", i + 1, l));
        }
        Ok((out, changed))
    }

    pub fn get_symbol_context(&self, q: &str, snippet_lines: usize) -> Result<Response> {
        let s = match self.resolve_one(q)? {
            Ok(s) => s,
            Err(r) => return Ok(r),
        };
        let page = Page::new(Some(MAX_LIMIT), None);
        let mut text = String::new();
        text.push_str(&format!("{} {}  {}:{}  sid={}\n", s.kind, s.qualname, s.path, lines(s.start_line, s.end_line), s.sid));
        if !s.sig.is_empty() {
            text.push_str(&format!("sig: {}\n", s.sig));
        }
        if !s.doc.is_empty() {
            text.push_str(&format!("doc: {}\n", s.doc));
        }
        if let Some(pid) = s.parent_id
            && let Some(p) = self.sym_by_id(pid)?
        {
            text.push_str(&format!("in: {} {} ({}:{})\n", p.kind, p.qualname, p.path, p.start_line));
        }
        let mut children: Vec<(String, String, i64)> = {
            let mut st = self.conn().prepare("SELECT kind, name, start_line FROM symbols WHERE parent_id = ?1 ORDER BY start_line LIMIT 41")?;
            let v = st.query_map([s.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            v
        };
        if !children.is_empty() {
            let more = children.len() > 40;
            children.truncate(40);
            let list: Vec<String> = children.iter().map(|(k, n, l)| format!("{k} {n}:{l}")).collect();
            text.push_str(&format!("members: {}{}\n", list.join(", "), if more { ", …" } else { "" }));
        }
        let impls = self.find_implementations(&s.name, &Page::new(Some(12), None))?;
        let rel_rows: Vec<String> = impls
            .results
            .iter()
            .filter(|r| (r.rel == "implements" && s.kind != "trait") || (r.rel == "implemented_by" && s.kind == "trait"))
            .map(|r| format!("{} ({}:{})", r.name, r.path, r.lines))
            .collect();
        if !rel_rows.is_empty() {
            let label = if s.kind == "trait" { "implemented by" } else { "implements" };
            text.push_str(&format!("{label} [syntax]: {}\n", rel_rows.join(", ")));
        }
        let (uses, notes) = self.uses(&s, false)?;
        let callers = self.find_callers(&s.sid, false, &Page::new(Some(8), None))?;
        if !callers.results.is_empty() {
            text.push_str(&format!("callers ({} total, {}):\n", callers.total, uses.first().map(|u| u.provenance.as_str()).unwrap_or("syntax")));
            for r in &callers.results {
                text.push_str(&format!("  {} {}:{} — {}\n", r.name, r.path, r.lines, r.detail));
            }
        }
        let callees = self.callees(&s, 15)?;
        if !callees.is_empty() {
            text.push_str("calls:\n");
            for c in callees {
                text.push_str(&format!("  {c}\n"));
            }
        }
        text.push_str(&format!("references: {} ({})\n", uses.len(), notes.join("; ")));
        if snippet_lines > 0 {
            let end = s.end_line.min(s.start_line + i64::try_from(snippet_lines).unwrap_or(40) - 1);
            let (snip, changed) = self.snippet(&s.path, s.start_line, end)?;
            text.push_str(&format!(
                "source{}{}:\n{snip}",
                if end < s.end_line {
                    format!(" (first {} of {} lines)", end - s.start_line + 1, s.end_line - s.start_line + 1)
                } else {
                    String::new()
                },
                if changed { " [file changed since index; lines may be off]" } else { "" }
            ));
        }
        let mut r = self.respond("get_symbol_context", q, &page, 1, vec![sym_row(&s, "definition")], Vec::new())?;
        r.text = text;
        Ok(r)
    }

    /// What a symbol's body calls: resolved targets (SCIP) where available, else names with the
    /// number of same-named definitions.
    fn callees(&self, s: &Sym, max: usize) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut st = self.conn().prepare(
            "SELECT DISTINCT d.qualname, f.path, d.start_line FROM refs r JOIN symbols d ON d.scip = r.target
             JOIN files f ON f.id = d.file_id WHERE r.enclosing_id = ?1 AND r.provenance = 'scip' AND r.kind = 'call' LIMIT 60",
        )?;
        for r in st.query_map([s.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
            let (q, p, l) = r?;
            let name = q.rsplit("::").next().unwrap_or(&q).to_string();
            if seen.insert(name) && out.len() < max {
                out.push(format!("{q} ({p}:{l}) [scip]"));
            }
        }
        let mut st = self.conn().prepare(
            "SELECT r.name, r.kind FROM refs r WHERE r.enclosing_id = ?1 AND r.provenance = 'syntax'
             AND r.kind IN ('call','method') GROUP BY r.name, r.kind ORDER BY min(r.line) LIMIT 80",
        )?;
        let names: Vec<(String, String)> = st.query_map([s.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        for (name, kind) in names {
            // Common names (`new`, `to_string`, …) mostly resolve to std or many types: noise.
            if out.len() >= max || COMMON_NAMES.contains(&name.as_str()) || !seen.insert(name.clone()) {
                continue;
            }
            // `.m()` can only be a method; `f()` / `T::f()` a function, method, constructor or variant.
            let kinds = if kind == "method" { "'method'" } else { "'fn','method','struct','variant'" };
            let defs: Vec<(String, String, i64)> = {
                let mut st = self.conn().prepare(&format!(
                    "SELECT s.qualname, f.path, s.start_line FROM symbols s JOIN files f ON f.id = s.file_id
                     WHERE s.name = ?1 AND s.kind IN ({kinds}) LIMIT 3"
                ))?;
                let v = st.query_map([&name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                v
            };
            match defs.len() {
                0 => {} // external (std, dependencies): not worth the tokens
                1 => {
                    if let Some((q, p, l)) = defs.first() {
                        out.push(format!("{q} ({p}:{l}) [syntax, only definition of that name]"));
                    }
                }
                _ => out.push(format!("{name} [syntax, {}+ definitions of that name]", defs.len())),
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------- change impact

    pub fn get_change_impact(&self, items: &[String], page: &Page) -> Result<Response> {
        let mut notes = Vec::new();
        let mut seeds: Vec<Sym> = Vec::new();
        let mut seed_files: HashSet<String> = HashSet::new();
        for it in items {
            let fid: Option<i64> = self.conn().query_row("SELECT id FROM files WHERE path = ?1", [it], |r| r.get(0)).optional()?;
            if let Some(fid) = fid {
                seed_files.insert(it.clone());
                let sql = format!(
                    "SELECT {SYM_COLS} FROM symbols s JOIN files f ON f.id = s.file_id
                     WHERE s.file_id = ?1 AND s.kind NOT IN ('impl','mod') LIMIT 400"
                );
                let mut st = self.conn().prepare(&sql)?;
                seeds.extend(st.query_map([fid], sym_from_row)?.collect::<Result<Vec<_>, _>>()?);
            } else {
                let found = self.resolve(it)?;
                if found.is_empty() {
                    notes.push(format!("`{it}` is neither an indexed file nor a symbol"));
                }
                seeds.extend(found);
            }
        }
        let mut by_file: BTreeMap<String, (usize, bool, BTreeMap<String, usize>)> = BTreeMap::new();
        let mut skipped = Vec::new();
        for s in &seeds {
            if s.scip.is_none() {
                let defs: i64 = self.conn().query_row("SELECT count(*) FROM symbols WHERE name = ?1 AND kind != 'impl'", [&s.name], |r| r.get(0))?;
                if defs > 3 || s.name.len() < 3 || COMMON_NAMES.contains(&s.name.as_str()) {
                    skipped.push(s.name.clone());
                    continue;
                }
            }
            let (uses, _) = self.uses(s, false)?;
            for u in uses {
                let e = by_file.entry(u.path.clone()).or_insert((0, u.is_test, BTreeMap::new()));
                e.0 += 1;
                if !u.enclosing.is_empty() {
                    *e.2.entry(u.enclosing.clone()).or_insert(0) += 1;
                }
            }
        }
        if !skipped.is_empty() {
            skipped.sort();
            skipped.dedup();
            notes.push(format!(
                "{} common or ambiguous name(s) not traced without SCIP data: {}",
                skipped.len(),
                skipped.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        let mut files: Vec<(String, usize, bool, BTreeMap<String, usize>)> = by_file.into_iter().map(|(p, (n, t, e))| (p, n, t, e)).collect();
        files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut rows: Vec<Row> = files
            .iter()
            .map(|(p, n, t, encl)| {
                let mut top: Vec<(&String, &usize)> = encl.iter().collect();
                top.sort_by(|a, b| b.1.cmp(a.1));
                let names: Vec<String> = top
                    .iter()
                    .take(4)
                    .map(|(q, _)| q.rsplit("::").take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("::"))
                    .collect();
                Row {
                    rel: if *t {
                        "affected_test".into()
                    } else if seed_files.contains(p) {
                        "changed_file".into()
                    } else {
                        "affected_file".into()
                    },
                    path: p.clone(),
                    lines: String::new(),
                    kind: String::new(),
                    name: names.join(", "),
                    detail: format!("{n} reference(s)"),
                    ..Row::default()
                }
            })
            .collect();
        // Packages: those owning a seed or affected file, plus every package that depends on a
        // seed package (transitively).
        let mut seed_pkgs: HashSet<String> = HashSet::new();
        for p in seeds.iter().map(|s| s.path.clone()).chain(seed_files.iter().cloned()) {
            let name: Option<String> = self
                .conn()
                .query_row("SELECT p.name FROM files f JOIN packages p ON p.id = f.package_id WHERE f.path = ?1", [&p], |r| r.get(0))
                .optional()?;
            if let Some(n) = name {
                seed_pkgs.insert(n);
            }
        }
        let mut dependents: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> = seed_pkgs.iter().cloned().collect();
        while let Some(p) = frontier.pop() {
            let mut st = self.conn().prepare("SELECT DISTINCT p.name FROM package_deps d JOIN packages p ON p.id = d.package_id WHERE d.dep = ?1")?;
            let v: Vec<String> = st.query_map([&p], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for n in v {
                if !seed_pkgs.contains(&n) && dependents.insert(n.clone()) && dependents.len() < 500 {
                    frontier.push(n);
                }
            }
        }
        let mut seed_list: Vec<String> = seed_pkgs.into_iter().collect();
        seed_list.sort();
        let mut dep_list: Vec<String> = dependents.into_iter().collect();
        dep_list.sort();
        if !seed_list.is_empty() {
            rows.push(Row {
                rel: "package".into(),
                name: seed_list.join(", "),
                detail: format!("test: cargo test -p {}", seed_list.join(" -p ")),
                provenance: "manifest".into(),
                ..Row::default()
            });
        }
        if !dep_list.is_empty() {
            rows.push(Row {
                rel: "dependent_packages".into(),
                name: dep_list.join(", "),
                detail: "depend on the changed package(s); rebuild/test if public items changed".into(),
                provenance: "manifest".into(),
                ..Row::default()
            });
        }
        notes.push(format!("{} seed symbol(s); one hop of references, ranked by count", seeds.len()));
        let total = rows.len();
        self.respond("get_change_impact", &items.join(" "), page, total, Self::paginate(&rows, page), notes)
    }

    // --------------------------------------------------------------------------- task context

    pub fn get_task_context(&self, query: &str, max_tokens: usize) -> Result<Response> {
        let max_tokens = max_tokens.clamp(200, 20_000);
        let budget = max_tokens * 4; // ~4 characters per token (estimate, not a tokenizer)
        let mut scored: Vec<(i64, f64, String)> = Vec::new();
        let mut seen = HashSet::new();
        // Identifiers in the query (snake_case, CamelCase, paths) rank first; plain words only
        // through full text, or every "room" and "token" in the codebase would outrank the task.
        let identifier_like = |t: &str| {
            t.contains('_') || t.contains("::") || t.chars().skip(1).any(char::is_uppercase) || t.chars().next().is_some_and(char::is_uppercase)
        };
        for tok in query
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
            .map(|t| t.trim_matches(':'))
            .filter(|t| t.len() >= 3 && identifier_like(t))
        {
            for s in self.resolve(tok).unwrap_or_default().into_iter().take(3) {
                if seen.insert(s.id) {
                    scored.push((s.id, 3.0, "named in the query".into()));
                }
            }
        }
        if let Some(fq) = Self::fts_query(query) {
            let mut st = self.conn().prepare(
                "SELECT s.id FROM symbols_fts JOIN symbols s ON s.id = symbols_fts.rowid WHERE symbols_fts MATCH ?1
                 ORDER BY bm25(symbols_fts, 10.0, 6.0, 4.0, 3.0, 2.0, 1.0, 1.0) + s.is_test * 2 + (s.kind = 'impl') LIMIT 40",
            )?;
            let ids: Vec<i64> = st.query_map([fq], |r| r.get(0))?.collect::<Result<_, _>>()?;
            for (rank, id) in ids.into_iter().enumerate() {
                if seen.insert(id) {
                    scored.push((id, 2.0 / (1.0 + rank as f64 * 0.35), "text match".into()));
                }
            }
        }
        // One hop of graph expansion from the strongest seeds.
        let seeds: Vec<(i64, f64)> = scored.iter().take(5).map(|(id, s, _)| (*id, *s)).collect();
        for (id, score) in seeds {
            let Some(s) = self.sym_by_id(id)? else { continue };
            let callers = self.find_callers(&s.sid, false, &Page::new(Some(3), None)).ok();
            for r in callers.map(|c| c.results).unwrap_or_default() {
                if let Some(c) = self.resolve(&r.sid).ok().and_then(|v| v.into_iter().next())
                    && seen.insert(c.id)
                {
                    scored.push((c.id, score * 0.35, format!("calls {}", s.name)));
                }
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut text = String::new();
        let mut rows = Vec::new();
        let mut also = Vec::new();
        for (i, (id, _, why)) in scored.iter().enumerate() {
            let Some(s) = self.sym_by_id(*id)? else { continue };
            let head = format!("## {}:{} {} {} ({why})\n", s.path, lines(s.start_line, s.end_line), s.kind, s.qualname);
            let mut block = head.clone();
            if !s.doc.is_empty() {
                block.push_str(&format!("// {}\n", s.doc));
            }
            let body_lines: i64 = if i < 3 {
                60
            } else if i < 8 {
                12
            } else {
                0
            };
            if body_lines > 0 && s.kind != "mod" {
                let end = s.end_line.min(s.start_line + body_lines - 1);
                let (snip, changed) = self.snippet(&s.path, s.start_line, end)?;
                if changed {
                    block.push_str("[file changed since index; lines may be off]\n");
                }
                block.push_str(&snip);
                if end < s.end_line {
                    block.push_str(&format!("      … {} more lines\n", s.end_line - end));
                }
            } else if !s.sig.is_empty() {
                block.push_str(&format!("{}\n", s.sig));
            }
            if text.len() + block.len() <= budget {
                text.push_str(&block);
                rows.push(sym_row(&s, why));
            } else {
                also.push(format!("{} {}:{}", s.qualname, s.path, s.start_line));
            }
        }
        if !also.is_empty() {
            let mut tail = String::from("## also relevant\n");
            for a in &also {
                if text.len() + tail.len() + a.len() + 1 > budget {
                    break;
                }
                tail.push_str(a);
                tail.push('\n');
            }
            text.push_str(&tail);
        }
        let page = Page::new(Some(MAX_LIMIT), None);
        let total = rows.len();
        let notes = vec![format!(
            "~{} tokens (estimated at 4 chars/token) of a {max_tokens}-token budget; lexical match + one hop of callers",
            text.len() / 4
        )];
        let mut r = self.respond("get_task_context", query, &page, total, rows, notes)?;
        r.text = text;
        Ok(r)
    }

    // -------------------------------------------------------------------------------- status

    pub fn status(&self) -> Result<Response> {
        let count = |sql: &str| -> i64 { self.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        let text = format!(
            "files {}  packages {}  symbols {}  refs {} syntax / {} scip  imports {}  impls {}  db {} KiB\n",
            count("SELECT count(*) FROM files"),
            count("SELECT count(*) FROM packages"),
            count("SELECT count(*) FROM symbols"),
            count("SELECT count(*) FROM refs WHERE provenance='syntax'"),
            count("SELECT count(*) FROM refs WHERE provenance='scip'"),
            count("SELECT count(*) FROM imports"),
            count("SELECT count(*) FROM impls"),
            std::fs::metadata(&self.db.path).map(|m| m.len() / 1024).unwrap_or(0)
                + std::fs::metadata(self.db.path.with_extension("db-wal")).map(|m| m.len() / 1024).unwrap_or(0),
        );
        let page = Page::new(Some(1), None);
        let mut r = self.respond("status", "", &page, 0, Vec::new(), Vec::new())?;
        r.text = text;
        Ok(r)
    }
}

#[derive(Debug, Clone)]
struct Use {
    path: String,
    line: i64,
    col: i64,
    kind: String,
    enclosing: String,
    enclosing_id: Option<i64>,
    provenance: String,
    is_test: bool,
}

fn unreachable_sym() -> Sym {
    Sym {
        id: -1,
        sid: String::new(),
        name: String::new(),
        qualname: String::new(),
        kind: String::new(),
        start_line: 0,
        end_line: 0,
        sig: String::new(),
        doc: String::new(),
        is_test: false,
        scip: None,
        parent_id: None,
        path: String::new(),
        file_lang: String::new(),
        semantic_current: false,
    }
}

/// Compact text rendering (default for the CLI and MCP).
pub fn render(r: &Response) -> String {
    let f = &r.freshness;
    let mut out = format!(
        "[{} gen {} @ {}{}; semantic: {}]\n",
        r.op,
        f.generation,
        if f.indexed_rev.is_empty() { "-" } else { &f.indexed_rev },
        if f.stale { format!(", STALE: {} file(s) changed", f.changed_files) } else { String::new() },
        f.semantic.split(" (").next().unwrap_or(&f.semantic)
    );
    for n in &r.notes {
        out.push_str(&format!("note: {n}\n"));
    }
    for row in &r.results {
        let mut line = String::new();
        if !row.rel.is_empty() {
            line.push_str(&format!("{} ", row.rel));
        }
        if !row.path.is_empty() {
            line.push_str(&row.path);
            if !row.lines.is_empty() {
                line.push_str(&format!(":{}", row.lines));
            }
            line.push(' ');
        }
        if !row.kind.is_empty() {
            line.push_str(&format!("{} ", row.kind));
        }
        line.push_str(&row.name);
        if row.sid.contains('#') {
            line.push_str(&format!(" [{}]", row.sid));
        }
        if !row.detail.is_empty() {
            line.push_str(&format!(" — {}", row.detail));
        }
        if !row.provenance.is_empty() {
            line.push_str(&format!(" ({})", row.provenance));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if let Some(n) = r.next_offset {
        out.push_str(&format!("… {} of {} shown; next offset={n}\n", r.results.len(), r.total));
    } else if r.total > 0 && r.op != "get_symbol_context" && r.op != "get_task_context" {
        out.push_str(&format!("({} result(s))\n", r.total));
    }
    if !r.text.is_empty() {
        out.push_str(&r.text);
    }
    out
}
