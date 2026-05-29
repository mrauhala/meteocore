use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::MapsError;

/// Maximum map pixels (width * height). 64M matches the engine-geotiff cap so
/// requests that pass API validation never get rejected further down the stack.
pub const MAX_MAP_PIXELS: u64 = 64_000_000;

/// Maximum single dimension (width or height). 8000 chosen so 8000 × 8000
/// equals MAX_MAP_PIXELS — a square at the per-dim cap doesn't trip the
/// pixel cap with a confusing second error.
pub const MAX_MAP_DIMENSION: u32 = 8000;

/// Supported CRS identifiers.
const SUPPORTED_CRS: &[&str] = &["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"];

/// Supported output formats.
///
/// `image/png` auto-selects an 8-bit indexed-palette encoding ("PNG8") when
/// the rendered image carries ≤256 distinct colours (every colormap layer);
/// the encoder falls back to 32-bit RGBA above that. Content-type is
/// `image/png` either way — clients can't tell, and no second `f=` value is
/// needed.
const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

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
    /// EDR-style parameter selector for multi-parameter raster engines
    /// (GRIB, multi-param QueryData). Non-OGC for now — a standardised
    /// path/query form is on the OGC Maps roadmap and will replace this.
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    /// Vertical level selector (e.g. a radar elevation angle). Rejected
    /// with HTTP 400 for collections with no vertical dimension.
    pub elevation: Option<String>,
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
    pub parameter_name: Option<String>,
    /// Vertical level, parsed from the `elevation` query parameter.
    pub z: Option<f64>,
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
            "image/webp" => ds_render::ImageFormat::Webp,
            _ => ds_render::ImageFormat::Png,
        };

        // DATETIME / TIME
        let time = self.datetime.as_deref().map(parse_time).transpose()?;

        // OGC API Maps fixes `bbox-crs` to CRS:84 (checked above), so `bbox` is
        // always WGS84 degrees; the `crs` parameter selects the *output* CRS.
        // For a projected output CRS the map frame must cover the requested
        // geographic box, so forward-project it to a projected envelope, render
        // the projection over that, and widen the WGS84 read window to the
        // envelope's inverse so the engine reads the right source pixels
        // (#160 — previously these codes silently rendered as WGS84).
        let (bbox, output_crs) = match crs.as_str() {
            "EPSG:3857" => (bbox, ds_core::map_engine::OutputCrs::WebMercator),
            "EPSG:3067" | "EPSG:3035" => {
                let proj_crs = ds_core::geo::projected_output_crs(&crs).ok_or_else(|| {
                    MapsError::BadRequest(format!("CRS '{crs}' has no projection definition"))
                })?;
                let proj_bbox = ds_core::geo::projected_envelope(&proj_crs, bbox);
                // None means the projected frame is entirely outside the CRS's
                // valid domain — reject (400) rather than reading a global window.
                let wgs84 =
                    ds_core::geo::wgs84_envelope(&proj_crs, proj_bbox).ok_or_else(|| {
                        MapsError::BadRequest(
                            "bbox is outside the valid area of the requested crs".to_string(),
                        )
                    })?;
                (
                    wgs84,
                    ds_core::map_engine::OutputCrs::Projected {
                        crs: proj_crs,
                        bbox: proj_bbox,
                    },
                )
            }
            _ => (bbox, ds_core::map_engine::OutputCrs::Wgs84),
        };

        // parameter-name — validation that the name is in the engine's list
        // happens in the handler (we don't have the engine here). Just trim and
        // reject empty/blank to keep handler logic simple.
        let parameter_name = self
            .parameter_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // ELEVATION — a single vertical level. Multi-value selection is an
        // EDR concern; a map renders exactly one layer.
        let z = match self
            .elevation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| {
                        MapsError::BadRequest(format!("elevation '{raw}' is not a finite number"))
                    })?,
            ),
            None => None,
        };

        Ok(ValidatedMapParams {
            bbox,
            width,
            height,
            crs,
            time,
            output_crs,
            format,
            parameter_name,
            z,
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

    fn query_with_crs(crs: &str) -> MapQueryParams {
        MapQueryParams {
            // CRS:84 bbox over Finland (bbox-crs is always CRS:84 in OGC Maps).
            bbox: Some("19,59,32,70".to_string()),
            width: Some(256),
            height: Some(256),
            crs: Some(crs.to_string()),
            datetime: None,
            transparent: None,
            format: None,
            bbox_crs: None,
            parameter_name: None,
            elevation: None,
        }
    }

    #[test]
    fn validate_projected_output_crs_3067() {
        // #160: a projected output CRS must produce OutputCrs::Projected, not a
        // silent Wgs84 fallback. The bbox stays CRS:84 (bbox-crs), but the
        // engine read window widens to the projected frame's WGS84 envelope.
        let validated = query_with_crs("EPSG:3067").validate().unwrap();
        match validated.output_crs {
            ds_core::map_engine::OutputCrs::Projected { ref crs, bbox } => {
                assert!(matches!(crs, ds_core::geo::Crs::TransverseMercator { .. }));
                // Projected envelope of the CRS:84 box: easting near the 500 km
                // false-easting band, northing in the millions of metres.
                assert!(bbox[1] > 5_000_000.0 && bbox[3] > 6_000_000.0, "{bbox:?}");
            }
            other => panic!("expected Projected, got {other:?}"),
        }
        // The read window stays in plausible WGS84 degrees.
        let [w, s, e, n] = validated.bbox;
        assert!(
            w > 10.0 && e < 40.0 && s > 55.0 && n < 75.0,
            "{:?}",
            validated.bbox
        );
    }

    #[test]
    fn validate_wgs84_and_webmercator_unchanged() {
        assert_eq!(
            query_with_crs("CRS:84").validate().unwrap().output_crs,
            ds_core::map_engine::OutputCrs::Wgs84
        );
        assert_eq!(
            query_with_crs("EPSG:3857").validate().unwrap().output_crs,
            ds_core::map_engine::OutputCrs::WebMercator
        );
    }
}
