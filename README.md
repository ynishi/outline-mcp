# outline-mcp

Tree-structured knowledge base as an [MCP](https://modelcontextprotocol.io/) server.

LLM sessions are ephemeral. **outline-mcp** gives them a persistent, editable knowledge tree — sections and content nodes that can be browsed (`toc`), annotated with properties, and evolved across sessions. Nodes with `inject=true` are automatically included in session context.

## Quick Start

Books live in a Postgres database you run yourself. outline-mcp is a personal project published as-is: there is no hosted instance and no bundled database.

```bash
cargo install --path crates/outline-mcp

export OUTLINE_MCP_DATABASE_URL=postgres://user:pass@host:5432/outline
outline-mcp migrate          # create / update the schema; run once per upgrade
outline-mcp                  # stdio
```

The server refuses to start when no database is configured, when the database cannot be reached (the message says whether the host is unreachable, the login was rejected, or the database does not exist), or when `migrate` has not been run against it. `--database-url <url>` overrides the environment variable on every subcommand.

### Claude Code (`~/.claude.json`)

```json
{
  "mcpServers": {
    "outline": {
      "command": "outline-mcp",
      "env": { "OUTLINE_MCP_DATABASE_URL": "postgres://user:pass@host:5432/outline" }
    }
  }
}
```

### Shared server (many devices)

`--mcp-http` serves the same tools over MCP's streamable HTTP transport. Every write is one database transaction with a per-book version check, so several server processes or devices can share one database.

```bash
outline-mcp --mcp-http                          # 127.0.0.1:8486
outline-mcp --mcp-http --bind 127.0.0.1:9000
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
OUTLINE_MCP_HTTP_TOKEN=<token> outline-mcp --mcp-http --bind 0.0.0.0:8486
```

Every request must then carry `Authorization: Bearer <token>` — set it wherever your MCP client configures request headers. TLS termination is a reverse-proxy concern. Logs go to stderr in both transports; stdout stays reserved for the stdio protocol channel.

Service templates for running this as a daemon live in `contrib/systemd/` and `contrib/launchd/` (both take the database URL from their environment block). `docs/runbooks/` covers hosting on Fly.io with an unmanaged Fly Postgres app (`fly-io-deploy.md`, config in `contrib/fly/fly.toml`), importing a pre-Postgres shelf (`data-migration.md`) and an end-to-end smoke checklist (`multi-device-smoke.md`).

## Workflow

```
shelf  →  select_book  →  toc  →  node_create / node_update / node_move
                                   node_batch_move / node_batch_update / node_query
                                   checklist / import / init / book_update / book_delete / gen_routing
                                   snapshot_create / snapshot_list / snapshot_restore / snapshot_delete
                                   node_history / dump
```

1. **`init`** — Create a new empty book
2. **`node_create`** — Add sections and content nodes (with optional `properties`)
3. **`toc`** — View the table of contents with numbered IDs (e.g. `1`, `2-3`). Supports `filter` by properties
4. **`select_book`** — Select a book. Nodes with `inject=true` property have their body auto-appended (draft nodes excluded)
5. **`checklist`** — Export a section (or the whole book) as a Markdown checklist with checkboxes
6. **`node_update`** — Edit title, body, type, placeholder, properties, or status (`active`/`draft`) of a node
7. **`node_move`** — Relocate or delete nodes (with descendants)
8. **`node_batch_move`** — Move multiple nodes in a single atomic call (requires UUID or UUID-prefix IDs; to delete a node, use `node_move` with `action: "remove"`)
9. **`node_batch_update`** — Update title/body/type/properties/status on multiple nodes atomically
10. **`node_query`** — Search nodes by property values, status (`active`/`draft`), or type (`section`/`content`); optionally include body in results
11. **`import`** — Import a book from a previously exported JSON file
12. **`gen_routing`** — Generate a Markdown routing table from nodes with `routing` property across all books
13. **`snapshot_create`** / **`snapshot_list`** / **`snapshot_restore`** / **`snapshot_delete`** — Full book versioning (create, list, restore, delete one snapshot; a labeled snapshot needs `force: true` to delete)
14. **`node_history`** — View per-node change log with before/after diffs
15. **`dump`** — Export full book as JSON file
16. **`book_delete`** — Delete a book with its nodes, history and snapshots (cannot be undone). The slug is required; a book with any nodes, history or snapshots needs `force: true`, and `dry_run: true` only reports what would be removed. A write to the book between the check and the delete makes the delete fail without removing anything
17. **`book_update`** — Change a book's title and / or `max_depth` (slug required). Lowering `max_depth` below an existing node's depth is refused and names that node; the change appears in `book_history` as a `book_update` entry

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
│       │   └── store.rs  # BookStore / SnapshotStore ports
│       ├── application/  # Use cases
│       │   ├── service.rs # BookService (one commit per write)
│       │   └── eject.rs  # EjectService (Markdown/JSON export & import)
│       └── infra/        # Persistence
│           ├── pg_store.rs     # Postgres adapter (sqlx); migrations/ holds the schema
│           ├── memory_store.rs # In-memory adapter (tests)
│           └── shelf_import.rs # Reader for pre-Postgres shelves (import-shelf)
├── outline-mcp-rmcp/     # MCP layer (rmcp; stdio + streamable HTTP)
│   └── src/
│       ├── server.rs     # Transports (run / run_http) + server type
│       ├── tools.rs      # Tool handlers
│       └── resources.rs  # Resource handlers
└── outline-mcp/          # Binary crate (CLI entry point)
    └── src/
        ├── main.rs       # Flag parsing (--database-url / --mcp-http / --bind), transport dispatch
        ├── db.rs         # Database URL resolution and connection errors
        ├── migrate_db.rs # `migrate` subcommand
        └── import_shelf.rs # `import-shelf` subcommand
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

### From a shelf directory (0.14 or earlier)

Earlier versions kept each book as `{slug}.json` in a shelf directory, with history in `{slug}.changelog.json` / `{slug}.events.db` and snapshots in `{slug}.snap.*.json`. The server no longer reads that directory; import it once into the database:

```
outline-mcp migrate
outline-mcp import-shelf --shelf <shelf-dir>            # every book
outline-mcp import-shelf --shelf <shelf-dir> --slug rust  # one book
```

For each book the importer copies the tree, the change history and the snapshots with their original timestamps, then reads the book back from the database and prints one row per book comparing source and database: node count, a hash over every node's content and position, history entries and snapshots. It exits non-zero if any book fails or differs. History is read entry by entry rather than by replaying the event log, so a book whose event log no longer replays (the warning described in #2) still imports with its full history.

A slug that already exists in the database is refused; `--replace` overwrites it. The shelf directory is only read, never modified — keep it until you have checked the report.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
