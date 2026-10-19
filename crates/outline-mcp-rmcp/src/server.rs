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
use outline_mcp_core::domain::model::book::TemplateBook;
use outline_mcp_core::domain::model::id::NodeId;
use outline_mcp_core::domain::store::{BookStore, SnapshotDelete, SnapshotStore, StoreError};

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

    let router = http_router(storage, token, loopback);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, auth = if loopback { "loopback" } else { "bearer-token" }, "serving MCP over streamable HTTP at /mcp and a read-only view at /ui");
    axum::serve(listener, router).await?;
    Ok(())
}

/// The HTTP router `run_http` serves: `/mcp` (streamable HTTP, behind the
/// bearer check when `token` is set) merged with the read-only `/ui`.
pub(crate) fn http_router(
    storage: Storage,
    token: Option<std::sync::Arc<str>>,
    loopback: bool,
) -> axum::Router {
    use std::sync::Arc;

    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let ui_storage = storage.clone();
    let server = OutlineMcpServer::new(storage);
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
    if let Some(token) = token.clone() {
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
    // The read-only browser view is merged after the bearer layer above, so
    // it is not wrapped by it: browsers send HTTP Basic, which `ui` checks
    // against the same token (or, without one, a loopback `Host`).
    let router = router.merge(crate::ui::router(ui_storage, token.clone()));
    // `Router::layer` above also wrapped the default fallback, so a path that
    // is neither /mcp nor /ui answered with the bearer 401. Merging replaces
    // that fallback with the default 404; restore the same answer: 401
    // without a valid bearer token, 404 with one.
    match token {
        Some(token) => router.fallback(move |req: axum::extract::Request| {
            let token = Arc::clone(&token);
            async move {
                use axum::response::IntoResponse;
                if bearer_token_matches(req.headers(), &token) {
                    axum::http::StatusCode::NOT_FOUND.into_response()
                } else {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        "missing or invalid bearer token",
                    )
                        .into_response()
                }
            }
        }),
        None => router,
    }
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

/// The error a session gets when the book it works on no longer exists.
pub(crate) const BOOK_GONE: &str =
    "The selected book no longer exists (it may have been deleted). \
     Use `shelf` to list books and `select_book` to choose one.";

/// What a book holds, as reported (and removed) by `book_delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BookFootprint {
    pub nodes: usize,
    pub history: usize,
    pub snapshots: usize,
    /// The book version the counts were read at; the delete is conditional
    /// on it.
    pub version: i64,
}

impl BookFootprint {
    /// Nothing that a delete would lose: no nodes, history or snapshots.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes == 0 && self.history == 0 && self.snapshots == 0
    }
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

    /// The book a read-only report looks at: `slug` when given, otherwise the
    /// selected book. Returns the slug with the loaded tree.
    pub(crate) async fn report_target(
        &self,
        slug: Option<&str>,
    ) -> Result<(String, TemplateBook), McpError> {
        match slug {
            Some(slug) => {
                crate::request::validate_slug(slug)?;
                let book = self
                    .storage
                    .books
                    .load(slug)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("Failed to read book '{slug}': {e}"), None)
                    })?
                    .ok_or_else(|| {
                        McpError::invalid_params(
                            format!("Book '{slug}' not found. Use `shelf` to list books."),
                            None,
                        )
                    })?;
                Ok((slug.to_string(), book.value))
            }
            None => {
                let slug = self.selected_slug()?;
                let book = self
                    .service_for(&slug)
                    .await?
                    .read_tree()
                    .await
                    .map_err(Self::to_mcp_error)?;
                Ok((slug, book))
            }
        }
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
            version: book.version,
        }))
    }

    /// The `book_delete` tool: delete `slug` with its nodes, history and
    /// snapshots. A book with any nodes, history or snapshots is refused
    /// unless `force`; `dry_run` only reports. The delete is conditional on
    /// the version the counts were read at, so a write in between makes it
    /// delete nothing. Clears this session's selection when it pointed at
    /// the deleted book; other sessions are not touched.
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
        if !force && !fp.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "Book '{slug}' is not empty ({fp}). Deleting it cannot be undone; \
                     pass `force: true` to delete it anyway, or `dry_run: true` to only report."
                ),
                None,
            ));
        }

        let deleted = match self.storage.books.delete(slug, fp.version).await {
            Ok(deleted) => deleted,
            Err(StoreError::Conflict { .. }) => {
                return Err(McpError::invalid_params(
                    format!(
                        "Book '{slug}' changed since it was checked ({fp} at that point); \
                         nothing was deleted. Check it again and retry."
                    ),
                    None,
                ))
            }
            Err(e) => {
                return Err(McpError::internal_error(
                    format!("Failed to delete book '{slug}': {e}"),
                    None,
                ))
            }
        };
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

    /// The `book_update` tool: change `slug`'s title, max depth and / or
    /// archived flag. At least one field is required; the book must exist.
    /// Validation failures (empty title, max depth 0 or below an existing
    /// node) are invalid params naming the problem; nothing is committed then.
    pub(crate) async fn update_book(
        &self,
        slug: &str,
        title: Option<String>,
        max_depth: Option<u8>,
        archived: Option<bool>,
    ) -> Result<String, McpError> {
        if title.is_none() && max_depth.is_none() && archived.is_none() {
            return Err(McpError::invalid_params(
                "Nothing to change: pass `title`, `max_depth` and / or `archived`.",
                None,
            ));
        }
        if !self.book_exists(slug).await? {
            return Err(McpError::invalid_params(
                format!("Book '{slug}' not found. Use `shelf` to list books."),
                None,
            ));
        }
        let svc = self.service_for(slug).await?;
        let up = svc
            .update_book(title, max_depth, archived)
            .await
            .map_err(|e| book_update_error(slug, e))?;
        let (b, a) = (&up.before, &up.after);
        if !up.changed {
            return Ok(format!(
                "No change: '{slug}' already has title \"{}\", max_depth {} and archived {}.",
                a.title, a.max_depth, a.archived
            ));
        }
        let mut parts = Vec::new();
        if b.title != a.title {
            parts.push(format!("title \"{}\" → \"{}\"", b.title, a.title));
        }
        if b.max_depth != a.max_depth {
            parts.push(format!("max_depth {} → {}", b.max_depth, a.max_depth));
        }
        if b.archived != a.archived {
            parts.push(if a.archived { "archived" } else { "unarchived" }.to_string());
        }
        Ok(format!("Updated book '{slug}': {}.", parts.join(", ")))
    }

    /// The `snapshot_delete` tool: delete one snapshot of the selected book.
    /// `timestamp` is the millis value `snapshot_list` shows. A labeled
    /// snapshot is refused unless `force`. The book itself is not changed, so
    /// nothing is added to its history.
    pub(crate) async fn delete_snapshot(
        &self,
        timestamp: &str,
        force: bool,
    ) -> Result<String, McpError> {
        let millis: i64 = timestamp.parse().map_err(|_| {
            McpError::invalid_params(
                format!("Invalid timestamp: '{timestamp}'. Must be a millis integer."),
                None,
            )
        })?;
        let slug = self.selected_slug()?;
        let ts = outline_mcp_core::domain::model::timestamp::Timestamp::from_millis(millis);

        // The store checks the label and removes in one step, so a label
        // attached concurrently still blocks a delete without `force`.
        let outcome = self
            .storage
            .snapshots
            .delete(&slug, ts, force)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Failed to delete snapshot {millis}: {e}"), None)
            })?;
        match outcome {
            SnapshotDelete::Deleted { label: Some(label) } => Ok(format!(
                "Deleted snapshot {millis} ('{label}') of '{slug}'."
            )),
            SnapshotDelete::Deleted { label: None } => {
                Ok(format!("Deleted snapshot {millis} of '{slug}'."))
            }
            SnapshotDelete::NotFound => Err(McpError::invalid_params(
                format!("Snapshot {millis} not found in '{slug}'. Use `snapshot_list`."),
                None,
            )),
            SnapshotDelete::Labeled(label) => Err(McpError::invalid_params(
                format!(
                    "Snapshot {millis} in '{slug}' is labeled '{label}'. Deleting it cannot be \
                     undone; pass `force: true` to delete it anyway."
                ),
                None,
            )),
        }
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
        match e {
            // The book the session works on is gone: deleted by another
            // session (`book_delete`) after this one selected it, or — for a
            // write — between its read and its commit. Not a server fault.
            AppError::BookNotFound | AppError::Conflict { actual: None, .. } => {
                McpError::invalid_params(BOOK_GONE, None)
            }
            // A rejected request (cycle, depth, unknown node, …) is the
            // caller's input, whether it came from a dry run or a real run.
            e @ AppError::Domain(_) => McpError::invalid_params(format!("{e}"), None),
            // So is a file `import` cannot read as a tree.
            e @ (AppError::ImportInvalidType(_) | AppError::ImportInvalid(_)) => {
                McpError::invalid_params(format!("{e}"), None)
            }
            e => McpError::internal_error(format!("{e}"), None),
        }
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
            return reject_book_level(id);
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

/// Map a `book_update` failure for the slug-addressed tool: the messages name
/// the slug, never "the selected book".
fn book_update_error(slug: &str, e: AppError) -> McpError {
    match e {
        AppError::Domain(d) => McpError::invalid_params(format!("{d}"), None),
        AppError::Conflict { actual: None, .. } | AppError::BookNotFound => {
            McpError::invalid_params(
                format!("Book '{slug}' not found. Use `shelf` to list books."),
                None,
            )
        }
        AppError::Conflict { .. } => McpError::invalid_params(
            format!("Book '{slug}' changed while updating; nothing was changed — retry."),
            None,
        ),
        e => OutlineMcpServer::to_mcp_error(e),
    }
}

/// A full UUID given as a node reference, unless it is the book-level id
/// (the nil UUID that marks book-level history entries), which names no node.
pub(crate) fn reject_book_level(id: NodeId) -> Result<NodeId, McpError> {
    if id.is_book_level() {
        return Err(McpError::invalid_params(
            format!("'{id}' is not a node id (it marks book-level history entries)"),
            None,
        ));
    }
    Ok(id)
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
                 Lifecycle: status `active`, `draft` (not written yet) or `archived` (retired: \
                 not injected, hidden from `toc`/`node_query` unless asked for). `node_verify` \
                 records that a node was confirmed; `node_query` `verified_before` lists stale ones.\n\
                 Links: `link_add`/`link_remove` keep typed links between nodes (e.g. `supersedes`, \
                 `see_also`); `links` lists a node's outgoing links and backlinks; `node_query` \
                 `linked_to` finds the nodes pointing at one.\n\
                 Review: `revision_propose` attaches a proposed change to a node without changing it \
                 (reads and injection keep the current content); `revision_show` diffs it, \
                 `revision_approve` applies it as one history entry, `revision_reject` drops it, \
                 `revisions_pending` lists them.\n\
                 \n\
                 Tools: `shelf` → `select_book` → `toc` → `node_create`/`node_update`/`node_move`. \
                 `checklist` for task export. `init` for new book, `book_update` to retitle, change max_depth or archive a book (hidden from `shelf`, nothing injected), `book_delete` to remove one.\n\
                 History: `snapshot_create`/`snapshot_list`/`snapshot_restore`/`snapshot_delete` for versioning. \
                 `node_history` for change tracking. `dump` for full export.\n\
                 Health: `book_lint` (structural smells) and `book_stats` (shape and freshness), both read-only.\n\
                 Batch: `node_batch_move`/`node_batch_update` for bulk operations (UUID required; \
                 `node_create` and `node_move` return it). `node_move`/`node_batch_move` take \
                 `dry_run` to preview what moves along with a node.\n\
                 Query: `node_query` for searching nodes by properties/status/type/`text`. \
                 On a large book use `toc` with `max_depth`/`limit`.\n\
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

    // Used by the book_delete tests below; earlier tests import them locally.
    use outline_mcp_core::domain::model::book::AddNodeRequest;
    use outline_mcp_core::domain::model::node::NodeType;
    use std::collections::HashMap;

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

    /// (nodes, history, snapshots) of `slug`, or `None` if it does not exist.
    async fn counts(server: &OutlineMcpServer, slug: &str) -> Option<(usize, usize, usize)> {
        server
            .book_footprint(slug)
            .await
            .unwrap()
            .map(|fp| (fp.nodes, fp.history, fp.snapshots))
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
        assert_eq!(counts(&server, "full").await, Some((1, 1, 1)));
    }

    #[tokio::test]
    async fn book_delete_dry_run_reports_and_changes_nothing() {
        let server = memory_server();
        book_with_content(&server, "full").await;

        for force in [false, true] {
            let msg = server.delete_book("full", force, true).await.unwrap();
            assert!(msg.contains("would delete 'full': 1 nodes"), "{msg}");
        }
        assert_eq!(counts(&server, "full").await, Some((1, 1, 1)));
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
        assert_eq!(counts(&server, "other").await, Some((1, 1, 1)));
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

    #[tokio::test]
    async fn book_delete_refuses_a_node_less_book_with_history_without_force() {
        let server = memory_server();
        book_with_content(&server, "hist").await;
        // Remove the node and the snapshot: only history is left.
        let svc = server.service_for("hist").await.unwrap();
        let id = svc.read_tree().await.unwrap().root_nodes()[0];
        svc.remove_node(id).await.unwrap();
        server.storage.snapshots.delete_all("hist").await.unwrap();
        assert_eq!(counts(&server, "hist").await, Some((0, 2, 0)));

        let err = server.delete_book("hist", false, false).await.unwrap_err();
        assert!(err.message.contains("not empty"), "{}", err.message);
        assert!(err.message.contains("0 nodes, 2 history entries"));
        server.delete_book("hist", true, false).await.unwrap();
        assert_eq!(counts(&server, "hist").await, None);
    }

    #[tokio::test]
    async fn book_delete_after_a_concurrent_write_deletes_nothing() {
        let server = memory_server();
        book_with_content(&server, "busy").await;
        let fp = server.book_footprint("busy").await.unwrap().unwrap();

        // Another session writes after the counts were read.
        server
            .service_for("busy")
            .await
            .unwrap()
            .add_node(AddNodeRequest {
                parent: None,
                title: "late".to_string(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .await
            .unwrap();

        let err = server
            .storage
            .books
            .delete("busy", fp.version)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }), "{err}");
        assert_eq!(counts(&server, "busy").await, Some((2, 2, 1)));
    }

    #[tokio::test]
    async fn a_session_whose_book_was_deleted_gets_a_clear_error() {
        let shared = memory_server();
        book_with_content(&shared, "gone").await;
        let reader = shared.for_new_session();
        *reader.selected.write().unwrap() = Some("gone".into());

        shared
            .for_new_session()
            .delete_book("gone", true, false)
            .await
            .unwrap();

        // Read path (what `toc` does).
        let err = reader
            .service()
            .await
            .unwrap()
            .read_tree()
            .await
            .map_err(OutlineMcpServer::to_mcp_error)
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("no longer exists"), "{}", err.message);

        // Write path (what `node_create` does).
        let err = reader
            .service()
            .await
            .unwrap()
            .add_node(AddNodeRequest {
                parent: None,
                title: "x".to_string(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .await
            .map_err(OutlineMcpServer::to_mcp_error)
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("no longer exists"), "{}", err.message);
    }

    #[tokio::test]
    async fn delete_then_init_the_same_slug_starts_empty() {
        let server = memory_server();
        book_with_content(&server, "reuse").await;
        server.delete_book("reuse", true, false).await.unwrap();

        server
            .service_for("reuse")
            .await
            .unwrap()
            .create_book("Again", 4)
            .await
            .unwrap();
        assert_eq!(counts(&server, "reuse").await, Some((0, 0, 0)));
    }

    /// The selected book `slug` with two snapshots: (unlabeled, labeled).
    async fn book_with_two_snapshots(server: &OutlineMcpServer, slug: &str) -> (i64, i64) {
        book_with_content(server, slug).await;
        let book = server
            .service_for(slug)
            .await
            .unwrap()
            .read_tree()
            .await
            .unwrap();
        let labeled = server
            .storage
            .snapshots
            .create(slug, &book, Some("keep"))
            .await
            .unwrap();
        let all = server.storage.snapshots.list(slug).await.unwrap();
        let unlabeled = all.iter().find(|s| s.label.is_none()).unwrap().timestamp;
        *server.selected.write().unwrap() = Some(slug.into());
        (unlabeled.as_millis(), labeled.timestamp.as_millis())
    }

    async fn snapshot_millis(server: &OutlineMcpServer, slug: &str) -> Vec<i64> {
        let mut v: Vec<i64> = server
            .storage
            .snapshots
            .list(slug)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.timestamp.as_millis())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn snapshot_delete_of_an_unknown_timestamp_is_an_error() {
        let server = memory_server();
        let (a, b) = book_with_two_snapshots(&server, "s").await;
        let err = server.delete_snapshot("1", false).await.unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
        let err = server
            .delete_snapshot("not-a-number", false)
            .await
            .unwrap_err();
        assert!(err.message.contains("Invalid timestamp"), "{}", err.message);
        assert_eq!(snapshot_millis(&server, "s").await, {
            let mut v = vec![a, b];
            v.sort();
            v
        });
    }

    #[tokio::test]
    async fn snapshot_delete_refuses_a_labeled_snapshot_without_force() {
        let server = memory_server();
        let (_, labeled) = book_with_two_snapshots(&server, "s").await;
        let err = server
            .delete_snapshot(&labeled.to_string(), false)
            .await
            .unwrap_err();
        assert!(err.message.contains("labeled 'keep'"), "{}", err.message);
        assert!(snapshot_millis(&server, "s").await.contains(&labeled));

        let msg = server
            .delete_snapshot(&labeled.to_string(), true)
            .await
            .unwrap();
        assert!(msg.contains("('keep')"), "{msg}");
        assert!(!snapshot_millis(&server, "s").await.contains(&labeled));
    }

    #[tokio::test]
    async fn snapshot_delete_of_an_unlabeled_snapshot_leaves_the_rest() {
        let server = memory_server();
        let (unlabeled, labeled) = book_with_two_snapshots(&server, "s").await;
        let before = counts(&server, "s").await.map(|(n, h, _)| (n, h));

        server
            .delete_snapshot(&unlabeled.to_string(), false)
            .await
            .unwrap();
        assert_eq!(snapshot_millis(&server, "s").await, vec![labeled]);
        // The book and its history are untouched.
        assert_eq!(counts(&server, "s").await.map(|(n, h, _)| (n, h)), before);
        // Reading the deleted snapshot (restore / dump / diff all go through
        // load_snapshot) is now "not found".
        let err = server.load_snapshot("s", unlabeled).await.unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
    }

    #[tokio::test]
    async fn snapshot_delete_needs_a_selected_book() {
        let server = memory_server();
        let err = server.delete_snapshot("1", true).await.unwrap_err();
        assert!(err.message.contains("No book selected"), "{}", err.message);
    }

    #[tokio::test]
    async fn book_update_max_depth_zero_is_invalid_params() {
        let server = memory_server();
        server
            .service_for("z")
            .await
            .unwrap()
            .create_book("Z", 3)
            .await
            .unwrap();
        let err = server
            .update_book("z", None, Some(0), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        let book = server
            .service_for("z")
            .await
            .unwrap()
            .read_tree()
            .await
            .unwrap();
        assert_eq!(book.max_depth(), 3, "nothing committed");
    }

    #[test]
    fn book_update_errors_name_the_slug() {
        let conflict = book_update_error(
            "b",
            AppError::Conflict {
                expected: Some(1),
                actual: Some(2),
            },
        );
        assert_eq!(conflict.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            conflict.message.contains("Book 'b' changed while updating"),
            "{}",
            conflict.message
        );

        for gone in [
            AppError::BookNotFound,
            AppError::Conflict {
                expected: Some(1),
                actual: None,
            },
        ] {
            let e = book_update_error("b", gone);
            assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert!(e.message.contains("Book 'b' not found"), "{}", e.message);
            assert!(!e.message.contains("selected"), "{}", e.message);
        }
    }

    #[tokio::test]
    async fn the_book_level_id_is_not_a_node_reference() {
        let server = memory_server();
        server
            .service_for("n")
            .await
            .unwrap()
            .create_book("N", 3)
            .await
            .unwrap();
        *server.selected.write().unwrap() = Some("n".into());
        // node_history / node_update / ... resolve through resolve_id: the nil
        // UUID (digits and dashes only) never reaches a store lookup.
        let nil = NodeId::book_level().to_string();
        let err = server.resolve_id(&nil).await.unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);

        // The batch tools parse full UUIDs directly; that path rejects it too.
        let err = reject_book_level(NodeId::book_level()).unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("not a node id"), "{}", err.message);
        let id = NodeId::new();
        assert_eq!(reject_book_level(id).unwrap(), id);
    }
}
