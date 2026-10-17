# codegraph

An incremental code-intelligence graph for coding agents working in this repository: find a
symbol, its callers, references, implementations, dependencies and the impact of a change without
re-scanning the tree, through a CLI or a read-only MCP server.

Developer tooling only (fork addition). It is a Cargo workspace of its own: it never joins the
product build, `Cargo.lock`, `cargo xtask ci` or the WASM build, and it never modifies tracked
files. The index lives in `.codegraph/` (self-ignored, never committed).

## Use

```sh
cargo build --release --manifest-path tools/codegraph/Cargo.toml
alias codegraph=$PWD/tools/codegraph/target/release/codegraph

codegraph index            # first run builds everything (~7 s here); later runs re-parse only changes
codegraph semantic         # optional: rust-analyzer SCIP for resolved references (~5 min, ~3 GB RAM)
codegraph status           # freshness and size
```

Run `codegraph index` after pulling, merging, switching branches or editing; it is cheap when
little changed. Every answer starts with a freshness line and says `STALE` when files changed since
the last index.

| Operation | CLI | MCP tool |
|---|---|---|
| find_symbol(query) | `codegraph symbol <query> [--kind fn]` | `find_symbol` |
| find_references(symbol) | `codegraph refs <symbol>` | `find_references` |
| find_callers(symbol) | `codegraph callers <symbol>` | `find_callers` |
| find_dependencies(module) | `codegraph deps <package\|file\|module>` | `find_dependencies` |
| find_implementations(trait) | `codegraph impls <trait>` | `find_implementations` |
| get_symbol_context(symbol) | `codegraph context <symbol> [--lines 40]` | `get_symbol_context` |
| get_change_impact(files_or_symbols) | `codegraph impact <file\|symbol>...` | `get_change_impact` |
| get_task_context(query, max_tokens) | `codegraph task-context "<words>" [--max-tokens 2000]` | `get_task_context` |
| task memory | `codegraph task new\|add\|done\|status\|validate\|show\|list` | `task_get`, `task_list`, `task_record` |

A symbol is a sid (`rust:figcraft_cloud_storage::sessions::rotate_session`), a qualified name or
suffix (`Session::rotate`, `figcraft_collab_net::Backend`) or a bare name; an ambiguous name returns
the candidates (or pass `--all` / `all: true`). Results are compact text by default (`--json` /
`format: "json"`), 20 per page (`--limit`, at most 200; `--offset` for the next page).

Task records (`.codegraph/tasks.db`) are separate from the code graph and survive rebuilds:

```sh
codegraph task new S5-003 "Room leases in PostgreSQL"
codegraph task add S5-003 symbol "lease acquisition" --ref figcraft_cloud_storage::room_leases::acquire_room
codegraph task add S5-003 todo "drain test"
codegraph task validate S5-003     # records HEAD as last validated
codegraph task show S5-003         # symbol refs are located in the current index
```

### Agents

`.mcp.json` at the repository root registers the server for MCP clients that read it (Claude Code
asks before enabling it). Elsewhere, configure a stdio server running
`cargo run --quiet --release --manifest-path tools/codegraph/Cargo.toml -- mcp` from the repository
root (Codex: `[mcp_servers.codegraph]` with that `command`/`args` in `~/.codex/config.toml`). Build
once first so the first start is not a compile. The MCP tools never index, write files or run
commands; `task_record` writes only to the task store. Pass `require_fresh: true` to get an error
instead of an answer from a stale index.

## How it works

Git changes → tree-sitter (syntax) and SCIP (semantic) indexers → SQLite graph (FTS5) → CLI / MCP.

- **Files**: `git ls-files` (tracked + untracked-not-ignored), so `target/`, `node_modules/` and
  anything ignored never enter; `@generated` / `DO NOT EDIT` files and files over 1 MiB are skipped.
- **Changes**: size + mtime, then a BLAKE3 content hash; only changed files are re-parsed, files
  that vanished are dropped (a rename is a delete plus an add). Branch switches and merges are just
  changed files. A change in the set of packages re-parses everything once (module paths change).
- **Atomicity**: one `BEGIN IMMEDIATE` transaction per run; readers (WAL) see the previous or the
  next generation, never a partial one. Concurrent indexers serialise; a corrupt or incompatible
  database is moved aside and rebuilt.
- **Syntax layer** (tree-sitter: Rust, TypeScript/TSX/JS): definitions with qualified names,
  signatures, doc comments and ranges; name occurrences (calls, method calls, types, paths, macros,
  and `f(…)` / `.m(…)` inside macro arguments); `use` / `import` paths; `impl Trait for Type` and
  `implements`. All of it is labelled `syntax`: a match by name, never claimed resolved.
- **Semantic layer** (optional): `rust-analyzer scip` (or any SCIP file, e.g. from `scip-typescript`
  via `codegraph semantic --scip file`). Definitions get SCIP identities and references get exact
  targets (`scip`). SCIP data is attached only to files whose content the SCIP run saw; when a file
  changes, its SCIP rows are dropped and its references fall back to labelled name matches, while
  definitions that keep their qualified name keep their identity.
- **No stale edges**: cross-file relationships are stored as names or SCIP symbol ids and joined at
  query time, so changing one file never leaves a dangling edge in another.

Limits: Rust modules are derived from file paths (`#[path]` and re-exports through `pub use` are not
followed); implementations are matched by trait name; calls through trait objects and function
pointers are not resolved without SCIP; TypeScript has no semantic layer unless a SCIP file is
imported.

## Benchmark

`codegraph bench [--out report.md]` measures indexing in a throwaway worktree, compares `git grep`
with the graph on the same questions, and scores reference accuracy against SCIP. Results on this
repository: [BENCHMARK.md](BENCHMARK.md).

## Develop

```sh
cd tools/codegraph && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Tests build throwaway Git repositories and cover discovery, cross-file references, incremental
updates, deletion and rename, branch switching, merges, stale-edge removal, limits, concurrent
indexing and querying, corruption, task records and SCIP import. CI: `.github/workflows/codegraph.yml`.
