use async_trait::async_trait;

use super::model::book::TemplateBook;
use super::model::changelog::ChangeEntry;
use super::model::id::NodeId;

/// A stored value together with the version it was read at.
///
/// The version is what [`BookStore::commit`] compares against, so a writer
/// that read version `n` either commits `n + 1` or learns that someone else
/// committed first.
#[derive(Debug, Clone)]
pub struct Versioned<T> {
    /// The stored value.
    pub value: T,
    /// The version of `value` in the store.
    pub version: i64,
}

/// The result of a successful [`BookStore::commit`].
#[derive(Debug, Clone)]
pub struct Committed {
    /// The book's version after the commit.
    pub version: i64,
    /// Non-fatal problems the store reports about the commit.
    ///
    /// Always empty for an atomic store: if the events could not be recorded
    /// the commit fails instead. Only the legacy shelf adapter, which writes
    /// the book and its events separately, fills it.
    pub warnings: Vec<String>,
}

/// One entry of [`BookStore::list_books`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookSummary {
    /// The book's slug (its key in the store).
    pub slug: String,
    /// The book's title.
    pub title: String,
}

/// Errors a [`BookStore`] reports.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The book's stored version is not the one the caller read.
    ///
    /// `expected: None` means the caller meant to create the book; `actual:
    /// None` means the book does not exist.
    #[error("version conflict: expected {expected:?}, found {actual:?}")]
    Conflict {
        /// The version the caller committed against.
        expected: Option<i64>,
        /// The version currently stored.
        actual: Option<i64>,
    },
    /// The backend failed (connection, query, serialization).
    #[error("store backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl StoreError {
    /// Wrap any backend error.
    pub fn backend(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Backend(e.into())
    }
}

/// Storage port where one write is one commit of a book's state together
/// with the change events that produced it.
///
/// An implementation either stores both or neither, so the history can never
/// silently fall behind the tree. Every call names the book by `slug`.
#[async_trait]
pub trait BookStore: Send + Sync {
    /// Load a book and its version, or `None` if it does not exist.
    async fn load(&self, slug: &str) -> Result<Option<Versioned<TemplateBook>>, StoreError>;

    /// Store `book` and append `events`, in one commit.
    ///
    /// `expected_version: None` creates the book and fails with
    /// [`StoreError::Conflict`] if it already exists; `Some(v)` replaces it
    /// and fails with a conflict unless its stored version is `v`.
    async fn commit(
        &self,
        slug: &str,
        expected_version: Option<i64>,
        book: &TemplateBook,
        events: &[ChangeEntry],
    ) -> Result<Committed, StoreError>;

    /// Every recorded event of a book, oldest first.
    async fn history(&self, slug: &str) -> Result<Vec<ChangeEntry>, StoreError>;

    /// The recorded events of one node, oldest first.
    async fn history_by_node(
        &self,
        slug: &str,
        node_id: NodeId,
    ) -> Result<Vec<ChangeEntry>, StoreError>;

    /// Every stored book, ordered by slug.
    async fn list_books(&self) -> Result<Vec<BookSummary>, StoreError>;
}
