use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, DatetimeInterval, SortDirection, SortKey};
use serde::Deserialize;

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct ItemsQueryParams {
    pub bbox: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub datetime: Option<String>,
    pub sortby: Option<String>,
}

/// Parse an OGC API – Features Part 8 `sortby` value.
///
/// Comma-separated `[+|-]?<property>`; `+` (or no prefix) is ascending, `-` is
/// descending. Validated against the collection's advertised `sortables`, so an
/// unknown or unsupported property is a 400 rather than a silently ignored
/// parameter — which is the behaviour this replaces.
///
/// **A literal `+` in a query string decodes to a space**, so `sortby=+id`
/// arrives here as `" id"`. A leading space is therefore treated as the
/// ascending marker it actually is; clients that percent-encode it as `%2B`
/// (as the spec's own example does) land on the same result.
pub fn parse_sortby(s: &str, sortables: &[&str]) -> Result<Vec<SortKey>, DataServerError> {
    let invalid = |msg: String| DataServerError::InvalidParameter(msg);
    let valid_list = || sortables.join(", ");

    let mut keys: Vec<SortKey> = Vec::new();
    for raw in s.split(',') {
        // Trim BEFORE reading the direction marker. A decoded `+` arrives as
        // a leading space, but so does cosmetic whitespace after a comma
        // (`sortby=score, -size`, or `%20` from a client that encodes it) —
        // reading the marker first would see the space, take the ascending
        // branch, and then reject the legitimate `-size` as a malformed
        // property name. Trimming first makes both cases fall out: with the
        // space gone, a bare property is ascending by default anyway.
        let trimmed = raw.trim();
        let (direction, rest) = match trimmed.strip_prefix('-') {
            Some(rest) => (SortDirection::Descending, rest),
            None => (
                SortDirection::Ascending,
                trimmed.strip_prefix('+').unwrap_or(trimmed),
            ),
        };
        let property = rest.trim();
        if property.is_empty() {
            return Err(invalid(format!(
                "sortby: empty sort term in '{s}' (expected [+|-]<property>, comma-separated)"
            )));
        }
        // Part 8 pattern `[+|-]?[A-Za-z_].*`.
        if !property
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return Err(invalid(format!(
                "sortby: property '{property}' must start with a letter or underscore"
            )));
        }
        if !sortables.contains(&property) {
            return Err(invalid(if sortables.is_empty() {
                "sortby: this collection does not support sorting".to_string()
            } else {
                format!(
                    "sortby: unknown sort property '{property}' (valid: {})",
                    valid_list()
                )
            }));
        }
        if keys.iter().any(|k| k.property == property) {
            return Err(invalid(format!(
                "sortby: property '{property}' listed more than once"
            )));
        }
        keys.push(SortKey {
            property: property.to_string(),
            direction,
        });
    }
    if keys.is_empty() {
        return Err(invalid(
            "sortby: at least one sort property is required".into(),
        ));
    }
    Ok(keys)
}

pub fn parse_bbox(s: &str) -> Result<Bbox, DataServerError> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 && parts.len() != 6 {
        return Err(DataServerError::InvalidBbox(
            "bbox must have 4 values (2D) or 6 values (3D): west,south,east,north[,min-height,max-height]".into(),
        ));
    }

    let values: Vec<f64> = parts
        .iter()
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .map_err(|e| DataServerError::InvalidBbox(format!("invalid number: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // For 6-value bbox, ignore height dimensions (indices 2,5 in raw order are heights)
    // OGC format: west, south, [min-height,] east, north [,max-height]
    // Actually the 6-value format is: west, south, min-height, east, north, max-height
    let (west, south, east, north) = if values.len() == 6 {
        (values[0], values[1], values[3], values[4])
    } else {
        (values[0], values[1], values[2], values[3])
    };

    Bbox::new(west, south, east, north).map_err(DataServerError::InvalidBbox)
}

/// Parse an OGC datetime parameter value.
/// Supports: instant ("2024-01-01T00:00:00Z"), interval ("start/end"),
/// and open intervals ("../end", "start/..", "../..").
pub fn parse_datetime(s: &str) -> Result<DatetimeInterval, DataServerError> {
    if let Some((start_str, end_str)) = s.split_once('/') {
        let start = if start_str == ".." {
            None
        } else {
            Some(
                start_str
                    .parse()
                    .map_err(|e| DataServerError::InvalidDatetime(format!("{start_str}: {e}")))?,
            )
        };
        let end = if end_str == ".." {
            None
        } else {
            Some(
                end_str
                    .parse()
                    .map_err(|e| DataServerError::InvalidDatetime(format!("{end_str}: {e}")))?,
            )
        };
        Ok(DatetimeInterval { start, end })
    } else {
        let instant = s
            .parse()
            .map_err(|e| DataServerError::InvalidDatetime(format!("{s}: {e}")))?;
        Ok(DatetimeInterval {
            start: Some(instant),
            end: Some(instant),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_bbox() {
        let bbox = parse_bbox("24.0,60.0,25.0,61.0").unwrap();
        assert!((bbox.west - 24.0).abs() < f64::EPSILON);
        assert!((bbox.north - 61.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_bbox_with_spaces() {
        let bbox = parse_bbox("24.0, 60.0, 25.0, 61.0").unwrap();
        assert!((bbox.west - 24.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_bbox_6_values() {
        let bbox = parse_bbox("24.0,60.0,0.0,25.0,61.0,100.0").unwrap();
        assert!((bbox.west - 24.0).abs() < f64::EPSILON);
        assert!((bbox.east - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_bbox_wrong_count() {
        assert!(parse_bbox("24.0,60.0,25.0").is_err());
        assert!(parse_bbox("24.0,60.0,25.0,61.0,70.0").is_err()); // 5 values invalid
    }

    #[test]
    fn parse_bbox_nan() {
        assert!(parse_bbox("NaN,60.0,25.0,61.0").is_err());
    }

    #[test]
    fn parse_bbox_not_a_number() {
        assert!(parse_bbox("abc,60.0,25.0,61.0").is_err());
    }

    #[test]
    fn parse_bbox_antimeridian() {
        // west > east is valid — indicates antimeridian-crossing bbox
        let bbox = parse_bbox("170.0,-10.0,-170.0,10.0").unwrap();
        assert!(bbox.crosses_antimeridian());
    }

    #[test]
    fn parse_bbox_reversed_lat() {
        // south > north is still invalid
        assert!(parse_bbox("24.0,61.0,25.0,60.0").is_err());
    }

    #[test]
    fn parse_datetime_instant() {
        let dt = parse_datetime("2024-01-01T00:00:00Z").unwrap();
        assert!(dt.start.is_some());
        assert_eq!(dt.start, dt.end);
    }

    #[test]
    fn parse_datetime_interval() {
        let dt = parse_datetime("2024-01-01T00:00:00Z/2024-01-02T00:00:00Z").unwrap();
        assert!(dt.start.is_some());
        assert!(dt.end.is_some());
        assert!(dt.start.unwrap() < dt.end.unwrap());
    }

    #[test]
    fn parse_datetime_open_start() {
        let dt = parse_datetime("../2024-01-01T00:00:00Z").unwrap();
        assert!(dt.start.is_none());
        assert!(dt.end.is_some());
    }

    #[test]
    fn parse_datetime_open_end() {
        let dt = parse_datetime("2024-01-01T00:00:00Z/..").unwrap();
        assert!(dt.start.is_some());
        assert!(dt.end.is_none());
    }

    #[test]
    fn parse_datetime_fully_open() {
        let dt = parse_datetime("../..").unwrap();
        assert!(dt.start.is_none());
        assert!(dt.end.is_none());
    }

    #[test]
    fn parse_datetime_invalid() {
        assert!(parse_datetime("not-a-date").is_err());
    }
}
