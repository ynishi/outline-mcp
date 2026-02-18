//! Snapshot tests — render_markdown, render_json output regression detection.

mod common;

use common::TestBook;
use insta::{assert_json_snapshot, assert_snapshot};

use outline_mcp::application::eject::{EjectService, EjectTree};

// =============================================================================
// Markdown snapshots
// =============================================================================

#[test]
fn snapshot_markdown_full() {
    let tb = TestBook::standard();
    let md = EjectService::render_markdown(&tb.book, true, None);
    assert_snapshot!("markdown_full", md);
}

#[test]
fn snapshot_markdown_no_placeholders() {
    let tb = TestBook::standard();
    let md = EjectService::render_markdown(&tb.book, false, None);
    assert_snapshot!("markdown_no_placeholders", md);
}

#[test]
fn snapshot_markdown_subtree() {
    let tb = TestBook::standard();
    let md = EjectService::render_markdown(&tb.book, true, Some(tb.ids["design"]));
    assert_snapshot!("markdown_subtree_design", md);
}

// =============================================================================
// JSON snapshots
// =============================================================================

#[test]
fn snapshot_json_full() {
    let tb = TestBook::standard();
    let tree = EjectService::build_tree(&tb.book, None);

    // UUIDを安定化（スナップショット比較のため）
    let stable = stabilize_tree(tree);
    assert_json_snapshot!("json_full", stable);
}

#[test]
fn snapshot_json_subtree() {
    let tb = TestBook::standard();
    let tree = EjectService::build_tree(&tb.book, Some(tb.ids["implementation"]));

    let stable = stabilize_tree(tree);
    assert_json_snapshot!("json_subtree_implementation", stable);
}

// =============================================================================
// Helpers — UUID安定化
// =============================================================================

/// テスト毎にUUIDが変わるため、連番に置換してスナップショット比較を安定させる。
fn stabilize_tree(mut tree: EjectTree) -> EjectTree {
    let mut counter = 0;
    for node in &mut tree.nodes {
        stabilize_node(node, &mut counter);
    }
    tree
}

fn stabilize_node(node: &mut outline_mcp::application::eject::EjectTreeNode, counter: &mut usize) {
    *counter += 1;
    node.id = format!("stable-id-{counter}");
    for child in &mut node.children {
        stabilize_node(child, counter);
    }
}
