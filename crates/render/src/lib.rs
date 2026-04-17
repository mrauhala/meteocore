pub mod colormap;
mod encode;

pub use colormap::{
    parse_hex_color, BuiltinColormap, ColorMap, ColorStop, IntegerLutColorMap, LinearColorMap,
    LutColorMap,
};
pub use encode::{encode_jpeg, encode_png, encode_webp};

use std::hash::{Hash, Hasher};
use std::time::Duration;

/// Maximum time to wait for a render semaphore permit before returning 503.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use quick_cache::sync::Cache;

use ds_core::error::DataServerError;
use ds_core::map_engine::RasterTile;

/// Output image format.
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
}

impl CacheKey {
    /// Compute an ETag from the cache key for HTTP caching.
    pub fn etag(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        let hash = hasher.finish();
        format!("\"{hash:016x}\"")
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

/// Weight function: count the byte size of each cached rendered image.
#[derive(Clone)]
struct RenderedWeighter;

impl quick_cache::Weighter<CacheKey, Bytes> for RenderedWeighter {
    fn weight(&self, _key: &CacheKey, val: &Bytes) -> u64 {
        val.len() as u64 + 64 // data + overhead
    }
}

/// Cache for rendered map images (Tier 2), weighted by byte size.
/// Keys are quantized to improve hit rates for tiled clients.
/// No TTL — radar measurements are immutable once produced.
/// Cache is invalidated on collection reload.
pub struct RenderedCache {
    cache: Cache<CacheKey, Bytes, RenderedWeighter>,
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

    pub fn get(&self, key: &CacheKey) -> Option<Bytes> {
        let result = self.cache.get(key);
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    pub fn insert(&self, key: CacheKey, value: Bytes) {
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

/// Render a legend image showing the colormap scale.
///
/// Produces a vertical gradient with labeled tick marks.
/// Width: `width` pixels, Height: `height` pixels.
pub fn render_legend(
    colormap: &dyn ColorMap,
    min: f64,
    max: f64,
    width: u32,
    height: u32,
    format: ImageFormat,
) -> Result<Vec<u8>, DataServerError> {
    let gradient_width = (width * 2 / 5).max(10); // left 40% is gradient
    let mut rgba = vec![255u8; (width * height * 4) as usize]; // white background

    for y in 0..height {
        // Map y position to value (top = max, bottom = min)
        let frac = y as f64 / (height.saturating_sub(1).max(1)) as f64;
        let value = max - frac * (max - min);
        let color = colormap.color(Some(value));

        for x in 0..gradient_width {
            let idx = ((y * width + x) * 4) as usize;
            rgba[idx] = color[0];
            rgba[idx + 1] = color[1];
            rgba[idx + 2] = color[2];
            rgba[idx + 3] = color[3];
        }
    }

    match format {
        ImageFormat::Png => encode::encode_png(&rgba, width, height),
        ImageFormat::Jpeg => encode::encode_jpeg(&rgba, width, height),
        ImageFormat::Webp => encode::encode_webp(&rgba, width, height),
    }
}

/// Colorize a raster tile into an RGBA buffer using a colormap.
fn colorize(tile: &RasterTile, colormap: &dyn ColorMap) -> Vec<u8> {
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
        let legend = render_legend(&cmap, 0.0, 1.0, 40, 200, ImageFormat::Png).unwrap();
        assert!(legend.starts_with(&[0x89, b'P', b'N', b'G']));
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
}
