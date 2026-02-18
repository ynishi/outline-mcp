//! Integration tests — BookService, EjectService file I/O, validators.

mod common;

use common::{assert_error_contains, TestBook};

use outline_mcp::application::eject::{EjectConfig, EjectFormat, EjectService};
use outline_mcp::application::service::BookService;
use outline_mcp::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use outline_mcp::domain::model::node::NodeType;
use outline_mcp::infra::json_store::JsonBookRepository;

// =============================================================================
// BookService CRUD (with InMemoryRepo)
// =============================================================================

#[test]
fn service_create_and_read() {
    let svc = TestBook::service_with_book(&TemplateBook::new("Empty", 4));
    let book = svc.read_tree().unwrap();
    assert_eq!(book.title(), "Empty");
    assert_eq!(book.node_count(), 0);
}

#[test]
fn service_add_node() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);

    let id = svc
        .add_node(AddNodeRequest {
            parent: None,
            title: "New Section".into(),
            node_type: NodeType::Section,
            body: None,
            placeholder: None,
            position: usize::MAX,
        })
        .unwrap();

    let book = svc.read_tree().unwrap();
    assert_eq!(book.node_count(), tb.book.node_count() + 1);
    assert_eq!(book.get_node(id).unwrap().title(), "New Section");
}

#[test]
fn service_update_node() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);
    let design_id = tb.ids["design"];

    svc.update_node(
        design_id,
        UpdateNodeRequest {
            title: Some("Architecture".into()),
            body: Some(Some("Updated body".into())),
            node_type: None,
            placeholder: None,
        },
    )
    .unwrap();

    let book = svc.read_tree().unwrap();
    let node = book.get_node(design_id).unwrap();
    assert_eq!(node.title(), "Architecture");
    assert_eq!(node.body(), Some("Updated body"));
}

#[test]
fn service_move_node() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);
    let code_id = tb.ids["code"];
    let design_id = tb.ids["design"];

    // Implementation配下のcodeをDesign配下に移動
    svc.move_node(code_id, Some(design_id), 0).unwrap();

    let book = svc.read_tree().unwrap();
    let design = book.get_node(design_id).unwrap();
    assert!(design.children().contains(&code_id));
    assert_eq!(book.get_node(code_id).unwrap().parent(), Some(design_id));
}

#[test]
fn service_remove_node() {
    let tb = TestBook::standard();
    let original_count = tb.book.node_count();
    let svc = TestBook::service_with_book(&tb.book);

    // Design (+ 2 children) を削除 → 3ノード減
    svc.remove_node(tb.ids["design"]).unwrap();

    let book = svc.read_tree().unwrap();
    assert_eq!(book.node_count(), original_count - 3);
}

#[test]
fn service_read_nonexistent_book_errors() {
    let repo = common::InMemoryRepo::new();
    let svc = BookService::new(repo);
    let result = svc.read_tree();
    assert_error_contains(result, "book not found");
}

// =============================================================================
// EjectService file I/O
// =============================================================================

#[test]
fn eject_writes_markdown_file() {
    let tb = TestBook::standard();
    let dir = tempfile::tempdir().unwrap();

    let config = EjectConfig {
        output_dir: dir.path().to_path_buf(),
        filename: "test_output.md".to_string(),
        include_placeholders: true,
        format: EjectFormat::Markdown,
        subtree_root: None,
    };

    let path = EjectService::eject(&tb.book, &config).unwrap();
    assert!(path.exists());

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("# Test Runbook"));
    assert!(content.contains("- [ ] Define requirements"));
}

#[test]
fn eject_writes_json_file() {
    let tb = TestBook::standard();
    let dir = tempfile::tempdir().unwrap();

    let config = EjectConfig {
        output_dir: dir.path().to_path_buf(),
        filename: "test_output.json".to_string(),
        include_placeholders: true,
        format: EjectFormat::Json,
        subtree_root: None,
    };

    let path = EjectService::eject(&tb.book, &config).unwrap();
    assert!(path.exists());

    let content = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["title"], "Test Runbook");
}

#[test]
fn eject_subtree_only() {
    let tb = TestBook::standard();
    let dir = tempfile::tempdir().unwrap();

    let config = EjectConfig {
        output_dir: dir.path().to_path_buf(),
        filename: "subtree.md".to_string(),
        include_placeholders: true,
        format: EjectFormat::Markdown,
        subtree_root: Some(tb.ids["design"]),
    };

    let path = EjectService::eject(&tb.book, &config).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();

    assert!(content.contains("# Design"));
    assert!(content.contains("- [ ] Define requirements"));
    assert!(!content.contains("Implementation"));
}

// =============================================================================
// BookService with JsonBookRepository (file-backed)
// =============================================================================

#[test]
fn service_json_repo_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.json");

    let repo = JsonBookRepository::new(&path);
    let svc = BookService::new(repo);

    let book = svc.create_book("File Test", 3).unwrap();
    assert_eq!(book.title(), "File Test");

    // 新たなServiceインスタンスで読み直す
    let repo2 = JsonBookRepository::new(&path);
    let svc2 = BookService::new(repo2);
    let loaded = svc2.read_tree().unwrap();
    assert_eq!(loaded.title(), "File Test");
}

// =============================================================================
// Import max recursion guard
// =============================================================================

#[test]
fn import_rejects_deep_nesting() {
    use outline_mcp::application::eject::{EjectTree, EjectTreeNode};

    // 40段のネスト（制限は32）
    let mut node = EjectTreeNode {
        id: "leaf".into(),
        title: "Leaf".into(),
        node_type: "content".into(),
        body: None,
        placeholder: None,
        children: vec![],
    };
    for i in (0..40).rev() {
        node = EjectTreeNode {
            id: format!("level-{i}"),
            title: format!("Level {i}"),
            node_type: "section".into(),
            body: None,
            placeholder: None,
            children: vec![node],
        };
    }

    let tree = EjectTree {
        title: "Deep".into(),
        max_depth: 50, // Bookのmax_depthは広くてもimportの再帰制限で弾く
        nodes: vec![node],
    };

    let result = EjectService::import_tree(&tree);
    assert!(result.is_err());
}
