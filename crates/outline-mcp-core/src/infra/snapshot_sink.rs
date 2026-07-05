//! `SnapshotDumpSink` — the [`SyncProjectionSink`] that persists book-level
//! snapshot dumps requested via `crate::infra::snapshot::SnapshotService`.
//!
//! # Architecture
//!
//! `SnapshotService` appends whole-book-state diffs to a **dedicated
//! snapshot stream** per book slug (see
//! `crate::infra::snapshot::snapshot_stream_key`), distinct from the
//! fine-grained per-node changelog stream used by
//! `crate::infra::ai_store_changelog::AiStoreChangeLogRepository`. Both
//! streams are typically served by the same `Store` (and therefore share the
//! same registered sinks), so this sink filters `commit` dispatch by stream
//! identity: every append on the dedicated snapshot stream is, by
//! construction, an explicit "please materialize a dump" request from
//! `SnapshotService`, so `commit` always writes for that stream and is a
//! no-op for any other stream — in particular, the changelog stream's
//! frequent per-mutation appends never trigger a dump.
//!
//! ## Why not an `auto_commit` boolean?
//!
//! `ProjectionSink::commit` is dispatched identically whether the triggering
//! append came from ordinary `Store::append` traffic or from
//! `Store::materialize_to_sink` — the method receives no marker
//! distinguishing the two call sites. A boolean flag toggled inside the sink
//! cannot tell "the snapshot stream's owner just asked for a dump" apart
//! from "some unrelated stream just mutated" without either a shared mutable
//! flag (racy across concurrent callers) or a back-reference into `Store`
//! (which `SyncProjectionSink::commit` cannot use anyway, since it is a sync
//! method and `Store`'s read methods are async). Stream identity is the one
//! piece of context `commit` genuinely has on every call, so filtering on it
//! is the deterministic, race-free option.
//!
//! ## Where does the label come from?
//!
//! `ProjectionSink::on_label_set` receives `(stream, label, at: Seq, state)`
//! — no event timestamp. Since this sink's file naming convention
//! (`{slug}.snap.{millis}.json`) requires the exact wall-clock millis, and
//! `on_label_set` cannot recover it without an extra async round trip this
//! sync trait cannot make, labels instead travel in `Event::meta` as
//! `{"label": <string | null>}`, set by `SnapshotService::create`. `commit`
//! reads it straight off the `Event` it is already handed, so no
//! `on_label_set` / `on_label_deleted` override is needed (both keep the
//! trait's no-op default).
//!
//! ## [`SnapshotOnlySink`]: filtering dispatch via `ProjectionSink::accepts`
//!
//! `SyncProjectionSink` (the trait this module's [`SnapshotDumpSink`]
//! implements) has no `accepts` hook of its own, so the stream-identity
//! check in `commit` above cannot be hoisted onto it directly. [`ai_store_core::Store`]'s
//! automatic dispatch (`append`, `catch_up`, `rebuild`, `label_set` /
//! `label_delete`) does honor [`ProjectionSink::accepts`], though — and once
//! a book's snapshot stream and its per-node changelog stream
//! (`crate::infra::ai_store_changelog::AiStoreChangeLogRepository`) share
//! the same `Store` (and therefore the same registered sinks), every
//! ordinary node mutation would otherwise still pay for a
//! `tokio::task::spawn_blocking` round trip into `SnapshotDumpSink::commit`
//! just to be told "not my stream". [`SnapshotOnlySink`] wraps a
//! `BlockingSink<SnapshotDumpSink>` in a thin [`ProjectionSink`] that
//! overrides `accepts` to check stream identity one layer up, so the facade
//! skips the dispatch (and the `catch_up`/`rebuild` bookkeeping that goes
//! with it) entirely for any stream other than the dedicated snapshot
//! stream. `commit`'s own stream check is kept as defense in depth against
//! [`ai_store_core::Store::materialize_to_sink`], which is documented to
//! bypass `accepts` entirely.

use std::path::PathBuf;

use ai_store_core::{Event, ProjectionSink, Seq, StoreError, StreamId};
use ai_store_sync::{BlockingSink, SyncProjectionSink};
use async_trait::async_trait;
use serde_json::Value;

use crate::infra::snapshot::{snapshot_stream_key, write_meta, write_snapshot_body, SnapshotMeta};

fn to_store_err(e: Box<dyn std::error::Error + Send + Sync>) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Writes `{slug}.snap.{millis}.json` (+ sidecar `.meta.json` when a label
/// is present) whenever the dedicated snapshot stream for `slug` is
/// appended to. See module docs for why dispatch is filtered by stream
/// identity rather than a mutable flag.
pub struct SnapshotDumpSink {
    shelf_dir: PathBuf,
    slug: String,
    stream_key: String,
}

impl SnapshotDumpSink {
    /// Constructs a sink that writes into `shelf_dir` for `slug`, responding
    /// only to appends on `slug`'s dedicated snapshot stream.
    pub fn new(shelf_dir: PathBuf, slug: impl Into<String>) -> Self {
        let slug = slug.into();
        let stream_key = snapshot_stream_key(&slug);
        Self {
            shelf_dir,
            slug,
            stream_key,
        }
    }
}

impl SyncProjectionSink for SnapshotDumpSink {
    fn id(&self) -> &str {
        "snapshot-dump"
    }

    fn commit(
        &self,
        stream: &StreamId,
        _seq: Seq,
        state: &Value,
        event: &Event,
    ) -> Result<(), StoreError> {
        if stream.as_str() != self.stream_key {
            // Not our stream (e.g. the per-node changelog). Ordinary
            // dispatch is already filtered upstream by `SnapshotOnlySink`'s
            // `ProjectionSink::accepts` override — this check only matters
            // as defense in depth against `Store::materialize_to_sink`,
            // which is documented to bypass `accepts`.
            return Ok(());
        }

        let millis = event.at.0;
        write_snapshot_body(&self.shelf_dir, &self.slug, millis, state).map_err(to_store_err)?;

        let label = event
            .meta
            .get("label")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(label) = label {
            let meta = SnapshotMeta {
                label: Some(label),
                created_at: Some(millis),
            };
            write_meta(&self.shelf_dir, &self.slug, millis, &meta).map_err(to_store_err)?;
        }

        Ok(())
    }
}

/// Presents a [`SnapshotDumpSink`] to [`ai_store_core::Store`] as a
/// [`ProjectionSink`] that only accepts `slug`'s dedicated snapshot stream
/// (see module docs for why this filter cannot live on `SnapshotDumpSink`
/// itself). This is the constructor production call sites
/// (`OutlineMcpServer::store_for`, `migrate_slug`) should use instead of
/// wrapping [`SnapshotDumpSink`] in a bare [`BlockingSink`] directly.
pub struct SnapshotOnlySink {
    inner: BlockingSink<SnapshotDumpSink>,
    stream_key: String,
}

impl SnapshotOnlySink {
    /// Constructs the filtered sink for `slug`, wrapping a fresh
    /// [`SnapshotDumpSink`] in [`BlockingSink::new`] (`spawn_blocking`
    /// dispatch, since the wrapped sink does file I/O).
    pub fn new(shelf_dir: PathBuf, slug: impl Into<String>) -> Self {
        let slug = slug.into();
        let stream_key = snapshot_stream_key(&slug);
        Self {
            inner: BlockingSink::new(SnapshotDumpSink::new(shelf_dir, slug)),
            stream_key,
        }
    }
}

#[async_trait]
impl ProjectionSink for SnapshotOnlySink {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn accepts(&self, stream: &StreamId) -> bool {
        stream.as_str() == self.stream_key
    }

    async fn commit(
        &self,
        stream: &StreamId,
        seq: Seq,
        state: &Value,
        event: &Event,
    ) -> Result<(), StoreError> {
        self.inner.commit(stream, seq, state, event).await
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ai_store_core::{CacheBackend, EventBackend, Label, Store, StoreConfig};
    use ai_store_mem::{MemCacheBackend, MemEventBackend};
    use std::path::Path;
    use std::sync::Arc;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("outline-mcp-snapshot-sink-test-{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
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

    fn snapshot_stream(slug: &str) -> StreamId {
        StreamId::new(snapshot_stream_key(slug))
    }

    #[tokio::test]
    async fn test_commit_on_snapshot_stream_writes_file() {
        let dir = temp_dir("commit");
        let store = make_store(&dir, "book-a");
        let stream = snapshot_stream("book-a");

        let patch = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"title": "t"}));
        let meta = serde_json::json!({ "label": null });
        let committed = store
            .append(&stream, "book_snapshot", patch, meta)
            .await
            .expect("append");
        let millis = committed.at.0;

        let path = dir.join(format!("book-a.snap.{millis}.json"));
        assert!(path.exists(), "snapshot body should be written: {path:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_commit_ignores_other_streams() {
        let dir = temp_dir("ignore");
        let store = make_store(&dir, "book-b");
        // append to a totally different stream (mimics the changelog stream)
        let changelog_stream = StreamId::new("book-b");

        let patch = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"x": 1}));
        let meta = serde_json::json!({});
        store
            .append(&changelog_stream, "node_updated", patch, meta)
            .await
            .expect("append to changelog stream");

        // no snapshot file should have been written anywhere under dir
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            entries.is_empty(),
            "commit on a foreign stream must not write anything, found: {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_commit_with_label_writes_sidecar() {
        let dir = temp_dir("label");
        let store = make_store(&dir, "book-c");
        let stream = snapshot_stream("book-c");

        let patch = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"title": "t"}));
        let meta = serde_json::json!({ "label": "rating-pass" });
        let committed = store
            .append(&stream, "book_snapshot", patch, meta)
            .await
            .expect("append");
        let millis = committed.at.0;

        let meta_path = dir.join(format!("book-c.snap.{millis}.meta.json"));
        assert!(meta_path.exists(), "sidecar should exist: {meta_path:?}");
        let content = std::fs::read_to_string(&meta_path).expect("read sidecar");
        assert!(content.contains("rating-pass"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_label_set_does_not_duplicate_body_write() {
        // on_label_set keeps the trait default no-op: labeling an existing
        // seq must not produce a second body file.
        let dir = temp_dir("label-set-noop");
        let store = make_store(&dir, "book-d");
        let stream = snapshot_stream("book-d");

        let patch = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"title": "t"}));
        let meta = serde_json::json!({ "label": null });
        let committed = store
            .append(&stream, "book_snapshot", patch, meta)
            .await
            .expect("append");

        store
            .label_set(&stream, &Label::new("later-label"), committed.seq)
            .await
            .expect("label_set");

        let count = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".json") && !n.ends_with(".meta.json"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(count, 1, "label_set must not write an extra body file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_snapshot_only_sink_accepts_only_its_own_stream() {
        let dir = temp_dir("accepts");
        let sink = SnapshotOnlySink::new(dir.clone(), "book-e".to_string());
        let own_stream = snapshot_stream("book-e");
        let other_stream = StreamId::new("book-e");
        assert!(sink.accepts(&own_stream));
        assert!(!sink.accepts(&other_stream));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_snapshot_only_sink_filters_dispatch_via_store() {
        let dir = temp_dir("only-sink-dispatch");
        let events: Arc<dyn EventBackend> = Arc::new(MemEventBackend::new());
        let cache: Arc<dyn CacheBackend> = Arc::new(MemCacheBackend::new());
        let sink = SnapshotOnlySink::new(dir.clone(), "book-f".to_string());
        let store = Arc::new(Store::new(
            events,
            cache,
            Vec::new(),
            vec![Arc::new(sink)],
            StoreConfig::default(),
        ));

        // Append to a foreign stream (mimics the changelog stream): `accepts`
        // must cause the facade to skip dispatch entirely, so no snapshot
        // file lands at all (not even a no-op `commit` call).
        let changelog_stream = StreamId::new("book-f");
        let patch = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"x": 1}));
        store
            .append(
                &changelog_stream,
                "node_updated",
                patch,
                serde_json::json!({}),
            )
            .await
            .expect("append to changelog stream");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            entries.is_empty(),
            "foreign stream must not dump a file, found: {entries:?}"
        );

        // Append to the dedicated snapshot stream: this must still dump.
        let own_stream = snapshot_stream("book-f");
        let patch2 = json_patch::diff(&serde_json::Value::Null, &serde_json::json!({"title": "t"}));
        store
            .append(
                &own_stream,
                "book_snapshot",
                patch2,
                serde_json::json!({ "label": null }),
            )
            .await
            .expect("append to snapshot stream");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert_eq!(entries.len(), 1, "own stream must dump exactly one file");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
