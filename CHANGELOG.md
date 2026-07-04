# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.10.0] - 2026-07-04

### Added

- **ai-store integration for the snapshot subsystem.** `SnapshotService` now persists change history to an `ai-store` `Store` (backed by `ai-store-sqlite`) on a dedicated per-book stream, with `SnapshotDumpSink` maintaining the same `{slug}.snap.{millis}.json` (+ optional `.meta.json` sidecar) disk layout as before. A new SQLite file per book (`{shelf_dir}/{slug}.events.db`) holds the event log; the pre-existing on-disk `.snap.*.json` files stay in place and remain interoperable.
- **`outline-mcp migrate-snapshots --shelf <path> [--slug <slug>]`** CLI sub-command to backfill pre-integration disk snapshots into the new event log with their original timestamps preserved (via `Store::import_event`). Idempotent (safe to re-run), refuses to touch a stream that already carries events from a different clock (mixed-clock stream risk). See "Upgrading from 0.9.1 or earlier" in the README.
- **Startup orphan-snapshot warning.** On first access to a slug, the server emits `tracing::warn!` if disk snapshots exist that are not yet in the event log, pointing at the `migrate-snapshots` command. Note: this warning goes to `stderr`; MCP clients that swallow server stderr (Claude Code included) will not surface it — the migration command is the reliable way to check.
- **`AiStoreChangeLogRepository`** as a sibling `ChangeLogRepository` implementation over `ai-store` (not yet live in the production wiring — see "Not wired" below).

### Changed

- **`BookRepository` and `ChangeLogRepository` are now `#[async_trait]`.** The full handler → service → repository chain runs on `tokio` end-to-end; the previous sync/async bridge is gone. All infra implementations (`JsonBookRepository`, `JsonChangeLogRepository`, `AiStoreChangeLogRepository`) migrate to `async fn` with `tokio::fs` for file I/O. Fixed an incidental `RwLockReadGuard`-across-`.await` `Send` bug in `shelf()` surfaced by the migration.
- **`AiStoreChangeLogRepository::load_by_node`** uses `Store::read_by_meta("node_id", ...)` for sub-linear lookups on backends that index meta (SQLite); mem/fileproj remain linear scan with no regression.
- **New dependencies**: `ai-store-core = "0.4"`, `ai-store-sqlite = "0.4"`, `ai-store-sync = "0.4"`, `async-trait = "0.1"`, `json-patch = "4"`, `tracing`, `tracing-subscriber` (stderr-only; MCP stdout stays clean). `tokio` gains the `fs` feature.

### Migration

Consumers with existing on-disk snapshots (`.snap.{millis}.json` files) must run the migrator once after upgrading:

```
outline-mcp migrate-snapshots --shelf <path-to-shelf-directory>
```

Snapshots that have not been migrated remain on disk but are invisible to `snapshot_list` / `snapshot_restore` until the migrator has folded them into the event log. Back up the shelf directory before upgrading. See README §Upgrading.

### Known limitations

- Post-hoc labeled snapshots (labels attached via `snapshot_tag` after `snapshot_create`) lose the "time the label was attached" value in the sidecar `.meta.json`'s `created_at` field; on migration this collapses to the snapshot's own timestamp. The label text itself is preserved, and `created_at` is not exposed through any MCP tool — this is an internal-metadata cosmetic drift.
- `AiStoreChangeLogRepository` is not yet wired into `server.rs`; `JsonChangeLogRepository` remains the live changelog writer. Live swap requires async DI across ~20 unrelated handlers and is deferred.

### Deprecated

### Removed

### Fixed

### Security

## [0.9.1] - 2026-07-02

### Changed

- **`rmcp` dependency**: bump `0.15` → `1.7` (Cargo resolves to `1.8.0`). Adapt to `#[non_exhaustive]` API in rmcp 1.x by switching `ServerInfo` / `Implementation` / `ReadResourceResult` construction from struct literals to their `new()` + `with_*()` builders (`crates/outline-mcp-rmcp/src/{server,resources}.rs`). No behavior change; MCP protocol version and stdio JSON-RPC surface are unchanged.

## [0.9.0] - 2026-07-02

### Added

- **`outline-mcp-rmcp` crate**: new library crate holding the rmcp (MCP) interface layer — `OutlineMcpServer` (`ServerHandler` impl), the 21 `#[tool]` handlers, MCP request DTOs, and bundled `outline://guides/*` resources. Consumers that want to embed the outline-mcp server directly (e.g. as part of a larger MCP host) can depend on `outline-mcp-rmcp` and construct `OutlineMcpServer` without going through the `outline-mcp` binary.

### Changed

- **`outline-mcp` binary crate**: reduced to a thin entry point (~20 lines) that resolves the shelf directory from argv/env and calls `outline_mcp_rmcp::run`. The previous `interface::mcp` module (struct, tool_router, request/helpers/resources) moved to `outline-mcp-rmcp`. CLI arguments and the stdio JSON-RPC protocol are unchanged.
- **`docs/guides/`**: relocated from `crates/outline-mcp/docs/guides/` to `crates/outline-mcp-rmcp/docs/guides/`, alongside the `resources.rs` module that bundles them via `include_str!`.

### Deprecated

### Removed

### Fixed

### Security

## [0.8.1] - 2026-07-01

### Fixed

- **Publish tarball path resolution**: relocate `docs/guides/` from repo root into `crates/outline-mcp/docs/guides/` so the `include_str!` inside `resources.rs` resolves inside the published crate tarball. Previously `cargo publish -p outline-mcp` failed at the verify step because the workspace-root `docs/` directory was not part of the bin crate's package. Docker build simplified accordingly (`COPY docs ./docs` removed; `docs/` is now included via `COPY crates ./crates`).

## [0.8.0] - 2026-07-01

### Added

- **`outline-mcp-core` SDK crate**: new `rmcp`-independent library crate exposing `domain` / `application` / `infra` for embedding the Outline tree / snapshot / changelog logic in downstream applications. Root crate `#![warn(missing_docs)]` with crate-root ArchDoc narrative.

### Changed

- **Workspace layout**: repository split into a Cargo workspace with two members — `crates/outline-mcp-core` (SDK) and `crates/outline-mcp` (binary). Common metadata and dependencies unified under `[workspace.package]` / `[workspace.dependencies]`. The `outline-mcp` binary crate name and CLI entry point are unchanged.
- **Dockerfile**: build context copies `crates/` instead of `src/` to match the new layout.

### Deprecated

### Removed

### Fixed

### Security

### Breaking

- Library consumers previously importing types via the `outline_mcp` crate (e.g. `use outline_mcp::domain::model::TemplateBook`) must switch to `outline_mcp_core` (`use outline_mcp_core::domain::model::TemplateBook`). The binary CLI, `outline-mcp` crate name on crates.io, and MCP transport surface are unaffected.

## [0.7.0] - 2026-07-01

### Added

- **Snapshot inspection tools**: `snapshot_dump` / `snapshot_dump_all` / `snapshot_tag` for reading and labeling snapshot contents without restoring
- **`book_history`**: whole-book edit timeline aggregating per-node history into a single chronological view
- **Snapshot workflow guide as MCP Resource**: expose the snapshot operational guide via `outline://guides/snapshot` so clients can discover the recommended flow

### Changed

- Fix `uninlined_format_args` Clippy lint in `EjectService::render_node` (`src/application/eject.rs`): inline `indent`/`converted`/`ph` variables into format strings (no behavior change)

### Fixed

- Encode OCI package version in identifier per MCP Registry schema

## [0.6.0] - 2026-04-12

### Added

- **Batch operations**: `node_batch_move` and `node_batch_update` for applying multiple mutations in a single call
  - `node_batch_move` — move or delete multiple nodes atomically (all-or-nothing: saves only when all moves succeed)
  - `node_batch_update` — update title/body/type/properties/status on multiple nodes atomically
  - Both tools require UUID or UUID-prefix IDs (hierarchical toc IDs are intentionally unsupported to avoid positional drift)
- **`node_query`** — search nodes by property values, status, or node type; optionally include body in results
  - Supports `filter` (key-value property match), `status` (`active` / `draft`), `kind` (`section` / `content`)
  - `include_body=true` returns full node body alongside title, UUID, and properties

## [0.5.0] - 2026-03-25

### Added

- **History management**: snapshot, node_history, and dump tools for versioning and change tracking
  - `snapshot_create` / `snapshot_list` / `snapshot_restore` — full book versioning
  - `node_history` — per-node change log with before/after diffs
  - `dump` — export full book as JSON file
- **Node status**: `node_update` now supports `status` parameter (`active` / `draft`)
  - Draft nodes are excluded from `select_book` context injection
- `gen_routing` tool: generate Markdown routing tables from nodes with `routing` property across all books
  - `routing` property defines work scenarios (use `|` separator for multiple)
  - `routing_ref` property overrides default `§ID Title` reference text
  - Groups nodes with the same routing value into a single table row

## [0.3.0] - 2025-07-14

### Added

- Node properties: arbitrary key-value metadata on nodes (`properties` parameter)
- Context injection: nodes with `inject=true` have their body auto-included in `select_book` output
- Property tags in `toc` output: boolean properties shown as `[inject]`
- Property filtering in `toc`: `filter={"inject": "true"}`

## [0.2.3] - 2025-07-09

### Added

- `select_book` now auto-displays TOC on selection
- `quiet` option for `select_book` to suppress TOC output

## [0.2.2] - 2025-07-07

### Fixed

- Sanitize node titles for default checklist filenames (path traversal prevention, special character handling)

## [0.2.1] - 2025-07-06

### Changed

- Rewrite MCP server instructions to purpose-driven format

## [0.2.0] - 2025-07-05

### Added

- Initial public release
- Multi-book shelf management (`shelf`, `select_book`, `init`)
- Tree-structured nodes with sections and content (`node_create`, `node_update`, `node_move`)
- Table of contents with hierarchical numbered IDs (`toc`)
- Markdown and JSON export (`checklist`)
- JSON import (`import`)
- Node ID resolution: hierarchical IDs, UUID, prefix match, title substring match
