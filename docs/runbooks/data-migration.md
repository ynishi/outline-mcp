# Data migration — moving the shelf to the central daemon host

Procedure for migrating an existing outline-mcp shelf
(`~/.config/outline-mcp/books/`) from one host to the central `--mcp-http`
daemon host with plain `tar`. Mirrors mini-app-mcp's
`docs/runbooks/data-migration.md`; layout facts below are from
`outline-mcp-rmcp::server` / `outline-mcp-core::infra::snapshot_sink`.

## 0. Data layout

```
~/.config/outline-mcp/books/
├── <slug>.json                        # book body (1 file per book)
├── <slug>.events.db (+ -wal / -shm)   # ai-store event log (snapshots + changelog)
├── <slug>.snap.<millis>.json          # snapshot dumps (SnapshotOnlySink output)
├── <slug>.snap.<millis>.meta.json     # snapshot label sidecar (when labeled)
└── <slug>.changelog.json              # legacy sidecar (pre ai-store), may be absent
```

- The shelf is flat: **one book = one `<slug>.json` plus its same-slug
  sidecars**, so a single book can be migrated by copying its slug's files
  (see §5).
- `-wal` / `-shm` are SQLite WAL sidecars. Copying them is safe **only while
  no process has the DB open** — SQLite recovers from the WAL on next open.
  Never tar a live shelf.

## 1. Source host: stop every server and client

outline-mcp has **two consumer shapes**, both must be stopped:

```sh
# standalone daemon / stdio server, if any
pgrep -fl outline-mcp
# macOS daemon, if installed:
#   launchctl bootout gui/$(id -u)/io.github.ynishi.outline-mcp
# Linux daemon, if installed:
#   systemctl --user stop outline-mcp
```

- **stdio instances** are spawned by MCP clients (editor / agent sessions) —
  close those sessions.
- **In-process embedders leave no trace in pgrep.** A host can link this crate
  and open the shelf directory directly instead of spawning `outline-mcp`, so
  the checks above find nothing while the shelf is still being written
  (`local-develop-server` is one such host). Close every session of any such
  client on the source host too, or its in-process handle keeps writing the
  old inodes and those writes are lost after the move.

**Expect**: no `outline-mcp` process, and no live session of any in-process
embedder, on the source host.

## 2. Source host: archive

```sh
tar -C ~/.config/outline-mcp -czf /tmp/outline-books.tgz books
```

## 3. Transfer

```sh
scp /tmp/outline-books.tgz <central-host>:/tmp/
```

## 4. Central host: stop, set aside, extract, start

```sh
systemctl --user stop outline-mcp
mkdir -p ~/.config/outline-mcp
[ -d ~/.config/outline-mcp/books ] && mv ~/.config/outline-mcp/books ~/.config/outline-mcp/books.bak.$(date +%Y%m%d)   # keep, do not delete
tar -C ~/.config/outline-mcp -xzf /tmp/outline-books.tgz
systemctl --user start outline-mcp
```

Set the existing dir aside even when it is empty — a daemon that already ran
on this host has created the dir, and extracting into that mixed state is
asking for confusion.

Verify:

```sh
systemctl --user is-active outline-mcp
curl -s -o /dev/null -w "%{http_code}\n" -X POST http://127.0.0.1:8486/mcp \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"migrate","version":"0"}}}'
```

**Expect**: `active` and `200`. Then call `shelf` from any MCP client and
confirm every migrated book slug is listed, and `select_book` + `toc` on a
known book shows its nodes.

## 5. Single-book variant

```sh
# source (after §1 stop) — one slug + all its sidecars
tar -C ~/.config/outline-mcp/books -czf /tmp/<slug>.tgz \
  $(cd ~/.config/outline-mcp/books && ls <slug>.json <slug>.events.db* <slug>.snap.* 2>/dev/null)
# central host: stop daemon → set aside same-slug files → extract → start
tar -C ~/.config/outline-mcp/books -xzf /tmp/<slug>.tgz
```

The shelf is scanned per call, so a running daemon picks the new book up on
the next `shelf` — but the stop/start bracket keeps the WAL rule in §0 honest.

## 6. Post-migration checks and caveats

- **Ephemeral sidecars**: after the daemon reopens an `.events.db`, its
  `-shm` differs from the archived copy. Expected — regenerated shared
  memory, not data.
- **Legacy snapshot dumps**: if the source shelf predates the ai-store
  snapshot stream, import the on-disk `.snap.*.json` dumps into the event
  log once, on the central host:

  ```sh
  outline-mcp migrate-snapshots --shelf ~/.config/outline-mcp/books
  ```

  Idempotent per its timestamp contract; run it before trusting
  `snapshot_list` / `snapshot_restore` on migrated books.
- **Dangling symlinks**: books are normally real files, but check anyway —
  `find ~/.config/outline-mcp/books -type l ! -exec test -e {} \; -print`
  (expect: no output).
- **Old host becomes a client**: after migration the source host must reach
  the shelf through the central daemon only. Leaving an embedded outline (or
  a standalone stdio outline-mcp) pointed at the old local shelf means two
  writable copies — two diverging sources of truth. Switch the source host to
  the HTTP endpoint (via whatever remote-server setting the embedder exposes,
  or a direct MCP HTTP registration), or move the old shelf out of the
  default path so an accidental embed start finds nothing.
- **No node-level merge**: this procedure replaces files wholesale. If both
  sides accumulated distinct edits to the same book, merging is manual
  (snapshot dumps + eyeballs) — there is no merge subcommand. The `.bak`
  dir from §4 is the safety net; keep it until the migration is confirmed.
