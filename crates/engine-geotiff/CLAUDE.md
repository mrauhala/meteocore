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
  EPSG sphere ellipsoid) is projected on that sphere (LCC `radius`, #810;
  TM/LAEA `figure`, #1192); everything else on WGS84, EPSG:3067/3035 by
  code too. `crs_label` claims those codes only on a WGS84-like figure
  (`Ellipsoid::is_wgs84_like`). Extent and footprint guard:
  `bbox_with_poles` / `footprint_envelope`, never `bbox()`; a full-turn
  footprint bounds latitude only (`every_longitude_near`). LCC 2SP
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
  review). The scan also skips hidden names (`.name`) whatever
  `exclude_patterns` holds (#1009), and the remote scan skips objects in a
  hidden folder below the prefix, such as rsync `--delay-updates`'
  `.~tmp~/` (#1029); the local scan never descends into a folder. The
  engine keeps its post-processing:
  the pending-file readiness check and metadata reuse (local), header reads
  (remote). The readiness check is the shared
  `ds_storage::discovery::PendingFile` over the file's size
  (`ObjectVersion::sized`, #1067): a file that appears between polls is
  catalogued once two more polls see its size unchanged, and the initial
  scan accepts every file at once. The local scan skips symlinks, because
  metadata reuse compares the directory entry's own size, mtime and inode.
  A dynamic source passes no `max_files` to the scan: `do_scan` trims after
  the metadata pass, so a file that fails to parse does not cost a slot.
- **Remote header read (#1003):** `TiffMetadata::from_header_read` returns
  `Err` when the range read fails and `Ok(None)` when the bytes are not a
  COG header. Only `Ok(None)` takes the full-download fallback. On `Err`
  the scan leaves the file out of the catalog, so the next poll reads its
  header again. A timeout is not a non-COG, and a full download from the
  same stalled storage only stalls longer.

## STAC metadata loading (#90, #889, #1075)

- Startup reads only the collection extent. Each `poll_cycle` adds items
  newer than the newest entry as stubs (the first poll: the last hour), then
  `preload_stac_metadata` fetches the header/IFD of the ones it just
  discovered, on the poll runtime: newest `STAC_PRELOAD_MAX_ITEMS` only,
  `STAC_PRELOAD_CONCURRENCY` in flight, async reqwest end to end. Never loop
  the sync `load_stac_entry_metadata` there: that is N sequential blocking
  round trips (root Critical Rule 9).
- The request path is the fallback for older items (on-demand
  `fetch_stac_range`), items past the cap and failed preloads. A render
  loads its one item (`ensure_metadata`). An EDR query loads every stub in
  its window through `load_cold_stac_entries` (#889); a query without
  `datetime`, every stub in the catalog. More than
  `STAC_REQUEST_MAX_COLD_ITEMS` (400; a closed one-day window of
  five-minute frames is 289 items) is a 400 `QueryTooLarge` naming both
  counts and telling the client to narrow, or add, a datetime window.
  `ensure_entries_loaded` counts an on-demand STAC response before it
  installs it, so a refused query leaves no stubs behind to count against
  later queries. The stubs a query without `datetime` can still meet are a
  poll backlog past the preload cap and items whose load failed; a windowed
  query loads those in its window.
- The on-demand listing (`list_stac_window`, through
  `StacClient::fetch_items_until`) counts the window's cold items page by
  page and stops paging once they pass `STAC_REQUEST_MAX_COLD_ITEMS`
  (#1075). Later pages only add items, so the 400 gives the count so far as
  "at least N" and installs nothing; a listing that reaches its last page
  gives the exact count. A catalog with `max_files` lists the window in
  full: its trim can drop window items again, and the count after the trim
  decides, as before.
- An area query checks its window's grid between that count and the loads
  (`check_window`, #1075): the bbox must meet the grid, then the mask and
  the response budgets, in `query_area`'s order. The answer uses the grid of
  the window's first entry, so that is the grid checked. Loaded, it is
  checked at once and a refused query loads nothing. A stub loads alone
  first, one round trip, so a refused query loads that one item, not the
  window. The loaded entry nearest it only decides whether that extra
  round trip is needed: its grid passing the check lets the first entry
  load with the rest. Entries can differ in grid, so never refuse on that
  estimate. The checks after the loads stay, for scanned sources and for a
  catalog that changed meanwhile.
- The cold items load `STAC_PRELOAD_CONCURRENCY` at a time under one
  `block_on` of the STAC bridge, and none starts once the request's
  `ds_core::deadline` has passed. Never loop `ensure_metadata` over a
  window's cold items: that is N sequential round trips on the request
  worker. Its one loop is the wait afterwards for the items another loader
  held, bounded by that loader's claims.
- Every loader shares `fetch_stac_entry_metadata` and the
  `loading_in_flight` single-flight set. The preload and a query's
  concurrent loads claim through `claim_and_load_stac_entry`, which
  `try_claim`s without parking: a future parked on the condvar would stall
  every other load of its `block_on`. The preload skips an item someone else
  is loading. A query waits for such items (`ensure_metadata`) only after its
  own loads finish, holding no claim, so two loaders never wait on each
  other; it loads an item itself only when that loader gave it up. Install
  through `install_stac_metadata`, whose `rcu` keeps concurrent loaders from
  dropping each other's update.

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
budget and its client-visible failure. `decode_budget` owns the static and
env var; the accounting is `ds_executor::budget::ByteBudget`, shared with
engine-zarr.

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
  clamp [1,1024]). It's I/O-bound — size by RTT, not cores. It bounds the
  fetches of all concurrent renders together. A projected (meta-tiled) WMS
  render asks for only the new source tiles of one meta-tile at a time, so
  a larger pool does not shorten its first view; a whole-viewport
  `get_raster_tile` (WMS EPSG:4326, Maps) fetches every missing tile at once
  and does wait on the pool. Raise it (32–64) when cold renders queue on a
  high-RTT store.
- **First-view pre-warm (#1004, `src/prewarm.rs`):** the WMS meta-tile loop
  is sequential, so a cold remote frame pays one storage round trip for
  every meta-tile that reaches a source tile no earlier one fetched
  (`docs/performance/cog-first-frame.md`). `prewarm_new_frames` therefore
  reads the encoded tiles of the frames a poll discovers (object store and
  STAC) into the compressed tile cache: the newest `PREWARM_MAX_FRAMES`
  absent from the previous catalog, levels coarsest first while the frame
  stays within `prewarm::frame_cap` (`MC_COG_PREWARM_MB`, default 32 MiB,
  0 off, at most 1/8 of the tile cache), coalesced by `range_batch::plan`,
  4 reads in flight on the poll runtime's blocking pool through the
  explicit-handle bridges, within `PREWARM_BUDGET`. `poll_loop` warms the
  startup catalog before its first sleep. It fills the compressed cache
  only: a decoded OPERA frame is ~100 MB of the shared decoded-chunk
  cache. Each read holds `decode_budget::BUDGET.try_reserve_background`,
  which leaves half the budget to requests and is not a counted rejection.
  While requests hold more, a read waits up to `prewarm::BUSY_WAIT` (2 s)
  and is then left to them: never skip at the first refusal, or a render
  burst at poll time leaves the new frame cold. It logs `Pre-warmed N new
  frame(s) for first views: …` per poll, and a WARN for reads that failed,
  were cut off by `PREWARM_BUDGET` or gave up on a busy budget.
- A render cut off by its deadline keeps every tile it already fetched:
  each tile is cached as it lands and only in-flight reads are dropped, so
  the client's retry fetches the rest
  (`a_deadline_cut_read_keeps_fetched_tiles_for_the_retry`).

## Remote tile read failures (#1051, #1074)

`reader::read_remote_chunk_with_retry` is the one policy for every remote
tile read: map renders (WMS, Maps, Tiles), EDR area and radius
(`read_bbox`) and EDR position (`read_pixel`). `read_encoded_chunk` gives a
failure a class, `TileReadError`, and the class decides, never the message:

- **`Fetch`**: a storage or transport error, or a short body. Retried twice
  (50 ms, then 100 ms, within the request deadline). Still failing, the read
  fails with `ResourceExhausted` and the detail is logged: HTTP 503 with
  `Retry-After` in WMS/Maps/Tiles, 503 in EDR. Never an `Ok` tile with
  nodata in its place: the rendered and meta-tile caches keep every `Ok`
  tile with no TTL, and an explicit-time response is browser-cached for a
  day, so a gap from a short storage outage outlived it.
- **`Refused`**: the store refused the range for good, typed by
  `DataStore::get_range_or_refusal` (object store) or
  `read_http_range_answer` (STAC assets) as a `ds_storage::RangeRefusal`:
  404, 416 or a response whose total size ends before the range, or
  401/403. Never retried, and recorded in the source's `ObjectHealth`. A
  gone object's tile is nodata at once; a forbidden one fails the read
  with `ResourceExhausted`. See "Objects gone after the scan" below.
- **`Decode`**: the bytes arrived but do not decode. A logged nodata gap at
  once, not retried: the compressed cache already holds the same bytes.
- **`Fatal`**: decode admission, the deadline and an invalid tile layout
  fail the read at once and are never retried. Admission and the deadline
  fail the request in every API; an invalid layout fails a map render, but
  on EDR it is the #858 unreadable-file null (see EDR below). A fetch
  closure passes the storage bridge's `DeadlineExceeded` through unchanged
  (`range_read_error`), so a deadline is never retried or reported as a
  failed store.
- An empty tile (`byte_count == 0`) is no failure: nodata, without I/O.

The poll-time pre-warm (`prewarm.rs`, `reader::fetch_encoded_range`) is not
a read under this policy: it caches only bytes that arrived and logs a WARN
for reads that failed, so it can neither fail a request nor insert a gap.

## Objects gone after the scan (#1074)

A remote object deleted, truncated or forbidden after the scan catalogued
it answers its tile ranges with a refusal (`Refused` above). What happens
depends on the kind:

- **Gone (404, 416, shorter):** the request that meets it draws those
  tiles as nodata (EDR: nulls with 200), as engine-odim draws a missing
  volume. The refusal is set once in the source's
  `reader::ObjectHealth`, which every clone of the catalog entry shares,
  and later reads of that object make no I/O. Every catalog accessor skips
  the entry at once (`FileEntry::is_gone`): `Catalog::select_timestamp`,
  so `resolve_time` and `get_raster_tile` still share one selection (#507),
  `Catalog::live` for `RasterInfo` and `data_age`, and `filter_by_datetime`
  for EDR. The request path never builds a catalog; it writes only that
  atomic. `note_object_health` runs after each read (a drop guard on the
  map path), logs a WARN once per object and refreshes the `RasterInfo`
  snapshot.
- **Removal:** the next catalog swap screens the catalog
  (`gone::GoneObjects::screen`, from `do_scan` before the `max_files` trim,
  from `store_stac_catalog`, and in `ensure_entries_loaded` before a query
  counts its window's cold items), removes the entry and keeps it, keyed by
  path (object key or asset href) and size, the identity the scan's own
  reuse compares. An object-store listing that still names the key with
  that size reuses the kept entry without a header read, and the screen
  removes it again; a key the complete listing no longer names, or names
  with another size, ends the record. A STAC listing that still names the
  href with that `file:size` adds a stub, which the screen removes; a new
  href or size is a new object. A same-size rewrite under the same key is
  not told apart, as on the scan's reuse path.
- **Nothing left:** a map or map tile over an empty catalog, before the
  first file or after every file is gone, has no timestep to select.
  `get_raster_tile` answers `LocationNotFound("No data available for this
  collection")`: a 404 with that message in Maps and Tiles, while WMS draws
  its error tile. Never an `Engine` error, which is an opaque 500 there
  (#1136, `empty_catalog_map_is_not_found`).
- **`content_version`:** 0 until a timestep the screen removed holds an
  entry again, then it moves before that catalog is stored. Renders cached
  under the timestep may hold the gap, or the frame the time snapped to.
  Do not move it on every drop: a rolling-window bucket deletes frames all
  the time, and each move leaves every render the collection cached unread.
- **Forbidden (401/403):** the read fails with `ResourceExhausted`, a 503
  in every API, without retries. It is the server's credentials or a
  policy, not the data: a nodata tile would mislead and would stay in the
  no-TTL caches after the fix (the #1051 lesson). Counted in
  `ObjectHealth`, never marked gone, so the timestep stays: dropping on it
  would empty the catalog. A WARN at most once a minute per collection
  (`gone::ForbiddenWarnings`); `note_object_health` runs before an EDR
  query returns the error, and from the drop guard on the map path.

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
the limit (#858). The polygon mask tests each cell of the window against every
ring vertex, so `query_area` first checks the shared
`ds_core::feature::check_mask_budget`, before `query_bbox` reads a file
(#889): over 20 M edge tests is a 400, as on GRIB and QueryData. A rectangle
is exempt; a radius query's 65-vertex circle trips it past about 307 k cells.
Per timestep, only a file that cannot be read (`Engine`, `Storage`, `Io`:
`is_unreadable_file`), such as a local file that fails to open or decode or a
remote file with an invalid tile layout, becomes logged nulls; any other read
error fails the query — client errors stay 400, admission/deadline 503/504. A
remote tile fetch is no unreadable file: one that still fails after its
retries is `ResourceExhausted`, so the whole area query is a 503 and never a
timestep of nulls with HTTP 200 (#1051). Remote bytes that do not decode are
nulls in that tile's cells. Position follows the same policy: a failed remote
fetch is a 503, an undecodable tile or an invalid tile layout a null. On a
STAC source a query first loads the metadata of the cold items in its window,
bounded and concurrent (see STAC metadata loading above). The area and mask
budgets need the window's grid, so on a STAC source they also run before
those loads, on the window's first entry, which loads alone first when it is
cold (#1075).

Encoded remote tile ranges are validated before fetching: each is capped at
64 MiB and included in decode admission alongside raw and boxed output buffers.
Direct HTTP range bodies are also streamed with the requested length as a cap,
including when the origin omits Content-Length or ignores Range. Invalid tile
index arithmetic fails the read before any I/O, which on EDR is the
unreadable-file null above; fetch and decode failures follow "Remote tile
read failures" above.

Interactive render deadlines propagate through Rayon tile fetching to both
object-store and direct HTTP range/body reads. All retries share one absolute
end time; deadline errors stop retries and fail the request with 503 rather
than becoming nodata gaps. Background scans have no render deadline. Their
full-download fallback uses `FetchBudget::Background` (120 s), so
object_store's retry of a stalled body gets to run (#1011).
