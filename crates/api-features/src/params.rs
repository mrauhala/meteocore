use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, DatetimeInterval};
use serde::Deserialize;

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct ItemsQueryParams {
    pub bbox: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub datetime: Option<String>,
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
