//! Shared test harness for integration tests.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;

use outline_mcp::application::service::BookService;
use outline_mcp::domain::model::book::{AddNodeRequest, TemplateBook};
use outline_mcp::domain::model::id::NodeId;
use outline_mcp::domain::model::node::NodeType;
use outline_mcp::domain::repository::BookRepository;

// =============================================================================
// InMemoryBookRepository — テスト用リポジトリ
// =============================================================================

#[derive(Debug, thiserror::Error)]
#[error("in-memory store error")]
pub struct InMemoryError;

/// ファイルI/O不要のインメモリリポジトリ。
pub struct InMemoryRepo {
    store: RefCell<HashMap<String, String>>,
}

impl InMemoryRepo {
    pub fn new() -> Self {
        Self {
            store: RefCell::new(HashMap::new()),
        }
    }
}

impl BookRepository for InMemoryRepo {
    type Error = InMemoryError;

    fn load(&self) -> Result<Option<TemplateBook>, Self::Error> {
        let store = self.store.borrow();
        match store.get("book") {
            Some(json) => {
                let book: TemplateBook = serde_json::from_str(json).unwrap();
                Ok(Some(book))
            }
            None => Ok(None),
        }
    }

    fn save(&self, book: &TemplateBook) -> Result<(), Self::Error> {
        let json = serde_json::to_string(book).unwrap();
        self.store.borrow_mut().insert("book".to_string(), json);
        Ok(())
    }
}

// =============================================================================
// TestBook — 構造化済みテスト用Book作成ヘルパー
// =============================================================================

/// テスト用のBook構造。IDを名前で引ける。
pub struct TestBook {
    pub book: TemplateBook,
    pub ids: HashMap<&'static str, NodeId>,
}

impl TestBook {
    /// 標準的なテスト用Book:
    /// ```text
    /// 1. Design (section)
    ///   1-1. Define requirements (content, placeholder: "requirements list")
    ///   1-2. API design (content, body: "REST endpoints")
    /// 2. Implementation (section)
    ///   2-1. Write code (content)
    ///   2-2. Write tests (content, body: "- unit\n- integration")
    /// ```
    pub fn standard() -> Self {
        let mut book = TemplateBook::new("Test Runbook", 4);
        let mut ids = HashMap::new();

        let design = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Design".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        ids.insert("design", design);

        let req = book
            .add_node(AddNodeRequest {
                parent: Some(design),
                title: "Define requirements".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: Some("requirements list".into()),
                position: usize::MAX,
            })
            .unwrap();
        ids.insert("requirements", req);

        let api = book
            .add_node(AddNodeRequest {
                parent: Some(design),
                title: "API design".into(),
                node_type: NodeType::Content,
                body: Some("REST endpoints".into()),
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        ids.insert("api", api);

        let impl_sec = book
            .add_node(AddNodeRequest {
                parent: None,
                title: "Implementation".into(),
                node_type: NodeType::Section,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        ids.insert("implementation", impl_sec);

        let code = book
            .add_node(AddNodeRequest {
                parent: Some(impl_sec),
                title: "Write code".into(),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        ids.insert("code", code);

        let tests = book
            .add_node(AddNodeRequest {
                parent: Some(impl_sec),
                title: "Write tests".into(),
                node_type: NodeType::Content,
                body: Some("- unit\n- integration".into()),
                placeholder: None,
                position: usize::MAX,
            })
            .unwrap();
        ids.insert("tests", tests);

        Self { book, ids }
    }

    /// InMemoryRepoにBookを保存してBookServiceを返す。
    pub fn service_with_book(book: &TemplateBook) -> BookService<InMemoryRepo> {
        let repo = InMemoryRepo::new();
        repo.save(book).unwrap();
        BookService::new(repo)
    }
}

// =============================================================================
// Assertion helpers
// =============================================================================

/// 結果がErrで、メッセージに指定文字列を含むことをassert。
#[allow(dead_code)]
pub fn assert_error_contains<T: std::fmt::Debug>(
    result: Result<T, impl std::fmt::Display>,
    expected: &str,
) {
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(expected),
                "Expected error containing '{expected}', got: '{msg}'"
            );
        }
        Ok(v) => panic!("Expected error containing '{expected}', got Ok({v:?})"),
    }
}
