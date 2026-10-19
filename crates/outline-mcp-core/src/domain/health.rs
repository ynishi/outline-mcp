//! Read-only health checks over a book: `lint` (structural smells) and
//! `stats` (shape and freshness numbers).
//!
//! Both work on a loaded [`TemplateBook`] and never write: no store access,
//! no events. Callers decide where the book comes from.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::model::book::TemplateBook;
use super::model::changelog::NodeStatus;
use super::model::id::NodeId;
use super::model::node::{NodeType, TemplateNode};
use super::model::timestamp::Timestamp;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

// =============================================================================
// Lint
// =============================================================================

/// One kind of lint check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintCheck {
    /// Root-level nodes that are not sections.
    RootContent,
    /// Pairs of nodes whose titles share most of their tokens.
    SimilarTitles,
    /// Property keys that collide once case, `_`, `-` and spaces are ignored.
    PropertyKeyVariants,
    /// Sections without children.
    EmptySections,
    /// Draft nodes not updated for a long time (or never stamped).
    StaleDrafts,
    /// Links whose target node is not in the book (left by data restored or
    /// imported from elsewhere).
    DanglingLinks,
    /// Pending revisions older than `stale_days`, or whose node changed after
    /// they were proposed.
    StaleProposals,
}

impl LintCheck {
    /// Every check, in report order.
    pub const ALL: [LintCheck; 7] = [
        LintCheck::RootContent,
        LintCheck::SimilarTitles,
        LintCheck::PropertyKeyVariants,
        LintCheck::EmptySections,
        LintCheck::StaleDrafts,
        LintCheck::DanglingLinks,
        LintCheck::StaleProposals,
    ];

    /// The snake_case name used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            LintCheck::RootContent => "root_content",
            LintCheck::SimilarTitles => "similar_titles",
            LintCheck::PropertyKeyVariants => "property_key_variants",
            LintCheck::EmptySections => "empty_sections",
            LintCheck::StaleDrafts => "stale_drafts",
            LintCheck::DanglingLinks => "dangling_links",
            LintCheck::StaleProposals => "stale_proposals",
        }
    }

    /// Parse a wire name; `None` for an unknown one.
    pub fn parse(s: &str) -> Option<Self> {
        LintCheck::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Knobs for [`lint`].
#[derive(Debug, Clone)]
pub struct LintOptions {
    /// Checks to run; empty runs all of them.
    pub checks: Vec<LintCheck>,
    /// Minimum title similarity (Jaccard over tokens, inclusive), in `(0, 1]`.
    ///
    /// [`lint`] clamps a value outside that range into it (NaN reads as the
    /// default 0.6): at 0 or below every pair of titles would qualify, and
    /// the candidate search only considers pairs that share a token.
    /// Callers taking user input should reject such values instead.
    pub threshold: f64,
    /// Maximum findings reported per check; the rest are counted.
    pub limit: usize,
    /// A draft older than this many days is stale.
    pub stale_days: u32,
    /// "Now" for staleness, injectable for tests.
    pub now: Timestamp,
}

impl Default for LintOptions {
    fn default() -> Self {
        Self {
            checks: Vec::new(),
            threshold: 0.6,
            limit: 50,
            stale_days: 90,
            now: Timestamp::now(),
        }
    }
}

/// One finding.
#[derive(Debug, Clone, Serialize)]
pub struct LintFinding {
    /// Which check produced it.
    pub kind: LintCheck,
    /// The nodes involved, in book order (for similar titles: the pair).
    pub nodes: Vec<NodeId>,
    /// Human-readable detail (titles, variants, ages).
    pub detail: String,
    /// Title similarity, for `similar_titles` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// The result of [`lint`].
#[derive(Debug, Clone, Serialize)]
pub struct LintReport {
    /// Checks that ran, in report order.
    pub checks: Vec<LintCheck>,
    /// Reported findings, grouped by check in report order.
    pub findings: Vec<LintFinding>,
    /// Per check that ran, how many findings were left out by `limit`
    /// (0 when none were); every check in `checks` has an entry.
    pub omitted: BTreeMap<LintCheck, usize>,
}

/// `threshold` forced into `(0, 1]`; NaN becomes the default.
fn effective_threshold(t: f64) -> f64 {
    if t.is_nan() {
        LintOptions::default().threshold
    } else {
        t.clamp(f64::MIN_POSITIVE, 1.0)
    }
}

/// Run the selected checks over `book`. Pure: reads only.
///
/// `opts.threshold` is clamped into `(0, 1]` (see [`LintOptions::threshold`]).
pub fn lint(book: &TemplateBook, opts: &LintOptions) -> LintReport {
    let checks: Vec<LintCheck> = if opts.checks.is_empty() {
        LintCheck::ALL.to_vec()
    } else {
        LintCheck::ALL
            .into_iter()
            .filter(|c| opts.checks.contains(c))
            .collect()
    };
    // Archived nodes (and everything under an archived section) are retired:
    // no check looks at them, so an archived original never pairs with its
    // active replacement and an archived empty section is not reported.
    let order: Vec<&TemplateNode> = book
        .all_nodes_dfs()
        .into_iter()
        .filter(|n| !book.is_archived_or_under_archived(n.id()))
        .collect();

    let mut findings = Vec::new();
    let mut omitted = BTreeMap::new();
    for &check in &checks {
        let mut found = match check {
            LintCheck::RootContent => root_content(book, &order),
            LintCheck::SimilarTitles => similar_titles(&order, effective_threshold(opts.threshold)),
            LintCheck::PropertyKeyVariants => property_key_variants(&order),
            LintCheck::EmptySections => empty_sections(&order),
            LintCheck::StaleDrafts => stale_drafts(&order, opts.stale_days, opts.now),
            LintCheck::DanglingLinks => dangling_links(book, &order),
            LintCheck::StaleProposals => stale_proposals(book, opts.stale_days, opts.now),
        };
        let left_out = found.len().saturating_sub(opts.limit);
        omitted.insert(check, left_out);
        found.truncate(opts.limit);
        findings.extend(found);
    }
    LintReport {
        checks,
        findings,
        omitted,
    }
}

fn root_content(book: &TemplateBook, order: &[&TemplateNode]) -> Vec<LintFinding> {
    book.root_nodes()
        .iter()
        .filter_map(|&id| order.iter().find(|n| n.id() == id).copied())
        .filter(|n| *n.node_type() != NodeType::Section)
        .map(|n| LintFinding {
            kind: LintCheck::RootContent,
            nodes: vec![n.id()],
            detail: format!("\"{}\" is a content node at the root", n.title()),
            score: None,
        })
        .collect()
}

fn dangling_links(book: &TemplateBook, order: &[&TemplateNode]) -> Vec<LintFinding> {
    order
        .iter()
        .flat_map(|n| {
            n.links()
                .iter()
                .filter(|l| book.get_node(l.target()).is_none())
                .map(move |l| LintFinding {
                    kind: LintCheck::DanglingLinks,
                    nodes: vec![n.id()],
                    detail: format!(
                        "\"{}\" has a {} link to {}, which is not in the book",
                        n.title(),
                        l.rel(),
                        l.target()
                    ),
                    score: None,
                })
        })
        .collect()
}

fn empty_sections(order: &[&TemplateNode]) -> Vec<LintFinding> {
    order
        .iter()
        .filter(|n| *n.node_type() == NodeType::Section && n.children().is_empty())
        .map(|n| LintFinding {
            kind: LintCheck::EmptySections,
            nodes: vec![n.id()],
            detail: format!("section \"{}\" has no children", n.title()),
            score: None,
        })
        .collect()
}

fn stale_drafts(order: &[&TemplateNode], stale_days: u32, now: Timestamp) -> Vec<LintFinding> {
    let limit_ms = i64::from(stale_days) * DAY_MS;
    order
        .iter()
        .filter(|n| n.status() == NodeStatus::Draft)
        .filter_map(|n| match n.updated_at() {
            None => Some((n, "never updated".to_string())),
            Some(at) => {
                let age = now.as_millis() - at.as_millis();
                (age > limit_ms).then(|| (n, format!("last updated {} days ago", age / DAY_MS)))
            }
        })
        .map(|(n, age)| LintFinding {
            kind: LintCheck::StaleDrafts,
            nodes: vec![n.id()],
            detail: format!("draft \"{}\": {age}", n.title()),
            score: None,
        })
        .collect()
}

/// Unlike the other checks this one also looks at archived nodes (and nodes
/// under an archived section): a revision pending there can no longer be
/// approved, only rejected, so it is reported for that reason. That keeps the
/// count of pending revisions it can see equal to `book_stats`'
/// `pending_proposals` and to `revisions_pending`.
fn stale_proposals(book: &TemplateBook, stale_days: u32, now: Timestamp) -> Vec<LintFinding> {
    let limit_ms = i64::from(stale_days) * DAY_MS;
    book.all_nodes_dfs()
        .into_iter()
        .filter_map(|n| {
            let p = n.proposal()?;
            let age = now.as_millis() - p.at.as_millis();
            let mut why = Vec::new();
            if book.is_archived_or_under_archived(n.id()) {
                why.push("on an archived node — reject it".to_string());
            }
            if p.is_stale(n) {
                why.push("the node changed after it was proposed".to_string());
            }
            if age > limit_ms {
                why.push(format!("proposed {} days ago", age / DAY_MS));
            }
            (!why.is_empty()).then(|| LintFinding {
                kind: LintCheck::StaleProposals,
                nodes: vec![n.id()],
                detail: format!(
                    "pending revision of \"{}\"{}: {}",
                    n.title(),
                    p.by.as_deref()
                        .map(|b| format!(" by {b}"))
                        .unwrap_or_default(),
                    why.join("; ")
                ),
                score: None,
            })
        })
        .collect()
}

/// `added_by`, `Added-By` and `added by` all normalize to `addedby`.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

fn property_key_variants(order: &[&TemplateNode]) -> Vec<LintFinding> {
    // normalized -> raw key -> nodes using it (book order)
    let mut groups: BTreeMap<String, BTreeMap<String, Vec<NodeId>>> = BTreeMap::new();
    for n in order {
        for key in n.properties().keys() {
            groups
                .entry(normalize_key(key))
                .or_default()
                .entry(key.clone())
                .or_default()
                .push(n.id());
        }
    }
    groups
        .into_values()
        .filter(|variants| variants.len() > 1)
        .map(|variants| {
            // The most used spelling is the reference; nodes using any other
            // spelling are the ones to fix.
            let majority = variants
                .iter()
                .max_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| b.0.cmp(a.0)))
                .map(|(k, _)| k.clone())
                .unwrap_or_default();
            let detail = variants
                .iter()
                .map(|(k, ids)| format!("\"{k}\" ×{}", ids.len()))
                .collect::<Vec<_>>()
                .join(", ");
            let mut nodes: Vec<NodeId> = variants
                .iter()
                .filter(|(k, _)| **k != majority)
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect();
            nodes.sort_by_key(|id| order.iter().position(|n| n.id() == *id));
            nodes.dedup();
            LintFinding {
                kind: LintCheck::PropertyKeyVariants,
                nodes,
                detail: format!("property key variants: {detail} (most used: \"{majority}\")"),
                score: None,
            }
        })
        .collect()
}

/// Whether `c` belongs to a script written without spaces between words.
///
/// Punctuation inside these blocks (the katakana middle dot `・` U+30FB and
/// the double hyphen `゠` U+30A0, and the halfwidth middle dot U+FF65 just
/// below the halfwidth range) is a separator, not part of a run.
fn is_cjk(c: char) -> bool {
    if matches!(c, '\u{30A0}' | '\u{30FB}') {
        return false;
    }
    matches!(c as u32,
        0x3040..=0x30FF     // Hiragana, Katakana
        | 0x3400..=0x4DBF   // CJK Extension A
        | 0x4E00..=0x9FFF   // CJK Unified Ideographs
        | 0xF900..=0xFAFF   // CJK Compatibility Ideographs
        | 0xAC00..=0xD7AF   // Hangul syllables
        | 0xFF66..=0xFF9F   // Halfwidth Katakana
        | 0x20000..=0x2FA1F // CJK Extension B and later, Compatibility Supplement
    )
}

/// Title tokens: lowercase words for alphabetic scripts, character bigrams for
/// CJK runs (a single CJK character is its own token).
pub fn title_tokens(title: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_word(word: &mut String, tokens: &mut HashSet<String>) {
        if !word.is_empty() {
            tokens.insert(std::mem::take(word));
        }
    }
    fn flush_cjk(run: &mut Vec<char>, tokens: &mut HashSet<String>) {
        match run.len() {
            0 => {}
            1 => {
                tokens.insert(run[0].to_string());
            }
            _ => {
                for pair in run.windows(2) {
                    tokens.insert(pair.iter().collect());
                }
            }
        }
        run.clear();
    }

    for c in title.chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut tokens);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut tokens);
            word.extend(c.to_lowercase());
        } else {
            flush_word(&mut word, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        }
    }
    flush_word(&mut word, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
}

/// Jaccard similarity of two sorted, deduplicated token-id lists.
fn jaccard(a: &[u32], b: &[u32]) -> f64 {
    let (mut i, mut j, mut inter) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

fn similar_titles(order: &[&TemplateNode], threshold: f64) -> Vec<LintFinding> {
    // Intern tokens so each title is a sorted Vec<u32>.
    let mut interner: HashMap<String, u32> = HashMap::new();
    let token_ids: Vec<Vec<u32>> = order
        .iter()
        .map(|n| {
            let mut ids: Vec<u32> = title_tokens(n.title())
                .into_iter()
                .map(|t| {
                    let next = interner.len() as u32;
                    *interner.entry(t).or_insert(next)
                })
                .collect();
            ids.sort_unstable();
            ids
        })
        .collect();
    // Normalized titles, to let identical one-token titles through.
    let normalized: Vec<String> = order
        .iter()
        .map(|n| n.title().trim().to_lowercase())
        .collect();

    // Inverted index: only pairs sharing at least one token can reach a
    // positive threshold, so candidates come from shared postings.
    let mut postings: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, ids) in token_ids.iter().enumerate() {
        for &t in ids {
            postings.entry(t).or_default().push(i);
        }
    }

    let eps = 1e-9;
    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    for (i, a) in token_ids.iter().enumerate() {
        if a.is_empty() {
            continue;
        }
        seen.clear();
        for t in a {
            for &j in &postings[t] {
                if j <= i || !seen.insert(j) {
                    continue;
                }
                let b = &token_ids[j];
                // Two one-token titles say too little to call them similar —
                // unless they are the same title, the clearest duplicate.
                if a.len() < 2 && b.len() < 2 && normalized[i] != normalized[j] {
                    continue;
                }
                // Upper bound on Jaccard from the sizes alone.
                let (lo, hi) = if a.len() < b.len() {
                    (a.len(), b.len())
                } else {
                    (b.len(), a.len())
                };
                if (lo as f64 / hi as f64) + eps < threshold {
                    continue;
                }
                let score = jaccard(a, b);
                if score + eps >= threshold {
                    pairs.push((i, j, score));
                }
            }
        }
    }

    pairs.sort_by(|x, y| {
        y.2.partial_cmp(&x.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(x.0.cmp(&y.0))
            .then(x.1.cmp(&y.1))
    });
    pairs
        .into_iter()
        .map(|(i, j, score)| LintFinding {
            kind: LintCheck::SimilarTitles,
            nodes: vec![order[i].id(), order[j].id()],
            detail: format!(
                "\"{}\" ~ \"{}\" (similarity {:.2})",
                order[i].title(),
                order[j].title(),
                score
            ),
            score: Some((score * 100.0).round() / 100.0),
        })
        .collect()
}

// =============================================================================
// Stats
// =============================================================================

/// One histogram bucket of children-per-section.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Bucket {
    /// Range of child counts: "0", "1", "2-5", "6-10", "11-50" or ">50".
    pub children: &'static str,
    /// Sections whose child count falls in the range.
    pub sections: usize,
}

/// Children-per-section numbers.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChildrenStats {
    /// Most children of any section.
    pub max: usize,
    /// Mean children per section (0 when there are no sections).
    pub mean: f64,
    /// Sections per child-count range, in ascending range order.
    pub histogram: Vec<Bucket>,
}

impl ChildrenStats {
    /// Sections in the bucket named `children` (0 for an unknown name).
    pub fn bucket(&self, children: &str) -> usize {
        self.histogram
            .iter()
            .find(|b| b.children == children)
            .map_or(0, |b| b.sections)
    }
}

/// How recently nodes were updated; the buckets do not overlap.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct UpdatedStats {
    /// Updated within the last 7 days.
    pub last_7d: usize,
    /// Updated 7 to 30 days ago.
    pub last_30d: usize,
    /// Updated 30 to 90 days ago.
    pub last_90d: usize,
    /// Updated more than 90 days ago.
    pub older: usize,
    /// No update time recorded.
    pub unknown: usize,
}

/// The result of [`stats`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BookStats {
    /// All nodes.
    pub nodes: usize,
    /// Section nodes.
    pub sections: usize,
    /// Content nodes.
    pub contents: usize,
    /// Deepest node (root = 1; 0 for an empty book).
    pub max_depth: u8,
    /// Mean node depth (0 for an empty book).
    pub mean_depth: f64,
    /// Children per section.
    pub children: ChildrenStats,
    /// Node count per status; always has every status ("active", "draft",
    /// "archived"), 0 when no node has it.
    pub by_status: BTreeMap<String, usize>,
    /// Nodes `select_book` injects: property `inject=true`, active, and not
    /// under an archived section.
    pub injected: usize,
    /// Update recency.
    pub updated: UpdatedStats,
    /// Distinct property keys across the book.
    pub property_keys: usize,
    /// Typed links across the book (outgoing links of every node).
    pub links: usize,
    /// Nodes with a pending revision.
    pub pending_proposals: usize,
}

fn bucket(n: usize) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2..=5 => "2-5",
        6..=10 => "6-10",
        11..=50 => "11-50",
        _ => ">50",
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Shape and freshness numbers for `book`. Pure: reads only.
pub fn stats(book: &TemplateBook, now: Timestamp) -> BookStats {
    let nodes = book.all_nodes_dfs();
    let mut sections = 0;
    let mut child_counts = Vec::new();
    let mut by_status: BTreeMap<String, usize> = NodeStatus::ALL
        .into_iter()
        .map(|s| (s.as_str().to_string(), 0))
        .collect();
    let mut injected = 0;
    let mut updated = UpdatedStats::default();
    let mut keys: HashSet<&str> = HashSet::new();
    let mut depth_sum = 0u64;
    let mut max_depth = 0u8;

    for n in &nodes {
        let depth = book.depth_of(n.id());
        depth_sum += u64::from(depth);
        max_depth = max_depth.max(depth);
        if *n.node_type() == NodeType::Section {
            sections += 1;
            child_counts.push(n.children().len());
        }
        *by_status
            .entry(n.status().as_str().to_string())
            .or_default() += 1;
        // What `select_book` actually injects: the property on an active node
        // that is not under an archived section.
        if n.get_property("inject") == Some("true")
            && n.status() == NodeStatus::Active
            && !book.is_archived_or_under_archived(n.id())
        {
            injected += 1;
        }
        match n.updated_at() {
            None => updated.unknown += 1,
            Some(at) => {
                let age = now.as_millis() - at.as_millis();
                if age <= 7 * DAY_MS {
                    updated.last_7d += 1;
                } else if age <= 30 * DAY_MS {
                    updated.last_30d += 1;
                } else if age <= 90 * DAY_MS {
                    updated.last_90d += 1;
                } else {
                    updated.older += 1;
                }
            }
        }
        keys.extend(n.properties().keys().map(String::as_str));
    }

    let mut histogram: Vec<Bucket> = ["0", "1", "2-5", "6-10", "11-50", ">50"]
        .into_iter()
        .map(|children| Bucket {
            children,
            sections: 0,
        })
        .collect();
    for &c in &child_counts {
        let name = bucket(c);
        if let Some(b) = histogram.iter_mut().find(|b| b.children == name) {
            b.sections += 1;
        }
    }
    let children = ChildrenStats {
        max: child_counts.iter().copied().max().unwrap_or(0),
        mean: if child_counts.is_empty() {
            0.0
        } else {
            round2(child_counts.iter().sum::<usize>() as f64 / child_counts.len() as f64)
        },
        histogram,
    };

    BookStats {
        nodes: nodes.len(),
        sections,
        contents: nodes.len() - sections,
        max_depth,
        mean_depth: if nodes.is_empty() {
            0.0
        } else {
            round2(depth_sum as f64 / nodes.len() as f64)
        },
        children,
        by_status,
        injected,
        updated,
        property_keys: keys.len(),
        links: book.link_count(),
        pending_proposals: nodes.iter().filter(|n| n.proposal().is_some()).count(),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::book::{AddNodeRequest, UpdateNodeRequest};

    fn add(book: &mut TemplateBook, parent: Option<NodeId>, title: &str, ty: NodeType) -> NodeId {
        book.add_node(AddNodeRequest {
            parent,
            title: title.to_string(),
            node_type: ty,
            body: None,
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        })
        .unwrap()
    }

    fn empty_update() -> UpdateNodeRequest {
        UpdateNodeRequest {
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: None,
            status: None,
        }
    }

    fn with_props(book: &mut TemplateBook, id: NodeId, props: &[(&str, &str)]) {
        let mut req = empty_update();
        req.properties = Some(
            props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        book.update_node(id, req).unwrap();
    }

    fn set_status(book: &mut TemplateBook, id: NodeId, status: NodeStatus) {
        let mut req = empty_update();
        req.status = Some(status);
        book.update_node(id, req).unwrap();
    }

    fn kinds(r: &LintReport, k: LintCheck) -> Vec<&LintFinding> {
        r.findings.iter().filter(|f| f.kind == k).collect()
    }

    fn only(check: LintCheck) -> LintOptions {
        LintOptions {
            checks: vec![check],
            ..Default::default()
        }
    }

    #[test]
    fn root_content_flags_content_at_root_only() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "Section", NodeType::Section);
        add(&mut b, Some(s), "Nested content", NodeType::Content);
        let c = add(&mut b, None, "Loose", NodeType::Content);
        let r = lint(&b, &only(LintCheck::RootContent));
        let f = kinds(&r, LintCheck::RootContent);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].nodes, vec![c]);
    }

    #[test]
    fn empty_sections_flags_childless_sections_only() {
        let mut b = TemplateBook::new("B", 4);
        let empty = add(&mut b, None, "Empty", NodeType::Section);
        let full = add(&mut b, None, "Full", NodeType::Section);
        add(&mut b, Some(full), "x", NodeType::Content);
        let r = lint(&b, &only(LintCheck::EmptySections));
        let f = kinds(&r, LintCheck::EmptySections);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].nodes, vec![empty]);
    }

    #[test]
    fn similar_titles_finds_latin_pairs_and_skips_unrelated() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let a = add(&mut b, Some(s), "Git branching strategy", NodeType::Content);
        let c = add(
            &mut b,
            Some(s),
            "git Branching Strategy notes",
            NodeType::Content,
        );
        add(&mut b, Some(s), "Commit message rules", NodeType::Content);
        let r = lint(&b, &only(LintCheck::SimilarTitles));
        let f = kinds(&r, LintCheck::SimilarTitles);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].nodes, vec![a, c]);
        assert_eq!(f[0].score, Some(0.75));
    }

    #[test]
    fn similar_titles_uses_cjk_bigrams() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let a = add(&mut b, Some(s), "ブランチ運用ルール", NodeType::Content);
        let c = add(&mut b, Some(s), "ブランチ運用のルール", NodeType::Content);
        add(&mut b, Some(s), "コミット分割", NodeType::Content);
        let r = lint(&b, &only(LintCheck::SimilarTitles));
        let f = kinds(&r, LintCheck::SimilarTitles);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].nodes, vec![a, c]);
    }

    #[test]
    fn similar_titles_threshold_is_inclusive() {
        // {a, b, c} vs {a, b, d}: 2 / 4 = 0.5
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        add(&mut b, Some(s), "alpha beta gamma", NodeType::Content);
        add(&mut b, Some(s), "alpha beta delta", NodeType::Content);
        let at = |t: f64| {
            lint(
                &b,
                &LintOptions {
                    checks: vec![LintCheck::SimilarTitles],
                    threshold: t,
                    ..Default::default()
                },
            )
            .findings
            .len()
        };
        assert_eq!(at(0.5), 1);
        assert_eq!(at(0.51), 0);
    }

    #[test]
    fn similar_titles_skips_one_token_pairs_unless_identical() {
        let mut b = TemplateBook::new("B", 4);
        let s1 = add(&mut b, None, "S1", NodeType::Section);
        let s2 = add(&mut b, None, "S2", NodeType::Section);
        // Same one-word title under different parents: the clearest duplicate.
        let o1 = add(&mut b, Some(s1), "Overview", NodeType::Content);
        let o2 = add(&mut b, Some(s2), " overview ", NodeType::Content);
        // One token each and sharing it, but not the same title.
        add(&mut b, Some(s1), "Summary", NodeType::Content);
        add(&mut b, Some(s2), "Summary!", NodeType::Content);
        // CJK two-character titles are one bigram token each.
        let d1 = add(&mut b, Some(s1), "設計", NodeType::Content);
        let d2 = add(&mut b, Some(s2), "設計", NodeType::Content);
        add(&mut b, Some(s2), "設計。", NodeType::Content);
        let r = lint(&b, &only(LintCheck::SimilarTitles));
        let ids = |v: &[NodeId]| v.iter().map(|i| i.to_string()).collect::<Vec<_>>();
        let mut pairs: Vec<Vec<String>> = kinds(&r, LintCheck::SimilarTitles)
            .iter()
            .map(|f| ids(&f.nodes))
            .collect();
        pairs.sort();
        let mut want = vec![ids(&[o1, o2]), ids(&[d1, d2])];
        want.sort();
        assert_eq!(pairs, want, "{:?}", r.findings);
        assert!(r.findings.iter().all(|f| f.score == Some(1.0)));
    }

    #[test]
    fn similar_titles_does_not_pair_different_one_word_titles() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        add(&mut b, Some(s), "Overview", NodeType::Content);
        add(&mut b, Some(s), "Summary", NodeType::Content);
        let r = lint(&b, &only(LintCheck::SimilarTitles));
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn title_tokens_split_on_cjk_punctuation_and_mixed_scripts() {
        let t = title_tokens("Git・運用");
        assert!(t.iter().all(|tok| !tok.contains('・')), "{t:?}");
        let mut got: Vec<_> = t.into_iter().collect();
        got.sort();
        assert_eq!(got, ["git", "運用"]);

        let mut got: Vec<_> = title_tokens("API設計方針").into_iter().collect();
        got.sort();
        assert_eq!(got, ["api", "方針", "計方", "設計"]);

        // CJK Extension B characters form a run, not a "word".
        let ext_b = "\u{20B9F}\u{20BB7}";
        let got: Vec<_> = title_tokens(ext_b).into_iter().collect();
        assert_eq!(got, [ext_b.to_string()]);
    }

    #[test]
    fn threshold_outside_range_is_clamped() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        add(&mut b, Some(s), "alpha beta gamma delta", NodeType::Content);
        add(&mut b, Some(s), "alpha epsilon zeta eta", NodeType::Content);
        add(&mut b, Some(s), "unrelated words here", NodeType::Content);
        let run = |t: f64| {
            lint(
                &b,
                &LintOptions {
                    checks: vec![LintCheck::SimilarTitles],
                    threshold: t,
                    ..Default::default()
                },
            )
            .findings
            .len()
        };
        // 0 and below act as the smallest positive threshold: pairs sharing a
        // token qualify, pairs sharing none do not.
        assert_eq!(run(0.0), 1);
        assert_eq!(run(-3.0), 1);
        // Above 1 acts as 1; NaN acts as the default 0.6.
        assert_eq!(run(7.0), 0);
        assert_eq!(run(f64::NAN), 0);
    }

    #[test]
    fn property_key_variants_reports_minority_spellings() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let n1 = add(&mut b, Some(s), "one", NodeType::Content);
        let n2 = add(&mut b, Some(s), "two", NodeType::Content);
        let n3 = add(&mut b, Some(s), "three", NodeType::Content);
        with_props(&mut b, n1, &[("added_by", "x")]);
        with_props(&mut b, n2, &[("added_by", "y")]);
        with_props(&mut b, n3, &[("Added-By", "z"), ("scope", "rust")]);
        let r = lint(&b, &only(LintCheck::PropertyKeyVariants));
        let f = kinds(&r, LintCheck::PropertyKeyVariants);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].nodes, vec![n3]);
        assert!(f[0].detail.contains("\"added_by\" ×2"), "{}", f[0].detail);
        assert!(f[0].detail.contains("\"Added-By\" ×1"), "{}", f[0].detail);
    }

    #[test]
    fn property_key_variants_when_one_node_uses_two_spellings() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let both = add(&mut b, Some(s), "both", NodeType::Content);
        let plain = add(&mut b, Some(s), "plain", NodeType::Content);
        with_props(&mut b, both, &[("added_by", "x"), ("Added-By", "y")]);
        with_props(&mut b, plain, &[("added_by", "z")]);
        let r = lint(&b, &only(LintCheck::PropertyKeyVariants));
        let f = kinds(&r, LintCheck::PropertyKeyVariants);
        assert_eq!(f.len(), 1, "{f:?}");
        // "added_by" is the majority (2 uses); the node also carrying the
        // minority spelling is listed once.
        assert_eq!(f[0].nodes, vec![both]);
        assert!(f[0].detail.contains("\"added_by\" ×2"), "{}", f[0].detail);
        assert!(f[0].detail.contains("\"Added-By\" ×1"), "{}", f[0].detail);
    }

    #[test]
    fn property_key_variants_silent_when_keys_are_consistent() {
        let mut b = TemplateBook::new("B", 4);
        let n = add(&mut b, None, "one", NodeType::Content);
        with_props(&mut b, n, &[("added_by", "x"), ("scope", "y")]);
        let r = lint(&b, &only(LintCheck::PropertyKeyVariants));
        assert!(r.findings.is_empty());
    }

    #[test]
    fn stale_drafts_respects_age_and_status() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let d = add(&mut b, Some(s), "draft", NodeType::Content);
        let a = add(&mut b, Some(s), "active", NodeType::Content);
        set_status(&mut b, d, NodeStatus::Draft);
        set_status(&mut b, a, NodeStatus::Active);
        let updated = b.get_node(d).unwrap().updated_at().unwrap();
        let opts = |days_later: i64| LintOptions {
            checks: vec![LintCheck::StaleDrafts],
            stale_days: 90,
            now: Timestamp::from_millis(updated.as_millis() + days_later * DAY_MS),
            ..Default::default()
        };
        assert!(lint(&b, &opts(30)).findings.is_empty());
        let f = lint(&b, &opts(120)).findings;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].nodes, vec![d]);
        assert!(f[0].detail.contains("120 days"), "{}", f[0].detail);
    }

    /// `book` with the `updated_at` of node `id` removed, as for nodes written
    /// before the field existed.
    fn without_updated_at(book: &TemplateBook, id: NodeId) -> TemplateBook {
        let mut v = serde_json::to_value(book).unwrap();
        let key = id.to_string();
        let node = v["nodes"]
            .as_object_mut()
            .and_then(|nodes| nodes.get_mut(&key))
            .expect("node in serialized book");
        node.as_object_mut().unwrap().remove("updated_at");
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn stale_drafts_reports_a_draft_never_updated() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let d = add(&mut b, Some(s), "old draft", NodeType::Content);
        set_status(&mut b, d, NodeStatus::Draft);
        let b = without_updated_at(&b, d);
        assert!(b.get_node(d).unwrap().updated_at().is_none());
        let f = lint(&b, &only(LintCheck::StaleDrafts)).findings;
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].nodes, vec![d]);
        assert!(f[0].detail.contains("never updated"), "{}", f[0].detail);
    }

    #[test]
    fn omitted_lists_every_check_that_ran() {
        let mut b = TemplateBook::new("B", 4);
        add(&mut b, None, "Empty", NodeType::Section);
        let r = lint(&b, &LintOptions::default());
        assert_eq!(r.omitted.len(), LintCheck::ALL.len());
        assert!(r.omitted.values().all(|&n| n == 0), "{:?}", r.omitted);

        let r = lint(&b, &only(LintCheck::RootContent));
        assert_eq!(
            r.omitted.into_iter().collect::<Vec<_>>(),
            [(LintCheck::RootContent, 0)]
        );
    }

    #[test]
    fn limit_caps_findings_and_counts_the_rest() {
        let mut b = TemplateBook::new("B", 4);
        for i in 0..5 {
            add(&mut b, None, &format!("Empty {i}"), NodeType::Section);
        }
        let r = lint(
            &b,
            &LintOptions {
                checks: vec![LintCheck::EmptySections],
                limit: 2,
                ..Default::default()
            },
        );
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.omitted.get(&LintCheck::EmptySections), Some(&3));
    }

    #[test]
    fn checks_parse_round_trips_and_rejects_unknown() {
        for c in LintCheck::ALL {
            assert_eq!(LintCheck::parse(c.as_str()), Some(c));
        }
        assert_eq!(LintCheck::parse("nope"), None);
    }

    #[test]
    fn similar_titles_is_fast_on_1200_nodes() {
        let words = [
            "git", "branch", "commit", "rust", "test", "review", "release", "deploy", "docs",
            "lint", "mcp", "agent", "tool", "book", "node", "snapshot", "history", "query",
            "search", "import", "export", "fly", "postgres", "schema", "index", "cache",
        ];
        let mut b = TemplateBook::new("Big", 6);
        let mut sections = Vec::new();
        for s in 0..24 {
            sections.push(add(
                &mut b,
                None,
                &format!("Section {s}"),
                NodeType::Section,
            ));
        }
        let mut seed = 12345u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for i in 0..1176 {
            let title = format!(
                "{} {} {} {} {i}",
                words[next() % words.len()],
                words[next() % words.len()],
                words[next() % words.len()],
                words[next() % words.len()]
            );
            let parent = sections[i % sections.len()];
            add(&mut b, Some(parent), &title, NodeType::Content);
        }
        assert_eq!(b.node_count(), 1200);
        let start = std::time::Instant::now();
        let r = lint(&b, &LintOptions::default());
        let elapsed = start.elapsed();
        eprintln!(
            "lint over 1200 nodes: {elapsed:?}, {} findings, omitted {:?}",
            r.findings.len(),
            r.omitted
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "lint took {elapsed:?}"
        );
    }

    #[test]
    fn stats_on_a_fixed_book() {
        let mut b = TemplateBook::new("B", 4);
        let s1 = add(&mut b, None, "S1", NodeType::Section);
        let s2 = add(&mut b, None, "S2", NodeType::Section);
        let c1 = add(&mut b, Some(s1), "c1", NodeType::Content);
        add(&mut b, Some(s1), "c2", NodeType::Content);
        let inner = add(&mut b, Some(s1), "inner", NodeType::Section);
        add(&mut b, Some(inner), "deep", NodeType::Content);
        with_props(&mut b, c1, &[("inject", "true"), ("scope", "x")]);
        set_status(&mut b, s2, NodeStatus::Draft);

        let now = Timestamp::now();
        let st = stats(&b, now);
        assert_eq!(st.nodes, 6);
        assert_eq!(st.sections, 3);
        assert_eq!(st.contents, 3);
        assert_eq!(st.max_depth, 3);
        // depths: S1 1, S2 1, c1 2, c2 2, inner 2, deep 3 → 11 / 6
        assert_eq!(st.mean_depth, 1.83);
        assert_eq!(st.children.max, 3);
        // S1 3, S2 0, inner 1 → 4 / 3
        assert_eq!(st.children.mean, 1.33);
        assert_eq!(st.children.bucket("0"), 1);
        assert_eq!(st.children.bucket("1"), 1);
        assert_eq!(st.children.bucket("2-5"), 1);
        assert_eq!(st.children.bucket(">50"), 0);
        let order: Vec<&str> = st.children.histogram.iter().map(|b| b.children).collect();
        assert_eq!(order, ["0", "1", "2-5", "6-10", "11-50", ">50"]);
        assert_eq!(st.by_status["active"], 5);
        assert_eq!(st.by_status["draft"], 1);
        assert_eq!(st.injected, 1);
        assert_eq!(st.updated.last_7d, 6);
        assert_eq!(st.property_keys, 2);
    }

    #[test]
    fn stats_on_an_empty_book() {
        let st = stats(&TemplateBook::new("E", 4), Timestamp::now());
        assert_eq!(st.nodes, 0);
        assert_eq!(st.max_depth, 0);
        assert_eq!(st.mean_depth, 0.0);
        assert_eq!(st.children.mean, 0.0);
        // Every status key is present, at 0.
        assert_eq!(
            st.by_status.into_iter().collect::<Vec<_>>(),
            [
                ("active".to_string(), 0),
                ("archived".to_string(), 0),
                ("draft".to_string(), 0)
            ]
        );
    }

    #[test]
    fn lint_ignores_archived_nodes_and_their_subtrees() {
        let mut b = TemplateBook::new("B", 4);
        let live = add(&mut b, None, "Live", NodeType::Section);
        let old = add(
            &mut b,
            Some(live),
            "Branch strategy guide",
            NodeType::Content,
        );
        add(
            &mut b,
            Some(live),
            "Branch strategy guide",
            NodeType::Content,
        );
        set_status(&mut b, old, NodeStatus::Archived);
        // An archived empty section, and an archived section holding a
        // content node at its root level and a stale-looking draft.
        let gone = add(&mut b, None, "Gone", NodeType::Section);
        set_status(&mut b, gone, NodeStatus::Archived);
        let attic = add(&mut b, None, "Attic", NodeType::Section);
        let d = add(&mut b, Some(attic), "Old draft", NodeType::Content);
        with_props(&mut b, d, &[("added_by", "x")]);
        set_status(&mut b, d, NodeStatus::Draft);
        set_status(&mut b, attic, NodeStatus::Archived);
        let root_c = add(&mut b, None, "Loose", NodeType::Content);
        set_status(&mut b, root_c, NodeStatus::Archived);

        let r = lint(
            &b,
            &LintOptions {
                stale_days: 0,
                ..LintOptions::default()
            },
        );
        assert!(
            kinds(&r, LintCheck::SimilarTitles).is_empty(),
            "an archived original must not pair with its replacement: {r:?}"
        );
        assert!(kinds(&r, LintCheck::EmptySections).is_empty(), "{r:?}");
        assert!(kinds(&r, LintCheck::RootContent).is_empty(), "{r:?}");
        let stale = without_updated_at(&b, d);
        let r = lint(&stale, &only(LintCheck::StaleDrafts));
        assert!(
            kinds(&r, LintCheck::StaleDrafts).is_empty(),
            "a draft under an archived section is retired: {r:?}"
        );
    }

    #[test]
    fn stats_does_not_count_inject_under_an_archived_section() {
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "Old rules", NodeType::Section);
        let c = add(&mut b, Some(s), "Rule", NodeType::Content);
        with_props(&mut b, c, &[("inject", "true")]);
        set_status(&mut b, s, NodeStatus::Archived);
        let st = stats(&b, Timestamp::now());
        assert_eq!(st.injected, 0);
    }

    #[test]
    fn stats_counts_archived_and_injects_only_active_nodes() {
        let mut b = TemplateBook::new("B", 4);
        let a = add(&mut b, None, "a", NodeType::Content);
        let r = add(&mut b, None, "r", NodeType::Content);
        for id in [a, r] {
            with_props(&mut b, id, &[("inject", "true")]);
        }
        set_status(&mut b, r, NodeStatus::Archived);
        let st = stats(&b, Timestamp::now());
        assert_eq!(st.by_status["archived"], 1);
        assert_eq!(st.by_status["active"], 1);
        assert_eq!(st.injected, 1, "an archived node is not injected");
    }

    #[test]
    fn stats_updated_buckets_at_their_boundaries() {
        let mut b = TemplateBook::new("B", 4);
        let n = add(&mut b, None, "n", NodeType::Content);
        let at = b.get_node(n).unwrap().updated_at().unwrap().as_millis();
        let bucket_at = |age_ms: i64| {
            let u = stats(&b, Timestamp::from_millis(at + age_ms)).updated;
            match (u.last_7d, u.last_30d, u.last_90d, u.older, u.unknown) {
                (1, 0, 0, 0, 0) => "7d",
                (0, 1, 0, 0, 0) => "30d",
                (0, 0, 1, 0, 0) => "90d",
                (0, 0, 0, 1, 0) => "older",
                other => panic!("node counted in several buckets: {other:?}"),
            }
        };
        assert_eq!(bucket_at(0), "7d");
        assert_eq!(bucket_at(7 * DAY_MS), "7d");
        assert_eq!(bucket_at(7 * DAY_MS + 1), "30d");
        assert_eq!(bucket_at(30 * DAY_MS), "30d");
        assert_eq!(bucket_at(30 * DAY_MS + 1), "90d");
        assert_eq!(bucket_at(90 * DAY_MS), "90d");
        assert_eq!(bucket_at(90 * DAY_MS + 1), "older");

        let b = without_updated_at(&b, n);
        assert_eq!(stats(&b, Timestamp::now()).updated.unknown, 1);
    }

    /// `book` with node `gone` cut out of the serialized tree but every link
    /// to it left in place — what restoring data from elsewhere can leave.
    fn without_node_keeping_links(book: &TemplateBook, gone: NodeId) -> TemplateBook {
        let mut v = serde_json::to_value(book).unwrap();
        let key = gone.to_string();
        v["nodes"].as_object_mut().unwrap().remove(&key);
        v["root_nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|x| x.as_str() != Some(key.as_str()));
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn dangling_links_reports_links_to_missing_nodes_only() {
        let mut b = TemplateBook::new("B", 4);
        let a = add(&mut b, None, "A", NodeType::Section);
        let t = add(&mut b, None, "Target", NodeType::Section);
        let k = add(&mut b, None, "Kept", NodeType::Section);
        b.add_link(a, t, "supersedes").unwrap();
        b.add_link(a, k, "see_also").unwrap();
        assert!(lint(&b, &only(LintCheck::DanglingLinks))
            .findings
            .is_empty());

        let b = without_node_keeping_links(&b, t);
        let f = lint(&b, &only(LintCheck::DanglingLinks)).findings;
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].nodes, vec![a]);
        assert!(f[0].detail.contains("supersedes") && f[0].detail.contains(&t.to_string()));
        assert!(
            LintCheck::ALL.contains(&LintCheck::DanglingLinks),
            "in the default set"
        );
        assert_eq!(
            LintCheck::parse("dangling_links"),
            Some(LintCheck::DanglingLinks)
        );
    }

    #[test]
    fn stats_counts_links() {
        let mut b = TemplateBook::new("B", 4);
        let a = add(&mut b, None, "A", NodeType::Section);
        let c = add(&mut b, None, "C", NodeType::Section);
        assert_eq!(stats(&b, Timestamp::now()).links, 0);
        b.add_link(a, c, "x").unwrap();
        b.add_link(c, a, "y").unwrap();
        assert_eq!(stats(&b, Timestamp::now()).links, 2);
    }

    fn revision(at: Timestamp) -> crate::domain::model::node::Revision {
        crate::domain::model::node::Revision {
            title: Some("New title".to_string()),
            body: None,
            properties: None,
            properties_patch: None,
            base: String::new(),
            by: Some("alice".to_string()),
            note: None,
            at,
        }
    }

    #[test]
    fn stale_proposals_reports_old_and_outdated_revisions_only() {
        let now = Timestamp::now();
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let fresh = add(&mut b, Some(s), "Fresh", NodeType::Content);
        let old = add(&mut b, Some(s), "Old", NodeType::Content);
        let changed = add(&mut b, Some(s), "Changed", NodeType::Content);
        b.propose_revision(fresh, revision(now), false).unwrap();
        b.propose_revision(
            old,
            revision(Timestamp::from_millis(now.as_millis() - 100 * DAY_MS)),
            false,
        )
        .unwrap();
        b.propose_revision(changed, revision(now), false).unwrap();
        b.update_node(
            changed,
            crate::domain::model::book::UpdateNodeRequest {
                title: None,
                body: Some(Some("edited meanwhile".to_string())),
                node_type: None,
                placeholder: None,
                properties: None,
                properties_patch: None,
                status: None,
            },
        )
        .unwrap();

        let r = lint(
            &b,
            &LintOptions {
                checks: vec![LintCheck::StaleProposals],
                now,
                ..Default::default()
            },
        );
        let f = kinds(&r, LintCheck::StaleProposals);
        let ids: Vec<NodeId> = f.iter().map(|f| f.nodes[0]).collect();
        assert_eq!(ids, vec![old, changed], "{f:?}");
        assert!(f[0].detail.contains("100 days ago"), "{}", f[0].detail);
        assert!(f[1].detail.contains("changed after"), "{}", f[1].detail);
        assert!(LintCheck::ALL.contains(&LintCheck::StaleProposals));
        assert_eq!(
            LintCheck::parse("stale_proposals"),
            Some(LintCheck::StaleProposals)
        );
        assert_eq!(stats(&b, now).pending_proposals, 3);
    }

    #[test]
    fn stale_proposals_reports_a_revision_left_on_an_archived_node() {
        let now = Timestamp::now();
        let mut b = TemplateBook::new("B", 4);
        let s = add(&mut b, None, "S", NodeType::Section);
        let n = add(&mut b, Some(s), "N", NodeType::Content);
        b.propose_revision(n, revision(now), false).unwrap();
        b.update_node(
            s,
            crate::domain::model::book::UpdateNodeRequest {
                title: None,
                body: None,
                node_type: None,
                placeholder: None,
                properties: None,
                properties_patch: None,
                status: Some(NodeStatus::Archived),
            },
        )
        .unwrap();

        let r = lint(
            &b,
            &LintOptions {
                checks: vec![LintCheck::StaleProposals],
                now,
                ..Default::default()
            },
        );
        let f = kinds(&r, LintCheck::StaleProposals);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].nodes, vec![n]);
        assert!(
            f[0].detail.contains("on an archived node"),
            "{}",
            f[0].detail
        );
        // The same revision is counted by book_stats.
        assert_eq!(stats(&b, now).pending_proposals, 1);
    }
}
