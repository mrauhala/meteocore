//! Minimal HTML representations + content negotiation for the OGC API metadata
//! endpoints (landing page, `/conformance`, `/collections`, `/collections/{id}`),
//! satisfying the OGC API – Common – Part 2 `conf/html` class (#296).
//!
//! Pure (no framework deps): the API crates pass plain data (titles, links,
//! collection cards) and `axum::response::Html`-wrap the returned `String`.
//! ds-core can't consume `serde_json::Value` (it doesn't depend on serde_json),
//! so HTML is built from primitive inputs the handlers already have, not by
//! re-parsing the JSON response.

/// Escape the five HTML/XML special characters. All collection-derived text
/// (titles, descriptions) must pass through this before interpolation.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Which representation a request resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    Json,
    Html,
}

/// An unsupported `?f=` value. The API crates map this to HTTP 400.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct NegotiationError(pub String);

/// Raw `?f=` query parameter, deserialized by handlers that don't already take
/// a `SearchQueryParams` (landing / conformance / collection-detail).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct FormatParams {
    pub f: Option<String>,
}

/// Resolve the requested representation. `?f=` wins over the `Accept` header;
/// `f=json|html` (case-insensitive); an unknown `f` is a 400. With no `f`, HTML
/// is served only when `Accept` lists `text/html` with a non-zero q-value;
/// everything else (missing header, `application/json`, `*/*`, or an explicit
/// `text/html;q=0` opt-out) defaults to **JSON** — so the JSON-only behaviour
/// is unchanged unless a client explicitly asks for HTML.
///
/// A `text/html` or `text/*` range is honoured (per RFC 9110 §12.5.1), but not
/// `*/*` — so API clients that send `Accept: */*` (e.g. curl) keep getting JSON.
/// Full cross-type q-value preference ordering is not implemented (a client
/// wanting a specific format can always send `?f=`).
pub fn negotiate(f: Option<&str>, accept: Option<&str>) -> Result<Wanted, NegotiationError> {
    if let Some(f) = f.map(str::trim).filter(|s| !s.is_empty()) {
        return match f.to_ascii_lowercase().as_str() {
            "json" => Ok(Wanted::Json),
            "html" => Ok(Wanted::Html),
            other => Err(NegotiationError(format!(
                "unknown format '{other}'; expected 'json' or 'html'"
            ))),
        };
    }
    if accept.is_some_and(accept_allows_html) {
        return Ok(Wanted::Html);
    }
    Ok(Wanted::Json)
}

/// True if the `Accept` header makes `text/html` acceptable. The effective
/// q-value for `text/html` is taken from the **most specific** matching range —
/// an exact `text/html` overrides the `text/*` wildcard (RFC 9110 §12.5.1), so
/// `text/html;q=0, text/*` correctly yields JSON. `*/*` is ignored entirely, so
/// API clients sending `Accept: */*` (e.g. curl) stay on the JSON default. A
/// malformed q-value (e.g. `q=abc`) is treated as `0` — not acceptable — so a
/// garbled header falls back to the safe JSON default rather than being read as
/// maximally preferred.
fn accept_allows_html(accept: &str) -> bool {
    let mut exact: Option<f32> = None; // q of an explicit `text/html` range
    let mut wildcard: Option<f32> = None; // q of a `text/*` range
    for entry in accept.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let media = parts.next().unwrap_or("");
        let is_exact = media.eq_ignore_ascii_case("text/html");
        let is_wild = media.eq_ignore_ascii_case("text/*");
        if !is_exact && !is_wild {
            continue;
        }
        let mut q = 1.0_f32; // absent q defaults to 1.0
        for p in parts {
            if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                q = v.trim().parse::<f32>().unwrap_or(0.0);
            }
        }
        if is_exact {
            exact = Some(q);
        } else {
            wildcard = Some(q);
        }
    }
    exact.or(wildcard).is_some_and(|q| q > 0.0)
}

/// A hyperlink rendered in an HTML page (owned so callers build it inline).
#[derive(Debug, Clone)]
pub struct LinkView {
    pub href: String,
    pub rel: String,
    pub title: Option<String>,
}

impl LinkView {
    pub fn new(href: impl Into<String>, rel: impl Into<String>, title: Option<&str>) -> Self {
        LinkView {
            href: href.into(),
            rel: rel.into(),
            title: title.map(str::to_string),
        }
    }
}

/// One collection in a `/collections` list or a single `/collections/{id}` page.
#[derive(Debug, Clone)]
pub struct CollectionCard {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Link to this collection's own metadata resource.
    pub self_href: String,
    /// Configured keywords (may be empty), rendered as chips.
    pub keywords: Vec<String>,
    /// Optional license as `(title, href)`, rendered as a link.
    pub license: Option<(String, String)>,
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:60rem;margin:2rem auto;\
padding:0 1rem;line-height:1.5;color:#1a1a1a}h1{font-size:1.5rem}a{color:#0b66c3}\
code{background:#f2f2f2;padding:.1em .3em;border-radius:3px;font-size:.9em}\
ul{padding-left:1.1rem}li{margin:.25rem 0}.rel{color:#777;font-size:.85em}\
.desc{color:#444}.nav{font-size:.9em}\
.kws{margin:.3em 0}.kw{display:inline-block;background:#eef;border-radius:3px;\
padding:.05em .4em;margin:.1em .25em .1em 0;font-size:.8em;color:#334}\
.license{color:#555;font-size:.9em}";

/// Wrap `body` in the page skeleton. **`body` must already be HTML-escaped** —
/// only `title` is escaped here. All builders in this module construct `body`
/// from `escape()`d pieces; a caller that interpolates raw user-derived text
/// into `body` would introduce stored XSS.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n{body}</body>\n</html>\n",
        escape(title)
    )
}

/// Render keywords as a row of chips. Empty input → empty string.
fn render_keywords(keywords: &[String]) -> String {
    if keywords.is_empty() {
        return String::new();
    }
    let mut s = String::from("<div class=\"kws\">");
    for k in keywords {
        s.push_str(&format!("<span class=\"kw\">{}</span>", escape(k)));
    }
    s.push_str("</div>\n");
    s
}

/// Render the license as a labelled link. `None` → empty string.
fn render_license(license: &Option<(String, String)>) -> String {
    match license {
        Some((title, href)) => format!(
            "<p class=\"license\">License: <a href=\"{}\">{}</a></p>\n",
            escape(href),
            escape(title)
        ),
        None => String::new(),
    }
}

fn render_links(links: &[LinkView], class: &str) -> String {
    if links.is_empty() {
        return String::new();
    }
    let mut s = format!("<ul class=\"{}\">\n", escape(class));
    for l in links {
        let label = l
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(&l.href);
        s.push_str(&format!(
            "<li><a href=\"{}\">{}</a> <span class=\"rel\">[{}]</span></li>\n",
            escape(&l.href),
            escape(label),
            escape(&l.rel)
        ));
    }
    s.push_str("</ul>\n");
    s
}

/// Landing page (`GET /{api}/`).
pub fn landing_html(title: &str, description: &str, links: &[LinkView]) -> String {
    let mut body = format!("<h1>{}</h1>\n", escape(title));
    if !description.is_empty() {
        body.push_str(&format!("<p class=\"desc\">{}</p>\n", escape(description)));
    }
    body.push_str(&render_links(links, "nav"));
    page(title, &body)
}

/// Conformance declaration (`GET /{api}/conformance`). `nav` carries links back
/// to the landing page and a `rel="alternate"` pointer to the JSON
/// representation, so the HTML page is not a navigation dead-end.
pub fn conformance_html(classes: &[&str], nav: &[LinkView]) -> String {
    let mut body = String::from("<h1>Conformance classes</h1>\n");
    body.push_str(&render_links(nav, "nav"));
    body.push_str("<ul>\n");
    for c in classes {
        body.push_str(&format!("<li><code>{}</code></li>\n", escape(c)));
    }
    body.push_str("</ul>\n");
    page("Conformance classes", &body)
}

/// Collections list (`GET /{api}/collections`). `nav` carries self/next/prev.
pub fn collections_html(title: &str, cards: &[CollectionCard], nav: &[LinkView]) -> String {
    let mut body = format!("<h1>{}</h1>\n", escape(title));
    body.push_str(&render_links(nav, "nav"));
    body.push_str("<ul class=\"collections\">\n");
    for c in cards {
        let label = if c.title.is_empty() { &c.id } else { &c.title };
        body.push_str(&format!(
            "<li><a href=\"{}\"><strong>{}</strong></a> <code>{}</code>",
            escape(&c.self_href),
            escape(label),
            escape(&c.id)
        ));
        if !c.description.is_empty() {
            body.push_str(&format!(
                "<div class=\"desc\">{}</div>",
                escape(&c.description)
            ));
        }
        body.push_str(&render_keywords(&c.keywords));
        body.push_str("</li>\n");
    }
    body.push_str("</ul>\n");
    page(title, &body)
}

/// Single collection (`GET /{api}/collections/{id}`).
pub fn collection_html(card: &CollectionCard, links: &[LinkView]) -> String {
    let title = if card.title.is_empty() {
        &card.id
    } else {
        &card.title
    };
    let mut body = format!(
        "<h1>{}</h1>\n<p><code>{}</code></p>\n",
        escape(title),
        escape(&card.id)
    );
    if !card.description.is_empty() {
        body.push_str(&format!(
            "<p class=\"desc\">{}</p>\n",
            escape(&card.description)
        ));
    }
    body.push_str(&render_keywords(&card.keywords));
    body.push_str(&render_license(&card.license));
    body.push_str(&render_links(links, "links"));
    page(title, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_handles_all_entities() {
        assert_eq!(escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn negotiate_f_wins_over_accept() {
        assert_eq!(
            negotiate(Some("json"), Some("text/html")).unwrap(),
            Wanted::Json
        );
        assert_eq!(negotiate(Some("HTML"), None).unwrap(), Wanted::Html);
    }

    #[test]
    fn negotiate_accept_and_default() {
        assert_eq!(
            negotiate(None, Some("text/html,application/json")).unwrap(),
            Wanted::Html
        );
        assert_eq!(
            negotiate(None, Some("application/json")).unwrap(),
            Wanted::Json
        );
        assert_eq!(negotiate(None, None).unwrap(), Wanted::Json);
        assert_eq!(negotiate(Some(""), None).unwrap(), Wanted::Json);
    }

    #[test]
    fn negotiate_accept_honours_q_values() {
        // Explicit opt-out: text/html;q=0 must NOT select HTML.
        assert_eq!(
            negotiate(None, Some("text/html;q=0, application/json")).unwrap(),
            Wanted::Json
        );
        assert_eq!(
            negotiate(None, Some("text/html; q=0.0")).unwrap(),
            Wanted::Json
        );
        // Non-zero q (or no q) selects HTML.
        assert_eq!(
            negotiate(None, Some("text/html;q=0.9")).unwrap(),
            Wanted::Html
        );
        assert_eq!(
            negotiate(None, Some("application/json, text/html;q=0.8")).unwrap(),
            Wanted::Html
        );
        // `*/*` is not an explicit text/html range → JSON (curl-style clients).
        assert_eq!(negotiate(None, Some("*/*")).unwrap(), Wanted::Json);
        // `text/*` matches text/html (RFC 9110 §12.5.1); `text/*;q=0` opts out.
        assert_eq!(negotiate(None, Some("text/*")).unwrap(), Wanted::Html);
        assert_eq!(negotiate(None, Some("text/*;q=0")).unwrap(), Wanted::Json);
        // Most-specific wins: an exact text/html;q=0 overrides the text/* wildcard.
        assert_eq!(
            negotiate(None, Some("text/html;q=0, text/*")).unwrap(),
            Wanted::Json
        );
        assert_eq!(
            negotiate(None, Some("text/html, text/*;q=0")).unwrap(),
            Wanted::Html
        );
        // A malformed q-value must NOT be read as maximally preferred — it falls
        // back to the safe JSON default (regression: was `unwrap_or(1.0)`).
        assert_eq!(
            negotiate(None, Some("text/html;q=abc")).unwrap(),
            Wanted::Json
        );
        // Malformed exact still loses to a healthy wildcard? No — most-specific
        // wins, so the garbled exact (q=0) overrides text/* and yields JSON.
        assert_eq!(
            negotiate(None, Some("text/html;q=nope, text/*")).unwrap(),
            Wanted::Json
        );
    }

    #[test]
    fn negotiate_unknown_f_is_error() {
        assert!(negotiate(Some("xml"), None).is_err());
    }

    #[test]
    fn builders_escape_and_render() {
        let cards = [CollectionCard {
            id: "radar".into(),
            title: "A <b>Radar</b>".into(),
            description: "x & y".into(),
            self_href: "/maps/collections/radar".into(),
            keywords: vec!["weather".into(), "ra<d>ar".into()],
            license: Some(("CC-BY 4.0".into(), "https://example/lic".into())),
        }];
        let html = collections_html("Collections", &cards, &[]);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("A &lt;b&gt;Radar&lt;/b&gt;"));
        assert!(html.contains("x &amp; y"));
        assert!(html.contains("/maps/collections/radar"));
        // raw unescaped title must not leak
        assert!(!html.contains("<b>Radar</b>"));
        // keyword chips render and are escaped
        assert!(html.contains("class=\"kw\">weather</span>"));
        assert!(html.contains("ra&lt;d&gt;ar"));

        // detail page renders keywords + the license link
        let detail = collection_html(&cards[0], &[]);
        assert!(detail.contains("class=\"kw\">weather</span>"));
        assert!(detail.contains("https://example/lic") && detail.contains("CC-BY 4.0"));
    }

    #[test]
    fn landing_and_conformance_render() {
        let links = [LinkView::new(
            "/edr/collections",
            "data",
            Some("Collections"),
        )];
        let l = landing_html("MeteoCore - EDR", "desc", &links);
        assert!(l.contains("MeteoCore - EDR") && l.contains("/edr/collections"));
        let nav = [LinkView::new("/edr/", "up", Some("Landing page"))];
        let c = conformance_html(&["http://example/conf/core"], &nav);
        assert!(c.contains("http://example/conf/core"));
        // Not a dead-end: the nav link back to the landing page is rendered.
        assert!(c.contains("/edr/") && c.contains("Landing page"));
    }
}
