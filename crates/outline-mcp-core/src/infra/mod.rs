/// ai-store facade-backed `ChangeLogRepository` implementation (sibling to
/// `changelog_store`). Read by `shelf_import` only.
pub mod ai_store_changelog;
/// `ChangeLogRepository` bridging the legacy JSON changelog format to the
/// ai-store-backed one via a read-time merge. Read by `shelf_import` only.
pub mod changelog_bridge;
/// JSON-file-backed `ChangeLogRepository` implementation (pre-Postgres shelf).
pub mod changelog_store;
/// JSON-file-backed `BookRepository` implementation (pre-Postgres shelf).
pub mod json_store;
/// `BookStore` over the pre-port book file + changelog (non-atomic; tests and
/// reading old shelves only).
pub mod legacy_shelf_store;
/// In-memory `BookStore` / `SnapshotStore` (tests, embedding).
pub mod memory_store;
/// Postgres `BookStore` / `SnapshotStore`: book rows and their events in one
/// transaction.
pub mod pg_store;
/// `import-shelf`: bring a pre-Postgres shelf directory into the stores.
pub mod shelf_import;
