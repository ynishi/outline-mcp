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

    /// The underlying `BookRepository` failed to load or save.
    #[error("storage error: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),

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
