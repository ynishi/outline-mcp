use outline_mcp_core::domain::model::book::TemplateBook;
use outline_mcp_core::domain::model::id::NodeId;
use outline_mcp_core::domain::model::node::TemplateNode;

/// Boolean property をタグ表示用に整形する。
pub(crate) fn format_property_tags(node: &TemplateNode) -> String {
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
pub(crate) fn format_toc(book: &TemplateBook, nodes: &[&TemplateNode]) -> String {
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
pub(crate) fn is_hierarchical_id(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Book全体の (階層番号, NodeId) マッピングをDFS順で構築する。
pub(crate) fn build_hierarchical_ids(book: &TemplateBook) -> Vec<(String, NodeId)> {
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
pub(crate) fn find_hierarchical_id(book: &TemplateBook, target: NodeId) -> Option<String> {
    build_hierarchical_ids(book)
        .into_iter()
        .find(|(_, id)| *id == target)
        .map(|(num, _)| num)
}

/// The changed fields of a `book_update` history entry, from its
/// `{"title":…,"max_depth":…}` before / after JSON: e.g.
/// `title "A" → "B", max_depth 3 → 4`. Unchanged fields are left out.
pub(crate) fn describe_book_update(before: Option<&str>, after: Option<&str>) -> String {
    let parse = |s: Option<&str>| {
        s.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let (b, a) = (parse(before), parse(after));
    let mut parts = Vec::new();
    if b["title"] != a["title"] {
        parts.push(format!("title {} → {}", b["title"], a["title"]));
    }
    if b["max_depth"] != a["max_depth"] {
        parts.push(format!("max_depth {} → {}", b["max_depth"], a["max_depth"]));
    }
    if parts.is_empty() {
        "no change".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod book_update_tests {
    use super::describe_book_update;

    #[test]
    fn lists_only_changed_fields() {
        let before = r#"{"title":"A","max_depth":3}"#;
        assert_eq!(
            describe_book_update(Some(before), Some(r#"{"title":"B","max_depth":4}"#)),
            r#"title "A" → "B", max_depth 3 → 4"#
        );
        assert_eq!(
            describe_book_update(Some(before), Some(r#"{"title":"A","max_depth":5}"#)),
            "max_depth 3 → 5"
        );
    }
}
