use std::path::PathBuf;

use crate::domain::model::book::TemplateBook;
use crate::domain::repository::BookRepository;

#[derive(Debug, thiserror::Error)]
pub enum JsonStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// JSONファイルによるBookRepository実装。
/// 1 Book = 1 JSONファイル。
pub struct JsonBookRepository {
    path: PathBuf,
}

impl JsonBookRepository {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl BookRepository for JsonBookRepository {
    type Error = JsonStoreError;

    fn load(&self) -> Result<Option<TemplateBook>, Self::Error> {
        if !self.path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&self.path)?;
        let book: TemplateBook = serde_json::from_str(&content)?;
        Ok(Some(book))
    }

    fn save(&self, book: &TemplateBook) -> Result<(), Self::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(book)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::AddNodeRequest;
    use crate::domain::model::node::NodeType;

    #[test]
    fn roundtrip_save_load() {
        let dir = std::env::temp_dir().join("outline-mcp-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("test-book.json");

        let repo = JsonBookRepository::new(&path);

        // 初回loadはNone
        assert!(repo.load().unwrap().is_none());

        // Book作成→保存→読み込み
        let mut book = TemplateBook::new("Roundtrip Test", 3);
        book.add_node(AddNodeRequest {
            parent: None,
            title: "Step 1".into(),
            node_type: NodeType::Content,
            body: Some("description".into()),
            placeholder: Some("notes".into()),
            position: usize::MAX,
        })
        .unwrap();

        repo.save(&book).unwrap();

        let loaded = repo.load().unwrap().unwrap();
        assert_eq!(loaded.title(), "Roundtrip Test");
        assert_eq!(loaded.node_count(), 1);
        assert_eq!(loaded.root_nodes().len(), 1);

        let node = loaded.get_node(loaded.root_nodes()[0]).unwrap();
        assert_eq!(node.title(), "Step 1");
        assert_eq!(node.body(), Some("description"));
        assert_eq!(node.placeholder(), Some("notes"));

        // cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
