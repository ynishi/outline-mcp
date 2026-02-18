use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let book_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("outline-book.json"));

    outline_mcp::interface::mcp::run(book_path).await
}
