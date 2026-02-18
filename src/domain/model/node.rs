use serde::{Deserialize, Serialize};

use super::id::NodeId;

/// ノードの種別。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeType {
    /// 分類ノード（子を持つことが期待される）
    Section,
    /// 情報ノード（知識・手順・チェック項目など）
    Content,
}

/// Template上のノード。Bookが所有し、Bookを通じて操作する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateNode {
    id: NodeId,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    title: String,
    body: Option<String>,
    node_type: NodeType,
    /// Eject時に展開される記入欄のヒントテキスト
    placeholder: Option<String>,
}

impl TemplateNode {
    pub(crate) fn new(
        id: NodeId,
        parent: Option<NodeId>,
        title: String,
        node_type: NodeType,
    ) -> Self {
        Self {
            id,
            parent,
            children: Vec::new(),
            title,
            body: None,
            node_type,
            placeholder: None,
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    pub fn node_type(&self) -> &NodeType {
        &self.node_type
    }

    pub fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    // --- 内部操作（Book経由でのみ呼ばれる） ---

    pub(crate) fn set_title(&mut self, title: String) {
        self.title = title;
    }

    pub(crate) fn set_body(&mut self, body: Option<String>) {
        self.body = body;
    }

    pub(crate) fn set_node_type(&mut self, node_type: NodeType) {
        self.node_type = node_type;
    }

    pub(crate) fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    pub(crate) fn set_parent(&mut self, parent: Option<NodeId>) {
        self.parent = parent;
    }

    pub(crate) fn add_child(&mut self, child_id: NodeId, position: usize) {
        let pos = position.min(self.children.len());
        self.children.insert(pos, child_id);
    }

    pub(crate) fn remove_child(&mut self, child_id: NodeId) {
        self.children.retain(|id| *id != child_id);
    }
}
