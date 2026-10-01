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
    /// Output format: `CoverageJSON` (default), `PNG` (plot) or, on a
    /// station collection, `GeoJSON` ([`query_formats`]).
    pub f: Option<String>,
    /// PNG plot dimensions (ignored for CoverageJSON).
    pub width: Option<u32>,
    pub height: Option<u32>,
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
}

impl EdrFormat {
    /// The `f` token, as advertised in `output_formats`.
    pub fn name(self) -> &'static str {
        match self {
            EdrFormat::CoverageJson => "CoverageJSON",
            EdrFormat::GeoJson => "GeoJSON",
            EdrFormat::Png => "PNG",
        }
    }

    /// The media type a response in this format carries.
    pub fn media_type(self) -> &'static str {
        match self {
            EdrFormat::CoverageJson => COVERAGE_JSON_MEDIA_TYPE,
            EdrFormat::GeoJson => "application/geo+json",
            EdrFormat::Png => "image/png",
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
            _ => None,
        }
    }
}

/// Parse the `f` query parameter. Absent/blank → CoverageJSON.
/// `coveragejson`, `geojson` and `png` are accepted case-insensitively, and
/// so are their media types (#510): `application/vnd.cov+json` (what the
/// responses carry, EDR 1.2, #920), `application/prs.coverage+json` (EDR
/// 1.1's type, still accepted; the response is the same CoverageJSON under
/// the 1.2 type), `application/geo+json` and `image/png`. A `+` sent
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
        Some(media) => EdrFormat::from_media_type(media).ok_or_else(|| {
            DataServerError::InvalidParameter(format!(
                "Unsupported output format '{media}' — expected 'CoverageJSON', 'GeoJSON' or 'PNG'"
            ))
        }),
    }
}

/// The output formats a data query offers, default first, in the order they
/// are advertised in `data_queries.*.link.variables.output_formats`. GeoJSON
/// is offered for the point-shaped queries (`locations`, `position`,
/// `radius`) of an engine that serves station series
/// (`EdrEngine::serves_station_series`); gridded results and `area` keep
/// CoverageJSON (#929). PNG plots a single series or profile, so area and
/// radius results are not offered as PNG.
pub fn query_formats(query_type: &str, station_series: bool) -> &'static [EdrFormat] {
    use EdrFormat::{CoverageJson, GeoJson, Png};
    match (query_type, station_series) {
        ("locations" | "position", true) => &[CoverageJson, GeoJson, Png],
        ("locations" | "position" | "trajectory", _) => &[CoverageJson, Png],
        ("radius", true) => &[CoverageJson, GeoJson],
        _ => &[CoverageJson],
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
        use EdrFormat::{CoverageJson, GeoJson, Png};
        assert_eq!(
            query_formats("position", true),
            [CoverageJson, GeoJson, Png]
        );
        assert_eq!(
            query_formats("locations", true),
            [CoverageJson, GeoJson, Png]
        );
        assert_eq!(query_formats("radius", true), [CoverageJson, GeoJson]);
        assert_eq!(query_formats("area", true), [CoverageJson]);
        for qt in ["locations", "position", "trajectory"] {
            assert_eq!(query_formats(qt, false), [CoverageJson, Png], "{qt}");
        }
        for qt in ["area", "radius", "cube"] {
            assert_eq!(query_formats(qt, false), [CoverageJson], "{qt}");
        }
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
