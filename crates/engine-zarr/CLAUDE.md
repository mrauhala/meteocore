# engine-zarr crate — Claude Instructions

Zarr V2/V3 multidimensional-array engine (cloud-native, CF conventions),
tracked in #125. Phases 1–3 ship today: local + remote (S3/HTTP) stores,
WGS84 lat-lon grids, multi-variable EDR position queries (bilinear), EDR
area/radius (a CRS84 `Grid` over the polygon bbox at native resolution,
≤ 256 cells per axis, 1M-value budget, ONE `read_window_span` subset retrieval
per variable for the whole timestep span — two across the antimeridian —
cells outside the polygon masked to null — `QueryPolygon::sample_grid`,
#671; the NATIVE read is budgeted too via `Catalog::window_dims`, and at
most 8 variables per request since each is a sequential blocking read — up to 16 subsets across the antimeridian, each potentially many network reads; across the seam the two windows do not
bracket each other, so cells within half a native cell of ±180° on a
periodic store are nearest-only or null, and a native 0..360 longitude
axis is not normalised — position and area alike only answer requests in
the store's own frame — #667), WMS/Maps/Tiles rendering, CF time decoding, CF
packing, chunk LRU cache.
NOT yet: per-item-CRS STAC mode (Phase 4), kerchunk (Phase 5).

## The one load-bearing rule

**`concurrent_target(1)` is correctness, not tuning.** `zarrs` parallelises
multi-chunk retrieval with rayon by default. Those workers lose the calling
thread's request deadline and explicit runtime context; plain `ds-storage`
can fall back to constructing a runtime per call.
`catalog::single_threaded_opts()` pins retrieval to the calling thread via
`CodecOptions::with_concurrent_target(1)`. **Every `retrieve_*` call MUST go
through it.** Icechunk's explicit inner-chunk fan-out in `decoded.rs` uses a
separate, shared four-thread pool, propagates the absolute deadline to every
job, and admits all active decode workspaces before launch. Each individual
zarrs retrieval still uses target 1. Do not send plain `DsStore` reads to this
pool: they do not use the Icechunk runtime bridge.

## Architecture

- The Zarr format + codec pipeline is handled by the `zarrs` crate
  (blosc/zstd/gzip/crc32c/sharding/transpose + filesystem + ndarray). This
  engine only adds CF semantics, the OGC domain mapping, the storage bridge,
  and the poll-and-swap lifecycle. blosc/zstd build from C via `cmake`+`cc`.
- **Plain storage = ds-storage:** `src/store.rs` `DsStore`
  implements zarrs' `ReadableStorageTraits` + `ListableStorageTraits` over
  `ds_storage::DataStore`, with a `quick_cache` LRU of full chunk-object
  bytes (byte ranges served by slicing the cached buffer). Group/child
  discovery uses one-level delimiter listing (`DataStore::list_dir`), not a
  recursive chunk-key walk.
- **Catalog** (`catalog::build`): opens the root group, lists child arrays,
  treats 1-D arrays named after their dim as CF coordinate variables,
  classifies dims via `cf::classify_axis` (coord-var
  `standard_name`/`units` first, name heuristic second — projected metre
  axes resolve to `Other`, not degrees), validates lat/lon monotonic,
  exposes remaining geographic data variables as parameters. **A time axis
  is required** (PointSeries needs `t`). Unsupported-dtype variables are
  skipped at build with a WARN.
- **Rendering (Phase 3):** `get_raster_tile` reads a 2-D spatial window
  covering the bbox (`Catalog::read_window`, +1 cell margin), then samples
  per output pixel — per-pixel only for cheap `Wgs84`/`WebMercator`
  `project_node`; via `ProjectionGrid` for `Projected` output (#203).
  `raster_info()` is a cached `ArcSwap<RasterInfo>` rebuilt on catalog swap
  (#211). Window sampling uses `cf::locate` (ascending/descending/irregular
  axes); the window read inherits `concurrent_target(1)`.
- **Reads:** retrieve native `ArrayBytes`, then convert using the exact dtype.
  The read path branches on `data_type()` and widens every supported
  int/float to `f64`. Fill sentinels are compared against the RAW
  (pre-scale) value; NaN/±inf map to nodata.
- **Source memory admission:** `read_budget::BUDGET` is process-wide and
  survives reload/snapshot changes (`MC_ZARR_READ_MEMORY_MB`, default 1024 MiB).
  Reserve before variable payload reads: native subset + typed conversion +
  raw/physical f64 buffers + axes/window overhead + a four-buffer decode/index
  allowance. Use full stored chunk shapes, not clipped overlaps. Sharded reads
  account for full-shard fast paths when decoded-cache splitting is absent.
  `Window` owns an `Arc<Permit>` until the last window of its span is dropped;
  position reads hold it through interpolation. Admission fails immediately
  with `ResourceExhausted`, avoiding waits while holding executor slots or
  earlier windows. Never move the permit onto the HTTP waiter. The estimate
  excludes encoded objects, codec-private scratch, catalog metadata, caches,
  and API outputs. It is not an allocator-enforced memory ceiling. Icechunk
  admission reserves the largest decode workspace times the admitted fan-out
  (up to four touched inner chunks). Reduce fan-out as available memory falls;
  reject only when even one workspace plus the source buffers cannot fit.
  The returned permit carries the concurrency limit used by the reader.
- **Forecast axes / instances (#337):** with a CF `forecast_reference_time`
  axis AND a `forecast_period`/lead axis (e.g. dynamical.org AIFS/GFS/
  ICON-EU), every run on the reference axis is an EDR instance / WMS
  `DIM_REFERENCE_TIME` value (`Catalog::runs`, `RasterInfo.reference_times`);
  the latest run is the default and provides `Catalog::times`. Reads take the
  run (`Catalog::resolve_run(reference_time)` → reference-axis index) and
  each run's valid times are run + leads (`Catalog::valid_times`). The
  render path and both cache-key resolvers (`resolve_time`,
  `resolve_reference_time`) share that selection (#507/#521).
- **Bad-chunking WARN:** `time=1, lat=full, lon=full` chunking is
  pathological for point queries; use effective inner shapes for sharded
  arrays. Startup also logs outer/inner shapes, native bytes, and time steps
  per chunk to expose temporal decode amplification in map reads.

## APIs

The `engine_type → supported_apis` allowlist in `server/src/admin.rs` lists
`"zarr" => &["edr", "wms", "maps", "tiles"]`. WMS/Maps/Tiles need a `[wms]`
colormap (or a `style_bundle`) like the other raster engines; each variable
becomes its own layer via `register_parameter_layer_styles`.

## Config

`data_path` (local dir or `s3://`/`http(s)://` URL) XOR
`endpoint`+`bucket`+`path`; optional `path` sub-path, `zarr_version`
(advisory — zarrs auto-detects), `parameters` filter, `poll_interval_secs`
(default 300), `cache_mb` (default 256).

## Icechunk (feature `icechunk`, #335)

A `[collections.zarr.icechunk]` table makes the source a transactional
Icechunk repo (e.g. dynamical.org datasets). Off by default; the engine
errors clearly if the table is set without the feature.

- Repo location reuses `data_path`/`endpoint`+`bucket`+`path`; the table
  picks the version (`branch` HEAD, default `main`, or `tag`/`snapshot`).
- `src/store.rs` `EngineStore` is the backend-agnostic wrapper (catalog
  stays non-generic). `src/source.rs` separates the source from the session:
  the Icechunk source retains the repository/client and opens a pinned
  read-only session for each published catalog. `AsyncIcechunkStore` is
  bridged by the sync Store adapter through `runtime::run`, including full
  consumption of range streams within the same absolute request deadline.
  Background I/O has a 30-second per-operation timeout. The persistent
  runtime supports CLI, current-thread, and blocking-worker callers.
  Keep `concurrent_target(1)` within each explicitly admitted chunk job.
- **S3 backend = icechunk's `object_store` backend, NOT `aws-sdk-s3`** (deps
  use `default-features = false, features = ["object-store-s3",
  "object-store-fs"]`; saves ~20 MB binary).
- **Anonymous access is `S3Options::with_anonymous(true)`** — the
  object_store backend keys skip-signing off `S3Options.anonymous`, NOT the
  `S3Credentials` arg. Without it, it falls through to the AWS credential
  chain → EC2 IMDS and hangs off-EC2. Public datasets only.
- Icechunk owns its own object storage (does not go through ds-storage).
  `cache_mb` configures its compressed payload cache (0 disables retention).
  Repository clients and immutable payload caches survive snapshot changes.
- `src/decoded.rs` caches native inner chunks with `ds-cache`, keyed by
  snapshot ID, array path, and chunk coordinates. The engine owns one budget
  shared across all catalogs; old readers keep their snapshot identity.
  `icechunk.decoded_cache_mb` defaults to 256 MiB, separate from `cache_mb`;
  zero disables retention but keeps bounded inner-chunk retrieval. Plain
  mutable stores and shards with outer transforms bypass this reader. Cache
  fills expand to at most 64 MiB native chunks, and only if
  they fit the configured budget. This limits cache-fill expansion, not all
  codec/source memory. Reads process batches no larger than the admitted
  fan-out, bounding both queued work and completed chunks awaiting copying.
  A single-chunk batch stays on the caller; larger batches use the shared
  four-worker pool. Each worker installs/restores the request deadline and
  drives async storage through the persistent Icechunk runtime; decode runs
  off the I/O reactor. All jobs join on errors and unwinding before the
  caller can release its memory permit. Failed fills release single-flight
  guards; waits observe the
  request deadline (30 seconds without one). CF conversion stays in catalog.
  Expose hit/fill counters and resident/capacity bytes through
  `decoded_cache_metrics()` and per-collection `zarr_decoded_cache_*` metrics.
- Poll resolves the selected version and rebuilds only when its snapshot ID
  changes. Publish session + catalog atomically; failed builds leave the
  previous revision active and retry the next poll. Explicit snapshots stay
  pinned; tags resolve their tag target. In-flight readers retain their old
  snapshot. The snapshot ID also determines the nonzero render content version,
  so corrections at existing timestamps invalidate images and no-op polls do not.
- Windows hold compact f64 samples with NaN nodata. Conversion/reordering
  does not allocate an intermediate Option<f64> buffer.
- Tests: network-free e2e `cargo test -p engine-zarr --features icechunk`;
  live probe `… --test icechunk -- --ignored --nocapture probe_models`.
  `src/icechunk/tests.rs` uses non-inline compressed shards to test payload
  retention, deadlines, refresh failure/retry, pinned reads, and map sampling.
  Live cache timing: `cargo test -p engine-zarr --features icechunk map_cache_latency_probe -- --ignored --nocapture`.

## Fixture

`testdata/zarr-era5-t2m` (committed), regenerated by
`cargo run -p engine-zarr --example gen_fixture`. The field is linear in
lat/lon, so bilinear is exact and assertions are tight.
