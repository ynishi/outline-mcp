use super::model::id::NodeId;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("node not found: {0}")]
    NodeNotFound(NodeId),

    #[error("max depth {max} exceeded at node {node_id}")]
    MaxDepthExceeded { node_id: NodeId, max: u8 },

    #[error("cannot move node {0} under its own descendant")]
    CyclicMove(NodeId),
}
