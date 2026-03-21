# outline-mcp

Tree-structured knowledge base as an [MCP](https://modelcontextprotocol.io/) server.

LLM sessions are ephemeral. **outline-mcp** gives them a persistent, editable knowledge tree — sections and content nodes that can be browsed (`toc`), annotated with properties, and evolved across sessions. Nodes with `inject=true` are automatically included in session context.

## Quick Start

```bash
cargo install --path .
```

### Claude Code (`~/.claude.json`)

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

If the path argument is omitted, defaults to `outline-book.json` in the current directory.

## Workflow

```
shelf  →  select_book  →  toc  →  node_create / node_update / node_move
                                   checklist / import / init
```

1. **`init`** — Create a new empty book
2. **`node_create`** — Add sections and content nodes (with optional `properties`)
3. **`toc`** — View the table of contents with numbered IDs (e.g. `1`, `2-3`). Supports `filter` by properties
4. **`select_book`** — Select a book. Nodes with `inject=true` property have their body auto-appended
5. **`checklist`** — Export a section (or the whole book) as a Markdown checklist with checkboxes
6. **`node_update`** — Edit title, body, type, placeholder, or properties of a node
7. **`node_move`** — Relocate or delete nodes (with descendants)
8. **`import`** — Import a book from a previously exported JSON file

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

These IDs (`1`, `1-2`, `2-1`, etc.) work in all tools. Full UUIDs and title substring matching are also supported as fallbacks.

### Node Properties

Nodes can have key-value properties for metadata:

```
node_create  title="My Rule"  properties={"inject": "true", "scope": "rust"}
```

- **`inject=true`** — Node body is automatically included in `select_book` output (context injection)
- Properties with value `"true"` appear as tags in `toc`: `1. My Rule [inject]`
- `toc` supports filtering: `filter={"inject": "true"}` shows only matching nodes
- Properties are preserved in JSON export/import

## Architecture

```
src/
├── domain/          # Core model (TemplateBook, TemplateNode, NodeId)
│   ├── model/       # Aggregate root + value objects
│   ├── error.rs     # Domain errors
│   └── repository.rs # BookRepository trait
├── application/     # Use cases
│   ├── service.rs   # BookService (CRUD)
│   └── eject.rs     # EjectService (Markdown/JSON export & import)
├── infra/           # Persistence
│   └── json_store.rs # JSON file repository (atomic write)
└── interface/       # Transport
    └── mcp.rs       # MCP server (rmcp, stdio)
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
