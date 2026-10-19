//! Markdown to HTML for the browser view ([`crate::ui`]), and the HTML /
//! URL escaping its templates use.
//!
//! A node body is untrusted text, so the renderer produces no active
//! content:
//!
//! - raw HTML in the Markdown (blocks and inline) is shown as escaped text,
//!   never passed through;
//! - a link keeps its `href` only when the URL is relative or uses `http`,
//!   `https` or `mailto`; any other scheme (`javascript:`, `data:`, ...)
//!   drops the link and keeps its text;
//! - an image is never loaded: it becomes a link to its URL (under the same
//!   rule) with the alt text as the label, so viewing a node fetches nothing
//!   from another host.
//!
//! Headings carry an `id` and a `¶` link, generated here from the heading
//! text, never from raw HTML.

use std::collections::{HashMap, HashSet};

/// Bodies larger than this are shown as escaped preformatted text instead of
/// being rendered from Markdown, bounding the work one page view can cause.
pub(crate) const MAX_RENDERED_BODY: usize = 1024 * 1024;

use pulldown_cmark::{html, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

/// Render a Markdown body to an HTML fragment (no surrounding page).
///
/// Every heading gets a GitHub-style `id` (lowercase text, spaces as `-`,
/// punctuation other than `-` and `_` dropped, non-ASCII letters kept; a
/// repeat gets `-1`, `-2`, ...; an empty slug is `section`) and a `¶` link
/// to it.
pub(crate) fn markdown(body: &str) -> String {
    if body.len() > MAX_RENDERED_BODY {
        return format!(
            "<p class=\"meta\">This body is {} bytes, over the {}-byte rendering limit; it is shown as plain text.</p><pre>{}</pre>",
            body.len(),
            MAX_RENDERED_BODY,
            escape(body)
        );
    }
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parsed: Vec<Event<'_>> = Parser::new_ext(body, opts).collect();
    let ids = heading_ids(&parsed);
    let mut next_id = ids.iter();
    let mut current: Option<&String> = None;
    // For each open link / image: was its start event kept?
    let mut open: Vec<bool> = Vec::new();
    let events = parsed.into_iter().filter_map(|ev| match ev {
        Event::Start(Tag::Heading { level, .. }) => {
            current = next_id.next();
            let id = current.map(String::as_str).unwrap_or("section");
            Some(Event::Html(
                format!("<h{} id=\"{}\">", heading_level(level), escape(id)).into(),
            ))
        }
        Event::End(TagEnd::Heading(level)) => {
            let mut tail = String::new();
            if let Some(id) = current.take() {
                tail.push_str(&format!(
                    " <a class=\"anchor\" href=\"#{}\">¶</a>",
                    escape(id)
                ));
            }
            tail.push_str(&format!("</h{}>\n", heading_level(level)));
            Some(Event::Html(tail.into()))
        }
        Event::Html(s) | Event::InlineHtml(s) => Some(Event::Text(s)),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let keep = safe_href(&dest_url);
            open.push(keep);
            keep.then_some(Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }))
        }
        Event::Start(Tag::Image {
            dest_url,
            title,
            id,
            ..
        }) => {
            let keep = safe_href(&dest_url);
            open.push(keep);
            keep.then_some(Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                title,
                id,
            }))
        }
        Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => open
            .pop()
            .unwrap_or(false)
            .then_some(Event::End(TagEnd::Link)),
        other => Some(other),
    });
    let mut out = String::with_capacity(body.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

/// The de-duplicated id of every heading, in order.
fn heading_ids(events: &[Event<'_>]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    // Next suffix to try per base slug, so N repeats of one heading cost
    // O(N) overall instead of O(N²). The `used` check still guards against a
    // heading whose own text already reads like a suffixed id (`a-1`).
    let mut next_suffix: HashMap<String, usize> = HashMap::new();
    let mut text: Option<String> = None;
    for ev in events {
        match ev {
            Event::Start(Tag::Heading { .. }) => text = Some(String::new()),
            Event::End(TagEnd::Heading(_)) => {
                let base = slug(&text.take().unwrap_or_default());
                let id = if used.insert(base.clone()) {
                    base
                } else {
                    let n = next_suffix.entry(base.clone()).or_insert(1);
                    loop {
                        let candidate = format!("{base}-{n}");
                        *n += 1;
                        if used.insert(candidate.clone()) {
                            break candidate;
                        }
                    }
                };
                ids.push(id);
            }
            Event::Text(s) | Event::Code(s) | Event::Html(s) | Event::InlineHtml(s) => {
                if let Some(t) = text.as_mut() {
                    t.push_str(s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(t) = text.as_mut() {
                    t.push(' ');
                }
            }
            _ => {}
        }
    }
    ids
}

/// GitHub-style slug of heading text (before de-duplication).
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == '-' || c == '_' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "section".to_string()
    } else {
        out
    }
}

fn heading_level(l: HeadingLevel) -> usize {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// `true` when `url` is relative (no scheme) or its scheme is `http`,
/// `https` or `mailto`. Browsers ignore ASCII whitespace and control
/// characters inside a scheme (`java\tscript:`), so those are removed
/// before comparing.
pub(crate) fn safe_href(url: &str) -> bool {
    let Some(colon) = url.find(':') else {
        return true;
    };
    // A ':' after the first '/', '?' or '#' is part of a relative URL.
    if url[..colon].contains(['/', '?', '#']) {
        return true;
    }
    let scheme: String = url[..colon]
        .chars()
        .filter(|c| !(c.is_ascii_whitespace() || c.is_ascii_control()))
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(scheme.as_str(), "http" | "https" | "mailto")
}

/// Escape text for an HTML element body or a double-quoted attribute.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encode a query-string value (everything but RFC 3986
/// unreserved characters).
pub(crate) fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_html_is_escaped_text() {
        let out = markdown("# a\n<script>x</script>");
        assert!(out.contains(r#"<h1 id="a">a "#), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(!out.contains("<script>"), "{out}");
    }

    #[test]
    fn inline_html_is_escaped_text() {
        let out = markdown("hi <img src=x onerror=alert(1)> there");
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains("&lt;img"), "{out}");
    }

    #[test]
    fn unsafe_links_lose_their_href() {
        let out = markdown("[x](javascript:alert(1))");
        assert!(!out.contains("href"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("javascript"), "{out}");
        let out = markdown("[y](<JaVa\tScript:alert(1)>)");
        assert!(!out.contains("href"), "{out}");
        let out = markdown("[ok](https://example.com/a)");
        assert!(out.contains(r#"href="https://example.com/a""#), "{out}");
    }

    #[test]
    fn images_become_links_and_load_nothing() {
        let out = markdown("![alt](https://example.com/i.png)");
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains(r#"href="https://example.com/i.png""#), "{out}");
        assert!(out.contains("alt"), "{out}");
    }

    #[test]
    fn heading_ids_are_unique() {
        let out = markdown("# A\n# A\n## 設計 方針\n");
        assert!(out.contains(r#"id="a""#), "{out}");
        assert!(out.contains(r#"id="a-1""#), "{out}");
        assert!(out.contains(r#"id="設計-方針""#), "{out}");
    }

    #[test]
    fn literal_suffixed_heading_does_not_collide() {
        // "a-1" written literally, then two more "a": ids stay unique.
        let ids = heading_ids(&Parser::new("# a\n# a-1\n# a\n# a\n").collect::<Vec<_>>());
        assert_eq!(ids, vec!["a", "a-1", "a-2", "a-3"]);
    }

    #[test]
    fn many_identical_headings_are_fast_and_unique() {
        let body = "# a\n".repeat(50_000);
        let started = std::time::Instant::now();
        let ids = heading_ids(&Parser::new(&body).collect::<Vec<_>>());
        let elapsed = started.elapsed();
        assert!(elapsed.as_secs_f64() < 1.0, "took {elapsed:?}");
        assert_eq!(ids.len(), 50_000);
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 50_000);
    }

    #[test]
    fn oversized_body_is_plain_escaped_text() {
        let body = format!("# t\n<script>x</script>\n{}", "a".repeat(MAX_RENDERED_BODY));
        let out = markdown(&body);
        assert!(out.contains("rendering limit"), "{}", &out[..200]);
        assert!(!out.contains("<h1"));
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn autolinks_and_nested_links_never_keep_javascript() {
        for src in [
            "<javascript:alert(1)>",
            "[![x](https://a)](javascript:b)",
            "[![x](javascript:a)](https://b)",
        ] {
            let out = markdown(src).to_ascii_lowercase();
            assert!(!out.contains("href=\"javascript"), "{src}: {out}");
            assert!(!out.contains("<img"), "{src}: {out}");
        }
    }

    #[test]
    fn escape_and_query_value() {
        assert_eq!(
            escape(r#"<a href="x">'&"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;"
        );
        assert_eq!(query_value("a b/設"), "a%20b%2F%E8%A8%AD");
    }
}
