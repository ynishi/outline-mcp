use serde::{Deserialize, Serialize};
use std::fmt;

/// Unique identifier for a `TemplateBook`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BookId(uuid::Uuid);

impl Default for BookId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl BookId {
    /// Generate a new random `BookId` (UUID v4).
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a stored UUID (storage adapters only).
    pub(crate) fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }

    /// The underlying UUID (storage adapters only).
    pub(crate) fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl fmt::Display for BookId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a `TemplateNode` within a book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(uuid::Uuid);

impl Default for NodeId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl NodeId {
    /// Generate a new random `NodeId` (UUID v4).
    pub fn new() -> Self {
        Self::default()
    }

    /// The id history entries about the book itself carry (the nil UUID):
    /// no node ever has it, since node ids are random v4 UUIDs.
    pub const fn book_level() -> Self {
        Self(uuid::Uuid::nil())
    }

    /// Whether this is [`Self::book_level`].
    pub fn is_book_level(&self) -> bool {
        self.0.is_nil()
    }

    /// 短縮ID（UUIDの先頭8文字）
    pub fn short(&self) -> String {
        self.0.to_string()[..8].to_string()
    }

    /// Wrap a stored UUID (storage adapters only).
    pub(crate) fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }

    /// The underlying UUID (storage adapters only).
    pub(crate) fn as_uuid(&self) -> uuid::Uuid {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
