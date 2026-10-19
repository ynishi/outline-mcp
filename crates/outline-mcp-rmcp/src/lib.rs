#![warn(missing_docs)]

//! # outline-mcp-rmcp
//!
//! ## Architecture
//!
//! `outline-mcp-rmcp` is the MCP transport layer for outline-mcp: it wires
//! `outline-mcp-core`'s `BookService` / `EjectService` onto the Model
//! Context Protocol via the `rmcp` crate (stdio transport, `#[tool_router]`
//! dispatch, and `resources/list` / `resources/read` for bundled guides).
//! It has no `main` of its own; the `outline-mcp` binary crate is a thin
//! wrapper that constructs [`OutlineMcpServer`] and drives it over stdio.
//!
//! ## Design
//!
//! - `server`: [`OutlineMcpServer`] — holds the [`Storage`] (the
//!   `BookStore` / `SnapshotStore` the books live in) and the currently
//!   selected book, and implements `ServerHandler`.
//! - `tools`: the `#[tool]`-annotated MCP tool handlers (node CRUD,
//!   TOC/checklist, snapshot/history, batch operations, query).
//! - `request`: MCP request DTOs (`schemars::JsonSchema` + `serde`) and
//!   their validation helpers.
//! - `helpers`: hierarchical-ID (`toc` numbering) bookkeeping shared by
//!   `server` and `tools`.
//! - `resources`: bundled Markdown guides exposed via `outline://guides/*`.
//!
//! Consumers that only need to run the server as-is should build a
//! [`Storage`] and call [`run`] / [`run_http`]. Consumers that want to embed
//! the server directly (e.g. as part of a larger MCP host) can construct
//! [`OutlineMcpServer`] and drive it with any `rmcp` transport.

mod health;
mod helpers;
mod request;
mod resources;
mod revision;
mod server;
mod tools;

pub use server::{run, run_http, OutlineMcpServer, Storage};
