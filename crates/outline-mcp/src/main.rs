//! Thin entry point: dispatches the `migrate-snapshots` CLI subcommand (see
//! `cli`), or else parses the shelf directory from argv/env and hands off
//! to `outline_mcp_rmcp::run`, which owns the MCP server (rmcp transport,
//! tool_router, resources) and its `outline-mcp-core` wiring.

use std::path::PathBuf;

mod cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut argv = std::env::args().skip(1);
    let first = argv.next();

    if first.as_deref() == Some("migrate-snapshots") {
        let exit_code = cli::run(argv).await?;
        std::process::exit(exit_code);
    }

    let shelf_dir = first.map(PathBuf::from).unwrap_or_else(|| {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".config/outline-mcp/books")
    });

    outline_mcp_rmcp::run(shelf_dir).await
}
