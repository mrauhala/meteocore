//! OGC API – Common – Part 4 ("Discovery within many collections", draft
//! [25-046]) — a subset of the **Searchable Collections** requirements class.
//! The 2026-09-17 draft also requires `sd` and `resolution`; those are not
//! implemented, so API layers must not advertise that class yet.
//!
//! Filtering, sorting and pagination for the `/collections` resource: `bbox` /
//! `bbox-crs`, `datetime`, `q`, `query`, `sortby` (§8, by `title` or `id`
//! only, #1141), and `limit` + `offset`. The logic lives here
//! (not in the API crates) so EDR, Maps, Tiles, and Features share one
//! implementation — ds-core never builds `serde_json::Value`; api-common uses
//! [`SearchQueryParams::parse`] + [`search`] and assembles HTTP responses/links.
//!
//! Only CRS84 is supported for `bbox` / `bbox-crs` (every collection bbox is
//! advertised in CRS84). The Sortable class's sortables resource, and the
//! Filterable (CQL2) / Hierarchical classes, are intentionally not implemented.
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
#[error("{message}")]
pub struct SearchError {
    message: String,
    parameter: Option<String>,
}

impl SearchError {
    fn new(msg: impl Into<String>) -> Self {
        SearchError {
            message: msg.into(),
            parameter: None,
        }
    }

    /// An error about the query parameter `name`.
    fn at(name: &str, msg: impl Into<String>) -> Self {
        SearchError {
            message: msg.into(),
            parameter: Some(name.to_owned()),
        }
    }

    /// The query parameter at fault, as the request named it: an HTML
    /// catalog marks that field (#1132). `None` when no single one is.
    pub fn parameter(&self) -> Option<&str> {
        self.parameter.as_deref()
    }

    fn naming(mut self, name: &str) -> Self {
        self.parameter = Some(name.to_owned());
        self
    }
}

/// The searchable facets of one collection. Borrows from the caller's config /
/// engine snapshot; build one per collection in the same order as the response
/// list, then pass the slice to [`search`].
#[derive(Debug, Clone)]
pub struct CollectionMatch<'a> {
    /// The collection id: a `sortby` key, and the tie-break of every order.
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    /// The collection's configured keywords (may be empty). Matched by `q`
    /// and `query` alongside title and description.
    pub keywords: &'a [String],
    /// The ids of the theme concepts its description advertises (#1356),
    /// WIS2 topic paths such as `weather/prediction/forecast`. Matched like
    /// keywords; `/` separates words, so a broader topic matches the
    /// deeper ones below it.
    pub themes: Vec<&'a str>,
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
    /// OR alternatives of required/excluded phrases. Empty = no `query`.
    pub query: Vec<TextAlternative>,
    /// The `sortby` keys, applied in turn, ties by id. Empty = no `sortby`:
    /// [`search`] keeps the input order, and the caller picks the default.
    pub sortby: Vec<CollectionSortKey>,
    pub limit: usize,
    pub offset: usize,
}

/// One comma-separated `query` alternative. All required phrases must match;
/// no excluded phrase may match. Separate phrases can match separate properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextAlternative {
    required: Vec<String>,
    excluded: Vec<String>,
}

/// A collection property `sortby` can order the list by (draft 25-046 §8).
/// Only these two: a collection description has no other member every
/// collection carries, and none that says when its data last changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionSortProperty {
    Title,
    Id,
}

impl CollectionSortProperty {
    pub const ALL: &'static [Self] = &[Self::Title, Self::Id];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Id => "id",
        }
    }
}

/// One collection-list `sortby` key: a property, ascending unless
/// `descending`. Unlike [`crate::feature::SortKey`], which names any
/// property an engine lists as sortable, it names one of the two
/// properties every collection description has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionSortKey {
    pub property: CollectionSortProperty,
    pub descending: bool,
}

impl CollectionSortKey {
    pub const fn ascending(property: CollectionSortProperty) -> Self {
        Self {
            property,
            descending: false,
        }
    }
}

impl std::fmt::Display for CollectionSortKey {
    /// The key as `sortby` writes it: `title`, or `-title` descending.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sign = if self.descending { "-" } else { "" };
        write!(f, "{sign}{}", self.property.name())
    }
}

/// `keys` as one `sortby` value: `-title,id`.
pub fn sortby_value(keys: &[CollectionSortKey]) -> String {
    keys.iter()
        .map(CollectionSortKey::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse a `sortby` value: comma-separated property names, each ascending
/// or prefixed `+` (ascending) or `-` (descending). Form decoding turns an
/// unescaped `+` into a space, which is the same ascending order. The edge
/// cases follow the Features `/items` parser: whitespace around a key and
/// after its sign is ignored, and a property may be named only once.
fn parse_sortby(value: &str) -> Result<Vec<CollectionSortKey>, SearchError> {
    let sortable = || {
        CollectionSortProperty::ALL
            .iter()
            .map(|p| p.name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut keys: Vec<CollectionSortKey> = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        let (descending, name) = match item.strip_prefix('-') {
            Some(name) => (true, name),
            None => (false, item.strip_prefix('+').unwrap_or(item)),
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(SearchError::new(format!(
                "sortby: each comma-separated key must name a property: {}",
                sortable()
            )));
        }
        let property = CollectionSortProperty::ALL
            .iter()
            .copied()
            .find(|p| p.name() == name)
            .ok_or_else(|| {
                SearchError::new(format!(
                    "Unknown sortby property '{name}'; sortable: {}",
                    sortable()
                ))
            })?;
        if keys.iter().any(|k| k.property == property) {
            return Err(SearchError::new(format!(
                "sortby: property '{name}' listed more than once"
            )));
        }
        keys.push(CollectionSortKey {
            property,
            descending,
        });
    }
    Ok(keys)
}

/// Sort `indices` into `items` by `keys`, then by id. A title compares
/// case-insensitively, with whitespace normalized; an empty title sorts as
/// the id, which is what a list shows for it.
fn sort_matches(items: &[CollectionMatch], indices: &mut [usize], keys: &[CollectionSortKey]) {
    let titles: Vec<String> = if keys
        .iter()
        .any(|k| k.property == CollectionSortProperty::Title)
    {
        items
            .iter()
            .map(|it| {
                let shown = if it.title.trim().is_empty() {
                    it.id
                } else {
                    it.title
                };
                normalize_text(shown)
            })
            .collect()
    } else {
        Vec::new()
    };
    indices.sort_by(|&a, &b| {
        keys.iter()
            .map(|key| {
                let order = match key.property {
                    CollectionSortProperty::Title => titles[a]
                        .cmp(&titles[b])
                        .then_with(|| items[a].title.cmp(items[b].title)),
                    CollectionSortProperty::Id => items[a].id.cmp(items[b].id),
                };
                if key.descending {
                    order.reverse()
                } else {
                    order
                }
            })
            .find(|order| order.is_ne())
            .unwrap_or_else(|| items[a].id.cmp(items[b].id))
    });
}

/// The outcome of a [`search`]: the total match count and the indices of the
/// items on the requested page, plus pagination cursors.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Total collections matching the filters (before pagination).
    pub number_matched: usize,
    /// Indices into the `items` slice for this page, in `sortby` order, or
    /// in input order without `sortby`.
    pub page: Vec<usize>,
    pub has_next: bool,
    pub next_offset: usize,
    pub has_prev: bool,
    pub prev_offset: usize,
}

/// Parse and validate the raw `/collections` query parameters. Returns
/// [`SearchError`] (→ HTTP 400) for malformed input. For the complete set
/// including advanced `query`, use [`SearchQueryParams::parse`].
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
            return Err(SearchError::at(
                CollectionParameter::BboxCrs.name(),
                "Only CRS84 is supported for bbox-crs (e.g. \
                 http://www.opengis.net/def/crs/OGC/1.3/CRS84)",
            ));
        }
    }

    let bbox = match bbox.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(parse_bbox(s).map_err(|e| e.naming(CollectionParameter::Bbox.name()))?),
        None => None,
    };

    let datetime = match datetime.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let (start, end) = parse_datetime_interval(s).map_err(|e| {
                SearchError::at(
                    CollectionParameter::Datetime.name(),
                    format!("Invalid datetime: {e}"),
                )
            })?;
            // An inverted interval (end before start) would silently exclude
            // every overlapping collection; reject it as a 400.
            if start > end {
                return Err(SearchError::at(
                    CollectionParameter::Datetime.name(),
                    "datetime: start must be <= end",
                ));
            }
            Some((start, end))
        }
        None => None,
    };

    let q = match q.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s
            .split(',')
            .map(normalize_text)
            .filter(|t| !t.is_empty())
            .collect(),
        None => Vec::new(),
    };

    let limit = match limit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let n: usize = s.parse().map_err(|_| {
                SearchError::at(
                    CollectionParameter::Limit.name(),
                    format!("Invalid limit: '{s}' is not a positive integer"),
                )
            })?;
            if n < 1 {
                return Err(SearchError::at(
                    CollectionParameter::Limit.name(),
                    "limit must be >= 1",
                ));
            }
            n.min(MAX_LIMIT)
        }
        None => DEFAULT_LIMIT,
    };

    let offset = match offset.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.parse().map_err(|_| {
            SearchError::at(
                CollectionParameter::Offset.name(),
                format!("Invalid offset: '{s}' is not a non-negative integer"),
            )
        })?,
        None => 0,
    };

    Ok(SearchParams {
        bbox,
        datetime,
        q,
        query: Vec::new(),
        sortby: Vec::new(),
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

/// Filter `items` by the parameters, sort the matches by `sortby` and
/// apply `offset`/`limit` pagination.
pub fn search(items: &[CollectionMatch], p: &SearchParams) -> SearchResult {
    let mut matched: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches(it, p))
        .map(|(i, _)| i)
        .collect();
    if !p.sortby.is_empty() {
        sort_matches(items, &mut matched, &p.sortby);
    }

    let window = page_window(matched.len(), p.offset, p.limit);
    SearchResult {
        number_matched: matched.len(),
        page: matched[window.range()].to_vec(),
        has_next: window.has_next,
        next_offset: window.next_offset,
        has_prev: window.has_prev,
        prev_offset: window.prev_offset,
    }
}

/// One `offset`/`limit` page over `number_matched` ordered items: the
/// arithmetic behind `/collections` paging, shared with every other
/// offset-paged list (EDR `/locations`, #922) so their links agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindow {
    /// First item on the page (`number_matched` when `offset` is past the end).
    pub start: usize,
    /// One past the last item on the page.
    pub end: usize,
    pub has_next: bool,
    pub next_offset: usize,
    pub has_prev: bool,
    pub prev_offset: usize,
}

impl PageWindow {
    /// The page's item indices; empty when `offset` is past the end.
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }
}

/// Page `number_matched` ordered items at `offset`, at most `limit` per page.
pub fn page_window(number_matched: usize, offset: usize, limit: usize) -> PageWindow {
    let end = offset.saturating_add(limit).min(number_matched);
    PageWindow {
        start: offset.min(number_matched),
        end,
        has_next: end < number_matched,
        next_offset: offset.saturating_add(limit),
        // Offer `prev` only from a non-empty result page (`offset <
        // number_matched`). An out-of-range or exactly-off-the-end `offset`
        // yields an empty page with no `prev`, since `prev` implies a
        // preceding result page. A `next` link never produces `offset ==
        // number_matched`, so normal paging is unaffected.
        has_prev: offset > 0 && offset < number_matched,
        prev_offset: offset.saturating_sub(limit),
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
    if p.q.is_empty() && p.query.is_empty() {
        return true;
    }
    // Normalize each field once; never join properties or keyword entries,
    // which would manufacture phrase matches across metadata boundaries.
    let fields: Vec<_> = [it.title, it.description]
        .into_iter()
        .chain(it.keywords.iter().map(String::as_str))
        .chain(it.themes.iter().copied())
        .map(normalize_text)
        .collect();
    let contains = |term: &String| fields.iter().any(|field| text_match(field, term));
    (p.q.is_empty() || p.q.iter().any(contains))
        && (p.query.is_empty()
            || p.query.iter().any(|alternative| {
                alternative.required.iter().all(contains)
                    && !alternative.excluded.iter().any(contains)
            }))
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

/// Unicode lowercase and whitespace normalization, preserving punctuation.
fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Match normalized literal text with alphanumeric word boundaries. Internal
/// punctuation is literal (e.g. united-states); only whitespace can separate
/// words in a phrase. This also rejects partial words at either phrase edge.
fn text_match(text: &str, term: &str) -> bool {
    text.char_indices().any(|(start, _)| {
        let end = start + term.len();
        !text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
            && text[start..].starts_with(term)
            && !text[end..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
    })
}

/// Draft 25-046 §7.7: commas have lower precedence than required/excluded
/// terms. An operator starts a new phrase only at a whitespace/item boundary;
/// internal +/- remain literal. A negative-only alternative is a complement.
fn parse_query(query: &str) -> Result<Vec<TextAlternative>, SearchError> {
    query
        .split(',')
        .map(|item| {
            let mut alternative = TextAlternative {
                required: Vec::new(),
                excluded: Vec::new(),
            };
            let mut phrase = String::new();
            let mut excluded = false;
            let finish = |phrase: &mut String, excluded, alternative: &mut TextAlternative| {
                if !phrase.is_empty() {
                    let target = if excluded {
                        &mut alternative.excluded
                    } else {
                        &mut alternative.required
                    };
                    target.push(std::mem::take(phrase));
                }
            };
            for word in item.split_whitespace() {
                let word = if word.starts_with(['+', '-']) {
                    finish(&mut phrase, excluded, &mut alternative);
                    excluded = word.starts_with('-');
                    let term = &word[1..];
                    if term.is_empty() {
                        return Err(SearchError::new(
                            "query: '+' and '-' must immediately prefix a term",
                        ));
                    }
                    term
                } else {
                    word
                };
                if !phrase.is_empty() {
                    phrase.push(' ');
                }
                phrase.push_str(&word.to_lowercase());
            }
            finish(&mut phrase, excluded, &mut alternative);
            if alternative.required.is_empty() && alternative.excluded.is_empty() {
                return Err(SearchError::new(
                    "query must contain non-empty comma-separated alternatives",
                ));
            }
            Ok(alternative)
        })
        .collect()
}

/// Supported collection discovery controls. HTTP validation and OpenAPI use
/// this inventory; new controls must also be implemented in the parser/search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionParameter {
    Bbox,
    BboxCrs,
    Datetime,
    Q,
    Query,
    Sortby,
    Limit,
    Offset,
    Format,
}

impl CollectionParameter {
    pub const ALL: &'static [Self] = &[
        Self::Bbox,
        Self::BboxCrs,
        Self::Datetime,
        Self::Q,
        Self::Query,
        Self::Sortby,
        Self::Limit,
        Self::Offset,
        Self::Format,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Bbox => "bbox",
            Self::BboxCrs => "bbox-crs",
            Self::Datetime => "datetime",
            Self::Q => "q",
            Self::Query => "query",
            Self::Sortby => "sortby",
            Self::Limit => "limit",
            Self::Offset => "offset",
            Self::Format => "f",
        }
    }
}

/// Raw `/collections` query parameters, constructed from decoded pairs by the
/// shared api-common extractor. Unsupported and duplicate controls are rejected;
/// [`parse`](Self::parse) validates it into
/// [`SearchParams`] and [`query_string`](Self::query_string) rebuilds a
/// link href for the same request at a different offset.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchQueryParams {
    pub bbox: Option<String>,
    #[serde(rename = "bbox-crs")]
    pub bbox_crs: Option<String>,
    pub datetime: Option<String>,
    pub q: Option<String>,
    pub query: Option<String>,
    pub sortby: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
    /// Output-format selector (`json`/`html`) for content negotiation. Not a
    /// search facet — captured here only so the `/collections` handler can read
    /// it without a second, conflicting `Query` extractor. See `ds_core::html`.
    pub f: Option<String>,
}

impl SearchQueryParams {
    /// Consume decoded query pairs without silently discarding unsupported or
    /// duplicate controls. Keep percent decoding at the HTTP boundary.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, SearchError> {
        let mut params = Self::default();
        for (name, value) in pairs {
            let parameter = CollectionParameter::ALL
                .iter()
                .find(|p| p.name() == name)
                .ok_or_else(|| {
                    SearchError::at(
                        &name,
                        format!(
                            "Unknown collection query parameter '{name}'; supported: {}",
                            CollectionParameter::ALL
                                .iter()
                                .map(|p| p.name())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?;
            let slot = match parameter {
                CollectionParameter::Bbox => &mut params.bbox,
                CollectionParameter::BboxCrs => &mut params.bbox_crs,
                CollectionParameter::Datetime => &mut params.datetime,
                CollectionParameter::Q => &mut params.q,
                CollectionParameter::Query => &mut params.query,
                CollectionParameter::Sortby => &mut params.sortby,
                CollectionParameter::Limit => &mut params.limit,
                CollectionParameter::Offset => &mut params.offset,
                CollectionParameter::Format => &mut params.f,
            };
            if slot.replace(value).is_some() {
                return Err(SearchError::at(
                    &name,
                    format!("Duplicate collection query parameter '{name}'"),
                ));
            }
        }
        Ok(params)
    }

    /// Validate into [`SearchParams`] (→ HTTP 400 on bad input).
    pub fn parse(&self) -> Result<SearchParams, SearchError> {
        let mut params = parse_search_params(
            self.bbox.as_deref(),
            self.bbox_crs.as_deref(),
            self.datetime.as_deref(),
            self.q.as_deref(),
            self.limit.as_deref(),
            self.offset.as_deref(),
        )?;
        if let Some(query) = &self.query {
            params.query =
                parse_query(query).map_err(|e| e.naming(CollectionParameter::Query.name()))?;
        }
        if let Some(sortby) = self.sortby.as_deref().filter(|s| !s.trim().is_empty()) {
            params.sortby =
                parse_sortby(sortby).map_err(|e| e.naming(CollectionParameter::Sortby.name()))?;
        }
        Ok(params)
    }

    /// This request with its page size and order spelled out: `limit` and
    /// `sortby` set to the values the response used where the request gave
    /// none. A link to the other representation needs them, because JSON and
    /// HTML default to different ones (#1141).
    pub fn explicit_page(&self, limit: usize, sortby: &[CollectionSortKey]) -> Self {
        let mut explicit = self.clone();
        if explicit
            .limit
            .as_deref()
            .is_none_or(|l| l.trim().is_empty())
        {
            explicit.limit = Some(limit.to_string());
        }
        if !sortby.is_empty()
            && explicit
                .sortby
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
        {
            explicit.sortby = Some(sortby_value(sortby));
        }
        explicit
    }

    /// The query string (leading `?`, or empty) for this request at `offset`,
    /// for building `self`/`next`/`prev` link hrefs. `limit` is the *resolved*
    /// (clamped) value — when the client supplied a `limit`, links reflect the
    /// honoured value, not the raw request (a `limit=9999` clamped to 1000 must
    /// not leak `9999` into links). When the client supplied none, links omit
    /// `limit` so default-page URLs stay clean.
    pub fn query_string(&self, limit: usize, offset: usize) -> String {
        self.query_string_for_format(limit, offset, self.f.as_deref())
    }

    /// Like [`query_string`](Self::query_string) but forces the `f` (format)
    /// selector instead of echoing the request's own. Used to build a
    /// `rel="alternate"` link from one representation to the other while
    /// preserving every search/pagination parameter (bbox, datetime, q, query,
    /// limit, offset).
    pub fn query_string_with_format(&self, limit: usize, offset: usize, f: &str) -> String {
        self.query_string_for_format(limit, offset, Some(f))
    }

    fn query_string_for_format(&self, limit: usize, offset: usize, f: Option<&str>) -> String {
        let limit_str = self.limit.as_ref().map(|_| limit.to_string());
        let mut result = page_query_string(
            self.bbox.as_deref(),
            self.bbox_crs.as_deref(),
            self.datetime.as_deref(),
            self.q.as_deref(),
            limit_str.as_deref(),
            offset,
            f,
        );
        if let Some(query) = &self.query {
            result.push(if result.is_empty() { '?' } else { '&' });
            result.push_str("query=");
            result.push_str(&encode_query_value(query));
        }
        if let Some(sortby) = self.sortby.as_deref().filter(|s| !s.trim().is_empty()) {
            result.push(if result.is_empty() { '?' } else { '&' });
            result.push_str("sortby=");
            result.push_str(&encode_query_value(sortby));
        }
        result
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
            parts.push(format!("{key}={}", encode_query_value(v)));
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

/// Percent-encode a query-parameter name or value for a paging link, leaving
/// unreserved characters and the query-safe sub-delims `,` `:` `/` intact for
/// readability.
pub fn encode_query_value(s: &str) -> String {
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
            id: "",
            title,
            description,
            keywords: &[],
            themes: Vec::new(),
            bbox,
            time,
        }
    }

    /// Like [`cm`] but with keywords, for the `q`-over-keywords tests.
    fn cm_kw<'a>(
        title: &'a str,
        description: &'a str,
        keywords: &'a [String],
    ) -> CollectionMatch<'a> {
        CollectionMatch {
            id: "",
            title,
            description,
            keywords,
            themes: Vec::new(),
            bbox: None,
            time: None,
        }
    }

    fn text_params(q: Option<&str>, query: Option<&str>) -> SearchParams {
        SearchQueryParams {
            q: q.map(str::to_owned),
            query: query.map(str::to_owned),
            ..Default::default()
        }
        .parse()
        .unwrap()
    }

    #[test]
    fn draft_query_examples_and_operator_precedence() {
        let keywords = ["weather".into(), "extreme weather".into()];
        let items = [
            cm("Canada", "Weather observations", None, None),
            cm("Canada", "Extreme weather", None, None),
            cm("United States", "Weather observations", None, None),
            cm("United western states", "", None, None),
            cm("United-states", "", None, None),
            cm_kw("Canada", "", &keywords),
        ];
        for (query, expected) in [
            ("canada +weather -extreme", vec![0]),
            (
                "canada +weather -extreme,united states +weather -extreme",
                vec![0, 2],
            ),
            ("united +states", vec![2, 3, 4]),
            ("united-states", vec![4]),
            ("canada +extreme weather", vec![1, 5]),
            ("united,states", vec![2, 3, 4]),
            ("+weather -extreme", vec![0, 2]),
            ("-weather", vec![3, 4]),
            // Exclusion belongs to its OR alternative, not the entire query.
            ("canada -extreme,+extreme", vec![0, 1, 5]),
            ("canada +weather -weather", vec![]),
            ("WEATHER +CANADA -EXTREME", vec![0]),
        ] {
            assert_eq!(
                search(&items, &text_params(None, Some(query))).page,
                expected,
                "{query}"
            );
        }
    }

    #[test]
    fn q_and_query_share_whitespace_and_phrase_boundaries() {
        let keywords = ["SEA\tSURFACE\nTemperature".into()];
        let items = [
            cm("Sea\t  surface\n temperature", "", None, None),
            cm("undersea surface temperatures", "", None, None),
            cm("sea,surface temperature", "", None, None),
            cm("sea surface", "temperature", None, None),
            cm_kw("", "", &keywords),
            cm("Sea\u{a0}surface\u{2003}temperature", "", None, None),
            cm("undersea surface temperature", "", None, None),
            cm("sea surface temperatures", "", None, None),
        ];
        for term in ["sea surface temperature", "SEA\n  SURFACE\tTEMPERATURE"] {
            for params in [text_params(Some(term), None), text_params(None, Some(term))] {
                assert_eq!(search(&items, &params).page, vec![0, 4, 5]);
            }
        }
        // Substring candidates can overlap; an earlier partial-word candidate
        // must not hide a later whole phrase.
        let items = [cm("aa a a", "", None, None)];
        assert_eq!(
            search(&items, &text_params(Some("a a"), None)).page,
            vec![0]
        );
    }

    #[test]
    fn query_phrases_stay_within_fields_but_requirements_can_span_them() {
        let keywords = ["sea".into(), "surface temperature".into()];
        let items = [cm_kw("Finnish", "Weather", &keywords)];
        for query in [
            "finnish weather",
            "sea surface",
            "+sea surface",
            "surface +finnish weather",
        ] {
            assert_eq!(
                search(&items, &text_params(None, Some(query))).number_matched,
                0,
                "{query}"
            );
        }
        for query in [
            "finnish +weather +sea +surface temperature",
            "-sea surface",
            "finnish -sea surface",
        ] {
            assert_eq!(
                search(&items, &text_params(None, Some(query))).number_matched,
                1,
                "{query}"
            );
        }
        assert_eq!(
            search(
                &items,
                &text_params(None, Some("finnish -surface temperature"))
            )
            .number_matched,
            0
        );
    }

    #[test]
    fn query_internal_signs_are_literal_and_matching_is_case_insensitive() {
        let items = [
            cm("SÄÄ C++ united-states radar+hail", "", None, None),
            cm("sää c united states radar hail", "", None, None),
        ];
        for query in [
            "SÄÄ +c++",
            "united-states",
            "radar+hail",
            "-united-states +SÄÄ",
        ] {
            let expected = if query.starts_with('-') {
                vec![1]
            } else {
                vec![0]
            };
            assert_eq!(
                search(&items, &text_params(None, Some(query))).page,
                expected,
                "{query}"
            );
        }
        assert_eq!(
            search(&items, &text_params(Some("united-states"), None)).page,
            vec![0]
        );
    }

    #[test]
    fn q_and_query_are_combined_with_and() {
        let items = [
            cm("Radar", "Weather", None, None),
            cm("Wind", "Weather", None, None),
        ];
        assert_eq!(
            search(&items, &text_params(Some("radar"), Some("+weather"))).page,
            vec![0]
        );
        assert_eq!(
            search(&items, &text_params(Some("radar"), Some("wind"))).number_matched,
            0
        );
    }

    #[test]
    fn query_rejects_empty_alternatives_and_dangling_operators() {
        for query in [
            "",
            " ",
            ",",
            "radar,",
            ",radar",
            "radar,,wind",
            "+",
            "-",
            "radar +",
            "radar - hail",
            "+ weather",
        ] {
            let raw = SearchQueryParams {
                query: Some(query.into()),
                ..Default::default()
            };
            assert!(raw.parse().is_err(), "{query:?}");
        }
    }

    #[test]
    fn query_navigation_preserves_encoded_operators() {
        let raw = SearchQueryParams {
            query: Some("radar +SÄÄ -extreme weather,united-states".into()),
            ..Default::default()
        };
        for link in [
            raw.query_string(1, 2),
            raw.query_string_with_format(1, 2, "html"),
        ] {
            assert!(
                link.contains("query=radar%20%2BS%C3%84%C3%84%20-extreme%20weather,united-states"),
                "{link}"
            );
            assert!(link.contains("offset=2"));
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

    /// Each rejection names the parameter at fault, which an HTML catalog
    /// marks (#1132), and keeps its message.
    #[test]
    fn rejections_name_the_parameter_at_fault() {
        let fault = |pairs: &[(&str, &str)]| {
            let pairs = pairs
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            let error = SearchQueryParams::from_pairs(pairs)
                .and_then(|query| query.parse())
                .unwrap_err();
            (error.parameter().map(str::to_owned), error.to_string())
        };
        for (pairs, parameter, message) in [
            (
                &[("bbox", "1,2,3")][..],
                "bbox",
                "bbox must have 4 or 6 numbers",
            ),
            (
                &[("datetime", "yesterday")],
                "datetime",
                "Invalid datetime: ",
            ),
            (
                &[("datetime", "2026-01-02T00:00:00Z/2026-01-01T00:00:00Z")],
                "datetime",
                "datetime: start must be <= end",
            ),
            (&[("bbox-crs", "EPSG:3857")], "bbox-crs", "Only CRS84"),
            (&[("limit", "0")], "limit", "limit must be >= 1"),
            (&[("offset", "x")], "offset", "Invalid offset: 'x'"),
            (&[("query", "+")], "query", "query: '+' and '-'"),
            (
                &[("q", "a"), ("q", "b")],
                "q",
                "Duplicate collection query parameter 'q'",
            ),
            (
                &[("nope", "1")],
                "nope",
                "Unknown collection query parameter 'nope'",
            ),
        ] {
            let (got, text) = fault(pairs);
            assert_eq!(got.as_deref(), Some(parameter), "{pairs:?}");
            assert!(text.starts_with(message), "{pairs:?}: {text}");
        }
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
    fn q_matches_keywords() {
        let kws = [
            "precipitation".to_string(),
            "sea surface temperature".to_string(),
        ];
        let items = [cm_kw("Radar", "Composite", &kws)];
        let n = |q: &str| {
            search(
                &items,
                &parse_search_params(None, None, None, Some(q), None, None).unwrap(),
            )
            .number_matched
        };
        // whole keyword, case-insensitive
        assert_eq!(n("Precipitation"), 1);
        // whole word *within* a multi-word keyword
        assert_eq!(n("surface"), 1);
        // partial word does not match
        assert_eq!(n("precip"), 0);
        // phrase substring across a keyword
        assert_eq!(n("surface temperature"), 1);
        // term present in neither title/description nor keywords
        assert_eq!(n("snow"), 0);
    }

    /// Theme concept ids are searched like keywords (#1356): a broader WIS2
    /// topic finds the collections classified below it, a cut level none.
    #[test]
    fn q_and_query_match_theme_ids() {
        let mut nwp = cm("ECMWF", "Forecast", None, None);
        nwp.themes = vec!["weather/prediction/forecast/medium-range/deterministic/global"];
        let mut radar = cm("Radar", "Composite", None, None);
        radar.themes = vec!["weather/surface-based-observations/weather-radar"];
        let items = [nwp, radar];
        let matched =
            |q: Option<&str>, query: Option<&str>| search(&items, &text_params(q, query)).page;
        assert_eq!(matched(Some("weather/prediction"), None), [0]);
        assert_eq!(
            matched(
                Some("weather/prediction/forecast/medium-range/deterministic/global"),
                None
            ),
            [0]
        );
        assert_eq!(matched(Some("weather"), None), [0, 1]);
        assert_eq!(matched(Some("weather-radar"), None), [1]);
        assert!(matched(Some("weather/predictio"), None).is_empty());
        assert!(matched(Some("weather/prediction/forecasts"), None).is_empty());
        assert_eq!(matched(None, Some("weather -weather/prediction")), [1]);
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
    fn page_window_edges() {
        // Last, partial page: no next, prev one full page back.
        let w = page_window(5, 4, 2);
        assert_eq!((w.range(), w.has_next, w.has_prev), (4..5, false, true));
        assert_eq!(w.prev_offset, 2);
        // Exactly off the end and far past it: empty, no links.
        for offset in [5, usize::MAX] {
            let w = page_window(5, offset, 2);
            assert!(w.range().is_empty() && !w.has_next && !w.has_prev);
        }
        // A limit larger than the list: one page, nothing to navigate.
        let w = page_window(3, 0, usize::MAX);
        assert_eq!((w.range(), w.has_next, w.has_prev), (0..3, false, false));
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
    fn query_string_with_format_overrides_and_preserves_filters() {
        // The alternate (?f=json) link forces the format but keeps every
        // search/pagination param — even when the request itself was f=html.
        let sp = SearchQueryParams {
            q: Some("radar".to_string()),
            limit: Some("10".to_string()),
            f: Some("html".to_string()),
            ..Default::default()
        };
        assert_eq!(
            sp.query_string_with_format(10, 20, "json"),
            "?q=radar&limit=10&f=json&offset=20"
        );
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

    /// `sortby` (draft 25-046 §8, #1141): title compares case-insensitively,
    /// an empty title as the id, `-` descends, and ties fall back to the id
    /// so every page boundary is stable.
    #[test]
    fn sortby_orders_by_title_or_id_with_id_tie_break() {
        let item = |id, title| CollectionMatch {
            id,
            title,
            description: "",
            keywords: &[],
            themes: Vec::new(),
            bbox: None,
            time: None,
        };
        let items = [
            item("z-alerts", "Alerts"),
            item("a-radar", "radar composite"),
            item("m-blank", " "),
            item("b-radar", "Radar composite"),
            item("c-gfs", "GFS"),
        ];
        let order = |sortby: &str| {
            let params = SearchQueryParams {
                sortby: Some(sortby.into()),
                ..Default::default()
            }
            .parse()
            .unwrap();
            search(&items, &params)
                .page
                .iter()
                .map(|&i| items[i].id)
                .collect::<Vec<_>>()
        };
        // "Radar composite" and "radar composite" fold to one key; the
        // exact title then orders them (uppercase first), before the id.
        assert_eq!(
            order("title"),
            ["z-alerts", "c-gfs", "m-blank", "b-radar", "a-radar"]
        );
        assert_eq!(
            order(" title"),
            order("title"),
            "a decoded '+' is ascending"
        );
        assert_eq!(order("+title"), order("title"));
        assert_eq!(
            order("-title"),
            ["a-radar", "b-radar", "m-blank", "c-gfs", "z-alerts"]
        );
        assert_eq!(
            order("id"),
            ["a-radar", "b-radar", "c-gfs", "m-blank", "z-alerts"]
        );
        assert_eq!(
            order("-id"),
            ["z-alerts", "m-blank", "c-gfs", "b-radar", "a-radar"]
        );
        // Without sortby the input order stands.
        let unsorted = search(&items, &SearchQueryParams::default().parse().unwrap());
        assert_eq!(unsorted.page, [0, 1, 2, 3, 4]);
        // Sorting happens before paging.
        let page = SearchQueryParams {
            sortby: Some("id".into()),
            limit: Some("2".into()),
            offset: Some("2".into()),
            ..Default::default()
        }
        .parse()
        .unwrap();
        let ids: Vec<_> = search(&items, &page)
            .page
            .iter()
            .map(|&i| items[i].id)
            .collect();
        assert_eq!(ids, ["c-gfs", "m-blank"]);
    }

    #[test]
    fn sortby_rejects_unknown_repeated_and_empty_keys() {
        for (value, message) in [
            (
                "updated",
                "Unknown sortby property 'updated'; sortable: title, id",
            ),
            ("Title", "Unknown sortby property 'Title'"),
            (
                "title,",
                "sortby: each comma-separated key must name a property",
            ),
            ("-", "sortby: each comma-separated key must name a property"),
            (
                "title,-title",
                "sortby: property 'title' listed more than once",
            ),
            ("id, +id", "sortby: property 'id' listed more than once"),
        ] {
            let error = SearchQueryParams::from_pairs(vec![("sortby".into(), value.into())])
                .and_then(|query| query.parse())
                .unwrap_err();
            assert_eq!(error.parameter(), Some("sortby"), "{value}");
            assert!(error.to_string().starts_with(message), "{value}: {error}");
        }
        // An empty value is no sortby, like an empty q.
        let empty = SearchQueryParams::from_pairs(vec![("sortby".into(), String::new())])
            .unwrap()
            .parse()
            .unwrap();
        assert!(empty.sortby.is_empty());
        let keys = SearchQueryParams::from_pairs(vec![("sortby".into(), "-title,id".into())])
            .unwrap()
            .parse()
            .unwrap()
            .sortby;
        assert_eq!(sortby_value(&keys), "-title,id");
        // Space after the sign is ignored, as in Features `/items`.
        let spaced = SearchQueryParams::from_pairs(vec![("sortby".into(), "- title, id".into())])
            .unwrap()
            .parse()
            .unwrap()
            .sortby;
        assert_eq!(spaced, keys);
    }

    /// Links keep the request's sortby; an alternate link spells out the
    /// page size and order the response used (#1141).
    #[test]
    fn links_keep_sortby_and_explicit_page_fills_only_what_is_missing() {
        let sorted = SearchQueryParams {
            q: Some("radar".into()),
            sortby: Some("-title".into()),
            ..Default::default()
        };
        assert_eq!(
            sorted.query_string_with_format(24, 24, "html"),
            "?q=radar&f=html&offset=24&sortby=-title"
        );
        let title = [CollectionSortKey::ascending(CollectionSortProperty::Title)];
        let bare = SearchQueryParams::default();
        assert_eq!(
            bare.explicit_page(24, &title)
                .query_string_with_format(24, 0, "json"),
            "?limit=24&f=json&sortby=title"
        );
        // What the request gave stays as it gave it.
        let given = SearchQueryParams {
            limit: Some("5".into()),
            ..sorted
        };
        assert_eq!(
            given
                .explicit_page(5, &title)
                .query_string_with_format(5, 0, "json"),
            "?q=radar&limit=5&f=json&sortby=-title"
        );
    }
}
