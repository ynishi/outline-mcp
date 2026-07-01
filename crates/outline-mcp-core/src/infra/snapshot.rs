use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::model::book::TemplateBook;
use crate::domain::model::timestamp::Timestamp;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

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

/// スナップショットの作成・一覧・復元を行うサービス。
///
/// ファイル命名: `{slug}.snap.{millis}.json`
/// millis は Timestamp の Unix ミリ秒値（数値）。
///
/// sidecar (label): `{slug}.snap.{millis}.meta.json` — 本体 schema は不変、
/// meta は独立 file で扱う (旧 snapshot 混在で壊さない)。
pub struct SnapshotService;

impl SnapshotService {
    /// 現在の Book のスナップショットを作成する。
    ///
    /// 作成したファイルのパスを返す。`label` を指定すると sidecar `.meta.json` も同時に書く。
    pub fn create(
        shelf_dir: &Path,
        slug: &str,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<PathBuf, BoxError> {
        std::fs::create_dir_all(shelf_dir).map_err(|e| -> BoxError { Box::new(e) })?;

        let millis = Timestamp::now().as_millis();
        let path = shelf_dir.join(format!("{slug}.snap.{millis}.json"));

        let content =
            serde_json::to_string_pretty(book).map_err(|e| -> BoxError { Box::new(e) })?;

        // atomic write: tmp → rename
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &content).map_err(|e| -> BoxError { Box::new(e) })?;
        std::fs::rename(&tmp, &path).map_err(|e| -> BoxError { Box::new(e) })?;

        if let Some(label) = label {
            let meta = SnapshotMeta {
                label: Some(label.to_string()),
                created_at: Some(millis),
            };
            Self::write_meta(shelf_dir, slug, millis, &meta)?;
        }

        Ok(path)
    }

    /// sidecar `.meta.json` の path を返す (存在検査は行わない)。
    pub fn meta_path(shelf_dir: &Path, slug: &str, timestamp_millis: i64) -> PathBuf {
        shelf_dir.join(format!("{slug}.snap.{timestamp_millis}.meta.json"))
    }

    /// sidecar `.meta.json` を atomic write する。
    fn write_meta(
        shelf_dir: &Path,
        slug: &str,
        timestamp_millis: i64,
        meta: &SnapshotMeta,
    ) -> Result<PathBuf, BoxError> {
        std::fs::create_dir_all(shelf_dir).map_err(|e| -> BoxError { Box::new(e) })?;
        let path = Self::meta_path(shelf_dir, slug, timestamp_millis);
        let content =
            serde_json::to_string_pretty(meta).map_err(|e| -> BoxError { Box::new(e) })?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &content).map_err(|e| -> BoxError { Box::new(e) })?;
        std::fs::rename(&tmp, &path).map_err(|e| -> BoxError { Box::new(e) })?;
        Ok(path)
    }

    /// sidecar `.meta.json` を読む。存在しない / parse 失敗は `None` を返す (silent fallback)。
    fn read_meta(shelf_dir: &Path, slug: &str, timestamp_millis: i64) -> Option<SnapshotMeta> {
        let path = Self::meta_path(shelf_dir, slug, timestamp_millis);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 既存の snapshot に label を事後追記 (sidecar のみ書く、本体 snapshot は触らない)。
    ///
    /// snapshot 本体が存在しない場合は error を返す。
    pub fn tag(
        shelf_dir: &Path,
        slug: &str,
        timestamp_millis: i64,
        label: &str,
    ) -> Result<PathBuf, BoxError> {
        let snapshot_path = shelf_dir.join(format!("{slug}.snap.{timestamp_millis}.json"));
        if !snapshot_path.exists() {
            return Err(format!("snapshot not found: {slug} at millis {timestamp_millis}").into());
        }
        let existing = Self::read_meta(shelf_dir, slug, timestamp_millis).unwrap_or_default();
        let created_at = existing.created_at.or(Some(Timestamp::now().as_millis()));
        let meta = SnapshotMeta {
            label: Some(label.to_string()),
            created_at,
        };
        Self::write_meta(shelf_dir, slug, timestamp_millis, &meta)
    }

    /// slug に対応するスナップショット一覧を返す（タイムスタンプ降順）。
    pub fn list(shelf_dir: &Path, slug: &str) -> Result<Vec<SnapshotInfo>, BoxError> {
        if !shelf_dir.exists() {
            return Ok(Vec::new());
        }

        let prefix = format!("{slug}.snap.");
        let suffix = ".json";

        let mut infos: Vec<SnapshotInfo> = std::fs::read_dir(shelf_dir)
            .map_err(|e| -> BoxError { Box::new(e) })?
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
                let label = Self::read_meta(shelf_dir, slug, millis).and_then(|m| m.label);
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

    /// 指定 millis のスナップショットから Book を復元する。
    pub fn restore(
        shelf_dir: &Path,
        slug: &str,
        timestamp_millis: i64,
    ) -> Result<TemplateBook, BoxError> {
        let path = shelf_dir.join(format!("{slug}.snap.{timestamp_millis}.json"));
        if !path.exists() {
            return Err(format!("snapshot not found: {slug} at millis {timestamp_millis}").into());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| -> BoxError { Box::new(e) })?;
        let book: TemplateBook =
            serde_json::from_str(&content).map_err(|e| -> BoxError { Box::new(e) })?;
        Ok(book)
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
    use std::collections::HashMap;

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

    #[test]
    fn test_create_returns_path_that_exists() {
        let dir = temp_dir("create");
        let book = make_book("Snapshot Test");
        let path = SnapshotService::create(&dir, "my-book", &book, None).expect("create snapshot");
        assert!(path.exists(), "snapshot file should exist at {path:?}");
        // ファイル名が期待するパターンに一致するか
        let fname = path.file_name().unwrap().to_str().unwrap();
        assert!(fname.starts_with("my-book.snap."), "file name: {fname}");
        assert!(fname.ends_with(".json"), "file name: {fname}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_returns_created_snapshots() {
        let dir = temp_dir("list");
        let book = make_book("List Test");
        SnapshotService::create(&dir, "list-book", &book, None).expect("create 1");
        // 同一ミリ秒衝突を避けるため少し待つ（テスト用途のみ）
        std::thread::sleep(std::time::Duration::from_millis(2));
        SnapshotService::create(&dir, "list-book", &book, None).expect("create 2");

        let infos = SnapshotService::list(&dir, "list-book").expect("list");
        assert_eq!(infos.len(), 2, "should have 2 snapshots");
        // 降順チェック
        assert!(
            infos[0].timestamp >= infos[1].timestamp,
            "should be sorted descending"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_empty_when_no_snapshots() {
        let dir = temp_dir("list-empty");
        let infos = SnapshotService::list(&dir, "no-snaps").expect("list empty");
        assert!(infos.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restore_roundtrip() {
        let dir = temp_dir("restore");
        let book = make_book("Restore Test");
        let path = SnapshotService::create(&dir, "restore-book", &book, None).expect("create");

        // ファイル名からmillisを取得
        // "restore-book.snap.{millis}.json" → stemは "restore-book.snap.{millis}"
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem
            .rsplit('.')
            .next()
            .unwrap()
            .parse()
            .expect("parse millis");

        let restored = SnapshotService::restore(&dir, "restore-book", millis).expect("restore");
        assert_eq!(restored.title(), "Restore Test");
        assert_eq!(restored.node_count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restore_nonexistent_returns_error() {
        let dir = temp_dir("restore-err");
        let result = SnapshotService::restore(&dir, "no-book", 999_999_999);
        assert!(result.is_err(), "should return error for missing snapshot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_with_label_writes_sidecar() {
        let dir = temp_dir("create-label");
        let book = make_book("Labeled Snap");
        let path = SnapshotService::create(&dir, "lb", &book, Some("rating-pass"))
            .expect("create with label");
        assert!(path.exists());

        // sidecar が存在する
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();
        let meta_path = SnapshotService::meta_path(&dir, "lb", millis);
        assert!(meta_path.exists(), "sidecar meta.json should exist");

        // list に label が乗る
        let infos = SnapshotService::list(&dir, "lb").expect("list");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].label.as_deref(), Some("rating-pass"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_label_none_when_no_sidecar() {
        let dir = temp_dir("create-no-label");
        let book = make_book("NoLabel");
        SnapshotService::create(&dir, "nl", &book, None).expect("create");
        let infos = SnapshotService::list(&dir, "nl").expect("list");
        assert_eq!(infos.len(), 1);
        assert!(infos[0].label.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tag_writes_sidecar_after_creation() {
        let dir = temp_dir("tag");
        let book = make_book("TagLater");
        let path = SnapshotService::create(&dir, "tg", &book, None).expect("create");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();

        SnapshotService::tag(&dir, "tg", millis, "post-hoc").expect("tag");

        let infos = SnapshotService::list(&dir, "tg").expect("list");
        assert_eq!(infos[0].label.as_deref(), Some("post-hoc"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tag_overwrites_existing_label() {
        let dir = temp_dir("tag-overwrite");
        let book = make_book("Overwrite");
        let path = SnapshotService::create(&dir, "ow", &book, Some("initial")).expect("create");
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let millis: i64 = stem.rsplit('.').next().unwrap().parse().unwrap();

        SnapshotService::tag(&dir, "ow", millis, "updated").expect("tag overwrite");

        let infos = SnapshotService::list(&dir, "ow").expect("list");
        assert_eq!(infos[0].label.as_deref(), Some("updated"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tag_errors_when_snapshot_missing() {
        let dir = temp_dir("tag-missing");
        let res = SnapshotService::tag(&dir, "nope", 999_999_999, "x");
        assert!(res.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_ignores_meta_sidecar_files() {
        let dir = temp_dir("list-meta-ignore");
        let book = make_book("MetaIgnore");
        SnapshotService::create(&dir, "mi", &book, Some("with-meta")).expect("create");
        // list は 1 件だけ (meta を snapshot として拾わない)
        let infos = SnapshotService::list(&dir, "mi").expect("list");
        assert_eq!(infos.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_does_not_include_non_snapshot_files() {
        let dir = temp_dir("list-filter");
        // 通常のbookファイルを作成
        std::fs::write(dir.join("my-book.json"), "{}").expect("write book");
        // changelogファイルを作成
        std::fs::write(dir.join("my-book.changelog.json"), "[]").expect("write changelog");

        let infos = SnapshotService::list(&dir, "my-book").expect("list");
        assert!(
            infos.is_empty(),
            "non-snapshot files should not be listed as snapshots"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
