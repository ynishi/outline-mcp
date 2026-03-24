use std::collections::HashMap;
use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use crate::application::eject::{EjectConfig, EjectFormat, EjectService, EjectTree};

use super::helpers::{build_hierarchical_ids, find_hierarchical_id, format_toc};
use super::request::{
    normalize_text, parse_node_type, sanitize_for_filename, unescape_newlines, validate_filename,
    validate_import_path, validate_slug, McpDumpRequest, McpEjectRequest, McpGenRoutingRequest,
    McpImportRequest, McpInitRequest, McpNodeCreateRequest, McpNodeHistoryRequest,
    McpNodeMoveRequest, McpNodeUpdateRequest, McpSelectBookRequest, McpShelfRequest,
    McpSnapshotCreateRequest, McpSnapshotListRequest, McpSnapshotRestoreRequest, McpTocRequest,
};
use super::OutlineMcpServer;

use crate::domain::model::book::AddNodeRequest;
use crate::domain::model::book::UpdateNodeRequest;
use crate::domain::model::changelog::{ChangeAction, ChangeEntry, NodeStatus};
use crate::domain::model::timestamp::Timestamp;
use crate::infra::changelog_store::JsonChangeLogRepository;
use crate::infra::snapshot::SnapshotService;

#[tool_router(vis = "pub(super)")]
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
            properties: req.properties.unwrap_or_default(),
        };

        let (id, warning) = svc.add_node(add_req).map_err(Self::to_mcp_error)?;

        // 階層番号を逆引き
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;
        let hier = find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());

        let mut msg = format!(
            "Created: {}. {}",
            hier,
            book.get_node(id).map(|n| n.title()).unwrap_or("?")
        );
        if let Some(w) = warning {
            msg.push_str(&format!("\n[WARNING] {w}"));
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
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
            properties: req.properties,
        };

        let ((), warning) = svc
            .update_node(id, update_req)
            .map_err(Self::to_mcp_error)?;

        let book = svc.read_tree().map_err(Self::to_mcp_error)?;
        let hier = find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());

        let mut msg = format!(
            "Updated: {}. {}",
            hier,
            book.get_node(id).map(|n| n.title()).unwrap_or("?")
        );
        if let Some(w) = warning {
            msg.push_str(&format!("\n[WARNING] {w}"));
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
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
                let ((), warning) = svc
                    .move_node(id, new_parent, position)
                    .map_err(Self::to_mcp_error)?;

                let book = svc.read_tree().map_err(Self::to_mcp_error)?;
                let hier =
                    find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());
                let mut msg = format!(
                    "Moved → {}. {}",
                    hier,
                    book.get_node(id).map(|n| n.title()).unwrap_or("?")
                );
                if let Some(w) = warning {
                    msg.push_str(&format!("\n[WARNING] {w}"));
                }
                Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                    msg,
                )]))
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

                let ((), warning) = svc.remove_node(id).map_err(Self::to_mcp_error)?;
                let mut msg = format!("Removed: {}. {} (and descendants)", hier, title);
                if let Some(w) = warning {
                    msg.push_str(&format!("\n[WARNING] {w}"));
                }
                Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                    msg,
                )]))
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

        let mut nodes = match subtree_id {
            Some(root_id) => book.subtree_nodes(root_id),
            None => book.all_nodes_dfs(),
        };

        // プロパティフィルタ
        if let Some(ref filter) = req.filter {
            if !filter.is_empty() {
                nodes.retain(|node| {
                    filter
                        .iter()
                        .all(|(k, v)| node.get_property(k).map(|pv| pv == v).unwrap_or(false))
                });
            }
        }

        if nodes.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                "No matching nodes. Use `node_create` to add nodes.",
            )]));
        }

        let output = format_toc(&book, &nodes);
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
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

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!("Checklist exported to: {}", path.display()),
        )]))
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

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!("Imported '{}': {} nodes", tree.title, node_count),
        )]))
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

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Created book: '{}' (slug: {}, max_depth: {}). Auto-selected.",
                book.title(),
                req.slug,
                book.max_depth()
            ),
        )]))
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
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
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

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
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

        // Context Injection: inject=true ノードの body を自動出力
        let inject_filter = {
            let mut m = HashMap::new();
            m.insert("inject".to_string(), "true".to_string());
            m
        };
        let injected_nodes: Vec<_> = book
            .nodes_matching(&inject_filter)
            .into_iter()
            .filter(|node| node.status() != NodeStatus::Draft)
            .collect();
        let inject_section = if injected_nodes.is_empty() {
            String::new()
        } else {
            let mut buf = format!(
                "\n\n---\n# Injected Context ({} rules)\n",
                injected_nodes.len()
            );
            for node in &injected_nodes {
                buf.push_str(&format!("\n## {}\n", node.title()));
                if let Some(body) = node.body() {
                    buf.push_str(body);
                    buf.push('\n');
                }
            }
            buf
        };

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Selected: {} — \"{}\" ({} nodes){}{}",
                slug,
                book.title(),
                book.node_count(),
                toc_section,
                inject_section
            ),
        )]))
    }

    #[tool(
        name = "gen_routing",
        description = "Generate a Markdown routing table from nodes with `routing` property across all books. Set `routing` property on nodes to define work scenarios (e.g. routing=\"Git操作\"). Use `|` separator for multiple scenarios. Optional `routing_ref` property overrides the default §ID reference (e.g. routing_ref=\"select_book で全体参照\").",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn gen_routing(
        &self,
        #[allow(unused_variables)] Parameters(_req): Parameters<McpGenRoutingRequest>,
    ) -> Result<CallToolResult, McpError> {
        let slugs = self.list_book_slugs()?;

        // Collect: (scene, book_slug, reference_text)
        let mut entries: Vec<(String, String, String)> = Vec::new();

        for slug in &slugs {
            let svc = self.service_for(slug);
            let book = match svc.read_tree() {
                Ok(b) => b,
                Err(_) => continue,
            };

            let id_map = build_hierarchical_ids(&book);

            for node in book.all_nodes_dfs() {
                let routing = match node.get_property("routing") {
                    Some(v) => v.to_string(),
                    None => continue,
                };

                let reference = if let Some(r) = node.get_property("routing_ref") {
                    r.to_string()
                } else {
                    let hier = id_map
                        .iter()
                        .find(|(_, id)| *id == node.id())
                        .map(|(num, _)| num.as_str())
                        .unwrap_or("?");
                    format!("§{} {}", hier, node.title())
                };

                for scene in routing.split('|') {
                    let scene = scene.trim();
                    if !scene.is_empty() {
                        entries.push((scene.to_string(), slug.clone(), reference.clone()));
                    }
                }
            }
        }

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                "No nodes with `routing` property found. Add `routing` property to nodes to include them in the routing table.",
            )]));
        }

        // Group same (scene, book) → merge references
        let mut grouped: Vec<(String, String, Vec<String>)> = Vec::new();
        for (scene, book, reference) in &entries {
            if let Some(existing) = grouped.iter_mut().find(|(s, b, _)| s == scene && b == book) {
                existing.2.push(reference.clone());
            } else {
                grouped.push((scene.clone(), book.clone(), vec![reference.clone()]));
            }
        }

        // Sort by book, then scene
        grouped.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        let mut output = String::from("| 場面 | Book | ノード |\n|---|---|---|\n");
        for (scene, book, refs) in &grouped {
            output.push_str(&format!("| {} | {} | {} |\n", scene, book, refs.join(", ")));
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
    }

    #[tool(
        name = "snapshot_create",
        description = "Create a snapshot of the current book state. Use `snapshot_list` to view saved snapshots and `snapshot_restore` to revert.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn snapshot_create(
        &self,
        #[allow(unused_variables)] Parameters(_req): Parameters<McpSnapshotCreateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

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

        let path = SnapshotService::create(&self.shelf_dir, &slug, &book).map_err(|e| {
            McpError::internal_error(format!("Failed to create snapshot: {e}"), None)
        })?;

        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        // ファイル名からタイムスタンプ(millis)を取得
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let millis_str = stem.rsplit('.').next().unwrap_or("");

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!("Snapshot created: {} ({} bytes)", millis_str, size_bytes),
        )]))
    }

    #[tool(
        name = "snapshot_list",
        description = "List all snapshots for the selected book. Use the timestamp value with `snapshot_restore` to revert to a specific state.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn snapshot_list(
        &self,
        #[allow(unused_variables)] Parameters(_req): Parameters<McpSnapshotListRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

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

        let infos = SnapshotService::list(&self.shelf_dir, &slug).map_err(|e| {
            McpError::internal_error(format!("Failed to list snapshots: {e}"), None)
        })?;

        if infos.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                format!("No snapshots for \"{}\".", book.title()),
            )]));
        }

        let mut output = format!(
            "# Snapshots for \"{}\" ({} snapshots)\n\n",
            book.title(),
            infos.len()
        );
        for (i, info) in infos.iter().enumerate() {
            let size_kb = info.size_bytes as f64 / 1024.0;
            output.push_str(&format!(
                "{}. {} — {} ({:.1} KB)\n",
                i + 1,
                info.timestamp.to_iso8601(),
                info.timestamp.as_millis(),
                size_kb
            ));
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
    }

    #[tool(
        name = "snapshot_restore",
        description = "Restore the selected book from a snapshot. This overwrites the current book state. Use `snapshot_list` to find available timestamps.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn snapshot_restore(
        &self,
        Parameters(req): Parameters<McpSnapshotRestoreRequest>,
    ) -> Result<CallToolResult, McpError> {
        let millis: i64 = req.timestamp.parse().map_err(|_| {
            McpError::invalid_params(
                format!(
                    "Invalid timestamp: '{}'. Must be a millis integer.",
                    req.timestamp
                ),
                None,
            )
        })?;

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

        let restored = SnapshotService::restore(&self.shelf_dir, &slug, millis).map_err(|e| {
            McpError::internal_error(format!("Failed to restore snapshot: {e}"), None)
        })?;

        let node_count = restored.node_count();

        // changelog に Restore エントリを記録（ベストエフォート）
        let cl_repo = JsonChangeLogRepository::new(&self.shelf_dir, &slug);
        let ts = Timestamp::now();
        let mut warning: Option<String> = None;
        for id in restored.all_node_ids() {
            let entry = ChangeEntry::new(id, ChangeAction::Restore, None, None, ts);
            if let Err(e) = crate::domain::repository::ChangeLogRepository::append(&cl_repo, &entry)
            {
                warning = Some(format!("changelog write failed: {e}"));
                break;
            }
        }

        let svc = self.service()?;
        svc.save_book(&restored).map_err(Self::to_mcp_error)?;

        let mut msg = format!(
            "Restored from snapshot {}. {} nodes.",
            req.timestamp, node_count
        );
        if let Some(w) = warning {
            msg.push_str(&format!("\n[WARNING] {w}"));
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "node_history",
        description = "Show the change history for a specific node. Returns entries in chronological order (oldest first).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn node_history(
        &self,
        Parameters(req): Parameters<McpNodeHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let id = self.resolve_id(&req.node_id)?;

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

        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

        let title = book
            .get_node(id)
            .map(|n| n.title().to_string())
            .unwrap_or_else(|| id.short().to_string());

        let cl_repo = JsonChangeLogRepository::new(&self.shelf_dir, &slug);
        let mut entries = crate::domain::repository::ChangeLogRepository::load_by_node(
            &cl_repo, id,
        )
        .map_err(|e| McpError::internal_error(format!("Failed to load history: {e}"), None))?;

        // 時系列順（古い順）
        entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                format!("No history for \"{}\".", title),
            )]));
        }

        let mut output = format!(
            "# History for \"{}\" ({} entries)\n\n",
            title,
            entries.len()
        );
        for (i, entry) in entries.iter().enumerate() {
            let action_str = match entry.action {
                ChangeAction::Create => "create",
                ChangeAction::Update => "update",
                ChangeAction::Delete => "delete",
                ChangeAction::Move => "move",
                ChangeAction::Restore => "restore",
            };
            output.push_str(&format!(
                "{}. [{}] {}\n",
                i + 1,
                entry.timestamp.to_iso8601(),
                action_str
            ));
            if let Some(ref before) = entry.before {
                output.push_str(&format!("   before: {}\n", before));
            }
            if let Some(ref after) = entry.after {
                output.push_str(&format!("   after: {}\n", after));
            }
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
    }

    #[tool(
        name = "dump",
        description = "Export the entire selected book to a file. Unlike `checklist`, this always exports the full book (no subtree). Supports markdown (default) and json formats.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn dump(
        &self,
        Parameters(req): Parameters<McpDumpRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

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

        let default_ext = match format {
            EjectFormat::Markdown => "md",
            EjectFormat::Json => "json",
        };

        let filename = match req.filename {
            Some(f) => f,
            None => format!("{}.{}", sanitize_for_filename(book.title()), default_ext),
        };
        validate_filename(&filename)?;

        let output_dir = PathBuf::from(&req.output_dir);

        let config = EjectConfig {
            output_dir,
            filename,
            include_placeholders: true,
            format,
            subtree_root: None,
        };

        let path = EjectService::eject(&book, &config).map_err(Self::to_mcp_error)?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!("Book dumped to: {}", path.display()),
        )]))
    }
}
