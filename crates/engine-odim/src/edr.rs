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
//! Composite queries reuse the process-wide byte-bounded composite cache.
//! Cold reads and HDF5 parsing are blocking. `api_edr::executor` runs these
//! synchronous query methods on its dedicated multi-thread runtime, with
//! bounded admission and deadlines (#178); HTTP workers do not execute them.
//! The multi-thread worker context supports the storage `block_in_place`
//! bridge. Trajectories use the executor's blocking pool and an explicit
//! runtime handle for remote reads instead.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{parse_area_coords, AreaGridAxes, QueryPolygon, MAX_EXACT_AREA_CELLS};
use ds_core::geo::Crs;
use ds_core::map_engine::OutputCrs;
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use ds_core::resample::ProjectionGrid;

use crate::catalog::CatalogEntry;
use crate::engine::{footprint_guard_envelope, OdimEngine};
use crate::reader::{OdimComposite, VALUE_PRECISION};

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
    let (col, row) = source_px(composite, lon, lat)?;
    sample_px(composite, col, row, gain, offset, nodata, undetect)
}

/// The composite's fractional pixel `(col, row)` at a WGS84 `(lon, lat)`,
/// measured from the grid's north-west *corner* (pixel `c` spans
/// `[c, c + 1)`): `None` where the point projects to a non-finite
/// coordinate.
fn source_px(composite: &OdimComposite, lon: f64, lat: f64) -> Option<(f64, f64)> {
    let (x, y) = composite.crs.forward(lon, lat);
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let [src_w, src_s, src_e, src_n] = composite.bbox;
    let src_dx = (src_e - src_w) / composite.xsize as f64;
    let src_dy = (src_n - src_s) / composite.ysize as f64;
    Some(((x - src_w) / src_dx, (src_n - y) / src_dy))
}

/// [`sample_bilinear`] at the fractional pixel `(col, row)` [`source_px`]
/// measures.
fn sample_px(
    composite: &OdimComposite,
    col: f64,
    row: f64,
    gain: f64,
    offset: f64,
    nodata: f64,
    undetect: Option<f64>,
) -> Option<f64> {
    if !col.is_finite() || !row.is_finite() {
        return None;
    }
    let (rows, cols) = composite.pixels.shape();

    // Fractional position relative to pixel *centres*. A pixel `c`
    // spans native-x `[src_w + c·dx, src_w + (c+1)·dx)`, so its
    // centre sits at `src_w + (c+0.5)·dx` — hence the `-0.5`.
    let col_f = col - 0.5;
    let row_f = row - 0.5;
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
    let col = col.floor() as i64;
    let row = row.floor() as i64;
    if col < 0 || col >= cols as i64 || row < 0 || row >= rows as i64 {
        return None;
    }
    composite
        .pixels
        .sample(row as usize, col as usize, gain, offset, nodata, undetect)
}

/// The composite geometry an area grid's pixel positions depend on.
type Geometry = (Crs, [f64; 4], u32, u32);

/// Each area cell's composite pixel, `None` outside the polygon or the grid.
type Positions = Vec<Option<(f64, f64)>>;

fn geometry(composite: &OdimComposite) -> Geometry {
    (
        composite.crs.clone(),
        composite.bbox,
        composite.xsize,
        composite.ysize,
    )
}

impl OdimEngine {
    /// Each area cell's composite pixel, as [`source_px`] measures it,
    /// `None` outside the polygon. A lat/lon composite's is affine and
    /// exact per cell. A projected one's (stereographic, LAEA, TM) is a
    /// forward transform per cell, exact as a position query's, up to
    /// [`MAX_EXACT_AREA_CELLS`]; past it the transforms would dominate the
    /// answer (Critical Rule 5), and a larger grid's come from a coarse
    /// [`ProjectionGrid`] that evaluates the transform at its nodes only,
    /// as the map's does, the cells outside the catalog's footprint left
    /// out, as the map's ghost guard leaves them (#449, #1436).
    fn area_positions(
        &self,
        composite: &OdimComposite,
        polygon: &QueryPolygon,
        axes: &AreaGridAxes,
        mask: &[bool],
    ) -> Positions {
        let (nx, ny) = axes.dims();
        if matches!(composite.crs, Crs::Wgs84) || nx * ny <= MAX_EXACT_AREA_CELLS {
            return axes
                .y
                .iter()
                .flat_map(|&y| axes.x.iter().map(move |&x| (x, y)))
                .zip(mask)
                .map(|((x, y), &inside)| inside.then(|| source_px(composite, x, y)).flatten())
                .collect();
        }
        let b = &polygon.bbox;
        let east = if b.crosses_antimeridian() {
            b.east + 360.0
        } else {
            b.east
        };
        let (rows, cols) = composite.pixels.shape();
        let window = OutputCrs::Wgs84.footprint_pixel_window(
            [b.west, b.south, east, b.north],
            footprint_guard_envelope(self.seed_spatial_extent),
            nx as u32,
            ny as u32,
        );
        let grid = ProjectionGrid::build_2d_within(
            nx as u32,
            ny as u32,
            window,
            cols as u32,
            rows as u32,
            |fx, fy| {
                (
                    b.west + fx * (east - b.west),
                    b.north - fy * (b.north - b.south),
                )
            },
            |lon, lat| source_px(composite, lon, lat).unwrap_or((f64::NAN, f64::NAN)),
        );
        let (x0, x1, y0, y1) = window;
        (0..ny)
            .flat_map(|iy| (0..nx).map(move |ix| (ix, iy)))
            .map(|(ix, iy)| {
                let (ox, oy) = (ix as u32, iy as u32);
                let inside =
                    mask[axes.index(ix, iy)] && (x0..=x1).contains(&ox) && (y0..=y1).contains(&oy);
                inside.then(|| grid.sample(ox, oy))
            })
            .collect()
    }

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

    /// Reject a `parameters` filter that names anything but this engine's
    /// single quantity (`select_parameters`). A `None` filter (all
    /// parameters) is fine.
    fn check_parameter_filter(&self, parameters: Option<&[String]>) -> Result<(), DataServerError> {
        ds_core::edr_engine::select_parameters(parameters, &[self.parameter.as_str()])?;
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
                standard_name: None,
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
                // The request's time is up (#1399): no gap stands in for it.
                Err(DataServerError::DeadlineExceeded) => {
                    return Err(DataServerError::DeadlineExceeded)
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
                precision: VALUE_PRECISION,
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
    /// `None`. Every scan in the window (all of them without one) is a
    /// step of its `t` axis: `[t, y, x]`.
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

        // Grid resolution comes from the seed composite's dimensions
        // (every timestep shares the same grid) — no probe load, so
        // a single unreadable first file no longer hard-fails the
        // whole query the way an earlier `load_composite(...)?` did.
        // `crs84_bbox_spans` keeps an antimeridian-crossing extent
        // (`west > east`, #180) narrow instead of ~360° wide.
        let (lon_span, lat_span) = ds_core::geo::crs84_bbox_spans(self.seed_spatial_extent);
        let deg_per_px_lon = (lon_span / self.seed_xsize as f64).max(f64::MIN_POSITIVE);
        let deg_per_px_lat = (lat_span / self.seed_ysize as f64).max(f64::MIN_POSITIVE);

        // Shared cell-centre construction (ds-core, #671): x ascends
        // west→east, y descends north→south, at the composite's resolution
        // while the answer fits the shared per-response budget (#673), and
        // evenly coarser past it (#1436): an unfiltered query over a full
        // 5-min catalog is coarsened to fit rather than refused, and only
        // more scans than the budget has values is a 400.
        let axes = polygon.fit_grid(deg_per_px_lon, deg_per_px_lat, entries.len(), 1)?;
        let (nx, ny) = axes.dims();
        let mask = polygon.cell_mask(&axes);

        let mut times = Vec::with_capacity(entries.len());
        let mut all_values: Vec<Option<f64>> = Vec::with_capacity(entries.len() * ny * nx);
        // Each cell's composite pixel, placed once per composite geometry
        // (every scan of a catalog normally shares it) rather than once per
        // cell and scan.
        let mut placed: Option<(Geometry, Positions)> = None;

        for entry in &entries {
            times.push(entry.time);
            let composite = match self.load_composite(&entry.location) {
                Ok(c) => c,
                Err(DataServerError::DeadlineExceeded) => {
                    return Err(DataServerError::DeadlineExceeded)
                }
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
            ds_core::deadline::check()?;
            let key = geometry(&composite);
            if placed.as_ref().is_none_or(|(placed, _)| *placed != key) {
                let positions = self.area_positions(&composite, polygon, &axes, &mask);
                placed = Some((key, positions));
            }
            let positions = &placed.as_ref().expect("placed above").1;
            let gain = self.gain_override.unwrap_or(composite.gain);
            let offset = self.offset_override.unwrap_or(composite.offset);
            let nodata = self.nodata_override.unwrap_or(composite.nodata);
            all_values.extend(positions.iter().map(|position| {
                position.and_then(|(col, row)| {
                    sample_px(
                        &composite,
                        col,
                        row,
                        gain,
                        offset,
                        nodata,
                        composite.undetect,
                    )
                })
            }));
        }

        // Every catalog entry is a timestamped scan, so the scans served are
        // always on a `t` axis, one scan included (#1412).
        let domain = DomainDescription::Grid {
            x: axes.x,
            y: axes.y,
            t: Some(times),
            z: None,
            coarsened: axes.coarsened,
        };
        let shape = vec![entries.len(), ny, nx];
        let axis_names = vec!["t".to_string(), "y".to_string(), "x".to_string()];

        let mut ranges = HashMap::new();
        ranges.insert(
            self.parameter.clone(),
            NdArray {
                shape,
                axis_names,
                values: all_values,
                precision: VALUE_PRECISION,
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
    /// Area and radius answers are fitted to the value budget (#1436).
    fn coarsens_areas(&self) -> bool {
        true
    }

    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        // ODIM composites are gridded fields, not station networks.
        Ok(LocationSnapshot::empty())
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
        vec![
            "position".to_string(),
            "area".to_string(),
            "radius".to_string(),
        ]
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
