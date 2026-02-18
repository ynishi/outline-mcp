use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::id::{BookId, NodeId};
use super::node::{NodeType, TemplateNode};
use crate::domain::error::DomainError;

/// ノード追加リクエスト
pub struct AddNodeRequest {
    pub parent: Option<NodeId>,
    pub title: String,
    pub node_type: NodeType,
    pub body: Option<String>,
    pub placeholder: Option<String>,
    /// 兄弟内での挿入位置（末尾ならusize::MAX）
    pub position: usize,
}

/// ノード更新リクエスト（Noneのフィールドは変更しない）
pub struct UpdateNodeRequest {
    pub title: Option<String>,
    pub body: Option<Option<String>>,
    pub node_type: Option<NodeType>,
    pub placeholder: Option<Option<String>>,
}

/// Template Book — 集約ルート。全ノード操作はここを経由する。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateBook {
    id: BookId,
    title: String,
    max_depth: u8,
    nodes: HashMap<NodeId, TemplateNode>,
    root_nodes: Vec<NodeId>,
}

impl TemplateBook {
    pub fn new(title: impl Into<String>, max_depth: u8) -> Self {
        Self {
            id: BookId::new(),
            title: title.into(),
            max_depth,
            nodes: HashMap::new(),
            root_nodes: Vec::new(),
        }
    }

    pub fn id(&self) -> BookId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn max_depth(&self) -> u8 {
        self.max_depth
    }

    pub fn root_nodes(&self) -> &[NodeId] {
        &self.root_nodes
    }

    pub fn get_node(&self, id: NodeId) -> Option<&TemplateNode> {
        self.nodes.get(&id)
    }

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

        Ok(())
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
        self.nodes.remove(&id);
        for desc_id in descendants {
            self.nodes.remove(&desc_id);
        }

        Ok(())
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
            })
            .unwrap();

        assert_eq!(book.root_nodes().len(), 1);
        assert_eq!(book.get_node(id).unwrap().title(), "Design");
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
            })
            .unwrap();

        let result = book.add_node(AddNodeRequest {
            parent: Some(l2),
            title: "L3 - too deep".into(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
        });

        assert!(matches!(
            result,
            Err(DomainError::MaxDepthExceeded { max: 2, .. })
        ));
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

        let child = book
            .add_node(AddNodeRequest {
                parent: Some(a),
                title: "Task".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
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
            })
            .unwrap();

        book.update_node(
            id,
            UpdateNodeRequest {
                title: Some("New".into()),
                body: Some(Some("description".into())),
                node_type: Some(NodeType::Content),
                placeholder: None,
            },
        )
        .unwrap();

        let node = book.get_node(id).unwrap();
        assert_eq!(node.title(), "New");
        assert_eq!(node.body(), Some("description"));
        assert_eq!(*node.node_type(), NodeType::Content);
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
            })
            .unwrap();

        let all = book.all_nodes_dfs();
        let ids: Vec<NodeId> = all.iter().map(|n| n.id()).collect();
        assert_eq!(ids, vec![a, a1, a2, b]);
    }
}
