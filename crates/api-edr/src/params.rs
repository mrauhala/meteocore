use ds_core::error::DataServerError;
use serde::Deserialize;

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
}

/// EDR response output format selected by the `f` query parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdrFormat {
    /// OGC CoverageJSON (the default).
    CoverageJson,
    /// A rendered PNG plot (vertical profile or time series).
    Png,
}

/// Parse the `f` query parameter. Absent/blank → CoverageJSON. `coveragejson`
/// and `png` are accepted case-insensitively; anything else is a 400.
pub fn parse_edr_format(f: Option<&str>) -> Result<EdrFormat, DataServerError> {
    match f.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(EdrFormat::CoverageJson),
        Some(s) if s.eq_ignore_ascii_case("coveragejson") => Ok(EdrFormat::CoverageJson),
        Some(s) if s.eq_ignore_ascii_case("png") => Ok(EdrFormat::Png),
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

/// Split a position-query `coords` value into one or more `POINT(lon lat)` WKT
/// strings. Accepts either a single `POINT(lon lat)` or a
/// `MULTIPOINT((lon lat),(lon lat),...)` (nested form) /
/// `MULTIPOINT(lon lat, lon lat, ...)` (flat form). The returned strings are
/// always normalized to `POINT(lon lat)` so that existing engine
/// `query_position` implementations can be reused unchanged.
pub fn split_position_coords(coords: &str) -> Result<Vec<String>, DataServerError> {
    let trimmed = coords.trim();

    // POINT(lon lat) — single point, passed through unchanged.
    if starts_with_ignore_ascii_case(trimmed, "POINT") {
        return Ok(vec![trimmed.to_string()]);
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
                coords[0].parse::<f64>().map_err(|_| {
                    DataServerError::InvalidParameter(format!(
                        "MULTIPOINT element '{part}' has invalid longitude"
                    ))
                })?;
                coords[1].parse::<f64>().map_err(|_| {
                    DataServerError::InvalidParameter(format!(
                        "MULTIPOINT element '{part}' has invalid latitude"
                    ))
                })?;
                Ok(format!("POINT({} {})", coords[0], coords[1]))
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
    fn point_case_insensitive() {
        let points = split_position_coords("point(24.94 60.17)").unwrap();
        assert_eq!(points, vec!["point(24.94 60.17)".to_string()]);
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
}
