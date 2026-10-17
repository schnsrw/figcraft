//! codegraph: an incremental code-intelligence graph for coding agents (developer tooling).
//!
//! Git changes -> tree-sitter (syntax) and SCIP (semantic) indexers -> SQLite graph -> CLI / MCP.
//! See README.md.

pub mod bench;
pub mod db;
pub mod indexer;
pub mod inventory;
pub mod mcp;
pub mod query;
pub mod scip_import;
pub mod syntax;
pub mod tasks;
