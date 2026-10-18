/// ai-store facade-backed `ChangeLogRepository` implementation (sibling to `changelog_store`).
pub mod ai_store_changelog;
/// `ChangeLogRepository` bridging the legacy JSON changelog format to the
/// ai-store-backed one via a read-time merge (no dedicated migration step).
pub mod changelog_bridge;
/// JSON-file-backed `ChangeLogRepository` implementation.
pub mod changelog_store;
/// JSON-file-backed `BookRepository` implementation.
pub mod json_store;
/// `BookStore` over the pre-port book file + changelog (non-atomic, kept for
/// the running server until books move to an atomic store).
pub mod legacy_shelf_store;
/// In-memory `BookStore` (tests, embedding).
pub mod memory_store;
/// Postgres `BookStore`: book rows and their events in one transaction.
pub mod pg_store;
/// Snapshot creation / listing / restore service.
pub mod snapshot;
/// One-shot migrator for pre-`ai-store` on-disk snapshot dumps, plus an
/// exact orphan count for startup warnings.
pub mod snapshot_migrator;
/// `SyncProjectionSink` that persists book-level snapshot dumps for `snapshot`.
pub mod snapshot_sink;
