//! Internal meta-tiling for arbitrary-bbox Web Mercator WMS GetMap renders (#202).
//!
//! A fullscreen WMS client (e.g. OpenLayers `ImageWMS`) requests an arbitrary
//! bbox + width + height per pan/zoom event, so the rendered-image cache — keyed
//! on the exact bbox — almost never hits (≈3% in production). Meta-tiling fixes
//! the *strategy*: decompose each Web Mercator GetMap into a grid of fixed
//! 256×256 tiles aligned to the WebMercatorQuad grid, render and cache *those*
//! (fixed bbox + fixed size → repeating key → high hit rate), then resample the
//! covered tiles into the client's exact viewport. The expensive per-source work
//! (TIFF decode, per-pixel projection, colorize) is cached at tile granularity
//! and reused across overlapping viewports; the final crop/resample is cheap.
//!
//! ## Resolution ladder
//!
//! Tiles are rendered at one of a **half-octave** ladder of resolutions whose
//! even steps coincide with the standard integer WebMercator zoom levels
//! (`Z0_RES / 2^(level/2)`). For each request we snap to the *finest* ladder
//! step whose resolution is still ≤ the request's ground resolution, so the
//! mosaic is always **downsampled** to the viewport (crisp, never upscaled).
//! A discrete-zoom OpenLayers view lands exactly on an even step → pure crop, no
//! resample. A smooth-zoom view snaps at most one half-octave finer (≤ 1.41×
//! linear over-render, paid only on a tile cache miss; ≤ 1.19× perceived
//! softening relative to a nearest-neighbour snap).
//!
//! ## Scope
//!
//! Web Mercator (EPSG:3857) requests only — that is the production hot path and
//! it keeps assembly a pure affine resample in Mercator metres (no reprojection;
//! the engine already reprojects source→Mercator inside each tile render). Other
//! CRSs fall back to a direct single-shot render. Degenerate or pathologically
//! large requests return [`MetaTile::Fallback`] so the caller renders directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};

use ds_core::error::DataServerError;
use ds_core::map_engine::RasterTile;

use crate::colorize;
use crate::{ColorMap, ImageFormat};

/// Tile edge length in pixels (WebMercatorQuad standard).
const TILE_PX: u32 = 256;
/// EPSG:3857 sphere radius (metres) — from the shared source of truth.
const R: f64 = ds_core::web_mercator::EARTH_RADIUS;
/// Half the Web Mercator world span in metres (`π·R`). The grid origin is the
/// top-left corner `(-ORIGIN, +ORIGIN)`.
const ORIGIN: f64 = std::f64::consts::PI * R;
/// Ground resolution at zoom 0 (metres/pixel): `2·ORIGIN / 256 ≈ 156543.034`.
const Z0_RES: f64 = (2.0 * ORIGIN) / TILE_PX as f64;
/// Maximum half-octave ladder level (`level/2 ≈ zoom`, so ~zoom 24).
const MAX_LEVEL: i32 = 48;
/// Tile-budget floor so small requests always qualify regardless of rounding.
const MIN_TILE_BUDGET: usize = 64;
/// Tile-budget multiplier over the request's own pixel-implied tile count
/// (`width·height / 256²`). Half-octave snapping inflates each axis by at most
/// √2 (≈2× tiles) and grid straddle adds one row and column, so a square-pixel
/// request never needs more than ~3× its pixel-implied count; 4× leaves margin.
const TILE_BUDGET_FACTOR: usize = 4;

/// Maximum covering tiles allowed for a `width × height` request before
/// meta-tiling declines to a direct render (#491). Pixel-proportional: a fixed
/// 256-tile cap put retina-class viewports (≥4K) right on the cliff, randomly
/// flipping the same window between cached meta-tiling and uncached direct
/// rendering depending on how the bbox landed on the ladder. Scaling with the
/// request keeps every square-pixel viewport cacheable (WMS bounds requests at
/// 8000 px/dim, 64 Mpx) while still declining the pathology the guard exists
/// for: a bbox whose aspect wildly mismatches the pixel aspect snaps to a fine
/// level along one axis and demands orders of magnitude more tiles than the
/// pixels justify.
fn tile_budget(width: u32, height: u32) -> usize {
    let px_tiles =
        (width as usize).saturating_mul(height as usize) / (TILE_PX as usize * TILE_PX as usize);
    TILE_BUDGET_FACTOR.saturating_mul(px_tiles) + MIN_TILE_BUDGET
}

/// Requests declined because their covering tile count exceeded
/// [`tile_budget`] (cumulative since process start). Surfaced on `/metrics` as
/// `metatile_declines_total`; a sustained rate means clients are sending
/// viewports outside the cacheable envelope (extreme bbox/pixel aspect
/// mismatch) and paying an uncached direct render every frame.
static BUDGET_DECLINES: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of tile-budget declines, for the `/metrics` scrape.
pub fn budget_declines_total() -> u64 {
    BUDGET_DECLINES.load(Ordering::Relaxed)
}

// Web Mercator metre <-> WGS84 degree conversions come from the shared
// `ds_core::web_mercator` module (the single source of truth, #452): `lat_to_y`
// is UNCLAMPED — correct for the viewport bounds, which must match the client's
// request bbox even past ±85°. The ±85° tile-grid clamp ([`LAT_LIMIT_DEG`]) is
// applied explicitly, only where tile *indices* are selected (`row0`/`row1`).
use ds_core::web_mercator::{lat_to_y, lon_to_x, x_to_lon, y_to_lat, LAT_LIMIT_DEG};

/// Resolution (metres/pixel) of a half-octave ladder level.
fn level_res(level: i32) -> f64 {
    Z0_RES / 2f64.powf(level as f64 / 2.0)
}

/// Snap a ground resolution to the finest half-octave ladder level whose
/// resolution is still ≤ `res` (i.e. never coarser → the mosaic is downsampled,
/// never upscaled).
///
/// A small epsilon (`1e-6` in log2 space) makes an *exact* standard-zoom
/// resolution snap to its own level instead of one step finer — without it,
/// floating-point noise would round every discrete-zoom request up a half-octave
/// and force a needless 1.41× over-render. The bounded cost: a resolution that
/// falls *just below* a step, within the epsilon band, can snap down to it and
/// upscale by at most `2^(5e-7) ≈ 1.0000003×` (sub-nanometre per pixel) — visually
/// nil, and well worth avoiding the per-request over-render.
///
/// Clamped at the **low** end to level 0 (`Z0_RES`, the whole world in one tile);
/// a request coarser than that still downsamples, so 0 is safe. The **high** end
/// is intentionally *not* clamped: a value `> MAX_LEVEL` means the request is
/// finer than the deepest ladder step (~9 mm/px — an absurd over-zoom), and the
/// caller declines to meta-tiling [`MetaTile::Fallback`] rather than clamp (which
/// would render coarser-than-requested tiles and silently upscale).
fn snap_level(res_m_per_px: f64) -> i32 {
    if !res_m_per_px.is_finite() || res_m_per_px <= 0.0 {
        return MAX_LEVEL + 1; // decline → Fallback
    }
    // L_level = Z0_RES / 2^(level/2) ≤ res  ⇔  level ≥ 2·log2(Z0_RES / res)
    let level = (2.0 * (Z0_RES / res_m_per_px).log2() - 1e-6).ceil() as i32;
    level.max(0)
}

/// The fixed part of a tile's cache key (everything but the tile's grid
/// position). Built once per request and cloned into each tile's [`TileKey`].
#[derive(Clone, Debug)]
pub struct TileKeyPrefix {
    pub layer: String,
    pub parameter: Option<String>,
    pub style: String,
    pub time: Option<DateTime<Utc>>,
    /// Vertical level, pre-quantized via [`crate::quantize_z`].
    pub z: Option<i64>,
    /// Forecast model run (reference time); `None` = the engine's latest run.
    /// Keeps tiles for distinct runs from colliding in the meta-tile cache.
    pub reference_time: Option<DateTime<Utc>>,
}

/// Cache key for one rendered+colorized meta-tile.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    layer: String,
    parameter: Option<String>,
    style: String,
    time: Option<DateTime<Utc>>,
    z: Option<i64>,
    reference_time: Option<DateTime<Utc>>,
    level: i32,
    col: i64,
    row: i64,
}

/// A cached tile's decoded, colorized pixels. `Some` holds the
/// `TILE_PX × TILE_PX × 4` RGBA buffer; `None` marks an all-nodata tile, cached
/// as a lightweight marker so a sparse extent (e.g. radar outside coverage) is
/// neither re-decoded on every request nor allowed to crowd the cache with
/// 256 KB zero buffers. A `None` marker is still a cache *hit*.
#[derive(Clone)]
pub struct CachedTilePixels {
    rgba: Option<Arc<[u8]>>,
}

/// Weight function: pixel bytes (zero for a nodata marker) + a flat
/// allowance for the owned key strings + node overhead.
fn weigh_tile_pixels(key: &TileKey, val: &CachedTilePixels) -> u64 {
    val.rgba.as_ref().map_or(0, |r| r.len()) as u64
        + key.layer.len() as u64
        + key.parameter.as_ref().map_or(0, String::len) as u64
        + key.style.len() as u64
        + 96
}

/// LRU cache of decoded, colorized meta-tiles (RGBA), weighted by byte size.
///
/// Distinct from the per-collection GeoTIFF *compressed-byte* tile cache and
/// from the request-keyed [`crate::RenderedCache`]. This one is keyed on the
/// fixed tile grid so overlapping fullscreen viewports reuse the same entries.
pub struct TilePixelCache {
    cache: ds_cache::ByteBoundedCache<TileKey, CachedTilePixels>,
}

impl TilePixelCache {
    pub fn new(capacity_mb: u64) -> Self {
        // ~256 KB per RGBA tile for initial map sizing.
        Self {
            cache: ds_cache::ByteBoundedCache::new(
                capacity_mb.saturating_mul(ds_cache::MIB),
                256 * 1024,
                weigh_tile_pixels,
            ),
        }
    }

    /// (hits, misses) counters.
    pub fn stats(&self) -> (u64, u64) {
        self.cache.stats()
    }
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }
    pub fn capacity(&self) -> u64 {
        self.cache.capacity_bytes()
    }
    pub fn len(&self) -> usize {
        self.cache.len()
    }
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
    /// Snapshot for `/metrics`.
    pub fn metrics(&self) -> ds_cache::CacheMetrics {
        self.cache.metrics()
    }
}

/// Per-render phase timing, returned with [`MetaTile::Image`] so the caller can
/// log where a slow render spent its time (the framework-free render layer can't
/// log itself). Times are wall-clock milliseconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetaTileStats {
    /// Covering tiles for this request.
    pub tiles: u32,
    /// Of those, how many were cache misses (rendered via the engine closure).
    pub misses: u32,
    /// Wall time of the whole tile loop: engine `get_raster_tile` (file read +
    /// decode + reproject) plus colorize for the `misses`, plus cache lookups
    /// for hits (normally negligible). For a cold render (all misses) this is
    /// the miss-render time — the metric the cold-tail diagnosis cares about.
    pub tile_loop_ms: u64,
    /// Time assembling the mosaic into the output (nearest-neighbour resample).
    pub assemble_ms: u64,
    /// Time encoding the final image (PNG/JPEG/WebP). 0 for an all-nodata render.
    pub encode_ms: u64,
}

/// Outcome of a meta-tiled render.
pub enum MetaTile {
    /// Assembled, encoded image bytes in the requested format, plus phase timing.
    Image {
        bytes: Vec<u8>,
        stats: MetaTileStats,
    },
    /// Every covered pixel is nodata — the caller should emit its own
    /// transparent tile (matching the existing all-nodata fast path). `stats`
    /// still carries the tile-loop timing (assemble/encode are skipped, so 0).
    Empty { stats: MetaTileStats },
    /// Meta-tiling declined (degenerate bbox, over-zoom past the ladder, or a
    /// covering tile count above [`tile_budget`]); the caller should fall back
    /// to a direct single-shot render.
    Fallback,
}

/// Render a Web Mercator GetMap by decomposing it into cached 256×256 tiles and
/// resampling them to the exact viewport.
///
/// `bbox_deg` is the viewport in WGS84 degrees `[west, south, east, north]`
/// (already converted from EPSG:3857 metres by the handler). `render_tile` must
/// render one tile at the given WGS84-degree bbox and pixel size in Web Mercator
/// (i.e. call `get_raster_tile(bbox, w, h, OutputCrs::WebMercator, …)`); the
/// helper colorizes and caches its result.
#[allow(clippy::too_many_arguments)]
pub fn render_metatiled<F>(
    bbox_deg: [f64; 4],
    width: u32,
    height: u32,
    prefix: &TileKeyPrefix,
    colormap: &dyn ColorMap,
    format: ImageFormat,
    cache: &TilePixelCache,
    render_tile: F,
) -> Result<MetaTile, DataServerError>
where
    F: Fn([f64; 4], u32, u32) -> Result<RasterTile, DataServerError>,
{
    let [w, s, e, n] = bbox_deg;
    if width == 0
        || height == 0
        || !(w.is_finite() && s.is_finite() && e.is_finite() && n.is_finite())
    {
        return Ok(MetaTile::Fallback);
    }

    // Viewport in Web Mercator metres. Mercator Y increases northward. The
    // bounds use the shared UNCLAMPED `lat_to_y`, so res_x/res_y and the output
    // coordinate map match the client's request bbox even when it reaches past
    // ±85° toward a pole; the ±85° clamp belongs only to *tile selection*
    // (`row0`/`row1` below), not the viewport mapping (#452).
    let west_m = lon_to_x(w);
    let east_m = lon_to_x(e);
    let north_m = lat_to_y(n);
    let south_m = lat_to_y(s);
    if !(east_m > west_m && north_m > south_m) {
        // Antimeridian crossing or degenerate extent — let the caller render directly.
        return Ok(MetaTile::Fallback);
    }

    // Snap to the ladder using the finer of the two axis resolutions (so neither
    // axis is upscaled), then derive the tile grid at that level.
    let res = ((east_m - west_m) / width as f64).min((north_m - south_m) / height as f64);
    let level = snap_level(res);
    if level > MAX_LEVEL {
        // Requested resolution is finer than the deepest ladder step (~9 mm/px —
        // an absurd over-zoom). Meta-tiling would have to upscale; render
        // directly at the exact requested resolution instead.
        return Ok(MetaTile::Fallback);
    }
    let span = TILE_PX as f64 * level_res(level); // tile edge length in metres

    // Covering tile index range. Origin is the world top-left (-ORIGIN, +ORIGIN);
    // column grows eastward, row grows southward. Sample points sit strictly
    // inside the viewport, so a tiny epsilon trims a spurious tile when an edge
    // lands exactly on a grid line.
    let eps = span * 1e-9;
    // Tile rows use the ±85°-CLAMPED bounds (`lat_to_y`): tiles only exist
    // within the valid Web Mercator world, so output pixels past ±85° (present
    // when the exact `north_m`/`south_m` above run to a pole) have no tile and
    // the assembly draws them transparent — no phantom rows to render.
    let col0 = ((west_m + ORIGIN) / span).floor() as i64;
    let col1 = ((east_m + ORIGIN - eps) / span).floor() as i64;
    let row0 = ((ORIGIN - lat_to_y(n.clamp(-LAT_LIMIT_DEG, LAT_LIMIT_DEG))) / span).floor() as i64;
    let row1 =
        ((ORIGIN - lat_to_y(s.clamp(-LAT_LIMIT_DEG, LAT_LIMIT_DEG)) - eps) / span).floor() as i64;
    let ncols = (col1 - col0 + 1).max(1) as usize;
    let nrows = (row1 - row0 + 1).max(1) as usize;
    if ncols.saturating_mul(nrows) > tile_budget(width, height) {
        BUDGET_DECLINES.fetch_add(1, Ordering::Relaxed);
        return Ok(MetaTile::Fallback);
    }

    // Render or fetch each covering tile's RGBA pixels. `None` = an all-nodata
    // tile (cached as a marker, drawn transparent at assembly time). The cover
    // is a dense rectangle, so the tiles live in a row-major `Vec` indexed by
    // grid position — the assembly loop below samples it 4× per output pixel,
    // and a hash lookup there was ~8× the cost of the whole resample at
    // megapixel viewports.
    let mut tiles = Mosaic {
        col0,
        row0,
        ncols,
        nrows,
        tiles: Vec::with_capacity(ncols * nrows),
    };
    let mut any_data = false;
    let mut misses: u32 = 0;
    let tiles_count = (ncols * nrows) as u32;
    let t_loop = Instant::now();
    for row in row0..=row1 {
        for col in col0..=col1 {
            let key = TileKey {
                layer: prefix.layer.clone(),
                parameter: prefix.parameter.clone(),
                style: prefix.style.clone(),
                time: prefix.time,
                z: prefix.z,
                reference_time: prefix.reference_time,
                level,
                col,
                row,
            };
            let rgba = if let Some(c) = cache.cache.get(&key) {
                any_data |= c.rgba.is_some();
                c.rgba
            } else {
                misses += 1;
                // Tile bbox in metres → WGS84 degrees for the engine call.
                let tx0 = -ORIGIN + col as f64 * span;
                let tx1 = tx0 + span;
                let ty_top = ORIGIN - row as f64 * span;
                let ty_bot = ty_top - span;
                let tbbox = [
                    x_to_lon(tx0),
                    y_to_lat(ty_bot),
                    x_to_lon(tx1),
                    y_to_lat(ty_top),
                ];
                let tile = render_tile(tbbox, TILE_PX, TILE_PX)?;
                // All-nodata tiles are cached as a `None` marker: cheap to store
                // (no 256 KB buffer) yet still a cache hit, so a sparse extent is
                // not re-decoded every request nor crowds out real-data tiles.
                let entry: Option<Arc<[u8]>> = if tile.is_empty() {
                    None
                } else {
                    any_data = true;
                    Some(Arc::<[u8]>::from(
                        colorize(&tile, colormap).into_boxed_slice(),
                    ))
                };
                cache.cache.insert(
                    key,
                    CachedTilePixels {
                        rgba: entry.clone(),
                    },
                );
                entry
            };
            tiles.tiles.push(rgba);
        }
    }
    let tile_loop_ms = t_loop.elapsed().as_millis() as u64;

    if !any_data {
        return Ok(MetaTile::Empty {
            stats: MetaTileStats {
                tiles: tiles_count,
                misses,
                tile_loop_ms,
                assemble_ms: 0,
                encode_ms: 0,
            },
        });
    }

    // Resample the tile mosaic into the exact viewport. Output pixels are
    // uniform in Mercator metres, so this is a pure affine map into global
    // tile-pixel space, sampled with premultiplied-alpha bilinear (avoids dark
    // fringes bleeding from transparent nodata pixels).
    let res_x = (east_m - west_m) / width as f64;
    let res_y = (north_m - south_m) / height as f64;
    let to_global_x = |x_m: f64| (x_m + ORIGIN) / span * TILE_PX as f64;
    let to_global_y = |y_m: f64| (ORIGIN - y_m) / span * TILE_PX as f64;

    // usize arithmetic throughout: `width * height * 4` overflows `u32` past
    // ~46 340 px/side. Callers must keep `width * height` within a sane bound
    // (the WMS handler enforces MAX_MAP_PIXELS = 64M, MAX_MAP_DIMENSION = 8000).
    let (w_us, h_us) = (width as usize, height as usize);
    let t_assemble = Instant::now();
    let mut out = vec![0u8; w_us * h_us * 4];
    for py in 0..height {
        let y_m = north_m - (py as f64 + 0.5) * res_y;
        let gy = to_global_y(y_m);
        for px in 0..width {
            let x_m = west_m + (px as f64 + 0.5) * res_x;
            let gx = to_global_x(x_m);
            let rgba = sample_nearest(&tiles, gx, gy);
            let o = (py as usize * w_us + px as usize) * 4;
            out[o..o + 4].copy_from_slice(&rgba);
        }
    }
    let assemble_ms = t_assemble.elapsed().as_millis() as u64;

    let t_encode = Instant::now();
    let bytes = match format {
        ImageFormat::Png => crate::encode_png(&out, width, height)?,
        ImageFormat::Jpeg => crate::encode_jpeg(&out, width, height)?,
        ImageFormat::Webp => crate::encode_webp(&out, width, height)?,
    };
    let encode_ms = t_encode.elapsed().as_millis() as u64;
    Ok(MetaTile::Image {
        bytes,
        stats: MetaTileStats {
            tiles: tiles_count,
            misses,
            tile_loop_ms,
            assemble_ms,
            encode_ms,
        },
    })
}

/// The rendered covering tiles, stored row-major over the dense rectangular
/// grid `[col0..col0+ncols) × [row0..row0+nrows)`. `None` = an all-nodata
/// tile. Indexed (not hashed) because the assembly loop samples it 4× per
/// output pixel.
struct Mosaic {
    col0: i64,
    row0: i64,
    ncols: usize,
    nrows: usize,
    tiles: Vec<Option<Arc<[u8]>>>,
}

impl Mosaic {
    /// The tile at grid position `(col, row)`, or `None` for a nodata marker
    /// or a position outside the cover (only reachable ≤1px past an edge).
    #[inline]
    fn tile(&self, col: i64, row: i64) -> Option<&Arc<[u8]>> {
        let c = col.wrapping_sub(self.col0) as usize;
        let r = row.wrapping_sub(self.row0) as usize;
        if c >= self.ncols || r >= self.nrows {
            return None;
        }
        self.tiles[r * self.ncols + c].as_ref()
    }
}

/// Fetch one global tile-pixel (nearest) from the covering set; transparent if
/// the pixel falls outside the rendered tiles (only possible ≤1px past an edge).
#[inline]
fn global_pixel(tiles: &Mosaic, xi: i64, yi: i64) -> [u8; 4] {
    let col = xi.div_euclid(TILE_PX as i64);
    let row = yi.div_euclid(TILE_PX as i64);
    let lx = xi.rem_euclid(TILE_PX as i64) as usize;
    let ly = yi.rem_euclid(TILE_PX as i64) as usize;
    match tiles.tile(col, row) {
        // Present with data; nodata markers and absent tiles (≤1px past an
        // edge) are transparent.
        Some(rgba) => {
            let o = (ly * TILE_PX as usize + lx) * 4;
            [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]]
        }
        None => [0, 0, 0, 0],
    }
}

/// Nearest-neighbour sample of the mosaic at global tile-pixel coordinates
/// `(gx, gy)`: the texel whose cell `[n, n+1)` contains the sample point, i.e.
/// `floor(gx)` — the same source→pixel convention the engine and the direct
/// (non-meta) render path use (`world_to_pixel_f64` + `floor`).
///
/// **Assembly is deliberately nearest, not bilinear (#202 follow-up).** The
/// source layers are categorical/discrete — a radar dBZ composite colorizes to
/// a small fixed palette (≤256 colours) — and bilinearly blending adjacent
/// texels *fabricates intermediate colours*: a 46-colour palette became ~22 000
/// shades plus a full 256-level alpha ramp, which defeats the encoder's PNG8
/// indexing and bloated the output ~9× per pixel (0.68 vs 0.077 B/px), worst on
/// high-DPI screens whose request resolution falls between zoom-ladder levels
/// (maximal fractional sampling ⇒ blending on nearly every pixel). Nearest
/// preserves the discrete palette so PNG8 is kept, makes the Web Mercator path
/// match the direct / EPSG:3067 path in character, and is cheaper (one lookup
/// vs four texels + premultiplied blend). The cost is crisper (un-smoothed)
/// edges when up/down-scaling between ladder levels — but genuinely continuous
/// fields colorize to >256 colours and stay RGBA32 regardless, so they forgo
/// only cosmetic smoothing, not size.
#[inline]
fn sample_nearest(tiles: &Mosaic, gx: f64, gy: f64) -> [u8; 4] {
    global_pixel(tiles, gx.floor() as i64, gy.floor() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×1-texel mosaic in one tile: texel (0,0)=`a`, texel (1,0)=`b`, rest
    /// `fill`. Lets us probe sampling exactly across a colour boundary.
    fn two_color_tile(a: [u8; 4], b: [u8; 4], fill: [u8; 4]) -> Mosaic {
        let mut tile = vec![0u8; TILE_PX as usize * TILE_PX as usize * 4];
        for px in tile.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        tile[0..4].copy_from_slice(&a); // texel (0,0)
        tile[4..8].copy_from_slice(&b); // texel (1,0)
        Mosaic {
            col0: 0,
            row0: 0,
            ncols: 1,
            nrows: 1,
            tiles: vec![Some(Arc::from(tile.into_boxed_slice()))],
        }
    }

    #[test]
    fn sample_nearest_picks_containing_texel() {
        let a = [37u8, 211, 99, 255];
        let b = [10u8, 20, 30, 128];
        let m = two_color_tile(a, b, [0, 0, 0, 0]);
        // gx in [0,1) → texel 0; gx in [1,2) → texel 1. y stays in row 0.
        assert_eq!(sample_nearest(&m, 0.5, 0.5), a);
        assert_eq!(sample_nearest(&m, 0.99, 0.5), a);
        assert_eq!(sample_nearest(&m, 1.0, 0.5), b);
        assert_eq!(sample_nearest(&m, 1.5, 0.5), b);
    }

    #[test]
    fn sample_nearest_never_blends_across_a_boundary() {
        // Regression for the PNG8 bloat: assembling across a colour boundary
        // must return one of the two source colours, NEVER a fabricated blend.
        // (Bilinear assembly produced ~22k shades from a ≤256-colour palette,
        // forcing RGBA32 and ~9× larger images.)
        let a = [200u8, 0, 0, 255];
        let b = [0u8, 0, 200, 255];
        let m = two_color_tile(a, b, [0, 0, 0, 0]);
        for i in 0..=100 {
            let gx = i as f64 / 100.0 * 2.0; // sweep 0..2 across the boundary
            let s = sample_nearest(&m, gx, 0.5);
            assert!(
                s == a || s == b || s == [0, 0, 0, 0],
                "nearest must not blend: got {s:?} at gx={gx}"
            );
        }
    }

    #[test]
    fn sample_nearest_transparent_outside_tiles() {
        // Fully-transparent region (the common radar case) → [0, 0, 0, 0].
        let empty = Mosaic {
            col0: 0,
            row0: 0,
            ncols: 1,
            nrows: 1,
            tiles: vec![None],
        };
        assert_eq!(sample_nearest(&empty, 100.3, 50.7), [0, 0, 0, 0]);
    }

    #[test]
    fn standard_zoom_resolutions_snap_exactly() {
        // Resolution at integer WebMercator zoom z is Z0_RES / 2^z, which must
        // snap to even ladder level 2z with no over-render (level_res == res).
        for z in 0..=20 {
            let res = Z0_RES / 2f64.powi(z);
            let level = snap_level(res);
            assert_eq!(level, 2 * z, "zoom {z} should snap to even level {}", 2 * z);
            assert!(
                (level_res(level) - res).abs() / res < 1e-9,
                "level_res must equal the standard-zoom resolution"
            );
        }
    }

    #[test]
    fn snap_never_upscales_and_is_bounded() {
        // For a resolution well between zoom 8 and 9 (outside the snap_level
        // epsilon band — see its doc for the bounded ~2^(5e-7) exception), the
        // chosen level must be finer (≤ res) and at most one half-octave finer.
        let res8 = Z0_RES / 2f64.powi(8);
        let res9 = Z0_RES / 2f64.powi(9);
        let res = (res8 + res9) / 2.0; // between two standard zooms
        let level = snap_level(res);
        assert!(
            level_res(level) <= res,
            "must never upscale (≤ requested res)"
        );
        assert!(
            level_res(level) >= res / std::f64::consts::SQRT_2 - 1e-6,
            "≤ one half-octave finer"
        );
        assert!((16..=18).contains(&level));
    }

    #[test]
    fn mercator_roundtrip() {
        for &(lon, lat) in &[(0.0, 0.0), (24.94, 60.17), (-122.4, 37.8), (180.0, 85.0)] {
            let x = lon_to_x(lon);
            let y = lat_to_y(lat);
            assert!((x_to_lon(x) - lon).abs() < 1e-6, "lon roundtrip");
            assert!((y_to_lat(y) - lat).abs() < 1e-4, "lat roundtrip");
        }
        // Grid corners.
        assert!((lon_to_x(-180.0) + ORIGIN).abs() < 1e-3);
        assert!((lat_to_y(ds_core::web_mercator::LAT_LIMIT_DEG) - ORIGIN).abs() < 1.0);
    }

    /// A solid colormap so every in-extent pixel is opaque red; lets us assert
    /// the assembled output without depending on a real engine.
    struct SolidRed;
    impl ColorMap for SolidRed {
        fn color(&self, value: Option<f64>) -> [u8; 4] {
            match value {
                Some(_) => [255, 0, 0, 255],
                None => [0, 0, 0, 0],
            }
        }
    }

    fn solid_tile(_b: [f64; 4], w: u32, h: u32) -> Result<RasterTile, DataServerError> {
        Ok(RasterTile {
            width: w,
            height: h,
            values: vec![Some(1.0); (w * h) as usize].into(),
        })
    }

    #[test]
    fn assembles_solid_region_and_caches_tiles() {
        let cache = TilePixelCache::new(64);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
            reference_time: None,
        };
        // A modest viewport around Helsinki at ~zoom 6.
        let bbox = [20.0, 58.0, 30.0, 64.0];
        let out = render_metatiled(
            bbox,
            512,
            512,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            solid_tile,
        )
        .unwrap();
        let bytes = match out {
            MetaTile::Image { bytes, .. } => bytes,
            _ => panic!("expected an image"),
        };
        assert!(
            bytes.starts_with(&[0x89, b'P', b'N', b'G']),
            "valid PNG header"
        );
        // The SolidRed mosaic is ≤256 colours (solid red + transparent edge),
        // so the encode path must take the indexed-palette branch. Pinning
        // the colour type here ensures a regression that bypasses
        // `encode_png_indexed` (always falling back to RGBA) is caught in the
        // metatile assembly path too, not only in the encoder unit tests.
        {
            let decoder = png::Decoder::new(&bytes[..]);
            let reader = decoder.read_info().unwrap();
            assert_eq!(
                reader.info().color_type,
                png::ColorType::Indexed,
                "a ≤256-colour mosaic must encode via the indexed-palette path"
            );
        }
        let (misses_first, _) = (cache.stats().1, ());
        assert!(misses_first > 0, "first render populates the tile cache");

        // A second, overlapping viewport must reuse cached tiles (hits > 0).
        let bbox2 = [21.0, 59.0, 31.0, 65.0];
        let _ = render_metatiled(
            bbox2,
            512,
            512,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            solid_tile,
        )
        .unwrap();
        let (hits, _) = cache.stats();
        assert!(hits > 0, "overlapping viewport should hit cached tiles");
    }

    #[test]
    fn all_nodata_returns_empty() {
        let cache = TilePixelCache::new(16);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
            reference_time: None,
        };
        let empty_tile = |_b: [f64; 4], w: u32, h: u32| {
            Ok(RasterTile {
                width: w,
                height: h,
                values: vec![None; (w * h) as usize].into(),
            })
        };
        let out = render_metatiled(
            [20.0, 58.0, 30.0, 64.0],
            256,
            256,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            empty_tile,
        )
        .unwrap();
        assert!(matches!(out, MetaTile::Empty { .. }));
    }

    #[test]
    fn tile_budget_scales_with_request_pixels() {
        // `4 · (W·H / 256²) + 64`, integer math pinned exactly.
        assert_eq!(tile_budget(10, 10), 64); // floor dominates tiny requests
        assert_eq!(tile_budget(1920, 1080), 188);
        assert_eq!(tile_budget(4503, 2255), 680); // observed prod retina client
        assert_eq!(tile_budget(8192, 8192), 4160);
    }

    #[test]
    fn aspect_mismatch_tile_count_falls_back() {
        let cache = TilePixelCache::new(16);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
            reference_time: None,
        };
        // The pathology the budget guards (#491): a whole-world-wide bbox on a
        // tall, narrow image. The fine y-resolution snaps the ladder deep, so
        // the world-spanning x-axis needs ~46 columns → ~2100 tiles, while the
        // request's own pixels justify a budget of only 192. Must decline (and
        // count the decline) rather than render a tile grid orders of
        // magnitude larger than the viewport.
        let declined_before = budget_declines_total();
        let out = render_metatiled(
            [-179.0, -85.0, 179.0, 85.0],
            256,
            8192,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            solid_tile,
        )
        .unwrap();
        assert!(matches!(out, MetaTile::Fallback));
        assert!(budget_declines_total() > declined_before);
    }

    #[test]
    fn retina_viewport_above_old_cap_still_metatiles() {
        let cache = TilePixelCache::new(256);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
            reference_time: None,
        };
        // Regression pin for #491: a square-pixel 4K-retina viewport whose
        // requested resolution sits just above a ladder step, so the half-
        // octave snap inflates the cover past the old fixed cap of 256 tiles.
        // It must meta-tile (cacheable) instead of falling back to an uncached
        // direct render. Build the bbox in Mercator metres around southern
        // Finland at 1.38× a ladder resolution, square pixels on both axes.
        let (width, height) = (3840u32, 2160u32);
        let res = level_res(12) * 1.38; // snaps to level 12, ~1.38×/axis inflation
        let (cx, cy) = (2_700_000.0, 8_500_000.0);
        let half_w = width as f64 * res / 2.0;
        let half_h = height as f64 * res / 2.0;
        let bbox = [
            x_to_lon(cx - half_w),
            y_to_lat(cy - half_h),
            x_to_lon(cx + half_w),
            y_to_lat(cy + half_h),
        ];
        let out = render_metatiled(
            bbox,
            width,
            height,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            solid_tile,
        )
        .unwrap();
        match out {
            MetaTile::Image { stats, .. } => {
                assert!(
                    stats.tiles as usize > 256,
                    "cover ({} tiles) should exceed the old fixed cap for this pin to bite",
                    stats.tiles
                );
                assert!(
                    (stats.tiles as usize) <= tile_budget(width, height),
                    "cover ({} tiles) must fit the pixel-proportional budget ({})",
                    stats.tiles,
                    tile_budget(width, height)
                );
            }
            _ => panic!("expected MetaTile::Image, got Fallback/Empty"),
        }
    }

    #[test]
    fn extreme_resolution_declines_to_fallback() {
        // A resolution finer than the deepest ladder step (~9 mm/px) would force
        // upscaling; snap_level must signal it and render_metatiled must decline
        // to Fallback rather than clamp + upscale.
        assert!(
            snap_level(0.001) > MAX_LEVEL,
            "1 mm/px is finer than the ladder"
        );
        assert_eq!(
            snap_level(Z0_RES / 2f64.powi(20)),
            40,
            "in-range still snaps"
        );
        let cache = TilePixelCache::new(16);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
            reference_time: None,
        };
        // ~1 µm/px viewport (tiny bbox, large image).
        let out = render_metatiled(
            [24.9400, 60.1700, 24.9401, 60.1701],
            2048,
            2048,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            solid_tile,
        )
        .unwrap();
        assert!(matches!(out, MetaTile::Fallback));
    }

    /// Linearly encode one Web Mercator metre coordinate into the red channel,
    /// over `[lo, hi]`, so a decoded pixel's red reveals which ground coordinate
    /// it was assembled from. Always opaque.
    struct AxisEncode {
        lo: f64,
        hi: f64,
    }
    impl ColorMap for AxisEncode {
        fn color(&self, value: Option<f64>) -> [u8; 4] {
            match value {
                Some(v) => {
                    let r = ((v - self.lo) / (self.hi - self.lo) * 255.0)
                        .round()
                        .clamp(0.0, 255.0) as u8;
                    [r, 0, 0, 255]
                }
                None => [0, 0, 0, 0],
            }
        }
    }

    /// Decode the RGBA pixels of a PNG produced by [`render_metatiled`].
    ///
    /// `encode_png` auto-selects an 8-bit indexed-palette encoding for
    /// buffers with ≤256 distinct colours (the common colormap case) and
    /// falls back to 32-bit RGBA otherwise. Either branch must round-trip
    /// to the same per-pixel RGBA — handle both colour types here so the
    /// fidelity tests below don't constrain which branch ran.
    fn decode_rgba(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder.read_info().unwrap();
        let info_meta = reader.info().clone();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buf).unwrap();
        let w = frame.width;
        let h = frame.height;
        let used = &buf[..frame.buffer_size()];
        match frame.color_type {
            png::ColorType::Rgba => (w, h, used.to_vec()),
            png::ColorType::Indexed => {
                // `.expect` keeps a malformed indexed PNG (PLTE missing)
                // legible rather than panicking with an opaque OOB index on
                // the next line. Our encoder always writes a PLTE.
                let plte = info_meta
                    .palette
                    .as_deref()
                    .expect("indexed PNG must carry a PLTE chunk");
                let trns = info_meta.trns.as_deref().unwrap_or(&[]);
                let mut out = Vec::with_capacity((w * h * 4) as usize);
                for &idx in used {
                    let p = idx as usize;
                    let r = plte[p * 3];
                    let g = plte[p * 3 + 1];
                    let b = plte[p * 3 + 2];
                    let a = if p < trns.len() { trns[p] } else { 255 };
                    out.extend_from_slice(&[r, g, b, a]);
                }
                (w, h, out)
            }
            other => panic!("unexpected decoded PNG colour type: {other:?}"),
        }
    }

    /// The strongest fidelity guard: assemble a field whose value at every pixel
    /// is that pixel's own Web Mercator X (longitude axis), encoded into red.
    /// The decoded output's red at pixel `px` must match the colormap of that
    /// output pixel's *own* mercator-X — i.e. the assembly places data at the
    /// geographically correct location. A column shift, transpose, or wrong-tile
    /// bug moves the gradient and fails the check. A separate run does the same
    /// for the Y (latitude) axis to catch row shifts / vertical flips.
    #[test]
    fn assembled_pixels_track_geography() {
        let bbox = [10.0, 55.0, 30.0, 65.0];
        let (w, h) = (400u32, 300u32);
        let west_m = lon_to_x(bbox[0]);
        let east_m = lon_to_x(bbox[2]);
        let north_m = lat_to_y(bbox[3]);
        let south_m = lat_to_y(bbox[1]);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
            reference_time: None,
        };

        // --- X axis: value = mercator-X, constant down each column. ---
        let x_render = |b: [f64; 4], tw: u32, th: u32| {
            let tw_west = lon_to_x(b[0]);
            let tw_east = lon_to_x(b[2]);
            let mut values = Vec::with_capacity((tw * th) as usize);
            for _row in 0..th {
                for i in 0..tw {
                    values.push(Some(
                        tw_west + (i as f64 + 0.5) / tw as f64 * (tw_east - tw_west),
                    ));
                }
            }
            Ok(RasterTile {
                width: tw,
                height: th,
                values: values.into(),
            })
        };
        let cmap = AxisEncode {
            lo: west_m,
            hi: east_m,
        };
        let cache = TilePixelCache::new(64);
        let out = render_metatiled(
            bbox,
            w,
            h,
            &prefix,
            &cmap,
            ImageFormat::Png,
            &cache,
            x_render,
        )
        .unwrap();
        let bytes = match out {
            MetaTile::Image { bytes, .. } => bytes,
            _ => panic!("expected image"),
        };
        let (dw, dh, rgba) = decode_rgba(&bytes);
        assert_eq!((dw, dh), (w, h));
        let mut max_diff = 0i32;
        // Skip a 2px border: bilinear neighbours there can fall in uncovered tiles.
        for py in 2..h - 2 {
            for px in 2..w - 2 {
                let x_m = west_m + (px as f64 + 0.5) / w as f64 * (east_m - west_m);
                let expected = cmap.color(Some(x_m))[0] as i32;
                let got = rgba[((py * w + px) * 4) as usize] as i32;
                max_diff = max_diff.max((expected - got).abs());
            }
        }
        assert!(
            max_diff <= 3,
            "X gradient mismatch: max diff {max_diff} (geometry/shift bug?)"
        );

        // --- Y axis: value = mercator-Y, constant across each row. ---
        let y_render = |b: [f64; 4], tw: u32, th: u32| {
            let tn = lat_to_y(b[3]);
            let ts = lat_to_y(b[1]);
            let mut values = Vec::with_capacity((tw * th) as usize);
            for row in 0..th {
                let y_m = tn - (row as f64 + 0.5) / th as f64 * (tn - ts);
                for _i in 0..tw {
                    values.push(Some(y_m));
                }
            }
            Ok(RasterTile {
                width: tw,
                height: th,
                values: values.into(),
            })
        };
        let cmap_y = AxisEncode {
            lo: south_m,
            hi: north_m,
        };
        let cache_y = TilePixelCache::new(64);
        let out_y = render_metatiled(
            bbox,
            w,
            h,
            &prefix,
            &cmap_y,
            ImageFormat::Png,
            &cache_y,
            y_render,
        )
        .unwrap();
        let bytes_y = match out_y {
            MetaTile::Image { bytes, .. } => bytes,
            _ => panic!("expected image"),
        };
        let (_, _, rgba_y) = decode_rgba(&bytes_y);
        let mut max_diff_y = 0i32;
        for py in 2..h - 2 {
            let y_m = north_m - (py as f64 + 0.5) / h as f64 * (north_m - south_m);
            let expected = cmap_y.color(Some(y_m))[0] as i32;
            for px in 2..w - 2 {
                let got = rgba_y[((py * w + px) * 4) as usize] as i32;
                max_diff_y = max_diff_y.max((expected - got).abs());
            }
        }
        assert!(
            max_diff_y <= 3,
            "Y gradient mismatch: max diff {max_diff_y} (row shift / vertical flip?)"
        );
    }

    #[test]
    fn meta_and_direct_paths_agree_at_extreme_latitude() {
        // The cross-path regression guard for #452: a zoomed-out EPSG:3857 GetMap
        // whose bbox runs past the ±85° web-mercator limit must place data at the
        // same latitude via the meta-tile path as via the direct single-shot
        // render. The closure paints a horizontal data stripe at lat 55..70
        // (Mercator-spaced like a real tile); the meta-tile band must read at
        // ~55..70 AND match the direct band. The pre-fix clamp of the viewport
        // bounds to ±85° shrank the assembled vertical span, displacing the
        // meta-tile stripe ~10° north of where the direct render puts it.
        fn stripe_tile(b: [f64; 4], w: u32, h: u32) -> Result<RasterTile, DataServerError> {
            let (my_n, my_s) = (lat_to_y(b[3]), lat_to_y(b[1]));
            let mut values = vec![None; (w * h) as usize];
            for row in 0..h {
                let fy = (row as f64 + 0.5) / h as f64;
                let lat = y_to_lat(my_n - fy * (my_n - my_s));
                if (55.0..=70.0).contains(&lat) {
                    for col in 0..w {
                        values[(row * w + col) as usize] = Some(1.0);
                    }
                }
            }
            Ok(RasterTile {
                width: w,
                height: h,
                values: values.into(),
            })
        }
        let cache = TilePixelCache::new(64);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "d".into(),
            time: None,
            z: None,
            reference_time: None,
        };
        let bbox = [-150.0, -30.0, 150.0, 87.0]; // north well past the 85° limit
        let (w, h) = (400u32, 400u32);
        let bytes = match render_metatiled(
            bbox,
            w,
            h,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            stripe_tile,
        )
        .unwrap()
        {
            MetaTile::Image { bytes, .. } => bytes,
            other => panic!(
                "expected an image (got Empty/Fallback): {}",
                matches!(other, MetaTile::Fallback)
            ),
        };
        // Expand indexed+tRNS → RGBA and find the opaque-pixel latitude band, read
        // back the way the client does: over the FULL request bbox in Mercator.
        let mut dec = png::Decoder::new(std::io::Cursor::new(&bytes));
        dec.set_transformations(png::Transformations::EXPAND);
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        let ch = info.color_type.samples();
        let (my_n, my_s) = (lat_to_y(bbox[3]), lat_to_y(bbox[1]));
        let (mut lat_lo, mut lat_hi) = (f64::MAX, f64::MIN);
        for row in 0..h {
            let lat = y_to_lat(my_n - (row as f64 + 0.5) / h as f64 * (my_n - my_s));
            for col in 0..w {
                let a = if ch >= 4 {
                    buf[((row * w + col) as usize) * ch + 3]
                } else {
                    255
                };
                if a > 20 {
                    lat_lo = lat_lo.min(lat);
                    lat_hi = lat_hi.max(lat);
                }
            }
        }
        // Band must read at ~55..70, not displaced north. Outer bounds allow a
        // little tile-granularity spread; inner bounds prove the stripe is there.
        assert!(
            lat_hi <= 72.0 && lat_lo >= 53.0,
            "data band lat[{lat_lo:.1},{lat_hi:.1}] displaced from ~55..70"
        );
        assert!(
            lat_hi >= 68.0 && lat_lo <= 57.0,
            "data band lat[{lat_lo:.1},{lat_hi:.1}] missing or too narrow"
        );

        // Cross-path check: render the SAME bbox single-shot (the direct path a
        // raster engine / WMS-direct fallback takes) and confirm the meta-tile
        // path placed the stripe at the same latitudes. Before #452 the meta band
        // sat ~10° north of this direct band; they now share `web_mercator` and
        // must agree to within a tile-granularity slop.
        let direct = stripe_tile(bbox, w, h).unwrap();
        let (mut d_lo, mut d_hi) = (f64::MAX, f64::MIN);
        for row in 0..h {
            let lat = y_to_lat(my_n - (row as f64 + 0.5) / h as f64 * (my_n - my_s));
            // The stripe is full-width, so column 0 represents the whole row.
            if direct.values.value_at((row * w) as usize).is_some() {
                d_lo = d_lo.min(lat);
                d_hi = d_hi.max(lat);
            }
        }
        assert!(
            (lat_lo - d_lo).abs() < 1.5 && (lat_hi - d_hi).abs() < 1.5,
            "meta band lat[{lat_lo:.1},{lat_hi:.1}] disagrees with direct band \
             lat[{d_lo:.1},{d_hi:.1}]"
        );
    }
}
