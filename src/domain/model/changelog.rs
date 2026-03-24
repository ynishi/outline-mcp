use serde::{Deserialize, Serialize};

use super::id::NodeId;
use super::timestamp::Timestamp;

/// ノードのライフサイクル状態。
///
/// `#[serde(default)]` で既存JSONが `status` フィールドを持たない場合に `Active` として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    #[default]
    Active,
    Draft,
}

/// ChangeLog に記録するアクション種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    Create,
    Update,
    Delete,
    Move,
    /// snapshot restore 時に記録する。
    Restore,
}

/// ChangeLog の1エントリ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    pub node_id: NodeId,
    pub action: ChangeAction,
    /// 変更前のノードの JSON 表現（Create の場合は None）。
    pub before: Option<String>,
    /// 変更後のノードの JSON 表現（Delete の場合は None）。
    pub after: Option<String>,
    pub timestamp: Timestamp,
}

impl ChangeEntry {
    /// 新しい ChangeEntry を生成する。
    pub fn new(
        node_id: NodeId,
        action: ChangeAction,
        before: Option<String>,
        after: Option<String>,
        timestamp: Timestamp,
    ) -> Self {
        Self {
            node_id,
            action,
            before,
            after,
            timestamp,
        }
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node_id() -> NodeId {
        NodeId::new()
    }

    #[test]
    fn test_node_status_default_is_active() {
        let status = NodeStatus::default();
        assert_eq!(status, NodeStatus::Active);
    }

    #[test]
    fn test_node_status_serde_roundtrip() {
        let status = NodeStatus::Draft;
        let json = serde_json::to_string(&status).expect("serialize");
        assert_eq!(json, r#""draft""#);
        let deserialized: NodeStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized, NodeStatus::Draft);
    }

    #[test]
    fn test_node_status_active_serde() {
        let json = r#""active""#;
        let status: NodeStatus = serde_json::from_str(json).expect("deserialize");
        assert_eq!(status, NodeStatus::Active);
    }

    #[test]
    fn test_change_action_serde_roundtrip() {
        for (action, expected_json) in &[
            (ChangeAction::Create, r#""create""#),
            (ChangeAction::Update, r#""update""#),
            (ChangeAction::Delete, r#""delete""#),
            (ChangeAction::Move, r#""move""#),
            (ChangeAction::Restore, r#""restore""#),
        ] {
            let json = serde_json::to_string(action).expect("serialize");
            assert_eq!(&json, expected_json, "action: {action:?}");
            let deserialized: ChangeAction = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&deserialized, action);
        }
    }

    #[test]
    fn test_change_entry_serde_roundtrip() {
        let node_id = sample_node_id();
        let ts = Timestamp::from_millis(1_700_000_000_000);
        let entry = ChangeEntry::new(
            node_id,
            ChangeAction::Update,
            Some(r#"{"title":"old"}"#.to_string()),
            Some(r#"{"title":"new"}"#.to_string()),
            ts,
        );

        let json = serde_json::to_string(&entry).expect("serialize");
        let restored: ChangeEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.node_id, node_id);
        assert_eq!(restored.action, ChangeAction::Update);
        assert_eq!(restored.before.as_deref(), Some(r#"{"title":"old"}"#));
        assert_eq!(restored.after.as_deref(), Some(r#"{"title":"new"}"#));
        assert_eq!(restored.timestamp.as_millis(), 1_700_000_000_000);
    }

    #[test]
    fn test_change_entry_with_none_fields() {
        let node_id = sample_node_id();
        let ts = Timestamp::from_millis(0);
        let entry = ChangeEntry::new(node_id, ChangeAction::Create, None, None, ts);

        let json = serde_json::to_string(&entry).expect("serialize");
        let restored: ChangeEntry = serde_json::from_str(&json).expect("deserialize");

        assert!(restored.before.is_none());
        assert!(restored.after.is_none());
    }
}
