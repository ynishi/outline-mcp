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

    /// An update supplied both `properties` (replace) and `properties_patch`
    /// (merge). Applying both would make the result depend on their order, so
    /// the caller has to say which one it means.
    #[error(
        "node {0}: `properties` (replace) and `properties_patch` (merge) are mutually exclusive"
    )]
    ConflictingPropertyUpdate(NodeId),

    /// A book title must not be empty (or whitespace only).
    #[error("book title must not be empty")]
    EmptyBookTitle,

    /// A book's max depth must be at least 1.
    #[error("max depth must be at least 1")]
    InvalidMaxDepth,

    /// Lowering the max depth would leave an existing node deeper than the
    /// new limit.
    #[error(
        "max depth {requested} is below node \"{title}\" ({node_id}) at depth {depth}; \
         move or remove it first"
    )]
    MaxDepthBelowExistingNode {
        /// The requested max depth.
        requested: u8,
        /// The deepest existing node.
        node_id: NodeId,
        /// That node's title.
        title: String,
        /// That node's depth (root = 1).
        depth: u8,
    },
}
