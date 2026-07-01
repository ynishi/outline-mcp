//! MCP Resources for outline-mcp.
//!
//! Guides are baked into the binary at compile time via `include_str!`
//! so that `outline://guides/<slug>` returns the exact markdown that
//! ships with the crate. Sources live under `docs/guides/` at this
//! crate's root.

use rmcp::model::{
    ListResourcesResult, RawResource, ReadResourceResult, Resource, ResourceContents,
};

/// A single guide bundled with the crate.
struct Guide {
    uri: &'static str,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    body: &'static str,
}

const GUIDES: &[Guide] = &[Guide {
    uri: "outline://guides/snapshot-workflow",
    name: "snapshot-workflow",
    title: "Snapshot Workflow — Versioning a Book",
    description: "How to version a book with snapshot_create / snapshot_tag / snapshot_diff / snapshot_dump / snapshot_dump_all, and how to see raw edit history with book_history.",
    body: include_str!("../docs/guides/snapshot-workflow.md"),
}];

/// Return the list of bundled guide resources.
pub(crate) fn list_all() -> ListResourcesResult {
    use rmcp::model::Annotated;
    let resources: Vec<Resource> = GUIDES
        .iter()
        .map(|g| {
            let raw = RawResource {
                uri: g.uri.to_string(),
                name: g.name.to_string(),
                title: Some(g.title.to_string()),
                description: Some(g.description.to_string()),
                mime_type: Some("text/markdown".to_string()),
                size: Some(g.body.len() as u32),
                icons: None,
                meta: None,
            };
            Annotated {
                raw,
                annotations: None,
            }
        })
        .collect();

    ListResourcesResult::with_all_items(resources)
}

/// Read a guide by URI. Returns None if no bundled guide matches.
pub(crate) fn read(uri: &str) -> Option<ReadResourceResult> {
    let guide = GUIDES.iter().find(|g| g.uri == uri)?;
    Some(ReadResourceResult {
        contents: vec![ResourceContents::TextResourceContents {
            uri: guide.uri.to_string(),
            mime_type: Some("text/markdown".to_string()),
            text: guide.body.to_string(),
            meta: None,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_returns_bundled_guides() {
        let result = list_all();
        assert!(!result.resources.is_empty());
        let snapshot_guide = result
            .resources
            .iter()
            .find(|r| r.raw.uri == "outline://guides/snapshot-workflow")
            .expect("snapshot-workflow guide should be listed");
        assert_eq!(
            snapshot_guide.raw.mime_type.as_deref(),
            Some("text/markdown")
        );
        assert!(snapshot_guide.raw.size.unwrap_or(0) > 0);
    }

    #[test]
    fn read_returns_body_for_known_uri() {
        let result =
            read("outline://guides/snapshot-workflow").expect("known URI should return contents");
        assert_eq!(result.contents.len(), 1);
        let ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } = &result.contents[0]
        else {
            panic!("expected text contents");
        };
        assert_eq!(uri, "outline://guides/snapshot-workflow");
        assert_eq!(mime_type.as_deref(), Some("text/markdown"));
        assert!(
            text.contains("Snapshot Workflow"),
            "body should contain guide title"
        );
    }

    #[test]
    fn read_returns_none_for_unknown_uri() {
        assert!(read("outline://guides/does-not-exist").is_none());
        assert!(read("http://example.com/foo").is_none());
    }
}
