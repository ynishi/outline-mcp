//! Integration tests — BookService, EjectService file I/O, validators.

mod common;

use common::{assert_error_contains, TestBook};

use outline_mcp_core::application::eject::{EjectConfig, EjectFormat, EjectService};
use outline_mcp_core::application::service::BookService;
use outline_mcp_core::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use outline_mcp_core::domain::model::node::NodeType;
use outline_mcp_core::infra::json_store::JsonBookRepository;
use outline_mcp_core::infra::legacy_shelf_store::LegacyShelfStore;

// =============================================================================
// BookService CRUD (with InMemoryRepo)
// =============================================================================

#[tokio::test]
async fn service_create_and_read() {
    let svc = TestBook::service_with_book(&TemplateBook::new("Empty", 4));
    let book = svc.read_tree().await.unwrap();
    assert_eq!(book.title(), "Empty");
    assert_eq!(book.node_count(), 0);
}

#[tokio::test]
async fn service_add_node() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);

    let (id, _warning) = svc
        .add_node(AddNodeRequest {
            parent: None,
            title: "New Section".into(),
            node_type: NodeType::Section,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: std::collections::HashMap::new(),
        })
        .await
        .unwrap();

    let book = svc.read_tree().await.unwrap();
    assert_eq!(book.node_count(), tb.book.node_count() + 1);
    assert_eq!(book.get_node(id).unwrap().title(), "New Section");
}

#[tokio::test]
async fn service_update_node() {
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
            properties: None,
            properties_patch: None,
            status: None,
        },
    )
    .await
    .map(|((), _warning)| ())
    .unwrap();

    let book = svc.read_tree().await.unwrap();
    let node = book.get_node(design_id).unwrap();
    assert_eq!(node.title(), "Architecture");
    assert_eq!(node.body(), Some("Updated body"));
}

/// Changing one property must not cost the others — including a value a caller
/// could not have parsed back out of the text rendering (`, ` and `=` inside
/// it), which is the case that made "read them all and resend" unsafe.
#[tokio::test]
async fn service_update_node_properties_patch_preserves_untouched_keys() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);
    let design_id = tb.ids["design"];

    let seed: std::collections::HashMap<String, String> = [
        ("gate_status", "pending"),
        ("kind", "Pitfall"),
        ("evidence_quote", "note: a, b = c"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    svc.update_node(
        design_id,
        UpdateNodeRequest {
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: Some(seed),
            properties_patch: None,
            status: None,
        },
    )
    .await
    .unwrap();

    svc.update_node(
        design_id,
        UpdateNodeRequest {
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: Some(
                [("gate_status".to_string(), "approved".to_string())]
                    .into_iter()
                    .collect(),
            ),
            status: None,
        },
    )
    .await
    .unwrap();

    // Re-read through the repository so this covers persistence, not just the
    // in-memory aggregate.
    let book = svc.read_tree().await.unwrap();
    let node = book.get_node(design_id).unwrap();
    assert_eq!(node.get_property("gate_status"), Some("approved"));
    assert_eq!(node.get_property("kind"), Some("Pitfall"));
    assert_eq!(node.get_property("evidence_quote"), Some("note: a, b = c"));
    assert_eq!(node.properties().len(), 3);
}

#[tokio::test]
async fn service_update_node_rejects_replace_and_patch_together() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);
    let design_id = tb.ids["design"];

    let result = svc
        .update_node(
            design_id,
            UpdateNodeRequest {
                title: None,
                body: None,
                node_type: None,
                placeholder: None,
                properties: Some(std::collections::HashMap::new()),
                properties_patch: Some([("k".to_string(), "v".to_string())].into_iter().collect()),
                status: None,
            },
        )
        .await;

    assert_error_contains(result, "mutually exclusive");
}

#[tokio::test]
async fn service_move_node() {
    let tb = TestBook::standard();
    let svc = TestBook::service_with_book(&tb.book);
    let code_id = tb.ids["code"];
    let design_id = tb.ids["design"];

    // Implementation配下のcodeをDesign配下に移動
    svc.move_node(code_id, Some(design_id), 0)
        .await
        .map(|((), _warning)| ())
        .unwrap();

    let book = svc.read_tree().await.unwrap();
    let design = book.get_node(design_id).unwrap();
    assert!(design.children().contains(&code_id));
    assert_eq!(book.get_node(code_id).unwrap().parent(), Some(design_id));
}

#[tokio::test]
async fn service_remove_node() {
    let tb = TestBook::standard();
    let original_count = tb.book.node_count();
    let svc = TestBook::service_with_book(&tb.book);

    // Design (+ 2 children) を削除 → 3ノード減
    svc.remove_node(tb.ids["design"])
        .await
        .map(|((), _warning)| ())
        .unwrap();

    let book = svc.read_tree().await.unwrap();
    assert_eq!(book.node_count(), original_count - 3);
}

#[tokio::test]
async fn service_read_nonexistent_book_errors() {
    let repo = common::InMemoryRepo::new();
    let svc = BookService::new(LegacyShelfStore::new(repo), "test");
    let result = svc.read_tree().await;
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

#[tokio::test]
async fn service_json_repo_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.json");

    let repo = JsonBookRepository::new(&path);
    let svc = BookService::new(LegacyShelfStore::new(repo), "book");

    let book = svc.create_book("File Test", 3).await.unwrap();
    assert_eq!(book.title(), "File Test");

    // 新たなServiceインスタンスで読み直す
    let repo2 = JsonBookRepository::new(&path);
    let svc2 = BookService::new(LegacyShelfStore::new(repo2), "book");
    let loaded = svc2.read_tree().await.unwrap();
    assert_eq!(loaded.title(), "File Test");
}

// =============================================================================
// Import max recursion guard
// =============================================================================

#[test]
fn import_rejects_deep_nesting() {
    use outline_mcp_core::application::eject::{EjectTree, EjectTreeNode};

    // 40段のネスト（制限は32）
    let mut node = EjectTreeNode {
        id: "leaf".into(),
        title: "Leaf".into(),
        node_type: "content".into(),
        body: None,
        placeholder: None,
        children: vec![],
        properties: std::collections::HashMap::new(),
    };
    for i in (0..40).rev() {
        node = EjectTreeNode {
            id: format!("level-{i}"),
            title: format!("Level {i}"),
            node_type: "section".into(),
            body: None,
            placeholder: None,
            children: vec![node],
            properties: std::collections::HashMap::new(),
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
