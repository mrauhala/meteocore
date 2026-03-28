use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::MapsError;

/// Maximum map pixels (width * height). 8 million = 2828x2828.
pub const MAX_MAP_PIXELS: u64 = 8_000_000;

/// Maximum single dimension (width or height).
pub const MAX_MAP_DIMENSION: u32 = 4096;

/// Supported CRS identifiers.
const SUPPORTED_CRS: &[&str] = &["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"];

/// Supported output formats.
const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg"];

/// Query parameters for OGC API Maps get_map / get_styled_map endpoints.
#[derive(Debug, Deserialize)]
pub struct MapQueryParams {
    pub bbox: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub crs: Option<String>,
    pub datetime: Option<String>,
    pub transparent: Option<String>,
    #[serde(rename = "f")]
    pub format: Option<String>,
    #[serde(rename = "bbox-crs")]
    pub bbox_crs: Option<String>,
}

/// Validated map request parameters.
pub struct ValidatedMapParams {
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    pub crs: String,
    pub time: Option<DateTime<Utc>>,
    pub output_crs: ds_core::map_engine::OutputCrs,
    pub format: ds_render::ImageFormat,
}

impl MapQueryParams {
    /// Validate and extract map parameters, applying defaults.
    pub fn validate(&self) -> Result<ValidatedMapParams, MapsError> {
        // BBOX-CRS — only CRS:84 supported
        if let Some(ref bbox_crs) = self.bbox_crs {
            let normalized = bbox_crs.trim();
            if normalized != "CRS:84"
                && normalized != "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
            {
                return Err(MapsError::BadRequest(format!(
                    "bbox-crs '{bbox_crs}' is not supported. Only CRS:84 is supported for bbox coordinates."
                )));
            }
        }

        // BBOX — required
        let bbox_str = self
            .bbox
            .as_deref()
            .ok_or_else(|| MapsError::BadRequest("Missing required parameter: bbox".into()))?;
        let bbox = parse_bbox(bbox_str)?;

        // WIDTH — default 256
        let width = self.width.unwrap_or(256);

        // HEIGHT — default 256
        let height = self.height.unwrap_or(256);

        // Validate dimensions
        if width == 0 || height == 0 {
            return Err(MapsError::BadRequest(
                "width and height must be greater than 0".into(),
            ));
        }
        if width > MAX_MAP_DIMENSION || height > MAX_MAP_DIMENSION {
            return Err(MapsError::BadRequest(format!(
                "width and height must not exceed {MAX_MAP_DIMENSION}"
            )));
        }
        if (width as u64) * (height as u64) > MAX_MAP_PIXELS {
            return Err(MapsError::BadRequest(format!(
                "width * height ({}) exceeds maximum of {MAX_MAP_PIXELS}",
                width as u64 * height as u64
            )));
        }

        // CRS — default CRS:84
        let crs = self.crs.as_deref().unwrap_or("CRS:84");
        if !SUPPORTED_CRS.contains(&crs) {
            return Err(MapsError::BadRequest(format!(
                "CRS '{crs}' is not supported. Supported: {}",
                SUPPORTED_CRS.join(", ")
            )));
        }
        let crs = crs.to_string();

        // FORMAT — default image/png
        let format_str = self.format.as_deref().unwrap_or("image/png");
        if !SUPPORTED_FORMATS.contains(&format_str) {
            return Err(MapsError::BadRequest(format!(
                "Format '{format_str}' is not supported. Supported: {}",
                SUPPORTED_FORMATS.join(", ")
            )));
        }
        let format = match format_str {
            "image/jpeg" => ds_render::ImageFormat::Jpeg,
            _ => ds_render::ImageFormat::Png,
        };

        // DATETIME / TIME
        let time = self.datetime.as_deref().map(parse_time).transpose()?;

        let output_crs = match crs.as_str() {
            "EPSG:3857" => ds_core::map_engine::OutputCrs::WebMercator,
            _ => ds_core::map_engine::OutputCrs::Wgs84,
        };

        Ok(ValidatedMapParams {
            bbox,
            width,
            height,
            crs,
            time,
            output_crs,
            format,
        })
    }
}

/// Parse bbox string. OGC API Maps always uses lon/lat order (west,south,east,north).
fn parse_bbox(bbox_str: &str) -> Result<[f64; 4], MapsError> {
    let parts: Vec<f64> = bbox_str
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<f64>()
                .map_err(|_| MapsError::BadRequest(format!("Invalid bbox value: '{s}'")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if parts.len() != 4 {
        return Err(MapsError::BadRequest(
            "bbox must have exactly 4 values: west,south,east,north".into(),
        ));
    }

    for v in &parts {
        if !v.is_finite() {
            return Err(MapsError::BadRequest(
                "bbox values must be finite numbers".into(),
            ));
        }
    }

    let [west, south, east, north] = [parts[0], parts[1], parts[2], parts[3]];

    if west >= east || south >= north {
        return Err(MapsError::BadRequest(
            "bbox: west must be less than east, south must be less than north".into(),
        ));
    }

    Ok([west, south, east, north])
}

/// Parse an ISO 8601 timestamp for the datetime parameter.
fn parse_time(s: &str) -> Result<DateTime<Utc>, MapsError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Ok(dt.and_utc());
    }
    Err(MapsError::BadRequest(format!(
        "Cannot parse datetime '{s}' as ISO 8601"
    )))
}

/// Supported CRS list for collection metadata.
pub fn supported_crs_list() -> &'static [&'static str] {
    SUPPORTED_CRS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bbox_valid() {
        let bbox = parse_bbox("10,55,30,70").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
    }

    #[test]
    fn test_parse_bbox_invalid_count() {
        assert!(parse_bbox("10,55,30").is_err());
    }

    #[test]
    fn test_parse_bbox_invalid_order() {
        assert!(parse_bbox("30,55,10,70").is_err()); // west > east
    }

    #[test]
    fn test_parse_bbox_nan() {
        assert!(parse_bbox("NaN,0,1,1").is_err());
    }

    #[test]
    fn test_parse_time_rfc3339() {
        assert!(parse_time("2024-01-01T00:00:00Z").is_ok());
    }

    #[test]
    fn test_parse_time_invalid() {
        assert!(parse_time("not-a-time").is_err());
    }
}
