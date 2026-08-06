pub mod colormap;
mod encode;
/// 5×7 bitmap font for in-image text (legend labels). Crate-internal — an
/// implementation detail of the legend renderer, not part of the public API.
pub(crate) mod font;
pub mod metatile;
pub mod palette;
pub mod palette_formats;
pub mod plot;
pub mod rasterize;
pub mod style;

pub use colormap::{
    parse_hex_color, BuiltinColormap, ColorMap, ColorStop, IntegerLutColorMap, LinearColorMap,
    LutColorMap, OverlayColorMap,
};
pub use encode::{encode_jpeg, encode_png, encode_webp};
pub use metatile::{render_metatiled, MetaTile, MetaTileStats, TileKeyPrefix, TilePixelCache};
pub use palette::{
    builtin_palette, builtin_palette_arc, builtin_palettes, Interpolation, Palette, PaletteInsert,
    PaletteRegistry,
};
pub use palette_formats::{parse_cpt, parse_gdal_txt};
pub use plot::{render_chart, render_heatmap, Heatmap, Panel, Series};
pub use rasterize::{fill_polygon, Combine};
pub use style::{ResolvedColormap, StyleContext, StyleSpec};

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
    /// The palette the colormap was built from — stops, interpolation mode
    /// and title, kept for machine-readable legends. `colormap` is the
    /// render-ready (possibly integer-LUT-wrapped, possibly overlay-wrapped)
    /// form of this palette sampled over `min..max`.
    pub palette: Arc<Palette>,
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

/// Weight function: count the byte size of each cached rendered image
/// (bytes + 18-byte quoted 16-hex-char ETag string + 64-byte overhead).
fn weigh_rendered(_key: &CacheKey, val: &CachedRendered) -> u64 {
    val.bytes().len() as u64 + val.etag().len() as u64 + 64
}

/// Cache for rendered map images (Tier 2), weighted by byte size.
/// Keys are quantized to improve hit rates for tiled clients.
/// No TTL — radar measurements are immutable once produced.
/// Cache is invalidated on collection reload.
pub struct RenderedCache {
    cache: ds_cache::ByteBoundedCache<CacheKey, CachedRendered>,
}

impl RenderedCache {
    pub fn new(capacity_mb: u64) -> Self {
        // Estimate ~60 KB per tile for initial hash map sizing.
        Self {
            cache: ds_cache::ByteBoundedCache::new(
                capacity_mb.saturating_mul(ds_cache::MIB),
                60 * 1024,
                weigh_rendered,
            ),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<CachedRendered> {
        self.cache.get(key)
    }

    pub fn insert(&self, key: CacheKey, value: CachedRendered) {
        self.cache.insert(key, value);
    }

    /// Return (hits, misses) counters.
    pub fn stats(&self) -> (u64, u64) {
        self.cache.stats()
    }

    /// Current weight (bytes used) of the cache.
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }

    /// Maximum weight (bytes) the cache will hold.
    pub fn capacity(&self) -> u64 {
        self.cache.capacity_bytes()
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is currently empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Snapshot for `/metrics`.
    pub fn metrics(&self) -> ds_cache::CacheMetrics {
        self.cache.metrics()
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

/// "Nice" round tick values to label a legend gradient over `[min, max]`.
///
/// Reuses the chart axis tick generator ([`plot::nice_ticks`], the shared 1-2-5
/// nice-number algorithm) and clips to `[min, max]` — a colorbar tick must map
/// onto the gradient, so unlike a chart axis it can't extend a half-step past
/// the data extent. Aims for ~6 ticks; an empty result (degenerate / inverted
/// range) means the caller draws no ticks.
fn legend_ticks(min: f64, max: f64) -> Vec<f64> {
    plot::nice_ticks(min, max, 6)
        .into_iter()
        .filter(|&v| v >= min && v <= max)
        .collect()
}

/// Format a tick value for a legend label with the *fewest* decimals (0–6) that
/// reproduce it, so round numbers stay clean (`10`, `0.2`) while a small-range
/// gradient keeps its digits (a `[0, 0.001]` scale labels `0.0002`, not `0`).
/// A fixed decimal count can't do both — too few collapses tiny ticks to `0`,
/// too many leaves trailing zeros on round ones. Values too small to show at
/// 6 dp fall back to scientific notation (`2e-8`) so sub-µ ranges stay legible.
fn format_tick(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string(); // also catches -0.0
    }
    // Smallest precision whose round-trip is within a relative epsilon of `v`.
    // The ticks come from `(t/step).round()*step` so they carry float noise
    // (0.30000000000000004); the tolerance snaps that back to "0.3".
    let mut chosen = format!("{v:.6}");
    for prec in 0..=6 {
        let s = format!("{v:.prec$}");
        if s.parse::<f64>()
            .is_ok_and(|p| (p - v).abs() <= v.abs() * 1e-9)
        {
            chosen = s;
            break;
        }
    }
    // A value too small to show at 6 dp prints as all zeros — fall back to
    // scientific notation ("2e-8") so a sub-µ range's ticks stay distinct
    // instead of all collapsing to "0" (the font renders `e`/`-`/digits).
    if chosen
        .trim_start_matches('-')
        .bytes()
        .all(|b| b == b'0' || b == b'.')
    {
        format!("{v:e}")
    } else {
        chosen
    }
}

/// Render a legend image showing the colormap scale.
///
/// Produces a vertical colour gradient (top = `max`, bottom = `min`) with a
/// bordered swatch, a nice-number tick scale with numeric value labels, and an
/// optional `title` (e.g. `"reflectivity (dBZ)"`). The title may contain
/// newlines to stack multiple lines — the WMS handler uses a second line for the
/// selected style's name. Labels are drawn with the embedded [`font`] so
/// `ds-render` stays framework-free.
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
    const LINE_GAP: u32 = 2; // vertical gap between stacked title lines

    let mut rgba = vec![255u8; (width as usize * height as usize) * 4]; // white background

    // Reserve a title block at the top. The title may carry several lines
    // (newline-separated) — e.g. "<parameter> (<unit>)" plus the selected WMS
    // style's name — each drawn top-aligned. Drawn only when there's vertical
    // room for the whole block plus a usable gradient below; a single line
    // reduces to the original one-row reservation.
    let title_lines: Vec<&str> = title
        .map(|t| t.split('\n').filter(|l| !l.is_empty()).collect())
        .unwrap_or_default();
    let title_h = if title_lines.is_empty() {
        0
    } else {
        let n = title_lines.len() as u32;
        let block = n * font::GLYPH_H * SCALE + (n - 1) * LINE_GAP;
        if height >= PAD * 2 + block + 20 {
            for (i, line) in title_lines.iter().enumerate() {
                let ly = PAD + i as u32 * (font::GLYPH_H * SCALE + LINE_GAP);
                font::draw_text(
                    &mut rgba, width, height, PAD as i32, ly as i32, line, TEXT, SCALE,
                );
            }
            block + PAD // block + a small gap below it before the gradient
        } else {
            0
        }
    };

    // Will we have horizontal room for tick labels? Size the decision from the
    // *actual widest* label (e.g. "-32", "0.0008"), not a fixed 2-digit guess —
    // otherwise a narrow legend whose labels are wider than "00" would pass the
    // check and then clip the labels' right edge. Empty ticks (degenerate range)
    // → no labels, full-width swatch.
    let ticks = legend_ticks(min, max);
    let max_label_w = ticks
        .iter()
        .map(|v| font::text_width(&format_tick(*v), SCALE))
        .max()
        .unwrap_or(0);
    let want_labels =
        !ticks.is_empty() && width >= PAD * 2 + 12 + TICK_LEN + LABEL_GAP + max_label_w;

    let grad_x0 = PAD;

    // Swatch width: a slim bar when we're labelling (the labels carry the
    // information), or the historical ~40% when there's no room for labels.
    // Both branches must keep `grad_x0 + swatch_w <= width` — the swatch starts
    // at `grad_x0`, so capping at `width` alone would let the gradient loop spill
    // `grad_x0` columns past the row end into the next row (corruption, and an
    // out-of-bounds write at the smallest requestable sizes).
    let swatch_w = if want_labels {
        ((width / 6).clamp(14, 30)).min(width.saturating_sub(2 * PAD).max(1))
    } else {
        (width * 2 / 5).max(10).min(width.saturating_sub(grad_x0))
    };

    let grad_y0 = PAD + title_h;
    // Bottom of the swatch; `grad_h` is 0 when the image is too short to hold a
    // PAD-margined swatch below the title. All pixel writes below are bounds-
    // checked (via `set_px`), so a 0-height swatch simply draws nothing rather
    // than spilling past the buffer at pathologically small heights.
    let grad_h = height.saturating_sub(PAD).saturating_sub(grad_y0);

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
            set_px(&mut rgba, width, height, grad_x0 + gx, py, px);
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
        for v in &ticks {
            let v = *v;
            let py = value_to_y(v);
            // Short tick mark butting up against the swatch edge.
            for tx in 0..TICK_LEN {
                set_px(&mut rgba, width, height, tick_x0 + tx, py, TEXT);
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

/// Write one RGBA pixel, clipped to the buffer bounds. The legend's geometry can
/// place a swatch row or tick mark off-canvas at pathologically small sizes; this
/// keeps every write safe so the renderer degrades to "draws nothing" instead of
/// panicking.
#[inline]
fn set_px(rgba: &mut [u8], width: u32, height: u32, x: u32, y: u32, color: [u8; 4]) {
    if x < width && y < height {
        let idx = ((y * width + x) * 4) as usize;
        rgba[idx..idx + 4].copy_from_slice(&color);
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
///
/// The `U8` variant takes the #206 fast path: bake the colormap into a
/// 256-entry LUT (256 colormap evaluations instead of one per pixel) and
/// index it by the raw byte — no per-pixel float decode, boxing, or dyn
/// dispatch. The LUT entry is *defined* as `colormap.color(value_at(i))`,
/// so the output is byte-identical to boxing the same samples to `F64`.
pub(crate) fn colorize(tile: &RasterTile, colormap: &dyn ColorMap) -> Vec<u8> {
    use ds_core::map_engine::RasterValues;
    let mut rgba = Vec::with_capacity((tile.width * tile.height * 4) as usize);
    match &tile.values {
        RasterValues::F64(values) => {
            for value in values {
                let color = colormap.color(*value);
                rgba.extend_from_slice(&color);
            }
        }
        RasterValues::U8 {
            data,
            nodata,
            gain,
            offset,
        } => {
            let mut lut = [[0u8; 4]; 256];
            for (raw, entry) in lut.iter_mut().enumerate() {
                let value = if Some(raw as u8) == *nodata {
                    None
                } else {
                    Some(raw as f64 * gain + offset)
                };
                *entry = colormap.color(value);
            }
            for &raw in data {
                rgba.extend_from_slice(&lut[raw as usize]);
            }
        }
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

    /// The #206 correctness property: colorizing a `U8` tile through the
    /// 256-entry LUT must produce RGBA byte-identical to boxing the same
    /// samples to `F64` — nodata sentinel, gain/offset decode, and every
    /// colormap edge included. If this ever breaks, the fast path is
    /// changing WHAT is rendered, not just how.
    #[test]
    fn u8_lut_colorize_matches_boxed_f64_exactly() {
        use ds_core::map_engine::RasterValues;
        // Every possible raw byte, plus the sentinel scattered through.
        let data: Vec<u8> = (0..=255u8).chain([255, 0, 128, 255]).collect();
        let (gain, offset, nodata) = (0.5, -32.0, Some(255u8));
        let boxed: Vec<Option<f64>> = data
            .iter()
            .map(|&raw| {
                if Some(raw) == nodata {
                    None
                } else {
                    Some(raw as f64 * gain + offset)
                }
            })
            .collect();
        let w = data.len() as u32;
        let u8_tile = RasterTile {
            width: w,
            height: 1,
            values: RasterValues::U8 {
                data,
                nodata,
                gain,
                offset,
            },
        };
        let f64_tile = RasterTile {
            width: w,
            height: 1,
            values: boxed.into(),
        };
        // A colormap whose stops sit at fractional dBZ values, so rounding
        // behaviour is exercised, over the packed byte range.
        let cmap = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, -32.0, 95.5);
        assert_eq!(
            colorize(&u8_tile, &cmap),
            colorize(&f64_tile, &cmap),
            "U8 LUT colorize must be byte-identical to the boxed F64 path"
        );
    }

    #[test]
    fn test_render_tile_png_produces_valid_png() {
        let tile = RasterTile {
            width: 4,
            height: 4,
            values: (0..16)
                .map(|i| Some(i as f64 / 15.0))
                .collect::<Vec<_>>()
                .into(),
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
            values: (0..16)
                .map(|i| Some(i as f64 / 15.0))
                .collect::<Vec<_>>()
                .into(),
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
            values: vec![None].into(),
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
            values: vec![None; 16].into(),
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
            values: vec![None, Some(1.0), None, None].into(),
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
    fn legend_ticks_land_on_round_values_within_range() {
        // Intent-level checks (robust to retuning the tick-density heuristic):
        // a 0..70 range yields ≥4 evenly-spaced ascending ticks that include
        // both extremes. Including the extremes is the property this PR fixed —
        // taking the step from the raw range, not a nice-rounded range, keeps
        // step 10 (0,10,…,70) instead of step 20 (0,20,40,60, dropping 70).
        let ticks = legend_ticks(0.0, 70.0);
        assert!(
            ticks.len() >= 4,
            "want a usable number of ticks, got {ticks:?}"
        );
        assert!(
            (ticks[0] - 0.0).abs() < 1e-9,
            "first tick should be the min"
        );
        assert!(
            (ticks[ticks.len() - 1] - 70.0).abs() < 1e-9,
            "last tick should be the max (extreme must not be dropped)"
        );
        let step = ticks[1] - ticks[0];
        assert!(step > 0.0);
        assert!(
            ticks.windows(2).all(|w| (w[1] - w[0] - step).abs() < 1e-9),
            "ticks should be uniformly spaced, got {ticks:?}"
        );

        // Every tick stays inside the gradient's value range (the legend clips
        // the chart axis generator, which may overshoot the extent).
        let ticks = legend_ticks(-32.0, 95.0);
        assert!(!ticks.is_empty());
        for t in &ticks {
            assert!((-32.0..=95.0).contains(t), "tick {t} escaped the range");
        }
        // A sub-unit range still produces sane ascending round ticks.
        let small = legend_ticks(0.0, 1.0);
        assert!(small.contains(&0.0) && small.contains(&1.0));
        assert!(small.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn legend_ticks_handle_degenerate_ranges() {
        // Zero-width / inverted / non-finite ranges yield no ticks (the legend
        // then draws a bare gradient) rather than panicking.
        assert!(legend_ticks(5.0, 5.0).is_empty());
        assert!(legend_ticks(70.0, 0.0).is_empty());
        assert!(legend_ticks(f64::NAN, 1.0).is_empty());
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
        // Float noise from `(t/step).round()*step` snaps to the clean value.
        assert_eq!(format_tick(0.1 + 0.2), "0.3");
    }

    #[test]
    fn format_tick_keeps_small_magnitude_digits() {
        // A `[0, 0.001]`-style range: ticks must not all collapse to "0" (the
        // bug a fixed `.3` format had).
        assert_eq!(format_tick(0.0002), "0.0002");
        assert_eq!(format_tick(0.0005), "0.0005");
        assert_eq!(format_tick(-0.0008), "-0.0008");
        // Distinct small ticks stay distinct.
        assert_ne!(format_tick(0.0002), format_tick(0.0004));
    }

    #[test]
    fn format_tick_uses_scientific_for_sub_micro() {
        // Below 6 dp, fixed notation would print "0" for every tick; scientific
        // notation keeps a sub-µ range's labels distinct. Exact zero is still "0".
        assert_eq!(format_tick(0.0), "0");
        assert_eq!(format_tick(2e-8), "2e-8");
        assert_ne!(format_tick(2e-8), format_tick(4e-8));
        // A tiny negative is rendered, not flattened to "0".
        assert_eq!(format_tick(-1e-10), "-1e-10");
    }

    #[test]
    fn render_legend_tiny_sizes_do_not_panic() {
        // The swatch starts at PAD, so a too-small width must not let the
        // gradient loop write past a row (corruption / OOB panic). Sweep the
        // pathological small sizes the API clamp permits (min 1×1).
        let cmap = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, -32.0, 95.0);
        for w in [1u32, 2, 4, 5, 8, 10, 13, 38, 40] {
            for h in [1u32, 2, 8, 200] {
                let png = render_legend(
                    &cmap,
                    -32.0,
                    95.0,
                    w,
                    h,
                    ImageFormat::Png,
                    Some("DBZH (dBZ)"),
                )
                .expect("tiny legend must still encode");
                assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "w={w} h={h}");
            }
        }
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
    fn render_legend_draws_multi_line_title() {
        // A second title line (the selected WMS style's name) is drawn and shifts
        // the gradient down, so the output differs from the single-line legend.
        let cmap = LutColorMap::from_builtin(BuiltinColormap::RadarDbz, -32.0, 95.0);
        let one = render_legend(
            &cmap,
            -32.0,
            95.0,
            180,
            300,
            ImageFormat::Png,
            Some("DBZH (dBZ)"),
        )
        .unwrap();
        let two = render_legend(
            &cmap,
            -32.0,
            95.0,
            180,
            300,
            ImageFormat::Png,
            Some("DBZH (dBZ)\nFMI Radar"),
        )
        .unwrap();
        assert!(two.starts_with(&[0x89, b'P', b'N', b'G']));
        assert_ne!(one, two, "the style-name line must affect the output");
        // Deterministic across calls.
        let two_again = render_legend(
            &cmap,
            -32.0,
            95.0,
            180,
            300,
            ImageFormat::Png,
            Some("DBZH (dBZ)\nFMI Radar"),
        )
        .unwrap();
        assert_eq!(two, two_again);
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
