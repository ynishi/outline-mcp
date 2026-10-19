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

    /// A node cannot link to itself.
    #[error("node {0} cannot link to itself")]
    SelfLink(NodeId),

    /// A link type must be 1–40 characters of `a-z`, `0-9`, `_`, `-`.
    #[error(
        "invalid link type {0:?}: use 1-40 characters of a-z, 0-9, '_' or '-' (e.g. supersedes, see_also)"
    )]
    InvalidLinkRel(String),

    /// A revision must change at least one of title, body, properties.
    #[error("a revision must change the title, the body or the properties")]
    EmptyRevision,

    /// The proposed revision would leave the node exactly as it is.
    #[error("node {0}: the revision would change nothing")]
    RevisionChangesNothing(NodeId),

    /// The node already has a pending revision.
    #[error("node {0} already has a pending revision; review it, or propose with replace: true")]
    ProposalExists(NodeId),

    /// The node has no pending revision.
    #[error("node {0} has no pending revision")]
    NoProposal(NodeId),

    /// The node changed after the revision was proposed.
    #[error(
        "node {0} changed after the revision was proposed; review it with revision_show, \
         then approve with force: true or reject it"
    )]
    StaleProposal(NodeId),

    /// Revisions are not proposed for, or applied to, archived nodes.
    #[error("node {0} is archived (or under an archived section); revisions do not apply to it")]
    RevisionOnArchived(NodeId),

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
