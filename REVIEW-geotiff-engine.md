# GeoTIFF Engine Review — Unified Report

**Date:** 2026-03-26
**Review team:** 5 specialized agents (Architect, Performance, Security, Devil's Advocate, UX/Config)
**Scope:** `crates/engine-geotiff/`, `crates/storage/`, server wiring in `crates/server/src/main.rs`

---

## Overall Assessment

The engine is **architecturally sound** with clean separation of concerns, proper trait integration, smart caching (compressed tiles, not decoded), and good CRS support. The codebase follows the project's architecture rules well. All five reviewers independently flagged issues in overlapping areas, which strengthens confidence in these findings.

---

## Critical Issues (Tier 1 — Safety)

### 1. Unchecked array access in tile decoding — panics on malformed data

**Found by:** Security, Devil's Advocate
**File:** `reader.rs:490-536`

Raw byte access (`raw[off]`, `raw[off+1]`, etc.) has no bounds checking. A truncated tile from a corrupted file or flaky network causes an **index-out-of-bounds panic** — instant DoS.

**Status: FIXED** — Added upfront bounds validation before the decode loop.

### 2. Integer overflow in byte-range calculation

**Found by:** Security
**File:** `reader.rs:576-582`

`offset + byte_count` from untrusted TIFF IFD data can overflow. No validation that the range falls within the file. Can cause panics or reads beyond file boundaries.

**Status: FIXED** — Added `checked_add()` with descriptive error.

### 3. GeoKey array bounds not validated

**Found by:** Security
**File:** `reader.rs:786-819`

**Status: ALREADY HANDLED** — Check exists at line 786-788. The security reviewer missed this.

### 4. Decompression bombs — no size limit on inflate

**Found by:** Security, Devil's Advocate
**File:** `reader.rs:438-453`

`read_to_end()` on deflate/LZW streams has no cap. A crafted 100-byte tile claiming to decompress to gigabytes = OOM. The existing `MAX_DECODED_TILE_BYTES` check is at parse time, not decompression time.

**Status: FIXED** — Added `.take(MAX_DECODED_TILE_BYTES)` to deflate stream, post-decode size check to LZW.

### 5. Decoded tile size check hardcodes 8 bytes/sample

**Found by:** Devil's Advocate
**File:** `reader.rs:208`

The check `tile_width * tile_height * 8` assumes worst-case F64 for all files. This is wrong for U8 (1 byte) and U16 (2 bytes) — it over-rejects small-sample tiles. More importantly, it doesn't account for `samples_per_pixel`, so a multi-band file could bypass the limit.

**Status: FIXED** — Now reads actual BitsPerSample and SamplesPerPixel from the TIFF tags.

---

## High-Priority Issues (Tier 2 — Reliability)

### 6. No timeout on remote file I/O

**Found by:** Devil's Advocate, Performance
**File:** `catalog.rs:343-357`

A hung S3 endpoint blocks the entire poll cycle indefinitely. No per-file timeout wrapping `from_header_read()`. One slow file stalls all catalogs.

### 7. Polling loop has no shutdown signal

**Found by:** Architect, Devil's Advocate
**File:** `lib.rs:276-284`

`poll_loop()` runs forever with no cancellation mechanism. Cannot gracefully stop polling, reload collections, or shut down cleanly. Orphaned tasks on collection reconfiguration.

**Proposal:** Add `tokio::sync::watch` channel for shutdown signaling.

### 8. Mutex poisoning causes silent poll death

**Found by:** Security
**File:** `lib.rs:192`

`pending.lock().unwrap()` panics if another thread panicked while holding the lock. Poll loop terminates silently; catalog stops updating.

**Proposal:** Use `poisoned.into_inner()` recovery pattern.

### 9. Spatial extent computed from first file only

**Found by:** Devil's Advocate
**File:** `lib.rs:251-252`

If files have different spatial extents, the reported extent is "whatever the first file happens to be." Silent incorrectness.

**Proposal:** Compute union of all file extents.

### 10. Silent failures on remote scan errors

**Found by:** Architect, UX/Config, Devil's Advocate
**File:** `lib.rs:222-241`, `lib.rs:320`

Failed prefix scans log a warning but silently reduce the catalog. If ALL files fail to download, the old (stale) catalog is kept with no alert.

**Proposal:** Collect scan errors and log aggregated summary. Track time-since-last-successful-scan.

---

## Performance Findings (Tier 3)

### 11. Multi-band iterator overhead — 5× CPU waste

**Found by:** Performance
**File:** `reader.rs:315-357`

Decodes all bands then skips to desired band via `skip(band_index).step_by(spp)`. For a 5-band file, processes 5× more pixels than needed.

**Proposal:** Add `decode_chunk_f64_band()` that samples only the requested band directly in the match arms.

### 12. Catalog poll clones entire entry set

**Found by:** Performance
**File:** `lib.rs:291-305`

10k files = ~5 MB allocated every poll cycle. With 60s poll interval: 300 MB/hour in allocations.

**Proposal:** Use `Arc<TiffMetadata>` or pass references instead of cloning.

### 13. PathBuf allocation on every cache lookup

**Found by:** Performance
**File:** `cache.rs:60-80`

Every tile read creates a new `PathBuf` for the cache key. 100-tile area query = 100 allocations.

**Proposal:** Use `Arc<Path>` cache keys.

### 14. Metadata re-downloaded on every poll cycle

**Found by:** Devil's Advocate, Performance
**File:** `catalog.rs:343-377`

Every poll re-downloads 64 KB headers for ALL remote files. 1000 files = 64 MB per poll cycle. Only caches by file_size match, not ETag/last-modified.

**Proposal:** Cache metadata by ETag or last-modified timestamp.

### 15. Unnecessary `.to_vec()` copies in decompression path

**Found by:** Performance
**File:** `reader.rs:253, 101, 437`

Bytes copied redundantly through Cursor wrapping. 64-512 KB wasted per remote tile.

---

## Architecture & Maintainability (Tier 4)

### 16. Datetime filtering duplicated in 3 places

**Found by:** Architect
**Files:** `lib.rs:346-350`, `lib.rs:434-438`, `catalog.rs:294-298`

Same range filter logic repeated. Should extract to a shared helper.

### 17. Coordinate parsing scattered in lib.rs

**Found by:** Architect
**File:** `lib.rs:774-910`

130+ lines of parsing utilities mixed with engine logic. Should be a separate module, potentially shared in `ds-core`.

### 18. Location query fallback parses lat,lon as location ID

**Found by:** Architect
**File:** `lib.rs:553-556`

`query_location()` interprets `location_id` as "lat,lon" — violates the semantic contract. GeoTIFF has no named locations; should return an explicit error.

### 19. Config overrides applied by mutation after scan

**Found by:** Architect
**File:** `lib.rs:258-265`

Violates immutability principle. Should apply overrides during `TiffMetadata` construction via `with_overrides()` builder method.

### 20. No rotation matrix support in GeoTransform

**Found by:** Architect
**File:** `geo.rs:97-108`

Pixel-to-world conversion assumes axis-aligned pixels. Rotated/skewed rasters give incorrect results.

---

## UX & Configuration

### 21. No early validation of config combinations

**Found by:** UX/Config
- `endpoint` without `bucket` silently falls back to local storage
- `filename_pattern` without `timestamp_format` gives a confusing error
- `poll_interval_secs = 0` would spin-loop with no warning
- `band = 2` on a single-band file returns all-None silently

### 22. Error messages lack guidance

**Found by:** UX/Config
**File:** `reader.rs:203-205`

"Not a tiled TIFF (TileWidth missing)" gives no fix guidance. Should suggest `gdal_translate -co TILED=YES`.

### 23. Documentation gaps in CLAUDE.md

**Found by:** UX/Config

Missing entirely:
- Supported CRS, compression types, data types
- GeoTIFF-specific config fields (`nodata`, `scale`, `offset`, `time_window`, `prefix_pattern`)
- COG requirement and conversion instructions
- Troubleshooting guide for common errors
- Polling behavior and file readiness semantics

### 24. No observability for catalog staleness

**Found by:** Devil's Advocate, UX/Config

No metric for "seconds since last successful poll." Operators can't tell if the catalog is 5 minutes or 5 hours stale. Cache hit/miss counters are cumulative (useless after a week).

### 25. Hardcoded limits not configurable or documented

**Found by:** UX/Config
- `MAX_RASTER_DIMENSION = 100,000` (reader.rs:13)
- `MAX_DECODED_TILE_BYTES = 64 MB` (reader.rs:14)
- `MAX_REMOTE_FILE_SIZE = 50 MB` (catalog.rs:246)
- `MAX_AREA_PIXELS = 1,000,000` (reader.rs:17)

No way for users to override these or even know they exist.

---

## Strengths

All reviewers noted these positive aspects:

1. **Clean module structure** — Each module (`lib.rs`, `catalog.rs`, `reader.rs`, `cache.rs`, `geo.rs`, `time_window.rs`) has a single responsibility
2. **Excellent dependency hygiene** — No framework deps in engine crate, proper trait integration
3. **Smart tile cache** — Caches compressed bytes (58× more memory-efficient than decoded)
4. **Lock-free concurrent access** — `ArcSwap` for catalog, `quick_cache` for tiles
5. **COG byte-range reads** — Efficient remote access with header-only parsing + fallback
6. **CRS transformation support** — WGS84, Transverse Mercator, LAEA, LCC
7. **Flexible config** — `filename_template` for simple cases, explicit regex for complex ones
8. **Resource limits** — Raster dimensions, tile sizes, area query pixels, remote file sizes
9. **File readiness tracking** — `pending` map prevents reading incomplete uploads
10. **Multi-band and data type support** — U8, U16, I16, F32, F64 with proper bit-width handling

---

## Recommended Implementation Order

| Tier | Focus | Items | Status |
|------|-------|-------|--------|
| **1** | Safety | #1-5 | **DONE** |
| **2** | Reliability | #6-10 | **DONE** |
| **3** | Performance | #11-15 | **DONE** |
| **4** | UX & Maintainability | #16-25 | **DONE** |
