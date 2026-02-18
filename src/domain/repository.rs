use super::model::book::TemplateBook;

/// 永続化の抽象。Infra層が実装する。
pub trait BookRepository {
    type Error: std::error::Error + Send + Sync + 'static;

    fn load(&self) -> Result<Option<TemplateBook>, Self::Error>;
    fn save(&self, book: &TemplateBook) -> Result<(), Self::Error>;
}
