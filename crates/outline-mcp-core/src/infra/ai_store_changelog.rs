use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use ai_store_core::{Seq, Store, StreamId};

use crate::domain::model::changelog::{ChangeAction, ChangeEntry};
use crate::domain::model::id::NodeId;
use crate::domain::model::timestamp::Timestamp;
use crate::domain::repository::ChangeLogRepository;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// `Event.kind` 文字列。 `ChangeAction` との対応は `action_to_kind` / `kind_to_action` を参照。
const KIND_CREATE: &str = "node_created";
const KIND_UPDATE: &str = "node_updated";
const KIND_DELETE: &str = "node_deleted";
const KIND_MOVE: &str = "node_moved";
const KIND_RESTORE: &str = "reverted";

/// `ChangeAction` → ai-store `Event.kind` 文字列マッピング。
fn action_to_kind(action: ChangeAction) -> &'static str {
    match action {
        ChangeAction::Create => KIND_CREATE,
        ChangeAction::Update => KIND_UPDATE,
        ChangeAction::Delete => KIND_DELETE,
        ChangeAction::Move => KIND_MOVE,
        ChangeAction::Restore => KIND_RESTORE,
    }
}

/// ai-store `Event.kind` 文字列 → `ChangeAction` 逆マッピング。
///
/// 未知の kind (ai-store 側で本 repository が書いていない event) は `None` を返す。
fn kind_to_action(kind: &str) -> Option<ChangeAction> {
    match kind {
        KIND_CREATE => Some(ChangeAction::Create),
        KIND_UPDATE => Some(ChangeAction::Update),
        KIND_DELETE => Some(ChangeAction::Delete),
        KIND_MOVE => Some(ChangeAction::Move),
        KIND_RESTORE => Some(ChangeAction::Restore),
        _ => None,
    }
}

/// `ChangeEntry` の meta 表現。 `node_id` を filter 軸として、 `before` / `after` を
/// JSON 文字列のまま素直に載せる (本実装は book state 全体を持たないため、
/// per-entry の before/after diff のみを patch として計算する)。
///
/// # Restore event の meta 欠落と `node_id: Option` の理由
///
/// `Store::revert` が自動生成する `"reverted"` kind の event は book-level (stream
/// 全体) の巻き戻しで特定 node に紐付かないため、 meta に `node_id` を持たない
/// (`{"revert_to": <seq>}` 相当、 本 repository が書いた形ではない)。 したがって
/// `EntryMeta.node_id` は `Option`、 `load_all` 側で欠落を `NodeId::default()`
/// (fresh random UUID) で補う近似を採用する。
///
/// 本 approximation は「Restore は per-node ではなく book 全体イベント」 という
/// 設計判断の副作用で、 book state を直接持つ後継実装 (`SnapshotService` を
/// ai-store `ProjectionSink` に統合する path) で本節ごと再設計される想定。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EntryMeta {
    #[serde(default)]
    node_id: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
}

/// before / after の JSON 文字列 (`ChangeEntry` 由来) を `serde_json::Value` に変換する。
///
/// `None` は `Value::Null` として扱う。文字列が JSON として不正な場合はエラーを返す。
fn to_value(s: &Option<String>) -> Result<Value, BoxError> {
    match s {
        Some(s) => serde_json::from_str(s).map_err(|e| -> BoxError { Box::new(e) }),
        None => Ok(Value::Null),
    }
}

/// ai-store facade (`Store`) を backend とする `ChangeLogRepository` 実装。
///
/// # Architecture
///
/// - **Stream 粒度**: 1 インスタンス = 1 slug = 1 `StreamId` (book-level stream)。
///   book 単位で event log を持ち、 restore は `Store::revert` で book 全体を
///   巻き戻す形と型整合する。 per-node stream にすると Move / snapshot 横断で
///   複数 stream の atomic 更新が必要になり Store の single-write-channel と
///   噛み合わないため採用しない。
/// - **Async 前提**: `ChangeLogRepository` は `#[async_trait]` 経由の async trait、
///   `Store` も async facade のため直接 `.await` する (blocking bridge は持たない)。
///
/// # ChangeAction → (kind, patch, meta) mapping
///
/// | `ChangeAction` | `Event.kind` | `patch` source | `meta` |
/// |---|---|---|---|
/// | `Create`  | `"node_created"` | `diff(Null, after)` | `{node_id, before, after}` |
/// | `Update`  | `"node_updated"` | `diff(before, after)` | `{node_id, before, after}` |
/// | `Delete`  | `"node_deleted"` | `diff(before, Null)` | `{node_id, before, after}` |
/// | `Move`    | `"node_moved"`   | `diff(before, after)` | `{node_id, before, after}` |
/// | `Restore` | `"reverted"`     | `Store::revert` が自動生成 | `{revert_to: seq}` (meta 上に `node_id` なし) |
///
/// # Patch 計算の approximation
///
/// `patch` は該当 `ChangeEntry` の before / after を JSON `Value` に parse した
/// うえでの `json_patch::diff` で、 **book 全体の state diff ではなく entry 単体の
/// diff** となる。 これは trait シグネチャ `append(&self, entry: &ChangeEntry)` が
/// 単一 entry しか受け取らないための近似であって、 book state を直接持つ後継実装
/// (`SnapshotService` の ai-store `ProjectionSink` 統合) で per-entry patch を
/// book-scope patch に格上げする予定。
pub struct AiStoreChangeLogRepository {
    store: Arc<Store>,
    stream: StreamId,
}

impl AiStoreChangeLogRepository {
    /// 指定した `Store` と slug (book) に対する repository を生成する。
    pub fn new(store: Arc<Store>, slug: impl Into<String>) -> Result<Self, BoxError> {
        let stream = StreamId::new(slug.into());
        Ok(Self { store, stream })
    }

    /// 内部の `Store` への参照。段2 (async 化) 移行時の橋渡し用途。
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// 現在の stream を指定した seq まで revert する。
    ///
    /// `ChangeLogRepository` trait には revert 相当のメソッドがない
    /// (`ChangeEntry::Restore` は target seq を保持しない) ため、段1では
    /// trait 外の専用メソッドとして公開する。段2 (trait 廃止) で正式な
    /// API に昇格させる想定。
    pub async fn revert_to(&self, to_seq: Seq) -> Result<(), BoxError> {
        self.store
            .revert(&self.stream, to_seq)
            .await
            .map_err(|e| -> BoxError { Box::new(e) })?;
        Ok(())
    }
}

#[async_trait]
impl ChangeLogRepository for AiStoreChangeLogRepository {
    async fn append(&self, entry: &ChangeEntry) -> Result<(), BoxError> {
        let kind = action_to_kind(entry.action);
        let before_value = to_value(&entry.before)?;
        let after_value = to_value(&entry.after)?;
        let patch = json_patch::diff(&before_value, &after_value);

        let meta = EntryMeta {
            node_id: Some(entry.node_id),
            before: entry.before.clone(),
            after: entry.after.clone(),
        };
        let meta_value = serde_json::to_value(&meta).map_err(|e| -> BoxError { Box::new(e) })?;

        self.store
            .append(&self.stream, kind, patch, meta_value)
            .await
            .map_err(|e| -> BoxError { Box::new(e) })?;

        Ok(())
    }

    async fn load_all(&self) -> Result<Vec<ChangeEntry>, BoxError> {
        let events = self
            .store
            .read(&self.stream, Seq::ZERO, usize::MAX)
            .await
            .map_err(|e| -> BoxError { Box::new(e) })?;

        let mut entries = Vec::with_capacity(events.len());
        for event in events {
            let Some(action) = kind_to_action(&event.kind) else {
                // 本 repository が書いていない event (未知の kind) は skip する。
                continue;
            };
            let meta: EntryMeta =
                serde_json::from_value(event.meta).map_err(|e| -> BoxError { Box::new(e) })?;
            entries.push(ChangeEntry::new(
                meta.node_id.unwrap_or_default(),
                action,
                meta.before,
                meta.after,
                Timestamp::from_millis(event.at.0),
            ));
        }
        Ok(entries)
    }

    async fn load_by_node(&self, node_id: NodeId) -> Result<Vec<ChangeEntry>, BoxError> {
        // `Store::read_by_meta` は meta の top-level `field` の値が `value` と
        // 一致する event のみ返す。 backend が index を持つ場合 (SQLite の
        // `json_extract` 等) は sub-linear、 mem/fileproj は linear scan にフォール
        // バック (regression なし)。
        let value = serde_json::to_value(node_id).map_err(|e| -> BoxError { Box::new(e) })?;
        let events = self
            .store
            .read_by_meta(&self.stream, "node_id", &value, Seq::ZERO, usize::MAX)
            .await
            .map_err(|e| -> BoxError { Box::new(e) })?;

        let mut entries = Vec::with_capacity(events.len());
        for event in events {
            let Some(action) = kind_to_action(&event.kind) else {
                continue;
            };
            let meta: EntryMeta =
                serde_json::from_value(event.meta).map_err(|e| -> BoxError { Box::new(e) })?;
            entries.push(ChangeEntry::new(
                meta.node_id.unwrap_or_default(),
                action,
                meta.before,
                meta.after,
                Timestamp::from_millis(event.at.0),
            ));
        }
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ai_store_mem::{MemCacheBackend, MemEventBackend};

    fn make_store() -> Arc<Store> {
        let events: Arc<dyn ai_store_core::EventBackend> = Arc::new(MemEventBackend::new());
        let cache: Arc<dyn ai_store_core::CacheBackend> = Arc::new(MemCacheBackend::new());
        Arc::new(Store::new(
            events,
            cache,
            Vec::new(),
            Vec::new(),
            Default::default(),
        ))
    }

    fn make_repo(slug: &str) -> AiStoreChangeLogRepository {
        AiStoreChangeLogRepository::new(make_store(), slug).expect("create repository")
    }

    #[tokio::test]
    async fn test_append_and_load_all_roundtrip() {
        let repo = make_repo("test-book");
        let id1 = NodeId::new();
        let id2 = NodeId::new();

        repo.append(&ChangeEntry::new(
            id1,
            ChangeAction::Create,
            None,
            Some(r#"{"title":"a"}"#.to_string()),
            Timestamp::from_millis(1_000),
        ))
        .await
        .expect("append create");
        repo.append(&ChangeEntry::new(
            id2,
            ChangeAction::Update,
            Some(r#"{"title":"old"}"#.to_string()),
            Some(r#"{"title":"new"}"#.to_string()),
            Timestamp::from_millis(2_000),
        ))
        .await
        .expect("append update");
        repo.append(&ChangeEntry::new(
            id1,
            ChangeAction::Delete,
            Some(r#"{"title":"a"}"#.to_string()),
            None,
            Timestamp::from_millis(3_000),
        ))
        .await
        .expect("append delete");

        let all = repo.load_all().await.expect("load_all");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].node_id, id1);
        assert_eq!(all[0].action, ChangeAction::Create);
        assert_eq!(all[0].after.as_deref(), Some(r#"{"title":"a"}"#));
        assert_eq!(all[1].node_id, id2);
        assert_eq!(all[1].action, ChangeAction::Update);
        assert_eq!(all[2].node_id, id1);
        assert_eq!(all[2].action, ChangeAction::Delete);
    }

    #[tokio::test]
    async fn test_load_by_node_filters_correctly() {
        let repo = make_repo("filter-book");
        let id_target = NodeId::new();
        let id_other = NodeId::new();

        repo.append(&ChangeEntry::new(
            id_target,
            ChangeAction::Create,
            None,
            Some(r#"{"title":"t"}"#.to_string()),
            Timestamp::from_millis(1_000),
        ))
        .await
        .expect("append 1");
        repo.append(&ChangeEntry::new(
            id_other,
            ChangeAction::Update,
            Some(r#"{"title":"o"}"#.to_string()),
            Some(r#"{"title":"o2"}"#.to_string()),
            Timestamp::from_millis(2_000),
        ))
        .await
        .expect("append 2");
        repo.append(&ChangeEntry::new(
            id_target,
            ChangeAction::Update,
            Some(r#"{"title":"t"}"#.to_string()),
            Some(r#"{"title":"t2"}"#.to_string()),
            Timestamp::from_millis(3_000),
        ))
        .await
        .expect("append 3");

        let filtered = repo.load_by_node(id_target).await.expect("load_by_node");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|e| e.node_id == id_target));
    }

    #[tokio::test]
    async fn test_load_all_empty_when_no_entries() {
        let repo = make_repo("empty-book");
        let all = repo.load_all().await.expect("load_all");
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn test_revert_reflected_in_change_entries() {
        let repo = make_repo("restore-book");
        let id = NodeId::new();

        repo.append(&ChangeEntry::new(
            id,
            ChangeAction::Create,
            None,
            Some(r#"{"title":"v1"}"#.to_string()),
            Timestamp::from_millis(1_000),
        ))
        .await
        .expect("append create");
        repo.append(&ChangeEntry::new(
            id,
            ChangeAction::Update,
            Some(r#"{"title":"v1"}"#.to_string()),
            Some(r#"{"title":"v2"}"#.to_string()),
            Timestamp::from_millis(2_000),
        ))
        .await
        .expect("append update");

        // Store::revert 経由で seq 1 (Create 直後) まで巻き戻す。
        repo.revert_to(Seq(1u64)).await.expect("revert");

        let all = repo.load_all().await.expect("load_all after revert");
        assert!(
            all.iter().any(|e| e.action == ChangeAction::Restore),
            "expected a Restore-kind entry after revert, got: {all:?}"
        );
    }
}
