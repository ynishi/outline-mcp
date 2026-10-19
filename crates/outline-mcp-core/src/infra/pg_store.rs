//! [`PgBookStore`]: books in Postgres, one transaction per write.
//!
//! # The commit
//!
//! [`BookStore::commit`] runs in one transaction:
//!
//! 1. Lock the book row (`SELECT version ... FOR UPDATE`) and compare its
//!    version with the caller's. A create inserts the row with
//!    `ON CONFLICT DO NOTHING` instead, so two concurrent creators cannot both
//!    win: the loser sees no row inserted and answers a conflict.
//! 2. Diff the stored node rows against the new book and write only what
//!    changed (delete the missing, upsert the new or different). A one-node
//!    edit on a book of a thousand nodes writes one row, plus the siblings
//!    whose position moved.
//! 3. Insert the write's events and bump the version.
//!
//! Either all of it commits or none of it does, so a stored change always
//! has its events — the property the pre-port storage lacked.
//!
//! # Events
//!
//! An event stores the whole node before and after as `jsonb`. History is
//! read row by row; nothing is replayed, so no stored event can make a later
//! read or write fail. `before` / `after` come back re-serialized from
//! `jsonb`, so key order and whitespace may differ from the strings written;
//! the JSON value is the same.
//!
//! Node rows and events are written with `UNNEST` statements of at most
//! [`CHUNK_ROWS`] rows and about [`CHUNK_BYTES`] of payload each, all inside
//! the one transaction. A small edit is one statement; an import of a
//! thousand-node book with an 8 MB history is a few dozen, and still commits
//! or rolls back as a whole. One statement per table was tried first: a
//! ~10 MB `UNNEST` sent a 256 MB Postgres machine into crash recovery.
//!
//! # Snapshots
//!
//! The same type implements [`SnapshotStore`] over the `snapshots` table
//! (`migrations/0002_snapshots.sql`): one row per snapshot holding the whole
//! book as `jsonb`, keyed by `(book, taken_at)`.
//!
//! # Connecting
//!
//! [`PgBookStore::connect_with`] fails fast and says which of the usual
//! causes it was ([`ConnectError`]); [`PgBookStore::pending_migrations`] tells
//! a server that the schema has not been migrated yet. Neither message
//! repeats the connection URL, which carries the password.
//!
//! All SQL is runtime-checked (`sqlx::query`), so building this crate needs
//! no database.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::domain::model::book::TemplateBook;
use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::{BookId, NodeId};
use crate::domain::model::node::{Link, NodeParts, TemplateNode};
use crate::domain::model::timestamp::Timestamp;
use crate::domain::store::{
    BookStore, BookSummary, Committed, SnapshotDelete, SnapshotInfo, SnapshotStore, StoreError,
    Versioned,
};

/// The bundled schema migrations (`crates/outline-mcp-core/migrations`).
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// How long [`PgBookStore::connect_with`] waits for a first connection by
/// default before reporting the database unreachable.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Most rows one bulk statement in [`BookStore::commit`] carries.
pub const CHUNK_ROWS: usize = 500;

/// Approximate payload one bulk statement in [`BookStore::commit`] carries;
/// a chunk closes at whichever of this and [`CHUNK_ROWS`] it reaches first.
pub const CHUNK_BYTES: usize = 1024 * 1024;

/// Split `items` into consecutive ranges of at most [`CHUNK_ROWS`] items and,
/// by `size`, about [`CHUNK_BYTES`] each. A single item larger than
/// [`CHUNK_BYTES`] gets a range of its own.
fn chunk_ranges<T>(items: &[T], size: impl Fn(&T) -> usize) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (i, item) in items.iter().enumerate() {
        let s = size(item);
        if i > start && (i - start >= CHUNK_ROWS || bytes + s > CHUNK_BYTES) {
            ranges.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += s;
    }
    if start < items.len() {
        ranges.push(start..items.len());
    }
    ranges
}

/// Books, nodes and events in Postgres. Cheap to clone (shares one pool).
#[derive(Clone)]
pub struct PgBookStore {
    pool: PgPool,
}

/// Why [`PgBookStore::connect_with`] could not connect.
///
/// The messages name the cause and what to check; none of them includes the
/// connection URL.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The URL could not be parsed as a Postgres connection URL.
    #[error("the database URL is not a valid Postgres connection URL: {0}")]
    InvalidUrl(String),
    /// No connection could be opened in time (host down, wrong host or port,
    /// network, TLS).
    #[error("could not reach the database server (check host, port and network): {0}")]
    Unreachable(String),
    /// The server rejected the user name or password.
    #[error("the database server rejected the login (check user name and password): {0}")]
    Authentication(String),
    /// The server is up but the named database does not exist.
    #[error("the database does not exist on the server (create it, or fix the database name in the URL): {0}")]
    DatabaseMissing(String),
    /// Anything else the driver reported.
    #[error("could not connect to the database: {0}")]
    Other(String),
}

impl ConnectError {
    fn classify(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::Configuration(inner) => Self::InvalidUrl(inner.to_string()),
            sqlx::Error::Database(db) => match db.code().as_deref() {
                Some("28P01") | Some("28000") => Self::Authentication(db.message().to_string()),
                Some("3D000") => Self::DatabaseMissing(db.message().to_string()),
                _ => Self::Other(db.message().to_string()),
            },
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolTimedOut => {
                Self::Unreachable(e.to_string())
            }
            _ => Self::Other(e.to_string()),
        }
    }
}

/// One `nodes` row, as written and as compared when diffing.
#[derive(Debug, Clone, PartialEq)]
struct NodeRow {
    id: Uuid,
    parent: Option<Uuid>,
    position: i32,
    title: String,
    body: Option<String>,
    node_type: String,
    placeholder: Option<String>,
    properties: Value,
    status: String,
    updated_at: Option<i64>,
    verified_at: Option<i64>,
    verified_by: Option<String>,
    links: Value,
}

impl NodeRow {
    /// Rough wire size of the row, for [`chunk_ranges`].
    fn approx_bytes(&self) -> usize {
        64 + self.title.len()
            + self.body.as_ref().map_or(0, String::len)
            + self.placeholder.as_ref().map_or(0, String::len)
            + self.properties.to_string().len()
            + self.links.to_string().len()
    }
}

impl PgBookStore {
    /// Connect to the database at `database_url`, waiting at most
    /// [`DEFAULT_CONNECT_TIMEOUT`]. Does not run migrations.
    pub async fn connect(database_url: &str) -> Result<Self, ConnectError> {
        Self::connect_with(database_url, DEFAULT_CONNECT_TIMEOUT).await
    }

    /// Connect to the database at `database_url`, waiting at most `timeout`
    /// for the first connection. Does not run migrations.
    pub async fn connect_with(database_url: &str, timeout: Duration) -> Result<Self, ConnectError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(timeout)
            .connect(database_url)
            .await
            .map_err(ConnectError::classify)?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the bundled migrations (`crates/outline-mcp-core/migrations`).
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await.map_err(StoreError::backend)
    }

    /// Versions of the bundled migrations not yet applied to this database,
    /// oldest first. Empty means the schema is current.
    pub async fn pending_migrations(&self) -> Result<Vec<i64>, StoreError> {
        let applied: Vec<i64> =
            match sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
                .fetch_all(&self.pool)
                .await
            {
                Ok(v) => v,
                // 42P01 = undefined_table: nothing has ever been migrated here.
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("42P01") => {
                    Vec::new()
                }
                Err(e) => return Err(StoreError::backend(e)),
            };
        Ok(MIGRATOR
            .iter()
            .map(|m| m.version)
            .filter(|v| !applied.contains(v))
            .collect())
    }

    /// Delete a book with its nodes, events and snapshots. Returns whether it
    /// existed. Used by `import-shelf --replace`.
    pub async fn delete_book(&self, slug: &str) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM books WHERE slug = $1")
            .bind(slug)
            .execute(&self.pool)
            .await
            .map_err(StoreError::backend)?
            .rows_affected();
        Ok(deleted > 0)
    }

    async fn node_rows(
        tx: &mut Transaction<'_, Postgres>,
        slug: &str,
    ) -> Result<HashMap<Uuid, NodeRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, parent, position, title, body, node_type, placeholder, properties, status, updated_at, \
                    verified_at, verified_by, links \
             FROM nodes WHERE book = $1",
        )
        .bind(slug)
        .fetch_all(&mut **tx)
        .await
        .map_err(StoreError::backend)?;
        rows.iter()
            .map(|r| {
                let row = node_row(r)?;
                Ok((row.id, row))
            })
            .collect()
    }
}

#[async_trait]
impl BookStore for PgBookStore {
    async fn load(&self, slug: &str) -> Result<Option<Versioned<TemplateBook>>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::backend)?;
        let Some(book_row) =
            sqlx::query("SELECT book_id, title, max_depth, version FROM books WHERE slug = $1")
                .bind(slug)
                .fetch_optional(&mut *tx)
                .await
                .map_err(StoreError::backend)?
        else {
            return Ok(None);
        };
        let rows = Self::node_rows(&mut tx, slug).await?;
        tx.commit().await.map_err(StoreError::backend)?;

        let book_id: Uuid = book_row.try_get("book_id").map_err(StoreError::backend)?;
        let title: String = book_row.try_get("title").map_err(StoreError::backend)?;
        let max_depth: i16 = book_row.try_get("max_depth").map_err(StoreError::backend)?;
        let version: i64 = book_row.try_get("version").map_err(StoreError::backend)?;
        let max_depth = u8::try_from(max_depth).map_err(StoreError::backend)?;

        let book = assemble(BookId::from_uuid(book_id), title, max_depth, rows)?;
        Ok(Some(Versioned {
            value: book,
            version,
        }))
    }

    async fn commit(
        &self,
        slug: &str,
        expected_version: Option<i64>,
        book: &TemplateBook,
        events: &[ChangeEntry],
    ) -> Result<Committed, StoreError> {
        let new_rows = rows_of(book)?;
        let max_depth = i16::from(book.max_depth());
        let mut tx = self.pool.begin().await.map_err(StoreError::backend)?;

        let (version, old_rows) = match expected_version {
            None => {
                let inserted = sqlx::query(
                    "INSERT INTO books (slug, book_id, title, max_depth, version) \
                     VALUES ($1, $2, $3, $4, 1) ON CONFLICT (slug) DO NOTHING",
                )
                .bind(slug)
                .bind(book.id().as_uuid())
                .bind(book.title())
                .bind(max_depth)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::backend)?
                .rows_affected();
                if inserted == 0 {
                    drop(tx);
                    let actual: Option<i64> =
                        sqlx::query_scalar("SELECT version FROM books WHERE slug = $1")
                            .bind(slug)
                            .fetch_optional(&self.pool)
                            .await
                            .map_err(StoreError::backend)?;
                    return Err(StoreError::Conflict {
                        expected: None,
                        actual,
                    });
                }
                (1, HashMap::new())
            }
            Some(expected) => {
                let actual: Option<i64> =
                    sqlx::query_scalar("SELECT version FROM books WHERE slug = $1 FOR UPDATE")
                        .bind(slug)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(StoreError::backend)?;
                if actual != Some(expected) {
                    return Err(StoreError::Conflict {
                        expected: Some(expected),
                        actual,
                    });
                }
                let version = expected + 1;
                sqlx::query(
                    "UPDATE books SET book_id = $2, title = $3, max_depth = $4, version = $5, \
                     updated_at = now() WHERE slug = $1",
                )
                .bind(slug)
                .bind(book.id().as_uuid())
                .bind(book.title())
                .bind(max_depth)
                .bind(version)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::backend)?;
                (version, Self::node_rows(&mut tx, slug).await?)
            }
        };

        let removed: Vec<Uuid> = old_rows
            .keys()
            .filter(|id| !new_rows.contains_key(id))
            .copied()
            .collect();
        for range in chunk_ranges(&removed, |_| 16) {
            sqlx::query("DELETE FROM nodes WHERE book = $1 AND id = ANY($2)")
                .bind(slug)
                .bind(&removed[range])
                .execute(&mut *tx)
                .await
                .map_err(StoreError::backend)?;
        }
        let changed_all: Vec<&NodeRow> = new_rows
            .values()
            .filter(|row| old_rows.get(&row.id) != Some(*row))
            .collect();
        for range in chunk_ranges(&changed_all, |r| r.approx_bytes()) {
            let changed = &changed_all[range];
            sqlx::query(
                "INSERT INTO nodes (book, id, parent, position, title, body, node_type, placeholder, properties, status, \
                                    updated_at, verified_at, verified_by, links) \
                 SELECT $1, * FROM UNNEST($2::uuid[], $3::uuid[], $4::int4[], $5::text[], $6::text[], \
                                          $7::text[], $8::text[], $9::jsonb[], $10::text[], $11::int8[], \
                                          $12::int8[], $13::text[], $14::jsonb[]) \
                 ON CONFLICT (book, id) DO UPDATE SET parent = EXCLUDED.parent, position = EXCLUDED.position, \
                 title = EXCLUDED.title, body = EXCLUDED.body, node_type = EXCLUDED.node_type, \
                 placeholder = EXCLUDED.placeholder, properties = EXCLUDED.properties, \
                 status = EXCLUDED.status, updated_at = EXCLUDED.updated_at, \
                 verified_at = EXCLUDED.verified_at, verified_by = EXCLUDED.verified_by, \
                 links = EXCLUDED.links",
            )
            .bind(slug)
            .bind(changed.iter().map(|r| r.id).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.parent).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.position).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.title.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.body.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.node_type.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.placeholder.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.properties.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.status.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.updated_at).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.verified_at).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.verified_by.clone()).collect::<Vec<_>>())
            .bind(changed.iter().map(|r| r.links.clone()).collect::<Vec<_>>())
            .execute(&mut *tx)
            .await
            .map_err(StoreError::backend)?;
        }

        let event_size = |e: &ChangeEntry| {
            64 + e.before.as_ref().map_or(0, String::len) + e.after.as_ref().map_or(0, String::len)
        };
        // Chunks run in order, and each keeps its own order, so `id` (the
        // history order) still follows `events`.
        for range in chunk_ranges(events, event_size) {
            let events = &events[range];
            let mut node_ids = Vec::with_capacity(events.len());
            let mut actions = Vec::with_capacity(events.len());
            let mut befores = Vec::with_capacity(events.len());
            let mut afters = Vec::with_capacity(events.len());
            let mut ats = Vec::with_capacity(events.len());
            for entry in events {
                // A book-level entry (`book_update`) has no node: NULL, which
                // `change_entry` reads back as `NodeId::book_level()`.
                node_ids.push((!entry.node_id.is_book_level()).then(|| entry.node_id.as_uuid()));
                actions.push(enum_str(&entry.action)?);
                befores.push(json_column(&entry.before)?);
                afters.push(json_column(&entry.after)?);
                ats.push(entry.timestamp.as_millis());
            }
            // ORDER BY ord keeps `id` (the history order) in the order given.
            sqlx::query(
                "INSERT INTO events (book, node_id, action, before, after, at) \
                 SELECT $1, u.node_id, u.action, u.before, u.after, u.at \
                 FROM UNNEST($2::uuid[], $3::text[], $4::jsonb[], $5::jsonb[], $6::int8[]) \
                      WITH ORDINALITY AS u(node_id, action, before, after, at, ord) \
                 ORDER BY u.ord",
            )
            .bind(slug)
            .bind(node_ids)
            .bind(actions)
            .bind(befores)
            .bind(afters)
            .bind(ats)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::backend)?;
        }

        tx.commit().await.map_err(StoreError::backend)?;
        Ok(Committed {
            version,
            warnings: Vec::new(),
        })
    }

    async fn history(&self, slug: &str) -> Result<Vec<ChangeEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT node_id, action, before, after, at FROM events WHERE book = $1 ORDER BY id",
        )
        .bind(slug)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::backend)?;
        rows.iter().map(change_entry).collect()
    }

    async fn history_by_node(
        &self,
        slug: &str,
        node_id: NodeId,
    ) -> Result<Vec<ChangeEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT node_id, action, before, after, at FROM events \
             WHERE book = $1 AND node_id = $2 ORDER BY id",
        )
        .bind(slug)
        .bind(node_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::backend)?;
        rows.iter().map(change_entry).collect()
    }

    async fn list_books(&self) -> Result<Vec<BookSummary>, StoreError> {
        let rows = sqlx::query("SELECT slug, title FROM books ORDER BY slug")
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::backend)?;
        rows.iter()
            .map(|r| {
                Ok(BookSummary {
                    slug: r.try_get("slug").map_err(StoreError::backend)?,
                    title: r.try_get("title").map_err(StoreError::backend)?,
                })
            })
            .collect()
    }

    /// One statement: nodes, events and snapshots go with the book row
    /// through `ON DELETE CASCADE`, so the deletion is atomic. The version
    /// condition is in the same statement, so a write committed after the
    /// caller's read makes it delete nothing.
    async fn delete(&self, slug: &str, expected_version: i64) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM books WHERE slug = $1 AND version = $2")
            .bind(slug)
            .bind(expected_version)
            .execute(&self.pool)
            .await
            .map_err(StoreError::backend)?
            .rows_affected();
        if deleted > 0 {
            return Ok(true);
        }
        let actual: Option<i64> = sqlx::query_scalar("SELECT version FROM books WHERE slug = $1")
            .bind(slug)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::backend)?;
        match actual {
            None => Ok(false),
            Some(v) => Err(StoreError::Conflict {
                expected: Some(expected_version),
                actual: Some(v),
            }),
        }
    }
}

#[async_trait]
impl SnapshotStore for PgBookStore {
    async fn create(
        &self,
        slug: &str,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError> {
        let tree = serde_json::to_value(book).map_err(StoreError::backend)?;
        let mut tx = self.pool.begin().await.map_err(StoreError::backend)?;
        // Lock the book row so two snapshots of one book cannot pick the same
        // millisecond.
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT version FROM books WHERE slug = $1 FOR UPDATE")
                .bind(slug)
                .fetch_optional(&mut *tx)
                .await
                .map_err(StoreError::backend)?;
        if exists.is_none() {
            return Err(StoreError::backend(format!("book '{slug}' does not exist")));
        }
        let latest: Option<i64> =
            sqlx::query_scalar("SELECT max(taken_at) FROM snapshots WHERE book = $1")
                .bind(slug)
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::backend)?;
        let now = Timestamp::now().as_millis();
        let taken_at = latest.map_or(now, |l| now.max(l + 1));
        let size: i32 = sqlx::query_scalar(
            "INSERT INTO snapshots (book, taken_at, label, tree) VALUES ($1, $2, $3, $4) \
             RETURNING octet_length(tree::text)",
        )
        .bind(slug)
        .bind(taken_at)
        .bind(label)
        .bind(&tree)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::backend)?;
        tx.commit().await.map_err(StoreError::backend)?;
        Ok(SnapshotInfo {
            timestamp: Timestamp::from_millis(taken_at),
            label: label.map(str::to_string),
            size_bytes: size_u64(size),
        })
    }

    async fn insert(
        &self,
        slug: &str,
        timestamp: Timestamp,
        book: &TemplateBook,
        label: Option<&str>,
    ) -> Result<SnapshotInfo, StoreError> {
        let tree = serde_json::to_value(book).map_err(StoreError::backend)?;
        let size: Option<i32> = sqlx::query_scalar(
            "INSERT INTO snapshots (book, taken_at, label, tree) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (book, taken_at) DO NOTHING RETURNING octet_length(tree::text)",
        )
        .bind(slug)
        .bind(timestamp.as_millis())
        .bind(label)
        .bind(&tree)
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::backend)?;
        let Some(size) = size else {
            return Err(StoreError::Conflict {
                expected: None,
                actual: Some(timestamp.as_millis()),
            });
        };
        Ok(SnapshotInfo {
            timestamp,
            label: label.map(str::to_string),
            size_bytes: size_u64(size),
        })
    }

    async fn list(&self, slug: &str) -> Result<Vec<SnapshotInfo>, StoreError> {
        let rows = sqlx::query(
            "SELECT taken_at, label, octet_length(tree::text) AS size FROM snapshots \
             WHERE book = $1 ORDER BY taken_at DESC",
        )
        .bind(slug)
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::backend)?;
        rows.iter()
            .map(|r| {
                let taken_at: i64 = r.try_get("taken_at").map_err(StoreError::backend)?;
                let size: i32 = r.try_get("size").map_err(StoreError::backend)?;
                Ok(SnapshotInfo {
                    timestamp: Timestamp::from_millis(taken_at),
                    label: r.try_get("label").map_err(StoreError::backend)?,
                    size_bytes: size_u64(size),
                })
            })
            .collect()
    }

    async fn get(
        &self,
        slug: &str,
        timestamp: Timestamp,
    ) -> Result<Option<TemplateBook>, StoreError> {
        let tree: Option<Value> =
            sqlx::query_scalar("SELECT tree FROM snapshots WHERE book = $1 AND taken_at = $2")
                .bind(slug)
                .bind(timestamp.as_millis())
                .fetch_optional(&self.pool)
                .await
                .map_err(StoreError::backend)?;
        tree.map(|t| serde_json::from_value(t).map_err(StoreError::backend))
            .transpose()
    }

    async fn tag(&self, slug: &str, timestamp: Timestamp, label: &str) -> Result<bool, StoreError> {
        let updated =
            sqlx::query("UPDATE snapshots SET label = $3 WHERE book = $1 AND taken_at = $2")
                .bind(slug)
                .bind(timestamp.as_millis())
                .bind(label)
                .execute(&self.pool)
                .await
                .map_err(StoreError::backend)?
                .rows_affected();
        Ok(updated > 0)
    }

    async fn delete(
        &self,
        slug: &str,
        timestamp: Timestamp,
        allow_labeled: bool,
    ) -> Result<SnapshotDelete, StoreError> {
        // The label condition is part of the DELETE, so a label attached
        // between a caller's check and this statement still blocks it.
        let deleted: Option<Option<String>> = sqlx::query_scalar(
            "DELETE FROM snapshots WHERE book = $1 AND taken_at = $2 \
             AND (label IS NULL OR $3) RETURNING label",
        )
        .bind(slug)
        .bind(timestamp.as_millis())
        .bind(allow_labeled)
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::backend)?;
        if let Some(label) = deleted {
            return Ok(SnapshotDelete::Deleted { label });
        }
        let existing: Option<Option<String>> =
            sqlx::query_scalar("SELECT label FROM snapshots WHERE book = $1 AND taken_at = $2")
                .bind(slug)
                .bind(timestamp.as_millis())
                .fetch_optional(&self.pool)
                .await
                .map_err(StoreError::backend)?;
        Ok(match existing {
            None => SnapshotDelete::NotFound,
            Some(Some(label)) => SnapshotDelete::Labeled(label),
            // Unlabeled but not deleted: it appeared after the DELETE ran
            // (a concurrent insert at the same timestamp). Report it as
            // not found for this call; nothing was removed.
            Some(None) => SnapshotDelete::NotFound,
        })
    }

    async fn delete_all(&self, slug: &str) -> Result<u64, StoreError> {
        Ok(sqlx::query("DELETE FROM snapshots WHERE book = $1")
            .bind(slug)
            .execute(&self.pool)
            .await
            .map_err(StoreError::backend)?
            .rows_affected())
    }
}

fn size_u64(size: i32) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// book <-> rows
// ---------------------------------------------------------------------------

/// Every node of `book` as a row. `position` is the index among siblings.
fn rows_of(book: &TemplateBook) -> Result<HashMap<Uuid, NodeRow>, StoreError> {
    let mut positions: HashMap<NodeId, i32> = HashMap::new();
    for (i, id) in book.root_nodes().iter().enumerate() {
        positions.insert(*id, index(i)?);
    }
    let ids: Vec<NodeId> = book.all_node_ids().collect();
    for id in &ids {
        if let Some(node) = book.get_node(*id) {
            for (i, child) in node.children().iter().enumerate() {
                positions.insert(*child, index(i)?);
            }
        }
    }

    let mut rows = HashMap::with_capacity(ids.len());
    for id in ids {
        let Some(node) = book.get_node(id) else {
            continue;
        };
        let properties = serde_json::to_value(node.properties()).map_err(StoreError::backend)?;
        let links = serde_json::to_value(node.links()).map_err(StoreError::backend)?;
        let row = NodeRow {
            id: id.as_uuid(),
            parent: node.parent().map(|p| p.as_uuid()),
            position: positions.get(&id).copied().unwrap_or(0),
            title: node.title().to_string(),
            body: node.body().map(str::to_string),
            node_type: enum_str(node.node_type())?,
            placeholder: node.placeholder().map(str::to_string),
            properties,
            status: enum_str(&node.status())?,
            updated_at: node.updated_at().map(|t| t.as_millis()),
            verified_at: node.verified_at().map(|t| t.as_millis()),
            verified_by: node.verified_by().map(str::to_string),
            links,
        };
        rows.insert(row.id, row);
    }
    Ok(rows)
}

/// Rebuild a book from its rows: children and root order come from
/// `(parent, position)`.
fn assemble(
    id: BookId,
    title: String,
    max_depth: u8,
    rows: HashMap<Uuid, NodeRow>,
) -> Result<TemplateBook, StoreError> {
    let mut by_parent: HashMap<Option<Uuid>, Vec<(i32, Uuid)>> = HashMap::new();
    for row in rows.values() {
        by_parent
            .entry(row.parent)
            .or_default()
            .push((row.position, row.id));
    }
    for siblings in by_parent.values_mut() {
        siblings.sort();
    }
    let ordered = |parent: Option<Uuid>| -> Vec<NodeId> {
        by_parent
            .get(&parent)
            .map(|s| s.iter().map(|(_, id)| NodeId::from_uuid(*id)).collect())
            .unwrap_or_default()
    };

    let mut nodes = HashMap::with_capacity(rows.len());
    for row in rows.values() {
        let properties: HashMap<String, String> =
            serde_json::from_value(row.properties.clone()).map_err(StoreError::backend)?;
        let links: Vec<Link> =
            serde_json::from_value(row.links.clone()).map_err(StoreError::backend)?;
        let node = TemplateNode::from_parts(NodeParts {
            id: NodeId::from_uuid(row.id),
            parent: row.parent.map(NodeId::from_uuid),
            children: ordered(Some(row.id)),
            title: row.title.clone(),
            body: row.body.clone(),
            node_type: str_enum(&row.node_type)?,
            placeholder: row.placeholder.clone(),
            properties,
            status: str_enum(&row.status)?,
            updated_at: row.updated_at.map(Timestamp::from_millis),
            verified_at: row.verified_at.map(Timestamp::from_millis),
            verified_by: row.verified_by.clone(),
            links,
        });
        nodes.insert(node.id(), node);
    }
    Ok(TemplateBook::from_parts(
        id,
        title,
        max_depth,
        nodes,
        ordered(None),
    ))
}

fn node_row(r: &PgRow) -> Result<NodeRow, StoreError> {
    Ok(NodeRow {
        id: r.try_get("id").map_err(StoreError::backend)?,
        parent: r.try_get("parent").map_err(StoreError::backend)?,
        position: r.try_get("position").map_err(StoreError::backend)?,
        title: r.try_get("title").map_err(StoreError::backend)?,
        body: r.try_get("body").map_err(StoreError::backend)?,
        node_type: r.try_get("node_type").map_err(StoreError::backend)?,
        placeholder: r.try_get("placeholder").map_err(StoreError::backend)?,
        properties: r.try_get("properties").map_err(StoreError::backend)?,
        status: r.try_get("status").map_err(StoreError::backend)?,
        updated_at: r.try_get("updated_at").map_err(StoreError::backend)?,
        verified_at: r.try_get("verified_at").map_err(StoreError::backend)?,
        verified_by: r.try_get("verified_by").map_err(StoreError::backend)?,
        links: r.try_get("links").map_err(StoreError::backend)?,
    })
}

fn change_entry(r: &PgRow) -> Result<ChangeEntry, StoreError> {
    let node_id: Option<Uuid> = r.try_get("node_id").map_err(StoreError::backend)?;
    let action: String = r.try_get("action").map_err(StoreError::backend)?;
    let before: Option<Value> = r.try_get("before").map_err(StoreError::backend)?;
    let after: Option<Value> = r.try_get("after").map_err(StoreError::backend)?;
    let at: i64 = r.try_get("at").map_err(StoreError::backend)?;
    Ok(ChangeEntry::new(
        // NULL is a book-level entry (`book_update`): the nil id.
        node_id.map_or(NodeId::book_level(), NodeId::from_uuid),
        str_enum(&action)?,
        before.map(|v| v.to_string()),
        after.map(|v| v.to_string()),
        Timestamp::from_millis(at),
    ))
}

/// A `ChangeEntry` side (JSON text) as a `jsonb` value.
fn json_column(s: &Option<String>) -> Result<Option<Value>, StoreError> {
    s.as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(StoreError::backend)
}

/// A unit enum as its serde name (`NodeType::Section` → `"Section"`).
fn enum_str<T: Serialize>(v: &T) -> Result<String, StoreError> {
    match serde_json::to_value(v).map_err(StoreError::backend)? {
        Value::String(s) => Ok(s),
        other => Err(StoreError::backend(format!(
            "expected a unit enum, serialized as {other}"
        ))),
    }
}

/// The inverse of [`enum_str`].
fn str_enum<T: DeserializeOwned>(s: &str) -> Result<T, StoreError> {
    serde_json::from_value(Value::String(s.to_string())).map_err(StoreError::backend)
}

fn index(i: usize) -> Result<i32, StoreError> {
    i32::try_from(i).map_err(StoreError::backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::AddNodeRequest;
    use crate::domain::model::node::NodeType;

    fn add(book: &mut TemplateBook, parent: Option<NodeId>, title: &str) -> NodeId {
        book.add_node(AddNodeRequest {
            parent,
            title: title.to_string(),
            node_type: NodeType::Section,
            body: Some(format!("{title} body")),
            placeholder: None,
            position: usize::MAX,
            properties: [("k".to_string(), title.to_string())].into(),
        })
        .unwrap()
    }

    #[test]
    fn rows_round_trip_preserves_order_and_fields() {
        let mut book = TemplateBook::new("B", 4);
        let a = add(&mut book, None, "a");
        let b = add(&mut book, None, "b");
        let a1 = add(&mut book, Some(a), "a1");
        let a2 = add(&mut book, Some(a), "a2");
        book.move_node(b, None, 0).unwrap();

        let rows = rows_of(&book).unwrap();
        let back = assemble(book.id(), book.title().to_string(), book.max_depth(), rows).unwrap();

        assert_eq!(back.root_nodes(), &[b, a]);
        assert_eq!(back.get_node(a).unwrap().children(), &[a1, a2]);
        assert_eq!(
            serde_json::to_value(back.get_node(a1).unwrap()).unwrap(),
            serde_json::to_value(book.get_node(a1).unwrap()).unwrap()
        );
        assert_eq!(back.node_count(), 4);
    }

    #[test]
    fn unchanged_rows_compare_equal_so_commit_skips_them() {
        let mut book = TemplateBook::new("B", 4);
        let a = add(&mut book, None, "a");
        let before = rows_of(&book).unwrap();
        let b = add(&mut book, None, "b");
        let after = rows_of(&book).unwrap();
        assert_eq!(before.get(&a.as_uuid()), after.get(&a.as_uuid()));
        assert!(!before.contains_key(&b.as_uuid()));
    }

    #[test]
    fn chunk_ranges_split_by_rows_and_bytes() {
        assert!(chunk_ranges::<u8>(&[], |_| 1).is_empty());

        let rows = vec![0u8; CHUNK_ROWS * 2 + 1];
        let ranges = chunk_ranges(&rows, |_| 1);
        assert_eq!(
            ranges,
            vec![
                0..CHUNK_ROWS,
                CHUNK_ROWS..CHUNK_ROWS * 2,
                CHUNK_ROWS * 2..CHUNK_ROWS * 2 + 1
            ]
        );

        // Three items of 0.4 MB: two fit under CHUNK_BYTES, the third starts a new chunk.
        let big = vec![CHUNK_BYTES * 2 / 5; 3];
        assert_eq!(chunk_ranges(&big, |s| *s), vec![0..2, 2..3]);

        // An item over the limit stands alone rather than being dropped.
        let huge = vec![1, CHUNK_BYTES * 3, 1];
        assert_eq!(chunk_ranges(&huge, |s| *s), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn enum_names_round_trip() {
        assert_eq!(enum_str(&NodeType::Section).unwrap(), "Section");
        let t: NodeType = str_enum("Content").unwrap();
        assert_eq!(t, NodeType::Content);
    }
}
