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
