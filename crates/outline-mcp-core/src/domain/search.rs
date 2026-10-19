//! Case-insensitive substring search over node titles and bodies.
//!
//! Matching works on `char`s with Unicode lowercasing, so CJK text (which
//! has no case) matches as plain substrings and Latin text matches
//! regardless of case. Books are loaded whole per call, so a linear scan is
//! all a query needs; there is no index.

use super::model::node::TemplateNode;

/// A prepared, lowercased search needle.
#[derive(Debug, Clone)]
pub struct TextQuery {
    needle: Vec<char>,
}

impl TextQuery {
    /// Prepare `text` for matching. `None` when it is empty after trimming —
    /// an empty needle would match everything, which is never what a caller
    /// meant.
    pub fn new(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Lowercase char by char, exactly as `match_at` lowercases the
        // haystack. `str::to_lowercase` is context-sensitive (a word-final
        // 'Σ' becomes 'ς'), `char::to_lowercase` is not ('Σ' is always 'σ'),
        // so mixing the two would miss e.g. "ΟΔΟΣ" in "ΟΔΟΣ".
        Some(Self {
            needle: trimmed.chars().flat_map(char::to_lowercase).collect(),
        })
    }

    /// Whether the node's title or body contains the needle.
    pub fn matches(&self, node: &TemplateNode) -> bool {
        self.find(node.title()).is_some() || node.body().is_some_and(|b| self.find(b).is_some())
    }

    /// A short excerpt (at most `max_chars` characters, newlines flattened)
    /// around the first match in the node's body; `None` when only the title
    /// matched or the node has no body.
    pub fn snippet(&self, node: &TemplateNode, max_chars: usize) -> Option<String> {
        let body = node.body()?;
        let chars: Vec<char> = body.chars().collect();
        let (start, len) = self.find_chars(&chars)?;
        Some(excerpt(&chars, start, len, max_chars))
    }

    /// Char offset and char length of the first match in `hay`.
    pub fn find(&self, hay: &str) -> Option<(usize, usize)> {
        let chars: Vec<char> = hay.chars().collect();
        self.find_chars(&chars)
    }

    fn find_chars(&self, hay: &[char]) -> Option<(usize, usize)> {
        (0..hay.len()).find_map(|start| self.match_at(hay, start).map(|len| (start, len)))
    }

    /// Number of `hay` chars the needle covers when matched at `start`.
    fn match_at(&self, hay: &[char], start: usize) -> Option<usize> {
        let mut ni = 0;
        let mut hi = start;
        while ni < self.needle.len() {
            let c = hay.get(hi)?;
            for lc in c.to_lowercase() {
                if self.needle.get(ni) != Some(&lc) {
                    return None;
                }
                ni += 1;
            }
            hi += 1;
        }
        Some(hi - start)
    }
}

/// At most `max_chars` characters in total, ellipses included (unless the
/// match itself is longer, in which case the match is returned whole).
fn excerpt(chars: &[char], start: usize, len: usize, max_chars: usize) -> String {
    if chars.len() <= max_chars {
        return chars
            .iter()
            .map(|&c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
    }
    // Leave room for a leading and a trailing ellipsis.
    let window = max_chars.saturating_sub(2).max(len);
    let lead = (window - len) / 3;
    let from = start
        .saturating_sub(lead)
        .min(chars.len().saturating_sub(window));
    let to = (from + window).min(chars.len());
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.extend(
        chars[from..to]
            .iter()
            .map(|&c| if c == '\n' || c == '\r' { ' ' } else { c }),
    );
    if to < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> TextQuery {
        TextQuery::new(s).expect("non-empty")
    }

    #[test]
    fn empty_or_blank_query_is_rejected() {
        assert!(TextQuery::new("").is_none());
        assert!(TextQuery::new("   ").is_none());
    }

    #[test]
    fn case_insensitive_latin() {
        assert_eq!(q("branch").find("Git Branch Strategy"), Some((4, 6)));
        assert_eq!(q("BRANCH").find("git branch"), Some((4, 6)));
        assert!(q("merge").find("Git Branch").is_none());
    }

    #[test]
    fn cjk_substring() {
        assert_eq!(q("運用").find("Git運用ルール"), Some((3, 2)));
        assert!(q("運転").find("Git運用ルール").is_none());
    }

    #[test]
    fn excerpt_is_bounded_and_flattens_newlines() {
        let body: String = "a".repeat(100) + "\nneedle\n" + &"b".repeat(100);
        let chars: Vec<char> = body.chars().collect();
        let (start, len) = q("NEEDLE").find(&body).unwrap();
        let s = excerpt(&chars, start, len, 40);
        assert!(s.contains("needle"), "{s}");
        assert!(!s.contains('\n'), "{s}");
        assert!(s.starts_with('…') && s.ends_with('…'), "{s}");
        // 40 chars in total, the two ellipses included.
        assert_eq!(s.chars().count(), 40);
    }

    #[test]
    fn greek_final_sigma_matches_itself() {
        // `str::to_lowercase("ΟΔΟΣ")` is "οδος" (final ς) while per-char
        // lowercasing gives "οδοσ"; needle and haystack must agree.
        assert_eq!(q("ΟΔΟΣ").find("ΟΔΟΣ"), Some((0, 4)));
        assert_eq!(q("οδοσ").find("Η ΟΔΟΣ"), Some((2, 4)));
    }

    #[test]
    fn excerpt_near_the_end_of_a_long_body_stays_bounded() {
        let body: String = "x".repeat(500) + "tail match";
        let chars: Vec<char> = body.chars().collect();
        let (start, len) = q("MATCH").find(&body).unwrap();
        let s = excerpt(&chars, start, len, 80);
        assert!(s.chars().count() <= 80, "{} chars: {s}", s.chars().count());
        assert!(s.ends_with("tail match"), "{s}");
        assert!(s.starts_with('…'), "{s}");
    }

    #[test]
    fn short_body_is_returned_whole() {
        let chars: Vec<char> = "short\nbody".chars().collect();
        assert_eq!(excerpt(&chars, 6, 4, 80), "short body");
    }
}
