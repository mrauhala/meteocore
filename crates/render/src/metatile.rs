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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use quick_cache::sync::Cache;

use ds_core::error::DataServerError;
use ds_core::map_engine::RasterTile;

use crate::colorize;
use crate::{ColorMap, ImageFormat};

/// Tile edge length in pixels (WebMercatorQuad standard).
const TILE_PX: u32 = 256;
/// EPSG:3857 sphere radius (metres).
const R: f64 = 6_378_137.0;
/// Half the Web Mercator world span in metres (`π·R`). The grid origin is the
/// top-left corner `(-ORIGIN, +ORIGIN)`.
const ORIGIN: f64 = std::f64::consts::PI * R;
/// Ground resolution at zoom 0 (metres/pixel): `2·ORIGIN / 256 ≈ 156543.034`.
const Z0_RES: f64 = (2.0 * ORIGIN) / TILE_PX as f64;
/// Maximum half-octave ladder level (`level/2 ≈ zoom`, so ~zoom 24).
const MAX_LEVEL: i32 = 48;
/// Web Mercator latitude limit in radians (~±85.0511°).
const LAT_LIMIT_RAD: f64 = 1.484_422_229_745_332_4;
/// Safety cap on covering tiles per request. A request that would need more
/// (tiny resolution over a huge bbox) declines to [`MetaTile::Fallback`].
const MAX_TILES: usize = 256;

// --- Web Mercator metre <-> WGS84 degree helpers (standard EPSG:3857) --------

fn lon_to_x(lon_deg: f64) -> f64 {
    R * lon_deg.to_radians()
}
fn lat_to_y(lat_deg: f64) -> f64 {
    let lat = lat_deg.to_radians().clamp(-LAT_LIMIT_RAD, LAT_LIMIT_RAD);
    R * (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln()
}
fn x_to_lon(x: f64) -> f64 {
    (x / R).to_degrees()
}
fn y_to_lat(y: f64) -> f64 {
    (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()
}

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
}

/// Cache key for one rendered+colorized meta-tile.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    layer: String,
    parameter: Option<String>,
    style: String,
    time: Option<DateTime<Utc>>,
    z: Option<i64>,
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

#[derive(Clone)]
struct TilePixelWeighter;

impl quick_cache::Weighter<TileKey, CachedTilePixels> for TilePixelWeighter {
    fn weight(&self, key: &TileKey, val: &CachedTilePixels) -> u64 {
        // Pixel bytes (zero for a nodata marker) + a flat allowance for the
        // owned key strings + node overhead.
        val.rgba.as_ref().map_or(0, |r| r.len()) as u64
            + key.layer.len() as u64
            + key.parameter.as_ref().map_or(0, String::len) as u64
            + key.style.len() as u64
            + 96
    }
}

/// LRU cache of decoded, colorized meta-tiles (RGBA), weighted by byte size.
///
/// Distinct from the per-collection GeoTIFF *compressed-byte* tile cache and
/// from the request-keyed [`crate::RenderedCache`]. This one is keyed on the
/// fixed tile grid so overlapping fullscreen viewports reuse the same entries.
pub struct TilePixelCache {
    cache: Cache<TileKey, CachedTilePixels, TilePixelWeighter>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl TilePixelCache {
    pub fn new(capacity_mb: u64) -> Self {
        let max_bytes = capacity_mb * 1024 * 1024;
        // ~256 KB per RGBA tile for initial map sizing.
        let estimated_items = if capacity_mb == 0 {
            0
        } else {
            (max_bytes / (256 * 1024)).max(1) as usize
        };
        Self {
            cache: Cache::with_weighter(estimated_items, max_bytes, TilePixelWeighter),
            capacity_bytes: max_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// (hits, misses) counters.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }
    pub fn capacity(&self) -> u64 {
        self.capacity_bytes
    }
    pub fn len(&self) -> usize {
        self.cache.len()
    }
    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
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
    /// Time assembling the mosaic into the output (premultiplied bilinear).
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
    /// Meta-tiling declined (degenerate bbox or > `MAX_TILES` tiles); the caller
    /// should fall back to a direct single-shot render.
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

    // Viewport in Web Mercator metres. Mercator Y increases northward.
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
    let col0 = ((west_m + ORIGIN) / span).floor() as i64;
    let col1 = ((east_m + ORIGIN - eps) / span).floor() as i64;
    let row0 = ((ORIGIN - north_m) / span).floor() as i64;
    let row1 = ((ORIGIN - south_m - eps) / span).floor() as i64;
    let ncols = (col1 - col0 + 1).max(1) as usize;
    let nrows = (row1 - row0 + 1).max(1) as usize;
    if ncols.saturating_mul(nrows) > MAX_TILES {
        return Ok(MetaTile::Fallback);
    }

    // Render or fetch each covering tile's RGBA pixels. `None` = an all-nodata
    // tile (cached as a marker, drawn transparent at assembly time).
    let mut tiles: HashMap<(i64, i64), Option<Arc<[u8]>>> = HashMap::with_capacity(ncols * nrows);
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
                level,
                col,
                row,
            };
            let rgba = if let Some(c) = cache.cache.get(&key) {
                cache.hits.fetch_add(1, Ordering::Relaxed);
                any_data |= c.rgba.is_some();
                c.rgba
            } else {
                cache.misses.fetch_add(1, Ordering::Relaxed);
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
            tiles.insert((col, row), rgba);
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
            let rgba = sample_bilinear(&tiles, gx, gy);
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

/// Fetch one global tile-pixel (nearest) from the covering set; transparent if
/// the pixel falls outside the rendered tiles (only possible ≤1px past an edge).
#[inline]
fn global_pixel(tiles: &HashMap<(i64, i64), Option<Arc<[u8]>>>, xi: i64, yi: i64) -> [u8; 4] {
    let col = xi.div_euclid(TILE_PX as i64);
    let row = yi.div_euclid(TILE_PX as i64);
    let lx = xi.rem_euclid(TILE_PX as i64) as usize;
    let ly = yi.rem_euclid(TILE_PX as i64) as usize;
    match tiles.get(&(col, row)) {
        // Present with data; nodata markers (`Some(None)`) and absent tiles
        // (≤1px past an edge) are transparent.
        Some(Some(rgba)) => {
            let o = (ly * TILE_PX as usize + lx) * 4;
            [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]]
        }
        _ => [0, 0, 0, 0],
    }
}

/// Premultiplied-alpha bilinear sample of the mosaic at global tile-pixel
/// coordinates `(gx, gy)` (pixel centres at integer + 0.5).
#[inline]
fn sample_bilinear(tiles: &HashMap<(i64, i64), Option<Arc<[u8]>>>, gx: f64, gy: f64) -> [u8; 4] {
    let fx = gx - 0.5;
    let fy = gy - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let dx = fx - x0;
    let dy = fy - y0;
    let (x0, y0) = (x0 as i64, y0 as i64);

    let c00 = premul(global_pixel(tiles, x0, y0));
    let c10 = premul(global_pixel(tiles, x0 + 1, y0));
    let c01 = premul(global_pixel(tiles, x0, y0 + 1));
    let c11 = premul(global_pixel(tiles, x0 + 1, y0 + 1));

    let mut acc = [0.0f64; 4];
    for i in 0..4 {
        let top = c00[i] * (1.0 - dx) + c10[i] * dx;
        let bot = c01[i] * (1.0 - dx) + c11[i] * dx;
        acc[i] = top * (1.0 - dy) + bot * dy;
    }
    unpremul(acc)
}

/// RGBA u8 → premultiplied f64 (RGB scaled by alpha; alpha kept in 0..255).
#[inline]
fn premul(c: [u8; 4]) -> [f64; 4] {
    let a = c[3] as f64 / 255.0;
    [
        c[0] as f64 * a,
        c[1] as f64 * a,
        c[2] as f64 * a,
        c[3] as f64,
    ]
}

/// Premultiplied f64 → straight RGBA u8.
#[inline]
fn unpremul(c: [f64; 4]) -> [u8; 4] {
    let a = c[3];
    if a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let inv = 255.0 / a;
    let r = (c[0] * inv).round().clamp(0.0, 255.0) as u8;
    let g = (c[1] * inv).round().clamp(0.0, 255.0) as u8;
    let b = (c[2] * inv).round().clamp(0.0, 255.0) as u8;
    [r, g, b, a.round().clamp(0.0, 255.0) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!((lat_to_y(85.0511287798066) - ORIGIN).abs() < 1.0);
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
            values: vec![Some(1.0); (w * h) as usize],
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
        };
        let empty_tile = |_b: [f64; 4], w: u32, h: u32| {
            Ok(RasterTile {
                width: w,
                height: h,
                values: vec![None; (w * h) as usize],
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
    fn huge_tile_count_falls_back() {
        let cache = TilePixelCache::new(16);
        let prefix = TileKeyPrefix {
            layer: "l".into(),
            parameter: None,
            style: "default".into(),
            time: None,
            z: None,
        };
        // Whole-world bbox at a tiny resolution forces > MAX_TILES → fallback.
        let out = render_metatiled(
            [-179.0, -85.0, 179.0, 85.0],
            8192,
            8192,
            &prefix,
            &SolidRed,
            ImageFormat::Png,
            &cache,
            solid_tile,
        )
        .unwrap();
        assert!(matches!(out, MetaTile::Fallback));
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
                let plte = info_meta.palette.as_deref().unwrap_or(&[]);
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
                values,
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
                values,
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
}
