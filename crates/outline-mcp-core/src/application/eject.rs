use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::domain::model::book::TemplateBook;
use crate::domain::model::changelog::NodeStatus;
use crate::domain::model::id::{BookId, NodeId};
use crate::domain::model::node::{Link, NodeParts, NodeType, Revision, TemplateNode};
use crate::domain::model::timestamp::Timestamp;

use super::error::AppError;

/// Eject出力フォーマット
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectFormat {
    /// Checkbox-style Markdown.
    Markdown,
    /// Tree-structured JSON (see `EjectTree`).
    Json,
}

/// Eject設定
pub struct EjectConfig {
    /// Directory the file will be written into.
    pub output_dir: std::path::PathBuf,
    /// Output file name.
    pub filename: String,
    /// Whether to include placeholder hints as fill-in fields.
    pub include_placeholders: bool,
    /// Output format (Markdown or JSON).
    pub format: EjectFormat,
    /// 部分木のルート（Noneなら全体）
    pub subtree_root: Option<NodeId>,
}

/// JSON Eject用のツリー構造DTO
///
/// Every field added after 0.14 is optional and left out when empty (an
/// active node has no `status`, a never-verified node no `verified_at`, …), so
/// files written by earlier versions still parse and import.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EjectTreeNode {
    /// Node ID (as a UUID string). `import` keeps it unless asked for new ids.
    pub id: String,
    /// Node title.
    pub title: String,
    /// Node type as a string (`"section"` or `"content"`).
    pub node_type: String,
    /// Optional markdown body content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Optional placeholder hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Child nodes, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<EjectTreeNode>,
    /// Key-value properties.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub properties: HashMap<String, String>,
    /// Lifecycle status; absent means active.
    #[serde(default, skip_serializing_if = "is_active")]
    pub status: NodeStatus,
    /// When the node's content was last edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
    /// When someone last confirmed the node (`node_verify`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<Timestamp>,
    /// Who confirmed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<String>,
    /// Outgoing typed links; `target` is the target node's `id` in this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<EjectLink>,
    /// A pending revision waiting for approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Revision>,
}

/// A typed link in an exported tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EjectLink {
    /// Link type (`supersedes`, `see_also`, …).
    pub rel: String,
    /// The target node's `id` (a UUID string, as exported).
    pub target: String,
}

fn is_active(status: &NodeStatus) -> bool {
    *status == NodeStatus::Active
}

/// The key an exported id is matched by: the canonical form of a UUID (so
/// `ABC…`, `{abc…}` and `urn:uuid:abc…` are the same id), else the trimmed
/// text (ids of files imported with `new_ids`, which need not be UUIDs).
fn id_key(raw: &str) -> String {
    let trimmed = raw.trim();
    uuid::Uuid::parse_str(trimmed)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| trimmed.to_string())
}

/// State threaded through the recursive first pass of an import.
struct ImportPass<'a> {
    max_depth: u8,
    opts: ImportOptions,
    current: Option<&'a TemplateBook>,
    /// Exported id (by [`id_key`]) → id in the imported book.
    keys: HashMap<String, NodeId>,
    /// Ids already assigned, registered before a node's children.
    seen: HashSet<NodeId>,
    nodes: HashMap<NodeId, TemplateNode>,
    pending_links: Vec<(NodeId, &'a EjectLink)>,
    pending_proposals: Vec<(NodeId, Revision)>,
    /// Nodes the file had no `updated_at` for that took the stamp of the
    /// same-id node in `current`; settled once the node is complete.
    inherited_stamps: Vec<NodeId>,
}

/// How `import` assigns node ids.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// `false` (default): keep each node's exported id, which must be a UUID
    /// and unique within the file, so a re-import of the same book updates
    /// it in place. `true`: give every node a new id (links between imported
    /// nodes are remapped), for importing a copy next to the original.
    pub new_ids: bool,
}

/// A link that `import` could not keep because its target is not in the
/// imported file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedLink {
    /// The node (id as imported) that carried the link.
    pub from: NodeId,
    /// The link type.
    pub rel: String,
    /// The target id as written in the file.
    pub target: String,
}

/// A pending revision that `import` could not keep because it breaks the
/// rules `revision_propose` enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedProposal {
    /// The node (id as imported) that carried the revision.
    pub node: NodeId,
    /// Why it was dropped.
    pub reason: String,
}

/// The result of an import: the book, plus links and pending revisions that
/// had to be dropped.
#[derive(Debug, Clone)]
pub struct ImportReport {
    /// The imported book.
    pub book: TemplateBook,
    /// Links whose target is not in the file, in file order.
    pub dropped_links: Vec<DroppedLink>,
    /// Pending revisions that break the proposal rules, in file order.
    pub dropped_proposals: Vec<DroppedProposal>,
}

/// The section an export was taken from, when it is not the whole book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EjectSubtreeRoot {
    /// The section's node id.
    pub id: String,
    /// The section's title.
    pub title: String,
}

/// JSON Eject 用のツリー全体 DTO (書籍全体 or 部分木)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EjectTree {
    /// Book (or subtree root) title.
    pub title: String,
    /// Configured maximum tree depth.
    pub max_depth: u8,
    /// Set when the file holds one section's children rather than the whole
    /// book. Importing such a file replaces the whole book with them, so the
    /// `import` tool refuses it unless forced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtree_root: Option<EjectSubtreeRoot>,
    /// Root-level nodes of the exported tree.
    pub nodes: Vec<EjectTreeNode>,
}

/// Template Book → 作業用ファイルへの変換
pub struct EjectService;

impl EjectService {
    /// Bookの内容をMarkdown文字列に変換する。
    pub fn render_markdown(
        book: &TemplateBook,
        include_placeholders: bool,
        subtree_root: Option<NodeId>,
    ) -> String {
        let mut buf = String::new();

        match subtree_root {
            Some(root_id) => {
                if let Some(node) = book.get_node(root_id) {
                    buf.push_str(&format!("# {}\n\n", node.title()));
                    for &child_id in node.children() {
                        if let Some(child) = book.get_node(child_id) {
                            Self::render_node(book, child, 0, include_placeholders, &mut buf);
                        }
                    }
                }
            }
            None => {
                buf.push_str(&format!("# {}\n\n", book.title()));
                for &root_id in book.root_nodes() {
                    if let Some(node) = book.get_node(root_id) {
                        Self::render_node(book, node, 0, include_placeholders, &mut buf);
                    }
                }
            }
        }

        buf
    }

    /// Bookの内容をJSON文字列（ツリー構造）に変換する。
    pub fn render_json(
        book: &TemplateBook,
        subtree_root: Option<NodeId>,
    ) -> Result<String, AppError> {
        let tree = Self::build_tree(book, subtree_root);
        serde_json::to_string_pretty(&tree).map_err(|e| AppError::Storage(Box::new(e)))
    }

    /// ツリー構造DTOを構築する。
    pub fn build_tree(book: &TemplateBook, subtree_root: Option<NodeId>) -> EjectTree {
        let root_ids: Vec<NodeId> = match subtree_root {
            Some(root_id) => book
                .get_node(root_id)
                .map(|n| n.children().to_vec())
                .unwrap_or_default(),
            None => book.root_nodes().to_vec(),
        };

        let title = match subtree_root {
            Some(root_id) => book
                .get_node(root_id)
                .map(|n| n.title().to_string())
                .unwrap_or_else(|| book.title().to_string()),
            None => book.title().to_string(),
        };

        let nodes = root_ids
            .iter()
            .filter_map(|id| Self::build_tree_node(book, *id))
            .collect();

        let subtree_root = subtree_root.and_then(|root_id| {
            book.get_node(root_id).map(|n| EjectSubtreeRoot {
                id: root_id.to_string(),
                title: n.title().to_string(),
            })
        });

        EjectTree {
            title,
            max_depth: book.max_depth(),
            subtree_root,
            nodes,
        }
    }

    fn build_tree_node(book: &TemplateBook, id: NodeId) -> Option<EjectTreeNode> {
        let node = book.get_node(id)?;
        let children = node
            .children()
            .iter()
            .filter_map(|cid| Self::build_tree_node(book, *cid))
            .collect();

        let node_type = match node.node_type() {
            NodeType::Section => "section",
            NodeType::Content => "content",
        };

        Some(EjectTreeNode {
            id: id.to_string(),
            title: node.title().to_string(),
            node_type: node_type.to_string(),
            body: node.body().map(|s| s.to_string()),
            placeholder: node.placeholder().map(|s| s.to_string()),
            children,
            properties: node.properties().clone(),
            status: node.status(),
            updated_at: node.updated_at(),
            verified_at: node.verified_at(),
            verified_by: node.verified_by().map(str::to_string),
            links: node
                .links()
                .iter()
                .map(|l| EjectLink {
                    rel: l.rel().to_string(),
                    target: l.target().to_string(),
                })
                .collect(),
            proposal: node.proposal().cloned(),
        })
    }

    /// EjectTree（JSON） → TemplateBook に変換する。
    /// 再帰の最大深度。max_depthとは別に、JSON構造自体のネスト爆弾を防ぐ。
    const IMPORT_MAX_RECURSION: u8 = 32;

    /// The section a file was exported from, when the file says so
    /// (`subtree_root`) or — for files written before that marker existed —
    /// when its ids give it away: a top-level node of the file is a node of
    /// `current` that has a parent there, so the file is that parent's
    /// subtree, not the whole book. Returns the section's id and title, and
    /// whether it was detected from ids (no marker). Ids that are not UUIDs
    /// cannot match and are ignored.
    pub fn exported_subtree(
        tree: &EjectTree,
        current: Option<&TemplateBook>,
    ) -> Option<(String, String, bool)> {
        if let Some(root) = &tree.subtree_root {
            return Some((root.id.clone(), root.title.clone(), false));
        }
        let current = current?;
        tree.nodes.iter().find_map(|node| {
            let uuid = uuid::Uuid::parse_str(node.id.trim()).ok()?;
            let parent = current.get_node(NodeId::from_uuid(uuid))?.parent()?;
            let section = current.get_node(parent)?;
            Some((parent.to_string(), section.title().to_string(), true))
        })
    }

    /// Convert a JSON `EjectTree` back into a `TemplateBook` with new node
    /// ids (links between imported nodes are remapped; links to nodes outside
    /// the file are dropped). Kept for callers of the pre-0.16 API; the
    /// `import` tool uses [`EjectService::import_tree_with`], which keeps ids.
    pub fn import_tree(tree: &EjectTree) -> Result<TemplateBook, AppError> {
        Self::import_tree_with(tree, ImportOptions { new_ids: true }).map(|r| r.book)
    }

    /// Convert a JSON `EjectTree` back into a `TemplateBook`, restoring each
    /// node's status, verification, links and pending revision.
    ///
    /// With `new_ids: false` every exported id must be a UUID, unique in the
    /// file; with `new_ids: true` every node gets a fresh id. Links whose
    /// target is not in the file are dropped and reported, and so are pending
    /// revisions that break the `revision_propose` rules. A node without an
    /// exported `updated_at` (files from earlier versions) is stamped now; use
    /// [`EjectService::import_tree_onto`] to keep the destination's stamp.
    pub fn import_tree_with(
        tree: &EjectTree,
        opts: ImportOptions,
    ) -> Result<ImportReport, AppError> {
        Self::import_tree_onto(tree, opts, None)
    }

    /// [`EjectService::import_tree_with`], importing over `current` (the
    /// book about to be replaced): a node the file has no `updated_at` for
    /// keeps the stamp of the same-id node in `current`, so re-importing an
    /// unchanged export written by an earlier version changes nothing.
    pub fn import_tree_onto(
        tree: &EjectTree,
        opts: ImportOptions,
        current: Option<&TemplateBook>,
    ) -> Result<ImportReport, AppError> {
        // Pass 1: assign ids, build the nodes and the parent/child structure.
        let mut pass = ImportPass {
            max_depth: tree.max_depth,
            opts,
            current,
            keys: HashMap::new(),
            seen: HashSet::new(),
            nodes: HashMap::new(),
            pending_links: Vec::new(),
            pending_proposals: Vec::new(),
            inherited_stamps: Vec::new(),
        };
        let mut roots = Vec::new();
        for node in &tree.nodes {
            roots.push(Self::import_tree_node(&mut pass, None, node, 1)?);
        }
        let ImportPass {
            keys,
            mut nodes,
            pending_links,
            pending_proposals,
            inherited_stamps,
            ..
        } = pass;

        // Pass 2: links, now that every id in the file is known.
        let mut dropped_links = Vec::new();
        for (from, link) in pending_links {
            let Some(&target) = keys.get(&id_key(&link.target)) else {
                dropped_links.push(DroppedLink {
                    from,
                    rel: link.rel.clone(),
                    target: link.target.clone(),
                });
                continue;
            };
            if target == from {
                return Err(AppError::ImportInvalid(format!(
                    "node {} links to itself",
                    link.target
                )));
            }
            let parsed = Link::new(&link.rel, target).ok_or_else(|| {
                AppError::ImportInvalid(format!(
                    "invalid link type {:?} (1-40 of a-z, 0-9, _ and -)",
                    link.rel
                ))
            })?;
            if let Some(node) = nodes.get_mut(&from) {
                node.add_link(parsed);
            }
        }

        let mut book = TemplateBook::from_parts(
            BookId::new(),
            tree.title.clone(),
            tree.max_depth,
            nodes,
            roots,
        );

        // Pass 3: pending revisions, checked against the finished tree with
        // the same rules as `revision_propose` (archived ancestors, empty or
        // no-op revisions, properties vs properties_patch).
        let mut dropped_proposals = Vec::new();
        for (id, revision) in pending_proposals {
            if let Err(e) = book.restore_proposal(id, revision) {
                dropped_proposals.push(DroppedProposal {
                    node: id,
                    reason: e.to_string(),
                });
            }
        }

        // Pass 4: a node that took the destination's stamp keeps it only if
        // it came out identical to the destination node (content, status,
        // verification, links, revision and place in the tree — what import
        // compares when it records history); otherwise it was changed by
        // this import and is stamped now.
        if let Some(current) = current {
            let now = Timestamp::now();
            for id in inherited_stamps {
                let same = match (book.get_node(id), current.get_node(id)) {
                    (Some(new), Some(old)) => {
                        serde_json::to_value(new).ok() == serde_json::to_value(old).ok()
                    }
                    _ => false,
                };
                if !same {
                    book.restamp(id, now)?;
                }
            }
        }

        Ok(ImportReport {
            book,
            dropped_links,
            dropped_proposals,
        })
    }

    fn import_tree_node<'a>(
        pass: &mut ImportPass<'a>,
        parent: Option<NodeId>,
        tree_node: &'a EjectTreeNode,
        depth: u8,
    ) -> Result<NodeId, AppError> {
        let opts = pass.opts;
        let max_depth = pass.max_depth;
        if depth > Self::IMPORT_MAX_RECURSION {
            return Err(AppError::ImportInvalidType(
                "maximum import nesting depth exceeded".to_string(),
            ));
        }

        let node_type = match tree_node.node_type.as_str() {
            "section" => NodeType::Section,
            "content" => NodeType::Content,
            // 旧フォーマット互換: checklist/reference/runnable → Content
            "checklist" | "reference" | "runnable" => NodeType::Content,
            other => return Err(AppError::ImportInvalidType(other.to_string())),
        };

        let id = if opts.new_ids {
            NodeId::new()
        } else {
            let uuid = uuid::Uuid::parse_str(tree_node.id.trim())
                .ok()
                .filter(|u| !u.is_nil())
                .ok_or_else(|| {
                    AppError::ImportInvalid(format!(
                        "node id {:?} is not a UUID; pass new_ids: true to give every node a new id",
                        tree_node.id
                    ))
                })?;
            NodeId::from_uuid(uuid)
        };
        // Registered before the children are visited, by the parsed id: the
        // same UUID spelled differently (case, braces, `urn:uuid:`) or a
        // descendant reusing an ancestor's id is a duplicate, not a second
        // node that would overwrite the first and loop the tree.
        let key = id_key(&tree_node.id);
        if pass.keys.insert(key, id).is_some() || !pass.seen.insert(id) {
            return Err(AppError::ImportInvalid(format!(
                "node id {:?} appears more than once in the file{}",
                tree_node.id,
                if opts.new_ids {
                    ""
                } else {
                    "; pass new_ids: true to give every node a new id"
                }
            )));
        }

        if depth > max_depth {
            return Err(AppError::Domain(
                crate::domain::error::DomainError::MaxDepthExceeded {
                    node_id: id,
                    max: max_depth,
                },
            ));
        }

        // Children first, so the node is built with its child ids.
        let mut children = Vec::with_capacity(tree_node.children.len());
        for child in &tree_node.children {
            children.push(Self::import_tree_node(pass, Some(id), child, depth + 1)?);
        }

        for link in &tree_node.links {
            pass.pending_links.push((id, link));
        }
        if let Some(revision) = &tree_node.proposal {
            pass.pending_proposals.push((id, revision.clone()));
        }

        let updated_at = match tree_node.updated_at {
            Some(at) => at,
            None => match pass
                .current
                .and_then(|b| b.get_node(id))
                .and_then(|n| n.updated_at())
            {
                // Tentative: kept only if the node turns out unchanged (pass 4).
                Some(at) => {
                    pass.inherited_stamps.push(id);
                    at
                }
                None => Timestamp::now(),
            },
        };

        pass.nodes.insert(
            id,
            TemplateNode::from_parts(NodeParts {
                id,
                parent,
                children,
                title: tree_node.title.clone(),
                body: tree_node.body.clone(),
                node_type,
                placeholder: tree_node.placeholder.clone(),
                properties: tree_node.properties.clone(),
                status: tree_node.status,
                updated_at: Some(updated_at),
                verified_at: tree_node.verified_at,
                verified_by: tree_node.verified_by.clone(),
                links: Vec::new(),
                proposal: None,
            }),
        );
        Ok(id)
    }

    /// ファイルに書き出す。
    pub fn eject(
        book: &TemplateBook,
        config: &EjectConfig,
    ) -> Result<std::path::PathBuf, AppError> {
        let content = match config.format {
            EjectFormat::Markdown => {
                Self::render_markdown(book, config.include_placeholders, config.subtree_root)
            }
            EjectFormat::Json => Self::render_json(book, config.subtree_root)?,
        };

        let path = config.output_dir.join(&config.filename);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::EjectIo)?;
        }

        std::fs::write(&path, content).map_err(AppError::EjectIo)?;
        Ok(path)
    }

    /// リスト行 (`- `, `* `) をチェックボックス形式に変換する。
    fn list_to_checkbox(line: &str) -> String {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            let leading = &line[..line.len() - trimmed.len()];
            format!("{leading}- [ ] {rest}")
        } else if let Some(rest) = trimmed.strip_prefix("* ") {
            let leading = &line[..line.len() - trimmed.len()];
            format!("{leading}- [ ] {rest}")
        } else {
            line.to_string()
        }
    }

    fn render_node(
        book: &TemplateBook,
        node: &TemplateNode,
        indent_level: usize,
        include_placeholders: bool,
        buf: &mut String,
    ) {
        let indent = "  ".repeat(indent_level);

        match node.node_type() {
            NodeType::Section => {
                let heading_level = (indent_level + 2).min(4);
                let hashes = "#".repeat(heading_level);
                buf.push_str(&format!("{} {}\n\n", hashes, node.title()));
            }
            NodeType::Content => {
                buf.push_str(&format!("{}- [ ] {}\n", indent, node.title()));
            }
        }

        if let Some(body) = node.body() {
            for line in body.lines() {
                let converted = Self::list_to_checkbox(line);
                buf.push_str(&format!("{indent}  {converted}\n"));
            }
        }

        if include_placeholders {
            if let Some(ph) = node.placeholder() {
                buf.push_str(&format!("{indent}  > {ph}: ___\n"));
            }
        }

        if !node.is_leaf() {
            buf.push('\n');
        }

        for &child_id in node.children() {
            if let Some(child) = book.get_node(child_id) {
                Self::render_node(book, child, indent_level + 1, include_placeholders, buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::AddNodeRequest;
    use crate::domain::model::node::NodeType;

    fn make_test_book() -> (TemplateBook, NodeId, NodeId) {
        let mut book = TemplateBook::new("Dev Runbook", 3);

        let design = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Design".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let req_id = book
            .add_node(AddNodeRequest {
                parent: Some(design),
                title: "Define requirements".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: Some("requirements list".into()),
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        book.add_node(AddNodeRequest {
            parent: Some(design),
            title: "API design".into(),
            node_type: NodeType::Content,
            body: Some("REST endpoints".into()),
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        })
        .unwrap();

        (book, design, req_id)
    }

    #[test]
    fn render_markdown_full() {
        let (book, _, _) = make_test_book();
        let md = EjectService::render_markdown(&book, true, None);

        assert!(md.contains("# Dev Runbook"));
        assert!(md.contains("## Design"));
        assert!(md.contains("- [ ] Define requirements"));
        assert!(md.contains("> requirements list: ___"));
        assert!(md.contains("- [ ] API design"));
        assert!(md.contains("REST endpoints"));
    }

    #[test]
    fn render_markdown_without_placeholders() {
        let (book, _, _) = make_test_book();
        let md = EjectService::render_markdown(&book, false, None);
        assert!(!md.contains("> requirements list"));
    }

    #[test]
    fn render_markdown_subtree() {
        let (book, design, _) = make_test_book();
        let md = EjectService::render_markdown(&book, true, Some(design));

        assert!(md.contains("# Design"));
        assert!(md.contains("- [ ] Define requirements"));
        assert!(!md.contains("# Dev Runbook"));
    }

    #[test]
    fn render_json_full() {
        let (book, _, _) = make_test_book();
        let json_str = EjectService::render_json(&book, None).unwrap();
        let tree: EjectTree = serde_json::from_str(&json_str).unwrap();

        assert_eq!(tree.title, "Dev Runbook");
        assert_eq!(tree.nodes.len(), 1);
        assert_eq!(tree.nodes[0].title, "Design");
        assert_eq!(tree.nodes[0].node_type, "section");
        assert_eq!(tree.nodes[0].children.len(), 2);
        assert_eq!(tree.nodes[0].children[0].node_type, "content");
        assert_eq!(tree.nodes[0].children[0].title, "Define requirements");
        assert_eq!(
            tree.nodes[0].children[0].placeholder,
            Some("requirements list".into())
        );
    }

    #[test]
    fn render_json_subtree() {
        let (book, design, _) = make_test_book();
        let json_str = EjectService::render_json(&book, Some(design)).unwrap();
        let tree: EjectTree = serde_json::from_str(&json_str).unwrap();

        assert_eq!(tree.title, "Design");
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.nodes[0].title, "Define requirements");
    }

    #[test]
    fn json_roundtrip_deserialize() {
        let (book, _, _) = make_test_book();
        let json_str = EjectService::render_json(&book, None).unwrap();
        let tree: EjectTree = serde_json::from_str(&json_str).unwrap();
        let re_json = serde_json::to_string_pretty(&tree).unwrap();

        assert_eq!(json_str, re_json);
    }

    #[test]
    fn import_tree_roundtrip() {
        let (book, _, _) = make_test_book();
        let tree = EjectService::build_tree(&book, None);
        let imported = EjectService::import_tree(&tree).unwrap();

        assert_eq!(imported.title(), "Dev Runbook");
        assert_eq!(imported.node_count(), 3);
        assert_eq!(imported.root_nodes().len(), 1);

        let root = imported.get_node(imported.root_nodes()[0]).unwrap();
        assert_eq!(root.title(), "Design");
        assert_eq!(root.children().len(), 2);

        let child0 = imported.get_node(root.children()[0]).unwrap();
        assert_eq!(child0.title(), "Define requirements");
        assert_eq!(child0.placeholder(), Some("requirements list"));

        let child1 = imported.get_node(root.children()[1]).unwrap();
        assert_eq!(child1.title(), "API design");
        assert_eq!(child1.body(), Some("REST endpoints"));
    }

    #[test]
    fn import_tree_invalid_type() {
        let tree = EjectTree {
            title: "Bad".into(),
            max_depth: 4,
            subtree_root: None,
            nodes: vec![EjectTreeNode {
                id: "dummy".into(),
                title: "Node".into(),
                node_type: "unknown_type".into(),
                ..Default::default()
            }],
        };

        let result = EjectService::import_tree(&tree);
        assert!(result.is_err());
    }

    /// A book with an archived node, a verified node, a link, a pending
    /// revision and properties.
    fn curated_book() -> (TemplateBook, NodeId, NodeId, NodeId) {
        use crate::domain::model::book::AddNodeRequest;
        let mut book = TemplateBook::new("Curated", 4);
        let add = |book: &mut TemplateBook, title: &str, parent: Option<NodeId>| {
            book.add_node(AddNodeRequest {
                parent,
                title: title.to_string(),
                node_type: if parent.is_none() {
                    NodeType::Section
                } else {
                    NodeType::Content
                },
                body: Some(format!("{title} body")),
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::from([("inject".to_string(), "true".to_string())]),
            })
            .unwrap()
        };
        let sec = add(&mut book, "Rules", None);
        let live = add(&mut book, "Live", Some(sec));
        let old = add(&mut book, "Old", Some(sec));
        book.update_node(
            old,
            crate::domain::model::book::UpdateNodeRequest {
                title: None,
                body: None,
                node_type: None,
                placeholder: None,
                properties: None,
                properties_patch: None,
                status: Some(NodeStatus::Archived),
            },
        )
        .unwrap();
        book.verify_node(
            live,
            Timestamp::from_millis(1_700_000_000_000),
            Some("alice".into()),
        )
        .unwrap();
        book.add_link(live, old, "supersedes").unwrap();
        book.propose_revision(
            live,
            Revision {
                title: Some("Live v2".into()),
                body: None,
                properties: None,
                properties_patch: None,
                base: String::new(),
                by: Some("bob".into()),
                note: None,
                at: Timestamp::from_millis(1_700_000_000_000),
            },
            false,
        )
        .unwrap();
        (book, sec, live, old)
    }

    #[test]
    fn export_import_round_trip_keeps_ids_and_curated_state() {
        let (book, sec, live, old) = curated_book();
        let json = EjectService::render_json(&book, None).unwrap();
        let tree: EjectTree = serde_json::from_str(&json).unwrap();
        let report = EjectService::import_tree_with(&tree, ImportOptions::default()).unwrap();
        assert!(report.dropped_links.is_empty());
        let back = report.book;

        for id in [sec, live, old] {
            let a = serde_json::to_value(book.get_node(id).unwrap()).unwrap();
            let b = serde_json::to_value(back.get_node(id).unwrap()).unwrap();
            assert_eq!(a, b, "node {id} differs after the round trip");
        }
        assert_eq!(back.root_nodes(), book.root_nodes());
        let l = back.get_node(live).unwrap();
        assert_eq!(l.verified_by(), Some("alice"));
        assert_eq!(l.links()[0].target(), old);
        assert!(l.proposal().is_some());
        assert_eq!(back.get_node(old).unwrap().status(), NodeStatus::Archived);
    }

    #[test]
    fn active_and_empty_fields_stay_out_of_the_export() {
        let (book, sec, _, _) = curated_book();
        let tree = EjectService::build_tree(&book, None);
        let json = serde_json::to_value(&tree).unwrap();
        let root = &json["nodes"][0];
        assert_eq!(root["id"], sec.to_string());
        for absent in ["status", "verified_at", "verified_by", "links", "proposal"] {
            assert!(root.get(absent).is_none(), "{absent} should be omitted");
        }
    }

    #[test]
    fn new_ids_remaps_links_between_imported_nodes() {
        let (book, _, live, old) = curated_book();
        let tree = EjectService::build_tree(&book, None);
        let back = EjectService::import_tree_with(&tree, ImportOptions { new_ids: true })
            .unwrap()
            .book;
        assert!(back.get_node(live).is_none(), "ids must be new");
        let new_live = back
            .all_nodes_dfs()
            .into_iter()
            .find(|n| n.title() == "Live")
            .unwrap();
        let new_old = back
            .all_nodes_dfs()
            .into_iter()
            .find(|n| n.title() == "Old")
            .unwrap();
        assert_ne!(new_old.id(), old);
        assert_eq!(new_live.links()[0].target(), new_old.id());
    }

    #[test]
    fn import_rejects_non_uuid_and_duplicate_ids_unless_new_ids() {
        let node = |id: &str| EjectTreeNode {
            id: id.into(),
            title: id.into(),
            node_type: "content".into(),
            ..Default::default()
        };
        let not_uuid = EjectTree {
            title: "T".into(),
            max_depth: 4,
            subtree_root: None,
            nodes: vec![node("a")],
        };
        let err = EjectService::import_tree_with(&not_uuid, ImportOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("new_ids"), "{err}");
        assert!(EjectService::import_tree_with(&not_uuid, ImportOptions { new_ids: true }).is_ok());

        let uuid = NodeId::new().to_string();
        let dup = EjectTree {
            title: "T".into(),
            max_depth: 4,
            subtree_root: None,
            nodes: vec![node(&uuid), node(&uuid)],
        };
        let err = EjectService::import_tree_with(&dup, ImportOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn links_to_nodes_outside_the_file_are_dropped_and_reported() {
        let (book, sec, live, old) = curated_book();
        // Export only the subtree under `sec` minus `old`: build, then cut.
        let mut tree = EjectService::build_tree(&book, None);
        tree.nodes[0].children.retain(|c| c.id != old.to_string());
        let report = EjectService::import_tree_with(&tree, ImportOptions::default()).unwrap();
        assert_eq!(report.dropped_links.len(), 1);
        assert_eq!(report.dropped_links[0].from, live);
        assert_eq!(report.dropped_links[0].target, old.to_string());
        assert!(report.book.get_node(live).unwrap().links().is_empty());
        assert!(report.book.get_node(sec).is_some());
    }

    #[test]
    fn self_links_and_invalid_link_types_are_refused() {
        let id = NodeId::new().to_string();
        let mut n = EjectTreeNode {
            id: id.clone(),
            title: "N".into(),
            node_type: "content".into(),
            ..Default::default()
        };
        n.links.push(EjectLink {
            rel: "see_also".into(),
            target: id.clone(),
        });
        let tree = EjectTree {
            title: "T".into(),
            max_depth: 4,
            subtree_root: None,
            nodes: vec![n.clone()],
        };
        assert!(
            EjectService::import_tree_with(&tree, ImportOptions::default())
                .unwrap_err()
                .to_string()
                .contains("itself")
        );

        let other = NodeId::new().to_string();
        n.links = vec![EjectLink {
            rel: "Not Valid!".into(),
            target: other.clone(),
        }];
        let tree = EjectTree {
            title: "T".into(),
            max_depth: 4,
            subtree_root: None,
            nodes: vec![
                n,
                EjectTreeNode {
                    id: other,
                    title: "O".into(),
                    node_type: "content".into(),
                    ..Default::default()
                },
            ],
        };
        assert!(
            EjectService::import_tree_with(&tree, ImportOptions::default())
                .unwrap_err()
                .to_string()
                .contains("invalid link type")
        );
    }

    #[test]
    fn a_014_export_without_the_new_fields_imports_with_its_ids() {
        let a = NodeId::new();
        let b = NodeId::new();
        let json = format!(
            r#"{{"title":"Old","max_depth":4,"nodes":[{{"id":"{a}","title":"A","node_type":"section","children":[{{"id":"{b}","title":"B","node_type":"content","body":"x","properties":{{"inject":"true"}}}}]}}]}}"#
        );
        let tree: EjectTree = serde_json::from_str(&json).unwrap();
        let book = EjectService::import_tree_with(&tree, ImportOptions::default())
            .unwrap()
            .book;
        let nb = book.get_node(b).unwrap();
        assert_eq!(nb.parent(), Some(a));
        assert_eq!(nb.status(), NodeStatus::Active);
        assert!(nb.updated_at().is_some(), "stamped on import");
        assert_eq!(nb.get_property("inject"), Some("true"));
    }

    #[test]
    fn import_enforces_max_depth() {
        let leaf = EjectTreeNode {
            id: NodeId::new().to_string(),
            title: "Leaf".into(),
            node_type: "content".into(),
            ..Default::default()
        };
        let tree = EjectTree {
            title: "T".into(),
            max_depth: 1,
            subtree_root: None,
            nodes: vec![EjectTreeNode {
                id: NodeId::new().to_string(),
                title: "S".into(),
                node_type: "section".into(),
                children: vec![leaf],
                ..Default::default()
            }],
        };
        assert!(EjectService::import_tree_with(&tree, ImportOptions::default()).is_err());
    }

    fn content(id: &str, title: &str) -> EjectTreeNode {
        EjectTreeNode {
            id: id.into(),
            title: title.into(),
            node_type: "content".into(),
            ..Default::default()
        }
    }

    fn tree_of(nodes: Vec<EjectTreeNode>) -> EjectTree {
        EjectTree {
            title: "T".into(),
            max_depth: 4,
            subtree_root: None,
            nodes,
        }
    }

    #[test]
    fn the_same_uuid_spelled_differently_is_a_duplicate() {
        let uuid = uuid::Uuid::new_v4();
        let canonical = uuid.to_string();
        for other in [
            canonical.to_uppercase(),
            format!("{{{canonical}}}"),
            format!("urn:uuid:{canonical}"),
            uuid.simple().to_string(),
        ] {
            // Siblings.
            let err = EjectService::import_tree_with(
                &tree_of(vec![content(&canonical, "A"), content(&other, "B")]),
                ImportOptions::default(),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("more than once"), "{other}: {err}");

            // A descendant reusing its ancestor's id must not loop the tree.
            let mut parent = content(&canonical, "P");
            parent.node_type = "section".into();
            parent.children = vec![content(&other, "C")];
            let err =
                EjectService::import_tree_with(&tree_of(vec![parent]), ImportOptions::default())
                    .unwrap_err()
                    .to_string();
            assert!(
                err.contains("more than once"),
                "{other} (descendant): {err}"
            );
        }
    }

    #[test]
    fn link_targets_match_any_spelling_of_the_id() {
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let mut na = content(&a.to_string(), "A");
        na.links.push(EjectLink {
            rel: "see_also".into(),
            target: b.to_string().to_uppercase(),
        });
        let report = EjectService::import_tree_with(
            &tree_of(vec![na, content(&b.to_string(), "B")]),
            ImportOptions::default(),
        )
        .unwrap();
        assert!(report.dropped_links.is_empty());
        let links = report
            .book
            .get_node(NodeId::from_uuid(a))
            .unwrap()
            .links()
            .to_vec();
        assert_eq!(links[0].target(), NodeId::from_uuid(b));
    }

    #[test]
    fn a_subtree_export_records_its_root() {
        let (book, sec, _, _) = curated_book();
        let whole = EjectService::build_tree(&book, None);
        assert!(whole.subtree_root.is_none());
        let json = serde_json::to_value(&whole).unwrap();
        assert!(
            json.get("subtree_root").is_none(),
            "absent for a whole-book export"
        );

        let part = EjectService::build_tree(&book, Some(sec));
        let root = part.subtree_root.clone().unwrap();
        assert_eq!(root.id, sec.to_string());
        assert_eq!(root.title, "Rules");
        let back: EjectTree = serde_json::from_str(&serde_json::to_string(&part).unwrap()).unwrap();
        assert_eq!(back.subtree_root, Some(root));
    }

    #[test]
    fn a_missing_updated_at_keeps_the_destination_stamp() {
        let (book, sec, live, _) = curated_book();
        let mut tree = EjectService::build_tree(&book, None);
        // Strip what a 0.14 export would not have had.
        fn strip(n: &mut EjectTreeNode) {
            n.updated_at = None;
            n.children.iter_mut().for_each(strip);
        }
        tree.nodes.iter_mut().for_each(strip);

        let onto = EjectService::import_tree_onto(&tree, ImportOptions::default(), Some(&book))
            .unwrap()
            .book;
        for id in [sec, live] {
            assert_eq!(
                onto.get_node(id).unwrap().updated_at(),
                book.get_node(id).unwrap().updated_at(),
                "stamp kept for {id}"
            );
        }
        // Without a destination (or with new ids) the node is stamped now.
        let fresh = EjectService::import_tree_with(&tree, ImportOptions::default())
            .unwrap()
            .book;
        assert!(fresh.get_node(live).unwrap().updated_at().is_some());
    }

    #[test]
    fn an_edited_node_without_stamp_is_stamped_now() {
        let (mut book, sec, live, _) = curated_book();
        let old = Timestamp::from_millis(1_600_000_000_000);
        for id in book.all_node_ids().collect::<Vec<_>>() {
            book.restamp(id, old).unwrap();
        }
        let mut tree = EjectService::build_tree(&book, None);
        fn strip(n: &mut EjectTreeNode) {
            n.updated_at = None;
            n.children.iter_mut().for_each(strip);
        }
        tree.nodes.iter_mut().for_each(strip);
        // Edit Live's body in the file, as a hand-edited 0.14 export would.
        let live_key = live.to_string();
        fn edit(n: &mut EjectTreeNode, key: &str) {
            if n.id == key {
                n.body = Some("edited in the file".into());
            }
            n.children.iter_mut().for_each(|c| edit(c, key));
        }
        tree.nodes.iter_mut().for_each(|n| edit(n, &live_key));

        let onto = EjectService::import_tree_onto(&tree, ImportOptions::default(), Some(&book))
            .unwrap()
            .book;
        assert_eq!(
            onto.get_node(sec).unwrap().updated_at(),
            Some(old),
            "unchanged node keeps the destination stamp"
        );
        let stamped = onto.get_node(live).unwrap().updated_at().unwrap();
        assert!(
            stamped > old,
            "edited node is stamped now, not with the old stamp"
        );
    }

    #[test]
    fn exported_subtree_is_recognized_with_or_without_the_marker() {
        let (book, sec, _, _) = curated_book();
        let whole = EjectService::build_tree(&book, None);
        assert_eq!(EjectService::exported_subtree(&whole, Some(&book)), None);

        let marked = EjectService::build_tree(&book, Some(sec));
        let (id, title, from_ids) = EjectService::exported_subtree(&marked, Some(&book)).unwrap();
        assert_eq!(
            (id, title.as_str(), from_ids),
            (sec.to_string(), "Rules", false)
        );

        // A 0.15.0-shaped subtree export: same nodes, no marker.
        let mut legacy = marked.clone();
        legacy.subtree_root = None;
        let (id, title, from_ids) = EjectService::exported_subtree(&legacy, Some(&book)).unwrap();
        assert_eq!(
            (id, title.as_str(), from_ids),
            (sec.to_string(), "Rules", true)
        );
        // Without the destination book there is nothing to recognize it from.
        assert_eq!(EjectService::exported_subtree(&legacy, None), None);
    }

    #[test]
    fn proposals_breaking_the_rules_are_dropped_and_reported() {
        let (book, _, live, old) = curated_book();
        let mut tree = EjectService::build_tree(&book, None);
        let base = tree.nodes[0].children[0].proposal.clone().unwrap();
        // On an archived node: not allowed.
        tree.nodes[0].children[1].proposal = Some(base.clone());
        // properties together with properties_patch: not allowed.
        let mut both = base.clone();
        both.properties = Some(HashMap::new());
        both.properties_patch = Some(HashMap::from([("k".into(), "v".into())]));
        tree.nodes[0].children[0].proposal = Some(both);

        let report = EjectService::import_tree_with(&tree, ImportOptions::default()).unwrap();
        let dropped: Vec<NodeId> = report.dropped_proposals.iter().map(|d| d.node).collect();
        assert_eq!(dropped, vec![live, old]);
        assert!(report.book.get_node(live).unwrap().proposal().is_none());
        assert!(report.book.get_node(old).unwrap().proposal().is_none());

        // A revision that changes nothing is dropped too.
        let mut tree = EjectService::build_tree(&book, None);
        let mut noop = base;
        noop.title = Some("Live".into());
        tree.nodes[0].children[0].proposal = Some(noop);
        let report = EjectService::import_tree_with(&tree, ImportOptions::default()).unwrap();
        assert_eq!(report.dropped_proposals.len(), 1);

        // A valid one survives unchanged (its base is kept).
        let tree = EjectService::build_tree(&book, None);
        let report = EjectService::import_tree_with(&tree, ImportOptions::default()).unwrap();
        assert!(report.dropped_proposals.is_empty());
        assert_eq!(
            report.book.get_node(live).unwrap().proposal(),
            book.get_node(live).unwrap().proposal()
        );
    }

    #[test]
    fn list_to_checkbox_dash() {
        assert_eq!(
            EjectService::list_to_checkbox("- cargo test"),
            "- [ ] cargo test"
        );
    }

    #[test]
    fn list_to_checkbox_asterisk() {
        assert_eq!(
            EjectService::list_to_checkbox("* cargo test"),
            "- [ ] cargo test"
        );
    }

    #[test]
    fn list_to_checkbox_indented() {
        assert_eq!(
            EjectService::list_to_checkbox("  - nested item"),
            "  - [ ] nested item"
        );
    }

    #[test]
    fn list_to_checkbox_non_list() {
        assert_eq!(EjectService::list_to_checkbox("plain text"), "plain text");
    }
}
