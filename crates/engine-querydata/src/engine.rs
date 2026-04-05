use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::model::{DomainDescription, Location, NdArray, ParameterDescription, QueryResult};

use crate::parse::QueryData;

/// QueryData engine serving multi-parameter NWP/observation gridded data.
///
/// A single querydata file contains multiple parameters, levels, and time steps.
/// The Engine trait exposes all parameters via EDR position queries.
/// The MapEngine trait renders a single parameter (selected by config) for WMS/Maps/Tiles.
pub struct QueryDataEngine {
    data: Arc<QueryData>,
    /// Parameter index to render for MapEngine. Defaults to 0 (first param).
    map_param_idx: usize,
    /// Collection ID for error messages.
    #[allow(dead_code)]
    collection_id: String,
}

impl QueryDataEngine {
    /// Create a new QueryDataEngine from a file path.
    pub fn new(
        path: &Path,
        collection_id: &str,
        wms_parameter: Option<&str>,
    ) -> Result<Self, DataServerError> {
        let data = QueryData::open(path).map_err(|e| {
            DataServerError::QueryData(format!("[{collection_id}] Failed to open: {e}"))
        })?;

        let map_param_idx = if let Some(name) = wms_parameter {
            data.param_index_by_name(name).ok_or_else(|| {
                DataServerError::QueryData(format!(
                    "[{collection_id}] wms_parameter '{name}' not found in file. \
                     Available: {}",
                    data.params
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?
        } else {
            0
        };

        tracing::info!(
            "[{}] Loaded querydata: {} params, {}x{} grid, {} levels, {} times",
            collection_id,
            data.params.len(),
            data.grid.nx,
            data.grid.ny,
            data.levels.len(),
            data.times.len(),
        );

        Ok(Self {
            data: Arc::new(data),
            map_param_idx,
            collection_id: collection_id.to_string(),
        })
    }

    /// Bilinear interpolation at (lon, lat) for a given parameter and time.
    /// Returns None if the point is outside the grid or all neighbors are missing.
    fn interpolate(
        &self,
        lon: f64,
        lat: f64,
        param_idx: usize,
        level_idx: usize,
        time_idx: usize,
    ) -> Option<f64> {
        let gt = self.data.grid.geo_transform();
        let (x, y) = gt.crs.forward(lon, lat);

        // Continuous pixel coordinates (fractional)
        let col_f = (x - gt.origin_x) / gt.pixel_width - 0.5;
        let row_f = (gt.origin_y - y) / gt.pixel_height - 0.5;

        let col0 = col_f.floor() as i64;
        let row0 = row_f.floor() as i64;

        let nx = self.data.grid.nx as i64;
        let ny = self.data.grid.ny as i64;

        // Check bounds (need 2x2 neighborhood)
        if col0 < -1 || col0 >= nx || row0 < -1 || row0 >= ny {
            return None;
        }

        let dx = col_f - col0 as f64;
        let dy = row_f - row0 as f64;

        // Get 4 neighbors, flipping row for bottom-left origin
        let mut vals = [None; 4]; // [TL, TR, BL, BR]
        for (i, (dr, dc)) in [(0, 0), (0, 1), (1, 0), (1, 1)].iter().enumerate() {
            let c = col0 + dc;
            let r = row0 + dr;
            if c >= 0 && c < nx && r >= 0 && r < ny {
                // Flip row: GeoTransform row 0 = north, querydata row 0 = south
                let qd_row = (ny - 1 - r) as usize;
                let grid_idx = qd_row * nx as usize + c as usize;
                vals[i] = self.data.value(param_idx, grid_idx, level_idx, time_idx);
            }
        }

        // Bilinear interpolation — fall back to nearest if some neighbors are missing
        match (vals[0], vals[1], vals[2], vals[3]) {
            (Some(tl), Some(tr), Some(bl), Some(br)) => {
                let top = tl + (tr - tl) * dx;
                let bot = bl + (br - bl) * dx;
                Some(top + (bot - top) * dy)
            }
            _ => {
                // Nearest neighbor fallback
                let nc = (col_f + 0.5).floor().clamp(0.0, (nx - 1) as f64) as usize;
                let nr = (row_f + 0.5).floor().clamp(0.0, (ny - 1) as f64) as usize;
                let qd_row = (ny as usize - 1) - nr;
                let grid_idx = qd_row * nx as usize + nc;
                self.data.value(param_idx, grid_idx, level_idx, time_idx)
            }
        }
    }

    /// Find the time index closest to the requested time.
    fn find_time_idx(&self, time: Option<DateTime<Utc>>) -> Option<usize> {
        if self.data.times.is_empty() {
            return None;
        }
        match time {
            None => Some(self.data.times.len() - 1), // latest
            Some(t) => {
                // Find closest time
                let mut best_idx = 0;
                let mut best_diff = i64::MAX;
                for (i, dt) in self.data.times.iter().enumerate() {
                    let diff = (dt.signed_duration_since(t)).num_seconds().abs();
                    if diff < best_diff {
                        best_diff = diff;
                        best_idx = i;
                    }
                }
                Some(best_idx)
            }
        }
    }

    /// Find time indices within a datetime range.
    fn find_time_range(
        &self,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Vec<(usize, DateTime<Utc>)> {
        match datetime {
            None => self
                .data
                .times
                .iter()
                .enumerate()
                .map(|(i, t)| (i, *t))
                .collect(),
            Some((start, end)) => self
                .data
                .times
                .iter()
                .enumerate()
                .filter(|(_, t)| **t >= start && **t <= end)
                .map(|(i, t)| (i, *t))
                .collect(),
        }
    }
}

impl Engine for QueryDataEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        // Grid data has no named locations
        Ok(vec![])
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "QueryData engine does not support location queries (use position query)".into(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        self.data.params.iter().map(|p| p.name.clone()).collect()
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.data
            .params
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    ParameterDescription {
                        label: p.name.clone(),
                        unit: String::new(), // querydata doesn't store units
                        observed_property: p.name.clone(),
                    },
                )
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let first = self.data.times.first()?;
        let last = self.data.times.last()?;
        Some((*first, *last))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        let bl = self.data.grid.area.bottom_left;
        let tr = self.data.grid.area.top_right;
        Some([bl.0, bl.1, tr.0, tr.1])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string()]
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        let (lat, lon) = parse_coords(coords)?;

        let time_indices = self.find_time_range(datetime);
        if time_indices.is_empty() {
            return Err(DataServerError::QueryData(
                "No data available for the requested time range".into(),
            ));
        }

        let times: Vec<DateTime<Utc>> = time_indices.iter().map(|(_, t)| *t).collect();

        // Filter parameters
        let param_indices: Vec<(usize, &crate::parse::ParamInfo)> = self
            .data
            .params
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                parameters
                    .is_none_or(|filter| filter.iter().any(|f| f.eq_ignore_ascii_case(&p.name)))
            })
            .collect();

        if param_indices.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No matching parameters found".into(),
            ));
        }

        let domain = DomainDescription::PointSeries {
            x: lon,
            y: lat,
            t: times,
        };

        let mut params_map = HashMap::new();
        let mut ranges = HashMap::new();

        for (pi, param) in &param_indices {
            let values: Vec<Option<f64>> = time_indices
                .iter()
                .map(|(ti, _)| self.interpolate(lon, lat, *pi, 0, *ti))
                .collect();

            params_map.insert(
                param.name.clone(),
                ParameterDescription {
                    label: param.name.clone(),
                    unit: String::new(),
                    observed_property: param.name.clone(),
                },
            );

            ranges.insert(
                param.name.clone(),
                NdArray {
                    shape: vec![values.len()],
                    axis_names: vec!["t".to_string()],
                    values,
                },
            );
        }

        Ok(QueryResult {
            domain,
            parameters: params_map,
            ranges,
        })
    }
}

impl MapEngine for QueryDataEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
    ) -> Result<RasterTile, DataServerError> {
        let time_idx = self.find_time_idx(time).ok_or_else(|| {
            DataServerError::QueryData("No data available for the requested time".into())
        })?;

        let [west, south, east, north] = bbox;
        let param_idx = self.map_param_idx;

        let mut values = Vec::with_capacity((width * height) as usize);

        for row in 0..height {
            let lat = match output_crs {
                OutputCrs::Wgs84 => north - (row as f64 + 0.5) * (north - south) / height as f64,
                OutputCrs::WebMercator => {
                    let merc_north = lat_to_merc_y(north);
                    let merc_south = lat_to_merc_y(south);
                    let merc_y =
                        merc_north - (row as f64 + 0.5) * (merc_north - merc_south) / height as f64;
                    merc_y_to_lat(merc_y)
                }
            };

            for col in 0..width {
                let lon = west + (col as f64 + 0.5) * (east - west) / width as f64;
                values.push(self.interpolate(lon, lat, param_idx, 0, time_idx));
            }
        }

        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let param_name = self
            .data
            .params
            .get(self.map_param_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        let gt = self.data.grid.geo_transform();
        let bbox = gt.bbox();

        let native_crs = match self.data.grid.area.crs {
            ds_core::geo::Crs::Wgs84 => "EPSG:4326".to_string(),
            ds_core::geo::Crs::RotatedLatLon { .. } => "rotated_ll".to_string(),
            _ => "projected".to_string(),
        };

        RasterInfo {
            native_crs,
            spatial_extent: Some(bbox),
            times: self.data.times.clone(),
            parameter: param_name,
            unit: String::new(),
        }
    }
}

/// Parse EDR position query coordinates.
/// Accepts `POINT(lon lat)` or `lon,lat` format.
/// Returns (lat, lon).
fn parse_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();

    if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split_whitespace().collect();
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
        return Ok((lat, lon));
    }

    // Try comma-separated: lon,lat
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() == 2 {
        let lon: f64 = parts[0].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid longitude: {}", parts[0]))
        })?;
        let lat: f64 = parts[1].trim().parse().map_err(|_| {
            DataServerError::InvalidParameter(format!("Invalid latitude: {}", parts[1]))
        })?;
        return Ok((lat, lon));
    }

    Err(DataServerError::InvalidParameter(
        "Expected POINT(lon lat) or lon,lat format".into(),
    ))
}

/// Convert latitude (degrees) to Web Mercator Y (meters).
fn lat_to_merc_y(lat_deg: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    let lat_rad = lat_deg.to_radians();
    R * ((std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan()).ln()
}

/// Convert Web Mercator Y (meters) to latitude (degrees).
fn merc_y_to_lat(y: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / R).exp().atan()).to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_file() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/202604042019_202604040600_ecmwf_kenya_surface.sqd")
    }

    #[test]
    fn engine_parameters() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        let engine = QueryDataEngine::new(&path, "test", None).unwrap();
        let params = engine.get_parameters();
        assert_eq!(params.len(), 10);
        assert!(params.contains(&"Mean Sea Level Pressure (msl)".to_string()));
    }

    #[test]
    fn engine_spatial_extent() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        let engine = QueryDataEngine::new(&path, "test", None).unwrap();
        let bbox = engine.get_spatial_extent().unwrap();
        assert!((bbox[0] - (-40.0)).abs() < 0.01);
        assert!((bbox[1] - (-60.25)).abs() < 0.01);
        assert!((bbox[2] - 100.0).abs() < 0.01);
        assert!((bbox[3] - 60.0).abs() < 0.01);
    }

    #[test]
    fn engine_temporal_extent() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        let engine = QueryDataEngine::new(&path, "test", None).unwrap();
        let (first, last) = engine.get_temporal_extent().unwrap();
        assert_eq!(
            first.format("%Y-%m-%dT%H:%M").to_string(),
            "2026-04-04T06:00"
        );
        assert!(last > first);
    }

    #[test]
    fn engine_position_query() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        let engine = QueryDataEngine::new(&path, "test", None).unwrap();

        // Query a point in Kenya (Nairobi: 36.8, -1.3)
        let result = engine
            .query_position("POINT(36.8 -1.3)", None, None)
            .unwrap();

        // Should have all 10 parameters
        assert_eq!(result.parameters.len(), 10);
        assert_eq!(result.ranges.len(), 10);

        // Check that temperature has valid values
        let temp = result.ranges.get("2 Metre Temperature (2t)").unwrap();
        let has_values = temp.values.iter().any(|v| v.is_some());
        assert!(has_values, "Temperature should have some values");
    }

    #[test]
    fn engine_position_query_filtered_params() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        let engine = QueryDataEngine::new(&path, "test", None).unwrap();

        let params = vec!["2 Metre Temperature (2t)".to_string()];
        let result = engine
            .query_position("POINT(36.8 -1.3)", None, Some(&params))
            .unwrap();

        assert_eq!(result.parameters.len(), 1);
        assert!(result.parameters.contains_key("2 Metre Temperature (2t)"));
    }

    #[test]
    fn map_engine_raster_tile() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        // Use temperature as the map parameter
        let engine = QueryDataEngine::new(&path, "test", Some("2 Metre Temperature (2t)")).unwrap();

        // Small tile over Kenya
        let tile = engine
            .get_raster_tile([33.0, -5.0, 42.0, 5.0], 16, 16, None, &OutputCrs::Wgs84)
            .unwrap();

        assert_eq!(tile.width, 16);
        assert_eq!(tile.height, 16);
        assert_eq!(tile.values.len(), 256);

        // Should have some non-None values
        let non_none = tile.values.iter().filter(|v| v.is_some()).count();
        assert!(non_none > 0, "Tile should have some data values");
    }

    #[test]
    fn map_engine_raster_info() {
        let path = test_file();
        if !path.exists() {
            return;
        }
        let engine = QueryDataEngine::new(&path, "test", Some("2 Metre Temperature (2t)")).unwrap();
        let info = engine.raster_info();

        assert_eq!(info.parameter, "2 Metre Temperature (2t)");
        assert_eq!(info.native_crs, "EPSG:4326");
        assert_eq!(info.times.len(), 49);
        assert!(info.spatial_extent.is_some());
    }
}
