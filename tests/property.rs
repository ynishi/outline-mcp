//! Property-based tests — invariant verification with proptest.

mod common;

use common::TestBook;
use proptest::prelude::*;

use outline_mcp::application::eject::EjectService;
use outline_mcp::domain::model::book::{AddNodeRequest, TemplateBook};
use outline_mcp::domain::model::node::NodeType;

// =============================================================================
// is_hierarchical_id は interface::mcp の private関数のため、
// 同等ロジックをここで再実装してテストする。
// =============================================================================

fn is_hierarchical_id(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

proptest! {
    /// 純粋な数字列は常にhierarchical IDとして認識される。
    #[test]
    fn hierarchical_id_accepts_digits(n in 1u32..1000) {
        prop_assert!(is_hierarchical_id(&n.to_string()));
    }

    /// "N-M" 形式は常にhierarchical IDとして認識される。
    #[test]
    fn hierarchical_id_accepts_two_level(
        a in 1u32..100,
        b in 1u32..100,
    ) {
        let id = format!("{a}-{b}");
        prop_assert!(is_hierarchical_id(&id));
    }

    /// UUID v4文字列（ハイフン含むhex）はhierarchical IDではない。
    #[test]
    fn hierarchical_id_rejects_uuid(
        a in "[0-9a-f]{8}",
        b in "[0-9a-f]{4}",
        c in "[0-9a-f]{4}",
        d in "[0-9a-f]{4}",
        e in "[0-9a-f]{12}",
    ) {
        let uuid = format!("{a}-{b}-{c}-{d}-{e}");
        // hex文字(a-f)を含むため、純数字ではない → false
        // (稀に全桁が0-9のみの場合を除く)
        if uuid.chars().any(|c| c.is_ascii_alphabetic()) {
            prop_assert!(!is_hierarchical_id(&uuid));
        }
    }

    /// 空文字列はhierarchical IDではない。
    #[test]
    fn hierarchical_id_rejects_arbitrary_strings(s in "[a-z]{1,20}") {
        prop_assert!(!is_hierarchical_id(&s));
    }
}

// =============================================================================
// TemplateBook invariants
// =============================================================================

proptest! {
    /// add_node → remove_node でnode_countが元に戻る。
    #[test]
    fn add_remove_preserves_count(title in "[A-Za-z ]{1,30}") {
        let tb = TestBook::standard();
        let mut book = tb.book.clone();
        let before = book.node_count();

        let id = book.add_node(AddNodeRequest {
            parent: None,
            title,
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
        }).unwrap();

        prop_assert_eq!(book.node_count(), before + 1);

        book.remove_node(id).unwrap();
        prop_assert_eq!(book.node_count(), before);
    }

    /// depth_of は常に 1 以上。
    #[test]
    fn depth_always_gte_one(title in "[A-Za-z]{1,20}") {
        let mut book = TemplateBook::new("Depth Test", 4);
        let id = book.add_node(AddNodeRequest {
            parent: None,
            title,
            node_type: NodeType::Section,
            body: None,
            placeholder: None,
            position: usize::MAX,
        }).unwrap();

        prop_assert!(book.depth_of(id) >= 1);
    }

    /// all_nodes_dfs の要素数 == node_count。
    #[test]
    fn dfs_count_equals_node_count(n in 1usize..10) {
        let mut book = TemplateBook::new("Count Test", 4);
        for i in 0..n {
            book.add_node(AddNodeRequest {
                parent: None,
                title: format!("Node {i}"),
                node_type: NodeType::Content,
                body: None,
                placeholder: None,
                position: usize::MAX,
            }).unwrap();
        }

        prop_assert_eq!(book.all_nodes_dfs().len(), book.node_count());
    }
}

// =============================================================================
// Markdown render invariants
// =============================================================================

proptest! {
    /// render_markdownの出力は常にBook titleで始まる。
    #[test]
    fn markdown_starts_with_book_title(title in "[A-Za-z ]{1,30}") {
        let book = TemplateBook::new(&title, 4);
        let md = EjectService::render_markdown(&book, true, None);
        let expected = format!("# {}", title);
        prop_assert!(md.starts_with(&expected));
    }

    /// Content nodeは必ず "- [ ]" チェックボックスとしてレンダリングされる。
    #[test]
    fn content_renders_as_checkbox(node_title in "[A-Za-z]{1,20}") {
        let mut book = TemplateBook::new("CB Test", 4);
        book.add_node(AddNodeRequest {
            parent: None,
            title: node_title.clone(),
            node_type: NodeType::Content,
            body: None,
            placeholder: None,
            position: usize::MAX,
        }).unwrap();

        let md = EjectService::render_markdown(&book, true, None);
        let expected = format!("- [ ] {}", node_title);
        prop_assert!(md.contains(&expected));
    }
}
