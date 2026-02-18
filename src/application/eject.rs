use serde::{Deserialize, Serialize};

use crate::domain::model::book::{AddNodeRequest, TemplateBook};
use crate::domain::model::id::NodeId;
use crate::domain::model::node::{NodeType, TemplateNode};

use super::error::AppError;

/// Eject出力フォーマット
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectFormat {
    Markdown,
    Json,
}

/// Eject設定
pub struct EjectConfig {
    pub output_dir: std::path::PathBuf,
    pub filename: String,
    pub include_placeholders: bool,
    pub format: EjectFormat,
    /// 部分木のルート（Noneなら全体）
    pub subtree_root: Option<NodeId>,
}

/// JSON Eject用のツリー構造DTO
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EjectTreeNode {
    pub id: String,
    pub title: String,
    pub node_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<EjectTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EjectTree {
    pub title: String,
    pub max_depth: u8,
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

        EjectTree {
            title,
            max_depth: book.max_depth(),
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
        })
    }

    /// EjectTree（JSON） → TemplateBook に変換する。
    /// 再帰の最大深度。max_depthとは別に、JSON構造自体のネスト爆弾を防ぐ。
    const IMPORT_MAX_RECURSION: u8 = 32;

    pub fn import_tree(tree: &EjectTree) -> Result<TemplateBook, AppError> {
        let mut book = TemplateBook::new(&tree.title, tree.max_depth);
        for node in &tree.nodes {
            Self::import_tree_node(&mut book, None, node, 0)?;
        }
        Ok(book)
    }

    fn import_tree_node(
        book: &mut TemplateBook,
        parent: Option<NodeId>,
        tree_node: &EjectTreeNode,
        depth: u8,
    ) -> Result<(), AppError> {
        if depth >= Self::IMPORT_MAX_RECURSION {
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

        let id = book.add_node(AddNodeRequest {
            parent,
            title: tree_node.title.clone(),
            node_type,
            body: tree_node.body.clone(),
            placeholder: tree_node.placeholder.clone(),
            position: usize::MAX,
        })?;

        for child in &tree_node.children {
            Self::import_tree_node(book, Some(id), child, depth + 1)?;
        }

        Ok(())
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
                buf.push_str(&format!("{}  {}\n", indent, converted));
            }
        }

        if include_placeholders {
            if let Some(ph) = node.placeholder() {
                buf.push_str(&format!("{}  > {}: ___\n", indent, ph));
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
            })
            .unwrap();

        book.add_node(AddNodeRequest {
            parent: Some(design),
            title: "API design".into(),
            node_type: NodeType::Content,
            body: Some("REST endpoints".into()),
            placeholder: None,
            position: usize::MAX,
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
            nodes: vec![EjectTreeNode {
                id: "dummy".into(),
                title: "Node".into(),
                node_type: "unknown_type".into(),
                body: None,
                placeholder: None,
                children: vec![],
            }],
        };

        let result = EjectService::import_tree(&tree);
        assert!(result.is_err());
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
