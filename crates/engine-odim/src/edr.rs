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
use std::sync::{Arc, Condvar, Mutex};

use chrono::{DateTime, Utc};
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{parse_area_coords, AreaGridAxes, QueryPolygon, MAX_EXACT_AREA_CELLS};
use ds_core::geo::Crs;
use ds_core::map_engine::OutputCrs;
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    ParameterDescriptions, QueryResult,
};
use ds_core::resample::ProjectionGrid;

use crate::catalog::{CatalogEntry, Location};
use crate::engine::{footprint_guard_envelope, LoadError, OdimEngine};
use crate::read_budget::{Budget, Permit};
use crate::reader::{OdimComposite, VALUE_PRECISION};

/// The most composites one EDR position or area query over a remote
/// catalog reads at once (#1495). A remote composite is a HEAD and a
/// whole-object GET, mostly waiting on the store, then an HDF5 decode:
/// eight overlap eight of them, so a 144-frame window pays 18 round trips,
/// not 144. Every read past a query's first is admitted by
/// `read_budget::BUDGET` ([`FrameAdmission`]).
pub(crate) const MAX_FRAME_READS: usize = 8;

/// How many of `entries`' composites a query reads at once:
/// [`MAX_FRAME_READS`] when a store serves every one, else one at a time,
/// as before #1495: a local file has no round trip to overlap.
pub(crate) fn frame_reads(entries: &[CatalogEntry]) -> usize {
    if entries
        .iter()
        .all(|entry| matches!(entry.location, Location::Remote { .. }))
    {
        MAX_FRAME_READS
    } else {
        1
    }
}

/// How the composite reads one EDR query has in flight are admitted
/// (#1495), as a PVOL walk's reads ahead are. Until its composite enters
/// `COMPOSITE_CACHE`, a read holds the file's bytes and the decode, about
/// 1.25 × the decoded array: OPERA's 4400 × 3800 `f32` grid, 67 MB decoded,
/// holds about 87 MB.
///
/// - One read at a time is unbudgeted, as every read was when a query read
///   one composite at a time.
/// - Each read beside it reserves the file's size, from its HEAD, and the
///   decode ([`OdimEngine::seed_decode_bytes`]) from the budget with
///   [`Budget::try_reserve_background`], which leaves half the budget to
///   the PVOL walks that must have theirs, and which counts no rejection.
/// - A read that gets no reservation waits for the unbudgeted one to end:
///   never refused, it is then read as one at a time would have read it,
///   until the request's deadline, which is `DeadlineExceeded`.
///
/// Only a read that fetches the file itself is admitted
/// (`engine::CompositeReader::load`): a resident composite, or another
/// request's fill the read joins first, holding nothing, takes no HEAD and
/// no admission. A read that claims the fill takes its HEAD and tries
/// [`Self::try_admit`], which never waits. Only without room does it give
/// the claim back and wait in [`Self::admit`], holding nothing. So a map
/// render or another query that wants the same composite waits for a fill
/// in progress, never behind a query's admission wait. The wait is on a
/// condition variable inside the read's `block_in_place`, never on a
/// runtime worker (#1200).
pub(crate) struct FrameAdmission {
    budget: Arc<Budget>,
    decode_bytes: u64,
    /// Whether the query's unbudgeted read is taken.
    unbudgeted: Mutex<bool>,
    freed: Condvar,
}

/// What admitted one composite read: held until its composite is decoded,
/// and given back on drop.
pub(crate) enum Admitted {
    /// The query's one unbudgeted read.
    Unbudgeted { _read: UnbudgetedRead },
    /// A reservation of the read budget.
    Reserved { _permit: Permit },
}

/// The query's unbudgeted read, taken: dropping it lets the next read have
/// it.
pub(crate) struct UnbudgetedRead(Arc<FrameAdmission>);

impl Drop for UnbudgetedRead {
    fn drop(&mut self) {
        *self.0.unbudgeted.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.0.freed.notify_one();
    }
}

impl FrameAdmission {
    /// The admission of one query's reads, reserving from `budget` each
    /// read's file and `decode_bytes`.
    pub(crate) fn new(budget: Arc<Budget>, decode_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            budget,
            decode_bytes,
            unbudgeted: Mutex::new(false),
            freed: Condvar::new(),
        })
    }

    /// Admit a read of a file of `file_bytes` without waiting: the
    /// unbudgeted read if it is free, else a reservation, else `None`.
    pub(crate) fn try_admit(self: &Arc<Self>, file_bytes: u64) -> Option<Admitted> {
        if let Some(read) = self.take_unbudgeted() {
            return Some(Admitted::Unbudgeted { _read: read });
        }
        let bytes = file_bytes.saturating_add(self.decode_bytes);
        let permit = self.budget.try_reserve_background(bytes)?;
        Some(Admitted::Reserved { _permit: permit })
    }

    /// Admit a read of a file of `file_bytes`: [`Self::try_admit`], else the
    /// unbudgeted read once it is given back, waiting no longer than the
    /// request's deadline. Never called holding a fill claim.
    pub(crate) fn admit(self: &Arc<Self>, file_bytes: u64) -> Result<Admitted, DataServerError> {
        if let Some(admitted) = self.try_admit(file_bytes) {
            return Ok(admitted);
        }
        let deadline = ds_core::deadline::current();
        let mut taken = self.unbudgeted.lock().unwrap_or_else(|e| e.into_inner());
        while *taken {
            taken = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(DataServerError::DeadlineExceeded);
                    }
                    self.freed
                        .wait_timeout(taken, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.freed.wait(taken).unwrap_or_else(|e| e.into_inner()),
            };
        }
        *taken = true;
        Ok(Admitted::Unbudgeted {
            _read: UnbudgetedRead(Arc::clone(self)),
        })
    }

    /// The unbudgeted read, if no read has it.
    fn take_unbudgeted(self: &Arc<Self>) -> Option<UnbudgetedRead> {
        let mut taken = self.unbudgeted.lock().unwrap_or_else(|e| e.into_inner());
        (!*taken).then(|| {
            *taken = true;
            UnbudgetedRead(Arc::clone(self))
        })
    }
}

/// Read the composites of `entries` with `read`, at most `concurrency` at
/// once, and hand each entry's index and value to `place` (#1495). Each
/// read is a job of `ds_executor::field_jobs::run_field_jobs`: a task of
/// the query runtime, inside `block_in_place`, under the query's deadline,
/// so a wait on `COMPOSITE_CACHE` for another request's fill of the same
/// file holds no runtime worker (#1200), and the fill is shared as before.
/// On the first error or once the deadline passes no read starts, and the
/// reads already running finish before this returns the error: what they
/// read stays cached for a retry.
pub(crate) fn read_frames<T: Send + 'static>(
    concurrency: usize,
    entries: Vec<CatalogEntry>,
    read: impl Fn(&CatalogEntry) -> Result<T, DataServerError> + Send + Sync + 'static,
    mut place: impl FnMut(usize, T) + Send,
) -> Result<(), DataServerError> {
    ds_executor::field_jobs::run_field_jobs(
        ds_executor::query_runtime(),
        concurrency,
        entries.into_iter().enumerate(),
        move |(index, entry): (usize, CatalogEntry)| read(&entry).map(|value| (index, value)),
        |(index, value)| {
            place(index, value);
            Ok(())
        },
    )
}

/// The value overrides of the collection's config, applied over each
/// composite's own.
#[derive(Clone, Copy)]
struct Calibration {
    gain: Option<f64>,
    offset: Option<f64>,
    nodata: Option<f64>,
}

impl Calibration {
    /// `(gain, offset, nodata)` for `composite`.
    fn of(&self, composite: &OdimComposite) -> (f64, f64, f64) {
        (
            self.gain.unwrap_or(composite.gain),
            self.offset.unwrap_or(composite.offset),
            self.nodata.unwrap_or(composite.nodata),
        )
    }
}

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

/// An area query's grid, shared by its frame reads (#1495), and each
/// cell's composite pixel, placed once per composite geometry (every scan
/// of a catalog normally shares it) rather than once per cell and scan.
struct AreaFrames {
    polygon: QueryPolygon,
    axes: AreaGridAxes,
    mask: Vec<bool>,
    seed_spatial_extent: [f64; 4],
    placed: Mutex<Option<(Geometry, Arc<Positions>)>>,
}

impl AreaFrames {
    /// Each cell's pixel of `composite`, placed for its geometry by the
    /// first frame read of it while the others wait. The wait is for that
    /// computation alone, never for I/O.
    fn positions(&self, composite: &OdimComposite) -> Arc<Positions> {
        let key = geometry(composite);
        let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((geometry, positions)) = placed.as_ref() {
            if *geometry == key {
                return Arc::clone(positions);
            }
        }
        let positions = Arc::new(area_positions(
            self.seed_spatial_extent,
            composite,
            &self.polygon,
            &self.axes,
            &self.mask,
        ));
        *placed = Some((key, Arc::clone(&positions)));
        positions
    }
}

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
    seed_spatial_extent: [f64; 4],
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
        footprint_guard_envelope(seed_spatial_extent),
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

impl OdimEngine {
    /// The admission of one EDR query's composite reads (#1495).
    fn frame_admission(&self) -> Arc<FrameAdmission> {
        FrameAdmission::new(
            Arc::clone(&crate::read_budget::BUDGET),
            self.seed_decode_bytes,
        )
    }

    /// The collection's value overrides.
    fn calibration(&self) -> Calibration {
        Calibration {
            gain: self.gain_override,
            offset: self.offset_override,
            nodata: self.nodata_override,
        }
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

    /// The single-entry parameter-description map a query result carries:
    /// the collection's one quantity, as the engine's `descriptions` has it.
    fn parameter_map(&self) -> HashMap<String, ParameterDescription> {
        self.descriptions
            .iter()
            .map(|(name, description)| (name.clone(), description.clone()))
            .collect()
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

        let times: Vec<DateTime<Utc>> = entries.iter().map(|entry| entry.time).collect();
        let mut values = vec![None; entries.len()];
        let collection: Arc<str> = Arc::from(self.collection_id.as_str());
        let calibration = self.calibration();
        let admission = self.frame_admission();
        let reader = self.composite_reader();
        // A few composites at a time over a remote catalog (#1495).
        read_frames(
            frame_reads(&entries),
            entries,
            move |entry| match reader
                .load(&entry.location, &admission)
                .map_err(LoadError::for_query)
            {
                Ok(composite) => {
                    let (gain, offset, nodata) = calibration.of(&composite);
                    Ok(sample_bilinear(
                        &composite,
                        lon,
                        lat,
                        gain,
                        offset,
                        nodata,
                        composite.undetect,
                    ))
                }
                // The request's time is up (#1399), or the read failed
                // (#1496): no gap stands in for it, since a null would be
                // kept as "no data" by every cache of the answer.
                Err(e) if e.class().is_transient() => Err(e),
                Err(e) => {
                    // A file gone since the scan or whose bytes do not
                    // decode shouldn't fail the whole series: emit a gap
                    // and carry on.
                    tracing::warn!(
                        "[{collection}] EDR position query: failed to load `{}`: {e}",
                        entry.location.id()
                    );
                    Ok(None)
                }
            },
            |index, value| values[index] = value,
        )?;

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

        let times: Vec<DateTime<Utc>> = entries.iter().map(|entry| entry.time).collect();
        // `[t][y][x]`; a file gone or undecodable stays a timestep of nulls.
        let cells = ny * nx;
        let mut all_values: Vec<Option<f64>> = vec![None; times.len() * cells];
        let area = Arc::new(AreaFrames {
            polygon: polygon.clone(),
            axes,
            mask,
            seed_spatial_extent: self.seed_spatial_extent,
            placed: Mutex::new(None),
        });
        let frame_area = Arc::clone(&area);
        let collection: Arc<str> = Arc::from(self.collection_id.as_str());
        let calibration = self.calibration();
        let admission = self.frame_admission();
        let reader = self.composite_reader();

        // A few composites at a time over a remote catalog (#1495), as in
        // `query_point`.
        read_frames(
            frame_reads(&entries),
            entries,
            move |entry| {
                let read = reader.load(&entry.location, &admission);
                let composite = match read.map_err(LoadError::for_query) {
                    Ok(c) => c,
                    // The deadline or a failed read (#1399, #1496), as in
                    // `query_point`: the query fails, never a timestep of
                    // nulls.
                    Err(e) if e.class().is_transient() => return Err(e),
                    // Gone since the scan, or bytes that do not decode.
                    Err(e) => {
                        tracing::warn!(
                            "[{collection}] EDR area query: failed to load `{}`: {e}",
                            entry.location.id()
                        );
                        return Ok(None);
                    }
                };
                ds_core::deadline::check()?;
                let positions = frame_area.positions(&composite);
                let (gain, offset, nodata) = calibration.of(&composite);
                let values: Vec<Option<f64>> = positions
                    .iter()
                    .map(|position| {
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
                    })
                    .collect();
                Ok(Some(values))
            },
            |index, values| {
                if let Some(values) = values {
                    let step = &mut all_values[index * cells..(index + 1) * cells];
                    for (cell, value) in step.iter_mut().zip(values) {
                        *cell = value;
                    }
                }
            },
        )?;

        // Every catalog entry is a timestamped scan, so the scans served are
        // always on a `t` axis, one scan included (#1412).
        let shape = vec![times.len(), ny, nx];
        let domain = DomainDescription::Grid {
            x: area.axes.x.clone(),
            y: area.axes.y.clone(),
            t: Some(times),
            z: None,
            coarsened: area.axes.coarsened.clone(),
        };
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

    fn get_parameter_descriptions(&self) -> Arc<ParameterDescriptions> {
        Arc::clone(&self.descriptions)
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
