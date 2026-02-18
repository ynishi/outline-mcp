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

    outline_mcp::interface::mcp::run(shelf_dir).await
}
