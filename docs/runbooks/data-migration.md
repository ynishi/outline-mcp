# Data migration — moving a pre-Postgres shelf into the database

Up to 0.14, outline-mcp stored each book as files in a shelf directory
(default `~/.config/outline-mcp/books/`). The server now reads only Postgres;
`outline-mcp import-shelf` copies a shelf into it once. This runbook covers a
local shelf and a shelf on a Fly volume.

## 0. Source layout

```
<shelf>/
├── <slug>.json                        # book body (1 file per book)
├── <slug>.events.db (+ -wal / -shm)   # event log (history + snapshots), 0.10–0.14
├── <slug>.snap.<millis>.json          # snapshot dumps
├── <slug>.snap.<millis>.meta.json     # snapshot label sidecar (when labeled)
└── <slug>.changelog.json              # older history sidecar, may be absent
```

The importer reads all of them. History is read entry by entry rather than
by replaying the event log, so a book whose log no longer replays (the
`patch application failed` warning, issue #2) still imports with its full
history.

## 1. Stop every writer of the shelf

The shelf must not change while it is copied, and nothing should keep
writing to it afterwards — once imported, the database is the only copy that
counts.

```sh
pgrep -fl outline-mcp
# macOS daemon, if installed:   launchctl bootout gui/$(id -u)/io.github.ynishi.outline-mcp
# Linux daemon, if installed:   systemctl --user stop outline-mcp
```

Close the MCP client sessions that spawn stdio servers, too. Hosts that
embed the crate and open the shelf in-process leave no trace in `pgrep`;
close their sessions as well.

## 2. Get the shelf onto the machine that runs the import

**Local shelf**: nothing to do; use its path below.

**Shelf on a Fly volume** (a deploy from before Postgres, machine still
running the old image):

```sh
fly ssh console --app <your-app-name> -C "tar -czf /data/books-export.tgz -C /data books"
fly ssh sftp get /data/books-export.tgz ./books-export.tgz --app <your-app-name>
mkdir -p ./shelf-export && tar -xzf ./books-export.tgz -C ./shelf-export
# the shelf is now ./shelf-export/books
```

Stop clients from writing to the old deployment before taking the archive;
writes made after it are not in the import.

Copy the directory as a whole, as the `tar` above does. Each
`<slug>.events.db` has `-wal` / `-shm` siblings, and the `-wal` file can
hold history that has not been written back into `.events.db` yet; a copy
that picks only `*.json` and `*.events.db` can import with entries missing,
and the comparison table cannot notice, because it compares the import with
that same copy.

## 3. Migrate and import

```sh
export OUTLINE_MCP_DATABASE_URL="postgres://<user>:<password>@<host>:5432/<database>"
# for a Fly Postgres app, through `fly proxy 15433:5432 --app <db-app>`:
#   postgres://<user>:<password>@127.0.0.1:15433/<database>

outline-mcp migrate
outline-mcp import-shelf --shelf <shelf-dir>               # every book
outline-mcp import-shelf --shelf <shelf-dir> --slug rust   # one book
```

The importer prints one row per book and a summary:

```
slug         result  nodes(src/db)  hash  history(src/db)  snapshots(src/db)
development  ok      1121/1121      same  5110/5110        0/0
...
29 book(s): 29 matched, 0 failed or differ
```

**Expect**: every row `ok` / `same`, equal counts on both sides, and exit 0.
Any book that fails or differs makes the command exit non-zero; its row says
which comparison did not match.

A slug that already exists in the database is refused. Re-run with
`--replace` to overwrite it — for example after fixing a source file, or to
take a fresher copy of a shelf that was still being written.

## 4. Point clients at the new server

Every host that used the shelf, locally or through the old deployment, now
talks to the database-backed server: start `outline-mcp` with
`OUTLINE_MCP_DATABASE_URL`, or register the HTTP endpoint (see
`fly-io-deploy.md` §7). An embedder or stdio server still pointed at the old
shelf path refuses to start and points at `import-shelf`.

## 5. Keep the source until you are sure

The importer only reads the shelf. Keep the shelf directory (and, for Fly,
the old volume) until the imported books have been in use for a while; it is
the fallback if something turns out to be missing.
