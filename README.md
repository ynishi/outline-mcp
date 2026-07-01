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

The repository is a Cargo workspace with two crates: an rmcp-independent SDK (`outline-mcp-core`) and the MCP server binary (`outline-mcp`).

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
│           └── snapshot.rs
└── outline-mcp/          # Binary crate (MCP server, depends on outline-mcp-core)
    └── src/
        ├── main.rs       # Entry point
        └── interface/
            └── mcp/      # MCP handlers (rmcp, stdio)
```

Downstream applications that want to embed the tree / snapshot / changelog logic without pulling `rmcp` can depend on `outline-mcp-core` directly:

```toml
[dependencies]
outline-mcp-core = "0.7"
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

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
