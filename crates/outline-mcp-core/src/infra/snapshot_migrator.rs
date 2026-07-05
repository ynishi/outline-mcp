//! One-shot migrator for the pre-`ai-store` `{slug}.snap.{millis}.json`
//! snapshot dump convention, plus an exact orphan count for startup
//! warnings.
//!
//! # Architecture
//!
//! Before `crate::infra::snapshot::SnapshotService` existed, a book's
//! snapshot files were plain filesystem dumps with no backing event log —
//! restoring one only ever worked by reading the file directly. Now that
//! `SnapshotService::restore` reconstructs state through an ai-store
//! [`Store`] (`crate::infra::snapshot::snapshot_stream_key`'s dedicated
//! stream), a legacy dump that was never appended to that stream is an
//! **orphan**: still readable off disk by `crate::infra::snapshot::list_snapshots`
//! (a pure filesystem scan), but not resolvable through `Store::seq_at_time`
//! / `Store::state_at`.
//!
//! [`migrate_snapshots`] closes that gap by reading each orphaned file and
//! importing its content into the stream via [`Store::import_event`],
//! preserving the file's own millis as the imported event's `Timestamp` (see
//! next section). [`count_orphan_snapshots`] is an exact count of how many
//! such files remain.
//!
//! ## Historical timestamps are preserved via `Store::import_event`
//!
//! [`Store::append`] always has the backend stamp `at` as wall-clock *now*.
//! `ai-store-core` 0.4 adds [`Store::import_event`] specifically for
//! import/migration paths: the caller supplies `at` directly, and the
//! backend records exactly that value instead of "now" (both
//! `ai_store_sqlite` and `ai_store_mem` override
//! `ai_store_core::EventBackend::import_event` to honor it; a backend that
//! has not is signaled via `StoreError::BackendUnsupported` — see
//! [`migrate_snapshots`]'s handling of that case). [`migrate_snapshots`]
//! calls it with `at` set to the legacy file's own millis, so:
//!
//! - `SnapshotService::restore(M)` (`M` the *original* file's millis, as
//!   `SnapshotService::list` still reports it) now succeeds after migration
//!   — there **is** an event at exactly `M` reconstructing to this content.
//! - `crate::infra::snapshot_sink::SnapshotDumpSink`, registered on `store`,
//!   dumps every append on the snapshot stream to disk keyed by the
//!   *event's* `at` (see that module's `commit`). Because the imported
//!   event's `at` equals the source file's own millis, this dump lands at
//!   the **same path** the legacy file already occupied — rewriting it with
//!   (semantically) identical content rather than materializing a second
//!   file. A migration run no longer doubles the on-disk file count the way
//!   a wall-clock-stamped import would.
//!
//! ## Precondition: only backfill an empty stream
//!
//! [`Store::import_event`]'s contract (see its doc comment) only guarantees
//! `Store::seq_at_time` behaves intuitively when a stream's `at` values are
//! non-decreasing in `seq` order — true by construction when backfilling
//! into an **empty** stream in chronological order (what
//! [`migrate_snapshots`] does: files are sorted oldest-first before any
//! import), but not guaranteed if the stream already carries events from
//! some other clock (ordinary `Store::append` domain writes, or an
//! unrelated import). [`migrate_snapshots`] therefore checks `Store::head`
//! before importing anything **new**: if there is at least one
//! not-yet-accounted-for disk file (see idempotency below) and the stream
//! is non-empty, the entire batch is refused — a single entry is added to
//! [`MigrationReport::failed`] and nothing is imported this run. The
//! failure is one entry for the whole run rather than one per file, because
//! the risk (a mixed-clock stream) applies to the stream as a whole, not to
//! any individual file.
//!
//! This check is skipped whenever every disk file is already accounted for
//! (see idempotency below): a stream holding only prior *migrated* imports,
//! with nothing new left to backfill, is not itself the risk this guards
//! against, so re-running [`migrate_snapshots`] over an already-fully
//! -migrated slug still succeeds as a no-op.
//!
//! ## Idempotency across repeated runs
//!
//! Because the imported event's `at` is now literally the source file's own
//! millis (previous section), "has this file already been imported?"
//! reduces to a single check: does any existing event on the stream carry
//! that millis as its own `Timestamp`? A pre-γ' version of this module
//! needed a second axis — a `meta.orig_millis` marker — because imports
//! back then were wall-clock-stamped and so never equaled the source
//! file's millis on their own. [`migrate_snapshots`] builds this set once
//! per run from `Store::read`'s full history and skips any file whose
//! millis is already in it.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai_store_core::{
    CacheBackend, EventBackend, Label, Seq, Store, StoreConfig, StoreError,
    Timestamp as StoreTimestamp,
};
use serde_json::Value;

use crate::domain::model::book::TemplateBook;
use crate::domain::model::timestamp::Timestamp;
use crate::infra::snapshot::{list_snapshots, snapshot_path, snapshot_stream_key, SnapshotInfo};
use crate::infra::snapshot_sink::SnapshotOnlySink;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// `Event.kind` for a snapshot appended by [`migrate_snapshots`]. Distinct
/// from `crate::infra::snapshot`'s `"book_snapshot"` purely for provenance
/// in `Store::read` output — `SnapshotDumpSink::commit` dispatches by
/// stream identity, not `kind`, so this does not change dump behavior.
const KIND_SNAPSHOT_IMPORTED: &str = "snapshot_imported";

fn box_store_err(e: StoreError) -> BoxError {
    Box::new(e)
}

/// Outcome of a single [`import_one`] call. Distinguishes a systemic backend
/// incapability — every remaining pending file would fail identically —
/// from a problem specific to this one file, so [`migrate_snapshots`] knows
/// whether to keep processing the rest of the batch.
enum ImportError {
    /// The backend declined [`Store::import_event`] outright
    /// (`StoreError::BackendUnsupported`). Carries the backend's message.
    BackendUnsupported(String),
    /// A problem specific to this file (invalid JSON, a shape mismatch
    /// against [`TemplateBook`], or an append-time gate/schema failure).
    Other(BoxError),
}

/// Outcome of one [`migrate_snapshots`] run.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MigrationReport {
    /// Total number of on-disk snapshot files matched by the
    /// `{slug}.snap.{millis}.json` naming pattern.
    pub scanned: usize,
    /// Files newly imported into the stream during this run.
    pub imported: usize,
    /// Files already accounted for (see module docs) and left untouched.
    pub skipped: usize,
    /// Files that could not be imported, paired with the error that stopped
    /// each one. One corrupt legacy file does not abort the rest of the
    /// run; a stream-wide precondition failure (non-empty stream, or a
    /// backend that does not support `Store::import_event`) contributes a
    /// single entry here and stops the run instead.
    pub failed: Vec<(PathBuf, String)>,
}

impl fmt::Display for MigrationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  scanned:  {}", self.scanned)?;
        writeln!(f, "  imported: {}", self.imported)?;
        writeln!(f, "  skipped:  {}", self.skipped)?;
        write!(f, "  failed:   {}", self.failed.len())?;
        if self.failed.is_empty() {
            writeln!(f)
        } else {
            writeln!(f)?;
            for (path, err) in &self.failed {
                writeln!(f, "    - {}: {err}", path.display())?;
            }
            Ok(())
        }
    }
}

/// Scans `{shelf_dir}/{slug}.snap.{millis}.json` disk files and imports each
/// not-yet-accounted-for one into `store`'s dedicated snapshot stream (see
/// `crate::infra::snapshot::snapshot_stream_key`), preserving each file's own
/// millis as the imported event's `Timestamp`. See module docs for the
/// precondition and idempotency contract this function guarantees.
///
/// `store` must have a `crate::infra::snapshot_sink::SnapshotDumpSink`
/// registered for imports to have any observable effect on disk — the same
/// precondition `SnapshotService::new` documents, since both types append to
/// the same stream.
///
/// # Errors
///
/// Returns `Err` only when the run cannot meaningfully proceed at all (e.g.
/// `shelf_dir` cannot be listed, or the stream's existing events cannot be
/// read). Per-file problems, a non-empty-stream precondition failure, and a
/// backend that does not support `Store::import_event` are all collected
/// into [`MigrationReport::failed`] instead (see that field's doc comment
/// for how each shows up).
pub async fn migrate_snapshots(
    shelf_dir: &Path,
    slug: &str,
    store: Arc<Store>,
) -> Result<MigrationReport, BoxError> {
    let stream = ai_store_core::StreamId::new(snapshot_stream_key(slug));

    let mut infos = list_snapshots(shelf_dir, slug)?;
    // Oldest first: importing in this order into an initially-empty stream
    // keeps `at` non-decreasing in `seq` order, which `Store::seq_at_time`
    // requires for intuitive results (see module docs).
    infos.sort_by_key(|info| info.timestamp);

    let existing = store
        .read(&stream, Seq::ZERO, usize::MAX)
        .await
        .map_err(box_store_err)?;
    let accounted_for: HashSet<i64> = existing.iter().map(|e| e.at.0).collect();

    let mut report = MigrationReport {
        scanned: infos.len(),
        ..Default::default()
    };

    let mut pending: Vec<&SnapshotInfo> = Vec::new();
    for info in &infos {
        if accounted_for.contains(&info.timestamp.as_millis()) {
            report.skipped += 1;
        } else {
            pending.push(info);
        }
    }

    if pending.is_empty() {
        return Ok(report);
    }

    // Precondition: refuse to backfill historical timestamps into a stream
    // that is not empty (see module docs for why this check is scoped to
    // "there is something new to import" rather than every run).
    if let Some(head) = store.head(&stream).await.map_err(box_store_err)? {
        report.failed.push((
            shelf_dir.to_path_buf(),
            format!(
                "stream head is at seq {} (non-empty); refusing to import {} historical-timestamp file(s) into a mixed-clock stream",
                head.0,
                pending.len(),
            ),
        ));
        return Ok(report);
    }

    for info in pending {
        match import_one(
            &stream,
            &store,
            shelf_dir,
            slug,
            info.timestamp.as_millis(),
            info.label.as_deref(),
        )
        .await
        {
            Ok(()) => report.imported += 1,
            Err(ImportError::BackendUnsupported(msg)) => {
                report.failed.push((
                    info.path.clone(),
                    format!(
                        "backend does not support import_event ({msg}); aborting remaining imports"
                    ),
                ));
                // Every remaining pending file would fail identically (same
                // backend, same missing capability) — stop instead of
                // repeating the same error once per file.
                break;
            }
            Err(ImportError::Other(e)) => report.failed.push((info.path.clone(), e.to_string())),
        }
    }

    Ok(report)
}

/// Imports the single disk file at `{shelf_dir}/{slug}.snap.{millis}.json`,
/// preserving `millis` as the imported event's [`StoreTimestamp`] via
/// [`Store::import_event`] (see module docs).
async fn import_one(
    stream: &ai_store_core::StreamId,
    store: &Arc<Store>,
    shelf_dir: &Path,
    slug: &str,
    millis: i64,
    label: Option<&str>,
) -> Result<(), ImportError> {
    let path = snapshot_path(shelf_dir, slug, millis);
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| ImportError::Other(Box::new(e)))?;
    let raw: Value = serde_json::from_str(&content).map_err(|e| ImportError::Other(Box::new(e)))?;
    // Validate the legacy dump actually deserializes as a `TemplateBook`
    // (what `SnapshotService::restore` will expect of it) before writing it
    // into the store — a structurally-invalid dump should surface as a
    // `MigrationReport::failed` entry now, not a silently-broken restore
    // later.
    let _: TemplateBook =
        serde_json::from_value(raw.clone()).map_err(|e| ImportError::Other(Box::new(e)))?;

    let current = store
        .state(stream)
        .await
        .map_err(|e| ImportError::Other(box_store_err(e)))?;
    let patch = json_patch::diff(&current, &raw);

    let mut meta = serde_json::Map::new();
    meta.insert(
        "label".to_string(),
        label
            .map(|l| Value::String(l.to_string()))
            .unwrap_or(Value::Null),
    );
    meta.insert(
        "imported_at".to_string(),
        Value::from(Timestamp::now().as_millis()),
    );

    let committed = store
        .import_event(
            stream,
            KIND_SNAPSHOT_IMPORTED,
            patch,
            Value::Object(meta),
            StoreTimestamp(millis),
        )
        .await
        .map_err(|e| match e {
            StoreError::BackendUnsupported(msg) => ImportError::BackendUnsupported(msg),
            other => ImportError::Other(box_store_err(other)),
        })?;

    // Best-effort mirror into the store's own label registry, matching
    // `SnapshotService::tag`'s convention: the sidecar `.meta.json`'s
    // `label` (written by `SnapshotDumpSink::commit` reading it straight
    // off the just-imported `Event::meta`, no separate call needed) is this
    // service's source of truth for `list`/`tag`, so a `label_set` failure
    // here must not roll back the import itself.
    if let Some(label) = label {
        let _ = store
            .label_set(stream, &Label::new(label), committed.seq)
            .await;
    }

    Ok(())
}

/// Exact count of `slug`'s on-disk snapshot files that have not yet been
/// imported into `store`'s dedicated snapshot stream by [`migrate_snapshots`].
///
/// Since [`migrate_snapshots`] preserves each imported file's original
/// millis as the event's own [`StoreTimestamp`] (see module docs) rather
/// than stamping a new wall-clock time, "imported" and "not yet imported"
/// reduce to a plain set difference: a disk file's millis is imported iff
/// some event on the stream carries that exact `Timestamp`. This is the
/// same set [`migrate_snapshots`] itself would skip, so this function's
/// result is exact — not an over-estimate — both before and after any
/// number of migration runs.
pub async fn count_orphan_snapshots(
    shelf_dir: &Path,
    slug: &str,
    store: Arc<Store>,
) -> Result<usize, BoxError> {
    let disk: HashSet<i64> = list_snapshots(shelf_dir, slug)?
        .into_iter()
        .map(|info| info.timestamp.as_millis())
        .collect();
    let stream = ai_store_core::StreamId::new(snapshot_stream_key(slug));
    let imported: HashSet<i64> = store
        .read(&stream, Seq::ZERO, usize::MAX)
        .await
        .map_err(box_store_err)?
        .iter()
        .map(|e| e.at.0)
        .collect();
    Ok(disk.difference(&imported).count())
}

/// One-shot entry point for the `outline-mcp migrate-snapshots` CLI
/// subcommand: opens (or creates) `slug`'s ai-store SQLite backend under
/// `shelf_dir` — the same `{shelf_dir}/{slug}.events.db` file and
/// `SnapshotDumpSink` wiring `OutlineMcpServer::store_for` uses — migrates
/// its legacy on-disk snapshot files, then lets the backend close.
///
/// Unlike `migrate_snapshots`, callers do not need to construct their own
/// `Store` (or open `ai_store_sqlite::SqliteBackends` themselves) first;
/// this is the function a one-shot CLI process calls directly, once per
/// slug it wants to migrate.
pub async fn migrate_slug(shelf_dir: &Path, slug: &str) -> Result<MigrationReport, BoxError> {
    tokio::fs::create_dir_all(shelf_dir).await?;
    let db_path = shelf_dir.join(format!("{slug}.events.db"));
    let backends = ai_store_sqlite::SqliteBackends::open(&db_path)
        .await
        .map_err(box_store_err)?;
    let events: Arc<dyn EventBackend> = Arc::new(backends.events);
    let cache: Arc<dyn CacheBackend> = Arc::new(backends.cache);
    let sink = SnapshotOnlySink::new(shelf_dir.to_path_buf(), slug.to_string());
    let store = Arc::new(Store::new(
        events,
        cache,
        Vec::new(),
        vec![Arc::new(sink)],
        StoreConfig::default(),
    ));

    let report = migrate_snapshots(shelf_dir, slug, store).await;
    // `backends.driver` (bound above, still in scope) keeps the SQLite
    // background thread alive for every `.await` in `migrate_snapshots` —
    // it is only dropped once this function returns. A one-shot CLI
    // invocation has no further use for it afterward, so no explicit
    // `.shutdown()` is needed (see `OutlineMcpServer`'s `SnapshotStoreEntry`
    // doc comment for why dropping without it is documented as safe).
    drop(backends.driver);
    report
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::AddNodeRequest;
    use crate::domain::model::node::NodeType;
    use crate::infra::snapshot::{write_meta, write_snapshot_body, SnapshotMeta, SnapshotService};
    use crate::infra::snapshot_sink::SnapshotDumpSink;
    use ai_store_core::{Committed, Event, NewEvent, StreamId};
    use ai_store_mem::{MemCacheBackend, MemEventBackend};
    use ai_store_sync::BlockingSink;
    use async_trait::async_trait;
    use std::collections::HashMap;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("outline-mcp-migrator-test-{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_book(title: &str) -> TemplateBook {
        let mut book = TemplateBook::new(title, 3);
        book.add_node(AddNodeRequest {
            parent: None,
            title: "Node 1".into(),
            node_type: NodeType::Content,
            body: Some("body text".into()),
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        })
        .expect("add node");
        book
    }

    fn make_store(shelf_dir: &Path, slug: &str) -> Arc<Store> {
        let events: Arc<dyn EventBackend> = Arc::new(MemEventBackend::new());
        let cache: Arc<dyn CacheBackend> = Arc::new(MemCacheBackend::new());
        let sink = SnapshotDumpSink::new(shelf_dir.to_path_buf(), slug.to_string());
        Arc::new(Store::new(
            events,
            cache,
            Vec::new(),
            vec![Arc::new(BlockingSink::new(sink))],
            StoreConfig::default(),
        ))
    }

    /// Wraps an [`EventBackend`] but deliberately does not override
    /// `import_event`, so the trait's default (`StoreError::BackendUnsupported`)
    /// applies — simulates a backend that has not opted into historical
    /// timestamp injection, without depending on a specific third-party
    /// backend never adding support.
    struct NoImportEventBackend(Arc<dyn EventBackend>);

    #[async_trait]
    impl EventBackend for NoImportEventBackend {
        async fn append(&self, stream: &StreamId, rec: NewEvent) -> Result<Committed, StoreError> {
            self.0.append(stream, rec).await
        }
        async fn read(
            &self,
            stream: &StreamId,
            from: Seq,
            limit: usize,
        ) -> Result<Vec<Event>, StoreError> {
            self.0.read(stream, from, limit).await
        }
        async fn head(&self, stream: &StreamId) -> Result<Option<Seq>, StoreError> {
            self.0.head(stream).await
        }
        async fn seq_at_time(
            &self,
            stream: &StreamId,
            at: StoreTimestamp,
        ) -> Result<Option<Seq>, StoreError> {
            self.0.seq_at_time(stream, at).await
        }
        async fn streams(&self) -> Result<Vec<StreamId>, StoreError> {
            self.0.streams().await
        }
        async fn label_set(
            &self,
            stream: &StreamId,
            label: &Label,
            at: Seq,
        ) -> Result<(), StoreError> {
            self.0.label_set(stream, label, at).await
        }
        async fn label_resolve(
            &self,
            stream: &StreamId,
            label: &Label,
        ) -> Result<Option<Seq>, StoreError> {
            self.0.label_resolve(stream, label).await
        }
        async fn labels(&self, stream: &StreamId) -> Result<Vec<(Label, Seq)>, StoreError> {
            self.0.labels(stream).await
        }
        async fn label_delete(&self, stream: &StreamId, label: &Label) -> Result<bool, StoreError> {
            self.0.label_delete(stream, label).await
        }
        // `import_event` intentionally not overridden: the default trait
        // impl returns `StoreError::BackendUnsupported`.
    }

    fn make_store_without_import_event(shelf_dir: &Path, slug: &str) -> Arc<Store> {
        let inner: Arc<dyn EventBackend> = Arc::new(MemEventBackend::new());
        let events: Arc<dyn EventBackend> = Arc::new(NoImportEventBackend(inner));
        let cache: Arc<dyn CacheBackend> = Arc::new(MemCacheBackend::new());
        let sink = SnapshotDumpSink::new(shelf_dir.to_path_buf(), slug.to_string());
        Arc::new(Store::new(
            events,
            cache,
            Vec::new(),
            vec![Arc::new(BlockingSink::new(sink))],
            StoreConfig::default(),
        ))
    }

    /// Writes a legacy on-disk snapshot file directly (bypassing `Store`
    /// entirely), simulating a pre-`ai-store` install's dump.
    fn write_legacy_snapshot(
        dir: &Path,
        slug: &str,
        millis: i64,
        book: &TemplateBook,
        label: Option<&str>,
    ) {
        let state = serde_json::to_value(book).expect("serialize book");
        write_snapshot_body(dir, slug, millis, &state).expect("write body");
        if let Some(label) = label {
            let meta = SnapshotMeta {
                label: Some(label.to_string()),
                created_at: Some(millis),
            };
            write_meta(dir, slug, millis, &meta).expect("write meta");
        }
    }

    #[tokio::test]
    async fn test_migrate_imports_all_legacy_files_preserving_millis() {
        let dir = temp_dir("happy");
        let slug = "book-a";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), Some("first"));
        write_legacy_snapshot(&dir, slug, 2_000, &make_book("V2"), None);
        write_legacy_snapshot(&dir, slug, 3_000, &make_book("V3"), Some("third"));

        let store = make_store(&dir, slug);
        let report = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("migrate");

        assert_eq!(report.scanned, 3);
        assert_eq!(report.imported, 3);
        assert_eq!(report.skipped, 0);
        assert!(report.failed.is_empty(), "failed: {:?}", report.failed);

        let svc = SnapshotService::new(Arc::clone(&store), dir.clone(), slug.to_string());
        let infos = svc.list().await.expect("list");
        // Historical timestamps are preserved: the imported event's `at`
        // equals the source file's own millis, so `SnapshotDumpSink` writes
        // back to the same path instead of materializing a new one — still
        // exactly 3 files on disk after migration.
        assert_eq!(infos.len(), 3, "no duplicate files should be created");

        let labeled: Vec<&str> = infos.iter().filter_map(|i| i.label.as_deref()).collect();
        assert!(labeled.contains(&"first"));
        assert!(labeled.contains(&"third"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrate_is_idempotent() {
        let dir = temp_dir("idempotent");
        let slug = "book-b";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), None);

        let store = make_store(&dir, slug);
        let first = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("first migrate");
        assert_eq!(first.imported, 1);
        assert_eq!(first.skipped, 0);

        let second = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("second migrate");
        assert_eq!(
            second.scanned, 1,
            "no new file was materialized by the first run"
        );
        assert_eq!(second.imported, 0, "nothing new to import on a second run");
        assert_eq!(second.skipped, 1, "the file's millis is already imported");
        assert!(second.failed.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrate_reports_failed_for_corrupt_file() {
        let dir = temp_dir("corrupt");
        let slug = "book-c";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("Good"), None);
        // Write a second, structurally-invalid "snapshot" directly (not
        // valid JSON at all).
        std::fs::write(dir.join(format!("{slug}.snap.2000.json")), "{ not json")
            .expect("write corrupt file");

        let store = make_store(&dir, slug);
        let report = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("migrate");

        assert_eq!(report.scanned, 2);
        assert_eq!(report.imported, 1, "the valid file should still import");
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0]
            .0
            .ends_with(format!("{slug}.snap.2000.json")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrate_refuses_when_stream_nonempty_with_pending_files() {
        let dir = temp_dir("nonempty-refuse");
        let slug = "book-h";
        let store = make_store(&dir, slug);
        let stream = StreamId::new(snapshot_stream_key(slug));

        // Simulate the stream already carrying an ordinary (wall-clock)
        // domain write, unrelated to migration. `store`'s registered
        // `SnapshotDumpSink` also materializes this event's own dump file
        // (keyed by its wall-clock `at`), which is why `report.scanned`
        // below counts 2 disk files, not 1.
        let patch = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"x": 1}));
        store
            .append(&stream, "book_snapshot", patch, serde_json::json!({}))
            .await
            .expect("seed a wall-clock event");

        // A legacy file not yet accounted for.
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("Pending"), None);

        let report = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("migrate");

        assert_eq!(report.scanned, 2);
        assert_eq!(report.imported, 0, "the whole batch must be refused");
        assert_eq!(
            report.skipped, 1,
            "the seeded wall-clock event's own dump file is already accounted for"
        );
        assert_eq!(report.failed.len(), 1, "one entry for the whole run");
        assert!(report.failed[0].1.contains("non-empty"));

        // The legacy file itself must be left untouched.
        let path = snapshot_path(&dir, slug, 1_000);
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrate_allows_reruns_over_already_migrated_nonempty_stream() {
        // A stream that only holds prior *migrated* imports (nothing new to
        // backfill) must not trip the non-empty-stream precondition — see
        // module docs.
        let dir = temp_dir("nonempty-noop");
        let slug = "book-i";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), None);

        let store = make_store(&dir, slug);
        let first = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("first migrate");
        assert_eq!(first.imported, 1);

        // Stream is now non-empty (holds the imported event), but there is
        // nothing pending — this must still succeed as a no-op.
        let second = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("second migrate over a non-empty, fully-migrated stream");
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 1);
        assert!(second.failed.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrate_aborts_remaining_on_backend_unsupported() {
        let dir = temp_dir("backend-unsupported");
        let slug = "book-j";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), None);
        write_legacy_snapshot(&dir, slug, 2_000, &make_book("V2"), None);

        let store = make_store_without_import_event(&dir, slug);
        let report = migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("migrate");

        assert_eq!(report.scanned, 2);
        assert_eq!(report.imported, 0);
        assert_eq!(
            report.failed.len(),
            1,
            "only one entry, the run stops instead of repeating the same error"
        );
        assert!(report.failed[0].1.contains("backend does not support"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_restore_after_migrate_reconstructs_book_at_original_millis() {
        let dir = temp_dir("restore");
        let slug = "book-d";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("Restorable"), None);

        let store = make_store(&dir, slug);
        migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("migrate");

        let svc = SnapshotService::new(Arc::clone(&store), dir.clone(), slug.to_string());
        // The historical timestamp is preserved, so restoring at the
        // *original* file's millis now succeeds directly.
        let restored = svc
            .restore(1_000)
            .await
            .expect("restore at original millis");
        assert_eq!(restored.title(), "Restorable");
        assert_eq!(restored.node_count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_count_orphan_snapshots_before_migration() {
        let dir = temp_dir("count-before");
        let slug = "book-e";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), None);
        write_legacy_snapshot(&dir, slug, 2_000, &make_book("V2"), None);

        let store = make_store(&dir, slug);
        let count = count_orphan_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("count");
        assert_eq!(count, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_count_orphan_snapshots_zero_when_none_on_disk() {
        let dir = temp_dir("count-zero");
        let slug = "book-f";
        let store = make_store(&dir, slug);
        let count = count_orphan_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("count");
        assert_eq!(count, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_count_orphan_snapshots_exact_after_migration() {
        // Before γ' this over-counted after a migration run (a new dump
        // file was materialized per import); now the imported event lands
        // at the same millis, so the count is exact both before and after.
        let dir = temp_dir("count-exact");
        let slug = "book-k";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), None);
        write_legacy_snapshot(&dir, slug, 2_000, &make_book("V2"), None);

        let store = make_store(&dir, slug);
        migrate_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("migrate");

        let count = count_orphan_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("count");
        assert_eq!(count, 0, "both files are now imported");

        write_legacy_snapshot(&dir, slug, 3_000, &make_book("V3"), None);
        let count = count_orphan_snapshots(&dir, slug, Arc::clone(&store))
            .await
            .expect("count");
        assert_eq!(count, 1, "only the newly-added file is still an orphan");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_migrate_slug_opens_own_store() {
        let dir = temp_dir("migrate-slug");
        let slug = "book-g";
        write_legacy_snapshot(&dir, slug, 1_000, &make_book("V1"), Some("solo"));

        let report = migrate_slug(&dir, slug).await.expect("migrate_slug");
        assert_eq!(report.scanned, 1);
        assert_eq!(report.imported, 1);
        assert!(report.failed.is_empty());

        // Running it again against the same on-disk state (fresh SQLite
        // open) should be idempotent — the same single file, now already
        // imported, is just skipped.
        let second = migrate_slug(&dir, slug).await.expect("migrate_slug again");
        assert_eq!(second.scanned, 1);
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_migration_report_display_lists_failures() {
        let report = MigrationReport {
            scanned: 2,
            imported: 1,
            skipped: 0,
            failed: vec![(PathBuf::from("/tmp/x.snap.1.json"), "bad json".to_string())],
        };
        let rendered = report.to_string();
        assert!(rendered.contains("scanned:  2"));
        assert!(rendered.contains("imported: 1"));
        assert!(rendered.contains("failed:   1"));
        assert!(rendered.contains("/tmp/x.snap.1.json"));
        assert!(rendered.contains("bad json"));
    }
}
