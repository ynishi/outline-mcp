//! MCP Server for outline-mcp
//!
//! MCP Protocol (stdio) <-> application::BookService / EjectService
//!
//! 7 tools: init, node_create, node_update, node_move, toc, checklist, import

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rmcp::{
    handler::server::{tool::ToolCallContext, tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::application::eject::{EjectConfig, EjectFormat, EjectService, EjectTree};
use crate::application::error::AppError;
use crate::application::service::BookService;
use crate::domain::model::book::{AddNodeRequest, UpdateNodeRequest};
use crate::domain::model::id::NodeId;
use crate::domain::model::node::NodeType;
use crate::infra::json_store::JsonBookRepository;

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
struct OutlineMcpServer {
    shelf_dir: PathBuf,
    selected: Arc<RwLock<Option<String>>>,
    tool_router: ToolRouter<Self>,
}

impl OutlineMcpServer {
    fn new(shelf_dir: PathBuf) -> Self {
        Self {
            shelf_dir,
            selected: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// slug からBookファイルパスを返す。
    fn book_path(&self, slug: &str) -> PathBuf {
        self.shelf_dir.join(format!("{slug}.json"))
    }

    /// 選択中BookのServiceを返す。未選択ならエラー。
    fn service(&self) -> Result<BookService<JsonBookRepository>, McpError> {
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
        Ok(BookService::new(repo))
    }

    /// 指定slugのServiceを返す（選択状態不要）。
    fn service_for(&self, slug: &str) -> BookService<JsonBookRepository> {
        let repo = JsonBookRepository::new(self.book_path(slug));
        BookService::new(repo)
    }

    /// Shelf内のslug一覧をソート順で返す。
    fn list_book_slugs(&self) -> Result<Vec<String>, McpError> {
        if !self.shelf_dir.exists() {
            return Ok(Vec::new());
        }
        let dir = std::fs::read_dir(&self.shelf_dir)
            .map_err(|e| McpError::internal_error(format!("Failed to read shelf: {e}"), None))?;
        let mut slugs: Vec<String> = dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
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
    fn resolve_book_ref(&self, book_ref: &str) -> Result<String, McpError> {
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

    fn to_mcp_error(e: AppError) -> McpError {
        McpError::internal_error(format!("{e}"), None)
    }

    /// 階層番号 / Full UUID / short prefix / title部分一致 → NodeId。
    ///
    /// 優先順位:
    /// 1. 階層番号 (e.g. "1", "2-3") — `toc` 出力と対応
    /// 2. Full UUID
    /// 3. 短縮UUIDプレフィックス
    /// 4. タイトル部分一致（フォールバック）
    fn resolve_id(&self, s: &str) -> Result<NodeId, McpError> {
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
                                .map(|n| format!("'{}' ({})", n.title(), hier))
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
                title: Some("Outline MCP — Session Guide & Checklist".to_string()),
                description: Some(
                    "Tree-structured runbook with numbered IDs. \
                     2-step workflow: `toc` → pick ID → `checklist`."
                        .to_string(),
                ),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Create and manage action-ready checklists.\n\
                 \n\
                 Intended flow: capture knowledge as content nodes (one verifiable action each), \
                 organize under section nodes, export via `checklist` when executing tasks.\n\
                 \n\
                 Tools: `shelf` → `select_book` → `toc` → `node_create`/`node_update`/`node_move`, `checklist`. \
                 `init` for new book."
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
// Request types
// =============================================================================

/// slugが安全なファイル名であることを検証する。
fn validate_slug(slug: &str) -> Result<(), McpError> {
    if slug.is_empty() {
        return Err(McpError::invalid_params("slug must not be empty", None));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(McpError::invalid_params(
            "slug must contain only alphanumeric characters, hyphens, and underscores",
            None,
        ));
    }
    Ok(())
}

/// タイトルをファイル名に安全な文字列に変換する。
/// 英数字・`-_.()`以外を`_`に置換し、連続`_`を圧縮、先頭末尾の`_`を除去する。
fn sanitize_for_filename(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();

    // 連続`_`を1つに圧縮
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_underscore = true; // true開始で先頭`_`を除去
    for c in sanitized.chars() {
        if c == '_' {
            if !prev_underscore {
                result.push('_');
            }
            prev_underscore = true;
        } else {
            result.push(c);
            prev_underscore = false;
        }
    }

    // 末尾`_`を除去
    while result.ends_with('_') {
        result.pop();
    }

    // `..`をpath traversal防止のため`_`に置換
    while result.contains("..") {
        result = result.replace("..", "_");
    }

    if result.is_empty() {
        "untitled".to_string()
    } else {
        result
    }
}

/// filenameにパス区切り文字や".."が含まれていないことを検証する。
fn validate_filename(filename: &str) -> Result<(), McpError> {
    if filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.is_empty()
    {
        return Err(McpError::invalid_params(
            "filename must not contain path separators, '..', or be empty",
            None,
        ));
    }
    Ok(())
}

/// importパスの拡張子を検証する。
fn validate_import_path(file_path: &str) -> Result<PathBuf, McpError> {
    let path = PathBuf::from(file_path);
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => Ok(path),
        _ => Err(McpError::invalid_params(
            "Only .json files can be imported",
            None,
        )),
    }
}

fn parse_node_type(s: &str) -> Result<NodeType, McpError> {
    match s {
        "section" => Ok(NodeType::Section),
        "content" => Ok(NodeType::Content),
        other => Err(McpError::invalid_params(
            format!("Unknown node_type: '{other}'. Use: section, content"),
            None,
        )),
    }
}

/// MCP経由のテキストに含まれるリテラル `\n` を実際の改行に変換する。
fn unescape_newlines(s: &str) -> String {
    s.replace("\\n", "\n")
}

fn normalize_text(s: Option<String>) -> Option<String> {
    s.map(|v| unescape_newlines(&v))
}

fn parse_node_id(s: &str) -> Result<NodeId, McpError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| McpError::invalid_params(format!("Invalid node_id: '{s}'"), None))
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpNodeCreateRequest {
    #[schemars(
        description = "Parent ID from `toc` output (e.g. '1', '2-3'). Omit for root-level node. UUID also accepted."
    )]
    pub parent: Option<String>,
    #[schemars(description = "Node title (required)")]
    pub title: String,
    #[schemars(description = "Node type: section or content")]
    pub node_type: String,
    #[schemars(description = "Optional markdown body content")]
    pub body: Option<String>,
    #[schemars(
        description = "Optional placeholder hint for checklist export (e.g. 'write test cases here')"
    )]
    pub placeholder: Option<String>,
    #[schemars(description = "Position among siblings (0-based). Omit to append at end.")]
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpNodeUpdateRequest {
    #[schemars(description = "Node ID from `toc` output (e.g. '2-3'). UUID also accepted.")]
    pub node_id: String,
    #[schemars(description = "New title (omit to keep current)")]
    pub title: Option<String>,
    #[schemars(description = "New body (null to clear, omit to keep current)")]
    pub body: Option<Option<String>>,
    #[schemars(description = "New node type: section or content")]
    pub node_type: Option<String>,
    #[schemars(description = "New placeholder hint (null to clear)")]
    pub placeholder: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpNodeMoveRequest {
    #[schemars(description = "Node ID from `toc` output (e.g. '2-3'). UUID also accepted.")]
    pub node_id: String,
    #[schemars(description = "Action: 'move' to relocate, 'remove' to delete (with descendants)")]
    pub action: String,
    #[schemars(
        description = "New parent ID from `toc` output (null for root). Required for 'move' action."
    )]
    pub new_parent: Option<String>,
    #[schemars(description = "Position among new siblings (0-based). Default: append at end.")]
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpTocRequest {
    #[schemars(description = "Section ID from `toc` output (e.g. '2'). Omit to show entire book.")]
    pub subtree_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpEjectRequest {
    #[schemars(description = "Output directory path (default: current directory)")]
    pub output_dir: Option<String>,
    #[schemars(description = "Output filename (default: '<book-title>.md')")]
    pub filename: Option<String>,
    #[schemars(description = "Include placeholder hints as fill-in fields (default: true)")]
    pub include_placeholders: Option<bool>,
    #[schemars(description = "Output format: 'markdown' (default) or 'json' (tree-structured)")]
    pub format: Option<String>,
    #[schemars(
        description = "Section ID from `toc` output (e.g. '2'). Omit to export entire book."
    )]
    pub subtree_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpImportRequest {
    #[schemars(description = "Path to JSON file exported by eject (format: json)")]
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpInitRequest {
    #[schemars(description = "Book title")]
    pub title: String,
    #[schemars(
        description = "Book slug for filename (e.g. 'rust', 'development'). Alphanumeric, hyphens, underscores only."
    )]
    pub slug: String,
    #[schemars(description = "Maximum tree depth (default: 4, recommended: 3-4)")]
    pub max_depth: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpShelfRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpSelectBookRequest {
    #[schemars(
        description = "Book to select: number from `shelf` output (e.g. '1') or book slug (e.g. 'rust')"
    )]
    pub book: String,

    #[schemars(description = "Suppress TOC output (default: false)")]
    #[serde(default)]
    pub quiet: bool,
}

// =============================================================================
// Tool implementations
// =============================================================================

#[tool_router]
impl OutlineMcpServer {
    #[tool(
        name = "node_create",
        description = "Add a new node to the book. Use a parent ID from `toc` output (e.g. '1') to nest under a section, or omit for root-level.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn node_create(
        &self,
        Parameters(req): Parameters<McpNodeCreateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let node_type = parse_node_type(&req.node_type)?;
        let parent = req
            .parent
            .as_deref()
            .map(|s| self.resolve_id(s))
            .transpose()?;

        let add_req = AddNodeRequest {
            parent,
            title: unescape_newlines(&req.title),
            node_type,
            body: normalize_text(req.body),
            placeholder: normalize_text(req.placeholder),
            position: req.position.unwrap_or(usize::MAX),
        };

        let id = svc.add_node(add_req).map_err(Self::to_mcp_error)?;

        // 階層番号を逆引き
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;
        let hier = find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Created: {}. {}",
            hier,
            book.get_node(id).map(|n| n.title()).unwrap_or("?")
        ))]))
    }

    #[tool(
        name = "node_update",
        description = "Edit a node's title, body, type, or placeholder. Specify the node by ID from `toc` output (e.g. '2-3'). Only specified fields are changed.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn node_update(
        &self,
        Parameters(req): Parameters<McpNodeUpdateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let id = self.resolve_id(&req.node_id)?;
        let node_type = req.node_type.as_deref().map(parse_node_type).transpose()?;

        let update_req = UpdateNodeRequest {
            title: req.title.map(|t| unescape_newlines(&t)),
            body: req.body.map(normalize_text),
            node_type,
            placeholder: req.placeholder.map(normalize_text),
        };

        svc.update_node(id, update_req)
            .map_err(Self::to_mcp_error)?;

        let book = svc.read_tree().map_err(Self::to_mcp_error)?;
        let hier = find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Updated: {}. {}",
            hier,
            book.get_node(id).map(|n| n.title()).unwrap_or("?")
        ))]))
    }

    #[tool(
        name = "node_move",
        description = "Move or delete a node (and its descendants). Specify node by ID from `toc` output (e.g. '2-3'). Action 'move' relocates, 'remove' deletes.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn node_move(
        &self,
        Parameters(req): Parameters<McpNodeMoveRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let id = self.resolve_id(&req.node_id)?;

        match req.action.as_str() {
            "move" => {
                let new_parent = req
                    .new_parent
                    .as_deref()
                    .map(|s| self.resolve_id(s))
                    .transpose()?;
                let position = req.position.unwrap_or(usize::MAX);
                svc.move_node(id, new_parent, position)
                    .map_err(Self::to_mcp_error)?;

                let book = svc.read_tree().map_err(Self::to_mcp_error)?;
                let hier =
                    find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Moved → {}. {}",
                    hier,
                    book.get_node(id).map(|n| n.title()).unwrap_or("?")
                ))]))
            }
            "remove" => {
                // 削除前に階層番号を取得
                let book = svc.read_tree().map_err(Self::to_mcp_error)?;
                let hier =
                    find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());
                let title = book
                    .get_node(id)
                    .map(|n| n.title().to_string())
                    .unwrap_or_default();

                svc.remove_node(id).map_err(Self::to_mcp_error)?;
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Removed: {}. {} (and descendants)",
                    hier, title
                ))]))
            }
            other => Err(McpError::invalid_params(
                format!("Unknown action: '{other}'. Use: move, remove"),
                None,
            )),
        }
    }

    #[tool(
        name = "toc",
        description = "Show table of contents with numbered IDs (e.g. 1, 1-1, 2-3). Run this first — use the returned IDs to specify nodes in `checklist`, `node_create`, and other tools.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn toc(
        &self,
        Parameters(req): Parameters<McpTocRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

        let subtree_id = req
            .subtree_root
            .as_deref()
            .map(|s| self.resolve_id(s))
            .transpose()?;

        let nodes = match subtree_id {
            Some(root_id) => book.subtree_nodes(root_id),
            None => book.all_nodes_dfs(),
        };

        if nodes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Book is empty. Use `node_create` to add nodes.",
            )]));
        }

        let output = format_toc(&book, &nodes);
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        name = "checklist",
        description = "Export a section as a Markdown checklist with checkboxes. First run `toc` to find the section ID, then pass it as subtree_root (e.g. '2'). Omit subtree_root for full book export. Book is NOT modified.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn checklist(
        &self,
        Parameters(req): Parameters<McpEjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

        let include_placeholders = req.include_placeholders.unwrap_or(true);
        let format = match req.format.as_deref() {
            Some("json") => EjectFormat::Json,
            Some("markdown") | None => EjectFormat::Markdown,
            Some(other) => {
                return Err(McpError::invalid_params(
                    format!("Unknown format: '{other}'. Use: markdown, json"),
                    None,
                ))
            }
        };
        let subtree_root = req
            .subtree_root
            .as_deref()
            .map(|s| self.resolve_id(s))
            .transpose()?;

        let output_dir = req
            .output_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let default_ext = match format {
            EjectFormat::Markdown => "md",
            EjectFormat::Json => "json",
        };
        let filename = req.filename.unwrap_or_else(|| {
            match subtree_root {
                Some(root_id) => {
                    // subtree指定時: "2_Testing.md", "6-3_DSL_Architecture.md"
                    let hier =
                        find_hierarchical_id(&book, root_id).unwrap_or_else(|| "0".to_string());
                    let title = book
                        .get_node(root_id)
                        .map(|n| sanitize_for_filename(n.title()))
                        .unwrap_or_else(|| "unknown".to_string());
                    format!("{}_{}.{}", hier, title, default_ext)
                }
                None => {
                    format!("{}.{}", sanitize_for_filename(book.title()), default_ext)
                }
            }
        });
        validate_filename(&filename)?;

        let config = EjectConfig {
            output_dir,
            filename,
            include_placeholders,
            format,
            subtree_root,
        };

        let path = EjectService::eject(&book, &config).map_err(Self::to_mcp_error)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Checklist exported to: {}",
            path.display()
        ))]))
    }

    #[tool(
        name = "import",
        description = "Import a book from a JSON file (previously exported with `checklist` format: json). Replaces the current book entirely.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn import(
        &self,
        Parameters(req): Parameters<McpImportRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let import_path = validate_import_path(&req.file_path)?;
        let content = std::fs::read_to_string(&import_path)
            .map_err(|e| McpError::internal_error(format!("Failed to read file: {e}"), None))?;
        let tree: EjectTree = serde_json::from_str(&content)
            .map_err(|e| McpError::invalid_params(format!("Invalid JSON: {e}"), None))?;

        let book = EjectService::import_tree(&tree).map_err(Self::to_mcp_error)?;
        let node_count = book.node_count();
        svc.save_book(&book).map_err(Self::to_mcp_error)?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Imported '{}': {} nodes",
            tree.title, node_count
        ))]))
    }

    #[tool(
        name = "init",
        description = "Create a new book in the shelf. Requires a slug (filename) and title. Auto-selects the new book.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn init(
        &self,
        Parameters(req): Parameters<McpInitRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_slug(&req.slug)?;

        let path = self.book_path(&req.slug);
        if path.exists() {
            return Err(McpError::invalid_params(
                format!(
                    "Book '{}' already exists. Choose a different slug.",
                    req.slug
                ),
                None,
            ));
        }

        std::fs::create_dir_all(&self.shelf_dir).map_err(|e| {
            McpError::internal_error(format!("Failed to create shelf directory: {e}"), None)
        })?;

        let svc = self.service_for(&req.slug);
        let max_depth = req.max_depth.unwrap_or(4);
        let book = svc
            .create_book(&req.title, max_depth)
            .map_err(Self::to_mcp_error)?;

        // Auto-select
        let mut guard = self
            .selected
            .write()
            .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
        *guard = Some(req.slug.clone());

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Created book: '{}' (slug: {}, max_depth: {}). Auto-selected.",
            book.title(),
            req.slug,
            book.max_depth()
        ))]))
    }

    #[tool(
        name = "shelf",
        description = "List all books in the shelf. Shows book slugs, titles, and node counts. The currently selected book is marked with ★.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn shelf(
        &self,
        #[allow(unused_variables)] Parameters(_req): Parameters<McpShelfRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slugs = self.list_book_slugs()?;

        if slugs.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Shelf is empty. Use `init` to create a new book.",
            )]));
        }

        let selected = self
            .selected
            .read()
            .map_err(|_| McpError::internal_error("Lock poisoned", None))?;

        let mut entries: Vec<(String, String, usize)> = Vec::new();
        for slug in &slugs {
            let svc = self.service_for(slug);
            match svc.read_tree() {
                Ok(book) => {
                    entries.push((slug.clone(), book.title().to_string(), book.node_count()));
                }
                Err(_) => {
                    entries.push((slug.clone(), "(failed to load)".to_string(), 0));
                }
            }
        }

        let mut output = format!("# Shelf ({} books)\n\n", entries.len());
        for (i, (slug, title, count)) in entries.iter().enumerate() {
            let marker = if selected.as_deref() == Some(slug.as_str()) {
                " ★"
            } else {
                ""
            };
            output.push_str(&format!(
                "{}. {} — \"{}\" ({} nodes){}\n",
                i + 1,
                slug,
                title,
                count,
                marker
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        name = "select_book",
        description = "Select a book to work with. Use a number from `shelf` output or a book slug. All subsequent operations (toc, node_create, etc.) will target the selected book. Automatically shows TOC unless quiet=true.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn select_book(
        &self,
        Parameters(req): Parameters<McpSelectBookRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slug = self.resolve_book_ref(&req.book)?;

        let path = self.book_path(&slug);
        if !path.exists() {
            return Err(McpError::invalid_params(
                format!(
                    "Book '{}' not found in shelf. Use `shelf` to list available books.",
                    slug
                ),
                None,
            ));
        }

        let svc = self.service_for(&slug);
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

        let mut guard = self
            .selected
            .write()
            .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
        *guard = Some(slug.clone());

        let toc_section = if req.quiet {
            String::new()
        } else {
            let nodes = book.all_nodes_dfs();
            if nodes.is_empty() {
                String::from("\n(empty)")
            } else {
                format!("\n\n{}", format_toc(&book, &nodes))
            }
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Selected: {} — \"{}\" ({} nodes){}",
            slug,
            book.title(),
            book.node_count(),
            toc_section
        ))]))
    }
}

// =============================================================================
// Helpers — Hierarchical ID (e.g. "1", "2-3", "1-2-1")
// =============================================================================

use crate::domain::model::book::TemplateBook;
use crate::domain::model::node::TemplateNode;

/// Book の全ノードを TOC 形式にフォーマットする。
fn format_toc(book: &TemplateBook, nodes: &[&TemplateNode]) -> String {
    let id_map = build_hierarchical_ids(book);
    let mut output = format!("# {} ({} nodes)\n\n", book.title(), book.node_count());
    for node in nodes {
        let depth = book.depth_of(node.id());
        let indent = "  ".repeat(depth.saturating_sub(1) as usize);
        let hier_id = id_map
            .iter()
            .find(|(_, id)| *id == node.id())
            .map(|(num, _)| num.as_str())
            .unwrap_or("?");
        output.push_str(&format!("{}{}. {}\n", indent, hier_id, node.title()));
    }
    output
}

/// 階層番号かどうか判定（`1`, `2-3`, `1-2-1` 等）
fn is_hierarchical_id(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Book全体の (階層番号, NodeId) マッピングをDFS順で構築する。
fn build_hierarchical_ids(book: &TemplateBook) -> Vec<(String, NodeId)> {
    let mut result = Vec::new();
    for (i, &root_id) in book.root_nodes().iter().enumerate() {
        let num = format!("{}", i + 1);
        result.push((num.clone(), root_id));
        collect_children_ids(book, root_id, &num, &mut result);
    }
    result
}

fn collect_children_ids(
    book: &TemplateBook,
    parent_id: NodeId,
    parent_num: &str,
    result: &mut Vec<(String, NodeId)>,
) {
    if let Some(node) = book.get_node(parent_id) {
        for (j, &child_id) in node.children().iter().enumerate() {
            let num = format!("{}-{}", parent_num, j + 1);
            result.push((num.clone(), child_id));
            collect_children_ids(book, child_id, &num, result);
        }
    }
}

/// 指定NodeIdの階層番号を逆引きする。
fn find_hierarchical_id(book: &TemplateBook, target: NodeId) -> Option<String> {
    build_hierarchical_ids(book)
        .into_iter()
        .find(|(_, id)| *id == target)
        .map(|(num, _)| num)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_node_type_valid() {
        assert_eq!(parse_node_type("section").unwrap(), NodeType::Section);
        assert_eq!(parse_node_type("content").unwrap(), NodeType::Content);
    }

    #[test]
    fn parse_node_type_invalid() {
        assert!(parse_node_type("unknown").is_err());
    }

    #[test]
    fn server_info() {
        let server = OutlineMcpServer::new(PathBuf::from("/tmp/test-shelf"));
        let info = server.get_info();
        assert_eq!(info.server_info.name, "outline-mcp");
        assert!(!info.server_info.version.is_empty());
    }

    #[test]
    fn init_request_with_slug() {
        let req: McpInitRequest =
            serde_json::from_str(r#"{"title": "Test", "slug": "test"}"#).unwrap();
        assert_eq!(req.title, "Test");
        assert_eq!(req.slug, "test");
        assert!(req.max_depth.is_none());
    }

    #[test]
    fn validate_slug_valid() {
        assert!(validate_slug("rust").is_ok());
        assert!(validate_slug("my-book").is_ok());
        assert!(validate_slug("dev_standards").is_ok());
        assert!(validate_slug("book123").is_ok());
    }

    #[test]
    fn validate_slug_invalid() {
        assert!(validate_slug("").is_err());
        assert!(validate_slug("has space").is_err());
        assert!(validate_slug("path/traversal").is_err());
        assert!(validate_slug("dot..dot").is_err());
        assert!(validate_slug("日本語").is_err());
    }

    #[test]
    fn shelf_request_empty() {
        let _req: McpShelfRequest = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn select_book_request() {
        let req: McpSelectBookRequest = serde_json::from_str(r#"{"book": "rust"}"#).unwrap();
        assert_eq!(req.book, "rust");
        assert!(!req.quiet);
    }

    #[test]
    fn select_book_request_quiet() {
        let req: McpSelectBookRequest =
            serde_json::from_str(r#"{"book": "rust", "quiet": true}"#).unwrap();
        assert_eq!(req.book, "rust");
        assert!(req.quiet);
    }

    #[test]
    fn node_create_request_minimal() {
        let req: McpNodeCreateRequest =
            serde_json::from_str(r#"{"title": "Step 1", "node_type": "content"}"#).unwrap();
        assert_eq!(req.title, "Step 1");
        assert!(req.parent.is_none());
        assert!(req.body.is_none());
    }

    #[test]
    fn node_move_request_remove() {
        let req: McpNodeMoveRequest = serde_json::from_str(
            r#"{"node_id": "00000000-0000-0000-0000-000000000001", "action": "remove"}"#,
        )
        .unwrap();
        assert_eq!(req.action, "remove");
        assert!(req.new_parent.is_none());
    }

    #[test]
    fn eject_request_defaults() {
        let req: McpEjectRequest = serde_json::from_str("{}").unwrap();
        assert!(req.output_dir.is_none());
        assert!(req.filename.is_none());
        assert!(req.include_placeholders.is_none());
        assert!(req.format.is_none());
        assert!(req.subtree_root.is_none());
    }

    #[test]
    fn import_request_parse() {
        let req: McpImportRequest =
            serde_json::from_str(r#"{"file_path": "/tmp/book.json"}"#).unwrap();
        assert_eq!(req.file_path, "/tmp/book.json");
    }

    // ---- Hierarchical ID tests ----

    #[test]
    fn is_hierarchical_id_valid() {
        assert!(is_hierarchical_id("1"));
        assert!(is_hierarchical_id("2-3"));
        assert!(is_hierarchical_id("1-2-1"));
        assert!(is_hierarchical_id("10-20-30"));
    }

    #[test]
    fn is_hierarchical_id_invalid() {
        assert!(!is_hierarchical_id(""));
        assert!(!is_hierarchical_id("abc"));
        assert!(!is_hierarchical_id("1-"));
        assert!(!is_hierarchical_id("-1"));
        assert!(!is_hierarchical_id("1--2"));
        assert!(!is_hierarchical_id("a1b2c3d4")); // UUID short prefix
    }

    #[test]
    fn build_hierarchical_ids_flat() {
        use crate::domain::model::book::AddNodeRequest;

        let mut book = TemplateBook::new("Test", 4);
        let a = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "A".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        let b = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "B".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();

        let ids = build_hierarchical_ids(&book);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ("1".to_string(), a));
        assert_eq!(ids[1], ("2".to_string(), b));
    }

    #[test]
    fn build_hierarchical_ids_nested() {
        use crate::domain::model::book::AddNodeRequest;

        let mut book = TemplateBook::new("Test", 4);
        let sec = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Section".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        let c1 = book
            .add_node(AddNodeRequest {
                parent: Some(sec),
                title: "Child 1".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        let c2 = book
            .add_node(AddNodeRequest {
                parent: Some(sec),
                title: "Child 2".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();

        let ids = build_hierarchical_ids(&book);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], ("1".to_string(), sec));
        assert_eq!(ids[1], ("1-1".to_string(), c1));
        assert_eq!(ids[2], ("1-2".to_string(), c2));
    }

    #[test]
    fn find_hierarchical_id_lookup() {
        use crate::domain::model::book::AddNodeRequest;

        let mut book = TemplateBook::new("Test", 4);
        let _a = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "A".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        let b = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "B".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();

        assert_eq!(find_hierarchical_id(&book, b), Some("2".to_string()));
    }

    // ---- sanitize_for_filename tests ----

    #[test]
    fn sanitize_basic_title() {
        assert_eq!(sanitize_for_filename("Hello World"), "Hello_World");
    }

    #[test]
    fn sanitize_slash_in_title() {
        assert_eq!(sanitize_for_filename("TCP/IP Basics"), "TCP_IP_Basics");
        assert_eq!(sanitize_for_filename("I/O Operations"), "I_O_Operations");
    }

    #[test]
    fn sanitize_backslash() {
        assert_eq!(sanitize_for_filename("Windows\\Linux"), "Windows_Linux");
    }

    #[test]
    fn sanitize_preserves_allowed_chars() {
        assert_eq!(
            sanitize_for_filename("Review (Google EP)"),
            "Review_(Google_EP)"
        );
        assert_eq!(sanitize_for_filename("v2.0-beta_1"), "v2.0-beta_1");
    }

    #[test]
    fn sanitize_collapses_consecutive_underscores() {
        assert_eq!(sanitize_for_filename("a / b / c"), "a_b_c");
        assert_eq!(sanitize_for_filename("foo   bar"), "foo_bar");
    }

    #[test]
    fn sanitize_strips_leading_trailing_underscores() {
        assert_eq!(sanitize_for_filename(" leading"), "leading");
        assert_eq!(sanitize_for_filename("trailing "), "trailing");
        assert_eq!(sanitize_for_filename(" both "), "both");
    }

    #[test]
    fn sanitize_double_dot_prevention() {
        assert_eq!(sanitize_for_filename("Config..Settings"), "Config_Settings");
        assert_eq!(sanitize_for_filename("a...b"), "a_.b");
    }

    #[test]
    fn sanitize_special_characters() {
        assert_eq!(sanitize_for_filename("file: name?"), "file_name");
        assert_eq!(sanitize_for_filename("a<b>c|d\"e"), "a_b_c_d_e");
    }

    #[test]
    fn sanitize_unicode() {
        assert_eq!(sanitize_for_filename("日本語タイトル"), "untitled");
        assert_eq!(sanitize_for_filename("混合 Mixed テスト"), "Mixed");
    }

    #[test]
    fn sanitize_empty_and_whitespace() {
        assert_eq!(sanitize_for_filename(""), "untitled");
        assert_eq!(sanitize_for_filename("   "), "untitled");
        assert_eq!(sanitize_for_filename("///"), "untitled");
    }
}
