use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::id::{BookId, NodeId};
use super::node::{
    content_fingerprint, normalize_rel, BodyChange, Link, NodeType, Revision, TemplateNode,
};
use super::timestamp::Timestamp;
use crate::domain::error::DomainError;

/// ノード追加リクエスト
pub struct AddNodeRequest {
    /// Parent node ID, or `None` for a root-level node.
    pub parent: Option<NodeId>,
    /// Node title.
    pub title: String,
    /// Node type (`Section` or `Content`).
    pub node_type: NodeType,
    /// Optional markdown body content.
    pub body: Option<String>,
    /// Optional placeholder hint used by `checklist`/`eject` export.
    pub placeholder: Option<String>,
    /// 兄弟内での挿入位置（末尾ならusize::MAX）
    pub position: usize,
    /// Optional key-value properties (e.g. `inject`, `scope`).
    pub properties: HashMap<String, String>,
}

/// ノード更新リクエスト（Noneのフィールドは変更しない）
pub struct UpdateNodeRequest {
    /// New title, or `None` to keep the current one.
    pub title: Option<String>,
    /// New body: `Some(None)` clears it, `None` keeps the current value.
    pub body: Option<Option<String>>,
    /// New node type, or `None` to keep the current one.
    pub node_type: Option<NodeType>,
    /// New placeholder: `Some(None)` clears it, `None` keeps the current value.
    pub placeholder: Option<Option<String>>,
    /// Replacement property map, or `None` to keep the current one.
    ///
    /// Mutually exclusive with [`properties_patch`](Self::properties_patch).
    pub properties: Option<HashMap<String, String>>,
    /// Properties to merge into the existing map: listed keys are inserted or
    /// overwritten, unlisted keys survive. `None` keeps the current map.
    ///
    /// Mutually exclusive with [`properties`](Self::properties).
    pub properties_patch: Option<HashMap<String, String>>,
    /// New lifecycle status, or `None` to keep the current one.
    pub status: Option<super::changelog::NodeStatus>,
}

/// Template Book — 集約ルート。全ノード操作はここを経由する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateBook {
    id: BookId,
    title: String,
    max_depth: u8,
    nodes: HashMap<NodeId, TemplateNode>,
    root_nodes: Vec<NodeId>,
    /// Retired: kept readable, but hidden from `shelf` and the browser view's
    /// shelf unless asked for, and its nodes are not injected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    archived: bool,
}

impl TemplateBook {
    /// Create a new, empty book with the given title and max depth.
    pub fn new(title: impl Into<String>, max_depth: u8) -> Self {
        Self {
            id: BookId::new(),
            title: title.into(),
            max_depth,
            nodes: HashMap::new(),
            root_nodes: Vec::new(),
            archived: false,
        }
    }

    /// Rebuild a book from stored parts (storage adapters only). The caller
    /// owns consistency: every id in `root_nodes` and in each node's
    /// `children` must be a key of `nodes`, as the adapter wrote it.
    pub(crate) fn from_parts(
        id: BookId,
        title: String,
        max_depth: u8,
        nodes: HashMap<NodeId, TemplateNode>,
        root_nodes: Vec<NodeId>,
    ) -> Self {
        Self {
            id,
            title,
            max_depth,
            nodes,
            root_nodes,
            archived: false,
        }
    }

    /// Return the book's unique ID.
    pub fn id(&self) -> BookId {
        self.id
    }

    /// Return the book's title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Return the book's configured maximum tree depth.
    pub fn max_depth(&self) -> u8 {
        self.max_depth
    }

    /// Whether the book is archived (retired, see [`Self::set_archived`]).
    pub fn is_archived(&self) -> bool {
        self.archived
    }

    /// Archive or unarchive the book. Its nodes are untouched: an archived
    /// book stays readable and writable, it is only left out of listings and
    /// context injection.
    pub fn set_archived(&mut self, archived: bool) {
        self.archived = archived;
    }

    /// Rename the book. The title is trimmed and must not be empty.
    pub fn set_title(&mut self, title: impl Into<String>) -> Result<(), DomainError> {
        let title = title.into();
        let title = title.trim();
        if title.is_empty() {
            return Err(DomainError::EmptyBookTitle);
        }
        self.title = title.to_string();
        Ok(())
    }

    /// Change the maximum tree depth. Refused when it is 0, or when an
    /// existing node is deeper than `max_depth` (depth as in [`Self::depth_of`],
    /// root = 1 — the same measure `add_node` checks against); the error names
    /// the deepest node.
    pub fn set_max_depth(&mut self, max_depth: u8) -> Result<(), DomainError> {
        if max_depth == 0 {
            return Err(DomainError::InvalidMaxDepth);
        }
        let deepest = self
            .nodes
            .keys()
            .map(|&id| (self.depth_of(id), id))
            .max_by_key(|&(depth, _)| depth);
        if let Some((depth, node_id)) = deepest {
            if depth > max_depth {
                let title = self
                    .nodes
                    .get(&node_id)
                    .map(|n| n.title().to_string())
                    .unwrap_or_default();
                return Err(DomainError::MaxDepthBelowExistingNode {
                    requested: max_depth,
                    node_id,
                    title,
                    depth,
                });
            }
        }
        self.max_depth = max_depth;
        Ok(())
    }

    /// Return the IDs of root-level nodes, in order.
    pub fn root_nodes(&self) -> &[NodeId] {
        &self.root_nodes
    }

    /// Look up a node by ID.
    pub fn get_node(&self, id: NodeId) -> Option<&TemplateNode> {
        self.nodes.get(&id)
    }

    /// Return the total number of nodes in the book.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// ノード追加。深さ制限を検証してから挿入する。
    pub fn add_node(&mut self, req: AddNodeRequest) -> Result<NodeId, DomainError> {
        // 親の存在チェック
        if let Some(parent_id) = req.parent {
            if !self.nodes.contains_key(&parent_id) {
                return Err(DomainError::NodeNotFound(parent_id));
            }
        }

        // 深さチェック
        let new_depth = match req.parent {
            Some(pid) => self.depth_of(pid) + 1,
            None => 1,
        };
        let node_id = NodeId::new();
        if new_depth > self.max_depth {
            return Err(DomainError::MaxDepthExceeded {
                node_id,
                max: self.max_depth,
            });
        }

        let mut node = TemplateNode::new(node_id, req.parent, req.title, req.node_type);
        node.set_body(req.body);
        node.set_placeholder(req.placeholder);
        if !req.properties.is_empty() {
            node.set_properties(req.properties);
        }

        self.nodes.insert(node_id, node);

        // 親の children or root_nodes に挿入
        match req.parent {
            Some(parent_id) => {
                let parent = self
                    .nodes
                    .get_mut(&parent_id)
                    .ok_or(DomainError::NodeNotFound(parent_id))?;
                parent.add_child(node_id, req.position);
            }
            None => {
                let pos = req.position.min(self.root_nodes.len());
                self.root_nodes.insert(pos, node_id);
            }
        }

        Ok(node_id)
    }

    /// ノード更新。
    pub fn update_node(&mut self, id: NodeId, req: UpdateNodeRequest) -> Result<(), DomainError> {
        // Rejected before any field is touched: applying replace and merge in
        // the same call would make the outcome depend on which ran last.
        if req.properties.is_some() && req.properties_patch.is_some() {
            return Err(DomainError::ConflictingPropertyUpdate(id));
        }

        let node = self
            .nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?;

        if let Some(title) = req.title {
            node.set_title(title);
        }
        if let Some(body) = req.body {
            node.set_body(body);
        }
        if let Some(node_type) = req.node_type {
            node.set_node_type(node_type);
        }
        if let Some(placeholder) = req.placeholder {
            node.set_placeholder(placeholder);
        }
        if let Some(properties) = req.properties {
            node.set_properties(properties);
        }
        if let Some(patch) = req.properties_patch {
            node.patch_properties(patch);
        }
        if let Some(status) = req.status {
            node.set_status(status);
        }

        Ok(())
    }

    /// `true` when the node or one of its ancestors is archived: retiring a
    /// section retires what is under it.
    pub fn is_archived_or_under_archived(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            match self.nodes.get(&c) {
                Some(n) if n.status() == super::changelog::NodeStatus::Archived => return true,
                Some(n) => cur = n.parent(),
                None => return false,
            }
        }
        false
    }

    /// Attach `revision` to node `id` as its pending revision, without
    /// changing the node's content. `revision.base` is set to the node's
    /// current [`content_fingerprint`].
    ///
    /// Refused when the node is archived (or under an archived section), when
    /// the revision changes no field or would leave the node as it is, when it
    /// both replaces and patches properties, and when a revision is already
    /// pending unless `replace`.
    pub fn propose_revision(
        &mut self,
        id: NodeId,
        mut revision: Revision,
        replace: bool,
    ) -> Result<(), DomainError> {
        let node = self.nodes.get(&id).ok_or(DomainError::NodeNotFound(id))?;
        if node.proposal().is_some() && !replace {
            return Err(DomainError::ProposalExists(id));
        }
        self.check_revision(id, &revision)?;
        revision.base = content_fingerprint(node);
        self.nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .set_proposal(Some(revision));
        Ok(())
    }

    /// Set a node's last-edited stamp. Import-only: everything else stamps
    /// through the operation that changed the node.
    pub(crate) fn restamp(&mut self, id: NodeId, at: Timestamp) -> Result<(), DomainError> {
        self.nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .set_updated_at(Some(at));
        Ok(())
    }

    /// Put back a pending revision read from an export, unchanged (its
    /// `base`, proposer and time are kept, so one that was stale stays
    /// stale), after the same checks [`Self::propose_revision`] makes.
    pub(crate) fn restore_proposal(
        &mut self,
        id: NodeId,
        revision: Revision,
    ) -> Result<(), DomainError> {
        self.check_revision(id, &revision)?;
        self.nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .set_proposal(Some(revision));
        Ok(())
    }

    /// The rules every pending revision obeys: the node exists and is not
    /// archived (or under an archived section), the revision changes at
    /// least one field, does not combine `properties` with
    /// `properties_patch`, and would change the node's content.
    fn check_revision(&self, id: NodeId, revision: &Revision) -> Result<(), DomainError> {
        let node = self.nodes.get(&id).ok_or(DomainError::NodeNotFound(id))?;
        if self.is_archived_or_under_archived(id) {
            return Err(DomainError::RevisionOnArchived(id));
        }
        if revision.fields().is_empty() {
            return Err(DomainError::EmptyRevision);
        }
        if revision.properties.is_some() && revision.properties_patch.is_some() {
            return Err(DomainError::ConflictingPropertyUpdate(id));
        }
        let mut applied = node.clone();
        apply_revision_content(&mut applied, revision);
        if content_fingerprint(&applied) == content_fingerprint(node) {
            return Err(DomainError::RevisionChangesNothing(id));
        }
        Ok(())
    }

    /// Apply node `id`'s pending revision to its content (through the same
    /// path as [`Self::update_node`]) and clear it. Returns the applied
    /// revision.
    ///
    /// Refused when there is none, when the node is archived (or under an
    /// archived section), and — unless `force` — when the node changed after
    /// the revision was proposed.
    pub fn approve_revision(&mut self, id: NodeId, force: bool) -> Result<Revision, DomainError> {
        let node = self.nodes.get(&id).ok_or(DomainError::NodeNotFound(id))?;
        let revision = node
            .proposal()
            .cloned()
            .ok_or(DomainError::NoProposal(id))?;
        if self.is_archived_or_under_archived(id) {
            return Err(DomainError::RevisionOnArchived(id));
        }
        if revision.is_stale(node) && !force {
            return Err(DomainError::StaleProposal(id));
        }
        self.update_node(
            id,
            UpdateNodeRequest {
                title: revision.title.clone(),
                body: revision.body.clone().map(|b| match b {
                    BodyChange::Set(text) => Some(text),
                    BodyChange::Clear => None,
                }),
                node_type: None,
                placeholder: None,
                properties: revision.properties.clone(),
                properties_patch: revision.properties_patch.clone(),
                status: None,
            },
        )?;
        self.nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .set_proposal(None);
        Ok(revision)
    }

    /// Discard node `id`'s pending revision; returns it.
    pub fn reject_revision(&mut self, id: NodeId) -> Result<Revision, DomainError> {
        self.nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .set_proposal(None)
            .ok_or(DomainError::NoProposal(id))
    }

    /// Nodes with a pending revision, in book (depth-first) order.
    pub fn nodes_with_proposals(&self) -> Vec<&TemplateNode> {
        self.all_nodes_dfs()
            .into_iter()
            .filter(|n| n.proposal().is_some())
            .collect()
    }

    /// Mark a node as verified at `at` by `by` (content unchanged).
    pub fn verify_node(
        &mut self,
        id: NodeId,
        at: Timestamp,
        by: Option<String>,
    ) -> Result<(), DomainError> {
        self.nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .set_verified(at, by);
        Ok(())
    }

    /// Add a typed link from `from` to `to`. Both must exist in this book and
    /// differ; `rel` is normalized (see [`Link::new`]). Returns `false` (no
    /// change) when the same link already exists.
    pub fn add_link(&mut self, from: NodeId, to: NodeId, rel: &str) -> Result<bool, DomainError> {
        if !self.nodes.contains_key(&to) {
            return Err(DomainError::NodeNotFound(to));
        }
        if from == to {
            return Err(DomainError::SelfLink(from));
        }
        let link =
            Link::new(rel, to).ok_or_else(|| DomainError::InvalidLinkRel(rel.to_string()))?;
        Ok(self
            .nodes
            .get_mut(&from)
            .ok_or(DomainError::NodeNotFound(from))?
            .add_link(link))
    }

    /// Remove the links from `from` to `to` — only those of type `rel` when
    /// given, every type otherwise. Returns how many were removed.
    pub fn remove_link(
        &mut self,
        from: NodeId,
        to: NodeId,
        rel: Option<&str>,
    ) -> Result<usize, DomainError> {
        let rel = match rel {
            Some(r) => {
                Some(normalize_rel(r).ok_or_else(|| DomainError::InvalidLinkRel(r.to_string()))?)
            }
            None => None,
        };
        Ok(self
            .nodes
            .get_mut(&from)
            .ok_or(DomainError::NodeNotFound(from))?
            .remove_links_to(to, rel.as_deref()))
    }

    /// Incoming links of `target`: `(source node, link)` pairs, in book
    /// (depth-first) order. Derived on every call; nothing is stored.
    pub fn backlinks(&self, target: NodeId) -> Vec<(NodeId, &Link)> {
        self.all_nodes_dfs()
            .into_iter()
            .flat_map(|n| {
                n.links()
                    .iter()
                    .filter(move |l| l.target() == target)
                    .map(move |l| (n.id(), l))
            })
            .collect()
    }

    /// Every node's incoming links at once: target → `(source node, link)`
    /// pairs in book (depth-first) order, the same pairs [`Self::backlinks`]
    /// returns per target. One pass over the book, for callers that need the
    /// backlinks of many nodes (a per-node `backlinks` call would be O(n²)).
    pub fn backlink_index(&self) -> HashMap<NodeId, Vec<(NodeId, &Link)>> {
        let mut index: HashMap<NodeId, Vec<(NodeId, &Link)>> = HashMap::new();
        for n in self.all_nodes_dfs() {
            for l in n.links() {
                index.entry(l.target()).or_default().push((n.id(), l));
            }
        }
        index
    }

    /// Links whose target is not a node of this book (left behind by data
    /// restored or imported from elsewhere): `(source node, link)` pairs.
    pub fn dangling_links(&self) -> Vec<(NodeId, &Link)> {
        self.all_nodes_dfs()
            .into_iter()
            .flat_map(|n| {
                n.links()
                    .iter()
                    .filter(|l| !self.nodes.contains_key(&l.target()))
                    .map(move |l| (n.id(), l))
            })
            .collect()
    }

    /// Total number of links in the book.
    pub fn link_count(&self) -> usize {
        self.nodes.values().map(|n| n.links().len()).sum()
    }

    /// ノード移動。循環参照と深さ超過を検証する。
    pub fn move_node(
        &mut self,
        id: NodeId,
        new_parent: Option<NodeId>,
        position: usize,
    ) -> Result<(), DomainError> {
        self.validate_move(id, new_parent)?;
        self.detach_from_parent(id)?;
        self.attach_to_parent(id, new_parent, position)?;
        Ok(())
    }

    /// ノード削除（子孫ごと再帰的に削除）
    pub fn remove_node(&mut self, id: NodeId) -> Result<(), DomainError> {
        if !self.nodes.contains_key(&id) {
            return Err(DomainError::NodeNotFound(id));
        }

        // 子孫IDを収集
        let descendants = self.collect_descendants(id);

        // 親から除去
        let parent = self
            .nodes
            .get(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .parent();
        match parent {
            Some(p_id) => {
                let p = self
                    .nodes
                    .get_mut(&p_id)
                    .ok_or(DomainError::NodeNotFound(p_id))?;
                p.remove_child(id);
            }
            None => {
                self.root_nodes.retain(|nid| *nid != id);
            }
        }

        // 本体 + 子孫を削除
        let mut removed: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        removed.insert(id);
        self.nodes.remove(&id);
        for desc_id in descendants {
            self.nodes.remove(&desc_id);
            removed.insert(desc_id);
        }

        // Links into the removed subtree would dangle: drop them from the
        // nodes that remain.
        for node in self.nodes.values_mut() {
            node.remove_links_into(&removed);
        }

        Ok(())
    }

    /// The ids of every descendant of `id` (not `id` itself), depth-first.
    /// Empty when the node has no children or does not exist.
    pub fn descendant_ids(&self, id: NodeId) -> Vec<NodeId> {
        self.collect_descendants(id)
    }

    /// 指定ノードを含むサブツリーのノード一覧（DFS順）
    pub fn subtree_nodes(&self, root: NodeId) -> Vec<&TemplateNode> {
        let mut result = Vec::new();
        self.collect_subtree_dfs(root, &mut result);
        result
    }

    /// 全ノードIDのイテレータ
    pub fn all_node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    /// 指定プロパティ条件に一致するノードをDFS順で返す
    pub fn nodes_matching(&self, filter: &HashMap<String, String>) -> Vec<&TemplateNode> {
        self.all_nodes_dfs()
            .into_iter()
            .filter(|node| {
                filter
                    .iter()
                    .all(|(k, v)| node.get_property(k).map(|pv| pv == v).unwrap_or(false))
            })
            .collect()
    }

    /// 全ノードをDFS順で返す（Eject用）
    pub fn all_nodes_dfs(&self) -> Vec<&TemplateNode> {
        let mut result = Vec::new();
        for &root_id in &self.root_nodes {
            self.collect_subtree_dfs(root_id, &mut result);
        }
        result
    }

    /// ノードの深さを返す（ルート=1）。破損データの無限ループを防御する。
    pub fn depth_of(&self, id: NodeId) -> u8 {
        let mut depth = 1u8;
        let mut current = id;
        while let Some(parent) = self.nodes.get(&current).and_then(|n| n.parent()) {
            depth = depth.saturating_add(1);
            if depth == u8::MAX {
                break;
            }
            current = parent;
        }
        depth
    }

    // --- Private helpers ---

    fn validate_move(&self, id: NodeId, new_parent: Option<NodeId>) -> Result<(), DomainError> {
        if !self.nodes.contains_key(&id) {
            return Err(DomainError::NodeNotFound(id));
        }
        if let Some(np_id) = new_parent {
            if !self.nodes.contains_key(&np_id) {
                return Err(DomainError::NodeNotFound(np_id));
            }
            if self.is_descendant_of(np_id, id) {
                return Err(DomainError::CyclicMove(id));
            }
        }
        let subtree_max = self.subtree_max_depth(id);
        let current_depth = self.depth_of(id);
        let new_base_depth = match new_parent {
            Some(np_id) => self.depth_of(np_id).saturating_add(1),
            None => 1,
        };
        let depth_delta = subtree_max.saturating_sub(current_depth);
        if new_base_depth.saturating_add(depth_delta) > self.max_depth {
            return Err(DomainError::MaxDepthExceeded {
                node_id: id,
                max: self.max_depth,
            });
        }
        Ok(())
    }

    fn detach_from_parent(&mut self, id: NodeId) -> Result<(), DomainError> {
        let old_parent = self
            .nodes
            .get(&id)
            .ok_or(DomainError::NodeNotFound(id))?
            .parent();
        match old_parent {
            Some(op_id) => {
                let op = self
                    .nodes
                    .get_mut(&op_id)
                    .ok_or(DomainError::NodeNotFound(op_id))?;
                op.remove_child(id);
            }
            None => {
                self.root_nodes.retain(|nid| *nid != id);
            }
        }
        Ok(())
    }

    fn attach_to_parent(
        &mut self,
        id: NodeId,
        new_parent: Option<NodeId>,
        position: usize,
    ) -> Result<(), DomainError> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or(DomainError::NodeNotFound(id))?;
        node.set_parent(new_parent);
        match new_parent {
            Some(np_id) => {
                let np = self
                    .nodes
                    .get_mut(&np_id)
                    .ok_or(DomainError::NodeNotFound(np_id))?;
                np.add_child(id, position);
            }
            None => {
                let pos = position.min(self.root_nodes.len());
                self.root_nodes.insert(pos, id);
            }
        }
        Ok(())
    }

    fn is_descendant_of(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut current = node;
        while let Some(parent) = self.nodes.get(&current).and_then(|n| n.parent()) {
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    }

    fn subtree_max_depth(&self, root: NodeId) -> u8 {
        let mut max = self.depth_of(root);
        let descendants = self.collect_descendants(root);
        for d in descendants {
            let d_depth = self.depth_of(d);
            if d_depth > max {
                max = d_depth;
            }
        }
        max
    }

    fn collect_descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        if let Some(node) = self.nodes.get(&id) {
            for &child_id in node.children() {
                result.push(child_id);
                result.extend(self.collect_descendants(child_id));
            }
        }
        result
    }

    fn collect_subtree_dfs<'a>(&'a self, id: NodeId, out: &mut Vec<&'a TemplateNode>) {
        if let Some(node) = self.nodes.get(&id) {
            out.push(node);
            for &child_id in node.children() {
                self.collect_subtree_dfs(child_id, out);
            }
        }
    }
}

/// Apply a revision's content fields to `node` (title, body, properties),
/// the way approving it would. Used to tell whether it changes anything.
fn apply_revision_content(node: &mut TemplateNode, revision: &Revision) {
    if let Some(title) = &revision.title {
        node.set_title(title.clone());
    }
    match &revision.body {
        Some(BodyChange::Set(text)) => node.set_body(Some(text.clone())),
        Some(BodyChange::Clear) => node.set_body(None),
        None => {}
    }
    if let Some(properties) = &revision.properties {
        node.set_properties(properties.clone());
    }
    if let Some(patch) = &revision.properties_patch {
        node.patch_properties(patch.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_book() -> TemplateBook {
        TemplateBook::new("Test Book", 4)
    }

    #[test]
    fn add_root_node() {
        let mut book = make_book();
        let id = book
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

        assert_eq!(book.root_nodes().len(), 1);
        assert_eq!(book.get_node(id).unwrap().title(), "Design");
    }

    #[test]
    fn archived_ancestor_hides_descendants() {
        let mut book = make_book();
        let add = |book: &mut TemplateBook, parent| {
            book.add_node(AddNodeRequest {
                parent,
                title: "n".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap()
        };
        let top = add(&mut book, None);
        let mid = add(&mut book, Some(top));
        let leaf = add(&mut book, Some(mid));
        let other = add(&mut book, None);
        assert!(!book.is_archived_or_under_archived(leaf));
        book.nodes
            .get_mut(&mid)
            .unwrap()
            .set_status(super::super::changelog::NodeStatus::Archived);
        assert!(book.is_archived_or_under_archived(mid));
        assert!(book.is_archived_or_under_archived(leaf));
        assert!(!book.is_archived_or_under_archived(top));
        assert!(!book.is_archived_or_under_archived(other));
        assert!(!book.is_archived_or_under_archived(NodeId::new()));
    }

    #[test]
    fn add_child_to_section() {
        let mut book = make_book();
        let parent = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Phase 1".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let child = book
            .add_node(AddNodeRequest {
                parent: Some(parent),
                title: "Write tests".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: Some("list test cases here".into()),
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let parent_node = book.get_node(parent).unwrap();
        assert_eq!(parent_node.children().len(), 1);
        assert_eq!(parent_node.children()[0], child);
    }

    #[test]
    fn reject_exceeding_max_depth() {
        let mut book = TemplateBook::new("Shallow", 2);
        let l1 = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "L1".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let l2 = book
            .add_node(AddNodeRequest {
                parent: Some(l1),
                title: "L2".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let result = book.add_node(AddNodeRequest {
            parent: Some(l2),
            title: "L3 - too deep".into(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        });

        assert!(matches!(
            result,
            Err(DomainError::MaxDepthExceeded { max: 2, .. })
        ));
    }

    fn child(parent: Option<NodeId>, title: &str) -> AddNodeRequest {
        AddNodeRequest {
            parent,
            title: title.into(),
            node_type: NodeType::Section,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        }
    }

    #[test]
    fn set_max_depth_refuses_to_go_below_the_deepest_node() {
        let mut book = TemplateBook::new("B", 3);
        let l1 = book.add_node(child(None, "L1")).unwrap();
        let l2 = book.add_node(child(Some(l1), "L2")).unwrap();

        match book.set_max_depth(1) {
            Err(DomainError::MaxDepthBelowExistingNode {
                requested: 1,
                node_id,
                title,
                depth: 2,
            }) => {
                assert_eq!(node_id, l2);
                assert_eq!(title, "L2");
            }
            other => panic!("expected MaxDepthBelowExistingNode, got {other:?}"),
        }
        assert_eq!(
            book.max_depth(),
            3,
            "a refused change leaves the book as it was"
        );
        assert!(matches!(
            book.set_max_depth(0),
            Err(DomainError::InvalidMaxDepth)
        ));

        // Equal to the deepest node is fine.
        book.set_max_depth(2).unwrap();
        assert!(book.add_node(child(Some(l2), "L3")).is_err());
    }

    #[test]
    fn raising_max_depth_allows_deeper_nodes() {
        let mut book = TemplateBook::new("B", 1);
        let l1 = book.add_node(child(None, "L1")).unwrap();
        assert!(book.add_node(child(Some(l1), "L2")).is_err());
        book.set_max_depth(2).unwrap();
        assert!(book.add_node(child(Some(l1), "L2")).is_ok());
    }

    #[test]
    fn set_title_trims_and_rejects_empty() {
        let mut book = TemplateBook::new("Old", 2);
        book.set_title("  New  ").unwrap();
        assert_eq!(book.title(), "New");
        assert!(matches!(
            book.set_title("   "),
            Err(DomainError::EmptyBookTitle)
        ));
        assert_eq!(book.title(), "New");
    }

    #[test]
    fn move_node_between_parents() {
        let mut book = make_book();
        let a = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "A".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
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
                properties: HashMap::new(),
            })
            .unwrap();

        let child = book
            .add_node(AddNodeRequest {
                parent: Some(a),
                title: "Task".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        book.move_node(child, Some(b), 0).unwrap();

        assert!(book.get_node(a).unwrap().children().is_empty());
        assert_eq!(book.get_node(b).unwrap().children(), &[child]);
        assert_eq!(book.get_node(child).unwrap().parent(), Some(b));
    }

    #[test]
    fn reject_cyclic_move() {
        let mut book = make_book();
        let parent = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Parent".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let child = book
            .add_node(AddNodeRequest {
                parent: Some(parent),
                title: "Child".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let result = book.move_node(parent, Some(child), 0);
        assert!(matches!(result, Err(DomainError::CyclicMove(_))));
    }

    #[test]
    fn remove_node_with_descendants() {
        let mut book = make_book();
        let root = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Root".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let child = book
            .add_node(AddNodeRequest {
                parent: Some(root),
                title: "Child".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let _grandchild = book
            .add_node(AddNodeRequest {
                parent: Some(child),
                title: "Grandchild".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        assert_eq!(book.node_count(), 3);
        book.remove_node(root).unwrap();
        assert_eq!(book.node_count(), 0);
        assert!(book.root_nodes().is_empty());
    }

    #[test]
    fn update_node_title_and_type() {
        let mut book = make_book();
        let id = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Old".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        book.update_node(
            id,
            UpdateNodeRequest {
                title: Some("New".into()),
                body: Some(Some("description".into())),
                node_type: Some(NodeType::Content),
                placeholder: None,
                properties: None,
                properties_patch: None,
                status: None,
            },
        )
        .unwrap();

        let node = book.get_node(id).unwrap();
        assert_eq!(node.title(), "New");
        assert_eq!(node.body(), Some("description"));
        assert_eq!(*node.node_type(), NodeType::Content);
    }

    /// Add a node carrying `properties`, returning its id.
    fn node_with_props(book: &mut TemplateBook, props: &[(&str, &str)]) -> NodeId {
        book.add_node(AddNodeRequest {
            parent: None,
            title: "N".into(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
        .unwrap()
    }

    fn patch_only(patch: &[(&str, &str)]) -> UpdateNodeRequest {
        UpdateNodeRequest {
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: Some(
                patch
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            status: None,
        }
    }

    #[test]
    fn properties_patch_sets_listed_keys_and_keeps_the_rest() {
        let mut book = make_book();
        // `evidence` is the shape that motivated the patch path: a free-text
        // value a caller cannot safely round-trip through a lossy transport.
        let id = node_with_props(
            &mut book,
            &[
                ("gate_status", "pending"),
                ("kind", "Pitfall"),
                ("evidence", "a, b = c"),
            ],
        );

        book.update_node(id, patch_only(&[("gate_status", "approved")]))
            .unwrap();

        let node = book.get_node(id).unwrap();
        assert_eq!(node.get_property("gate_status"), Some("approved"));
        assert_eq!(node.get_property("kind"), Some("Pitfall"));
        assert_eq!(node.get_property("evidence"), Some("a, b = c"));
        assert_eq!(node.properties().len(), 3);
    }

    #[test]
    fn properties_patch_adds_keys_to_a_node_that_had_none() {
        let mut book = make_book();
        let id = node_with_props(&mut book, &[]);

        book.update_node(id, patch_only(&[("inject", "true")]))
            .unwrap();

        assert_eq!(
            book.get_node(id).unwrap().get_property("inject"),
            Some("true")
        );
    }

    #[test]
    fn empty_properties_patch_is_a_no_op() {
        let mut book = make_book();
        let id = node_with_props(&mut book, &[("kind", "Fact")]);

        book.update_node(id, patch_only(&[])).unwrap();

        let node = book.get_node(id).unwrap();
        assert_eq!(node.get_property("kind"), Some("Fact"));
        assert_eq!(node.properties().len(), 1);
    }

    #[test]
    fn properties_and_properties_patch_together_are_rejected() {
        let mut book = make_book();
        let id = node_with_props(&mut book, &[("kind", "Fact")]);

        let mut req = patch_only(&[("gate_status", "approved")]);
        req.properties = Some(HashMap::new());

        let err = book.update_node(id, req).unwrap_err();
        assert!(matches!(err, DomainError::ConflictingPropertyUpdate(_)));

        // Rejected before anything was applied.
        let node = book.get_node(id).unwrap();
        assert_eq!(node.get_property("kind"), Some("Fact"));
        assert_eq!(node.get_property("gate_status"), None);
    }

    #[test]
    fn dfs_order() {
        let mut book = make_book();
        let a = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "A".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let a1 = book
            .add_node(AddNodeRequest {
                parent: Some(a),
                title: "A-1".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let a2 = book
            .add_node(AddNodeRequest {
                parent: Some(a),
                title: "A-2".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let b = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "B".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();

        let all = book.all_nodes_dfs();
        let ids: Vec<NodeId> = all.iter().map(|n| n.id()).collect();
        assert_eq!(ids, vec![a, a1, a2, b]);
    }

    fn add(book: &mut TemplateBook, parent: Option<NodeId>, title: &str) -> NodeId {
        book.add_node(AddNodeRequest {
            parent,
            title: title.into(),
            node_type: NodeType::Section,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        })
        .unwrap()
    }

    #[test]
    fn links_add_validate_and_derive_backlinks() {
        let mut book = make_book();
        let a = add(&mut book, None, "A");
        let b = add(&mut book, None, "B");
        let c = add(&mut book, None, "C");
        assert!(book.add_link(a, b, "supersedes").unwrap());
        assert!(book.add_link(c, b, "See_Also").unwrap());
        assert!(
            !book.add_link(a, b, "supersedes").unwrap(),
            "duplicate is a no-op"
        );
        assert!(matches!(
            book.add_link(a, a, "x"),
            Err(DomainError::SelfLink(_))
        ));
        assert!(matches!(
            book.add_link(a, b, "see also"),
            Err(DomainError::InvalidLinkRel(_))
        ));
        assert!(matches!(
            book.add_link(a, NodeId::new(), "x"),
            Err(DomainError::NodeNotFound(_))
        ));

        let back: Vec<(NodeId, String)> = book
            .backlinks(b)
            .into_iter()
            .map(|(s, l)| (s, l.rel().to_string()))
            .collect();
        assert_eq!(back, vec![(a, "supersedes".into()), (c, "see_also".into())]);
        assert_eq!(book.link_count(), 2);
        assert_eq!(book.remove_link(c, b, Some("supersedes")).unwrap(), 0);
        assert_eq!(book.remove_link(c, b, None).unwrap(), 1);
        assert_eq!(book.backlinks(b).len(), 1);
    }

    #[test]
    fn moving_a_target_keeps_its_backlinks() {
        let mut book = make_book();
        let a = add(&mut book, None, "A");
        let s = add(&mut book, None, "S");
        let b = add(&mut book, None, "B");
        book.add_link(a, b, "see_also").unwrap();
        book.move_node(b, Some(s), 0).unwrap();
        assert_eq!(book.backlinks(b).len(), 1);
        assert!(book.dangling_links().is_empty());
    }

    #[test]
    fn removing_a_subtree_drops_links_into_it_only() {
        let mut book = make_book();
        let a = add(&mut book, None, "A");
        let s = add(&mut book, None, "S");
        let s1 = add(&mut book, Some(s), "S1");
        let other = add(&mut book, None, "Other");
        book.add_link(a, s, "x").unwrap();
        book.add_link(a, s1, "y").unwrap();
        book.add_link(a, other, "z").unwrap();
        book.add_link(s1, other, "w").unwrap();
        book.remove_node(s).unwrap();
        let rels: Vec<&str> = book
            .get_node(a)
            .unwrap()
            .links()
            .iter()
            .map(|l| l.rel())
            .collect();
        assert_eq!(rels, vec!["z"]);
        assert!(book.dangling_links().is_empty());
        assert_eq!(book.link_count(), 1);
    }

    #[test]
    fn backlink_index_matches_a_per_node_scan() {
        let mut book = make_book();
        let a = add(&mut book, None, "A");
        let b = add(&mut book, None, "B");
        let b1 = add(&mut book, Some(b), "B1");
        let c = add(&mut book, None, "C");
        book.add_link(a, b, "supersedes").unwrap();
        book.add_link(c, b, "see_also").unwrap();
        book.add_link(b1, a, "see_also").unwrap();
        book.add_link(a, b1, "x").unwrap();
        book.add_link(c, a, "y").unwrap();

        let index = book.backlink_index();
        for id in book.all_node_ids().collect::<Vec<_>>() {
            let scan: Vec<(NodeId, String)> = book
                .backlinks(id)
                .into_iter()
                .map(|(s, l)| (s, l.rel().to_string()))
                .collect();
            let indexed: Vec<(NodeId, String)> = index
                .get(&id)
                .map(|v| v.iter().map(|(s, l)| (*s, l.rel().to_string())).collect())
                .unwrap_or_default();
            assert_eq!(indexed, scan, "backlinks of {id}");
        }
        assert!(!index.contains_key(&c), "C has no incoming links");
    }

    #[test]
    fn absent_and_empty_body_are_the_same_content() {
        let mut book = make_book();
        let id = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Bare".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .unwrap();
        let before = content_fingerprint(book.get_node(id).unwrap());
        let mut with_empty = book.get_node(id).unwrap().clone();
        with_empty.set_body(Some(String::new()));
        assert_eq!(content_fingerprint(&with_empty), before);

        let empty_body = Revision {
            title: None,
            body: Some(BodyChange::Set(String::new())),
            properties: None,
            properties_patch: None,
            base: String::new(),
            by: None,
            note: None,
            at: Timestamp::now(),
        };
        assert!(matches!(
            book.propose_revision(id, empty_body, false),
            Err(DomainError::RevisionChangesNothing(_))
        ));
        assert!(book.get_node(id).unwrap().proposal().is_none());
    }
}
