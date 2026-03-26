//! Coordinate parsing utilities for EDR query parameters.
//!
//! Parses WKT and simple coordinate formats used in OGC API - EDR
//! position and area queries.

use ds_core::error::DataServerError;

/// Parse EDR position query coordinates.
/// Accepts `POINT(lon lat)` (WKT) or `lon,lat` format.
/// Returns (lat, lon).
pub fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();

    // Try WKT POINT format: POINT(lon lat)
    if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.trim().split_whitespace().collect();
        if parts.len() != 2 {
            return Err(DataServerError::InvalidParameter(
                "Expected POINT(lon lat) format".into(),
            ));
        }
        let lon: f64 = parts[0].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return validate_coords(lat, lon);
    }

    // Try simple "lon,lat" format
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return validate_coords(lat, lon);
    }

    Err(DataServerError::InvalidParameter(
        "Expected coords as POINT(lon lat) or lon,lat".into(),
    ))
}

fn validate_coords(lat: f64, lon: f64) -> Result<(f64, f64), DataServerError> {
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(DataServerError::InvalidParameter(format!(
            "Coordinates out of range: lat={lat}, lon={lon}"
        )));
    }
    Ok((lat, lon))
}

