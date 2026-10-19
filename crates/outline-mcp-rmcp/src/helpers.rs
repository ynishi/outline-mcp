use outline_mcp_core::domain::model::book::TemplateBook;
use outline_mcp_core::domain::model::changelog::NodeStatus;
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
        let archived = if node.status() == NodeStatus::Archived {
            " [archived]"
        } else {
            ""
        };
        output.push_str(&format!(
            "{}{}. {}{}{}\n",
            indent,
            hier_id,
            node.title(),
            archived,
            tags
        ));
    }
    output
}

/// The toc of `nodes` limited to `max_depth` (from the book root) and
/// `limit` nodes, with a closing line counting what was left out.
pub(crate) fn format_toc_limited(
    book: &TemplateBook,
    nodes: &[&TemplateNode],
    max_depth: Option<u8>,
    limit: Option<usize>,
) -> String {
    let within_depth: Vec<&TemplateNode> = nodes
        .iter()
        .copied()
        .filter(|n| max_depth.is_none_or(|d| book.depth_of(n.id()) <= d))
        .collect();
    let shown: Vec<&TemplateNode> = match limit {
        Some(l) => within_depth.iter().copied().take(l).collect(),
        None => within_depth,
    };
    let hidden = nodes.len() - shown.len();
    let mut out = format_toc(book, &shown);
    if hidden > 0 {
        out.push_str(&format!(
            "… {hidden} more nodes not shown (raise max_depth / limit)\n"
        ));
    }
    out
}

/// How many archived nodes (archived, or under an archived section) the
/// toc view would have shown had archived nodes been included: only nodes
/// within `max_depth` and among the first `limit` of those count, so the
/// number never includes nodes the view would not show anyway.
pub(crate) fn archived_hidden_count(
    book: &TemplateBook,
    nodes: &[&TemplateNode],
    max_depth: Option<u8>,
    limit: Option<usize>,
) -> usize {
    nodes
        .iter()
        .filter(|n| max_depth.is_none_or(|d| book.depth_of(n.id()) <= d))
        .take(limit.unwrap_or(usize::MAX))
        .filter(|n| book.is_archived_or_under_archived(n.id()))
        .count()
}

/// The closing line naming hidden archived nodes, or an empty string.
pub(crate) fn archived_hidden_line(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!("… {count} archived nodes hidden (include_archived: true to show)\n")
    }
}

/// ` (id: <uuid>)` — appended to result lines so the id can go straight into
/// the UUID-only batch tools.
pub(crate) fn id_suffix(id: NodeId) -> String {
    format!(" (id: {id})")
}

/// `2-1. Title [archived] (id: <uuid>)` for a link endpoint, or
/// `(not in the book) (id: <uuid>)` when the node no longer exists.
pub(crate) fn describe_endpoint(book: &TemplateBook, id: NodeId) -> String {
    match book.get_node(id) {
        Some(n) => {
            let hier = find_hierarchical_id(book, id).unwrap_or_else(|| id.short().to_string());
            let archived = if book.is_archived_or_under_archived(id) {
                " [archived]"
            } else {
                ""
            };
            format!("{hier}. {}{archived}{}", n.title(), id_suffix(id))
        }
        None => format!("(not in the book){}", id_suffix(id)),
    }
}

/// A link endpoint's status as listings show it: `"archived"` for an archived
/// node or one under an archived section (as [`describe_endpoint`] marks it),
/// `"missing"` when the node is not in the book, the node's own status
/// otherwise.
pub(crate) fn endpoint_status(book: &TemplateBook, id: NodeId) -> &'static str {
    match book.get_node(id) {
        None => "missing",
        Some(_) if book.is_archived_or_under_archived(id) => "archived",
        Some(n) => n.status().as_str(),
    }
}

/// One compact line listing a node's outgoing links, `rel → title`, for
/// listings; empty when the node has none. Archived targets are marked
/// `[archived]`, like the `links` tool does.
pub(crate) fn links_line(book: &TemplateBook, node: &TemplateNode) -> String {
    if node.links().is_empty() {
        return String::new();
    }
    let parts: Vec<String> = node
        .links()
        .iter()
        .map(|l| {
            let title = book
                .get_node(l.target())
                .map(|t| t.title().to_string())
                .unwrap_or_else(|| "(not in the book)".to_string());
            let archived = if endpoint_status(book, l.target()) == "archived" {
                " [archived]"
            } else {
                ""
            };
            format!(
                "{} → {}{} ({})",
                l.rel(),
                title,
                archived,
                l.target().short()
            )
        })
        .collect();
    format!("Links: {}", parts.join("; "))
}

/// `Dropped N link(s) pointing at the removed nodes, from: a, b\n` — the
/// links that removing `id` (with its descendants) takes away from the nodes
/// that remain — or an empty string when there are none.
pub(crate) fn dropped_links_line(book: &TemplateBook, id: NodeId) -> String {
    let removed: std::collections::HashSet<NodeId> =
        std::iter::once(id).chain(book.descendant_ids(id)).collect();
    let mut count = 0;
    let mut holders = Vec::new();
    for n in book.all_nodes_dfs() {
        if removed.contains(&n.id()) {
            continue;
        }
        let k = n
            .links()
            .iter()
            .filter(|l| removed.contains(&l.target()))
            .count();
        if k > 0 {
            count += k;
            holders.push(n.title().to_string());
        }
    }
    if count == 0 {
        return String::new();
    }
    let shown: Vec<_> = holders
        .iter()
        .take(DESCENDANT_TITLES_SHOWN)
        .cloned()
        .collect();
    let more = holders.len().saturating_sub(shown.len());
    format!(
        "Dropped {count} link(s) pointing at the removed nodes, from: {}{}\n",
        shown.join(", "),
        if more > 0 {
            format!(", … {more} more")
        } else {
            String::new()
        }
    )
}

/// At most this many descendant titles are listed in a result line.
const DESCENDANT_TITLES_SHOWN: usize = 10;

/// ` <connector> N descendants: a, b, …` (e.g. ` with 2 descendants: a, b`)
/// for the descendants of `id` in `book` (titles capped at ten), or an empty
/// string when it has none. Result lines put this before [`id_suffix`], so
/// ` (id: <uuid>)` stays the last element of the line.
pub(crate) fn describe_descendants(book: &TemplateBook, id: NodeId, connector: &str) -> String {
    let desc = book.descendant_ids(id);
    if desc.is_empty() {
        return String::new();
    }
    let titles: Vec<&str> = desc
        .iter()
        .take(DESCENDANT_TITLES_SHOWN)
        .filter_map(|d| book.get_node(*d).map(|n| n.title()))
        .collect();
    let more = desc.len().saturating_sub(DESCENDANT_TITLES_SHOWN);
    let noun = if desc.len() == 1 {
        "descendant"
    } else {
        "descendants"
    };
    let mut s = format!(" {connector} {} {noun}: {}", desc.len(), titles.join(", "));
    if more > 0 {
        s.push_str(&format!(", … (+{more} more)"));
    }
    s
}

/// One line per node of the subtree rooted at `id` — the node and each
/// descendant — as `old → new` hierarchical positions between two versions of
/// the book. A node missing from `after` is shown as removed.
pub(crate) fn describe_subtree_change(
    before: &TemplateBook,
    after: &TemplateBook,
    id: NodeId,
) -> String {
    describe_subtrees_change(before, after, &[id])
}

/// [`describe_subtree_change`] for several subtree roots at once (a batch):
/// every node of every subtree is listed once, in first-seen order, even when
/// one root is a descendant of another. Positions come from `after`, the
/// book with every operation applied in order, so they match the real result.
pub(crate) fn describe_subtrees_change(
    before: &TemplateBook,
    after: &TemplateBook,
    roots: &[NodeId],
) -> String {
    let before_ids = build_hierarchical_ids(before);
    let after_ids = build_hierarchical_ids(after);
    let pos = |ids: &[(String, NodeId)], n: NodeId| {
        ids.iter().find(|(_, x)| *x == n).map(|(h, _)| h.clone())
    };
    let mut seen = std::collections::HashSet::new();
    let nodes: Vec<NodeId> = roots
        .iter()
        .flat_map(|&r| std::iter::once(r).chain(before.descendant_ids(r)))
        .filter(|n| seen.insert(*n))
        .collect();
    let mut out = String::new();
    for n in nodes {
        let title = before.get_node(n).map(|x| x.title()).unwrap_or("?");
        let old = pos(&before_ids, n).unwrap_or_else(|| "?".into());
        let line = match pos(&after_ids, n) {
            Some(new) => format!("  {old}. {title} → {new}{}\n", id_suffix(n)),
            None => format!("  {old}. {title} → removed{}\n", id_suffix(n)),
        };
        out.push_str(&line);
    }
    out
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
/// `{"title":…,"max_depth":…,"archived":…}` before / after JSON: e.g.
/// `title "A" → "B", max_depth 3 → 4, archived`. Unchanged fields are left out.
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
    // Entries written before books could be archived have no `archived`.
    let archived = |v: &serde_json::Value| v["archived"].as_bool().unwrap_or(false);
    if archived(&b) != archived(&a) {
        parts.push(
            if archived(&a) {
                "archived"
            } else {
                "unarchived"
            }
            .to_string(),
        );
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
        assert_eq!(
            describe_book_update(
                Some(before),
                Some(r#"{"title":"A","max_depth":3,"archived":true}"#)
            ),
            "archived"
        );
    }
}
