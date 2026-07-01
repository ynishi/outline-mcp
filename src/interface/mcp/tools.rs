use std::collections::HashMap;
use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use crate::application::eject::{EjectConfig, EjectFormat, EjectService, EjectTree};

use super::helpers::{build_hierarchical_ids, find_hierarchical_id, format_toc};
use super::request::{
    normalize_text, parse_node_id, parse_node_status, parse_node_type, sanitize_for_filename,
    unescape_newlines, validate_filename, validate_import_path, validate_slug, McpBatchMoveRequest,
    McpBatchUpdateRequest, McpDumpRequest, McpEjectRequest, McpGenRoutingRequest, McpImportRequest,
    McpInitRequest, McpNodeCreateRequest, McpNodeHistoryRequest, McpNodeMoveRequest,
    McpNodeQueryRequest, McpNodeUpdateRequest, McpSelectBookRequest, McpShelfRequest,
    McpBookHistoryRequest, McpSnapshotCreateRequest, McpSnapshotDiffRequest,
    McpSnapshotDumpAllRequest, McpSnapshotDumpRequest, McpSnapshotListRequest,
    McpSnapshotRestoreRequest, McpSnapshotTagRequest, McpTocRequest,
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

        let status = req.status.as_deref().map(parse_node_status).transpose()?;

        let update_req = UpdateNodeRequest {
            title: req.title.map(|t| unescape_newlines(&t)),
            body: req.body.map(normalize_text),
            node_type,
            placeholder: req.placeholder.map(normalize_text),
            properties: req.properties,
            status,
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
        description = "Create a snapshot of the current book state. Optionally attach a label (sidecar '.meta.json'; snapshot body schema is unchanged). Use `snapshot_list` to view saved snapshots and `snapshot_restore` to revert.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn snapshot_create(
        &self,
        Parameters(req): Parameters<McpSnapshotCreateRequest>,
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

        let label = match req.label.as_deref() {
            Some(s) => Some(validate_snapshot_label(s)?),
            None => None,
        };

        let path = SnapshotService::create(&self.shelf_dir, &slug, &book, label.as_deref())
            .map_err(|e| {
                McpError::internal_error(format!("Failed to create snapshot: {e}"), None)
            })?;

        let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        // ファイル名からタイムスタンプ(millis)を取得
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let millis_str = stem.rsplit('.').next().unwrap_or("");

        let label_note = label
            .as_deref()
            .map(|l| format!(" [label: {l}]"))
            .unwrap_or_default();

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Snapshot created: {} ({} bytes){}",
                millis_str, size_bytes, label_note
            ),
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
            let label_note = info
                .label
                .as_deref()
                .map(|l| format!(" [{}]", l))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}. {} — {} ({:.1} KB){}\n",
                i + 1,
                info.timestamp.to_iso8601(),
                info.timestamp.as_millis(),
                size_kb,
                label_note
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
        name = "snapshot_tag",
        description = "Attach or overwrite a label on an existing snapshot. Writes only the sidecar '.meta.json'; the snapshot body is not touched. Use `snapshot_list` to find timestamps.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn snapshot_tag(
        &self,
        Parameters(req): Parameters<McpSnapshotTagRequest>,
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

        let label = validate_snapshot_label(&req.label)?;

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

        let meta_path =
            SnapshotService::tag(&self.shelf_dir, &slug, millis, &label).map_err(|e| {
                McpError::internal_error(format!("Failed to tag snapshot: {e}"), None)
            })?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Snapshot {} tagged as '{}' (meta: {})",
                millis,
                label,
                meta_path.display()
            ),
        )]))
    }

    #[tool(
        name = "snapshot_diff",
        description = "Unified diff between two snapshots (from_ts must be strictly less than to_ts). Both snapshots are rendered as Markdown and compared with the `similar` crate. Response is a JSON object with 'from' / 'to' metadata (timestamp / label / iso) and a unified-diff 'diff' string; the diff header uses the label when present, otherwise the timestamp.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn snapshot_diff(
        &self,
        Parameters(req): Parameters<McpSnapshotDiffRequest>,
    ) -> Result<CallToolResult, McpError> {
        let from_ms: i64 = req.from_ts.parse().map_err(|_| {
            McpError::invalid_params(
                format!("Invalid from_ts: '{}'. Must be a millis integer.", req.from_ts),
                None,
            )
        })?;
        let to_ms: i64 = req.to_ts.parse().map_err(|_| {
            McpError::invalid_params(
                format!("Invalid to_ts: '{}'. Must be a millis integer.", req.to_ts),
                None,
            )
        })?;
        if from_ms >= to_ms {
            return Err(McpError::invalid_params(
                format!(
                    "from_ts ({from_ms}) must be strictly less than to_ts ({to_ms})"
                ),
                None,
            ));
        }
        let context_lines = req.context_lines.unwrap_or(3);

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

        // 両 snapshot の meta (label) を取りに行くために list を舐める
        let infos = SnapshotService::list(&self.shelf_dir, &slug).map_err(|e| {
            McpError::internal_error(format!("Failed to list snapshots: {e}"), None)
        })?;
        let find_label = |ms: i64| -> Option<String> {
            infos
                .iter()
                .find(|i| i.timestamp.as_millis() == ms)
                .and_then(|i| i.label.clone())
        };

        let from_book = SnapshotService::restore(&self.shelf_dir, &slug, from_ms).map_err(|e| {
            McpError::internal_error(format!("Failed to load from snapshot: {e}"), None)
        })?;
        let to_book = SnapshotService::restore(&self.shelf_dir, &slug, to_ms).map_err(|e| {
            McpError::internal_error(format!("Failed to load to snapshot: {e}"), None)
        })?;

        let from_md = EjectService::render_markdown(&from_book, true, None);
        let to_md = EjectService::render_markdown(&to_book, true, None);

        let from_ts = Timestamp::from_millis(from_ms);
        let to_ts = Timestamp::from_millis(to_ms);
        let from_label = find_label(from_ms);
        let to_label = find_label(to_ms);
        let from_header = diff_header_name(from_label.as_deref(), from_ms);
        let to_header = diff_header_name(to_label.as_deref(), to_ms);

        let diff = similar::TextDiff::from_lines(&from_md, &to_md);
        let mut diff_out = String::new();
        diff_out.push_str(&format!(
            "--- {} ({} / {})\n",
            from_header,
            from_ms,
            from_ts.to_iso8601()
        ));
        diff_out.push_str(&format!(
            "+++ {} ({} / {})\n",
            to_header,
            to_ms,
            to_ts.to_iso8601()
        ));
        for hunk in diff.unified_diff().context_radius(context_lines).iter_hunks() {
            diff_out.push_str(&hunk.to_string());
        }

        let response = serde_json::json!({
            "from": {
                "timestamp": from_ms,
                "label": from_label,
                "iso": from_ts.to_iso8601(),
            },
            "to": {
                "timestamp": to_ms,
                "label": to_label,
                "iso": to_ts.to_iso8601(),
            },
            "diff": diff_out,
        });

        let text = serde_json::to_string_pretty(&response).map_err(|e| {
            McpError::internal_error(format!("Failed to serialize diff response: {e}"), None)
        })?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            text,
        )]))
    }

    #[tool(
        name = "snapshot_dump",
        description = "Dump a single snapshot to a subdirectory as 'book.md' (or 'book.json'). The live book on the shelf is NOT touched. After running, use `Bash(diff -u <dir1>/book.md <dir2>/book.md)` for unified diff between snapshots.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn snapshot_dump(
        &self,
        Parameters(req): Parameters<McpSnapshotDumpRequest>,
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

        let format = parse_dump_format(req.format.as_deref())?;
        let overwrite = req.overwrite.unwrap_or(false);

        // list を舐めて index を確定 (01 = 最古)
        let mut infos = SnapshotService::list(&self.shelf_dir, &slug).map_err(|e| {
            McpError::internal_error(format!("Failed to list snapshots: {e}"), None)
        })?;
        infos.reverse(); // 昇順 (最古が先頭)
        let total = infos.len();
        let (idx, info) = infos
            .iter()
            .enumerate()
            .find(|(_, i)| i.timestamp.as_millis() == millis)
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("Snapshot not found for timestamp {millis}"),
                    None,
                )
            })?;

        let book = SnapshotService::restore(&self.shelf_dir, &slug, millis).map_err(|e| {
            McpError::internal_error(format!("Failed to load snapshot: {e}"), None)
        })?;

        let root = PathBuf::from(&req.output_dir);
        let subdir = subdir_name(idx + 1, total, info.timestamp.as_millis());
        let out_dir = root.join(&subdir);
        prepare_dump_dir(&out_dir, overwrite)?;

        let filename = dump_filename(&format);
        let config = EjectConfig {
            output_dir: out_dir.clone(),
            filename: filename.to_string(),
            include_placeholders: true,
            format,
            subtree_root: None,
        };
        let path = EjectService::eject(&book, &config).map_err(Self::to_mcp_error)?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!("Snapshot dumped to: {}", path.display()),
        )]))
    }

    #[tool(
        name = "snapshot_dump_all",
        description = "Dump every snapshot for the selected book into 'vNN_<millis>' subdirectories (01 = oldest). Each subdir contains 'book.md' (or 'book.json'). The live book on the shelf is NOT touched. After running, use `Bash(diff -u <dir>/v03_*/book.md <dir>/v04_*/book.md)` for unified diff between consecutive snapshots.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn snapshot_dump_all(
        &self,
        Parameters(req): Parameters<McpSnapshotDumpAllRequest>,
    ) -> Result<CallToolResult, McpError> {
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

        let format = parse_dump_format(req.format.as_deref())?;
        let overwrite = req.overwrite.unwrap_or(false);

        let mut infos = SnapshotService::list(&self.shelf_dir, &slug).map_err(|e| {
            McpError::internal_error(format!("Failed to list snapshots: {e}"), None)
        })?;
        if infos.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                "No snapshots to dump.".to_string(),
            )]));
        }
        infos.reverse(); // 01 = 最古

        let root = PathBuf::from(&req.output_dir);
        let total = infos.len();
        let filename = dump_filename(&format);
        let mut written: Vec<String> = Vec::with_capacity(total);

        for (idx, info) in infos.iter().enumerate() {
            let millis = info.timestamp.as_millis();
            let book = SnapshotService::restore(&self.shelf_dir, &slug, millis).map_err(|e| {
                McpError::internal_error(
                    format!("Failed to load snapshot {millis}: {e}"),
                    None,
                )
            })?;

            let subdir = subdir_name(idx + 1, total, millis);
            let out_dir = root.join(&subdir);
            prepare_dump_dir(&out_dir, overwrite)?;

            let config = EjectConfig {
                output_dir: out_dir.clone(),
                filename: filename.to_string(),
                include_placeholders: true,
                format: format.clone(),
                subtree_root: None,
            };
            let path = EjectService::eject(&book, &config).map_err(Self::to_mcp_error)?;
            written.push(path.display().to_string());
        }

        let mut output = format!(
            "Dumped {} snapshots to: {}\n\n",
            written.len(),
            root.display()
        );
        for line in &written {
            output.push_str(&format!("- {line}\n"));
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
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
        entries.sort_by_key(|a| a.timestamp);

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
        name = "book_history",
        description = "List the full edit history of the selected book (all nodes, chronological). Each entry shows action / node hierarchical id / title. Snapshot labels are cross-referenced when the entry's timestamp matches a snapshot. Use `since` / `until` (millis) to bound a range — e.g. pair with two snapshot timestamps from `snapshot_list` to see what changed between them.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn book_history(
        &self,
        Parameters(req): Parameters<McpBookHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
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

        let since = parse_optional_millis(req.since.as_deref(), "since")?;
        let until = parse_optional_millis(req.until.as_deref(), "until")?;
        if let (Some(s), Some(u)) = (since, until) {
            if s > u {
                return Err(McpError::invalid_params(
                    format!("since ({s}) must be <= until ({u})"),
                    None,
                ));
            }
        }
        let limit = req.limit.unwrap_or(50);

        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

        let cl_repo = JsonChangeLogRepository::new(&self.shelf_dir, &slug);
        let mut entries = crate::domain::repository::ChangeLogRepository::load_all(&cl_repo)
            .map_err(|e| {
                McpError::internal_error(format!("Failed to load history: {e}"), None)
            })?;

        // 時系列降順 (最新が先頭) — 後で range 絞り + limit
        entries.sort_by_key(|e| std::cmp::Reverse(e.timestamp));

        if let Some(s) = since {
            entries.retain(|e| e.timestamp.as_millis() >= s);
        }
        if let Some(u) = until {
            entries.retain(|e| e.timestamp.as_millis() <= u);
        }

        let total_in_range = entries.len();
        if limit > 0 && entries.len() > limit {
            entries.truncate(limit);
        }

        // snapshot label cross-ref
        let snap_infos = SnapshotService::list(&self.shelf_dir, &slug).unwrap_or_default();
        let snap_label_by_millis: HashMap<i64, String> = snap_infos
            .iter()
            .filter_map(|i| {
                i.label
                    .as_ref()
                    .map(|l| (i.timestamp.as_millis(), l.clone()))
            })
            .collect();

        if entries.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                format!("No history for \"{}\" in the requested range.", book.title()),
            )]));
        }

        let showing_note = if limit > 0 && total_in_range > limit {
            format!(
                " (showing newest {} of {} in range)",
                entries.len(),
                total_in_range
            )
        } else {
            format!(" ({} entries)", entries.len())
        };

        let mut output = format!(
            "# History for \"{}\"{}\n\n",
            book.title(),
            showing_note
        );

        for (i, entry) in entries.iter().enumerate() {
            let action_str = match entry.action {
                ChangeAction::Create => "create",
                ChangeAction::Update => "update",
                ChangeAction::Delete => "delete",
                ChangeAction::Move => "move",
                ChangeAction::Restore => "restore",
            };
            let hier = find_hierarchical_id(&book, entry.node_id)
                .unwrap_or_else(|| entry.node_id.short().to_string());
            let title = book
                .get_node(entry.node_id)
                .map(|n| n.title().to_string())
                .unwrap_or_else(|| "(deleted)".to_string());
            let label_note = snap_label_by_millis
                .get(&entry.timestamp.as_millis())
                .map(|l| format!(" [snapshot: {l}]"))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}. [{}] {} node={} title=\"{}\"{}\n",
                i + 1,
                entry.timestamp.to_iso8601(),
                action_str,
                hier,
                title,
                label_note
            ));
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

    /// UUID文字列をNodeIdに解決する。フルUUIDまたは短縮プレフィックスを受け付ける。
    /// 階層番号やタイトル一致は受け付けない（バッチ操作のtoc IDズレ問題回避）。
    fn resolve_uuid(&self, s: &str) -> Result<crate::domain::model::id::NodeId, McpError> {
        // 1. Full UUID
        if let Ok(id) = parse_node_id(s) {
            return Ok(id);
        }
        // 2. Short prefix in current book
        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;
        let matches: Vec<crate::domain::model::id::NodeId> = book
            .all_node_ids()
            .filter(|id| id.to_string().starts_with(s))
            .collect();
        match matches.len() {
            1 => Ok(matches[0]),
            0 => Err(McpError::invalid_params(
                format!("No node with UUID starting with '{s}'"),
                None,
            )),
            n => Err(McpError::invalid_params(
                format!("Ambiguous UUID prefix '{s}' matches {n} nodes"),
                None,
            )),
        }
    }

    #[tool(
        name = "node_batch_move",
        description = "Move multiple nodes in a single atomic operation. All nodes must be specified by UUID (not toc ID). Use `node_query` or `dump` to find UUIDs. All moves succeed or none are saved.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn node_batch_move(
        &self,
        Parameters(req): Parameters<McpBatchMoveRequest>,
    ) -> Result<CallToolResult, McpError> {
        let total = req.moves.len();

        // Resolve all UUIDs first, reporting the first failure with its index
        let mut resolved: Vec<(
            crate::domain::model::id::NodeId,
            Option<crate::domain::model::id::NodeId>,
            usize,
        )> = Vec::with_capacity(total);

        for (i, item) in req.moves.iter().enumerate() {
            let id = self.resolve_uuid(&item.node_id).map_err(|e| {
                McpError::invalid_params(
                    format!(
                        "Batch move failed at operation {}/{total} (node {}): {e}. No changes saved.",
                        i + 1,
                        item.node_id
                    ),
                    None,
                )
            })?;
            let new_parent = item
                .new_parent
                .as_deref()
                .map(|s| self.resolve_uuid(s))
                .transpose()
                .map_err(|e| {
                    McpError::invalid_params(
                        format!(
                            "Batch move failed at operation {}/{total} (node {}): parent UUID: {e}. No changes saved.",
                            i + 1,
                            item.node_id
                        ),
                        None,
                    )
                })?;
            let position = item.position.unwrap_or(usize::MAX);
            resolved.push((id, new_parent, position));
        }

        let svc = self.service()?;
        let (count, warnings) = svc.batch_move(resolved).map_err(|e| {
            McpError::internal_error(format!("Batch move failed: {e}. No changes saved."), None)
        })?;

        let mut msg = format!("Batch move complete: {count}/{total} operations succeeded.");
        for w in warnings.into_iter().flatten() {
            msg.push_str(&format!("\n[WARNING] {w}"));
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "node_batch_update",
        description = "Update multiple nodes' properties, status, title, or body in a single atomic operation. All nodes must be specified by UUID.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn node_batch_update(
        &self,
        Parameters(req): Parameters<McpBatchUpdateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let total = req.updates.len();

        // Resolve all UUIDs first
        let mut resolved: Vec<(
            crate::domain::model::id::NodeId,
            crate::domain::model::book::UpdateNodeRequest,
        )> = Vec::with_capacity(total);

        for (i, item) in req.updates.iter().enumerate() {
            let id = self.resolve_uuid(&item.node_id).map_err(|e| {
                McpError::invalid_params(
                    format!(
                        "Batch update failed at operation {}/{total} (node {}): {e}. No changes saved.",
                        i + 1,
                        item.node_id
                    ),
                    None,
                )
            })?;
            let status = item
                .status
                .as_deref()
                .map(parse_node_status)
                .transpose()
                .map_err(|e| {
                    McpError::invalid_params(
                        format!(
                            "Batch update failed at operation {}/{total} (node {}): {e}. No changes saved.",
                            i + 1,
                            item.node_id
                        ),
                        None,
                    )
                })?;
            let update_req = crate::domain::model::book::UpdateNodeRequest {
                title: item.title.as_deref().map(unescape_newlines),
                body: item.body.clone().map(|b| b.map(|s| unescape_newlines(&s))),
                node_type: None,
                placeholder: None,
                properties: item.properties.clone(),
                status,
            };
            resolved.push((id, update_req));
        }

        let svc = self.service()?;
        let (count, warnings) = svc.batch_update(resolved).map_err(|e| {
            McpError::internal_error(format!("Batch update failed: {e}. No changes saved."), None)
        })?;

        let mut msg = format!("Batch update complete: {count}/{total} operations succeeded.");
        for w in warnings.into_iter().flatten() {
            msg.push_str(&format!("\n[WARNING] {w}"));
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        description = "Query nodes by properties, status, type, or subtree. Returns UUIDs needed for batch operations. Use `include_body: true` to include node content.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn node_query(
        &self,
        Parameters(req): Parameters<McpNodeQueryRequest>,
    ) -> Result<CallToolResult, McpError> {
        use crate::domain::model::node::NodeType;

        let svc = self.service()?;
        let book = svc.read_tree().map_err(Self::to_mcp_error)?;

        let root_id = req
            .subtree_root
            .as_deref()
            .map(|s| self.resolve_id(s))
            .transpose()?;

        let mut nodes = match root_id {
            Some(id) => book.subtree_nodes(id),
            None => book.all_nodes_dfs(),
        };

        if let Some(ref filter) = req.filter {
            if !filter.is_empty() {
                nodes.retain(|node| {
                    filter
                        .iter()
                        .all(|(k, v)| node.get_property(k).map(|pv| pv == v).unwrap_or(false))
                });
            }
        }

        if let Some(ref k) = req.kind {
            let nt = parse_node_type(k)?;
            nodes.retain(|n| n.node_type() == &nt);
        }

        if let Some(ref s) = req.status {
            let st = parse_node_status(s)?;
            nodes.retain(|n| n.status() == st);
        }

        if nodes.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                "No matching nodes found.",
            )]));
        }

        let mut output = format!("# Query Results ({} matches)\n", nodes.len());
        for (i, node) in nodes.iter().enumerate() {
            let short = node.id().short();
            let full = node.id().to_string();
            let type_str = match node.node_type() {
                NodeType::Section => "section",
                NodeType::Content => "content",
            };
            let status_str = match node.status() {
                crate::domain::model::changelog::NodeStatus::Active => "active",
                crate::domain::model::changelog::NodeStatus::Draft => "draft",
            };
            output.push_str(&format!(
                "\n{}. [{}] {}\n   UUID: {}\n   Type: {}\n   Status: {}\n",
                i + 1,
                short,
                node.title(),
                full,
                type_str,
                status_str,
            ));
            let props = node.properties();
            if !props.is_empty() {
                let props_str = props
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!("   Properties: {}\n", props_str));
            }
            if req.include_body {
                if let Some(body) = node.body() {
                    output.push_str(&format!("   Body: {}\n", body));
                }
            }
            output.push_str("   ---\n");
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
    }
}

// ---------------------------------------------------------------------------
// snapshot label / dump helpers
// ---------------------------------------------------------------------------

/// snapshot label の validation。
///
/// - 空 / 前後空白のみは拒否
/// - 64 文字上限
/// - 許可文字: 英数字 / space / `-_.:,()`  (path traversal 防止 & filename 化しないので厳格すぎない範囲)
/// - trim した form を返す
fn validate_snapshot_label(raw: &str) -> Result<String, McpError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(McpError::invalid_params(
            "label must not be empty or whitespace only",
            None,
        ));
    }
    if trimmed.chars().count() > 64 {
        return Err(McpError::invalid_params(
            "label must be 64 characters or fewer",
            None,
        ));
    }
    let ok = trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | ':' | ',' | '(' | ')'));
    if !ok {
        return Err(McpError::invalid_params(
            "label may only contain letters, digits, spaces, and '-_.:,()'",
            None,
        ));
    }
    Ok(trimmed.to_string())
}


/// optional millis 文字列を i64 に parse する。None は None を返す。
fn parse_optional_millis(s: Option<&str>, field: &str) -> Result<Option<i64>, McpError> {
    match s {
        None => Ok(None),
        Some(v) => v.parse::<i64>().map(Some).map_err(|_| {
            McpError::invalid_params(
                format!("Invalid {field}: '{v}'. Must be a millis integer."),
                None,
            )
        }),
    }
}

/// diff header の名前部分を決める。label があれば label、なければ timestamp 文字列。
fn diff_header_name(label: Option<&str>, millis: i64) -> String {
    match label {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => millis.to_string(),
    }
}

fn parse_dump_format(s: Option<&str>) -> Result<EjectFormat, McpError> {
    match s {
        Some("json") => Ok(EjectFormat::Json),
        Some("markdown") | None => Ok(EjectFormat::Markdown),
        Some(other) => Err(McpError::invalid_params(
            format!("Unknown format: '{other}'. Use: markdown, json"),
            None,
        )),
    }
}

fn dump_filename(format: &EjectFormat) -> &'static str {
    match format {
        EjectFormat::Markdown => "book.md",
        EjectFormat::Json => "book.json",
    }
}

fn subdir_name(index: usize, total: usize, millis: i64) -> String {
    let width = if total >= 100 { 3 } else { 2 };
    format!("v{:0width$}_{}", index, millis, width = width)
}

fn prepare_dump_dir(dir: &std::path::Path, overwrite: bool) -> Result<(), McpError> {
    if dir.exists() {
        if !overwrite {
            return Err(McpError::invalid_params(
                format!(
                    "Output subdir already exists: {}. Pass overwrite=true to allow.",
                    dir.display()
                ),
                None,
            ));
        }
        std::fs::remove_dir_all(dir).map_err(|e| {
            McpError::internal_error(
                format!("Failed to remove existing dir {}: {}", dir.display(), e),
                None,
            )
        })?;
    }
    std::fs::create_dir_all(dir).map_err(|e| {
        McpError::internal_error(
            format!("Failed to create dir {}: {}", dir.display(), e),
            None,
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod dump_helpers_tests {
    use super::*;

    #[test]
    fn subdir_name_pads_two_digits() {
        assert_eq!(subdir_name(1, 5, 12345), "v01_12345");
        assert_eq!(subdir_name(9, 12, 999), "v09_999");
        assert_eq!(subdir_name(12, 12, 100), "v12_100");
    }

    #[test]
    fn subdir_name_uses_three_digits_when_needed() {
        assert_eq!(subdir_name(1, 150, 7), "v001_7");
        assert_eq!(subdir_name(150, 150, 7), "v150_7");
    }

    #[test]
    fn parse_dump_format_default_markdown() {
        assert!(matches!(
            parse_dump_format(None).unwrap(),
            EjectFormat::Markdown
        ));
        assert!(matches!(
            parse_dump_format(Some("markdown")).unwrap(),
            EjectFormat::Markdown
        ));
        assert!(matches!(
            parse_dump_format(Some("json")).unwrap(),
            EjectFormat::Json
        ));
    }

    #[test]
    fn parse_dump_format_rejects_unknown() {
        assert!(parse_dump_format(Some("yaml")).is_err());
    }

    #[test]
    fn dump_filename_by_format() {
        assert_eq!(dump_filename(&EjectFormat::Markdown), "book.md");
        assert_eq!(dump_filename(&EjectFormat::Json), "book.json");
    }

    #[test]
    fn prepare_dump_dir_creates_new() {
        let dir = std::env::temp_dir().join("outline-mcp-dump-helper-new");
        let _ = std::fs::remove_dir_all(&dir);
        prepare_dump_dir(&dir, false).expect("create");
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_dump_dir_errors_when_exists_without_overwrite() {
        let dir = std::env::temp_dir().join("outline-mcp-dump-helper-existing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let res = prepare_dump_dir(&dir, false);
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_optional_millis_handles_none_some_bad() {
        assert!(matches!(parse_optional_millis(None, "since"), Ok(None)));
        assert!(matches!(
            parse_optional_millis(Some("1782885148593"), "since"),
            Ok(Some(1782885148593))
        ));
        assert!(parse_optional_millis(Some("abc"), "since").is_err());
    }

    #[test]
    fn diff_header_prefers_label() {
        assert_eq!(diff_header_name(Some("v03_rating"), 12345), "v03_rating");
    }

    #[test]
    fn diff_header_falls_back_to_millis_when_label_absent() {
        assert_eq!(diff_header_name(None, 12345), "12345");
    }

    #[test]
    fn diff_header_falls_back_to_millis_when_label_empty() {
        assert_eq!(diff_header_name(Some(""), 999), "999");
    }

    #[test]
    fn validate_label_accepts_normal_input() {
        assert_eq!(
            validate_snapshot_label(" rating-pass ").unwrap(),
            "rating-pass"
        );
        assert_eq!(
            validate_snapshot_label("v03 L3 sketch").unwrap(),
            "v03 L3 sketch"
        );
        assert_eq!(
            validate_snapshot_label("Milestone (1.0): draft").unwrap(),
            "Milestone (1.0): draft"
        );
    }

    #[test]
    fn validate_label_rejects_empty() {
        assert!(validate_snapshot_label("").is_err());
        assert!(validate_snapshot_label("   ").is_err());
    }

    #[test]
    fn validate_label_rejects_too_long() {
        let long: String = "a".repeat(65);
        assert!(validate_snapshot_label(&long).is_err());
        let ok: String = "a".repeat(64);
        assert!(validate_snapshot_label(&ok).is_ok());
    }

    #[test]
    fn validate_label_rejects_disallowed_chars() {
        assert!(validate_snapshot_label("bad/slash").is_err());
        assert!(validate_snapshot_label("back\\slash").is_err());
        assert!(validate_snapshot_label("nl\nne").is_err());
    }

    #[test]
    fn prepare_dump_dir_overwrites_when_flag_set() {
        let dir = std::env::temp_dir().join("outline-mcp-dump-helper-overwrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale.txt"), "old").unwrap();
        prepare_dump_dir(&dir, true).expect("overwrite");
        assert!(dir.exists());
        assert!(!dir.join("stale.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
