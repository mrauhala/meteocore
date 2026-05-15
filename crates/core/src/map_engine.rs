use chrono::{DateTime, Utc};

use crate::error::DataServerError;

/// The output CRS for map rendering, determining how pixels map to coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputCrs {
    /// WGS84 geographic (CRS:84 / EPSG:4326). Linear lat/lon mapping.
    Wgs84,
    /// Web Mercator (EPSG:3857). Bbox is in WGS84 degrees but pixel Y spacing
    /// follows the Mercator projection (non-linear in latitude).
    WebMercator,
}

/// A raster tile that can be colorized and served as a map image.
pub struct RasterTile {
    pub width: u32,
    pub height: u32,
    /// Row-major pixel values. None = nodata (transparent).
    pub values: Vec<Option<f64>>,
}

impl RasterTile {
    /// Returns true if all pixel values are nodata (None).
    pub fn is_empty(&self) -> bool {
        self.values.iter().all(Option::is_none)
    }
}

/// Metadata about a map-capable raster collection.
pub struct RasterInfo {
    /// Native CRS identifier (e.g., "EPSG:3067").
    pub native_crs: String,
    /// Native spatial extent [west, south, east, north] in WGS84.
    pub spatial_extent: Option<[f64; 4]>,
    /// Available timestamps, oldest first (ascending).
    pub times: Vec<DateTime<Utc>>,
    /// Default parameter name (e.g., "reflectivity").
    pub parameter: String,
    /// Unit of measurement (e.g., "dBZ").
    pub unit: String,
    /// All available parameters. Empty means single-parameter engine (use `parameter`).
    /// For multi-parameter engines (e.g., querydata), each entry is a (short_name, title) pair.
    pub parameters: Vec<(String, String)>,
}

/// Trait for serving raster data as map images.
///
/// Separate from `EdrEngine` (EDR) and `FeatureEngine` (Features).
/// Only raster engines (GeoTIFF, future NetCDF/GRIB) implement this.
pub trait MapEngine: Send + Sync {
    /// Extract a raster tile for the given bbox and output dimensions.
    ///
    /// The bbox is in WGS84 [west, south, east, north].
    /// The `output_crs` controls how pixels map to coordinates:
    /// - `Wgs84`: linear interpolation in lon/lat
    /// - `WebMercator`: pixels have equal spacing in Mercator Y (meters),
    ///   which is non-linear in latitude
    ///
    /// The optional `parameter` selects which data parameter to render when the
    /// engine supports multiple parameters (e.g., querydata with 10+ NWP fields).
    /// Engines that serve a single parameter (e.g., GeoTIFF) ignore this.
    /// The value comes from the style's `parameter` config field.
    ///
    /// The engine handles CRS reprojection to source data internally.
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
    ) -> Result<RasterTile, DataServerError>;

    /// Return metadata for capabilities documents.
    ///
    /// **Expected complexity: O(1) (or as close as practical).** Callers
    /// invoke this on the hot tile/map path to validate `?parameter-name=`
    /// against `parameters`, before acquiring the render semaphore. Engines
    /// should serve from an `ArcSwap`/`RwLock` snapshot, not recompute on
    /// every call. If your engine genuinely needs to derive metadata
    /// per-request, cache it.
    fn raster_info(&self) -> RasterInfo;
}
