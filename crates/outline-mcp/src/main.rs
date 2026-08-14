//! Thin entry point: dispatches the `migrate-snapshots` CLI subcommand (see
//! `cli`), or else parses the shelf directory / transport flags from argv and
//! hands off to `outline_mcp_rmcp::run` (stdio) or `run_http` (streamable
//! HTTP), which own the MCP server (rmcp transport, tool_router, resources)
//! and its `outline-mcp-core` wiring.
//!
//! Usage:
//!
//! ```text
//! outline-mcp [shelf_dir]                          # stdio (default)
//! outline-mcp [shelf_dir] --mcp-http [--bind ADDR] # streamable HTTP daemon
//! outline-mcp migrate-snapshots --shelf <path>     # CLI subcommand
//! ```
//!
//! `--bind` defaults to `127.0.0.1:8486`. Non-loopback binds require
//! `OUTLINE_MCP_HTTP_TOKEN` (see `outline_mcp_rmcp::run_http`).

use std::path::PathBuf;

mod cli;

/// Default bind address for `--mcp-http` (loopback; SSOT-daemon deployments
/// override with `--bind 0.0.0.0:8486` + `OUTLINE_MCP_HTTP_TOKEN`).
const DEFAULT_BIND: &str = "127.0.0.1:8486";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.first().map(String::as_str) == Some("migrate-snapshots") {
        let exit_code = cli::run(argv.into_iter().skip(1)).await?;
        std::process::exit(exit_code);
    }

    let mut shelf_arg: Option<PathBuf> = None;
    let mut http = false;
    let mut bind: Option<String> = None;

    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mcp-http" => http = true,
            "--bind" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--bind requires a value (host:port)"))?;
                bind = Some(value);
            }
            other if other.starts_with('-') => {
                anyhow::bail!("unrecognized flag: {other} (expected --mcp-http / --bind ADDR)");
            }
            other => {
                if shelf_arg.is_some() {
                    anyhow::bail!("unexpected extra positional argument: {other}");
                }
                shelf_arg = Some(PathBuf::from(other));
            }
        }
    }
    if bind.is_some() && !http {
        anyhow::bail!("--bind only applies with --mcp-http");
    }

    let shelf_dir = shelf_arg.unwrap_or_else(|| {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".config/outline-mcp/books")
    });

    if http {
        let bind = bind.as_deref().unwrap_or(DEFAULT_BIND);
        outline_mcp_rmcp::run_http(shelf_dir, bind).await
    } else {
        outline_mcp_rmcp::run(shelf_dir).await
    }
}
