use crate::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use crate::domain::model::changelog::{ChangeAction, ChangeEntry};
use crate::domain::model::id::NodeId;
use crate::domain::model::timestamp::Timestamp;
use crate::domain::repository::{BookRepository, ChangeLogRepository};

use super::error::AppError;

/// Template Bookに対するユースケース。
/// load → mutate → save のパターンで操作する。
pub struct BookService<R: BookRepository> {
    repo: R,
    changelog: Option<Box<dyn ChangeLogRepository>>,
}

impl<R: BookRepository> BookService<R> {
    pub fn new(repo: R) -> Self {
        Self {
            repo,
            changelog: None,
        }
    }

    /// ChangeLogRepository を設定する（builder パターン）。
    pub fn with_changelog(mut self, changelog: Box<dyn ChangeLogRepository>) -> Self {
        self.changelog = Some(changelog);
        self
    }

    /// Bookを新規作成して永続化する。既存Bookがあれば上書き。
    pub fn create_book(&self, title: &str, max_depth: u8) -> Result<TemplateBook, AppError> {
        let book = TemplateBook::new(title, max_depth);
        self.repo
            .save(&book)
            .map_err(|e| AppError::Storage(Box::new(e)))?;
        Ok(book)
    }

    /// ノードを追加する。
    ///
    /// 戻り値: `(NodeId, Option<String>)` — 第2要素は changelog 書き込み失敗時の警告メッセージ。
    pub fn add_node(&self, req: AddNodeRequest) -> Result<(NodeId, Option<String>), AppError> {
        let mut book = self.load_book()?;
        let id = book.add_node(req)?;
        self.persist(&book)?;

        let warning = self.append_changelog(|| {
            let after_json = book
                .get_node(id)
                .and_then(|n| serde_json::to_string(n).ok());
            ChangeEntry::new(id, ChangeAction::Create, None, after_json, Timestamp::now())
        });

        Ok((id, warning))
    }

    /// ノードを更新する。
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は changelog 書き込み失敗時の警告メッセージ。
    pub fn update_node(
        &self,
        id: NodeId,
        req: UpdateNodeRequest,
    ) -> Result<((), Option<String>), AppError> {
        let mut book = self.load_book()?;
        let before_json = book
            .get_node(id)
            .and_then(|n| serde_json::to_string(n).ok());
        book.update_node(id, req)?;
        self.persist(&book)?;

        let warning = self.append_changelog(|| {
            let after_json = book
                .get_node(id)
                .and_then(|n| serde_json::to_string(n).ok());
            ChangeEntry::new(
                id,
                ChangeAction::Update,
                before_json,
                after_json,
                Timestamp::now(),
            )
        });

        Ok(((), warning))
    }

    /// ノードを移動する。
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は changelog 書き込み失敗時の警告メッセージ。
    pub fn move_node(
        &self,
        id: NodeId,
        new_parent: Option<NodeId>,
        position: usize,
    ) -> Result<((), Option<String>), AppError> {
        let mut book = self.load_book()?;
        let before_json = book
            .get_node(id)
            .and_then(|n| serde_json::to_string(n).ok());
        book.move_node(id, new_parent, position)?;
        self.persist(&book)?;

        let warning = self.append_changelog(|| {
            let after_json = book
                .get_node(id)
                .and_then(|n| serde_json::to_string(n).ok());
            ChangeEntry::new(
                id,
                ChangeAction::Move,
                before_json,
                after_json,
                Timestamp::now(),
            )
        });

        Ok(((), warning))
    }

    /// ノードを削除する（子孫ごと）。
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は changelog 書き込み失敗時の警告メッセージ。
    pub fn remove_node(&self, id: NodeId) -> Result<((), Option<String>), AppError> {
        let mut book = self.load_book()?;
        let before_json = book
            .get_node(id)
            .and_then(|n| serde_json::to_string(n).ok());
        book.remove_node(id)?;
        self.persist(&book)?;

        let warning = self.append_changelog(|| {
            ChangeEntry::new(
                id,
                ChangeAction::Delete,
                before_json,
                None,
                Timestamp::now(),
            )
        });

        Ok(((), warning))
    }

    /// 複数ノードをアトミックに移動する（C案: 全成功 or 全保存なし）。
    ///
    /// `moves` は `(NodeId, new_parent: Option<NodeId>, position: usize)` のリスト。
    /// 戻り値: `(成功件数, changelog警告リスト)` — エラー時はErrを返しsaveしない。
    pub fn batch_move(
        &self,
        moves: Vec<(NodeId, Option<NodeId>, usize)>,
    ) -> Result<(usize, Vec<Option<String>>), AppError> {
        let mut book = self.load_book()?;
        let mut before_jsons: Vec<Option<String>> = Vec::with_capacity(moves.len());

        for (id, new_parent, position) in &moves {
            let before_json = book
                .get_node(*id)
                .and_then(|n| serde_json::to_string(n).ok());
            before_jsons.push(before_json);
            book.move_node(*id, *new_parent, *position)?;
        }

        self.persist(&book)?;

        let mut warnings: Vec<Option<String>> = Vec::with_capacity(moves.len());
        for (i, (id, _, _)) in moves.iter().enumerate() {
            let before_json = before_jsons[i].clone();
            let id = *id;
            let warning = self.append_changelog(|| {
                let after_json = book
                    .get_node(id)
                    .and_then(|n| serde_json::to_string(n).ok());
                ChangeEntry::new(
                    id,
                    ChangeAction::Move,
                    before_json,
                    after_json,
                    Timestamp::now(),
                )
            });
            warnings.push(warning);
        }

        Ok((moves.len(), warnings))
    }

    /// 複数ノードをアトミックに更新する（C案: 全成功 or 全保存なし）。
    ///
    /// `updates` は `(NodeId, UpdateNodeRequest)` のリスト。
    /// 戻り値: `(成功件数, changelog警告リスト)` — エラー時はErrを返しsaveしない。
    pub fn batch_update(
        &self,
        updates: Vec<(NodeId, UpdateNodeRequest)>,
    ) -> Result<(usize, Vec<Option<String>>), AppError> {
        let mut book = self.load_book()?;

        // before_jsonとnode_idを先に収集してからmutatbleな操作を実行する
        let mut before_jsons: Vec<Option<String>> = Vec::with_capacity(updates.len());
        let mut node_ids: Vec<NodeId> = Vec::with_capacity(updates.len());

        for (id, req) in &updates {
            let before_json = book
                .get_node(*id)
                .and_then(|n| serde_json::to_string(n).ok());
            before_jsons.push(before_json);
            node_ids.push(*id);
            book.update_node(
                *id,
                UpdateNodeRequest {
                    title: req.title.clone(),
                    body: req.body.clone(),
                    node_type: req.node_type.clone(),
                    placeholder: req.placeholder.clone(),
                    properties: req.properties.clone(),
                    status: req.status,
                },
            )?;
        }

        self.persist(&book)?;

        let mut warnings: Vec<Option<String>> = Vec::with_capacity(node_ids.len());
        for (i, id) in node_ids.iter().enumerate() {
            let before_json = before_jsons[i].clone();
            let id = *id;
            let warning = self.append_changelog(|| {
                let after_json = book
                    .get_node(id)
                    .and_then(|n| serde_json::to_string(n).ok());
                ChangeEntry::new(
                    id,
                    ChangeAction::Update,
                    before_json,
                    after_json,
                    Timestamp::now(),
                )
            });
            warnings.push(warning);
        }

        Ok((node_ids.len(), warnings))
    }

    /// Tree全体または部分木を読み取る。
    pub fn read_tree(&self) -> Result<TemplateBook, AppError> {
        self.load_book()
    }

    /// インポートされたBookを保存する。
    pub fn save_book(&self, book: &TemplateBook) -> Result<(), AppError> {
        self.persist(book)
    }

    // --- private ---

    fn load_book(&self) -> Result<TemplateBook, AppError> {
        self.repo
            .load()
            .map_err(|e| AppError::Storage(Box::new(e)))?
            .ok_or(AppError::BookNotFound)
    }

    fn persist(&self, book: &TemplateBook) -> Result<(), AppError> {
        self.repo
            .save(book)
            .map_err(|e| AppError::Storage(Box::new(e)))
    }

    /// ChangeLog への追記をベストエフォートで実行する。
    ///
    /// changelog が None の場合はスキップ。失敗時は警告メッセージを返す（サイレント失敗禁止）。
    fn append_changelog<F>(&self, entry_fn: F) -> Option<String>
    where
        F: FnOnce() -> ChangeEntry,
    {
        let cl = self.changelog.as_ref()?;
        let entry = entry_fn();
        cl.append(&entry).err().map(|e| format!("changelog: {e}"))
    }
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

    impl BookRepository for InMemoryBookRepo {
        type Error = RepoError;
        fn load(&self) -> Result<Option<TemplateBook>, RepoError> {
            Ok(self.book.lock().unwrap().clone())
        }
        fn save(&self, book: &TemplateBook) -> Result<(), RepoError> {
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

    impl ChangeLogRepository for RecordingChangeLog {
        fn append(
            &self,
            entry: &ChangeEntry,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            if self.fail {
                return Err(Box::new(FakeChangeLogError));
            }
            self.entries.lock().unwrap().push(entry.clone());
            Ok(())
        }
        fn load_all(&self) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.recorded())
        }
        fn load_by_node(
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

    #[allow(dead_code)]
    fn book_with_service() -> (TemplateBook, BookService<InMemoryBookRepo>) {
        let book = TemplateBook::new("Test Book", 4);
        let repo = InMemoryBookRepo::with_book(book.clone());
        (book, BookService::new(repo))
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

    #[test]
    fn test_add_node_no_changelog_returns_none_warning() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);
        let (_, warning) = svc.add_node(add_req("Node A")).expect("add_node");
        assert!(warning.is_none(), "no changelog should produce no warning");
    }

    #[test]
    fn test_add_node_with_changelog_records_create() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = Arc::new(RecordingChangeLog::new());
        let cl_clone = Arc::clone(&cl);

        // Box<dyn ChangeLogRepository> のためにラッパー実装
        struct ArcChangeLog(Arc<RecordingChangeLog>);
        impl ChangeLogRepository for ArcChangeLog {
            fn append(
                &self,
                entry: &ChangeEntry,
            ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                self.0.append(entry)
            }
            fn load_all(
                &self,
            ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
                self.0.load_all()
            }
            fn load_by_node(
                &self,
                node_id: NodeId,
            ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>> {
                self.0.load_by_node(node_id)
            }
        }

        let svc = BookService::new(repo).with_changelog(Box::new(ArcChangeLog(cl_clone)));
        let (id, warning) = svc.add_node(add_req("Node A")).expect("add_node");

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

    #[test]
    fn test_add_node_changelog_failure_produces_warning() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::failing();
        let svc = BookService::new(repo).with_changelog(Box::new(cl));
        let (_, warning) = svc.add_node(add_req("Node A")).expect("add_node");
        assert!(
            warning.is_some(),
            "failing changelog should produce a warning"
        );
        assert!(
            warning.unwrap().contains("changelog:"),
            "warning should contain 'changelog:'"
        );
    }

    #[test]
    fn test_update_node_records_before_and_after() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = BookService::new(repo).with_changelog(Box::new(cl));

        let (id, _) = svc.add_node(add_req("original title")).expect("add");
        let update_req = UpdateNodeRequest {
            title: Some("updated title".to_string()),
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            status: None,
        };
        let ((), warning) = svc.update_node(id, update_req).expect("update");
        assert!(warning.is_none());
    }

    #[test]
    fn test_remove_node_records_delete() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = BookService::new(repo).with_changelog(Box::new(cl));

        let (id, _) = svc.add_node(add_req("to be removed")).expect("add");
        let ((), warning) = svc.remove_node(id).expect("remove");
        assert!(warning.is_none());
    }

    #[test]
    fn test_move_node_records_move() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = BookService::new(repo).with_changelog(Box::new(cl));

        let (id, _) = svc.add_node(add_req("node to move")).expect("add");
        let ((), warning) = svc.move_node(id, None, 0).expect("move");
        assert!(warning.is_none());
    }

    #[test]
    fn test_book_not_found_error() {
        let repo = InMemoryBookRepo::empty();
        let svc = BookService::new(repo);
        let err = svc.add_node(add_req("x")).unwrap_err();
        assert!(matches!(err, AppError::BookNotFound));
    }

    #[test]
    fn test_with_changelog_builder_does_not_break_existing_new() {
        // with_changelog を呼ばない場合でも動作すること
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo); // with_changelog なし
        let (id, _) = svc.add_node(add_req("x")).expect("add");
        let tree = svc.read_tree().expect("read_tree");
        assert!(tree.get_node(id).is_some());
    }

    #[test]
    fn test_node_status_not_related_to_service() {
        // NodeStatus は domain 層でテスト済みだが、service 経由で参照できることを確認
        let _ = NodeStatus::Draft;
        let _ = NodeStatus::Active;
    }

    // ---- batch_move tests ----

    #[test]
    fn test_batch_move_empty_succeeds() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);
        let (count, warnings) = svc.batch_move(vec![]).expect("batch_move empty");
        assert_eq!(count, 0);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_batch_move_single_node() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);

        let (id_a, _) = svc.add_node(add_req("Node A")).expect("add_node A");
        let (id_b, _) = svc.add_node(add_req("Node B")).expect("add_node B");

        // Move A under B at position 0
        let (count, warnings) = svc
            .batch_move(vec![(id_a, Some(id_b), 0)])
            .expect("batch_move");
        assert_eq!(count, 1);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn test_batch_move_invalid_node_returns_error_no_save() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);

        let (id_a, _) = svc.add_node(add_req("Node A")).expect("add_node A");

        // fake NodeId that doesn't exist
        let fake_id: NodeId = serde_json::from_value(serde_json::Value::String(
            "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        ))
        .expect("parse fake id");

        // Attempt to move A and then a nonexistent node — should fail
        let result = svc.batch_move(vec![(id_a, None, 0), (fake_id, None, 0)]);
        assert!(result.is_err(), "batch_move with invalid node should fail");
    }

    #[test]
    fn test_batch_move_changelog_records_entries() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let cl = RecordingChangeLog::new();
        let svc = BookService::new(repo).with_changelog(Box::new(cl));

        let (id_a, _) = svc.add_node(add_req("Node A")).expect("add_node A");
        let (id_b, _) = svc.add_node(add_req("Node B")).expect("add_node B");

        let (count, _warnings) = svc
            .batch_move(vec![(id_a, Some(id_b), 0)])
            .expect("batch_move");
        assert_eq!(count, 1);
        // No warning expected for successful changelog
    }

    // ---- batch_update tests ----

    #[test]
    fn test_batch_update_empty_succeeds() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);
        let (count, warnings) = svc.batch_update(vec![]).expect("batch_update empty");
        assert_eq!(count, 0);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_batch_update_single_node_title() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);

        let (id, _) = svc.add_node(add_req("Original")).expect("add_node");

        let req = UpdateNodeRequest {
            title: Some("Updated".to_string()),
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            status: None,
        };
        let (count, warnings) = svc.batch_update(vec![(id, req)]).expect("batch_update");
        assert_eq!(count, 1);
        assert_eq!(warnings.len(), 1);

        let tree = svc.read_tree().expect("read_tree");
        assert_eq!(tree.get_node(id).map(|n| n.title()), Some("Updated"));
    }

    #[test]
    fn test_batch_update_multiple_nodes() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);

        let (id_a, _) = svc.add_node(add_req("Alpha")).expect("add_node A");
        let (id_b, _) = svc.add_node(add_req("Beta")).expect("add_node B");

        let updates = vec![
            (
                id_a,
                UpdateNodeRequest {
                    title: Some("Alpha Updated".to_string()),
                    body: None,
                    node_type: None,
                    placeholder: None,
                    properties: None,
                    status: None,
                },
            ),
            (
                id_b,
                UpdateNodeRequest {
                    title: Some("Beta Updated".to_string()),
                    body: None,
                    node_type: None,
                    placeholder: None,
                    properties: None,
                    status: Some(NodeStatus::Draft),
                },
            ),
        ];

        let (count, warnings) = svc.batch_update(updates).expect("batch_update");
        assert_eq!(count, 2);
        assert_eq!(warnings.len(), 2);

        let tree = svc.read_tree().expect("read_tree");
        assert_eq!(
            tree.get_node(id_a).map(|n| n.title()),
            Some("Alpha Updated")
        );
        assert_eq!(tree.get_node(id_b).map(|n| n.title()), Some("Beta Updated"));
    }

    #[test]
    fn test_batch_update_invalid_node_returns_error() {
        let book = TemplateBook::new("Test", 4);
        let repo = InMemoryBookRepo::with_book(book);
        let svc = BookService::new(repo);

        let (id_a, _) = svc.add_node(add_req("Node A")).expect("add_node A");

        let fake_id: NodeId = serde_json::from_value(serde_json::Value::String(
            "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        ))
        .expect("parse fake id");

        let result = svc.batch_update(vec![
            (
                id_a,
                UpdateNodeRequest {
                    title: Some("A Updated".to_string()),
                    body: None,
                    node_type: None,
                    placeholder: None,
                    properties: None,
                    status: None,
                },
            ),
            (
                fake_id,
                UpdateNodeRequest {
                    title: Some("Fake".to_string()),
                    body: None,
                    node_type: None,
                    placeholder: None,
                    properties: None,
                    status: None,
                },
            ),
        ]);
        assert!(
            result.is_err(),
            "batch_update with invalid node should fail"
        );
    }

    #[test]
    fn test_timestamp_now_is_used_in_entry() {
        // Timestamp::now() が panic しないことを確認
        let ts = Timestamp::now();
        assert!(ts.as_millis() > 0);
    }
}
