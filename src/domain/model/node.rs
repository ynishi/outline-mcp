use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::changelog::NodeStatus;
use super::id::NodeId;
use super::timestamp::Timestamp;

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
    /// 任意のkey-valueメタデータ（inject, scope等）
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    properties: HashMap<String, String>,
    /// ノードのライフサイクル状態。既存JSONファイルには存在しないため `#[serde(default)]` で Active に。
    #[serde(default)]
    status: NodeStatus,
    /// 最終更新タイムスタンプ。既存JSONファイルには存在しないため `#[serde(default)]` で None に。
    #[serde(default)]
    updated_at: Option<Timestamp>,
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
            properties: HashMap::new(),
            status: NodeStatus::Active,
            updated_at: Some(Timestamp::now()),
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

    pub fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }

    pub fn get_property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(|s| s.as_str())
    }

    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    pub fn status(&self) -> NodeStatus {
        self.status
    }

    pub fn updated_at(&self) -> Option<Timestamp> {
        self.updated_at
    }

    // --- 内部操作（Book経由でのみ呼ばれる） ---

    pub(crate) fn set_title(&mut self, title: String) {
        self.title = title;
        self.updated_at = Some(Timestamp::now());
    }

    pub(crate) fn set_body(&mut self, body: Option<String>) {
        self.body = body;
        self.updated_at = Some(Timestamp::now());
    }

    pub(crate) fn set_node_type(&mut self, node_type: NodeType) {
        self.node_type = node_type;
        self.updated_at = Some(Timestamp::now());
    }

    pub(crate) fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
        self.updated_at = Some(Timestamp::now());
    }

    pub(crate) fn set_parent(&mut self, parent: Option<NodeId>) {
        self.parent = parent;
        self.updated_at = Some(Timestamp::now());
    }

    pub(crate) fn add_child(&mut self, child_id: NodeId, position: usize) {
        let pos = position.min(self.children.len());
        self.children.insert(pos, child_id);
    }

    pub(crate) fn remove_child(&mut self, child_id: NodeId) {
        self.children.retain(|id| *id != child_id);
    }

    pub(crate) fn set_properties(&mut self, properties: HashMap<String, String>) {
        self.properties = properties;
        self.updated_at = Some(Timestamp::now());
    }

    pub(crate) fn set_status(&mut self, status: NodeStatus) {
        self.status = status;
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node() -> TemplateNode {
        TemplateNode::new(NodeId::new(), None, "test".to_string(), NodeType::Content)
    }

    #[test]
    fn test_new_sets_status_active_and_updated_at() {
        let node = make_node();
        assert_eq!(node.status(), NodeStatus::Active);
        assert!(node.updated_at().is_some());
    }

    #[test]
    fn test_set_status() {
        let mut node = make_node();
        node.set_status(NodeStatus::Draft);
        assert_eq!(node.status(), NodeStatus::Draft);
        node.set_status(NodeStatus::Active);
        assert_eq!(node.status(), NodeStatus::Active);
    }

    #[test]
    fn test_set_title_updates_updated_at() {
        let mut node = make_node();
        let before = node.updated_at();
        // 同一ミリ秒内で実行される可能性があるため、タイムスタンプが Some であることのみ確認
        node.set_title("new title".to_string());
        assert!(node.updated_at().is_some());
        // before が Some の場合、updated_at が更新されていることを確認（同一ミリ秒でも Some になる）
        assert!(node.updated_at() >= before);
    }

    #[test]
    fn test_set_body_updates_updated_at() {
        let mut node = make_node();
        node.set_body(Some("body".to_string()));
        assert!(node.updated_at().is_some());
        assert_eq!(node.body(), Some("body"));
    }

    #[test]
    fn test_set_properties_updates_updated_at() {
        let mut node = make_node();
        let mut props = HashMap::new();
        props.insert("inject".to_string(), "true".to_string());
        node.set_properties(props);
        assert!(node.updated_at().is_some());
        assert_eq!(node.get_property("inject"), Some("true"));
    }

    #[test]
    fn test_serde_backward_compat_missing_fields() {
        // 既存JSONにstatus/updated_atがない場合のデシリアライズテスト
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "parent": null,
            "children": [],
            "title": "legacy",
            "body": null,
            "node_type": "Content",
            "placeholder": null
        }"#;
        let node: TemplateNode = serde_json::from_str(json).expect("deserialize legacy json");
        assert_eq!(node.status(), NodeStatus::Active);
        assert!(node.updated_at().is_none());
    }

    #[test]
    fn test_serde_roundtrip_with_new_fields() {
        let mut node = make_node();
        node.set_status(NodeStatus::Draft);
        node.set_title("hello".to_string());

        let json = serde_json::to_string(&node).expect("serialize");
        let restored: TemplateNode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.status(), NodeStatus::Draft);
        assert_eq!(restored.title(), "hello");
        assert!(restored.updated_at().is_some());
    }
}
