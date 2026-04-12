# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

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
