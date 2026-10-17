//! The SQLite store: schema, opening (with corruption and version checks) and metadata.
//!
//! One database file per repository under `.codegraph/index.db`. Every index update runs in one
//! `BEGIN IMMEDIATE` transaction, so readers (WAL mode) see either the previous generation or the
//! next one, never a half-written graph. Cross-file relationships are stored as names or SCIP symbol
//! strings and joined at query time, so a changed file cannot leave a stale edge behind in another.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bump when the schema or extraction changes meaning; an index of another version is rebuilt.
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Cargo packages and npm packages, from their manifests.
CREATE TABLE packages (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,      -- crate name with '-' as written in the manifest
  ident TEXT NOT NULL,            -- the name code uses (crate: '-' -> '_')
  lang TEXT NOT NULL,             -- rust | ts
  manifest TEXT NOT NULL,         -- repo-relative manifest path
  root TEXT NOT NULL              -- repo-relative package directory ('' = repo root)
);
CREATE TABLE package_deps (
  package_id INTEGER NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
  dep TEXT NOT NULL,              -- dependency name as written in the manifest
  kind TEXT NOT NULL              -- normal | dev | build
);
CREATE INDEX package_deps_dep ON package_deps(dep);

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,      -- repo-relative, '/' separated
  package_id INTEGER REFERENCES packages(id) ON DELETE SET NULL,
  lang TEXT NOT NULL,
  module TEXT NOT NULL,           -- rust: crate::a::b (syntax-derived); ts: path without extension
  is_test INTEGER NOT NULL,
  hash TEXT NOT NULL,             -- blake3 of the content
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  lines INTEGER NOT NULL,
  generation INTEGER NOT NULL,    -- index generation that last (re)parsed this file
  semantic_hash TEXT              -- content hash the SCIP data was computed from; NULL = none
);
CREATE INDEX files_module ON files(module);

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  sid TEXT NOT NULL,              -- stable id: <lang>:<qualname>[#n]
  name TEXT NOT NULL,
  qualname TEXT NOT NULL,
  kind TEXT NOT NULL,
  parent_id INTEGER,              -- enclosing symbol (impl, trait, mod, class)
  start_line INTEGER NOT NULL,    -- 1-based, inclusive
  end_line INTEGER NOT NULL,
  name_line INTEGER NOT NULL,
  name_col INTEGER NOT NULL,      -- 0-based byte column of the name
  sig TEXT NOT NULL,
  doc TEXT NOT NULL,
  is_test INTEGER NOT NULL,
  scip INTEGER                    -- scip_syms.id of this definition (from the last SCIP import)
);
CREATE INDEX symbols_file ON symbols(file_id);
CREATE INDEX symbols_name ON symbols(name);
CREATE INDEX symbols_sid ON symbols(sid);
CREATE INDEX symbols_qualname ON symbols(qualname);
CREATE INDEX symbols_scip ON symbols(scip);

-- Contentless full-text index over symbols (rowid = symbols.id).
CREATE VIRTUAL TABLE symbols_fts USING fts5(
  name, words, qualname, sig, doc, path, body,
  content='', contentless_delete=1, tokenize='unicode61'
);

-- Occurrences of names in code. Syntax rows are unresolved name matches; SCIP rows carry the
-- exact target symbol. Both are owned by the file they occur in and deleted with it.
CREATE TABLE refs (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  line INTEGER NOT NULL,          -- 1-based
  col INTEGER NOT NULL,           -- 0-based byte column
  name TEXT NOT NULL,
  kind TEXT NOT NULL,             -- call | method | type | path | macro | ref (SCIP non-call)
  enclosing_id INTEGER,           -- innermost symbol containing the occurrence
  target INTEGER,                 -- scip_syms.id (provenance = scip); NULL for syntax rows
  provenance TEXT NOT NULL        -- syntax | scip
);
CREATE INDEX refs_name ON refs(name);
CREATE INDEX refs_target ON refs(target);
CREATE INDEX refs_file ON refs(file_id);
CREATE INDEX refs_enclosing ON refs(enclosing_id);

-- Interned SCIP symbol strings (rust-analyzer / scip-typescript identities).
CREATE TABLE scip_syms (id INTEGER PRIMARY KEY, symbol TEXT NOT NULL UNIQUE);

-- `use` / `import` statements, resolved to files at query time.
CREATE TABLE imports (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  line INTEGER NOT NULL,
  path TEXT NOT NULL              -- rust: a::b::C (expanded); ts: the module specifier
);
CREATE INDEX imports_file ON imports(file_id);
CREATE INDEX imports_path ON imports(path);

-- impl Trait for Type / class X implements Y (syntax).
CREATE TABLE impls (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  symbol_id INTEGER NOT NULL,     -- the impl block / class symbol
  trait_name TEXT NOT NULL,
  trait_path TEXT NOT NULL,
  type_name TEXT NOT NULL,
  line INTEGER NOT NULL
);
CREATE INDEX impls_trait ON impls(trait_name);
CREATE INDEX impls_type ON impls(type_name);
"#;

/// The database is readable but not an index this version can use.
#[derive(Debug)]
pub struct Incompatible(pub String);

impl std::fmt::Display for Incompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Incompatible {}

pub struct Db {
    pub conn: Connection,
    pub path: PathBuf,
}

/// Where the index lives for a repository root.
pub fn index_dir(root: &Path) -> PathBuf {
    root.join(".codegraph")
}

pub fn index_path(root: &Path) -> PathBuf {
    index_dir(root).join("index.db")
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(60))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn is_corruption(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _))
                if matches!(f.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
        )
    })
}

/// Opens an existing index for reading. Fails with an actionable message when it is missing,
/// unreadable or of another schema version.
pub fn open_read(root: &Path) -> Result<Db> {
    let path = index_path(root);
    if !path.is_file() {
        bail!("no code index at {}: run `codegraph index` first", path.display());
    }
    let res = (|| -> Result<Db> {
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        configure(&conn)?;
        let db = Db { conn, path: path.clone() };
        let v = db.schema_version()?;
        if v != Some(SCHEMA_VERSION) {
            bail!("code index at {} has schema {:?}, this codegraph needs {}: run `codegraph index`", path.display(), v, SCHEMA_VERSION);
        }
        Ok(db)
    })();
    match res {
        Err(e) if is_corruption(&e) => Err(e.context(format!("code index at {} is corrupt: run `codegraph index --rebuild`", path.display()))),
        other => other,
    }
}

/// Opens (creating, migrating or replacing as needed) the index for writing.
/// A corrupt or other-version database is moved aside and a fresh one created.
pub fn open_write(root: &Path, rebuild: bool) -> Result<Db> {
    let dir = index_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // The index never belongs in Git.
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    let path = index_path(root);
    if rebuild {
        remove_db(&path)?;
    }
    match try_open_write(&path) {
        Ok(db) => Ok(db),
        // Only damage or an incompatible schema justifies starting over; anything else (a lock
        // held too long, a full disk) is reported and the index is left alone.
        Err(e) if !is_corruption(&e) && e.downcast_ref::<Incompatible>().is_none() => Err(e),
        Err(e) => {
            let reason = if is_corruption(&e) { "corrupt" } else { "incompatible" };
            eprintln!("codegraph: index is {reason} ({e:#}); rebuilding from scratch");
            if path.exists() {
                let aside = path.with_extension(format!("db.{reason}"));
                let _ = std::fs::remove_file(&aside);
                std::fs::rename(&path, &aside)?;
                for ext in ["db-wal", "db-shm"] {
                    let _ = std::fs::remove_file(path.with_extension(ext));
                }
            }
            try_open_write(&path)
        }
    }
}

fn remove_db(path: &Path) -> Result<()> {
    for ext in ["db", "db-wal", "db-shm"] {
        let p = path.with_extension(ext);
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    Ok(())
}

fn try_open_write(path: &Path) -> Result<Db> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let db = Db { conn, path: path.to_path_buf() };
    match db.schema_version()? {
        Some(SCHEMA_VERSION) => {
            // Cheap structural check; a failure here triggers a rebuild.
            let ok: String = db.conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
            if ok != "ok" {
                return Err(Incompatible(format!("quick_check: {ok}")).into());
            }
            Ok(db)
        }
        Some(v) => Err(Incompatible(format!("schema version {v} (need {SCHEMA_VERSION})")).into()),
        None => {
            db.conn.execute_batch(&format!("BEGIN; {SCHEMA} COMMIT;"))?;
            db.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
            db.set_meta("generation", "0")?;
            Ok(db)
        }
    }
}

impl Db {
    fn schema_version(&self) -> Result<Option<i64>> {
        let has_meta: bool = self.conn.query_row("SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='meta'", [], |r| r.get(0))?;
        if !has_meta {
            return Ok(None);
        }
        let v: Option<String> = self.conn.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).optional()?;
        Ok(v.and_then(|s| s.parse().ok()))
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn generation(&self) -> Result<i64> {
        Ok(self.meta("generation")?.and_then(|s| s.parse().ok()).unwrap_or(0))
    }
}
