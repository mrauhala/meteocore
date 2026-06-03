//! OGC API – Common – Part 4 ("Discovery within many collections", draft
//! [25-046]) — the **Searchable Collections** requirements class.
//!
//! Filtering and pagination for the `/collections` resource: `bbox` /
//! `bbox-crs`, `datetime`, `q`, and `limit` + `offset`. The logic lives here
//! (not in the API crates) so EDR, Maps, Tiles, and Features share one
//! implementation — ds-core never builds `serde_json::Value`, so the crates
//! call [`parse_search_params`] + [`search`] and assemble the JSON response
//! themselves (and build `next`/`prev` link hrefs with [`page_query_string`]).
//!
//! Only CRS84 is supported for `bbox` / `bbox-crs` (every collection bbox is
//! advertised in CRS84). Sortable / Filterable (CQL2) / Hierarchical classes
//! are intentionally not implemented.
//!
//! [25-046]: https://docs.ogc.org/DRAFTS/25-046.html

use crate::datetime::parse_datetime_interval;
use chrono::{DateTime, Utc};

/// Server default page size when `limit` is absent. Sized so realistic
/// catalogs (including a full OPERA radar network) come back in one page and
/// existing single-page clients see no behaviour change.
pub const DEFAULT_LIMIT: usize = 1000;
/// Maximum honoured `limit`. A larger requested value is clamped to this (per
/// the draft, an over-large `limit` is not an error).
pub const MAX_LIMIT: usize = 1000;

/// `bbox-crs` values accepted as CRS84 (URI, CURIE, and short forms).
const ACCEPTED_BBOX_CRS: &[&str] = &[
    "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
    "https://www.opengis.net/def/crs/OGC/1.3/CRS84",
    "[OGC:CRS84]",
    "OGC:CRS84",
    "CRS84",
    "CRS:84",
];

/// A bad `/collections` query parameter. The API crates map this to HTTP 400.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct SearchError(pub String);

impl SearchError {
    fn new(msg: impl Into<String>) -> Self {
        SearchError(msg.into())
    }
}

/// The searchable facets of one collection. Borrows from the caller's config /
/// engine snapshot; build one per collection in the same order as the response
/// list, then pass the slice to [`search`].
#[derive(Debug, Clone)]
pub struct CollectionMatch<'a> {
    pub title: &'a str,
    pub description: &'a str,
    /// CRS84 bbox `[west, south, east, north]`, if the collection has one.
    pub bbox: Option<[f64; 4]>,
    /// Temporal interval `(start, end)`, if the collection has one.
    pub time: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Validated, parsed search parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchParams {
    /// CRS84 query bbox `[west, south, east, north]`.
    pub bbox: Option<[f64; 4]>,
    /// Query interval `(start, end)`.
    pub datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Lower-cased, non-empty free-text terms (OR semantics). Empty = no `q`.
    pub q: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

/// The outcome of a [`search`]: the total match count and the indices of the
/// items on the requested page, plus pagination cursors.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Total collections matching the filters (before pagination).
    pub number_matched: usize,
    /// Indices into the `items` slice for this page, in input order.
    pub page: Vec<usize>,
    pub has_next: bool,
    pub next_offset: usize,
    pub has_prev: bool,
    pub prev_offset: usize,
}

/// Parse and validate the raw `/collections` query parameters. Returns
/// [`SearchError`] (→ HTTP 400) for malformed input.
pub fn parse_search_params(
    bbox: Option<&str>,
    bbox_crs: Option<&str>,
    datetime: Option<&str>,
    q: Option<&str>,
    limit: Option<&str>,
    offset: Option<&str>,
) -> Result<SearchParams, SearchError> {
    // bbox-crs is only meaningful with a bbox, but validate it whenever present.
    if let Some(crs) = bbox_crs.map(str::trim).filter(|s| !s.is_empty()) {
        if !ACCEPTED_BBOX_CRS
            .iter()
            .any(|c| c.eq_ignore_ascii_case(crs))
        {
            return Err(SearchError::new(
                "Only CRS84 is supported for bbox-crs (e.g. \
                 http://www.opengis.net/def/crs/OGC/1.3/CRS84)",
            ));
        }
    }

    let bbox = match bbox.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(parse_bbox(s)?),
        None => None,
    };

    let datetime = match datetime.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let (start, end) = parse_datetime_interval(s)
                .map_err(|e| SearchError::new(format!("Invalid datetime: {e}")))?;
            // An inverted interval (end before start) would silently exclude
            // every overlapping collection; reject it as a 400.
            if start > end {
                return Err(SearchError::new("datetime: start must be <= end"));
            }
            Some((start, end))
        }
        None => None,
    };

    let q = match q.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
        None => Vec::new(),
    };

    let limit = match limit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let n: usize = s.parse().map_err(|_| {
                SearchError::new(format!("Invalid limit: '{s}' is not a positive integer"))
            })?;
            if n < 1 {
                return Err(SearchError::new("limit must be >= 1"));
            }
            n.min(MAX_LIMIT)
        }
        None => DEFAULT_LIMIT,
    };

    let offset = match offset.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.parse().map_err(|_| {
            SearchError::new(format!(
                "Invalid offset: '{s}' is not a non-negative integer"
            ))
        })?,
        None => 0,
    };

    Ok(SearchParams {
        bbox,
        datetime,
        q,
        limit,
        offset,
    })
}

/// Parse a `bbox` query value: 4 numbers (2D) or 6 (3D, vertical axis dropped).
fn parse_bbox(s: &str) -> Result<[f64; 4], SearchError> {
    let nums: Result<Vec<f64>, _> = s.split(',').map(|p| p.trim().parse::<f64>()).collect();
    let nums =
        nums.map_err(|_| SearchError::new("bbox must be a comma-separated list of numbers"))?;
    // `f64::parse` accepts "NaN"/"Inf"/"-Inf"; those would slip past the
    // intersection test (NaN compares false → silent empty result) and must be
    // rejected as a 400.
    if nums.iter().any(|n| !n.is_finite()) {
        return Err(SearchError::new("bbox values must be finite numbers"));
    }
    let bbox = match nums.len() {
        // 2D: west,south,east,north
        4 => [nums[0], nums[1], nums[2], nums[3]],
        // 3D: west,south,minz,east,north,maxz — drop the vertical axis.
        6 => [nums[0], nums[1], nums[3], nums[4]],
        _ => return Err(SearchError::new("bbox must have 4 or 6 numbers")),
    };
    // Latitude must not be inverted (south <= north). Longitude inversion is
    // legitimate — `west > east` represents an anti-meridian-crossing bbox.
    if bbox[1] > bbox[3] {
        return Err(SearchError::new("bbox: south must be <= north"));
    }
    // CRS84 range. Out-of-range coordinates would otherwise pass through and
    // silently match nothing (HTTP 200, numberMatched 0) instead of a 400.
    if bbox[1] < -90.0 || bbox[3] > 90.0 {
        return Err(SearchError::new(
            "bbox: latitude values must be within [-90, 90]",
        ));
    }
    // Anti-meridian (west > east) is allowed, but both must be in range.
    if !(-180.0..=180.0).contains(&bbox[0]) || !(-180.0..=180.0).contains(&bbox[2]) {
        return Err(SearchError::new(
            "bbox: longitude values must be within [-180, 180]",
        ));
    }
    Ok(bbox)
}

/// Filter `items` by the parameters and apply `offset`/`limit` pagination.
pub fn search(items: &[CollectionMatch], p: &SearchParams) -> SearchResult {
    let matched: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches(it, p))
        .map(|(i, _)| i)
        .collect();

    let number_matched = matched.len();
    let end = p.offset.saturating_add(p.limit).min(number_matched);
    let page: Vec<usize> = if p.offset < number_matched {
        matched[p.offset..end].to_vec()
    } else {
        Vec::new()
    };

    SearchResult {
        number_matched,
        page,
        has_next: end < number_matched,
        next_offset: p.offset.saturating_add(p.limit),
        // Offer `prev` only from a non-empty result page (`offset < number_matched`,
        // matching the page-population guard above). An out-of-range or
        // exactly-off-the-end `offset` yields an empty page with no `prev`,
        // since `prev` implies a preceding result page. A `next` link never
        // produces `offset == number_matched`, so normal paging is unaffected.
        has_prev: p.offset > 0 && p.offset < number_matched,
        prev_offset: p.offset.saturating_sub(p.limit),
    }
}

fn matches(it: &CollectionMatch, p: &SearchParams) -> bool {
    // "Unknown extent ≡ unbounded" (OGC API – Common – Part 4 §7.14.2/§7.14.3):
    // a collection that declares no spatial/temporal extent matches any
    // bbox/datetime filter, rather than being excluded — otherwise cold-start
    // engines (bbox/time still None) and extent-less collections silently
    // vanish from filtered /collections responses.
    if let Some(qbox) = p.bbox {
        match it.bbox {
            Some(cbox) if bbox_intersects(qbox, cbox) => {}
            None => {}
            Some(_) => return false,
        }
    }
    if let Some((qs, qe)) = p.datetime {
        match it.time {
            Some((cs, ce)) if qs <= ce && cs <= qe => {}
            None => {}
            Some(_) => return false,
        }
    }
    if !p.q.is_empty() && !q_matches(&p.q, it.title, it.description) {
        return false;
    }
    true
}

/// CRS84 bbox intersection, anti-meridian aware.
fn bbox_intersects(q: [f64; 4], c: [f64; 4]) -> bool {
    // Latitude (no wrap).
    let lat = q[1] <= c[3] && c[1] <= q[3];
    lat && lon_overlaps(q[0], q[2], c[0], c[2])
}

/// Longitude overlap where a box with `west > east` wraps the anti-meridian.
fn lon_overlaps(qw: f64, qe: f64, cw: f64, ce: f64) -> bool {
    match (qw > qe, cw > ce) {
        (false, false) => qw <= ce && cw <= qe,
        // Exactly one wraps: it covers [w, 180] ∪ [-180, e].
        (true, false) | (false, true) => qw <= ce || cw <= qe,
        // Both wrap: each spans the anti-meridian, so they always overlap.
        (true, true) => true,
    }
}

/// Whole-word (or, for terms containing whitespace, phrase) match of any `q`
/// term against the collection's title or description. Terms are pre-lowercased;
/// comparison is Unicode-case-insensitive (Finnish ä/ö etc.).
///
/// Keyword search is intentionally not included: `CollectionConfig` has no
/// `keywords` field yet (tracked as a follow-up), so matching only the fields
/// that actually exist avoids carrying dead code.
fn q_matches(terms: &[String], title: &str, description: &str) -> bool {
    terms.iter().any(|term| {
        if term.chars().any(char::is_whitespace) {
            // Phrase: case-insensitive substring within a *single* field, so the
            // boundary between two fields can't form a phantom match (e.g. title
            // "Finnish Weather" + description "Helsinki …" must not match
            // "weather helsinki").
            let t = term.as_str();
            title.to_lowercase().contains(t) || description.to_lowercase().contains(t)
        } else {
            word_match(title, term) || word_match(description, term)
        }
    })
}

/// True if `term` (already lower-cased) equals a whole word of `text`. Words
/// are maximal runs of alphanumeric characters (Unicode-aware).
fn word_match(text: &str, term: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| w.to_lowercase() == term)
}

/// Raw `/collections` query parameters, deserialized by the API crates'
/// handlers (`Query<SearchQueryParams>`). Defined here so all four surfaces
/// share one extractor; [`parse`](Self::parse) validates it into
/// [`SearchParams`] and [`query_string`](Self::query_string) rebuilds a
/// link href for the same request at a different offset.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SearchQueryParams {
    pub bbox: Option<String>,
    #[serde(rename = "bbox-crs")]
    pub bbox_crs: Option<String>,
    pub datetime: Option<String>,
    pub q: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
    /// Output-format selector (`json`/`html`) for content negotiation. Not a
    /// search facet — captured here only so the `/collections` handler can read
    /// it without a second, conflicting `Query` extractor. See `ds_core::html`.
    pub f: Option<String>,
}

impl SearchQueryParams {
    /// Validate into [`SearchParams`] (→ HTTP 400 on bad input).
    pub fn parse(&self) -> Result<SearchParams, SearchError> {
        parse_search_params(
            self.bbox.as_deref(),
            self.bbox_crs.as_deref(),
            self.datetime.as_deref(),
            self.q.as_deref(),
            self.limit.as_deref(),
            self.offset.as_deref(),
        )
    }

    /// The query string (leading `?`, or empty) for this request at `offset`,
    /// for building `self`/`next`/`prev` link hrefs. `limit` is the *resolved*
    /// (clamped) value — when the client supplied a `limit`, links reflect the
    /// honoured value, not the raw request (a `limit=9999` clamped to 1000 must
    /// not leak `9999` into links). When the client supplied none, links omit
    /// `limit` so default-page URLs stay clean.
    pub fn query_string(&self, limit: usize, offset: usize) -> String {
        let limit_str = self.limit.as_ref().map(|_| limit.to_string());
        page_query_string(
            self.bbox.as_deref(),
            self.bbox_crs.as_deref(),
            self.datetime.as_deref(),
            self.q.as_deref(),
            limit_str.as_deref(),
            offset,
            self.f.as_deref(),
        )
    }
}

/// Reconstruct the `/collections` query string (leading `?`, or empty) for a
/// `self`/`next`/`prev` link: the original raw filter values plus the given
/// `offset` (omitted when 0). Percent-encodes values; keeps `,` `:` `/`
/// readable (all valid in a query component per RFC 3986).
pub fn page_query_string(
    bbox: Option<&str>,
    bbox_crs: Option<&str>,
    datetime: Option<&str>,
    q: Option<&str>,
    limit: Option<&str>,
    offset: usize,
    f: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |key: &str, val: Option<&str>| {
        if let Some(v) = val.map(str::trim).filter(|s| !s.is_empty()) {
            parts.push(format!("{key}={}", encode_qval(v)));
        }
    };
    push("bbox", bbox);
    push("bbox-crs", bbox_crs);
    push("datetime", datetime);
    push("q", q);
    push("limit", limit);
    // Preserve the requested format across pagination links so an HTML
    // `/collections` page's next/prev links stay HTML (don't revert to JSON).
    push("f", f);
    if offset > 0 {
        parts.push(format!("offset={offset}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// Percent-encode a query-parameter value, leaving unreserved characters and
/// the query-safe sub-delims `,` `:` `/` intact for readability.
fn encode_qval(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b','
            | b':'
            | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn cm<'a>(
        title: &'a str,
        description: &'a str,
        bbox: Option<[f64; 4]>,
        time: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> CollectionMatch<'a> {
        CollectionMatch {
            title,
            description,
            bbox,
            time,
        }
    }

    #[test]
    fn parse_defaults() {
        let p = parse_search_params(None, None, None, None, None, None).unwrap();
        assert_eq!(p.limit, DEFAULT_LIMIT);
        assert_eq!(p.offset, 0);
        assert!(p.bbox.is_none() && p.datetime.is_none() && p.q.is_empty());
    }

    #[test]
    fn parse_bbox_4_and_6() {
        let p = parse_search_params(Some("20,60,30,70"), None, None, None, None, None).unwrap();
        assert_eq!(p.bbox, Some([20.0, 60.0, 30.0, 70.0]));
        // 6-number form drops the vertical axis.
        let p6 =
            parse_search_params(Some("20,60,0,30,70,5000"), None, None, None, None, None).unwrap();
        assert_eq!(p6.bbox, Some([20.0, 60.0, 30.0, 70.0]));
    }

    #[test]
    fn parse_rejects_bad_bbox_and_limit_and_crs() {
        assert!(parse_search_params(Some("1,2,3"), None, None, None, None, None).is_err());
        assert!(parse_search_params(Some("a,b,c,d"), None, None, None, None, None).is_err());
        assert!(parse_search_params(None, None, None, None, Some("0"), None).is_err());
        assert!(parse_search_params(None, None, None, None, Some("-3"), None).is_err());
        assert!(
            parse_search_params(Some("0,0,1,1"), Some("EPSG:3857"), None, None, None, None)
                .is_err()
        );
    }

    #[test]
    fn parse_rejects_nonfinite_and_inverted_bbox() {
        // NaN / Inf must not slip through as a silent empty result.
        assert!(parse_search_params(Some("NaN,60,30,70"), None, None, None, None, None).is_err());
        assert!(parse_search_params(Some("0,60,Inf,70"), None, None, None, None, None).is_err());
        // south > north is always invalid (longitude inversion stays allowed).
        assert!(parse_search_params(Some("0,70,30,60"), None, None, None, None, None).is_err());
        assert!(parse_search_params(Some("170,50,-170,60"), None, None, None, None, None).is_ok());
        // out-of-range latitude / longitude rejected.
        assert!(parse_search_params(Some("0,200,10,300"), None, None, None, None, None).is_err());
        assert!(parse_search_params(Some("200,0,300,10"), None, None, None, None, None).is_err());
    }

    #[test]
    fn phrase_q_does_not_match_across_fields() {
        let items = [cm("Finnish Weather", "Helsinki radar", None, None)];
        // "weather helsinki" spans the title→description boundary — must NOT match.
        let across =
            parse_search_params(None, None, None, Some("weather helsinki"), None, None).unwrap();
        assert_eq!(search(&items, &across).number_matched, 0);
        // a phrase within a single field still matches.
        let within =
            parse_search_params(None, None, None, Some("finnish weather"), None, None).unwrap();
        assert_eq!(search(&items, &within).number_matched, 1);
    }

    #[test]
    fn no_prev_exactly_off_the_end() {
        let items: Vec<CollectionMatch> = (0..4).map(|_| cm("a", "", None, None)).collect();
        // offset == number_matched: empty page, no prev (prev implies a result page).
        let p = parse_search_params(None, None, None, None, Some("2"), Some("4")).unwrap();
        let r = search(&items, &p);
        assert!(r.page.is_empty());
        assert!(!r.has_prev);
    }

    #[test]
    fn parse_rejects_inverted_datetime() {
        assert!(parse_search_params(
            None,
            None,
            Some("2024-12-01T00:00:00Z/2024-01-01T00:00:00Z"),
            None,
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn parse_accepts_crs84_bbox_crs_forms() {
        for crs in [
            "CRS84",
            "OGC:CRS84",
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        ] {
            assert!(
                parse_search_params(Some("0,0,1,1"), Some(crs), None, None, None, None).is_ok()
            );
        }
    }

    #[test]
    fn limit_clamped_to_max() {
        let p = parse_search_params(None, None, None, None, Some("99999"), None).unwrap();
        assert_eq!(p.limit, MAX_LIMIT);
    }

    #[test]
    fn bbox_filter_includes_and_excludes() {
        let inside = cm("a", "", Some([20.0, 60.0, 25.0, 65.0]), None);
        let outside = cm("b", "", Some([-10.0, -10.0, -5.0, -5.0]), None);
        // "unknown extent ≡ unbounded": a collection with no bbox matches.
        let no_bbox = cm("c", "", None, None);
        let items = [inside, outside, no_bbox];
        let p = parse_search_params(Some("0,50,30,70"), None, None, None, None, None).unwrap();
        let r = search(&items, &p);
        // intersecting (0) + unbounded no-bbox (2); the disjoint one (1) is out.
        assert_eq!(r.page, vec![0, 2]);
        assert_eq!(r.number_matched, 2);
    }

    #[test]
    fn unknown_extent_is_unbounded() {
        // No spatial and no temporal extent → matches both bbox and datetime
        // filters (OGC API – Common – Part 4 §7.14.2 / §7.14.3).
        let items = [cm("a", "", None, None)];
        let bbox = parse_search_params(Some("0,0,1,1"), None, None, None, None, None).unwrap();
        assert_eq!(search(&items, &bbox).number_matched, 1);
        let dt = parse_search_params(None, None, Some("2026-06-02T00:00:00Z"), None, None, None)
            .unwrap();
        assert_eq!(search(&items, &dt).number_matched, 1);
    }

    #[test]
    fn prev_suppressed_when_offset_out_of_range() {
        let items: Vec<CollectionMatch> = (0..3).map(|_| cm("a", "", None, None)).collect();
        // offset far beyond the 3 matches: empty page, and no `prev` chain.
        let p = parse_search_params(None, None, None, None, Some("2"), Some("5000")).unwrap();
        let r = search(&items, &p);
        assert!(r.page.is_empty());
        assert!(!r.has_prev, "out-of-range offset must not offer prev");
    }

    #[test]
    fn antimeridian_bbox_overlaps() {
        // Query wraps the anti-meridian: [170, -170] covers 170..180 and -180..-170.
        assert!(lon_overlaps(170.0, -170.0, 175.0, 178.0));
        assert!(lon_overlaps(170.0, -170.0, -179.0, -171.0));
        assert!(!lon_overlaps(170.0, -170.0, 0.0, 10.0));
    }

    #[test]
    fn datetime_overlap_filter() {
        let c = cm(
            "a",
            "",
            None,
            Some((t("2026-06-01T00:00:00Z"), t("2026-06-03T00:00:00Z"))),
        );
        let items = [c];
        let hit = parse_search_params(None, None, Some("2026-06-02T12:00:00Z"), None, None, None)
            .unwrap();
        assert_eq!(search(&items, &hit).number_matched, 1);
        let miss = parse_search_params(None, None, Some("2026-07-01T00:00:00Z"), None, None, None)
            .unwrap();
        assert_eq!(search(&items, &miss).number_matched, 0);
    }

    #[test]
    fn q_whole_word_and_unicode() {
        let c = cm("Helsinki Radar", "Reflectivity composite sää", None, None);
        let items = [c];
        // whole word, case-insensitive
        assert_eq!(
            search(
                &items,
                &parse_search_params(None, None, None, Some("radar"), None, None).unwrap()
            )
            .number_matched,
            1
        );
        // whole word in description, Unicode case-insensitive (SÄÄ -> sää)
        assert_eq!(
            search(
                &items,
                &parse_search_params(None, None, None, Some("SÄÄ"), None, None).unwrap()
            )
            .number_matched,
            1
        );
        // partial word does NOT match whole-word term
        assert_eq!(
            search(
                &items,
                &parse_search_params(None, None, None, Some("rada"), None, None).unwrap()
            )
            .number_matched,
            0
        );
        // phrase (whitespace) → substring
        assert_eq!(
            search(
                &items,
                &parse_search_params(None, None, None, Some("helsinki radar"), None, None).unwrap()
            )
            .number_matched,
            1
        );
    }

    #[test]
    fn pagination_cursors() {
        let items: Vec<CollectionMatch> = (0..5).map(|_| cm("a", "", None, None)).collect();
        let p = parse_search_params(None, None, None, None, Some("2"), Some("2")).unwrap();
        let r = search(&items, &p);
        assert_eq!(r.number_matched, 5);
        assert_eq!(r.page, vec![2, 3]);
        assert!(r.has_next && r.next_offset == 4);
        assert!(r.has_prev && r.prev_offset == 0);
    }

    #[test]
    fn query_string_roundtrip() {
        let qs = page_query_string(
            Some("20,60,30,70"),
            None,
            Some("2026-06-02T00:00:00Z"),
            Some("radar"),
            Some("10"),
            10,
            None,
        );
        assert_eq!(
            qs,
            "?bbox=20,60,30,70&datetime=2026-06-02T00:00:00Z&q=radar&limit=10&offset=10"
        );
        // No params, offset 0 → empty (backward compatible self link).
        assert_eq!(page_query_string(None, None, None, None, None, 0, None), "");
        // Spaces get percent-encoded.
        assert_eq!(
            page_query_string(None, None, None, Some("heavy rain"), None, 0, None),
            "?q=heavy%20rain"
        );
    }

    #[test]
    fn query_string_preserves_format_across_pagination() {
        // An HTML /collections page's next/prev links must keep ?f=html.
        let sp = SearchQueryParams {
            f: Some("html".to_string()),
            ..Default::default()
        };
        assert_eq!(sp.query_string(DEFAULT_LIMIT, 20), "?f=html&offset=20");
        // No format requested → links omit f.
        let none = SearchQueryParams::default();
        assert_eq!(none.query_string(DEFAULT_LIMIT, 20), "?offset=20");
    }

    #[test]
    fn query_string_reflects_clamped_limit() {
        // Client asked for an over-large limit; links must carry the resolved
        // (clamped) value, not the raw request.
        let sp = SearchQueryParams {
            limit: Some("99999".to_string()),
            ..Default::default()
        };
        let resolved = sp.parse().unwrap().limit; // == MAX_LIMIT
        assert_eq!(resolved, MAX_LIMIT);
        assert_eq!(sp.query_string(resolved, 0), format!("?limit={MAX_LIMIT}"));
        // Client supplied no limit → links omit it (clean default-page URL).
        let none = SearchQueryParams::default();
        assert_eq!(none.query_string(DEFAULT_LIMIT, 0), "");
    }
}
