//! [`HistoryPreservingChangeLogRepository`]: bridges the legacy
//! `{slug}.changelog.json` format to the ai-store-backed one without a
//! dedicated migration step.
//!
//! # Architecture
//!
//! Unlike the snapshot subsystem (`crate::infra::snapshot_migrator`), the
//! per-node changelog has no dedicated `migrate-changelog` CLI command.
//! `ChangeEntry` history is advisory (an audit trail read via
//! `node_history` / `book_history`), not the source of truth for a book's
//! current tree — that role belongs to `crate::infra::json_store::JsonBookRepository`,
//! which this repository swap never touches. That lower stake makes a
//! lazy, read-time merge a sufficient bridge instead of a batch import:
//!
//! - **Writes** always go to [`AiStoreChangeLogRepository`] only — the
//!   legacy `{slug}.changelog.json` file is frozen at the moment a
//!   deployment first constructs this type, exactly as it was left by the
//!   prior (JSON-only) version.
//! - **Reads** (`load_all` / `load_by_node`) concatenate the frozen legacy
//!   entries first, then the ai-store entries. This ordering is correct
//!   because every legacy entry necessarily predates every ai-store entry —
//!   the cutover moment is "now" (when this deployment first runs), and
//!   nothing appends to the JSON file after that. A fresh book (no
//!   pre-existing `{slug}.changelog.json`) reads as pure ai-store history,
//!   since [`JsonChangeLogRepository::load_all`] returns an empty `Vec` for
//!   a missing file.
//!
//! If a `migrate-changelog` command is ever added (importing legacy entries
//! into the ai-store stream itself, mirroring
//! `crate::infra::snapshot_migrator::migrate_snapshots`), this bridge can be
//! dropped in favor of [`AiStoreChangeLogRepository`] alone — nothing about
//! [`ChangeLogRepository`]'s trait shape depends on it existing.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use ai_store_core::Store;

use crate::domain::model::changelog::ChangeEntry;
use crate::domain::model::id::NodeId;
use crate::domain::repository::ChangeLogRepository;
use crate::infra::ai_store_changelog::AiStoreChangeLogRepository;
use crate::infra::changelog_store::JsonChangeLogRepository;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// [`ChangeLogRepository`] that writes new entries to
/// [`AiStoreChangeLogRepository`] while still surfacing history recorded by
/// a prior version's [`JsonChangeLogRepository`]. See the module docs for
/// why a read-time merge is sufficient here (unlike the snapshot
/// subsystem's dedicated migrator).
pub struct HistoryPreservingChangeLogRepository {
    ai_store: AiStoreChangeLogRepository,
    legacy: JsonChangeLogRepository,
}

impl HistoryPreservingChangeLogRepository {
    /// Constructs a repository for `slug`, backed by `store`'s dedicated
    /// per-node changelog stream (see
    /// [`AiStoreChangeLogRepository::new`]) with a read-only fallback onto
    /// `{shelf_dir}/{slug}.changelog.json`.
    pub fn new(
        store: Arc<Store>,
        shelf_dir: impl Into<PathBuf>,
        slug: impl Into<String>,
    ) -> Result<Self, BoxError> {
        let shelf_dir = shelf_dir.into();
        let slug = slug.into();
        Ok(Self {
            ai_store: AiStoreChangeLogRepository::new(store, slug.clone())?,
            legacy: JsonChangeLogRepository::new(shelf_dir, slug),
        })
    }
}

#[async_trait]
impl ChangeLogRepository for HistoryPreservingChangeLogRepository {
    async fn append(&self, entry: &ChangeEntry) -> Result<(), BoxError> {
        // New writes land in ai-store only — the legacy file is frozen at
        // cutover (see module docs).
        self.ai_store.append(entry).await
    }

    async fn load_all(&self) -> Result<Vec<ChangeEntry>, BoxError> {
        let mut entries = self.legacy.load_all().await?;
        entries.extend(self.ai_store.load_all().await?);
        Ok(entries)
    }

    async fn load_by_node(&self, node_id: NodeId) -> Result<Vec<ChangeEntry>, BoxError> {
        let mut entries = self.legacy.load_by_node(node_id).await?;
        entries.extend(self.ai_store.load_by_node(node_id).await?);
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::changelog::ChangeAction;
    use crate::domain::model::timestamp::Timestamp;
    use ai_store_core::{CacheBackend, EventBackend, StoreConfig};
    use ai_store_mem::{MemCacheBackend, MemEventBackend};

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("outline-mcp-changelog-bridge-test-{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_store() -> Arc<Store> {
        let events: Arc<dyn EventBackend> = Arc::new(MemEventBackend::new());
        let cache: Arc<dyn CacheBackend> = Arc::new(MemCacheBackend::new());
        Arc::new(Store::new(
            events,
            cache,
            Vec::new(),
            Vec::new(),
            StoreConfig::default(),
        ))
    }

    fn make_entry(node_id: NodeId, action: ChangeAction, millis: i64) -> ChangeEntry {
        ChangeEntry::new(node_id, action, None, None, Timestamp::from_millis(millis))
    }

    #[tokio::test]
    async fn test_fresh_book_no_legacy_file_reads_pure_ai_store() {
        let dir = temp_dir("fresh");
        let repo =
            HistoryPreservingChangeLogRepository::new(make_store(), &dir, "fresh-book").unwrap();

        let id = NodeId::new();
        repo.append(&make_entry(id, ChangeAction::Create, 1_000))
            .await
            .expect("append");

        let all = repo.load_all().await.expect("load_all");
        assert_eq!(all.len(), 1, "no legacy file: pure ai-store history");
        assert_eq!(all[0].node_id, id);
        assert_eq!(all[0].action, ChangeAction::Create);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_load_all_merges_legacy_then_ai_store_in_order() {
        let dir = temp_dir("merge-all");
        let slug = "merge-book";

        // Simulate a prior JSON-only deployment's history frozen on disk.
        let legacy_only = JsonChangeLogRepository::new(&dir, slug);
        let legacy_id = NodeId::new();
        outline_mcp_core_test_append(&legacy_only, legacy_id, ChangeAction::Create, 1_000).await;

        let repo = HistoryPreservingChangeLogRepository::new(make_store(), &dir, slug).unwrap();
        let fresh_id = NodeId::new();
        repo.append(&make_entry(fresh_id, ChangeAction::Update, 2_000))
            .await
            .expect("append to ai-store");

        let all = repo.load_all().await.expect("load_all");
        assert_eq!(all.len(), 2, "legacy entry + new ai-store entry");
        assert_eq!(all[0].node_id, legacy_id, "legacy entries come first");
        assert_eq!(all[0].action, ChangeAction::Create);
        assert_eq!(all[1].node_id, fresh_id, "ai-store entries come after");
        assert_eq!(all[1].action, ChangeAction::Update);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_load_by_node_merges_both_sources_filtered() {
        let dir = temp_dir("merge-by-node");
        let slug = "filter-book";
        let target = NodeId::new();
        let other = NodeId::new();

        let legacy_only = JsonChangeLogRepository::new(&dir, slug);
        outline_mcp_core_test_append(&legacy_only, target, ChangeAction::Create, 1_000).await;
        outline_mcp_core_test_append(&legacy_only, other, ChangeAction::Create, 1_500).await;

        let repo = HistoryPreservingChangeLogRepository::new(make_store(), &dir, slug).unwrap();
        repo.append(&make_entry(target, ChangeAction::Update, 2_000))
            .await
            .expect("append target update");
        repo.append(&make_entry(other, ChangeAction::Update, 2_500))
            .await
            .expect("append other update");

        let filtered = repo.load_by_node(target).await.expect("load_by_node");
        assert_eq!(
            filtered.len(),
            2,
            "one legacy + one ai-store entry for target"
        );
        assert!(filtered.iter().all(|e| e.node_id == target));
        assert_eq!(filtered[0].action, ChangeAction::Create, "legacy first");
        assert_eq!(filtered[1].action, ChangeAction::Update, "ai-store second");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_append_does_not_touch_legacy_file() {
        let dir = temp_dir("append-frozen");
        let slug = "frozen-book";

        let legacy_only = JsonChangeLogRepository::new(&dir, slug);
        outline_mcp_core_test_append(&legacy_only, NodeId::new(), ChangeAction::Create, 1_000)
            .await;
        let legacy_path = dir.join(format!("{slug}.changelog.json"));
        let before = std::fs::read_to_string(&legacy_path).expect("read legacy file");

        let repo = HistoryPreservingChangeLogRepository::new(make_store(), &dir, slug).unwrap();
        repo.append(&make_entry(NodeId::new(), ChangeAction::Update, 2_000))
            .await
            .expect("append");

        let after = std::fs::read_to_string(&legacy_path).expect("re-read legacy file");
        assert_eq!(before, after, "legacy changelog file must be untouched");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-only helper: appends directly through [`ChangeLogRepository`]'s
    /// trait method (disambiguating from any same-named inherent method).
    async fn outline_mcp_core_test_append(
        repo: &JsonChangeLogRepository,
        node_id: NodeId,
        action: ChangeAction,
        millis: i64,
    ) {
        ChangeLogRepository::append(repo, &make_entry(node_id, action, millis))
            .await
            .expect("append to legacy repo");
    }
}
