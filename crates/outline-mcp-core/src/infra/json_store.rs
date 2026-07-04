use std::path::PathBuf;

use async_trait::async_trait;

use crate::domain::model::book::TemplateBook;
use crate::domain::repository::BookRepository;

/// Errors raised by `JsonBookRepository`.
#[derive(Debug, thiserror::Error)]
pub enum JsonStoreError {
    /// Underlying file I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The stored JSON could not be parsed (or serialized).
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// JSONファイルによるBookRepository実装。
/// 1 Book = 1 JSONファイル。
pub struct JsonBookRepository {
    path: PathBuf,
}

impl JsonBookRepository {
    /// Create a repository backed by the JSON file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl BookRepository for JsonBookRepository {
    type Error = JsonStoreError;

    async fn load(&self) -> Result<Option<TemplateBook>, Self::Error> {
        let content = match tokio::fs::read_to_string(&self.path).await {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let book: TemplateBook = serde_json::from_str(&content)?;
        Ok(Some(book))
    }

    async fn save(&self, book: &TemplateBook) -> Result<(), Self::Error> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let content = serde_json::to_string_pretty(book)?;
        let tmp = self.path.with_extension("tmp");
        tokio::fs::write(&tmp, &content).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::AddNodeRequest;
    use crate::domain::model::node::NodeType;

    #[tokio::test]
    async fn roundtrip_save_load() {
        let dir = std::env::temp_dir().join("outline-mcp-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("test-book.json");

        let repo = JsonBookRepository::new(&path);

        // 初回loadはNone
        assert!(repo.load().await.unwrap().is_none());

        // Book作成→保存→読み込み
        let mut book = TemplateBook::new("Roundtrip Test", 3);
        book.add_node(AddNodeRequest {
            parent: None,
            title: "Step 1".into(),
            node_type: NodeType::Content,
            body: Some("description".into()),
            placeholder: Some("notes".into()),
            position: usize::MAX,
            properties: std::collections::HashMap::new(),
        })
        .unwrap();

        repo.save(&book).await.unwrap();

        let loaded = repo.load().await.unwrap().unwrap();
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
