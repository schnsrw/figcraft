//! A minimal local stdio MCP server (rmcp, the official Rust SDK) over the graph.
//!
//! Read-only over source and index: no tool writes files, runs commands or (re)indexes; indexing
//! stays a separate CLI step. The only writes go to the task-context store (`task_record`). Every
//! response is bounded (limits are clamped) and starts with the index freshness line.

use crate::db;
use crate::query::{self, Ctx, Page};
use crate::tasks;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::path::PathBuf;

/// Hard cap on one response, whatever the parameters ask for.
const MAX_RESPONSE_CHARS: usize = 60_000;

#[derive(Clone)]
pub struct Server {
    root: PathBuf,
    #[allow(dead_code, reason = "read by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct Common {
    /// Maximum results (default 20, at most 200).
    pub limit: Option<usize>,
    /// Results to skip, for the next page.
    pub offset: Option<usize>,
    /// "text" (compact, default) or "json".
    pub format: Option<String>,
    /// Fail instead of answering when source files changed since the last index run.
    pub require_fresh: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SymbolQuery {
    /// Name, qualified name (`Type::method`, `crate::mod::item`) or words describing it.
    pub query: String,
    /// Only this kind: fn, method, struct, enum, trait, type, const, static, macro, mod, variant, impl, class, interface.
    pub kind: Option<String>,
    #[serde(flatten)]
    pub common: Common,
}

#[derive(Deserialize, JsonSchema)]
pub struct SymbolRef {
    /// A sid (`rust:crate::mod::Item`), a qualified name or suffix (`Type::method`), or a bare name.
    pub symbol: String,
    /// When the name matches several symbols, use all of them instead of returning candidates.
    pub all: Option<bool>,
    #[serde(flatten)]
    pub common: Common,
}

#[derive(Deserialize, JsonSchema)]
pub struct ModuleRef {
    /// A package (`figcraft-engine`), a repo-relative file path, or a Rust module path.
    pub module: String,
    #[serde(flatten)]
    pub common: Common,
}

#[derive(Deserialize, JsonSchema)]
pub struct TraitRef {
    /// Trait or interface name; also lists what a type of that name implements.
    pub trait_or_interface: String,
    #[serde(flatten)]
    pub common: Common,
}

#[derive(Deserialize, JsonSchema)]
pub struct ContextRef {
    /// A sid, qualified name or unambiguous bare name.
    pub symbol: String,
    /// Source lines of the definition to include (default 40, at most 200; 0 for none).
    pub snippet_lines: Option<usize>,
    pub format: Option<String>,
    pub require_fresh: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImpactRef {
    /// Repo-relative file paths and/or symbols that change.
    pub files_or_symbols: Vec<String>,
    #[serde(flatten)]
    pub common: Common,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskQuery {
    /// What the task is about, in words and/or symbol names.
    pub query: String,
    /// Token budget for the returned context (default 2000, 200..20000; estimated at 4 chars/token).
    pub max_tokens: Option<usize>,
    pub format: Option<String>,
    pub require_fresh: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskId {
    /// Task identifier.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskList {
    /// Include done and dropped tasks.
    pub include_closed: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskRecord {
    /// Task identifier (letters, digits, - _ . /).
    pub id: String,
    /// Creates the task (or updates its objective) when given.
    pub objective: Option<String>,
    /// symbol | file | decision | constraint | done | todo | test | failure | note. Omit with only `objective`/`status`.
    pub kind: Option<String>,
    /// The entry text (short; at most 2000 characters are kept).
    pub text: Option<String>,
    /// A graph symbol (sid or qualified name) or a file path the entry is about.
    #[serde(rename = "ref")]
    pub r#ref: Option<String>,
    /// open | blocked | done | dropped.
    pub status: Option<String>,
    /// Record HEAD as the revision the task was last validated on.
    pub validated: Option<bool>,
}

fn text(s: String) -> CallToolResult {
    let s = if s.len() > MAX_RESPONSE_CHARS {
        let mut end = MAX_RESPONSE_CHARS;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… truncated at {MAX_RESPONSE_CHARS} characters; narrow the query or lower the limit", s.get(..end).unwrap_or(""))
    } else {
        s
    };
    CallToolResult::success(vec![ContentBlock::text(s)])
}

fn fail(msg: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg)])
}

impl Server {
    pub fn new(root: PathBuf) -> Self {
        Self { root, tool_router: Self::tool_router() }
    }

    /// Opens the index, runs `f`, renders the answer (or a clear error).
    fn answer(&self, format: Option<&str>, require_fresh: Option<bool>, f: impl FnOnce(&Ctx) -> anyhow::Result<query::Response>) -> CallToolResult {
        let db = match db::open_read(&self.root) {
            Ok(d) => d,
            Err(e) => return fail(format!("{e:#}")),
        };
        let ctx = Ctx { db: &db, root: &self.root };
        match f(&ctx) {
            Ok(r) => {
                if require_fresh == Some(true) && r.freshness.stale {
                    return fail(format!(
                        "index is stale: {} source file(s) changed since generation {} (e.g. {}); run `codegraph index`",
                        r.freshness.changed_files,
                        r.freshness.generation,
                        r.freshness.changed_sample.join(", ")
                    ));
                }
                if format == Some("json") {
                    text(serde_json::to_string(&r).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")))
                } else {
                    text(query::render(&r))
                }
            }
            Err(e) => fail(format!("{e:#}")),
        }
    }
}

#[tool_router]
impl Server {
    #[tool(
        description = "Find symbols by name, qualified name or descriptive words (exact name, prefix, then full-text). Returns path:lines, kind, qualified name and signature."
    )]
    async fn find_symbol(&self, Parameters(p): Parameters<SymbolQuery>) -> Result<CallToolResult, McpError> {
        let page = Page::new(p.common.limit, p.common.offset);
        Ok(self.answer(p.common.format.as_deref(), p.common.require_fresh, |c| c.find_symbol(&p.query, p.kind.as_deref(), &page)))
    }

    #[tool(
        description = "Where a symbol is used. Resolved via SCIP when the semantic index is current, otherwise unresolved name matches (labelled syntax)."
    )]
    async fn find_references(&self, Parameters(p): Parameters<SymbolRef>) -> Result<CallToolResult, McpError> {
        let page = Page::new(p.common.limit, p.common.offset);
        Ok(self.answer(p.common.format.as_deref(), p.common.require_fresh, |c| c.find_references(&p.symbol, p.all == Some(true), &page)))
    }

    #[tool(
        description = "Functions and methods that call a symbol, grouped by caller with call lines. Coverage of dynamic dispatch, function pointers and macros is incomplete."
    )]
    async fn find_callers(&self, Parameters(p): Parameters<SymbolRef>) -> Result<CallToolResult, McpError> {
        let page = Page::new(p.common.limit, p.common.offset);
        Ok(self.answer(p.common.format.as_deref(), p.common.require_fresh, |c| c.find_callers(&p.symbol, p.all == Some(true), &page)))
    }

    #[tool(
        description = "Dependencies of a package (manifest dependencies and dependents) or of a file/module (its imports and the files importing it)."
    )]
    async fn find_dependencies(&self, Parameters(p): Parameters<ModuleRef>) -> Result<CallToolResult, McpError> {
        let page = Page::new(p.common.limit, p.common.offset);
        Ok(self.answer(p.common.format.as_deref(), p.common.require_fresh, |c| c.find_dependencies(&p.module, &page)))
    }

    #[tool(
        description = "Implementations of a trait or interface (impl blocks / implements clauses), and what a type of that name implements. Syntax-level: matched by name."
    )]
    async fn find_implementations(&self, Parameters(p): Parameters<TraitRef>) -> Result<CallToolResult, McpError> {
        let page = Page::new(p.common.limit, p.common.offset);
        Ok(self.answer(p.common.format.as_deref(), p.common.require_fresh, |c| c.find_implementations(&p.trait_or_interface, &page)))
    }

    #[tool(description = "One symbol's definition, signature, doc, members, implementations, callers, callees and a bounded source snippet.")]
    async fn get_symbol_context(&self, Parameters(p): Parameters<ContextRef>) -> Result<CallToolResult, McpError> {
        let n = p.snippet_lines.unwrap_or(40).min(200);
        Ok(self.answer(p.format.as_deref(), p.require_fresh, |c| c.get_symbol_context(&p.symbol, n)))
    }

    #[tool(
        description = "What a change to files or symbols may affect: referencing files and symbols (ranked), affected tests, the owning packages and the packages depending on them."
    )]
    async fn get_change_impact(&self, Parameters(p): Parameters<ImpactRef>) -> Result<CallToolResult, McpError> {
        let page = Page::new(p.common.limit, p.common.offset);
        let items: Vec<String> = p.files_or_symbols.into_iter().take(50).collect();
        Ok(self.answer(p.common.format.as_deref(), p.common.require_fresh, |c| c.get_change_impact(&items, &page)))
    }

    #[tool(
        description = "Start-of-task context: the most relevant symbols for a description (full-text + exact names + one hop of callers) with source snippets, packed into a token budget."
    )]
    async fn get_task_context(&self, Parameters(p): Parameters<TaskQuery>) -> Result<CallToolResult, McpError> {
        let max = p.max_tokens.unwrap_or(2000);
        Ok(self.answer(p.format.as_deref(), p.require_fresh, |c| c.get_task_context(&p.query, max)))
    }

    #[tool(description = "Index freshness and size: generation, indexed Git revision, files changed since, semantic (SCIP) coverage and row counts.")]
    async fn index_status(&self) -> Result<CallToolResult, McpError> {
        Ok(self.answer(None, None, |c| c.status()))
    }

    #[tool(
        description = "A persistent task record: objective, relevant symbols and files, decisions, constraints, done and remaining work, tests and failures (symbol refs located in the current index)."
    )]
    async fn task_get(&self, Parameters(p): Parameters<TaskId>) -> Result<CallToolResult, McpError> {
        let r = tasks::open(&self.root).and_then(|c| tasks::get(&c, &self.root, &p.id));
        Ok(match r {
            Ok(Some(t)) => text(tasks::render(&t)),
            Ok(None) => fail(format!("no task `{}`", p.id)),
            Err(e) => fail(format!("{e:#}")),
        })
    }

    #[tool(description = "Open (or all) task records with their remaining-work counts.")]
    async fn task_list(&self, Parameters(p): Parameters<TaskList>) -> Result<CallToolResult, McpError> {
        let r = tasks::open(&self.root).and_then(|c| tasks::list(&c, p.include_closed == Some(true)));
        Ok(match r {
            Ok(v) if v.is_empty() => text("no tasks".into()),
            Ok(v) => text(v.into_iter().map(|(id, st, obj, todo)| format!("{id} [{st}] {todo} todo — {obj}\n")).collect()),
            Err(e) => fail(format!("{e:#}")),
        })
    }

    #[tool(
        description = "Write to the task-context store (never to source): create a task (objective), add one entry (kind + text, optional ref), set status, or mark it validated at HEAD."
    )]
    async fn task_record(&self, Parameters(p): Parameters<TaskRecord>) -> Result<CallToolResult, McpError> {
        let r = (|| -> anyhow::Result<String> {
            let c = tasks::open(&self.root)?;
            let mut done = Vec::new();
            if let Some(o) = &p.objective {
                tasks::create(&c, &p.id, o)?;
                done.push("objective set".to_string());
            }
            if let (Some(k), Some(t)) = (&p.kind, &p.text) {
                let id = tasks::add(&c, &p.id, k, t, p.r#ref.as_deref())?;
                done.push(format!("{k} #{id} added"));
            }
            if let Some(s) = &p.status {
                tasks::set_status(&c, &p.id, s)?;
                done.push(format!("status {s}"));
            }
            if p.validated == Some(true) {
                done.push(format!("validated at {}", tasks::validate(&c, &self.root, &p.id)?));
            }
            if done.is_empty() {
                anyhow::bail!("nothing to record: give objective, kind+text, status or validated");
            }
            Ok(format!("task {}: {}", p.id, done.join(", ")))
        })();
        Ok(match r {
            Ok(s) => text(s),
            Err(e) => fail(format!("{e:#}")),
        })
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codegraph", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Code graph of this repository (read-only). Start with get_task_context or find_symbol, then \
                 get_symbol_context / find_callers / find_references / get_change_impact. Results carry path:line \
                 ranges and provenance (scip = resolved, syntax = name match). The first line says whether the index \
                 is stale; re-index with the `codegraph index` CLI.",
            )
    }
}

pub fn serve(root: PathBuf) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        let service = Server::new(root).serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}
