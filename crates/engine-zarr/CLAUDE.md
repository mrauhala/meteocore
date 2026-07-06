# engine-zarr crate — Claude Instructions

Zarr V2/V3 multidimensional-array engine (cloud-native, CF conventions),
tracked in #125. Phases 1–3 ship today: local + remote (S3/HTTP) stores,
WGS84 lat-lon grids, multi-variable EDR position queries (bilinear),
WMS/Maps/Tiles rendering, CF time decoding, CF packing, chunk LRU cache.
NOT yet: per-item-CRS STAC mode (Phase 4), kerchunk (Phase 5), EDR
instances (#337 — the engine pins the latest run internally).

## The one load-bearing rule

**`concurrent_target(1)` is correctness, not tuning.** `zarrs` parallelises
multi-chunk retrieval with rayon by default and would call the storage layer
from rayon workers — where `ds-storage`'s `block_in_place` bridge PANICS.
`catalog::single_threaded_opts()` pins retrieval to the calling thread via
`CodecOptions::with_concurrent_target(1)`. **Every `retrieve_*` call MUST go
through it.**

## Architecture

- The Zarr format + codec pipeline is handled by the `zarrs` crate
  (blosc/zstd/gzip/crc32c/sharding/transpose + filesystem + ndarray). This
  engine only adds CF semantics, the OGC domain mapping, the storage bridge,
  and the poll-and-swap lifecycle. blosc/zstd build from C via `cmake`+`cc`.
- **Storage = ds-storage for every backend:** `src/store.rs` `DsStore`
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
- **Reads:** `retrieve_array_subset_opt::<Vec<T>>` requires the exact dtype,
  so the read path branches on `data_type()` and widens every supported
  int/float to `f64`. Fill sentinels are compared against the RAW
  (pre-scale) value; NaN/±inf map to nodata.
- **Forecast axes:** with a CF `forecast_reference_time` axis AND a
  `forecast_period`/lead axis (e.g. dynamical.org AIFS/GFS/ICON-EU), the
  engine uses the latest run and exposes valid time = run + lead as the time
  axis (`cf::parse_duration_seconds` decodes the lead units).
- **Bad-chunking WARN:** `time=1, lat=full, lon=full` chunking is
  pathological for point queries; logged at startup, still served.

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
  stays non-generic); `src/icechunk.rs` opens repo → read-only session →
  `AsyncIcechunkStore` → `AsyncToSyncStorageAdapter` (its `block_on` mirrors
  ds-storage; safe because retrieval is `concurrent_target(1)`).
- **S3 backend = icechunk's `object_store` backend, NOT `aws-sdk-s3`** (deps
  use `default-features = false, features = ["object-store-s3",
  "object-store-fs"]`; saves ~20 MB binary). `icechunk` still needs one
  interim `[patch.crates-io]` in root `Cargo.toml` (#340).
- **Anonymous access is `S3Options::with_anonymous(true)`** — the
  object_store backend keys skip-signing off `S3Options.anonymous`, NOT the
  `S3Credentials` arg. Without it, it falls through to the AWS credential
  chain → EC2 IMDS and hangs off-EC2. Public datasets only.
- Icechunk owns its own object storage (does not go through ds-storage).
  New snapshots are picked up on **reload**, not poll (v1).
- Tests: network-free e2e `cargo test -p engine-zarr --features icechunk`;
  live probe `… --test icechunk -- --ignored --nocapture probe_models`.

## Fixture

`testdata/zarr-era5-t2m` (committed), regenerated by
`cargo run -p engine-zarr --example gen_fixture`. The field is linear in
lat/lon, so bilinear is exact and assertions are tight.
