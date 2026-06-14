pub mod colormap;
mod encode;
pub mod font;
pub mod metatile;
pub mod plot;

pub use colormap::{
    parse_hex_color, BuiltinColormap, ColorMap, ColorStop, IntegerLutColorMap, LinearColorMap,
    LutColorMap, OverlayColorMap,
};
pub use encode::{encode_jpeg, encode_png, encode_webp};
pub use metatile::{render_metatiled, MetaTile, MetaTileStats, TileKeyPrefix, TilePixelCache};
pub use plot::{render_chart, render_heatmap, Heatmap, Panel, Series};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Maximum time to wait for a render semaphore permit before returning 503.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(30);

use bytes::Bytes;
use chrono::{DateTime, Utc};
use quick_cache::sync::Cache;

use ds_core::error::DataServerError;
use ds_core::map_engine::RasterTile;

/// Output image format.
///
/// [`Png`] auto-selects an 8-bit indexed-palette encoding ("PNG8") when the
/// rendered image carries ≤256 distinct RGBA colours — typical of every
/// colormap-rendered layer (radar, classification, single-parameter rasters).
/// Bytes are roughly 3–4× smaller than the 32-bit RGBA path for the same
/// image; content-type is `image/png` either way. See [`encode_png`] for the
/// byte-level contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    pub fn content_type(&self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
        }
    }
}

// ---------------------------------------------------------------------------
// Shared types for rendered image caching (used by api-wms and api-maps)
// ---------------------------------------------------------------------------

/// A named style with its colormap and value range.
#[derive(Clone)]
pub struct StyleInfo {
    pub name: String,
    pub title: String,
    pub colormap: Arc<dyn ColorMap>,
    pub min: f64,
    pub max: f64,
    /// Data parameter to render. For multi-parameter engines, selects which
    /// parameter's data is returned by `get_raster_tile()`. None = engine default.
    pub parameter: Option<String>,
}

/// Cache key for rendered map images.
/// Bbox values are quantized to microdegrees (6 decimal places) for stable hashing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub layer: String,
    pub style: String,
    pub format: u8, // 0=png, 1=jpeg, 2=webp
    pub crs: String,
    pub bbox: [i64; 4], // bbox quantized to microdegrees (6 decimal places)
    pub width: u32,
    pub height: u32,
    pub time: Option<DateTime<Utc>>,
    /// Optional parameter name override (e.g. `parameter-name=temperature`).
    /// Distinct cache entries for the same (layer, style, bbox, time) when
    /// the caller asks for a different parameter than the style's default.
    pub parameter: Option<String>,
    /// Optional vertical level, quantized to millidegrees/milli-units so the
    /// key stays `Hash`/`Eq`. Use [`quantize_z`] to build it.
    pub z: Option<i64>,
    /// Optional forecast model run (reference time). Distinct cache entries for
    /// the same (layer, time, bbox, …) when a client pins a non-latest run via
    /// the WMS `reference_time` dimension. `None` = the engine's latest run.
    pub reference_time: Option<DateTime<Utc>>,
}

/// Quantize a vertical level for use in a [`CacheKey`] — millidegrees /
/// milli-units, enough to keep distinct elevation sweeps apart.
///
/// `z` must be finite; a non-finite value would saturate the `as i64`
/// cast to a meaningless key. Callers validate `z` at the API boundary.
pub fn quantize_z(z: f64) -> i64 {
    debug_assert!(z.is_finite(), "quantize_z requires a finite z value");
    (z * 1000.0).round() as i64
}

/// FNV-1a 64-bit mix. Fixed algorithm — safe to serialise into HTTP `ETag`
/// headers because the output does not change with a rustc upgrade. The
/// stdlib `DefaultHasher` is explicitly unspecified across releases and
/// must not leave the process.
const FNV1A_OFFSET: u64 = 0xcbf29ce484222325;
const FNV1A_PRIME: u64 = 0x100000001b3;

#[inline]
fn fnv1a_mix(state: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *state ^= b as u64;
        *state = state.wrapping_mul(FNV1A_PRIME);
    }
}

impl CacheKey {
    /// Compute an ETag from the cache key for HTTP caching.
    pub fn etag(&self) -> String {
        let mut h = FNV1A_OFFSET;
        fnv1a_mix(&mut h, self.layer.as_bytes());
        fnv1a_mix(&mut h, b"|");
        fnv1a_mix(&mut h, self.style.as_bytes());
        fnv1a_mix(&mut h, b"|");
        fnv1a_mix(&mut h, &[self.format]);
        fnv1a_mix(&mut h, self.crs.as_bytes());
        fnv1a_mix(&mut h, b"|");
        for v in &self.bbox {
            fnv1a_mix(&mut h, &v.to_le_bytes());
        }
        fnv1a_mix(&mut h, &self.width.to_le_bytes());
        fnv1a_mix(&mut h, &self.height.to_le_bytes());
        match self.time {
            Some(t) => {
                fnv1a_mix(&mut h, &[1u8]);
                // `timestamp_nanos_opt` returns None for dates outside [1677-2262];
                // for radar data we never come anywhere near those bounds, but the
                // fallback to `timestamp_millis()` keeps the ETag deterministic
                // even for pathological values rather than panicking.
                let ts = t
                    .timestamp_nanos_opt()
                    .unwrap_or_else(|| t.timestamp_millis().saturating_mul(1_000_000));
                fnv1a_mix(&mut h, &ts.to_le_bytes());
            }
            None => fnv1a_mix(&mut h, &[0u8]),
        }
        match &self.parameter {
            Some(p) => {
                fnv1a_mix(&mut h, &[1u8]);
                fnv1a_mix(&mut h, p.as_bytes());
            }
            None => fnv1a_mix(&mut h, &[0u8]),
        }
        match self.z {
            Some(z) => {
                fnv1a_mix(&mut h, &[1u8]);
                fnv1a_mix(&mut h, &z.to_le_bytes());
            }
            None => fnv1a_mix(&mut h, &[0u8]),
        }
        match self.reference_time {
            Some(rt) => {
                fnv1a_mix(&mut h, &[1u8]);
                // Mirror the `time` mixing: nanos when in range, else a
                // saturating millis fallback so the ETag stays deterministic
                // for pathological dates rather than panicking.
                let ts = rt
                    .timestamp_nanos_opt()
                    .unwrap_or_else(|| rt.timestamp_millis().saturating_mul(1_000_000));
                fnv1a_mix(&mut h, &ts.to_le_bytes());
            }
            None => fnv1a_mix(&mut h, &[0u8]),
        }
        format!("\"{h:016x}\"")
    }
}

/// Check whether an `If-None-Match` header value matches a given ETag.
///
/// Handles comma-separated lists, the `*` wildcard, and the `W/` weak prefix
/// per RFC 7232 §3.2 (weak comparison).
pub fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let etag_bare = etag
        .trim()
        .strip_prefix("W/")
        .unwrap_or(etag.trim())
        .trim_matches('"');

    if_none_match.split(',').any(|tag| {
        let tag = tag.trim();
        if tag == "*" {
            return true;
        }
        let tag_bare = tag.strip_prefix("W/").unwrap_or(tag).trim_matches('"');
        tag_bare == etag_bare
    })
}

/// Quantize a floating-point bbox to integer microdegrees for cache key stability.
pub fn quantize_bbox(bbox: &[f64; 4]) -> [i64; 4] {
    [
        (bbox[0] * 1_000_000.0).round() as i64,
        (bbox[1] * 1_000_000.0).round() as i64,
        (bbox[2] * 1_000_000.0).round() as i64,
        (bbox[3] * 1_000_000.0).round() as i64,
    ]
}

/// Cached rendered image payload plus its content-derived ETag.
///
/// The ETag hashes the encoded bytes themselves via FNV-1a so two cache
/// entries under the same `CacheKey` with different content (e.g. data
/// refresh, encoder change, colormap fix) produce different ETags. A
/// stable key-derived ETag would let stale browser caches survive a
/// server-side fix indefinitely, since `If-None-Match` would keep
/// returning 304 against fresh content (the bug #145 fixed for raster
/// tiles, mirroring #136's fix for vector tiles).
/// Both fields are private to seal the invariant `etag == FNV-1a(bytes)`.
/// `bytes` was previously `pub`, which let a caller mutate the payload
/// without updating the ETag and silently break `If-None-Match`
/// (a browser holding the real ETag would get a full 200 instead of 304,
/// or vice-versa).
#[derive(Debug, Clone)]
pub struct CachedRendered {
    bytes: Bytes,
    etag: String,
}

impl CachedRendered {
    /// Build a cache entry from encoded bytes, deriving the ETag from
    /// the content via FNV-1a. Format matches `ds_mvt::CachedTile::etag`
    /// and `CacheKey::etag` so the same `etag_matches` helper works
    /// across raster and vector responses. FNV-1a (not the stdlib
    /// `DefaultHasher`) so the ETag is stable across rustc versions —
    /// a binary rebuild against unchanged content keeps the same ETag
    /// and browser caches survive the redeploy.
    pub fn new(bytes: Bytes) -> Self {
        let mut h = FNV1A_OFFSET;
        fnv1a_mix(&mut h, bytes.as_ref());
        let etag = format!("\"{h:016x}\"");
        Self { bytes, etag }
    }

    /// Quoted 16-hex-char (64-bit FNV-1a) ETag string suitable for the
    /// `ETag` response header.
    pub fn etag(&self) -> &str {
        &self.etag
    }

    /// Borrow the cached bytes — e.g. for byte-length accounting in the
    /// cache weighter.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Consume the entry and yield the bytes — used by the handlers when
    /// building the HTTP response body. Move semantics keep the invariant
    /// intact: there is no way to retain the `CachedRendered` while
    /// swapping out its bytes.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

/// Max distinct `(width, height)` transparent tiles retained (see
/// [`empty_tile`]). 64 covers any realistic mix of client viewport sizes;
/// beyond that the LRU recycles.
const EMPTY_TILE_CACHE_ITEMS: usize = 64;

/// Process-global cache of transparent (all-nodata) PNGs keyed by output
/// dimensions, as ready-to-serve [`CachedRendered`] entries (bytes + their
/// stable FNV-1a ETag).
///
/// WMS / Maps `GetMap` take an all-nodata fast path on off-coverage viewports;
/// the encoded transparent PNG and its ETag are identical for a given
/// `(width, height)`, so encoding once per distinct size and cloning the
/// `Arc`-backed bytes is far cheaper than re-allocating `width·height·4` zero
/// bytes and re-encoding + re-hashing on every empty response (#171). The Tiles
/// service (always 256×256) sources its single global from here too, so all
/// three raster APIs share one mechanism. Bounded by item count so a
/// `?WIDTH=…&HEIGHT=…` fan-out can't pin unbounded memory — an abuser just
/// cycles the LRU; a real client uses a handful of viewport sizes.
static EMPTY_TILE_CACHE: std::sync::LazyLock<Cache<(u32, u32), CachedRendered>> =
    std::sync::LazyLock::new(|| Cache::new(EMPTY_TILE_CACHE_ITEMS));

/// A fully-transparent `width`×`height` PNG as a ready-to-serve
/// [`CachedRendered`], encoded once per distinct size and cloned thereafter
/// (#171). Use this for the all-nodata fast path instead of allocating and
/// encoding a fresh transparent image on every empty response.
pub fn empty_tile(width: u32, height: u32) -> Result<CachedRendered, DataServerError> {
    EMPTY_TILE_CACHE.get_or_insert_with(&(width, height), || {
        let rgba = vec![0u8; width as usize * height as usize * 4];
        Ok(CachedRendered::new(Bytes::from(encode_png(
            &rgba, width, height,
        )?)))
    })
}

/// Weight function: count the byte size of each cached rendered image.
#[derive(Clone)]
struct RenderedWeighter;

impl quick_cache::Weighter<CacheKey, CachedRendered> for RenderedWeighter {
    fn weight(&self, _key: &CacheKey, val: &CachedRendered) -> u64 {
        // bytes + 18-byte quoted 16-hex-char (64-bit FNV-1a) ETag string + 64-byte overhead
        val.bytes().len() as u64 + val.etag().len() as u64 + 64
    }
}

/// Cache for rendered map images (Tier 2), weighted by byte size.
/// Keys are quantized to improve hit rates for tiled clients.
/// No TTL — radar measurements are immutable once produced.
/// Cache is invalidated on collection reload.
pub struct RenderedCache {
    cache: Cache<CacheKey, CachedRendered, RenderedWeighter>,
    capacity_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl RenderedCache {
    pub fn new(capacity_mb: u64) -> Self {
        let max_bytes = capacity_mb * 1024 * 1024;
        // Estimate ~60 KB per tile for initial hash map sizing
        let estimated_items = if capacity_mb == 0 {
            0
        } else {
            ((capacity_mb * 1024 * 1024) / (60 * 1024)).max(1) as usize
        };
        Self {
            cache: Cache::with_weighter(estimated_items, max_bytes, RenderedWeighter),
            capacity_bytes: max_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<CachedRendered> {
        let result = self.cache.get(key);
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    pub fn insert(&self, key: CacheKey, value: CachedRendered) {
        self.cache.insert(key, value);
    }

    /// Return (hits, misses) counters.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Current weight (bytes used) of the cache.
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }

    /// Maximum weight (bytes) the cache will hold.
    pub fn capacity(&self) -> u64 {
        self.capacity_bytes
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is currently empty.
    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
    }
}

/// Render a raster tile to image bytes using the given colormap and format.
///
/// If the tile is entirely nodata (all values are `None`), skips colorization
/// and produces a fully transparent image directly. This is significantly faster
/// for large tiles since it avoids iterating every pixel through the colormap.
pub fn render_tile(
    tile: &RasterTile,
    colormap: &dyn ColorMap,
    format: ImageFormat,
) -> Result<Vec<u8>, DataServerError> {
    // Short-circuit for empty tiles: skip colorization, produce transparent RGBA directly.
    let rgba = if tile.is_empty() {
        vec![0u8; (tile.width * tile.height * 4) as usize]
    } else {
        colorize(tile, colormap)
    };
    match format {
        ImageFormat::Png => encode::encode_png(&rgba, tile.width, tile.height),
        ImageFormat::Jpeg => encode::encode_jpeg(&rgba, tile.width, tile.height),
        ImageFormat::Webp => encode::encode_webp(&rgba, tile.width, tile.height),
    }
}

/// Render a raster tile to PNG bytes using the given colormap.
pub fn render_tile_png(
    tile: &RasterTile,
    colormap: &dyn ColorMap,
) -> Result<Vec<u8>, DataServerError> {
    render_tile(tile, colormap, ImageFormat::Png)
}

/// Compute "nice" round tick values spanning `[min, max]` aiming for roughly
/// `target` ticks, using the classic 1-2-5 nice-number algorithm (Heckbert,
/// *Graphics Gems*). Returns values inside `[min, max]` (inclusive, with a small
/// tolerance) in ascending order, so a legend's labels land on human-friendly
/// round numbers (0, 10, 20, …) rather than the raw extents.
///
/// Degenerate inputs (`min == max`, non-finite bounds) yield a single tick at
/// `min`. `min`/`max` may be passed in either order.
fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() || min == max {
        return vec![min];
    }
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };

    // Round a number to a "nice" 1-2-5 × 10^k value.
    fn nice_num(range: f64, round: bool) -> f64 {
        let exp = range.log10().floor();
        let frac = range / 10f64.powf(exp);
        let nice = if round {
            if frac < 1.5 {
                1.0
            } else if frac < 3.0 {
                2.0
            } else if frac < 7.0 {
                5.0
            } else {
                10.0
            }
        } else if frac <= 1.0 {
            1.0
        } else if frac <= 2.0 {
            2.0
        } else if frac <= 5.0 {
            5.0
        } else {
            10.0
        };
        nice * 10f64.powf(exp)
    }

    let target = target.max(2);
    // Step straight from the raw range (not a nice-rounded range): rounding the
    // range up first tends to overshoot the step and drop the extreme ticks
    // (0..70 → step 20 → no 70). From the raw range, 0..70/5 → step 10 → 0,10,…,70.
    let step = nice_num((hi - lo) / (target - 1) as f64, true);
    if !step.is_finite() || step <= 0.0 {
        return vec![lo, hi];
    }
    let graph_lo = (lo / step).floor() * step;
    let graph_hi = (hi / step).ceil() * step;

    let mut ticks = Vec::new();
    let tol = step * 1e-6;
    // Bound the loop defensively so a pathological step can't spin forever.
    let max_ticks = target * 4 + 4;
    let mut v = graph_lo;
    while v <= graph_hi + tol && ticks.len() < max_ticks {
        if v >= lo - tol && v <= hi + tol {
            ticks.push(v);
        }
        v += step;
    }
    if ticks.is_empty() {
        ticks.push(lo);
        ticks.push(hi);
    }
    ticks
}

/// Format a tick value for a legend label: fixed enough decimals to be exact for
/// the nice-number ticks [`nice_ticks`] produces, with trailing zeros and a
/// dangling decimal point stripped (`10.000` → `10`, `0.200` → `0.2`). Negative
/// zero normalises to `0`.
fn format_tick(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string(); // also catches -0.0
    }
    let s = format!("{v:.3}");
    let trimmed = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    if trimmed == "-0" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Render a legend image showing the colormap scale.
///
/// Produces a vertical colour gradient (top = `max`, bottom = `min`) with a
/// bordered swatch, a nice-number tick scale with numeric value labels, and an
/// optional `title` line (e.g. `"reflectivity (dBZ)"`). Labels are drawn with
/// the embedded [`font`] so `ds-render` stays framework-free.
///
/// The layout adapts to the requested size: the title and the tick labels are
/// each drawn only when there is room, so the function degrades gracefully to a
/// bare gradient for very small `width`/`height` (e.g. a 40×200 thumbnail) while
/// producing a fully labelled legend at the WMS default size.
pub fn render_legend(
    colormap: &dyn ColorMap,
    min: f64,
    max: f64,
    width: u32,
    height: u32,
    format: ImageFormat,
    title: Option<&str>,
) -> Result<Vec<u8>, DataServerError> {
    // Normalise so the gradient always runs large-at-top → small-at-bottom even
    // if a caller passes the bounds reversed.
    let (min, max) = if min <= max { (min, max) } else { (max, min) };
    let span = max - min;

    const PAD: u32 = 4;
    const TEXT: [u8; 4] = [0, 0, 0, 255]; // opaque black labels
    const BORDER: [u8; 4] = [80, 80, 80, 255]; // grey swatch border
    const SCALE: u32 = 1;
    const TICK_LEN: u32 = 4; // tick mark length past the swatch
    const LABEL_GAP: u32 = 3; // gap between tick mark and its label

    let mut rgba = vec![255u8; (width as usize * height as usize) * 4]; // white background

    // Reserve a title row at the top when a title is given and there's vertical
    // room for it plus a usable gradient below.
    let title_h = match title {
        Some(t) if !t.is_empty() && height >= PAD * 2 + font::GLYPH_H * SCALE + 24 => {
            font::draw_text(
                &mut rgba, width, height, PAD as i32, PAD as i32, t, TEXT, SCALE,
            );
            font::GLYPH_H * SCALE + PAD // text + a small gap below it
        }
        _ => 0,
    };

    // Will we have horizontal room for tick labels? Need: swatch + tick mark +
    // gap + a 2-digit label (~"00") at minimum.
    let min_label_w = font::text_width("00", SCALE);
    let want_labels = width >= PAD * 2 + 12 + TICK_LEN + LABEL_GAP + min_label_w;

    // Swatch width: a slim bar when we're labelling (the labels carry the
    // information), or the historical ~40% when there's no room for labels.
    let swatch_w = if want_labels {
        ((width / 6).clamp(14, 30)).min(width.saturating_sub(2 * PAD).max(1))
    } else {
        (width * 2 / 5).max(10).min(width)
    };

    let grad_x0 = PAD;
    let grad_y0 = PAD + title_h;
    let grad_y1 = height.saturating_sub(PAD).max(grad_y0 + 1);
    let grad_h = grad_y1 - grad_y0;

    // Map a value to its pixel row in the gradient (top = max, bottom = min).
    let value_to_y = |value: f64| -> u32 {
        let frac = if span > 0.0 {
            ((max - value) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        grad_y0 + (frac * (grad_h.saturating_sub(1)) as f64).round() as u32
    };

    // Paint the gradient swatch. Colours are composited over white so a colormap
    // with transparent low values (e.g. radar dBZ below the floor) reads as a
    // solid faded swatch instead of a see-through strip.
    for gy in 0..grad_h {
        let frac = gy as f64 / (grad_h.saturating_sub(1).max(1)) as f64;
        let value = max - frac * span;
        let c = colormap.color(Some(value));
        let a = c[3] as u32;
        let over_white = |ch: u8| -> u8 { ((ch as u32 * a + 255 * (255 - a)) / 255) as u8 };
        let px = [over_white(c[0]), over_white(c[1]), over_white(c[2]), 255];
        let py = grad_y0 + gy;
        for gx in 0..swatch_w {
            let idx = ((py * width + (grad_x0 + gx)) * 4) as usize;
            rgba[idx..idx + 4].copy_from_slice(&px);
        }
    }

    // 1px border around the swatch so it stands out against the white canvas.
    draw_rect_border(
        &mut rgba, width, height, grad_x0, grad_y0, swatch_w, grad_h, BORDER,
    );

    // Tick marks + numeric labels in the reserved right-hand area.
    if want_labels {
        let tick_x0 = grad_x0 + swatch_w;
        let label_x = tick_x0 + TICK_LEN + LABEL_GAP;
        let half_text = (font::GLYPH_H * SCALE) as i32 / 2;
        for v in nice_ticks(min, max, 6) {
            let py = value_to_y(v);
            // Short tick mark butting up against the swatch edge.
            for tx in 0..TICK_LEN {
                let idx = ((py * width + (tick_x0 + tx)) * 4) as usize;
                rgba[idx..idx + 4].copy_from_slice(&TEXT);
            }
            // Label, vertically centred on the tick row.
            font::draw_text(
                &mut rgba,
                width,
                height,
                label_x as i32,
                py as i32 - half_text,
                &format_tick(v),
                TEXT,
                SCALE,
            );
        }
    }

    match format {
        ImageFormat::Png => encode::encode_png(&rgba, width, height),
        ImageFormat::Jpeg => encode::encode_jpeg(&rgba, width, height),
        ImageFormat::Webp => encode::encode_webp(&rgba, width, height),
    }
}

/// Draw a 1px rectangle outline into an RGBA buffer. `(x, y)` is the top-left
/// corner, `w`×`h` the size. Clipped to the buffer bounds.
#[allow(clippy::too_many_arguments)]
fn draw_rect_border(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: [u8; 4],
) {
    if w == 0 || h == 0 {
        return;
    }
    let mut put = |px: u32, py: u32| {
        if px < width && py < height {
            let idx = ((py * width + px) * 4) as usize;
            rgba[idx..idx + 4].copy_from_slice(&color);
        }
    };
    let x1 = x + w - 1;
    let y1 = y + h - 1;
    for px in x..=x1 {
        put(px, y);
        put(px, y1);
    }
    for py in y..=y1 {
        put(x, py);
        put(x1, py);
    }
}

/// Colorize a raster tile into an RGBA buffer using a colormap.
pub(crate) fn colorize(tile: &RasterTile, colormap: &dyn ColorMap) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((tile.width * tile.height * 4) as usize);
    for value in &tile.values {
        let color = colormap.color(*value);
        rgba.extend_from_slice(&color);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::map_engine::RasterTile;

    #[test]
    fn empty_tile_is_deterministic_and_memoized() {
        let a = empty_tile(64, 48).expect("encode empty tile");
        let b = empty_tile(64, 48).expect("encode empty tile");
        // Same dimensions → identical bytes + ETag (the second call is a cache
        // hit), so browsers can revalidate empty responses across requests.
        assert_eq!(a.etag(), b.etag(), "same (w,h) must yield a stable ETag");
        assert_eq!(
            a.bytes(),
            b.bytes(),
            "same (w,h) must yield identical bytes"
        );
        // Different dimensions → different content (and a different ETag).
        let c = empty_tile(100, 100).expect("encode empty tile");
        assert_ne!(a.etag(), c.etag(), "different (w,h) must differ");
        assert!(!a.bytes().is_empty(), "an empty tile is still a real PNG");
    }

    #[test]
    fn test_render_tile_png_produces_valid_png() {
        let tile = RasterTile {
            width: 4,
            height: 4,
            values: (0..16).map(|i| Some(i as f64 / 15.0)).collect(),
        };
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 1.0);
        let png_bytes = render_tile_png(&tile, &cmap).unwrap();
        assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn test_render_tile_jpeg() {
        let tile = RasterTile {
            width: 4,
            height: 4,
            values: (0..16).map(|i| Some(i as f64 / 15.0)).collect(),
        };
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 1.0);
        let jpeg_bytes = render_tile(&tile, &cmap, ImageFormat::Jpeg).unwrap();
        assert!(jpeg_bytes[0] == 0xFF && jpeg_bytes[1] == 0xD8);
    }

    #[test]
    fn test_nodata_produces_transparent_pixel() {
        let tile = RasterTile {
            width: 1,
            height: 1,
            values: vec![None],
        };
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 1.0);
        let rgba = colorize(&tile, &cmap);
        assert_eq!(rgba, vec![0, 0, 0, 0]); // fully transparent
    }

    #[test]
    fn test_empty_tile_skips_colorize() {
        let tile = RasterTile {
            width: 4,
            height: 4,
            values: vec![None; 16],
        };
        assert!(tile.is_empty());
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 1.0);
        let png_bytes = render_tile(&tile, &cmap, ImageFormat::Png).unwrap();
        // Should produce a valid PNG (the short-circuit path still encodes)
        assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn test_non_empty_tile_is_not_empty() {
        let tile = RasterTile {
            width: 2,
            height: 2,
            values: vec![None, Some(1.0), None, None],
        };
        assert!(!tile.is_empty());
    }

    #[test]
    fn test_render_legend_png() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Viridis, 0.0, 1.0);
        let legend = render_legend(&cmap, 0.0, 1.0, 40, 200, ImageFormat::Png, None).unwrap();
        assert!(legend.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn nice_ticks_lands_on_round_values() {
        // A 0..70 dBZ-style range with ~6 ticks → 0,10,…,70.
        let ticks = nice_ticks(0.0, 70.0, 6);
        assert_eq!(ticks, vec![0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0]);
        // All ticks stay within the requested range.
        for t in nice_ticks(-32.0, 95.0, 6) {
            assert!((-32.0..=95.0).contains(&t));
        }
        // A sub-unit range still produces sane round ticks.
        let small = nice_ticks(0.0, 1.0, 6);
        assert!(small.contains(&0.0) && small.contains(&1.0));
        assert!(small.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn nice_ticks_handles_degenerate_ranges() {
        // Zero-width range → a single tick at the value.
        assert_eq!(nice_ticks(5.0, 5.0, 6), vec![5.0]);
        // Non-finite bounds → a single (non-finite) tick, never a panic/hang.
        let nan_ticks = nice_ticks(f64::NAN, 1.0, 6);
        assert_eq!(nan_ticks.len(), 1);
        assert!(nan_ticks[0].is_nan());
        // Reversed bounds are normalised, not dropped.
        assert_eq!(nice_ticks(70.0, 0.0, 6), nice_ticks(0.0, 70.0, 6));
    }

    #[test]
    fn format_tick_strips_trailing_zeros() {
        assert_eq!(format_tick(10.0), "10");
        assert_eq!(format_tick(0.0), "0");
        assert_eq!(format_tick(-0.0), "0");
        assert_eq!(format_tick(-32.0), "-32");
        assert_eq!(format_tick(0.2), "0.2");
        assert_eq!(format_tick(0.25), "0.25");
        assert_eq!(format_tick(1.5), "1.5");
    }

    #[test]
    fn render_legend_with_title_is_larger_and_deterministic() {
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Viridis, 0.0, 1.0);
        // A labelled legend at the WMS default size encodes to a valid PNG…
        let a = render_legend(
            &cmap,
            0.0,
            70.0,
            180,
            300,
            ImageFormat::Png,
            Some("reflectivity (dBZ)"),
        )
        .unwrap();
        assert!(a.starts_with(&[0x89, b'P', b'N', b'G']));
        // …and is byte-for-byte deterministic across calls.
        let b = render_legend(
            &cmap,
            0.0,
            70.0,
            180,
            300,
            ImageFormat::Png,
            Some("reflectivity (dBZ)"),
        )
        .unwrap();
        assert_eq!(a, b, "legend rendering must be deterministic");
        // Different title → different bytes (the title is actually drawn).
        let c = render_legend(&cmap, 0.0, 70.0, 180, 300, ImageFormat::Png, Some("other")).unwrap();
        assert_ne!(a, c, "the title must affect the rendered output");
        // Tick labels are drawn: a titled legend differs from one rendered into
        // a swatch-only size where labels don't fit.
        let bare = render_legend(&cmap, 0.0, 70.0, 16, 200, ImageFormat::Png, None).unwrap();
        assert_ne!(a, bare);
    }

    #[test]
    fn test_etag_matches_exact() {
        assert!(etag_matches("\"abc123\"", "\"abc123\""));
    }

    #[test]
    fn test_etag_matches_no_match() {
        assert!(!etag_matches("\"abc123\"", "\"def456\""));
    }

    #[test]
    fn test_etag_matches_multiple() {
        assert!(etag_matches("\"aaa\", \"bbb\", \"ccc\"", "\"bbb\""));
    }

    #[test]
    fn test_etag_matches_multiple_no_match() {
        assert!(!etag_matches("\"aaa\", \"bbb\"", "\"ccc\""));
    }

    #[test]
    fn test_etag_matches_wildcard() {
        assert!(etag_matches("*", "\"anything\""));
    }

    #[test]
    fn test_etag_matches_weak_prefix() {
        assert!(etag_matches("W/\"abc123\"", "\"abc123\""));
        assert!(etag_matches("\"abc123\"", "W/\"abc123\""));
        assert!(etag_matches("W/\"abc123\"", "W/\"abc123\""));
    }

    #[test]
    fn test_etag_matches_weak_in_list() {
        assert!(etag_matches("\"aaa\", W/\"bbb\"", "\"bbb\""));
    }

    #[test]
    fn cache_key_etag_changes_when_time_bumps() {
        let base = CacheKey {
            layer: "radar".into(),
            style: "default".into(),
            format: 0,
            crs: "EPSG:3857".into(),
            bbox: [0, 0, 1, 1],
            width: 256,
            height: 256,
            time: Some(
                chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
            parameter: None,
            z: None,
            reference_time: None,
        };
        let mut later = base.clone();
        later.time = Some(
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        );
        assert_ne!(base.etag(), later.etag());
    }

    #[test]
    fn cache_key_etag_changes_when_parameter_changes() {
        let base = CacheKey {
            layer: "ecmwf-fc".into(),
            style: "default".into(),
            format: 0,
            crs: "EPSG:3857".into(),
            bbox: [0, 0, 1, 1],
            width: 256,
            height: 256,
            time: None,
            parameter: Some("2t".into()),
            z: None,
            reference_time: None,
        };
        let mut other = base.clone();
        other.parameter = Some("10u".into());
        assert_ne!(base.etag(), other.etag());

        let mut absent = base.clone();
        absent.parameter = None;
        assert_ne!(base.etag(), absent.etag());
    }

    #[test]
    fn cache_key_etag_changes_when_reference_time_changes() {
        // Two requests identical except for the forecast run (WMS
        // `reference_time` dimension) must not collide in the rendered cache —
        // different runs produce different pixels under the same valid TIME.
        let base = CacheKey {
            layer: "ecmwf-fc".into(),
            style: "default".into(),
            format: 0,
            crs: "EPSG:3857".into(),
            bbox: [0, 0, 1, 1],
            width: 256,
            height: 256,
            time: None,
            parameter: None,
            z: None,
            reference_time: Some(
                chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
        };
        let mut other = base.clone();
        other.reference_time = Some(
            chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        );
        assert_ne!(base.etag(), other.etag());

        let mut absent = base.clone();
        absent.reference_time = None;
        assert_ne!(base.etag(), absent.etag());
    }

    #[test]
    fn cache_key_etag_uses_stable_fnv1a_not_default_hasher() {
        // Pinned golden value — see the matching `VectorTileKey` test for
        // the rationale. If this changes you've either intentionally rotated
        // the ETag algorithm (cache-bust event) or accidentally regressed to
        // `DefaultHasher`, which mutates across rustc versions.
        let key = CacheKey {
            layer: "radar".into(),
            style: "default".into(),
            format: 0,
            crs: "EPSG:3857".into(),
            bbox: [0, 0, 0, 0],
            width: 256,
            height: 256,
            time: None,
            parameter: None,
            z: None,
            reference_time: None,
        };
        // Pinned after introducing the `reference_time` field (cache-bust event).
        assert_eq!(key.etag(), "\"92a1d2349689898e\"");
    }

    #[test]
    fn cached_rendered_etag_is_stable_for_same_bytes() {
        // Same bytes -> same ETag. The ETag is content-derived so a binary
        // rebuild against unchanged content keeps the same ETag and browser
        // caches survive the redeploy.
        let a = CachedRendered::new(Bytes::from_static(b"\x89PNG\r\n\x1a\n... pixels ..."));
        let b = CachedRendered::new(Bytes::from_static(b"\x89PNG\r\n\x1a\n... pixels ..."));
        assert_eq!(a.etag(), b.etag());
    }

    #[test]
    fn cached_rendered_etag_differs_for_distinct_bytes() {
        // This is the #145 regression check: two encodings under the same
        // CacheKey (e.g. a server-side colormap fix that produces different
        // PNG pixels) must produce different ETags so a browser holding the
        // pre-fix entry refetches instead of getting an infinite 304.
        let pre_fix = CachedRendered::new(Bytes::from_static(b"old-broken-png-bytes"));
        let post_fix = CachedRendered::new(Bytes::from_static(b"new-correct-png-bytes"));
        assert_ne!(pre_fix.etag(), post_fix.etag());
    }

    #[test]
    fn cached_rendered_etag_uses_stable_fnv1a_not_default_hasher() {
        // Pinned golden value — mirrors `cache_key_etag_uses_stable_fnv1a_not_default_hasher`
        // and `ds_mvt::CachedTile`'s analogue. If this changes you've either
        // intentionally rotated the algorithm (cache-bust event for browser
        // ETag caches) or accidentally regressed to a non-stable hasher.
        let cached = CachedRendered::new(Bytes::from_static(b"hello"));
        assert_eq!(cached.etag(), "\"a430d84680aabd0b\"");
    }
}
