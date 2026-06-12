use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::geo::Crs;
use crate::vertical::VerticalDimension;

/// The output CRS for map rendering, determining how pixels map to coordinates.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputCrs {
    /// WGS84 geographic (CRS:84 / EPSG:4326). Linear lat/lon mapping.
    Wgs84,
    /// Web Mercator (EPSG:3857). Bbox is in WGS84 degrees but pixel Y spacing
    /// follows the Mercator projection (non-linear in latitude).
    WebMercator,
    /// A projected output CRS (e.g. EPSG:3067 TM35FIN, EPSG:3035 ETRS89-LAEA).
    ///
    /// The output pixel grid is laid out **linearly in the projected metres** of
    /// `crs` over `bbox`, then each grid node is inverse-projected to WGS84
    /// lon/lat before the engine samples its source. This is what makes a
    /// Finland-native client's `CRS=EPSG:3067&BBOX=<metres>` request render
    /// correctly instead of treating the metres as degrees (#160/#251).
    ///
    /// `bbox` is the request rectangle in `crs`'s metres,
    /// `[min_e, min_n, max_e, max_n]`. The WGS84 bounding box the engine uses to
    /// pick its read window / overview is passed separately as the
    /// `get_raster_tile` `bbox` argument (see [`wgs84_envelope`]).
    ///
    /// [`wgs84_envelope`]: crate::geo::wgs84_envelope
    Projected {
        /// Projection definition (from [`crate::geo::projected_output_crs`]).
        crs: Crs,
        /// Request rectangle in projected metres `[min_e, min_n, max_e, max_n]`.
        bbox: [f64; 4],
    },
}

/// Convert WGS84 latitude (degrees) to Web Mercator Y (metres). Shared by every
/// map engine's output-axis mapping so the equal-Y-metres spacing of
/// `OutputCrs::WebMercator` is computed identically everywhere.
fn lat_to_merc_y(lat_deg: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    R * ((std::f64::consts::FRAC_PI_4 + lat_deg.to_radians() / 2.0).tan()).ln()
}

/// Inverse of [`lat_to_merc_y`].
///
/// `π/2 - 2·atan(exp(-y/R))` is algebraically equal to the standard EPSG:3857
/// inverse `2·atan(exp(y/R)) - π/2` but negates the exponent so `exp()` decays
/// toward zero as |y| grows rather than overflowing — numerically stable across
/// the full ±π/2 range under f64.
fn merc_y_to_lat(y: f64) -> f64 {
    const R: f64 = 6_378_137.0;
    (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / R).exp().atan()).to_degrees()
}

impl OutputCrs {
    /// Map a fractional output position `(fx, fy)` in `[0, 1]²` to WGS84
    /// `(lon, lat)` degrees, where `fx = 0` is the west/left edge and `fy = 0`
    /// the north/top edge.
    ///
    /// `wgs84_bbox` is the request's WGS84 bounding box `[west, south, east,
    /// north]`, used by the `Wgs84` and `WebMercator` variants. The `Projected`
    /// variant ignores it and instead interpolates linearly in its own carried
    /// projected metres before inverse-projecting, so output pixels are square
    /// in the requested projection (the inverse may be non-finite outside the
    /// projection's valid domain — callers finite-check, exactly as they do for
    /// `GeoTransform::world_to_pixel`).
    ///
    /// This is the single shared output→world mapping for every `MapEngine`
    /// (#160/#251): engines feed it to [`crate::resample::ProjectionGrid`] (or
    /// call it per pixel for non-gridded sources) instead of re-deriving the
    /// per-CRS axis math.
    pub fn project_node(&self, wgs84_bbox: [f64; 4], fx: f64, fy: f64) -> (f64, f64) {
        let [west, south, east, north] = wgs84_bbox;
        match self {
            OutputCrs::Wgs84 => (west + fx * (east - west), north - fy * (north - south)),
            OutputCrs::WebMercator => {
                // Pixels are equally spaced in Mercator Y metres: interpolate in
                // Mercator Y, then convert back to latitude.
                let (my_n, my_s) = (lat_to_merc_y(north), lat_to_merc_y(south));
                (
                    west + fx * (east - west),
                    merc_y_to_lat(my_n - fy * (my_n - my_s)),
                )
            }
            OutputCrs::Projected { crs, bbox } => {
                let [min_e, min_n, max_e, max_n] = bbox;
                let e = min_e + fx * (max_e - min_e);
                let n = max_n - fy * (max_n - min_n);
                crs.inverse(e, n).unwrap_or((f64::NAN, f64::NAN))
            }
        }
    }

    /// Inverse of [`Self::project_node`]: map a WGS84 `(lon, lat)` to the
    /// fractional output position `(fx, fy)` (`fx = 0` west/left edge,
    /// `fy = 0` north/top edge). Results may fall outside `[0, 1]²` (the
    /// point is off-tile) or be non-finite (outside a projection's valid
    /// domain) — callers bounds/finite-check.
    ///
    /// For **per-vertex** use only (painting overlay geometry, locating a
    /// handful of points): the per-pixel direction stays
    /// [`Self::project_node`] via `ProjectionGrid` (never project per
    /// pixel).
    pub fn world_to_fraction(&self, wgs84_bbox: [f64; 4], lon: f64, lat: f64) -> (f64, f64) {
        let [west, south, east, north] = wgs84_bbox;
        match self {
            OutputCrs::Wgs84 => (
                (lon - west) / (east - west),
                (north - lat) / (north - south),
            ),
            OutputCrs::WebMercator => {
                let (my_n, my_s) = (lat_to_merc_y(north), lat_to_merc_y(south));
                (
                    (lon - west) / (east - west),
                    (my_n - lat_to_merc_y(lat)) / (my_n - my_s),
                )
            }
            OutputCrs::Projected { crs, bbox } => {
                // `wgs84_bbox` is not used here: the projected tile extents
                // are already embedded in this variant's `bbox` field
                // (mirroring `project_node`).
                let [min_e, min_n, max_e, max_n] = bbox;
                let (e, n) = crs.forward(lon, lat);
                ((e - min_e) / (max_e - min_e), (max_n - n) / (max_n - min_n))
            }
        }
    }
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
#[derive(Debug, Clone)]
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
    /// The collection's vertical axis, when it has one (e.g. radar elevation
    /// sweeps, pressure levels). `None` for collections with no vertical
    /// dimension.
    pub vertical: Option<VerticalDimension>,
    /// Native grid cell counts `[x_cells, y_cells]` (columns, rows), used to
    /// advertise spatial resolution via OGC API Common Part 2
    /// `extent.spatial.grid`. `None` when the source has no regular geographic
    /// grid (e.g. polar radar volumes) or when the cell counts are not cheaply
    /// available without decoding data.
    pub grid_size: Option<[u32; 2]>,
    /// Optional short label distinguishing this layer from sibling layers that
    /// share a parent grouping (e.g. a radar site place name like "Vihti").
    /// WMS prepends it to child-layer titles so flat clients that ignore the
    /// parent-layer tree can still tell siblings apart. `None` for standalone
    /// collections.
    pub layer_subtitle: Option<String>,
    /// Available forecast model runs (reference times). **Contract: sorted
    /// ascending, so the latest run is `.last()`** — engines build this from a
    /// reference-time-keyed `BTreeMap`, and consumers depend on the ordering
    /// (WMS advertises `.last()` as the `reference_time` dimension's `default`).
    /// Empty for non-forecast collections. WMS surfaces these as a custom
    /// `reference_time` dimension and `get_raster_tile`'s `reference_time`
    /// argument selects one (`None` ⇒ latest); see [`crate::instances`].
    pub reference_times: Vec<DateTime<Utc>>,
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
    /// The optional `z` selects a vertical level (e.g. radar elevation angle,
    /// pressure level). Engines with no vertical dimension ignore it; engines
    /// that have one resolve it against `raster_info().vertical`.
    ///
    /// The optional `reference_time` selects a forecast model run against
    /// `raster_info().reference_times` (`None` ⇒ the latest run, the default and
    /// only behaviour for non-forecast engines, which ignore it). See
    /// [`crate::instances`].
    ///
    /// The engine handles CRS reprojection to source data internally.
    #[allow(clippy::too_many_arguments)] // bbox/size/time/crs/parameter/z/reference_time are all genuine selectors
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::projected_output_crs;

    const FINLAND_WGS84: [f64; 4] = [19.0, 59.0, 32.0, 70.0]; // [w, s, e, n]

    #[test]
    fn project_node_wgs84_is_linear_corners() {
        let crs = OutputCrs::Wgs84;
        // fx=0,fy=0 is the NW corner (west, north); fx=1,fy=1 is SE (east, south).
        assert_eq!(crs.project_node(FINLAND_WGS84, 0.0, 0.0), (19.0, 70.0));
        assert_eq!(crs.project_node(FINLAND_WGS84, 1.0, 1.0), (32.0, 59.0));
        // The centre is the arithmetic midpoint (linear in both axes).
        let (lon, lat) = crs.project_node(FINLAND_WGS84, 0.5, 0.5);
        assert!((lon - 25.5).abs() < 1e-9 && (lat - 64.5).abs() < 1e-9);
    }

    #[test]
    fn project_node_web_mercator_pins_corners_and_bows_centre() {
        let crs = OutputCrs::WebMercator;
        // Longitude is still linear; the corner latitudes are exact.
        let (lon0, lat0) = crs.project_node(FINLAND_WGS84, 0.0, 0.0);
        assert!((lon0 - 19.0).abs() < 1e-9 && (lat0 - 70.0).abs() < 1e-9);
        let (_, lat1) = crs.project_node(FINLAND_WGS84, 1.0, 1.0);
        assert!((lat1 - 59.0).abs() < 1e-9);
        // The mid-row latitude sits north of the linear midpoint (Mercator rows
        // are equally spaced in metres, which compress toward the pole).
        let (_, latm) = crs.project_node(FINLAND_WGS84, 0.5, 0.5);
        assert!(
            latm > 64.5,
            "Mercator mid-row {latm} should exceed linear 64.5"
        );
    }

    #[test]
    fn project_node_projected_inverts_projected_metres() {
        // Build a projected bbox by forward-projecting a known lon/lat, then
        // confirm project_node inverts the correct corner — proving the bbox is
        // read as metres and inverse-projected, not treated as degrees (#251).
        let proj = projected_output_crs("EPSG:3067").unwrap();
        let (e_w, n_s) = proj.forward(20.0, 60.0); // SW-ish in projected space
        let (e_e, n_n) = proj.forward(30.0, 68.0); // NE-ish
        let bbox = [e_w.min(e_e), n_s.min(n_n), e_w.max(e_e), n_s.max(n_n)];
        let out = OutputCrs::Projected {
            crs: proj.clone(),
            bbox,
        };
        // NW corner (fx=0,fy=0) → (min_e, max_n) inverse.
        let expect = proj.inverse(bbox[0], bbox[3]).unwrap();
        let got = out.project_node([0.0; 4], 0.0, 0.0); // wgs84_bbox is ignored
        assert!((got.0 - expect.0).abs() < 1e-9 && (got.1 - expect.1).abs() < 1e-9);
        // A degrees-as-metres bug would land lon/lat in the millions; assert sane.
        let (lon, lat) = out.project_node([0.0; 4], 0.5, 0.5);
        assert!(
            (15.0..35.0).contains(&lon) && (55.0..72.0).contains(&lat),
            "{lon},{lat}"
        );
    }

    #[test]
    fn world_to_fraction_inverts_project_node_for_every_variant() {
        let proj = projected_output_crs("EPSG:3067").unwrap();
        let (e_w, n_s) = proj.forward(20.0, 60.0);
        let (e_e, n_n) = proj.forward(30.0, 68.0);
        let variants = [
            OutputCrs::Wgs84,
            OutputCrs::WebMercator,
            OutputCrs::Projected {
                crs: proj,
                bbox: [e_w.min(e_e), n_s.min(n_n), e_w.max(e_e), n_s.max(n_n)],
            },
        ];
        for out in &variants {
            for &(fx, fy) in &[(0.0, 0.0), (1.0, 1.0), (0.25, 0.75), (0.5, 0.5)] {
                let (lon, lat) = out.project_node(FINLAND_WGS84, fx, fy);
                let (gx, gy) = out.world_to_fraction(FINLAND_WGS84, lon, lat);
                assert!(
                    (gx - fx).abs() < 1e-9 && (gy - fy).abs() < 1e-9,
                    "{out:?}: ({fx},{fy}) → ({lon},{lat}) → ({gx},{gy})"
                );
            }
            // Off-tile points land outside [0,1] rather than clamping.
            let (gx, _) = out.world_to_fraction(FINLAND_WGS84, 5.0, 64.0);
            assert!(gx < 0.0, "west of the tile must map to fx < 0, got {gx}");
        }
    }
}
