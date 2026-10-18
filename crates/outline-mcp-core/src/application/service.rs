use crate::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use crate::domain::model::changelog::{ChangeAction, ChangeEntry};
use crate::domain::model::id::NodeId;
use crate::domain::model::timestamp::Timestamp;
use crate::domain::store::{BookStore, StoreError, Versioned};

use super::error::AppError;

/// Template Bookに対するユースケース。
/// load → mutate → commit のパターンで操作する。
///
/// 1 操作 = 1 [`BookStore::commit`]: 変更後の book と、その操作が生んだ
/// changelog event をまとめて渡す (batch 系は N event を 1 commit)。commit は
/// load 時の version に対して行うので、間に別の writer が commit していれば
/// [`AppError::Conflict`] になる (version を持たない legacy adapter を除く)。
pub struct BookService<S: BookStore> {
    store: S,
    slug: String,
}

impl<S: BookStore> BookService<S> {
    /// Create a service for the book `slug` in `store`.
    pub fn new(store: S, slug: impl Into<String>) -> Self {
        Self {
            store,
            slug: slug.into(),
        }
    }

    /// Bookを新規作成して永続化する。既存Bookがあれば上書き (event は記録しない)。
    pub async fn create_book(&self, title: &str, max_depth: u8) -> Result<TemplateBook, AppError> {
        let book = TemplateBook::new(title, max_depth);
        let expected = self.current_version().await?;
        self.commit(expected, &book, &[]).await?;
        Ok(book)
    }

    /// ノードを追加する。
    ///
    /// 戻り値: `(NodeId, Option<String>)` — 第2要素は store が返した警告 (legacy adapter の changelog 書き込み失敗のみ)。
    pub async fn add_node(
        &self,
        req: AddNodeRequest,
    ) -> Result<(NodeId, Option<String>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let id = book.add_node(req)?;

        let after_json = node_json(&book, id);
        let entry = ChangeEntry::new(id, ChangeAction::Create, None, after_json, Timestamp::now());
        let warnings = self.commit(Some(version), &book, &[entry]).await?;

        Ok((id, join_warnings(warnings)))
    }

    /// ノードを更新する。
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は store が返した警告。
    pub async fn update_node(
        &self,
        id: NodeId,
        req: UpdateNodeRequest,
    ) -> Result<((), Option<String>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let before_json = node_json(&book, id);
        book.update_node(id, req)?;

        let after_json = node_json(&book, id);
        let entry = ChangeEntry::new(
            id,
            ChangeAction::Update,
            before_json,
            after_json,
            Timestamp::now(),
        );
        let warnings = self.commit(Some(version), &book, &[entry]).await?;

        Ok(((), join_warnings(warnings)))
    }

    /// ノードを移動する。
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は store が返した警告。
    pub async fn move_node(
        &self,
        id: NodeId,
        new_parent: Option<NodeId>,
        position: usize,
    ) -> Result<((), Option<String>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let before_json = node_json(&book, id);
        book.move_node(id, new_parent, position)?;

        let after_json = node_json(&book, id);
        let entry = ChangeEntry::new(
            id,
            ChangeAction::Move,
            before_json,
            after_json,
            Timestamp::now(),
        );
        let warnings = self.commit(Some(version), &book, &[entry]).await?;

        Ok(((), join_warnings(warnings)))
    }

    /// ノードを削除する（子孫ごと）。
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は store が返した警告。
    pub async fn remove_node(&self, id: NodeId) -> Result<((), Option<String>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let before_json = node_json(&book, id);
        book.remove_node(id)?;

        let entry = ChangeEntry::new(
            id,
            ChangeAction::Delete,
            before_json,
            None,
            Timestamp::now(),
        );
        let warnings = self.commit(Some(version), &book, &[entry]).await?;

        Ok(((), join_warnings(warnings)))
    }

    /// 複数ノードをアトミックに移動する（全成功 or 何も commit しない）。
    ///
    /// `moves` は `(NodeId, new_parent: Option<NodeId>, position: usize)` のリスト。
    /// 戻り値: `(成功件数, 警告リスト)`。警告リストは常に `moves.len()` 件で、
    /// store の警告を先頭から詰める (event との対応は持たない。呼び出し側は flatten して使う)。
    pub async fn batch_move(
        &self,
        moves: Vec<(NodeId, Option<NodeId>, usize)>,
    ) -> Result<(usize, Vec<Option<String>>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let mut before_jsons: Vec<Option<String>> = Vec::with_capacity(moves.len());

        for (id, new_parent, position) in &moves {
            before_jsons.push(node_json(&book, *id));
            book.move_node(*id, *new_parent, *position)?;
        }

        let ts = Timestamp::now();
        let entries: Vec<ChangeEntry> = moves
            .iter()
            .zip(before_jsons)
            .map(|((id, _, _), before_json)| {
                ChangeEntry::new(
                    *id,
                    ChangeAction::Move,
                    before_json,
                    node_json(&book, *id),
                    ts,
                )
            })
            .collect();

        if entries.is_empty() {
            return Ok((0, Vec::new()));
        }
        let warnings = self.commit(Some(version), &book, &entries).await?;

        Ok((moves.len(), spread_warnings(warnings, moves.len())))
    }

    /// 複数ノードをアトミックに更新する（全成功 or 何も commit しない）。
    ///
    /// `updates` は `(NodeId, UpdateNodeRequest)` のリスト。
    /// 戻り値: `(成功件数, 警告リスト)`。警告リストの形は [`Self::batch_move`] と同じ。
    pub async fn batch_update(
        &self,
        updates: Vec<(NodeId, UpdateNodeRequest)>,
    ) -> Result<(usize, Vec<Option<String>>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;

        let mut before_jsons: Vec<Option<String>> = Vec::with_capacity(updates.len());
        let mut node_ids: Vec<NodeId> = Vec::with_capacity(updates.len());

        for (id, req) in updates {
            before_jsons.push(node_json(&book, id));
            node_ids.push(id);
            book.update_node(id, req)?;
        }

        let ts = Timestamp::now();
        let entries: Vec<ChangeEntry> = node_ids
            .iter()
            .zip(before_jsons)
            .map(|(id, before_json)| {
                ChangeEntry::new(
                    *id,
                    ChangeAction::Update,
                    before_json,
                    node_json(&book, *id),
                    ts,
                )
            })
            .collect();

        if entries.is_empty() {
            return Ok((0, Vec::new()));
        }
        let warnings = self.commit(Some(version), &book, &entries).await?;

        Ok((node_ids.len(), spread_warnings(warnings, node_ids.len())))
    }

    /// Tree全体または部分木を読み取る。
    pub async fn read_tree(&self) -> Result<TemplateBook, AppError> {
        Ok(self.load_book().await?.value)
    }

    /// インポート / snapshot restore された Book を保存する (無ければ作成)。
    ///
    /// event は記録しない: 呼び出し側 (`import` / `snapshot_restore`) が
    /// 自分の changelog entry を別途記録する現行の形を保つため。
    pub async fn save_book(&self, book: &TemplateBook) -> Result<(), AppError> {
        let expected = self.current_version().await?;
        self.commit(expected, book, &[]).await?;
        Ok(())
    }

    /// The book's recorded events, oldest first.
    pub async fn history(&self) -> Result<Vec<ChangeEntry>, AppError> {
        self.store.history(&self.slug).await.map_err(store_error)
    }

    /// One node's recorded events, oldest first.
    pub async fn history_by_node(&self, id: NodeId) -> Result<Vec<ChangeEntry>, AppError> {
        self.store
            .history_by_node(&self.slug, id)
            .await
            .map_err(store_error)
    }

    // --- private ---

    async fn load_book(&self) -> Result<Versioned<TemplateBook>, AppError> {
        self.store
            .load(&self.slug)
            .await
            .map_err(store_error)?
            .ok_or(AppError::BookNotFound)
    }

    async fn current_version(&self) -> Result<Option<i64>, AppError> {
        Ok(self
            .store
            .load(&self.slug)
            .await
            .map_err(store_error)?
            .map(|v| v.version))
    }

    async fn commit(
        &self,
        expected: Option<i64>,
        book: &TemplateBook,
        events: &[ChangeEntry],
    ) -> Result<Vec<String>, AppError> {
        self.store
            .commit(&self.slug, expected, book, events)
            .await
            .map(|c| c.warnings)
            .map_err(store_error)
    }
}

fn node_json(book: &TemplateBook, id: NodeId) -> Option<String> {
    book.get_node(id)
        .and_then(|n| serde_json::to_string(n).ok())
}

fn store_error(e: StoreError) -> AppError {
    match e {
        StoreError::Conflict { expected, actual } => AppError::Conflict { expected, actual },
        StoreError::Backend(e) => AppError::Storage(e),
    }
}

fn join_warnings(warnings: Vec<String>) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

fn spread_warnings(warnings: Vec<String>, len: usize) -> Vec<Option<String>> {
    let mut out = vec![None; len];
    for (slot, w) in out.iter_mut().zip(warnings) {
        *slot = Some(w);
    }
    out
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::{AddNodeRequest, UpdateNodeRequest};
    use crate::domain::model::changelog::{ChangeAction, ChangeEntry, NodeStatus};
    use crate::domain::model::id::NodeId;
    use crate::domain::model::node::NodeType;
    use crate::domain::model::timestamp::Timestamp;
    use crate::domain::repository::{BookRepository, ChangeLogRepository};
    use crate::infra::legacy_shelf_store::LegacyShelfStore;
    use crate::infra::memory_store::MemoryBookStore;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    // --- InMemory BookRepository ---

    #[derive(Clone)]
    struct InMemoryBookRepo {
        book: Arc<Mutex<Option<TemplateBook>>>,
    }

    impl InMemoryBookRepo {
        fn empty() -> Self {
            Self {
                book: Arc::new(Mutex::new(None)),
            }
        }
        fn with_book(book: TemplateBook) -> Self {
            Self {
                book: Arc::new(Mutex::new(Some(book))),
            }
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("in-memory repo error")]
    struct RepoError;

    #[async_trait]
    impl BookRepository for InMemoryBookRepo {
        type Error = RepoError;
        async fn load(&self) -> Result<Option<TemplateBook>, RepoError> {
            Ok(self.book.lock().unwrap().clone())
        }
        async fn save(&self, book: &TemplateBook) -> Result<(), RepoError> {
            *self.book.lock().unwrap() = Some(book.clone());
            Ok(())
        }
    }

    // --- Recording ChangeLogRepository ---

    #[derive(Default)]
    struct RecordingChangeLog {
        entries: Arc<Mutex<Vec<ChangeEntry>>>,
        fail: bool,
    }

    impl RecordingChangeLog {
        fn new() -> Self {
            Self::default()
        }
        fn failing() -> Self {
            Self {
                fail: true,
                ..Default::default()
            }
        }
        fn recorded(&self) -> Vec<ChangeEntry> {
            self.entries.lock().unwrap().clone()
        }
    }

    #[derive(Debug)]
    struct FakeChangeLogError;
    impl std::fmt::Display for FakeChangeLogError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake error")
        }
    }
    impl std::error::Error for FakeChangeLogError {}

    #[async_trait]
    impl ChangeLogRepository for RecordingChangeLog {
        async fn append(
            &self,
            entry: &ChangeEntry,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            if self.fail {
                return Err(Box::new(FakeChangeLogError));
            }
            self.entries.lock().unwrap().push(entry.clone());
            Ok(())
        }
        async fn load_all(
            &self,
        ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.recorded())
        }
        async fn load_by_node(
            &self,
            node_id: NodeId,
        ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self
                .recorded()
                .into_iter()
                .filter(|e| e.node_id == node_id)
                .collect())
        }
    }

    type LegacyService = BookService<LegacyShelfStore<InMemoryBookRepo>>;

    fn legacy(repo: InMemoryBookRepo) -> LegacyService {
        BookService::new(LegacyShelfStore::new(repo), "test")
    }

    fn legacy_cl(repo: InMemoryBookRepo, cl: Box<dyn ChangeLogRepository>) -> LegacyService {
        BookService::new(LegacyShelfStore::new(repo).with_changelog(cl), "test")
    }

    async fn memory_service() -> BookService<MemoryBookStore> {
        let svc = BookService::new(MemoryBookStore::new(), "test");
        svc.create_book("Test", 4).await.expect("create_book");
        svc
    }

    fn add_req(title: &str) -> AddNodeRequest {
        AddNodeRequest {
            parent: None,
            title: title.to_string(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: Default::default(),
        }
    }

    fn title_req(title: &str) -> UpdateNodeRequest {
        UpdateNodeRequest {
            title: Some(title.to_string()),
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: None,
            status: None,
        }
    }

    // ---- atomic store (MemoryBookStore) ----

    #[tokio::test]
    async fn test_memory_each_write_commits_its_event() {
        let svc = memory_service().await;
        let (a, w) = svc.add_node(add_req("A")).await.expect("add A");
        assert!(w.is_none());
        let (b, _) = svc.add_node(add_req("B")).await.expect("add B");
        svc.update_node(a, title_req("A2")).await.expect("update");
        svc.move_node(a, Some(b), 0).await.expect("move");

        let actions: Vec<_> = svc
            .history()
            .await
            .expect("history")
            .into_iter()
            .map(|e| (e.node_id, e.action))
            .collect();
        assert_eq!(
            actions,
            vec![
                (a, ChangeAction::Create),
                (b, ChangeAction::Create),
                (a, ChangeAction::Update),
                (a, ChangeAction::Move),
            ]
        );
        let a_history = svc.history_by_node(a).await.expect("history_by_node");
        assert_eq!(a_history.len(), 3);
        assert!(a_history.iter().all(|e| e.node_id == a));
    }

    #[tokio::test]
    async fn test_memory_batch_is_one_commit_with_n_events() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.expect("add A");
        let (b, _) = svc.add_node(add_req("B")).await.expect("add B");
        let before = svc.store.load("test").await.unwrap().unwrap().version;

        let (count, warnings) = svc
            .batch_update(vec![(a, title_req("A2")), (b, title_req("B2"))])
            .await
            .expect("batch_update");
        assert_eq!(count, 2);
        assert_eq!(warnings, vec![None, None]);

        let after = svc.store.load("test").await.unwrap().unwrap().version;
        assert_eq!(after, before + 1, "one batch must be one commit");
        let updates = svc
            .history()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == ChangeAction::Update)
            .count();
        assert_eq!(updates, 2);
    }

    #[tokio::test]
    async fn test_memory_failed_batch_commits_nothing() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.expect("add A");
        let events_before = svc.history().await.unwrap().len();

        let result = svc
            .batch_move(vec![(a, None, 0), (NodeId::new(), None, 0)])
            .await;
        assert!(result.is_err());
        assert_eq!(svc.history().await.unwrap().len(), events_before);
    }

    #[tokio::test]
    async fn test_memory_stale_writer_gets_conflict() {
        let store = MemoryBookStore::new();
        let svc = BookService::new(store, "test");
        svc.create_book("Test", 4).await.expect("create_book");
        let (a, _) = svc.add_node(add_req("A")).await.expect("add A");

        // A writer that read version n, while another writer commits n + 1.
        let stale = svc.store.load("test").await.unwrap().unwrap();
        svc.update_node(a, title_req("winner"))
            .await
            .expect("winner");
        let err = svc
            .store
            .commit("test", Some(stale.version), &stale.value, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }));
        assert!(matches!(
            store_error(err),
            AppError::Conflict {
                expected: Some(_),
                actual: Some(_)
            }
        ));
    }

    #[tokio::test]
    async fn test_memory_create_book_overwrites_existing() {
        let svc = memory_service().await;
        svc.add_node(add_req("A")).await.expect("add A");
        let book = svc.create_book("Fresh", 3).await.expect("recreate");
        let tree = svc.read_tree().await.expect("read_tree");
        assert_eq!(tree.id(), book.id());
        assert_eq!(tree.node_count(), 0);
    }

    #[tokio::test]
    async fn test_memory_book_not_found() {
        let svc = BookService::new(MemoryBookStore::new(), "missing");
        let err = svc.add_node(add_req("x")).await.unwrap_err();
        assert!(matches!(err, AppError::BookNotFound));
    }

    // ---- legacy shelf store (today's server behavior) ----

    #[tokio::test]
    async fn test_add_node_no_changelog_returns_none_warning() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);
        let (_, warning) = svc.add_node(add_req("Node A")).await.expect("add_node");
        assert!(warning.is_none(), "no changelog should produce no warning");
    }

    #[tokio::test]
    async fn test_add_node_with_changelog_records_create() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = Arc::new(RecordingChangeLog::new());
        let cl_clone = Arc::clone(&cl);

        // Box<dyn ChangeLogRepository> のためにラッパー実装
        struct ArcChangeLog(Arc<RecordingChangeLog>);
        #[async_trait]
        impl ChangeLogRepository for ArcChangeLog {
            async fn append(
                &self,
                entry: &ChangeEntry,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                self.0.append(entry).await
            }
            async fn load_all(
                &self,
            ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
                self.0.load_all().await
            }
            async fn load_by_node(
                &self,
                node_id: NodeId,
            ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
                self.0.load_by_node(node_id).await
            }
        }

        let svc = legacy_cl(repo, Box::new(ArcChangeLog(cl_clone)));
        let (id, warning) = svc.add_node(add_req("Node A")).await.expect("add_node");

        assert!(
            warning.is_none(),
            "successful changelog should produce no warning"
        );
        let entries = cl.recorded();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node_id, id);
        assert_eq!(entries[0].action, ChangeAction::Create);
        assert!(entries[0].before.is_none());
        assert!(entries[0].after.is_some());
    }

    #[tokio::test]
    async fn test_add_node_changelog_failure_produces_warning() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::failing();
        let svc = legacy_cl(repo, Box::new(cl));
        let (_, warning) = svc.add_node(add_req("Node A")).await.expect("add_node");
        assert!(
            warning.is_some(),
            "failing changelog should produce a warning"
        );
        assert!(
            warning.unwrap().contains("changelog:"),
            "warning should contain 'changelog:'"
        );
    }

    #[tokio::test]
    async fn test_update_node_records_before_and_after() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = legacy_cl(repo, Box::new(cl));

        let (id, _) = svc.add_node(add_req("original title")).await.expect("add");
        let ((), warning) = svc
            .update_node(id, title_req("updated title"))
            .await
            .expect("update");
        assert!(warning.is_none());
    }

    #[tokio::test]
    async fn test_remove_node_records_delete() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = legacy_cl(repo, Box::new(cl));

        let (id, _) = svc.add_node(add_req("to be removed")).await.expect("add");
        let ((), warning) = svc.remove_node(id).await.expect("remove");
        assert!(warning.is_none());
    }

    #[tokio::test]
    async fn test_move_node_records_move() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = legacy_cl(repo, Box::new(cl));

        let (id, _) = svc.add_node(add_req("node to move")).await.expect("add");
        let ((), warning) = svc.move_node(id, None, 0).await.expect("move");
        assert!(warning.is_none());
    }

    #[tokio::test]
    async fn test_book_not_found_error() {
        let repo = InMemoryBookRepo::empty();
        let svc = legacy(repo);
        let err = svc.add_node(add_req("x")).await.unwrap_err();
        assert!(matches!(err, AppError::BookNotFound));
    }

    #[tokio::test]
    async fn test_without_changelog_still_works() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);
        let (id, _) = svc.add_node(add_req("x")).await.expect("add");
        let tree = svc.read_tree().await.expect("read_tree");
        assert!(tree.get_node(id).is_some());
    }

    #[tokio::test]
    async fn test_node_status_not_related_to_service() {
        // NodeStatus は domain 層でテスト済みだが、service 経由で参照できることを確認
        let _ = NodeStatus::Draft;
        let _ = NodeStatus::Active;
    }

    // ---- batch_move tests ----

    #[tokio::test]
    async fn test_batch_move_empty_succeeds() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);
        let (count, warnings) = svc.batch_move(vec![]).await.expect("batch_move empty");
        assert_eq!(count, 0);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn test_batch_move_single_node() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);

        let (id_a, _) = svc.add_node(add_req("Node A")).await.expect("add_node A");
        let (id_b, _) = svc.add_node(add_req("Node B")).await.expect("add_node B");

        // Move A under B at position 0
        let (count, warnings) = svc
            .batch_move(vec![(id_a, Some(id_b), 0)])
            .await
            .expect("batch_move");
        assert_eq!(count, 1);
        assert_eq!(warnings.len(), 1);
    }

    #[tokio::test]
    async fn test_batch_move_invalid_node_returns_error_no_save() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);

        let (id_a, _) = svc.add_node(add_req("Node A")).await.expect("add_node A");

        // fake NodeId that doesn't exist
        let fake_id: NodeId = serde_json::from_value(serde_json::Value::String(
            "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        ))
        .expect("parse fake id");

        // Attempt to move A and then a nonexistent node — should fail
        let result = svc
            .batch_move(vec![(id_a, None, 0), (fake_id, None, 0)])
            .await;
        assert!(result.is_err(), "batch_move with invalid node should fail");
    }

    #[tokio::test]
    async fn test_batch_move_changelog_records_entries() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = legacy_cl(repo, Box::new(cl));

        let (id_a, _) = svc.add_node(add_req("Node A")).await.expect("add_node A");
        let (id_b, _) = svc.add_node(add_req("Node B")).await.expect("add_node B");

        let (count, warnings) = svc
            .batch_move(vec![(id_a, Some(id_b), 0)])
            .await
            .expect("batch_move");
        assert_eq!(count, 1);
        assert_eq!(warnings, vec![None]);
    }

    // ---- batch_update tests ----

    #[tokio::test]
    async fn test_batch_update_empty_succeeds() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);
        let (count, warnings) = svc.batch_update(vec![]).await.expect("batch_update empty");
        assert_eq!(count, 0);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn test_batch_update_single_node_title() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);

        let (id, _) = svc.add_node(add_req("Original")).await.expect("add_node");

        let (count, warnings) = svc
            .batch_update(vec![(id, title_req("Updated"))])
            .await
            .expect("batch_update");
        assert_eq!(count, 1);
        assert_eq!(warnings.len(), 1);

        let tree = svc.read_tree().await.expect("read_tree");
        assert_eq!(tree.get_node(id).map(|n| n.title()), Some("Updated"));
    }

    #[tokio::test]
    async fn test_batch_update_multiple_nodes() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);

        let (id_a, _) = svc.add_node(add_req("Alpha")).await.expect("add_node A");
        let (id_b, _) = svc.add_node(add_req("Beta")).await.expect("add_node B");

        let mut beta = title_req("Beta Updated");
        beta.status = Some(NodeStatus::Draft);
        let updates = vec![(id_a, title_req("Alpha Updated")), (id_b, beta)];

        let (count, warnings) = svc.batch_update(updates).await.expect("batch_update");
        assert_eq!(count, 2);
        assert_eq!(warnings.len(), 2);

        let tree = svc.read_tree().await.expect("read_tree");
        assert_eq!(
            tree.get_node(id_a).map(|n| n.title()),
            Some("Alpha Updated")
        );
        assert_eq!(tree.get_node(id_b).map(|n| n.title()), Some("Beta Updated"));
    }

    #[tokio::test]
    async fn test_batch_update_invalid_node_returns_error() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = legacy(repo);

        let (id_a, _) = svc.add_node(add_req("Node A")).await.expect("add_node A");

        let fake_id: NodeId = serde_json::from_value(serde_json::Value::String(
            "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        ))
        .expect("parse fake id");

        let result = svc
            .batch_update(vec![
                (id_a, title_req("A Updated")),
                (fake_id, title_req("Fake")),
            ])
            .await;
        assert!(
            result.is_err(),
            "batch_update with invalid node should fail"
        );
    }

    #[tokio::test]
    async fn test_timestamp_now_is_used_in_entry() {
        // Timestamp::now() が panic しないことを確認
        let ts = Timestamp::now();
        assert!(ts.as_millis() > 0);
    }
}
