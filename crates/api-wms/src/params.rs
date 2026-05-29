use chrono::{DateTime, Utc};
use ds_core::map_engine::OutputCrs;
use serde::Deserialize;

use crate::error::WmsError;

/// Maximum map pixels (width * height). 64M matches the engine-geotiff cap so
/// requests that pass API validation never get rejected further down the stack.
pub const MAX_MAP_PIXELS: u64 = 64_000_000;

/// Maximum single dimension (width or height). 8000 chosen so 8000 × 8000
/// equals MAX_MAP_PIXELS — a square at the per-dim cap doesn't trip the
/// pixel cap with a confusing second error.
pub const MAX_MAP_DIMENSION: u32 = 8000;

/// Supported CRS identifiers.
const SUPPORTED_CRS: &[&str] = &["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"];

/// Supported output formats for `GetMap` and `GetLegendGraphic`.
///
/// Single source of truth: capabilities iterates this slice when emitting
/// `<Format>` children, and `parse_image_format` mirrors it. If you add a
/// format here, extend the match in `parse_image_format` to match.
///
/// `image/png` auto-selects an 8-bit indexed-palette encoding ("PNG8") when
/// the rendered image carries ≤256 distinct colours (every colormap layer);
/// the encoder falls back to 32-bit RGBA above that. Content-type is
/// `image/png` either way — no second `FORMAT=` value is needed because the
/// choice is invisible to clients.
pub const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// Parse a WMS `FORMAT=` query parameter into the corresponding `ImageFormat`.
///
/// `None` (parameter omitted) defaults to PNG, matching the WMS 1.3.0
/// convention. Any value outside `SUPPORTED_FORMATS` yields
/// `WmsError::InvalidFormat`. Used by both `validate_get_map` and the
/// `GetLegendGraphic` handler so the two paths can't drift.
pub fn parse_image_format(format: Option<&str>) -> Result<ds_render::ImageFormat, WmsError> {
    match format {
        None | Some("image/png") => Ok(ds_render::ImageFormat::Png),
        Some("image/jpeg") => Ok(ds_render::ImageFormat::Jpeg),
        Some("image/webp") => Ok(ds_render::ImageFormat::Webp),
        Some(other) => Err(WmsError::invalid_format(other)),
    }
}

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
    #[serde(alias = "ELEVATION", alias = "Elevation")]
    pub elevation: Option<String>,
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
    /// Vertical level from the WMS `ELEVATION` dimension, when supplied.
    pub elevation: Option<f64>,
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

        // BBOX — also resolves the output CRS (axis order + reprojection both
        // depend on the CRS, so they're determined together).
        let bbox_str = self
            .bbox
            .as_deref()
            .ok_or(WmsError::missing_parameter("BBOX"))?;
        let (bbox, output_crs) = parse_bbox(bbox_str, &crs)?;

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
        let image_format = parse_image_format(self.format.as_deref())?;

        // TRANSPARENT
        let transparent = self
            .transparent
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(true);

        // TIME
        let time = self.time.as_deref().map(parse_time).transpose()?;

        // ELEVATION — a single vertical level. WMS 1.3.0 permits a
        // comma-separated list, but this server renders one layer per
        // request, so a list is rejected with an explicit message.
        let elevation = match self
            .elevation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) if raw.contains(',') => {
                return Err(WmsError::invalid_parameter(
                    "ELEVATION must be a single value; comma-separated lists are not supported",
                ));
            }
            Some(raw) => Some(
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| {
                        WmsError::invalid_parameter(&format!(
                            "ELEVATION '{raw}' is not a finite number"
                        ))
                    })?,
            ),
            None => None,
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
            elevation,
        })
    }
}

pub enum WmsRequestType {
    GetCapabilities,
    GetMap,
    GetLegendGraphic,
}

/// Parse a BBOX string and resolve the output CRS, handling WMS 1.3.0 axis
/// order and reprojection.
///
/// WMS 1.3.0 axis order depends on CRS:
/// - CRS:84    → lon,lat order: BBOX=west,south,east,north
/// - EPSG:4326 → lat,lon order: BBOX=south,west,north,east (swapped)
/// - EPSG:3857/3067/3035 → easting,northing: BBOX=minx,miny,maxx,maxy
///
/// Returns the WGS84 bounding box `[west, south, east, north]` the engine reads
/// against, paired with the [`OutputCrs`] that tells the engine how output
/// pixels map to coordinates:
/// - CRS:84 / EPSG:4326 → bbox is already WGS84 degrees; [`OutputCrs::Wgs84`].
/// - EPSG:3857 → bbox metres reprojected to a WGS84 box; [`OutputCrs::WebMercator`].
/// - EPSG:3067 / EPSG:3035 → bbox is projected metres; [`OutputCrs::Projected`]
///   carries it, and the returned WGS84 box is its inverse-projected envelope so
///   the engine reads the right source window (#160/#251). Previously these two
///   codes fell through to `Wgs84` and the engine read projected metres as
///   degrees → fully-transparent tiles.
fn parse_bbox(bbox_str: &str, crs: &str) -> Result<([f64; 4], OutputCrs), WmsError> {
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

    // WMS 1.3.0 axis order: EPSG:4326 uses lat/lon, everything else x/y.
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

    match crs {
        "EPSG:3857" => {
            // Web Mercator metres → WGS84 degrees; pixel Y stays Mercator-spaced.
            let (lon1, lat1) = epsg3857_to_wgs84(x1, y1);
            let (lon2, lat2) = epsg3857_to_wgs84(x2, y2);
            Ok(([lon1, lat1, lon2, lat2], OutputCrs::WebMercator))
        }
        "EPSG:3067" | "EPSG:3035" => {
            // Projected metres: keep them in the OutputCrs so the engine lays
            // output pixels out in the projection, and pass its inverse-
            // projected WGS84 envelope as the read window.
            let proj_crs = ds_core::geo::projected_output_crs(crs).ok_or_else(|| {
                WmsError::invalid_parameter(&format!("CRS '{crs}' has no projection definition"))
            })?;
            let proj_bbox = [x1, y1, x2, y2];
            let wgs84 = ds_core::geo::wgs84_envelope(&proj_crs, proj_bbox);
            Ok((
                wgs84,
                OutputCrs::Projected {
                    crs: proj_crs,
                    bbox: proj_bbox,
                },
            ))
        }
        _ => {
            // CRS:84 and EPSG:4326 are already in WGS84 degrees.
            Ok(([x1, y1, x2, y2], OutputCrs::Wgs84))
        }
    }
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
        let (bbox, crs) = parse_bbox("10,55,30,70", "CRS:84").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
        assert_eq!(crs, OutputCrs::Wgs84);
    }

    #[test]
    fn test_bbox_epsg4326_axis_swap() {
        // EPSG:4326 uses lat/lon order: south,west,north,east
        let (bbox, crs) = parse_bbox("55,10,70,30", "EPSG:4326").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
        assert_eq!(crs, OutputCrs::Wgs84);
    }

    #[test]
    fn test_bbox_invalid() {
        assert!(parse_bbox("10,55,5,70", "CRS:84").is_err()); // west > east
        assert!(parse_bbox("NaN,0,1,1", "CRS:84").is_err());
    }

    #[test]
    fn test_bbox_epsg3857_reprojection() {
        // Web Mercator bbox covering roughly lon 1.5-14.3, lat 53.0-63.0
        let (bbox, crs) =
            parse_bbox("171318.93,6897641.62,2528475.00,9153471.98", "EPSG:3857").unwrap();
        // Should be reprojected to WGS84 degrees
        assert!((bbox[0] - 1.539).abs() < 0.01); // west ≈ 1.54°
        assert!((bbox[1] - 52.536).abs() < 0.01); // south ≈ 52.54°
        assert!((bbox[2] - 22.714).abs() < 0.01); // east ≈ 22.71°
        assert!((bbox[3] - 63.216).abs() < 0.01); // north ≈ 63.22°
        assert_eq!(crs, OutputCrs::WebMercator);
    }

    #[test]
    fn test_bbox_epsg3067_projected() {
        // Regression for #251/#160: FMI's native TM35FIN extent. The bbox is in
        // EPSG:3067 metres and must NOT be passed through as WGS84 degrees.
        let (bbox, crs) = parse_bbox(
            "-118331.366408,6335621.167014,875567.731907,7907751.537264",
            "EPSG:3067",
        )
        .unwrap();

        // OutputCrs carries the projected request rectangle verbatim.
        match &crs {
            OutputCrs::Projected {
                bbox: proj,
                crs: proj_crs,
            } => {
                assert_eq!(
                    *proj,
                    [
                        -118331.366408,
                        6335621.167014,
                        875567.731907,
                        7907751.537264
                    ]
                );
                // EPSG:3067 is Transverse Mercator (TM35FIN).
                assert!(matches!(
                    proj_crs,
                    ds_core::geo::Crs::TransverseMercator { .. }
                ));
            }
            other => panic!("expected Projected, got {other:?}"),
        }

        // The returned WGS84 envelope must be plausible Nordic degrees, NOT the
        // metres treated as degrees (a metres-as-degrees bug puts these in the
        // millions). FMI's composite is wide, so the lon span reaches ~10–37°E.
        let [west, south, east, north] = bbox;
        assert!(
            (-30.0..60.0).contains(&west) && (-30.0..60.0).contains(&east),
            "envelope lon {west}..{east} should be degrees"
        );
        assert!(
            (45.0..75.0).contains(&south) && (45.0..75.0).contains(&north),
            "envelope lat {south}..{north} should be degrees"
        );
        assert!(west < east && south < north);
    }

    #[test]
    fn test_bbox_epsg3035_projected() {
        // EPSG:3035 (ETRS89-LAEA) is also projected metres → Projected, not Wgs84.
        let (_bbox, crs) = parse_bbox("4200000,3200000,5300000,5000000", "EPSG:3035").unwrap();
        assert!(matches!(crs, OutputCrs::Projected { .. }));
    }

    #[test]
    fn test_parse_time() {
        assert!(parse_time("2024-01-01T00:00:00Z").is_ok());
        assert!(parse_time("2024-01-01T00:00:00+00:00").is_ok());
        assert!(parse_time("2024-01-01T00:00").is_ok());
        assert!(parse_time("not-a-time").is_err());
    }

    #[test]
    fn parse_image_format_accepts_all_supported_types() {
        assert!(matches!(
            parse_image_format(Some("image/png")).unwrap(),
            ds_render::ImageFormat::Png
        ));
        assert!(matches!(
            parse_image_format(Some("image/jpeg")).unwrap(),
            ds_render::ImageFormat::Jpeg
        ));
        assert!(matches!(
            parse_image_format(Some("image/webp")).unwrap(),
            ds_render::ImageFormat::Webp
        ));
    }

    #[test]
    fn parse_image_format_defaults_to_png_when_absent() {
        assert!(matches!(
            parse_image_format(None).unwrap(),
            ds_render::ImageFormat::Png
        ));
    }

    #[test]
    fn parse_image_format_rejects_unknown_with_invalid_format() {
        // Locks in the fix for #161: any value outside SUPPORTED_FORMATS
        // must return InvalidFormat, not silently fall back to PNG. The
        // pre-fix handler returned PNG for image/gif and any other typo
        // with no error, leaving capabilities-trusting clients confused.
        for bogus in ["image/gif", "image/avif", "", "png", "image/webp;q=1"] {
            let err = parse_image_format(Some(bogus)).expect_err(bogus);
            assert!(
                matches!(err, WmsError::InvalidFormat(_)),
                "expected InvalidFormat for {bogus:?}, got {err:?}"
            );
        }
    }

    /// Adding an entry to `SUPPORTED_FORMATS` without extending the match in
    /// `parse_image_format` will cause this test to fail — update both
    /// together. (Introduced to prevent the same capabilities-vs-handler
    /// drift that caused #161.)
    #[test]
    fn parse_image_format_covers_every_supported_format() {
        for fmt in SUPPORTED_FORMATS {
            let parsed = parse_image_format(Some(fmt)).unwrap_or_else(|_| {
                panic!("SUPPORTED_FORMATS entry {fmt:?} not handled by parse_image_format")
            });
            assert_eq!(
                parsed.content_type(),
                *fmt,
                "parse_image_format({fmt:?}) → ImageFormat with wrong content_type {}",
                parsed.content_type()
            );
        }
    }
}
