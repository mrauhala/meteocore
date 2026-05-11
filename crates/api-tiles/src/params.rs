use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::TilesError;

/// Absolute maximum zoom level.
pub const MAX_ZOOM_LEVEL: u32 = 24;

/// Default per-collection maximum zoom level.
pub const DEFAULT_MAX_ZOOM: u32 = 18;

/// Standard tile size in pixels.
pub const TILE_SIZE: u32 = 256;

/// Supported raster output formats for map tiles.
const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// MVT format aliases. Either form works in `?f=` for clients that prefer
/// a short token or the canonical MIME.
pub const MVT_FORMAT_TOKENS: &[&str] = &["mvt", "application/vnd.mapbox-vector-tile"];

/// Query parameters for tile requests.
#[derive(Debug, Deserialize)]
pub struct TileQueryParams {
    pub datetime: Option<String>,
    #[serde(rename = "f")]
    pub format: Option<String>,
}

impl TileQueryParams {
    /// Whether the client requested a Mapbox Vector Tile via `?f=mvt`.
    pub fn is_mvt(&self) -> bool {
        self.format
            .as_deref()
            .map(|f| MVT_FORMAT_TOKENS.contains(&f))
            .unwrap_or(false)
    }
}

/// Validated tile query parameters.
pub struct ValidatedTileParams {
    pub time: Option<DateTime<Utc>>,
    pub format: ds_render::ImageFormat,
}

impl TileQueryParams {
    pub fn validate(&self) -> Result<ValidatedTileParams, TilesError> {
        let format_str = self.format.as_deref().unwrap_or("image/png");
        if !SUPPORTED_FORMATS.contains(&format_str) {
            return Err(TilesError::BadRequest(format!(
                "Format '{format_str}' is not supported. Supported: {}",
                SUPPORTED_FORMATS.join(", ")
            )));
        }
        let format = match format_str {
            "image/jpeg" => ds_render::ImageFormat::Jpeg,
            "image/webp" => ds_render::ImageFormat::Webp,
            _ => ds_render::ImageFormat::Png,
        };

        let time = self.datetime.as_deref().map(parse_time).transpose()?;

        Ok(ValidatedTileParams { time, format })
    }
}

/// Parse an ISO 8601 timestamp.
fn parse_time(s: &str) -> Result<DateTime<Utc>, TilesError> {
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
    Err(TilesError::BadRequest(format!(
        "Cannot parse datetime '{s}' as ISO 8601"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_default_format() {
        let params = TileQueryParams {
            datetime: None,
            format: None,
        };
        let validated = params.validate().unwrap();
        assert!(matches!(validated.format, ds_render::ImageFormat::Png));
    }

    #[test]
    fn test_validate_jpeg_format() {
        let params = TileQueryParams {
            datetime: None,
            format: Some("image/jpeg".to_string()),
        };
        let validated = params.validate().unwrap();
        assert!(matches!(validated.format, ds_render::ImageFormat::Jpeg));
    }

    #[test]
    fn test_validate_invalid_format() {
        let params = TileQueryParams {
            datetime: None,
            format: Some("text/html".to_string()),
        };
        assert!(params.validate().is_err());
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
