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

/// A typed, directed reference from one node to another node of the same
/// book (`supersedes`, `see_also`, …). Stored on the source node by target
/// id, so moving the target does not break it; backlinks are derived by
/// scanning the book.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Link {
    rel: String,
    target: NodeId,
}

/// Longest accepted link type.
pub const MAX_LINK_REL_LEN: usize = 40;

impl Link {
    /// A link of type `rel` to `target`. `rel` is normalized (trimmed,
    /// lowercased) and must be 1–40 characters of `a-z`, `0-9`, `_`, `-`;
    /// otherwise `None`.
    pub fn new(rel: &str, target: NodeId) -> Option<Self> {
        normalize_rel(rel).map(|rel| Self { rel, target })
    }

    /// The link type, normalized.
    pub fn rel(&self) -> &str {
        &self.rel
    }

    /// The node the link points at.
    pub fn target(&self) -> NodeId {
        self.target
    }
}

/// Normalize a link type: trimmed, lowercased, 1–40 of `[a-z0-9_-]`.
pub fn normalize_rel(rel: &str) -> Option<String> {
    let rel = rel.trim().to_lowercase();
    let ok = !rel.is_empty()
        && rel.chars().count() <= MAX_LINK_REL_LEN
        && rel
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    ok.then_some(rel)
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
    /// When someone last confirmed the node still matches reality
    /// (`node_verify`). Absent in data written before verification existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_at: Option<Timestamp>,
    /// Who confirmed it, as passed to `node_verify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_by: Option<String>,
    /// Outgoing typed links to other nodes of the book. Absent in data
    /// written before links existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    links: Vec<Link>,
}

/// Every stored field of a [`TemplateNode`], for adapters that keep nodes as
/// rows rather than as one serialized book.
pub(crate) struct NodeParts {
    pub(crate) id: NodeId,
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    pub(crate) node_type: NodeType,
    pub(crate) placeholder: Option<String>,
    pub(crate) properties: HashMap<String, String>,
    pub(crate) status: NodeStatus,
    pub(crate) updated_at: Option<Timestamp>,
    pub(crate) verified_at: Option<Timestamp>,
    pub(crate) verified_by: Option<String>,
    pub(crate) links: Vec<Link>,
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
            verified_at: None,
            verified_by: None,
            links: Vec::new(),
        }
    }

    /// Rebuild a node from stored fields (storage adapters only). No setter
    /// runs, so `updated_at` keeps its stored value.
    pub(crate) fn from_parts(parts: NodeParts) -> Self {
        Self {
            id: parts.id,
            parent: parts.parent,
            children: parts.children,
            title: parts.title,
            body: parts.body,
            node_type: parts.node_type,
            placeholder: parts.placeholder,
            properties: parts.properties,
            status: parts.status,
            updated_at: parts.updated_at,
            verified_at: parts.verified_at,
            verified_by: parts.verified_by,
            links: parts.links,
        }
    }

    /// Return the node's unique ID.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// Return the parent node's ID, or `None` for a root-level node.
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// Return the IDs of direct children, in order.
    pub fn children(&self) -> &[NodeId] {
        &self.children
    }

    /// Return the node's title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Return the node's markdown body content, if any.
    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    /// Return the node's type (`Section` or `Content`).
    pub fn node_type(&self) -> &NodeType {
        &self.node_type
    }

    /// Return the node's placeholder hint text, if any.
    pub fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    /// Return the node's key-value properties.
    pub fn properties(&self) -> &HashMap<String, String> {
        &self.properties
    }

    /// Look up a single property value by key.
    pub fn get_property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(|s| s.as_str())
    }

    /// Return `true` if the node has no children.
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Return the node's lifecycle status.
    pub fn status(&self) -> NodeStatus {
        self.status
    }

    /// Return the node's last-updated timestamp, if known.
    pub fn updated_at(&self) -> Option<Timestamp> {
        self.updated_at
    }

    /// When the node was last verified (`node_verify`), if ever.
    pub fn verified_at(&self) -> Option<Timestamp> {
        self.verified_at
    }

    /// Who last verified the node, if recorded.
    pub fn verified_by(&self) -> Option<&str> {
        self.verified_by.as_deref()
    }

    /// Outgoing typed links, in the order they were added.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// `true` when the node has never been verified or was last verified
    /// strictly before `cutoff`.
    pub fn verified_before(&self, cutoff: Timestamp) -> bool {
        self.verified_at.is_none_or(|t| t < cutoff)
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

    /// Merge `patch` into the existing properties: listed keys are inserted or
    /// overwritten, unlisted keys are left alone.
    ///
    /// This exists because [`set_properties`](Self::set_properties) replaces
    /// the whole map, which forces a caller that wants to change one key to
    /// read every other key back and resend it. Where the map has to travel
    /// over a wire that is not lossless, that round-trip is where properties
    /// get dropped. Removal still goes through `set_properties` — a patch
    /// carries no way to say "absent".
    pub(crate) fn patch_properties(&mut self, patch: HashMap<String, String>) {
        if patch.is_empty() {
            return;
        }
        self.properties.extend(patch);
        self.updated_at = Some(Timestamp::now());
    }

    // Subtask 4 (snapshot_restore) で使用予定
    #[allow(dead_code)]
    pub(crate) fn set_status(&mut self, status: NodeStatus) {
        self.status = status;
    }

    /// Record a verification. `updated_at` is left alone: verifying confirms
    /// the content, it does not change it, so "last edited" stays meaningful.
    pub(crate) fn set_verified(&mut self, at: Timestamp, by: Option<String>) {
        self.verified_at = Some(at);
        self.verified_by = by;
    }

    /// Add `link`; `false` (and no change) when the same link exists.
    ///
    /// Links are curation metadata, like a verification: `updated_at` is left
    /// alone, so "last edited" keeps meaning the node's own content.
    pub(crate) fn add_link(&mut self, link: Link) -> bool {
        if self.links.contains(&link) {
            return false;
        }
        self.links.push(link);
        true
    }

    /// Remove the links to `target` (only those of type `rel` when given).
    /// Returns how many were removed. `updated_at` is left alone.
    pub(crate) fn remove_links_to(&mut self, target: NodeId, rel: Option<&str>) -> usize {
        let before = self.links.len();
        self.links
            .retain(|l| !(l.target == target && rel.is_none_or(|r| l.rel == r)));
        before - self.links.len()
    }

    /// Remove every link whose target is in `targets`; returns how many.
    /// `updated_at` is left alone (the node itself did not change).
    pub(crate) fn remove_links_into(
        &mut self,
        targets: &std::collections::HashSet<NodeId>,
    ) -> usize {
        let before = self.links.len();
        self.links.retain(|l| !targets.contains(&l.target));
        before - self.links.len()
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
        assert!(node.verified_at().is_none() && node.verified_by().is_none());
        assert!(node.links().is_empty());
    }

    #[test]
    fn test_links_are_omitted_until_set_and_round_trip() {
        let mut node = make_node();
        assert!(!serde_json::to_string(&node).unwrap().contains("links"));
        let target = NodeId::new();
        assert!(node.add_link(Link::new("See_Also", target).unwrap()));
        assert!(
            !node.add_link(Link::new("see_also", target).unwrap()),
            "duplicate"
        );
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"rel\":\"see_also\""), "{json}");
        let back: TemplateNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back.links(), node.links());
        assert_eq!(node.remove_links_to(target, Some("supersedes")), 0);
        assert_eq!(node.remove_links_to(target, None), 1);
        assert!(node.links().is_empty());
    }

    #[test]
    fn test_link_rel_is_normalized_and_validated() {
        let t = NodeId::new();
        assert_eq!(Link::new("  Supersedes ", t).unwrap().rel(), "supersedes");
        assert_eq!(
            Link::new("merged-from_2", t).unwrap().rel(),
            "merged-from_2"
        );
        for bad in ["", "   ", "see also", "参照", "a/b", &"x".repeat(41)] {
            assert!(Link::new(bad, t).is_none(), "{bad:?}");
        }
        assert!(Link::new(&"x".repeat(40), t).is_some());
    }

    #[test]
    fn test_verification_fields_are_omitted_until_set_and_round_trip() {
        let mut node = make_node();
        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("verified"), "{json}");

        let at = Timestamp::from_millis(1_700_000_000_000);
        node.set_verified(at, Some("alice".into()));
        let back: TemplateNode =
            serde_json::from_str(&serde_json::to_string(&node).unwrap()).unwrap();
        assert_eq!(back.verified_at(), Some(at));
        assert_eq!(back.verified_by(), Some("alice"));
        assert!(back.verified_before(Timestamp::from_millis(at.as_millis() + 1)));
        assert!(!back.verified_before(at), "strictly before");
        assert!(make_node().verified_before(at), "never verified counts");
    }

    #[test]
    fn test_archived_status_round_trips() {
        let mut node = make_node();
        node.set_status(NodeStatus::Archived);
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"status\":\"archived\""), "{json}");
        let back: TemplateNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status(), NodeStatus::Archived);
        assert_eq!(NodeStatus::Archived.as_str(), "archived");
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
