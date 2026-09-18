//! Shared HTML escaping, content negotiation and resource view types.
//! Page rendering lives in `api_common::workbench`.

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
    /// Optional license as `(title, href?)`: rendered as a link when an href is
    /// present, else as the plain name (a free-text license with no URL).
    pub license: Option<(String, Option<String>)>,
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
}
