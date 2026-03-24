use ds_core::error::DataServerError;
use ds_core::feature::Bbox;
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
    if parts.len() != 4 {
        return Err(DataServerError::InvalidBbox(
            "bbox must have exactly 4 values: west,south,east,north".into(),
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

    Bbox::new(values[0], values[1], values[2], values[3])
        .map_err(DataServerError::InvalidBbox)
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
    fn parse_bbox_wrong_count() {
        assert!(parse_bbox("24.0,60.0,25.0").is_err());
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
    fn parse_bbox_reversed() {
        assert!(parse_bbox("25.0,60.0,24.0,61.0").is_err());
    }
}
