use std::path::PathBuf;

use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::NodeId;
use crate::domain::repository::ChangeLogRepository;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// JSON ファイルによる ChangeLogRepository 実装。
///
/// 1インスタンス = 1 slug = 1 ファイル (`{slug}.changelog.json`)。
/// append は read → push → atomic write パターン。
pub struct JsonChangeLogRepository {
    shelf_dir: PathBuf,
    slug: String,
}

impl JsonChangeLogRepository {
    pub fn new(shelf_dir: impl Into<PathBuf>, slug: impl Into<String>) -> Self {
        Self {
            shelf_dir: shelf_dir.into(),
            slug: slug.into(),
        }
    }

    fn changelog_path(&self) -> PathBuf {
        self.shelf_dir.join(format!("{}.changelog.json", self.slug))
    }
}

impl ChangeLogRepository for JsonChangeLogRepository {
    fn append(&self, entry: &ChangeEntry) -> Result<(), BoxError> {
        let path = self.changelog_path();

        let mut entries: Vec<ChangeEntry> = if path.exists() {
            let content =
                std::fs::read_to_string(&path).map_err(|e| -> BoxError { Box::new(e) })?;
            serde_json::from_str(&content).map_err(|e| -> BoxError { Box::new(e) })?
        } else {
            Vec::new()
        };

        entries.push(entry.clone());

        let content =
            serde_json::to_string_pretty(&entries).map_err(|e| -> BoxError { Box::new(e) })?;

        // atomic write: tmp → rename
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| -> BoxError { Box::new(e) })?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &content).map_err(|e| -> BoxError { Box::new(e) })?;
        std::fs::rename(&tmp, &path).map_err(|e| -> BoxError { Box::new(e) })?;

        Ok(())
    }

    fn load_all(&self) -> Result<Vec<ChangeEntry>, BoxError> {
        let path = self.changelog_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path).map_err(|e| -> BoxError { Box::new(e) })?;
        let entries: Vec<ChangeEntry> =
            serde_json::from_str(&content).map_err(|e| -> BoxError { Box::new(e) })?;
        Ok(entries)
    }

    fn load_by_node(&self, node_id: NodeId) -> Result<Vec<ChangeEntry>, BoxError> {
        let all = self.load_all()?;
        Ok(all.into_iter().filter(|e| e.node_id == node_id).collect())
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::changelog::{ChangeAction, ChangeEntry};
    use crate::domain::model::id::NodeId;
    use crate::domain::model::timestamp::Timestamp;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("outline-mcp-changelog-test-{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_entry(node_id: NodeId, action: ChangeAction, millis: i64) -> ChangeEntry {
        ChangeEntry::new(node_id, action, None, None, Timestamp::from_millis(millis))
    }

    #[test]
    fn test_append_and_load_all_roundtrip() {
        let dir = temp_dir("append-load");
        let repo = JsonChangeLogRepository::new(&dir, "test-book");

        let id1 = NodeId::new();
        let id2 = NodeId::new();
        let e1 = make_entry(id1, ChangeAction::Create, 1_000);
        let e2 = make_entry(id2, ChangeAction::Update, 2_000);

        repo.append(&e1).expect("append e1");
        repo.append(&e2).expect("append e2");

        let all = repo.load_all().expect("load_all");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].node_id, id1);
        assert_eq!(all[0].action, ChangeAction::Create);
        assert_eq!(all[0].timestamp.as_millis(), 1_000);
        assert_eq!(all[1].node_id, id2);
        assert_eq!(all[1].action, ChangeAction::Update);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_all_empty_when_no_file() {
        let dir = temp_dir("load-empty");
        let repo = JsonChangeLogRepository::new(&dir, "nonexistent");
        let all = repo.load_all().expect("load_all on missing");
        assert!(all.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_by_node_filters_correctly() {
        let dir = temp_dir("load-by-node");
        let repo = JsonChangeLogRepository::new(&dir, "filter-book");

        let id_target = NodeId::new();
        let id_other = NodeId::new();

        repo.append(&make_entry(id_target, ChangeAction::Create, 1_000))
            .expect("append 1");
        repo.append(&make_entry(id_other, ChangeAction::Update, 2_000))
            .expect("append 2");
        repo.append(&make_entry(id_target, ChangeAction::Update, 3_000))
            .expect("append 3");

        let filtered = repo.load_by_node(id_target).expect("load_by_node");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|e| e.node_id == id_target));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_two_instances_independent() {
        let dir = temp_dir("multi-slug");

        let id = NodeId::new();
        let repo_a = JsonChangeLogRepository::new(&dir, "book-a");
        let repo_b = JsonChangeLogRepository::new(&dir, "book-b");

        repo_a
            .append(&make_entry(id, ChangeAction::Create, 1_000))
            .expect("append a");
        repo_b
            .append(&make_entry(id, ChangeAction::Delete, 2_000))
            .expect("append b");

        let a_entries = repo_a.load_all().expect("load a");
        let b_entries = repo_b.load_all().expect("load b");
        assert_eq!(a_entries.len(), 1);
        assert_eq!(b_entries.len(), 1);
        assert_eq!(a_entries[0].action, ChangeAction::Create);
        assert_eq!(b_entries[0].action, ChangeAction::Delete);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_changelog_file_naming() {
        let dir = temp_dir("file-naming");
        let repo = JsonChangeLogRepository::new(&dir, "my-slug");
        let id = NodeId::new();
        repo.append(&make_entry(id, ChangeAction::Create, 1_000))
            .expect("append");

        let expected_path = dir.join("my-slug.changelog.json");
        assert!(
            expected_path.exists(),
            "changelog file should be created at {expected_path:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
