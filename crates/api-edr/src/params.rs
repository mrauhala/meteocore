use chrono::{DateTime, Utc};
use ds_core::datetime::{parse_datetime_interval, parse_iso8601_duration};
use ds_core::edr_engine::TrajectoryShape;
use ds_core::error::DataServerError;
use ds_core::feature::DatetimeInterval;
use serde::Deserialize;

use crate::response::{COVERAGE_JSON_MEDIA_TYPE, LEGACY_COVERAGE_JSON_MEDIA_TYPE};

/// Most instants one `datetime` list may name. Each is its own engine
/// query (see [`DatetimeSelector::Instants`]), run one after another on one
/// EDR executor slot, and a query can be blocking remote I/O, so the count is
/// kept small (root CLAUDE.md Critical Rule 9); the merged response also
/// shares one value budget (`crate::datetime_list::MAX_LIST_VALUES`).
pub const MAX_DATETIME_INSTANTS: usize = 16;

/// A parsed EDR `datetime` value (`/req/core/datetime-response` D).
#[derive(Debug, Clone, PartialEq)]
pub enum DatetimeSelector {
    /// An instant `(t, t)` or an interval. An open end is the
    /// `parse_datetime_interval` sentinel (`MIN_UTC` / `MAX_UTC`).
    Window(DateTime<Utc>, DateTime<Utc>),
    /// A list `T1,T2,T3` of two or more distinct instants, ascending, or the
    /// instants a repeating interval `Rn/date-time/duration` expands to. Each
    /// is queried as the single instant `(t, t)`, so it is matched exactly
    /// as a request naming that instant alone would be.
    Instants(Vec<DateTime<Utc>>),
}

impl DatetimeSelector {
    /// The window the selection spans: what the settled-response
    /// `Cache-Control` policy is decided on.
    pub fn envelope(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        match self {
            Self::Window(start, end) => (*start, *end),
            // Non-empty by construction (`parse_datetime` builds it).
            Self::Instants(v) => (v[0], v[v.len() - 1]),
        }
    }

    /// The selection as the intervals a location must have an observation
    /// in (`EdrEngine::location_time_filter`, #932): the window, its open
    /// ends unbounded, or one instant interval per listed instant, so each
    /// is matched exactly.
    pub fn intervals(&self) -> Vec<DatetimeInterval> {
        let bound = |t: DateTime<Utc>| {
            (t != DateTime::<Utc>::MIN_UTC && t != DateTime::<Utc>::MAX_UTC).then_some(t)
        };
        match self {
            Self::Window(start, end) => vec![DatetimeInterval {
                start: bound(*start),
                end: bound(*end),
            }],
            Self::Instants(v) => v
                .iter()
                .map(|t| DatetimeInterval {
                    start: Some(*t),
                    end: Some(*t),
                })
                .collect(),
        }
    }
}

/// Parse the EDR `datetime` query parameter: an RFC 3339 instant, an
/// interval (`start/end`, `../end`, `start/..`), or one of EDR 1.2's two
/// multi-instant forms, at most [`MAX_DATETIME_INSTANTS`] instants each:
///
/// - `list of datetimes`: a comma-separated list of instants. A list element
///   must be an instant, not an interval. Repeated instants collapse; a list
///   that collapses to one instant is that instant.
/// - `repeating interval`: `Rn/date-time/duration`, the `n` instants
///   `start + i × duration` for `i` in `0..n`, `n` counting instants as
///   `z=Rn/min/step` counts levels. `R1` is the start alone.
///
/// An interval that ends before it starts is a 400, as in Features, Maps
/// and Tiles: it selects nothing, and the station engines' range lookups
/// panic on it.
pub fn parse_datetime(raw: Option<&str>) -> Result<Option<DatetimeSelector>, DataServerError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    // An RFC 3339 date-time starts with a digit, so a leading `R` is
    // unambiguous.
    if let Some(rest) = raw.strip_prefix(['R', 'r']) {
        return parse_datetime_repeating(raw, rest).map(Some);
    }
    if !raw.contains(',') {
        let (start, end) = parse_datetime_interval(raw)?;
        if start > end {
            return Err(DataServerError::InvalidDatetime(format!(
                "'{raw}' ends before it starts"
            )));
        }
        return Ok(Some(DatetimeSelector::Window(start, end)));
    }
    let elements: Vec<&str> = raw.split(',').map(str::trim).collect();
    if elements.len() > MAX_DATETIME_INSTANTS {
        return Err(DataServerError::InvalidDatetime(format!(
            "a datetime list names {} instants; the maximum is {MAX_DATETIME_INSTANTS}",
            elements.len()
        )));
    }
    let mut instants = elements
        .into_iter()
        .map(|element| {
            if element.is_empty() {
                return Err(DataServerError::InvalidDatetime(
                    "a datetime list has an empty element — check for a stray comma".into(),
                ));
            }
            if element.contains('/') {
                return Err(DataServerError::InvalidDatetime(format!(
                    "'{element}': a datetime list holds instants only, not intervals"
                )));
            }
            let (instant, _) = parse_datetime_interval(element)?;
            Ok(instant)
        })
        .collect::<Result<Vec<_>, _>>()?;
    instants.sort_unstable();
    instants.dedup();
    Ok(Some(match instants[..] {
        [only] => DatetimeSelector::Window(only, only),
        _ => DatetimeSelector::Instants(instants),
    }))
}

/// Expand the EDR 1.2 repeating interval `Rn/date-time/duration`
/// (`/req/core/datetime-response` D) after its `R`; `raw` is the whole
/// value, for messages.
///
/// `n` counts instants, not repetitions after the first: `R4/T/PT6H` is `T`,
/// `T+6h`, `T+12h`, `T+18h`. The grammar ("R[number of repetitions]") has no
/// example, so this follows the standard's one query-parameter example of
/// the form, `z=R20/100/50` = "20 height levels" (mirrored by [`parse_z`]);
/// the 1.2 OpenAPI temporal extent example `R12/…09:00Z/PT1H`,
/// `R4/…21:00Z/PT3H`, `R4/…09:00Z/PT6H`, whose runs abut without overlap
/// only when `n` counts instants; and ISO 8601 parsers such as aniso8601
/// (`R3/1981-04-05/P1D` is three dates). The informative collection-response
/// annex reads it the other way (`R4/100/5` as `[100, …, 120]`, five values).
///
/// `n` is 1…[`MAX_DATETIME_INSTANTS`]; `R0`, a missing (unbounded) `n` and
/// ISO's `R-1` are 400s, as no request can expand an unbounded series. The
/// duration is [`parse_iso8601_duration`]'s: positive, in weeks or days,
/// hours, minutes and whole seconds. Calendar years and months are a 400,
/// since a month's length depends on the instant it is added to.
fn parse_datetime_repeating(raw: &str, rest: &str) -> Result<DatetimeSelector, DataServerError> {
    let invalid = |detail: String| DataServerError::InvalidDatetime(format!("'{raw}': {detail}"));
    if raw.contains(',') {
        return Err(invalid(
            "a repeating interval cannot be part of a datetime list".into(),
        ));
    }
    let parts: Vec<&str> = rest.split('/').map(str::trim).collect();
    let [count, start, duration] = parts[..] else {
        return Err(invalid(
            "a repeating interval must be `Rn/date-time/duration`, e.g. \
             R4/2026-10-01T00:00:00Z/PT6H"
                .into(),
        ));
    };
    if count.is_empty() || count == "-1" {
        return Err(invalid(format!(
            "an unbounded repeating interval is not supported; give the number of \
             instants, at most {MAX_DATETIME_INSTANTS}"
        )));
    }
    if !count.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(format!(
            "repeating interval count 'R{count}' must be a positive whole number of instants"
        )));
    }
    // All digits: a parse failure can only be an overflow, past any cap.
    let n = count.parse::<usize>().unwrap_or(usize::MAX);
    if n == 0 {
        return Err(invalid(
            "a repeating interval names at least one instant; R0 names none".into(),
        ));
    }
    if n > MAX_DATETIME_INSTANTS {
        return Err(invalid(format!(
            "a repeating interval names {count} instants; the maximum is {MAX_DATETIME_INSTANTS}"
        )));
    }
    let start = start.parse::<DateTime<Utc>>().map_err(|e| {
        invalid(format!(
            "repeating interval start '{start}' is not an RFC 3339 date-time: {e}"
        ))
    })?;
    let step = parse_iso8601_duration(duration).map_err(|e| match e {
        DataServerError::Config(msg) => invalid(msg),
        other => other,
    })?;
    let mut instants = Vec::with_capacity(n);
    let mut t = start;
    instants.push(t);
    for _ in 1..n {
        t = t.checked_add_signed(step).ok_or_else(|| {
            invalid("the repeating interval runs past the supported date range".into())
        })?;
        instants.push(t);
    }
    Ok(match instants[..] {
        [only] => DatetimeSelector::Window(only, only),
        _ => DatetimeSelector::Instants(instants),
    })
}

/// The one CRS data queries accept: `coords` are read, and results written,
/// in OGC:CRS84, WGS 84 longitude/latitude. Every data query advertises it as
/// the `crs` of its `link.variables.crs_details` (EDR 1.2 `/req/edr/rc-crs`).
/// The `crs` query parameter that would select another one is #84.
pub const DATA_QUERY_CRS: &str = "CRS84";

/// WKT of [`DATA_QUERY_CRS`], advertised in every `crs_details`. Its home is
/// `ds_core::geo`, next to the other WGS 84 constants.
pub use ds_core::geo::CRS84_WKT;

#[derive(Debug, Deserialize)]
pub struct LocationQueryParams {
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    pub z: Option<String>,
    /// Output format: `CoverageJSON` (default), `PNG` (plot) or, on a
    /// station collection, `GeoJSON` ([`query_formats`]).
    pub f: Option<String>,
    /// PNG plot dimensions (ignored for CoverageJSON).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// EDR 1.2 `limit` on top-level coverages; see [`parse_limit`].
    pub limit: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PositionQueryParams {
    pub coords: String,
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    pub z: Option<String>,
    /// Output format: `CoverageJSON` (default), `PNG` (plot) or, on a
    /// station collection, `GeoJSON` ([`query_formats`]).
    pub f: Option<String>,
    /// PNG plot dimensions (ignored for CoverageJSON).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// EDR 1.2 `limit` on top-level coverages; see [`parse_limit`].
    pub limit: Option<String>,
}

/// EDR response output format selected by the `f` query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdrFormat {
    /// OGC CoverageJSON (the default), served as
    /// [`COVERAGE_JSON_MEDIA_TYPE`].
    CoverageJson,
    /// EDR GeoJSON: one feature per location, for station series (#929).
    GeoJson,
    /// A rendered PNG plot (vertical profile or time series).
    Png,
    /// An HTML page of the response (EDR `/req/html/definition`, #971):
    /// offered by every data query, after its data formats.
    Html,
}

impl EdrFormat {
    /// The `f` token, as advertised in `output_formats`.
    pub fn name(self) -> &'static str {
        match self {
            EdrFormat::CoverageJson => "CoverageJSON",
            EdrFormat::GeoJson => "GeoJSON",
            EdrFormat::Png => "PNG",
            EdrFormat::Html => "HTML",
        }
    }

    /// The media type a response in this format carries.
    pub fn media_type(self) -> &'static str {
        match self {
            EdrFormat::CoverageJson => COVERAGE_JSON_MEDIA_TYPE,
            EdrFormat::GeoJson => "application/geo+json",
            EdrFormat::Png => "image/png",
            EdrFormat::Html => "text/html",
        }
    }

    /// The format a media type names (lowercase), if any. `application/json`
    /// names none: both JSON formats are JSON, so it leaves the default.
    fn from_media_type(media: &str) -> Option<EdrFormat> {
        match media {
            COVERAGE_JSON_MEDIA_TYPE | LEGACY_COVERAGE_JSON_MEDIA_TYPE => {
                Some(EdrFormat::CoverageJson)
            }
            "application/geo+json" => Some(EdrFormat::GeoJson),
            "image/png" => Some(EdrFormat::Png),
            "text/html" => Some(EdrFormat::Html),
            _ => None,
        }
    }
}

/// Parse the `f` query parameter. Absent/blank → CoverageJSON.
/// `coveragejson`, `geojson`, `png` and `html` are accepted case-insensitively, and
/// so are their media types (#510): `application/vnd.cov+json` (what the
/// responses carry, EDR 1.2, #920), `application/prs.coverage+json` (EDR
/// 1.1's type, still accepted; the response is the same CoverageJSON under
/// the 1.2 type), `application/geo+json`, `image/png` and `text/html`. A `+` sent
/// unencoded arrives as a space, so a space inside a media type reads as
/// `+`. Anything else is a 400. Whether the query offers the format is the
/// caller's check ([`query_formats`]).
pub fn parse_edr_format(f: Option<&str>) -> Result<EdrFormat, DataServerError> {
    let f = f
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase().replace(' ', "+"));
    match f.as_deref() {
        None => Ok(EdrFormat::CoverageJson),
        Some("coveragejson") => Ok(EdrFormat::CoverageJson),
        Some("geojson") => Ok(EdrFormat::GeoJson),
        Some("png") => Ok(EdrFormat::Png),
        Some("html") => Ok(EdrFormat::Html),
        Some(media) => EdrFormat::from_media_type(media).ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Unsupported output format '{media}' — expected 'CoverageJSON', 'GeoJSON', 'PNG' or 'HTML'"
            ))
        }),
    }
}

/// The output formats a data query offers, default first, in the order they
/// are advertised in `data_queries.*.link.variables.output_formats`: the
/// one list the handlers negotiate over and the metadata advertises. GeoJSON
/// is offered for the point-shaped queries (`locations`, `position`,
/// `radius`) of an engine that serves station series
/// (`EdrEngine::serves_station_series`); gridded results, `area` and `cube`
/// keep CoverageJSON only (#929). PNG plots a single series or profile, so area and
/// radius results are not offered as PNG. A trajectory is never GeoJSON; it
/// is a PNG heatmap only as a radar cross-section
/// (`EdrEngine::trajectory_shape`), an along-path trajectory (#926) being
/// CoverageJSON otherwise. Every query also offers HTML (#971), last, so an
/// `Accept` naming it beside a data format at the same q keeps the data.
pub fn query_formats(
    query_type: &str,
    station_series: bool,
    trajectory: TrajectoryShape,
) -> &'static [EdrFormat] {
    use EdrFormat::{CoverageJson, GeoJson, Html, Png};
    match (query_type, station_series) {
        ("locations" | "position", true) => &[CoverageJson, GeoJson, Png, Html],
        ("locations" | "position", false) => &[CoverageJson, Png, Html],
        ("trajectory", _) => match trajectory {
            TrajectoryShape::CrossSection => &[CoverageJson, Png, Html],
            TrajectoryShape::AlongPath => &[CoverageJson, Html],
        },
        ("radius", true) => &[CoverageJson, GeoJson, Html],
        _ => &[CoverageJson, Html],
    }
}

/// The formats of the `/locations` list (#971): GeoJSON, the default, and
/// HTML. `json` and `application/json` in `f` name GeoJSON, as on `items`.
pub const LOCATIONS_LIST_FORMATS: [EdrFormat; 2] = [EdrFormat::GeoJson, EdrFormat::Html];

/// Negotiate the `/locations` list's or `items`' format over
/// [`LOCATIONS_LIST_FORMATS`], like [`negotiate_edr_format`]: an `f` naming
/// neither is a 400 naming both, not the GeoJSON the list once answered for
/// any `f` (#605).
pub fn negotiate_list_format(
    f: Option<&str>,
    accept: Option<&str>,
    what: &str,
) -> Result<NegotiatedFormat, DataServerError> {
    let Some(f) = f.map(str::trim).filter(|f| !f.is_empty()) else {
        return negotiate_edr_format(None, accept, &LOCATIONS_LIST_FORMATS, what);
    };
    let format = match f.to_ascii_lowercase().as_str() {
        "json" | "application/json" => Ok(EdrFormat::GeoJson),
        _ => parse_edr_format(Some(f)),
    };
    match format {
        Ok(format) if LOCATIONS_LIST_FORMATS.contains(&format) => Ok(NegotiatedFormat {
            format,
            vary_accept: false,
        }),
        _ => Err(DataServerError::InvalidParameter(format!(
            "Unsupported output format '{f}' for {what}; available: GeoJSON, HTML"
        ))),
    }
}

/// A data query's chosen representation. `vary_accept`: the `Accept`
/// header chose it among several offered formats (no `f`), so the response
/// must carry `Vary: Accept`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedFormat {
    pub format: EdrFormat,
    pub vary_accept: bool,
}

/// Choose a data query's output format among the `offered` ones (default
/// first). An explicit `f` wins; a format the query does not offer is a 400
/// naming the ones it does. Without `f`, the `Accept` header picks the
/// offered format with the highest q-value among the media types it names
/// explicitly (ties go to the earlier offered format; `q=0` excludes).
/// Wildcards and `application/json` name no format, and nothing acceptable
/// falls back to the default rather than 406, like the metadata resources'
/// negotiation.
pub fn negotiate_edr_format(
    f: Option<&str>,
    accept: Option<&str>,
    offered: &[EdrFormat],
    what: &str,
) -> Result<NegotiatedFormat, DataServerError> {
    let default = offered.first().copied().unwrap_or(EdrFormat::CoverageJson);
    if f.is_some_and(|f| !f.trim().is_empty()) {
        let format = parse_edr_format(f)?;
        if !offered.contains(&format) {
            let names: Vec<&str> = offered.iter().map(|f| f.name()).collect();
            return Err(DataServerError::InvalidParameter(format!(
                "{} output is not available for {what}; available: {}",
                format.name(),
                names.join(", ")
            )));
        }
        return Ok(NegotiatedFormat {
            format,
            vary_accept: false,
        });
    }
    let format = accept
        .and_then(|accept| accept_preference(accept, offered))
        .unwrap_or(default);
    Ok(NegotiatedFormat {
        format,
        vary_accept: offered.len() > 1,
    })
}

/// The offered format an `Accept` header prefers, if it names one.
fn accept_preference(accept: &str, offered: &[EdrFormat]) -> Option<EdrFormat> {
    let mut best: Option<(f32, usize)> = None;
    for entry in accept.split(',') {
        let mut parts = entry.split(';').map(str::trim);
        let media = parts.next().unwrap_or("").to_ascii_lowercase();
        let Some(rank) = EdrFormat::from_media_type(&media)
            .and_then(|format| offered.iter().position(|o| *o == format))
        else {
            continue;
        };
        // Absent q is 1; a malformed one is 0 (not acceptable), as in
        // `ds_core::html::negotiate`.
        let q = parts
            .filter_map(|p| p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")))
            .map(|v| v.trim().parse::<f32>().unwrap_or(0.0))
            .next_back()
            .unwrap_or(1.0);
        if q > 0.0 && best.is_none_or(|(bq, br)| q > bq || (q == bq && rank < br)) {
            best = Some((q, rank));
        }
    }
    best.map(|(_, rank)| offered[rank])
}

/// Default plot dimensions when `width`/`height` aren't supplied. The actual
/// safe range is enforced inside `ds_render::render_chart`, so user input
/// passes through unclamped here — one source of truth for the bounds.
pub fn plot_dimensions(width: Option<u32>, height: Option<u32>) -> (u32, u32) {
    (width.unwrap_or(800), height.unwrap_or(600))
}

/// Largest `limit` honoured, the maximum of EDR 1.2
/// `/req/edr/rc-limit-definition`. A larger value is clamped to it, not an
/// error (`/req/edr/REQ_rc-limit-response` C).
pub const MAX_LIMIT: usize = 10_000;

/// Parse the EDR 1.2 `limit` parameter. Absent or blank → `None`: no limit,
/// which keeps today's responses (the complete `/locations` inventory, every
/// coverage of a data query) rather than the spec's suggested default of 10.
/// An integer ≥ 1 is honoured up to [`MAX_LIMIT`]; a larger one, even one too
/// long for any integer type, is clamped to it. Anything else — zero, a
/// sign, a fraction, an exponent, a non-number — is a 400 naming the range.
pub fn parse_limit(raw: Option<&str>) -> Result<Option<usize>, DataServerError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let invalid = || {
        DataServerError::InvalidParameter(format!(
            "Invalid limit '{raw}': expected an integer from 1 to {MAX_LIMIT} \
             (larger values are clamped to {MAX_LIMIT})"
        ))
    };
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    // Digits only, so the one parse failure left is overflow: clamp it.
    let n = raw.parse::<usize>().unwrap_or(usize::MAX);
    if n == 0 {
        return Err(invalid());
    }
    Ok(Some(n.min(MAX_LIMIT)))
}

/// A `/locations` request made with `limit`: one page of the list (EDR 1.2
/// locations paging, #922). Without `limit` there is no paging and the whole
/// list is served as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocationsPaging {
    /// Resolved page size (clamped to [`MAX_LIMIT`]); links carry this value.
    pub limit: usize,
    pub offset: usize,
}

/// A parsed `/locations` request: its filters and its page (#922, #932).
#[derive(Debug, Clone)]
pub struct LocationsQuery {
    /// `bbox`: only the locations whose point lies inside it, edges
    /// included, are listed, before paging.
    pub bbox: Option<ds_core::feature::Bbox>,
    /// `datetime`, in the data queries' grammar ([`parse_datetime`]): only
    /// the locations with an observation in it are listed, before paging,
    /// as the engine's `EdrEngine::location_time_filter` decides.
    pub datetime: Option<DatetimeSelector>,
    /// `None` without `limit`: the whole list.
    pub paging: Option<LocationsPaging>,
    /// Every pair but `limit` and `offset`, in request order, repeated
    /// verbatim in the links (so `bbox`, `datetime` and `f` survive paging).
    pub preserved: Vec<(String, String)>,
}

impl LocationsQuery {
    /// Whether the request names a filter. Only then does an unpaged list's
    /// `self` link carry the query; without one the body stays the complete
    /// inventory's, byte for byte.
    pub fn is_filtered(&self) -> bool {
        self.bbox.is_some() || self.datetime.is_some()
    }

    /// `base` plus this request's query: the preserved pairs, then, when
    /// paged, the resolved `limit` and `offset` (omitted when 0), as on
    /// `/collections`.
    pub fn href(&self, base: &str, offset: usize) -> String {
        use ds_core::collection_search::encode_query_value as enc;
        let mut query: Vec<String> = self
            .preserved
            .iter()
            .map(|(name, value)| format!("{}={}", enc(name), enc(value)))
            .collect();
        if let Some(paging) = &self.paging {
            query.push(format!("limit={}", paging.limit));
            if offset > 0 {
                query.push(format!("offset={offset}"));
            }
        }
        if query.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", query.join("&"))
        }
    }
}

/// Query parameters `/locations` accepts: the EDR filters `bbox` and
/// `datetime`, the paging pair `limit`/`offset`, and `f`.
pub const LOCATIONS_PARAMETERS: [&str; 5] = ["limit", "offset", "bbox", "datetime", "f"];

/// Parse the `/locations` query. A parameter outside [`LOCATIONS_PARAMETERS`],
/// a repeated `limit`, `offset`, `bbox` or `datetime`, an invalid value, or an
/// `offset` without a `limit` (there is no page to offset into) is a 400, so
/// a typo such as `limti` cannot return the unpaged list as if it worked
/// (#605).
///
/// `bbox` is the EDR 1.2 parameter: CRS84, four numbers, `west > east`
/// crossing the antimeridian, or six whose vertical pair must be numbers and
/// is otherwise ignored. A location is a 2-D point with no height to test, as
/// `items` ignores the heights and a collection without a vertical extent
/// ignores `z` (`/req/edr/z-response` A). `datetime` takes [`parse_datetime`]'s
/// grammar; whether the collection can filter by it is the engine's answer,
/// known only once the query runs.
pub fn parse_locations_query(
    pairs: Vec<(String, String)>,
) -> Result<LocationsQuery, DataServerError> {
    let (mut limit, mut offset, mut bbox, mut datetime) = (None, None, None, None);
    let mut preserved = Vec::new();
    for (name, value) in pairs {
        let slot = match name.as_str() {
            "limit" => &mut limit,
            "offset" => &mut offset,
            "bbox" => &mut bbox,
            "datetime" => &mut datetime,
            "f" => {
                preserved.push((name, value));
                continue;
            }
            _ => {
                return Err(DataServerError::InvalidParameter(format!(
                    "Unknown query parameter '{name}' for /locations; valid parameters: {}",
                    LOCATIONS_PARAMETERS.join(", ")
                )));
            }
        };
        if slot.is_some() {
            return Err(DataServerError::InvalidParameter(format!(
                "Duplicate query parameter '{name}'"
            )));
        }
        if matches!(name.as_str(), "bbox" | "datetime") {
            preserved.push((name, value.clone()));
        }
        *slot = Some(value);
    }
    let bbox = bbox
        .as_deref()
        .map(|raw| parse_cube_bbox(raw).map(|(bbox, _heights)| bbox))
        .transpose()?;
    let datetime = parse_datetime(datetime.as_deref())?;
    let resolved_offset = parse_offset(offset.as_deref())?;
    let paging = match parse_limit(limit.as_deref())? {
        Some(limit) => Some(LocationsPaging {
            limit,
            offset: resolved_offset,
        }),
        None if offset.is_some_and(|o| !o.trim().is_empty()) => {
            return Err(DataServerError::InvalidParameter(
                "offset pages the location list and requires limit".into(),
            ))
        }
        None => None,
    };
    Ok(LocationsQuery {
        bbox,
        datetime,
        paging,
        preserved,
    })
}

/// Parse the `/locations` `offset` (the offset pagination extension
/// `/collections` also uses). Absent or blank → 0; otherwise a non-negative
/// integer, else a 400.
pub fn parse_offset(raw: Option<&str>) -> Result<usize, DataServerError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    raw.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| raw.parse::<usize>().ok())
        .flatten()
        .ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Invalid offset '{raw}': expected a non-negative integer"
            ))
        })
}

#[derive(Debug, Deserialize)]
pub struct AreaQueryParams {
    pub coords: String,
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    pub z: Option<String>,
    /// Output format. Area queries only support `CoverageJSON`; `PNG` is
    /// rejected (an area result is gridded / multi-coverage, not a single
    /// plot), and so is `GeoJSON` (not a point query, #929).
    pub f: Option<String>,
    /// EDR 1.2 `limit` on top-level coverages; see [`parse_limit`].
    pub limit: Option<String>,
}

/// Radius query parameters (OGC API - EDR 1.1 `radius`): everything
/// within `within` `within-units` of a WKT `POINT`. Both distance
/// parameters are required by the spec's OpenAPI; the accepted units are
/// [`WITHIN_UNITS`]. Like area, the result is multi-coverage / gridded, so
/// `PNG` is rejected; a station collection also offers `GeoJSON`.
#[derive(Debug, Deserialize)]
pub struct RadiusQueryParams {
    pub coords: String,
    pub within: String,
    #[serde(rename = "within-units")]
    pub within_units: String,
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    pub z: Option<String>,
    pub f: Option<String>,
    /// EDR 1.2 `limit` on top-level coverages; see [`parse_limit`].
    pub limit: Option<String>,
}

/// Cube query parameters (OGC API - EDR 1.2 `cube`, #925). Built from the
/// raw query pairs by [`CubeQueryParams::from_pairs`], so an unknown or
/// repeated parameter is a 400 naming the accepted ones rather than being
/// dropped by serde.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CubeQueryParams {
    pub bbox: Option<String>,
    pub z: Option<String>,
    pub datetime: Option<String>,
    pub parameter_name: Option<String>,
    pub resolution_x: Option<String>,
    pub resolution_y: Option<String>,
    pub resolution_z: Option<String>,
    pub crs: Option<String>,
    pub f: Option<String>,
}

/// The query parameters a cube request accepts, in the order the 1.2
/// OpenAPI lists them.
pub const CUBE_PARAMETERS: [&str; 9] = [
    "bbox",
    "z",
    "datetime",
    "parameter-name",
    "resolution-x",
    "resolution-y",
    "resolution-z",
    "crs",
    "f",
];

impl CubeQueryParams {
    /// Collect the cube parameters from the query pairs, rejecting a name
    /// outside [`CUBE_PARAMETERS`] and a parameter given twice.
    pub fn from_pairs(pairs: Vec<(String, String)>) -> Result<Self, DataServerError> {
        let mut params = Self::default();
        for (name, value) in pairs {
            let slot = match name.as_str() {
                "bbox" => &mut params.bbox,
                "z" => &mut params.z,
                "datetime" => &mut params.datetime,
                "parameter-name" => &mut params.parameter_name,
                "resolution-x" => &mut params.resolution_x,
                "resolution-y" => &mut params.resolution_y,
                "resolution-z" => &mut params.resolution_z,
                "crs" => &mut params.crs,
                "f" => &mut params.f,
                other => {
                    return Err(DataServerError::InvalidParameter(format!(
                        "Unknown cube query parameter '{other}'; accepted: {}",
                        CUBE_PARAMETERS.join(", ")
                    )))
                }
            };
            if slot.replace(value).is_some() {
                return Err(DataServerError::InvalidParameter(format!(
                    "Cube query parameter '{name}' is given more than once"
                )));
            }
        }
        Ok(params)
    }
}

/// The cube `bbox`: `minx,miny,maxx,maxy`, or six numbers
/// `minx,miny,minz,maxx,maxy,maxz` whose vertical pair becomes a `z`
/// interval (overridden by an explicit `z`, EDR `/req/edr/rc-cube` C).
/// CRS84 only; `minx > maxx` crosses the antimeridian. `/locations` parses
/// its `bbox` here too and drops the vertical pair.
pub fn parse_cube_bbox(
    raw: &str,
) -> Result<(ds_core::feature::Bbox, Option<ZSelector>), DataServerError> {
    let values: Vec<f64> = raw
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| {
                    DataServerError::InvalidBbox(format!(
                        "bbox value '{}' is not a finite number",
                        part.trim()
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let (horizontal, vertical) = match *values.as_slice() {
        [w, s, e, n] => ([w, s, e, n], None),
        [w, s, lo, e, n, hi] => (
            [w, s, e, n],
            Some(ZSelector::Interval {
                min: lo.min(hi),
                max: lo.max(hi),
            }),
        ),
        _ => {
            return Err(DataServerError::InvalidBbox(format!(
                "bbox must be 4 numbers (minx,miny,maxx,maxy) or 6 \
                 (minx,miny,minz,maxx,maxy,maxz), got {}",
                values.len()
            )))
        }
    };
    let [w, s, e, n] = horizontal;
    let bbox = ds_core::feature::Bbox::new(w, s, e, n).map_err(DataServerError::InvalidBbox)?;
    Ok((bbox, vertical))
}

/// Largest accepted `resolution-x`/`-y`/`-z`: no larger count can pass the
/// shared response budget ([`ds_core::feature::MAX_AREA_VALUES`]).
pub const MAX_RESOLUTION: usize = ds_core::feature::MAX_AREA_VALUES;

/// Parse a `resolution-x`/`-y`/`-z` value (`name` is the parameter): a
/// whole number of positions along the axis from 0 to [`MAX_RESOLUTION`].
/// `0` asks for the native resolution, the same as leaving it out, so both
/// are `None`. Anything else is a 400 stating the valid range (EDR
/// `/req/edr/resolution-x-response` D).
pub fn parse_resolution(name: &str, raw: Option<&str>) -> Result<Option<usize>, DataServerError> {
    let Some(raw) = raw.map(str::trim) else {
        return Ok(None);
    };
    match raw.parse::<usize>() {
        Ok(0) => Ok(None),
        Ok(n) if n <= MAX_RESOLUTION => Ok(Some(n)),
        _ => Err(DataServerError::InvalidParameter(format!(
            "{name} must be a whole number from 0 (native resolution) to {MAX_RESOLUTION}, \
             got '{raw}'"
        ))),
    }
}

/// The one CRS the data queries serve, as the collection metadata lists it.
pub const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";

/// Accept a data-query `crs` naming CRS84 (its OGC URI over http or https,
/// `CRS84` or `OGC:CRS84`, bracketed or not, case-insensitively); anything
/// else is a 400 naming the supported CRS.
pub fn check_crs(crs: Option<&str>) -> Result<(), DataServerError> {
    let Some(crs) = crs.map(str::trim) else {
        return Ok(());
    };
    let lower = crs.to_ascii_lowercase();
    let name = lower.trim_start_matches('[').trim_end_matches(']');
    let accepted = [
        "http://www.opengis.net/def/crs/ogc/1.3/crs84",
        "https://www.opengis.net/def/crs/ogc/1.3/crs84",
        "crs84",
        "ogc:crs84",
    ];
    if accepted.contains(&name) {
        return Ok(());
    }
    Err(DataServerError::InvalidParameter(format!(
        "crs '{crs}' is not supported; data queries are served in {CRS84} only"
    )))
}

/// `within-units` values the radius query accepts, in the order they are
/// advertised in `data_queries.radius.link.variables.within_units`.
pub const WITHIN_UNITS: [&str; 3] = ["km", "m", "mi"];

/// Largest accepted radius. A radius query is a "near this point" question;
/// anything bigger is a continental area query in disguise — and for the
/// engines that sample the circle's *bounding box*, a 5000 km circle at
/// 45° would have spanned ~173° of longitude. The engines' own
/// `QueryTooLarge` budgets still apply below it.
pub const MAX_WITHIN_M: f64 = 1_000_000.0;

/// Parse `within` + `within-units` into metres. Rejects a non-finite or
/// non-positive distance, an unknown unit (case-insensitive), and a radius
/// over [`MAX_WITHIN_M`].
pub fn parse_within_metres(within: &str, units: &str) -> Result<f64, DataServerError> {
    let value: f64 = within.trim().parse().map_err(|_| {
        DataServerError::InvalidParameter(format!("within must be a number, got '{within}'"))
    })?;
    if !value.is_finite() || value <= 0.0 {
        return Err(DataServerError::InvalidParameter(
            "within must be a finite, positive distance".into(),
        ));
    }
    let per_unit = match units.trim().to_ascii_lowercase().as_str() {
        "km" => 1_000.0,
        "m" => 1.0,
        "mi" => 1_609.344,
        other => {
            return Err(DataServerError::InvalidParameter(format!(
                "within-units '{other}' is not supported; use one of {}",
                WITHIN_UNITS.join(", ")
            )))
        }
    };
    let metres = value * per_unit;
    if metres > MAX_WITHIN_M {
        return Err(DataServerError::InvalidParameter(format!(
            "within exceeds the maximum radius of {} km",
            MAX_WITHIN_M / 1_000.0
        )));
    }
    Ok(metres)
}

/// Trajectory query parameters. `coords` is a WKT `LINESTRING`; a
/// gridded (along-path) collection also takes `LINESTRING Z`, `M` and `ZM`,
/// where Z is each vertex's level and M its time in Unix epoch seconds.
/// `z` selects levels from the collection's advertised vertical extent (a
/// list or a `min/max` interval): the levels a 2-D or M path is sampled on,
/// or, for a radar cross-section, the *elevation angles* bounding which
/// sweeps build it — whose own axis is derived height. The corridor query
/// (`corridor-width` / `corridor-height`) is a separate follow-up.
#[derive(Debug, Deserialize)]
pub struct TrajectoryQueryParams {
    pub coords: String,
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    pub z: Option<String>,
    /// Output format: `CoverageJSON` (default) or, for a radar
    /// cross-section, `PNG` — a colour-mapped heatmap (distance × height).
    pub f: Option<String>,
    /// PNG image dimensions (ignored for CoverageJSON).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Not supported on trajectory, which EDR 1.2 gives no `limit`: read
    /// only so a request carrying it is a 400, not a silently unlimited 200.
    pub limit: Option<String>,
}

/// A parsed EDR `z` selector: either an explicit list of levels or an
/// interval. The interval is resolved against the collection's advertised
/// vertical levels at the handler boundary (see [`resolve_z_levels`]) so
/// engines keep their `Option<&[f64]>` contract.
#[derive(Debug, Clone, PartialEq)]
pub enum ZSelector {
    /// Discrete levels: `z=0.5`, `z=850,700,500`, or the levels a
    /// recurring interval `z=Rn/min/step` expands to.
    Levels(Vec<f64>),
    /// An interval: closed `z=min/max`, or open `z=../max` / `z=min/..`
    /// (EDR 1.2 `/req/edr/z-response`). An open end is `-∞` / `+∞`, so it
    /// reaches the extreme advertised level.
    Interval { min: f64, max: f64 },
}

/// Most levels a recurring `z=Rn/min/step` may expand to. The count is
/// the number of levels, as in the standard's example: `z=R20/100/50` is
/// "20 levels at 50 unit intervals starting at level 100".
pub const MAX_Z_RECURRENCES: u32 = 1000;

/// An interval bound as it is written in a request: `..` when open.
fn fmt_z_bound(v: f64) -> String {
    if v.is_finite() {
        v.to_string()
    } else {
        "..".to_string()
    }
}

/// Parse one finite `f64` from a `z` token, rejecting `inf`/`nan` (a
/// non-finite level would poison `quantize_z` cache keys and `nearest_sweep`
/// comparisons downstream).
fn parse_z_value(part: &str) -> Result<f64, DataServerError> {
    part.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Invalid `z` value '{}' — expected a finite number",
                part.trim()
            ))
        })
}

/// Parse the EDR `z` query parameter (EDR 1.2 `/req/edr/z-response`):
///
/// - a level or a comma-separated list: `z=0.5`, `z=850,700,500`;
/// - a closed interval `z=850/500` (order-independent);
/// - an open interval `z=../850` or `z=500/..`, reaching the lowest or
///   highest advertised level;
/// - a recurring interval `z=Rn/min/step`: `n` levels from `min`, `step`
///   apart (`step` may be negative, never zero), at most
///   [`MAX_Z_RECURRENCES`]. It becomes a list, snapped like one.
///
/// An absent or blank value yields `None` (the whole vertical extent / a
/// profile).
pub fn parse_z(z: Option<&str>) -> Result<Option<ZSelector>, DataServerError> {
    let Some(raw) = z.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    if let Some(rest) = raw.strip_prefix(['R', 'r']) {
        return parse_z_recurring(rest).map(Some);
    }

    // Interval form — exactly one slash; either end may be `..` (open).
    if raw.contains('/') {
        let parts: Vec<&str> = raw.split('/').map(str::trim).collect();
        let [a, b] = parts[..] else {
            return Err(DataServerError::InvalidParameter(
                "`z` interval must be `min/max`, `../max` or `min/..` (one slash), or a \
                 recurring `Rn/min/step`"
                    .into(),
            ));
        };
        let (min, max) = match (a, b) {
            ("..", "..") => {
                return Err(DataServerError::InvalidParameter(
                    "`z` interval `../..` has no bound; omit `z` for every level".into(),
                ))
            }
            ("..", b) => (f64::NEG_INFINITY, parse_z_value(b)?),
            (a, "..") => (parse_z_value(a)?, f64::INFINITY),
            (a, b) => {
                let (a, b) = (parse_z_value(a)?, parse_z_value(b)?);
                if a <= b {
                    (a, b)
                } else {
                    (b, a)
                }
            }
        };
        return Ok(Some(ZSelector::Interval { min, max }));
    }

    let levels: Vec<f64> = raw
        .split(',')
        .map(|part| {
            if part.trim().is_empty() {
                return Err(DataServerError::InvalidParameter(
                    "`z` has an empty element — check for a stray comma".into(),
                ));
            }
            parse_z_value(part)
        })
        .collect::<Result<_, _>>()?;
    Ok((!levels.is_empty()).then_some(ZSelector::Levels(levels)))
}

/// Expand the part of a recurring `z=Rn/min/step` after the `R`.
fn parse_z_recurring(rest: &str) -> Result<ZSelector, DataServerError> {
    let parts: Vec<&str> = rest.split('/').map(str::trim).collect();
    let [count, min, step] = parts[..] else {
        return Err(DataServerError::InvalidParameter(
            "`z` recurring interval must be `Rn/min/step`, e.g. R20/100/50".into(),
        ));
    };
    let n: u32 = count.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
        DataServerError::InvalidParameter(format!(
            "`z` recurring interval count 'R{count}' must be a positive whole number of levels"
        ))
    })?;
    if n > MAX_Z_RECURRENCES {
        return Err(DataServerError::InvalidParameter(format!(
            "`z` recurring interval R{n} exceeds the maximum of {MAX_Z_RECURRENCES} levels"
        )));
    }
    let min = parse_z_value(min)?;
    let step = parse_z_value(step)?;
    if step == 0.0 {
        return Err(DataServerError::InvalidParameter(
            "`z` recurring interval step must be non-zero".into(),
        ));
    }
    let levels: Vec<f64> = (0..n).map(|i| min + step * f64::from(i)).collect();
    if levels.iter().any(|v| !v.is_finite()) {
        return Err(DataServerError::InvalidParameter(
            "`z` recurring interval runs past the finite number range".into(),
        ));
    }
    Ok(ZSelector::Levels(levels))
}

/// Resolve a [`ZSelector`] into the concrete level list an engine samples.
///
/// - `Levels` pass through unchanged (the engine applies its list rule to
///   each: ODIM snaps to the nearest sweep, GRIB requires an exact level).
/// - `Interval { min, max }` expands to the collection's advertised levels
///   that fall within `[min, max]` (inclusive; an open end is infinite, so
///   it reaches the extreme level). An interval that selects no advertised
///   level is a 400 — the caller asked for a band the collection doesn't
///   cover.
///
/// `extent` is the collection's advertised vertical levels; it must be
/// present for an interval (callers drop `z` for a collection without a
/// vertical dimension first).
pub fn resolve_z_levels(
    sel: &ZSelector,
    extent: Option<&ds_core::vertical::VerticalDimension>,
) -> Result<Vec<f64>, DataServerError> {
    match sel {
        ZSelector::Levels(v) => Ok(v.clone()),
        ZSelector::Interval { min, max } => {
            let levels = extent.map(|e| e.levels.as_slice()).ok_or_else(|| {
                DataServerError::InvalidParameter(
                    "a `z` interval needs a collection with a vertical extent".into(),
                )
            })?;
            let selected: Vec<f64> = levels
                .iter()
                .copied()
                .filter(|v| *v >= *min && *v <= *max)
                .collect();
            if selected.is_empty() {
                return Err(DataServerError::InvalidParameter(format!(
                    "`z` interval {}/{} selects none of the collection's \
                     available levels",
                    fmt_z_bound(*min),
                    fmt_z_bound(*max)
                )));
            }
            Ok(selected)
        }
    }
}

/// Limits apply to decoded coordinates, before per-point allocations/queries.
pub const MAX_POSITION_COORD_BYTES: usize = 16 * 1024;
pub const MAX_POSITION_POINTS: usize = 64;
/// Most engine position lookups one request may make: MULTIPOINT points ×
/// `datetime` list instants. Each instant re-queries every point, and a
/// lookup can be blocking remote I/O, so the product is capped jointly
/// rather than letting the two limits multiply to 64 × 16 (root CLAUDE.md
/// Critical Rule 9).
pub const MAX_POSITION_LOOKUPS: usize = 256;
/// Combined position response budget, including every point and parameter.
pub const MAX_POSITION_VALUES: usize = 1_000_000;

/// Most location ids one locations query may list (EDR 1.2
/// `/req/edr/REQ_rc-locationid-definition`, #923), the MULTIPOINT cap. Each
/// id is one engine call, made in turn, so this bounds that sequence.
pub const MAX_LOCATION_IDS: usize = MAX_POSITION_POINTS;
/// Combined values of a multi-location response, every location and
/// parameter included: the MULTIPOINT budget.
pub const MAX_LOCATION_VALUES: usize = MAX_POSITION_VALUES;
/// Most engine location lookups one request may make: listed ids ×
/// `datetime` list instants. Each instant re-queries every id, so the
/// product is capped jointly, as for MULTIPOINT ([`MAX_POSITION_LOOKUPS`]),
/// rather than letting the two limits multiply to 64 × 16 (root CLAUDE.md
/// Critical Rule 9).
pub const MAX_LOCATION_LOOKUPS: usize = MAX_POSITION_LOOKUPS;

/// Split a locations query's `{locationId}` path segment, still
/// percent-encoded as it arrived, into the ids it lists (EDR 1.2
/// `/req/edr/REQ_rc-locationid-definition`: a comma-delimited list, OpenAPI
/// `style: simple`, `explode: false`).
///
/// A literal comma separates ids. `%2C` is a comma inside an id, which is
/// how simple-style serialization sends one, so an id containing a comma
/// stays addressable, alone or in a list. Each element is then
/// percent-decoded exactly as the router decodes a whole segment. Repeats
/// collapse to their first occurrence, keeping request order. Elements are
/// not trimmed: ids may contain spaces. An empty element (`a,,b`, a leading
/// or trailing comma), an element that is not UTF-8 once decoded, or more
/// than [`MAX_LOCATION_IDS`] elements, counted before repeats collapse, is a
/// 400.
pub fn split_location_ids(segment: &str) -> Result<Vec<String>, DataServerError> {
    if segment.bytes().filter(|&b| b == b',').count() >= MAX_LOCATION_IDS {
        return Err(DataServerError::QueryTooLarge(format!(
            "locationId lists more than {MAX_LOCATION_IDS} locations"
        )));
    }
    let mut ids: Vec<String> = Vec::new();
    for element in segment.split(',') {
        if element.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "locationId has an empty element: separate location ids with single commas".into(),
            ));
        }
        let id = percent_encoding::percent_decode_str(element)
            .decode_utf8()
            .map_err(|_| {
                DataServerError::InvalidParameter(
                    "locationId element is not UTF-8 once percent-decoded".into(),
                )
            })?
            .into_owned();
        // At most MAX_LOCATION_IDS elements, so a linear scan is cheap.
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Split a position-query `coords` value into one or more `POINT(lon lat)` WKT
/// strings. Accepts either a single `POINT(lon lat)` or a
/// `MULTIPOINT((lon lat),(lon lat),...)` (nested form) /
/// `MULTIPOINT(lon lat, lon lat, ...)` (flat form). The returned strings are
/// always normalized to `POINT(lon lat)` so that existing engine
/// `query_position` implementations can be reused unchanged.
pub fn split_position_coords(coords: &str) -> Result<Vec<String>, DataServerError> {
    if coords.len() > MAX_POSITION_COORD_BYTES {
        return Err(DataServerError::QueryTooLarge(format!(
            "coords exceeds {MAX_POSITION_COORD_BYTES} bytes"
        )));
    }
    let trimmed = coords.trim();

    // Normalize only the keyword so engines receive the same WKT spelling.
    if starts_with_ignore_ascii_case(trimmed, "POINT") {
        let normalized = format!("POINT{}", &trimmed[5..]);
        ds_core::feature::parse_point_coords(&normalized)?;
        return Ok(vec![normalized]);
    }

    // MULTIPOINT(...) — split into individual POINT strings.
    if let Some(rest) = strip_prefix_ignore_ascii_case(trimmed, "MULTIPOINT") {
        let inner = rest
            .trim_start()
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .ok_or_else(|| {
                DataServerError::InvalidParameter(
                    "MULTIPOINT geometry must be wrapped in parentheses".into(),
                )
            })?;

        if inner.bytes().filter(|&b| b == b',').count() >= MAX_POSITION_POINTS {
            return Err(DataServerError::QueryTooLarge(format!(
                "MULTIPOINT exceeds {MAX_POSITION_POINTS} points"
            )));
        }
        let points: Vec<String> = inner
            .split(',')
            .map(|part| {
                let part = part.trim();
                // Nested form "(lon lat)" — strip the inner parens.
                let point_body = part
                    .strip_prefix('(')
                    .and_then(|s| s.strip_suffix(')'))
                    .unwrap_or(part)
                    .trim();
                // Validate "lon lat" so we fail fast before reaching any engine.
                let coords: Vec<&str> = point_body.split_whitespace().collect();
                if coords.len() != 2 {
                    return Err(DataServerError::InvalidParameter(format!(
                        "MULTIPOINT element '{part}' is not 'lon lat'"
                    )));
                }
                let point = format!("POINT({} {})", coords[0], coords[1]);
                ds_core::feature::parse_point_coords(&point)?;
                Ok(point)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if points.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "MULTIPOINT geometry must contain at least one point".into(),
            ));
        }

        return Ok(points);
    }

    Err(DataServerError::InvalidParameter(
        "Expected WKT POINT or MULTIPOINT geometry".into(),
    ))
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    strip_prefix_ignore_ascii_case(s, prefix).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::vertical::{VerticalDimension, VerticalKind};

    #[test]
    fn edr_format_accepts_tokens_and_media_types() {
        for f in [
            None,
            Some(""),
            Some("CoverageJSON"),
            Some("application/prs.coverage+json"),
            Some("application/prs.coverage json"),
            Some("application/vnd.cov+json"),
        ] {
            assert_eq!(
                parse_edr_format(f).unwrap(),
                EdrFormat::CoverageJson,
                "{f:?}"
            );
        }
        for f in ["png", "PNG", "image/png"] {
            assert_eq!(parse_edr_format(Some(f)).unwrap(), EdrFormat::Png, "{f}");
        }
        for f in [
            "GeoJSON",
            "geojson",
            "GEOJSON",
            "application/geo+json",
            "Application/Geo+JSON",
            "application/geo json",
        ] {
            assert_eq!(
                parse_edr_format(Some(f)).unwrap(),
                EdrFormat::GeoJson,
                "{f}"
            );
        }
        for f in ["json", "application/json", "image/jpeg", "geo+json"] {
            assert!(parse_edr_format(Some(f)).is_err(), "{f}");
        }
    }

    #[test]
    fn geojson_is_offered_for_point_queries_of_station_series_only() {
        use EdrFormat::{CoverageJson, GeoJson, Html, Png};
        let along = TrajectoryShape::AlongPath;
        assert_eq!(
            query_formats("position", true, along),
            [CoverageJson, GeoJson, Png, Html]
        );
        assert_eq!(
            query_formats("locations", true, along),
            [CoverageJson, GeoJson, Png, Html]
        );
        assert_eq!(
            query_formats("radius", true, along),
            [CoverageJson, GeoJson, Html]
        );
        assert_eq!(query_formats("area", true, along), [CoverageJson, Html]);
        assert_eq!(query_formats("cube", true, along), [CoverageJson, Html]);
        for qt in ["locations", "position"] {
            assert_eq!(
                query_formats(qt, false, along),
                [CoverageJson, Png, Html],
                "{qt}"
            );
        }
        for qt in ["area", "radius", "cube"] {
            assert_eq!(
                query_formats(qt, false, along),
                [CoverageJson, Html],
                "{qt}"
            );
        }
    }

    /// HTML (#971): `f=html` or `text/html`, and an `Accept` naming it. A
    /// browser's `Accept` names only HTML explicitly, so it gets the page;
    /// a tie with a data format keeps the data format, and wildcards keep
    /// the default.
    #[test]
    fn html_is_negotiated_by_f_and_accept_after_the_data_formats() {
        use EdrFormat::{CoverageJson, GeoJson, Html};
        for f in ["html", "HTML", "text/html", "Text/HTML"] {
            assert_eq!(parse_edr_format(Some(f)).unwrap(), Html, "{f}");
        }
        let offered = query_formats("area", false, TrajectoryShape::AlongPath);
        let pick = |accept: &str| {
            negotiate_edr_format(None, Some(accept), offered, "q")
                .unwrap()
                .format
        };
        let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
        assert_eq!(pick(browser), Html);
        assert_eq!(pick("text/html, application/vnd.cov+json"), CoverageJson);
        assert_eq!(
            pick("text/html;q=0.5, application/vnd.cov+json"),
            CoverageJson
        );
        assert_eq!(pick("text/*"), CoverageJson);
        assert_eq!(pick("*/*"), CoverageJson);
        // The location list: GeoJSON by default and for `json`, HTML on
        // request, anything else a 400.
        let list = |f: Option<&str>, accept: Option<&str>| {
            negotiate_list_format(f, accept, "the location list").map(|n| n.format)
        };
        assert_eq!(list(None, None).unwrap(), GeoJson);
        assert_eq!(list(None, Some(browser)).unwrap(), Html);
        for f in [
            "json",
            "application/json",
            "GeoJSON",
            "application/geo+json",
        ] {
            assert_eq!(list(Some(f), Some(browser)).unwrap(), GeoJson, "{f}");
        }
        assert_eq!(list(Some("html"), None).unwrap(), Html);
        for f in ["foo", "CoverageJSON", "PNG"] {
            assert!(list(Some(f), None).is_err(), "{f}");
        }
    }

    /// A trajectory's formats follow its shape (#926), never GeoJSON: a
    /// radar cross-section is also a PNG heatmap, an along-path trajectory
    /// CoverageJSON only — whatever the engine's station-series flag.
    #[test]
    fn trajectory_formats_follow_the_shape() {
        use EdrFormat::{CoverageJson, Html, Png};
        for station_series in [false, true] {
            assert_eq!(
                query_formats("trajectory", station_series, TrajectoryShape::CrossSection),
                [CoverageJson, Png, Html]
            );
            assert_eq!(
                query_formats("trajectory", station_series, TrajectoryShape::AlongPath),
                [CoverageJson, Html]
            );
        }
        // Along a path: PNG and GeoJSON are 400s, `Accept: image/png` falls
        // back to CoverageJSON (with `Vary`: HTML is offered too).
        let along = query_formats("trajectory", false, TrajectoryShape::AlongPath);
        for f in ["PNG", "GeoJSON"] {
            assert!(negotiate_edr_format(Some(f), None, along, "trajectory queries").is_err());
        }
        assert_eq!(
            negotiate_edr_format(None, Some("image/png"), along, "trajectory queries").unwrap(),
            NegotiatedFormat {
                format: CoverageJson,
                vary_accept: true
            }
        );
        // A cross-section: PNG by `f` or by `Accept` (then with `Vary`).
        let section = query_formats("trajectory", false, TrajectoryShape::CrossSection);
        assert!(
            negotiate_edr_format(Some("GeoJSON"), None, section, "trajectory queries").is_err()
        );
        assert_eq!(
            negotiate_edr_format(Some("png"), None, section, "trajectory queries")
                .unwrap()
                .format,
            Png
        );
        assert_eq!(
            negotiate_edr_format(None, Some("image/png"), section, "trajectory queries").unwrap(),
            NegotiatedFormat {
                format: Png,
                vary_accept: true
            }
        );
    }

    #[test]
    fn explicit_f_must_be_offered() {
        use EdrFormat::{CoverageJson, GeoJson, Png};
        let offered = [CoverageJson, GeoJson, Png];
        let got = negotiate_edr_format(
            Some("geojson"),
            Some("image/png"),
            &offered,
            "position queries",
        )
        .unwrap();
        assert_eq!(
            got,
            NegotiatedFormat {
                format: GeoJson,
                vary_accept: false
            }
        );
        let err = negotiate_edr_format(
            Some("GeoJSON"),
            None,
            &[CoverageJson, Png],
            "position queries",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("GeoJSON output is not available for position queries")
                && err.contains("available: CoverageJSON, PNG"),
            "{err}"
        );
    }

    #[test]
    fn accept_picks_the_preferred_offered_media_type() {
        use EdrFormat::{CoverageJson, GeoJson, Png};
        let offered = [CoverageJson, GeoJson, Png];
        let pick = |accept: Option<&str>| {
            let got = negotiate_edr_format(None, accept, &offered, "q").unwrap();
            assert!(got.vary_accept);
            got.format
        };
        assert_eq!(pick(None), CoverageJson);
        assert_eq!(pick(Some("*/*")), CoverageJson);
        assert_eq!(pick(Some("application/json")), CoverageJson);
        assert_eq!(pick(Some("application/geo+json")), GeoJson);
        assert_eq!(pick(Some("Application/Geo+JSON; charset=utf-8")), GeoJson);
        assert_eq!(pick(Some("image/png")), Png);
        assert_eq!(
            pick(Some("application/geo+json;q=0.5, application/vnd.cov+json")),
            CoverageJson
        );
        assert_eq!(
            pick(Some(
                "application/prs.coverage+json;q=0.2, application/geo+json;q=0.9"
            )),
            GeoJson
        );
        // Equal q: the earlier offered format wins.
        assert_eq!(pick(Some("image/png, application/geo+json")), GeoJson);
        // q=0 and malformed q exclude; nothing left → the default.
        assert_eq!(pick(Some("application/geo+json;q=0")), CoverageJson);
        assert_eq!(pick(Some("application/geo+json;q=abc")), CoverageJson);
        // A format the query does not offer is not chosen, and a query
        // with one format does not vary.
        let radius = negotiate_edr_format(None, Some("image/png"), &[CoverageJson], "q").unwrap();
        assert_eq!(
            radius,
            NegotiatedFormat {
                format: CoverageJson,
                vary_accept: false
            }
        );
    }

    #[test]
    fn location_ids_split_on_literal_commas_in_request_order() {
        assert_eq!(split_location_ids("EGLL").unwrap(), ["EGLL"]);
        assert_eq!(
            split_location_ids("EGLL,EFHK,CYOW").unwrap(),
            ["EGLL", "EFHK", "CYOW"]
        );
        // Repeats collapse to their first occurrence.
        assert_eq!(split_location_ids("b,a,b,a,c").unwrap(), ["b", "a", "c"]);
        assert_eq!(split_location_ids("a,a").unwrap(), ["a"]);
    }

    #[test]
    fn location_ids_are_percent_decoded_after_splitting() {
        // `%2C` is a comma inside an id, a literal comma separates ids.
        assert_eq!(
            split_location_ids("Helsinki%2C%20Kaisaniemi,Oulu").unwrap(),
            ["Helsinki, Kaisaniemi", "Oulu"]
        );
        // Not form decoding: `+` is itself, and spaces are kept untrimmed.
        assert_eq!(split_location_ids("a+b,%20c").unwrap(), ["a+b", " c"]);
        assert_eq!(
            split_location_ids("%C3%85land,x").unwrap(),
            ["\u{c5}land", "x"]
        );
        let err = split_location_ids("%FF,x").unwrap_err().to_string();
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn location_ids_reject_empty_elements() {
        for segment in [",a", "a,", "a,,b", ","] {
            let err = split_location_ids(segment).unwrap_err();
            assert!(
                matches!(&err, DataServerError::InvalidParameter(m) if m.contains("empty element")),
                "{segment}: {err}"
            );
        }
    }

    #[test]
    fn location_ids_are_capped_before_repeats_collapse() {
        let at_cap = vec!["s"; MAX_LOCATION_IDS].join(",");
        assert_eq!(split_location_ids(&at_cap).unwrap(), ["s"]);
        let distinct: Vec<String> = (0..MAX_LOCATION_IDS).map(|i| format!("s{i}")).collect();
        assert_eq!(
            split_location_ids(&distinct.join(",")).unwrap().len(),
            MAX_LOCATION_IDS
        );
        let over = vec!["s"; MAX_LOCATION_IDS + 1].join(",");
        let err = split_location_ids(&over).unwrap_err();
        assert!(
            matches!(&err, DataServerError::QueryTooLarge(m) if m.contains("more than 64")),
            "{err}"
        );
    }

    #[test]
    fn parse_z_none_and_blank() {
        assert_eq!(parse_z(None).unwrap(), None);
        assert_eq!(parse_z(Some("   ")).unwrap(), None);
    }

    #[test]
    fn parse_z_single_and_list() {
        assert_eq!(
            parse_z(Some("0.5")).unwrap(),
            Some(ZSelector::Levels(vec![0.5]))
        );
        assert_eq!(
            parse_z(Some("850,700,500")).unwrap(),
            Some(ZSelector::Levels(vec![850.0, 700.0, 500.0]))
        );
    }

    #[test]
    fn parse_z_interval_orders_endpoints() {
        assert_eq!(
            parse_z(Some("0.3/15")).unwrap(),
            Some(ZSelector::Interval {
                min: 0.3,
                max: 15.0
            })
        );
        // Reversed endpoints normalise to (min, max).
        assert_eq!(
            parse_z(Some("850/500")).unwrap(),
            Some(ZSelector::Interval {
                min: 500.0,
                max: 850.0
            })
        );
    }

    #[test]
    fn parse_z_rejects_bad_interval_and_values() {
        assert!(parse_z(Some("1/2/3")).is_err());
        assert!(parse_z(Some("a/2")).is_err());
        assert!(parse_z(Some("nan")).is_err());
        assert!(parse_z(Some("1,,3")).is_err());
    }

    #[test]
    fn parse_z_open_intervals() {
        assert_eq!(
            parse_z(Some("../850")).unwrap(),
            Some(ZSelector::Interval {
                min: f64::NEG_INFINITY,
                max: 850.0
            })
        );
        assert_eq!(
            parse_z(Some("500/..")).unwrap(),
            Some(ZSelector::Interval {
                min: 500.0,
                max: f64::INFINITY
            })
        );
        // Whitespace around the parts is tolerated, like the other forms.
        assert_eq!(
            parse_z(Some(" .. / 850 ")).unwrap(),
            parse_z(Some("../850")).unwrap()
        );
    }

    #[test]
    fn parse_z_rejects_bad_open_intervals() {
        for z in [
            "../..",
            "..",
            "../",
            "/..",
            "../abc",
            "inf/..",
            "..,850",
            "../850/..",
        ] {
            assert!(parse_z(Some(z)).is_err(), "{z}");
        }
    }

    /// `R20/100/50` is the standard's example: "20 levels at 50 unit
    /// intervals starting at level 100" — the count is of levels.
    #[test]
    fn parse_z_recurring_expands_to_levels() {
        let Some(ZSelector::Levels(levels)) = parse_z(Some("R20/100/50")).unwrap() else {
            panic!("a recurring interval is a list");
        };
        assert_eq!(levels.len(), 20);
        assert_eq!(levels[0], 100.0);
        assert_eq!(levels[19], 1050.0);
        assert_eq!(
            parse_z(Some("R3/1000/-150")).unwrap(),
            Some(ZSelector::Levels(vec![1000.0, 850.0, 700.0]))
        );
        assert_eq!(
            parse_z(Some("r1/0.5/1")).unwrap(),
            Some(ZSelector::Levels(vec![0.5]))
        );
        let max = format!("R{MAX_Z_RECURRENCES}/0/1");
        let Some(ZSelector::Levels(levels)) = parse_z(Some(&max)).unwrap() else {
            panic!("the cap itself is accepted");
        };
        assert_eq!(levels.len(), MAX_Z_RECURRENCES as usize);
    }

    #[test]
    fn parse_z_rejects_bad_recurring_intervals() {
        for z in [
            "R",
            "R/100/50",
            "R0/100/50",
            "R-2/100/50",
            "R1.5/100/50",
            "R20/100",
            "R20/100/50/1",
            "R20/../50",
            "R20/100/0",
            "R20/abc/50",
            "R20/100/inf",
            "R2/1e308/1e308",
        ] {
            assert!(parse_z(Some(z)).is_err(), "{z}");
        }
        let over = format!("R{}/0/1", MAX_Z_RECURRENCES + 1);
        let err = parse_z(Some(&over)).unwrap_err().to_string();
        assert!(err.contains(&MAX_Z_RECURRENCES.to_string()), "{err}");
    }

    fn pressure_extent() -> VerticalDimension {
        VerticalDimension::new(
            VerticalKind::Pressure,
            vec![1000.0, 850.0, 700.0, 500.0, 250.0],
        )
    }

    #[test]
    fn resolve_z_levels_open_interval_reaches_the_extreme_level() {
        let ext = pressure_extent();
        let below = parse_z(Some("../700")).unwrap().unwrap();
        assert_eq!(
            resolve_z_levels(&below, Some(&ext)).unwrap(),
            vec![700.0, 500.0, 250.0]
        );
        let above = parse_z(Some("700/..")).unwrap().unwrap();
        assert_eq!(
            resolve_z_levels(&above, Some(&ext)).unwrap(),
            vec![1000.0, 850.0, 700.0]
        );
        // Open past every level: none selected, and the message shows `..`.
        let none = parse_z(Some("1001/..")).unwrap().unwrap();
        let err = resolve_z_levels(&none, Some(&ext)).unwrap_err().to_string();
        assert!(err.contains("1001/.."), "{err}");
    }

    #[test]
    fn resolve_z_levels_passes_a_recurring_list_through() {
        let sel = parse_z(Some("R3/1000/-150")).unwrap().unwrap();
        assert_eq!(
            resolve_z_levels(&sel, Some(&pressure_extent())).unwrap(),
            vec![1000.0, 850.0, 700.0]
        );
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn parse_datetime_instant_and_intervals() {
        assert_eq!(parse_datetime(None).unwrap(), None);
        let at = t("2024-01-01T03:00:00Z");
        assert_eq!(
            parse_datetime(Some("2024-01-01T03:00:00Z")).unwrap(),
            Some(DatetimeSelector::Window(at, at))
        );
        assert_eq!(
            parse_datetime(Some("../2024-01-01T03:00:00Z")).unwrap(),
            Some(DatetimeSelector::Window(DateTime::<Utc>::MIN_UTC, at))
        );
        assert!(parse_datetime(Some("")).is_err());
        assert!(parse_datetime(Some("not-a-date")).is_err());
        // An interval that ends before it starts selects nothing: a 400.
        let err = parse_datetime(Some("2024-01-02T00:00:00Z/2024-01-01T00:00:00Z")).unwrap_err();
        assert!(err.to_string().contains("ends before it starts"), "{err}");
        assert!(parse_datetime(Some("2024-01-01T00:00:00Z/2024-01-01T00:00:00Z")).is_ok());
        assert!(parse_datetime(Some("../..")).is_ok());
    }

    /// The intervals `location_time_filter` receives (#932): a window with
    /// its open ends unbounded, a list as one instant interval per element.
    #[test]
    fn datetime_selector_intervals() {
        let (a, b) = (t("2024-01-01T00:00:00Z"), t("2024-01-01T06:00:00Z"));
        let interval = |start, end| DatetimeInterval { start, end };
        let intervals = |raw| parse_datetime(Some(raw)).unwrap().unwrap().intervals();
        assert_eq!(
            intervals("2024-01-01T00:00:00Z/2024-01-01T06:00:00Z"),
            [interval(Some(a), Some(b))]
        );
        assert_eq!(
            intervals("../2024-01-01T06:00:00Z"),
            [interval(None, Some(b))]
        );
        assert_eq!(
            intervals("2024-01-01T00:00:00Z/.."),
            [interval(Some(a), None)]
        );
        assert_eq!(intervals("../.."), [interval(None, None)]);
        assert_eq!(
            intervals("2024-01-01T06:00:00Z,2024-01-01T00:00:00Z"),
            [interval(Some(a), Some(a)), interval(Some(b), Some(b))]
        );
    }

    /// The EDR 1.2 example `2018-02-12T00:00Z,2018-02-12T01:00Z,2018-02-14T12:00Z`
    /// omits seconds; RFC 3339 requires them, so it is written with them here.
    #[test]
    fn parse_datetime_list_sorts_and_collapses_repeats() {
        let got = parse_datetime(Some(
            "2018-02-14T12:00:00Z,2018-02-12T00:00:00Z, 2018-02-12T01:00:00Z,2018-02-12T00:00:00+00:00",
        ))
        .unwrap();
        assert_eq!(
            got,
            Some(DatetimeSelector::Instants(vec![
                t("2018-02-12T00:00:00Z"),
                t("2018-02-12T01:00:00Z"),
                t("2018-02-14T12:00:00Z"),
            ]))
        );
        assert_eq!(
            got.unwrap().envelope(),
            (t("2018-02-12T00:00:00Z"), t("2018-02-14T12:00:00Z"))
        );
        // A list that collapses to one instant is that instant.
        let at = t("2018-02-12T00:00:00Z");
        assert_eq!(
            parse_datetime(Some("2018-02-12T00:00:00Z,2018-02-12T00:00:00Z")).unwrap(),
            Some(DatetimeSelector::Window(at, at))
        );
    }

    #[test]
    fn parse_datetime_rejects_bad_lists() {
        for raw in [
            "2018-02-12T00:00:00Z,",
            ",2018-02-12T00:00:00Z",
            "2018-02-12T00:00:00Z,,2018-02-12T01:00:00Z",
            "2018-02-12T00:00:00Z,2018-02-12T01:00:00Z/..",
            "../2018-02-12T00:00:00Z,2018-02-12T01:00:00Z",
            "2018-02-12T00:00:00Z,..",
            "2018-02-12T00:00:00Z,tomorrow",
        ] {
            assert!(parse_datetime(Some(raw)).is_err(), "{raw}");
        }
        let at_cap = vec!["2018-02-12T00:00:00Z"; MAX_DATETIME_INSTANTS].join(",");
        assert!(parse_datetime(Some(&at_cap)).is_ok());
        let over = vec!["2018-02-12T00:00:00Z"; MAX_DATETIME_INSTANTS + 1].join(",");
        let err = parse_datetime(Some(&over)).unwrap_err().to_string();
        assert!(err.contains(&MAX_DATETIME_INSTANTS.to_string()), "{err}");
    }

    fn utc(rfc3339: &str) -> DateTime<Utc> {
        rfc3339.parse().unwrap()
    }

    /// The instants a `datetime` value selects, whichever variant holds them.
    fn instants(raw: &str) -> Vec<DateTime<Utc>> {
        match parse_datetime(Some(raw)).unwrap() {
            Some(DatetimeSelector::Instants(v)) => v,
            Some(DatetimeSelector::Window(a, b)) if a == b => vec![a],
            other => panic!("{raw}: not instants: {other:?}"),
        }
    }

    /// `Rn` counts instants (#933): `R4/T/PT6H` is T, T+6h, T+12h, T+18h, as
    /// `z=R20/100/50` is 20 levels.
    #[test]
    fn parse_datetime_repeating_counts_instants() {
        assert_eq!(
            parse_datetime(Some("R4/2026-10-01T00:00:00Z/PT6H")).unwrap(),
            Some(DatetimeSelector::Instants(vec![
                utc("2026-10-01T00:00:00Z"),
                utc("2026-10-01T06:00:00Z"),
                utc("2026-10-01T12:00:00Z"),
                utc("2026-10-01T18:00:00Z"),
            ]))
        );
        // The same selector the equivalent list parses to.
        assert_eq!(
            parse_datetime(Some("R3/2024-01-01T01:00:00Z/PT1H")).unwrap(),
            parse_datetime(Some(
                "2024-01-01T01:00:00Z,2024-01-01T02:00:00Z,2024-01-01T03:00:00Z"
            ))
            .unwrap()
        );
        // `R1` is the start alone, like a one-instant list; `r` is accepted
        // as `z` accepts it.
        let at = utc("2026-10-01T00:00:00Z");
        assert_eq!(
            parse_datetime(Some("R1/2026-10-01T00:00:00Z/PT6H")).unwrap(),
            Some(DatetimeSelector::Window(at, at))
        );
        assert_eq!(instants("r2/2026-10-01T00:00:00Z/PT6H").len(), 2);
        // At the cap.
        let full = instants(&format!(
            "R{MAX_DATETIME_INSTANTS}/2026-10-01T00:00:00Z/PT1H"
        ));
        assert_eq!(full.len(), MAX_DATETIME_INSTANTS);
        assert_eq!(full.last(), Some(&utc("2026-10-01T15:00:00Z")));
    }

    /// Durations are fixed lengths added to the UTC start: days across a
    /// leap day and a year end, weeks, minutes past 60, mixed units, and a
    /// start with an offset.
    #[test]
    fn parse_datetime_repeating_expands_durations() {
        let cases: [(&str, &[&str]); 7] = [
            (
                "R3/2024-02-28T12:00:00Z/P1D",
                &[
                    "2024-02-28T12:00:00Z",
                    "2024-02-29T12:00:00Z",
                    "2024-03-01T12:00:00Z",
                ],
            ),
            (
                "R2/2026-12-31T23:30:00Z/PT1H",
                &["2026-12-31T23:30:00Z", "2027-01-01T00:30:00Z"],
            ),
            (
                "R3/2026-10-01T00:00:00Z/P1W",
                &[
                    "2026-10-01T00:00:00Z",
                    "2026-10-08T00:00:00Z",
                    "2026-10-15T00:00:00Z",
                ],
            ),
            (
                "R3/2026-10-01T00:00:00Z/PT90M",
                &[
                    "2026-10-01T00:00:00Z",
                    "2026-10-01T01:30:00Z",
                    "2026-10-01T03:00:00Z",
                ],
            ),
            (
                "R2/2026-10-01T00:00:00Z/P1DT1H30M15S",
                &["2026-10-01T00:00:00Z", "2026-10-02T01:30:15Z"],
            ),
            (
                // The offset normalises to UTC before the steps are added.
                "R2/2026-10-01T02:00:00+02:00/PT1H",
                &["2026-10-01T00:00:00Z", "2026-10-01T01:00:00Z"],
            ),
            (
                // A fractional start keeps its fraction on every step.
                "R2/2026-10-01T00:00:00.5Z/PT10S",
                &["2026-10-01T00:00:00.5Z", "2026-10-01T00:00:10.5Z"],
            ),
        ];
        for (raw, expected) in cases {
            let expected: Vec<_> = expected.iter().map(|s| utc(s)).collect();
            assert_eq!(instants(raw), expected, "{raw}");
        }
    }

    #[test]
    fn parse_datetime_rejects_bad_repeating_intervals() {
        let t = "2026-10-01T00:00:00Z";
        // (value, a fragment the 400 must name)
        let cases = [
            (format!("R0/{t}/PT1H"), "R0 names none"),
            (format!("R/{t}/PT1H"), "unbounded"),
            (format!("R-1/{t}/PT1H"), "unbounded"),
            (format!("R+4/{t}/PT1H"), "positive whole number"),
            (format!("R4.0/{t}/PT1H"), "positive whole number"),
            (format!("Rx/{t}/PT1H"), "positive whole number"),
            (
                format!("R{}/{t}/PT1H", MAX_DATETIME_INSTANTS + 1),
                "the maximum is 16",
            ),
            (
                format!("R99999999999999999999999/{t}/PT1H"),
                "the maximum is 16",
            ),
            (format!("R4/{t}"), "Rn/date-time/duration"),
            ("R4".to_string(), "Rn/date-time/duration"),
            (format!("R4/{t}/PT1H/PT1H"), "Rn/date-time/duration"),
            ("R4/../PT1H".to_string(), "not an RFC 3339 date-time"),
            (format!("R4/PT1H/{t}"), "not an RFC 3339 date-time"),
            (format!("R4/{t}/{t}"), "must start with 'P'"),
            (format!("R4/{t}/PT0H"), "zero or negative"),
            (format!("R4/{t}/P0D"), "zero or negative"),
            (format!("R4/{t}/-PT1H"), "must start with 'P'"),
            (format!("R4/{t}/P1M"), "months ('M')"),
            (format!("R4/{t}/P1Y"), "years ('Y')"),
            (format!("R4/{t}/PT1.5S"), "seconds"),
            (format!("R4/{t}/P99999999999999999D"), "too long"),
            (
                "R2/9999-12-31T00:00:00Z/P100000000D".to_string(),
                "supported date range",
            ),
            (
                format!("R2/{t}/PT1H,{t}"),
                "cannot be part of a datetime list",
            ),
            (format!("{t},R2/{t}/PT1H"), "instants only"),
        ];
        for (raw, fragment) in cases {
            let err = parse_datetime(Some(&raw)).unwrap_err();
            assert!(
                matches!(err, DataServerError::InvalidDatetime(_)),
                "{raw}: {err:?}"
            );
            let msg = err.to_string();
            assert!(msg.contains(fragment), "{raw}: {msg}");
            assert!(!msg.contains("Config"), "{raw}: {msg}");
        }
    }

    #[test]
    fn resolve_z_levels_passes_through_list() {
        let sel = ZSelector::Levels(vec![1.5, 9.0]);
        assert_eq!(resolve_z_levels(&sel, None).unwrap(), vec![1.5, 9.0]);
    }

    #[test]
    fn resolve_z_levels_expands_interval_against_extent() {
        let ext = VerticalDimension::new(
            VerticalKind::ElevationAngle,
            vec![
                0.3, 0.7, 1.5, 2.0, 3.0, 5.0, 7.0, 9.0, 11.0, 15.0, 25.0, 45.0,
            ],
        );
        let sel = ZSelector::Interval {
            min: 0.3,
            max: 15.0,
        };
        let got = resolve_z_levels(&sel, Some(&ext)).unwrap();
        assert_eq!(
            got,
            vec![0.3, 0.7, 1.5, 2.0, 3.0, 5.0, 7.0, 9.0, 11.0, 15.0]
        );
    }

    #[test]
    fn resolve_z_levels_interval_outside_extent_is_error() {
        let ext = VerticalDimension::new(VerticalKind::ElevationAngle, vec![0.3, 0.7, 1.5]);
        let sel = ZSelector::Interval {
            min: 20.0,
            max: 30.0,
        };
        assert!(resolve_z_levels(&sel, Some(&ext)).is_err());
        // An interval with no extent at all is also an error.
        assert!(resolve_z_levels(&sel, None).is_err());
    }

    #[test]
    fn single_point_passthrough() {
        let points = split_position_coords("POINT(24.94 60.17)").unwrap();
        assert_eq!(points, vec!["POINT(24.94 60.17)".to_string()]);
    }

    #[test]
    fn coordinate_byte_boundary_is_inclusive() {
        let point = "POINT(1 2)";
        let at_limit = format!(
            "{point}{}",
            " ".repeat(MAX_POSITION_COORD_BYTES - point.len())
        );
        assert_eq!(split_position_coords(&at_limit).unwrap(), vec![point]);
        assert!(split_position_coords(&(at_limit + " ")).is_err());
    }

    #[test]
    fn point_case_insensitive() {
        let points = split_position_coords("point(24.94 60.17)").unwrap();
        assert_eq!(points, vec!["POINT(24.94 60.17)".to_string()]);
    }

    #[test]
    fn multipoint_nested_form() {
        let points =
            split_position_coords("MULTIPOINT((24.94 60.17),(23.76 61.5),(27.67 62.9))").unwrap();
        assert_eq!(
            points,
            vec![
                "POINT(24.94 60.17)".to_string(),
                "POINT(23.76 61.5)".to_string(),
                "POINT(27.67 62.9)".to_string(),
            ]
        );
    }

    #[test]
    fn multipoint_flat_form() {
        let points = split_position_coords("MULTIPOINT(24.94 60.17, 23.76 61.5)").unwrap();
        assert_eq!(
            points,
            vec![
                "POINT(24.94 60.17)".to_string(),
                "POINT(23.76 61.5)".to_string(),
            ]
        );
    }

    #[test]
    fn multipoint_case_insensitive() {
        let points = split_position_coords("MultiPoint((1 2),(3 4))").unwrap();
        assert_eq!(points.len(), 2);
    }

    #[test]
    fn multipoint_rejects_non_numeric() {
        assert!(split_position_coords("MULTIPOINT((a b),(1 2))").is_err());
    }

    #[test]
    fn multipoint_rejects_wrong_arity() {
        assert!(split_position_coords("MULTIPOINT((1 2 3),(4 5))").is_err());
    }

    #[test]
    fn rejects_polygon() {
        assert!(split_position_coords("POLYGON((0 0,1 0,1 1,0 1,0 0))").is_err());
    }

    #[test]
    fn cube_parameters_are_known_and_given_once() {
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let params = CubeQueryParams::from_pairs(pairs(&[
            ("bbox", "0,0,1,1"),
            ("z", "850"),
            ("resolution-x", "10"),
            ("parameter-name", "t"),
        ]))
        .unwrap();
        assert_eq!(params.bbox.as_deref(), Some("0,0,1,1"));
        assert_eq!(params.z.as_deref(), Some("850"));
        assert_eq!(params.resolution_x.as_deref(), Some("10"));
        assert_eq!(params.parameter_name.as_deref(), Some("t"));
        assert_eq!(params.resolution_y, None);
        let unknown = CubeQueryParams::from_pairs(pairs(&[("coords", "POINT(0 0)")]))
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("'coords'"), "{unknown}");
        assert!(unknown.contains(&CUBE_PARAMETERS.join(", ")), "{unknown}");
        let repeated = CubeQueryParams::from_pairs(pairs(&[("z", "850"), ("z", "500")]))
            .unwrap_err()
            .to_string();
        assert!(repeated.contains("'z'"), "{repeated}");
    }

    #[test]
    fn cube_bbox_is_four_or_six_numbers() {
        let (bbox, z) = parse_cube_bbox("20, 55,30,65").unwrap();
        assert_eq!(
            (bbox.west, bbox.south, bbox.east, bbox.north),
            (20.0, 55.0, 30.0, 65.0)
        );
        assert_eq!(z, None);
        // The vertical pair of six numbers is an interval, in either order.
        let (bbox, z) = parse_cube_bbox("20,55,1000,30,65,500").unwrap();
        assert_eq!((bbox.east, bbox.north), (30.0, 65.0));
        assert_eq!(
            z,
            Some(ZSelector::Interval {
                min: 500.0,
                max: 1000.0
            })
        );
        // Across the antimeridian: west > east is kept as given.
        let (bbox, _) = parse_cube_bbox("170,10,-170,20").unwrap();
        assert!(bbox.crosses_antimeridian());
        for bad in [
            "",
            "1,2,3",
            "1,2,3,4,5",
            "a,0,1,1",
            "0,0,1,inf",
            "0,0,200,1",
            "0,10,1,5",
        ] {
            assert!(
                matches!(parse_cube_bbox(bad), Err(DataServerError::InvalidBbox(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn resolution_is_a_count_from_zero_to_the_budget() {
        assert_eq!(parse_resolution("resolution-x", None).unwrap(), None);
        assert_eq!(parse_resolution("resolution-x", Some("0")).unwrap(), None);
        assert_eq!(
            parse_resolution("resolution-x", Some(" 10 ")).unwrap(),
            Some(10)
        );
        assert_eq!(
            parse_resolution("resolution-z", Some("1000000")).unwrap(),
            Some(MAX_RESOLUTION)
        );
        for bad in ["-1", "1.5", "ten", "", "1000001"] {
            let err = parse_resolution("resolution-y", Some(bad))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("resolution-y") && err.contains("from 0") && err.contains("1000000"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn crs_is_crs84_only() {
        for ok in [
            None,
            Some(CRS84),
            Some("https://www.opengis.net/def/crs/OGC/1.3/CRS84"),
            Some("CRS84"),
            Some("ogc:crs84"),
            Some("[OGC:CRS84]"),
        ] {
            assert!(check_crs(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "EPSG:4326",
            "native",
            "http://www.opengis.net/def/crs/EPSG/0/3067",
        ] {
            let err = check_crs(Some(bad)).unwrap_err().to_string();
            assert!(err.contains(CRS84), "{bad}: {err}");
        }
    }

    #[test]
    fn limit_parses_clamps_and_rejects() {
        assert_eq!(parse_limit(None).unwrap(), None);
        assert_eq!(parse_limit(Some("  ")).unwrap(), None);
        assert_eq!(parse_limit(Some("1")).unwrap(), Some(1));
        assert_eq!(parse_limit(Some(" 25 ")).unwrap(), Some(25));
        assert_eq!(parse_limit(Some("10000")).unwrap(), Some(MAX_LIMIT));
        // Above the maximum clamps, including values no integer type holds.
        assert_eq!(parse_limit(Some("10001")).unwrap(), Some(MAX_LIMIT));
        assert_eq!(
            parse_limit(Some("99999999999999999999999999")).unwrap(),
            Some(MAX_LIMIT)
        );
        for bad in ["0", "000", "-1", "+5", "1.5", "1e3", "abc", "5,6", "NaN"] {
            let err = parse_limit(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("1 to 10000"), "{bad}: {err}");
        }
    }

    #[test]
    fn offset_parses_and_rejects() {
        assert_eq!(parse_offset(None).unwrap(), 0);
        assert_eq!(parse_offset(Some("")).unwrap(), 0);
        assert_eq!(parse_offset(Some("0")).unwrap(), 0);
        assert_eq!(parse_offset(Some("20")).unwrap(), 20);
        for bad in ["-1", "+1", "1.0", "x", "99999999999999999999999999"] {
            assert!(parse_offset(Some(bad)).is_err(), "{bad}");
        }
    }

    fn pairs(q: &[(&str, &str)]) -> Vec<(String, String)> {
        q.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// An unknown parameter is a 400 naming the valid ones; the accepted
    /// filters pass through to the links (#605, #932).
    #[test]
    fn locations_query_rejects_unknown_parameters() {
        let err = parse_locations_query(pairs(&[("limti", "5")])).unwrap_err();
        assert!(err.to_string().contains("limti"), "{err}");
        assert!(err.to_string().contains("limit, offset"), "{err}");
        assert!(parse_locations_query(pairs(&[("sortby", "id")])).is_err());
        let query = parse_locations_query(pairs(&[
            ("bbox", "0,0,1,1"),
            ("datetime", "../2026-01-01T00:00:00Z"),
        ]))
        .unwrap();
        assert_eq!(query.paging, None);
        assert!(query.is_filtered());
        let query = parse_locations_query(pairs(&[("bbox", "0,0,1,1"), ("limit", "2")])).unwrap();
        assert_eq!(query.paging.unwrap().limit, 2);
    }

    #[test]
    fn locations_query_splits_query() {
        // No limit: the complete inventory, other parameters untouched.
        let query = parse_locations_query(pairs(&[("f", "json")])).unwrap();
        assert_eq!(query.paging, None);
        assert!(!query.is_filtered());
        assert_eq!(query.href("b", 0), "b?f=json");
        assert_eq!(
            parse_locations_query(pairs(&[("limit", "")]))
                .unwrap()
                .paging,
            None
        );
        let query = parse_locations_query(pairs(&[
            ("f", "geo json"),
            ("limit", "50000"),
            ("offset", "20"),
        ]))
        .unwrap();
        let page = query.paging.unwrap();
        assert_eq!((page.limit, page.offset), (MAX_LIMIT, 20));
        // Links repeat the other parameters, encoded, and the clamped limit.
        assert_eq!(
            query.href("https://x/locations", 10020),
            "https://x/locations?f=geo%20json&limit=10000&offset=10020"
        );
        assert_eq!(query.href("b", 0), "b?f=geo%20json&limit=10000");
        assert_eq!(parse_locations_query(Vec::new()).unwrap().href("b", 0), "b");
        for bad in [
            &[("offset", "3")][..],
            &[("limit", "0")],
            &[("limit", "2"), ("offset", "-1")],
            &[("limit", "2"), ("limit", "3")],
            &[("limit", "2"), ("offset", "1"), ("offset", "1")],
        ] {
            assert!(parse_locations_query(pairs(bad)).is_err(), "{bad:?}");
        }
    }

    /// `bbox` and `datetime` are parsed, kept in request order for the
    /// links, and a malformed or repeated one is a 400 naming it (#932).
    #[test]
    fn locations_query_parses_its_filters() {
        let query = parse_locations_query(pairs(&[
            ("datetime", "2026-01-01T00:00:00Z/.."),
            ("limit", "2"),
            ("bbox", "170, 10,-170,20"),
        ]))
        .unwrap();
        let bbox = query.bbox.unwrap();
        assert_eq!(
            (bbox.west, bbox.south, bbox.east, bbox.north),
            (170.0, 10.0, -170.0, 20.0)
        );
        assert!(bbox.crosses_antimeridian());
        assert!(matches!(
            query.datetime,
            Some(DatetimeSelector::Window(_, end)) if end == DateTime::<Utc>::MAX_UTC
        ));
        assert_eq!(
            query.href("b", 2),
            "b?datetime=2026-01-01T00:00:00Z/..&bbox=170,%2010,-170,20&limit=2&offset=2"
        );
        // Six numbers: the vertical pair is checked, then dropped.
        let six = parse_locations_query(pairs(&[("bbox", "20,55,1000,30,65,0")]))
            .unwrap()
            .bbox
            .unwrap();
        assert_eq!(
            (six.west, six.south, six.east, six.north),
            (20.0, 55.0, 30.0, 65.0)
        );
        // The list form is the data queries' list.
        let list = parse_locations_query(pairs(&[(
            "datetime",
            "2026-01-01T06:00:00Z,2026-01-01T00:00:00Z",
        )]))
        .unwrap();
        assert!(matches!(list.datetime, Some(DatetimeSelector::Instants(ref v)) if v.len() == 2));

        for (bad, names) in [
            (&[("bbox", "1,2,3")][..], "Invalid bbox"),
            (&[("bbox", "1,2,3,4,5")], "Invalid bbox"),
            (&[("bbox", "a,0,1,1")], "Invalid bbox"),
            (&[("bbox", "0,0,1,1,NaN,2")], "Invalid bbox"),
            (&[("bbox", "")], "Invalid bbox"),
            (&[("bbox", "0,60,1,50")], "Invalid bbox"),
            (&[("bbox", "190,0,200,1")], "Invalid bbox"),
            (&[("bbox", "0,0,1,1"), ("bbox", "0,0,2,2")], "'bbox'"),
            (&[("datetime", "yesterday")], "datetime"),
            (&[("datetime", "")], "datetime"),
            (
                &[("datetime", "2026-01-01T00:00:00Z,2026-01-02T00:00:00Z/..")],
                "datetime",
            ),
            (&[("datetime", ".."), ("limit", "2")], "datetime"),
            (
                &[
                    ("datetime", "2026-01-01T00:00:00Z"),
                    ("datetime", "2026-01-02T00:00:00Z"),
                ],
                "'datetime'",
            ),
        ] {
            let err = parse_locations_query(pairs(bad)).unwrap_err().to_string();
            assert!(err.contains(names), "{bad:?}: {err}");
        }
    }

    #[test]
    fn within_units_convert_and_validate() {
        assert_eq!(parse_within_metres("10", "km").unwrap(), 10_000.0);
        assert_eq!(parse_within_metres("250", "M").unwrap(), 250.0);
        assert!((parse_within_metres("1", "mi").unwrap() - 1_609.344).abs() < 1e-9);
        assert!(parse_within_metres("abc", "km").is_err());
        assert!(parse_within_metres("0", "km").is_err());
        assert!(parse_within_metres("-3", "km").is_err());
        assert!(parse_within_metres("inf", "km").is_err());
        assert!(parse_within_metres("10", "furlong").is_err());
        assert!(parse_within_metres("1001", "km").is_err());
        assert!(parse_within_metres("1000", "km").is_ok());
    }

    /// `crs_details` promises CRS84, whose first axis is longitude: the WKT
    /// must say so, or a client that reads it swaps every coordinate. The
    /// standard's own example WKT is EPSG:4326's, latitude first.
    #[test]
    fn crs84_wkt_is_longitude_first() {
        let lon = CRS84_WKT
            .find(r#"AXIS["geodetic longitude (Lon)",east,ORDER[1]"#)
            .expect("longitude axis, order 1");
        let lat = CRS84_WKT
            .find(r#"AXIS["geodetic latitude (Lat)",north,ORDER[2]"#)
            .expect("latitude axis, order 2");
        assert!(lon < lat);
        assert!(
            CRS84_WKT.starts_with(r#"GEOGCRS["WGS 84 (CRS84)",DATUM["World Geodetic System 1984""#)
        );
        assert!(CRS84_WKT.ends_with(r#"ID["OGC","CRS84"]]"#));
        assert!(!CRS84_WKT.contains("4326"));
        // Brackets balance and never close more than they opened.
        let depth = CRS84_WKT.chars().try_fold(0i32, |d, c| {
            let d = d + i32::from(c == '[') - i32::from(c == ']');
            (d >= 0).then_some(d)
        });
        assert_eq!(depth, Some(0));
    }
}
