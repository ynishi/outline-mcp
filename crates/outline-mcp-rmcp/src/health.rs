//! Rendering for `book_lint` / `book_stats`: request options → core options,
//! and core reports → text or JSON. The analysis itself lives in
//! `outline_mcp_core::domain::health` and is read-only.

use std::collections::HashMap;

use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

use outline_mcp_core::domain::health::{BookStats, LintCheck, LintOptions, LintReport};
use outline_mcp_core::domain::model::book::TemplateBook;
use outline_mcp_core::domain::model::id::NodeId;

use crate::helpers::build_hierarchical_ids;
use crate::request::McpBookLintRequest;

/// Request → core options; out-of-range values are the caller's error.
pub(crate) fn lint_options(req: &McpBookLintRequest) -> Result<LintOptions, McpError> {
    let mut opts = LintOptions::default();
    if let Some(checks) = &req.checks {
        let mut parsed = Vec::new();
        for c in checks {
            let check = LintCheck::parse(c).ok_or_else(|| {
                let valid: Vec<&str> = LintCheck::ALL.iter().map(|c| c.as_str()).collect();
                McpError::invalid_params(
                    format!("Unknown check: '{c}'. Use: {}", valid.join(", ")),
                    None,
                )
            })?;
            parsed.push(check);
        }
        opts.checks = parsed;
    }
    if let Some(t) = req.threshold {
        if !(t > 0.0 && t <= 1.0) {
            return Err(McpError::invalid_params(
                format!("threshold must be in (0, 1], got {t}"),
                None,
            ));
        }
        opts.threshold = t;
    }
    if let Some(l) = req.limit {
        if l == 0 {
            return Err(McpError::invalid_params("limit must be at least 1", None));
        }
        opts.limit = l;
    }
    if let Some(d) = req.stale_days {
        opts.stale_days = d;
    }
    Ok(opts)
}

/// Hierarchical id ("2-3") and title for each node, for rendering.
fn locate(book: &TemplateBook) -> HashMap<NodeId, String> {
    build_hierarchical_ids(book)
        .into_iter()
        .map(|(hier, id)| (id, hier))
        .collect()
}

pub(crate) fn lint_text(book: &TemplateBook, slug: &str, report: &LintReport) -> String {
    let ids = locate(book);
    let mut out = format!(
        "# Lint for \"{}\" ({slug}): {} finding(s)\n",
        book.title(),
        report.findings.len() + report.omitted.values().sum::<usize>()
    );
    for &check in &report.checks {
        let found: Vec<_> = report.findings.iter().filter(|f| f.kind == check).collect();
        let more = report.omitted.get(&check).copied().unwrap_or(0);
        if found.is_empty() {
            out.push_str(&format!("\n## {}: none\n", check.as_str()));
            continue;
        }
        out.push_str(&format!(
            "\n## {} ({})\n",
            check.as_str(),
            found.len() + more
        ));
        for f in found {
            let at: Vec<String> = f
                .nodes
                .iter()
                .map(|id| {
                    ids.get(id)
                        .cloned()
                        .unwrap_or_else(|| id.short().to_string())
                })
                .collect();
            out.push_str(&format!("- {}: {}\n", at.join(", "), f.detail));
        }
        if more > 0 {
            out.push_str(&format!("… {more} more not shown (raise `limit`)\n"));
        }
    }
    out
}

pub(crate) fn lint_json(book: &TemplateBook, slug: &str, report: &LintReport) -> Value {
    let ids = locate(book);
    json!({
        "book": slug,
        "title": book.title(),
        "checks": report.checks.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "findings": report.findings.iter().map(|f| {
            let mut o = json!({
                "kind": f.kind.as_str(),
                "nodes": f.nodes.iter().map(|id| json!({
                    "id": id.to_string(),
                    "toc_id": ids.get(id),
                    "title": book.get_node(*id).map(|n| n.title()),
                })).collect::<Vec<_>>(),
                "detail": f.detail,
            });
            if let Some(s) = f.score {
                o["score"] = json!(s);
            }
            o
        }).collect::<Vec<_>>(),
        "omitted": report.omitted.iter()
            .map(|(k, v)| (k.as_str().to_string(), json!(v)))
            .collect::<serde_json::Map<_, _>>(),
    })
}

pub(crate) fn stats_text(book: &TemplateBook, slug: &str, s: &BookStats) -> String {
    let hist = s
        .children
        .histogram
        .iter()
        .map(|b| format!("{}: {}", b.children, b.sections))
        .collect::<Vec<_>>()
        .join(", ");
    let status = s
        .by_status
        .iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# Stats for \"{}\" ({slug})\n\
         Nodes: {} ({} sections, {} content)\n\
         Depth: max {}, mean {}\n\
         Children per section: max {}, mean {}; sections by child count — {hist}\n\
         Status: {}\n\
         Injected (inject=true): {}\n\
         Updated: ≤7d {}, 7–30d {}, 30–90d {}, >90d {}, unknown {}\n\
         Distinct property keys: {}\n\
         Links: {}\n\
         Pending revisions: {}\n",
        book.title(),
        s.nodes,
        s.sections,
        s.contents,
        s.max_depth,
        s.mean_depth,
        s.children.max,
        s.children.mean,
        if status.is_empty() {
            "-".to_string()
        } else {
            status
        },
        s.injected,
        s.updated.last_7d,
        s.updated.last_30d,
        s.updated.last_90d,
        s.updated.older,
        s.updated.unknown,
        s.property_keys,
        s.links,
        s.pending_proposals,
    )
}

pub(crate) fn stats_json(book: &TemplateBook, slug: &str, s: &BookStats) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_else(|_| json!({}));
    v["book"] = json!(slug);
    v["title"] = json!(book.title());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> McpBookLintRequest {
        McpBookLintRequest {
            slug: None,
            checks: None,
            threshold: None,
            limit: None,
            stale_days: None,
            format: None,
        }
    }

    #[test]
    fn options_default_and_validate() {
        let o = lint_options(&req()).unwrap();
        assert!(o.checks.is_empty());
        assert_eq!(o.limit, 50);

        let mut r = req();
        r.checks = Some(vec!["nope".into()]);
        assert!(lint_options(&r)
            .unwrap_err()
            .message
            .contains("Unknown check"));

        for bad in [0.0, -0.1, 1.5] {
            let mut r = req();
            r.threshold = Some(bad);
            assert!(lint_options(&r).is_err(), "{bad}");
        }
        let mut r = req();
        r.limit = Some(0);
        assert!(lint_options(&r).is_err());

        let mut r = req();
        r.checks = Some(vec!["empty_sections".into(), "root_content".into()]);
        r.threshold = Some(1.0);
        let o = lint_options(&r).unwrap();
        assert_eq!(o.checks.len(), 2);
        assert_eq!(o.threshold, 1.0);
    }
}
