use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::domain::model::book::TemplateBook;
use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::NodeId;
use crate::domain::store::{BookStore, BookSummary, Committed, StoreError, Versioned};

struct Stored {
    book: TemplateBook,
    version: i64,
    events: Vec<ChangeEntry>,
}

/// In-memory [`BookStore`], for tests and embedding.
///
/// Enforces the same version contract as the Postgres adapter: a create
/// fails on an existing slug, a replace fails unless the caller read the
/// current version. Versions start at 1.
#[derive(Default)]
pub struct MemoryBookStore {
    books: Mutex<BTreeMap<String, Stored>>,
}

impl MemoryBookStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BookStore for MemoryBookStore {
    async fn load(&self, slug: &str) -> Result<Option<Versioned<TemplateBook>>, StoreError> {
        Ok(self.books.lock().await.get(slug).map(|s| Versioned {
            value: s.book.clone(),
            version: s.version,
        }))
    }

    async fn commit(
        &self,
        slug: &str,
        expected_version: Option<i64>,
        book: &TemplateBook,
        events: &[ChangeEntry],
    ) -> Result<Committed, StoreError> {
        let mut books = self.books.lock().await;
        let actual = books.get(slug).map(|s| s.version);
        if actual != expected_version {
            return Err(StoreError::Conflict {
                expected: expected_version,
                actual,
            });
        }
        let version = actual.unwrap_or(0) + 1;
        let stored = books.entry(slug.to_string()).or_insert_with(|| Stored {
            book: book.clone(),
            version,
            events: Vec::new(),
        });
        stored.book = book.clone();
        stored.version = version;
        stored.events.extend_from_slice(events);
        Ok(Committed {
            version,
            warnings: Vec::new(),
        })
    }

    async fn history(&self, slug: &str) -> Result<Vec<ChangeEntry>, StoreError> {
        Ok(self
            .books
            .lock()
            .await
            .get(slug)
            .map(|s| s.events.clone())
            .unwrap_or_default())
    }

    async fn history_by_node(
        &self,
        slug: &str,
        node_id: NodeId,
    ) -> Result<Vec<ChangeEntry>, StoreError> {
        Ok(self
            .history(slug)
            .await?
            .into_iter()
            .filter(|e| e.node_id == node_id)
            .collect())
    }

    async fn list_books(&self) -> Result<Vec<BookSummary>, StoreError> {
        Ok(self
            .books
            .lock()
            .await
            .iter()
            .map(|(slug, s)| BookSummary {
                slug: slug.clone(),
                title: s.book.title().to_string(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::changelog::ChangeAction;
    use crate::domain::model::timestamp::Timestamp;

    fn entry(id: NodeId) -> ChangeEntry {
        ChangeEntry::new(
            id,
            ChangeAction::Create,
            None,
            None,
            Timestamp::from_millis(1),
        )
    }

    #[tokio::test]
    async fn create_then_replace_bumps_version() {
        let store = MemoryBookStore::new();
        let book = TemplateBook::new("b", 4);
        let c = store.commit("b", None, &book, &[]).await.unwrap();
        assert_eq!(c.version, 1);
        let c = store.commit("b", Some(1), &book, &[]).await.unwrap();
        assert_eq!(c.version, 2);
        assert_eq!(store.load("b").await.unwrap().unwrap().version, 2);
    }

    #[tokio::test]
    async fn create_on_existing_slug_conflicts() {
        let store = MemoryBookStore::new();
        let book = TemplateBook::new("b", 4);
        store.commit("b", None, &book, &[]).await.unwrap();
        let err = store.commit("b", None, &book, &[]).await.unwrap_err();
        assert!(matches!(
            err,
            StoreError::Conflict {
                expected: None,
                actual: Some(1)
            }
        ));
    }

    #[tokio::test]
    async fn stale_version_conflicts_and_stores_nothing() {
        let store = MemoryBookStore::new();
        let book = TemplateBook::new("b", 4);
        store.commit("b", None, &book, &[]).await.unwrap();
        store.commit("b", Some(1), &book, &[]).await.unwrap();
        let id = NodeId::new();
        let err = store
            .commit("b", Some(1), &book, &[entry(id)])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StoreError::Conflict {
                expected: Some(1),
                actual: Some(2)
            }
        ));
        assert!(store.history("b").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replace_on_missing_slug_conflicts() {
        let store = MemoryBookStore::new();
        let book = TemplateBook::new("b", 4);
        let err = store.commit("b", Some(1), &book, &[]).await.unwrap_err();
        assert!(matches!(
            err,
            StoreError::Conflict {
                expected: Some(1),
                actual: None
            }
        ));
    }

    #[tokio::test]
    async fn history_by_node_filters_and_list_books_orders_by_slug() {
        let store = MemoryBookStore::new();
        let (a, b) = (NodeId::new(), NodeId::new());
        store
            .commit("z", None, &TemplateBook::new("Z", 4), &[entry(a), entry(b)])
            .await
            .unwrap();
        store
            .commit("a", None, &TemplateBook::new("A", 4), &[])
            .await
            .unwrap();
        let h = store.history_by_node("z", a).await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].node_id, a);
        let slugs: Vec<_> = store
            .list_books()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.slug)
            .collect();
        assert_eq!(slugs, vec!["a", "z"]);
    }
}
