# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- **Browser view search by id** — `/ui/` search also matches a UUID prefix (8 or more hex characters), and `#<toc number>` (for example `#2-3`) opens that node.
- **Browser view searches all books** — the shelf's search box now searches every book and groups the hits by book (no book picker; archived books are skipped unless `archived=1`), a book's search page is titled with the book and links to the all-books search, and a node page shows its toc number and short id as separately selectable snippets.
- **`node_locate`** — finds which book holds a node by its UUID or a prefix of at least 8 hex digits (the short id), across every book and without a selected book. Each hit is `<slug> "<book title>": <toc>. <title> (id: <uuid>, <status>)`; archived books are skipped unless `include_archived: true`. Toc numbers and titles are refused: they mean nothing outside one book.

### Changed

- **An id of another book is named, not reported missing** — every tool still resolves ids within the selected book, but a UUID prefix (8 or more hex digits) that names no node there and does name one elsewhere is refused with that book's slug, the node's toc number and title, and the `select_book` to run, instead of falling through to the title match (`resolve_id`) or `No node with UUID starting with` (`node_batch_move` / `node_batch_update`). Archived books are named too. A prefix no book has keeps the old message.

### Deprecated

### Removed

### Fixed

- **An all-digit short id resolves** (#37). `resolve_id` read a node reference made of digits as a toc number and, when no node sat at that position, refused it with `No node at position` — so a node whose short id happens to be all digits (about 2.3 % of them, `99102601` say) could not be referred to by its short id in `node_history`, `node_update`, `node_move`, `links`, `node_verify` and the other tools that resolve through it (the batch tools and the full UUID were unaffected). A toc number still wins when it names a node; when it names none and the string looks like a UUID prefix (8 or more hex digits), the UUID prefix lookup runs next. `2-3` or `17` with no node at that position keep the toc-position error.

### Security

## [0.15.2] - 2026-10-19

### Added

- **Read-only browser view at `/ui/`** (`--mcp-http` only): shelf, table of contents with inject / status / verification / pending-revision / link markers (archived subtrees hidden unless `?archived=1`), node pages (rendered Markdown body with raw HTML shown as text, properties, links and backlinks, pending revision diff, history), text search and book history. GET only; no script and no external assets (strict CSP, `no-store`, `no-referrer`). With `OUTLINE_MCP_HTTP_TOKEN` set it takes HTTP Basic with the token as the password; without one it serves loopback `Host`s only. `/mcp` and its bearer check are unchanged, and so is the answer for any path outside `/ui` (bearer 401 with a token set, 404 otherwise). Bodies over 1 MiB are shown as plain text rather than rendered; the shelf computes node counts for the first 200 books and a node page lists its newest 100 history entries.

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.15.1] - 2026-10-19

The library API change of `outline-mcp-core` listed under Changed (new public fields, a new `AppError` variant) is breaking for code that builds those structs with literals or matches `AppError` exhaustively, although this is a patch release.

### Added

- **`import` `new_ids`** (#22) — `new_ids: true` gives every imported node a fresh id and remaps links between imported nodes, for importing a copy next to the original or a file whose ids are not UUIDs.
- **`import` reports what it did** (#22): nodes created, updated and deleted (every node of the book that is not in the file is deleted, and the result says so), plus links and pending revisions it had to drop. A pending revision in the file that breaks the `revision_propose` rules (on an archived node, `properties` with `properties_patch`, or changing nothing) is dropped and reported instead of failing the import.
- **Subtree exports are marked and guarded** (#22). An export of one section (`subtree_root`) records it as `subtree_root: {id, title}`; `import` refuses such a file unless `force: true`, because it holds only that section's children and would replace the whole book with them. Subtree exports written by 0.15.0 or earlier carry no marker; when ids are kept, `import` recognizes them from their ids (a top-level node of the file sits under a section in the book) and refuses them the same way. Everything the import decides from the destination book (this guard, which nodes keep their stamp) comes from the same load the commit goes over, so a concurrent write makes the import fail instead of mixing two versions.

### Changed

- **Export / import keep a book's curated state** (#22). The JSON export (`dump`, `checklist` format json) now carries each node's `status`, `updated_at`, `verified_at` / `verified_by`, `links` and pending `proposal` (each left out when empty, so active nodes and files from earlier versions look as before), and `import` restores them. Before, a round trip turned archived and draft nodes active, forgot verifications, and dropped every link and pending revision.
- **`import` keeps node ids** (#22). The ids in the file are now kept (they must be UUIDs, unique in the file; otherwise the call fails and points at `new_ids`), so re-importing a book's own export records history only for nodes that differ — an unchanged export records nothing. Files exported by 0.15.0 or earlier (no new fields) already carry UUID ids and import as is. They have no `updated_at`: a node keeps the stamp of the same-id node in the book it is imported into only when it comes out identical to that node, and is stamped now otherwise. They also cannot carry archived status, verification, links or pending revisions, so re-importing one into its own book reverts those (archived nodes become active, verification, links and revisions are cleared) and records an update for each node that had any of them; it records nothing only for nodes that had none. An id is matched by its UUID value, so the same UUID spelled differently (case, braces, `urn:uuid:`) is a duplicate. Links whose target is not in the file are dropped and listed in the result. Import errors are reported as invalid parameters rather than internal errors. The default `checklist` export leaves archived nodes out, so importing it deletes them; use `dump`, or `checklist` with `include_archived: true`, for a full round trip.
- **JSON exports include `updated_at` for every node** (#22), along with the other new optional fields, so programs reading `dump` / `checklist` JSON see more keys (nothing removed or renamed).
- **Library API (`outline-mcp-core`)**: `EjectTreeNode` and `EjectTree` gained public fields (`status`, `updated_at`, `verified_at`, `verified_by`, `links`, `proposal`; `subtree_root`) and `AppError` gained `ImportInvalid` — struct literals without `..Default::default()` and exhaustive matches on `AppError` need updating. `EjectService::import_tree` now restores status, verification, links and pending revisions too, but still gives every node a new id (as before) and drops links to nodes outside the file; `EjectService::import_tree_with` keeps ids, and `EjectService::import_tree_onto` additionally keeps the destination's `updated_at` for nodes the file has none for. `BookService::import_book_counted` returns the created / updated / deleted counts.

### Deprecated

### Removed

### Fixed

- **Dependency `chacha20` 0.10.1 → 0.10.2.** 0.10.1 was yanked from crates.io; it came in through `rand` 0.10 (used by `rmcp` and `sqlx-postgres`). Lockfile only.

### Security

## [0.15.0] - 2026-10-19

### Added

- **Fly.io hosting** — `contrib/fly/fly.toml` (stateless machine, edge TLS, bearer auth via `OUTLINE_MCP_HTTP_TOKEN`, database URL from the `OUTLINE_MCP_DATABASE_URL` secret, `--bind 0.0.0.0:8486`; no volume, so the machine may stop when idle) and `docs/runbooks/fly-io-deploy.md` (an unmanaged Fly Postgres app in the same org, `fly postgres attach` for the secret, `migrate` and `import-shelf` through `fly proxy`, deploy, smoke, client registration, rollback, backup by volume snapshots of the database app plus `pg_dump`). Verified against a real deploy of the Postgres layout (nrt): 401 without a token / 200 with one, the 29 imported books listed, writes recorded in history.
- **`.dockerignore`** — the Dockerfile only COPYs `Cargo.toml` / `Cargo.lock` / `crates/`, but the build context uploaded everything (`target/` alone is multiple GB, re-uploaded on every `fly deploy` / `docker build`).
- **`BookStore` storage port with a Postgres adapter** (groundwork for #2) — one write commits the book state and its change events together, so a write whose history entry cannot be recorded is not persisted. Ships a Postgres adapter (sqlx 0.9; `books` / `nodes` / `events` tables, optimistic locking on a per-book version, only changed node rows rewritten), an in-memory adapter for tests, and an `outline-mcp migrate [--database-url <url>]` subcommand reading `OUTLINE_MCP_DATABASE_URL` that exits with a message naming the variable when no database is configured.
- **`outline-mcp import-shelf --shelf <dir> [--slug <slug>] [--replace]`** — imports a pre-Postgres shelf (book JSON, `{slug}.changelog.json`, the per-book event log, `{slug}.snap.*.json`) with original timestamps, then reads each book back and prints a per-book comparison (node count, content hash, history entries, snapshots), exiting non-zero on any mismatch. History is read entry by entry instead of replaying the event log, so books whose log no longer replays (#2) import with their full history. Checked on a 29-book shelf, including a 1121-node book with 5110 history entries: 29 matched.
- **`book_delete`** (#8) — deletes a book with its nodes, history and snapshots, in one statement on Postgres; on other storage adapters the book and its snapshots are removed in two steps. The slug is required (the selected book is never deleted implicitly); a book with any nodes, history or snapshots is refused with what would be removed unless `force: true`; `dry_run: true` only reports; a missing book is an error. The delete is conditional on the book version its contents were checked at, so a write in between makes it fail and delete nothing. It clears the calling session's selection when that pointed at the deleted book, and no other session's; a session whose selected book was deleted elsewhere now gets "the selected book no longer exists" instead of an internal error. Store ports gain `BookStore::delete(slug, expected_version)` and `SnapshotStore::delete_all`.
- **Snapshots on Postgres** (`snapshots` table, migration `0002`). `snapshot_restore` is one commit that writes the restored tree and its history entries together.
- **`snapshot_delete`** (#10) — deletes one snapshot of the selected book, by the timestamp `snapshot_list` shows. A labeled snapshot is refused without `force: true`; an unknown timestamp is an error. The book is not changed, so nothing is added to its history.
- **`book_update`** (#9) — changes a book's title and / or `max_depth` (slug required, at least one field). Lowering `max_depth` below an existing node's depth is refused and names the deepest node; an empty title is refused. The change is one commit with a new `book_update` history entry (a book-level entry: no node, before / after `{"title","max_depth"}`), listed by `book_history`; setting the values a book already has commits nothing. `snapshot_restore` and `import` record the same entry when the restored or imported book's title or `max_depth` differs from the current one. Earlier versions cannot read `book_update` entries: after downgrading, `book_history` of a book that has one fails to load.
- **`book_lint` / `book_stats`** (#5) — read-only health reports for a book (`slug`, defaulting to the selected book; `format: "json"` for programs). `book_lint` reports content nodes at the root, pairs of similar titles (Jaccard over title words, character pairs for CJK runs, `threshold` default 0.6; pairs where both titles are a single token are skipped unless the titles are identical, and a two-character CJK title counts as one token), property keys that differ only by case / `_` / `-` / spaces (the less-used spellings are listed), sections without children and drafts not updated for `stale_days` (default 90); `checks` selects a subset and `limit` (default 50 per check) caps the output with a count of the rest. `book_stats` reports node counts, max / mean depth, children per section (max, mean, histogram), counts by status, injected nodes, update recency and distinct property keys. In JSON, `by_status` always carries every status and `omitted` every check that ran, 0 when empty. Neither writes anything; similar titles over 1,200 nodes take about 0.1 s.

- **Agent ergonomics** (#3):
  - `node_query` takes `text`, a case-insensitive substring of title or body (CJK included), combined with the other filters; results carry a snippet around the first body match (`Match:` line, or `snippet` in JSON).
  - `toc` takes `max_depth` (depth from the book root, `1` = root level only, also under `subtree_root`) and `limit`, and closes with a line counting the nodes left out.
  - `node_create`, `node_move` and `node_batch_move` result lines end with ` (id: <uuid>)` — always the last element of the line — which the UUID-only batch tools accept directly.
  - `node_move` and `node_batch_move` name the descendants that moved or were removed with a node (up to ten titles), before the id: `Moved → 2-1. Git with 2 descendants: a, b (id: …)`. The remove result changes wording: `Removed: 2-1. Git and 2 descendants: a, b (id: …)`, or `Removed: 2-1. Git (id: …)` without descendants (it used to end with `(and descendants)` in every case).
  - Both take `dry_run` to list every node that would move or be removed, old → new position, without changing anything; in a batch each node is listed once even when a moved node's descendant is itself in the batch.
  - A rejected move or remove (cycle, depth limit, unknown node) is an invalid-params error in both dry and real runs.
- **Curated knowledge, part 1** (Refs #4):
  - A third node status, `archived` (retired): kept readable, and — together with everything under an archived section, active `inject=true` children included — never injected by `select_book`, left out of `gen_routing`, ignored by every `book_lint` check (an archived original no longer pairs with its replacement as a similar title), left out of `checklist` exports (`include_archived: true` keeps them), and hidden from `node_query`, `toc` and the TOC `select_book` prints unless asked for (`node_query` `status: "archived"` or `include_archived: true`; `toc` `include_archived: true`, which marks them `[archived]`; otherwise the TOC closes with a line counting the hidden nodes the view would have shown). A `toc` / `checklist` whose `subtree_root` is itself archived shows that subtree. `node_update` / `node_batch_update` accept it, and `book_stats` counts it (its `injected` count now counts what `select_book` injects).
  - Downgrading: earlier versions cannot load a book that has an archived node (the status is unknown to them), and an older binary writing a node leaves its `verified_at` / `verified_by` columns untouched.
  - `node_verify { node_ids, by? }` records that nodes were confirmed to still match reality: verification time (now) and who, one history entry per node in one commit; content and last-edited time are unchanged. Stored in two new nullable columns (migration `0003`; run `outline-mcp migrate`).
  - `node_query` `verified_before` (UTC date, datetime or Unix millis; impossible dates such as `2026-02-30` and 10-digit numbers that look like Unix seconds are refused) returns nodes never verified or last verified strictly before it; results show `Verified: <time> by <who>` / `Verified: never`, and JSON results carry `verified_at` / `verified_by`.
- **Typed links between nodes** (Refs #4):
  - `link_add { from, to, rel }` / `link_remove { from, to, rel? }` keep directed links between nodes of one book; `rel` is free-form but normalized (1–40 of `a-z 0-9 _ -`, lowercased), e.g. `supersedes`, `superseded_by`, `see_also`, `merged_from`. Self-links are refused; adding an existing link changes nothing. Each change is an update of the source node in history (links are part of the node, so before/after show them). Like a verification, a link change does not touch the node's `updated_at`, so `stale_drafts` and the update-recency counts keep reflecting edits to the node itself.
  - Links point at the target's id, so moving the target keeps them. Removing a node also drops every link pointing into the removed subtree, in the same commit, with an update entry per affected node; the `node_move` remove result (and its `dry_run`) says how many links were dropped.
  - `links { node_id, direction?, rel? }` lists outgoing links and backlinks (derived when the book is read; nothing extra is stored). `node_query` takes `linked_to` (+ `link_rel`), shows a `Links:` line (archived targets marked `[archived]`), and its JSON carries `links` and `backlinks` arrays whose entries include `target_status` / `source_status` (`active`, `draft`, `archived` — also for a node under an archived section — or `missing`).
  - `book_lint` gains `dangling_links` (links whose target is not in the book, e.g. after restoring older data; restore keeps them as they were) and `book_stats` counts links.
  - `import` (the `checklist` JSON format) does not carry links — nor status or verification — so importing an exported book drops them; this is an existing limitation of that format.
  - Storage: migration `0004` adds a `links` column (JSON, default empty) to nodes. Earlier versions do not write that column, so their updates leave a node's links as they were; but a node deleted by an earlier version leaves links pointing at it behind, which `book_lint`'s `dangling_links` reports.
- **Pending revisions** (#4):
  - `revision_propose { node_id, title?, body? | clear_body, properties? | properties_patch?, by?, note?, replace? }` attaches a proposed content change to a node without changing it. Until it is approved, injection, `select_book`, `toc`, `dump`, `checklist` and every other read show the current content. One pending revision per node (`replace: true` swaps it); a revision that would change nothing, or one for an archived node (or a node under an archived section), is refused.
  - `revision_show { node_id }` shows the proposer, note, age and a diff against the current content: title old → new, a unified diff of the body, properties added / changed / removed. It warns when the node changed after the revision was proposed.
  - `revision_approve { node_id, by?, force? }` applies the revision through the same path as `node_update` and clears it, in one commit recorded as one update in history; `updated_at` moves as for any edit. A revision whose node changed since it was proposed is refused unless `force: true`. `revision_reject { node_id, by? }` drops it. `by` on approve / reject appears in the tool result only; history records the node's before / after, not who reviewed.
  - `revisions_pending { subtree_root? }` lists the nodes with a pending revision (proposer, changed fields, age, stale flag). `node_query` shows a `Proposal:` line and its JSON carries a `proposal` summary (`by`, `note`, `at`, `fields`, `stale`; null when none).
  - Proposing and rejecting are recorded in history as updates of the node and, like links and verification, leave `updated_at` alone.
  - `book_lint` gains `stale_proposals` (revisions older than `stale_days`, or whose node changed since) in the default set; `book_stats` counts pending revisions. Unlike the other checks, `stale_proposals` also reports a revision left on a node that was archived after it was proposed ("on an archived node — reject it": it can no longer be approved), so its count matches `book_stats` and `revisions_pending`.
  - An absent body and an empty one count as the same content: proposing `body: ""` for a node without a body is refused as changing nothing. `body: ""` never clears a body; `clear_body: true` does.
  - Snapshots carry pending revisions (restore brings them back as they were); `import` (the `checklist` JSON format) does not.
  - Storage: migration `0005` adds a nullable `proposal` column (JSON) to nodes. Earlier versions do not write that column, so a pending revision survives their updates, and a node they edit keeps an outdated revision that `revision_show` and `stale_proposals` report as stale. Likewise, an earlier version restoring a snapshot taken by this one leaves the nodes' current pending revisions in place (reported as stale once the content differs).

### Changed

- **Crate metadata for the release**: the crates declare `rust-version = "1.94"` (what sqlx 0.9 requires) and ship the README; `outline-mcp-core` / `outline-mcp-rmcp` carry docs.rs metadata; `server.json` lists `OUTLINE_MCP_DATABASE_URL` as a required environment variable of the package.
- **Output additions for programs reading results** (nothing removed or renamed): `book_lint`'s default check set gains `dangling_links` and `stale_proposals`; `node_query` JSON always carries `proposal` (null when none) alongside `links` / `backlinks`; `book_stats` JSON gains `links` and `pending_proposals`.
- **Docker image builds with Rust 1.97** (was 1.88). sqlx 0.9 requires rustc 1.94 or later, so the image no longer built.
- **Storage is Postgres; `serve` / `stdio` require a database.** The server takes `OUTLINE_MCP_DATABASE_URL` (or `--database-url`) and refuses to start without one, on a database `migrate` has not brought up to date, or when the connection fails — within about 5 s, saying whether the host was unreachable, the login was rejected or the database does not exist. Every MCP tool now runs on the store, so a write and its history entry are committed together and a stale per-book version is rejected instead of overwriting a concurrent write. Fixes #2 for books served from Postgres.
- **Bulk writes are chunked.** A commit still runs in one transaction, but node and history rows go in statements of at most 500 rows and about 1 MB each. Writing a large book in one statement per table (an 8 MB history in a single `UNNEST`) sent a 256 MB Postgres machine into crash recovery during an import; the same 29-book import now runs in chunks and still rolls back as a whole if any chunk fails.
- **Daemon templates and runbooks target Postgres.** `contrib/systemd` and `contrib/launchd` drop the shelf path and take the database URL from their environment block; `docs/runbooks/data-migration.md` is now the `import-shelf` procedure (local shelf or a shelf on a Fly volume) and `multi-device-smoke.md` adds the startup-refusal checks.

### Deprecated

### Removed

- **The shelf directory as runtime storage**, and with it the shelf path argument (passing one prints a pointer to `import-shelf`) and `migrate-snapshots`. The event-log dependency remains only inside `import-shelf` until existing shelves have been moved.

### Fixed

- **Each HTTP session has its own selected book** (#6). Every streamable-HTTP session used to share one selection, so `select_book` (or `init`) in one session redirected the next write of every other session into that book. Sessions now share the storage but start with no book selected and keep their own selection; stdio is unchanged.
- **`import` records the replacement in history** (#11). The tool replaced the selected book without writing any history, so `book_history` / `node_history` showed nothing of it. It now commits the new tree together with a delete entry per removed node and a create entry per imported node, and reports how many entries were recorded. Imported nodes get new ids, so re-importing an unchanged export still records every node as deleted and created; the deleted nodes' history stays readable through `node_history` with their full UUID. No MCP tool changes a book's nodes without recording it any more (the book's title and `max_depth` are not part of history; see #9).
- **Removing a node records every removed node** (#3). `node_move` with `action: "remove"` deleted the node's descendants too but recorded a history entry only for the node itself, so a descendant's `node_history` never showed its deletion. Each removed node now gets a delete entry, with its content before removal, in the same commit. A move still records a move for the moved node only; its descendants keep their parent.

### Security

## [0.14.0] - 2026-08-15

### Added

- **`format: "json"` on `node_query`** (`crates/outline-mcp-rmcp/src/{request,tools}.rs`). Returns one JSON object — `{matches, nodes:[{id, short, title, node_type, status, placeholder, properties, body?}]}` — via `CallToolResult::structured`, which carries both `structuredContent` and the serialized text, as the spec recommends for backwards compatibility. Default stays `"text"`, so existing callers are untouched; an unrecognised value is rejected rather than silently treated as text, since the failure would otherwise surface as a decode error far from the typo.

  The text rendering was never safe to parse. Properties are drawn on one line and only the *first* line of a body gets its `   Body: ` prefix, so a property value containing a newline truncates the line — dropping whichever keys `HashMap` iteration happened to place after it — a body starting with a fence loses its marker to the prefix, and a `---` line inside a body is indistinguishable from the entry separator. Measured on a live 1121-node book: 63 nodes carry a newline in `evidence_quote`, and two runs of the same parse recovered 869 then 863 of 900 `kind` values. Downstream that meant a knowledge-placement gate deleting nodes whose evidence it could not see. The JSON path returns all 1121 nodes with zero body or property mismatches, including the 51 bodies starting with `## ` and the 3 containing a bare `---`.

  Zero matches returns `{"matches":0,"nodes":[]}` rather than the "No matching nodes found." sentence. A poller asking "is there anything pending" gets that answer most of the time, so it is the case a JSON caller can least afford to have return prose.

  No `outputSchema` is declared: it would oblige every response to carry structured content, which the default text mode does not.

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.13.0] - 2026-08-15

### Added

- **`properties_patch` on `node_update` and `node_batch_update`** (`crates/outline-mcp-rmcp/src/request.rs`, `crates/outline-mcp-core/src/domain/model/{book,node}.rs`). Merges the listed keys into a node's existing properties and leaves the rest alone, where `properties` replaces the whole map. Until now a caller that wanted to change one key had to read every other key back and resend it, and there was no way to read them back reliably from another host: `dump` writes its JSON on the *server's* filesystem, and `node_query` renders properties as `k=v, k=v` text whose values legitimately contain both `", "` and `"="`. A caller that got that round-trip wrong destroyed the properties it failed to parse. The two fields are mutually exclusive — supplying both is rejected as `DomainError::ConflictingPropertyUpdate` before any field is written, since applying replace and merge in one call would make the result depend on which ran last. Removal still goes through `properties`; a patch has no way to say "absent".

### Changed

- **Crate metadata gains `homepage` and `documentation`** (`Cargo.toml` `[workspace.package]`, inherited by all three member crates). Their absence made every release run warn `The Homebrew publish job is enabled but no homepage was specified`, and left the generated Formula without a homepage.
- **`outline-mcp-rmcp`'s description now names both transports.** It read "... and stdio transport wiring over outline-mcp-core", which stopped being accurate when the streamable HTTP transport landed in 0.12.0. crates.io showed the stale wording for that release.

### Deprecated

### Removed

### Fixed

### Security

## [0.12.0] - 2026-08-14

### Added

- **Streamable HTTP transport for SSOT-daemon mode** (`--mcp-http`, `crates/outline-mcp-rmcp/src/server.rs::run_http`, `crates/outline-mcp/src/main.rs`). One central process owns the shelf directory (book JSON + per-slug `.events.db`) and remote devices connect as MCP clients over `http://<bind>/mcp`, which preserves the existing single-writer storage model without any cross-device sync or conflict handling. `--bind` defaults to `127.0.0.1:8486`. Security model: on a loopback bind a token is optional and rmcp's default `Host` header validation guards against DNS rebinding; on a non-loopback bind `OUTLINE_MCP_HTTP_TOKEN` is mandatory and startup is refused without it (the check runs before any listener is opened), after which every request must present that token as a bearer credential in the `Authorization` header. TLS termination is a reverse-proxy concern. Pulls in `rmcp`'s `transport-streamable-http-server` feature plus `axum` 0.8; the stdio transport is unchanged and remains the default with no flag.
- **systemd / launchd daemon templates** (`contrib/systemd/outline-mcp.service`, `contrib/launchd/io.github.ynishi.outline-mcp.plist`) for running `--mcp-http` as a user-level service.
- **Deployment runbooks** (`docs/runbooks/data-migration.md`, `docs/runbooks/multi-device-smoke.md`). The first covers moving an existing shelf to a central host (flat-shelf layout, WAL copy rule, single-book variant, legacy snapshot migration); the second is an eight-item `[OK]`/`[NG]` smoke checklist for the HTTP transport.
- **Homebrew formula publish job** (`.github/workflows/release.yml`). Pushes the cargo-dist-generated Formula to the `ynishi/homebrew-outline-mcp` tap on tag push.

### Changed

### Deprecated

### Removed

- **Drop `smithery.yaml`** (added in `0.11.1`). Smithery has moved to a Streamable HTTP-only publish model — the web submit form now rejects stdio servers and requires a hosted HTTPS endpoint. `smithery.yaml` declared `startCommand.type: stdio`, which is no longer a valid publish target. Note that the `--mcp-http` transport added in this same release does not change the conclusion: it is a self-hosted daemon meant for loopback or LAN use, not the hosted HTTPS endpoint a Smithery listing requires. The file is removed to avoid signaling a distribution channel that does not work.

### Fixed

- **Log lines no longer carry ANSI colour escapes** (`crates/outline-mcp-rmcp/src/server.rs`). Both the stdio and the `--mcp-http` subscriber called `tracing_subscriber::fmt().with_writer(std::io::stderr)` without `.with_ansi(false)`; tracing-subscriber does not probe the writer for TTY-ness, so escape codes were written verbatim into every non-terminal destination — a client's captured stderr in stdio mode, and `StandardErrorPath` / the journal under the launchd / systemd daemon forms. `grep`-ing a daemon log for the startup line returned `^[[2m…^[[0m`-laden output. Both subscribers now disable ANSI unconditionally, matching the expectation stated in `docs/runbooks/multi-device-smoke.md` §2.

### Security

## [0.11.1] - 2026-07-23

### Added

- **Smithery hosted runtime setup** (`smithery.yaml`). Declares stdio `startCommand` targeting the `outline-mcp` binary so Smithery can auto-build the server from the existing `Dockerfile`.
- **cargo-dist multi-target binary distribution** (`dist-workspace.toml`, `Cargo.toml [profile.dist]`, `.github/workflows/release.yml`). Ships prebuilt binaries for `aarch64-apple-darwin` / `aarch64-unknown-linux-gnu` / `x86_64-apple-darwin` / `x86_64-unknown-linux-gnu` / `x86_64-pc-windows-msvc` via cargo-dist `v0.32.0`, exposes shell / powershell / homebrew installers, and pushes a Formula to the `ynishi/homebrew-outline-mcp` tap on tag push. `allow-dirty = ["ci"]` keeps the existing `release` job (GHCR Docker push + MCP Registry publish) coexisting alongside the autogenerated cargo-dist jobs.

## [0.11.0] - 2026-07-05

### Changed

- **Bump `ai-store` 0.7 → 0.9** (`ai-store-core` / `ai-store-sqlite` / `ai-store-sync`, dev-only `ai-store-mem`). `Store::append` / `Store::import_event` / `Store::revert` now return `Committed { seq, at }` instead of a bare `Seq`; `SnapshotService::create` and the snapshot migrator use the returned `at` directly instead of a follow-up `Store::read` round trip. `ProjectionSink::accepts` is now honored by the facade's automatic dispatch; a new `SnapshotOnlySink` (wrapping `SnapshotDumpSink` in a `BlockingSink`) filters dispatch to the dedicated snapshot stream, so per-node changelog appends on a `Store` shared with the snapshot subsystem no longer trigger a no-op `spawn_blocking` call. `OutlineMcpServer::store_for` now builds its per-slug `Store` via `ai_store_sqlite::SqliteStore::open_with` instead of hand-wiring `SqliteBackends` + `Store::new`, which also switches sink checkpoints from in-memory-only to SQLite-persisted (unexercised today — nothing in this server calls `Store::catch_up` / `Store::rebuild`).
- **`AiStoreChangeLogRepository` is now wired into `server.rs`.** The changelog writer for `node_create` / `node_update` / `node_move` / `node_batch_move` / `node_batch_update` / `remove_node` / the restore trail recorded by `snapshot_restore` is now ai-store-backed rather than the JSON-file `JsonChangeLogRepository` (resolves the "not yet wired" limitation noted in 0.10.0). A new `HistoryPreservingChangeLogRepository` (`outline-mcp-core::infra::changelog_bridge`) bridges pre-existing `{slug}.changelog.json` history: new appends land in ai-store only, while `load_all` / `load_by_node` (used by `node_history` / `book_history`) concatenate the frozen legacy entries ahead of the ai-store ones, so upgrading consumers keep their full changelog history with no dedicated migration step.

### Deprecated

### Removed

### Fixed

### Security

## [0.10.1] - 2026-07-05

### Changed

- **Bump `ai-store` 0.4 → 0.7** (`ai-store-core` / `ai-store-sqlite` / `ai-store-sync` / `ai-store-mem`). API is unchanged across 0.5/0.6/0.7 per the ai-store CHANGELOG (the new `SqliteBackend` trait in 0.7 is purely additive; the existing inherent `new` constructors and `SqliteBackends::open` signature are preserved). Transitively bumps `rusqlite` 0.31 → 0.37 and `libsqlite3-sys` 0.28 → 0.35; outline-mcp holds no direct `rusqlite` dependency so no band conflict for downstream. No code changes required in `outline-mcp-core` / `outline-mcp-rmcp`; all 194 workspace tests continue to pass.

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
