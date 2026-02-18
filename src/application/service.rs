use crate::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use crate::domain::model::id::NodeId;
use crate::domain::repository::BookRepository;

use super::error::AppError;

/// Template Bookに対するユースケース。
/// load → mutate → save のパターンで操作する。
pub struct BookService<R: BookRepository> {
    repo: R,
}

impl<R: BookRepository> BookService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
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
    pub fn add_node(&self, req: AddNodeRequest) -> Result<NodeId, AppError> {
        let mut book = self.load_book()?;
        let id = book.add_node(req)?;
        self.persist(&book)?;
        Ok(id)
    }

    /// ノードを更新する。
    pub fn update_node(&self, id: NodeId, req: UpdateNodeRequest) -> Result<(), AppError> {
        let mut book = self.load_book()?;
        book.update_node(id, req)?;
        self.persist(&book)?;
        Ok(())
    }

    /// ノードを移動する。
    pub fn move_node(
        &self,
        id: NodeId,
        new_parent: Option<NodeId>,
        position: usize,
    ) -> Result<(), AppError> {
        let mut book = self.load_book()?;
        book.move_node(id, new_parent, position)?;
        self.persist(&book)?;
        Ok(())
    }

    /// ノードを削除する（子孫ごと）。
    pub fn remove_node(&self, id: NodeId) -> Result<(), AppError> {
        let mut book = self.load_book()?;
        book.remove_node(id)?;
        self.persist(&book)?;
        Ok(())
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
}
