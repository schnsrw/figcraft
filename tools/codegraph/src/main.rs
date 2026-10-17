//! `codegraph` command line. Indexing commands write the graph; every other command only reads.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use codegraph::query::{self, Ctx, Page};
use codegraph::{bench, db, indexer, inventory, mcp, scip_import, tasks};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "codegraph", version, about = "Incremental code graph for coding agents (SQLite + tree-sitter + SCIP)")]
struct Cli {
    /// Repository (any directory inside the Git work tree; default: the current one).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Print JSON instead of compact text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct PageArgs {
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    offset: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build or incrementally update the index (only changed files are re-parsed).
    Index {
        /// Discard the index and build it from scratch.
        #[arg(long)]
        rebuild: bool,
    },
    /// Semantic pass: run `rust-analyzer scip` (minutes) and attach resolved references.
    Semantic {
        /// Import this SCIP file instead of running rust-analyzer (e.g. from scip-typescript).
        #[arg(long)]
        scip: Option<PathBuf>,
    },
    /// Index freshness and size.
    Status,
    /// find_symbol
    Symbol {
        query: String,
        #[arg(long)]
        kind: Option<String>,
        #[command(flatten)]
        page: PageArgs,
    },
    /// find_references
    Refs {
        symbol: String,
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    /// find_callers
    Callers {
        symbol: String,
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    /// find_dependencies (package, file or module)
    Deps {
        module: String,
        #[command(flatten)]
        page: PageArgs,
    },
    /// find_implementations
    Impls {
        name: String,
        #[command(flatten)]
        page: PageArgs,
    },
    /// get_symbol_context
    Context {
        symbol: String,
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// get_change_impact (files and/or symbols)
    Impact {
        items: Vec<String>,
        #[command(flatten)]
        page: PageArgs,
    },
    /// get_task_context
    TaskContext {
        query: String,
        #[arg(long, default_value_t = 2000)]
        max_tokens: usize,
    },
    /// Persistent task records (separate from the code graph).
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Serve the read-only MCP tools over stdio.
    Mcp,
    /// Measure indexing, retrieval and grep-vs-graph on representative queries.
    Bench {
        /// Write the Markdown report here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Symbol names to benchmark (default: a fixed representative set for this repository).
        #[arg(long, value_delimiter = ',')]
        symbols: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Create a task (or update its objective).
    New {
        id: String,
        objective: String,
    },
    /// Add an entry: kind is symbol|file|decision|constraint|done|todo|test|failure|note.
    Add {
        id: String,
        kind: String,
        text: String,
        #[arg(long = "ref")]
        r#ref: Option<String>,
    },
    /// Mark todo #entry done.
    Done {
        id: String,
        entry: i64,
    },
    /// Set the task status (open, blocked, done, dropped).
    Status {
        id: String,
        status: String,
    },
    /// Record HEAD as the revision the task was validated on.
    Validate {
        id: String,
    },
    Show {
        id: String,
    },
    List {
        #[arg(long)]
        all: bool,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("codegraph: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let start = match &cli.root {
        Some(r) => r.clone(),
        None => std::env::current_dir()?,
    };
    let root = inventory::repo_root(&start)?;
    let print = |r: &query::Response| {
        if cli.json {
            println!("{}", serde_json::to_string_pretty(r).unwrap_or_default());
        } else {
            print!("{}", query::render(r));
        }
    };
    match cli.cmd {
        Cmd::Index { rebuild } => {
            let s = indexer::update(&root, rebuild)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!(
                    "generation {}: {} files, {} parsed ({} new), {} deleted, {} unchanged{} in {} ms",
                    s.generation,
                    s.files,
                    s.parsed,
                    s.added,
                    s.deleted,
                    s.unchanged,
                    if s.full { " (full)" } else { "" },
                    s.ms
                );
            }
        }
        Cmd::Semantic { scip } => {
            let s = scip_import::semantic(&root, scip.as_deref())?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!(
                    "semantic ({}): {} documents, {} files attached, {} skipped (changed during the run), {} definitions matched, {} references, {} ms",
                    s.tool, s.documents, s.files_attached, s.files_skipped_changed, s.definitions_matched, s.refs_written, s.ms
                );
            }
        }
        Cmd::Mcp => mcp::serve(root)?,
        Cmd::Task { cmd } => {
            let c = tasks::open(&root)?;
            match cmd {
                TaskCmd::New { id, objective } => {
                    tasks::create(&c, &id, &objective)?;
                    println!("task {id} created");
                }
                TaskCmd::Add { id, kind, text, r#ref } => {
                    let e = tasks::add(&c, &id, &kind, &text, r#ref.as_deref())?;
                    println!("{kind} #{e} added to {id}");
                }
                TaskCmd::Done { id, entry } => {
                    tasks::complete_entry(&c, &id, entry)?;
                    println!("#{entry} done");
                }
                TaskCmd::Status { id, status } => {
                    tasks::set_status(&c, &id, &status)?;
                    println!("{id}: {status}");
                }
                TaskCmd::Validate { id } => println!("{id} validated at {}", tasks::validate(&c, &root, &id)?),
                TaskCmd::Show { id } => match tasks::get(&c, &root, &id)? {
                    Some(t) if cli.json => println!("{}", serde_json::to_string_pretty(&t)?),
                    Some(t) => print!("{}", tasks::render(&t)),
                    None => bail!("no task `{id}`"),
                },
                TaskCmd::List { all } => {
                    for (id, st, obj, todo) in tasks::list(&c, all)? {
                        println!("{id} [{st}] {todo} todo — {obj}");
                    }
                }
            }
        }
        Cmd::Bench { out, symbols } => {
            let report = bench::run(&root, &symbols)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, &report)?;
                    println!("wrote {}", p.display());
                }
                None => print!("{report}"),
            }
        }
        cmd => {
            let db = db::open_read(&root)?;
            let ctx = Ctx { db: &db, root: &root };
            let pg = |p: PageArgs| Page::new(p.limit, p.offset);
            let r = match cmd {
                Cmd::Status => ctx.status()?,
                Cmd::Symbol { query, kind, page } => ctx.find_symbol(&query, kind.as_deref(), &pg(page))?,
                Cmd::Refs { symbol, all, page } => ctx.find_references(&symbol, all, &pg(page))?,
                Cmd::Callers { symbol, all, page } => ctx.find_callers(&symbol, all, &pg(page))?,
                Cmd::Deps { module, page } => ctx.find_dependencies(&module, &pg(page))?,
                Cmd::Impls { name, page } => ctx.find_implementations(&name, &pg(page))?,
                Cmd::Context { symbol, lines } => ctx.get_symbol_context(&symbol, lines.min(200))?,
                Cmd::Impact { items, page } => ctx.get_change_impact(&items, &pg(page))?,
                Cmd::TaskContext { query, max_tokens } => ctx.get_task_context(&query, max_tokens)?,
                _ => bail!("unhandled command"),
            };
            print(&r);
        }
    }
    Ok(())
}
