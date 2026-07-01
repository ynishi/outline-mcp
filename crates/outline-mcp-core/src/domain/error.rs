use super::model::id::NodeId;

/// Errors raised by `TemplateBook` mutations (add / update / move / remove).
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// The referenced node does not exist in the book.
    #[error("node not found: {0}")]
    NodeNotFound(NodeId),

    /// Adding or moving a node would exceed the book's configured max depth.
    #[error("max depth {max} exceeded at node {node_id}")]
    MaxDepthExceeded {
        /// The node that would exceed the depth limit.
        node_id: NodeId,
        /// The book's configured maximum depth.
        max: u8,
    },

    /// A move would place a node under one of its own descendants.
    #[error("cannot move node {0} under its own descendant")]
    CyclicMove(NodeId),
}
