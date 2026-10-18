//! [`LegacyShelfStore`]: the pre-`BookStore` storage — one book file plus a
//! separate changelog — behind the [`BookStore`] port.
//!
//! # Not atomic, by inheritance
//!
//! This adapter keeps exactly what the server did before the port existed:
//! it saves the book first, then appends each event to the changelog, and a
//! changelog failure becomes a [`Committed::warnings`] entry
//! (`changelog: <error>`) instead of failing the commit. The book is then
//! stored while its event is not — the defect the port exists to remove. It
//! stays only so the running server keeps its behavior until books move to
//! an atomic store; do not build new features on it.
//!
//! # No optimistic locking
//!
//! The book file carries no version. [`BookStore::load`] reports version 0,
//! and [`BookStore::commit`] ignores `expected_version`, so concurrent
//! writers to one book can still overwrite each other here.
//!
//! # One book per instance
//!
//! An instance wraps one [`BookRepository`] (one book file) and ignores the
//! `slug` argument; the caller constructs one per slug, as the server does.

use async_trait::async_trait;

use crate::domain::model::book::TemplateBook;
use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::NodeId;
use crate::domain::repository::{BookRepository, ChangeLogRepository};
use crate::domain::store::{BookStore, BookSummary, Committed, StoreError, Versioned};

/// [`BookStore`] over a [`BookRepository`] and an optional
/// [`ChangeLogRepository`], with the non-atomic semantics described in the
/// module docs.
pub struct LegacyShelfStore<R> {
    repo: R,
    changelog: Option<Box<dyn ChangeLogRepository>>,
}

impl<R> LegacyShelfStore<R> {
    /// Wrap a book repository with no changelog: events are dropped.
    pub fn new(repo: R) -> Self {
        Self {
            repo,
            changelog: None,
        }
    }

    /// Record events to `changelog` (builder).
    pub fn with_changelog(mut self, changelog: Box<dyn ChangeLogRepository>) -> Self {
        self.changelog = Some(changelog);
        self
    }
}

#[async_trait]
impl<R> BookStore for LegacyShelfStore<R>
where
    R: BookRepository + Send + Sync,
{
    async fn load(&self, _slug: &str) -> Result<Option<Versioned<TemplateBook>>, StoreError> {
        let book = self.repo.load().await.map_err(StoreError::backend)?;
        Ok(book.map(|value| Versioned { value, version: 0 }))
    }

    async fn commit(
        &self,
        _slug: &str,
        _expected_version: Option<i64>,
        book: &TemplateBook,
        events: &[ChangeEntry],
    ) -> Result<Committed, StoreError> {
        self.repo.save(book).await.map_err(StoreError::backend)?;
        let mut warnings = Vec::new();
        if let Some(cl) = &self.changelog {
            for entry in events {
                if let Err(e) = cl.append(entry).await {
                    warnings.push(format!("changelog: {e}"));
                }
            }
        }
        Ok(Committed {
            version: 0,
            warnings,
        })
    }

    async fn history(&self, _slug: &str) -> Result<Vec<ChangeEntry>, StoreError> {
        match &self.changelog {
            Some(cl) => cl.load_all().await.map_err(StoreError::Backend),
            None => Ok(Vec::new()),
        }
    }

    async fn history_by_node(
        &self,
        _slug: &str,
        node_id: NodeId,
    ) -> Result<Vec<ChangeEntry>, StoreError> {
        match &self.changelog {
            Some(cl) => cl.load_by_node(node_id).await.map_err(StoreError::Backend),
            None => Ok(Vec::new()),
        }
    }

    async fn list_books(&self) -> Result<Vec<BookSummary>, StoreError> {
        // One book per instance and no slug of its own: the shelf directory
        // listing (`list_book_slugs` in the server) stays the source of slugs.
        Ok(Vec::new())
    }

    async fn delete(&self, _slug: &str) -> Result<bool, StoreError> {
        // Only `import-shelf` reads through this adapter; nothing deletes a
        // pre-Postgres shelf through it.
        Err(StoreError::backend(
            "deleting a book is not supported on the legacy shelf adapter",
        ))
    }
}
