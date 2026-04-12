use std::collections::HashMap;
use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::model::changelog::NodeStatus;
use crate::domain::model::id::NodeId;
use crate::domain::model::node::NodeType;

// =============================================================================
// Validation helpers
// =============================================================================

/// slugが安全なファイル名であることを検証する。
pub(super) fn validate_slug(slug: &str) -> Result<(), McpError> {
    if slug.is_empty() {
        return Err(McpError::invalid_params("slug must not be empty", None));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(McpError::invalid_params(
            "slug must contain only alphanumeric characters, hyphens, and underscores",
            None,
        ));
    }
    Ok(())
}

/// タイトルをファイル名に安全な文字列に変換する。
/// 英数字・`-_.()`以外を`_`に置換し、連続`_`を圧縮、先頭末尾の`_`を除去する。
pub(super) fn sanitize_for_filename(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();

    // 連続`_`を1つに圧縮
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_underscore = true; // true開始で先頭`_`を除去
    for c in sanitized.chars() {
        if c == '_' {
            if !prev_underscore {
                result.push('_');
            }
            prev_underscore = true;
        } else {
            result.push(c);
            prev_underscore = false;
        }
    }

    // 末尾`_`を除去
    while result.ends_with('_') {
        result.pop();
    }

    // `..`をpath traversal防止のため`_`に置換
    while result.contains("..") {
        result = result.replace("..", "_");
    }

    if result.is_empty() {
        "untitled".to_string()
    } else {
        result
    }
}

/// filenameにパス区切り文字や".."が含まれていないことを検証する。
pub(super) fn validate_filename(filename: &str) -> Result<(), McpError> {
    if filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.is_empty()
    {
        return Err(McpError::invalid_params(
            "filename must not contain path separators, '..', or be empty",
            None,
        ));
    }
    Ok(())
}

/// importパスの拡張子を検証する。
pub(super) fn validate_import_path(file_path: &str) -> Result<PathBuf, McpError> {
    let path = PathBuf::from(file_path);
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => Ok(path),
        _ => Err(McpError::invalid_params(
            "Only .json files can be imported",
            None,
        )),
    }
}

pub(super) fn parse_node_type(s: &str) -> Result<NodeType, McpError> {
    match s {
        "section" => Ok(NodeType::Section),
        "content" => Ok(NodeType::Content),
        other => Err(McpError::invalid_params(
            format!("Unknown node_type: '{other}'. Use: section, content"),
            None,
        )),
    }
}

pub(super) fn parse_node_status(s: &str) -> Result<NodeStatus, McpError> {
    match s {
        "active" => Ok(NodeStatus::Active),
        "draft" => Ok(NodeStatus::Draft),
        other => Err(McpError::invalid_params(
            format!("Unknown status: '{other}'. Use: active, draft"),
            None,
        )),
    }
}

/// MCP経由のテキストに含まれるリテラル `\n` を実際の改行に変換する。
pub(super) fn unescape_newlines(s: &str) -> String {
    s.replace("\\n", "\n")
}

pub(super) fn normalize_text(s: Option<String>) -> Option<String> {
    s.map(|v| unescape_newlines(&v))
}

pub(super) fn parse_node_id(s: &str) -> Result<NodeId, McpError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| McpError::invalid_params(format!("Invalid node_id: '{s}'"), None))
}

// =============================================================================
// Request types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpNodeCreateRequest {
    #[schemars(
        description = "Parent ID from `toc` output (e.g. '1', '2-3'). Omit for root-level node. UUID also accepted."
    )]
    pub parent: Option<String>,
    #[schemars(description = "Node title (required)")]
    pub title: String,
    #[schemars(description = "Node type: section or content")]
    pub node_type: String,
    #[schemars(description = "Optional markdown body content")]
    pub body: Option<String>,
    #[schemars(
        description = "Optional placeholder hint for checklist export (e.g. 'write test cases here')"
    )]
    pub placeholder: Option<String>,
    #[schemars(description = "Position among siblings (0-based). Omit to append at end.")]
    pub position: Option<usize>,
    #[schemars(
        description = "Optional key-value properties (e.g. {\"inject\": \"true\", \"scope\": \"rust\"})"
    )]
    pub properties: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpNodeUpdateRequest {
    #[schemars(description = "Node ID from `toc` output (e.g. '2-3'). UUID also accepted.")]
    pub node_id: String,
    #[schemars(description = "New title (omit to keep current)")]
    pub title: Option<String>,
    #[schemars(description = "New body (null to clear, omit to keep current)")]
    pub body: Option<Option<String>>,
    #[schemars(description = "New node type: section or content")]
    pub node_type: Option<String>,
    #[schemars(description = "New placeholder hint (null to clear)")]
    pub placeholder: Option<Option<String>>,
    #[schemars(description = "Replace all properties (omit to keep current). Pass {} to clear.")]
    pub properties: Option<HashMap<String, String>>,
    #[schemars(
        description = "Node status: 'active' or 'draft'. Draft nodes are excluded from select_book inject."
    )]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpNodeMoveRequest {
    #[schemars(description = "Node ID from `toc` output (e.g. '2-3'). UUID also accepted.")]
    pub node_id: String,
    #[schemars(description = "Action: 'move' to relocate, 'remove' to delete (with descendants)")]
    pub action: String,
    #[schemars(
        description = "New parent ID from `toc` output (null for root). Required for 'move' action."
    )]
    pub new_parent: Option<String>,
    #[schemars(description = "Position among new siblings (0-based). Default: append at end.")]
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpTocRequest {
    #[schemars(description = "Section ID from `toc` output (e.g. '2'). Omit to show entire book.")]
    pub subtree_root: Option<String>,
    #[schemars(
        description = "Filter by properties (e.g. {\"inject\": \"true\"}). Only matching nodes shown."
    )]
    pub filter: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpEjectRequest {
    #[schemars(description = "Output directory path (default: current directory)")]
    pub output_dir: Option<String>,
    #[schemars(description = "Output filename (default: '<book-title>.md')")]
    pub filename: Option<String>,
    #[schemars(description = "Include placeholder hints as fill-in fields (default: true)")]
    pub include_placeholders: Option<bool>,
    #[schemars(description = "Output format: 'markdown' (default) or 'json' (tree-structured)")]
    pub format: Option<String>,
    #[schemars(
        description = "Section ID from `toc` output (e.g. '2'). Omit to export entire book."
    )]
    pub subtree_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpImportRequest {
    #[schemars(description = "Path to JSON file exported by eject (format: json)")]
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpInitRequest {
    #[schemars(description = "Book title")]
    pub title: String,
    #[schemars(
        description = "Book slug for filename (e.g. 'rust', 'development'). Alphanumeric, hyphens, underscores only."
    )]
    pub slug: String,
    #[schemars(description = "Maximum tree depth (default: 4, recommended: 3-4)")]
    pub max_depth: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpShelfRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpGenRoutingRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpSnapshotCreateRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpSnapshotListRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpSnapshotRestoreRequest {
    #[schemars(description = "Timestamp (millis) from snapshot_list output")]
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpNodeHistoryRequest {
    #[schemars(description = "Node ID from `toc` output (e.g. '2-3'). UUID also accepted.")]
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpDumpRequest {
    #[schemars(description = "Output directory path")]
    pub output_dir: String,
    #[schemars(description = "Output format: 'markdown' (default) or 'json'")]
    pub format: Option<String>,
    #[schemars(description = "Output filename (default: '<book-title>.<ext>')")]
    pub filename: Option<String>,
}

// =============================================================================
// Batch operation request types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpBatchMoveItem {
    #[schemars(description = "Node UUID (use node_query or dump to find UUIDs)")]
    pub node_id: String,
    #[schemars(description = "New parent UUID (null for root)")]
    pub new_parent: Option<String>,
    #[schemars(description = "Position among new siblings (0-based). Default: append at end.")]
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpBatchMoveRequest {
    #[schemars(description = "List of move operations. All nodes are identified by UUID.")]
    pub moves: Vec<McpBatchMoveItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpBatchUpdateItem {
    #[schemars(description = "Node UUID")]
    pub node_id: String,
    #[schemars(description = "New title (omit to keep current)")]
    pub title: Option<String>,
    #[schemars(description = "New body (null to clear, omit to keep current)")]
    pub body: Option<Option<String>>,
    #[schemars(description = "Replace all properties (omit to keep current). Pass {} to clear.")]
    pub properties: Option<HashMap<String, String>>,
    #[schemars(description = "Node status: 'active' or 'draft'")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpBatchUpdateRequest {
    #[schemars(description = "List of update operations. All nodes are identified by UUID.")]
    pub updates: Vec<McpBatchUpdateItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub(super) struct McpSelectBookRequest {
    #[schemars(
        description = "Book to select: number from `shelf` output (e.g. '1') or book slug (e.g. 'rust')"
    )]
    pub book: String,

    #[schemars(description = "Suppress TOC output (default: false)")]
    #[serde(default)]
    pub quiet: bool,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_node_type_valid() {
        assert_eq!(parse_node_type("section").unwrap(), NodeType::Section);
        assert_eq!(parse_node_type("content").unwrap(), NodeType::Content);
    }

    #[test]
    fn parse_node_type_invalid() {
        assert!(parse_node_type("unknown").is_err());
    }

    #[test]
    fn init_request_with_slug() {
        let req: McpInitRequest =
            serde_json::from_str(r#"{"title": "Test", "slug": "test"}"#).unwrap();
        assert_eq!(req.title, "Test");
        assert_eq!(req.slug, "test");
        assert!(req.max_depth.is_none());
    }

    #[test]
    fn validate_slug_valid() {
        assert!(validate_slug("rust").is_ok());
        assert!(validate_slug("my-book").is_ok());
        assert!(validate_slug("dev_standards").is_ok());
        assert!(validate_slug("book123").is_ok());
    }

    #[test]
    fn validate_slug_invalid() {
        assert!(validate_slug("").is_err());
        assert!(validate_slug("has space").is_err());
        assert!(validate_slug("path/traversal").is_err());
        assert!(validate_slug("dot..dot").is_err());
        assert!(validate_slug("日本語").is_err());
    }

    #[test]
    fn shelf_request_empty() {
        let _req: McpShelfRequest = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn select_book_request() {
        let req: McpSelectBookRequest = serde_json::from_str(r#"{"book": "rust"}"#).unwrap();
        assert_eq!(req.book, "rust");
        assert!(!req.quiet);
    }

    #[test]
    fn select_book_request_quiet() {
        let req: McpSelectBookRequest =
            serde_json::from_str(r#"{"book": "rust", "quiet": true}"#).unwrap();
        assert_eq!(req.book, "rust");
        assert!(req.quiet);
    }

    #[test]
    fn node_create_request_minimal() {
        let req: McpNodeCreateRequest =
            serde_json::from_str(r#"{"title": "Step 1", "node_type": "content"}"#).unwrap();
        assert_eq!(req.title, "Step 1");
        assert!(req.parent.is_none());
        assert!(req.body.is_none());
    }

    #[test]
    fn node_move_request_remove() {
        let req: McpNodeMoveRequest = serde_json::from_str(
            r#"{"node_id": "00000000-0000-0000-0000-000000000001", "action": "remove"}"#,
        )
        .unwrap();
        assert_eq!(req.action, "remove");
        assert!(req.new_parent.is_none());
    }

    #[test]
    fn eject_request_defaults() {
        let req: McpEjectRequest = serde_json::from_str("{}").unwrap();
        assert!(req.output_dir.is_none());
        assert!(req.filename.is_none());
        assert!(req.include_placeholders.is_none());
        assert!(req.format.is_none());
        assert!(req.subtree_root.is_none());
    }

    #[test]
    fn import_request_parse() {
        let req: McpImportRequest =
            serde_json::from_str(r#"{"file_path": "/tmp/book.json"}"#).unwrap();
        assert_eq!(req.file_path, "/tmp/book.json");
    }

    // ---- sanitize_for_filename tests ----

    #[test]
    fn sanitize_basic_title() {
        assert_eq!(sanitize_for_filename("Hello World"), "Hello_World");
    }

    #[test]
    fn sanitize_slash_in_title() {
        assert_eq!(sanitize_for_filename("TCP/IP Basics"), "TCP_IP_Basics");
        assert_eq!(sanitize_for_filename("I/O Operations"), "I_O_Operations");
    }

    #[test]
    fn sanitize_backslash() {
        assert_eq!(sanitize_for_filename("Windows\\Linux"), "Windows_Linux");
    }

    #[test]
    fn sanitize_preserves_allowed_chars() {
        assert_eq!(
            sanitize_for_filename("Review (Google EP)"),
            "Review_(Google_EP)"
        );
        assert_eq!(sanitize_for_filename("v2.0-beta_1"), "v2.0-beta_1");
    }

    #[test]
    fn sanitize_collapses_consecutive_underscores() {
        assert_eq!(sanitize_for_filename("a / b / c"), "a_b_c");
        assert_eq!(sanitize_for_filename("foo   bar"), "foo_bar");
    }

    #[test]
    fn sanitize_strips_leading_trailing_underscores() {
        assert_eq!(sanitize_for_filename(" leading"), "leading");
        assert_eq!(sanitize_for_filename("trailing "), "trailing");
        assert_eq!(sanitize_for_filename(" both "), "both");
    }

    #[test]
    fn sanitize_double_dot_prevention() {
        assert_eq!(sanitize_for_filename("Config..Settings"), "Config_Settings");
        assert_eq!(sanitize_for_filename("a...b"), "a_.b");
    }

    #[test]
    fn sanitize_special_characters() {
        assert_eq!(sanitize_for_filename("file: name?"), "file_name");
        assert_eq!(sanitize_for_filename("a<b>c|d\"e"), "a_b_c_d_e");
    }

    #[test]
    fn sanitize_unicode() {
        assert_eq!(sanitize_for_filename("日本語タイトル"), "untitled");
        assert_eq!(sanitize_for_filename("混合 Mixed テスト"), "Mixed");
    }

    #[test]
    fn sanitize_empty_and_whitespace() {
        assert_eq!(sanitize_for_filename(""), "untitled");
        assert_eq!(sanitize_for_filename("   "), "untitled");
        assert_eq!(sanitize_for_filename("///"), "untitled");
    }

    #[test]
    fn gen_routing_request_empty() {
        let _req: McpGenRoutingRequest = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn snapshot_create_request_empty() {
        let _req: McpSnapshotCreateRequest = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn snapshot_list_request_empty() {
        let _req: McpSnapshotListRequest = serde_json::from_str("{}").unwrap();
    }

    #[test]
    fn snapshot_restore_request_parse() {
        let req: McpSnapshotRestoreRequest =
            serde_json::from_str(r#"{"timestamp": "1700000000000"}"#).unwrap();
        assert_eq!(req.timestamp, "1700000000000");
    }

    #[test]
    fn node_history_request_parse() {
        let req: McpNodeHistoryRequest = serde_json::from_str(r#"{"node_id": "2-3"}"#).unwrap();
        assert_eq!(req.node_id, "2-3");
    }

    #[test]
    fn dump_request_parse_minimal() {
        let req: McpDumpRequest = serde_json::from_str(r#"{"output_dir": "/tmp/out"}"#).unwrap();
        assert_eq!(req.output_dir, "/tmp/out");
        assert!(req.format.is_none());
        assert!(req.filename.is_none());
    }

    #[test]
    fn dump_request_parse_full() {
        let req: McpDumpRequest = serde_json::from_str(
            r#"{"output_dir": "/tmp/out", "format": "json", "filename": "book.json"}"#,
        )
        .unwrap();
        assert_eq!(req.format.as_deref(), Some("json"));
        assert_eq!(req.filename.as_deref(), Some("book.json"));
    }

    // ---- batch request type tests ----

    #[test]
    fn batch_move_request_minimal() {
        let req: McpBatchMoveRequest = serde_json::from_str(
            r#"{"moves": [{"node_id": "00000000-0000-0000-0000-000000000001", "new_parent": null, "position": null}]}"#,
        )
        .unwrap();
        assert_eq!(req.moves.len(), 1);
        assert_eq!(req.moves[0].node_id, "00000000-0000-0000-0000-000000000001");
        assert!(req.moves[0].new_parent.is_none());
        assert!(req.moves[0].position.is_none());
    }

    #[test]
    fn batch_move_request_with_parent_and_position() {
        let req: McpBatchMoveRequest = serde_json::from_str(
            r#"{"moves": [{"node_id": "00000000-0000-0000-0000-000000000001",
                           "new_parent": "00000000-0000-0000-0000-000000000002",
                           "position": 3}]}"#,
        )
        .unwrap();
        assert_eq!(
            req.moves[0].new_parent.as_deref(),
            Some("00000000-0000-0000-0000-000000000002")
        );
        assert_eq!(req.moves[0].position, Some(3));
    }

    #[test]
    fn batch_move_request_empty_moves() {
        let req: McpBatchMoveRequest = serde_json::from_str(r#"{"moves": []}"#).unwrap();
        assert!(req.moves.is_empty());
    }

    #[test]
    fn batch_update_request_minimal() {
        let req: McpBatchUpdateRequest = serde_json::from_str(
            r#"{"updates": [{"node_id": "00000000-0000-0000-0000-000000000001"}]}"#,
        )
        .unwrap();
        assert_eq!(req.updates.len(), 1);
        assert!(req.updates[0].title.is_none());
        assert!(req.updates[0].body.is_none());
        assert!(req.updates[0].properties.is_none());
        assert!(req.updates[0].status.is_none());
    }

    #[test]
    fn batch_update_request_full() {
        let req: McpBatchUpdateRequest = serde_json::from_str(
            r#"{"updates": [{"node_id": "00000000-0000-0000-0000-000000000001",
                             "title": "New Title",
                             "body": "content here",
                             "properties": {"key": "val"},
                             "status": "draft"}]}"#,
        )
        .unwrap();
        assert_eq!(req.updates[0].title.as_deref(), Some("New Title"));
        assert_eq!(
            req.updates[0].body.as_ref().and_then(|b| b.as_deref()),
            Some("content here")
        );
        let props = req.updates[0].properties.as_ref().unwrap();
        assert_eq!(props.get("key").map(String::as_str), Some("val"));
        assert_eq!(req.updates[0].status.as_deref(), Some("draft"));
    }

    #[test]
    fn batch_update_request_body_null_deserializes() {
        // Without #[serde(default)] or custom deserializer, serde treats both
        // `"body": null` and omitting `body` as None for Option<Option<T>>.
        // This matches the existing McpNodeUpdateRequest behavior.
        let req: McpBatchUpdateRequest = serde_json::from_str(
            r#"{"updates": [{"node_id": "00000000-0000-0000-0000-000000000001", "body": null}]}"#,
        )
        .unwrap();
        assert!(req.updates[0].body.is_none());
    }
}
