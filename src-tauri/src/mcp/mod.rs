//! MCP server for AI agents: `lowquery mcp` speaks the Model Context
//! Protocol over stdio, so Claude, Cursor, VS Code and other clients can
//! list the connections a user opened to agents, read schemas and run SQL
//! within each connection's [`AiAccess`](crate::model::AiAccess) level.
//!
//! - `server.rs`: JSON-RPC over stdio, agent sessions, limits.
//! - `tools.rs`: tool definitions and handlers.
//! - `guard.rs`: one statement per call (a CQL batch counts as one), read-only
//!   classifier, Redis lists.
//! - `format.rs`: result tables, output caps, error text.
//! - `catalog.rs`: per-engine search (Cassandra: `system_schema`), foreign keys and
//!   execution plans.
//! - `audit.rs`: the activity log shown in the app.
//! - `e2e_tests.rs`: the server against real servers, one test per engine.

pub mod audit;
pub mod catalog;
#[cfg(test)]
mod e2e_tests;
mod format;
mod guard;
mod server;
mod tools;

use std::path::PathBuf;

/// Folder with `connections.json` and the activity log.
pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "LowBloat", "LowQuery")
        .expect("user configuration directory unavailable")
        .config_dir()
        .to_path_buf()
}

/// Entry point of `lowquery mcp`: serves stdin/stdout until stdin closes.
pub fn run() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    server::serve(config_dir());
}
