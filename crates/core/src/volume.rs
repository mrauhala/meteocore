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
}

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
#[derive(Debug, Clone)]
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
    /// Flat index of cell `(i_r, i_a, i_h)` into [`Self::values`] (height fastest).
    pub fn index(&self, i_r: usize, i_a: usize, i_h: usize) -> usize {
        (i_r * self.dims[1] + i_a) * self.dims[2] + i_h
    }

    /// Count of cells with a **finite** sampled value (excludes `NaN` *and*
    /// `±∞`) — for diagnostics / emptiness checks.
    pub fn valid_count(&self) -> usize {
        self.values.iter().filter(|v| v.is_finite()).count()
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
    /// volumetric output), this defaults to an error, so an engine implements it
    /// only if it supports the structured cylindrical grid (#351 / #357).
    fn read_voxel_grid(
        &self,
        _quantity: Option<&str>,
        _time: Option<DateTime<Utc>>,
        _dims: Option<[usize; 3]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<VoxelGrid, DataServerError> {
        Err(DataServerError::Engine(
            "voxel grid not supported by this engine".into(),
        ))
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
}
