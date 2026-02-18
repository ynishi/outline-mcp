# outline-mcp

Tree-structured session guide & checklist as an [MCP](https://modelcontextprotocol.io/) server.

LLM sessions are ephemeral. **outline-mcp** gives them a persistent, editable runbook — a tree of sections and content nodes that can be browsed (`toc`), exported as checklists (`checklist`), and evolved across sessions.

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
init  →  node_create  →  toc  →  checklist
         node_update
         node_move
         import
```

1. **`init`** — Create a new empty book
2. **`node_create`** — Add sections and content nodes
3. **`toc`** — View the table of contents with numbered IDs (e.g. `1`, `2-3`)
4. **`checklist`** — Export a section (or the whole book) as a Markdown checklist with checkboxes
5. **`node_update`** — Edit title, body, type, or placeholder of a node
6. **`node_move`** — Relocate or delete nodes (with descendants)
7. **`import`** — Import a book from a previously exported JSON file

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
