//! Thin entry point: parses the shelf directory from argv/env and hands off
//! to `outline_mcp_rmcp::run`, which owns the MCP server (rmcp transport,
//! tool_router, resources) and its `outline-mcp-core` wiring.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let shelf_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".config/outline-mcp/books")
        });

    outline_mcp_rmcp::run(shelf_dir).await
}
