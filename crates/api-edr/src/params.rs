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

/// Clamp the requested plot dimensions to a sane range, defaulting to 800×600.
/// The upper bound is intentionally modest — the plot renders synchronously on
/// the request worker, so the worst-case buffer stays small.
pub fn plot_dimensions(width: Option<u32>, height: Option<u32>) -> (u32, u32) {
    (
        width.unwrap_or(800).clamp(160, 2000),
        height.unwrap_or(600).clamp(120, 2000),
    )
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

/// Parse the EDR `z` query parameter — a comma-separated list of numeric
/// vertical levels (e.g. `z=850,700,500` or a single `z=0.5`). An absent
/// or blank value yields `None` (the whole vertical extent / a profile).
///
/// The EDR `min/max` interval form is not supported — pass the discrete
/// levels explicitly (the available set is advertised in the collection's
/// vertical extent).
pub fn parse_z(z: Option<&str>) -> Result<Option<Vec<f64>>, DataServerError> {
    let Some(raw) = z.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let levels: Vec<f64> = raw
        .split(',')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return Err(DataServerError::InvalidParameter(
                    "`z` has an empty element — check for a stray comma".into(),
                ));
            }
            // `parse::<f64>()` also accepts "inf"/"nan"; reject those so a
            // non-finite level can't reach `quantize_z` (→ `i64::MAX`
            // cache aliasing) or `nearest_sweep` (NaN distance comparisons).
            part.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!(
                        "Invalid `z` value '{part}' — expected a finite number"
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    Ok((!levels.is_empty()).then_some(levels))
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
