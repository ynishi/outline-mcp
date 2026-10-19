use std::collections::HashMap;
use std::path::PathBuf;

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use outline_mcp_core::application::eject::{EjectConfig, EjectFormat, EjectService, EjectTree};

use crate::helpers::{
    archived_hidden_count, archived_hidden_line, build_hierarchical_ids, describe_book_update,
    describe_descendants, describe_endpoint, describe_subtree_change, describe_subtrees_change,
    dropped_links_line, endpoint_status, find_hierarchical_id, format_toc, format_toc_limited,
    id_suffix, links_line,
};
use crate::request::{
    normalize_text, parse_node_id, parse_node_status, parse_node_type, sanitize_for_filename,
    unescape_newlines, validate_filename, validate_import_path, validate_slug, McpBatchMoveRequest,
    McpBatchUpdateRequest, McpBookDeleteRequest, McpBookHistoryRequest, McpBookLintRequest,
    McpBookStatsRequest, McpBookUpdateRequest, McpDumpRequest, McpEjectRequest,
    McpGenRoutingRequest, McpImportRequest, McpInitRequest, McpLinkAddRequest,
    McpLinkRemoveRequest, McpLinksRequest, McpNodeCreateRequest, McpNodeHistoryRequest,
    McpNodeMoveRequest, McpNodeQueryRequest, McpNodeUpdateRequest, McpNodeVerifyRequest,
    McpRevisionApproveRequest, McpRevisionProposeRequest, McpRevisionRejectRequest,
    McpRevisionShowRequest, McpRevisionsPendingRequest, McpSelectBookRequest, McpShelfRequest,
    McpSnapshotCreateRequest, McpSnapshotDeleteRequest, McpSnapshotDiffRequest,
    McpSnapshotDumpAllRequest, McpSnapshotDumpRequest, McpSnapshotListRequest,
    McpSnapshotRestoreRequest, McpSnapshotTagRequest, McpTocRequest,
};
use crate::revision::{
    age as revision_age, fields_label, proposal_json, proposal_line, revision_diff,
};
use crate::server::OutlineMcpServer;

use outline_mcp_core::domain::model::book::AddNodeRequest;
use outline_mcp_core::domain::model::book::UpdateNodeRequest;
use outline_mcp_core::domain::model::changelog::{ChangeAction, NodeStatus};
use outline_mcp_core::domain::model::node::{normalize_rel, BodyChange, Revision};
use outline_mcp_core::domain::model::timestamp::Timestamp;
use outline_mcp_core::domain::search::TextQuery;

/// Length of the excerpt `node_query` shows around a `text` match.
const SNIPPET_CHARS: usize = 80;

#[tool_router(vis = "pub(crate)")]
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
        let svc = self.service().await?;
        let node_type = parse_node_type(&req.node_type)?;
        let parent = match req.parent.as_deref() {
            Some(s) => Some(self.resolve_id(s).await?),
            None => None,
        };

        let add_req = AddNodeRequest {
            parent,
            title: unescape_newlines(&req.title),
            node_type,
            body: normalize_text(req.body),
            placeholder: normalize_text(req.placeholder),
            position: req.position.unwrap_or(usize::MAX),
            properties: req.properties.unwrap_or_default(),
        };

        let (id, warning) = svc.add_node(add_req).await.map_err(Self::to_mcp_error)?;

        // 階層番号を逆引き
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let hier = find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());

        let mut msg = format!(
            "Created: {}. {}{}",
            hier,
            book.get_node(id).map(|n| n.title()).unwrap_or("?"),
            id_suffix(id)
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
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;
        let node_type = req.node_type.as_deref().map(parse_node_type).transpose()?;

        let status = req.status.as_deref().map(parse_node_status).transpose()?;

        let update_req = UpdateNodeRequest {
            title: req.title.map(|t| unescape_newlines(&t)),
            body: req.body.map(normalize_text),
            node_type,
            placeholder: req.placeholder.map(normalize_text),
            properties: req.properties,
            properties_patch: req.properties_patch,
            status,
        };

        let ((), warning) = svc
            .update_node(id, update_req)
            .await
            .map_err(Self::to_mcp_error)?;

        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
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
        name = "node_verify",
        description = "Record that someone confirmed these nodes still match reality: sets their verification time to now (and `by`, if given). Content and last-edited time are unchanged; each node gets one entry in history, all in one commit. Find nodes nobody has confirmed lately with `node_query` `verified_before`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn node_verify(
        &self,
        Parameters(req): Parameters<McpNodeVerifyRequest>,
    ) -> Result<CallToolResult, McpError> {
        if req.node_ids.is_empty() {
            return Err(McpError::invalid_params(
                "node_ids must name at least one node",
                None,
            ));
        }
        let svc = self.service().await?;
        let mut ids = Vec::with_capacity(req.node_ids.len());
        for s in &req.node_ids {
            let id = self.resolve_id(s).await?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let by = req
            .by
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty());
        let at = svc
            .verify_nodes(&ids, by.clone())
            .await
            .map_err(Self::to_mcp_error)?;

        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let mut msg = format!(
            "Verified {} node(s) at {}{}:",
            ids.len(),
            at.to_iso8601(),
            by.as_deref()
                .map(|b| format!(" by {b}"))
                .unwrap_or_default()
        );
        for id in &ids {
            let hier = find_hierarchical_id(&book, *id).unwrap_or_else(|| id.short().to_string());
            let title = book.get_node(*id).map(|n| n.title()).unwrap_or("?");
            msg.push_str(&format!("\n  {hier}. {title}{}", id_suffix(*id)));
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "link_add",
        description = "Add a typed link from one node to another node of the selected book (e.g. rel 'supersedes', 'superseded_by', 'see_also', 'merged_from'; any 1-40 of a-z 0-9 _ -). Links follow the target's id, so moving the target keeps them; removing a node drops the links pointing at it. Recorded in history as an update of the source node. Adding an existing link changes nothing. List links with `links`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn link_add(
        &self,
        Parameters(req): Parameters<McpLinkAddRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service().await?;
        let from = self.resolve_id(&req.from).await?;
        let to = self.resolve_id(&req.to).await?;
        let added = svc
            .add_link(from, to, &req.rel)
            .await
            .map_err(Self::to_mcp_error)?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let rel = normalize_rel(&req.rel).unwrap_or_else(|| req.rel.clone());
        let msg = if added {
            format!(
                "Linked: {}\n  —{rel}→ {}",
                describe_endpoint(&book, from),
                describe_endpoint(&book, to)
            )
        } else {
            format!(
                "Already linked (nothing changed): {}\n  —{rel}→ {}",
                describe_endpoint(&book, from),
                describe_endpoint(&book, to)
            )
        };
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "link_remove",
        description = "Remove the links from one node to another (only those of type `rel` when given, every type otherwise). Recorded in history as an update of the source node. A target that no longer exists can be named by its full UUID (see `book_lint` dangling_links).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn link_remove(
        &self,
        Parameters(req): Parameters<McpLinkRemoveRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service().await?;
        let from = self.resolve_id(&req.from).await?;
        let to = self.resolve_id(&req.to).await?;
        let removed = svc
            .remove_link(from, to, req.rel.as_deref())
            .await
            .map_err(Self::to_mcp_error)?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let what = match req.rel.as_deref() {
            Some(r) => format!(
                "{} link(s)",
                normalize_rel(r).unwrap_or_else(|| r.to_string())
            ),
            None => "link(s)".to_string(),
        };
        let msg = if removed == 0 {
            format!(
                "No {what} from {} to {} (nothing changed).",
                describe_endpoint(&book, from),
                describe_endpoint(&book, to)
            )
        } else {
            format!(
                "Removed {removed} {what}: {}\n  → {}",
                describe_endpoint(&book, from),
                describe_endpoint(&book, to)
            )
        };
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "links",
        description = "List a node's links: outgoing (`direction: \"out\"`), incoming backlinks (`\"in\"`), or both (default). Optional `rel` filters by type. Each line gives the toc ID, title, [archived] mark and UUID of the other end; a target no longer in the book is shown as such.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn links(
        &self,
        Parameters(req): Parameters<McpLinksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (show_out, show_in) = match req.direction.as_deref().unwrap_or("both") {
            "out" => (true, false),
            "in" => (false, true),
            "both" => (true, true),
            other => {
                return Err(McpError::invalid_params(
                    format!("direction must be 'out', 'in' or 'both', not {other:?}"),
                    None,
                ))
            }
        };
        let rel = match req.rel.as_deref() {
            Some(r) => Some(normalize_rel(r).ok_or_else(|| {
                McpError::invalid_params(format!("invalid link type {r:?}"), None)
            })?),
            None => None,
        };
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let node = book
            .get_node(id)
            .ok_or_else(|| McpError::invalid_params(format!("No node {id} in this book."), None))?;
        let keep = |r: &str| rel.as_deref().is_none_or(|want| want == r);

        let mut msg = format!("# Links of {}\n", describe_endpoint(&book, id));
        if show_out {
            let out: Vec<_> = node.links().iter().filter(|l| keep(l.rel())).collect();
            msg.push_str(&format!("\n## Outgoing ({})\n", out.len()));
            for l in out {
                msg.push_str(&format!(
                    "- {} → {}\n",
                    l.rel(),
                    describe_endpoint(&book, l.target())
                ));
            }
        }
        if show_in {
            let incoming: Vec<_> = book
                .backlinks(id)
                .into_iter()
                .filter(|(_, l)| keep(l.rel()))
                .collect();
            msg.push_str(&format!("\n## Incoming ({})\n", incoming.len()));
            for (src, l) in incoming {
                msg.push_str(&format!(
                    "- {} ← {}\n",
                    l.rel(),
                    describe_endpoint(&book, src)
                ));
            }
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "revision_propose",
        description = "Propose a change to a node's title, body and/or properties for someone to approve, without changing the node. Until `revision_approve`, injection, toc, dump and every read keep showing the current content. One pending revision per node (`replace: true` swaps it). Not for archived nodes. Recorded in history; last-edited time is unchanged. Review with `revision_show`, list with `revisions_pending`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn revision_propose(
        &self,
        Parameters(req): Parameters<McpRevisionProposeRequest>,
    ) -> Result<CallToolResult, McpError> {
        if req.body.is_some() && req.clear_body {
            return Err(McpError::invalid_params(
                "`body` and `clear_body` are mutually exclusive",
                None,
            ));
        }
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;
        // Clearing goes only through `clear_body`; `body: ""` proposes an empty
        // body, which counts as no change for a node without one (an absent
        // and an empty body compare equal, see `content_fingerprint`).
        let body = if req.clear_body {
            Some(BodyChange::Clear)
        } else {
            req.body.map(|b| BodyChange::Set(unescape_newlines(&b)))
        };
        let revision = Revision {
            title: req.title.map(|t| unescape_newlines(&t)),
            body,
            properties: req.properties,
            properties_patch: req.properties_patch,
            base: String::new(),
            by: trimmed(req.by),
            note: trimmed(req.note),
            at: Timestamp::now(),
        };
        let stored = svc
            .propose_revision(id, revision, req.replace)
            .await
            .map_err(Self::to_mcp_error)?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let msg = format!(
            "Proposed a revision of {} ({}){}. The node is unchanged until `revision_approve`; review with `revision_show`.",
            describe_endpoint(&book, id),
            fields_label(&stored),
            stored
                .by
                .as_deref()
                .map(|b| format!(" by {b}"))
                .unwrap_or_default()
        );
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "revision_show",
        description = "Show a node's pending revision: who proposed it, when, the note, and a diff against the current content (title old → new, unified diff of the body, properties added / changed / removed). Warns when the node changed after the revision was proposed.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn revision_show(
        &self,
        Parameters(req): Parameters<McpRevisionShowRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let node = book
            .get_node(id)
            .ok_or_else(|| McpError::invalid_params(format!("No node {id} in this book."), None))?;
        let Some(rev) = node.proposal() else {
            return Err(McpError::invalid_params(
                format!("{} has no pending revision.", describe_endpoint(&book, id)),
                None,
            ));
        };
        let mut msg = format!(
            "# Pending revision of {}\nProposed{} at {} ({})\n",
            describe_endpoint(&book, id),
            rev.by
                .as_deref()
                .map(|b| format!(" by {b}"))
                .unwrap_or_default(),
            rev.at.to_iso8601(),
            revision_age(Timestamp::now(), rev.at)
        );
        if let Some(note) = &rev.note {
            msg.push_str(&format!("Note: {note}\n"));
        }
        if rev.is_stale(node) {
            msg.push_str(
                "WARNING: the node changed after this revision was proposed; the diff below is against the current content. Approving needs `force: true`.\n",
            );
        }
        msg.push('\n');
        msg.push_str(&revision_diff(node, rev));
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "revision_approve",
        description = "Apply a node's pending revision and clear it, in one commit recorded as one update in history (validated like `node_update`). Refused when the node changed after the revision was proposed unless `force: true` — check `revision_show` first.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn revision_approve(
        &self,
        Parameters(req): Parameters<McpRevisionApproveRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;
        let applied = svc
            .approve_revision(id, req.force)
            .await
            .map_err(Self::to_mcp_error)?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let msg = format!(
            "Approved{} and applied the revision of {} ({}){}.",
            trimmed(req.by)
                .map(|b| format!(" by {b}"))
                .unwrap_or_default(),
            describe_endpoint(&book, id),
            fields_label(&applied),
            applied
                .by
                .as_deref()
                .map(|b| format!(", proposed by {b}"))
                .unwrap_or_default()
        );
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "revision_reject",
        description = "Discard a node's pending revision without applying it. Recorded in history; the node's content is unchanged.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn revision_reject(
        &self,
        Parameters(req): Parameters<McpRevisionRejectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;
        let rejected = svc.reject_revision(id).await.map_err(Self::to_mcp_error)?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let msg = format!(
            "Rejected{} the revision of {} ({}){}. The node is unchanged.",
            trimmed(req.by)
                .map(|b| format!(" by {b}"))
                .unwrap_or_default(),
            describe_endpoint(&book, id),
            fields_label(&rejected),
            rejected
                .by
                .as_deref()
                .map(|b| format!(", proposed by {b}"))
                .unwrap_or_default()
        );
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "revisions_pending",
        description = "List the nodes of the selected book that have a pending revision (optionally under `subtree_root`): toc ID, title, UUID, proposer, changed fields, age, and whether the node changed since (stale).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn revisions_pending(
        &self,
        Parameters(req): Parameters<McpRevisionsPendingRequest>,
    ) -> Result<CallToolResult, McpError> {
        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let scope: Option<Vec<_>> = match req.subtree_root.as_deref() {
            Some(s) => {
                let root = self.resolve_id(s).await?;
                Some(book.subtree_nodes(root).iter().map(|n| n.id()).collect())
            }
            None => None,
        };
        let now = Timestamp::now();
        let pending: Vec<_> = book
            .nodes_with_proposals()
            .into_iter()
            .filter(|n| scope.as_ref().is_none_or(|ids| ids.contains(&n.id())))
            .collect();
        let mut msg = format!("# Pending revisions ({})\n", pending.len());
        for n in pending {
            if let Some(rev) = n.proposal() {
                msg.push_str(&format!(
                    "- {}\n  {}\n",
                    describe_endpoint(&book, n.id()),
                    proposal_line(n, rev, now)
                ));
            }
        }
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "node_move",
        description = "Move or delete a node together with all of its descendants. Specify node by ID from `toc` output (e.g. '2-3'). Action 'move' relocates, 'remove' deletes. The result names the descendants that went with it. Pass `dry_run: true` to see exactly which nodes would move or be removed (old → new positions) without changing anything. History: a remove records a delete for the node and every descendant; a move records a move for the node only (its descendants keep their parent).",
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
        let svc = self.service().await?;
        let id = self.resolve_id(&req.node_id).await?;

        match req.action.as_str() {
            "move" => {
                let new_parent = match req.new_parent.as_deref() {
                    Some(s) => Some(self.resolve_id(s).await?),
                    None => None,
                };
                let position = req.position.unwrap_or(usize::MAX);

                if req.dry_run {
                    let (before, after) = svc
                        .preview(|b| b.move_node(id, new_parent, position))
                        .await
                        .map_err(Self::to_mcp_error)?;
                    let n = 1 + before.descendant_ids(id).len();
                    let msg = format!(
                        "Dry run: would move {n} node(s):\n{}Nothing changed.",
                        describe_subtree_change(&before, &after, id)
                    );
                    return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                        msg,
                    )]));
                }

                let ((), warning) = svc
                    .move_node(id, new_parent, position)
                    .await
                    .map_err(Self::to_mcp_error)?;

                // The move is committed; a failure to read the tree back for
                // the report must not turn it into an error the caller would
                // retry.
                let mut msg = match svc.read_tree().await {
                    Ok(book) => {
                        let hier = find_hierarchical_id(&book, id)
                            .unwrap_or_else(|| id.short().to_string());
                        format!(
                            "Moved → {}. {}{}{}",
                            hier,
                            book.get_node(id).map(|n| n.title()).unwrap_or("?"),
                            describe_descendants(&book, id, "with"),
                            id_suffix(id)
                        )
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "node_move: committed, but reading the tree back failed");
                        format!(
                            "Moved{} (committed; the new position could not be read back: {e})",
                            id_suffix(id)
                        )
                    }
                };
                if let Some(w) = warning {
                    msg.push_str(&format!("\n[WARNING] {w}"));
                }
                Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                    msg,
                )]))
            }
            "remove" => {
                // 削除前に階層番号を取得
                let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
                let hier =
                    find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());
                let title = book
                    .get_node(id)
                    .map(|n| n.title().to_string())
                    .unwrap_or_default();
                let descendants = describe_descendants(&book, id, "and");
                let dropped = dropped_links_line(&book, id);

                if req.dry_run {
                    let (before, after) = svc
                        .preview(|b| b.remove_node(id))
                        .await
                        .map_err(Self::to_mcp_error)?;
                    let n = 1 + before.descendant_ids(id).len();
                    let msg = format!(
                        "Dry run: would remove {n} node(s):\n{}{}Nothing changed.",
                        describe_subtree_change(&before, &after, id),
                        dropped.replace("Dropped", "Would drop")
                    );
                    return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                        msg,
                    )]));
                }

                let ((), warning) = svc.remove_node(id).await.map_err(Self::to_mcp_error)?;
                let mut msg = format!(
                    "Removed: {}. {}{}{}",
                    hier,
                    title,
                    descendants,
                    id_suffix(id)
                );
                if !dropped.is_empty() {
                    msg.push('\n');
                    msg.push_str(dropped.trim_end());
                }
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
        description = "Show table of contents with numbered IDs (e.g. 1, 1-1, 2-3). Run this first — use the returned IDs to specify nodes in `checklist`, `node_create`, and other tools. On a large book, `max_depth: 1` shows only the root-level nodes and `limit` caps the count; a last line says how many nodes were left out.",
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
        // Validate the arguments before anything can return early (an empty
        // book or an empty filter result must not hide a bad argument).
        if req.max_depth == Some(0) {
            return Err(McpError::invalid_params(
                "max_depth must be at least 1 (1 = root-level nodes only)",
                None,
            ));
        }
        if req.limit == Some(0) {
            return Err(McpError::invalid_params("limit must be at least 1", None));
        }

        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

        let subtree_id = match req.subtree_root.as_deref() {
            Some(s) => Some(self.resolve_id(s).await?),
            None => None,
        };

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

        // Archived nodes (and everything under an archived section) are
        // hidden unless asked for, and the count of hidden ones covers only
        // what this view (max_depth / limit) would have shown. A subtree_root
        // that is itself archived (or under an archived section) was asked
        // for by name, so its subtree is shown, marked [archived].
        let root_is_archived = subtree_id.is_some_and(|id| book.is_archived_or_under_archived(id));
        let show_archived = req.include_archived || root_is_archived;
        let archived_hidden = if show_archived {
            0
        } else {
            archived_hidden_count(&book, &nodes, req.max_depth, req.limit)
        };
        if !show_archived {
            nodes.retain(|node| !book.is_archived_or_under_archived(node.id()));
        }
        let archived_line = archived_hidden_line(archived_hidden);

        if nodes.is_empty() {
            let msg = if archived_hidden > 0 {
                format!("No matching nodes.\n{archived_line}")
            } else {
                "No matching nodes. Use `node_create` to add nodes.".to_string()
            };
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                msg,
            )]));
        }

        let mut output = format_toc_limited(&book, &nodes, req.max_depth, req.limit);
        output.push_str(&archived_line);
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            output,
        )]))
    }

    #[tool(
        name = "checklist",
        description = "Export a section as a Markdown checklist with checkboxes. First run `toc` to find the section ID, then pass it as subtree_root (e.g. '2'). Omit subtree_root for full book export. Archived nodes and their subtrees are left out unless `include_archived` is true or subtree_root is itself archived. Book is NOT modified.",
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
        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

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
        let subtree_root = match req.subtree_root.as_deref() {
            Some(s) => Some(self.resolve_id(s).await?),
            None => None,
        };
        // Retired nodes are not tasks: leave archived subtrees out of the
        // export (an in-memory copy; the stored book is not touched), unless
        // asked for or the requested subtree_root is itself archived.
        let root_is_archived =
            subtree_root.is_some_and(|id| book.is_archived_or_under_archived(id));
        let book = if req.include_archived || root_is_archived {
            book
        } else {
            without_archived(book)
        };

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
        description = "Import a book from a JSON file (previously exported with `checklist` format: json). Replaces the current book entirely; the replacement is recorded in history (a delete per removed node, a create per imported node; imported nodes get new ids, and removed nodes' history is reachable via `node_history` with their full UUID).",
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
        let svc = self.service().await?;
        let import_path = validate_import_path(&req.file_path)?;
        let content = std::fs::read_to_string(&import_path)
            .map_err(|e| McpError::internal_error(format!("Failed to read file: {e}"), None))?;
        let tree: EjectTree = serde_json::from_str(&content)
            .map_err(|e| McpError::invalid_params(format!("Invalid JSON: {e}"), None))?;

        let book = EjectService::import_tree(&tree).map_err(Self::to_mcp_error)?;
        let node_count = book.node_count();
        let recorded = svc.import_book(&book).await.map_err(Self::to_mcp_error)?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Imported '{}': {} nodes ({} history entries recorded)",
                tree.title, node_count, recorded
            ),
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

        if self.book_exists(&req.slug).await? {
            return Err(McpError::invalid_params(
                format!(
                    "Book '{}' already exists. Choose a different slug.",
                    req.slug
                ),
                None,
            ));
        }

        let svc = self.service_for(&req.slug).await?;
        let max_depth = req.max_depth.unwrap_or(4);
        let book = svc
            .create_book(&req.title, max_depth)
            .await
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
        name = "book_update",
        description = "Change a book's title and / or max_depth. The slug must be given explicitly; pass at least one of `title` / `max_depth`. Lowering max_depth below the depth of an existing node is refused and names that node (move or remove it first). The change is one commit and appears in `book_history` as a `book_update` entry.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn book_update(
        &self,
        Parameters(req): Parameters<McpBookUpdateRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_slug(&req.slug)?;
        let msg = self
            .update_book(&req.slug, req.title, req.max_depth)
            .await?;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "book_delete",
        description = "Delete a book with all its nodes, history and snapshots. Cannot be undone: the book's snapshots are deleted too, so there is nothing to restore from. The slug must be given explicitly. A book that has any nodes, history or snapshots is refused unless `force: true`; `dry_run: true` reports what would be removed and changes nothing. If the book is written after its contents were checked, nothing is deleted and the call fails; check again and retry. Clears this session's selection if it pointed at the deleted book.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn book_delete(
        &self,
        Parameters(req): Parameters<McpBookDeleteRequest>,
    ) -> Result<CallToolResult, McpError> {
        validate_slug(&req.slug)?;
        let msg = self.delete_book(&req.slug, req.force, req.dry_run).await?;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
        )]))
    }

    #[tool(
        name = "book_lint",
        description = "Check a book for structural smells, read-only: content nodes at the root (root_content), pairs of nodes with similar titles (similar_titles; Jaccard over title words, character pairs for CJK; two single-token titles are paired only when identical), property keys that differ only by case / `_` / `-` / spaces (property_key_variants), sections without children (empty_sections), drafts not updated for `stale_days` (stale_drafts), links whose target is not in the book (dangling_links), and pending revisions that are older than `stale_days` or outdated (stale_proposals). Archived nodes and everything under an archived section are ignored, except that stale_proposals also reports a revision left on an archived node (it can only be rejected). Reports node toc IDs and UUIDs; changes nothing. `slug` defaults to the selected book. Use `format: \"json\"` when a program reads the result.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn book_lint(
        &self,
        Parameters(req): Parameters<McpBookLintRequest>,
    ) -> Result<CallToolResult, McpError> {
        let format = parse_query_format(req.format.as_deref())?;
        let opts = crate::health::lint_options(&req)?;
        let (slug, book) = self.report_target(req.slug.as_deref()).await?;
        let report = outline_mcp_core::domain::health::lint(&book, &opts);
        Ok(match format {
            QueryFormat::Json => {
                CallToolResult::structured(crate::health::lint_json(&book, &slug, &report))
            }
            QueryFormat::Text => CallToolResult::success(vec![rmcp::model::Content::text(
                crate::health::lint_text(&book, &slug, &report),
            )]),
        })
    }

    #[tool(
        name = "book_stats",
        description = "Measure a book, read-only: node counts (sections / content), max and mean depth, children per section (max, mean, histogram), counts by status, injected nodes (inject=true), how recently nodes were updated (≤7d / 7–30d / 30–90d / older / unknown), and distinct property keys. Changes nothing. `slug` defaults to the selected book. Use `format: \"json\"` when a program reads the result.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn book_stats(
        &self,
        Parameters(req): Parameters<McpBookStatsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let format = parse_query_format(req.format.as_deref())?;
        let (slug, book) = self.report_target(req.slug.as_deref()).await?;
        let stats = outline_mcp_core::domain::health::stats(
            &book,
            outline_mcp_core::domain::model::timestamp::Timestamp::now(),
        );
        Ok(match format {
            QueryFormat::Json => {
                CallToolResult::structured(crate::health::stats_json(&book, &slug, &stats))
            }
            QueryFormat::Text => CallToolResult::success(vec![rmcp::model::Content::text(
                crate::health::stats_text(&book, &slug, &stats),
            )]),
        })
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
        let slugs = self.list_book_slugs().await?;

        if slugs.is_empty() {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                "Shelf is empty. Use `init` to create a new book.",
            )]));
        }

        // Guard は clone した値だけ保持して即座に drop する（`.await` を跨いで
        // `RwLockReadGuard` (非 Send) を持ち越すと `#[tool]` の Send 境界を破る）。
        let selected: Option<String> = {
            let guard = self
                .selected
                .read()
                .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
            guard.clone()
        };

        let mut entries: Vec<(String, String, usize)> = Vec::new();
        for slug in &slugs {
            match self.service_for(slug).await {
                Ok(svc) => match svc.read_tree().await {
                    Ok(book) => {
                        entries.push((slug.clone(), book.title().to_string(), book.node_count()));
                    }
                    Err(_) => {
                        entries.push((slug.clone(), "(failed to load)".to_string(), 0));
                    }
                },
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
        let slug = self.resolve_book_ref(&req.book).await?;

        if !self.book_exists(&slug).await? {
            return Err(McpError::invalid_params(
                format!(
                    "Book '{}' not found in shelf. Use `shelf` to list available books.",
                    slug
                ),
                None,
            ));
        }

        let svc = self.service_for(&slug).await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

        let mut guard = self
            .selected
            .write()
            .map_err(|_| McpError::internal_error("Lock poisoned", None))?;
        *guard = Some(slug.clone());

        let toc_section = if req.quiet {
            String::new()
        } else {
            // Same view as `toc` without arguments: archived subtrees hidden,
            // with the closing line counting them.
            let all = book.all_nodes_dfs();
            let hidden = archived_hidden_count(&book, &all, None, None);
            let nodes: Vec<_> = all
                .into_iter()
                .filter(|n| !book.is_archived_or_under_archived(n.id()))
                .collect();
            if nodes.is_empty() && hidden == 0 {
                String::from("\n(empty)")
            } else {
                format!(
                    "\n\n{}{}",
                    format_toc(&book, &nodes),
                    archived_hidden_line(hidden)
                )
            }
        };

        // Context Injection: inject=true ノードの body を自動出力
        let inject_filter = {
            let mut m = HashMap::new();
            m.insert("inject".to_string(), "true".to_string());
            m
        };
        // Only active nodes outside archived sections: archiving a section
        // retires everything under it, including active inject=true children.
        let injected_nodes: Vec<_> = book
            .nodes_matching(&inject_filter)
            .into_iter()
            .filter(|node| {
                node.status() == NodeStatus::Active
                    && !book.is_archived_or_under_archived(node.id())
            })
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
        let slugs = self.list_book_slugs().await?;

        // Collect: (scene, book_slug, reference_text)
        let mut entries: Vec<(String, String, String)> = Vec::new();

        for slug in &slugs {
            let svc = match self.service_for(slug).await {
                Ok(svc) => svc,
                Err(_) => continue,
            };
            let book = match svc.read_tree().await {
                Ok(b) => b,
                Err(_) => continue,
            };

            let id_map = build_hierarchical_ids(&book);

            for node in book.all_nodes_dfs() {
                // Retired guidance should not route anyone to it.
                if book.is_archived_or_under_archived(node.id()) {
                    continue;
                }
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
        description = "Create a snapshot of the current book state. Optionally attach a label. Use `snapshot_list` to view saved snapshots and `snapshot_restore` to revert.",
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
        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

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

        let info = self
            .snapshots()
            .create(&slug, &book, label.as_deref())
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Failed to create snapshot: {e}"), None)
            })?;

        let label_note = label
            .as_deref()
            .map(|l| format!(" [label: {l}]"))
            .unwrap_or_default();

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Snapshot created: {} ({} bytes){}",
                info.timestamp.as_millis(),
                info.size_bytes,
                label_note
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
        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

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

        let infos = self.snapshots().list(&slug).await.map_err(|e| {
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

        let restored = self.load_snapshot(&slug, millis).await?;
        let node_count = restored.node_count();

        // The restored tree and one Restore event per node commit together.
        let svc = self.service_for(&slug).await?;
        svc.restore_book(&restored)
            .await
            .map_err(Self::to_mcp_error)?;

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!(
                "Restored from snapshot {}. {} nodes.",
                req.timestamp, node_count
            ),
        )]))
    }

    #[tool(
        name = "snapshot_tag",
        description = "Attach or overwrite a label on an existing snapshot. Only the label changes; the snapshot's book is not touched. Use `snapshot_list` to find timestamps.",
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

        let found = self
            .snapshots()
            .tag(&slug, Timestamp::from_millis(millis), &label)
            .await
            .map_err(|e| McpError::internal_error(format!("Failed to tag snapshot: {e}"), None))?;
        if !found {
            return Err(McpError::invalid_params(
                format!("Snapshot not found for timestamp {millis}"),
                None,
            ));
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            format!("Snapshot {} tagged as '{}'", millis, label),
        )]))
    }

    #[tool(
        name = "snapshot_delete",
        description = "Delete one snapshot of the selected book (timestamp from `snapshot_list`). Cannot be undone. A labeled snapshot needs `force: true`. The book itself is not changed, so nothing is added to its history.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn snapshot_delete(
        &self,
        Parameters(req): Parameters<McpSnapshotDeleteRequest>,
    ) -> Result<CallToolResult, McpError> {
        let msg = self.delete_snapshot(&req.timestamp, req.force).await?;
        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            msg,
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
                format!(
                    "Invalid from_ts: '{}'. Must be a millis integer.",
                    req.from_ts
                ),
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
                format!("from_ts ({from_ms}) must be strictly less than to_ts ({to_ms})"),
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

        // 両 snapshot の label を取りに行くために list を舐める
        let infos = self.snapshots().list(&slug).await.map_err(|e| {
            McpError::internal_error(format!("Failed to list snapshots: {e}"), None)
        })?;
        let find_label = |ms: i64| -> Option<String> {
            infos
                .iter()
                .find(|i| i.timestamp.as_millis() == ms)
                .and_then(|i| i.label.clone())
        };

        let from_book = self.load_snapshot(&slug, from_ms).await?;
        let to_book = self.load_snapshot(&slug, to_ms).await?;

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
        for hunk in diff
            .unified_diff()
            .context_radius(context_lines)
            .iter_hunks()
        {
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
        let mut infos = self.snapshots().list(&slug).await.map_err(|e| {
            McpError::internal_error(format!("Failed to list snapshots: {e}"), None)
        })?;
        infos.reverse(); // 昇順 (最古が先頭)
        let total = infos.len();
        let (idx, info) = infos
            .iter()
            .enumerate()
            .find(|(_, i)| i.timestamp.as_millis() == millis)
            .ok_or_else(|| {
                McpError::invalid_params(format!("Snapshot not found for timestamp {millis}"), None)
            })?;

        let book = self.load_snapshot(&slug, millis).await?;

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

        let mut infos = self.snapshots().list(&slug).await.map_err(|e| {
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
            let book = self.load_snapshot(&slug, millis).await?;

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
        let id = self.resolve_id(&req.node_id).await?;

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

        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

        let title = book
            .get_node(id)
            .map(|n| n.title().to_string())
            .unwrap_or_else(|| id.short().to_string());

        let mut entries = self
            .service_for(&slug)
            .await?
            .history_by_node(id)
            .await
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
            let action_str = action_name(entry.action);
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

        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

        let mut entries = svc
            .history()
            .await
            .map_err(|e| McpError::internal_error(format!("Failed to load history: {e}"), None))?;

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

        // snapshot label cross-ref (best-effort: a snapshot listing failure
        // should not break history display)
        let snap_infos = self.snapshots().list(&slug).await.unwrap_or_default();
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
                format!(
                    "No history for \"{}\" in the requested range.",
                    book.title()
                ),
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

        let mut output = format!("# History for \"{}\"{}\n\n", book.title(), showing_note);

        for (i, entry) in entries.iter().enumerate() {
            let label_note = snap_label_by_millis
                .get(&entry.timestamp.as_millis())
                .map(|l| format!(" [snapshot: {l}]"))
                .unwrap_or_default();
            if entry.node_id.is_book_level() {
                output.push_str(&format!(
                    "{}. [{}] {} {}{}\n",
                    i + 1,
                    entry.timestamp.to_iso8601(),
                    action_name(entry.action),
                    describe_book_update(entry.before.as_deref(), entry.after.as_deref()),
                    label_note
                ));
                continue;
            }
            let action_str = action_name(entry.action);
            let hier = find_hierarchical_id(&book, entry.node_id)
                .unwrap_or_else(|| entry.node_id.short().to_string());
            let title = book
                .get_node(entry.node_id)
                .map(|n| n.title().to_string())
                .unwrap_or_else(|| "(deleted)".to_string());
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
        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

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
    async fn resolve_uuid(
        &self,
        s: &str,
    ) -> Result<outline_mcp_core::domain::model::id::NodeId, McpError> {
        // 1. Full UUID
        if let Ok(id) = parse_node_id(s) {
            return crate::server::reject_book_level(id);
        }
        // 2. Short prefix in current book
        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;
        let matches: Vec<outline_mcp_core::domain::model::id::NodeId> = book
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
        description = "Move multiple nodes in a single atomic operation. All nodes must be specified by UUID (not toc ID); `node_create`, `node_move` and `node_query` return them. All moves succeed or none are saved. Each moved node takes its descendants along; the result lists them. Pass `dry_run: true` to see every node that would move (old → new positions) without changing anything.",
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
            outline_mcp_core::domain::model::id::NodeId,
            Option<outline_mcp_core::domain::model::id::NodeId>,
            usize,
        )> = Vec::with_capacity(total);

        for (i, item) in req.moves.iter().enumerate() {
            let id = self.resolve_uuid(&item.node_id).await.map_err(|e| {
                McpError::invalid_params(
                    format!(
                        "Batch move failed at operation {}/{total} (node {}): {e}. No changes saved.",
                        i + 1,
                        item.node_id
                    ),
                    None,
                )
            })?;
            let new_parent = match item.new_parent.as_deref() {
                Some(s) => Some(self.resolve_uuid(s).await.map_err(|e| {
                    McpError::invalid_params(
                        format!(
                            "Batch move failed at operation {}/{total} (node {}): parent UUID: {e}. No changes saved.",
                            i + 1,
                            item.node_id
                        ),
                        None,
                    )
                })?),
                None => None,
            };
            let position = item.position.unwrap_or(usize::MAX);
            resolved.push((id, new_parent, position));
        }

        let svc = self.service().await?;

        if req.dry_run {
            let plan = resolved.clone();
            let (before, after) = svc
                .preview(|b| {
                    for (id, new_parent, position) in plan {
                        b.move_node(id, new_parent, position)?;
                    }
                    Ok(())
                })
                .await
                .map_err(|e| {
                    McpError::invalid_params(
                        format!("Batch move would fail: {e}. Nothing changed."),
                        None,
                    )
                })?;
            let roots: Vec<_> = resolved.iter().map(|(id, _, _)| *id).collect();
            let mut msg = format!("Dry run: {total} move(s) would succeed:\n");
            msg.push_str(&describe_subtrees_change(&before, &after, &roots));
            msg.push_str("Nothing changed.");
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                msg,
            )]));
        }

        let ids: Vec<_> = resolved.iter().map(|(id, _, _)| *id).collect();
        let (count, warnings) = svc.batch_move(resolved).await.map_err(|e| {
            // Same error code as the dry run for the same rejected input.
            let msg = format!("Batch move failed: {e}. No changes saved.");
            match e {
                outline_mcp_core::application::error::AppError::Domain(_) => {
                    McpError::invalid_params(msg, None)
                }
                _ => McpError::internal_error(msg, None),
            }
        })?;

        let mut msg = format!("Batch move complete: {count}/{total} operations succeeded.");
        // The batch is committed; a failed read-back only degrades the report.
        match svc.read_tree().await {
            Ok(book) => {
                for id in ids {
                    let hier =
                        find_hierarchical_id(&book, id).unwrap_or_else(|| id.short().to_string());
                    msg.push_str(&format!(
                        "\n- Moved → {}. {}{}{}",
                        hier,
                        book.get_node(id).map(|n| n.title()).unwrap_or("?"),
                        describe_descendants(&book, id, "with"),
                        id_suffix(id)
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "node_batch_move: committed, but reading the tree back failed");
                msg.push_str(&format!(
                    "\n(committed; new positions could not be read back: {e})"
                ));
                for id in ids {
                    msg.push_str(&format!("\n- Moved{}", id_suffix(id)));
                }
            }
        }
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
            outline_mcp_core::domain::model::id::NodeId,
            outline_mcp_core::domain::model::book::UpdateNodeRequest,
        )> = Vec::with_capacity(total);

        for (i, item) in req.updates.iter().enumerate() {
            let id = self.resolve_uuid(&item.node_id).await.map_err(|e| {
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
            let update_req = outline_mcp_core::domain::model::book::UpdateNodeRequest {
                title: item.title.as_deref().map(unescape_newlines),
                body: item.body.clone().map(|b| b.map(|s| unescape_newlines(&s))),
                node_type: None,
                placeholder: None,
                properties: item.properties.clone(),
                properties_patch: item.properties_patch.clone(),
                status,
            };
            resolved.push((id, update_req));
        }

        let svc = self.service().await?;
        let (count, warnings) = svc.batch_update(resolved).await.map_err(|e| {
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
        description = "Query nodes by properties, status, type, subtree, or `text` (case-insensitive substring of title or body; CJK works; results show a snippet of the first body match). All given filters must match. Returns UUIDs needed for batch operations. Use `include_body: true` to include node content. Programs should pass `format: \"json\"` — the default text rendering is lossy for property values containing newlines and for bodies starting with a fence.",
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
        use outline_mcp_core::domain::model::node::NodeType;

        let svc = self.service().await?;
        let book = svc.read_tree().await.map_err(Self::to_mcp_error)?;

        let root_id = match req.subtree_root.as_deref() {
            Some(s) => Some(self.resolve_id(s).await?),
            None => None,
        };

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

        // Archived nodes (and nodes under an archived section) only come back
        // when asked for: `status: "archived"` or `include_archived`.
        match req.status.as_deref() {
            Some(s) => {
                let st = parse_node_status(s)?;
                nodes.retain(|n| n.status() == st);
                if st != NodeStatus::Archived && !req.include_archived {
                    nodes.retain(|n| !book.is_archived_or_under_archived(n.id()));
                }
            }
            None if !req.include_archived => {
                nodes.retain(|n| !book.is_archived_or_under_archived(n.id()));
            }
            None => {}
        }

        let verified_cutoff = match req.verified_before.as_deref() {
            Some(s) => Some(
                Timestamp::parse_moment(s)
                    .map_err(|e| McpError::invalid_params(format!("verified_before: {e}"), None))?,
            ),
            None => None,
        };
        if let Some(cutoff) = verified_cutoff {
            nodes.retain(|n| n.verified_before(cutoff));
        }

        let text_query = match req.text.as_deref() {
            Some(t) => Some(
                TextQuery::new(t)
                    .ok_or_else(|| McpError::invalid_params("text must not be empty", None))?,
            ),
            None => None,
        };
        if let Some(ref q) = text_query {
            nodes.retain(|n| q.matches(n));
        }

        if let Some(ref to) = req.linked_to {
            let target = self.resolve_id(to).await?;
            let rel = match req.link_rel.as_deref() {
                Some(r) => Some(normalize_rel(r).ok_or_else(|| {
                    McpError::invalid_params(format!("link_rel: invalid link type {r:?}"), None)
                })?),
                None => None,
            };
            nodes.retain(|n| {
                n.links()
                    .iter()
                    .any(|l| l.target() == target && rel.as_deref().is_none_or(|r| l.rel() == r))
            });
        }

        let snippet = |node: &outline_mcp_core::domain::model::node::TemplateNode| {
            text_query
                .as_ref()
                .and_then(|q| q.snippet(node, SNIPPET_CHARS))
        };

        // Before the empty check on purpose. Zero matches is the ordinary
        // outcome for a poller (the placement gate asks "is there anything
        // pending" every three hours and the answer is usually no), so a
        // caller that asked for JSON must get JSON then too — otherwise the
        // quiet day is the one that breaks it.
        if parse_query_format(req.format.as_deref())? == QueryFormat::Json {
            // One pass over the book for every node's backlinks.
            let backlink_index = book.backlink_index();
            let out = serde_json::json!({
                "matches": nodes.len(),
                "nodes": nodes.iter().map(|node| {
                    let mut o = serde_json::json!({
                        "id": node.id().to_string(),
                        "short": node.id().short(),
                        "title": node.title(),
                        "node_type": match node.node_type() {
                            NodeType::Section => "section",
                            NodeType::Content => "content",
                        },
                        "status": node.status().as_str(),
                        // null when never verified.
                        "verified_at": node.verified_at().map(|t| t.to_iso8601()),
                        "verified_by": node.verified_by(),
                        "placeholder": node.placeholder(),
                        // Always an object, never omitted: a caller has to be
                        // able to tell "this node has no properties" from
                        // "properties did not come back".
                        "properties": node.properties(),
                        // Outgoing links; always an array.
                        // `target_status` / `source_status`: "active", "draft",
                        // "archived" (also for a node under an archived
                        // section) or "missing" (not in the book).
                        "links": node.links().iter().map(|l| serde_json::json!({
                            "rel": l.rel(),
                            "target": l.target().to_string(),
                            "target_title": book.get_node(l.target()).map(|t| t.title()),
                            "target_status": endpoint_status(&book, l.target()),
                        })).collect::<Vec<_>>(),
                        // Incoming links (derived from the book); always an array.
                        "backlinks": backlink_index.get(&node.id()).map(|v| v.as_slice()).unwrap_or(&[]).iter().map(|(src, l)| serde_json::json!({
                            "rel": l.rel(),
                            "source": src.to_string(),
                            "source_title": book.get_node(*src).map(|s| s.title()),
                            "source_status": endpoint_status(&book, *src),
                        })).collect::<Vec<_>>(),
                        // The pending revision's summary (by, note, at,
                        // fields, stale), or null when there is none.
                        "proposal": node.proposal().map(|p| proposal_json(node, p)),
                    });
                    // Present only when asked for, so "I did not request the
                    // body" and "the body is empty" stay distinguishable.
                    if req.include_body {
                        o["body"] = serde_json::json!(node.body());
                    }
                    // Present whenever `text` was given; null when only the
                    // title matched.
                    if text_query.is_some() {
                        o["snippet"] = serde_json::json!(snippet(node));
                    }
                    o
                }).collect::<Vec<_>>(),
            });
            return Ok(CallToolResult::structured(out));
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
            let status_str = node.status().as_str();
            output.push_str(&format!(
                "\n{}. [{}] {}\n   UUID: {}\n   Type: {}\n   Status: {}\n",
                i + 1,
                short,
                node.title(),
                full,
                type_str,
                status_str,
            ));
            // Shown when the node has been verified, and as "never" when the
            // caller is asking about verification.
            match (node.verified_at(), verified_cutoff) {
                (Some(at), _) => output.push_str(&format!(
                    "   Verified: {}{}\n",
                    at.to_iso8601(),
                    node.verified_by()
                        .map(|b| format!(" by {b}"))
                        .unwrap_or_default()
                )),
                (None, Some(_)) => output.push_str("   Verified: never\n"),
                (None, None) => {}
            }
            let props = node.properties();
            if !props.is_empty() {
                let props_str = props
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!("   Properties: {}\n", props_str));
            }
            let links = links_line(&book, node);
            if !links.is_empty() {
                output.push_str(&format!("   {links}\n"));
            }
            if let Some(p) = node.proposal() {
                output.push_str(&format!(
                    "   {}\n",
                    proposal_line(node, p, Timestamp::now())
                ));
            }
            if let Some(s) = snippet(node) {
                output.push_str(&format!("   Match: {}\n", s));
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

/// The word a history listing shows for an action.
/// `book` without its archived nodes and everything under them (an
/// in-memory copy for exports; nothing is committed).
fn without_archived(
    mut book: outline_mcp_core::domain::model::book::TemplateBook,
) -> outline_mcp_core::domain::model::book::TemplateBook {
    // Only the top-most archived nodes need removing: removal takes the
    // subtree with it.
    let tops: Vec<outline_mcp_core::domain::model::id::NodeId> = book
        .all_nodes_dfs()
        .into_iter()
        .filter(|n| {
            n.status() == NodeStatus::Archived
                && !n
                    .parent()
                    .is_some_and(|p| book.is_archived_or_under_archived(p))
        })
        .map(|n| n.id())
        .collect();
    for id in tops {
        // Ids come from the book itself, so removal cannot fail.
        let _ = book.remove_node(id);
    }
    book
}

/// A free-text field trimmed, `None` when empty.
fn trimmed(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn action_name(action: ChangeAction) -> &'static str {
    match action {
        ChangeAction::Create => "create",
        ChangeAction::Update => "update",
        ChangeAction::Delete => "delete",
        ChangeAction::Move => "move",
        ChangeAction::Restore => "restore",
        ChangeAction::BookUpdate => "book_update",
    }
}

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

/// Output shape for `node_query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryFormat {
    /// The human-readable listing this tool has always produced.
    Text,
    /// One JSON object, for callers that parse the result.
    Json,
}

/// Unknown values are rejected rather than defaulted, so a typo surfaces here
/// instead of as a decode failure in the caller — the text a program did not
/// ask for is still valid text.
fn parse_query_format(s: Option<&str>) -> Result<QueryFormat, McpError> {
    match s {
        Some("json") => Ok(QueryFormat::Json),
        Some("text") | None => Ok(QueryFormat::Text),
        Some(other) => Err(McpError::invalid_params(
            format!("Unknown format: '{other}'. Use: text, json"),
            None,
        )),
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
mod query_format_tests {
    use super::*;

    #[test]
    fn parse_query_format_defaults_to_text() {
        assert_eq!(parse_query_format(None).unwrap(), QueryFormat::Text);
        assert_eq!(parse_query_format(Some("text")).unwrap(), QueryFormat::Text);
        assert_eq!(parse_query_format(Some("json")).unwrap(), QueryFormat::Json);
    }

    #[test]
    fn parse_query_format_rejects_unknown() {
        // Silently falling back to text would surface as a decode failure in
        // the caller, a long way from the typo that caused it.
        let err = parse_query_format(Some("markdown")).unwrap_err();
        assert!(err.to_string().contains("Unknown format"));
        assert!(parse_query_format(Some("")).is_err());
    }

    /// The values that make the text rendering lossy, kept in one place so a
    /// reader can see what the JSON path has to survive.
    const HOSTILE_VALUE: &str = "note: a, b = c\nsecond line\n---\n```lua";

    #[test]
    fn json_body_survives_values_that_break_the_text_rendering() {
        // `Properties: k=v, k=v` truncates at the first newline in a value,
        // and a `---` line inside a body is indistinguishable from the entry
        // separator. serde_json escapes both, so neither can leak into the
        // frame.
        let encoded = serde_json::json!({ "properties": { "evidence_quote": HOSTILE_VALUE } });
        let round_tripped = encoded["properties"]["evidence_quote"].as_str().unwrap();
        assert_eq!(
            round_tripped, HOSTILE_VALUE,
            "value must survive byte-for-byte"
        );

        let serialized = serde_json::to_string(&encoded).unwrap();
        assert!(
            !serialized.contains('\n'),
            "raw newline must not reach the wire"
        );
        let decoded: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            decoded["properties"]["evidence_quote"].as_str().unwrap(),
            HOSTILE_VALUE
        );
    }
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

#[cfg(test)]
mod import_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::domain::model::node::NodeType;
    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::server::Storage;

    fn add_req(title: &str) -> AddNodeRequest {
        AddNodeRequest {
            parent: None,
            title: title.to_string(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn import_tool_records_the_replacement_in_history() {
        let server = OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ));
        let svc = server.service_for("imp").await.unwrap();
        svc.create_book("Imp", 4).await.unwrap();
        let (old, _) = svc.add_node(add_req("old")).await.unwrap();
        *server.selected.write().unwrap() = Some("imp".into());

        // An export of a different tree: its nodes get fresh ids on import.
        let mut other = outline_mcp_core::domain::model::book::TemplateBook::new("Imp", 4);
        other.add_node(add_req("new 1")).unwrap();
        other.add_node(add_req("new 2")).unwrap();
        let json = EjectService::render_json(&other, None).unwrap();
        let path = std::env::temp_dir().join(format!(
            "outline-mcp-import-test-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).unwrap();

        let result = server
            .import(Parameters(McpImportRequest {
                file_path: path.to_string_lossy().into_owned(),
            }))
            .await
            .expect("import");
        let _ = std::fs::remove_file(&path);
        let text = format!("{:?}", result.content);
        assert!(text.contains("3 history entries recorded"), "{text}");

        let history = svc.history().await.unwrap();
        let tail: Vec<_> = history[1..].iter().map(|e| e.action).collect();
        assert_eq!(
            tail.iter().filter(|a| **a == ChangeAction::Delete).count(),
            1
        );
        assert_eq!(
            tail.iter().filter(|a| **a == ChangeAction::Create).count(),
            2
        );
        let old_history = svc.history_by_node(old).await.unwrap();
        assert_eq!(old_history.last().unwrap().action, ChangeAction::Delete);
    }
}

#[cfg(test)]
mod book_update_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::domain::model::node::NodeType;
    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::server::Storage;

    fn server() -> OutlineMcpServer {
        OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ))
    }

    fn req(slug: &str, title: Option<&str>, max_depth: Option<u8>) -> McpBookUpdateRequest {
        McpBookUpdateRequest {
            slug: slug.into(),
            title: title.map(str::to_string),
            max_depth,
        }
    }

    fn text(r: &CallToolResult) -> String {
        format!("{:?}", r.content)
    }

    async fn book_with_depth_two_node(server: &OutlineMcpServer, slug: &str) {
        let svc = server.service_for(slug).await.unwrap();
        svc.create_book("Old Title", 3).await.unwrap();
        let (parent, _) = svc
            .add_node(AddNodeRequest {
                parent: None,
                title: "top".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .await
            .unwrap();
        svc.add_node(AddNodeRequest {
            parent: Some(parent),
            title: "nested".into(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn validation_errors_change_nothing() {
        let s = server();
        book_with_depth_two_node(&s, "b").await;

        let none = s.book_update(Parameters(req("b", None, None))).await;
        assert!(none.unwrap_err().message.contains("Nothing to change"));
        let missing = s
            .book_update(Parameters(req("nope", Some("X"), None)))
            .await;
        assert!(missing.unwrap_err().message.contains("not found"));
        let empty = s.book_update(Parameters(req("b", Some("   "), None))).await;
        assert!(empty.unwrap_err().message.contains("must not be empty"));
        let shallow = s.book_update(Parameters(req("b", None, Some(1)))).await;
        let msg = shallow.unwrap_err().message.to_string();
        assert!(
            msg.contains("\"nested\"") && msg.contains("depth 2"),
            "{msg}"
        );

        let book = s.service_for("b").await.unwrap().read_tree().await.unwrap();
        assert_eq!((book.title(), book.max_depth()), ("Old Title", 3));
    }

    #[tokio::test]
    async fn update_is_reported_and_listed_in_book_history() {
        let s = server();
        book_with_depth_two_node(&s, "b").await;

        let r = s
            .book_update(Parameters(req("b", Some("New Title"), Some(4))))
            .await
            .unwrap();
        let t = text(&r);
        assert!(
            t.contains(r#"title \"Old Title\" → \"New Title\""#) && t.contains("max_depth 3 → 4"),
            "{t}"
        );

        *s.selected.write().unwrap() = Some("b".into());
        let h = s
            .book_history(Parameters(McpBookHistoryRequest {
                limit: None,
                since: None,
                until: None,
            }))
            .await
            .unwrap();
        let h = text(&h);
        assert!(h.contains("book_update"), "{h}");
        assert!(h.contains(r#"max_depth 3 → 4"#), "{h}");
        assert!(
            !h.contains("(deleted)"),
            "book-level entry rendered as a node: {h}"
        );

        // Same values again: no commit, no new history entry.
        let again = s
            .book_update(Parameters(req("b", Some("New Title"), Some(4))))
            .await
            .unwrap();
        assert!(text(&again).contains("No change"));
        let entries = s.service_for("b").await.unwrap().history().await.unwrap();
        let book_updates = entries
            .iter()
            .filter(|e| e.action == ChangeAction::BookUpdate)
            .count();
        assert_eq!(book_updates, 1);
    }
}

#[cfg(test)]
mod ergonomics_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::request::McpBatchMoveItem;
    use crate::server::Storage;

    async fn server_with_book() -> OutlineMcpServer {
        let s = OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ));
        s.service_for("e")
            .await
            .unwrap()
            .create_book("E", 4)
            .await
            .unwrap();
        *s.selected.write().unwrap() = Some("e".into());
        s
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Creates a node through the tool and returns (toc id, uuid) parsed from
    /// the result line `Created: <toc>. <title> (id: <uuid>)`.
    async fn create(
        s: &OutlineMcpServer,
        parent: Option<&str>,
        title: &str,
        body: Option<&str>,
    ) -> (String, String) {
        let r = s
            .node_create(Parameters(McpNodeCreateRequest {
                parent: parent.map(str::to_string),
                title: title.into(),
                node_type: "section".into(),
                body: body.map(str::to_string),
                placeholder: None,
                position: None,
                properties: None,
            }))
            .await
            .expect("create");
        let t = text(&r);
        let toc = t
            .strip_prefix("Created: ")
            .and_then(|r| r.split('.').next())
            .expect("toc id")
            .to_string();
        (toc, trailing_uuid(&t).expect("uuid in result"))
    }

    /// The uuid of a result line's final ` (id: <uuid>)` — the last element of
    /// every result line, so a title containing "(id: " cannot confuse it.
    fn trailing_uuid(line: &str) -> Option<String> {
        let rest = line.trim_end().strip_suffix(')')?;
        let uuid = rest.get(rest.len().checked_sub(36)?..)?;
        let head = rest.get(..rest.len() - 36)?;
        (head.ends_with(" (id: ") && crate::request::parse_node_id(uuid).is_ok())
            .then(|| uuid.to_string())
    }

    async fn book_version(s: &OutlineMcpServer) -> i64 {
        s.storage.books.load("e").await.unwrap().unwrap().version
    }

    #[test]
    fn trailing_uuid_ignores_id_text_inside_the_title() {
        let u = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let line = format!("Created: 1. odd (id: title) (id: {u})");
        assert_eq!(trailing_uuid(&line).as_deref(), Some(u));
        assert_eq!(trailing_uuid("Created: 1. no id"), None);
    }

    fn query(text: &str) -> McpNodeQueryRequest {
        McpNodeQueryRequest {
            filter: None,
            include_body: false,
            kind: None,
            status: None,
            include_archived: false,
            verified_before: None,
            subtree_root: None,
            text: Some(text.into()),
            linked_to: None,
            link_rel: None,
            format: None,
        }
    }

    async fn version(s: &OutlineMcpServer) -> usize {
        s.service_for("e")
            .await
            .unwrap()
            .history()
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn text_query_matches_body_only_cjk_and_ignores_case() {
        let s = server_with_book().await;
        create(&s, None, "Git", Some("ブランチ運用ルールの説明")).await;
        create(&s, None, "Branch Strategy", None).await;
        create(&s, None, "unrelated", Some("nothing here")).await;

        let r = text(&s.node_query(Parameters(query("運用"))).await.unwrap());
        assert!(r.contains("(1 matches)") && r.contains("Git"), "{r}");
        assert!(r.contains("Match: ブランチ運用ルールの説明"), "{r}");

        let r = text(&s.node_query(Parameters(query("branch"))).await.unwrap());
        assert!(
            r.contains("(1 matches)") && r.contains("Branch Strategy"),
            "{r}"
        );

        let blank = s.node_query(Parameters(query("  "))).await;
        assert!(blank.unwrap_err().message.contains("must not be empty"));
    }

    #[tokio::test]
    async fn text_query_respects_subtree_and_reports_snippet_in_json() {
        let s = server_with_book().await;
        let (a, _) = create(&s, None, "A", None).await;
        create(&s, Some(&a), "inside", Some("needle in A")).await;
        create(&s, None, "outside", Some("needle elsewhere")).await;

        let mut q = query("NEEDLE");
        q.subtree_root = Some(a);
        q.format = Some("json".into());
        let r = s.node_query(Parameters(q)).await.unwrap();
        let v = r.structured_content.expect("json");
        assert_eq!(v["matches"], 1);
        assert_eq!(v["nodes"][0]["title"], "inside");
        assert_eq!(v["nodes"][0]["snippet"], "needle in A");
    }

    #[tokio::test]
    async fn title_only_match_has_a_null_snippet_in_json() {
        let s = server_with_book().await;
        create(&s, None, "Needle title", Some("body without it")).await;
        let mut q = query("needle");
        q.format = Some("json".into());
        let v = s
            .node_query(Parameters(q))
            .await
            .unwrap()
            .structured_content
            .expect("json");
        assert_eq!(v["matches"], 1);
        assert!(v["nodes"][0]["snippet"].is_null(), "{v}");
    }

    #[tokio::test]
    async fn toc_max_depth_and_limit_report_hidden_nodes() {
        let s = server_with_book().await;
        let (a, _) = create(&s, None, "A", None).await;
        create(&s, Some(&a), "A1", None).await;
        create(&s, None, "B", None).await;

        let toc = |max_depth, limit| McpTocRequest {
            subtree_root: None,
            filter: None,
            max_depth,
            limit,
            include_archived: false,
        };
        let r = text(&s.toc(Parameters(toc(Some(1), None))).await.unwrap());
        assert!(
            r.contains("1. A") && r.contains("2. B") && !r.contains("A1"),
            "{r}"
        );
        assert!(r.contains("… 1 more nodes not shown"), "{r}");

        let r = text(&s.toc(Parameters(toc(None, Some(2)))).await.unwrap());
        assert!(r.contains("1-1. A1") && !r.contains("2. B"), "{r}");
        assert!(r.contains("… 1 more nodes not shown"), "{r}");

        let r = text(&s.toc(Parameters(toc(None, None))).await.unwrap());
        assert!(!r.contains("not shown"), "{r}");

        let zero = s.toc(Parameters(toc(Some(0), None))).await;
        assert!(zero.unwrap_err().message.contains("at least 1"));
    }

    #[tokio::test]
    async fn uuid_from_node_create_is_accepted_by_node_batch_move() {
        let s = server_with_book().await;
        let (_, a) = create(&s, None, "A", None).await;
        let (_, b) = create(&s, None, "B", None).await;

        let r = s
            .node_batch_move(Parameters(McpBatchMoveRequest {
                moves: vec![McpBatchMoveItem {
                    node_id: a.clone(),
                    new_parent: Some(b.clone()),
                    position: None,
                }],
                dry_run: false,
            }))
            .await
            .expect("batch move with the uuids node_create returned");
        let t = text(&r);
        assert!(
            t.contains("1/1") && t.contains(&format!("(id: {a})")),
            "{t}"
        );
    }

    #[tokio::test]
    async fn move_dry_run_lists_descendants_and_changes_nothing() {
        let s = server_with_book().await;
        let (a, a_id) = create(&s, None, "A", None).await;
        create(&s, Some(&a), "A1", None).await;
        create(&s, Some(&a), "A2", None).await;
        let (b, _) = create(&s, None, "B", None).await;
        let v = version(&s).await;

        let mv = |dry_run| McpNodeMoveRequest {
            node_id: a.clone(),
            action: "move".into(),
            new_parent: Some(b.clone()),
            position: None,
            dry_run,
        };
        let r = text(&s.node_move(Parameters(mv(true))).await.unwrap());
        assert!(r.contains("would move 3 node(s)"), "{r}");
        assert!(
            r.contains("1. A → 1-1") && r.contains("1-1. A1 → 1-1-1"),
            "{r}"
        );
        assert!(r.contains("Nothing changed"), "{r}");
        assert_eq!(version(&s).await, v, "dry run wrote history");

        let r = text(&s.node_move(Parameters(mv(false))).await.unwrap());
        assert_eq!(
            r,
            format!("Moved → 1-1. A with 2 descendants: A1, A2 (id: {a_id})")
        );
        assert_eq!(trailing_uuid(&r), Some(a_id));
    }

    #[tokio::test]
    async fn remove_without_descendants_ends_with_the_id() {
        let s = server_with_book().await;
        let (a, a_id) = create(&s, None, "Leaf", None).await;
        let r = text(
            &s.node_move(Parameters(McpNodeMoveRequest {
                node_id: a,
                action: "remove".into(),
                new_parent: None,
                position: None,
                dry_run: false,
            }))
            .await
            .unwrap(),
        );
        assert_eq!(r, format!("Removed: 1. Leaf (id: {a_id})"));
    }

    #[tokio::test]
    async fn rejected_move_is_invalid_params_in_dry_run_and_real_run() {
        let s = server_with_book().await;
        let (a, _) = create(&s, None, "A", None).await;
        let (a1, _) = create(&s, Some(&a), "A1", None).await;
        // Moving A under its own child is a cycle.
        for dry_run in [true, false] {
            let e = s
                .node_move(Parameters(McpNodeMoveRequest {
                    node_id: a.clone(),
                    action: "move".into(),
                    new_parent: Some(a1.clone()),
                    position: None,
                    dry_run,
                }))
                .await
                .unwrap_err();
            assert_eq!(
                e.code,
                rmcp::model::ErrorCode::INVALID_PARAMS,
                "dry_run={dry_run}: {e:?}"
            );
        }
    }

    fn batch(moves: Vec<(String, Option<String>)>, dry_run: bool) -> McpBatchMoveRequest {
        McpBatchMoveRequest {
            moves: moves
                .into_iter()
                .map(|(node_id, new_parent)| McpBatchMoveItem {
                    node_id,
                    new_parent,
                    position: None,
                })
                .collect(),
            dry_run,
        }
    }

    #[tokio::test]
    async fn batch_dry_run_commits_nothing_on_success_or_failure() {
        let s = server_with_book().await;
        let (_, a) = create(&s, None, "A", None).await;
        let (_, b) = create(&s, None, "B", None).await;
        let v = book_version(&s).await;

        let ok = s
            .node_batch_move(Parameters(batch(vec![(a.clone(), Some(b.clone()))], true)))
            .await
            .unwrap();
        assert!(text(&ok).contains("would succeed"), "{}", text(&ok));
        assert_eq!(book_version(&s).await, v, "successful dry run committed");

        // A under B, then B under A: a cycle on the second operation.
        let err = s
            .node_batch_move(Parameters(batch(
                vec![(a.clone(), Some(b.clone())), (b.clone(), Some(a.clone()))],
                true,
            )))
            .await
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{err:?}");
        assert_eq!(book_version(&s).await, v, "failed dry run committed");

        // The real run of the same failing batch: same code, nothing saved.
        let err = s
            .node_batch_move(Parameters(batch(
                vec![(a.clone(), Some(b.clone())), (b, Some(a))],
                false,
            )))
            .await
            .unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{err:?}");
        assert_eq!(book_version(&s).await, v);
    }

    #[tokio::test]
    async fn batch_moving_a_parent_and_its_child_previews_what_runs() {
        let s = server_with_book().await;
        let (a_toc, a) = create(&s, None, "A", None).await;
        let (_, a1) = create(&s, Some(&a_toc), "A1", None).await;
        let (_, b) = create(&s, None, "B", None).await;
        let (_, c) = create(&s, None, "C", None).await;
        // A (with A1) under B, then A1 alone under C.
        let moves = vec![(a.clone(), Some(b.clone())), (a1.clone(), Some(c.clone()))];

        let preview = text(
            &s.node_batch_move(Parameters(batch(moves.clone(), true)))
                .await
                .unwrap(),
        );
        // A1 is both a descendant of A and a batch item: listed once.
        assert_eq!(
            preview.matches(&format!("(id: {a1})")).count(),
            1,
            "{preview}"
        );

        s.node_batch_move(Parameters(batch(moves, false)))
            .await
            .unwrap();
        let toc = text(
            &s.toc(Parameters(McpTocRequest {
                subtree_root: None,
                filter: None,
                max_depth: None,
                limit: None,
                include_archived: false,
            }))
            .await
            .unwrap(),
        );
        // The preview's destinations are the real ones.
        assert!(
            preview.contains("1. A → 1-1") && toc.contains("1-1. A"),
            "{preview}\n{toc}"
        );
        assert!(
            preview.contains("1-1. A1 → 2-1") && toc.contains("2-1. A1"),
            "{preview}\n{toc}"
        );
    }

    #[tokio::test]
    async fn toc_rejects_zero_before_looking_at_the_book() {
        let s = server_with_book().await; // empty book
        let toc = |max_depth, limit| McpTocRequest {
            subtree_root: None,
            filter: None,
            max_depth,
            limit,
            include_archived: false,
        };
        let e = s.toc(Parameters(toc(Some(0), None))).await.unwrap_err();
        assert!(e.message.contains("max_depth"), "{e:?}");
        let e = s.toc(Parameters(toc(None, Some(0)))).await.unwrap_err();
        assert!(e.message.contains("limit"), "{e:?}");
    }

    #[tokio::test]
    async fn toc_subtree_root_with_max_depth_counts_from_the_book_root() {
        let s = server_with_book().await;
        let (a, _) = create(&s, None, "A", None).await;
        let (a1, _) = create(&s, Some(&a), "A1", None).await;
        create(&s, Some(&a1), "A1x", None).await;
        let r = text(
            &s.toc(Parameters(McpTocRequest {
                subtree_root: Some(a),
                filter: None,
                max_depth: Some(2),
                limit: None,
                include_archived: false,
            }))
            .await
            .unwrap(),
        );
        assert!(r.contains("1. A") && r.contains("1-1. A1"), "{r}");
        assert!(!r.contains("A1x"), "{r}");
        assert!(r.contains("… 1 more nodes not shown"), "{r}");
    }

    #[tokio::test]
    async fn remove_reports_descendants_and_records_every_delete() {
        let s = server_with_book().await;
        let (a, _) = create(&s, None, "A", None).await;
        create(&s, Some(&a), "A1", None).await;
        create(&s, Some(&a), "A2", None).await;
        let rm = |dry_run| McpNodeMoveRequest {
            node_id: a.clone(),
            action: "remove".into(),
            new_parent: None,
            position: None,
            dry_run,
        };

        let v = version(&s).await;
        let r = text(&s.node_move(Parameters(rm(true))).await.unwrap());
        assert!(
            r.contains("would remove 3 node(s)") && r.contains("→ removed"),
            "{r}"
        );
        assert_eq!(version(&s).await, v);

        let r = text(&s.node_move(Parameters(rm(false))).await.unwrap());
        assert!(
            r.starts_with("Removed: 1. A and 2 descendants: A1, A2 (id: "),
            "{r}"
        );
        assert!(trailing_uuid(&r).is_some(), "{r}");
        let history = s.service_for("e").await.unwrap().history().await.unwrap();
        let deletes = history[v..]
            .iter()
            .filter(|e| e.action == ChangeAction::Delete)
            .count();
        assert_eq!(deletes, 3);
    }
}

#[cfg(test)]
mod health_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::domain::model::id::NodeId;
    use outline_mcp_core::domain::model::node::NodeType;
    use outline_mcp_core::domain::store::BookStore;
    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::server::Storage;

    fn add_req(parent: Option<NodeId>, title: &str, ty: NodeType) -> AddNodeRequest {
        AddNodeRequest {
            parent,
            title: title.to_string(),
            node_type: ty,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        }
    }

    async fn server_with_book() -> (OutlineMcpServer, Arc<MemoryBookStore>) {
        let books = Arc::new(MemoryBookStore::new());
        let server = OutlineMcpServer::new(Storage::new(
            books.clone(),
            Arc::new(MemorySnapshotStore::new()),
        ));
        let svc = server.service_for("h").await.unwrap();
        svc.create_book("Health", 4).await.unwrap();
        svc.add_node(add_req(None, "Loose note", NodeType::Content))
            .await
            .unwrap();
        svc.add_node(add_req(None, "Empty section", NodeType::Section))
            .await
            .unwrap();
        let (g, _) = svc
            .add_node(add_req(None, "Git", NodeType::Section))
            .await
            .unwrap();
        svc.add_node(add_req(
            Some(g),
            "Git branching strategy",
            NodeType::Content,
        ))
        .await
        .unwrap();
        svc.add_node(add_req(
            Some(g),
            "git branching strategy v2",
            NodeType::Content,
        ))
        .await
        .unwrap();
        (server, books)
    }

    fn lint_req(slug: Option<&str>, format: Option<&str>) -> McpBookLintRequest {
        McpBookLintRequest {
            slug: slug.map(str::to_string),
            checks: None,
            threshold: None,
            limit: None,
            stale_days: None,
            format: format.map(str::to_string),
        }
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn lint_and_stats_change_nothing_and_report_findings() {
        let (server, books) = server_with_book().await;
        let before = books.load("h").await.unwrap().unwrap().version;
        let history_before = books.history("h").await.unwrap().len();

        let r = server
            .book_lint(Parameters(lint_req(Some("h"), None)))
            .await
            .expect("lint");
        let t = text(&r);
        assert!(t.contains("## root_content (1)"), "{t}");
        assert!(t.contains("## empty_sections (1)"), "{t}");
        assert!(t.contains("## similar_titles (1)"), "{t}");
        assert!(t.contains("## stale_drafts: none"), "{t}");

        let j = server
            .book_lint(Parameters(lint_req(Some("h"), Some("json"))))
            .await
            .expect("lint json");
        let v = j.structured_content.expect("structured");
        assert_eq!(v["book"], "h");
        let kinds: Vec<&str> = v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"similar_titles"), "{kinds:?}");
        let sim = v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["kind"] == "similar_titles")
            .unwrap();
        assert_eq!(sim["nodes"].as_array().unwrap().len(), 2);
        assert!(sim["nodes"][0]["toc_id"].is_string());

        let s = server
            .book_stats(Parameters(McpBookStatsRequest {
                slug: Some("h".into()),
                format: Some("json".into()),
            }))
            .await
            .expect("stats");
        let sv = s.structured_content.expect("structured");
        assert_eq!(sv["nodes"], 5);
        assert_eq!(sv["sections"], 2);
        assert_eq!(sv["max_depth"], 2);
        let st = server
            .book_stats(Parameters(McpBookStatsRequest {
                slug: Some("h".into()),
                format: None,
            }))
            .await
            .expect("stats text");
        assert!(
            text(&st).contains("Nodes: 5 (2 sections, 3 content)"),
            "{}",
            text(&st)
        );

        assert_eq!(books.load("h").await.unwrap().unwrap().version, before);
        assert_eq!(books.history("h").await.unwrap().len(), history_before);
    }

    #[tokio::test]
    async fn selected_book_is_the_default_and_unknown_slug_errors() {
        let (server, _) = server_with_book().await;
        let err = server
            .book_lint(Parameters(lint_req(None, None)))
            .await
            .unwrap_err();
        assert!(err.message.contains("No book selected"), "{}", err.message);

        *server.selected.write().unwrap() = Some("h".into());
        assert!(server
            .book_lint(Parameters(lint_req(None, None)))
            .await
            .is_ok());

        let err = server
            .book_stats(Parameters(McpBookStatsRequest {
                slug: Some("missing".into()),
                format: None,
            }))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("Book 'missing' not found"),
            "{}",
            err.message
        );

        let mut bad = lint_req(Some("h"), None);
        bad.checks = Some(vec!["nope".into()]);
        assert!(server.book_lint(Parameters(bad)).await.is_err());
    }

    #[tokio::test]
    async fn lint_limit_reports_how_many_more() {
        let (server, _) = server_with_book().await;
        let svc = server.service_for("h").await.unwrap();
        for i in 0..3 {
            svc.add_node(add_req(None, &format!("Empty {i}"), NodeType::Section))
                .await
                .unwrap();
        }
        let mut req = lint_req(Some("h"), None);
        req.checks = Some(vec!["empty_sections".into()]);
        req.limit = Some(2);
        let t = text(&server.book_lint(Parameters(req)).await.unwrap());
        assert!(t.contains("## empty_sections (4)"), "{t}");
        assert!(t.contains("… 2 more not shown"), "{t}");
    }
}

#[cfg(test)]
mod archived_verify_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::request::McpSelectBookRequest;
    use crate::server::Storage;

    async fn server_with_book() -> OutlineMcpServer {
        let s = OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ));
        s.service_for("e")
            .await
            .unwrap()
            .create_book("E", 4)
            .await
            .unwrap();
        *s.selected.write().unwrap() = Some("e".into());
        s
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Creates a node and returns its toc id.
    async fn create(
        s: &OutlineMcpServer,
        parent: Option<&str>,
        title: &str,
        props: &[(&str, &str)],
    ) -> String {
        let r = s
            .node_create(Parameters(McpNodeCreateRequest {
                parent: parent.map(str::to_string),
                title: title.into(),
                node_type: "section".into(),
                body: Some(format!("{title} body")),
                placeholder: None,
                position: None,
                properties: (!props.is_empty()).then(|| {
                    props
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect()
                }),
            }))
            .await
            .expect("create");
        text(&r)
            .strip_prefix("Created: ")
            .and_then(|r| r.split('.').next())
            .expect("toc id")
            .to_string()
    }

    async fn set_status(s: &OutlineMcpServer, id: &str, status: &str) {
        s.node_update(Parameters(McpNodeUpdateRequest {
            node_id: id.into(),
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: None,
            status: Some(status.into()),
        }))
        .await
        .expect("set status");
    }

    fn query() -> McpNodeQueryRequest {
        McpNodeQueryRequest {
            filter: None,
            include_body: false,
            kind: None,
            status: None,
            include_archived: false,
            verified_before: None,
            subtree_root: None,
            text: None,
            linked_to: None,
            link_rel: None,
            format: None,
        }
    }

    fn toc(include_archived: bool) -> McpTocRequest {
        McpTocRequest {
            subtree_root: None,
            filter: None,
            max_depth: None,
            limit: None,
            include_archived,
        }
    }

    async fn history_len(s: &OutlineMcpServer) -> usize {
        s.service_for("e")
            .await
            .unwrap()
            .history()
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn archiving_stops_injection() {
        let s = server_with_book().await;
        let rule = create(&s, None, "Rule", &[("inject", "true")]).await;
        let select = || McpSelectBookRequest {
            book: "e".into(),
            quiet: true,
        };
        let before = text(&s.select_book(Parameters(select())).await.unwrap());
        assert!(before.contains("Injected Context (1 rules)"), "{before}");

        set_status(&s, &rule, "archived").await;
        let after = text(&s.select_book(Parameters(select())).await.unwrap());
        assert!(!after.contains("Injected Context"), "{after}");
    }

    #[tokio::test]
    async fn node_query_hides_archived_unless_asked() {
        let s = server_with_book().await;
        create(&s, None, "Live", &[]).await;
        let old = create(&s, None, "Old", &[]).await;
        create(&s, Some(&old), "Under old", &[]).await;
        set_status(&s, &old, "archived").await;

        let t = text(&s.node_query(Parameters(query())).await.unwrap());
        assert!(t.contains("(1 matches)") && t.contains("Live"), "{t}");
        assert!(!t.contains("Old") && !t.contains("Under old"), "{t}");

        let mut q = query();
        q.include_archived = true;
        let t = text(&s.node_query(Parameters(q)).await.unwrap());
        assert!(t.contains("(3 matches)"), "{t}");

        let mut q = query();
        q.status = Some("archived".into());
        let t = text(&s.node_query(Parameters(q)).await.unwrap());
        assert!(t.contains("(1 matches)") && t.contains("Old"), "{t}");
        assert!(t.contains("Status: archived"), "{t}");

        // Active nodes under an archived section stay hidden for status
        // "active" too, unless include_archived.
        let mut q = query();
        q.status = Some("active".into());
        let t = text(&s.node_query(Parameters(q)).await.unwrap());
        assert!(t.contains("(1 matches)") && !t.contains("Under old"), "{t}");
    }

    #[tokio::test]
    async fn toc_hides_archived_subtrees_and_counts_them() {
        let s = server_with_book().await;
        create(&s, None, "Live", &[]).await;
        let old = create(&s, None, "Old", &[]).await;
        create(&s, Some(&old), "Under old", &[]).await;
        set_status(&s, &old, "archived").await;

        let t = text(&s.toc(Parameters(toc(false))).await.unwrap());
        assert!(t.contains("1. Live") && !t.contains("Old"), "{t}");
        assert!(
            t.contains("… 2 archived nodes hidden (include_archived: true to show)"),
            "{t}"
        );

        let t = text(&s.toc(Parameters(toc(true))).await.unwrap());
        assert!(
            t.contains("2. Old [archived]") && t.contains("2-1. Under old"),
            "{t}"
        );
        assert!(!t.contains("hidden"), "{t}");
    }

    #[tokio::test]
    async fn node_verify_records_one_commit_and_feeds_verified_before() {
        let s = server_with_book().await;
        let a = create(&s, None, "A", &[]).await;
        let b = create(&s, None, "B", &[]).await;
        create(&s, None, "C", &[]).await;
        let h0 = history_len(&s).await;

        let r = s
            .node_verify(Parameters(McpNodeVerifyRequest {
                node_ids: vec![a.clone(), b.clone(), a.clone()],
                by: Some("alice".into()),
            }))
            .await
            .unwrap();
        let t = text(&r);
        assert!(
            t.starts_with("Verified 2 node(s) at ") && t.contains(" by alice:"),
            "{t}"
        );
        assert_eq!(history_len(&s).await, h0 + 2, "one entry per distinct node");

        // A cutoff in the future: everything verified before it, plus C.
        let mut q = query();
        q.verified_before = Some("2999-01-01".into());
        let t = text(&s.node_query(Parameters(q)).await.unwrap());
        assert!(t.contains("(3 matches)"), "{t}");
        assert!(
            t.contains("by alice") && t.contains("Verified: never"),
            "{t}"
        );

        // A cutoff in the past: only the never-verified node.
        let mut q = query();
        q.verified_before = Some("1000000000000".into()); // 2001, as millis
        let t = text(&s.node_query(Parameters(q)).await.unwrap());
        assert!(t.contains("(1 matches)") && t.contains("] C"), "{t}");

        let mut q = query();
        q.verified_before = Some("last week".into());
        let e = s.node_query(Parameters(q)).await.unwrap_err();
        assert!(e.message.contains("verified_before"), "{e:?}");
    }

    #[tokio::test]
    async fn node_verify_with_an_unknown_node_records_nothing() {
        let s = server_with_book().await;
        let a = create(&s, None, "A", &[]).await;
        let h0 = history_len(&s).await;
        let e = s
            .node_verify(Parameters(McpNodeVerifyRequest {
                node_ids: vec![a, "9".into()],
                by: None,
            }))
            .await;
        assert!(e.is_err());
        assert_eq!(history_len(&s).await, h0);

        let e = s
            .node_verify(Parameters(McpNodeVerifyRequest {
                node_ids: vec![],
                by: None,
            }))
            .await
            .unwrap_err();
        assert!(e.message.contains("at least one"), "{e:?}");
    }

    #[tokio::test]
    async fn node_query_json_carries_status_and_verification() {
        let s = server_with_book().await;
        let a = create(&s, None, "A", &[]).await;
        s.node_verify(Parameters(McpNodeVerifyRequest {
            node_ids: vec![a.clone()],
            by: Some("bob".into()),
        }))
        .await
        .unwrap();
        set_status(&s, &a, "archived").await;
        let mut q = query();
        q.status = Some("archived".into());
        q.format = Some("json".into());
        let r = s.node_query(Parameters(q)).await.unwrap();
        let v = r.structured_content.expect("structured");
        let n = &v["nodes"][0];
        assert_eq!(n["status"], "archived");
        assert_eq!(n["verified_by"], "bob");
        assert!(n["verified_at"].as_str().unwrap().ends_with('Z'));
    }

    #[tokio::test]
    async fn archiving_a_section_stops_injecting_its_active_children() {
        let s = server_with_book().await;
        let old = create(&s, None, "Old rules", &[]).await;
        create(&s, Some(&old), "Rule", &[("inject", "true")]).await;
        let select = |quiet| McpSelectBookRequest {
            book: "e".into(),
            quiet,
        };
        let before = text(&s.select_book(Parameters(select(true))).await.unwrap());
        assert!(before.contains("Injected Context (1 rules)"), "{before}");

        set_status(&s, &old, "archived").await;
        let after = text(&s.select_book(Parameters(select(false))).await.unwrap());
        assert!(!after.contains("Injected Context"), "{after}");
        // The TOC select_book prints hides the archived subtree like `toc`.
        assert!(
            !after.contains("Old rules") && !after.contains("Rule"),
            "{after}"
        );
        assert!(
            after.contains("… 2 archived nodes hidden (include_archived: true to show)"),
            "{after}"
        );

        let r = s
            .book_stats(Parameters(crate::request::McpBookStatsRequest {
                slug: None,
                format: Some("json".into()),
            }))
            .await
            .unwrap();
        assert_eq!(r.structured_content.expect("json")["injected"], 0);
    }

    #[tokio::test]
    async fn toc_counts_only_hidden_nodes_the_view_would_have_shown() {
        let s = server_with_book().await;
        create(&s, None, "Live", &[]).await;
        let old = create(&s, None, "Old", &[]).await;
        for i in 0..5 {
            create(&s, Some(&old), &format!("Under {i}"), &[]).await;
        }
        set_status(&s, &old, "archived").await;

        let mut req = toc(false);
        req.max_depth = Some(1);
        let t = text(&s.toc(Parameters(req)).await.unwrap());
        // Only "Old" itself is within depth 1; its five children would not
        // have been shown at that depth either.
        assert!(
            t.contains("… 1 archived nodes hidden (include_archived: true to show)"),
            "{t}"
        );

        let mut req = toc(false);
        req.limit = Some(1);
        let t = text(&s.toc(Parameters(req)).await.unwrap());
        // The first node in toc order is "Live", which is shown.
        assert!(!t.contains("archived nodes hidden"), "{t}");
    }

    #[tokio::test]
    async fn toc_of_an_archived_subtree_root_shows_it() {
        let s = server_with_book().await;
        let old = create(&s, None, "Old", &[]).await;
        let under = create(&s, Some(&old), "Under old", &[]).await;
        set_status(&s, &old, "archived").await;

        for root in [&old, &under] {
            let mut req = toc(false);
            req.subtree_root = Some(root.clone());
            let t = text(&s.toc(Parameters(req)).await.unwrap());
            assert!(t.contains("Under old"), "{root}: {t}");
            assert!(!t.contains("No matching nodes"), "{root}: {t}");
            assert!(!t.contains("archived nodes hidden"), "{root}: {t}");
        }
    }

    #[tokio::test]
    async fn checklist_leaves_archived_subtrees_out() {
        let s = server_with_book().await;
        create(&s, None, "Live", &[]).await;
        let old = create(&s, None, "Old", &[]).await;
        create(&s, Some(&old), "Under old", &[]).await;
        set_status(&s, &old, "archived").await;

        let dir = std::env::temp_dir().join(format!("outline-checklist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let export = |name: &str, include_archived| crate::request::McpEjectRequest {
            output_dir: Some(dir.display().to_string()),
            filename: Some(name.into()),
            include_placeholders: None,
            format: None,
            subtree_root: None,
            include_archived,
        };
        s.checklist(Parameters(export("default.md", false)))
            .await
            .unwrap();
        s.checklist(Parameters(export("all.md", true)))
            .await
            .unwrap();
        let default = std::fs::read_to_string(dir.join("default.md")).unwrap();
        let all = std::fs::read_to_string(dir.join("all.md")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(default.contains("Live"), "{default}");
        assert!(
            !default.contains("Old") && !default.contains("Under old"),
            "{default}"
        );
        assert!(all.contains("Old") && all.contains("Under old"), "{all}");
        // The stored book is unchanged.
        let t = text(&s.toc(Parameters(toc(true))).await.unwrap());
        assert!(t.contains("Old [archived]"), "{t}");
    }
}

#[cfg(test)]
mod link_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::server::Storage;

    async fn server() -> OutlineMcpServer {
        let s = OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ));
        s.service_for("l")
            .await
            .unwrap()
            .create_book("L", 4)
            .await
            .unwrap();
        *s.selected.write().unwrap() = Some("l".into());
        s
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn node(s: &OutlineMcpServer, parent: Option<&str>, title: &str) {
        s.node_create(Parameters(McpNodeCreateRequest {
            parent: parent.map(str::to_string),
            title: title.into(),
            node_type: "section".into(),
            body: None,
            placeholder: None,
            position: None,
            properties: None,
        }))
        .await
        .expect("create");
    }

    async fn link(
        s: &OutlineMcpServer,
        from: &str,
        to: &str,
        rel: &str,
    ) -> Result<String, McpError> {
        s.link_add(Parameters(McpLinkAddRequest {
            from: from.into(),
            to: to.into(),
            rel: rel.into(),
        }))
        .await
        .map(|r| text(&r))
    }

    async fn list(s: &OutlineMcpServer, id: &str, direction: Option<&str>) -> String {
        text(
            &s.links(Parameters(McpLinksRequest {
                node_id: id.into(),
                direction: direction.map(str::to_string),
                rel: None,
            }))
            .await
            .expect("links"),
        )
    }

    async fn history_len(s: &OutlineMcpServer) -> usize {
        s.service().await.unwrap().history().await.unwrap().len()
    }

    #[tokio::test]
    async fn add_list_and_remove_links_both_directions() {
        let s = server().await;
        node(&s, None, "A").await;
        node(&s, None, "B").await;
        node(&s, None, "C").await;

        let t = link(&s, "1", "2", "Supersedes").await.unwrap();
        assert!(
            t.starts_with("Linked: 1. A") && t.contains("—supersedes→ 2. B"),
            "{t}"
        );
        link(&s, "3", "2", "see_also").await.unwrap();
        let h = history_len(&s).await;
        let t = link(&s, "1", "2", "supersedes").await.unwrap();
        assert!(t.starts_with("Already linked"), "{t}");
        assert_eq!(history_len(&s).await, h, "duplicate committed nothing");

        let t = list(&s, "2", Some("in")).await;
        assert!(t.contains("## Incoming (2)"), "{t}");
        assert!(
            t.contains("supersedes ← 1. A") && t.contains("see_also ← 3. C"),
            "{t}"
        );
        assert!(!t.contains("Outgoing"), "{t}");
        let t = list(&s, "1", None).await;
        assert!(
            t.contains("## Outgoing (1)") && t.contains("## Incoming (0)"),
            "{t}"
        );

        let r = s
            .link_remove(Parameters(McpLinkRemoveRequest {
                from: "3".into(),
                to: "2".into(),
                rel: None,
            }))
            .await
            .unwrap();
        assert!(text(&r).starts_with("Removed 1 link(s)"), "{}", text(&r));
        assert!(list(&s, "2", Some("in")).await.contains("## Incoming (1)"));
    }

    #[tokio::test]
    async fn self_links_bad_types_and_bad_direction_are_refused() {
        let s = server().await;
        node(&s, None, "A").await;
        node(&s, None, "B").await;
        let h = history_len(&s).await;
        assert!(link(&s, "1", "1", "x").await.is_err());
        let e = link(&s, "1", "2", "see also").await.unwrap_err();
        assert!(e.message.contains("invalid link type"), "{}", e.message);
        assert_eq!(history_len(&s).await, h, "nothing committed");
        let e = s
            .links(Parameters(McpLinksRequest {
                node_id: "1".into(),
                direction: Some("sideways".into()),
                rel: None,
            }))
            .await
            .unwrap_err();
        assert!(e.message.contains("direction"), "{}", e.message);
    }

    #[tokio::test]
    async fn moving_the_target_keeps_backlinks() {
        let s = server().await;
        node(&s, None, "A").await;
        node(&s, None, "Holder").await;
        node(&s, None, "B").await;
        link(&s, "1", "3", "see_also").await.unwrap();
        s.node_move(Parameters(McpNodeMoveRequest {
            node_id: "3".into(),
            action: "move".into(),
            new_parent: Some("2".into()),
            position: None,
            dry_run: false,
        }))
        .await
        .unwrap();
        let t = list(&s, "2-1", Some("in")).await;
        assert!(
            t.contains("## Incoming (1)") && t.contains("see_also ← 1. A"),
            "{t}"
        );
    }

    #[tokio::test]
    async fn removing_the_target_reports_and_drops_incoming_links() {
        let s = server().await;
        node(&s, None, "A").await;
        node(&s, None, "B").await;
        node(&s, Some("2"), "B child").await;
        node(&s, None, "C").await;
        link(&s, "1", "2", "supersedes").await.unwrap();
        link(&s, "3", "2-1", "see_also").await.unwrap();
        link(&s, "1", "3", "see_also").await.unwrap();

        let dry = |dry_run| McpNodeMoveRequest {
            node_id: "2".into(),
            action: "remove".into(),
            new_parent: None,
            position: None,
            dry_run,
        };
        let t = text(&s.node_move(Parameters(dry(true))).await.unwrap());
        assert!(t.contains("Would drop 2 link(s)"), "{t}");
        let t = text(&s.node_move(Parameters(dry(false))).await.unwrap());
        let mut lines = t.lines();
        let first = lines.next().unwrap();
        assert!(
            first.starts_with("Removed: 2. B") && first.ends_with(')'),
            "{t}"
        );
        assert!(
            lines
                .next()
                .unwrap()
                .starts_with("Dropped 2 link(s) pointing at the removed nodes, from: A, C"),
            "{t}"
        );
        // A keeps its link to C (now 2), C keeps nothing.
        let t = list(&s, "1", Some("out")).await;
        assert!(
            t.contains("## Outgoing (1)") && t.contains("see_also → 2. C"),
            "{t}"
        );
    }

    #[tokio::test]
    async fn node_query_filters_by_link_and_reports_links_in_json() {
        let s = server().await;
        node(&s, None, "Old").await;
        node(&s, None, "New").await;
        node(&s, None, "Other").await;
        link(&s, "2", "1", "supersedes").await.unwrap();
        link(&s, "3", "1", "see_also").await.unwrap();

        let q = |rel: Option<&str>, format: Option<&str>| McpNodeQueryRequest {
            filter: None,
            include_body: false,
            kind: None,
            status: None,
            include_archived: false,
            verified_before: None,
            subtree_root: None,
            text: None,
            linked_to: Some("1".into()),
            link_rel: rel.map(str::to_string),
            format: format.map(str::to_string),
        };
        let t = text(&s.node_query(Parameters(q(None, None))).await.unwrap());
        assert!(t.contains("(2 matches)"), "{t}");
        assert!(t.contains("Links: supersedes → Old"), "{t}");
        let t = text(
            &s.node_query(Parameters(q(Some("supersedes"), None)))
                .await
                .unwrap(),
        );
        assert!(t.contains("(1 matches)") && t.contains("New"), "{t}");

        let mut all = q(None, Some("json"));
        all.linked_to = None;
        let r = s.node_query(Parameters(all)).await.unwrap();
        let v = r.structured_content.expect("json");
        let old = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == "Old")
            .unwrap();
        assert_eq!(old["links"].as_array().unwrap().len(), 0);
        let back = old["backlinks"].as_array().unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0]["rel"], "supersedes");
        assert_eq!(back[0]["source_title"], "New");
    }

    fn query_req(linked_to: Option<&str>, link_rel: Option<&str>) -> McpNodeQueryRequest {
        McpNodeQueryRequest {
            filter: None,
            include_body: false,
            kind: None,
            status: None,
            include_archived: true,
            verified_before: None,
            subtree_root: None,
            text: None,
            linked_to: linked_to.map(str::to_string),
            link_rel: link_rel.map(str::to_string),
            format: None,
        }
    }

    #[tokio::test]
    async fn node_query_rejects_an_invalid_link_rel() {
        let s = server().await;
        node(&s, None, "A").await;
        let e = s
            .node_query(Parameters(query_req(Some("1"), Some("see also"))))
            .await
            .unwrap_err();
        assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS, "{e:?}");
        assert!(e.message.contains("link_rel"), "{}", e.message);
    }

    #[tokio::test]
    async fn link_endpoints_report_archived_status() {
        let s = server().await;
        node(&s, None, "Old").await;
        node(&s, None, "New").await;
        node(&s, None, "Retired source").await;
        link(&s, "2", "1", "supersedes").await.unwrap();
        link(&s, "3", "2", "see_also").await.unwrap();
        for id in ["1", "3"] {
            s.node_update(Parameters(McpNodeUpdateRequest {
                node_id: id.into(),
                title: None,
                body: None,
                node_type: None,
                placeholder: None,
                properties: None,
                properties_patch: None,
                status: Some("archived".into()),
            }))
            .await
            .expect("archive");
        }

        // Text: New's outgoing link points at an archived node.
        let t = text(
            &s.node_query(Parameters(query_req(Some("1"), None)))
                .await
                .unwrap(),
        );
        assert!(t.contains("Links: supersedes → Old [archived]"), "{t}");

        // JSON: target_status on New's link, source_status on New's backlink.
        let mut q = query_req(None, None);
        q.format = Some("json".into());
        let v = s
            .node_query(Parameters(q))
            .await
            .unwrap()
            .structured_content
            .expect("json");
        let new = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == "New")
            .unwrap();
        assert_eq!(new["links"][0]["target_status"], "archived", "{new}");
        assert_eq!(new["backlinks"][0]["source_status"], "archived", "{new}");
        let old = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == "Old")
            .unwrap();
        assert_eq!(old["backlinks"][0]["source_status"], "active", "{old}");
    }

    #[tokio::test]
    async fn link_remove_clears_a_dangling_target_by_full_uuid() {
        let s = server().await;
        node(&s, None, "A").await;
        let svc = s.service().await.unwrap();
        let book = svc.read_tree().await.unwrap();
        let a = book.root_nodes()[0];
        let gone = outline_mcp_core::domain::model::id::NodeId::new().to_string();

        // A link whose target is not in the book, as restored or imported
        // older data can carry: built through the stored JSON shape.
        let mut v = serde_json::to_value(&book).unwrap();
        let node = &mut v["nodes"][a.to_string()];
        node["links"] = serde_json::json!([{ "rel": "see_also", "target": gone }]);
        let dangling: outline_mcp_core::domain::model::book::TemplateBook =
            serde_json::from_value(v).unwrap();
        assert_eq!(dangling.dangling_links().len(), 1);
        svc.import_book(&dangling).await.unwrap();

        let r = s
            .link_remove(Parameters(McpLinkRemoveRequest {
                from: "1".into(),
                to: gone.clone(),
                rel: None,
            }))
            .await
            .expect("remove dangling");
        assert!(text(&r).starts_with("Removed 1 link(s)"), "{}", text(&r));
        let book = svc.read_tree().await.unwrap();
        assert!(book.dangling_links().is_empty());
    }
}

#[cfg(test)]
mod revision_tool_tests {
    use std::sync::Arc;

    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    use super::*;
    use crate::request::McpSelectBookRequest;
    use crate::server::Storage;
    use outline_mcp_core::domain::model::book::TemplateBook;
    use outline_mcp_core::domain::model::node::NodeType;

    async fn server_with_rule() -> (OutlineMcpServer, String) {
        let s = OutlineMcpServer::new(Storage::new(
            Arc::new(MemoryBookStore::new()),
            Arc::new(MemorySnapshotStore::new()),
        ));
        s.service_for("r")
            .await
            .unwrap()
            .create_book("R", 4)
            .await
            .unwrap();
        *s.selected.write().unwrap() = Some("r".into());
        let r = s
            .node_create(Parameters(McpNodeCreateRequest {
                parent: None,
                title: "Rule".into(),
                node_type: "content".into(),
                body: Some("rule v1".into()),
                placeholder: None,
                position: None,
                properties: Some([("inject".to_string(), "true".to_string())].into()),
            }))
            .await
            .unwrap();
        assert!(text(&r).starts_with("Created: 1."));
        (s, "1".to_string())
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn propose(node: &str, body: &str) -> McpRevisionProposeRequest {
        McpRevisionProposeRequest {
            node_id: node.into(),
            title: None,
            body: Some(body.into()),
            clear_body: false,
            properties: None,
            properties_patch: None,
            by: Some("alice".into()),
            note: Some("clearer wording".into()),
            replace: false,
        }
    }

    async fn injected(s: &OutlineMcpServer) -> String {
        text(
            &s.select_book(Parameters(McpSelectBookRequest {
                book: "r".into(),
                quiet: true,
            }))
            .await
            .unwrap(),
        )
    }

    async fn history_len(s: &OutlineMcpServer) -> usize {
        s.service().await.unwrap().history().await.unwrap().len()
    }

    #[tokio::test]
    async fn proposal_leaves_injection_alone_until_approved() {
        let (s, id) = server_with_rule().await;
        assert!(injected(&s).await.contains("rule v1"));

        let r = s
            .revision_propose(Parameters(propose(&id, "rule v2")))
            .await
            .unwrap();
        assert!(text(&r).contains("Proposed a revision"), "{}", text(&r));
        let inj = injected(&s).await;
        assert!(inj.contains("rule v1") && !inj.contains("rule v2"), "{inj}");

        let show = text(
            &s.revision_show(Parameters(McpRevisionShowRequest {
                node_id: id.clone(),
            }))
            .await
            .unwrap(),
        );
        assert!(show.contains("by alice") && show.contains("Note: clearer wording"));
        assert!(
            show.contains("-rule v1") && show.contains("+rule v2"),
            "{show}"
        );
        assert!(!show.contains("WARNING"), "{show}");

        let h0 = history_len(&s).await;
        let r = s
            .revision_approve(Parameters(McpRevisionApproveRequest {
                node_id: id.clone(),
                by: Some("bob".into()),
                force: false,
            }))
            .await
            .unwrap();
        assert!(
            text(&r).starts_with("Approved by bob") && text(&r).contains("proposed by alice"),
            "{}",
            text(&r)
        );
        assert_eq!(history_len(&s).await, h0 + 1, "approve is one entry");
        let inj = injected(&s).await;
        assert!(inj.contains("rule v2") && !inj.contains("rule v1"), "{inj}");
        let pending = text(
            &s.revisions_pending(Parameters(McpRevisionsPendingRequest {
                subtree_root: None,
            }))
            .await
            .unwrap(),
        );
        assert!(pending.starts_with("# Pending revisions (0)"), "{pending}");
    }

    #[tokio::test]
    async fn stale_proposal_is_refused_then_forced() {
        let (s, id) = server_with_rule().await;
        s.revision_propose(Parameters(propose(&id, "rule v2")))
            .await
            .unwrap();
        s.node_update(Parameters(McpNodeUpdateRequest {
            node_id: id.clone(),
            title: Some("Renamed".into()),
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: None,
            status: None,
        }))
        .await
        .unwrap();

        let pending = text(
            &s.revisions_pending(Parameters(McpRevisionsPendingRequest {
                subtree_root: None,
            }))
            .await
            .unwrap(),
        );
        assert!(pending.contains("[stale"), "{pending}");
        let e = s
            .revision_approve(Parameters(McpRevisionApproveRequest {
                node_id: id.clone(),
                by: None,
                force: false,
            }))
            .await
            .unwrap_err();
        assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(e.message.contains("changed after"), "{e:?}");
        let show = text(
            &s.revision_show(Parameters(McpRevisionShowRequest {
                node_id: id.clone(),
            }))
            .await
            .unwrap(),
        );
        assert!(show.contains("WARNING"), "{show}");
        s.revision_approve(Parameters(McpRevisionApproveRequest {
            node_id: id.clone(),
            by: None,
            force: true,
        }))
        .await
        .unwrap();
        let inj = injected(&s).await;
        assert!(inj.contains("rule v2"), "{inj}");
    }

    #[tokio::test]
    async fn reject_and_input_errors() {
        let (s, id) = server_with_rule().await;
        let mut both = propose(&id, "x");
        both.clear_body = true;
        assert!(s.revision_propose(Parameters(both)).await.is_err());
        let e = s
            .revision_propose(Parameters(propose(&id, "rule v1")))
            .await
            .unwrap_err();
        assert!(e.message.contains("change nothing"), "{e:?}");

        s.revision_propose(Parameters(propose(&id, "rule v2")))
            .await
            .unwrap();
        let e = s
            .revision_propose(Parameters(propose(&id, "rule v3")))
            .await
            .unwrap_err();
        assert!(
            e.message.contains("already has a pending revision"),
            "{e:?}"
        );

        // node_query shows the summary in text and JSON.
        let q = McpNodeQueryRequest {
            filter: None,
            include_body: false,
            kind: None,
            status: None,
            include_archived: false,
            verified_before: None,
            subtree_root: None,
            format: Some("json".into()),
            text: None,
            linked_to: None,
            link_rel: None,
        };
        let r = s.node_query(Parameters(q.clone())).await.unwrap();
        let v = r.structured_content.unwrap();
        let p = &v["nodes"][0]["proposal"];
        assert_eq!(p["by"], "alice");
        assert_eq!(p["fields"][0], "body");
        assert_eq!(p["stale"], false);
        let mut q_text = q;
        q_text.format = None;
        let t = text(&s.node_query(Parameters(q_text)).await.unwrap());
        assert!(t.contains("Proposal: pending by alice (body)"), "{t}");

        let r = s
            .revision_reject(Parameters(McpRevisionRejectRequest {
                node_id: id.clone(),
                by: Some("bob".into()),
            }))
            .await
            .unwrap();
        assert!(text(&r).starts_with("Rejected by bob"), "{}", text(&r));
        assert!(injected(&s).await.contains("rule v1"));
        assert!(s
            .revision_show(Parameters(McpRevisionShowRequest { node_id: id }))
            .await
            .is_err());
    }

    #[test]
    fn diff_lists_property_changes() {
        let mut book = TemplateBook::new("B", 4);
        let id = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "T".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: [
                    ("keep".to_string(), "1".to_string()),
                    ("old".to_string(), "x".to_string()),
                    ("scope".to_string(), "a".to_string()),
                ]
                .into(),
            })
            .unwrap();
        let rev = Revision {
            title: Some("T2".into()),
            body: None,
            properties: Some(
                [
                    ("keep".to_string(), "1".to_string()),
                    ("scope".to_string(), "b".to_string()),
                    ("new".to_string(), "y".to_string()),
                ]
                .into(),
            ),
            properties_patch: None,
            base: String::new(),
            by: None,
            note: None,
            at: Timestamp::now(),
        };
        let d = revision_diff(book.get_node(id).unwrap(), &rev);
        assert!(d.contains("Title: \"T\" → \"T2\""), "{d}");
        assert!(d.contains("+ new = \"y\""), "{d}");
        assert!(d.contains("~ scope: \"a\" → \"b\""), "{d}");
        assert!(d.contains("- old (was \"x\")"), "{d}");
        assert!(!d.contains("keep"), "{d}");
    }

    #[tokio::test]
    async fn readers_never_show_the_proposed_content() {
        let (s, id) = server_with_rule().await;
        let mut req = propose(&id, "PROPOSED BODY");
        req.title = Some("PROPOSED TITLE".into());
        req.properties_patch = Some([("routing".to_string(), "PROPOSED SCENE".to_string())].into());
        s.revision_propose(Parameters(req)).await.unwrap();

        let toc_text = text(
            &s.toc(Parameters(McpTocRequest {
                subtree_root: None,
                filter: None,
                max_depth: None,
                limit: None,
                include_archived: true,
            }))
            .await
            .unwrap(),
        );
        assert!(!toc_text.contains("PROPOSED"), "{toc_text}");

        let routing = text(
            &s.gen_routing(Parameters(McpGenRoutingRequest {}))
                .await
                .unwrap(),
        );
        assert!(!routing.contains("PROPOSED"), "{routing}");

        let dir = std::env::temp_dir().join(format!("outline-rev-readers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        s.checklist(Parameters(crate::request::McpEjectRequest {
            output_dir: Some(dir.display().to_string()),
            filename: Some("checklist.md".into()),
            include_placeholders: None,
            format: None,
            subtree_root: None,
            include_archived: true,
        }))
        .await
        .unwrap();
        let dump_dir = dir.join("dump");
        s.dump(Parameters(McpDumpRequest {
            output_dir: dump_dir.display().to_string(),
            format: Some("markdown".into()),
            filename: Some("dump.md".into()),
        }))
        .await
        .unwrap();
        let checklist = std::fs::read_to_string(dir.join("checklist.md")).unwrap();
        let dumped: String = std::fs::read_dir(&dump_dir)
            .unwrap()
            .filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            checklist.contains("rule v1") && !checklist.contains("PROPOSED"),
            "{checklist}"
        );
        assert!(
            dumped.contains("rule v1") && !dumped.contains("PROPOSED"),
            "{dumped}"
        );
    }

    #[tokio::test]
    async fn pending_list_scopes_to_a_subtree() {
        let (s, id) = server_with_rule().await;
        let sec = s
            .node_create(Parameters(McpNodeCreateRequest {
                parent: None,
                title: "Sec".into(),
                node_type: "section".into(),
                body: None,
                placeholder: None,
                position: None,
                properties: None,
            }))
            .await
            .unwrap();
        assert!(text(&sec).starts_with("Created: 2."), "{}", text(&sec));
        s.node_create(Parameters(McpNodeCreateRequest {
            parent: Some("2".into()),
            title: "Child".into(),
            node_type: "content".into(),
            body: Some("child v1".into()),
            placeholder: None,
            position: None,
            properties: None,
        }))
        .await
        .unwrap();
        s.revision_propose(Parameters(propose(&id, "rule v2")))
            .await
            .unwrap();
        s.revision_propose(Parameters(propose("2-1", "child v2")))
            .await
            .unwrap();

        let pending = |root: Option<&str>| McpRevisionsPendingRequest {
            subtree_root: root.map(str::to_string),
        };
        let all = text(
            &s.revisions_pending(Parameters(pending(None)))
                .await
                .unwrap(),
        );
        assert!(all.starts_with("# Pending revisions (2)"), "{all}");
        let scoped = text(
            &s.revisions_pending(Parameters(pending(Some("2"))))
                .await
                .unwrap(),
        );
        assert!(scoped.starts_with("# Pending revisions (1)"), "{scoped}");
        assert!(
            scoped.contains("Child") && !scoped.contains("Rule"),
            "{scoped}"
        );
    }

    #[tokio::test]
    async fn clear_body_proposal_removes_the_body_on_approve() {
        let (s, id) = server_with_rule().await;
        let mut req = propose(&id, "unused");
        req.body = None;
        req.clear_body = true;
        s.revision_propose(Parameters(req)).await.unwrap();
        s.revision_approve(Parameters(McpRevisionApproveRequest {
            node_id: id.clone(),
            by: None,
            force: false,
        }))
        .await
        .unwrap();
        let book = s.service().await.unwrap().read_tree().await.unwrap();
        let node = book.root_nodes()[0];
        assert_eq!(book.get_node(node).unwrap().body(), None);
    }

    #[tokio::test]
    async fn empty_body_proposal_on_a_bodyless_node_changes_nothing() {
        let (s, _) = server_with_rule().await;
        s.node_create(Parameters(McpNodeCreateRequest {
            parent: None,
            title: "Bare".into(),
            node_type: "content".into(),
            body: None,
            placeholder: None,
            position: None,
            properties: None,
        }))
        .await
        .unwrap();
        let e = s
            .revision_propose(Parameters(propose("2", "")))
            .await
            .unwrap_err();
        assert!(e.message.contains("change nothing"), "{e:?}");
    }

    #[tokio::test]
    async fn proposal_on_a_node_archived_later_is_linted_and_can_be_rejected() {
        let (s, id) = server_with_rule().await;
        s.revision_propose(Parameters(propose(&id, "rule v2")))
            .await
            .unwrap();
        s.node_update(Parameters(McpNodeUpdateRequest {
            node_id: id.clone(),
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: None,
            status: Some("archived".into()),
        }))
        .await
        .unwrap();

        let lint = text(
            &s.book_lint(Parameters(McpBookLintRequest {
                slug: None,
                checks: Some(vec!["stale_proposals".into()]),
                threshold: None,
                limit: None,
                stale_days: None,
                format: None,
            }))
            .await
            .unwrap(),
        );
        assert!(lint.contains("on an archived node"), "{lint}");
        let pending = text(
            &s.revisions_pending(Parameters(McpRevisionsPendingRequest {
                subtree_root: None,
            }))
            .await
            .unwrap(),
        );
        assert!(pending.starts_with("# Pending revisions (1)"), "{pending}");

        let e = s
            .revision_approve(Parameters(McpRevisionApproveRequest {
                node_id: id.clone(),
                by: None,
                force: true,
            }))
            .await
            .unwrap_err();
        assert_eq!(e.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        s.revision_reject(Parameters(McpRevisionRejectRequest {
            node_id: id,
            by: None,
        }))
        .await
        .unwrap();
        let pending = text(
            &s.revisions_pending(Parameters(McpRevisionsPendingRequest {
                subtree_root: None,
            }))
            .await
            .unwrap(),
        );
        assert!(pending.starts_with("# Pending revisions (0)"), "{pending}");
    }
}
