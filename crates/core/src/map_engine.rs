use std::sync::Arc;

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

// Web Mercator output-axis math comes from the shared `crate::web_mercator`
// module — the single source of truth for EPSG:3857 ↔ WGS84 so the meta-tile
// assembly, WMS/Tiles bbox conversions, and these output axes can't drift apart
// (#452).
use crate::web_mercator::{lat_to_y as lat_to_merc_y, y_to_lat as merc_y_to_lat};

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
    /// Row-major pixel values.
    pub values: RasterValues,
}

/// Pixel storage for a [`RasterTile`] (#206).
///
/// `F64` is the universal boxed form every engine can produce: 16 bytes per
/// pixel (`Option<f64>`), `None` = nodata. That representation is a 16×
/// inflation over 1-byte sources and its memory traffic dominates cold-render
/// cost under concurrency, so integer-typed render paths should produce the
/// compact `U8` form instead: raw samples plus the decode parameters
/// (`physical = raw as f64 * gain + offset`, `raw == nodata` ⇒ transparent),
/// which the renderer colorizes through a 256-entry LUT indexed directly by
/// the raw byte — no per-pixel float math or boxing anywhere in the pipeline.
/// Float render paths (#475) should produce `F32`: 4 bytes per pixel, still
/// colorized per pixel (a float cannot index a LUT).
///
/// Consumers must treat the variants as equivalent descriptions of the same
/// pixels: for every index, colorizing `U8`/`F32` must produce the
/// byte-identical RGBA that boxing the same sample to `F64` would (the LUT
/// entry is *defined* as `colormap.color(value_at(i))`; `F32` colorizes
/// [`RasterValues::decode_f32`], the same function `value_at` uses).
pub enum RasterValues {
    /// Row-major boxed physical values. None = nodata (transparent).
    F64(Vec<Option<f64>>),
    /// Row-major raw 8-bit samples + decode parameters.
    U8 {
        data: Vec<u8>,
        /// Raw value meaning nodata (transparent). `None` ⇒ every sample is
        /// a real value.
        nodata: Option<u8>,
        /// `physical = raw as f64 * gain + offset`.
        gain: f64,
        offset: f64,
    },
    /// Row-major physical values as `f32`. NaN and ±∞ are always nodata
    /// (transparent) — the natural "missing" of a float grid.
    F32 {
        data: Vec<f32>,
        /// Additional finite sentinel meaning nodata (e.g. a source's
        /// `-9999`). `None` ⇒ only non-finite samples are nodata.
        nodata: Option<f32>,
    },
}

impl From<Vec<Option<f64>>> for RasterValues {
    fn from(values: Vec<Option<f64>>) -> Self {
        RasterValues::F64(values)
    }
}

impl RasterValues {
    /// Number of pixels.
    pub fn len(&self) -> usize {
        match self {
            RasterValues::F64(v) => v.len(),
            RasterValues::U8 { data, .. } => data.len(),
            RasterValues::F32 { data, .. } => data.len(),
        }
    }

    /// Boxed-equivalent physical value of one `F32` sample: `None` for the
    /// sentinel and for NaN/±∞. The single definition shared by
    /// [`Self::value_at`] and the renderer's `F32` colorize, so the two
    /// cannot disagree on which pixels are transparent.
    #[inline]
    pub fn decode_f32(raw: f32, nodata: Option<f32>) -> Option<f64> {
        if !raw.is_finite() || Some(raw) == nodata {
            None
        } else {
            Some(f64::from(raw))
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Physical value at `idx`: `None` for nodata or out of range. The
    /// boxed-equivalent view of any variant — colorize fast paths must
    /// match this exactly.
    pub fn value_at(&self, idx: usize) -> Option<f64> {
        match self {
            RasterValues::F64(v) => v.get(idx).copied().flatten(),
            RasterValues::U8 {
                data,
                nodata,
                gain,
                offset,
            } => data.get(idx).and_then(|&raw| {
                if Some(raw) == *nodata {
                    None
                } else {
                    Some(raw as f64 * gain + offset)
                }
            }),
            RasterValues::F32 { data, nodata } => data
                .get(idx)
                .and_then(|&raw| Self::decode_f32(raw, *nodata)),
        }
    }

    /// Iterate the pixels as boxed-equivalent physical values (the `F64`
    /// view of any variant).
    pub fn iter_values(&self) -> impl Iterator<Item = Option<f64>> + '_ {
        (0..self.len()).map(move |i| self.value_at(i))
    }

    /// True when every pixel is nodata.
    pub fn is_all_nodata(&self) -> bool {
        match self {
            RasterValues::F64(v) => v.iter().all(Option::is_none),
            RasterValues::U8 { data, nodata, .. } => match nodata {
                Some(nd) => data.iter().all(|b| b == nd),
                // No nodata sentinel ⇒ every sample is a real value.
                None => data.is_empty(),
            },
            RasterValues::F32 { data, nodata } => data
                .iter()
                .all(|&raw| Self::decode_f32(raw, *nodata).is_none()),
        }
    }
}

impl RasterTile {
    /// Returns true if all pixel values are nodata.
    pub fn is_empty(&self) -> bool {
        self.values.is_all_nodata()
    }
}

/// A selectable raster parameter. Units describe the values returned by the
/// engine (after any display conversion); an empty unit means unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    /// Stable selector used in layer names and parameter-name queries.
    pub name: String,
    /// Human-readable parameter label.
    pub title: String,
    /// Unit of the returned raster values; empty when unknown.
    pub unit: String,
}

/// One channel of an RGB composite ([`CompositeDef`]): the physical value of
/// `parameter`, or the difference `parameter - minus`, stretched so `min`
/// gives intensity 0 and `max` full intensity, then shaped by `gamma`. The
/// API layer renders it with `ds_render::composite`, whose range, gamma and
/// nodata conventions these fields follow: `min > max` inverts the channel.
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeChannel {
    /// The parameter read, or the minuend of a difference.
    pub parameter: String,
    /// The parameter subtracted from `parameter`, for a band difference.
    pub minus: Option<String>,
    /// Physical value that maps to intensity 0.
    pub min: f64,
    /// Physical value that maps to full intensity.
    pub max: f64,
    /// Gamma, finite and `> 0`. `1.0` is a linear stretch.
    pub gamma: f64,
}

impl CompositeChannel {
    /// The parameters this channel reads, minuend first.
    pub fn parameters(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.parameter.as_str()).chain(self.minus.as_deref())
    }
}

/// An RGB composite a multi-parameter collection serves as its own layer
/// (#819): three channels computed from the collection's parameters, all
/// read from one timestep every parameter has. It has no numeric values of
/// its own, so it is not in [`RasterInfo::parameters`] and EDR does not
/// serve it. See [`MapEngine::composites`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompositeDef {
    /// Layer name, distinct from every parameter and other composite of the
    /// collection.
    pub name: String,
    /// Human-readable layer title.
    pub title: String,
    /// Red, green and blue.
    pub channels: [CompositeChannel; 3],
}

impl CompositeDef {
    /// The distinct parameters the channels read, in first-use order: red's
    /// minuend, red's subtrahend, then green's and blue's. This is the
    /// `parameters` list to hand [`MapEngine::get_raster_tiles`] and
    /// [`MapEngine::resolve_parameters_time`], and the plane order that
    /// `ds_render`'s `CompositeSpec` built from this definition indexes.
    pub fn parameters(&self) -> Vec<&str> {
        let mut parameters: Vec<&str> = Vec::with_capacity(6);
        for parameter in self.channels.iter().flat_map(CompositeChannel::parameters) {
            if !parameters.contains(&parameter) {
                parameters.push(parameter);
            }
        }
        parameters
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
    /// Unit of the default parameter (e.g., "dBZ").
    pub unit: String,
    /// All available parameters. Empty means single-parameter engine (use `parameter`).
    /// Multi-parameter engines carry each parameter's own display unit.
    pub parameters: Vec<ParameterInfo>,
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

impl RasterInfo {
    /// Resolve a selected parameter's unit without borrowing another field's
    /// unit. The legacy default unit is used only when no matching descriptor
    /// exists and the request selects the collection default.
    pub fn parameter_unit(&self, parameter: Option<&str>) -> Option<&str> {
        let name = parameter.unwrap_or(&self.parameter);
        let unit = if let Some(p) = self.parameters.iter().find(|p| p.name == name) {
            p.unit.as_str()
        } else if name == self.parameter {
            self.unit.as_str()
        } else {
            return None;
        };
        (!unit.trim().is_empty()).then_some(unit)
    }
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

    /// Several parameters' tiles for ONE request geometry and ONE timestep,
    /// in the order of `parameters`: the bands of an RGB composite. The
    /// other arguments are [`Self::get_raster_tile`]'s.
    ///
    /// `time` is the instant [`Self::resolve_parameters_time`] returned for
    /// the same `parameters`; `None` renders the latest timestep they share.
    /// Every tile comes from that one timestep. An engine whose parameters
    /// have their own time axes ([`Self::parameter_times`]) MUST override
    /// this: it renders every band from exactly `time` and fails when a band
    /// has no data then, instead of snapping bands to different timesteps.
    ///
    /// Default: [`Self::get_raster_tile`] once per parameter with the same
    /// arguments, which is correct when the parameters share one time axis.
    /// An engine should still override it when it can build the
    /// output→source coordinate map (Critical Rule 5) once per distinct
    /// source grid and sample every band from it.
    #[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        parameters
            .iter()
            .map(|parameter| {
                self.get_raster_tile(
                    bbox,
                    width,
                    height,
                    time,
                    output_crs,
                    Some(parameter),
                    z,
                    reference_time,
                )
            })
            .collect()
    }

    /// Return metadata for capabilities documents.
    ///
    /// **Expected complexity: O(1) (or as close as practical).** Callers
    /// invoke this on the hot tile/map path to validate `?parameter-name=`
    /// against `parameters`, before acquiring the render semaphore. Engines
    /// should serve from an `ArcSwap`/`RwLock` snapshot, not recompute on
    /// every call. If your engine genuinely needs to derive metadata
    /// per-request, cache it.
    fn raster_info(&self) -> RasterInfo;

    /// Shared metadata for request paths. Engines with cached descriptors can
    /// override this to avoid cloning parameter and time vectors on every read.
    fn raster_info_shared(&self) -> std::sync::Arc<RasterInfo> {
        std::sync::Arc::new(self.raster_info())
    }

    /// An explicit default valid time, independent of the advertised axis.
    /// `None` means use the last advertised timestep (the existing forecast
    /// convention). Alert engines override this with their snapshot's "now"
    /// while advertising future warnings. Must be O(1), with no I/O.
    fn default_time(&self) -> Option<DateTime<Utc>> {
        None
    }

    /// Resolve a requested time to the **exact timestep this engine would
    /// render** for it — the timestamp that must key any cache of the
    /// rendered output (#507).
    ///
    /// Engines that snap a requested time to an available timestep (e.g.
    /// latest-not-after selection over a file catalog) MUST override this
    /// with the same selection logic `get_raster_tile` uses, so that a
    /// request for a not-yet-ingested time T caches the T−1 pixels it
    /// actually renders under T−1's key — never under T's. `None` input
    /// means "whatever the engine treats as latest"; the override should
    /// return that concrete timestep so caches pin it too.
    ///
    /// With nothing to render yet (the catalog is still empty after a start
    /// or reload), falling back to the requested time is safe only when the
    /// render then *errors*, since errors are never cached. An engine that
    /// renders an empty tile instead MUST return `None`: an empty render
    /// keyed on T is served blank from the no-TTL meta-tile cache once T's
    /// data lands. The API layers also send an explicit `time` that resolves
    /// to `None` with a short, revalidating `Cache-Control` rather than
    /// `immutable`.
    ///
    /// `reference_time` selects a forecast model run, mirroring
    /// [`Self::get_raster_tile`]; non-forecast engines ignore it.
    ///
    /// Default: identity — correct only for engines whose time selection is
    /// exact-match (a mismatch then fails the render rather than silently
    /// snapping). **Expected complexity: O(log n) from a snapshot** — this
    /// runs on the hot render path before the cache lookup.
    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let _ = reference_time;
        time
    }

    /// Resolve a requested model run to the **exact run this engine would
    /// render** for `(time, reference_time)` — the run-axis twin of
    /// [`Self::resolve_time`], and the value that must key any no-TTL cache
    /// of the rendered output (#521).
    ///
    /// Engines that retain model runs (non-empty
    /// [`RasterInfo::reference_times`]) MUST override this with the same run
    /// selection `get_raster_tile` uses — share one helper so they cannot
    /// drift. `None` input means "whatever run the engine would pick for
    /// this time" (usually the latest, possibly an older run when the
    /// newest doesn't cover the valid time yet); the override must return
    /// that concrete reference time so caches key the run actually
    /// rendered, not a floating "latest". An explicit `Some(rt)` normally
    /// echoes back unchanged (exact-match run pinning); a pinned run the
    /// engine no longer retains should also echo — the render will error
    /// and cache nothing, so the key value is moot.
    ///
    /// Default: identity — correct only for engines without model runs
    /// (`reference_times` empty), where `None` stays `None` and nothing
    /// regenerates under the same valid time. **Expected complexity:
    /// O(log n) from a snapshot** — this runs on the hot render path before
    /// the cache lookup.
    fn resolve_reference_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let _ = time;
        reference_time
    }

    /// A value that changes whenever the pixels this engine would render for
    /// a **fixed** `(time, reference_time, z, parameter)` can change. The
    /// API layers fold it into the no-TTL rendered / meta-tile cache keys,
    /// so an engine whose content for a given instant is *revised in place*
    /// invalidates its cached tiles by returning a new value.
    ///
    /// Most engines never need this: a radar frame, a forecast step or a
    /// COG timestep is immutable once ingested, so `(time, reference_time)`
    /// already keys the pixels exactly — keep the default `0`. Override it
    /// for content that accumulates or is corrected under the same
    /// timestamps — a push-fed alert set (engine-cap: a warning published
    /// later is active at instants that were already rendered and cached,
    /// so every explicit `TIME=` tile went stale) — with a cheap snapshot
    /// read (e.g. the `FeatureEngine::data_version` the engine already
    /// keeps). It must NOT change on a rebuild that changed nothing, or
    /// the caches would churn for no reason. `0` is reserved for "never
    /// revised" — an overriding engine must return a non-zero value, as the
    /// API layers also use it to decide between an `immutable` and a
    /// revalidating `Cache-Control` for explicit-`TIME` responses. **O(1)
    /// from a snapshot** — this runs on the hot render path before the
    /// cache lookup.
    fn content_version(&self) -> u64 {
        0
    }

    /// The timesteps of one parameter, when a multi-parameter collection's
    /// parameters are not all available at the same times — a satellite
    /// collection whose bands and derived products land minutes apart.
    /// `RasterInfo::times` is then the union over the parameters (the
    /// collection's temporal extent is an envelope); this is one parameter's
    /// own axis, advertised per layer where a standard allows it (a WMS child
    /// layer's `time` dimension).
    ///
    /// Ascending (oldest first), like `RasterInfo::times`.
    ///
    /// Default `None`: the parameter has every time in `RasterInfo::times`.
    /// **O(1) from a snapshot** (Critical Rule 10): WMS GetCapabilities
    /// calls this for every parameter layer.
    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        let _ = parameter;
        None
    }

    /// [`Self::resolve_time`] for one parameter: the exact timestep this
    /// engine would render for `parameter` at `time`, the instant that must
    /// key the rendered caches (#507). `None` parameter means the one
    /// `get_raster_tile` renders by default.
    ///
    /// An engine with [`Self::parameter_times`] MUST override this with the
    /// same per-parameter selection `get_raster_tile` uses, or a request for
    /// a time the parameter lacks would cache another timestep's pixels
    /// under that time's key. Default: [`Self::resolve_time`].
    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let _ = parameter;
        self.resolve_time(time, reference_time)
    }

    /// [`Self::resolve_reference_time`] for one parameter: the exact model
    /// run this engine would render `parameter` from at `time`, the run
    /// that must key the rendered caches (#521). `None` parameter means the
    /// one `get_raster_tile` renders by default. The API layers resolve the
    /// run with this first, then pass it to
    /// [`Self::resolve_parameter_time`] and the render.
    ///
    /// An engine that retains runs and whose run selection depends on the
    /// parameter MUST override this with the selection `get_raster_tile`
    /// uses. GRIB does (#1005): an hour-window aggregate missing from the
    /// newest run's first steps renders from an older run that has it at
    /// that time, so the parameter-blind run would key the wrong pixels.
    /// Default: [`Self::resolve_reference_time`]. **O(log n) from a
    /// snapshot**, before the cache lookup.
    fn resolve_parameter_reference_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let _ = parameter;
        self.resolve_reference_time(time, reference_time)
    }

    /// [`Self::resolve_parameter_time`] for parameters rendered together,
    /// such as an RGB composite's bands: the one timestep
    /// [`Self::get_raster_tiles`] renders them all from. The API layer must
    /// key the rendered and meta-tile caches on it (#507) and pass it on as
    /// `get_raster_tiles`' `time`, so no band is drawn from a timestep the
    /// cache key does not name.
    ///
    /// Default: the selection [`select_common_time`] makes over the
    /// parameters' own axes ([`Self::parameter_times`]): the latest shared
    /// timestep at or before `time`, the earliest shared one when `time`
    /// precedes them all, the latest shared one for `None`. It returns
    /// `None` when the parameters share no timestep. When no parameter has
    /// its own axis, it is [`Self::resolve_parameter_time`] of the first.
    ///
    /// An engine with per-parameter axes whose own selection differs MUST
    /// override this with the selection its `get_raster_tiles` uses. Runs on
    /// the hot render path before the cache lookup: from a snapshot, no I/O.
    fn resolve_parameters_time(
        &self,
        parameters: &[&str],
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let axes: Vec<Arc<[DateTime<Utc>]>> = parameters
            .iter()
            .filter_map(|parameter| self.parameter_times(parameter))
            .collect();
        if axes.is_empty() {
            return self.resolve_parameter_time(parameters.first().copied(), time, reference_time);
        }
        let axes: Vec<&[DateTime<Utc>]> = axes.iter().map(|axis| &**axis).collect();
        select_common_time(&axes, time)
    }

    /// The RGB composites this collection serves as their own layers, each
    /// rendered by passing [`CompositeDef::parameters`] to
    /// [`Self::get_raster_tiles`]. Composite names are not in
    /// [`RasterInfo::parameters`] and are not a `parameter` for
    /// [`Self::get_raster_tile`].
    ///
    /// An engine that serves composites MUST also answer for a composite's
    /// name in:
    /// - [`Self::parameter_times`]: the timesteps every one of its
    ///   parameters has, kept in the engine's snapshot, not intersected per
    ///   call.
    /// - [`Self::resolve_parameter_time`]: the timestep
    ///   [`Self::resolve_parameters_time`] picks for its parameters, through
    ///   the same helper, so the cache key names the timestep rendered
    ///   (#507).
    ///
    /// Default: none. **O(1) from a snapshot** (Critical Rule 10): the API
    /// layers read it per request and per capabilities layer.
    fn composites(&self) -> Arc<[CompositeDef]> {
        static NONE: std::sync::LazyLock<Arc<[CompositeDef]>> =
            std::sync::LazyLock::new(|| Arc::from([]));
        NONE.clone()
    }
}

/// The timestep a multi-parameter render uses, chosen among the times
/// present in every one of `axes` (each ascending): the latest at or before
/// `time`, the earliest when `time` precedes them all, the latest for
/// `None`. `None` when the axes share no time, or there are none.
///
/// The default [`MapEngine::resolve_parameters_time`], and the selection an
/// engine with per-parameter axes renders a composite with. It walks the
/// shortest axis outward from `time` and binary-searches the others, so
/// parameters that share their recent timesteps resolve in a few probes.
pub fn select_common_time(
    axes: &[&[DateTime<Utc>]],
    time: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let shortest = (0..axes.len()).min_by_key(|&i| axes[i].len())?;
    let candidates = axes[shortest];
    let shared = |t: &&DateTime<Utc>| {
        axes.iter()
            .enumerate()
            .all(|(i, axis)| i == shortest || axis.binary_search(t).is_ok())
    };
    let split = time.map_or(candidates.len(), |time| {
        candidates.partition_point(|t| *t <= time)
    });
    candidates[..split]
        .iter()
        .rev()
        .find(shared)
        .or_else(|| candidates[split..].iter().find(shared))
        .copied()
}

/// The instant a map request that omits `TIME`/`datetime` renders, before
/// [`MapEngine::resolve_parameter_time`] snaps it: the engine's own default
/// ([`MapEngine::default_time`]), else the requested parameter's latest
/// time, else the collection's latest. WMS, Maps and Tiles all call this so
/// an omitted time means the same thing on every API.
pub fn default_request_time(
    engine: &dyn MapEngine,
    info: &RasterInfo,
    parameter: Option<&str>,
) -> Option<DateTime<Utc>> {
    engine.default_time().or_else(|| {
        parameter
            .and_then(|p| engine.parameter_times(p))
            .map_or_else(|| info.times.last().copied(), |times| times.last().copied())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `default_request_time`: the engine's own default wins, then the
    /// parameter's latest time, then the collection's.
    #[test]
    fn default_request_time_prefers_engine_then_parameter_then_collection() {
        struct Engine(Option<DateTime<Utc>>);
        impl MapEngine for Engine {
            fn get_raster_tile(
                &self,
                _: [f64; 4],
                _: u32,
                _: u32,
                _: Option<DateTime<Utc>>,
                _: &OutputCrs,
                _: Option<&str>,
                _: Option<f64>,
                _: Option<DateTime<Utc>>,
            ) -> Result<RasterTile, DataServerError> {
                unreachable!()
            }
            fn raster_info(&self) -> RasterInfo {
                unreachable!()
            }
            fn default_time(&self) -> Option<DateTime<Utc>> {
                self.0
            }
            fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
                (parameter == "late").then(|| Arc::from(vec![at(1)]))
            }
        }
        fn at(hour: u32) -> DateTime<Utc> {
            format!("2026-09-26T{hour:02}:00:00Z").parse().unwrap()
        }
        let info = RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: None,
            times: vec![at(1), at(2)],
            parameter: String::new(),
            unit: String::new(),
            parameters: Vec::new(),
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        };
        let plain = Engine(None);
        assert_eq!(
            default_request_time(&plain, &info, Some("late")),
            Some(at(1))
        );
        assert_eq!(
            default_request_time(&plain, &info, Some("other")),
            Some(at(2))
        );
        assert_eq!(default_request_time(&plain, &info, None), Some(at(2)));
        assert_eq!(
            default_request_time(&Engine(Some(at(5))), &info, Some("late")),
            Some(at(5))
        );
        // The default resolution is `resolve_time`'s identity, and the run
        // `resolve_reference_time`'s.
        assert_eq!(
            plain.resolve_parameter_time(Some("late"), Some(at(2)), None),
            Some(at(2))
        );
        assert_eq!(
            plain.resolve_parameter_reference_time(Some("late"), Some(at(2)), Some(at(1))),
            Some(at(1))
        );
    }

    fn hour(hour: u32) -> DateTime<Utc> {
        format!("2026-09-29T{hour:02}:00:00Z").parse().unwrap()
    }

    /// The default `get_raster_tiles` renders each parameter through
    /// `get_raster_tile` with the request's arguments, in order, and fails
    /// when one band fails.
    #[test]
    fn default_get_raster_tiles_loops_get_raster_tile() {
        type Call = (Option<String>, Option<DateTime<Utc>>, Option<f64>);
        struct Recorder(std::sync::Mutex<Vec<Call>>);
        const BANDS: [&str; 3] = ["red", "green", "blue"];
        impl MapEngine for Recorder {
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
            ) -> Result<RasterTile, DataServerError> {
                assert_eq!(bbox, [1.0, 2.0, 3.0, 4.0]);
                assert_eq!(output_crs, &OutputCrs::WebMercator);
                assert_eq!(reference_time, Some(hour(0)));
                self.0
                    .lock()
                    .unwrap()
                    .push((parameter.map(String::from), time, z));
                let band = BANDS
                    .iter()
                    .position(|b| Some(*b) == parameter)
                    .ok_or_else(|| DataServerError::InvalidParameter("unknown".into()))?;
                Ok(RasterTile {
                    width,
                    height,
                    values: vec![Some(band as f64); (width * height) as usize].into(),
                })
            }
            fn raster_info(&self) -> RasterInfo {
                unreachable!()
            }
        }
        let engine = Recorder(Default::default());
        let render = |parameters: &[&str]| {
            engine.get_raster_tiles(
                [1.0, 2.0, 3.0, 4.0],
                2,
                3,
                Some(hour(6)),
                &OutputCrs::WebMercator,
                parameters,
                Some(500.0),
                Some(hour(0)),
            )
        };
        let tiles = render(&["blue", "red", "green", "red"]).unwrap();
        let bands: Vec<Option<f64>> = tiles.iter().map(|t| t.values.value_at(0)).collect();
        assert_eq!(bands, [Some(2.0), Some(0.0), Some(1.0), Some(0.0)]);
        assert!(tiles.iter().all(|t| (t.width, t.height) == (2, 3)));
        let calls = std::mem::take(&mut *engine.0.lock().unwrap());
        assert_eq!(
            calls,
            ["blue", "red", "green", "red"].map(|p| (
                Some(p.to_string()),
                Some(hour(6)),
                Some(500.0)
            ))
        );
        assert!(render(&[]).unwrap().is_empty());
        assert!(render(&["red", "nope"]).is_err());
    }

    /// The default `resolve_parameters_time` picks among the timesteps every
    /// parameter has, with `resolve_parameter_time`'s latest-not-after rule.
    #[test]
    fn default_resolve_parameters_time_intersects_the_parameter_axes() {
        struct Axes;
        impl MapEngine for Axes {
            fn get_raster_tile(
                &self,
                _: [f64; 4],
                _: u32,
                _: u32,
                _: Option<DateTime<Utc>>,
                _: &OutputCrs,
                _: Option<&str>,
                _: Option<f64>,
                _: Option<DateTime<Utc>>,
            ) -> Result<RasterTile, DataServerError> {
                unreachable!()
            }
            fn raster_info(&self) -> RasterInfo {
                unreachable!()
            }
            /// Engines without per-parameter axes snap through this.
            fn resolve_time(
                &self,
                _: Option<DateTime<Utc>>,
                _: Option<DateTime<Utc>>,
            ) -> Option<DateTime<Utc>> {
                Some(hour(9))
            }
            fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
                let hours: &[u32] = match parameter {
                    "ir" => &[1, 2, 3, 4],
                    "wv" => &[2, 4],
                    "late" => &[5],
                    _ => return None,
                };
                Some(hours.iter().map(|&h| hour(h)).collect())
            }
        }
        let resolve =
            |parameters: &[&str], time| Axes.resolve_parameters_time(parameters, time, None);
        // The latest shared timestep, at or before the request.
        assert_eq!(resolve(&["ir", "wv"], None), Some(hour(4)));
        assert_eq!(resolve(&["ir", "wv"], Some(hour(4))), Some(hour(4)));
        assert_eq!(resolve(&["wv", "ir"], Some(hour(3))), Some(hour(2)));
        assert_eq!(resolve(&["ir", "wv", "ir"], Some(hour(3))), Some(hour(2)));
        // Before every shared timestep: the earliest, as a single parameter.
        assert_eq!(resolve(&["ir", "wv"], Some(hour(0))), Some(hour(2)));
        assert_eq!(resolve(&["ir"], Some(hour(0))), Some(hour(1)));
        assert_eq!(resolve(&["ir"], Some(hour(3))), Some(hour(3)));
        // No shared timestep.
        assert_eq!(resolve(&["ir", "late"], None), None);
        assert_eq!(resolve(&["ir", "late"], Some(hour(5))), None);
        // A parameter without its own axis constrains nothing; with no axis
        // at all the engine's own resolution applies.
        assert_eq!(resolve(&["plain", "wv"], None), Some(hour(4)));
        assert_eq!(resolve(&["plain"], Some(hour(1))), Some(hour(9)));
        assert_eq!(resolve(&[], Some(hour(1))), Some(hour(9)));
    }

    #[test]
    fn select_common_time_handles_empty_axes() {
        assert_eq!(select_common_time(&[], None), None);
        assert_eq!(select_common_time(&[&[]], Some(hour(1))), None);
        let one = [hour(1)];
        assert_eq!(select_common_time(&[&one, &[]], None), None);
        assert_eq!(
            select_common_time(&[&one, &one], Some(hour(0))),
            Some(hour(1))
        );
    }

    /// A composite's parameters are the distinct ones its channels read, in
    /// first-use order, minuend before subtrahend.
    #[test]
    fn composite_parameters_are_distinct_in_first_use_order() {
        let channel = |parameter: &str, minus: Option<&str>| CompositeChannel {
            parameter: parameter.into(),
            minus: minus.map(String::from),
            min: 0.0,
            max: 1.0,
            gamma: 1.0,
        };
        let def = CompositeDef {
            name: "airmass".into(),
            title: "Airmass RGB".into(),
            channels: [
                channel("wv_6_2", Some("wv_7_3")),
                channel("ir_9_6", Some("ir_10_3")),
                channel("wv_6_2", None),
            ],
        };
        assert_eq!(def.parameters(), ["wv_6_2", "wv_7_3", "ir_9_6", "ir_10_3"]);
        assert!(def.channels[0].parameters().eq(["wv_6_2", "wv_7_3"]));
        let one_band = CompositeDef {
            channels: [
                channel("ir", None),
                channel("ir", None),
                channel("ir", Some("ir")),
            ],
            ..def
        };
        assert_eq!(one_band.parameters(), ["ir"]);
    }

    /// An engine serves no composites unless it says so, and the default
    /// hands out one shared empty list rather than allocating per call.
    #[test]
    fn default_composites_are_none() {
        struct Plain;
        impl MapEngine for Plain {
            fn get_raster_tile(
                &self,
                _: [f64; 4],
                _: u32,
                _: u32,
                _: Option<DateTime<Utc>>,
                _: &OutputCrs,
                _: Option<&str>,
                _: Option<f64>,
                _: Option<DateTime<Utc>>,
            ) -> Result<RasterTile, DataServerError> {
                unreachable!()
            }
            fn raster_info(&self) -> RasterInfo {
                unreachable!()
            }
        }
        assert!(Plain.composites().is_empty());
        assert!(Arc::ptr_eq(&Plain.composites(), &Plain.composites()));
    }
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

    /// #475: the `F32` boxed view — sentinel and every non-finite sample are
    /// nodata, everything else widens exactly.
    #[test]
    fn f32_values_box_to_exact_widened_values() {
        let values = RasterValues::F32 {
            data: vec![1.5, -9999.0, f32::NAN, f32::INFINITY, -0.1, f32::MAX],
            nodata: Some(-9999.0),
        };
        assert_eq!(values.len(), 6);
        assert_eq!(
            values.iter_values().collect::<Vec<_>>(),
            vec![
                Some(1.5),
                None,
                None,
                None,
                Some(f64::from(-0.1f32)),
                Some(f64::from(f32::MAX)),
            ]
        );
        assert_eq!(values.value_at(6), None, "out of range");
        assert!(!values.is_all_nodata());

        let blank = RasterValues::F32 {
            data: vec![f32::NAN, -9999.0, f32::NEG_INFINITY],
            nodata: Some(-9999.0),
        };
        assert!(blank.is_all_nodata());
        // Without a sentinel the same finite value is real data.
        let real = RasterValues::F32 {
            data: vec![f32::NAN, -9999.0],
            nodata: None,
        };
        assert!(!real.is_all_nodata());
        assert_eq!(real.value_at(1), Some(-9999.0));
    }
}
