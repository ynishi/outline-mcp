//! Rendering of pending revisions for the `revision_*` tools and listings:
//! a one-line summary, a JSON summary, and a diff against the node's current
//! content.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

use outline_mcp_core::domain::model::node::{BodyChange, Revision, TemplateNode};
use outline_mcp_core::domain::model::timestamp::Timestamp;

/// Context lines around each change in the body diff.
const DIFF_CONTEXT: usize = 3;

/// `title, body` — the content fields the revision changes.
pub(crate) fn fields_label(rev: &Revision) -> String {
    rev.fields().join(", ")
}

/// Rough age of `at` relative to `now`: `just now`, `12m`, `5h`, `3d`.
pub(crate) fn age(now: Timestamp, at: Timestamp) -> String {
    let secs = (now.as_millis() - at.as_millis()).max(0) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// `Proposal: pending by alice (title, body) 2h ago [stale]` for listings.
pub(crate) fn proposal_line(node: &TemplateNode, rev: &Revision, now: Timestamp) -> String {
    format!(
        "Proposal: pending{} ({}) {}{}",
        rev.by
            .as_deref()
            .map(|b| format!(" by {b}"))
            .unwrap_or_default(),
        fields_label(rev),
        age(now, rev.at),
        if rev.is_stale(node) {
            " [stale: node changed since]"
        } else {
            ""
        }
    )
}

/// JSON summary of a pending revision, for `node_query` and listings.
pub(crate) fn proposal_json(node: &TemplateNode, rev: &Revision) -> Value {
    json!({
        "by": rev.by,
        "note": rev.note,
        "at": rev.at.to_iso8601(),
        "fields": rev.fields(),
        "stale": rev.is_stale(node),
    })
}

/// The properties the node would have after the revision.
fn proposed_properties(node: &TemplateNode, rev: &Revision) -> HashMap<String, String> {
    if let Some(p) = &rev.properties {
        return p.clone();
    }
    let mut merged = node.properties().clone();
    if let Some(patch) = &rev.properties_patch {
        merged.extend(patch.clone());
    }
    merged
}

/// Human-readable diff of the revision against the node's current content:
/// title old → new, a unified diff of the body, and properties added /
/// changed / removed. Fields the revision does not touch are not shown.
pub(crate) fn revision_diff(node: &TemplateNode, rev: &Revision) -> String {
    let mut out = String::new();
    if let Some(title) = &rev.title {
        if title == node.title() {
            out.push_str(&format!("Title: unchanged ({title:?})\n"));
        } else {
            out.push_str(&format!("Title: {:?} → {title:?}\n", node.title()));
        }
    }
    if let Some(change) = &rev.body {
        // An absent body and an empty one are the same here, as in
        // `content_fingerprint` and the "changes nothing" check.
        let current = node.body().unwrap_or("");
        let proposed = match change {
            BodyChange::Set(text) => text.as_str(),
            BodyChange::Clear => "",
        };
        if matches!(change, BodyChange::Clear) {
            out.push_str("Body: removed\n");
        } else if current == proposed {
            out.push_str("Body: unchanged\n");
        } else {
            out.push_str("Body:\n");
        }
        if current != proposed {
            let current_nl = with_trailing_newline(current);
            let proposed_nl = with_trailing_newline(proposed);
            let diff = similar::TextDiff::from_lines(&current_nl, &proposed_nl);
            out.push_str("--- current\n+++ proposed\n");
            for hunk in diff
                .unified_diff()
                .context_radius(DIFF_CONTEXT)
                .iter_hunks()
            {
                out.push_str(&hunk.to_string());
            }
        }
    }
    if rev.properties.is_some() || rev.properties_patch.is_some() {
        let current: BTreeMap<&String, &String> = node.properties().iter().collect();
        let proposed_map = proposed_properties(node, rev);
        let proposed: BTreeMap<&String, &String> = proposed_map.iter().collect();
        let mut lines = Vec::new();
        for (k, v) in &proposed {
            match current.get(k) {
                None => lines.push(format!("  + {k} = {v:?}")),
                Some(old) if old != v => lines.push(format!("  ~ {k}: {old:?} → {v:?}")),
                Some(_) => {}
            }
        }
        for (k, v) in &current {
            if !proposed.contains_key(k) {
                lines.push(format!("  - {k} (was {v:?})"));
            }
        }
        if lines.is_empty() {
            out.push_str("Properties: unchanged\n");
        } else {
            out.push_str("Properties:\n");
            for l in lines {
                out.push_str(&l);
                out.push('\n');
            }
        }
    }
    out
}

fn with_trailing_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}
