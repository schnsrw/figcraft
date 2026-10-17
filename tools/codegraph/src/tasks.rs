//! Persistent task context, kept apart from the code graph (`.codegraph/tasks.db`), so rebuilding
//! the index never loses it. Records are short structured entries, not summaries: an objective,
//! then symbols, files, decisions, constraints, done/remaining work, test results and failures,
//! each optionally pointing at a graph symbol (sid or qualified name) or a file.

use crate::db;
use crate::indexer::now_secs;
use crate::inventory;
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::path::Path;

pub const KINDS: &[&str] = &["symbol", "file", "decision", "constraint", "done", "todo", "test", "failure", "note"];
const MAX_TEXT: usize = 2000;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY,
  objective TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  validated_rev TEXT
);
CREATE TABLE IF NOT EXISTS entries (
  id INTEGER PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  ref TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS entries_task ON entries(task_id);
"#;

#[derive(Debug, Serialize)]
pub struct Entry {
    pub id: i64,
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// Where the referenced symbol or file is now (from the code index), or why it is not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub located: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub objective: String,
    pub status: String,
    pub validated_rev: Option<String>,
    pub updated_at: u64,
    pub entries: Vec<Entry>,
}

pub fn open(root: &Path) -> Result<Connection> {
    let dir = db::index_dir(root);
    std::fs::create_dir_all(&dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    let conn = Connection::open(dir.join("tasks.db"))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.len() <= MAX_TEXT {
        return s.to_string();
    }
    let mut end = MAX_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", s.get(..end).unwrap_or(""))
}

fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 80 || !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
        bail!("task id must be 1-80 characters of letters, digits, - _ . /");
    }
    Ok(())
}

pub fn create(conn: &Connection, id: &str, objective: &str) -> Result<()> {
    valid_id(id)?;
    let now = i64::try_from(now_secs()).unwrap_or(0);
    let n = conn.execute(
        "INSERT INTO tasks(id, objective, created_at, updated_at) VALUES(?1, ?2, ?3, ?3)
         ON CONFLICT(id) DO UPDATE SET objective = excluded.objective, updated_at = excluded.updated_at",
        params![id, clip(objective), now],
    )?;
    if n == 0 {
        bail!("could not create task {id}");
    }
    Ok(())
}

pub fn add(conn: &Connection, id: &str, kind: &str, text: &str, r#ref: Option<&str>) -> Result<i64> {
    if !KINDS.contains(&kind) {
        bail!("entry kind must be one of: {}", KINDS.join(", "));
    }
    let exists: bool = conn.query_row("SELECT count(*) > 0 FROM tasks WHERE id = ?1", [id], |r| r.get(0))?;
    if !exists {
        bail!("no task `{id}`; create it first (task new)");
    }
    let now = i64::try_from(now_secs()).unwrap_or(0);
    conn.execute(
        "INSERT INTO entries(task_id, kind, text, ref, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![id, kind, clip(text), r#ref.map(clip), now],
    )?;
    let eid = conn.last_insert_rowid();
    conn.execute("UPDATE tasks SET updated_at = ?2 WHERE id = ?1", params![id, now])?;
    Ok(eid)
}

/// Marks a todo entry done (moves it to `done`), or sets the task status.
pub fn set_status(conn: &Connection, id: &str, status: &str) -> Result<()> {
    if !["open", "blocked", "done", "dropped"].contains(&status) {
        bail!("status must be open, blocked, done or dropped");
    }
    let now = i64::try_from(now_secs()).unwrap_or(0);
    if conn.execute("UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1", params![id, status, now])? == 0 {
        bail!("no task `{id}`");
    }
    Ok(())
}

pub fn complete_entry(conn: &Connection, id: &str, entry: i64) -> Result<()> {
    if conn.execute("UPDATE entries SET kind = 'done' WHERE id = ?2 AND task_id = ?1 AND kind = 'todo'", params![id, entry])? == 0 {
        bail!("task `{id}` has no open todo #{entry}");
    }
    Ok(())
}

/// Records the Git revision the task's tests last passed on.
pub fn validate(conn: &Connection, root: &Path, id: &str) -> Result<String> {
    let rev = inventory::head_rev(root).unwrap_or_default();
    let now = i64::try_from(now_secs()).unwrap_or(0);
    if conn.execute("UPDATE tasks SET validated_rev = ?2, updated_at = ?3 WHERE id = ?1", params![id, rev, now])? == 0 {
        bail!("no task `{id}`");
    }
    Ok(rev)
}

pub fn get(conn: &Connection, root: &Path, id: &str) -> Result<Option<Task>> {
    let t: Option<(String, String, String, Option<String>, i64)> = conn
        .query_row("SELECT id, objective, status, validated_rev, updated_at FROM tasks WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    let Some((id, objective, status, validated_rev, updated)) = t else { return Ok(None) };
    let mut st = conn.prepare("SELECT id, kind, text, ref FROM entries WHERE task_id = ?1 ORDER BY id")?;
    let mut entries: Vec<Entry> = st
        .query_map([&id], |r| Ok(Entry { id: r.get(0)?, kind: r.get(1)?, text: r.get(2)?, r#ref: r.get(3)?, located: None }))?
        .collect::<Result<_, _>>()?;
    // Point references at the current index, when there is one.
    if let Ok(graph) = db::open_read(root) {
        for e in entries.iter_mut() {
            let Some(r) = e.r#ref.as_deref() else { continue };
            let file: Option<i64> = graph.conn.query_row("SELECT lines FROM files WHERE path = ?1", [r], |x| x.get(0)).optional()?;
            if file.is_some() {
                e.located = Some("file indexed".into());
                continue;
            }
            let hit: Option<(String, i64, i64)> = graph
                .conn
                .query_row(
                    "SELECT f.path, s.start_line, s.end_line FROM symbols s JOIN files f ON f.id = s.file_id
                     WHERE s.sid = ?1 OR s.qualname = ?1 LIMIT 1",
                    [r],
                    |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)),
                )
                .optional()?;
            e.located = Some(match hit {
                Some((p, a, b)) => format!("{p}:{a}-{b}"),
                None => "not in the current index (renamed or removed?)".into(),
            });
        }
    }
    Ok(Some(Task { id, objective, status, validated_rev, updated_at: u64::try_from(updated).unwrap_or(0), entries }))
}

pub fn list(conn: &Connection, include_closed: bool) -> Result<Vec<(String, String, String, i64)>> {
    let sql = if include_closed {
        "SELECT t.id, t.status, t.objective, (SELECT count(*) FROM entries e WHERE e.task_id = t.id AND e.kind = 'todo') FROM tasks t ORDER BY t.updated_at DESC LIMIT 100"
    } else {
        "SELECT t.id, t.status, t.objective, (SELECT count(*) FROM entries e WHERE e.task_id = t.id AND e.kind = 'todo') FROM tasks t WHERE t.status IN ('open','blocked') ORDER BY t.updated_at DESC LIMIT 100"
    };
    let mut st = conn.prepare(sql)?;
    let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

/// Compact text for agents: grouped by kind, in a fixed order.
pub fn render(t: &Task) -> String {
    let mut out = format!(
        "task {} [{}]{}\nobjective: {}\n",
        t.id,
        t.status,
        t.validated_rev.as_deref().map(|r| format!(" validated @ {}", r.chars().take(12).collect::<String>())).unwrap_or_default(),
        t.objective
    );
    for kind in KINDS {
        let items: Vec<&Entry> = t.entries.iter().filter(|e| e.kind == *kind).collect();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("{kind}:\n"));
        for e in items {
            out.push_str(&format!("  #{} {}", e.id, e.text));
            if let Some(r) = &e.r#ref {
                out.push_str(&format!(" -> {r}"));
            }
            if let Some(l) = &e.located {
                out.push_str(&format!(" ({l})"));
            }
            out.push('\n');
        }
    }
    out
}
