/// `TemplateBook` aggregate root and node add/update request types.
pub mod book;
/// ChangeLog entry types (`ChangeEntry`, `ChangeAction`, `NodeStatus`).
pub mod changelog;
/// `BookId` / `NodeId` value objects.
pub mod id;
/// `TemplateNode` and `NodeType`.
pub mod node;
/// `Timestamp` value object (Unix millis, ISO 8601 serde).
pub mod timestamp;
