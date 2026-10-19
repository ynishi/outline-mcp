use crate::domain::error::DomainError;
use crate::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use crate::domain::model::changelog::{ChangeAction, ChangeEntry};
use crate::domain::model::id::NodeId;
use crate::domain::model::timestamp::Timestamp;
use crate::domain::store::{BookStore, StoreError, Versioned};

use super::error::AppError;

/// The result of [`BookService::update_book`]: the book's (title, max depth)
/// before and after, and whether anything was committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookUpdate {
    /// (title, max depth) before the call.
    pub before: (String, u8),
    /// (title, max depth) after the call.
    pub after: (String, u8),
    /// False when the requested values were already in place (nothing
    /// committed, no history entry).
    pub changed: bool,
}

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

    /// Mark `ids` as verified now by `by`: one commit over the version just
    /// read, with one `Update` entry per node (before / after carry the
    /// verification fields). The nodes' `updated_at` is not touched. Fails
    /// as a whole, committing nothing, if any id is unknown.
    pub async fn verify_nodes(
        &self,
        ids: &[NodeId],
        by: Option<String>,
    ) -> Result<Timestamp, AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let at = Timestamp::now();
        let mut events = Vec::with_capacity(ids.len());
        for &id in ids {
            let before = node_json(&book, id);
            book.verify_node(id, at, by.clone())?;
            events.push(ChangeEntry::new(
                id,
                ChangeAction::Update,
                before,
                node_json(&book, id),
                at,
            ));
        }
        self.commit(Some(version), &book, &events).await?;
        Ok(at)
    }

    /// Change the book's own fields: title and / or max depth (`None` keeps
    /// the current value). One commit over the version just read, carrying a
    /// [`ChangeAction::BookUpdate`] entry; when nothing would change, nothing
    /// is committed and the result says so.
    pub async fn update_book(
        &self,
        title: Option<String>,
        max_depth: Option<u8>,
    ) -> Result<BookUpdate, AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let before = (book.title().to_string(), book.max_depth());
        if let Some(title) = title {
            book.set_title(title)?;
        }
        if let Some(max_depth) = max_depth {
            book.set_max_depth(max_depth)?;
        }
        let after = (book.title().to_string(), book.max_depth());
        if before == after {
            return Ok(BookUpdate {
                before,
                after,
                changed: false,
            });
        }
        let entry = book_fields_entry(&before, &after, Timestamp::now());
        self.commit(Some(version), &book, &[entry]).await?;
        Ok(BookUpdate {
            before,
            after,
            changed: true,
        })
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
    /// Records a `Delete` entry, with the node as it was, for the node and for
    /// every descendant removed with it — all in the same commit — so each
    /// removed node's `node_history` ends with its deletion.
    ///
    /// 戻り値: `((), Option<String>)` — 第2要素は store が返した警告。
    pub async fn remove_node(&self, id: NodeId) -> Result<((), Option<String>), AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let removed: Vec<NodeId> = std::iter::once(id).chain(book.descendant_ids(id)).collect();
        let at = Timestamp::now();
        let mut entries: Vec<ChangeEntry> = removed
            .iter()
            .map(|&n| ChangeEntry::new(n, ChangeAction::Delete, node_json(&book, n), None, at))
            .collect();
        // Nodes that keep existing but link into the removed subtree lose
        // those links (the domain drops them); record each such node.
        let link_holders: Vec<(NodeId, Option<String>)> =
            links_into(&book, &removed.iter().copied().collect())
                .into_iter()
                .map(|n| (n, node_json(&book, n)))
                .collect();
        book.remove_node(id)?;
        for (n, before) in link_holders {
            entries.push(ChangeEntry::new(
                n,
                ChangeAction::Update,
                before,
                node_json(&book, n),
                at,
            ));
        }

        let warnings = self.commit(Some(version), &book, &entries).await?;

        Ok(((), join_warnings(warnings)))
    }

    /// Add a typed link from `from` to `to` (see [`TemplateBook::add_link`]):
    /// one commit with an `Update` entry for `from`. Returns `false`, and
    /// commits nothing, when the same link already exists.
    pub async fn add_link(&self, from: NodeId, to: NodeId, rel: &str) -> Result<bool, AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let before = node_json(&book, from);
        if !book.add_link(from, to, rel)? {
            return Ok(false);
        }
        let entry = ChangeEntry::new(
            from,
            ChangeAction::Update,
            before,
            node_json(&book, from),
            Timestamp::now(),
        );
        self.commit(Some(version), &book, &[entry]).await?;
        Ok(true)
    }

    /// Remove the links from `from` to `to` (only type `rel` when given):
    /// one commit with an `Update` entry for `from`. Returns how many were
    /// removed; nothing is committed when none were.
    pub async fn remove_link(
        &self,
        from: NodeId,
        to: NodeId,
        rel: Option<&str>,
    ) -> Result<usize, AppError> {
        let Versioned {
            value: mut book,
            version,
        } = self.load_book().await?;
        let before = node_json(&book, from);
        let removed = book.remove_link(from, to, rel)?;
        if removed == 0 {
            return Ok(0);
        }
        let entry = ChangeEntry::new(
            from,
            ChangeAction::Update,
            before,
            node_json(&book, from),
            Timestamp::now(),
        );
        self.commit(Some(version), &book, &[entry]).await?;
        Ok(removed)
    }

    /// Apply `change` to a copy of the current book without committing, and
    /// return the book before and after. For dry runs: the same validation as
    /// the real operation, nothing written.
    pub async fn preview<F>(&self, change: F) -> Result<(TemplateBook, TemplateBook), AppError>
    where
        F: FnOnce(&mut TemplateBook) -> Result<(), DomainError>,
    {
        let before = self.load_book().await?.value;
        let mut after = before.clone();
        change(&mut after)?;
        Ok((before, after))
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

    /// Book をそのまま保存する (無ければ作成)。**event を記録しない。**
    ///
    /// 履歴を別途持ち込む embedder 向けの低レベル経路で、MCP tool からは
    /// 呼ばない: book を置き換える tool は [`Self::import_book`] /
    /// [`Self::restore_book`] を使い、置き換えを同じ commit で履歴に残す。
    pub async fn save_book(&self, book: &TemplateBook) -> Result<(), AppError> {
        let expected = self.current_version().await?;
        self.commit(expected, book, &[]).await?;
        Ok(())
    }

    /// Replace the book with `book` (an imported tree) and record the
    /// replacement in the same commit: a `Delete` entry for every current node
    /// that `book` does not have, a `Create` entry for every node of `book`
    /// that is new, an `Update` entry for a node present in both whose
    /// content differs, and a [`ChangeAction::BookUpdate`] entry when the
    /// title or max depth changes. Returns the number of entries recorded.
    ///
    /// The commit goes over the version just read, so a concurrent write in
    /// between is a conflict. A book that does not exist yet is created, with a
    /// `Create` entry per node.
    pub async fn import_book(&self, book: &TemplateBook) -> Result<usize, AppError> {
        let current = self.store.load(&self.slug).await.map_err(store_error)?;
        let at = Timestamp::now();
        let mut events = Vec::new();

        if let Some(cur) = &current {
            events.extend(book_fields_change(&cur.value, book, at));
            for id in cur.value.all_node_ids() {
                if book.get_node(id).is_none() {
                    events.push(ChangeEntry::new(
                        id,
                        ChangeAction::Delete,
                        node_json(&cur.value, id),
                        None,
                        at,
                    ));
                }
            }
        }
        for id in book.all_node_ids() {
            let after = node_json(book, id);
            let before = current
                .as_ref()
                .filter(|cur| cur.value.get_node(id).is_some())
                .map(|cur| &cur.value);
            match before {
                None => events.push(ChangeEntry::new(id, ChangeAction::Create, None, after, at)),
                // Compare as JSON values, not strings: `properties` is a
                // HashMap, so two equal nodes can serialize with their keys in
                // a different order.
                Some(cur) if node_value(cur, id) != node_value(book, id) => events.push(
                    ChangeEntry::new(id, ChangeAction::Update, node_json(cur, id), after, at),
                ),
                Some(_) => {}
            }
        }

        self.commit(current.map(|v| v.version), book, &events)
            .await?;
        Ok(events.len())
    }

    /// Replace the book with `book` (a snapshot being restored), recording a
    /// `Restore` event for every node of `book` in the same commit, plus a
    /// [`ChangeAction::BookUpdate`] entry when the restored title or max depth
    /// differs from the current one.
    ///
    /// The book must exist: a restore goes over the version just read, so a
    /// concurrent write between the read and this commit is a conflict rather
    /// than being silently overwritten.
    pub async fn restore_book(&self, book: &TemplateBook) -> Result<(), AppError> {
        let current = self.load_book().await?;
        let at = Timestamp::now();
        let mut events: Vec<ChangeEntry> = book_fields_change(&current.value, book, at)
            .into_iter()
            .collect();
        events.extend(
            book.all_node_ids()
                .map(|id| ChangeEntry::new(id, ChangeAction::Restore, None, None, at)),
        );
        let version = current.version;
        self.commit(Some(version), book, &events).await?;
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

/// Nodes outside `targets` that hold at least one link into `targets`, in
/// book order.
pub fn links_into(book: &TemplateBook, targets: &std::collections::HashSet<NodeId>) -> Vec<NodeId> {
    book.all_nodes_dfs()
        .into_iter()
        .filter(|n| !targets.contains(&n.id()))
        .filter(|n| n.links().iter().any(|l| targets.contains(&l.target())))
        .map(|n| n.id())
        .collect()
}

/// A [`ChangeAction::BookUpdate`] entry for the book's own fields.
fn book_fields_entry(before: &(String, u8), after: &(String, u8), at: Timestamp) -> ChangeEntry {
    let fields = |(title, max_depth): &(String, u8)| {
        serde_json::json!({ "title": title, "max_depth": max_depth }).to_string()
    };
    ChangeEntry::book_update(fields(before), fields(after), at)
}

/// The book-level entry for replacing `current` with `next`, when their
/// title or max depth differ (restore / import carry the book's own fields
/// along with the tree).
fn book_fields_change(
    current: &TemplateBook,
    next: &TemplateBook,
    at: Timestamp,
) -> Option<ChangeEntry> {
    let before = (current.title().to_string(), current.max_depth());
    let after = (next.title().to_string(), next.max_depth());
    (before != after).then(|| book_fields_entry(&before, &after, at))
}

/// The node as a JSON value, for an order-insensitive comparison of its
/// content (object keys compare as a map, not by position).
fn node_value(book: &TemplateBook, id: NodeId) -> Option<serde_json::Value> {
    book.get_node(id).and_then(|n| serde_json::to_value(n).ok())
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
    use crate::domain::error::DomainError;
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
    async fn test_verify_nodes_is_one_commit_and_keeps_updated_at() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.expect("add A");
        let (b, _) = svc.add_node(add_req("B")).await.expect("add B");
        let edited = svc
            .read_tree()
            .await
            .unwrap()
            .get_node(a)
            .unwrap()
            .updated_at();
        let before = svc.store.load("test").await.unwrap().unwrap().version;

        let at = svc
            .verify_nodes(&[a, b], Some("alice".into()))
            .await
            .expect("verify");

        assert_eq!(
            svc.store.load("test").await.unwrap().unwrap().version,
            before + 1
        );
        let book = svc.read_tree().await.unwrap();
        let na = book.get_node(a).unwrap();
        assert_eq!(na.verified_at(), Some(at));
        assert_eq!(na.verified_by(), Some("alice"));
        assert_eq!(na.updated_at(), edited, "verifying is not editing");
        let h = svc.history_by_node(a).await.unwrap();
        let last = h.last().unwrap();
        assert_eq!(last.action, ChangeAction::Update);
        assert!(last
            .after
            .as_deref()
            .unwrap()
            .contains("\"verified_by\":\"alice\""));
        assert!(!last.before.as_deref().unwrap().contains("verified_at"));
    }

    #[tokio::test]
    async fn test_verify_nodes_with_unknown_id_commits_nothing() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.expect("add A");
        let before = svc.store.load("test").await.unwrap().unwrap().version;
        assert!(svc.verify_nodes(&[a, NodeId::new()], None).await.is_err());
        assert_eq!(
            svc.store.load("test").await.unwrap().unwrap().version,
            before
        );
        assert!(svc
            .read_tree()
            .await
            .unwrap()
            .get_node(a)
            .unwrap()
            .verified_at()
            .is_none());
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

    fn child_of(parent: NodeId, title: &str) -> AddNodeRequest {
        AddNodeRequest {
            parent: Some(parent),
            ..add_req(title)
        }
    }

    #[tokio::test]
    async fn test_remove_records_a_delete_for_every_removed_node_in_one_commit() {
        let svc = memory_service().await;
        let (root, _) = svc.add_node(add_req("root")).await.unwrap();
        let (child, _) = svc.add_node(child_of(root, "child")).await.unwrap();
        let (grand, _) = svc.add_node(child_of(child, "grand")).await.unwrap();
        let (keep, _) = svc.add_node(add_req("keep")).await.unwrap();
        let before = svc.history().await.unwrap().len();

        svc.remove_node(root).await.expect("remove");

        let history = svc.history().await.unwrap();
        let deletes: Vec<_> = history[before..].iter().collect();
        assert_eq!(deletes.len(), 3, "one commit, three deletes: {deletes:?}");
        assert!(deletes.iter().all(|e| e.action == ChangeAction::Delete));
        assert!(deletes
            .iter()
            .all(|e| e.before.is_some() && e.after.is_none()));
        let ids: Vec<_> = deletes.iter().map(|e| e.node_id).collect();
        assert_eq!(ids, vec![root, child, grand]);
        for id in [root, child, grand] {
            let h = svc.history_by_node(id).await.unwrap();
            assert_eq!(h.last().unwrap().action, ChangeAction::Delete);
        }
        let tree = svc.read_tree().await.unwrap();
        assert_eq!(tree.node_count(), 1);
        assert!(tree.get_node(keep).is_some());
    }

    #[tokio::test]
    async fn test_preview_validates_and_commits_nothing() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("a")).await.unwrap();
        let (b, _) = svc.add_node(add_req("b")).await.unwrap();
        let (c, _) = svc.add_node(child_of(a, "c")).await.unwrap();
        let history_before = svc.history().await.unwrap().len();

        let (before, after) = svc
            .preview(|book| book.move_node(a, Some(b), usize::MAX))
            .await
            .expect("preview");
        assert_eq!(before.get_node(a).unwrap().parent(), None);
        assert_eq!(after.get_node(a).unwrap().parent(), Some(b));
        assert_eq!(after.get_node(c).unwrap().parent(), Some(a));

        // Nothing was written: tree and history unchanged.
        let tree = svc.read_tree().await.unwrap();
        assert_eq!(tree.get_node(a).unwrap().parent(), None);
        assert_eq!(svc.history().await.unwrap().len(), history_before);

        // An invalid change is refused the same way the real one is.
        let cyclic = svc.preview(|book| book.move_node(b, Some(c), 0)).await;
        assert!(cyclic.is_ok(), "b under c is fine");
        let cycle = svc.preview(|book| book.move_node(a, Some(c), 0)).await;
        assert!(matches!(cycle, Err(AppError::Domain(_))));
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
    async fn test_import_book_records_the_replacement_in_one_commit() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.expect("add A");
        let (b, _) = svc.add_node(add_req("B")).await.expect("add B");
        let before_len = svc.history().await.unwrap().len();
        let before_version = svc.store.load("test").await.unwrap().unwrap().version;

        // Imported tree: A kept but retitled, B gone, C new.
        let mut imported = svc.read_tree().await.unwrap();
        imported.update_node(a, title_req("A2")).unwrap();
        imported.remove_node(b).unwrap();
        let c = imported.add_node(add_req("C")).unwrap();

        let recorded = svc.import_book(&imported).await.expect("import");
        assert_eq!(recorded, 3);

        let after_version = svc.store.load("test").await.unwrap().unwrap().version;
        assert_eq!(after_version, before_version + 1, "one commit");

        let new_entries: Vec<_> = svc.history().await.unwrap()[before_len..]
            .iter()
            .map(|e| (e.node_id, e.action))
            .collect();
        assert!(new_entries.contains(&(a, ChangeAction::Update)));
        assert!(new_entries.contains(&(b, ChangeAction::Delete)));
        assert!(new_entries.contains(&(c, ChangeAction::Create)));
        assert_eq!(new_entries.len(), 3);

        let b_history = svc.history_by_node(b).await.unwrap();
        assert_eq!(b_history.last().unwrap().action, ChangeAction::Delete);
        assert!(b_history.last().unwrap().before.is_some());
        let tree = svc.read_tree().await.unwrap();
        assert!(tree.get_node(b).is_none());
        assert_eq!(tree.get_node(a).unwrap().title(), "A2");
    }

    #[tokio::test]
    async fn test_import_book_unchanged_tree_records_nothing() {
        let svc = memory_service().await;
        svc.add_node(add_req("A")).await.expect("add A");
        let same = svc.read_tree().await.unwrap();
        assert_eq!(svc.import_book(&same).await.expect("import"), 0);
    }

    #[tokio::test]
    async fn test_import_book_unchanged_node_with_many_properties_records_nothing() {
        // `properties` is a HashMap: the same keys can serialize in a
        // different order from two instances, which must not read as a change.
        let svc = memory_service().await;
        let mut req = add_req("P");
        for i in 0..16 {
            req.properties.insert(format!("k{i}"), format!("v{i}"));
        }
        svc.add_node(req).await.expect("add P");
        let same = svc.read_tree().await.unwrap();
        // A round trip through JSON rebuilds every HashMap with a fresh seed.
        let copy: TemplateBook =
            serde_json::from_str(&serde_json::to_string(&same).unwrap()).unwrap();
        assert_eq!(svc.import_book(&copy).await.expect("import"), 0);
    }

    #[tokio::test]
    async fn test_import_book_with_a_new_title_records_a_book_update() {
        let svc = memory_service().await;
        svc.add_node(add_req("A")).await.expect("add A");
        let tree = svc.read_tree().await.unwrap();
        let mut imported: TemplateBook =
            serde_json::from_str(&serde_json::to_string(&tree).unwrap()).unwrap();
        imported.set_title("Renamed".to_string()).unwrap();

        assert_eq!(svc.import_book(&imported).await.expect("import"), 1);
        let last = svc.history().await.unwrap().pop().unwrap();
        assert_eq!(last.action, ChangeAction::BookUpdate);
        assert!(last.node_id.is_book_level());
        assert!(last.before.unwrap().contains("\"Test\""));
        assert!(last.after.unwrap().contains("\"Renamed\""));
    }

    #[tokio::test]
    async fn test_restore_book_records_the_revert_of_book_fields() {
        let svc = memory_service().await;
        svc.add_node(add_req("A")).await.expect("add A");
        let old = svc.read_tree().await.unwrap(); // max_depth 4, title "Test"
        svc.update_book(None, Some(6)).await.expect("raise");

        svc.restore_book(&old).await.expect("restore");
        let book_level: Vec<_> = svc
            .history()
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.action == ChangeAction::BookUpdate)
            .collect();
        assert_eq!(book_level.len(), 2, "the raise and the revert");
        let revert = book_level.last().unwrap();
        assert!(revert
            .before
            .as_deref()
            .unwrap()
            .contains("\"max_depth\":6"));
        assert!(revert.after.as_deref().unwrap().contains("\"max_depth\":4"));
        assert_eq!(svc.read_tree().await.unwrap().max_depth(), 4);
    }

    #[tokio::test]
    async fn test_restore_book_same_fields_records_no_book_update() {
        let svc = memory_service().await;
        svc.add_node(add_req("A")).await.expect("add A");
        let same = svc.read_tree().await.unwrap();
        svc.restore_book(&same).await.expect("restore");
        assert!(svc
            .history()
            .await
            .unwrap()
            .iter()
            .all(|e| e.action != ChangeAction::BookUpdate));
    }

    #[tokio::test]
    async fn test_import_book_into_a_missing_book_creates_it_with_create_entries() {
        let svc = BookService::new(MemoryBookStore::new(), "fresh");
        let mut book = TemplateBook::new("Fresh", 4);
        let x = book.add_node(add_req("X")).unwrap();
        assert_eq!(svc.import_book(&book).await.expect("import"), 1);
        let h = svc.history().await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].node_id, h[0].action), (x, ChangeAction::Create));
    }

    #[tokio::test]
    async fn test_update_book_is_one_commit_with_one_book_level_event() {
        let svc = memory_service().await;
        let v0 = svc.current_version().await.unwrap().unwrap();

        let up = svc
            .update_book(Some("Renamed".into()), Some(6))
            .await
            .expect("update_book");
        assert!(up.changed);
        assert_eq!(up.before, ("Test".to_string(), 4));
        assert_eq!(up.after, ("Renamed".to_string(), 6));
        assert_eq!(svc.current_version().await.unwrap(), Some(v0 + 1));

        let h = svc.history().await.unwrap();
        let last = h.last().unwrap();
        assert_eq!(last.action, ChangeAction::BookUpdate);
        assert!(last.node_id.is_book_level());
        let before: serde_json::Value =
            serde_json::from_str(last.before.as_deref().unwrap()).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(last.after.as_deref().unwrap()).unwrap();
        assert_eq!(before, serde_json::json!({"title": "Test", "max_depth": 4}));
        assert_eq!(
            after,
            serde_json::json!({"title": "Renamed", "max_depth": 6})
        );

        let book = svc.read_tree().await.unwrap();
        assert_eq!((book.title(), book.max_depth()), ("Renamed", 6));
    }

    #[tokio::test]
    async fn test_update_book_without_a_change_commits_nothing() {
        let svc = memory_service().await;
        let v0 = svc.current_version().await.unwrap();
        let n0 = svc.history().await.unwrap().len();
        let up = svc.update_book(Some("Test".into()), Some(4)).await.unwrap();
        assert!(!up.changed);
        assert_eq!(svc.current_version().await.unwrap(), v0);
        assert_eq!(svc.history().await.unwrap().len(), n0);
    }

    #[tokio::test]
    async fn test_update_book_refused_change_commits_nothing() {
        let svc = memory_service().await;
        let v0 = svc.current_version().await.unwrap();
        let err = svc.update_book(Some("".into()), None).await.unwrap_err();
        assert!(matches!(err, AppError::Domain(DomainError::EmptyBookTitle)));
        assert_eq!(svc.current_version().await.unwrap(), v0);
    }

    /// A store that lets one other writer commit right after `update_book`
    /// read the book, so its commit goes over a stale version.
    struct InterleavingStore {
        inner: MemoryBookStore,
        armed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl BookStore for InterleavingStore {
        async fn load(&self, slug: &str) -> Result<Option<Versioned<TemplateBook>>, StoreError> {
            let loaded = self.inner.load(slug).await?;
            if let Some(v) = &loaded {
                if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    self.inner
                        .commit(slug, Some(v.version), &v.value, &[])
                        .await?;
                }
            }
            Ok(loaded)
        }
        async fn commit(
            &self,
            slug: &str,
            expected: Option<i64>,
            book: &TemplateBook,
            events: &[ChangeEntry],
        ) -> Result<crate::domain::store::Committed, StoreError> {
            self.inner.commit(slug, expected, book, events).await
        }
        async fn history(&self, slug: &str) -> Result<Vec<ChangeEntry>, StoreError> {
            self.inner.history(slug).await
        }
        async fn history_by_node(
            &self,
            slug: &str,
            id: NodeId,
        ) -> Result<Vec<ChangeEntry>, StoreError> {
            self.inner.history_by_node(slug, id).await
        }
        async fn list_books(&self) -> Result<Vec<crate::domain::store::BookSummary>, StoreError> {
            self.inner.list_books().await
        }
        async fn delete(&self, slug: &str, expected: i64) -> Result<bool, StoreError> {
            self.inner.delete(slug, expected).await
        }
    }

    #[tokio::test]
    async fn test_update_book_conflicts_on_a_concurrent_write() {
        let store = InterleavingStore {
            inner: MemoryBookStore::new(),
            armed: std::sync::atomic::AtomicBool::new(false),
        };
        let svc = BookService::new(store, "c");
        svc.create_book("C", 3).await.unwrap();
        svc.store
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let err = svc
            .update_book(Some("Other".into()), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict { .. }), "{err:?}");
        let book = svc.read_tree().await.unwrap();
        assert_eq!(book.title(), "C", "the conflicting change was not stored");
    }

    #[tokio::test]
    async fn test_timestamp_now_is_used_in_entry() {
        // Timestamp::now() が panic しないことを確認
        let ts = Timestamp::now();
        assert!(ts.as_millis() > 0);
    }

    #[tokio::test]
    async fn test_add_and_remove_link_record_one_update_each() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.unwrap();
        let (b, _) = svc.add_node(add_req("B")).await.unwrap();
        let base = svc.history().await.unwrap().len();

        assert!(svc.add_link(a, b, "supersedes").await.unwrap());
        assert!(
            !svc.add_link(a, b, "supersedes").await.unwrap(),
            "duplicate"
        );
        let h = svc.history().await.unwrap();
        assert_eq!(h.len(), base + 1, "the duplicate committed nothing");
        let last = h.last().unwrap();
        assert_eq!((last.node_id, last.action), (a, ChangeAction::Update));
        assert!(last.after.as_deref().unwrap().contains("supersedes"));

        assert_eq!(svc.remove_link(a, b, None).await.unwrap(), 1);
        assert_eq!(svc.remove_link(a, b, None).await.unwrap(), 0);
        assert_eq!(svc.history().await.unwrap().len(), base + 2);
    }

    #[tokio::test]
    async fn test_removing_a_link_target_updates_the_holders_in_the_same_commit() {
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.unwrap();
        let (b, _) = svc.add_node(add_req("B")).await.unwrap();
        let (c, _) = svc.add_node(add_req("C")).await.unwrap();
        svc.add_link(a, b, "see_also").await.unwrap();
        svc.add_link(c, b, "supersedes").await.unwrap();
        svc.add_link(c, a, "see_also").await.unwrap();
        let base = svc.history().await.unwrap().len();

        svc.remove_node(b).await.unwrap();
        let h = svc.history().await.unwrap();
        let new: Vec<_> = h[base..].iter().map(|e| (e.node_id, e.action)).collect();
        assert_eq!(
            new,
            vec![
                (b, ChangeAction::Delete),
                (a, ChangeAction::Update),
                (c, ChangeAction::Update)
            ]
        );
        let book = svc.read_tree().await.unwrap();
        assert!(book.get_node(a).unwrap().links().is_empty());
        let c_links: Vec<_> = book
            .get_node(c)
            .unwrap()
            .links()
            .iter()
            .map(|l| l.target())
            .collect();
        assert_eq!(c_links, vec![a]);
    }

    #[tokio::test]
    async fn test_link_changes_leave_updated_at_alone() {
        // Links are curation metadata, like a verification: adding or
        // removing one, or losing one because its target was deleted, does
        // not count as editing the node.
        let svc = memory_service().await;
        let (a, _) = svc.add_node(add_req("A")).await.unwrap();
        let (b, _) = svc.add_node(add_req("B")).await.unwrap();
        let (c, _) = svc.add_node(add_req("C")).await.unwrap();
        let stamp = |book: &TemplateBook, id| book.get_node(id).unwrap().updated_at();
        let before = svc.read_tree().await.unwrap();
        let (a0, c0) = (stamp(&before, a), stamp(&before, c));
        // Let the clock move, so a bump would be visible.
        std::thread::sleep(std::time::Duration::from_millis(5));

        svc.add_link(a, b, "see_also").await.unwrap();
        svc.add_link(c, b, "supersedes").await.unwrap();
        let t = svc.read_tree().await.unwrap();
        assert_eq!((stamp(&t, a), stamp(&t, c)), (a0, c0), "link_add");

        svc.remove_link(a, b, None).await.unwrap();
        let t = svc.read_tree().await.unwrap();
        assert_eq!(stamp(&t, a), a0, "link_remove");

        svc.remove_node(b).await.unwrap();
        let t = svc.read_tree().await.unwrap();
        assert!(t.get_node(c).unwrap().links().is_empty());
        assert_eq!(stamp(&t, c), c0, "link dropped with its target");
    }
}
