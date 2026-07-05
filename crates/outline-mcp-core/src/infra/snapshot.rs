//! Snapshot creation / listing / restore / tag / delete, backed by an
//! ai-store [`Store`].
//!
//! # Architecture
//!
//! `SnapshotService` owns a **dedicated event stream per book**, distinct
//! from the fine-grained per-node changelog stream used by
//! `crate::infra::ai_store_changelog::AiStoreChangeLogRepository`. The two
//! streams share the same `Store` (and therefore the same backend / SQLite
//! file), but a snapshot's stream carries whole-book-state diffs
//! (`current book JSON` -> `next book JSON`), never per-node diffs. This
//! separation is deliberate:
//!
//! - `AiStoreChangeLogRepository::append` computes an RFC 6902 diff between
//!   a single node's before/after JSON (see that module's doc comment for
//!   why this is only a per-entry approximation). Replaying that stream's
//!   patches would not reconstruct a valid `TemplateBook` — the patch
//!   operations reference paths inside a *node* object, not the *book* tree.
//! - By giving snapshots their own stream, `Store::state` / `Store::state_at`
//!   on *that* stream are always a faithful whole-book reconstruction,
//!   because every event appended to it is, by construction, a
//!   `serde_json::to_value(&TemplateBook)` diff produced by this module.
//!
//! The stream naming convention is `"{slug}::snapshots"` (see
//! [`snapshot_stream_key`]), so it never collides with the changelog's
//! `StreamId::new(slug)`.
//!
//! File artifacts keep the pre-existing naming convention
//! (`{slug}.snap.{millis}.json` + sidecar `{slug}.snap.{millis}.meta.json`)
//! and are written by `crate::infra::snapshot_sink::SnapshotDumpSink`,
//! registered as a `Store` sink. See that module's doc comment for why the
//! sink filters dispatch by stream identity instead of an `auto_commit`
//! toggle.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai_store_core::{Label, Store, StreamId, Timestamp as StoreTimestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::model::book::TemplateBook;
use crate::domain::model::timestamp::Timestamp;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Event kind used for whole-book-state snapshot appends.
const KIND_SNAPSHOT: &str = "book_snapshot";

/// Derives the dedicated snapshot [`StreamId`] key for a book slug.
///
/// Shared by [`SnapshotService`] (which appends to it) and
/// `crate::infra::snapshot_sink::SnapshotDumpSink` (which filters `commit`
/// dispatch by it), so the two stay in lockstep without either module
/// depending on the other's internals.
pub(crate) fn snapshot_stream_key(slug: &str) -> String {
    format!("{slug}::snapshots")
}

fn box_store_err(e: ai_store_core::StoreError) -> BoxError {
    Box::new(e)
}

/// スナップショットのメタ情報。
pub struct SnapshotInfo {
    /// When the snapshot was taken.
    pub timestamp: Timestamp,
    /// Path to the snapshot's JSON file.
    pub path: PathBuf,
    /// Size of the snapshot file, in bytes.
    pub size_bytes: u64,
    /// sidecar `.meta.json` から読んだ label。sidecar 不在時は `None`。
    pub label: Option<String>,
}

/// sidecar `.meta.json` の永続 form。
///
/// filename schema は `{slug}.snap.{millis}.meta.json`。
/// 本 struct の欠落は `label: None` fallback で扱う (旧 snapshot 混在で壊さない)。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SnapshotMeta {
    /// User-attached label for this snapshot, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// meta が最初に書き出された Unix ミリ秒。`create(label)` 経由なら snapshot 本体と同じ、
    /// `tag` 経由での事後追記なら tag 実行時刻。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// File I/O helpers (shared with `crate::infra::snapshot_sink::SnapshotDumpSink`)
// ---------------------------------------------------------------------------

/// Path to a snapshot body file. Existence is not checked.
pub(crate) fn snapshot_path(shelf_dir: &Path, slug: &str, millis: i64) -> PathBuf {
    shelf_dir.join(format!("{slug}.snap.{millis}.json"))
}

/// Path to a snapshot's sidecar `.meta.json`. Existence is not checked.
pub(crate) fn meta_path(shelf_dir: &Path, slug: &str, millis: i64) -> PathBuf {
    shelf_dir.join(format!("{slug}.snap.{millis}.meta.json"))
}

/// Atomically writes a snapshot's body (`state` materialized JSON).
pub(crate) fn write_snapshot_body(
    shelf_dir: &Path,
    slug: &str,
    millis: i64,
    state: &Value,
) -> Result<PathBuf, BoxError> {
    std::fs::create_dir_all(shelf_dir)?;
    let path = snapshot_path(shelf_dir, slug, millis);
    let content = serde_json::to_string_pretty(state)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Atomically writes a snapshot's sidecar `.meta.json`.
pub(crate) fn write_meta(
    shelf_dir: &Path,
    slug: &str,
    millis: i64,
    meta: &SnapshotMeta,
) -> Result<PathBuf, BoxError> {
    std::fs::create_dir_all(shelf_dir)?;
    let path = meta_path(shelf_dir, slug, millis);
    let content = serde_json::to_string_pretty(meta)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Reads a snapshot's sidecar `.meta.json`. Missing / unparsable sidecars
/// fall back to `None` (pre-existing snapshots without a sidecar are valid).
pub(crate) fn read_meta(shelf_dir: &Path, slug: &str, millis: i64) -> Option<SnapshotMeta> {
    let path = meta_path(shelf_dir, slug, millis);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Lists snapshot files for `slug`, newest first.
pub(crate) fn list_snapshots(shelf_dir: &Path, slug: &str) -> Result<Vec<SnapshotInfo>, BoxError> {
    if !shelf_dir.exists() {
        return Ok(Vec::new());
    }

    let prefix = format!("{slug}.snap.");
    let suffix = ".json";

    let mut infos: Vec<SnapshotInfo> = std::fs::read_dir(shelf_dir)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?.to_string();
            if !file_name.starts_with(&prefix) || !file_name.ends_with(suffix) {
                return None;
            }
            // sidecar `.meta.json` は snapshot 本体ではないので除外
            if file_name.ends_with(".meta.json") {
                return None;
            }
            // stem = "{slug}.snap.{millis}" → 最後の . 以降を millis としてパース
            let stem = path.file_stem()?.to_str()?;
            let millis_str = stem.rsplit('.').next()?;
            let millis: i64 = millis_str.parse().ok()?;
            let timestamp = Timestamp::from_millis(millis);
            let size_bytes = entry.metadata().ok()?.len();
            let label = read_meta(shelf_dir, slug, millis).and_then(|m| m.label);
            Some(SnapshotInfo {
                timestamp,
                path: path.clone(),
                size_bytes,
                label,
            })
        })
        .collect();

    // タイムスタンプ降順でソート（最新が先頭）
    infos.sort_by_key(|i| std::cmp::Reverse(i.timestamp));

    Ok(infos)
}

// ---------------------------------------------------------------------------
// SnapshotService
// ---------------------------------------------------------------------------

/// Creates, lists, restores, tags, and deletes book-level snapshots, backed
/// by a dedicated ai-store [`Store`] stream (see module docs).
pub struct SnapshotService {
    store: Arc<Store>,
    shelf_dir: PathBuf,
    slug: String,
    stream: StreamId,
}

impl SnapshotService {
    /// Constructs a service over `store` for the given book `slug`. `store`
    /// must have a `crate::infra::snapshot_sink::SnapshotDumpSink` sink
    /// registered for this to have any observable effect on disk.
    pub fn new(store: Arc<Store>, shelf_dir: PathBuf, slug: impl Into<String>) -> Self {
        let slug = slug.into();
        let stream = StreamId::new(snapshot_stream_key(&slug));
        Self {
            store,
            shelf_dir,
            slug,
            stream,
        }
    }

    /// Takes a snapshot of `book`'s current state. `label` is carried in the
    /// appended event's `meta` so the registered sink can write the sidecar
    /// without a second round trip.
    ///
    /// Returns the path of the written snapshot body file.
    pub async fn create(
        &self,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<PathBuf, BoxError> {
        let current = self
            .store
            .state(&self.stream)
            .await
            .map_err(box_store_err)?;
        let next = serde_json::to_value(book)?;
        let patch = json_patch::diff(&current, &next);
        let meta = serde_json::json!({ "label": label });
        let committed = self
            .store
            .append(&self.stream, KIND_SNAPSHOT, patch, meta)
            .await
            .map_err(box_store_err)?;
        Ok(snapshot_path(&self.shelf_dir, &self.slug, committed.at.0))
    }

    /// Attaches (or overwrites) a label on an existing snapshot. Only the
    /// sidecar `.meta.json` is written; the snapshot body is untouched.
    ///
    /// Also best-effort mirrors the label into the ai-store label registry
    /// (`Store::label_set`) for consumers that introspect it directly. This
    /// step is intentionally non-fatal: a snapshot created before this
    /// service existed (or the registry entry not resolving for any other
    /// reason) must not block attaching a label to its file, since the
    /// sidecar file is this service's source of truth for `list` / `tag`.
    pub async fn tag(&self, timestamp_millis: i64, label: &str) -> Result<PathBuf, BoxError> {
        let path = snapshot_path(&self.shelf_dir, &self.slug, timestamp_millis);
        if !path.exists() {
            return Err(format!(
                "snapshot not found: {} at millis {timestamp_millis}",
                self.slug
            )
            .into());
        }

        let existing = read_meta(&self.shelf_dir, &self.slug, timestamp_millis).unwrap_or_default();
        let created_at = existing.created_at.or(Some(timestamp_millis));
        let meta = SnapshotMeta {
            label: Some(label.to_string()),
            created_at,
        };
        let meta_path = write_meta(&self.shelf_dir, &self.slug, timestamp_millis, &meta)?;

        if let Ok(Some(seq)) = self
            .store
            .seq_at_time(&self.stream, StoreTimestamp(timestamp_millis))
            .await
        {
            let _ = self
                .store
                .label_set(&self.stream, &Label::new(label), seq)
                .await;
        }

        Ok(meta_path)
    }

    /// Lists snapshots for this book, newest first.
    pub async fn list(&self) -> Result<Vec<SnapshotInfo>, BoxError> {
        list_snapshots(&self.shelf_dir, &self.slug)
    }

    /// Restores the `TemplateBook` recorded at `timestamp_millis`.
    pub async fn restore(&self, timestamp_millis: i64) -> Result<TemplateBook, BoxError> {
        let seq = self
            .store
            .seq_at_time(&self.stream, StoreTimestamp(timestamp_millis))
            .await
            .map_err(box_store_err)?
            .ok_or_else(|| -> BoxError {
                format!(
                    "snapshot not found: {} at millis {timestamp_millis}",
                    self.slug
                )
                .into()
            })?;
        let state = self
            .store
            .state_at(&self.stream, seq)
            .await
            .map_err(box_store_err)?;
        let book: TemplateBook = serde_json::from_value(state)?;
        Ok(book)
    }

    /// Deletes the snapshot's file artifacts (body + sidecar). This does
    /// **not** erase the underlying ai-store event — the store is
    /// append-only by design, so `restore` for this same `timestamp_millis`
    /// remains possible via history replay. Deleting only removes the
    /// derived file dump, mirroring the "projections are re-derivable"
    /// contract sinks operate under.
    pub async fn delete(&self, timestamp_millis: i64) -> Result<(), BoxError> {
        let path = snapshot_path(&self.shelf_dir, &self.slug, timestamp_millis);
        if !path.exists() {
            return Err(format!(
                "snapshot not found: {} at millis {timestamp_millis}",
                self.slug
            )
            .into());
        }

        if let Some(label) =
            read_meta(&self.shelf_dir, &self.slug, timestamp_millis).and_then(|m| m.label)
        {
            let _ = self
                .store
                .label_delete(&self.stream, &Label::new(label))
                .await;
        }

        std::fs::remove_file(&path)?;
        let meta_path = meta_path(&self.shelf_dir, &self.slug, timestamp_millis);
        let _ = std::fs::remove_file(&meta_path);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::{AddNodeRequest, TemplateBook};
    use crate::domain::model::node::NodeType;
    use ai_store_core::{CacheBackend, EventBackend, StoreConfig};
    use ai_store_mem::{MemCacheBackend, MemEventBackend};
    use ai_store_sync::BlockingSink;
    use std::collections::HashMap;

    use crate::infra::snapshot_sink::SnapshotDumpSink;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("outline-mcp-snapshot-test-{suffix}"));
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

    fn make_service(shelf_dir: &Path, slug: &str) -> SnapshotService {
        let events: Arc<dyn EventBackend> = Arc::new(MemEventBackend::new());
        let cache: Arc<dyn CacheBackend> = Arc::new(MemCacheBackend::new());
        let sink = SnapshotDumpSink::new(shelf_dir.to_path_buf(), slug.to_string());
        let store = Arc::new(Store::new(
            events,
            cache,
            Vec::new(),
            vec![Arc::new(BlockingSink::new(sink))],
            StoreConfig::default(),
        ));
        SnapshotService::new(store, shelf_dir.to_path_buf(), slug)
    }

    #[tokio::test]
    async fn test_create_returns_path_that_exists() {
        let dir = temp_dir("create");
        let svc = make_service(&dir, "my-book");
        let book = make_book("Snapshot Test");
        let path = svc.create(&book, None).await.expect("create snapshot");
        assert!(path.exists(), "snapshot file should exist at {path:?}");
        let fname = path.file_name().unwrap().to_str().unwrap();
        assert!(fname.starts_with("my-book.snap."), "file name: {fname}");
        assert!(fname.ends_with(".json"), "file name: {fname}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_list_returns_created_snapshots() {
        let dir = temp_dir("list");
        let svc = make_service(&dir, "list-book");
        let book = make_book("List Test");
        svc.create(&book, None).await.expect("create 1");
        // 同一ミリ秒衝突を避けるため少し待つ（テスト用途のみ）
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        svc.create(&book, None).await.expect("create 2");

        let infos = svc.list().await.expect("list");
        assert_eq!(infos.len(), 2, "should have 2 snapshots");
        assert!(
            infos[0].timestamp >= infos[1].timestamp,
            "should be sorted descending"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_list_empty_when_no_snapshots() {
        let dir = temp_dir("list-empty");
        let svc = make_service(&dir, "no-snaps");
        let infos = svc.list().await.expect("list empty");
        assert!(infos.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_restore_roundtrip() {
        let dir = temp_dir("restore");
        let svc = make_service(&dir, "restore-book");
        let book = make_book("Restore Test");
        let path = svc.create(&book, None).await.expect("create");

        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem
            .rsplit('.')
            .next()
            .unwrap()
            .parse()
            .expect("parse millis");

        let restored = svc.restore(millis).await.expect("restore");
        assert_eq!(restored.title(), "Restore Test");
        assert_eq!(restored.node_count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_restore_nonexistent_returns_error() {
        let dir = temp_dir("restore-err");
        let svc = make_service(&dir, "no-book");
        let result = svc.restore(999_999_999).await;
        assert!(result.is_err(), "should return error for missing snapshot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_create_with_label_writes_sidecar() {
        let dir = temp_dir("create-label");
        let svc = make_service(&dir, "lb");
        let book = make_book("Labeled Snap");
        let path = svc
            .create(&book, Some("rating-pass"))
            .await
            .expect("create with label");
        assert!(path.exists());

        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();
        let meta_path = meta_path(&dir, "lb", millis);
        assert!(meta_path.exists(), "sidecar meta.json should exist");

        let infos = svc.list().await.expect("list");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].label.as_deref(), Some("rating-pass"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_list_label_none_when_no_sidecar() {
        let dir = temp_dir("create-no-label");
        let svc = make_service(&dir, "nl");
        let book = make_book("NoLabel");
        svc.create(&book, None).await.expect("create");
        let infos = svc.list().await.expect("list");
        assert_eq!(infos.len(), 1);
        assert!(infos[0].label.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_tag_writes_sidecar_after_creation() {
        let dir = temp_dir("tag");
        let svc = make_service(&dir, "tg");
        let book = make_book("TagLater");
        let path = svc.create(&book, None).await.expect("create");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();

        svc.tag(millis, "post-hoc").await.expect("tag");

        let infos = svc.list().await.expect("list");
        assert_eq!(infos[0].label.as_deref(), Some("post-hoc"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_tag_overwrites_existing_label() {
        let dir = temp_dir("tag-overwrite");
        let svc = make_service(&dir, "ow");
        let book = make_book("Overwrite");
        let path = svc.create(&book, Some("initial")).await.expect("create");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();

        svc.tag(millis, "updated").await.expect("tag overwrite");

        let infos = svc.list().await.expect("list");
        assert_eq!(infos[0].label.as_deref(), Some("updated"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_tag_errors_when_snapshot_missing() {
        let dir = temp_dir("tag-missing");
        let svc = make_service(&dir, "nope");
        let res = svc.tag(999_999_999, "x").await;
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_list_ignores_meta_sidecar_files() {
        let dir = temp_dir("list-meta-ignore");
        let svc = make_service(&dir, "mi");
        let book = make_book("MetaIgnore");
        svc.create(&book, Some("with-meta")).await.expect("create");
        let infos = svc.list().await.expect("list");
        assert_eq!(infos.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_list_does_not_include_non_snapshot_files() {
        let dir = temp_dir("list-filter");
        std::fs::write(dir.join("my-book.json"), "{}").expect("write book");
        std::fs::write(dir.join("my-book.changelog.json"), "[]").expect("write changelog");

        let infos = list_snapshots(&dir, "my-book").expect("list");
        assert!(
            infos.is_empty(),
            "non-snapshot files should not be listed as snapshots"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_delete_removes_body_and_sidecar() {
        let dir = temp_dir("delete");
        let svc = make_service(&dir, "del");
        let book = make_book("Delete Test");
        let path = svc.create(&book, Some("to-delete")).await.expect("create");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();
        let meta = meta_path(&dir, "del", millis);
        assert!(path.exists());
        assert!(meta.exists());

        svc.delete(millis).await.expect("delete");

        assert!(!path.exists(), "body should be removed");
        assert!(!meta.exists(), "sidecar should be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_returns_error() {
        let dir = temp_dir("delete-missing");
        let svc = make_service(&dir, "no-such");
        let res = svc.delete(999_999_999).await;
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_restore_after_delete_still_reconstructs_from_history() {
        // ai-store is append-only: deleting a snapshot's file artifacts does
        // not erase the underlying event, so restore still works via replay.
        let dir = temp_dir("restore-after-delete");
        let svc = make_service(&dir, "hist");
        let book = make_book("History Survives");
        let path = svc.create(&book, None).await.expect("create");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();

        svc.delete(millis).await.expect("delete");
        let restored = svc.restore(millis).await.expect("restore after delete");
        assert_eq!(restored.title(), "History Survives");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_multiple_creates_reconstruct_correct_book_per_seq() {
        let dir = temp_dir("multi-create");
        let svc = make_service(&dir, "multi");
        let book_v1 = make_book("Version 1");
        let mut book_v2 = book_v1.clone();
        book_v2
            .add_node(AddNodeRequest {
                parent: None,
                title: "Node 2".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
                properties: HashMap::new(),
            })
            .expect("add second node");

        let path1 = svc.create(&book_v1, None).await.expect("create v1");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let path2 = svc.create(&book_v2, None).await.expect("create v2");

        let millis1: i64 = path1
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let millis2: i64 = path2
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        let restored1 = svc.restore(millis1).await.expect("restore v1");
        let restored2 = svc.restore(millis2).await.expect("restore v2");
        assert_eq!(restored1.node_count(), 1);
        assert_eq!(restored2.node_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
