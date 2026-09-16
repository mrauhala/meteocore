# engine-geotiff crate — Claude Instructions

GeoTIFF/COG data engine. Read the root `CLAUDE.md` first — Critical Rules 5
(no per-pixel projection; `src/resample.rs` is the workspace reference
implementation) and 6–7 (poll on background runtime; ds-storage bridge)
apply here.

## Format & sources

- **Must be tiled COG.** Strip-based TIFFs are rejected. One parameter
  (band) per collection.
- **CRS:** WGS84, TM, LAEA, LCC, Stereographic (math in
  `ds-core/src/geo.rs`).
- **Reprojection:** `bbox_to_pixels()` samples 20 points per edge to capture
  projection curvature.
- **Data sources (mutually exclusive):** local directory (`data_path`), S3
  (`endpoint` + `bucket` + `prefix_pattern`), or STAC (`stac_url` +
  `stac_asset_allowlist`).
- **STAC security:** `stac_asset_allowlist` is mandatory (SSRF protection).
  HTTP redirects disabled. Pagination origin-checked.

## Decode admission

`MC_GEOTIFF_DECODE_MEMORY_MB` (default 1024; 0 rejects cold decodes) bounds
transient local/remote GeoTIFF decoding across collections and APIs. Local
cache hits bypass it; cache retention has its own budget. Local misses reserve
native output plus the decoder’s capped 64 MiB intermediate buffer. Remote
reservations include bounded raw output and boxed samples and stay owned until
parallel tile assembly releases them. Exhaustion propagates as HTTP 503, never
as transparent pixels or an error image. This is separate from render output
admission and does not cover other engines or the final source-window buffer.

## Caches

- **Tile cache:** compressed bytes in an LRU (default 256 MB), **remote
  sources only** — local files get compressed bytes free from the mmap/page
  cache.
- **Rendered image cache** (default 512 MB) shared across WMS/Maps/Tiles.
- **Decoded-chunk cache (#463):** process-global byte-bounded LRU of
  *decoded* native source tiles for **local** files
  (`MC_GEOTIFF_DECODED_CHUNK_CACHE_MB`, default 512, 0 disables). The WMS
  meta-tile loop renders one viewport as ~50–190 independent
  `get_raster_tile` calls whose covering source tiles overlap; without the
  memo each source tile is LZW/DEFLATE-decoded ~6× per frame. Keyed
  `(path, mtime, size, inode, ifd, chunk)` — inode included because
  mtime+size alone miss a same-size same-second atomic rename (#253) — so a
  replacement can't serve stale pixels. Band extraction + nodata +
  scale/offset are applied at copy time for the intersecting window only.
- Remote bbox reads can coalesce nearby compressed cache misses with
  `MC_COG_RANGE_BATCH_TILES` (default **1: disabled**, clamped 1–16). An
  opt-in batch is capped at 1 MiB, 4 KiB per gap and 10% total overfetch.
  Its input is charged to decode admission alongside each decoded tile.
  Cache entries own individual tile allocations: never insert `Bytes::slice`
  of a batch into the LRU, because its weigher would undercount retained RAM.
  Both object-store and direct HTTP use the same planner. Short/rejected
  batches fall back to individual reads; admission/deadline errors fail the
  request. Keep it opt-in until measured on the deployment: current OPERA
  tests reduce request count but do not establish a latency win. See
  `docs/performance/cog-range-batching.md`.
- Remote tile fetch concurrency: `MC_COG_TILE_CONCURRENCY` (default 16,
  clamp [1,1024]). It's I/O-bound — size by RTT, not cores.

## Rendering gotchas (hard-won)

- **Overview selection:** pick overviews with bounded upscale
  (`select_overview`, `MIN_OVERVIEW_FRACTION = 0.5`) — selecting full-res
  just above the largest overview caused 36 MP decodes for desktop WMS.
- **Edge-tile decode stride (#458):** the tiff crate clips the rightmost
  tile's data to its clipped width; indexing it with the full tile-width
  stride shears the data (venetian blinds / displaced east column). Local
  paths use the clipped width (`local_tile_data_width` in `src/reader.rs`);
  remote tiles are padded to `tile_width`. When zoom-out artifacts appear,
  bisect decode vs resample vs projection before theorizing.
- **u8 fast path (#206):** the map-render path produces `RasterValues::U8`
  for local u8 sources with an integer u8 nodata (`reader::read_bbox_u8`,
  self-gating with `Ok(None)` → boxed-f64 fallback).
- Low-zoom domain guard: `OutputCrs::footprint_pixel_window` (ghosts, #453);
  `ProjectionGrid` error probing must reach tile edges (#448 — the curtain
  bug was invisible on low-res fixtures; reproduce at production
  resolution).

## EDR

Position + area queries. Nearest-neighbour sampling of the source grid.

Encoded remote tile ranges are validated before fetching: each is capped at
64 MiB and included in decode admission alongside raw and boxed output buffers.
Direct HTTP range bodies are also streamed with the requested length as a cap,
including when the origin omits Content-Length or ignores Range. Invalid tile
index arithmetic fails the request; ordinary fetch/decode errors may still
produce nodata gaps.

Interactive render deadlines propagate through Rayon tile fetching to both
object-store and direct HTTP range/body reads. All retries share one absolute
end time; deadline errors stop retries and fail the request with 503 rather
than becoming nodata gaps. Background scans have no render deadline and retain
their existing storage timeout/retry policy.
