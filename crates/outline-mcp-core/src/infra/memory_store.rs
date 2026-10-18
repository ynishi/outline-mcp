use std::collections::BTreeMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::domain::model::book::TemplateBook;
use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::NodeId;
use crate::domain::model::timestamp::Timestamp;
use crate::domain::store::{
    BookStore, BookSummary, Committed, SnapshotInfo, SnapshotStore, StoreError, Versioned,
};

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

    async fn delete(&self, slug: &str, expected_version: i64) -> Result<bool, StoreError> {
        let mut books = self.books.lock().await;
        match books.get(slug).map(|s| s.version) {
            None => Ok(false),
            Some(v) if v == expected_version => Ok(books.remove(slug).is_some()),
            Some(v) => Err(StoreError::Conflict {
                expected: Some(expected_version),
                actual: Some(v),
            }),
        }
    }
}

struct StoredSnapshot {
    book: TemplateBook,
    label: Option<String>,
    size_bytes: u64,
}

impl StoredSnapshot {
    fn info(&self, millis: i64) -> SnapshotInfo {
        SnapshotInfo {
            timestamp: Timestamp::from_millis(millis),
            label: self.label.clone(),
            size_bytes: self.size_bytes,
        }
    }
}

/// In-memory [`SnapshotStore`], for tests and embedding. Keys are
/// `(slug, millis)`, as in the Postgres adapter.
#[derive(Default)]
pub struct MemorySnapshotStore {
    snapshots: Mutex<BTreeMap<(String, i64), StoredSnapshot>>,
}

impl MemorySnapshotStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

fn json_size(book: &TemplateBook) -> Result<u64, StoreError> {
    let bytes = serde_json::to_vec(book).map_err(StoreError::backend)?;
    Ok(bytes.len() as u64)
}

#[async_trait]
impl SnapshotStore for MemorySnapshotStore {
    async fn create(
        &self,
        slug: &str,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError> {
        let size_bytes = json_size(book)?;
        let mut snapshots = self.snapshots.lock().await;
        let latest = snapshots
            .range((slug.to_string(), i64::MIN)..=(slug.to_string(), i64::MAX))
            .next_back()
            .map(|((_, millis), _)| *millis);
        let now = Timestamp::now().as_millis();
        let millis = latest.map_or(now, |l| now.max(l + 1));
        let stored = StoredSnapshot {
            book: book.clone(),
            label: label.map(str::to_string),
            size_bytes,
        };
        let info = stored.info(millis);
        snapshots.insert((slug.to_string(), millis), stored);
        Ok(info)
    }

    async fn insert(
        &self,
        slug: &str,
        timestamp: Timestamp,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError> {
        let size_bytes = json_size(book)?;
        let mut snapshots = self.snapshots.lock().await;
        let key = (slug.to_string(), timestamp.as_millis());
        if snapshots.contains_key(&key) {
            return Err(StoreError::Conflict {
                expected: None,
                actual: Some(timestamp.as_millis()),
            });
        }
        let stored = StoredSnapshot {
            book: book.clone(),
            label: label.map(str::to_string),
            size_bytes,
        };
        let info = stored.info(timestamp.as_millis());
        snapshots.insert(key, stored);
        Ok(info)
    }

    async fn list(&self, slug: &str) -> Result<Vec<SnapshotInfo>, StoreError> {
        Ok(self
            .snapshots
            .lock()
            .await
            .range((slug.to_string(), i64::MIN)..=(slug.to_string(), i64::MAX))
            .rev()
            .map(|((_, millis), s)| s.info(*millis))
            .collect())
    }

    async fn get(
        &self,
        slug: &str,
        timestamp: Timestamp,
    ) -> Result<Option<TemplateBook>, StoreError> {
        Ok(self
            .snapshots
            .lock()
            .await
            .get(&(slug.to_string(), timestamp.as_millis()))
            .map(|s| s.book.clone()))
    }

    async fn tag(&self, slug: &str, timestamp: Timestamp, label: &str) -> Result<bool, StoreError> {
        Ok(
            match self
                .snapshots
                .lock()
                .await
                .get_mut(&(slug.to_string(), timestamp.as_millis()))
            {
                Some(s) => {
                    s.label = Some(label.to_string());
                    true
                }
                None => false,
            },
        )
    }

    async fn delete_all(&self, slug: &str) -> Result<u64, StoreError> {
        let mut snapshots = self.snapshots.lock().await;
        let before = snapshots.len();
        snapshots.retain(|(s, _), _| s != slug);
        Ok((before - snapshots.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::changelog::ChangeAction;

    #[tokio::test]
    async fn snapshots_list_newest_first_and_create_never_reuses_a_millisecond() {
        let store = MemorySnapshotStore::new();
        let book = TemplateBook::new("b", 4);
        let a = store.create("b", &book, Some("first")).await.unwrap();
        let b = store.create("b", &book, None).await.unwrap();
        assert!(b.timestamp > a.timestamp);
        let listed = store.list("b").await.unwrap();
        assert_eq!(listed, vec![b.clone(), a.clone()]);
        assert!(store.list("other").await.unwrap().is_empty());
        assert!(store.get("b", a.timestamp).await.unwrap().is_some());
        assert!(store
            .get("b", Timestamp::from_millis(1))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn snapshot_insert_keeps_the_timestamp_and_rejects_a_taken_one() {
        let store = MemorySnapshotStore::new();
        let book = TemplateBook::new("b", 4);
        let ts = Timestamp::from_millis(42);
        let info = store.insert("b", ts, &book, None).await.unwrap();
        assert_eq!(info.timestamp, ts);
        assert!(store.insert("b", ts, &book, None).await.is_err());
        assert!(store.tag("b", ts, "x").await.unwrap());
        assert!(!store
            .tag("b", Timestamp::from_millis(7), "x")
            .await
            .unwrap());
        assert_eq!(
            store.list("b").await.unwrap()[0].label.as_deref(),
            Some("x")
        );
    }

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

    #[tokio::test]
    async fn delete_removes_the_book_and_its_history_only() {
        let store = MemoryBookStore::new();
        store
            .commit(
                "gone",
                None,
                &TemplateBook::new("G", 4),
                &[entry(NodeId::new())],
            )
            .await
            .unwrap();
        store
            .commit(
                "kept",
                None,
                &TemplateBook::new("K", 4),
                &[entry(NodeId::new())],
            )
            .await
            .unwrap();

        assert!(store.delete("gone", 1).await.unwrap());
        assert!(store.load("gone").await.unwrap().is_none());
        assert!(store.history("gone").await.unwrap().is_empty());
        assert!(store.load("kept").await.unwrap().is_some());
        assert_eq!(store.history("kept").await.unwrap().len(), 1);

        assert!(
            !store.delete("gone", 1).await.unwrap(),
            "second delete finds nothing"
        );
        assert!(!store.delete("never", 1).await.unwrap());
    }

    #[tokio::test]
    async fn delete_at_a_stale_version_conflicts_and_keeps_the_book() {
        let store = MemoryBookStore::new();
        let book = TemplateBook::new("B", 4);
        store.commit("b", None, &book, &[]).await.unwrap();
        store
            .commit("b", Some(1), &book, &[entry(NodeId::new())])
            .await
            .unwrap();

        let err = store.delete("b", 1).await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Conflict {
                    expected: Some(1),
                    actual: Some(2)
                }
            ),
            "{err}"
        );
        assert!(store.load("b").await.unwrap().is_some());
        assert_eq!(store.history("b").await.unwrap().len(), 1);
        assert!(store.delete("b", 2).await.unwrap());
    }

    #[tokio::test]
    async fn snapshot_delete_all_removes_one_books_snapshots() {
        let store = MemorySnapshotStore::new();
        let book = TemplateBook::new("b", 4);
        store.create("gone", &book, None).await.unwrap();
        store.create("gone", &book, Some("x")).await.unwrap();
        store.create("kept", &book, None).await.unwrap();

        assert_eq!(store.delete_all("gone").await.unwrap(), 2);
        assert!(store.list("gone").await.unwrap().is_empty());
        assert_eq!(store.list("kept").await.unwrap().len(), 1);
        assert_eq!(store.delete_all("gone").await.unwrap(), 0);
    }
}
