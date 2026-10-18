# Fly.io hosting — deploy runbook

Host outline-mcp on a Fly.io Machine: a `--mcp-http` server with TLS at the
Fly edge and bearer-token auth, storing everything (books, history,
snapshots) in Postgres. The database here is an **unmanaged Fly Postgres**
app in the same org, reached over Fly's private network. The server machine
itself keeps no state, so it has no volume and may stop when idle.

Config lives in `contrib/fly/fly.toml`; the repo-root `Dockerfile` is used
as-is. Sibling runbooks: `data-migration.md` (moving a pre-Postgres shelf
into the database) and `multi-device-smoke.md` (transport smoke checklist).

Commands and flags below were checked against `flyctl` v0.4.105 `--help`.

## 0. Prerequisites

- `flyctl` installed and authenticated (`fly auth login`)
- `outline-mcp` built locally (`cargo install --path crates/outline-mcp`) —
  migrations and imports are run from your machine through `fly proxy`
- A bearer token of your choosing (e.g. `openssl rand -hex 32`)

## 1. Database (unmanaged Fly Postgres)

Unmanaged Fly Postgres is **not supported by Fly.io Support**: you own
backups, upgrades, high availability and recovery. It is billed as an
ordinary machine plus its volume. Fly's managed offering (`fly mpg`) is the
alternative when that is not acceptable.

```sh
fly postgres create --name <your-app-name>-db --region nrt \
  --initial-cluster-size 1 --vm-size shared-cpu-1x --vm-memory 512 --volume-size 1
```

Give the database machine at least **512 MB**. A 256 MB machine went into
crash recovery during a bulk import of a 1,100-node book before writes were
split into chunks; chunked writes are much lighter, but 256 MB leaves little
headroom. An existing database machine can be resized:

```sh
fly machine list --app <your-app-name>-db
fly machine update <machine-id> --app <your-app-name>-db --vm-memory 512
```

## 2. App and secrets

```sh
fly apps create <your-app-name>
```

Set `app = "<your-app-name>"` (and `primary_region` if not `nrt`) in
`contrib/fly/fly.toml`.

Attach the database. This creates a database and a role for the app and
stores the connection URL as a secret under the name the server reads:

```sh
fly postgres attach --app <your-app-name> --variable-name OUTLINE_MCP_DATABASE_URL <your-app-name>-db
```

The command prints the URL once; keep it for §3 and §4 (it is also readable
later with `fly ssh console -C env` on a running machine). Then the HTTP
token:

```sh
# Generate the server key once and keep it — clients need the same value (§7).
TOKEN=$(openssl rand -hex 32) && echo "$TOKEN"
fly secrets set --app <your-app-name> OUTLINE_MCP_HTTP_TOKEN=$TOKEN --stage
```

`OUTLINE_MCP_HTTP_TOKEN` is required: the server refuses a non-loopback bind
(`0.0.0.0:8486` inside the container) without it.

## 3. Migrate the schema

The server refuses to start on a database `outline-mcp migrate` has not
brought up to date, so migrate **before** the first deploy, from your
machine through a proxy:

```sh
fly proxy 15433:5432 --app <your-app-name>-db      # leave running in another terminal
outline-mcp migrate --database-url "postgres://<user>:<password>@127.0.0.1:15433/<database>"
```

Use the user, password and database from the URL `attach` printed, with the
host replaced by `127.0.0.1:15433`. Run `migrate` the same way after every
upgrade that adds a migration; it is idempotent.

## 4. Existing data

A fresh database starts with an empty shelf. To bring books over from an
earlier, volume-based deploy or from a local shelf, follow
`data-migration.md` now, while the proxy from §3 is still running and before
any client writes to the new deployment.

## 5. Deploy

From the repo root (the Dockerfile is auto-detected there):

```sh
fly deploy --config contrib/fly/fly.toml
```

If the app previously ran with a volume, its machine still has that volume
attached; flyctl may refuse a config without `[[mounts]]` for it. Destroying
the old machine (`fly machine destroy <id>`) lets the deploy create a fresh
one, and leaves the volume itself in place for rollback (§8).

## 6. Smoke

```sh
APP=https://<your-app-name>.fly.dev
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
# no token → 401
curl -s -o /dev/null -w 'no-auth:%{http_code}\n' -X POST "$APP/mcp" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
# token → 200
curl -s -o /dev/null -w 'good:%{http_code}\n' -X POST "$APP/mcp" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' -d "$INIT"
```

**Expect**: `no-auth:401` / `good:200`. `fly logs` shows
`serving MCP over streamable HTTP at /mcp ... auth="bearer-token"`. If the
machine exits instead, `fly logs` carries the reason: no database URL, the
database unreachable / login rejected / database missing, or migrations
pending.

Then, from a registered client (§7): `shelf` lists every book you imported,
and `select_book` + `node_history` on one of them shows its history.

## 7. Client registration

Register the endpoint with the token as a bearer credential in the
`Authorization` header:

```sh
claude mcp add --transport http outline https://<your-app-name>.fly.dev/mcp \
  --header "Authorization: Bearer <token>"
```

Register under a distinct name if a local stdio `outline` entry exists.

## 8. Rollback

Until you delete them, the previous release and (for a former volume-based
deploy) its volume still exist:

```sh
fly releases --app <your-app-name> --image  # find the previous image
fly deploy --config <previous fly.toml> --image <previous image>
```

Keep the old volume for some days after the cutover and delete it only once
the imported books have been in use without problems
(`fly volumes list` / `fly volumes destroy <id>`).

## 9. Backup

- **Volume snapshots**: Fly takes daily snapshots of the database app's
  volume (about 5 days kept). Restore by creating a new volume from a
  snapshot (`fly volumes snapshots list` / `fly volumes create
  --snapshot-id`) and a database machine on it.
- **Logical dumps**, on your own schedule, through the proxy from §3:

  ```sh
  pg_dump --format=custom --file outline-$(date +%Y%m%d).dump \
    "postgres://<user>:<password>@127.0.0.1:15433/<database>"
  ```

- Book-level restore points are also available inside the data via the
  `snapshot_create` / `snapshot_restore` MCP tools.

## 10. Notes

- The server machine is stateless: several machines may run against one
  database, and `auto_stop_machines = "stop"` is safe. Every write is one
  transaction with a per-book version check.
- The first request after an idle stop pays the machine's start time.
- Cost: the unmanaged database is one machine plus its volume, billed like
  any other; the server machine is billed only while running.
