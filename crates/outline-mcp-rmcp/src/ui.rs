//! A read-only browser view of the books, served by `--mcp-http` next to
//! `/mcp`.
//!
//! It is a projection for a human reading books, not part of the MCP
//! surface: it reads through the same [`Storage`] the tools use, has no route
//! that writes, and adds nothing to the store.
//!
//! | route (GET) | shows |
//! |---|---|
//! | `/ui/` | the shelf (slug, title, active node count — `active/all` with `?totals=1` —, last update) and a search form; archived books hidden unless `?archived=1` |
//! | `/ui/search?book=&q=` | `303 See Other` to the book's search page (the shelf form posts here) |
//! | `/ui/book/{slug}` | the book's table of contents with markers; archived subtrees hidden unless `?archived=1` |
//! | `/ui/book/{slug}/node/{uuid}` | one node: breadcrumbs, meta, properties, rendered body, links both ways, pending revision with its diff, children, history |
//! | `/ui/book/{slug}/search?q=` | nodes whose title or body contains `q` (case-insensitive), with snippets |
//! | `/ui/book/{slug}/history?limit=` | the book's history, newest first |
//!
//! Access: when `OUTLINE_MCP_HTTP_TOKEN` is set, every `/ui` request needs
//! HTTP Basic with that token as the password (any user name); otherwise
//! `401` with `WWW-Authenticate: Basic realm="outline-mcp"`. Without a token
//! (loopback binds only) the view accepts only a loopback `Host` and answers
//! `403` to anything else, the DNS-rebinding guard `/mcp` gets from rmcp's
//! `allowed_hosts`.
//!
//! Pages are plain HTML with inline CSS: no script, no external asset, a
//! `Content-Security-Policy` that forbids both, `Cache-Control: no-store`
//! and `Referrer-Policy: no-referrer`. Everything taken from a book is
//! HTML-escaped; node bodies go through [`crate::render::markdown`], which
//! shows raw HTML as text.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use outline_mcp_core::domain::model::book::TemplateBook;
use outline_mcp_core::domain::model::changelog::{ChangeAction, NodeStatus};
use outline_mcp_core::domain::model::id::NodeId;
use outline_mcp_core::domain::model::node::{NodeType, TemplateNode};
use outline_mcp_core::domain::model::timestamp::Timestamp;
use outline_mcp_core::domain::search::TextQuery;
use outline_mcp_core::domain::store::{StoreError, Versioned};
use serde::Deserialize;

use crate::helpers::{build_hierarchical_ids, describe_book_update, endpoint_status};
use crate::render::{escape, markdown, query_value};
use crate::request::parse_node_id;
use crate::revision::{proposal_line, revision_diff};
use crate::server::Storage;
use crate::tools::action_name;

/// Default and upper bound of history rows on one page.
const HISTORY_DEFAULT: usize = 100;
const HISTORY_MAX: usize = 1000;
/// Characters of body shown around a search match.
const SNIPPET_CHARS: usize = 160;
/// Search hits listed on one page.
const RESULT_LIMIT: usize = 200;
/// Books whose node count / last update the shelf computes (each needs a load).
const SHELF_LOAD_MAX: usize = 200;
/// History entries shown on a node page.
const NODE_HISTORY_MAX: usize = 100;

#[derive(Clone)]
struct Ui {
    storage: Storage,
}

/// The `/ui` routes, with Basic auth in front when `token` is set and a
/// loopback-`Host` check in front when it is not. Merge it into the HTTP
/// router outside the `/mcp` bearer layer; it owns its full paths.
///
/// Both the access check and the security headers are `route_layer`s: they
/// wrap only the `/ui` routes (including the `/ui/{*rest}` catch-all, so an
/// unknown `/ui` path is a 404 page behind the same checks), and never this
/// router's fallback. Merging therefore does not change how any path outside
/// `/ui` answers.
pub(crate) fn router(storage: Storage, token: Option<Arc<str>>) -> Router {
    let r = Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/search", get(shelf_search))
        .route("/ui/book/{slug}", get(book))
        .route("/ui/book/{slug}/", get(book))
        .route("/ui/book/{slug}/node/{id}", get(node))
        .route("/ui/book/{slug}/search", get(book_search))
        .route("/ui/book/{slug}/history", get(history))
        .route("/ui/{*rest}", get(not_found))
        .with_state(Ui { storage });
    let r = match token {
        Some(token) => r.route_layer(axum::middleware::from_fn(
            move |req: Request, next: Next| {
                let token = Arc::clone(&token);
                async move { require_basic(token, req, next).await }
            },
        )),
        None => r.route_layer(axum::middleware::from_fn(require_loopback_host)),
    };
    // Outermost, so 401 / 403 / 405 answers get the headers too.
    r.route_layer(axum::middleware::from_fn(security_headers))
}

/// An unknown path under `/ui`.
async fn not_found() -> UiResult {
    Err(UiError::NotFound("No such page.".into()))
}

/// Set the page security headers on every `/ui` response, whatever produced
/// it (a page, an error page, the access checks, a 405).
async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP_VALUE),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    resp
}

/// The page `Content-Security-Policy`: no script, no external asset.
const CSP_VALUE: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

// --- access -----------------------------------------------------------------

/// Without a token: pass only requests whose `Host` (or, when there is no
/// `Host` header as with HTTP/2, the request URI's authority) is loopback.
async fn require_loopback_host(req: Request, next: Next) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(v) => v.to_str().ok(),
        None => req.uri().authority().map(|a| a.as_str()),
    };
    if host.is_some_and(loopback_host) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden: host not allowed").into_response()
    }
}

/// `true` when a `Host` value names this machine: `localhost`, `127.0.0.1`
/// or `::1` (bare or as `[::1]`), with or without a numeric `:port`.
/// Case-insensitive. Anything else, including other `127.0.0.0/8`
/// addresses, a malformed port or trailing junk, is rejected.
pub(crate) fn loopback_host(value: &str) -> bool {
    fn port_ok(p: &str) -> bool {
        !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
    }
    if let Some(rest) = value.strip_prefix('[') {
        // brackets are only for an IPv6 literal
        let Some((inner, after)) = rest.split_once(']') else {
            return false;
        };
        let port_fine = after.is_empty() || after.strip_prefix(':').is_some_and(port_ok);
        return port_fine && inner == "::1";
    }
    let host = if value == "::1" {
        value
    } else {
        match value.split_once(':') {
            None => value,
            Some((h, p)) if port_ok(p) => h,
            Some(_) => return false,
        }
    };
    ["localhost", "127.0.0.1", "::1"]
        .iter()
        .any(|ok| host.eq_ignore_ascii_case(ok))
}

async fn require_basic(token: Arc<str>, req: Request, next: Next) -> Response {
    if basic_password_matches(req.headers(), &token) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(r#"Basic realm="outline-mcp", charset="UTF-8""#),
            )],
            "authentication required",
        )
            .into_response()
    }
}

/// Whether the request carries `Authorization: Basic <user:password>` whose
/// password equals `token` (any user name), compared in constant time.
fn basic_password_matches(headers: &axum::http::HeaderMap, token: &str) -> bool {
    let Some(encoded) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("basic").then_some(rest.trim())
        })
    else {
        return false;
    };
    let Some(decoded) = base64_decode(encoded) else {
        return false;
    };
    let Some(colon) = decoded.iter().position(|b| *b == b':') else {
        return false;
    };
    constant_time_eq(&decoded[colon + 1..], token.as_bytes())
}

/// Compare `presented` with `expected` in time that depends only on the
/// length of `expected` (the server's own token), with no early return on a
/// length mismatch.
fn constant_time_eq(presented: &[u8], expected: &[u8]) -> bool {
    let mut diff = u8::from(presented.len() != expected.len());
    for (i, &e) in expected.iter().enumerate() {
        diff |= presented.get(i).copied().unwrap_or(0) ^ e;
    }
    diff == 0
}

/// Decode standard base64 (RFC 4648) strictly: canonical `=` padding is
/// required and non-zero trailing bits are rejected. `None` on any error.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

// --- errors -----------------------------------------------------------------

enum UiError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl From<StoreError> for UiError {
    fn from(e: StoreError) -> Self {
        UiError::Internal(e.to_string())
    }
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            UiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            UiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            UiError::Internal(m) => {
                tracing::error!(error = %m, "ui request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        let body = format!(
            r#"<nav><a href="/ui/">outline</a></nav><h1>{}</h1><p>{}</p>"#,
            status.as_u16(),
            escape(&msg)
        );
        let mut r = page("error", &body);
        *r.status_mut() = status;
        r
    }
}

type UiResult = Result<Response, UiError>;

/// Slugs are `[A-Za-z0-9_-]+`; anything else is a bad request, so a slug
/// never reaches a page unescaped-unsafe or a store unvalidated.
fn check_slug(slug: &str) -> Result<(), UiError> {
    if !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(UiError::BadRequest(format!("invalid book slug: {slug}")))
    }
}

async fn load(ui: &Ui, slug: &str) -> Result<Versioned<TemplateBook>, UiError> {
    check_slug(slug)?;
    ui.storage
        .books
        .load(slug)
        .await?
        .ok_or_else(|| UiError::NotFound(format!("Book '{slug}' not found.")))
}

// --- handlers ---------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ShelfQuery {
    totals: Option<String>,
    archived: Option<String>,
}

/// The shelf's URL with its two toggles.
fn shelf_url(totals: bool, archived: bool) -> String {
    match (totals, archived) {
        (false, false) => "/ui/".to_string(),
        (true, false) => "/ui/?totals=1".to_string(),
        (false, true) => "/ui/?archived=1".to_string(),
        (true, true) => "/ui/?totals=1&amp;archived=1".to_string(),
    }
}

/// Nodes that are `active` and not under an archived section.
fn active_count(book: &TemplateBook) -> usize {
    book.all_nodes_dfs()
        .iter()
        .filter(|n| n.status() == NodeStatus::Active && !book.is_archived_or_under_archived(n.id()))
        .count()
}

async fn index(State(ui): State<Ui>, Query(q): Query<ShelfQuery>) -> UiResult {
    let books = ui.storage.books.list_books().await?;
    let totals = flag_on(q.totals.as_deref());
    let archived = flag_on(q.archived.as_deref());
    let mut body = String::from("<h1>outline</h1>");
    body.push_str(&shelf_search_form(
        &books.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
        "",
        "",
    ));
    if books.is_empty() {
        body.push_str("<p>No books yet.</p>");
        return Ok(page("outline", &body));
    }
    let hidden = if archived {
        0
    } else {
        books.iter().filter(|b| b.archived).count()
    };
    let books: Vec<_> = books.iter().filter(|b| archived || !b.archived).collect();
    body.push_str(&format!(
        r#"<p class="meta"><a href="{}">{} totals</a> · <a href="{}">{} archived books</a></p>"#,
        shelf_url(!totals, archived),
        if totals { "hide" } else { "show" },
        shelf_url(totals, !archived),
        if archived { "hide" } else { "show" },
    ));
    body.push_str(&format!(
        r#"<table><thead><tr><th>book</th><th>title</th><th class="n">{}</th><th>last update</th></tr></thead><tbody>"#,
        if totals { "active / all" } else { "active" }
    ));
    for (i, b) in books.iter().enumerate() {
        let title = if b.archived {
            format!(
                r#"{} <span class="tag archived">archived</span>"#,
                escape(&b.title)
            )
        } else {
            escape(&b.title)
        };
        // Node counts need the whole book; past SHELF_LOAD_MAX books, list
        // the rest without them so one page view stays bounded.
        if i >= SHELF_LOAD_MAX {
            body.push_str(&format!(
                r#"<tr><td><a href="/ui/book/{slug}">{slug}</a></td><td>{title}</td><td class="n"></td><td></td></tr>"#,
                slug = escape(&b.slug),
            ));
            continue;
        }
        let (count, last) = match ui.storage.books.load(&b.slug).await? {
            Some(v) => {
                let last = v
                    .value
                    .all_nodes_dfs()
                    .iter()
                    .filter_map(|n| n.updated_at())
                    .max();
                let active = active_count(&v.value);
                let count = if totals {
                    format!("{active}/{}", v.value.node_count())
                } else {
                    active.to_string()
                };
                (count, last)
            }
            None => ("0".to_string(), None),
        };
        body.push_str(&format!(
            r#"<tr><td><a href="/ui/book/{slug}">{slug}</a></td><td>{title}</td><td class="n">{count}</td><td>{last}</td></tr>"#,
            slug = escape(&b.slug),
            last = last.map(fmt_time).unwrap_or_default(),
        ));
    }
    body.push_str("</tbody></table>");
    if hidden > 0 {
        body.push_str(&format!(
            r#"<p class="meta">… {hidden} archived books hidden (<a href="{}">show</a>)</p>"#,
            shelf_url(totals, true)
        ));
    }
    if books.len() > SHELF_LOAD_MAX {
        body.push_str(&format!(
            r#"<p class="meta">Node counts and update times are shown for the first {SHELF_LOAD_MAX} books only.</p>"#
        ));
    }
    Ok(page("outline", &body))
}

#[derive(Debug, Default, Deserialize)]
struct ShelfSearch {
    book: Option<String>,
    q: Option<String>,
}

/// The shelf's search form names a book; send the browser to that book's
/// search page.
async fn shelf_search(Query(s): Query<ShelfSearch>) -> UiResult {
    let slug = s.book.unwrap_or_default();
    check_slug(&slug)?;
    let q = s.q.unwrap_or_default();
    Ok(Redirect::to(&format!(
        "/ui/book/{slug}/search?q={}",
        query_value(q.trim())
    ))
    .into_response())
}

#[derive(Debug, Default, Deserialize)]
struct BookQuery {
    archived: Option<String>,
}

fn flag_on(flag: Option<&str>) -> bool {
    matches!(flag, Some("1" | "true" | "yes"))
}

async fn book(
    State(ui): State<Ui>,
    Path(slug): Path<String>,
    Query(q): Query<BookQuery>,
) -> UiResult {
    let v = load(&ui, &slug).await?;
    let book = &v.value;
    let archived = flag_on(q.archived.as_deref());
    let mut body = format!(
        r#"<nav><a href="/ui/">outline</a></nav><h1>{title}</h1><p class="meta">{slug} · {count} nodes · max depth {depth} · version {version}</p><p class="meta"><code class="ref">outline:{slug}</code></p>"#,
        title = escape(book.title()),
        slug = escape(&slug),
        count = book.node_count(),
        depth = book.max_depth(),
        version = v.version,
    );
    body.push_str(&book_links(&slug, archived));
    body.push_str(&search_form(&slug, ""));
    let ids = build_hierarchical_ids(book);
    if ids.is_empty() {
        body.push_str("<p>No nodes yet.</p>");
        return Ok(page(book.title(), &body));
    }
    let mut hidden = 0usize;
    body.push_str(r#"<ul class="toc">"#);
    for (num, id) in &ids {
        let Some(n) = book.get_node(*id) else {
            continue;
        };
        if !archived && book.is_archived_or_under_archived(*id) {
            hidden += 1;
            continue;
        }
        let depth = num.matches('-').count();
        body.push_str(&format!(
            r#"<li style="margin-left:{indent}rem"><span class="num">{num}.</span> <a href="/ui/book/{slug}/node/{id}">{title}</a>{markers}</li>"#,
            indent = depth * 2,
            num = escape(num),
            slug = escape(&slug),
            id = id,
            title = escape(n.title()),
            markers = markers(book, n),
        ));
    }
    body.push_str("</ul>");
    if hidden > 0 {
        body.push_str(&format!(
            r#"<p class="meta">… {hidden} archived nodes hidden (<a href="/ui/book/{}?archived=1">show</a>)</p>"#,
            escape(&slug)
        ));
    }
    Ok(page(book.title(), &body))
}

async fn node(State(ui): State<Ui>, Path((slug, id)): Path<(String, String)>) -> UiResult {
    let v = load(&ui, &slug).await?;
    let book = &v.value;
    let id = parse_node_id(&id)
        .ok()
        .filter(|i| !i.is_book_level())
        .ok_or_else(|| UiError::BadRequest(format!("not a node id: {id}")))?;
    let n = book
        .get_node(id)
        .ok_or_else(|| UiError::NotFound(format!("Node {id} not found in '{slug}'.")))?;
    let num = build_hierarchical_ids(book)
        .into_iter()
        .find(|(_, i)| *i == id)
        .map(|(num, _)| num)
        .unwrap_or_default();

    let mut body = crumbs(book, &slug, n);
    body.push_str(&format!(
        r#"<h1>{title}</h1><p class="meta">{num} · {kind} · {status}{updated}{verified}</p><p class="meta"><code class="ref">outline:{slug}/{id}</code></p>"#,
        title = escape(n.title()),
        num = escape(&num),
        kind = kind_name(n.node_type()),
        status = status_label(book, n),
        updated = n
            .updated_at()
            .map(|t| format!(" · updated {}", fmt_time(t)))
            .unwrap_or_default(),
        verified = match n.verified_at() {
            Some(t) => format!(
                " · verified {}{}",
                fmt_time(t),
                n.verified_by()
                    .map(|b| format!(" by {}", escape(b)))
                    .unwrap_or_default()
            ),
            None => " · never verified".to_string(),
        },
        slug = escape(&slug),
        id = id,
    ));

    if !n.properties().is_empty() {
        let props: BTreeMap<&String, &String> = n.properties().iter().collect();
        body.push_str(r#"<table class="props"><tbody>"#);
        for (k, val) in props {
            body.push_str(&format!(
                "<tr><th>{}</th><td>{}</td></tr>",
                escape(k),
                escape(val)
            ));
        }
        body.push_str("</tbody></table>");
    }

    body.push_str("<article>");
    match n.body() {
        Some(b) if !b.trim().is_empty() => body.push_str(&markdown(b)),
        _ => body.push_str(r#"<p class="meta">(no body)</p>"#),
    }
    if let Some(p) = n.placeholder() {
        body.push_str(&format!(
            r#"<p class="meta">placeholder: {}</p>"#,
            escape(p)
        ));
    }
    body.push_str("</article>");

    if let Some(rev) = n.proposal() {
        body.push_str(&format!(
            r#"<h2>Pending revision</h2><p>{}</p><pre>{}</pre>"#,
            escape(&proposal_line(n, rev, Timestamp::now())),
            escape(&revision_diff(n, rev)),
        ));
    }

    let backlinks = book.backlinks(id);
    if !n.links().is_empty() || !backlinks.is_empty() {
        body.push_str("<h2>Links</h2><ul>");
        for l in n.links() {
            body.push_str(&format!(
                "<li>{} → {}</li>",
                escape(l.rel()),
                endpoint_html(book, &slug, l.target())
            ));
        }
        for (src, l) in &backlinks {
            body.push_str(&format!(
                "<li>{} ← {}</li>",
                escape(l.rel()),
                endpoint_html(book, &slug, *src)
            ));
        }
        body.push_str("</ul>");
    }

    if !n.children().is_empty() {
        body.push_str("<h2>Children</h2><ul>");
        for c in n.children() {
            body.push_str(&format!("<li>{}</li>", endpoint_html(book, &slug, *c)));
        }
        body.push_str("</ul>");
    }

    let mut entries = ui.storage.books.history_by_node(&slug, id).await?;
    entries.reverse();
    if !entries.is_empty() {
        body.push_str(r#"<h2>History</h2><ul class="hist">"#);
        for e in entries.iter().take(NODE_HISTORY_MAX) {
            body.push_str(&format!(
                "<li>{} {}</li>",
                fmt_time(e.timestamp),
                action_name(e.action)
            ));
        }
        body.push_str("</ul>");
        if entries.len() > NODE_HISTORY_MAX {
            body.push_str(&format!(
                r#"<p class="meta">{} older entries not shown.</p>"#,
                entries.len() - NODE_HISTORY_MAX
            ));
        }
    }
    Ok(page(n.title(), &body))
}

#[derive(Debug, Default, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    archived: Option<String>,
}

async fn book_search(
    State(ui): State<Ui>,
    Path(slug): Path<String>,
    Query(sq): Query<SearchQuery>,
) -> UiResult {
    let v = load(&ui, &slug).await?;
    let book = &v.value;
    let archived = flag_on(sq.archived.as_deref());
    let text = sq.q.unwrap_or_default();
    let mut body = format!(
        r#"<nav><a href="/ui/">outline</a> / <a href="/ui/book/{slug}">{slug}</a></nav><h1>Search</h1>"#,
        slug = escape(&slug)
    );
    body.push_str(&search_form(&slug, &text));
    let Some(query) = TextQuery::new(&text) else {
        return Ok(page("search", &body));
    };
    let ids = build_hierarchical_ids(book);
    let mut hits = 0usize;
    let mut skipped_archived = 0usize;
    body.push_str(r#"<ul class="hits">"#);
    for (num, id) in &ids {
        let Some(n) = book.get_node(*id) else {
            continue;
        };
        if !query.matches(n) {
            continue;
        }
        if !archived && book.is_archived_or_under_archived(*id) {
            skipped_archived += 1;
            continue;
        }
        hits += 1;
        if hits > RESULT_LIMIT {
            continue;
        }
        body.push_str(&format!(
            r#"<li><span class="num">{num}.</span> <a href="/ui/book/{slug}/node/{id}">{title}</a>{markers}{snippet}</li>"#,
            num = escape(num),
            slug = escape(&slug),
            id = id,
            title = escape(n.title()),
            markers = markers(book, n),
            snippet = query
                .snippet(n, SNIPPET_CHARS)
                .map(|s| format!("<pre>{}</pre>", escape(&s)))
                .unwrap_or_default(),
        ));
    }
    body.push_str("</ul>");
    if hits == 0 {
        body.push_str("<p>No match.</p>");
    } else if hits > RESULT_LIMIT {
        body.push_str(&format!(
            r#"<p class="meta">{} more matches not shown; narrow the query.</p>"#,
            hits - RESULT_LIMIT
        ));
    }
    if skipped_archived > 0 {
        body.push_str(&format!(
            r#"<p class="meta">… {skipped_archived} archived matches hidden (<a href="/ui/book/{}/search?q={}&amp;archived=1">show</a>)</p>"#,
            escape(&slug),
            escape(&query_value(&text)),
        ));
    }
    Ok(page("search", &body))
}

#[derive(Debug, Default, Deserialize)]
struct HistoryQuery {
    limit: Option<String>,
}

async fn history(
    State(ui): State<Ui>,
    Path(slug): Path<String>,
    Query(hq): Query<HistoryQuery>,
) -> UiResult {
    let v = load(&ui, &slug).await?;
    let book = &v.value;
    let limit = match hq.limit.as_deref() {
        None | Some("") => HISTORY_DEFAULT,
        Some(s) => s
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=HISTORY_MAX).contains(n))
            .ok_or_else(|| UiError::BadRequest(format!("limit must be 1..={HISTORY_MAX}: {s}")))?,
    };
    let mut entries = ui.storage.books.history(&slug).await?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    let total = entries.len();
    entries.truncate(limit);
    let mut body = format!(
        r#"<nav><a href="/ui/">outline</a> / <a href="/ui/book/{slug}">{slug}</a></nav><h1>History</h1><p class="meta">{total} entries</p>"#,
        slug = escape(&slug)
    );
    if entries.is_empty() {
        body.push_str("<p>No history.</p>");
        return Ok(page("history", &body));
    }
    body.push_str(
        r#"<table><thead><tr><th>time</th><th>action</th><th>node</th></tr></thead><tbody>"#,
    );
    for e in &entries {
        let what = if matches!(e.action, ChangeAction::BookUpdate) {
            escape(&describe_book_update(
                e.before.as_deref(),
                e.after.as_deref(),
            ))
        } else if book.get_node(e.node_id).is_some() {
            endpoint_html(book, &slug, e.node_id)
        } else {
            format!(
                "{} <span class=\"meta\">(deleted, {})</span>",
                escape(&entry_title(e.after.as_deref().or(e.before.as_deref()))),
                e.node_id.short()
            )
        };
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            fmt_time(e.timestamp),
            action_name(e.action),
            what
        ));
    }
    body.push_str("</tbody></table>");
    if total > limit {
        body.push_str(&format!(
            r#"<p class="meta">{} older entries not shown (<a href="/ui/book/{}/history?limit={}">show up to {}</a>)</p>"#,
            total - limit,
            escape(&slug),
            HISTORY_MAX.min(total),
            HISTORY_MAX.min(total),
        ));
    }
    Ok(page("history", &body))
}

// --- helpers ----------------------------------------------------------------

fn fmt_time(t: Timestamp) -> String {
    t.to_iso8601()
}

fn kind_name(t: &NodeType) -> &'static str {
    match t {
        NodeType::Section => "section",
        NodeType::Content => "content",
    }
}

/// The node's own status, or `archived (section)` when only an ancestor is
/// archived.
fn status_label(book: &TemplateBook, n: &TemplateNode) -> String {
    if n.status() != NodeStatus::Archived && book.is_archived_or_under_archived(n.id()) {
        format!("{} (under an archived section)", n.status().as_str())
    } else {
        n.status().as_str().to_string()
    }
}

/// The badges after a title in listings: `inject`, status other than
/// active, verification, pending revision, link count.
fn markers(book: &TemplateBook, n: &TemplateNode) -> String {
    let mut out = String::new();
    if n.get_property("inject") == Some("true") {
        out.push_str(r#" <span class="tag">inject</span>"#);
    }
    if n.status() != NodeStatus::Active {
        out.push_str(&format!(
            r#" <span class="tag {0}">{0}</span>"#,
            n.status().as_str()
        ));
    } else if book.is_archived_or_under_archived(n.id()) {
        out.push_str(r#" <span class="tag archived">archived</span>"#);
    }
    if let Some(t) = n.verified_at() {
        let date = fmt_time(t);
        out.push_str(&format!(
            r#" <span class="tag ok">✓ {}</span>"#,
            escape(date.get(..10).unwrap_or(&date))
        ));
    }
    if n.proposal().is_some() {
        out.push_str(r#" <span class="tag rev">✎ revision</span>"#);
    }
    if !n.links().is_empty() {
        out.push_str(&format!(
            r#" <span class="tag">{} link{}</span>"#,
            n.links().len(),
            if n.links().len() == 1 { "" } else { "s" }
        ));
    }
    out
}

/// A link to another node of the book with its status, or a note that it is
/// not in the book.
fn endpoint_html(book: &TemplateBook, slug: &str, id: NodeId) -> String {
    match book.get_node(id) {
        Some(n) => {
            let status = endpoint_status(book, id);
            let tag = if status == "active" {
                String::new()
            } else {
                format!(r#" <span class="tag {0}">{0}</span>"#, status)
            };
            format!(
                r#"<a href="/ui/book/{}/node/{}">{}</a>{}"#,
                escape(slug),
                id,
                escape(n.title()),
                tag
            )
        }
        None => format!(
            r#"<span class="meta">(not in the book, {})</span>"#,
            id.short()
        ),
    }
}

/// The `title` field of a history entry's node JSON.
fn entry_title(json: Option<&str>) -> String {
    json.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v["title"].as_str().map(str::to_string))
        .unwrap_or_else(|| "(untitled)".to_string())
}

fn crumbs(book: &TemplateBook, slug: &str, n: &TemplateNode) -> String {
    let mut chain = Vec::new();
    let mut cur = n.parent();
    while let Some(p) = cur {
        match book.get_node(p) {
            Some(pn) => {
                chain.push(pn);
                cur = pn.parent();
            }
            None => break,
        }
    }
    chain.reverse();
    let mut out = format!(
        r#"<nav><a href="/ui/">outline</a> / <a href="/ui/book/{0}">{0}</a>"#,
        escape(slug)
    );
    for p in chain {
        out.push_str(&format!(
            r#" / <a href="/ui/book/{}/node/{}">{}</a>"#,
            escape(slug),
            p.id(),
            escape(p.title())
        ));
    }
    out.push_str("</nav>");
    out
}

fn book_links(slug: &str, archived: bool) -> String {
    let toggle = if archived {
        format!(r#"<a href="/ui/book/{}">hide archived</a>"#, escape(slug))
    } else {
        format!(
            r#"<a href="/ui/book/{}?archived=1">show archived</a>"#,
            escape(slug)
        )
    };
    format!(
        r#"<p class="meta"><a href="/ui/book/{}/history">history</a> · {}</p>"#,
        escape(slug),
        toggle
    )
}

fn search_form(slug: &str, q: &str) -> String {
    format!(
        r#"<form action="/ui/book/{}/search" method="get"><input type="search" name="q" value="{}" placeholder="search titles and bodies"> <button type="submit">search</button></form>"#,
        escape(slug),
        escape(q),
    )
}

fn shelf_search_form(slugs: &[&str], selected: &str, q: &str) -> String {
    if slugs.is_empty() {
        return String::new();
    }
    let options: String = slugs
        .iter()
        .map(|s| {
            format!(
                r#"<option value="{0}"{1}>{0}</option>"#,
                escape(s),
                if *s == selected { " selected" } else { "" }
            )
        })
        .collect();
    format!(
        r#"<form action="/ui/search" method="get"><select name="book">{options}</select> <input type="search" name="q" value="{}" placeholder="search titles and bodies"> <button type="submit">search</button></form>"#,
        escape(q),
    )
}

const CSS: &str = "body{font:15px/1.55 system-ui,sans-serif;max-width:64rem;margin:0 auto;padding:1rem;color:#1d1d1f;background:#fff}\
a{color:#0b57d0}nav{margin-bottom:.5rem}nav,.meta{color:#666;font-size:.9em}\
table{border-collapse:collapse;width:100%}td,th{padding:.25rem .5rem;border-bottom:1px solid #ddd;text-align:left;vertical-align:top}.n{text-align:right}\
table.props{width:auto;margin:.5rem 0}table.props th{font-weight:600;color:#555}\
pre{background:#f5f5f5;padding:.5rem;overflow-x:auto;white-space:pre-wrap}code{background:#f5f5f5}.ref{-webkit-user-select:all;user-select:all;font-size:.8rem}a.anchor{text-decoration:none;font-size:.8em}\
article{border-top:1px solid #ddd;padding-top:.5rem}ul.toc,ul.hits,ul.hist{list-style:none;padding:0}ul.toc li{margin:.1rem 0}.num{color:#888;font-variant-numeric:tabular-nums}\
.tag{font-size:.75em;border:1px solid #ccc;border-radius:.6em;padding:0 .4em;margin-left:.2em;color:#555}.tag.archived{color:#999;border-color:#ddd}.tag.draft{color:#a15c00;border-color:#e0b070}.tag.ok{color:#1a7f37;border-color:#8cc89a}.tag.rev{color:#8250df;border-color:#c4a8f0}\
form{margin:.5rem 0}\
@media (prefers-color-scheme:dark){body{color:#e6e6e6;background:#151515}a{color:#8ab4f8}pre,code{background:#222}td,th,article{border-color:#333}.tag{border-color:#444;color:#bbb}}";

/// Wrap a fragment in a page. `title` is escaped here.
fn page(title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="referrer" content="no-referrer"><title>{} · outline</title><style>{CSS}</style></head><body>{body}</body></html>"#,
        escape(title)
    );
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP_VALUE),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            ),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use outline_mcp_core::application::service::BookService;
    use outline_mcp_core::domain::model::book::{AddNodeRequest, UpdateNodeRequest};
    use outline_mcp_core::domain::model::node::{BodyChange, Revision};
    use outline_mcp_core::domain::store::BookStore;
    use outline_mcp_core::infra::memory_store::{MemoryBookStore, MemorySnapshotStore};
    use tower::ServiceExt;

    // "anyone:secret" and "anyone:wrong"
    const GOOD: &str = "Basic YW55b25lOnNlY3JldA==";
    const BAD: &str = "Basic YW55b25lOndyb25n";
    const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

    struct Fixture {
        storage: Storage,
        root: NodeId,
        child: NodeId,
        old: NodeId,
    }

    fn req(title: &str, parent: Option<NodeId>, body: Option<&str>) -> AddNodeRequest {
        AddNodeRequest {
            parent,
            title: title.to_string(),
            node_type: if parent.is_none() {
                NodeType::Section
            } else {
                NodeType::Content
            },
            body: body.map(str::to_string),
            placeholder: None,
            position: usize::MAX,
            properties: HashMap::new(),
        }
    }

    fn status_req(status: NodeStatus) -> UpdateNodeRequest {
        UpdateNodeRequest {
            title: None,
            body: None,
            node_type: None,
            placeholder: None,
            properties: None,
            properties_patch: None,
            status: Some(status),
        }
    }

    /// A book `b` with: section "Rules" (inject) > content "Live" (body with
    /// raw HTML and a heading, a property with markup, verified, a pending
    /// revision, a link to "Old"); section "Attic" archived > content "Old".
    async fn fixture() -> Fixture {
        let books: Arc<dyn BookStore> = Arc::new(MemoryBookStore::new());
        let storage = Storage::new(books.clone(), Arc::new(MemorySnapshotStore::new()));
        let svc = BookService::new(books, "b");
        svc.create_book("The <Book>", 4).await.unwrap();
        let mut root_req = req("Rules", None, None);
        root_req
            .properties
            .insert("inject".to_string(), "true".to_string());
        let (root, _) = svc.add_node(root_req).await.unwrap();
        let mut child_req = req(
            "Live <script>alert(1)</script>",
            Some(root),
            Some("## Heading\n\nneedle text <script>alert(2)</script> end\n"),
        );
        child_req
            .properties
            .insert("note".to_string(), "<b>prop</b>".to_string());
        let (child, _) = svc.add_node(child_req).await.unwrap();
        let (attic, _) = svc.add_node(req("Attic", None, None)).await.unwrap();
        let (old, _) = svc
            .add_node(req("Old", Some(attic), Some("old needle")))
            .await
            .unwrap();
        svc.update_node(attic, status_req(NodeStatus::Archived))
            .await
            .unwrap();
        svc.add_link(child, old, "supersedes").await.unwrap();
        svc.verify_nodes(&[child], Some("alice".to_string()))
            .await
            .unwrap();
        svc.propose_revision(
            child,
            Revision {
                title: None,
                body: Some(BodyChange::Set("new body".to_string())),
                properties: None,
                properties_patch: None,
                base: String::new(),
                by: Some("bob".to_string()),
                note: None,
                at: Timestamp::now(),
            },
            false,
        )
        .await
        .unwrap();
        Fixture {
            storage,
            root,
            child,
            old,
        }
    }

    async fn call(
        app: Router,
        uri: &str,
        host: &str,
        auth: Option<&str>,
    ) -> (StatusCode, String, Response) {
        let mut r = HttpRequest::builder().uri(uri).header(header::HOST, host);
        if let Some(a) = auth {
            r = r.header(header::AUTHORIZATION, a);
        }
        let resp = app.oneshot(r.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 22).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        (status, text, Response::from_parts(parts, Body::empty()))
    }

    async fn open(f: &Fixture, uri: &str) -> (StatusCode, String, Response) {
        call(router(f.storage.clone(), None), uri, "localhost:8486", None).await
    }

    fn assert_headers(r: &Response, uri: &str) {
        assert_eq!(r.headers()[header::CONTENT_SECURITY_POLICY], CSP, "{uri}");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store", "{uri}");
        assert_eq!(
            r.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff",
            "{uri}"
        );
        assert_eq!(r.headers()[header::REFERRER_POLICY], "no-referrer", "{uri}");
    }

    #[tokio::test]
    async fn shelf_lists_books_with_counts() {
        let f = fixture().await;
        let (s, body, r) = open(&f, "/ui/").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"<a href="/ui/book/b">b</a>"#), "{body}");
        assert!(body.contains("The &lt;Book&gt;"), "{body}");
        // Rules and Live are active; Attic is archived, Old sits under it.
        assert!(body.contains(r#"<td class="n">2</td>"#), "{body}");
        assert!(body.contains(r#"href="/ui/?totals=1""#), "{body}");
        assert!(body.contains(r#"action="/ui/search""#), "{body}");
        assert_headers(&r, "/ui/");
        let (s, _, _) = open(&f, "/ui").await;
        assert_eq!(s, StatusCode::OK);
        let (s, body, _) = open(&f, "/ui/?totals=1").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"<td class="n">2/4</td>"#), "{body}");
        assert!(body.contains(r#"<a href="/ui/">hide totals</a>"#), "{body}");
    }

    #[tokio::test]
    async fn shelf_hides_archived_books_unless_asked() {
        let f = fixture().await;
        let svc = BookService::new(f.storage.books.clone(), "b");
        svc.update_book(None, None, Some(true)).await.unwrap();
        let (_, body, _) = open(&f, "/ui/").await;
        assert!(!body.contains(r#"<a href="/ui/book/b">b</a>"#), "{body}");
        assert!(body.contains("1 archived books hidden"), "{body}");
        let (_, body, _) = open(&f, "/ui/?archived=1&totals=1").await;
        assert!(body.contains(r#"<a href="/ui/book/b">b</a>"#), "{body}");
        assert!(
            body.contains(r#"<span class="tag archived">archived</span>"#),
            "{body}"
        );
        assert!(body.contains(r#"<td class="n">2/4</td>"#), "{body}");
        assert!(
            body.contains(r#"href="/ui/?totals=1">hide archived books"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn shelf_search_redirects_to_the_book() {
        let f = fixture().await;
        let (s, _, r) = open(&f, "/ui/search?book=b&q=a%20b").await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/ui/book/b/search?q=a%20b");
        let (s, _, _) = open(&f, "/ui/search?book=../x&q=a").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn toc_hides_archived_subtrees_unless_asked() {
        let f = fixture().await;
        let (s, body, r) = open(&f, "/ui/book/b").await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            body.contains(&format!("/ui/book/b/node/{}", f.root)),
            "{body}"
        );
        assert!(
            body.contains(&format!("/ui/book/b/node/{}", f.child)),
            "{body}"
        );
        assert!(
            !body.contains(&format!("/ui/book/b/node/{}", f.old)),
            "{body}"
        );
        assert!(body.contains("2 archived nodes hidden"), "{body}");
        assert!(
            body.contains(r#"<span class="tag">inject</span>"#),
            "{body}"
        );
        assert!(body.contains("✎ revision"), "{body}");
        assert!(body.contains("1 link"), "{body}");
        assert!(body.contains("✓ "), "{body}");
        assert!(body.contains("Live &lt;script&gt;"), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert_headers(&r, "/ui/book/b");

        let (_, body, _) = open(&f, "/ui/book/b?archived=1").await;
        assert!(
            body.contains(&format!("/ui/book/b/node/{}", f.old)),
            "{body}"
        );
        assert!(
            body.contains(r#"<span class="tag archived">archived</span>"#),
            "{body}"
        );
        assert!(!body.contains("archived nodes hidden"), "{body}");
    }

    #[tokio::test]
    async fn node_page_shows_everything_escaped() {
        let f = fixture().await;
        let uri = format!("/ui/book/b/node/{}", f.child);
        let (s, body, r) = open(&f, &uri).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert!(body.contains("Live &lt;script&gt;alert(1)"), "{body}");
        assert!(body.contains("&lt;script&gt;alert(2)"), "{body}");
        assert!(body.contains(r#"<h2 id="heading">Heading "#), "{body}");
        assert!(body.contains("&lt;b&gt;prop&lt;/b&gt;"), "{body}");
        assert!(body.contains("verified"), "{body}");
        assert!(body.contains("by alice"), "{body}");
        assert!(body.contains("Pending revision"), "{body}");
        assert!(body.contains("+new body"), "{body}");
        assert!(body.contains("supersedes →"), "{body}");
        assert!(
            body.contains(r#"<span class="tag archived">archived</span>"#),
            "{body}"
        );
        assert!(body.contains(&format!("outline:b/{}", f.child)), "{body}");
        assert!(
            body.contains(&format!(r#"href="/ui/book/b/node/{}">Rules</a>"#, f.root)),
            "{body}"
        );
        assert!(body.contains("<h2>History</h2>"), "{body}");
        assert_headers(&r, &uri);

        // backlink from the target's side
        let (_, body, _) = open(&f, &format!("/ui/book/b/node/{}", f.old)).await;
        assert!(body.contains("supersedes ←"), "{body}");
        assert!(body.contains("under an archived section"), "{body}");

        // children of a section
        let (_, body, _) = open(&f, &format!("/ui/book/b/node/{}", f.root)).await;
        assert!(body.contains("<h2>Children</h2>"), "{body}");
    }

    #[tokio::test]
    async fn search_finds_body_matches_and_hides_archived() {
        let f = fixture().await;
        let (s, body, r) = open(&f, "/ui/book/b/search?q=NEEDLE").await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            body.contains(&format!("/ui/book/b/node/{}", f.child)),
            "{body}"
        );
        assert!(
            !body.contains(&format!("/ui/book/b/node/{}", f.old)),
            "{body}"
        );
        assert!(body.contains("1 archived matches hidden"), "{body}");
        assert!(body.contains("&lt;script&gt;"), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert_headers(&r, "search");

        let (_, body, _) = open(&f, "/ui/book/b/search?q=needle&archived=1").await;
        assert!(
            body.contains(&format!("/ui/book/b/node/{}", f.old)),
            "{body}"
        );

        let (s, body, _) = open(&f, "/ui/book/b/search?q=").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"action="/ui/book/b/search""#), "{body}");
        assert!(!body.contains("/ui/book/b/node/"), "{body}");

        let (_, body, _) = open(&f, "/ui/book/b/search?q=%3Cscript%3E%22").await;
        assert!(!body.contains("<script"), "{body}");
        assert!(body.contains("&lt;script&gt;&quot;"), "{body}");
    }

    #[tokio::test]
    async fn history_lists_newest_first_and_validates_limit() {
        let f = fixture().await;
        let (s, body, r) = open(&f, "/ui/book/b/history").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("<td>create</td>"), "{body}");
        assert!(body.contains("<td>update</td>"), "{body}");
        assert_headers(&r, "history");
        let (_, body, _) = open(&f, "/ui/book/b/history?limit=1").await;
        assert_eq!(body.matches("<tr><td>").count(), 1, "{body}");
        assert!(body.contains("older entries not shown"), "{body}");
        let (s, _, _) = open(&f, "/ui/book/b/history?limit=0").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn errors_are_pages_with_the_same_headers() {
        let f = fixture().await;
        for (uri, code) in [
            ("/ui/book/missing", StatusCode::NOT_FOUND),
            (
                "/ui/book/b/node/00000000-0000-4000-8000-000000000000",
                StatusCode::NOT_FOUND,
            ),
            ("/ui/book/b/node/not-a-uuid", StatusCode::BAD_REQUEST),
            (
                "/ui/book/b/node/00000000-0000-0000-0000-000000000000",
                StatusCode::BAD_REQUEST,
            ),
            ("/ui/book/bad%20slug", StatusCode::BAD_REQUEST),
        ] {
            let (s, body, r) = open(&f, uri).await;
            assert_eq!(s, code, "{uri}: {body}");
            assert_headers(&r, uri);
            assert!(!body.contains("<script"), "{uri}");
        }
    }

    #[tokio::test]
    async fn only_get_is_routed() {
        let f = fixture().await;
        let r = HttpRequest::builder()
            .method("POST")
            .uri("/ui/book/b")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        let resp = router(f.storage.clone(), None).oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn without_token_only_loopback_host_is_served() {
        let f = fixture().await;
        for (host, code) in [
            ("localhost:8486", StatusCode::OK),
            ("127.0.0.1", StatusCode::OK),
            ("[::1]:8486", StatusCode::OK),
            ("evil.example", StatusCode::FORBIDDEN),
            ("127.0.0.2", StatusCode::FORBIDDEN),
        ] {
            let (s, _, _) = call(router(f.storage.clone(), None), "/ui/", host, None).await;
            assert_eq!(s, code, "{host}");
        }
    }

    #[tokio::test]
    async fn with_token_basic_is_required_and_host_is_not_checked() {
        let f = fixture().await;
        let app = || router(f.storage.clone(), Some(Arc::from("secret")));
        let (s, _, r) = call(app(), "/ui/", "outline.example.com", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(r.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .starts_with(r#"Basic realm="outline-mcp""#));
        let (s, _, _) = call(app(), "/ui/", "outline.example.com", Some(BAD)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _, _) = call(app(), "/ui/", "outline.example.com", Some("Bearer secret")).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, body, _) = call(app(), "/ui/", "outline.example.com", Some(GOOD)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let (s, _, _) = call(
            app(),
            "/ui/",
            "outline.example.com",
            Some("basic YW55b25lOnNlY3JldA=="),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "lowercase scheme");
        for bad in [
            "Basic YW55b25lOnNlY3JldA",   // padding missing
            "Basic YW55b25lOnNlY3JldA=",  // padding short
            "Basic YW55b25lOnNlY3JldB==", // non-zero trailing bits
            "Basic ",
            "Basic",
            "Basic YW55b25lOnNlY3JldA===",
        ] {
            let (s, _, _) = call(app(), "/ui/", "outline.example.com", Some(bad)).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{bad}");
        }
    }

    #[tokio::test]
    async fn non_utf8_authorization_is_refused() {
        let f = fixture().await;
        let r = HttpRequest::builder()
            .uri("/ui/")
            .header(header::HOST, "outline.example.com")
            .header(
                header::AUTHORIZATION,
                HeaderValue::from_bytes(b"Basic \xff\xfe").unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let resp = router(f.storage.clone(), Some(Arc::from("secret")))
            .oneshot(r)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn base64_is_strict() {
        assert_eq!(
            base64_decode("YW55b25lOnNlY3JldA==").unwrap(),
            b"anyone:secret"
        );
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert!(base64_decode("YQ").is_none(), "unpadded");
        assert!(base64_decode("YR==").is_none(), "trailing bits");
        assert!(base64_decode("Y").is_none());
        assert!(base64_decode("a$b=").is_none());
        assert_eq!(base64_decode("").unwrap(), b"");
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secre", b"secret"));
        assert!(!constant_time_eq(b"secretx", b"secret"));
        assert!(!constant_time_eq(b"", b"secret"));
        assert!(!constant_time_eq(b"secreT", b"secret"));
        // A presented value that is the token followed by NULs is not equal.
        assert!(!constant_time_eq(b"secret\0", b"secret"));
    }

    #[tokio::test]
    async fn every_ui_status_carries_the_security_headers() {
        let f = fixture().await;
        // 401 (token, no credentials)
        let (s, _, r) = call(
            router(f.storage.clone(), Some(Arc::from("secret"))),
            "/ui/",
            "outline.example.com",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_headers(&r, "401");
        // 403 (no token, foreign host)
        let (s, _, r) = call(
            router(f.storage.clone(), None),
            "/ui/",
            "evil.example",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_headers(&r, "403");
        // 404 (unknown /ui path)
        let (s, body, r) = open(&f, "/ui/nope/deeper").await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
        assert_headers(&r, "404");
        // 405
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/ui/")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        let r = router(f.storage.clone(), None).oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_headers(&r, "405");
    }

    #[tokio::test]
    async fn request_without_host_is_refused_without_token() {
        let f = fixture().await;
        let req = HttpRequest::builder()
            .uri("/ui/")
            .body(Body::empty())
            .unwrap();
        let r = router(f.storage.clone(), None).oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    /// The router `run_http` serves: merging /ui changes nothing outside
    /// /ui, and /mcp still needs the bearer token.
    #[tokio::test]
    async fn merged_http_router_keeps_mcp_and_other_paths_unchanged() {
        let f = fixture().await;
        let with_token =
            || crate::server::http_router(f.storage.clone(), Some(Arc::from("secret")), false);
        // /mcp: bearer required; Basic does not count.
        for auth in [None, Some(GOOD)] {
            let (s, body, r) = call(with_token(), "/mcp", "outline.example.com", auth).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{auth:?}");
            assert!(body.contains("bearer"), "{body}");
            assert!(r.headers().get(header::WWW_AUTHENTICATE).is_none());
        }
        // Paths outside /ui answer with the bearer layer's 401, not Basic.
        for path in ["/", "/favicon.ico", "/nope"] {
            let (s, body, r) = call(with_token(), path, "outline.example.com", None).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{path}");
            assert!(body.contains("bearer"), "{path}: {body}");
            assert!(
                r.headers().get(header::WWW_AUTHENTICATE).is_none(),
                "{path}"
            );
            let (s, _, _) = call(
                with_token(),
                path,
                "outline.example.com",
                Some("Bearer secret"),
            )
            .await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{path} with bearer");
        }
        // An unnormalized `/ui/../mcp` is a /ui path: Basic-protected, and
        // a 404 page even with credentials; it never reaches /mcp.
        let (s, _, r) = call(with_token(), "/ui/../mcp", "outline.example.com", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(r.headers().get(header::WWW_AUTHENTICATE).is_some());
        let (s, _, r) = call(
            with_token(),
            "/ui/../mcp",
            "outline.example.com",
            Some(GOOD),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_headers(&r, "/ui/../mcp");
        // /ui: Basic required.
        let (s, _, r) = call(with_token(), "/ui/", "outline.example.com", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(r.headers().get(header::WWW_AUTHENTICATE).is_some());
        let (s, _, _) = call(with_token(), "/ui/", "outline.example.com", Some(GOOD)).await;
        assert_eq!(s, StatusCode::OK);

        // Without a token: unknown paths are a plain 404, not the /ui 403.
        let open_router = || crate::server::http_router(f.storage.clone(), None, true);
        for path in ["/", "/nope"] {
            let (s, _, r) = call(open_router(), path, "evil.example", None).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
            assert!(
                r.headers().get(header::CONTENT_SECURITY_POLICY).is_none(),
                "{path}"
            );
        }
        let (s, _, _) = call(open_router(), "/ui/", "localhost", None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn node_page_history_is_capped() {
        let f = fixture().await;
        let svc = BookService::new(f.storage.books.clone(), "b");
        for i in 0..(NODE_HISTORY_MAX + 5) {
            svc.update_node(
                f.child,
                UpdateNodeRequest {
                    title: Some(format!("t{i}")),
                    body: None,
                    node_type: None,
                    placeholder: None,
                    properties: None,
                    properties_patch: None,
                    status: None,
                },
            )
            .await
            .unwrap();
        }
        let (s, body, _) = open(&f, &format!("/ui/book/b/node/{}", f.child)).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("older entries not shown"), "{body}");
    }

    #[test]
    fn loopback_host_accepts_only_this_machine() {
        for ok in [
            "localhost",
            "localhost:8080",
            "LOCALHOST",
            "127.0.0.1",
            "127.0.0.1:8486",
            "[::1]",
            "[::1]:8486",
            "::1",
        ] {
            assert!(loopback_host(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.example",
            "localhost.example",
            "example.localhost",
            "127.0.0.2",
            "0.0.0.0",
            "localhost:",
            "localhost:80x",
            "localhost:8080:1",
            "[::1",
            "[::1]x",
            "[::2]:8486",
            "::1:8486",
            "[localhost]",
        ] {
            assert!(!loopback_host(bad), "{bad}");
        }
    }
}
