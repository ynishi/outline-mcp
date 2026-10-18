// SMARTLINT: Status::InReview (1774100840)
//! [`OutlineMcpServer`]: the `ServerHandler` implementation and its
//! [`Storage`] / `selected` state.
//!
//! MCP Protocol (stdio / streamable HTTP) <-> application::BookService /
//! EjectService, over the [`BookStore`] / [`SnapshotStore`] ports. The binary
//! connects them to Postgres; tests use the in-memory adapters.

use std::sync::{Arc, RwLock};

use rmcp::{
    handler::server::{tool::ToolCallContext, tool::ToolRouter},
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams,
        ReadResourceResult, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};

use outline_mcp_core::application::error::AppError;
use outline_mcp_core::application::service::BookService;
use outline_mcp_core::domain::model::id::NodeId;
use outline_mcp_core::domain::store::{BookStore, SnapshotStore};

use crate::helpers::{build_hierarchical_ids, find_hierarchical_id, is_hierarchical_id};
use crate::request::parse_node_id;
use crate::resources;

// =============================================================================
// Public entry point
// =============================================================================

/// Where the server keeps books and snapshots.
///
/// One value backs every tool call and every HTTP session; cloning shares the
/// underlying stores.
#[derive(Clone)]
pub struct Storage {
    pub(crate) books: Arc<dyn BookStore>,
    pub(crate) snapshots: Arc<dyn SnapshotStore>,
}

impl Storage {
    /// Serve `books` and `snapshots` (typically the same Postgres store).
    pub fn new(books: Arc<dyn BookStore>, snapshots: Arc<dyn SnapshotStore>) -> Self {
        Self { books, snapshots }
    }
}

/// Serve MCP over stdio, on `storage`.
pub async fn run(storage: Storage) -> anyhow::Result<()> {
    // Best-effort: a minimal stderr-only subscriber so `tracing::warn!`
    // calls are actually visible somewhere. stdout is reserved for the MCP stdio
    // JSON-RPC transport below — writing anywhere else there would corrupt
    // the protocol stream, so this must never target stdout. `try_init`
    // (rather than `init`) tolerates a subscriber already having been
    // installed (e.g. by an embedding host, or a repeated call in tests).
    // ANSI is off because tracing-subscriber never probes the writer for
    // TTY-ness: stderr here is a client's captured pipe, not a terminal.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let server = OutlineMcpServer::new(storage);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve over streamable HTTP transport (multi-device / SSOT-daemon mode).
///
/// Remote devices connect as MCP clients to `http://<bind>/mcp`. Every write
/// is one database transaction with a version check, so concurrent clients
/// cannot silently overwrite each other's change: the later one gets a
/// conflict and retries.
///
/// # Security model
///
/// - Loopback bind (default `127.0.0.1:8486`): token optional. rmcp's default
///   `Host` header validation (loopback-only) guards against DNS rebinding.
/// - Non-loopback bind: `OUTLINE_MCP_HTTP_TOKEN` **must** be set or startup is
///   refused. When a token is set, every request must carry
///   `Authorization: Bearer <token>`. Host validation is disabled in this
///   case (the bearer token replaces it); TLS termination, if desired, is a
///   reverse-proxy concern.
pub async fn run_http(storage: Storage, bind: &str) -> anyhow::Result<()> {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    // Unlike the stdio path, stdout is not a protocol channel here, but the
    // subscriber still targets stderr so daemon logs behave identically in
    // both transports (and under systemd, both land in the journal). ANSI is
    // off for the same reason as the stdio path, and it matters more here:
    // a daemon's stderr is a log file (launchd `StandardErrorPath`) or the
    // journal, and colour escapes would be written into it verbatim.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let server = OutlineMcpServer::new(storage);

    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address {bind:?}: {e}"))?;
    let token = std::env::var("OUTLINE_MCP_HTTP_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(Arc::<str>::from);
    let loopback = addr.ip().is_loopback();
    if !loopback && token.is_none() {
        anyhow::bail!(
            "refusing to bind non-loopback address {addr} without OUTLINE_MCP_HTTP_TOKEN \
             (set the token, or bind a loopback address)"
        );
    }

    let mut http_config = StreamableHttpServerConfig::default();
    if !loopback {
        // Bearer auth replaces loopback Host validation for LAN/remote binds.
        http_config.allowed_hosts.clear();
    }
    // One server value per MCP session: the storage is shared, the selected
    // book is not, so `select_book` in one session never redirects another
    // session's writes.
    let service = StreamableHttpService::new(
        move || Ok(server.for_new_session()),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let mut router = axum::Router::new().nest_service("/mcp", service);
    if let Some(token) = token {
        router = router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let token = Arc::clone(&token);
                async move {
                    if bearer_token_matches(req.headers(), &token) {
                        next.run(req).await
                    } else {
                        axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNAUTHORIZED,
                            "missing or invalid bearer token",
                        ))
                    }
                }
            },
        ));
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, auth = if loopback { "loopback" } else { "bearer-token" }, "serving MCP over streamable HTTP at /mcp");
    axum::serve(listener, router).await?;
    Ok(())
}

/// Constant-time comparison of the request's `Authorization: Bearer` value
/// against the configured token.
fn bearer_token_matches(headers: &axum::http::HeaderMap, token: &str) -> bool {
    let Some(presented) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let (a, b) = (presented.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// =============================================================================
// MCP Server
// =============================================================================

/// What a book holds, as reported (and removed) by `book_delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BookFootprint {
    pub nodes: usize,
    pub history: usize,
    pub snapshots: usize,
}

impl std::fmt::Display for BookFootprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} nodes, {} history entries, {} snapshots",
            self.nodes, self.history, self.snapshots
        )
    }
}

/// The `BookService` every tool handler uses: one book of the server's
/// [`Storage`].
pub(crate) type ServerService = BookService<Arc<dyn BookStore>>;

/// The outline-mcp MCP server.
///
/// Holds the [`Storage`] and the currently selected book, and implements
/// `ServerHandler` by dispatching MCP tool calls onto
/// `outline_mcp_core::application::service::BookService` (books, history)
/// and the [`SnapshotStore`] (snapshots).
#[derive(Clone)]
pub struct OutlineMcpServer {
    pub(crate) storage: Storage,
    pub(crate) selected: Arc<RwLock<Option<String>>>,
    tool_router: ToolRouter<Self>,
}

impl OutlineMcpServer {
    /// Construct a new server over `storage`. No book is selected until
    /// `select_book` (or `init`) is called.
    pub fn new(storage: Storage) -> Self {
        Self {
            storage,
            selected: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// A server for a new MCP session: the same storage, no book selected.
    ///
    /// `Clone` shares `selected` (it is an `Arc`), which is right inside one
    /// session and wrong across sessions — the HTTP transport builds one
    /// server per session through this instead.
    pub(crate) fn for_new_session(&self) -> Self {
        Self {
            selected: Arc::new(RwLock::new(None)),
            ..self.clone()
        }
    }

    /// The snapshot store.
    pub(crate) fn snapshots(&self) -> &dyn SnapshotStore {
        self.storage.snapshots.as_ref()
    }

    /// The selected book's slug, or the "No book selected" error.
    pub(crate) fn selected_slug(&self) -> Result<String, McpError> {
        let guard = self
            .selected
            .read()
            .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
        guard.clone().ok_or_else(|| {
            McpError::invalid_params(
                "No book selected. Use `shelf` to list books and `select_book` to choose one.",
                None,
            )
        })
    }

    /// The book stored in `slug`'s snapshot at `millis`, or a
    /// "Snapshot not found" error.
    pub(crate) async fn load_snapshot(
        &self,
        slug: &str,
        millis: i64,
    ) -> Result<outline_mcp_core::domain::model::book::TemplateBook, McpError> {
        let ts = outline_mcp_core::domain::model::timestamp::Timestamp::from_millis(millis);
        self.snapshots()
            .get(slug, ts)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Failed to load snapshot {millis}: {e}"), None)
            })?
            .ok_or_else(|| {
                McpError::invalid_params(format!("Snapshot not found for timestamp {millis}"), None)
            })
    }

    /// Whether a book `slug` exists.
    pub(crate) async fn book_exists(&self, slug: &str) -> Result<bool, McpError> {
        self.storage
            .books
            .load(slug)
            .await
            .map(|b| b.is_some())
            .map_err(|e| {
                McpError::internal_error(format!("Failed to read book '{slug}': {e}"), None)
            })
    }

    /// 選択中BookのServiceを返す。未選択ならエラー。
    pub(crate) async fn service(&self) -> Result<ServerService, McpError> {
        let slug = self.selected_slug()?;
        self.service_for(&slug).await
    }

    /// 指定slugのServiceを返す（選択状態不要）。
    pub(crate) async fn service_for(&self, slug: &str) -> Result<ServerService, McpError> {
        Ok(BookService::new(Arc::clone(&self.storage.books), slug))
    }

    /// Every book's slug, sorted.
    pub(crate) async fn list_book_slugs(&self) -> Result<Vec<String>, McpError> {
        let books =
            self.storage.books.list_books().await.map_err(|e| {
                McpError::internal_error(format!("Failed to list books: {e}"), None)
            })?;
        Ok(books.into_iter().map(|b| b.slug).collect())
    }

    /// What deleting `slug` would remove, or `None` if there is no such book.
    pub(crate) async fn book_footprint(
        &self,
        slug: &str,
    ) -> Result<Option<BookFootprint>, McpError> {
        let read_err =
            |e| McpError::internal_error(format!("Failed to read book '{slug}': {e}"), None);
        let Some(book) = self.storage.books.load(slug).await.map_err(read_err)? else {
            return Ok(None);
        };
        let history = self.storage.books.history(slug).await.map_err(read_err)?;
        let snapshots = self.storage.snapshots.list(slug).await.map_err(read_err)?;
        Ok(Some(BookFootprint {
            nodes: book.value.node_count(),
            history: history.len(),
            snapshots: snapshots.len(),
        }))
    }

    /// The `book_delete` tool: delete `slug` with its nodes, history and
    /// snapshots. A book that still has nodes or snapshots is refused unless
    /// `force`; `dry_run` only reports. Clears this session's selection when
    /// it pointed at the deleted book; other sessions are not touched.
    pub(crate) async fn delete_book(
        &self,
        slug: &str,
        force: bool,
        dry_run: bool,
    ) -> Result<String, McpError> {
        let Some(fp) = self.book_footprint(slug).await? else {
            return Err(McpError::invalid_params(
                format!("Book '{slug}' not found. Use `shelf` to list books."),
                None,
            ));
        };
        if dry_run {
            return Ok(format!(
                "Dry run: would delete '{slug}': {fp}. Nothing changed."
            ));
        }
        if !force && (fp.nodes > 0 || fp.snapshots > 0) {
            return Err(McpError::invalid_params(
                format!(
                    "Book '{slug}' is not empty ({fp}). Deleting it cannot be undone; \
                     pass `force: true` to delete it anyway, or `dry_run: true` to only report."
                ),
                None,
            ));
        }

        let deleted = self.storage.books.delete(slug).await.map_err(|e| {
            McpError::internal_error(format!("Failed to delete book '{slug}': {e}"), None)
        })?;
        if !deleted {
            return Err(McpError::invalid_params(
                format!("Book '{slug}' not found (deleted concurrently?)."),
                None,
            ));
        }
        self.storage.snapshots.delete_all(slug).await.map_err(|e| {
            McpError::internal_error(
                format!("Deleted book '{slug}' but failed to delete its snapshots: {e}"),
                None,
            )
        })?;

        let mut cleared = false;
        {
            let mut guard = self
                .selected
                .write()
                .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
            if guard.as_deref() == Some(slug) {
                *guard = None;
                cleared = true;
            }
        }
        let mut msg = format!("Deleted book '{slug}': {fp}.");
        if cleared {
            msg.push_str(
                " It was this session's selected book; select another with `select_book`.",
            );
        }
        Ok(msg)
    }

    /// 番号 or slug → slug に解決する。
    pub(crate) async fn resolve_book_ref(&self, book_ref: &str) -> Result<String, McpError> {
        if let Ok(num) = book_ref.parse::<usize>() {
            let slugs = self.list_book_slugs().await?;
            if num == 0 || num > slugs.len() {
                return Err(McpError::invalid_params(
                    format!(
                        "Book number {} out of range (1-{}). Use `shelf` to see available books.",
                        num,
                        slugs.len()
                    ),
                    None,
                ));
            }
            return Ok(slugs[num - 1].clone());
        }
        Ok(book_ref.to_string())
    }

    pub(crate) fn to_mcp_error(e: AppError) -> McpError {
        McpError::internal_error(format!("{e}"), None)
    }

    /// 階層番号 / Full UUID / short prefix / title部分一致 → NodeId。
    ///
    /// 優先順位:
    /// 1. 階層番号 (e.g. "1", "2-3") — `toc` 出力と対応
    /// 2. Full UUID
    /// 3. 短縮UUIDプレフィックス
    /// 4. タイトル部分一致（フォールバック）
    pub(crate) async fn resolve_id(&self, s: &str) -> Result<NodeId, McpError> {
        // 1. 階層番号（"1", "2-3", "1-2-1" 等）
        if is_hierarchical_id(s) {
            let svc = self.service().await?;
            let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
            let mapping = build_hierarchical_ids(&book);
            if let Some((_, id)) = mapping.iter().find(|(num, _)| num == s) {
                return Ok(*id);
            }
            return Err(McpError::invalid_params(
                format!("No node at position '{s}'. Run `toc` to see available IDs."),
                None,
            ));
        }

        // 2. Full UUIDとして解析
        if let Ok(id) = parse_node_id(s) {
            return Ok(id);
        }

        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

        // 3. 短縮プレフィックスでBook内を検索
        let id_matches: Vec<NodeId> = book
            .all_node_ids()
            .filter(|id| id.to_string().starts_with(s))
            .collect();
        match id_matches.len() {
            1 => return Ok(id_matches[0]),
            n if n > 1 => {
                return Err(McpError::invalid_params(
                    format!("Ambiguous ID prefix: '{s}' matches {n} nodes"),
                    None,
                ))
            }
            _ => {}
        }

        // 4. タイトル部分一致（case-insensitive, フォールバック）
        let query = s.to_lowercase();
        let title_matches: Vec<NodeId> = book
            .all_nodes_dfs()
            .iter()
            .filter(|node| node.title().to_lowercase().contains(&query))
            .map(|node| node.id())
            .collect();
        match title_matches.len() {
            0 => Err(McpError::invalid_params(
                format!("No node found matching: '{s}'"),
                None,
            )),
            1 => Ok(title_matches[0]),
            n => Err(McpError::invalid_params(
                format!(
                    "Ambiguous title match: '{s}' matches {n} nodes: {}",
                    title_matches
                        .iter()
                        .map(|id| {
                            let hier = find_hierarchical_id(&book, *id)
                                .unwrap_or_else(|| id.short().to_string());
                            book.get_node(*id)
                                .map(|node| format!("'{}' ({})", node.title(), hier))
                                .unwrap_or(hier)
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None,
            )),
        }
    }
}

// =============================================================================
// ServerHandler impl
// =============================================================================

impl ServerHandler for OutlineMcpServer {
    fn get_info(&self) -> ServerInfo {
        let server_info = Implementation::new("outline-mcp", env!("CARGO_PKG_VERSION"))
            .with_title("Outline MCP — Tree-Structured Knowledge Base")
            .with_description(
                "Persistent tree-structured notes with numbered IDs and property-based context injection. \
                 Workflow: `shelf` → `select_book` → `toc` → create/update nodes.",
            );
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        ServerInfo::new(capabilities)
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_server_info(server_info)
            .with_instructions(
                "Create and manage tree-structured knowledge notes.\n\
                 \n\
                 Intended flow: organize knowledge as tree nodes (sections and content), \
                 browse with `toc`, and use node properties for metadata.\n\
                 \n\
                 Context Injection: nodes with property `inject=true` have their body \
                 automatically included in `select_book` output — use this to inject \
                 persistent rules/context into every session.\n\
                 \n\
                 Tools: `shelf` → `select_book` → `toc` → `node_create`/`node_update`/`node_move`. \
                 `checklist` for task export. `init` for new book.\n\
                 History: `snapshot_create`/`snapshot_list`/`snapshot_restore` for versioning. \
                 `node_history` for change tracking. `dump` for full export.\n\
                 Batch: `node_batch_move`/`node_batch_update` for bulk operations (UUID required). \
                 Query: `node_query` for searching nodes by properties/status/type.\n\
                 Resources: read guides via `outline://guides/<name>` (see `resources/list`).",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_ctx = ToolCallContext::new(self, request, context);
        self.tool_router.call(tool_ctx).await
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(resources::list_all())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        resources::read(&request.uri).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "Unknown resource: '{}'. Use `resources/list` to see available URIs.",
                    request.uri
                ),
                None,
            )
        })
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    pub(crate) fn memory_server() -> OutlineMcpServer {
        OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ))
    }

    #[test]
    fn server_info() {
        let info = memory_server().get_info();
        assert_eq!(info.server_info.name, "outline-mcp");
        assert!(!info.server_info.version.is_empty());
    }

    #[tokio::test]
    async fn test_service_for_writes_land_in_store_history_and_listing() {
        use outline_mcp_core::domain::model::book::AddNodeRequest;
        use outline_mcp_core::domain::model::node::NodeType;
        use std::collections::HashMap;

        let server = memory_server();
        let slug = "wiring-book";

        let svc = server.service_for(slug).await.expect("service_for");
        svc.create_book("Wiring Test", 4)
            .await
            .expect("create_book");
        let (id, warning) = svc
            .add_node(AddNodeRequest {
                parent: None,
                title: "Node".to_string(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .await
            .expect("add_node");
        assert!(warning.is_none(), "atomic store never warns: {warning:?}");

        // A second service over the same storage sees the write and its event.
        let entries = server
            .service_for(slug)
            .await
            .unwrap()
            .history()
            .await
            .expect("history");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node_id, id);
        assert_eq!(server.list_book_slugs().await.unwrap(), vec![slug]);
        assert!(server.book_exists(slug).await.unwrap());
        assert_eq!(server.resolve_book_ref("1").await.unwrap(), slug);
    }

    #[test]
    fn selected_slug_errors_until_a_book_is_selected() {
        let server = memory_server();
        assert!(server.selected_slug().is_err());
        *server.selected.write().unwrap() = Some("b".into());
        assert_eq!(server.selected_slug().unwrap(), "b");
    }

    #[test]
    fn new_session_starts_unselected_and_does_not_share_selection() {
        let shared = memory_server();
        *shared.selected.write().unwrap() = Some("x".into());

        let a = shared.for_new_session();
        let b = shared.for_new_session();
        assert!(
            a.selected_slug().is_err(),
            "a new session starts unselected"
        );

        *a.selected.write().unwrap() = Some("a-book".into());
        *b.selected.write().unwrap() = Some("b-book".into());
        assert_eq!(a.selected_slug().unwrap(), "a-book");
        assert_eq!(b.selected_slug().unwrap(), "b-book");
        assert_eq!(shared.selected_slug().unwrap(), "x");
    }

    #[tokio::test]
    async fn sessions_write_to_the_book_each_selected() {
        use outline_mcp_core::domain::model::book::AddNodeRequest;
        use outline_mcp_core::domain::model::node::NodeType;
        use std::collections::HashMap;

        let shared = memory_server();
        for (slug, title) in [("a-book", "A"), ("b-book", "B")] {
            shared
                .service_for(slug)
                .await
                .unwrap()
                .create_book(title, 4)
                .await
                .unwrap();
        }

        let a = shared.for_new_session();
        let b = shared.for_new_session();
        *a.selected.write().unwrap() = Some("a-book".into());
        *b.selected.write().unwrap() = Some("b-book".into());

        let node = |title: &str| AddNodeRequest {
            parent: None,
            title: title.to_string(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        };
        a.service()
            .await
            .unwrap()
            .add_node(node("from a"))
            .await
            .unwrap();
        b.service()
            .await
            .unwrap()
            .add_node(node("from b"))
            .await
            .unwrap();

        let titles = |book: outline_mcp_core::domain::model::book::TemplateBook| {
            book.all_nodes_dfs()
                .iter()
                .map(|n| n.title().to_string())
                .collect::<Vec<_>>()
        };
        let a_book = shared
            .service_for("a-book")
            .await
            .unwrap()
            .read_tree()
            .await
            .unwrap();
        let b_book = shared
            .service_for("b-book")
            .await
            .unwrap()
            .read_tree()
            .await
            .unwrap();
        assert_eq!(titles(a_book), vec!["from a"]);
        assert_eq!(titles(b_book), vec!["from b"]);
    }

    /// A book `slug` with one node (and its create event) and one snapshot.
    async fn book_with_content(server: &OutlineMcpServer, slug: &str) {
        use outline_mcp_core::domain::model::book::AddNodeRequest;
        use outline_mcp_core::domain::model::node::NodeType;
        use std::collections::HashMap;

        let svc = server.service_for(slug).await.unwrap();
        svc.create_book(slug, 4).await.unwrap();
        svc.add_node(AddNodeRequest {
            parent: None,
            title: "n".to_string(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        })
        .await
        .unwrap();
        let book = svc.read_tree().await.unwrap();
        server
            .storage
            .snapshots
            .create(slug, &book, None)
            .await
            .unwrap();
    }

    fn footprint(nodes: usize, history: usize, snapshots: usize) -> Option<BookFootprint> {
        Some(BookFootprint {
            nodes,
            history,
            snapshots,
        })
    }

    #[tokio::test]
    async fn book_delete_refuses_a_non_empty_book_without_force() {
        let server = memory_server();
        book_with_content(&server, "full").await;

        let err = server.delete_book("full", false, false).await.unwrap_err();
        assert!(err.message.contains("not empty"), "{}", err.message);
        assert!(err
            .message
            .contains("1 nodes, 1 history entries, 1 snapshots"));
        assert_eq!(
            server.book_footprint("full").await.unwrap(),
            footprint(1, 1, 1)
        );
    }

    #[tokio::test]
    async fn book_delete_dry_run_reports_and_changes_nothing() {
        let server = memory_server();
        book_with_content(&server, "full").await;

        for force in [false, true] {
            let msg = server.delete_book("full", force, true).await.unwrap();
            assert!(msg.contains("would delete 'full': 1 nodes"), "{msg}");
        }
        assert_eq!(
            server.book_footprint("full").await.unwrap(),
            footprint(1, 1, 1)
        );
    }

    #[tokio::test]
    async fn book_delete_with_force_removes_book_history_and_snapshots() {
        let server = memory_server();
        book_with_content(&server, "full").await;
        book_with_content(&server, "other").await;

        let msg = server.delete_book("full", true, false).await.unwrap();
        assert!(msg.contains("Deleted book 'full': 1 nodes"), "{msg}");
        assert_eq!(server.book_footprint("full").await.unwrap(), None);
        assert!(server
            .storage
            .books
            .history("full")
            .await
            .unwrap()
            .is_empty());
        assert!(server
            .storage
            .snapshots
            .list("full")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(server.list_book_slugs().await.unwrap(), vec!["other"]);
        assert_eq!(
            server.book_footprint("other").await.unwrap(),
            footprint(1, 1, 1)
        );
    }

    #[tokio::test]
    async fn book_delete_of_an_empty_book_needs_no_force() {
        let server = memory_server();
        server
            .service_for("empty")
            .await
            .unwrap()
            .create_book("E", 4)
            .await
            .unwrap();
        server.delete_book("empty", false, false).await.unwrap();
        assert_eq!(server.book_footprint("empty").await.unwrap(), None);
    }

    #[tokio::test]
    async fn book_delete_of_a_missing_book_is_an_error() {
        let server = memory_server();
        let err = server.delete_book("nope", true, false).await.unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
        let err = server.delete_book("nope", false, true).await.unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
    }

    #[tokio::test]
    async fn book_delete_clears_only_the_calling_sessions_selection() {
        let shared = memory_server();
        book_with_content(&shared, "full").await;
        let a = shared.for_new_session();
        let b = shared.for_new_session();
        *a.selected.write().unwrap() = Some("full".into());
        *b.selected.write().unwrap() = Some("full".into());

        let msg = a.delete_book("full", true, false).await.unwrap();
        assert!(msg.contains("this session's selected book"), "{msg}");
        assert!(a.selected_slug().is_err());
        assert_eq!(
            b.selected_slug().unwrap(),
            "full",
            "other sessions untouched"
        );

        // A session whose selection points elsewhere is told nothing about it.
        book_with_content(&shared, "again").await;
        let msg = b.delete_book("again", true, false).await.unwrap();
        assert!(!msg.contains("selected"), "{msg}");
    }
}
