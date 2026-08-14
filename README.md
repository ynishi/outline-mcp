# outline-mcp

Tree-structured knowledge base as an [MCP](https://modelcontextprotocol.io/) server.

LLM sessions are ephemeral. **outline-mcp** gives them a persistent, editable knowledge tree — sections and content nodes that can be browsed (`toc`), annotated with properties, and evolved across sessions. Nodes with `inject=true` are automatically included in session context.

## Quick Start

```bash
cargo install --path .
```

### Claude Code (`~/.claude.json`)

#### Native binary (after `cargo install`)

```json
{
  "mcpServers": {
    "outline": {
      "command": "outline-mcp",
      "args": ["/path/to/your-book.json"]
    }
  }
}
```

#### Docker (no Rust toolchain required)

```json
{
  "mcpServers": {
    "outline": {
      "command": "docker",
      "args": [
        "run", "-i", "--rm",
        "-v", "/path/to/data:/data",
        "ghcr.io/ynishi/outline-mcp:latest",
        "/data/your-book.json"
      ]
    }
  }
}
```

If the path argument is omitted, defaults to `outline-book.json` in the current directory.

### Multi-device (one shelf, many devices)

`--mcp-http` serves the same tools over MCP's streamable HTTP transport, so several devices can share one shelf. A single process owns the directory — book JSON plus the per-slug `.events.db` — which keeps the single-writer storage model intact, so there is no sync or conflict resolution to configure.

```bash
outline-mcp /path/to/books --mcp-http                          # 127.0.0.1:8486
outline-mcp /path/to/books --mcp-http --bind 127.0.0.1:9000
```

Point clients at `/mcp` on that address:

```json
{
  "mcpServers": {
    "outline": {
      "type": "http",
      "url": "http://127.0.0.1:8486/mcp"
    }
  }
}
```

Binding beyond loopback requires a token. Startup is refused without one, before any listener is opened:

```bash
OUTLINE_MCP_HTTP_TOKEN=<token> outline-mcp /path/to/books --mcp-http --bind 0.0.0.0:8486
```

Every request must then carry `Authorization: Bearer <token>` — set it wherever your MCP client configures request headers. TLS termination is a reverse-proxy concern. Logs go to stderr in both transports; stdout stays reserved for the stdio protocol channel.

Service templates for running this as a daemon live in `contrib/systemd/` and `contrib/launchd/`. `docs/runbooks/` covers moving an existing shelf to a central host (`data-migration.md`) and verifying a deployment end to end (`multi-device-smoke.md`).

## Workflow

```
shelf  →  select_book  →  toc  →  node_create / node_update / node_move
                                   node_batch_move / node_batch_update / node_query
                                   checklist / import / init / gen_routing
                                   snapshot_create / snapshot_list / snapshot_restore
                                   node_history / dump
```

1. **`init`** — Create a new empty book
2. **`node_create`** — Add sections and content nodes (with optional `properties`)
3. **`toc`** — View the table of contents with numbered IDs (e.g. `1`, `2-3`). Supports `filter` by properties
4. **`select_book`** — Select a book. Nodes with `inject=true` property have their body auto-appended (draft nodes excluded)
5. **`checklist`** — Export a section (or the whole book) as a Markdown checklist with checkboxes
6. **`node_update`** — Edit title, body, type, placeholder, properties, or status (`active`/`draft`) of a node
7. **`node_move`** — Relocate or delete nodes (with descendants)
8. **`node_batch_move`** — Move or delete multiple nodes in a single atomic call (requires UUID or UUID-prefix IDs)
9. **`node_batch_update`** — Update title/body/type/properties/status on multiple nodes atomically
10. **`node_query`** — Search nodes by property values, status (`active`/`draft`), or type (`section`/`content`); optionally include body in results
11. **`import`** — Import a book from a previously exported JSON file
12. **`gen_routing`** — Generate a Markdown routing table from nodes with `routing` property across all books
13. **`snapshot_create`** / **`snapshot_list`** / **`snapshot_restore`** — Full book versioning (create, list, restore)
14. **`node_history`** — View per-node change log with before/after diffs
15. **`dump`** — Export full book as JSON file

### Node IDs

`toc` assigns human-friendly numbered IDs:

```
1. Coding Standards
  1-1. Naming Conventions
  1-2. Error Handling
2. Testing
  2-1. Unit Tests
  2-2. Integration Tests
```

These IDs (`1`, `1-2`, `2-1`, etc.) work in most tools. Full UUIDs and title substring matching are also supported as fallbacks.

> **Note**: `node_batch_move` and `node_batch_update` require UUID or UUID-prefix IDs. Hierarchical toc IDs are intentionally rejected to prevent positional drift when the tree is modified mid-batch.

### Node Properties

Nodes can have key-value properties for metadata:

```
node_create  title="My Rule"  properties={"inject": "true", "scope": "rust"}
```

- **`inject=true`** — Node body is automatically included in `select_book` output (context injection)
- **`routing=<scene>`** — Marks the node for `gen_routing` output. Use `|` to assign multiple scenes (e.g. `routing="testing|TDD"`)
- **`routing_ref=<text>`** — Overrides the default `§ID Title` reference in the routing table (e.g. `routing_ref="select_book で全体参照"`)
- Properties with value `"true"` appear as tags in `toc`: `1. My Rule [inject]`
- `toc` supports filtering: `filter={"inject": "true"}` shows only matching nodes
- Properties are preserved in JSON export/import

## Architecture

The repository is a Cargo workspace with three crates: an rmcp-independent SDK (`outline-mcp-core`), the MCP protocol layer (`outline-mcp-rmcp`), and the server binary (`outline-mcp`).

```
crates/
├── outline-mcp-core/     # SDK crate (library, no rmcp dependency)
│   └── src/
│       ├── domain/       # Core model (TemplateBook, TemplateNode, NodeId)
│       │   ├── model/    # Aggregate root + value objects
│       │   ├── error.rs  # Domain errors
│       │   └── repository.rs # BookRepository trait
│       ├── application/  # Use cases
│       │   ├── service.rs # BookService (CRUD)
│       │   └── eject.rs  # EjectService (Markdown/JSON export & import)
│       └── infra/        # Persistence
│           ├── json_store.rs # JSON file repository (atomic write)
│           ├── changelog_store.rs
│           ├── ai_store_changelog.rs
│           └── snapshot.rs
├── outline-mcp-rmcp/     # MCP layer (rmcp; stdio + streamable HTTP)
│   └── src/
│       ├── server.rs     # Transports (run / run_http) + server type
│       ├── tools.rs      # Tool handlers
│       └── resources.rs  # Resource handlers
└── outline-mcp/          # Binary crate (CLI entry point)
    └── src/
        ├── main.rs       # Flag parsing (--mcp-http / --bind), transport dispatch
        └── cli.rs        # `migrate-snapshots` subcommand
```

Downstream applications that want to embed the tree / snapshot / changelog logic without pulling `rmcp` can depend on `outline-mcp-core` directly:

```toml
[dependencies]
outline-mcp-core = "0.12"
```

## Export Formats

### Markdown (default)

```markdown
# My Runbook

## Design

- [ ] Define requirements
  > requirements list: ___
- [ ] API design
  REST endpoints
```

### JSON

Tree-structured format that can be re-imported:

```json
{
  "title": "My Runbook",
  "max_depth": 4,
  "nodes": [
    {
      "title": "Design",
      "node_type": "section",
      "children": [...]
    }
  ]
}
```

## Upgrading

### From 0.9.1 or earlier

The snapshot subsystem now persists to a per-book SQLite event log (`{shelf_dir}/{slug}.events.db`) in addition to the existing on-disk `.snap.{millis}.json` files. Existing installs must run the migrator once to fold pre-existing on-disk snapshots into the event log — until they do, those snapshots stay on disk but are not visible to `snapshot_list` / `snapshot_restore`.

**1. Back up the shelf directory.** The migrator is idempotent and does not delete files, but the shelf directory is the source of truth for your books; a copy is cheap insurance.

```
cp -a <shelf-dir> <shelf-dir>.bak
```

**2. Run the migrator.**

```
outline-mcp migrate-snapshots --shelf <shelf-dir>
```

The migrator scans every `{slug}.snap.{millis}.json` file under `<shelf-dir>`, imports each into `{shelf-dir}/{slug}.events.db` with its original timestamp preserved, and leaves the source `.json` file in place. Output looks like:

```
== rust ==
  scanned:  3
  imported: 3
  skipped:  0
  failed:   0
```

Pass `--slug <slug>` to migrate one book at a time.

**3. Verify (optional).** Re-running the migrator is a no-op — every file will report as `skipped`.

### What the migrator does not do

- It does not delete the source `.snap.*.json` files. Keep them for a while as a second layer of backup.
- It will refuse a stream that already carries events from a different clock (e.g. a book that has been actively edited via `snapshot_create` between the upgrade and the migrator run). Run the migrator before doing new writes.
- The startup warning that steers you here is emitted via `tracing::warn!` on `stderr`. MCP clients that swallow server stderr (Claude Code included) will not surface it — treat the migrator command as the canonical way to check.

### Known limitations

- Snapshots that were **post-hoc labeled** via `snapshot_tag` (as opposed to labeled at `snapshot_create` time) lose the "time the label was attached" value in their sidecar `.meta.json`'s internal `created_at` field. The label text itself is preserved, and `created_at` is never exposed through the MCP surface — this is an internal-metadata drift, not user-visible.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
