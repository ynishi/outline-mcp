/// Domain error types (`DomainError`).
pub mod error;
/// Read-only book health checks: `lint` and `stats`.
pub mod health;
/// Domain model types: `TemplateBook`, nodes, IDs, changelog entries.
pub mod model;
/// Persistence-abstraction traits implemented by the `infra` layer.
pub mod repository;
/// Case-insensitive text search over node titles and bodies.
pub mod search;
/// The `BookStore` port: one commit of a book's state and its change events.
pub mod store;
