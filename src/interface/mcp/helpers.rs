use crate::domain::model::book::TemplateBook;
use crate::domain::model::id::NodeId;
use crate::domain::model::node::TemplateNode;

/// Boolean property をタグ表示用に整形する。
pub(super) fn format_property_tags(node: &TemplateNode) -> String {
    let props = node.properties();
    if props.is_empty() {
        return String::new();
    }
    let mut tags: Vec<&str> = props
        .iter()
        .filter(|(_, v)| *v == "true")
        .map(|(k, _)| k.as_str())
        .collect();
    if tags.is_empty() {
        return String::new();
    }
    tags.sort_unstable();
    format!(" [{}]", tags.join(", "))
}

/// Book の全ノードを TOC 形式にフォーマットする。
pub(super) fn format_toc(book: &TemplateBook, nodes: &[&TemplateNode]) -> String {
    let id_map = build_hierarchical_ids(book);
    let mut output = format!("# {} ({} nodes)\n\n", book.title(), book.node_count());
    for node in nodes {
        let depth = book.depth_of(node.id());
        let indent = "  ".repeat(depth.saturating_sub(1) as usize);
        let hier_id = id_map
            .iter()
            .find(|(_, id)| *id == node.id())
            .map(|(num, _)| num.as_str())
            .unwrap_or("?");
        let tags = format_property_tags(node);
        output.push_str(&format!(
            "{}{}. {}{}\n",
            indent,
            hier_id,
            node.title(),
            tags
        ));
    }
    output
}

/// 階層番号かどうか判定（`1`, `2-3`, `1-2-1` 等）
pub(super) fn is_hierarchical_id(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Book全体の (階層番号, NodeId) マッピングをDFS順で構築する。
pub(super) fn build_hierarchical_ids(book: &TemplateBook) -> Vec<(String, NodeId)> {
    let mut result = Vec::new();
    for (i, &root_id) in book.root_nodes().iter().enumerate() {
        let num = format!("{}", i + 1);
        result.push((num.clone(), root_id));
        collect_children_ids(book, root_id, &num, &mut result);
    }
    result
}

fn collect_children_ids(
    book: &TemplateBook,
    parent_id: NodeId,
    parent_num: &str,
    result: &mut Vec<(String, NodeId)>,
) {
    if let Some(node) = book.get_node(parent_id) {
        for (j, &child_id) in node.children().iter().enumerate() {
            let num = format!("{}-{}", parent_num, j + 1);
            result.push((num.clone(), child_id));
            collect_children_ids(book, child_id, &num, result);
        }
    }
}

/// 指定NodeIdの階層番号を逆引きする。
pub(super) fn find_hierarchical_id(book: &TemplateBook, target: NodeId) -> Option<String> {
    build_hierarchical_ids(book)
        .into_iter()
        .find(|(_, id)| *id == target)
        .map(|(num, _)| num)
}
