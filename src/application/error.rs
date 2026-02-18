use crate::domain::error::DomainError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Domain(#[from] DomainError),

    #[error("book not found: initialize first")]
    BookNotFound,

    #[error("storage error: {0}")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("eject I/O error: {0}")]
    EjectIo(#[source] std::io::Error),

    #[error("import: invalid node type: {0}")]
    ImportInvalidType(String),
}
