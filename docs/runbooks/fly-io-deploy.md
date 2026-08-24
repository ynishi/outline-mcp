# Fly.io hosting — deploy runbook

Host outline-mcp on a Fly.io Machine: one always-on daemon (`--mcp-http`)
with the shelf directory (book JSON files + per-slug `.events.db` SQLite)
on a persistent volume, TLS at the Fly edge, and bearer-token auth. The
single-writer model from the SSOT-daemon design is preserved — every
device talks to this one machine.

Config lives in `contrib/fly/fly.toml`; the repo-root `Dockerfile` is used
as-is (no extra build features or sidecars). Sibling runbooks:
`data-migration.md` (shelf layout / WAL copy rules) and
`multi-device-smoke.md` (HTTP transport smoke checklist) apply unchanged —
this runbook only adds the Fly-specific plumbing.

## 0. Prerequisites

- `flyctl` installed and authenticated (`fly auth login`)
- A bearer token of your choosing (e.g. `openssl rand -hex 32`)

## 1. Create app + volume

```sh
fly apps create <your-app-name>
fly volumes create outline_data --app <your-app-name> --region nrt --size 1 --yes
```

Then set `app = "<your-app-name>"` (and `primary_region` if not `nrt`) in
`contrib/fly/fly.toml`.

## 2. Secrets

Secrets become environment variables inside the machine — the same
`OUTLINE_MCP_HTTP_TOKEN` contract as a local daemon.

```sh
# Generate the server key once and keep it — clients need the same value (§6).
TOKEN=$(openssl rand -hex 32) && echo "$TOKEN"

fly secrets set --app <your-app-name> OUTLINE_MCP_HTTP_TOKEN=$TOKEN --stage
```

`OUTLINE_MCP_HTTP_TOKEN` is **required**: the server refuses to start on a
non-loopback bind (`0.0.0.0:8486` inside the container) without it.
`--stage` skips the redundant immediate release; the first `fly deploy`
picks the secret up.

## 3. Deploy

From the repo root (the Dockerfile is auto-detected there):

```sh
fly deploy --config contrib/fly/fly.toml
```

## 4. Smoke

```sh
APP=https://<your-app-name>.fly.dev
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
# no token → 401
curl -s -o /dev/null -w 'no-auth:%{http_code}\n' -X POST "$APP/mcp" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
# token → 200
curl -s -o /dev/null -w 'good:%{http_code}\n' -X POST "$APP/mcp" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
```

**Expect**: `no-auth:401` / `good:200`. `fly logs` shows
`serving MCP over streamable HTTP at /mcp ... auth="bearer-token"`.
The full transport checklist is `multi-device-smoke.md`.

## 5. Data migration (existing local shelf → volume)

Do this **before** pointing any client at the app, then no writes can race
the swap. Archive rules follow `data-migration.md` — in particular, stop
any local outline-mcp daemon / MCP client first so the `.events.db`
WAL/SHM files are quiescent:

```sh
# local host — default shelf is ~/.config/outline-mcp/books
tar -czf /tmp/outline-books.tar.gz -C ~/.config/outline-mcp books
fly ssh sftp put /tmp/outline-books.tar.gz /data/incoming.tar.gz --app <your-app-name>
```

Unpack on the machine, then restart so the daemon reopens the new files:

```sh
fly ssh console --app <your-app-name> -C "sh -c 'mkdir -p /data/books && tar -xzf /data/incoming.tar.gz -C /data --overwrite && rm /data/incoming.tar.gz'"
fly machine restart --app <your-app-name>
```

Verify with the §4 smoke, then a `shelf` tool call from a registered
client (§6) should list the migrated books.

## 6. Client registration

```sh
claude mcp add --transport http outline https://<your-app-name>.fly.dev/mcp \
  --header "Authorization: Bearer <token>"
```

Register under a distinct name if a local stdio `outline` entry exists.

## 7. Backup

Fly takes automatic daily volume snapshots (~5-day retention); restore by
creating a new volume from a snapshot (`fly volumes snapshots list` /
`fly volumes create --snapshot-id`). Book-level restore points are also
available inside the data itself via the `snapshot_create` /
`snapshot_restore` MCP tools. Off-Fly backup (S3-compatible upload) is not
implemented — if you need it, copy the shelf out over
`fly ssh sftp get` on your own schedule.

## 8. Notes / constraints

- **Exactly one machine.** The volume (and the single-writer model over
  the book files / SQLite event logs) belongs to one machine; do not scale
  out (`fly scale count 1`), and keep `auto_stop_machines = "off"`.
- The shelf directory is created on first write; a fresh volume with no
  migration simply starts with an empty shelf.
