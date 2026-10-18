use crate::domain::error::DomainError;

/// Application-layer errors surfaced by `BookService` and `EjectService`.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A domain invariant was violated (propagated from `TemplateBook`).
    #[error(transparent)]
    Domain(#[from] DomainError),

    /// No book has been created / loaded yet.
    #[error("book not found: initialize first")]
    BookNotFound,

    /// The underlying store failed to load or commit.
    #[error("storage error: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Another writer committed the book between this operation's read and
    /// its commit; nothing was stored. Re-reading and retrying is safe.
    #[error(
        "the book changed while this write was in progress (expected version {expected:?}, found {actual:?}); nothing was stored — read it again and retry"
    )]
    Conflict {
        /// The version this operation read.
        expected: Option<i64>,
        /// The version found at commit time.
        actual: Option<i64>,
    },

    /// File I/O failed while ejecting the book to disk.
    #[error("eject I/O error: {0}")]
    EjectIo(#[source] std::io::Error),

    /// An imported JSON tree contained an unrecognized node type.
    #[error("import: invalid node type: {0}")]
    ImportInvalidType(String),

    /// A snapshot operation failed (not found / I/O / serde).
    #[error("snapshot error: {0}")]
    Snapshot(String),
}
