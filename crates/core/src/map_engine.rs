use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::feature_pick::{FeaturePick, PickedFeatures};
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
                let (e, n) = projected_point(bbox, fx, fy);
                crs.inverse(e, n).unwrap_or((f64::NAN, f64::NAN))
            }
        }
    }

    /// The WGS84 `(lon, lat)` of the output position `(fx, fy)`, mapped as
    /// [`Self::project_node`] maps it, when that point lies on the Earth;
    /// `None` when it does not. For a single point, such as the pixel
    /// centre a WMS GetFeatureInfo reads (#1064), never per output pixel.
    ///
    /// Off the Earth are a geographic point past a pole, and a projected
    /// point outside its projection's valid area
    /// ([`crate::geo::outside_projection_domain`], #1089), where its inverse
    /// is `None` and `project_node` NaN: past the LAEA disk, and past a pole
    /// or Transverse Mercator's eastings where its series stops being exact
    /// ([`crate::geo::Crs::TransverseMercator`], #1113). Every point of a
    /// projected box wholly outside its projection is outside. The
    /// `Projected` variant ignores `wgs84_bbox`, as `project_node` does, so
    /// a box with no WGS84 envelope may pass NaN.
    pub fn position_on_earth(&self, wgs84_bbox: [f64; 4], fx: f64, fy: f64) -> Option<(f64, f64)> {
        let (lon, lat) = self.project_node(wgs84_bbox, fx, fy);
        (lon.is_finite() && lat.is_finite() && (-90.0..=90.0).contains(&lat)).then_some((lon, lat))
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
        match self.footprint_fractions(wgs84_bbox, src_env_wgs84) {
            Some(fractions) => footprint_pixels(fractions, width, height),
            None => (0, width.saturating_sub(1), 0, height.saturating_sub(1)),
        }
    }

    /// [`Self::footprint_pixel_window`], or `None` when the footprint lies
    /// wholly outside the `width`×`height` image, or the image is empty.
    ///
    /// The window clamps to the image, so a footprint off its right edge
    /// still gives the edge column, and every pixel of that column lies
    /// outside the footprint and its margin: the guard leaves nothing to
    /// draw there. A caller can then skip the render entirely, with no
    /// source read and no [`crate::resample::ProjectionGrid`]. Over a
    /// projected source whose forward folds far from its meridian (the
    /// Transverse Mercator series at hundreds of degrees of longitude, in a
    /// Web Mercator tile past ±180°), refining a grid for such a tile ran
    /// to its cell cap (#1129). Otherwise the window is
    /// [`Self::footprint_pixel_window`]'s, with the full image when no
    /// perimeter sample maps.
    pub fn footprint_window_in_image(
        &self,
        wgs84_bbox: [f64; 4],
        src_env_wgs84: [f64; 4],
        width: u32,
        height: u32,
    ) -> Option<(u32, u32, u32, u32)> {
        if width == 0 || height == 0 {
            return None;
        }
        match self.footprint_fractions(wgs84_bbox, src_env_wgs84) {
            Some([x_lo, x_hi, y_lo, y_hi])
                if x_hi < 0.0 || x_lo >= 1.0 || y_hi < 0.0 || y_lo >= 1.0 =>
            {
                None
            }
            Some(fractions) => Some(footprint_pixels(fractions, width, height)),
            None => Some((0, width - 1, 0, height - 1)),
        }
    }

    /// The fractional output box `[x_lo, x_hi, y_lo, y_hi]` a source's
    /// WGS84 envelope occupies, with its margin, unclamped; `None` when no
    /// perimeter sample maps to a finite position. See
    /// [`Self::footprint_pixel_window`].
    fn footprint_fractions(
        &self,
        wgs84_bbox: [f64; 4],
        src_env_wgs84: [f64; 4],
    ) -> Option<[f64; 4]> {
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
            return None;
        }
        // Margin: a fraction of the footprint's own output span, with a small
        // floor, so edge-sampling gaps and sub-pixel rounding never clip boundary
        // data. Far-away ghosts sit far outside, so the margin never readmits them.
        let mx = ((fx_hi - fx_lo) * 0.02).max(0.005);
        let my = ((fy_hi - fy_lo) * 0.02).max(0.005);
        Some([fx_lo - mx, fx_hi + mx, fy_lo - my, fy_hi + my])
    }

    /// The rows of a `height`-row output image over `wgs84_bbox` whose pixel
    /// centres lie inside this output CRS's domain, with the WGS84 box those
    /// rows span; `None` when no row does.
    ///
    /// Geographic output is cut row by row: CRS:84 and EPSG:4326 define
    /// latitude on [-90°, 90°], and a request box may reach past a pole.
    /// WMS 1.3.0 §7.3.3.6 wants empty content there, but an engine sampling
    /// such a row can draw: a grid's edge cell extends past a pole, and a
    /// projected source's forward maps latitude 95° to 85° on the far
    /// meridian (#1065). Latitude falls row by row, so the rows inside form
    /// one run.
    ///
    /// Web Mercator answers all rows: its latitudes stay inside (−90°, 90°).
    /// So does a projected output, except when its box lies wholly outside
    /// the projection's valid area
    /// ([`crate::geo::outside_projection_domain`]): then no pixel maps to the
    /// Earth, and the answer is `None` (#1089). That test reads the projected
    /// box only, so `wgs84_bbox` may be anything for such a box, which has no
    /// meaningful WGS84 envelope. In a box partly outside, the pixels past
    /// the LAEA disk, and past TM's poles or its area's east and west edges
    /// (#1113), are non-finite in [`Self::project_node`] and stay empty.
    ///
    /// The box is the request box cut at the outer edges of the first and
    /// last row, so a render of it at `rows.len()` rows samples the full
    /// image's pixel centres of those rows ([`get_raster_tile_in_domain`]).
    /// It cuts the image at the CRS's edge; it never clamps a coordinate
    /// (root CLAUDE.md Critical Rule 4). When every row is inside, `bbox` is
    /// `wgs84_bbox` unchanged.
    pub fn domain_rows(&self, wgs84_bbox: [f64; 4], height: u32) -> Option<DomainRows> {
        if height == 0 {
            return None;
        }
        if let OutputCrs::Projected { crs, bbox } = self {
            if crate::geo::outside_projection_domain(crs, *bbox) {
                return None;
            }
        }
        let [west, south, east, north] = wgs84_bbox;
        let geographic = (-90.0..=90.0).contains(&south) && (-90.0..=90.0).contains(&north);
        if !matches!(self, OutputCrs::Wgs84) || geographic {
            return Some(DomainRows {
                rows: 0..height,
                bbox: wgs84_bbox,
            });
        }
        // The centre latitude the engines sample for `row`.
        let lat = |row: u32| {
            let fy = (f64::from(row) + 0.5) / f64::from(height);
            self.project_node(wgs84_bbox, 0.5, fy).1
        };
        // Rows north of the domain come first, then those inside, then those
        // south of it.
        let top = first_row(height, |row| lat(row) <= 90.0);
        let end = first_row(height, |row| lat(row) < -90.0);
        if top >= end {
            return None;
        }
        let edge = |row: u32| north - f64::from(row) / f64::from(height) * (north - south);
        Some(DomainRows {
            rows: top..end,
            bbox: [west, edge(end), east, edge(top)],
        })
    }

    /// The pixels of a `width`×`height` output image over `wgs84_bbox`
    /// whose centres lie on the Earth ([`Self::position_on_earth`]): per
    /// row, north first, the run of columns that do, empty for a row with
    /// none. `None` when every pixel does, the usual case. The rest of the
    /// image lies outside the CRS's valid area, the part OGC API - Maps
    /// paints with `void-color` (#894). An engine draws nothing there.
    ///
    /// Web Mercator answers `None`: its latitudes stay inside (−90°, 90°).
    /// Geographic output is cut by row, at the poles
    /// ([`Self::domain_rows`]). A projected output's valid area is
    /// symmetric about a centre and narrows away from it
    /// ([`crate::geo::outside_projection_domain`]), so the pixels of a row
    /// on the Earth are a run around the column nearest that centre, found
    /// by bisection: at most `1 + 2·log2(width)` points a row, never one per
    /// pixel (root CLAUDE.md Critical Rule 5). When the four corner pixels
    /// lie on the Earth, every pixel does, and that answers `None`.
    pub fn earth_columns(
        &self,
        wgs84_bbox: [f64; 4],
        width: u32,
        height: u32,
    ) -> Option<Vec<std::ops::Range<u32>>> {
        if width == 0 || height == 0 {
            return None;
        }
        match self {
            OutputCrs::WebMercator => None,
            OutputCrs::Wgs84 => {
                let rows = self.domain_rows(wgs84_bbox, height).map(|d| d.rows);
                if rows == Some(0..height) {
                    return None;
                }
                let rows = rows.unwrap_or(0..0);
                Some(
                    (0..height)
                        .map(|row| if rows.contains(&row) { 0..width } else { 0..0 })
                        .collect(),
                )
            }
            OutputCrs::Projected { crs, bbox } => {
                let on_earth = |column: u32, row: u32| {
                    let fx = (f64::from(column) + 0.5) / f64::from(width);
                    let fy = (f64::from(row) + 0.5) / f64::from(height);
                    self.position_on_earth(wgs84_bbox, fx, fy).is_some()
                };
                let (last_column, last_row) = (width - 1, height - 1);
                let corners = [
                    (0, 0),
                    (last_column, 0),
                    (0, last_row),
                    (last_column, last_row),
                ];
                if corners.iter().all(|&(c, r)| on_earth(c, r)) {
                    return None;
                }
                let (centre_e, _) = crate::geo::projection_domain_centre(crs)?;
                let [min_e, _, max_e, _] = *bbox;
                // The column whose centre lies nearest the area's centre:
                // the one holding it, or the image's edge column (a NaN
                // from a degenerate box casts to column 0).
                let nearest = ((centre_e - min_e) / (max_e - min_e) * f64::from(width))
                    .floor()
                    .clamp(0.0, f64::from(last_column)) as u32;
                Some(
                    (0..height)
                        .map(|row| {
                            if !on_earth(nearest, row) {
                                return 0..0;
                            }
                            let start = first_in(0, nearest, |c| on_earth(c, row));
                            let end = first_in(nearest + 1, width, |c| !on_earth(c, row));
                            start..end
                        })
                        .collect(),
                )
            }
        }
    }
}

/// The projected metres `(e, n)` of the output position `(fx, fy)` in the
/// request rectangle `bbox` `[min_e, min_n, max_e, max_n]`: a projected
/// output lays its pixels out linearly in its metres.
fn projected_point(bbox: &[f64; 4], fx: f64, fy: f64) -> (f64, f64) {
    let [min_e, min_n, max_e, max_n] = *bbox;
    (min_e + fx * (max_e - min_e), max_n - fy * (max_n - min_n))
}

/// The inclusive pixels `(px_lo, px_hi, py_lo, py_hi)` of the fractional
/// footprint box `[x_lo, x_hi, y_lo, y_hi]` in a `width`×`height` image,
/// clamped to the image.
fn footprint_pixels(
    [x_lo, x_hi, y_lo, y_hi]: [f64; 4],
    width: u32,
    height: u32,
) -> (u32, u32, u32, u32) {
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
        to_px(x_lo, width),
        to_px(x_hi, width),
        to_px(y_lo, height),
        to_px(y_hi, height),
    )
}

/// The first of `0..height` for which `pred` holds, `height` when none
/// does; `pred` must be false for a prefix of the rows and true after it.
fn first_row(height: u32, pred: impl Fn(u32) -> bool) -> u32 {
    first_in(0, height, pred)
}

/// The first of `lo..hi` for which `pred` holds, `hi` when none does;
/// `pred` must be false for a prefix of the range and true after it.
fn first_in(lo: u32, hi: u32, pred: impl Fn(u32) -> bool) -> u32 {
    let (mut lo, mut hi) = (lo, hi);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// The part of an output image inside its CRS's domain:
/// [`OutputCrs::domain_rows`].
#[derive(Debug, Clone, PartialEq)]
pub struct DomainRows {
    /// The output rows inside the domain, north first.
    pub rows: std::ops::Range<u32>,
    /// The WGS84 box `[west, south, east, north]` those rows cover.
    pub bbox: [f64; 4],
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

    /// This tile as rows `top..top + self.height` of a `height`-row tile
    /// whose other rows are nodata: the full image of a render asked only
    /// for its [`OutputCrs::domain_rows`]. Every variant keeps its pixels'
    /// values; `U8` without a nodata sentinel, which has no raw value for
    /// the added rows, becomes the boxed `F64` of the same values.
    pub fn placed_in_rows(self, top: u32, height: u32) -> RasterTile {
        debug_assert!(
            top.checked_add(self.height)
                .is_some_and(|end| end <= height),
            "rows {top}..+{} do not fit in {height}",
            self.height
        );
        if top == 0 && self.height == height {
            return self;
        }
        let width = self.width as usize;
        let above = top as usize * width;
        let below = height.saturating_sub(top.saturating_add(self.height)) as usize * width;
        fn pad<T: Copy>(data: Vec<T>, nodata: T, above: usize, below: usize) -> Vec<T> {
            let mut padded = Vec::with_capacity(above + data.len() + below);
            padded.resize(above, nodata);
            padded.extend(data);
            padded.resize(padded.len() + below, nodata);
            padded
        }
        let values = match self.values {
            RasterValues::F64(values) => RasterValues::F64(pad(values, None, above, below)),
            RasterValues::F32 { data, nodata } => RasterValues::F32 {
                data: pad(data, f32::NAN, above, below),
                nodata,
            },
            RasterValues::U8 {
                data,
                nodata: Some(nodata),
                gain,
                offset,
            } => RasterValues::U8 {
                data: pad(data, nodata, above, below),
                nodata: Some(nodata),
                gain,
                offset,
            },
            values @ RasterValues::U8 { nodata: None, .. } => {
                RasterValues::F64(pad(values.iter_values().collect(), None, above, below))
            }
        };
        RasterTile {
            width: self.width,
            height,
            values,
        }
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
    /// The grid the source's samples lie on, in its own CRS, as the source
    /// states it (#1237): the CRS, cell counts, cell size and outer corner,
    /// and an EPSG code only when the source states one. Built once per
    /// load or poll. `None` without a regular grid (polar radar volumes,
    /// station data) or where the engine does not report it; a collection
    /// whose parameters lie on different grids reports its default
    /// parameter's. The collection description advertises it through
    /// [`crate::ogc_extent::build_extent`].
    pub native_grid: Option<crate::native_grid::NativeGrid>,
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
    ///
    /// An API layer rendering a client-chosen bbox directly calls this through
    /// [`get_raster_tile_in_domain`] (bands: [`get_raster_tiles_in_domain`]),
    /// never on its own: a `Wgs84` bbox may reach past a pole, and the helper
    /// asks only for the rows inside ±90° and leaves the rest empty, without
    /// clamping the bbox (WMS 1.3.0 §7.3.3.6, #1065).
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
    ///
    /// This is the collection-wide value: the API layers read it through
    /// [`Self::content_version_for`], whose default it is.
    fn content_version(&self) -> u64 {
        0
    }

    /// [`Self::content_version`] for one frame: a value that changes
    /// whenever the pixels [`Self::get_raster_tile`] renders for this
    /// `parameter`, `time`, `reference_time` and `z` can change. The API
    /// layers pass the arguments they render with, the time and run
    /// resolved by [`Self::resolve_parameter_time`] and
    /// [`Self::resolve_parameter_reference_time`] and the requested `z`, and
    /// key the rendered and meta-tile caches on the result.
    ///
    /// An engine whose collection-wide version moves with every catalog
    /// change, while most frames keep their pixels, overrides this so that
    /// a change orphans only the frames it touches (#1334). GRIB hashes the
    /// identity of the message the render reads, selected through the SAME
    /// helpers `get_raster_tile` uses: share them so the two cannot drift.
    /// A wrapper that derives a layer from other parameters combines their
    /// versions; every wrapper forwards it.
    ///
    /// The same rules as [`Self::content_version`]: `0` only for content
    /// never revised in place, and unchanged when nothing the frame draws
    /// changed. Default: [`Self::content_version`]. **From a snapshot, no
    /// I/O**: it runs on the hot render path before the cache lookup, hits
    /// included.
    fn content_version_for(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
        z: Option<f64>,
    ) -> u64 {
        let _ = (parameter, time, reference_time, z);
        self.content_version()
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

    /// [`Self::resolve_parameter_reference_time`] for parameters rendered
    /// together from one run, such as the u and v components a derived
    /// wind speed combines: the run that has them all, which
    /// [`Self::resolve_parameters_time`] then selects their one timestep in.
    ///
    /// An engine whose run selection depends on the parameter MUST override
    /// this with the selection its render uses. GRIB does (#1026): a
    /// component only an older run still has renders, with its partner,
    /// from that run. Default: the first parameter's run.
    fn resolve_parameters_reference_time(
        &self,
        parameters: &[&str],
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        self.resolve_parameter_reference_time(parameters.first().copied(), time, reference_time)
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

    /// Whether the layer of `parameter`, one of [`RasterInfo::parameters`],
    /// shows that parameter's own values: the values a position query of
    /// the same collection answers at a point. `false` for an overlay drawn
    /// from other data, such as a radar site's storm-cell outlines, which
    /// no point query answers.
    ///
    /// WMS GetFeatureInfo reads a layer's value with the collection's EDR
    /// position query (#1064), so it answers only for such layers: an
    /// overlay layer is `queryable="0"` and answers `LayerNotQueryable`,
    /// unless it draws features [`Self::pick_features`] picks (#1100).
    /// Default `true`. **O(1)**, no I/O: GetCapabilities reads it for every
    /// layer (Critical Rule 10).
    fn layer_shows_values(&self, parameter: &str) -> bool {
        let _ = parameter;
        true
    }

    /// Whether the layer of `parameter` draws features, such as alert
    /// areas, storm-cell outlines or lightning strikes, that
    /// [`Self::pick_features`] answers for at a pixel (#1100).
    ///
    /// WMS GetFeatureInfo then answers for the layer with the features the
    /// map draws at the clicked pixel instead of a position query's value,
    /// and marks it `queryable="1"`. Default `false`. **O(1)**, no I/O:
    /// GetCapabilities reads it for every layer (Critical Rule 10). An
    /// engine that returns `true` MUST override [`Self::pick_features`].
    fn layer_picks_features(&self, parameter: &str) -> bool {
        let _ = parameter;
        false
    }

    /// The features the layer of `pick.parameter` draws at the picked
    /// pixel, selected by the rule [`Self::get_raster_tile`] draws them
    /// with at `pick.time` (and `pick.reference_time`, `pick.z`), most
    /// relevant first, at most `pick.limit` of them and all of them
    /// counted. Nothing there is an empty answer; `LocationNotFound` and
    /// the other no-data errors are read as one too.
    ///
    /// Runs as an EDR query does: on ds-executor's query runtime, where
    /// `block_in_place` is valid and a `spawn_blocking` bridge is not, under
    /// the request's deadline (`ds_core::deadline`). Engines return domain
    /// types; the API layer writes the formats.
    ///
    /// Default: `InvalidParameter`, for a layer
    /// [`Self::layer_picks_features`] does not claim.
    fn pick_features(&self, pick: &FeaturePick<'_>) -> Result<PickedFeatures, DataServerError> {
        let _ = pick;
        Err(DataServerError::InvalidParameter(
            "this layer draws no features to pick".into(),
        ))
    }

    /// The vertical level [`Self::get_raster_tile`] draws `parameter` at
    /// when it is passed `z` (WMS `ELEVATION`, `None` when omitted), at the
    /// `time` and `reference_time` the render is passed, those
    /// [`Self::resolve_parameter_time`] and
    /// [`Self::resolve_parameter_reference_time`] resolved. One of
    /// [`RasterInfo::vertical`]'s levels; `None` for a layer without one.
    ///
    /// WMS GetFeatureInfo passes this level to the collection's position
    /// query and reports it (#1064), so the value it answers is one the map
    /// shows. An engine whose render draws another level than `z`, or than
    /// the first advertised level when `z` is `None`, MUST override this
    /// with the selection `get_raster_tile` uses, through a shared helper so
    /// the two cannot drift. GRIB draws the selected run's own first level;
    /// a radar volume draws the sweep nearest `z`, or the lowest, among
    /// those carrying the quantity. Default: `z`, else the first advertised
    /// level. From a snapshot, no I/O.
    fn resolve_elevation(
        &self,
        parameter: Option<&str>,
        z: Option<f64>,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<f64> {
        let _ = (parameter, time, reference_time);
        z.or_else(|| {
            self.raster_info_shared()
                .vertical
                .as_ref()
                .and_then(|vertical| vertical.levels.first().copied())
        })
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

/// [`MapEngine::get_raster_tile`] for a map request, asking the engine only
/// for the output rows inside the output CRS ([`OutputCrs::domain_rows`]):
/// the others stay nodata, the empty content WMS 1.3.0 §7.3.3.6 wants
/// outside a CRS's range. `Ok(None)` when no row is inside: there is nothing
/// to draw, and the engine is not called. The API layers' direct renders go
/// through this, so no engine is asked for a latitude past a pole (#1065)
/// or for a projected box wholly outside its projection (#1089).
#[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
pub fn get_raster_tile_in_domain(
    engine: &dyn MapEngine,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    time: Option<DateTime<Utc>>,
    output_crs: &OutputCrs,
    parameter: Option<&str>,
    z: Option<f64>,
    reference_time: Option<DateTime<Utc>>,
) -> Result<Option<RasterTile>, DataServerError> {
    let Some(domain) = output_crs.domain_rows(bbox, height) else {
        return Ok(None);
    };
    let rows = domain.rows.end - domain.rows.start;
    let tile = engine.get_raster_tile(
        domain.bbox,
        width,
        rows,
        time,
        output_crs,
        parameter,
        z,
        reference_time,
    )?;
    Ok(Some(tile.placed_in_rows(domain.rows.start, height)))
}

/// [`MapEngine::get_raster_tiles`] (a composite's bands) through
/// [`OutputCrs::domain_rows`], as [`get_raster_tile_in_domain`].
#[allow(clippy::too_many_arguments)] // mirrors get_raster_tiles
pub fn get_raster_tiles_in_domain(
    engine: &dyn MapEngine,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    time: Option<DateTime<Utc>>,
    output_crs: &OutputCrs,
    parameters: &[&str],
    z: Option<f64>,
    reference_time: Option<DateTime<Utc>>,
) -> Result<Option<Vec<RasterTile>>, DataServerError> {
    let Some(domain) = output_crs.domain_rows(bbox, height) else {
        return Ok(None);
    };
    let rows = domain.rows.end - domain.rows.start;
    let tiles = engine.get_raster_tiles(
        domain.bbox,
        width,
        rows,
        time,
        output_crs,
        parameters,
        z,
        reference_time,
    )?;
    Ok(Some(
        tiles
            .into_iter()
            .map(|tile| tile.placed_in_rows(domain.rows.start, height))
            .collect(),
    ))
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
            native_grid: None,
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
        assert_eq!(
            plain.resolve_parameters_reference_time(&["late", "other"], Some(at(2)), Some(at(1))),
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

    /// By default every frame's version is the collection's (#1334): an
    /// immutable engine stays `0`, a revised one keeps its value.
    #[test]
    fn default_content_version_for_is_the_collections() {
        struct Revised(u64);
        impl MapEngine for Revised {
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
            fn content_version(&self) -> u64 {
                self.0
            }
        }
        for version in [0, 7] {
            let engine = Revised(version);
            assert_eq!(engine.content_version_for(None, None, None, None), version);
            assert_eq!(
                engine.content_version_for(Some("t"), Some(hour(1)), Some(hour(0)), Some(850.0)),
                version
            );
        }
    }

    /// By default a map draws the requested level, else the first
    /// advertised one; a collection without levels has none.
    #[test]
    fn default_resolve_elevation_is_z_else_the_first_level() {
        struct Levels(Option<Vec<f64>>);
        impl MapEngine for Levels {
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
                RasterInfo {
                    native_crs: "CRS:84".into(),
                    spatial_extent: None,
                    times: Vec::new(),
                    parameter: "t".into(),
                    unit: "K".into(),
                    parameters: Vec::new(),
                    vertical: self.0.clone().map(|levels| {
                        VerticalDimension::new(crate::vertical::VerticalKind::Pressure, levels)
                    }),
                    grid_size: None,
                    layer_subtitle: None,
                    reference_times: Vec::new(),
                    native_grid: None,
                }
            }
        }
        let pressure = Levels(Some(vec![850.0, 500.0]));
        assert_eq!(
            pressure.resolve_elevation(None, None, None, None),
            Some(850.0)
        );
        assert_eq!(
            pressure.resolve_elevation(Some("t"), Some(500.0), None, None),
            Some(500.0)
        );
        assert_eq!(Levels(None).resolve_elevation(None, None, None, None), None);
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

    /// The WGS84 box of a Web Mercator meta-tile on the half-octave ladder,
    /// `2^(level/2)` tiles a world, with longitudes unwrapped past ±180° as
    /// the meta-tiling passes them.
    fn merc_tile(level: i32, col: i64, row: i64) -> [f64; 4] {
        use crate::web_mercator::{x_to_lon, y_to_lat, EARTH_RADIUS};
        let origin = std::f64::consts::PI * EARTH_RADIUS;
        let span = 2.0 * origin / 2f64.powf(f64::from(level) / 2.0);
        let (x0, top) = (-origin + col as f64 * span, origin - row as f64 * span);
        [
            x_to_lon(x0),
            y_to_lat(top - span),
            x_to_lon(x0 + span),
            y_to_lat(top),
        ]
    }

    /// `footprint_window_in_image` against `footprint_pixel_window`: the same
    /// window, or `None` when every pixel of the clamped window lies outside
    /// the footprint and its margin, so the guard would draw none of them.
    fn assert_in_image_agrees(crs: &OutputCrs, bbox: [f64; 4], env: [f64; 4], size: u32) -> bool {
        let window = crs.footprint_pixel_window(bbox, env, size, size);
        let Some(fractions) = crs.footprint_fractions(bbox, env) else {
            let full = Some((0, size - 1, 0, size - 1));
            assert_eq!(crs.footprint_window_in_image(bbox, env, size, size), full);
            return true;
        };
        let [x_lo, x_hi, y_lo, y_hi] = fractions;
        let s = f64::from(size);
        let meets = |p: u32, lo: f64, hi: f64| f64::from(p) / s <= hi && f64::from(p + 1) / s > lo;
        let (px_lo, px_hi, py_lo, py_hi) = window;
        let drawn = (px_lo..=px_hi).any(|px| meets(px, x_lo, x_hi))
            && (py_lo..=py_hi).any(|py| meets(py, y_lo, y_hi));
        assert_eq!(
            crs.footprint_window_in_image(bbox, env, size, size),
            drawn.then_some(window),
            "{crs:?} {bbox:?} over {env:?}"
        );
        drawn
    }

    /// #1129: a footprint wholly off the image has no window, and only then.
    /// The issue's off-world Web Mercator meta-tiles, either side of the
    /// world, hold no Finland; nor does the seam, where a source that
    /// crosses it keeps its window, whether its envelope comes unwrapped or
    /// spans the turn. Every tile of the ladder's first levels, a turn either
    /// side of the world, and the EPSG:3035 tiles around Europe agree with
    /// `footprint_pixel_window`.
    #[test]
    fn footprint_window_in_image_is_none_only_off_the_image() {
        let merc = OutputCrs::WebMercator;
        let seam = [170.0, 10.0, 190.0, 20.0];
        for (bbox, env, drawn) in [
            (merc_tile(3, 3, 2), FINLAND_WGS84, false),
            (merc_tile(3, -1, 2), FINLAND_WGS84, false),
            (merc_tile(2, 2, 0), FINLAND_WGS84, false),
            (merc_tile(4, 5, 3), FINLAND_WGS84, false),
            (merc_tile(3, 1, 0), FINLAND_WGS84, true),
            (merc_tile(3, 1, 1), FINLAND_WGS84, false),
            (seam, FINLAND_WGS84, false),
            (seam, [175.0, 12.0, 185.0, 18.0], true),
            (seam, [-180.0, 12.0, 180.0, 18.0], true),
            (
                [170.0, 10.0, -170.0, 20.0],
                [175.0, 12.0, 185.0, 18.0],
                false,
            ),
        ] {
            assert_eq!(
                assert_in_image_agrees(&merc, bbox, env, 256),
                drawn,
                "{bbox:?} over {env:?}"
            );
        }
        for level in 0..=8 {
            let n = 2f64.powf(f64::from(level) / 2.0).ceil() as i64;
            for row in 0..n {
                for col in -n..2 * n {
                    assert_in_image_agrees(&merc, merc_tile(level, col, row), FINLAND_WGS84, 256);
                }
            }
        }
        let laea = projected_output_crs("EPSG:3035").unwrap();
        let side = 1_024_000.0;
        for i in 0..8 {
            for j in 0..8 {
                let (e, n) = (f64::from(i) * side, f64::from(j) * side);
                let out = OutputCrs::Projected {
                    crs: laea.clone(),
                    bbox: [e, n, e + side, n + side],
                };
                assert_in_image_agrees(&out, [0.0; 4], FINLAND_WGS84, 256);
            }
        }
        assert_eq!(
            merc.footprint_window_in_image(merc_tile(3, 1, 0), FINLAND_WGS84, 0, 256),
            None,
            "an empty image has no pixel to draw"
        );
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

    /// The centre latitude of `row` of a `height`-row geographic image.
    fn row_lat(bbox: [f64; 4], row: u32, height: u32) -> f64 {
        let fy = (f64::from(row) + 0.5) / f64::from(height);
        OutputCrs::Wgs84.project_node(bbox, 0.5, fy).1
    }

    /// #1065: a geographic box past a pole keeps the rows whose centres lie
    /// on [-90°, 90°], and the box it hands the engine puts those rows'
    /// centres exactly where the full image has them.
    #[test]
    fn domain_rows_cut_a_geographic_image_at_the_poles() {
        let wgs84 = OutputCrs::Wgs84;
        // Inside the CRS, the poles included: every row, the box untouched.
        for bbox in [FINLAND_WGS84, [-180.0, -90.0, 180.0, 90.0]] {
            assert_eq!(
                wgs84.domain_rows(bbox, 100),
                Some(DomainRows { rows: 0..100, bbox })
            );
        }
        // Entirely past a pole, as ets-wms13's getmap:bbox-outside-crs asks
        // (row 0's centre is 90.1° north / 90.1° south): nothing.
        assert_eq!(wgs84.domain_rows([-10.0, 90.0, 10.0, 110.0], 100), None);
        assert_eq!(wgs84.domain_rows([-10.0, -110.0, 10.0, -90.0], 100), None);
        assert_eq!(wgs84.domain_rows(FINLAND_WGS84, 0), None);

        for (bbox, height, rows, inside) in [
            // Half past the north pole: the southern half.
            (
                [-10.0, 80.0, 10.0, 100.0],
                100,
                50..100,
                [-10.0, 80.0, 10.0, 90.0],
            ),
            // 0.1° rows: row 299 is centred at 89.95°S, row 300 at 90.05°S.
            (
                [-180.0, -120.0, 180.0, -60.0],
                600,
                0..300,
                [-180.0, -90.0, 180.0, -60.0],
            ),
            // Past both poles.
            (
                [-180.0, -100.0, 180.0, 100.0],
                200,
                10..190,
                [-180.0, -90.0, 180.0, 90.0],
            ),
            // The pole inside the first row: its centre (89.99°) is inside,
            // so the row stays whole, its top edge past the pole with it.
            (
                [0.0, 80.04, 10.0, 90.04],
                100,
                0..100,
                [0.0, 80.04, 10.0, 90.04],
            ),
        ] {
            let domain = wgs84.domain_rows(bbox, height).unwrap();
            assert_eq!(domain.rows, rows, "{bbox:?}");
            for (got, want) in domain.bbox.iter().zip(inside) {
                assert!((got - want).abs() < 1e-9, "{bbox:?}: {:?}", domain.bbox);
            }
            for row in 0..height {
                let lat = row_lat(bbox, row, height);
                assert_eq!(
                    domain.rows.contains(&row),
                    (-90.0..=90.0).contains(&lat),
                    "{bbox:?} row {row} at {lat}"
                );
            }
            let n = domain.rows.end - domain.rows.start;
            for k in 0..n {
                let full = row_lat(bbox, domain.rows.start + k, height);
                let part = row_lat(domain.bbox, k, n);
                assert!((full - part).abs() < 1e-9, "{bbox:?} row {k}");
            }
        }
    }

    /// Only geographic output is cut: a Web Mercator or projected output's
    /// latitudes cannot leave the CRS this way.
    #[test]
    fn domain_rows_leave_other_output_crs_whole() {
        let proj = projected_output_crs("EPSG:3067").unwrap();
        let past_pole = [-10.0, 80.0, 10.0, 100.0];
        for out in [
            OutputCrs::WebMercator,
            OutputCrs::Projected {
                crs: proj,
                bbox: [0.0, 0.0, 1.0, 1.0],
            },
        ] {
            assert_eq!(
                out.domain_rows(past_pole, 64),
                Some(DomainRows {
                    rows: 0..64,
                    bbox: past_pole
                }),
                "{out:?}"
            );
        }
    }

    /// A projected box wholly outside its projection's valid area has no row
    /// inside (#1089): an EPSG:3035 box past the LAEA disk, an EPSG:3067 one
    /// past a pole, past the eastings where its series stops being exact
    /// (#1113, #1130), or so far east that the TM inverse has no finite
    /// result. A box reaching into the area, or enclosing it, keeps every
    /// row.
    #[test]
    fn domain_rows_of_a_projected_box_outside_its_projection_are_none() {
        let laea = projected_output_crs("EPSG:3035").unwrap();
        let tm = projected_output_crs("EPSG:3067").unwrap();
        // The WGS84 box does not enter the test; such a box has none.
        let wgs84 = [0.0; 4];
        let projected = |crs: &Crs, bbox| OutputCrs::Projected {
            crs: crs.clone(),
            bbox,
        };
        for (crs, bbox) in [
            (&laea, [2.0e7, 2.0e7, 2.1e7, 2.1e7]),
            (&tm, [0.0, 1.5e7, 1.0e6, 1.6e7]),
            (&tm, [0.0, 2.1e7, 1.0e6, 2.2e7]),
            (&tm, [0.0, -2.2e7, 1.0e6, -2.1e7]),
            (&tm, [1.1e7, 6.5e6, 1.2e7, 7.0e6]),
            (&tm, [1.0e20, 6.5e6, 2.0e20, 7.0e6]),
        ] {
            assert_eq!(
                projected(crs, bbox).domain_rows(wgs84, 64),
                None,
                "{bbox:?}"
            );
        }
        for (crs, bbox) in [
            (&laea, [4.0e6, 3.0e6, 4.5e6, 3.5e6]),
            // Across the disk's rim.
            (&laea, [1.6e7, 2.2e6, 1.8e7, 4.2e6]),
            // Wider than the disk.
            (&laea, [-3.0e7, -3.0e7, 3.0e7, 3.0e7]),
            (&tm, [-1.2e5, 6.3e6, 8.8e5, 7.9e6]),
            // Across the North Pole, past the Snyder series' 50° east of the
            // meridian (#1113), and across the area's east edge.
            (&tm, [0.0, 9.0e6, 1.0e6, 1.1e7]),
            (&tm, [1_448_154.7, 7_240_773.4, 2_896_309.4, 8_688_928.1]),
            (&tm, [5.0e6, 6.5e6, 6.0e6, 7.0e6]),
            (&tm, [1.0e7, -1.0e6, 1.1e7, 1.0e6]),
            // Wider than the TM area.
            (&tm, [-3.0e7, -3.0e7, 3.0e7, 3.0e7]),
        ] {
            assert_eq!(
                projected(crs, bbox).domain_rows(wgs84, 64),
                Some(DomainRows {
                    rows: 0..64,
                    bbox: wgs84
                }),
                "{bbox:?}"
            );
        }
    }

    /// #894: `earth_columns` marks exactly the pixels whose centres lie on
    /// the Earth, checked here against every pixel's `position_on_earth`:
    /// rows past a pole of a geographic box, and the parts of a projected
    /// box past the LAEA disk or Transverse Mercator's area. An image wholly
    /// on the Earth, and every Web Mercator image, is `None`.
    #[test]
    fn earth_columns_are_the_pixels_on_the_earth() {
        let laea = projected_output_crs("EPSG:3035").unwrap();
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let projected = |crs: &Crs, bbox| OutputCrs::Projected {
            crs: crs.clone(),
            bbox,
        };
        let past_pole = [-10.0, 80.0, 10.0, 100.0];
        for (out, bbox) in [
            (OutputCrs::Wgs84, FINLAND_WGS84),
            (OutputCrs::Wgs84, [-180.0, -90.0, 180.0, 90.0]),
            (OutputCrs::WebMercator, past_pole),
            (projected(&laea, [4.0e6, 3.0e6, 4.5e6, 3.5e6]), [0.0; 4]),
            (projected(&tm, [-1.2e5, 6.3e6, 8.8e5, 7.9e6]), [0.0; 4]),
        ] {
            assert_eq!(out.earth_columns(bbox, 40, 30), None, "{out:?}");
        }
        assert_eq!(OutputCrs::Wgs84.earth_columns(past_pole, 0, 30), None);

        let (width, height) = (48, 40);
        let mut partial = 0;
        for (out, bbox) in [
            (OutputCrs::Wgs84, past_pole),
            (OutputCrs::Wgs84, [-180.0, -100.0, 180.0, 100.0]),
            (OutputCrs::Wgs84, [-10.0, 95.0, 10.0, 110.0]),
            // Across the disk's rim, and wider than the disk.
            (projected(&laea, [1.6e7, 2.2e6, 1.8e7, 4.2e6]), [0.0; 4]),
            (projected(&laea, [-3.0e7, -3.0e7, 3.0e7, 3.0e7]), [0.0; 4]),
            // Across the North Pole, the area's east edge, and wider than
            // the area; wholly past its east edge.
            (projected(&tm, [0.0, 9.0e6, 1.0e6, 1.1e7]), [0.0; 4]),
            (projected(&tm, [1.0e7, -1.0e6, 1.1e7, 1.0e6]), [0.0; 4]),
            (projected(&tm, [-3.0e7, -3.0e7, 3.0e7, 3.0e7]), [0.0; 4]),
            (projected(&tm, [1.1e7, 6.5e6, 1.2e7, 7.0e6]), [0.0; 4]),
        ] {
            let rows = out
                .earth_columns(bbox, width, height)
                .unwrap_or_else(|| panic!("{out:?} {bbox:?}: no pixel is off the Earth"));
            assert_eq!(rows.len(), height as usize);
            for (row, columns) in (0..height).zip(&rows) {
                for column in 0..width {
                    let fx = (f64::from(column) + 0.5) / f64::from(width);
                    let fy = (f64::from(row) + 0.5) / f64::from(height);
                    assert_eq!(
                        columns.contains(&column),
                        out.position_on_earth(bbox, fx, fy).is_some(),
                        "{out:?} {bbox:?}: pixel {column}, {row}"
                    );
                }
            }
            if rows
                .iter()
                .any(|r| !r.is_empty() && r.len() < width as usize)
            {
                partial += 1;
            }
        }
        // Rows cut inside a row, not only whole rows, were checked.
        assert!(partial >= 3, "{partial}");
    }

    /// A point's position is `project_node`'s when the point lies on the
    /// Earth, and `None` past a pole of a geographic box, past the LAEA
    /// disk, and past a pole or Transverse Mercator's eastings where its
    /// series stops being exact (#1064, #1089, #1113, #1130). A projected
    /// output reads its own metres, so a NaN WGS84 box does not matter.
    #[test]
    fn position_on_earth_is_none_off_the_earth() {
        let laea = projected_output_crs("EPSG:3035").unwrap();
        let tm = projected_output_crs("EPSG:3067").unwrap();
        let projected = |crs: &Crs, bbox| OutputCrs::Projected {
            crs: crs.clone(),
            bbox,
        };
        let nan = [f64::NAN; 4];

        // On the Earth: exactly `project_node`.
        for (out, wgs84) in [
            (OutputCrs::Wgs84, FINLAND_WGS84),
            (OutputCrs::WebMercator, FINLAND_WGS84),
            (projected(&tm, [4.905e5, 6.64e6, 5.105e5, 6.66e6]), nan),
            (projected(&laea, [4.0e6, 3.0e6, 4.5e6, 3.5e6]), nan),
        ] {
            for (fx, fy) in [(0.5, 0.5), (0.0, 1.0), (0.25, 0.75)] {
                let node = out.project_node(wgs84, fx, fy);
                assert_eq!(out.position_on_earth(wgs84, fx, fy), Some(node), "{out:?}");
            }
        }

        // Past a pole of a geographic box: row 0 of 80–100°N.
        assert_eq!(
            OutputCrs::Wgs84.position_on_earth([-10.0, 80.0, 10.0, 100.0], 0.5, 0.05),
            None
        );
        // Every point of a box wholly outside its projection: past the LAEA
        // disk, past TM's poles, north and south, and past its area's east
        // edge.
        for (crs, bbox) in [
            (&laea, [2.0e7, 2.0e7, 2.1e7, 2.1e7]),
            (&tm, [0.0, 1.5e7, 1.0e6, 1.6e7]),
            (&tm, [0.0, 2.1e7, 1.0e6, 2.2e7]),
            (&tm, [0.0, -2.2e7, 1.0e6, -2.1e7]),
            (&tm, [1.1e7, 6.5e6, 1.2e7, 7.0e6]),
        ] {
            let out = projected(crs, bbox);
            for (fx, fy) in [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0)] {
                assert_eq!(out.position_on_earth(nan, fx, fy), None, "{bbox:?}");
            }
        }
        // A TM box from 81°N across the pole: past it, the top and middle
        // points name no place, and `project_node` is NaN there as past the
        // LAEA disk; the bottom is on the Earth.
        let across = projected(&tm, [0.0, 9.0e6, 1.0e6, 2.1e7]);
        for fy in [0.0, 0.5] {
            let (lon, lat) = across.project_node(nan, 0.5, fy);
            assert!(lon.is_nan() && lat.is_nan(), "past the pole: {lon}, {lat}");
            assert_eq!(across.position_on_earth(nan, 0.5, fy), None);
        }
        let (_, lat) = across.position_on_earth(nan, 0.5, 1.0).unwrap();
        assert!((80.0..90.0).contains(&lat), "{lat}");

        // The top row of the #1110 meta-tile, 256 px, runs from 36° to 62°
        // east of the meridian. Past about 56° the Snyder series answered
        // with a place anywhere, (296.1°, 84.5°) at column 195 with macOS's
        // libm, and this passed it on (#1113). Krüger's names every column's
        // place (#1130), eastward and southward column by column, from
        // PROJ's 63.230 520 390°E 75.537 891 261°N at column 0 to
        // 88.996 950 258°E 66.092 962 685°N at column 255 (`cs2cs -f %.9f
        // +proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0
        // +datum=WGS84 +to +proj=longlat +datum=WGS84` of their centres).
        let tile = projected(&tm, [1_448_154.7, 7_240_773.4, 2_896_309.4, 8_688_928.1]);
        let top = |i: u32| tile.position_on_earth(nan, (f64::from(i) + 0.5) / 256.0, 0.5 / 256.0);
        let mut last = (f64::MIN, f64::MAX);
        for i in 0..256 {
            let (lon, lat) = top(i).unwrap_or_else(|| panic!("column {i}"));
            assert!(lon > last.0 && lat < last.1, "column {i}: {lon}, {lat}");
            last = (lon, lat);
        }
        for (i, lon, lat) in [
            (0, 63.230_520_390, 75.537_891_261),
            (255, 88.996_950_258, 66.092_962_685),
        ] {
            let (got_lon, got_lat) = top(i).unwrap();
            assert!(
                (got_lon - lon).abs() < 1e-8 && (got_lat - lat).abs() < 1e-8,
                "column {i}: {got_lon}, {got_lat}"
            );
        }
    }

    /// Placing a tile's rows in a taller tile keeps every pixel's value and
    /// makes the added rows nodata, for every `RasterValues` variant.
    #[test]
    fn placed_in_rows_pads_with_nodata_and_keeps_values() {
        let tiles = [
            RasterValues::F64(vec![Some(1.0), None, Some(3.0), Some(4.0)]),
            RasterValues::F32 {
                data: vec![1.0, -9999.0, 3.0, 4.0],
                nodata: Some(-9999.0),
            },
            RasterValues::U8 {
                data: vec![10, 255, 30, 40],
                nodata: Some(255),
                gain: 0.5,
                offset: -1.0,
            },
            // No sentinel: every sample is a value, so it is boxed.
            RasterValues::U8 {
                data: vec![0, 255, 30, 40],
                nodata: None,
                gain: 0.5,
                offset: -1.0,
            },
        ];
        for values in tiles {
            let before: Vec<Option<f64>> = values.iter_values().collect();
            let tile = RasterTile {
                width: 2,
                height: 2,
                values,
            };
            let placed = tile.placed_in_rows(1, 5);
            assert_eq!((placed.width, placed.height), (2, 5));
            let after: Vec<Option<f64>> = placed.values.iter_values().collect();
            let mut want = vec![None, None];
            want.extend(&before);
            want.extend([None; 4]);
            assert_eq!(after, want);
        }
        // Every row already in place: the same tile.
        let whole = RasterTile {
            width: 1,
            height: 2,
            values: RasterValues::U8 {
                data: vec![7, 8],
                nodata: None,
                gain: 1.0,
                offset: 0.0,
            },
        }
        .placed_in_rows(0, 2);
        assert!(matches!(
            whole.values,
            RasterValues::U8 { nodata: None, .. }
        ));
    }

    /// The engine is asked only for the rows inside the CRS, and not at all
    /// when there are none; the returned image has the requested size.
    #[test]
    fn rendering_in_the_domain_asks_the_engine_only_for_rows_inside_it() {
        /// Draws a value on every pixel it is asked for, and records the box
        /// and height of each request.
        #[derive(Default)]
        struct Everywhere(std::sync::Mutex<Vec<([f64; 4], u32)>>);
        impl MapEngine for Everywhere {
            fn get_raster_tile(
                &self,
                bbox: [f64; 4],
                width: u32,
                height: u32,
                _: Option<DateTime<Utc>>,
                _: &OutputCrs,
                _: Option<&str>,
                _: Option<f64>,
                _: Option<DateTime<Utc>>,
            ) -> Result<RasterTile, DataServerError> {
                self.0.lock().unwrap().push((bbox, height));
                Ok(RasterTile {
                    width,
                    height,
                    values: vec![Some(1.0); (width * height) as usize].into(),
                })
            }
            fn raster_info(&self) -> RasterInfo {
                unreachable!()
            }
        }
        let engine = Everywhere::default();
        let render = |bbox| {
            get_raster_tile_in_domain(
                &engine,
                bbox,
                4,
                10,
                None,
                &OutputCrs::Wgs84,
                None,
                None,
                None,
            )
            .unwrap()
        };

        assert!(render([-10.0, 90.0, 10.0, 110.0]).is_none());
        assert!(engine.0.lock().unwrap().is_empty(), "engine called");
        // Nor for a projected box wholly outside its projection (#1089): past
        // the LAEA disk, or past TM's pole or its area's east edge (#1113,
        // #1130). None of them has a read window.
        for (code, bbox) in [
            ("EPSG:3035", [2.0e7, 2.0e7, 2.1e7, 2.1e7]),
            ("EPSG:3067", [0.0, 2.1e7, 1.0e6, 2.2e7]),
            ("EPSG:3067", [1.1e7, 6.5e6, 1.2e7, 7.0e6]),
        ] {
            let crs = projected_output_crs(code).unwrap();
            let read = crate::geo::wgs84_envelope(&crs, bbox).unwrap_or([0.0; 4]);
            let outside = OutputCrs::Projected { crs, bbox };
            let tile =
                get_raster_tile_in_domain(&engine, read, 4, 10, None, &outside, None, None, None);
            assert!(tile.unwrap().is_none(), "{code}");
            assert!(engine.0.lock().unwrap().is_empty(), "{code}: engine called");
        }

        let tile = render([-10.0, 80.0, 10.0, 100.0]).unwrap();
        assert_eq!((tile.width, tile.height), (4, 10));
        let drawn: Vec<bool> = tile.values.iter_values().map(|v| v.is_some()).collect();
        assert_eq!(drawn, [[false; 20], [true; 20]].concat());
        let (bbox, rows) = engine.0.lock().unwrap()[0];
        assert_eq!(rows, 5);
        assert_eq!(bbox, [-10.0, 80.0, 10.0, 90.0]);

        let bands = get_raster_tiles_in_domain(
            &engine,
            [-10.0, -100.0, 10.0, -80.0],
            4,
            10,
            None,
            &OutputCrs::Wgs84,
            &["a", "b"],
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(bands.len(), 2);
        for band in bands {
            let drawn: Vec<bool> = band.values.iter_values().map(|v| v.is_some()).collect();
            assert_eq!(drawn, [[true; 20], [false; 20]].concat());
        }
    }
}
