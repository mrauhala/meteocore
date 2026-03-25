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

/// Parse EDR area query coordinates.
/// Accepts `POLYGON((lon1 lat1, lon2 lat2, ...))` (WKT) — extracts the bounding box.
/// Also accepts `bbox` format `west,south,east,north`.
/// Returns (west, south, east, north).
pub fn parse_bbox_coords(coords: &str) -> Result<(f64, f64, f64, f64), DataServerError> {
    let trimmed = coords.trim();

    // Try WKT POLYGON format
    if let Some(inner) = trimmed
        .strip_prefix("POLYGON((")
        .or_else(|| trimmed.strip_prefix("POLYGON (("))
        .and_then(|s| s.strip_suffix("))"))
    {
        let points: Vec<&str> = inner.split(',').collect();
        if points.len() < 3 {
            return Err(DataServerError::InvalidParameter(
                "POLYGON must have at least 3 coordinate pairs".into(),
            ));
        }

        let mut min_lon = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;

        for point in &points {
            let parts: Vec<&str> = point.trim().split_whitespace().collect();
            if parts.len() != 2 {
                return Err(DataServerError::InvalidParameter(format!(
                    "Invalid coordinate pair: '{}'", point.trim()
                )));
            }
            let lon: f64 = parts[0].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
            })?;
            let lat: f64 = parts[1].parse().map_err(|_| {
                DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
            })?;
            min_lon = min_lon.min(lon);
            max_lon = max_lon.max(lon);
            min_lat = min_lat.min(lat);
            max_lat = max_lat.max(lat);
        }

        return Ok((min_lon, min_lat, max_lon, max_lat));
    }

    // Try simple bbox format: west,south,east,north
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 4 {
        let west: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid west: {}", parts[0]))
        })?;
        let south: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid south: {}", parts[1]))
        })?;
        let east: f64 = parts[2].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid east: {}", parts[2]))
        })?;
        let north: f64 = parts[3].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid north: {}", parts[3]))
        })?;
        return Ok((west, south, east, north));
    }

    Err(DataServerError::InvalidParameter(
        "Expected coords as POLYGON((lon1 lat1, lon2 lat2, ...)) or west,south,east,north".into(),
    ))
}
