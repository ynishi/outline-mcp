// SMARTLINT: Status::InReview (1774100840)
//! [`OutlineMcpServer`]: the `ServerHandler` implementation and its
//! `shelf_dir` / `selected` state.
//!
//! MCP Protocol (stdio) <-> application::BookService / EjectService

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use ai_store_core::Store;
use ai_store_sqlite::SqliteStore;
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
use tokio::sync::Mutex as AsyncMutex;

use outline_mcp_core::application::error::AppError;
use outline_mcp_core::application::service::BookService;
use outline_mcp_core::domain::model::id::NodeId;
use outline_mcp_core::infra::changelog_bridge::HistoryPreservingChangeLogRepository;
use outline_mcp_core::infra::json_store::JsonBookRepository;
use outline_mcp_core::infra::snapshot::SnapshotService;
use outline_mcp_core::infra::snapshot_migrator::count_orphan_snapshots;
use outline_mcp_core::infra::snapshot_sink::SnapshotOnlySink;

use crate::helpers::{build_hierarchical_ids, find_hierarchical_id, is_hierarchical_id};
use crate::request::parse_node_id;
use crate::resources;

// =============================================================================
// Public entry point
// =============================================================================

/// MCP Serverを起動する。shelf_dirは複数Book格納ディレクトリ。
pub async fn run(shelf_dir: PathBuf) -> anyhow::Result<()> {
    // Best-effort: a minimal stderr-only subscriber so `tracing::warn!`
    // calls (e.g. `OutlineMcpServer::store_for`'s orphan-snapshot warning)
    // are actually visible somewhere. stdout is reserved for the MCP stdio
    // JSON-RPC transport below — writing anywhere else there would corrupt
    // the protocol stream, so this must never target stdout. `try_init`
    // (rather than `init`) tolerates a subscriber already having been
    // installed (e.g. by an embedding host, or a repeated call in tests).
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();

    let server = OutlineMcpServer::new(shelf_dir);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

/// The outline-mcp MCP server.
///
/// Holds the shelf directory (the directory containing one JSON file per
/// book) and the currently selected book, and implements `ServerHandler` by
/// dispatching MCP tool calls onto
/// `outline_mcp_core::application::service::BookService`.
#[derive(Clone)]
pub struct OutlineMcpServer {
    pub(crate) shelf_dir: PathBuf,
    pub(crate) selected: Arc<RwLock<Option<String>>>,
    tool_router: ToolRouter<Self>,
    /// Lazily constructed, slug-keyed `ai_store_sqlite::SqliteStore` handles
    /// (bundles the `Store`, its SQLite backend driver, and the shared
    /// `AsyncIsle` in one type — see `Self::store_for`) backing both
    /// `snapshot_service_for` and `changelog_for`. One SQLite file per slug
    /// (`{shelf_dir}/{slug}.events.db`), opened on first access and reused
    /// thereafter — opening spawns a dedicated backend thread
    /// (`ai-store-sqlite`), so this must not happen on every tool call.
    snapshot_stores: Arc<AsyncMutex<HashMap<String, SqliteStore>>>,
}

impl OutlineMcpServer {
    /// Construct a new server rooted at `shelf_dir` (the directory
    /// containing one JSON file per book). No book is selected until
    /// `select_book` (or `init`) is called.
    pub fn new(shelf_dir: PathBuf) -> Self {
        Self {
            shelf_dir,
            selected: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
            snapshot_stores: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Returns the (lazily constructed, cached) ai-store `Store` for `slug`,
    /// with a `SnapshotOnlySink` registered so snapshot dumps land on disk.
    /// Shared by both the snapshot subsystem (`Self::snapshot_service_for`)
    /// and the per-node changelog (`Self::changelog_for`).
    ///
    /// Built via `SqliteStore::open_with` — the one-call assembly
    /// `ai-store-sqlite` provides over hand-wiring `SqliteBackends` +
    /// `Store::new` (which this used to do, ignoring `SqliteBackends`'
    /// checkpoint backend entirely). `SqliteStore` bundles the `Store`, its
    /// SQLite backend driver, and the shared `AsyncIsle` in one cached
    /// value, and derives durable (SQLite-persisted) sink checkpoints along
    /// the way; nothing in this server currently calls `Store::catch_up` /
    /// `Store::rebuild` (the only consumers of that checkpoint), so this is
    /// presently inert but strictly more correct than the in-memory-only
    /// checkpoints the hand-wired construction had.
    ///
    /// `Store` is cheap to clone (every field is `Arc`-backed internally —
    /// see `ai_store_core::Store`'s doc comment), so returning a fresh
    /// `Arc::new(..)` around a `.clone()` of the cached `SqliteStore`'s
    /// `Store` on every call is equivalent to sharing one `Arc<Store>`: the
    /// per-stream write locks, checkpoint map, and registered sinks are the
    /// same underlying instances either way.
    pub(crate) async fn store_for(&self, slug: &str) -> Result<Arc<Store>, McpError> {
        {
            let cache = self.snapshot_stores.lock().await;
            if let Some(entry) = cache.get(slug) {
                return Ok(Arc::new(entry.store().clone()));
            }
        }

        std::fs::create_dir_all(&self.shelf_dir).map_err(|e| {
            McpError::internal_error(format!("Failed to create shelf directory: {e}"), None)
        })?;
        let db_path = self.shelf_dir.join(format!("{slug}.events.db"));
        let sink_shelf_dir = self.shelf_dir.clone();
        let sink_slug = slug.to_string();
        let sqlite_store = SqliteStore::open_with(&db_path, move |builder| {
            builder.sink(Arc::new(SnapshotOnlySink::new(sink_shelf_dir, sink_slug)))
        })
        .await
        .map_err(|e| {
            McpError::internal_error(
                format!("Failed to open event store for '{slug}': {e}"),
                None,
            )
        })?;
        let store = Arc::new(sqlite_store.store().clone());

        // Best-effort: warn about un-migrated legacy `.snap.*.json` files
        // for this slug now that its `Store` has been freshly constructed
        // (see `count_orphan_snapshots`'s doc comment — this is an exact
        // count of files not yet imported). `store_for` is lazy — called on
        // first access, not eagerly for every book at process startup — so
        // this warning surfaces on that same first touch rather than at
        // server boot. A failure to count (e.g. a permission error reading
        // `shelf_dir`) is silently ignored: this is a UX nicety, not
        // something that should block the store from being usable.
        if let Ok(count) = count_orphan_snapshots(&self.shelf_dir, slug, Arc::clone(&store)).await {
            if count > 0 {
                tracing::warn!(
                    "outline-mcp: {count} unmigrated snapshot(s) detected for slug '{slug}'. Run: outline-mcp migrate-snapshots --shelf {}",
                    self.shelf_dir.display()
                );
            }
        }

        let mut cache = self.snapshot_stores.lock().await;
        let entry = cache.entry(slug.to_string()).or_insert(sqlite_store);
        Ok(Arc::new(entry.store().clone()))
    }

    /// Convenience wrapper: `SnapshotService` bound to `slug`'s `Store`.
    pub(crate) async fn snapshot_service_for(
        &self,
        slug: &str,
    ) -> Result<SnapshotService, McpError> {
        let store = self.store_for(slug).await?;
        Ok(SnapshotService::new(
            store,
            self.shelf_dir.clone(),
            slug.to_string(),
        ))
    }

    /// slug からBookファイルパスを返す。
    pub(crate) fn book_path(&self, slug: &str) -> PathBuf {
        self.shelf_dir.join(format!("{slug}.json"))
    }

    /// Constructs the (ai-store-backed, JSON-history-preserving) changelog
    /// repository for `slug`, sharing `slug`'s `Store` with the snapshot
    /// subsystem (see `Self::store_for`). Single construction point used by
    /// `Self::service_for` and by tool handlers that query/append changelog
    /// entries outside of a `BookService` (e.g. `book_history`,
    /// `node_history`, the `Restore` entries `snapshot_restore` records).
    pub(crate) async fn changelog_for(
        &self,
        slug: &str,
    ) -> Result<HistoryPreservingChangeLogRepository, McpError> {
        let store = self.store_for(slug).await?;
        HistoryPreservingChangeLogRepository::new(store, self.shelf_dir.clone(), slug).map_err(
            |e| {
                McpError::internal_error(
                    format!("Failed to construct changelog for slug '{slug}': {e}"),
                    None,
                )
            },
        )
    }

    /// 選択中BookのServiceを返す。未選択ならエラー。
    pub(crate) async fn service(&self) -> Result<BookService<JsonBookRepository>, McpError> {
        let slug = {
            let guard = self
                .selected
                .read()
                .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
            guard
                .as_ref()
                .ok_or_else(|| {
                    McpError::invalid_params(
                        "No book selected. Use `shelf` to list books and `select_book` to choose one.",
                        None,
                    )
                })?
                .clone()
        };
        self.service_for(&slug).await
    }

    /// 指定slugのServiceを返す（選択状態不要）。
    pub(crate) async fn service_for(
        &self,
        slug: &str,
    ) -> Result<BookService<JsonBookRepository>, McpError> {
        let repo = JsonBookRepository::new(self.book_path(slug));
        let changelog = Box::new(self.changelog_for(slug).await?);
        Ok(BookService::new(repo).with_changelog(changelog))
    }

    /// Shelf内のslug一覧をソート順で返す。
    pub(crate) fn list_book_slugs(&self) -> Result<Vec<String>, McpError> {
        if !self.shelf_dir.exists() {
            return Ok(Vec::new());
        }
        let dir = std::fs::read_dir(&self.shelf_dir)
            .map_err(|e| McpError::internal_error(format!("Failed to read shelf: {e}"), None))?;
        let mut slugs: Vec<String> = dir
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                let ext_ok = path.extension().and_then(|x| x.to_str()) == Some("json");
                let stem_ok = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| !s.contains('.'))
                    .unwrap_or(false);
                ext_ok && stem_ok
            })
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)
            })
            .collect();
        slugs.sort();
        Ok(slugs)
    }

    /// 番号 or slug → slug に解決する。
    pub(crate) fn resolve_book_ref(&self, book_ref: &str) -> Result<String, McpError> {
        if let Ok(num) = book_ref.parse::<usize>() {
            let slugs = self.list_book_slugs()?;
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

    #[test]
    fn server_info() {
        let server = OutlineMcpServer::new(PathBuf::from("/tmp/test-shelf"));
        let info = server.get_info();
        assert_eq!(info.server_info.name, "outline-mcp");
        assert!(!info.server_info.version.is_empty());
    }

    #[tokio::test]
    async fn test_service_for_and_changelog_for_share_slug_history() {
        use outline_mcp_core::domain::model::book::AddNodeRequest;
        use outline_mcp_core::domain::model::node::NodeType;
        use outline_mcp_core::domain::repository::ChangeLogRepository;
        use std::collections::HashMap;

        let dir = std::env::temp_dir().join("outline-mcp-server-changelog-wiring-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp shelf dir");

        let server = OutlineMcpServer::new(dir.clone());
        let slug = "wiring-book";

        // `service_for` now backs its changelog with `changelog_for`'s
        // ai-store-shared `Store` (Task B wiring) instead of a standalone
        // `JsonChangeLogRepository` — this exercises that path end to end.
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
        assert!(
            warning.is_none(),
            "changelog append should succeed: {warning:?}"
        );

        // `changelog_for` shares `slug`'s `Store` with `service_for` (both
        // go through `Self::store_for`) — querying it directly must see
        // the entry `service_for`'s `BookService` just wrote.
        let cl_repo = server.changelog_for(slug).await.expect("changelog_for");
        let entries = ChangeLogRepository::load_all(&cl_repo)
            .await
            .expect("load_all");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node_id, id);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
