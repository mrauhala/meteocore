pub mod colormap;
mod encode;

pub use colormap::{
    parse_hex_color, BuiltinColormap, ColorMap, ColorStop, IntegerLutColorMap, LinearColorMap,
    LutColorMap,
};
pub use encode::{encode_jpeg, encode_png, encode_webp};

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
        };
        let mut other = base.clone();
        other.parameter = Some("10u".into());
        assert_ne!(base.etag(), other.etag());

        let mut absent = base.clone();
        absent.parameter = None;
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
        };
        // Pinned after introducing the `z` field (cache-bust event).
        assert_eq!(key.etag(), "\"95776756a198bd3a\"");
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
