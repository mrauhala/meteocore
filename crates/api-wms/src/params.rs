use chrono::{DateTime, Utc};
use ds_core::map_engine::OutputCrs;
use serde::Deserialize;

use crate::error::WmsError;

/// Maximum map pixels (width * height). 16M = 4096x4096 (matches MAX_MAP_DIMENSION).
pub const MAX_MAP_PIXELS: u64 = 16_777_216;

/// Maximum single dimension (width or height).
pub const MAX_MAP_DIMENSION: u32 = 4096;

/// Supported CRS identifiers.
const SUPPORTED_CRS: &[&str] = &["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"];

/// Supported output formats.
const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// Raw WMS query parameters (case-insensitive keys handled by axum).
#[derive(Debug, Deserialize)]
pub struct WmsQuery {
    #[serde(alias = "SERVICE", alias = "Service")]
    pub service: Option<String>,
    #[serde(alias = "REQUEST", alias = "Request")]
    pub request: Option<String>,
    #[serde(alias = "VERSION", alias = "Version")]
    pub version: Option<String>,
    #[serde(alias = "LAYERS", alias = "Layers")]
    pub layers: Option<String>,
    #[serde(alias = "LAYER", alias = "Layer")]
    pub layer: Option<String>,
    #[serde(alias = "STYLES", alias = "Styles")]
    pub styles: Option<String>,
    #[serde(alias = "STYLE", alias = "Style")]
    pub style: Option<String>,
    #[serde(alias = "CRS", alias = "Crs")]
    pub crs: Option<String>,
    #[serde(alias = "BBOX", alias = "Bbox")]
    pub bbox: Option<String>,
    #[serde(alias = "WIDTH", alias = "Width")]
    pub width: Option<String>,
    #[serde(alias = "HEIGHT", alias = "Height")]
    pub height: Option<String>,
    #[serde(alias = "FORMAT", alias = "Format")]
    pub format: Option<String>,
    #[serde(alias = "TRANSPARENT", alias = "Transparent")]
    pub transparent: Option<String>,
    #[serde(alias = "BGCOLOR", alias = "Bgcolor")]
    pub bgcolor: Option<String>,
    #[serde(alias = "TIME", alias = "Time")]
    pub time: Option<String>,
}

/// Validated GetMap parameters.
pub struct GetMapParams {
    pub layer: String,
    pub style: String,
    pub crs: String,
    /// Bbox in WGS84 [west, south, east, north], normalized from CRS axis order.
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    pub transparent: bool,
    pub time: Option<DateTime<Utc>>,
    /// Output CRS for pixel-to-coordinate mapping.
    pub output_crs: OutputCrs,
    /// Output image format.
    pub format: ds_render::ImageFormat,
}

impl WmsQuery {
    /// Extract and validate the REQUEST type.
    pub fn request_type(&self) -> Result<WmsRequestType, WmsError> {
        let request = self
            .request
            .as_deref()
            .ok_or(WmsError::missing_parameter("REQUEST"))?;

        match request {
            "GetCapabilities" => Ok(WmsRequestType::GetCapabilities),
            "GetMap" => Ok(WmsRequestType::GetMap),
            "GetLegendGraphic" => Ok(WmsRequestType::GetLegendGraphic),
            other => Err(WmsError::operation_not_supported(other)),
        }
    }

    /// Validate and extract GetMap parameters.
    pub fn validate_get_map(&self) -> Result<GetMapParams, WmsError> {
        // VERSION must be 1.3.0
        let version = self.version.as_deref().unwrap_or("1.3.0");
        if version != "1.3.0" {
            return Err(WmsError::invalid_parameter(&format!(
                "Unsupported VERSION '{version}'. Only 1.3.0 is supported."
            )));
        }

        // LAYERS — exactly one layer (Phase 1)
        let layers_str = self
            .layers
            .as_deref()
            .ok_or(WmsError::missing_parameter("LAYERS"))?;
        let layers: Vec<&str> = layers_str.split(',').collect();
        if layers.len() != 1 {
            return Err(WmsError::invalid_parameter(
                "Exactly one LAYERS value is supported",
            ));
        }
        let layer = layers[0].to_string();

        // CRS
        let crs = self
            .crs
            .as_deref()
            .ok_or(WmsError::missing_parameter("CRS"))?;
        if !SUPPORTED_CRS.contains(&crs) {
            return Err(WmsError::invalid_crs(crs));
        }
        let crs = crs.to_string();

        // BBOX
        let bbox_str = self
            .bbox
            .as_deref()
            .ok_or(WmsError::missing_parameter("BBOX"))?;
        let bbox = parse_bbox(bbox_str, &crs)?;

        // WIDTH
        let width: u32 = self
            .width
            .as_deref()
            .ok_or(WmsError::missing_parameter("WIDTH"))?
            .parse()
            .map_err(|_| WmsError::invalid_parameter("WIDTH must be a positive integer"))?;

        // HEIGHT
        let height: u32 = self
            .height
            .as_deref()
            .ok_or(WmsError::missing_parameter("HEIGHT"))?
            .parse()
            .map_err(|_| WmsError::invalid_parameter("HEIGHT must be a positive integer"))?;

        // Validate dimensions
        if width == 0 || height == 0 {
            return Err(WmsError::invalid_parameter(
                "WIDTH and HEIGHT must be greater than 0",
            ));
        }
        if width > MAX_MAP_DIMENSION || height > MAX_MAP_DIMENSION {
            return Err(WmsError::invalid_parameter(&format!(
                "WIDTH and HEIGHT must not exceed {MAX_MAP_DIMENSION}"
            )));
        }
        if (width as u64) * (height as u64) > MAX_MAP_PIXELS {
            return Err(WmsError::invalid_parameter(&format!(
                "WIDTH * HEIGHT ({}) exceeds maximum of {MAX_MAP_PIXELS}",
                width as u64 * height as u64
            )));
        }

        // FORMAT
        let format = self.format.as_deref().unwrap_or("image/png");
        if !SUPPORTED_FORMATS.contains(&format) {
            return Err(WmsError::invalid_format(format));
        }

        // TRANSPARENT
        let transparent = self
            .transparent
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(true);

        // TIME
        let time = self.time.as_deref().map(parse_time).transpose()?;

        let output_crs = match crs.as_str() {
            "EPSG:3857" => OutputCrs::WebMercator,
            _ => OutputCrs::Wgs84,
        };

        // STYLES — empty string or missing = "default"
        let style = self
            .styles
            .as_deref()
            .or(self.style.as_deref())
            .unwrap_or("");
        let style = if style.is_empty() {
            "default".to_string()
        } else {
            style.to_string()
        };

        let image_format = match format {
            "image/jpeg" => ds_render::ImageFormat::Jpeg,
            "image/webp" => ds_render::ImageFormat::Webp,
            _ => ds_render::ImageFormat::Png,
        };

        Ok(GetMapParams {
            layer,
            style,
            crs,
            bbox,
            width,
            height,
            transparent,
            time,
            output_crs,
            format: image_format,
        })
    }
}

pub enum WmsRequestType {
    GetCapabilities,
    GetMap,
    GetLegendGraphic,
}

/// Parse BBOX string, handling WMS 1.3.0 axis order.
///
/// WMS 1.3.0 axis order depends on CRS:
/// - EPSG:4326 → lat,lon order: BBOX=south,west,north,east
/// - CRS:84    → lon,lat order: BBOX=west,south,east,north
/// - EPSG:3857 → easting,northing: not yet supported for direct passthrough
///
/// Returns [west, south, east, north] in WGS84.
fn parse_bbox(bbox_str: &str, crs: &str) -> Result<[f64; 4], WmsError> {
    let parts: Vec<f64> = bbox_str
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<f64>()
                .map_err(|_| WmsError::invalid_parameter(&format!("Invalid BBOX value: '{s}'")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if parts.len() != 4 {
        return Err(WmsError::invalid_parameter(
            "BBOX must have exactly 4 values",
        ));
    }

    // Check for NaN/Infinity
    for v in &parts {
        if !v.is_finite() {
            return Err(WmsError::invalid_parameter(
                "BBOX values must be finite numbers",
            ));
        }
    }

    // WMS 1.3.0 axis order: EPSG:4326 uses lat/lon, CRS:84 uses lon/lat
    let [x1, y1, x2, y2] = match crs {
        "EPSG:4326" => {
            // BBOX = south, west, north, east (lat/lon order) → normalize to x/y
            [parts[1], parts[0], parts[3], parts[2]]
        }
        _ => {
            // CRS:84, EPSG:3857, EPSG:3067, EPSG:3035 use x/y order
            [parts[0], parts[1], parts[2], parts[3]]
        }
    };

    // Basic validation (in source CRS units)
    if x1 >= x2 || y1 >= y2 {
        return Err(WmsError::invalid_parameter(
            "BBOX: min values must be less than max values",
        ));
    }

    // Reproject to WGS84 [west, south, east, north] if needed
    let [west, south, east, north] = match crs {
        "EPSG:3857" => {
            // Web Mercator meters → WGS84 degrees
            let (lon1, lat1) = epsg3857_to_wgs84(x1, y1);
            let (lon2, lat2) = epsg3857_to_wgs84(x2, y2);
            [lon1, lat1, lon2, lat2]
        }
        _ => {
            // CRS:84 and EPSG:4326 are already in WGS84 degrees
            // EPSG:3067 and EPSG:3035 — the engine handles reprojection internally
            // via GeoTransform::world_to_pixel which calls Crs::forward
            [x1, y1, x2, y2]
        }
    };

    Ok([west, south, east, north])
}

/// Convert EPSG:3857 (Web Mercator) coordinates to WGS84 (lon/lat degrees).
fn epsg3857_to_wgs84(x: f64, y: f64) -> (f64, f64) {
    const EARTH_RADIUS: f64 = 6_378_137.0; // WGS84 semi-major axis
    let lon = x * 180.0 / (std::f64::consts::PI * EARTH_RADIUS);
    let lat = (std::f64::consts::PI * 0.5 - 2.0 * (-y / EARTH_RADIUS).exp().atan()).to_degrees();
    (lon, lat)
}

/// Parse an ISO 8601 timestamp for the TIME parameter.
fn parse_time(s: &str) -> Result<DateTime<Utc>, WmsError> {
    // Try RFC 3339 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Try basic ISO 8601 without timezone (assume UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Ok(dt.and_utc());
    }
    Err(WmsError::InvalidDimensionValue(format!(
        "Cannot parse TIME '{s}' as ISO 8601"
    )))
}

/// Supported CRS list for GetCapabilities.
pub fn supported_crs_list() -> &'static [&'static str] {
    SUPPORTED_CRS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bbox_crs84() {
        let bbox = parse_bbox("10,55,30,70", "CRS:84").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
    }

    #[test]
    fn test_bbox_epsg4326_axis_swap() {
        // EPSG:4326 uses lat/lon order: south,west,north,east
        let bbox = parse_bbox("55,10,70,30", "EPSG:4326").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
    }

    #[test]
    fn test_bbox_invalid() {
        assert!(parse_bbox("10,55,5,70", "CRS:84").is_err()); // west > east
        assert!(parse_bbox("NaN,0,1,1", "CRS:84").is_err());
    }

    #[test]
    fn test_bbox_epsg3857_reprojection() {
        // Web Mercator bbox covering roughly lon 1.5-14.3, lat 53.0-63.0
        let bbox = parse_bbox("171318.93,6897641.62,2528475.00,9153471.98", "EPSG:3857").unwrap();
        // Should be reprojected to WGS84 degrees
        assert!((bbox[0] - 1.539).abs() < 0.01); // west ≈ 1.54°
        assert!((bbox[1] - 52.536).abs() < 0.01); // south ≈ 52.54°
        assert!((bbox[2] - 22.714).abs() < 0.01); // east ≈ 22.71°
        assert!((bbox[3] - 63.216).abs() < 0.01); // north ≈ 63.22°
    }

    #[test]
    fn test_parse_time() {
        assert!(parse_time("2024-01-01T00:00:00Z").is_ok());
        assert!(parse_time("2024-01-01T00:00:00+00:00").is_ok());
        assert!(parse_time("2024-01-01T00:00").is_ok());
        assert!(parse_time("not-a-time").is_err());
    }
}
