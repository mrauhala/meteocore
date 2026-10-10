# engine-geotiff crate — Claude Instructions

GeoTIFF/COG data engine. Read the root `CLAUDE.md` first — Critical Rules 5
(no per-pixel projection; `src/resample.rs` is the workspace reference
implementation) and 6–7 (poll on background runtime; ds-storage bridge)
apply here.

## Format & sources

- **Must be tiled COG.** Strip-based TIFFs are rejected. One parameter
  (band) per collection.
- **CRS:** WGS84, TM, LAEA, LCC, Stereographic (math in
  `ds-core/src/geo.rs`). Earth model: an LCC whose geodetic GeoKeys
  describe a sphere (semi-minor = semi-major, inverse flattening 0, or an
  EPSG sphere ellipsoid) is projected on that sphere (`radius`, #810);
  everything else on WGS84. TM and LAEA have no sphere form yet. LCC 2SP
  takes its origin from the false-origin GeoKeys (3084-3087), the
  natural-origin ones (3080-3083) only as a fallback.
- **Reprojection:** `bbox_to_pixels()` samples 20 points per edge to capture
  projection curvature.
- **Data sources (mutually exclusive):** local directory (`data_path`), S3
  (`endpoint` + `bucket` + `prefix_pattern`), or STAC (`stac_url` +
  `stac_asset_allowlist`).
- **STAC security:** `stac_asset_allowlist` is mandatory (SSRF protection).
  HTTP redirects disabled. Pagination origin-checked.
- **Filename → timestamp:** the shared `ds_storage::discovery::FilenameMatcher`
  (#817), built once by `resolve_filename_config` (none in STAC mode) and
  shared with engine-odim — never re-implement it here. A
  `filename_template` is anchored `^…$`, so `.tmp`/`.part` partial uploads
  never match even with `exclude_patterns` emptied; an explicit
  `filename_pattern` is used as written and logged at WARN when unanchored.
- **Catalog scan:** `catalog::scan_directory` and `catalog::scan_remote` list
  and match through the shared `ds_storage::discovery::{scan_local,
  scan_remote}` (#817), which sorts, keeps one file per timestamp (greatest
  path or key wins, logged) and caps. A dynamic prefix pattern's prefixes are
  listed concurrently, at most `MAX_CONCURRENT_LISTS` at a time, never one
  `list` per prefix in a loop (root Critical Rule 9). `exclude_patterns` go
  into the scan as `ScanSpec.exclude`, for local and remote sources, and drop
  a basename before it is matched. Never filter them after the scan: an
  unanchored `filename_pattern` matches `….tif.part`, which sorts after its
  finished `….tif` and would win the timestamp before being dropped (#817
  review). The engine keeps its post-processing: the pending-file readiness
  check and metadata reuse (local), header reads (remote). The local scan skips
  symlinks, because metadata reuse compares the directory entry's own size,
  mtime and inode. A dynamic source passes no `max_files` to the scan:
  `do_scan` trims after the metadata pass, so a file that fails to parse
  does not cost a slot.

## STAC metadata loading (#90)

- Startup reads only the collection extent. Each `poll_cycle` adds items
  newer than the newest entry as stubs (the first poll: the last hour), then
  `preload_stac_metadata` fetches the header/IFD of the ones it just
  discovered, on the poll runtime: newest `STAC_PRELOAD_MAX_ITEMS` only,
  `STAC_PRELOAD_CONCURRENCY` in flight, async reqwest end to end. Never loop
  the sync `load_stac_entry_metadata` there: that is N sequential blocking
  round trips (root Critical Rule 9).
- The lazy request path (`ensure_metadata` / `ensure_entries_loaded`) stays
  the fallback for older items (on-demand `fetch_stac_range`), items past the
  cap and failed preloads. Both paths share `fetch_stac_entry_metadata` and
  the `loading_in_flight` single-flight set: the preload `try_claim`s without
  parking and skips an item a request is loading; a request for an item being
  preloaded waits for it. Install through `install_stac_metadata`, whose
  `rcu` keeps concurrent loaders from dropping each other's update.

## Decode admission

`MC_GEOTIFF_DECODE_MEMORY_MB` (default 1024; 0 rejects cold decodes) bounds
transient local/remote GeoTIFF decoding across collections and APIs.
Decoded-chunk cache hits bypass it; cache retention has its own budget. Local
misses reserve native output plus the decoder’s capped 64 MiB intermediate
buffer. Remote reservations include the encoded input, bounded raw output and
the one-band native tile, and stay owned until parallel tile assembly releases
them. Exhaustion propagates as HTTP 503, never
as transparent pixels or an error image. This is separate from render output
admission and does not cover other engines or the final source-window buffer.
A full-resolution map window has its own source-pixel cap,
`reader::MAX_MAP_PIXELS`; the root CLAUDE.md "Pixel budgets" lists each
budget and its client-visible failure.

## Caches

- **Tile cache:** compressed bytes in an LRU (default 256 MB), **remote
  sources only** — local files get compressed bytes free from the mmap/page
  cache.
- **Rendered image cache** (default 256 MB, `[wms] rendered_cache_mb`) shared
  across WMS/Maps/Tiles; see `crates/api-wms/CLAUDE.md`.
- **Decoded-chunk cache (#463, #468):** process-global byte-bounded LRU of
  *decoded* native source tiles for local files **and** remote COGs
  (`MC_GEOTIFF_DECODED_CHUNK_CACHE_MB`, default 512, 0 disables; one shared
  budget). The WMS meta-tile loop renders one viewport as ~50–190
  independent `get_raster_tile` calls whose covering source tiles overlap;
  without the memo each source tile is LZW/DEFLATE-decoded 4–8× per frame.
  Local keys are `(path, mtime, size, inode, ifd, chunk)` — inode included
  because mtime+size alone miss a same-size same-second atomic rename
  (#253) — so a replacement can't serve stale pixels. Remote keys are
  `(path, TileCache namespace, band, ifd, chunk)`: path-immutable like the
  compressed cache, per engine, one band only (a 2-band OPERA Float32 tile
  is 1 MB). Remote reads are deliberately NOT single-flight: a fill includes
  the range fetch, and a fetch-pool worker must not wait on another
  request's I/O. Nodata + scale/offset (and a local chunk's band
  extraction) are applied at copy time for the intersecting window only.
  Warm OPERA full-viewport renders fell from 124–235 ms to 53–82 ms
  (`docs/performance/geotiff-remote-decode.md`).
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

Area (and radius) answers the native pixel window of the polygon's bbox, like
GRIB: no `MAX_AREA_DIM` coarsening, which would need overview selection and a
strided polygon mask. `query_bbox` checks the shared
`ds_core::feature::check_area_budget` over `timesteps × ny × nx` before it
allocates the result or reads a file, so an over-budget area is a 400 naming
the limit (#858). Per timestep, only a file that cannot be read (`Engine`,
`Storage`, `Io`: `is_unreadable_file`) becomes logged nulls; any other read
error fails the query — client errors stay 400, admission/deadline 503/504.

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
