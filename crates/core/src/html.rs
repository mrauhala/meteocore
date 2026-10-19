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

/// `value` with at most `decimals` decimals, trailing zeros and a bare
/// decimal point dropped, and never `-0`: `10.870554` at 3 is `10.871`,
/// `135.0` is `135`. A non-finite value is Rust's own form (`NaN`, `inf`).
/// The HTML pages' one display form of a number (#1138); the exact value
/// stays in the JSON and, on a page, in the cell's `title`.
pub fn display_number(value: f64, decimals: usize) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    // Past 15 digits a fixed form is a long run of digits: an exponent.
    if value.abs() >= 1e15 {
        return exponent_form(value);
    }
    let mut text = format!("{value:.decimals$}");
    if text.contains('.') {
        let kept = text.trim_end_matches('0').trim_end_matches('.').len();
        text.truncate(kept);
    }
    if text == "-0" {
        text.remove(0);
    }
    text
}

/// Decimals a coordinate is displayed with: 5 is about 1 m on the ground.
pub const COORDINATE_DECIMALS: usize = 5;

/// A longitude or latitude for display: [`display_number`] at
/// [`COORDINATE_DECIMALS`], `24.933333333333334` as `24.93333`.
pub fn display_coordinate(value: f64) -> String {
    display_number(value, COORDINATE_DECIMALS)
}

/// A measured value for display: at most 3 decimals from 1 up, three
/// significant digits below 1, so a small quantity (a precipitation rate in
/// `kg m-2 s-1`) never displays as `0`, and below 0.0001 in exponent form
/// (`1.2e-7`), as from 1e15 up. Zero is `0`. At most 24 characters.
pub fn display_value(value: f64) -> String {
    let magnitude = value.abs();
    if !value.is_finite() || magnitude == 0.0 || magnitude >= 1.0 {
        return display_number(value, 3);
    }
    if magnitude < 1e-4 {
        return exponent_form(value);
    }
    // 0.5 → 3 decimals, 0.0123 → 4, 0.000123 → 6: three significant digits.
    let leading_zeros = (-magnitude.log10()).floor() as usize;
    display_number(value, leading_zeros + 3)
}

/// Three significant digits in exponent form, the mantissa's trailing
/// zeros dropped: `1.2e-7`, `-2.35e20`. At most 10 characters.
fn exponent_form(value: f64) -> String {
    let text = format!("{value:.2e}");
    match text.split_once('e') {
        Some((mantissa, exponent)) if mantissa.contains('.') => {
            let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
            format!("{mantissa}e{exponent}")
        }
        _ => text,
    }
}

/// The page icon: the brand mark as an inline SVG `data:` URL, so no HTML
/// page requests `/favicon.ico` (#1131). Every workbench page and the API
/// reference declare it. It holds no `"`, `&` or `<`: it goes into an
/// attribute as it is.
pub const FAVICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='8' fill='%23095c49'/%3E%3Cg fill='none' stroke='white' stroke-width='2.2' stroke-linecap='round'%3E%3Ccircle cx='16' cy='16' r='10'/%3E%3Ccircle cx='16' cy='16' r='5'/%3E%3Cpath d='M16 16 24 8'/%3E%3C/g%3E%3C/svg%3E";

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
        // The short tokens, and the media types other OGC API servers
        // accept in `f` too (#510).
        return match f.to_ascii_lowercase().as_str() {
            "json" | "application/json" => Ok(Wanted::Json),
            "html" | "text/html" => Ok(Wanted::Html),
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
    /// The link's media type (`type`), shown in an HTML link table.
    pub media_type: Option<String>,
}

impl LinkView {
    pub fn new(href: impl Into<String>, rel: impl Into<String>, title: Option<&str>) -> Self {
        LinkView {
            href: href.into(),
            rel: rel.into(),
            title: title.map(str::to_string),
            media_type: None,
        }
    }

    /// The same link with its media type.
    pub fn with_type(mut self, media_type: impl Into<String>) -> Self {
        self.media_type = Some(media_type.into());
        self
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

    /// The display forms of the issue's examples (#1138).
    #[test]
    fn numbers_display_rounded_without_trailing_zeros() {
        assert_eq!(display_value(10.870_554_054_541_278), "10.871");
        assert_eq!(display_value(135.0), "135");
        assert_eq!(display_value(-0.000_1), "-0.0001");
        assert_eq!(display_value(0.5), "0.5");
        assert_eq!(display_value(0.012_345), "0.0123");
        assert_eq!(display_value(0.000_123_456), "0.000123");
        assert_eq!(display_value(1.2e-7), "1.2e-7");
        assert_eq!(display_value(-2.345_6e-9), "-2.35e-9");
        assert_eq!(display_value(0.0), "0");
        assert_eq!(display_value(-0.0), "0");
        assert_eq!(display_value(1e20), "1e20");
        assert_eq!(display_value(-123_456_789.123_456), "-123456789.123");
        assert_eq!(
            display_coordinate(-1.234_567_890_123_456_7e300),
            "-1.23e300"
        );
        assert_eq!(display_value(f64::NAN), "NaN");
        assert_eq!(display_coordinate(24.933_333_333_333_334), "24.93333");
        assert_eq!(display_coordinate(-0.000_001), "0");
        assert_eq!(display_coordinate(60.0), "60");
        assert_eq!(display_number(51.540_771_484_375_03, 3), "51.541");
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
        assert!(negotiate(Some("application/xml"), None).is_err());
    }

    #[test]
    fn negotiate_accepts_media_type_aliases_in_f() {
        for f in ["application/json", "Application/JSON", " application/json "] {
            assert_eq!(negotiate(Some(f), None).unwrap(), Wanted::Json, "{f}");
        }
        assert_eq!(
            negotiate(Some("text/html"), Some("application/json")).unwrap(),
            Wanted::Html
        );
    }
}
