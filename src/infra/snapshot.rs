use std::path::{Path, PathBuf};

use crate::domain::model::book::TemplateBook;
use crate::domain::model::timestamp::Timestamp;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// スナップショットのメタ情報。
pub struct SnapshotInfo {
    pub timestamp: Timestamp,
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// スナップショットの作成・一覧・復元を行うサービス。
///
/// ファイル命名: `{slug}.snap.{millis}.json`
/// millis は Timestamp の Unix ミリ秒値（数値）。
pub struct SnapshotService;

impl SnapshotService {
    /// 現在の Book のスナップショットを作成する。
    ///
    /// 作成したファイルのパスを返す。
    pub fn create(shelf_dir: &Path, slug: &str, book: &TemplateBook) -> Result<PathBuf, BoxError> {
        std::fs::create_dir_all(shelf_dir).map_err(|e| -> BoxError { Box::new(e) })?;

        let millis = Timestamp::now().as_millis();
        let path = shelf_dir.join(format!("{slug}.snap.{millis}.json"));

        let content =
            serde_json::to_string_pretty(book).map_err(|e| -> BoxError { Box::new(e) })?;

        // atomic write: tmp → rename
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &content).map_err(|e| -> BoxError { Box::new(e) })?;
        std::fs::rename(&tmp, &path).map_err(|e| -> BoxError { Box::new(e) })?;

        Ok(path)
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
                // stem = "{slug}.snap.{millis}" → 最後の . 以降を millis としてパース
                let stem = path.file_stem()?.to_str()?;
                let millis_str = stem.rsplit('.').next()?;
                let millis: i64 = millis_str.parse().ok()?;
                let timestamp = Timestamp::from_millis(millis);
                let size_bytes = entry.metadata().ok()?.len();
                Some(SnapshotInfo {
                    timestamp,
                    path: path.clone(),
                    size_bytes,
                })
            })
            .collect();

        // タイムスタンプ降順でソート（最新が先頭）
        infos.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

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
        let path = SnapshotService::create(&dir, "my-book", &book).expect("create snapshot");
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
        SnapshotService::create(&dir, "list-book", &book).expect("create 1");
        // 同一ミリ秒衝突を避けるため少し待つ（テスト用途のみ）
        std::thread::sleep(std::time::Duration::from_millis(2));
        SnapshotService::create(&dir, "list-book", &book).expect("create 2");

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
        let path = SnapshotService::create(&dir, "restore-book", &book).expect("create");

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
