# HTTP transport and startup — smoke test runbook

End-to-end verification of the streamable HTTP transport (`--mcp-http`) and
of the startup checks on a real host. Every step binds loopback only unless
a test says otherwise. Mirrors mini-app-mcp's
`docs/runbooks/multi-device-smoke.md`.

## 0. Prerequisites

```sh
git pull && cargo install --path crates/outline-mcp
```

A **throwaway** Postgres database (the smoke creates a book in it), migrated:

```sh
export OUTLINE_MCP_DATABASE_URL="postgres://<user>:<password>@<host>:5432/<scratch-database>"
outline-mcp migrate
```

Shared shell fragments used below:

```sh
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
HDR=(-H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
```

> **Note on process cleanup**: `kill %1` does not work in non-interactive
> shells (no job control). Always capture the server pid with `SRV=$!` and
> `kill $SRV`, and finish with the residual-listener check in §8.

## 1. Startup refusals

```sh
( unset OUTLINE_MCP_DATABASE_URL; outline-mcp; echo "exit=$?" ) </dev/null
outline-mcp --database-url "postgres://nobody:wrong@127.0.0.1:1/none" </dev/null; echo "exit=$?"
outline-mcp ~/.config/outline-mcp/books </dev/null; echo "exit=$?"
```

**Expect**, each with a non-zero exit and no listener:

- `error: no database configured: set OUTLINE_MCP_DATABASE_URL or pass --database-url <url> ...`
- `error: could not connect to the database ...` within about 5 s, saying
  whether the host was unreachable, the login was rejected or the database
  does not exist — and never echoing the password
- `error: unexpected argument: ... run outline-mcp import-shelf --shelf ...`

## 2. HTTP consumption path (initialize → init book → shelf)

```sh
outline-mcp --mcp-http --bind 127.0.0.1:8496 2>server.err &
SRV=$!
sleep 1
SID=$(curl -s -D - -o /dev/null -X POST http://127.0.0.1:8496/mcp "${HDR[@]}" -d "$INIT" | grep -i mcp-session-id | tr -d '\r' | awk '{print $2}')
curl -s -X POST http://127.0.0.1:8496/mcp -H "Mcp-Session-Id: $SID" "${HDR[@]}" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' > /dev/null
curl -s -X POST http://127.0.0.1:8496/mcp -H "Mcp-Session-Id: $SID" "${HDR[@]}" -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"init","arguments":{"slug":"smoke-book","title":"Smoke"}}}'
curl -s -X POST http://127.0.0.1:8496/mcp -H "Mcp-Session-Id: $SID" "${HDR[@]}" -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"shelf","arguments":{}}}'
kill $SRV
```

**Expect**: initialize returns 200 with an `mcp-session-id` header; `init`
creates the book; `shelf` lists `smoke-book`.

## 3. Startup log

```sh
grep "serving MCP over streamable HTTP" server.err
```

**Expect**: an `INFO ... serving MCP over streamable HTTP at /mcp
addr=127.0.0.1:8496 auth="loopback"` line, free of ANSI escapes.

Logs go to **stderr** by design (stdout is the MCP protocol channel in stdio
mode). Under launchd this lands in `StandardErrorPath` (`.err`); an empty
`StandardOutPath` (`.log`) is expected. Under systemd, use `journalctl`.

## 4. Bearer auth (401 / 200)

Loopback bind — nothing is exposed off-host.

```sh
OUTLINE_MCP_HTTP_TOKEN=tok outline-mcp --mcp-http --bind 127.0.0.1:8497 2>/dev/null &
SRV=$!
sleep 1
curl -s -o /dev/null -w 'no-auth:%{http_code}\n'                              -X POST http://127.0.0.1:8497/mcp "${HDR[@]}" -d "$INIT"
curl -s -o /dev/null -w 'bad:%{http_code}\n'  -H 'Authorization: Bearer bad'  -X POST http://127.0.0.1:8497/mcp "${HDR[@]}" -d "$INIT"
curl -s -o /dev/null -w 'good:%{http_code}\n' -H 'Authorization: Bearer tok'  -X POST http://127.0.0.1:8497/mcp "${HDR[@]}" -d "$INIT"
kill $SRV
```

**Expect**: `no-auth:401` / `bad:401` / `good:200`.

## 5. Non-loopback bind refusal

The database is checked first, then this guard, both before any listener
is opened, so the test exposes nothing.

```sh
outline-mcp --mcp-http --bind 0.0.0.0:8498; echo "exit=$?"
```

**Expect**: `Error: refusing to bind non-loopback address 0.0.0.0:8498
without OUTLINE_MCP_HTTP_TOKEN ...` and non-zero exit, with no listener.

## 6. Stdio regression

```sh
echo "$INIT" | outline-mcp | head -c 80
```

**Expect**: stdout starts with `{"jsonrpc":"2.0","id":1,"result":{...` and
contains no log lines (logs are on stderr).

## 7. Daemon form

Both templates read the database URL from their environment block; fill it
in before installing.

### Linux (systemd user unit)

```sh
cp contrib/systemd/outline-mcp.service ~/.config/systemd/user/
# edit Environment=OUTLINE_MCP_DATABASE_URL=... in the copied unit
systemctl --user daemon-reload && systemctl --user start outline-mcp
systemctl --user is-active outline-mcp                      # → active
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8486/mcp "${HDR[@]}" -d "$INIT"   # → 200
journalctl --user -u outline-mcp | grep "serving MCP"       # same check as §3
# Teardown (skip on a host where the daemon stays enabled)
systemctl --user stop outline-mcp && rm ~/.config/systemd/user/outline-mcp.service && systemctl --user daemon-reload
```

### macOS (launchd user agent)

```sh
sed "s|__HOME__|$HOME|g" contrib/launchd/io.github.ynishi.outline-mcp.plist > ~/Library/LaunchAgents/io.github.ynishi.outline-mcp.plist
# edit the OUTLINE_MCP_DATABASE_URL string in EnvironmentVariables
plutil -lint ~/Library/LaunchAgents/io.github.ynishi.outline-mcp.plist      # → OK
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.ynishi.outline-mcp.plist
lsof -nP -iTCP:8486 | grep LISTEN                            # listener present
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8486/mcp "${HDR[@]}" -d "$INIT"   # → 200
grep "serving MCP" /tmp/outline-mcp.err                      # log lands in .err; empty .log is expected
# Teardown
launchctl bootout gui/$(id -u)/io.github.ynishi.outline-mcp
rm ~/Library/LaunchAgents/io.github.ynishi.outline-mcp.plist
```

## 8. Residual-listener check (always run last)

```sh
pgrep -f "outline-mcp .*--mcp-http" | grep -v $(systemctl --user show -p MainPID --value outline-mcp 2>/dev/null || echo 0) || echo "no residual listeners"
```

**Expect**: `no residual listeners` (an enabled daemon's own pid does not
count). If other pids are printed, kill them explicitly — leaked test
servers keep ports 8496/8497 occupied and skew the next run.

## Reporting

List all nine items (§1–§6, the §7 variant for your OS, §8) as a strict
`[OK]` / `[NG]` two-value checklist. An item that was not actually invoked is
`[NG]`, never "skipped" — an unexercised path is an unverified path.
