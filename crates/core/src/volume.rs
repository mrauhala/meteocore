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

    /// Metadata for the volumetric collection (quantities, valid times).
    ///
    /// Returns a shared snapshot so it stays **O(1) on a per-request path** (no
    /// recompute, no `Vec` clone) — engines should serve a cached
    /// `Arc<VolumeInfo>` rebuilt on data refresh, per the `RasterInfo` rule
    /// (#211).
    fn volume_info(&self) -> Arc<VolumeInfo>;
}
