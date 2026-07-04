use async_trait::async_trait;

use super::model::book::TemplateBook;
use super::model::changelog::ChangeEntry;
use super::model::id::NodeId;

/// 永続化の抽象。Infra層が実装する。
#[async_trait]
pub trait BookRepository {
    /// Storage-backend-specific error type.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Load the book, or `None` if it has never been created.
    async fn load(&self) -> Result<Option<TemplateBook>, Self::Error>;
    /// Persist the book, overwriting any existing stored state.
    async fn save(&self, book: &TemplateBook) -> Result<(), Self::Error>;
}

/// ChangeLog の永続化抽象。Infra層が実装する。
///
/// - インスタンスは slug 単位で生成される（1インスタンス = 1 slug）
/// - エラー型は `Box<dyn Error + Send + Sync>` を直接使用（trait object化しやすさを優先）
#[async_trait]
pub trait ChangeLogRepository: Send + Sync {
    /// ChangeEntry を changelog に追記する。
    async fn append(
        &self,
        entry: &ChangeEntry,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 全 ChangeEntry を返す。
    async fn load_all(&self) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>>;

    /// 特定ノードの ChangeEntry を返す。
    async fn load_by_node(
        &self,
        node_id: NodeId,
    ) -> Result<Vec<ChangeEntry>, Box<dyn std::error::Error + Send + Sync>>;
}
