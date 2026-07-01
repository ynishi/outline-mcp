# Snapshot Workflow — Versioning a Book

This guide explains how to version a book across successive edits using
outline-mcp's snapshot tools. The core idea is:

- `snapshot_create` freezes the whole book at a point in time and can
  attach an optional **label** (e.g. `v01_draft`) that survives
  restarts.
- `snapshot_list` shows every frozen version, labels included.
- `snapshot_tag` lets you attach or overwrite a label on a snapshot
  after the fact.
- `snapshot_diff` produces a unified diff between two snapshots (label
  or timestamp headers).
- `snapshot_dump` / `snapshot_dump_all` render snapshots into a
  directory as `book.md` (or `book.json`) so external tooling like
  `diff -u` or an editor's diff view can consume them.
- `book_history` shows every edit (Create / Update / Move / Delete /
  Restore) in chronological order across all nodes.

Node-level edits are already fully recorded in the changelog store —
so if all you want is "see every change ever made", `book_history`
alone is enough. Snapshots exist for the cases where you also want an
atomic restore point, a rendered-Markdown diff between two versions,
or a copy you can hand off outside the shelf.

## Basic loop

The common flow is: **edit → snapshot with a label → edit → snapshot
with a label**. Each snapshot is a full-book copy, so links inside the
book are never dangling — restore always brings back a self-consistent
state.

```
1. Make edits with node_create / node_update / node_move
2. snapshot_create(label: "v01_draft")
3. Make more edits
4. snapshot_create(label: "v02_reviewed")
5. snapshot_list                     # verify both labels are stored
6. snapshot_diff(from_ts=<v01 ts>, to_ts=<v02 ts>)
```

`snapshot_list` prints newest first with timestamps in millis and ISO
form; the label (if any) appears in `[brackets]` at the end of each
line.

## Labeling after the fact

If you already took a snapshot without a label and later realise you
want to mark it, use `snapshot_tag`:

```
snapshot_tag(timestamp: "<millis from snapshot_list>", label: "v03_final")
```

This writes only the sidecar `.meta.json`; the snapshot body itself is
never touched. Labels have a 64-character upper bound and are
restricted to letters, digits, spaces, and `-_.:,()` — no path
separators, no newlines. Existing labels are overwritten.

## Diffing snapshots

`snapshot_diff` compares two snapshots as Markdown and returns a
unified diff. Rules:

- `from_ts` must be **strictly less than** `to_ts`. The tool rejects
  reversed input rather than silently swapping.
- The response is a JSON object with three top-level fields:
  `from` / `to` (each carries `timestamp` / `label` / `iso`) and
  `diff` (the unified diff string).
- The unified diff `---` / `+++` headers use the label when present,
  otherwise the timestamp. This means diffs stay readable even in a
  pager or editor that only shows the header.

`context_lines` (default 3) controls how many surrounding lines
appear around each hunk.

## Rendering snapshots to disk

`snapshot_dump` writes a single snapshot into a fresh subdirectory of
your choice; `snapshot_dump_all` walks every snapshot in the current
book and writes them into `vNN_<millis>` subdirectories (01 is the
oldest). Each subdirectory contains `book.md` (or `book.json` when
`format: "json"` is passed).

Because the layout is stable, downstream tooling can consume it
directly:

```
snapshot_dump_all(output_dir: "/tmp/mybook-snapshots")

# then in a shell
diff -u /tmp/mybook-snapshots/v03_*/book.md /tmp/mybook-snapshots/v04_*/book.md
```

The dump path never touches the live shelf — the book is
deserialised into an ephemeral in-memory copy and rendered from
there, so a dump run has no side effects on your working book.

If a target subdirectory already exists, the dump refuses to
overwrite it by default. Pass `overwrite: true` to force replacement
(the entire subdirectory is removed and rewritten).

## When to use `snapshot_diff` vs `snapshot_dump_all`

Both compare snapshots; the split is about where the diff lives:

- `snapshot_diff` is best when you want the diff **inside the MCP
  response** — a single call gives you the labels, timestamps, ISOs,
  and unified diff in one JSON payload.
- `snapshot_dump_all` is best when the diff needs to leave the MCP
  session — for example, feeding a shell `diff -u`, opening the two
  files side by side in an editor, or importing a specific
  intermediate version into another book with `import`.

## Full edit trail: `book_history`

`book_history` returns the raw changelog for the selected book,
newest first. Every Create / Update / Move / Delete / Restore is
listed with the ISO timestamp, the action, the node's hierarchical
id, and its current title.

Pair it with two timestamps from `snapshot_list` to answer "what
changed between v03 and v07?" without leaving the tool surface:

```
snapshot_list                          # copy the two millis values
book_history(since: "<v03 millis>", until: "<v07 millis>")
```

`limit` defaults to 50 and can be set to `0` to return everything in
range.
