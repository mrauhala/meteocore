//! `EdrEngine` impl for engine-odim — position and area queries
//! against the same ODIM_H5 composite grid the `MapEngine` impl
//! serves.
//!
//! Both query types resample the native (projected) radar grid to
//! WGS84 query geometry:
//!
//! - **position** — bilinear interpolation at a single `(lon, lat)`,
//!   one value per catalog timestep, returned as a `PointSeries`
//!   coverage.
//! - **area** — a regular WGS84 lon/lat grid covering the requested
//!   polygon's bounding box, each cell bilinearly sampled and then
//!   masked to the polygon, returned as a `Grid` coverage.
//!
//! ODIM composites carry no station list, so `get_locations` is
//! empty and `query_location` is unsupported — clients use the
//! position endpoint instead.
//!
//! Phase 1.5 scope: the position query loads one composite per
//! timestep through `OdimEngine`'s single-entry cache, so a query
//! spanning N timesteps performs N sequential file reads. ODIM
//! directories hold at most `max_files` entries (typically ≤288 at
//! 5-min cadence), so this is acceptable for v1; a multi-entry
//! composite cache is a follow-up.
//!
//! Those reads are blocking (HDF5 parse). The api-edr handlers call
//! `EdrEngine` methods directly from `async fn`s without
//! `spawn_blocking`, so the work currently lands on a Tokio worker —
//! a pre-existing api-edr gap that affects every EDR engine, tracked
//! in issue #178 and to be fixed in the api-edr handlers.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{parse_area_coords, QueryPolygon};
use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
};

use crate::catalog::CatalogEntry;
use crate::engine::OdimEngine;
use crate::reader::OdimComposite;

/// Per-dimension cap on the area-query output grid. An ODIM
/// composite can be ~4400 px across; without a cap an area query
/// over the whole grid would emit a multi-megabyte coverage per
/// timestep. 256 keeps a single-timestep area response well under
/// 1 MB while still being finer than most display use cases need.
const MAX_AREA_DIM: usize = 256;

/// Cap on the number of timesteps an area query may span. The area
/// coverage is an `ny × nx` grid (each ≤ `MAX_AREA_DIM`) *per*
/// timestep, so an unbounded count would let one request allocate
/// hundreds of MB. 64 × 256 × 256 `Option<f64>` ≈ 67 MB worst case,
/// which bounds a deliberate area-over-time query while still
/// rejecting a "give me everything" request against a full
/// 5-min-cadence catalog (~288 entries). A position query has no
/// such cap because it yields only `N` scalars, not `N · ny · nx`.
const MAX_AREA_TIMESTEPS: usize = 64;

/// Parse an EDR `coords` value for a position query into
/// `(lat, lon)`. Accepts WKT `POINT(lon lat)` and the bare
/// `lon,lat` shorthand — the same two forms the other engines
/// accept (each engine carries its own copy; there is no shared
/// parser in `ds-core`).
fn parse_point_coords(coords: &str) -> Result<(f64, f64), DataServerError> {
    let trimmed = coords.trim();

    let pair = if let Some(inner) = trimmed
        .strip_prefix("POINT(")
        .or_else(|| trimmed.strip_prefix("POINT ("))
        .and_then(|s| s.strip_suffix(')'))
    {
        inner.split_whitespace().collect::<Vec<_>>()
    } else {
        trimmed.split(',').map(str::trim).collect::<Vec<_>>()
    };

    if pair.len() != 2 {
        return Err(DataServerError::InvalidParameter(
            "Expected POINT(lon lat) or lon,lat format".into(),
        ));
    }
    let lon: f64 = pair[0].parse().map_err(|_| {
        DataServerError::InvalidParameter(format!("Invalid longitude: {}", pair[0]))
    })?;
    let lat: f64 = pair[1]
        .parse()
        .map_err(|_| DataServerError::InvalidParameter(format!("Invalid latitude: {}", pair[1])))?;
    if !lon.is_finite() || !lat.is_finite() {
        return Err(DataServerError::InvalidParameter(
            "Coordinates must be finite numbers".into(),
        ));
    }
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return Err(DataServerError::InvalidParameter(format!(
            "Coordinates out of range: lon={lon}, lat={lat}"
        )));
    }
    Ok((lat, lon))
}

/// Bilinearly interpolate the composite's physical value at a WGS84
/// `(lon, lat)`. Returns `None` when the point falls outside the
/// grid, projects to a non-finite coordinate, or any of the four
/// surrounding pixels is `nodata`/`undetect` — in the last case it
/// degrades to a nearest-neighbour sample (matching what
/// `MapEngine::get_raster_tile` would return) rather than dropping
/// the point entirely.
fn sample_bilinear(
    composite: &OdimComposite,
    lon: f64,
    lat: f64,
    gain: f64,
    offset: f64,
    nodata: f64,
    undetect: Option<f64>,
) -> Option<f64> {
    let (x, y) = composite.crs.forward(lon, lat);
    if !x.is_finite() || !y.is_finite() {
        return None;
    }

    let [src_w, src_s, src_e, src_n] = composite.bbox;
    let src_dx = (src_e - src_w) / composite.xsize as f64;
    let src_dy = (src_n - src_s) / composite.ysize as f64;
    let (rows, cols) = composite.pixels.shape();

    // Fractional position relative to pixel *centres*. A pixel `c`
    // spans native-x `[src_w + c·dx, src_w + (c+1)·dx)`, so its
    // centre sits at `src_w + (c+0.5)·dx` — hence the `-0.5`.
    let col_f = (x - src_w) / src_dx - 0.5;
    let row_f = (src_n - y) / src_dy - 0.5;
    let col0 = col_f.floor();
    let row0 = row_f.floor();
    let fx = col_f - col0;
    let fy = row_f - row0;
    let c0 = col0 as i64;
    let r0 = row0 as i64;

    let in_bounds = |r: i64, c: i64| r >= 0 && c >= 0 && (r as usize) < rows && (c as usize) < cols;
    let s = |r: i64, c: i64| {
        composite
            .pixels
            .sample(r as usize, c as usize, gain, offset, nodata, undetect)
    };

    if in_bounds(r0, c0)
        && in_bounds(r0, c0 + 1)
        && in_bounds(r0 + 1, c0)
        && in_bounds(r0 + 1, c0 + 1)
    {
        if let (Some(tl), Some(tr), Some(bl), Some(br)) =
            (s(r0, c0), s(r0, c0 + 1), s(r0 + 1, c0), s(r0 + 1, c0 + 1))
        {
            let top = tl + (tr - tl) * fx;
            let bot = bl + (br - bl) * fx;
            return Some(top + (bot - top) * fy);
        }
    }

    // Fallback: nearest-neighbour, identical to the indexing
    // `get_raster_tile` uses.
    let col = ((x - src_w) / src_dx).floor() as i64;
    let row = ((src_n - y) / src_dy).floor() as i64;
    if col < 0 || col >= cols as i64 || row < 0 || row >= rows as i64 {
        return None;
    }
    composite
        .pixels
        .sample(row as usize, col as usize, gain, offset, nodata, undetect)
}

impl OdimEngine {
    /// Snapshot the catalog entries whose timestamps fall in the
    /// inclusive `datetime` range (or all entries when `None`).
    /// Returns owned clones so the `ArcSwap` guard drops immediately.
    fn catalog_entries_in_range(
        &self,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    ) -> Vec<CatalogEntry> {
        let snapshot = self.catalog.load();
        match datetime {
            Some((start, end)) => snapshot
                .iter()
                .filter(|e| e.time >= start && e.time <= end)
                .cloned()
                .collect(),
            None => snapshot.iter().cloned().collect(),
        }
    }

    /// Reject a `parameters` filter that doesn't name this engine's
    /// single quantity. A `None` filter (all parameters) is fine.
    fn check_parameter_filter(&self, parameters: Option<&[String]>) -> Result<(), DataServerError> {
        if let Some(params) = parameters {
            if !params.iter().any(|p| p == &self.parameter) {
                return Err(DataServerError::InvalidParameter(format!(
                    "Unknown parameter. Available: {}",
                    self.parameter
                )));
            }
        }
        Ok(())
    }

    /// Build the single-entry parameter-description map this engine
    /// advertises (one quantity per collection).
    fn parameter_map(&self) -> HashMap<String, ParameterDescription> {
        let mut map = HashMap::new();
        map.insert(
            self.parameter.clone(),
            ParameterDescription {
                label: self.parameter.replace('_', " "),
                unit: self.unit.clone(),
                observed_property: self.parameter.clone(),
            },
        );
        map
    }

    /// Position query: a `PointSeries` coverage with one bilinearly
    /// interpolated value per catalog timestep in range.
    fn query_point(
        &self,
        lat: f64,
        lon: f64,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        self.check_parameter_filter(parameters)?;

        let entries = self.catalog_entries_in_range(datetime);
        if entries.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No ODIM data available for the requested time range".into(),
            ));
        }

        let mut times = Vec::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());
        for entry in &entries {
            times.push(entry.time);
            match self.load_composite(&entry.location) {
                Ok(composite) => {
                    let gain = self.gain_override.unwrap_or(composite.gain);
                    let offset = self.offset_override.unwrap_or(composite.offset);
                    let nodata = self.nodata_override.unwrap_or(composite.nodata);
                    values.push(sample_bilinear(
                        &composite,
                        lon,
                        lat,
                        gain,
                        offset,
                        nodata,
                        composite.undetect,
                    ));
                }
                Err(e) => {
                    // A single unreadable file shouldn't fail the
                    // whole series — emit a gap and carry on.
                    tracing::warn!(
                        "[{}] EDR position query: failed to load `{}`: {e}",
                        self.collection_id,
                        entry.location.id()
                    );
                    values.push(None);
                }
            }
        }

        let mut ranges = HashMap::new();
        ranges.insert(
            self.parameter.clone(),
            NdArray {
                shape: vec![values.len()],
                axis_names: vec!["t".to_string()],
                values,
            },
        );

        Ok(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: times,
                z: None,
            },
            parameters: self.parameter_map(),
            ranges,
        })
    }

    /// Area query: a `Grid` coverage over the polygon's bounding
    /// box. The output grid is a regular WGS84 lon/lat lattice;
    /// cells whose centre falls outside the polygon are masked to
    /// `None`.
    fn query_polygon(
        &self,
        polygon: &QueryPolygon,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        self.check_parameter_filter(parameters)?;

        let entries = self.catalog_entries_in_range(datetime);
        if entries.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No ODIM data available for the requested time range".into(),
            ));
        }

        // Bound the response size. An area query produces an
        // `ny × nx` grid per timestep (`ny`, `nx` ≤ `MAX_AREA_DIM`),
        // so an unfiltered query over a full 5-min-cadence catalog
        // (`max_files` up to ~288) would allocate hundreds of MB.
        // Cap the timestep count and tell the client to narrow
        // `datetime` rather than silently truncating their request.
        if entries.len() > MAX_AREA_TIMESTEPS {
            return Err(DataServerError::InvalidParameter(format!(
                "Area query spans {} timesteps; the maximum is {MAX_AREA_TIMESTEPS}. \
                 Narrow the `datetime` range.",
                entries.len()
            )));
        }

        // Grid resolution comes from the seed composite's dimensions
        // (every timestep shares the same grid) — no probe load, so
        // a single unreadable first file no longer hard-fails the
        // whole query the way an earlier `load_composite(...)?` did.
        let [ll_lon, ll_lat, ur_lon, ur_lat] = self.seed_spatial_extent;
        let deg_per_px_lon =
            ((ur_lon - ll_lon).abs() / self.seed_xsize as f64).max(f64::MIN_POSITIVE);
        let deg_per_px_lat =
            ((ur_lat - ll_lat).abs() / self.seed_ysize as f64).max(f64::MIN_POSITIVE);

        let west = polygon.bbox.west;
        let south = polygon.bbox.south;
        let east = polygon.bbox.east;
        let north = polygon.bbox.north;

        // Match the source resolution, clamped to [1, MAX_AREA_DIM].
        let nx = (((east - west) / deg_per_px_lon).ceil() as usize).clamp(1, MAX_AREA_DIM);
        let ny = (((north - south) / deg_per_px_lat).ceil() as usize).clamp(1, MAX_AREA_DIM);

        // Cell centres. `x` ascends west→east, `y` descends
        // north→south (index 0 = north), matching the row order the
        // raster sampler and `NdArray` layout use.
        let cell_w = (east - west) / nx as f64;
        let cell_h = (north - south) / ny as f64;
        let x_values: Vec<f64> = (0..nx)
            .map(|ix| west + (ix as f64 + 0.5) * cell_w)
            .collect();
        let y_values: Vec<f64> = (0..ny)
            .map(|iy| north - (iy as f64 + 0.5) * cell_h)
            .collect();

        let has_time = entries.len() > 1;
        let mut times = Vec::with_capacity(entries.len());
        let mut all_values: Vec<Option<f64>> = Vec::with_capacity(entries.len() * ny * nx);

        for entry in &entries {
            times.push(entry.time);
            let composite = match self.load_composite(&entry.location) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "[{}] EDR area query: failed to load `{}`: {e}",
                        self.collection_id,
                        entry.location.id()
                    );
                    all_values.extend(std::iter::repeat_n(None, ny * nx));
                    continue;
                }
            };
            let gain = self.gain_override.unwrap_or(composite.gain);
            let offset = self.offset_override.unwrap_or(composite.offset);
            let nodata = self.nodata_override.unwrap_or(composite.nodata);
            for &y in &y_values {
                for &x in &x_values {
                    if polygon.contains(x, y) {
                        all_values.push(sample_bilinear(
                            &composite,
                            x,
                            y,
                            gain,
                            offset,
                            nodata,
                            composite.undetect,
                        ));
                    } else {
                        all_values.push(None);
                    }
                }
            }
        }

        let (domain, shape, axis_names) = if has_time {
            (
                DomainDescription::Grid {
                    x: x_values,
                    y: y_values,
                    t: Some(times),
                    z: None,
                },
                vec![entries.len(), ny, nx],
                vec!["t".to_string(), "y".to_string(), "x".to_string()],
            )
        } else {
            (
                DomainDescription::Grid {
                    x: x_values,
                    y: y_values,
                    t: None,
                    z: None,
                },
                vec![ny, nx],
                vec!["y".to_string(), "x".to_string()],
            )
        };

        let mut ranges = HashMap::new();
        ranges.insert(
            self.parameter.clone(),
            NdArray {
                shape,
                axis_names,
                values: all_values,
            },
        );

        Ok(QueryResult {
            domain,
            parameters: self.parameter_map(),
            ranges,
        })
    }
}

impl EdrEngine for OdimEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        // ODIM composites are gridded fields, not station networks.
        Ok(vec![])
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        // An ODIM composite is a gridded field with no named
        // locations, so any location id is genuinely "not found" →
        // HTTP 404 (`LocationNotFound`), not "bad request" (400).
        // Same variant `CsvEngine`/`PostgisEngine` use for an
        // unknown location id.
        Err(DataServerError::LocationNotFound(
            "ODIM engine has no named locations. \
             Use the position query endpoint instead (e.g. /position?coords=POINT(lon lat))."
                .into(),
        ))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec![self.parameter.clone()]
    }

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.parameter_map()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let snapshot = self.catalog.load();
        let first = snapshot.first()?.time;
        let last = snapshot.last()?.time;
        Some((first, last))
    }

    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        // Radar composites arrive at discrete (typically 5-min)
        // steps, so advertise the exact timestamps rather than just
        // the interval — same rationale as the GRIB engine.
        let times: Vec<DateTime<Utc>> = self.catalog.load().iter().map(|e| e.time).collect();
        if times.is_empty() {
            None
        } else {
            Some(times)
        }
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        // Seed field captured at construction — stable for the
        // engine's lifetime and independent of whether the render
        // cache has been warmed, so an `apis = ["edr"]`-only
        // collection still reports a real extent.
        Some(self.seed_spatial_extent)
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".to_string(), "area".to_string()]
    }

    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let (lat, lon) = parse_point_coords(coords)?;
        Ok(CoverageResponse::Single(
            self.query_point(lat, lon, datetime, parameters)?,
        ))
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        let polygon = parse_area_coords(coords)?;
        let result = self.query_polygon(&polygon, datetime, parameters)?;
        Ok(CoverageResponse::Single(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_point_coords_accepts_wkt() {
        let (lat, lon) = parse_point_coords("POINT(10.5 56.0)").unwrap();
        assert_eq!((lat, lon), (56.0, 10.5));
        // PROJ-style space before the paren is tolerated.
        let (lat, lon) = parse_point_coords("POINT (10.5 56.0)").unwrap();
        assert_eq!((lat, lon), (56.0, 10.5));
    }

    #[test]
    fn parse_point_coords_accepts_bare_pair() {
        let (lat, lon) = parse_point_coords("10.5, 56.0").unwrap();
        assert_eq!((lat, lon), (56.0, 10.5));
        let (lat, lon) = parse_point_coords("  -3.2,48.7 ").unwrap();
        assert_eq!((lat, lon), (48.7, -3.2));
    }

    #[test]
    fn parse_point_coords_rejects_malformed() {
        assert!(parse_point_coords("POINT(10.5)").is_err());
        assert!(parse_point_coords("10.5").is_err());
        assert!(parse_point_coords("a,b").is_err());
        assert!(parse_point_coords("10.5,56.0,3").is_err());
    }

    #[test]
    fn parse_point_coords_rejects_out_of_range() {
        // Longitude past ±180 / latitude past ±90 are rejected so a
        // transposed `lat,lon` pair fails loudly instead of querying
        // a nonsense location.
        assert!(parse_point_coords("200.0, 10.0").is_err());
        assert!(parse_point_coords("10.0, 95.0").is_err());
        assert!(parse_point_coords("POINT(10 91)").is_err());
    }

    #[test]
    fn parse_point_coords_rejects_non_finite() {
        assert!(parse_point_coords("NaN, 10.0").is_err());
        assert!(parse_point_coords("inf, 10.0").is_err());
    }
}
