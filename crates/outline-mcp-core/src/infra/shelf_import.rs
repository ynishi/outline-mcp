//! Import a pre-Postgres shelf directory into a [`BookStore`] +
//! [`SnapshotStore`] (the `import-shelf` command).
//!
//! # What a shelf holds, and where each part comes from
//!
//! | part | source | how it is read |
//! |---|---|---|
//! | the book | `{slug}.json` | [`JsonBookRepository`] |
//! | history | `{slug}.changelog.json` (oldest) + the ai-store `{slug}.events.db` changelog stream | [`HistoryPreservingChangeLogRepository::load_all`] |
//! | snapshots | `{slug}.snap.{millis}.json` + sidecar `{slug}.snap.{millis}.meta.json` (label) | read directly |
//!
//! History is read event by event from each event's stored before/after
//! (`Store::read`), never by replaying patches, so a stream whose replay fails
//! (an event that cannot be applied to the state before it) imports in full.
//! Snapshot files are the shelf's own record of which snapshots exist (the
//! snapshot tools listed exactly these files), so they are read as files.
//!
//! Timestamps are kept: each event keeps its time, each snapshot its
//! millisecond, each node its `updated_at`.
//!
//! # Verification
//!
//! [`import_book`] reads the imported book back and compares it with the
//! source: node count, a SHA-256 over every node's identity and content
//! ([`book_fingerprint`]), the number of history entries, and the number of
//! snapshots. A snapshot file that cannot be parsed counts against the source
//! and is reported, so it shows as a mismatch rather than disappearing.
//!
//! # Side effects on the source
//!
//! Opening `{slug}.events.db` goes through SQLite, which may create its
//! `-wal` / `-shm` files next to it. Nothing else in the shelf is written.
//! Import from a copy, or while no server is using the shelf.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai_store_sqlite::SqliteStore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::domain::model::book::TemplateBook;
use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::NodeId;
use crate::domain::model::timestamp::Timestamp;
use crate::domain::repository::{BookRepository, ChangeLogRepository};
use crate::domain::store::{BookStore, SnapshotStore, StoreError};
use crate::infra::changelog_bridge::HistoryPreservingChangeLogRepository;
use crate::infra::changelog_store::JsonChangeLogRepository;
use crate::infra::json_store::JsonBookRepository;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One snapshot file of a shelf book.
pub struct ShelfSnapshot {
    /// The millisecond in the file name.
    pub timestamp: Timestamp,
    /// The label from the sidecar `.meta.json`, if any.
    pub label: Option<String>,
    /// The book the file holds.
    pub book: TemplateBook,
}

/// Everything [`read_shelf_book`] found for one slug.
pub struct ShelfBook {
    /// The book's slug (its file stem).
    pub slug: String,
    /// The current book.
    pub book: TemplateBook,
    /// Every recorded change, legacy JSON entries first, then the ai-store
    /// stream's, each in stored order.
    pub history: Vec<ChangeEntry>,
    /// Parsed snapshot files, oldest first.
    pub snapshots: Vec<ShelfSnapshot>,
    /// Snapshot files that exist but could not be read, with the reason.
    pub unreadable_snapshots: Vec<(PathBuf, String)>,
}

/// The counts and hash [`import_book`] compares between source and store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// Number of nodes.
    pub nodes: usize,
    /// [`book_fingerprint`] of the book.
    pub hash: String,
    /// Number of history entries.
    pub history: usize,
    /// Number of snapshots (for the source: files found, readable or not).
    pub snapshots: usize,
}

/// Result of importing one book.
#[derive(Debug)]
pub struct ImportReport {
    /// The book's slug.
    pub slug: String,
    /// What the shelf holds.
    pub source: Fingerprint,
    /// What the store holds after the import.
    pub imported: Fingerprint,
    /// Snapshot files that could not be read (they make the counts differ).
    pub unreadable_snapshots: Vec<(PathBuf, String)>,
}

impl ImportReport {
    /// Whether the store holds exactly what the shelf held.
    pub fn matches(&self) -> bool {
        self.source == self.imported && self.unreadable_snapshots.is_empty()
    }
}

/// Why [`import_book`] did not import.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The slug already exists in the store and `replace` was not set.
    #[error("book '{0}' already exists in the database (pass --replace to overwrite it)")]
    AlreadyExists(String),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Slugs of the books in `shelf_dir`: `{slug}.json` files whose stem has no
/// `.` (which excludes `{slug}.snap.*.json` / `{slug}.changelog.json`),
/// sorted.
pub fn list_shelf_slugs(shelf_dir: &Path) -> std::io::Result<Vec<String>> {
    let mut slugs: Vec<String> = std::fs::read_dir(shelf_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            (!stem.contains('.')).then(|| stem.to_string())
        })
        .collect();
    slugs.sort();
    Ok(slugs)
}

/// Read the book, history and snapshots of `slug` from `shelf_dir`.
pub async fn read_shelf_book(shelf_dir: &Path, slug: &str) -> Result<ShelfBook, BoxError> {
    let book = JsonBookRepository::new(shelf_dir.join(format!("{slug}.json")))
        .load()
        .await?
        .ok_or_else(|| format!("{slug}.json not found in {}", shelf_dir.display()))?;

    let events_db = shelf_dir.join(format!("{slug}.events.db"));
    let history = if events_db.exists() {
        // `_sqlite` keeps the backend thread alive while the store is read.
        let sqlite = SqliteStore::open(&events_db).await?;
        let store = Arc::new(sqlite.store().clone());
        let repo = HistoryPreservingChangeLogRepository::new(store, shelf_dir, slug)?;
        let history = repo.load_all().await?;
        drop(sqlite);
        history
    } else {
        JsonChangeLogRepository::new(shelf_dir, slug)
            .load_all()
            .await?
    };

    let (snapshots, unreadable_snapshots) = read_snapshot_files(shelf_dir, slug)?;
    Ok(ShelfBook {
        slug: slug.to_string(),
        book,
        history,
        snapshots,
        unreadable_snapshots,
    })
}

#[derive(Deserialize)]
struct SnapshotMeta {
    #[serde(default)]
    label: Option<String>,
}

type SnapshotFiles = (Vec<ShelfSnapshot>, Vec<(PathBuf, String)>);

/// `{slug}.snap.{millis}.json` files (sidecars excluded), oldest first.
fn read_snapshot_files(shelf_dir: &Path, slug: &str) -> Result<SnapshotFiles, BoxError> {
    let prefix = format!("{slug}.snap.");
    let mut found: BTreeMap<i64, PathBuf> = BTreeMap::new();
    for entry in std::fs::read_dir(shelf_dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(millis) = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".json"))
            .filter(|rest| !rest.ends_with(".meta"))
            .and_then(|rest| rest.parse::<i64>().ok())
        else {
            continue;
        };
        found.insert(millis, path);
    }

    let mut snapshots = Vec::with_capacity(found.len());
    let mut unreadable = Vec::new();
    for (millis, path) in found {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<TemplateBook>(&s).map_err(|e| e.to_string()));
        match parsed {
            Ok(book) => {
                let meta_path = shelf_dir.join(format!("{slug}.snap.{millis}.meta.json"));
                let label = std::fs::read_to_string(meta_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<SnapshotMeta>(&s).ok())
                    .and_then(|m| m.label);
                snapshots.push(ShelfSnapshot {
                    timestamp: Timestamp::from_millis(millis),
                    label,
                    book,
                });
            }
            Err(reason) => unreadable.push((path, reason)),
        }
    }
    Ok((snapshots, unreadable))
}

/// SHA-256 (hex) over every node's id, parent, position among its siblings,
/// title, body, properties (key-sorted) and status, nodes sorted by id.
///
/// Two books with the same fingerprint have the same tree and the same
/// content; the book's own title, `updated_at` stamps, node type and
/// placeholder are not part of it.
pub fn book_fingerprint(book: &TemplateBook) -> String {
    let mut position: HashMap<NodeId, usize> = HashMap::new();
    for (i, id) in book.root_nodes().iter().enumerate() {
        position.insert(*id, i);
    }
    for id in book.all_node_ids() {
        if let Some(node) = book.get_node(id) {
            for (i, child) in node.children().iter().enumerate() {
                position.insert(*child, i);
            }
        }
    }

    let mut ids: Vec<NodeId> = book.all_node_ids().collect();
    ids.sort_by_key(|id| id.to_string());
    let mut hasher = Sha256::new();
    for id in ids {
        let Some(node) = book.get_node(id) else {
            continue;
        };
        let properties: BTreeMap<&String, &String> = node.properties().iter().collect();
        let record = serde_json::json!([
            id.to_string(),
            node.parent().map(|p| p.to_string()),
            position.get(&id),
            node.title(),
            node.body(),
            properties,
            node.status(),
        ]);
        hasher.update(record.to_string().as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn source_fingerprint(source: &ShelfBook) -> Fingerprint {
    Fingerprint {
        nodes: source.book.node_count(),
        hash: book_fingerprint(&source.book),
        history: source.history.len(),
        snapshots: source.snapshots.len() + source.unreadable_snapshots.len(),
    }
}

/// Write `source` into the stores as `source.slug` and read it back.
///
/// The book and its whole history go in one [`BookStore::commit`]; the
/// snapshots follow, each at its original millisecond. With `replace`, an
/// existing book of that slug is first removed by `delete` (which must also
/// remove its history and snapshots); without it, an existing book is an
/// [`ImportError::AlreadyExists`] and nothing is written.
pub async fn import_book<B, S, D, F>(
    books: &B,
    snapshots: &S,
    source: ShelfBook,
    replace: bool,
    delete: D,
) -> Result<ImportReport, ImportError>
where
    B: BookStore + ?Sized,
    S: SnapshotStore + ?Sized,
    D: FnOnce(String) -> F,
    F: std::future::Future<Output = Result<bool, StoreError>>,
{
    let slug = source.slug.clone();
    if books.load(&slug).await?.is_some() {
        if !replace {
            return Err(ImportError::AlreadyExists(slug));
        }
        delete(slug.clone()).await?;
    }

    let expected = source_fingerprint(&source);
    books
        .commit(&slug, None, &source.book, &source.history)
        .await?;
    for snap in &source.snapshots {
        snapshots
            .insert(&slug, snap.timestamp, &snap.book, snap.label.as_deref())
            .await?;
    }

    let stored = books
        .load(&slug)
        .await?
        .ok_or_else(|| StoreError::backend(format!("book '{slug}' missing right after import")))?
        .value;
    let imported = Fingerprint {
        nodes: stored.node_count(),
        hash: book_fingerprint(&stored),
        history: books.history(&slug).await?.len(),
        snapshots: snapshots.list(&slug).await?.len(),
    };
    Ok(ImportReport {
        slug,
        source: expected,
        imported,
        unreadable_snapshots: source.unreadable_snapshots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::AddNodeRequest;
    use crate::domain::model::changelog::ChangeAction;
    use crate::domain::model::node::NodeType;
    use crate::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};

    fn sample_book() -> TemplateBook {
        let mut book = TemplateBook::new("Shelf book", 4);
        let a = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "A".into(),
                node_type: NodeType::Section,
                body: Some("a body".into()),
                placeholder: None,
                position: usize::MAX,
                properties: [("inject".to_string(), "true".to_string())].into(),
            })
            .unwrap();
        book.add_node(AddNodeRequest {
            parent: Some(a),
            title: "A1".into(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: Default::default(),
        })
        .unwrap();
        book
    }

    fn write_shelf(dir: &Path, book: &TemplateBook) {
        std::fs::write(dir.join("b.json"), serde_json::to_string(book).unwrap()).unwrap();
        let id = book.root_nodes()[0];
        let legacy = vec![ChangeEntry::new(
            id,
            ChangeAction::Create,
            None,
            None,
            Timestamp::from_millis(10),
        )];
        std::fs::write(
            dir.join("b.changelog.json"),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("b.snap.1000.json"),
            serde_json::to_string(book).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("b.snap.1000.meta.json"), r#"{"label":"v1"}"#).unwrap();
        std::fs::write(
            dir.join("b.snap.2000.json"),
            serde_json::to_string(book).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn slugs_skip_sidecar_files() {
        let dir = tempfile::tempdir().unwrap();
        write_shelf(dir.path(), &sample_book());
        assert_eq!(list_shelf_slugs(dir.path()).unwrap(), vec!["b"]);
    }

    #[test]
    fn fingerprint_ignores_map_order_but_sees_content() {
        let book = sample_book();
        let json = serde_json::to_string(&book).unwrap();
        let again: TemplateBook = serde_json::from_str(&json).unwrap();
        assert_eq!(book_fingerprint(&book), book_fingerprint(&again));

        let mut changed = book.clone();
        let id = changed.root_nodes()[0];
        changed
            .update_node(
                id,
                crate::domain::model::book::UpdateNodeRequest {
                    title: None,
                    body: Some(Some("other".into())),
                    node_type: None,
                    placeholder: None,
                    properties: None,
                    properties_patch: None,
                    status: None,
                },
            )
            .unwrap();
        assert_ne!(book_fingerprint(&book), book_fingerprint(&changed));
    }

    #[tokio::test]
    async fn import_round_trips_and_refuses_an_existing_slug() {
        let dir = tempfile::tempdir().unwrap();
        let book = sample_book();
        write_shelf(dir.path(), &book);

        let books = MemoryBookStore::new();
        let snaps = MemorySnapshotStore::new();
        let source = read_shelf_book(dir.path(), "b").await.unwrap();
        assert_eq!(source.history.len(), 1);
        assert_eq!(source.snapshots.len(), 2);
        assert_eq!(source.snapshots[0].label.as_deref(), Some("v1"));

        let report = import_book(&books, &snaps, source, false, |_| async { Ok(true) })
            .await
            .unwrap();
        assert!(report.matches(), "{report:?}");
        assert_eq!(report.imported.nodes, 2);
        let listed = snaps.list("b").await.unwrap();
        assert_eq!(listed[1].timestamp, Timestamp::from_millis(1000));
        assert_eq!(listed[1].label.as_deref(), Some("v1"));
        assert_eq!(
            books.history("b").await.unwrap()[0].timestamp,
            Timestamp::from_millis(10)
        );

        let again = read_shelf_book(dir.path(), "b").await.unwrap();
        let err = import_book(&books, &snaps, again, false, |_| async { Ok(true) })
            .await
            .unwrap_err();
        assert!(matches!(err, ImportError::AlreadyExists(s) if s == "b"));
    }

    #[tokio::test]
    async fn unreadable_snapshot_is_a_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        write_shelf(dir.path(), &sample_book());
        std::fs::write(dir.path().join("b.snap.3000.json"), "{ not json").unwrap();

        let source = read_shelf_book(dir.path(), "b").await.unwrap();
        assert_eq!(source.unreadable_snapshots.len(), 1);
        let report = import_book(
            &MemoryBookStore::new(),
            &MemorySnapshotStore::new(),
            source,
            false,
            |_| async { Ok(true) },
        )
        .await
        .unwrap();
        assert!(!report.matches());
        assert_eq!(report.source.snapshots, 3);
        assert_eq!(report.imported.snapshots, 2);
    }
}
