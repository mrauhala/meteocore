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

    /// Inclusive output-pixel window `(px_lo, px_hi, py_lo, py_hi)` that a source
    /// raster's footprint can occupy in a `width`×`height` output image — a cheap
    /// domain guard for projected-raster resampling.
    ///
    /// `src_env_wgs84` is the source raster's WGS84 envelope `[w, s, e, n]`;
    /// `wgs84_bbox` is the requested tile bbox (for `Wgs84`/`WebMercator` output;
    /// it is **ignored for `Projected` output**, whose extents are embedded in the
    /// `Projected { crs, bbox }` variant — same as [`Self::world_to_fraction`]).
    /// The envelope perimeter is mapped
    /// to output-fraction space with [`Self::world_to_fraction`] (the per-vertex
    /// inverse of the per-pixel [`Self::project_node`] — only ~130 perimeter
    /// points, never per output pixel, per the #203 rule), the fractional extent
    /// is taken and expanded by a small margin so genuine boundary data is never
    /// clipped, then converted to inclusive pixel bounds clamped to the image.
    ///
    /// Purpose: at low zoom the coarse [`crate::resample::ProjectionGrid`] (and
    /// the source projection's own out-of-domain forward) can map a far-away
    /// output pixel onto a valid source pixel, painting "ghost" data far from
    /// the real coverage (e.g. radar echoes in the Arctic on a whole-world Web
    /// Mercator view that wraps past ±180°). Pixels outside this window are
    /// dropped to nodata. Shared by every projected raster engine (#449).
    ///
    /// If no perimeter sample yields a finite fraction (e.g. a projected output
    /// CRS whose inverse is undefined across the whole envelope) the guard is
    /// disabled (full image) rather than risk clipping real data.
    ///
    /// **Requires `src_env_wgs84` to be a true WGS84 `[w, s, e, n]` envelope** —
    /// it is fed to [`Self::world_to_fraction`] as lon/lat. A native-CRS extent
    /// (projected metres) would produce a nonsense window and silently disable
    /// the guard; engines must reproject to WGS84 before calling.
    ///
    /// **Limitations:**
    /// - A single output-space box, so on a viewport showing more than one world
    ///   copy (Web Mercator spanning > 360° of longitude) only the primary copy
    ///   of the footprint is kept; wrapped copies render as nodata (acceptable —
    ///   the alternative was ghost aliasing).
    /// - The perimeter walk assumes `w <= e` (and `s <= n`). An
    ///   **antimeridian-crossing** envelope (`w > e`, e.g. `w=170, e=-170`) steps
    ///   backwards through the interior instead of wrapping over ±180°, yielding
    ///   an over-wide window that effectively disables the guard for that source.
    ///   No current data type crosses the antimeridian (European/national
    ///   composites, regional rasters, geographic Zarr grids); revisit if one is
    ///   added.
    pub fn footprint_pixel_window(
        &self,
        wgs84_bbox: [f64; 4],
        src_env_wgs84: [f64; 4],
        width: u32,
        height: u32,
    ) -> (u32, u32, u32, u32) {
        let [w, s, e, n] = src_env_wgs84;
        let (mut fx_lo, mut fx_hi, mut fy_lo, mut fy_hi) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        let mut any = false;
        const STEPS: usize = 32;
        for i in 0..=STEPS {
            let t = i as f64 / STEPS as f64;
            let lon = w + t * (e - w);
            let lat = s + t * (n - s);
            // All four envelope edges (curved edges can bow past the corners).
            for (plon, plat) in [(lon, s), (lon, n), (w, lat), (e, lat)] {
                let (fx, fy) = self.world_to_fraction(wgs84_bbox, plon, plat);
                if fx.is_finite() && fy.is_finite() {
                    any = true;
                    fx_lo = fx_lo.min(fx);
                    fx_hi = fx_hi.max(fx);
                    fy_lo = fy_lo.min(fy);
                    fy_hi = fy_hi.max(fy);
                }
            }
        }
        if !any {
            return (0, width.saturating_sub(1), 0, height.saturating_sub(1));
        }
        // Margin: a fraction of the footprint's own output span, with a small
        // floor, so edge-sampling gaps and sub-pixel rounding never clip boundary
        // data. Far-away ghosts sit far outside, so the margin never readmits them.
        let mx = ((fx_hi - fx_lo) * 0.02).max(0.005);
        let my = ((fy_hi - fy_lo) * 0.02).max(0.005);
        fx_lo -= mx;
        fx_hi += mx;
        fy_lo -= my;
        fy_hi += my;
        let to_px = |f: f64, dim: u32| {
            // If dim == 0, `clamp(0.0, dim as f64 - 1.0)` panics (min > max) in
            // both debug and release; the assert below fires first in debug with
            // a clearer message. Callers always pass positive output dimensions.
            debug_assert!(
                dim > 0,
                "footprint_pixel_window: output dimension must be > 0"
            );
            (f * dim as f64).floor().clamp(0.0, dim as f64 - 1.0) as u32
        };
        (
            to_px(fx_lo, width),
            to_px(fx_hi, width),
            to_px(fy_lo, height),
            to_px(fy_hi, height),
        )
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

    #[test]
    fn footprint_window_bounds_a_small_source_in_a_wide_view() {
        // A whole-world-ish Web Mercator view; the Finland footprint occupies
        // only a narrow band, so the guard window must be a small sub-rectangle
        // — NOT the full image (that's what kills ghost echoes outside it).
        let crs = OutputCrs::WebMercator;
        let view = [-160.0, -60.0, 160.0, 80.0]; // wide [w,s,e,n]
        let (w, h) = (1000u32, 1000u32);
        let (px_lo, px_hi, py_lo, py_hi) = crs.footprint_pixel_window(view, FINLAND_WGS84, w, h);
        assert!(px_lo < px_hi && py_lo < py_hi, "window must be non-empty");
        // Finland (lon 19..32 of a -160..160 span) sits left-of-centre and is
        // narrow; the window must exclude the far edges where ghosts appear.
        assert!(
            px_lo > 0 && px_hi < w - 1,
            "x window must not span the image"
        );
        assert!(
            py_lo > 0 && py_hi < h - 1,
            "y window must not span the image"
        );
        // The footprint centre (lon 25.5, lat ~64.5) must fall inside the window.
        let (cfx, cfy) = crs.world_to_fraction(view, 25.5, 64.5);
        let (cx, cy) = ((cfx * w as f64) as u32, (cfy * h as f64) as u32);
        assert!(
            (px_lo..=px_hi).contains(&cx) && (py_lo..=py_hi).contains(&cy),
            "footprint centre must be inside the window"
        );
    }

    #[test]
    fn footprint_window_bounds_source_for_projected_output() {
        // ODIM COMP serves EPSG:3067 national-grid requests via `Projected`
        // output (a different `world_to_fraction` path — `crs.forward` against the
        // embedded projected bbox). A view wider than the source footprint must
        // yield a sub-window, and the footprint centre must fall inside it.
        let crs = projected_output_crs("EPSG:3067").unwrap();
        // Request rectangle in EPSG:3067 metres, deliberately wider than Finland.
        let (e0, n0) = crs.forward(5.0, 53.0);
        let (e1, n1) = crs.forward(45.0, 74.0);
        let out = OutputCrs::Projected {
            crs,
            bbox: [e0.min(e1), n0.min(n1), e0.max(e1), n0.max(n1)],
        };
        let (w, h) = (800u32, 800u32);
        // `wgs84_bbox` is ignored for `Projected`; pass the footprint either way.
        let (px_lo, px_hi, py_lo, py_hi) =
            out.footprint_pixel_window(FINLAND_WGS84, FINLAND_WGS84, w, h);
        assert!(px_lo < px_hi && py_lo < py_hi, "window must be non-empty");
        assert!(
            px_lo > 0 || px_hi < w - 1 || py_lo > 0 || py_hi < h - 1,
            "a view wider than the footprint must not span the full image"
        );
        let (cfx, cfy) = out.world_to_fraction(FINLAND_WGS84, 25.5, 64.5);
        let (cx, cy) = ((cfx * w as f64) as u32, (cfy * h as f64) as u32);
        assert!(
            (px_lo..=px_hi).contains(&cx) && (py_lo..=py_hi).contains(&cy),
            "footprint centre must be inside the window"
        );
    }

    #[test]
    fn footprint_window_is_permissive_when_view_is_inside_the_source() {
        // Zoomed INTO the data (view ⊂ footprint): the window must cover the
        // whole image so nothing legitimate is clipped.
        let crs = OutputCrs::WebMercator;
        let tight = [24.0, 63.0, 26.0, 65.0]; // well inside FINLAND_WGS84
        let (w, h) = (256u32, 256u32);
        let (px_lo, px_hi, py_lo, py_hi) = crs.footprint_pixel_window(tight, FINLAND_WGS84, w, h);
        assert_eq!((px_lo, px_hi, py_lo, py_hi), (0, w - 1, 0, h - 1));
    }
}
