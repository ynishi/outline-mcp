// SMARTLINT: Status::InReview (1774100840)
//! MCP Server for outline-mcp
//!
//! MCP Protocol (stdio) <-> application::BookService / EjectService
//!
//! 8 tools: init, node_create, node_update, node_move, toc, checklist, import, gen_routing

mod helpers;
mod request;
mod tools;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rmcp::{
    handler::server::{tool::ToolCallContext, tool::ToolRouter},
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};

use crate::application::error::AppError;
use crate::application::service::BookService;
use crate::domain::model::id::NodeId;
use crate::infra::changelog_store::JsonChangeLogRepository;
use crate::infra::json_store::JsonBookRepository;

use helpers::{build_hierarchical_ids, find_hierarchical_id, is_hierarchical_id};
use request::parse_node_id;

// =============================================================================
// Public entry point
// =============================================================================

/// MCP Serverを起動する。shelf_dirは複数Book格納ディレクトリ。
pub async fn run(shelf_dir: PathBuf) -> anyhow::Result<()> {
    let server = OutlineMcpServer::new(shelf_dir);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

#[derive(Clone)]
pub(super) struct OutlineMcpServer {
    pub(super) shelf_dir: PathBuf,
    pub(super) selected: Arc<RwLock<Option<String>>>,
    tool_router: ToolRouter<Self>,
}

impl OutlineMcpServer {
    pub(super) fn new(shelf_dir: PathBuf) -> Self {
        Self {
            shelf_dir,
            selected: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// slug からBookファイルパスを返す。
    pub(super) fn book_path(&self, slug: &str) -> PathBuf {
        self.shelf_dir.join(format!("{slug}.json"))
    }

    /// 選択中BookのServiceを返す。未選択ならエラー。
    pub(super) fn service(&self) -> Result<BookService<JsonBookRepository>, McpError> {
        let guard = self
            .selected
            .read()
            .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
        let slug = guard.as_ref().ok_or_else(|| {
            McpError::invalid_params(
                "No book selected. Use `shelf` to list books and `select_book` to choose one.",
                None,
            )
        })?;
        let repo = JsonBookRepository::new(self.book_path(slug));
        let changelog = Box::new(JsonChangeLogRepository::new(&self.shelf_dir, slug.as_str()));
        Ok(BookService::new(repo).with_changelog(changelog))
    }

    /// 指定slugのServiceを返す（選択状態不要）。
    pub(super) fn service_for(&self, slug: &str) -> BookService<JsonBookRepository> {
        let repo = JsonBookRepository::new(self.book_path(slug));
        let changelog = Box::new(JsonChangeLogRepository::new(&self.shelf_dir, slug));
        BookService::new(repo).with_changelog(changelog)
    }

    /// Shelf内のslug一覧をソート順で返す。
    pub(super) fn list_book_slugs(&self) -> Result<Vec<String>, McpError> {
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
    pub(super) fn resolve_book_ref(&self, book_ref: &str) -> Result<String, McpError> {
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

    pub(super) fn to_mcp_error(e: AppError) -> McpError {
        McpError::internal_error(format!("{e}"), None)
    }

    /// 階層番号 / Full UUID / short prefix / title部分一致 → NodeId。
    ///
    /// 優先順位:
    /// 1. 階層番号 (e.g. "1", "2-3") — `toc` 出力と対応
    /// 2. Full UUID
    /// 3. 短縮UUIDプレフィックス
    /// 4. タイトル部分一致（フォールバック）
    pub(super) fn resolve_id(&self, s: &str) -> Result<NodeId, McpError> {
        // 1. 階層番号（"1", "2-3", "1-2-1" 等）
        if is_hierarchical_id(s) {
            let svc = self.service()?;
            let book = svc.read_tree().map_err(Self::to_mcp_error)?;
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

        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

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
        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "outline-mcp".to_string(),
                title: Some("Outline MCP — Tree-Structured Knowledge Base".to_string()),
                description: Some(
                    "Persistent tree-structured notes with numbered IDs and property-based context injection. \
                     Workflow: `shelf` → `select_book` → `toc` → create/update nodes."
                        .to_string(),
                ),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: None,
            },
            instructions: Some(
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
                 Query: `node_query` for searching nodes by properties/status/type."
                    .to_string(),
            ),
        }
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
}
