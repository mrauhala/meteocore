use ds_core::error::DataServerError;
use serde::Deserialize;

use crate::response::{COVERAGE_JSON_MEDIA_TYPE, LEGACY_COVERAGE_JSON_MEDIA_TYPE};

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
    /// Output format: `CoverageJSON` (default) or `PNG` (plot).
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
    /// Output format: `CoverageJSON` (default) or `PNG` (plot).
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
    /// A rendered PNG plot (vertical profile or time series).
    Png,
}

/// Parse the `f` query parameter. Absent/blank → CoverageJSON. `coveragejson`
/// and `png` are accepted case-insensitively, and so are their media types
/// (#510): `application/vnd.cov+json` (what the responses carry, EDR 1.2,
/// #920), `application/prs.coverage+json` (EDR 1.1's type, still accepted;
/// the response is the same CoverageJSON under the 1.2 type) and
/// `image/png`. A `+` sent unencoded arrives as a space, so a space inside
/// a media type reads as `+`. Anything else is a 400.
pub fn parse_edr_format(f: Option<&str>) -> Result<EdrFormat, DataServerError> {
    let f = f
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase().replace(' ', "+"));
    match f.as_deref() {
        None => Ok(EdrFormat::CoverageJson),
        Some("coveragejson" | COVERAGE_JSON_MEDIA_TYPE | LEGACY_COVERAGE_JSON_MEDIA_TYPE) => {
            Ok(EdrFormat::CoverageJson)
        }
        Some("png" | "image/png") => Ok(EdrFormat::Png),
        Some(other) => Err(DataServerError::InvalidParameter(format!(
            "Unsupported output format '{other}' — expected 'CoverageJSON' or 'PNG'"
        ))),
    }
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

/// A `/locations` request made with `limit`: one page of the inventory
/// (EDR 1.2 locations paging, #922). Without `limit` there is no paging and
/// the complete inventory is served as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationsPaging {
    /// Resolved page size (clamped to [`MAX_LIMIT`]); links carry this value.
    pub limit: usize,
    pub offset: usize,
    /// Every other query pair, in request order, repeated verbatim in the
    /// page's navigation links (so an `f`, say, survives paging).
    pub preserved: Vec<(String, String)>,
}

impl LocationsPaging {
    /// `base` plus this request's query at `offset`: the preserved pairs,
    /// then the resolved `limit`, then `offset` (omitted when 0), as on
    /// `/collections`.
    pub fn href(&self, base: &str, offset: usize) -> String {
        use ds_core::collection_search::encode_query_value as enc;
        let mut query: Vec<String> = self
            .preserved
            .iter()
            .map(|(name, value)| format!("{}={}", enc(name), enc(value)))
            .collect();
        query.push(format!("limit={}", self.limit));
        if offset > 0 {
            query.push(format!("offset={offset}"));
        }
        format!("{base}?{}", query.join("&"))
    }
}

/// Query parameters `/locations` accepts. `bbox` and `datetime` are the
/// list's EDR filters: accepted but not applied yet (#932), and repeated in
/// paging links; `f` selects the representation.
pub const LOCATIONS_PARAMETERS: [&str; 5] = ["limit", "offset", "bbox", "datetime", "f"];

/// Split the `/locations` query into its paging request. `Ok(None)` when no
/// `limit` is given: the complete inventory. A repeated `limit`/`offset`, an
/// invalid value, an `offset` without a `limit` (there is no page to offset
/// into), or a parameter outside [`LOCATIONS_PARAMETERS`] is a 400, so a typo
/// such as `limti` cannot return the unpaged list as if it worked (#605).
/// The other accepted parameters are kept for the links.
pub fn parse_locations_paging(
    pairs: Vec<(String, String)>,
) -> Result<Option<LocationsPaging>, DataServerError> {
    let (mut limit, mut offset) = (None, None);
    let mut preserved = Vec::new();
    for (name, value) in pairs {
        let slot = match name.as_str() {
            "limit" => &mut limit,
            "offset" => &mut offset,
            known if LOCATIONS_PARAMETERS.contains(&known) => {
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
        if slot.replace(value).is_some() {
            return Err(DataServerError::InvalidParameter(format!(
                "Duplicate query parameter '{name}'"
            )));
        }
    }
    let resolved_offset = parse_offset(offset.as_deref())?;
    match parse_limit(limit.as_deref())? {
        Some(limit) => Ok(Some(LocationsPaging {
            limit,
            offset: resolved_offset,
            preserved,
        })),
        None if offset.is_some_and(|o| !o.trim().is_empty()) => {
            Err(DataServerError::InvalidParameter(
                "offset pages the location list and requires limit".into(),
            ))
        }
        None => Ok(None),
    }
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
    /// rejected (an area result is gridded / multi-coverage, not a single plot).
    pub f: Option<String>,
    /// EDR 1.2 `limit` on top-level coverages; see [`parse_limit`].
    pub limit: Option<String>,
}

/// Radius query parameters (OGC API - EDR 1.1 `radius`): everything
/// within `within` `within-units` of a WKT `POINT`. Both distance
/// parameters are required by the spec's OpenAPI; the accepted units are
/// [`WITHIN_UNITS`]. Like area, the result is multi-coverage / gridded, so
/// `PNG` is rejected.
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
/// CRS84 only; `minx > maxx` crosses the antimeridian.
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

/// Trajectory (vertical cross-section) query parameters. Accepts a WKT
/// `LINESTRING(lon lat, lon lat, …)` and the standard EDR filters; `z`
/// selects *elevation angles* from the collection's advertised vertical
/// extent (a list or a `min/max` interval), bounding which sweeps build
/// the cross-section — whose own axis is derived height. The corridor
/// variant (`corridor-width` / `corridor-height`) ships in a follow-up.
#[derive(Debug, Deserialize)]
pub struct TrajectoryQueryParams {
    pub coords: String,
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    pub z: Option<String>,
    /// Output format: `CoverageJSON` (default) or `PNG` — a colour-mapped
    /// cross-section heatmap (distance × height).
    pub f: Option<String>,
    /// PNG image dimensions (ignored for CoverageJSON).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Not supported on trajectory, which EDR 1.2 gives no `limit`: read
    /// only so a request carrying it is a 400, not a silently unlimited 200.
    pub limit: Option<String>,
}

/// A parsed EDR `z` selector: either an explicit list of levels or a
/// closed `min/max` interval. The interval is resolved against the
/// collection's advertised vertical levels at the handler boundary (see
/// [`resolve_z_levels`]) so engines keep their `Option<&[f64]>` contract.
#[derive(Debug, Clone, PartialEq)]
pub enum ZSelector {
    /// Discrete levels (`z=0.5` or `z=850,700,500`).
    Levels(Vec<f64>),
    /// A closed interval `z=min/max` (OGC EDR interval form).
    Interval { min: f64, max: f64 },
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

/// Parse the EDR `z` query parameter. Accepts a comma-separated list of
/// numeric levels (`z=850,700,500` / a single `z=0.5`) **or** the OGC
/// `min/max` interval form (`z=850/500`, order-independent). An absent or
/// blank value yields `None` (the whole vertical extent / a profile).
pub fn parse_z(z: Option<&str>) -> Result<Option<ZSelector>, DataServerError> {
    let Some(raw) = z.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    // Interval form `min/max` — exactly one slash, two finite endpoints.
    if raw.contains('/') {
        let parts: Vec<&str> = raw.split('/').collect();
        if parts.len() != 2 {
            return Err(DataServerError::InvalidParameter(
                "`z` interval must be `min/max` (one slash, two values)".into(),
            ));
        }
        let a = parse_z_value(parts[0])?;
        let b = parse_z_value(parts[1])?;
        let (min, max) = if a <= b { (a, b) } else { (b, a) };
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

/// Resolve a [`ZSelector`] into the concrete level list an engine samples.
///
/// - `Levels` pass through unchanged (the engine snaps each to its nearest
///   available level).
/// - `Interval { min, max }` expands to the collection's advertised levels
///   that fall within `[min, max]` (inclusive). An interval that selects no
///   advertised level is a 400 — the caller asked for a band the collection
///   doesn't cover.
///
/// `extent` is the collection's advertised vertical levels; it must be
/// present for an interval (callers gate `z` against a missing vertical
/// dimension first).
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
                    "`z` interval {min}/{max} selects none of the collection's \
                     available levels"
                )));
            }
            Ok(selected)
        }
    }
}

/// Limits apply to decoded coordinates, before per-point allocations/queries.
pub const MAX_POSITION_COORD_BYTES: usize = 16 * 1024;
pub const MAX_POSITION_POINTS: usize = 64;
/// Combined position response budget, including every point and parameter.
pub const MAX_POSITION_VALUES: usize = 1_000_000;

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
        for f in ["json", "application/json", "image/jpeg"] {
            assert!(parse_edr_format(Some(f)).is_err(), "{f}");
        }
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

    /// An unknown parameter is a 400 naming the valid ones; the accepted
    /// filters pass through to the links (#605, #932).
    #[test]
    fn locations_paging_rejects_unknown_parameters() {
        let pairs = |q: &[(&str, &str)]| {
            q.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        let err = parse_locations_paging(pairs(&[("limti", "5")])).unwrap_err();
        assert!(err.to_string().contains("limti"), "{err}");
        assert!(err.to_string().contains("limit, offset"), "{err}");
        assert!(parse_locations_paging(pairs(&[("sortby", "id")])).is_err());
        assert_eq!(
            parse_locations_paging(pairs(&[("bbox", "0,0,1,1"), ("datetime", "..")])).unwrap(),
            None
        );
        let page = parse_locations_paging(pairs(&[("bbox", "0,0,1,1"), ("limit", "2")]))
            .unwrap()
            .unwrap();
        assert_eq!(page.limit, 2);
    }

    #[test]
    fn locations_paging_splits_query() {
        let pairs = |q: &[(&str, &str)]| {
            q.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        // No limit: the complete inventory, other parameters untouched.
        assert_eq!(
            parse_locations_paging(pairs(&[("f", "json")])).unwrap(),
            None
        );
        assert_eq!(
            parse_locations_paging(pairs(&[("limit", "")])).unwrap(),
            None
        );
        let page = parse_locations_paging(pairs(&[
            ("f", "geo json"),
            ("limit", "50000"),
            ("offset", "20"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!((page.limit, page.offset), (MAX_LIMIT, 20));
        // Links repeat the other parameters, encoded, and the clamped limit.
        assert_eq!(
            page.href("https://x/locations", 10020),
            "https://x/locations?f=geo%20json&limit=10000&offset=10020"
        );
        assert_eq!(page.href("b", 0), "b?f=geo%20json&limit=10000");
        for bad in [
            &[("offset", "3")][..],
            &[("limit", "0")],
            &[("limit", "2"), ("offset", "-1")],
            &[("limit", "2"), ("limit", "3")],
            &[("limit", "2"), ("offset", "1"), ("offset", "1")],
        ] {
            assert!(parse_locations_paging(pairs(bad)).is_err(), "{bad:?}");
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
