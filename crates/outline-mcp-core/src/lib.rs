#![warn(missing_docs)]

//! # outline-mcp-core
//!
//! ## Architecture
//!
//! `outline-mcp-core` provides the domain model, application services, and
//! JSON-file-backed infrastructure behind outline-mcp's tree-structured
//! knowledge books. This crate has no dependency on `rmcp` or any MCP
//! transport; it is a pure Rust SDK that any front end (MCP server, CLI, or
//! a future transport) can embed.
//!
//! ## Design
//!
//! - `domain`: pure types and invariants (`TemplateBook`, node/id/changelog
//!   types, domain errors) with no I/O.
//! - `application`: use-case orchestration (`BookService`, `EjectService`)
//!   over the `domain` repository traits.
//! - `infra`: JSON-file-backed implementations of the `domain` repository
//!   traits (book storage, changelog storage, snapshotting).
//!
//! The `outline-mcp` binary crate depends on this crate and layers the MCP
//! transport (its own `interface` module) on top.

/// Application-layer use cases (`BookService`, `EjectService`) orchestrating
/// the domain model over the `domain` repository traits.
pub mod application;
/// Pure domain model and invariants (no I/O): `TemplateBook`, node types,
/// IDs, changelog entries, and domain errors.
pub mod domain;
/// JSON-file-backed infrastructure implementing the domain repository
/// traits: book storage, changelog storage, and snapshotting.
pub mod infra;
