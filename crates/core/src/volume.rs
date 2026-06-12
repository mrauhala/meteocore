//! Volumetric domain types and the [`VolumeEngine`] trait for OGC 3D Tiles
//! delivery.
//!
//! Engines that hold genuinely 3-D data (radar polar volumes today) sample it
//! into a [`VolumePointCloud`] — a georeferenced set of points, one per source
//! cell — which the framework-free `ds-3dtiles` crate encodes into a `.pnts`
//! tile + `tileset.json`. The point-cloud representation matches both the
//! `.pnts` tile format and the native polar sampling of a radar volume. A
//! dense, regular voxel grid (for the draft `EXT_primitive_voxels` path) is a
//! separate representation tracked in #351.
//!
//! Like the other engine traits, `VolumeEngine` returns domain types only —
//! colorization and byte encoding live in `ds-3dtiles`, not here.

use crate::cells::{
    extract_cells, track_cells, CellExtractionOptions, CellSet, TrackSet, TrackingOptions,
    MAX_TRACK_SCANS,
};
use crate::error::DataServerError;
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// One sample in a [`VolumePointCloud`]: an ECEF offset (metres) from the
/// cloud's [`VolumePointCloud::rtc_center`], plus the physical value measured
/// there.
///
/// The offset is `f32`: storing positions relative to a nearby center (rather
/// than absolute ECEF, which is ~6.4e6 m and would lose sub-metre precision in
/// `f32`) keeps the cloud compact while staying well within radar accuracy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumePoint {
    /// ECEF offset (metres) from `rtc_center`, in true ECEF axes (no glTF
    /// Y-up/Z-up convention — `.pnts` is ECEF-native).
    pub offset: [f32; 3],
    /// Physical value at this point (e.g. dBZ).
    pub value: f64,
}

/// A georeferenced volumetric point cloud, ready for OGC 3D Tiles encoding.
#[derive(Debug, Clone)]
pub struct VolumePointCloud {
    /// ECEF (EPSG:4978) metres of the local origin all [`VolumePoint::offset`]s
    /// are relative to — typically the radar antenna. Becomes the `.pnts`
    /// `RTC_CENTER`.
    pub rtc_center: [f64; 3],
    /// Geodetic bounding region `[west, south, east, north, min_height,
    /// max_height]` — lon/lat in **radians**, heights in metres. This is the
    /// 3D Tiles `region` bounding-volume layout (EPSG:4979).
    pub region: [f64; 6],
    /// The samples.
    pub points: Vec<VolumePoint>,
    /// Quantity id sampled (e.g. `"DBZH"`).
    pub quantity: String,
    /// Physical unit of [`VolumePoint::value`] (e.g. `"dBZ"`).
    pub unit: String,
}

/// Metadata describing a collection's volumetric content, for the API layer
/// (3D Tiles tileset listing, capabilities). Cheap to build from a snapshot.
#[derive(Debug, Clone, Default)]
pub struct VolumeInfo {
    /// `(id, label)` for each renderable quantity.
    pub quantities: Vec<(String, String)>,
    /// Distinct volume valid-times, ascending.
    pub times: Vec<DateTime<Utc>>,
    /// Default quantity id (used when a request names none).
    pub default_quantity: String,
    /// Unit of the default quantity.
    pub default_unit: String,
    /// Coverage bounding region `[west, south, east, north, min_h, max_h]`
    /// (lon/lat **radians**, heights metres) — a region guaranteed to *contain*
    /// the collection's content, for building the 3D Tiles `tileset.json`
    /// bounding volume without sampling the full volume. `None` if the
    /// collection has no known spatial extent yet.
    pub region: Option<[f64; 6]>,
    /// Voxel-grid capability **and** the metadata it requires, coupled in one
    /// `Option` so "supports but no origin" is unrepresentable. `Some` ⇒ the
    /// engine implements [`VoxelGrid`] sampling (via
    /// [`VolumeEngine::read_voxel_grid`]), so the **isosurface** 3D Tiles
    /// representation is available alongside the `.pnts` point cloud; `None` ⇒
    /// point-cloud only (the default `read_voxel_grid` is unsupported).
    pub voxel_grid: Option<VoxelGridCaps>,
}

/// Voxel-grid capability metadata for a [`VolumeInfo`] — present iff the engine
/// can produce a [`VoxelGrid`] (and thus an isosurface mesh).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoxelGridCaps {
    /// WGS84 `[lon_deg, lat_deg, height_m]` of the volume origin (the radar
    /// antenna) — the point all isosurface-mesh positions are stored relative
    /// to. Lets the 3D Tiles layer build the glTF tile `transform` (antenna
    /// ECEF) **without** sampling the grid.
    pub origin: [f64; 3],
    /// Cylinder radial extent (metres): the ground coverage radius, = the
    /// [`VoxelGrid::radius_range`] max the sampler produces. Lets the voxel 3D
    /// Tiles layer size the `3DTILES_bounding_volume_cylinder` **without**
    /// sampling the grid (the cylinder extent must match the grid exactly).
    pub radius_m: f64,
    /// Cylinder vertical extent (metres): the volume height ceiling, = the
    /// [`VoxelGrid::height_range`] max. Same purpose as [`Self::radius_m`].
    pub height_m: f64,
}

/// The physical value (dBZ) marking a radar voxel cell as **clear air / no
/// echo**: the level a producer fills `undetect` cells with, and the level a
/// 3D Tiles isosurface seals `NaN` corners at (`background`). Shared here so the
/// *fill* (the radar engine) and the *seal* (the API/encoder) stay in lockstep
/// — an isosurface threshold must be **above** this floor, or clear air would
/// fall inside the surface. v1 uses one reflectivity floor regardless of
/// quantity (per-quantity floors are a follow-up, tied to #350).
pub const NO_ECHO_FLOOR_DBZ: f32 = -32.0;

/// A regular **cylindrical voxel grid** sampled from a volume — the structured
/// substrate for true 3D Tiles voxel content (`EXT_primitive_voxels`, #351) and
/// isosurface meshing (#357). Cylinder axes match radar's native geometry:
/// `radius` = ground range (m), `angle` = azimuth (rad), `height` = metres above
/// the origin. `values` is one `f32` per cell with **`NaN` = no data** (outside
/// the surveyed beam fan / cone of silence), row-major with `height` varying
/// fastest, then `angle`, then `radius` — i.e.
/// `index = (i_r * n_angle + i_a) * n_height + i_h`.
///
/// The byte/axis-order conventions of the draft voxel glTF extensions (which
/// reorder axes between `3DTILES_content_voxels` and `EXT_primitive_voxels`) are
/// the *encoder's* concern; this domain type uses one natural order.
///
/// Not `Clone`: at the `MAX_VOXELS` cap the `values` buffer is ~128 MB, so
/// copies must be deliberate (move it into the encoder).
#[derive(Debug)]
pub struct VoxelGrid {
    /// Cylinder-axis origin (the radar antenna): WGS84 lon/lat degrees, height m.
    pub origin_lon: f64,
    pub origin_lat: f64,
    pub origin_height: f64,
    /// Grid dimensions `[n_radius, n_angle, n_height]`.
    pub dims: [usize; 3],
    /// Radial extent (ground range, metres): `[min, max]`.
    pub radius_range: [f64; 2],
    /// Angular extent (azimuth, radians): `[min, max]`.
    pub angle_range: [f64; 2],
    /// Height extent (metres above the origin): `[min, max]`.
    pub height_range: [f64; 2],
    /// One value per cell (`NaN` = no data); `len() == n_radius * n_angle * n_height`.
    pub values: Vec<f32>,
    /// Quantity id sampled (e.g. `"DBZH"`).
    pub quantity: String,
    /// Physical unit of `values` (e.g. `"dBZ"`).
    pub unit: String,
}

impl VoxelGrid {
    /// Flat index of cell `(i_r, i_a, i_h)` for a grid of `dims`
    /// `[n_radius, n_angle, n_height]` — height fastest, then angle, then
    /// radius. The single source of truth for the axis order (which producers
    /// must match while the glTF voxel spec settles); usable before the grid
    /// exists (the fill loop has no `self` yet).
    pub fn index_of(dims: [usize; 3], i_r: usize, i_a: usize, i_h: usize) -> usize {
        debug_assert!(
            i_r < dims[0] && i_a < dims[1] && i_h < dims[2],
            "voxel index ({i_r}, {i_a}, {i_h}) out of range for dims {dims:?}"
        );
        (i_r * dims[1] + i_a) * dims[2] + i_h
    }

    /// Flat index of cell `(i_r, i_a, i_h)` into [`Self::values`].
    pub fn index(&self, i_r: usize, i_a: usize, i_h: usize) -> usize {
        Self::index_of(self.dims, i_r, i_a, i_h)
    }

    /// Count of cells with a **finite** value (excludes `NaN` *and* `±∞`) — for
    /// diagnostics. Note this counts **every** finite cell, including a
    /// producer's finite "clear air / no echo" floor (e.g. the radar engine's
    /// `NO_ECHO_FLOOR`, #360) — so it is **not** equivalent to "has echo data":
    /// a clear-air-only grid still has `valid_count() > 0`. Callers needing an
    /// echo-emptiness check must use the engine's own echo count, not this.
    pub fn valid_count(&self) -> usize {
        self.values.iter().filter(|v| v.is_finite()).count()
    }
}

/// Query for [`VolumeEngine::read_cells`]: the usual selection knobs
/// (quantity / time / dims / reference time) plus the segmentation +
/// tracking options. Bundled in one struct so the trait method stays
/// readable and future knobs (e.g. a motion-field request) don't churn
/// every implementor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CellQuery {
    /// Quantity to segment (`None` → the engine default). Engines may
    /// restrict cells to reflectivity-like quantities (the VIL / linear-Z
    /// math assumes dBZ).
    pub quantity: Option<String>,
    /// Target scan: nearest retained volume (`None` → latest), the same
    /// selection rule as [`VolumeEngine::read_voxel_grid`].
    pub time: Option<DateTime<Utc>>,
    /// Voxel-grid resolution to segment on (`None` → engine default).
    /// Cells don't need a fine grid — callers typically pass a low tier.
    pub dims: Option<[usize; 3]>,
    /// Segmentation knobs (threshold, speckle floor, cap).
    pub extraction: CellExtractionOptions,
    /// Centroid-matching knobs.
    pub tracking: TrackingOptions,
    /// How many scans **before** the target to segment and track across
    /// (`0` → the target scan only, no trajectories).
    pub track_scans: usize,
    /// Forecast model run (`None` → latest; ignored by non-forecast engines).
    pub reference_time: Option<DateTime<Utc>>,
}

/// The cells + tracks product for one scan window — what every API surface
/// (3D Tiles, Features, WMS/Maps/Tiles) renders.
#[derive(Debug, Clone, PartialEq)]
pub struct CellProduct {
    /// Per-scan cell sets, ascending in time; the **last** entry is the
    /// target scan. A scan with no echo at the threshold is present as an
    /// empty set (so tracking sees the death). Never empty.
    pub cell_sets: Vec<(DateTime<Utc>, Arc<CellSet>)>,
    /// Trajectories across the window.
    pub tracks: TrackSet,
}

impl CellProduct {
    /// The target scan's cells.
    pub fn target(&self) -> &(DateTime<Utc>, Arc<CellSet>) {
        self.cell_sets.last().expect("cell_sets is never empty")
    }
}

/// An engine that samples a collection's data into a volumetric point cloud
/// for OGC 3D Tiles delivery.
///
/// Separate from `MapEngine`/`EdrEngine`/`FeatureEngine`: only engines with
/// genuinely 3-D data implement it. The API state keeps a registry of
/// `Arc<dyn VolumeEngine>` keyed by collection id, like the other traits.
pub trait VolumeEngine: Send + Sync {
    /// Sample the collection into a 3-D point cloud.
    ///
    /// - `quantity`: which parameter to sample (`None` → the default).
    /// - `time`: select the retained volume **nearest** this valid time
    ///   (`None` → latest). There is **no staleness cap** — the nearest
    ///   retained volume is always returned regardless of age (matching the
    ///   `MapEngine` raster path); a caller needing freshness must check the
    ///   returned time, or the engine config must bound retention.
    /// - `min_value`: drop points whose physical value is below this (`None` →
    ///   keep every non-nodata sample). E.g. a dBZ floor to cut clutter.
    /// - `reference_time`: forecast model run (`None` → latest; ignored by
    ///   non-forecast engines such as radar).
    ///
    /// Returns [`DataServerError::LocationNotFound`] when the selection yields
    /// no data (matching the other engines' "no data ⇒ 404" convention), so
    /// callers never have to encode an empty cloud.
    fn read_point_cloud(
        &self,
        quantity: Option<&str>,
        time: Option<DateTime<Utc>>,
        min_value: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<VolumePointCloud, DataServerError>;

    /// Sample the collection into a regular **cylindrical voxel grid**.
    ///
    /// - `quantity` / `time` / `reference_time`: as [`Self::read_point_cloud`].
    /// - `dims`: requested resolution `[n_radius, n_angle, n_height]`; `None`
    ///   lets the engine pick a sensible default from the volume geometry.
    ///
    /// Cells outside the surveyed beam fan are `NaN` (no fabricated data across
    /// the cone of silence). Returns [`DataServerError::LocationNotFound`] when
    /// the selection yields no sampled cell.
    ///
    /// **Optional capability:** unlike [`Self::read_point_cloud`] (the baseline
    /// volumetric output), this defaults to "not available", so an engine
    /// implements it only if it supports the structured cylindrical grid
    /// (#351 / #357). The default is [`DataServerError::LocationNotFound`]
    /// (→ 404) — a capability gap is "this resource doesn't exist for this
    /// collection", not a server fault (matching the no-data-in-window
    /// convention), so the future voxel route won't surface a misleading 500.
    ///
    /// Returns `Arc<VoxelGrid>` (not an owned grid): one resampled grid feeds
    /// several consumers — the isosurface, echo-top, and voxel encoders all
    /// mesh the same `(quantity, time, dims)` grid — so engines cache and
    /// share the (multi-MB) result instead of recomputing the multi-million-
    /// cell resample per representation.
    fn read_voxel_grid(
        &self,
        _quantity: Option<&str>,
        _time: Option<DateTime<Utc>>,
        _dims: Option<[usize; 3]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<Arc<VoxelGrid>, DataServerError> {
        Err(DataServerError::LocationNotFound(
            "voxel grid not supported by this engine".into(),
        ))
    }

    /// Segment storm cells on the target scan and track them across the
    /// preceding `query.track_scans` scans (#367).
    ///
    /// **Generic by default:** any engine with voxel-grid capability
    /// ([`VolumeInfo::voxel_grid`] is `Some`) gets cells for free — the
    /// default implementation walks [`VolumeInfo::times`] backwards from the
    /// target, reads one grid per scan via [`Self::read_voxel_grid`], runs
    /// [`extract_cells`] per scan and [`track_cells`] over the window.
    /// Engines override only to add caching (the ODIM engine memoizes
    /// per-volume cell sets — volume files are immutable).
    ///
    /// Returns `Ok` with an **empty** target set when the scan simply has no
    /// echo at the threshold (Features needs "no cells now" as a valid empty
    /// collection; the 3D Tiles layer maps all-empty to its 404) — unlike
    /// `read_point_cloud`/`read_voxel_grid`, whose empty case is an error.
    /// [`DataServerError::LocationNotFound`] is reserved for "this engine /
    /// collection has no voxel-grid capability or no volumes at all".
    ///
    /// **Quantity contract:** the quantity selected via [`CellQuery::quantity`]
    /// **must** be a radar reflectivity in dBZ — the linear-Z centroid
    /// weighting and the VIL integral in [`extract_cells`] are reflectivity
    /// physics and produce garbage for wind speed, temperature, etc. This
    /// default cannot check units generically, so an implementor whose
    /// quantities are not all reflectivity must gate (the ODIM override
    /// rejects non-dBZ-unit quantities with `InvalidParameter` → 400).
    ///
    /// **Concurrency contract:** same as [`Self::read_voxel_grid`] — call
    /// from a context where that method is callable (the 3D Tiles / raster
    /// paths run it under `spawn_blocking`).
    ///
    /// **Performance:** the default walks `track_scans + 1` scans with one
    /// [`Self::read_voxel_grid`] call each — for an engine backed by remote
    /// storage that is N sequential blocking reads on one thread, exactly
    /// the stall-multiplying pattern the engine concurrency rules prohibit.
    /// Such engines must override `read_cells` to cache per-scan
    /// segmentations (as the ODIM engine does) or batch the reads.
    fn read_cells(&self, query: &CellQuery) -> Result<CellProduct, DataServerError> {
        let info = self.volume_info();
        let Some(caps) = info.voxel_grid else {
            return Err(DataServerError::LocationNotFound(
                "storm cells not supported: engine has no voxel-grid capability".into(),
            ));
        };
        if info.times.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "storm cells: no volumes retained".into(),
            ));
        }
        // Nearest retained scan (latest if None) — the read_voxel_grid rule.
        let target_idx = match query.time {
            Some(t) => info
                .times
                .iter()
                .enumerate()
                .min_by_key(|(_, vt)| (**vt - t).num_seconds().abs())
                .map(|(i, _)| i)
                .expect("times is non-empty"),
            None => info.times.len() - 1,
        };
        // Clamp the window: each scan is one sequential blocking
        // read_voxel_grid, so an unbounded caller value would multiply the
        // stall arbitrarily (see the performance note above).
        let start = target_idx.saturating_sub(query.track_scans.min(MAX_TRACK_SCANS));
        let quantity_label = query
            .quantity
            .clone()
            .unwrap_or_else(|| info.default_quantity.clone());

        let mut cell_sets = Vec::with_capacity(target_idx - start + 1);
        for &t in &info.times[start..=target_idx] {
            let set = match self.read_voxel_grid(
                query.quantity.as_deref(),
                Some(t),
                query.dims,
                query.reference_time,
            ) {
                Ok(grid) => Arc::new(extract_cells(&grid, t, &query.extraction)),
                // The voxel-grid convention reports "no echo" as
                // LocationNotFound; for cells that is a valid empty scan.
                Err(DataServerError::LocationNotFound(_)) => Arc::new(CellSet::empty(
                    t,
                    quantity_label.clone(),
                    query.extraction.threshold,
                    caps.origin,
                )),
                Err(e) => return Err(e),
            };
            cell_sets.push((t, set));
        }
        let tracks = track_cells(&cell_sets, &query.tracking);
        Ok(CellProduct { cell_sets, tracks })
    }

    /// Metadata for the volumetric collection (quantities, valid times).
    ///
    /// Returns a shared snapshot so it stays **O(1) on a per-request path** (no
    /// recompute, no `Vec` clone) — engines should serve a cached
    /// `Arc<VolumeInfo>` rebuilt on data refresh, per the `RasterInfo` rule
    /// (#211).
    fn volume_info(&self) -> Arc<VolumeInfo>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voxel_grid_index_is_height_fastest_and_valid_count() {
        let dims = [2, 4, 3]; // n_radius=2, n_angle=4, n_height=3 → 24 cells
        let mut g = VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 250_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 20_000.0],
            values: vec![f32::NAN; 24],
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        };
        // index = (i_r * n_angle + i_a) * n_height + i_h — height varies fastest.
        assert_eq!(g.index(0, 0, 0), 0);
        assert_eq!(g.index(0, 0, 1), 1);
        assert_eq!(g.index(0, 1, 0), 3);
        assert_eq!(g.index(1, 0, 0), 12);
        assert_eq!(g.index(1, 3, 2), 23);

        assert_eq!(g.valid_count(), 0);
        let i = g.index(1, 2, 1);
        g.values[i] = 35.0;
        g.values[5] = f32::INFINITY; // non-finite is not "valid"
        assert_eq!(g.valid_count(), 1);
    }

    use chrono::TimeZone;

    /// Stub engine: three retained scans; the blob sits at azimuth column 0
    /// in scan 0 and 1, and the middle scan has no echo (exercises the
    /// empty-set path). Grids are rebuilt per call (no caching — that's the
    /// point of the default impl).
    struct StubVolumes {
        times: Vec<DateTime<Utc>>,
        info: Arc<VolumeInfo>,
    }

    impl StubVolumes {
        fn new() -> Self {
            let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
            let times: Vec<_> = (0..3)
                .map(|i| t0 + chrono::Duration::minutes(5 * i))
                .collect();
            let info = Arc::new(VolumeInfo {
                quantities: vec![("DBZH".into(), "Reflectivity".into())],
                times: times.clone(),
                default_quantity: "DBZH".into(),
                default_unit: "dBZ".into(),
                region: None,
                voxel_grid: Some(VoxelGridCaps {
                    origin: [24.5, 60.5, 100.0],
                    radius_m: 100_000.0,
                    height_m: 10_000.0,
                }),
            });
            Self { times, info }
        }

        fn grid_with_blob(&self) -> VoxelGrid {
            let dims = [20, 36, 10];
            let mut g = VoxelGrid {
                origin_lon: 24.5,
                origin_lat: 60.5,
                origin_height: 100.0,
                dims,
                radius_range: [0.0, 100_000.0],
                angle_range: [0.0, std::f64::consts::TAU],
                height_range: [0.0, 10_000.0],
                values: vec![f32::NAN; 20 * 36 * 10],
                quantity: "DBZH".into(),
                unit: "dBZ".into(),
            };
            for i_r in 4..8 {
                for i_h in 2..6 {
                    let idx = g.index(i_r, 9, i_h);
                    g.values[idx] = 45.0;
                }
            }
            g
        }
    }

    impl VolumeEngine for StubVolumes {
        fn read_point_cloud(
            &self,
            _quantity: Option<&str>,
            _time: Option<DateTime<Utc>>,
            _min_value: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<VolumePointCloud, DataServerError> {
            unimplemented!("not used by read_cells")
        }

        fn read_voxel_grid(
            &self,
            _quantity: Option<&str>,
            time: Option<DateTime<Utc>>,
            _dims: Option<[usize; 3]>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<Arc<VoxelGrid>, DataServerError> {
            // Middle scan: no echo ⇒ the voxel-grid 404 convention.
            if time == Some(self.times[1]) {
                return Err(DataServerError::LocationNotFound("no echoes".into()));
            }
            Ok(Arc::new(self.grid_with_blob()))
        }

        fn volume_info(&self) -> Arc<VolumeInfo> {
            Arc::clone(&self.info)
        }
    }

    #[test]
    fn default_read_cells_segments_window_and_tracks() {
        let engine = StubVolumes::new();
        let query = CellQuery {
            extraction: CellExtractionOptions {
                threshold: 35.0,
                min_volume_km3: 0.0,
                max_cells: 16,
            },
            track_scans: 2,
            ..CellQuery::default()
        };
        let product = engine.read_cells(&query).expect("cells");
        assert_eq!(product.cell_sets.len(), 3);
        // Scan 0 and 2 have the blob; scan 1 is the empty set, not an error.
        assert_eq!(product.cell_sets[0].1.cells.len(), 1);
        assert!(product.cell_sets[1].1.cells.is_empty());
        assert_eq!(product.cell_sets[2].1.cells.len(), 1);
        assert_eq!(product.target().0, engine.times[2]);
        // The empty middle scan breaks the trajectory: two 1-point tracks.
        assert_eq!(product.tracks.tracks.len(), 2);

        // track_scans = 0 ⇒ target scan only.
        let solo = engine
            .read_cells(&CellQuery {
                extraction: query.extraction,
                ..CellQuery::default()
            })
            .expect("cells");
        assert_eq!(solo.cell_sets.len(), 1);

        // Explicit target pinned to the empty middle scan.
        let pinned = engine
            .read_cells(&CellQuery {
                time: Some(engine.times[1]),
                extraction: query.extraction,
                ..CellQuery::default()
            })
            .expect("cells");
        assert!(pinned.target().1.cells.is_empty());
    }

    #[test]
    fn read_cells_without_voxel_caps_is_not_found() {
        struct NoCaps;
        impl VolumeEngine for NoCaps {
            fn read_point_cloud(
                &self,
                _q: Option<&str>,
                _t: Option<DateTime<Utc>>,
                _m: Option<f64>,
                _r: Option<DateTime<Utc>>,
            ) -> Result<VolumePointCloud, DataServerError> {
                unimplemented!()
            }
            fn volume_info(&self) -> Arc<VolumeInfo> {
                Arc::new(VolumeInfo::default())
            }
        }
        let err = NoCaps.read_cells(&CellQuery::default()).unwrap_err();
        assert!(matches!(err, DataServerError::LocationNotFound(_)));
    }
}
