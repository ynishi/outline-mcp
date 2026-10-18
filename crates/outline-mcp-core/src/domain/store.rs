use std::sync::Arc;

use async_trait::async_trait;

use super::model::book::TemplateBook;
use super::model::changelog::ChangeEntry;
use super::model::id::NodeId;
use super::model::timestamp::Timestamp;

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

#[async_trait]
impl<T: BookStore + ?Sized> BookStore for Arc<T> {
    async fn load(&self, slug: &str) -> Result<Option<Versioned<TemplateBook>>, StoreError> {
        (**self).load(slug).await
    }

    async fn commit(
        &self,
        slug: &str,
        expected_version: Option<i64>,
        book: &TemplateBook,
        events: &[ChangeEntry],
    ) -> Result<Committed, StoreError> {
        (**self).commit(slug, expected_version, book, events).await
    }

    async fn history(&self, slug: &str) -> Result<Vec<ChangeEntry>, StoreError> {
        (**self).history(slug).await
    }

    async fn history_by_node(
        &self,
        slug: &str,
        node_id: NodeId,
    ) -> Result<Vec<ChangeEntry>, StoreError> {
        (**self).history_by_node(slug, node_id).await
    }

    async fn list_books(&self) -> Result<Vec<BookSummary>, StoreError> {
        (**self).list_books().await
    }
}

/// One stored snapshot, as listed (the book itself is read with
/// [`SnapshotStore::get`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// When the snapshot was taken. Unique per book: it is the key
    /// `snapshot_restore` / `snapshot_tag` / `snapshot_dump` take.
    pub timestamp: Timestamp,
    /// The label attached at creation or by [`SnapshotStore::tag`].
    pub label: Option<String>,
    /// Size of the stored book JSON, in bytes.
    pub size_bytes: u64,
}

/// Whole-book snapshots, keyed by `(slug, timestamp)`.
///
/// A snapshot is a copy of the book at one moment. Restoring one is not this
/// port's business: the caller reads it with [`SnapshotStore::get`] and
/// commits it through [`BookStore`], so the restore lands in the history like
/// any other write.
#[async_trait]
pub trait SnapshotStore: Send + Sync {
    /// Store a snapshot of `book` taken now. When a snapshot of this book
    /// already has the current millisecond, the new one takes the next free
    /// one, so timestamps stay unique.
    async fn create(
        &self,
        slug: &str,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError>;

    /// Store a snapshot taken at `timestamp` (an import keeps the original
    /// moment). Fails with [`StoreError::Conflict`] if that timestamp is taken.
    async fn insert(
        &self,
        slug: &str,
        timestamp: Timestamp,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError>;

    /// The book's snapshots, newest first.
    async fn list(&self, slug: &str) -> Result<Vec<SnapshotInfo>, StoreError>;

    /// The book stored in one snapshot, or `None` if there is none at
    /// `timestamp`.
    async fn get(
        &self,
        slug: &str,
        timestamp: Timestamp,
    ) -> Result<Option<TemplateBook>, StoreError>;

    /// Set (or replace) a snapshot's label. Returns `false` if there is no
    /// snapshot at `timestamp`.
    async fn tag(&self, slug: &str, timestamp: Timestamp, label: &str) -> Result<bool, StoreError>;
}

#[async_trait]
impl<T: SnapshotStore + ?Sized> SnapshotStore for Arc<T> {
    async fn create(
        &self,
        slug: &str,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError> {
        (**self).create(slug, book, label).await
    }

    async fn insert(
        &self,
        slug: &str,
        timestamp: Timestamp,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError> {
        (**self).insert(slug, timestamp, book, label).await
    }

    async fn list(&self, slug: &str) -> Result<Vec<SnapshotInfo>, StoreError> {
        (**self).list(slug).await
    }

    async fn get(
        &self,
        slug: &str,
        timestamp: Timestamp,
    ) -> Result<Option<TemplateBook>, StoreError> {
        (**self).get(slug, timestamp).await
    }

    async fn tag(&self, slug: &str, timestamp: Timestamp, label: &str) -> Result<bool, StoreError> {
        (**self).tag(slug, timestamp, label).await
    }
}
