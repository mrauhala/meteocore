# MeteoCore — Claude Instructions

MeteoCore is a Rust workspace implementing OGC API - EDR, OGC API - Features,
OGC API - Maps, OGC API - Tiles, OGC WMS 1.3.0, and OGC 3D Tiles servers for
weather data (radar, NWP models, observations, alerts).

Crates: `ds-core` (traits + types + shared utilities, directory `crates/core`),
`ds-storage` (S3/HTTP/local object store, directory `crates/storage`; its
`discovery` module is the one home for time windows, strftime prefix
expansion, the filename → timestamp (and `{lead}`) matcher, the catalog scan
with bounded concurrent prefix LISTs (#816/#817), listing-free template
probing (`TemplateProber`: `{run}`/`{lead}` keys, sentinel HEADs, settling,
per-run publication) and the shared `PendingFile` settling rule (#1067);
`DataStore` adds budgeted and `If-Unmodified-Since` range reads and
per-host request limits for files read in place; every remote operation
runs in a `storage{collection, store, path}` span, and an engine names its
collection with `DataStore::for_collection(id)` where it builds the store,
#1233; its `throttle` module is the one home for HTTP host back-off on
429: `Retry-After` and the adaptive host limit, #1235; THREDDS catalog
discovery is `discovery::thredds`, #1170, and a generated HTTP document
without `Content-Length`, such as a THREDDS catalog, is read with
`DataStore::get_documents`, never a second client: object_store's GET fails
a chunked response),
`ds-render` (raster colorization + PNG encoding, directory `crates/render`),
`ds-cache` (shared byte-bounded LRU cache plumbing),
`ds-executor` (shared Tokio render admission/deadline execution and the one
query runtime EDR and WMS GetFeatureInfo share; API-facing infrastructure,
plus `budget::ByteBudget`, the fail-fast byte budget each engine's source-read
admission is an instance of — never another copy, #1067 — and
`field_jobs::run_field_jobs`, the bounded field-read scheduler of an EDR
query that GRIB and NetCDF share),
`ds-poll` (shared poll-loop lifecycle: the `Shutdown` handle + `PollTicker`
every engine's background poll loop uses — never hand-roll a
`tokio::select!` shutdown signal, #481),
`ds-wis2` (WMO WIS2 consumer client: Global Broker MQTT subscription,
notification parsing, dedup, payload download — shared by every engine
with a `[….wis2]` source; see `crates/ds-wis2/CLAUDE.md`),
`ds-bufr` (BUFR decoder, vendored from tinybufr; compressed strings, operator 208,
64-bit numerics; see `crates/ds-bufr/CLAUDE.md`),
`ds-mvt` (Mapbox Vector Tile encoder + LRU tile cache), `ds-3dtiles`
(OGC 3D Tiles encoder), engines (`engine-csv`, `engine-geojson`,
`engine-geotiff`, `engine-grib`, `engine-odim`, `engine-querydata`,
`engine-zarr`, `engine-postgis`, `engine-cap`, `engine-bufr`, `engine-satellite`,
`engine-netcdf`, `engine-station-grid` — station observations gridded on the
fly, a derived collection over a station collection), API layers
(`api-common` shared HTTP plumbing, `api-edr`,
`api-features`, `api-maps`, `api-tiles`, `api-wms`, `api-3dtiles`,
`api-mcp` — Model Context Protocol tools over the storm-cell surface), and
`server` (the binary).

**Per-crate notes live in `crates/<crate>/CLAUDE.md`.** Before working on one
of these crates, read its file — it holds that crate's rules and gotchas:

- `crates/ds-executor/CLAUDE.md` — bounded render queue, deadlines, worker permit ownership,
  the shared query runtime, the engines' fail-fast `ByteBudget`.
- `crates/server/CLAUDE.md` — CLI flags, no-config boot, auto-collections,
  reload/watcher trust model, proxy headers, the container contract the
  Helm chart (`charts/meteocore`) relies on.
- `crates/api-common/CLAUDE.md` — shared discovery validation, metadata/navigation, OpenAPI and conformance.
- `crates/api-wms/CLAUDE.md` — BBOX axis order, meta-tiling, TIME/ELEVATION/
  reference_time dimensions.
- `crates/api-edr/CLAUDE.md` — CoverageJSON schema compliance, domain types,
  instances. `crates/api-edr/README.md` is the EDR support-status page
  (conformance classes, query types, per-engine matrix) — update it in
  every PR that touches EDR behaviour, including engine `EdrEngine` impls.
- `crates/api-features/CLAUDE.md` — sortby/paging rules, caching twin.
  `crates/api-features/README.md` is the Features support-status page
  (conformance classes, `/items` parameters, per-engine matrix) — update it
  in every PR that touches Features behaviour, including engine
  `FeatureEngine` impls.
- `crates/api-3dtiles/CLAUDE.md` — routes, representations, caching, viewer.
- `crates/api-mcp/CLAUDE.md` — MCP tools, the auth boundary, why the tool
  set is restricted to nowcast collections.
- `crates/ds-3dtiles/CLAUDE.md` — .pnts / isosurface / echo-top / voxel
  encoders and their CesiumJS gotchas.
- `crates/ds-wis2/CLAUDE.md` — WIS2 broker/session rules, the 6×-per-cache
  duplicate fact, inline payloads, download policy.
- `crates/engine-odim/CLAUDE.md` — PVOL per-site model, pixel pre-warm,
  resampling, storm cells.
- `crates/ds-bufr/CLAUDE.md` — decoder provenance, format boundaries, independent fixtures.
- `crates/engine-bufr/CLAUDE.md` — BUFR decoder boundary (ds_bufr only in
  `decode.rs`), period-context + first-occurrence extraction rules, store
  bounds, EDR/Features semantics.
- `crates/engine-geotiff/CLAUDE.md`, `crates/engine-grib/CLAUDE.md`,
  `crates/engine-querydata/CLAUDE.md`, `crates/engine-zarr/CLAUDE.md`,
  `crates/engine-cap/CLAUDE.md`, `crates/engine-postgis/CLAUDE.md`,
  `crates/engine-nowcast/CLAUDE.md`, `crates/engine-satellite/CLAUDE.md`,
  `crates/engine-netcdf/CLAUDE.md`, `crates/engine-station-grid/CLAUDE.md` —
  one file per engine.

This root file holds only workspace-wide rules.

## Critical Rules

Violating these has caused production incidents. Never break them.

1. **Never commit directly to `main`.** Create a branch, open a PR.
2. **Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before
   committing.** CI rejects both formatting drift and clippy warnings.
3. **Never build XML with `format!()` or string concatenation** — XML
   injection risk. All XML output uses `quick-xml::Writer` (api-wms).
   `scripts/check_geo_safety.sh` enforces this in CI.
4. **Never re-implement EPSG:3857 ↔ WGS84 math.** Use `ds_core::web_mercator`
   (`lon_to_x` / `x_to_lon` / `lat_to_y` / `y_to_lat`, `EARTH_RADIUS`,
   `LAT_LIMIT_DEG`). Four hand-rolled copies once drifted and displaced data
   ~10° at low zoom (#452, fixed by consolidation in #454).
   `scripts/check_geo_safety.sh` enforces this in CI (magic constants have
   named homes too: `web_mercator::{EARTH_RADIUS, LAT_LIMIT_DEG}`,
   `geo::WGS84_A`).
   - Never clamp latitude when converting a viewport/bbox — zoomed-out
     requests legitimately reach past ±85°, and the client maps the returned
     image over the full requested extent.
   - Clamp to `LAT_LIMIT_DEG` only when selecting tile-grid indices.
   - Geographic (`OutputCrs::Wgs84`) direct renders of a client-chosen bbox go
     through `ds_core::map_engine::get_raster_tile_in_domain` /
     `get_raster_tiles_in_domain`, which cut the output rows centred past ±90°
     (nodata there) and never clamp. An engine asked for those rows samples its
     edge cells instead (#1065). For a projected box wholly outside its
     projection (`ds_core::geo::outside_projection_domain`) they return
     `None` without calling the engine (#1089). A single point, such as a
     WMS GetFeatureInfo pixel centre, goes through
     `OutputCrs::position_on_earth`, which tests the point itself (#1064).
5. **Never project per output pixel in a render path.** The CRS forward
   transform dominates render cost; per-pixel projection was a ~10×
   regression (#203). Evaluate the projection on a coarse `ProjectionGrid`
   and interpolate the output→source pixel map, or project per-vertex for
   vector geometry.
6. **Engine poll/scan loops run on the dedicated background runtime**
   (`poll_runtime()` in `server/src/main.rs`), never on the request-serving
   runtime (#221). Use explicit-runtime `ds-storage` APIs for blocking-pool
   work (see rule 7).
7. **`ds-storage` (`DataStore`) is a sync bridge over async object_store**
   (`block_in_place(|| handle.block_on(..))`). Keep its runtime context
   explicit:
   - Call it from the background poll runtime (or another dedicated runtime).
     A request's engine *query* runs as a task on the one shared query
     runtime, `ds_executor::query_runtime()` (threads `mc-query`, #1103):
     api-edr's executor, and `ds_executor::RenderJob::run_query` for WMS
     GetFeatureInfo (#1064). Each keeps its own admission and deadline.
   - Never from a request-handler task — it parks a request worker.
   - On `spawn_blocking` workers, use its explicit-runtime APIs (`get_on`,
     `get_range_on`, `get_range_on_with_budget`,
     `get_range_if_unmodified_on`). Do not rely on an ambient runtime handle.
     To overlap whole-object reads from synchronous code, start them with
     `DataStore::start_get_or_refusal` and collect each with
     `PendingGet::wait(Some(handle))`, bounding how many you start (#999).
     Several threads may wait for one read behind an `Arc` (#1127): each
     wait is bounded by its own thread's deadline, and the last holder
     dropping the read aborts it.
     The plain-Zarr adapter is a tested exception: the same synchronous
     `get_opt`/cache-fill path serves EDR async workers and `RenderJob` blocking
     workers. Its `block_in_place` bridge releases an async worker and runs
     directly on a blocking worker, retaining that worker's runtime handle.
     Keep both async-worker progress and actual render-executor regressions
     in `crates/engine-zarr/src/store/coalescing_tests.rs` and `refresh_tests.rs`.
     engine-satellite's evicted-scan fetch is the same exception (EDR on
     async workers, renders on blocking ones): `tests/refetch.rs`.
     So are engine-netcdf's remote files (#1067): hdf5-reader's synchronous
     storage callback crosses into the engine's persistent I/O runtime
     (`crates/engine-netcdf/src/io.rs`: `block_in_place` on a worker,
     directly on a blocking or plain thread, a scoped thread on a
     current-thread runtime), with the deadline captured on the calling
     thread. Ingest on a poll-runtime worker, renders on `RenderJob` blocking
     workers and on async workers are covered by
     `remote::tests::reads_run_on_the_poll_runtime_render_jobs_and_async_workers`
     and `io::tests::every_calling_thread_reaches_the_runtime`; EDR queries
     on the query runtime's workers and GetFeatureInfo's `run_query` by
     `remote::tests::queries_read_on_the_query_runtime_for_edr_and_get_feature_info`.
   - Never from a non-Tokio thread such as a rayon pool — that hits a
     construct-a-new-`Runtime`-per-call fallback (#222). For parallel remote
     fetches, use async concurrency (`join_all`) on the runtime, or pass a
     `Handle` into the worker.
   - `block_in_place` panics inside current-thread async execution or a
     `LocalSet`; it does **not** inherently panic inside `spawn_blocking`.
     Icechunk uses its own persistent I/O runtime and supports `RenderJob`
     blocking workers; concurrent shard reads through that executor are
     covered in `crates/engine-zarr/src/icechunk/tests.rs`.
   - A whole-object download on the poll runtime passes
     `FetchBudget::Background` (`get_with_budget` / `get_many_with_budget`,
     120 s), so object_store's own retry of a stalled 30 s attempt can run.
     Anything a request can wait on, a single-flight cache fill included,
     keeps `FetchBudget::Request` (30 s or the render deadline). See
     `crates/storage/README.md` (#1011). A new budgeted call gets a test over
     `ds_storage::test_util::{StalledBody, run_stalled}` (feature `test-util`,
     dev-dependencies only) that fails when its budget is swapped (#1055).
   - Never wait on a runtime worker for work that needs that runtime: a
     `std` lock or condvar, or a single-flight cache wait (`quick_cache`
     placeholders park the thread), whose holder downloads through this
     bridge or a task on the same runtime. Waiters parked on every worker
     leave nothing to run that task or drive its I/O and the deadline
     timers, and the runtime stalls for good (#1200). Take the lock if it is
     free, else wait inside `block_in_place`, until the request deadline:
     engine-odim's `PixelCache::with_file_load`, GRIB's field jobs.
8. **SQL safety:** every identifier goes through `quote_ident`, every value
   is a `$N` bind, no SQL text inside `format!()`.
   `scripts/check_sql_safety.sh` enforces this in CI.
9. **Never loop blocking I/O sequentially on one thread.** N sequential
   blocking network calls multiply the stall (the GRIB new-run probe once did
   32 ≈ seconds). Batch, bound the parallelism, or cap per cycle.
10. **Capability/metadata accessors used per request must be O(1)** from a
    snapshot (`ArcSwap`/`RwLock` read) — never recompute or allocate per call
    (#211).
11. **Do not leak internal error details to clients.** Generic messages for
    500 errors.
12. **After modifying CoverageJSON output (`api-edr/src/response.rs`), run
    `cargo test -p api-edr`** — all CoverageJSON must validate against the
    OGC schema (`schemas/coveragejson.json`).

## Build & Run

```bash
cargo build                  # Build all crates
cargo test                   # Run all tests
cargo run -p server          # Start server (reads config.toml)
cargo check -p <crate>       # Type-check a single crate
```

Server CLI flags, no-config boot, and `--auto-collections` are documented in
`crates/server/CLAUDE.md`.

### Fuzz testing

Fuzz targets live in `fuzz/` (separate from the workspace; `cargo-fuzz` +
libfuzzer; requires nightly):

```bash
cargo install cargo-fuzz                                          # One-time setup
cargo +nightly fuzz run fuzz_tiff_metadata -- -max_total_time=60  # Fuzz TIFF parser
cargo +nightly fuzz run fuzz_geo_transform -- -max_total_time=60  # Fuzz CRS transforms
```

### Dependency security (RustSec)

`cargo audit` is a **CI gate** — the `audit` job in `.github/workflows/ci.yml`
(#584). It runs on PRs, on `main`, and on the weekly `schedule`, so an
advisory published against a crate we already ship turns CI red with no code
change; on `main` it also blocks the Docker publish.

The policy lives in `.cargo/audit.toml` rather than in workflow flags, so a
local run gives the identical verdict:

```bash
cargo install cargo-audit --locked   # one-time
cargo audit                          # exactly what CI enforces
```

- `deny = ["warnings"]` — unmaintained / unsound / yanked crates fail the
  gate too, not just vulnerabilities. Letting those accumulate is how #584
  reached 11 vulnerabilities before anyone looked.
- Fix by bumping: `cargo update` for transitive crates, a manifest bump for
  direct ones. Most of #584 was `cargo update` alone.
- Adding an `ignore` entry is the last resort. Each one needs the reachability
  analysis *and* a tracking issue written in the comment above it (#594).
- CI additionally runs `cargo metadata --locked` — a Cargo.lock that disagrees
  with the manifests would make the audit meaningless, since it reads the lock.
- Dependabot (`.github/dependabot.yml`) proposes weekly bumps for direct
  crates and for GitHub Actions; purely transitive crates are refreshed by
  `cargo update`, prompted by the scheduled audit run.

### Licences (#1081)

MeteoCore is `MIT OR Apache-2.0` (`LICENSE-MIT`, `LICENSE-APACHE`). Every
member inherits `[workspace.package] license` with `license.workspace = true`;
a new crate without it fails CI. `crates/ds-bufr` keeps its own tinybufr
declaration and licence files.

- `deny.toml` is the dependency licence allow-list. The `licenses` CI job runs
  `cargo deny --locked check licenses` and gates the Docker publish like
  `audit`; advisories stay with `cargo audit`.
- `THIRD-PARTY-NOTICES.txt` is generated, never committed:
  `scripts/third_party_notices.sh` (cargo-about 0.9.2, `about.toml`,
  `third-party/notices.hbs`) runs in the Dockerfile's `notices` stage and in
  `build-binaries.yml`. A licence added to `deny.toml` usually belongs in
  `about.toml`'s `accepted` too.
- The script fails the build when a crate that compiles C/C++ with `cc` or
  `cmake` is not classified in its `bundled_licences`, or a listed licence
  file has moved. Add the bundled library's own licence file there; the
  crate's declared licence covers only its Rust code. blosc-src ships no
  licence files, so `third-party/c-blosc.txt` holds C-Blosc's, copied from
  the release blosc-src bundles; the script fails when that release changes.

### CodeQL scanning

`.github/workflows/codeql.yml` scans Rust, Actions, JavaScript/TypeScript, and
Python with `security-extended` on PRs to `main`, main pushes, and weekly.
Rust uses `build-mode: none` and the extractor's default all-features selection,
including Icechunk. Inspect extraction warnings when assessing coverage.
Results live in GitHub's Security → Code scanning tab. Triage the initial
baseline before configuring required merge checks; keep the RustSec audit gate
for dependency advisories. Dependabot updates the pinned CodeQL action.

## Project Tracking

Backlog is in GitHub Issues: https://github.com/mrauhala/meteocore/issues

- Labels: `priority: high|medium|low`; `effort: tiny|small|medium|large`;
  type labels (`bug`, `enhancement`, `security`, `performance`, `reliability`,
  `architecture`, `operational`, `spec-compliance`); `epic` for parent issues
  with task lists.
- Milestones: **v0.2** (radar engines + rendering fixes), **v0.3** (QueryData
  improvements + multi-band GeoTIFF), **v1.0** (spec compliance + production
  hardening).
- When completing work, close the issue: `gh issue close <number>`.
- Batch runs over the backlog (pick → worktree per issue → PR → review →
  self-merge, N at a time, filtered by focus/priority/effort) follow the
  `/issue-batch` skill in `.claude/skills/issue-batch/`.

```bash
gh issue list                              # All open issues
gh issue list -l "priority: high"          # High priority only
gh issue list --milestone "v0.2"           # Issues in v0.2 milestone
gh issue create --title "..." --label "bug,priority: high" --milestone "v0.2"
```

## Architecture Rules

- **Four core traits:** `EdrEngine` (EDR), `FeatureEngine` (Features),
  `MapEngine` (Maps/WMS/Tiles), `VolumeEngine` (3D Tiles — volumetric point
  clouds). They are separate traits — not every engine supports every API.
  Engines return domain types, never JSON/XML; serialization belongs in the
  API crates.
- **`ds-core` has no framework dependencies** (only chrono, serde, thiserror,
  toml). Keep it that way. Use the `PropertyValue` enum, not
  `serde_json::Value`, for feature properties. CRS math and `GeoTransform`
  live in `ds_core::geo`, shared by all engines.
- **`ds-render` has no framework dependencies** (no axum/tokio/http —
  encoders and codecs only: `png`, `jpeg-encoder`, `webp`, plus
  `serde_json` for the shared machine-readable legend document builder
  `legend_json`, kept in ds-render so WMS/Maps/Tiles can't drift apart).
  RGB composites live in `ds_render::composite` (#819): per-channel range
  with `min > max` = inverted, EUMETSAT gamma, band differences, and
  colour-bar-free legends (`composite_legend_json`, `render_composite_legend`).
  Map backgrounds live in `ds_render::background` (#894): `Background`
  (a fill and a void `Fill`) is `CacheKey.background`; WMS converts its
  `Option<[u8; 3]>` with `.into()`, Maps and map tiles parse `bgcolor`,
  `transparent`, `void-color`, `void-transparent` with `parse_background`
  and encode with `encode_map`/`empty_map`. The void (pixels off the CRS)
  is `OutputCrs::earth_columns`, computed only when the void fill differs.
  `ds-mvt` and `ds-3dtiles` are likewise framework-free byte encoders,
  mirroring `ds-render`.
- **Byte-bounded LRU caches go through `ds-cache`** (#480):
  `ds_cache::ByteBoundedCache` owns the weigher/env-parse/hit-miss-counter/
  metrics plumbing (including single-flight `get_or_insert_with`, and
  `get_or_join`, which reads a resident value or joins a fill in flight
  without ever inserting: a reader that must not evict, #1297); call
  sites keep their key type, weight fn, `MC_*_CACHE_MB` env var name and
  defaults. Don't hand-roll a `quick_cache` byte-weighted cache — before
  extraction the same ~40 lines were copy-pasted 12×. In `server/src/
  admin.rs`, a global cache's `/metrics` family is one `CacheMetricSet`
  static + one `update()` call in `metrics_handler`. The budget is split
  into equal per-shard slices, so entries that fit the total can still be
  evicted: a caller that plans what stays resident uses
  `new_single_shard` / `from_env_single_shard` (engine-satellite's scans,
  the GRIB grid and message caches) and compares against
  `shard_capacity_bytes`, never `capacity_bytes`.
  quick_cache keeps 97 % of a shard for entries inserted first or read, so
  unread fresh inserts share 3 %: a cache filled ahead of reads (a
  pre-warm) uses `new_with_hot_allocation` (engine-odim's `PIXEL_CACHE`,
  0.5, #992). The hot share also caps the largest entry a shard admits.
  A single-flight fill links its entry into that 3 % before evicting, so a
  fill heavier than it evicts itself unread: a one-shard cache whose
  entries can be that large uses `new_single_shard_with_hot_allocation(..,
  hot_allocation_holding(capacity, largest))` (the GRIB caches, #1332). A
  caller that will read a fill again but samples its own copy now `keep`s
  it, or the fills after it push it out of the cold ring (#1340).
  Every cache and admission budget is entered in the memory plan,
  `ds_cache::plan` (#1306), under its `/metrics` family as id
  (`metatile_cache`, `geotiff_decode`), a scope and its size source. A
  process-wide cache is built from `plan::env_bytes(VAR, default)` and
  `.registered(id, Scope::Engine(..), source)`; a `ByteBudget` or
  `RenderBudget` calls `.register`; per-collection caches are entered in
  `server/src/cache_plan.rs`. `cache_plan::tests::
  every_cache_and_budget_on_metrics_is_in_the_plan` fails when a
  `*_cache_bytes` or `*_capacity_bytes` family has no entry: a new engine
  type adds its fixture collection there.
  An engine whose data fixes what a cache must hold (a time window, a
  pre-warm set, a grid) publishes it as a `plan::CacheDemand` through its
  own `plan::DemandSlot`, under the cache id and scope the plan lists the
  cache by, computed where those facts change (poll, catalog refresh),
  never per request, weighing entries with the cache's own weight function
  (#1309); `.ahead()` marks a cache the engine fills ahead of requests.
  The plan sums a cache's slots, suggests demand + 15 % (a floor as is),
  and returns a `Shortfall` for the engine to log, with hysteresis: again
  only on a size change or 15 % more demand, forgotten under 90 % of the
  size. A collection's or source's slot checks only the cache entry its
  own engine registered after it, never a replaced engine's;
  `cache_plan::snapshot` covers the rest. A disabled cache (0) is never
  short. Never hand-roll a per-engine "cache too small" WARN.
  Every cache counts churn (#1308, `ds_cache::Churn`) in its `quick_cache`
  `Lifecycle`: evictions by ring, admitted and evicted bytes, refusals
  (heavier than a shard's hot target, not evictions) and bypasses. The hooks
  only move entries into the request state; counting happens after the shard
  lock is released, so never weigh, hash or lock under it. `with_ghosts()`
  adds an opt-in ring of evicted key hashes (2 × capacity, 16 stripes, about
  10–20 ns per miss) for ghost hits at 0.5, 1 and 2 × capacity; count such a
  cache's misses with `record_miss_for(key)`. `prewarmed()` marks a cache
  filled ahead of its reads.
  Every cache resizes in place (`set_capacity`, #1311), keeping what fits,
  and returns the capacity it applied. A shrink goes in steps of at most a
  tenth of the capacity. quick_cache calls `end_request` under the shard
  lock; during a resize it only parks the evicted entries, and the resize
  counts them (`record_evictions`) and drops them after quick_cache
  returns, as an insert does. Never count or drop in `end_request`. An
  insert or fill weighed while the cache was disabled counts nothing.
  A resize keeps the shard count and the hot share the cache was built
  with; a growth from 0 takes quick_cache's 0.97 (it resets a share only
  from 0, only to its default). So a shrink stops where each shard still
  holds 32 estimated entries, and `resizes_in_place_to(bytes)` compares the
  shards with a cache built at that size without a ceiling: a shrink is in
  place with no more of them, a growth with at least a quarter. A
  `hot_allocation_holding` cache also checks `holding_keeps`. A caller that
  can, builds a new cache otherwise. Build every cache at its configured
  size: `new_resizable(.., ceiling, ..)` counts shards from a ceiling, and
  only shrinks slices; it is for the computed plan (#1312), unused today.
  `with_entry_floor()` (satellite scans, station grids) never shrinks below
  4 × the heaviest entry admitted. `ByteBudget::resize` /
  `RenderBudget::resize` resize the admission budgets; the latter wakes its
  waiters, and a waiter that no longer fits is shed as oversize. A
  process-wide static has a `resize_*` next to its metrics accessor, which
  returns the applied size; nothing calls them until their sizes come from
  config.
- **API crates depend on ds-core and api-common** (shared Common HTTP glue),
  plus ds-executor for bounded render execution and the shared query runtime,
  ds-render for
  api-wms/api-maps, and api-edr for its `f=png` time-series plots — never on
  engine crates. The one API-to-API edge is api-edr → api-features: EDR
  `items` is the Features encoding and paging over the collection's
  `FeatureEngine` (#928). API state is a registry of engines keyed by
  collection ID.
- **The shared OGC API root at `/` (#789)** composes OGC API standards as
  `api_common::shared::BuildingBlock`s — Maps, Tiles and Features — over one
  landing page, conformance, OpenAPI document and collection catalog. Blocks
  reuse their per-API service's state (no second registry). Every standard is
  also its own per-API service with its own base route (`/edr/...`,
  `/features/...`, `/maps/...`, `/tiles/...`, `/wms/...`) — the Maps, Tiles
  and Features ones stay routed but are no longer advertised on the landing
  page or workspace switcher; build Maps, Tiles
  and Features links from the router's `Mount`, never a hard-coded prefix.
  EDR stays at `/edr` only (EDR 1.1 requires every `/collections` entry to be
  an EDR collection; #789 Phase 3). See
  `crates/api-common/CLAUDE.md` for the composition rules.
- **Collection routing is dynamic.** Handlers look up engines from a
  `HashMap<String, Arc<dyn …Engine>>` by collection ID from the URL path.
  Never hardcode collection IDs.
- **The `apis` config field is enforced.** A collection is wired to an API's
  router only if that API is listed in its `apis` array.
- **Tiles reuses `MapEngine`.** Tile z/x/y → bbox via TileMatrixSet math,
  then `MapEngine::get_raster_tile()`. No separate tile engine trait.
- **CORS is applied at the server level** (`CorsLayer` in
  `server/src/main.rs`), not in individual API crates.
- **Crate name gotcha:** the core crate is named `ds-core` in Cargo.toml
  (imported as `ds_core`); its directory is `crates/core/`. It was renamed
  from `core` because shadowing Rust's built-in `core` crate breaks proc
  macros like `#[tokio::main]`.

### Adding a new engine

1. Create `crates/engine-<name>/` with a `Cargo.toml` depending on `ds-core`,
   with `license.workspace = true` in its `[package]`.
2. Implement `EdrEngine`, `FeatureEngine`, `MapEngine`, and/or `VolumeEngine`.
3. Add the crate to workspace members in root `Cargo.toml`.
4. Add it as a dependency of `crates/server/Cargo.toml`.
5. Add a match arm for the new `engine_type` in `server/src/main.rs` /
   `server/src/admin.rs`, including the `engine_type → supported_apis`
   allowlist.
6. Wire it into the appropriate registries per the collection's `apis`.
7. Obey the Performance & Concurrency rules — especially: spawn the poll loop
   on the background runtime, and never project per output pixel.
   **A `MapEngine` returns `Err` for a deadline or a failed read** —
   `DeadlineExceeded`, or `ResourceExhausted` (503) for a storage failure
   that outlived its retries, with the detail logged — never an `Ok` tile
   with nodata in its place: the rendered and meta-tile caches keep every
   `Ok` tile with no TTL, so a gap from a short outage would outlive it
   (#993, #1051). Only a tile or field the source itself declares empty
   (a COG tile with `byte_count == 0`) or bytes that do not decode are
   nodata. A missing object is engine-specific: engine-odim draws a typed
   `NotFound` as nodata (#993/#996), GeoTIFF answers 503 until the next scan
   drops the entry.
   **If the engine snaps a requested time to an available timestep**
   (latest-not-after, nearest, …) instead of exact-matching, it MUST
   override `MapEngine::resolve_time` with the SAME selection logic
   `get_raster_tile` uses (share one helper so they cannot drift) — the API
   layers key the no-TTL rendered/meta-tile caches on it. Skipping this
   reintroduces the #507 cache poisoning (stale animation frames).
   **If the engine's parameters have different time axes** (a satellite
   collection whose products land minutes apart), keep `RasterInfo.times`
   the union and override `MapEngine::parameter_times` and
   `resolve_parameter_time` with the SAME per-parameter selection
   `get_raster_tile` uses. The API layers then advertise and default each
   parameter's own axis (WMS child-layer `time` dimension, Maps/Tiles
   `datetime`) and key the caches on its resolved timestep (#819).
   Such an engine MUST also override `MapEngine::get_raster_tiles`, the
   multi-band request an RGB composite renders: every band comes from the
   one timestep `resolve_parameters_time` returns, or the latest shared one
   for `None`, and a band without data then is an error, never a snap to
   another timestep. Its default `resolve_parameters_time` is
   `ds_core::map_engine::select_common_time` over the parameter axes:
   latest-not-after among the timesteps every band has. Override it too if
   the engine's own selection differs, and key a composite's caches on it.
   Where it can, `get_raster_tiles` builds the output→source coordinate
   map once per distinct source grid (engine-satellite, `render.rs`).
   An engine serving composites as layers (`MapEngine::composites`) answers
   for a composite's name in `parameter_times` (the timesteps every band
   has, kept in the snapshot, never intersected per call) and
   `resolve_parameter_time` (through the same helper as
   `resolve_parameters_time` over its bands). The API layers then serve
   each composite as its own layer with nothing more from the engine: a
   WMS child layer `coll/<composite>` with that `time` dimension, Maps/Tiles
   `parameter-name=<composite>` (listed in `parameter_names`, no unit), the
   `default` style only (`ds_render::COMPOSITE_STYLE`), and a channel-list
   legend. They key every cache on the composite's name and its resolved
   time, pass that same time to `get_raster_tiles`, compose with
   `ds_render::render_composite_tiles` (meta-tiles:
   `render_metatiled_composite`; WMS draws their RGBA forms,
   `render_composite_tiles_rgba` and `assemble_metatiled_composite`, and
   encodes once with the other layers), and render nothing when it is
   `None` (no shared scan).
   **If the engine retains model runs** (non-empty
   `RasterInfo.reference_times`), it MUST likewise override
   `MapEngine::resolve_reference_time` with the SAME run selection
   `get_raster_tile` uses (`None` ⇒ the concrete run it would render,
   including any cross-run fallback) — the caches key the run axis on it
   (#521). Skipping this freezes the first-rendered run's pixels when a
   newer run re-covers the same valid times. If the run also depends on
   the parameter (GRIB: an hour-window aggregate the newest run lacks at
   that time renders from an older run, #1005), override
   `resolve_parameter_reference_time` with that selection too: the API
   layers resolve the run with it, then pass that run to
   `resolve_parameter_time` and the render. Likewise
   `resolve_parameters_reference_time`, the run several parameters render
   together from (`DerivedWind`'s u and v; GRIB, #1026).
   **If the engine's content for a fixed `(time, reference_time)` can be
   revised in place** (a push-fed alert set: a warning published later is
   active at instants already rendered), it MUST override
   `MapEngine::content_version` with a value that moves with the content
   (engine-cap returns its `data_version`) — the no-TTL rendered and
   meta-tile caches key on it, through `MapEngine::content_version_for`
   (default: `content_version()`), and explicit-`TIME` responses then carry
   a short, revalidating `Cache-Control` instead of `immutable`.
   Immutable-timestep engines keep the default `0`. **If that value moves
   with every catalog change while most frames keep their pixels**, also
   override `content_version_for(parameter, time, reference_time, z)` with
   one frame's version, selected through the SAME helpers `get_raster_tile`
   uses (GRIB: `select_run_step` + `map_message`, hashing the drawn
   message's origin, offset and length, #1334) — otherwise every scan
   orphans every cached frame. A wrapper that forwards `MapEngine` methods
   forwards it too; a derived layer combines its inputs' versions
   (`ds_core::derive`).
   **If a parameter is derived from two steps** (engine-netcdf's per-step
   amount: the step and the run's step before, #1165), `resolve_time` /
   `resolve_reference_time` name only the step drawn, so the engine MUST
   move `content_version` when the other step changes under the same
   (time, run), such as a lead filled in inside a run.
   **If the engine's map layers have levels** (`RasterInfo.vertical`) and
   it draws another level than the requested `z`, or than the first
   advertised level without one, it MUST override
   `MapEngine::resolve_elevation` with the SAME selection
   `get_raster_tile` uses (GRIB: the run's own first level; ODIM PVOL: the
   nearest sweep carrying the quantity). WMS GetFeatureInfo passes that
   level to the position query as `z` (#1064).
   **If a map layer is an overlay** whose values the engine's EDR position
   query does not answer (ODIM `CELLS`), override
   `MapEngine::layer_shows_values` (O(1)) to return `false` for it.
   Otherwise WMS advertises it `queryable="1"` and answers with nodata or
   a 400 (#1064).
   **If a layer draws features** (alert areas, cell outlines, strikes),
   override `MapEngine::layer_picks_features` (O(1)) and `pick_features`
   with the SAME selection `get_raster_tile` draws them by (time rule, world
   copies), returning `ds_core::feature_pick` types. It runs on the query
   runtime, where `block_in_place` is valid. WMS GetFeatureInfo then answers
   those features (#1100). A forwarding wrapper (`ds_core::derive::Derived`)
   must forward both.
   **If a `FeatureEngine`'s features carry time**, apply
   `FeatureQuery::datetime` as OGC `datetime`: keep the features whose
   temporal geometry intersects the interval (an instant is `start == end`,
   ends inclusive) and those without one. Features `/items` and vector tiles
   (Tiles DateTime, #946) both pass it; the vector tile cache keys on it.
   Without time, return `false` from `has_time_dimension`: the HTML builder
   shows its time controls exactly when that is `true` (#1143).
   **If a `FeatureEngine`'s ids are opaque** (alert identifiers, track
   numbers), implement `label` from the feature's own values and
   `summary_properties`; a station whose EDR location has its id, or a site
   with its own collection, names it in `Feature::related` (#1143).
   **If the engine's parameters can be wind components** (u/v), implement
   `ds_core::wind::WindSource`: an O(1) `Arc<WindFacts>` snapshot, built at
   load/poll refresh, of what the source states per parameter (GRIB triple,
   CF `standard_name`, FMI number, the u/v frame, the level) and how the
   grid's axes lie (`GridAxes`). Report a frame only from source metadata,
   never assumed. Add the engine type to `config::DERIVE_WIND_ENGINES` and
   register it through `derived_wrapped` in `server/src/admin.rs`; the
   shared `ds_core::derive::Derived` wrapper does the rest (#897).
   **If the engine's parameters can be geopotential**, report in the same
   `WindFacts` every parameter's GRIB triple or CF `standard_name`, its
   level and its served unit; add the engine type to
   `config::DERIVE_GEOPOTENTIAL_HEIGHT_ENGINES`, and the wrapper derives
   geopotential height where the collection has none (#948). Never serve
   geopotential divided by g₀ under its own name: that is another quantity.
   **If the engine's parameters can be humidity ingredients** (temperature,
   dew point, relative or specific humidity), report in the same
   `WindFacts` each one's GRIB triple, CF `standard_name` or FMI number,
   served unit and level, and `ParameterFacts::vertical =
   Some(VerticalKind::Pressure)` for a parameter spanning a pressure axis;
   add the engine type to `config::DERIVE_HUMIDITY_ENGINES` (#898).
   **If the engine reads a regular grid**, set `RasterInfo.native_grid`
   (`NativeGrid::from_geo_transform`/`from_edges`) once per load or poll.
   Give an EPSG code only when the source states it (a GeoKey, or an exact
   projdef with its ellipsoid), never one derived from the `Crs`. A grid on
   several bands reports the default parameter's. Wrappers forward the
   source's grid, or report their own if they resample (#1237).
   **Set each EDR range's `NdArray::precision` from source metadata**
   (#1341), never from a parameter name: `ValuePrecision::F32` when the
   values are sampled from `f32`s (a decoder or cache holding `f32`, a
   float32 array or sample type), `F64` otherwise. A value computed from
   others (interpolated, unit-converted, derived) carries the coarsest of
   its inputs. EDR and WMS GetFeatureInfo write values through
   `ds_core::model::RangeValue`, so an `F32` value is its shortest `f32` text.
8. Ship a runnable (enabled) example collection config AND do an end-to-end
   server + curl smoke test against real data. Unit tests alone miss
   integration and unit-conversion bugs.

### Common collection discovery

EDR/Maps/Tiles/Features use `api-common` for collection request validation,
response/navigation assembly, shared metadata fields, OpenAPI and Common class
lists. Keep pure search policy and the `CollectionParameter` inventory in
`ds-core`; do not reintroduce API-local copies. Preserve API-specific metadata
and EDR's extent representation. Changes must update `docs/ogc-api-common-matrix.md`
and exercise `cargo test -p server --test common_discovery` alongside affected API
tests. Do not claim a draft class merely because some of its parameters work.

### Adding a new API endpoint

Same pattern for api-edr, api-features, api-maps, api-tiles:

1. Add the handler in `handlers.rs`, the route in `lib.rs`, new query params
   in `params.rs`, new response formats in `response.rs`.
2. **Always update `api_definition()` in `handlers.rs`** so the OpenAPI spec
   includes the new path. Tag every operation — with its collection id, or an
   `api_common::openapi_tags` group — or Swagger UI files it under "default"
   (enforced by `common_discovery`).
3. **An unknown query parameter must not be silently ignored.** serde drops
   unrecognized fields, so a parameter that is parsed but never validated
   returns 200 having done nothing — indistinguishable from success at the
   call site. That was the `sortby` bug (#605): validate against what the
   engine actually supports and return 400 naming the valid options.
   Extract a struct-shaped query with `api_common::JsonQuery`, never axum's
   `Query`, so a malformed or repeated parameter is the JSON 400, not
   axum's plain text (#1121). A route whose handler reads no query is no
   exception: route it with `api_common::errors::json_only_get` (JSON-only
   resources) or `format_only_get` (`/api`), which take `f` only (#1123).
   Extract a numeric path segment as `String` and parse it in the handler
   (`api_tiles::params::tile_indices`): axum's `Path` rejection is plain text.
4. **When a parameter comes from a standard, copy its schema verbatim** into
   `api_definition()` — including `style`/`explode` for array parameters,
   which is what makes the comma-separated form normative rather than
   incidental. `api-features` pins this with a test that validates `/api`
   against the bundled OpenAPI 3.0 meta-schema (`schemas/openapi-3.0.json`);
   the definition is hand-written `serde_json::json!`, so nothing else
   catches a typo in it. Validate `/api` only through the shared
   `#[path]` helper `crates/api-common/tests/support/openapi_schema.rs`:
   the meta-schema is Draft 4, where a plain `jsonschema::Validator::new`
   skips `uri-reference` and so never checks a `$ref` (#1045).

## Performance & Concurrency Rules

Hard-won from production incidents (epic #201; spike investigations
#221/#222). Apply them to every engine and rendering feature, not just where
they were found. Critical Rules 5–7, 9 and 10 above are part of this set.

- **All engines share ONE multi-thread request-serving Tokio runtime**
  (worker count ≈ CPU cores). A poll/scan loop doing blocking I/O on it parks
  a worker for the whole operation; when several collections' polls overlap,
  the pool starves and every collection's p99 spikes — multi-second, even at
  low load (#221, #208). That is why poll loops run on `poll_runtime()`.
  A blocking scan may additionally use `tokio::task::spawn_blocking` (the
  ODIM HDF5 scan does). Storage calls from those workers must follow the
  runtime-handle guidance in Critical Rule 7. The grib/geotiff/querydata poll
  bodies still do blocking I/O directly and are safe only because they run on
  the background runtime.
- **Background metadata refresh must not contend with request serving.**
  Discovering new data (S3 LIST, STAC HTTP, GRIB index, COG header) is
  background work — keep it off request-serving workers.
- **A cache only helps if its key matches the access pattern.** A rendered
  cache keyed on exact bbox+width+height got ~3% hits for fullscreen
  arbitrary-viewport WMS — pure wasted RAM (#202). Cache at a granularity
  that actually repeats (tile-aligned), or don't allocate the cache.
- **Decode to compact native types, not `Vec<Option<f64>>`** — boxing every
  sample is a 16× memory blowup (#206). `RasterTile.values` is the
  `RasterValues` enum: `F64` (boxed universal form),
  `U8 { data, nodata, gain, offset }` (raw bytes) or
  `F32 { data, nodata }` (4 B/px floats; NaN/±∞ and the optional sentinel
  are nodata, #475). Integer render paths
  should produce `U8`, which `ds-render` colorizes through a 256-entry LUT
  indexed by the raw byte (built per call; entry i ≡ `colormap.color(
  value_at(i))`, so the variants are pixel-identical by construction — pinned
  by the `u8_lut_colorize_matches_boxed_f64_exactly` test). Float render
  paths should produce `F32`: no LUT, per-pixel colormap evaluation of
  `RasterValues::decode_f32` — the same function `value_at` uses (pinned by
  `f32_colorize_matches_boxed_f64_exactly`). GeoTIFF's map
  path produces `U8` for local u8 sources with an integer u8 nodata
  (`reader::read_bbox_u8`, self-gating with `Ok(None)` → boxed fallback);
  GRIB's produces `F32` (f64 bilinear + display conversion, narrowed on
  store); every other engine constructs `F64` via `.into()`. Narrowing an
  f64 result to `F32` is not free: a value within half an f32 ulp of a
  colormap LUT edge can shift one entry — only a source that is natively
  f32 AND sampled without arithmetic is pixel-identical to its boxed form.
  Adding a `RasterValues` variant causes exhaustive-match compile errors in
  `colorize`/`value_at`/`is_all_nodata` (and engine-nowcast's frame intake)
  — keep them consistent.
- **All raster output→source coordinate mapping goes through
  `OutputCrs`/`ProjectionGrid`** (the `MapEngine::get_raster_tile` path). The
  WMS projected meta-tile assembly (`ds-render/src/metatile.rs`) is the
  ONLY place that re-derives the output coordinate map, and it must stay
  consistent with the engine path (#452). When debugging a "data displaced /
  misplaced" report, first confirm which render path the failing request
  uses: **WMS EPSG:3857/3067/3035 go through meta-tiling, not the direct
  `get_raster_tile` path that Maps/Tiles use.**
  A global lat/lon source in a projected CRS builds its grid with
  `ProjectionGrid::build_2d_periodic` (its column period) and wraps the
  sampled column: EPSG:3035's inverse cuts longitude at 170°W and every
  longitude meets at the pole, so plain `build_2d` interpolates straight
  through both in any Arctic view (#323; engine-grib does this, and
  `SourceGrid` does it for engine-netcdf and engine-querydata, #1188).
- **A new gridded engine renders through `ds_core::resample::SourceGrid`**
  (#1067), not a sampler of its own: decode the field to a row-major
  `&[f32]`, north row first, on its `GeoTransform`, and call `render_f32`
  (or `output_map` once, then `sample_f32` per field). An engine serving
  display units passes its conversion to `sample_f32_with`, applied in
  `f64` before the pixel is narrowed, and converts `value_at`'s result the
  same way, so a pixel and a point query agree (engine-netcdf, #1209). It is querydata's
  projected path, pinned to it on the MEPS fixture
  (`engine-querydata/src/engine/sampler_tests.rs`): half-pixel convention,
  bilinear with a nearest fallback, `RasterValues::F32`. Lat/lon and Web
  Mercator sources get the periodic map; any other gets the longitude
  within half a turn of the grid's centre and the
  `footprint_window_in_image` guard (#449/#453), and a footprint wholly
  off the image builds no grid at all (#1129). Build the `SourceGrid` once
  per grid, not per request: `new` inverse-projects the envelope. Its point
  queries (an EDR position, an area's cell centre) project each point once
  with `position` and sample every field there with `value_at`, the map's
  own sampling, so GetFeatureInfo answers what GetMap drew (engine-netcdf).
  An engine that cannot afford a decoded field shares the map alone:
  `output_map_quiet` (it logs `OutputMap::capped_error` itself), then
  `OutputMap::position` per pixel and its own read of its storage by the
  same rules, wrapping a global grid's columns (`global_period`) and
  checking the deadline per row (engine-querydata, #1190). grib and
  satellite keep their own samplers (#1172). A curvilinear grid (2-D
  lat/lon, no CRS) is `SourceGrid::from_curvilinear` over a
  `resample::CurvilinearGrid` built once per grid: its inverse lookup (k-d
  tree on unit vectors, then the cell around the nearest node) stands in
  for the projection at the coarse nodes and for `position`, never per
  pixel (#1174); `transform()` is then `None`, so size a field with
  `size()`.
- **Before blaming a mechanism for a latency spike, check the magnitude adds
  up.** A 2.3 MB local read from page cache is tens of ms, not seconds.
  Multi-second stalls at low load almost always mean blocking/contention
  (a parked runtime worker, a held lock, sequential network calls), not
  extra CPU.

### Pixel budgets: output vs source

Output caps bound the image or grid a client asks for; source budgets bound
what an engine decodes to produce it. They count different pixels and are
deliberately not tied by a ratio (#120): a projected source (TM35FIN radar
over Scandinavia) can need far more native pixels than the viewport has, and
a fixed ratio would reject valid viewports without proving safety. Passing
one never implies the other.

| Budget (home) | Bounds | Value | When it trips |
|---|---|---|---|
| `MAX_MAP_DIMENSION` / `MAX_MAP_PIXELS` (api-wms `params.rs`; `api_common::map_frame` for Maps and map tiles) | output px per side / `width × height` | 8000 / 64 M | 400 at validation (WMS `InvalidParameterValue`, Maps/Tiles `BadRequest`): "… must not exceed 8000" |
| `api_tiles::params::TILE_SIZE` | output, a map tile's default (`width`/`height`/`scale-denominator` resize it within the caps above) | 256 × 256 | never |
| `ds_executor::budget::RENDER_MEMORY` | output px × 32 B, every WMS/Maps/Tiles cache miss; an RGB composite adds 16 B per band plane past the first (`acquire_raster_planes`); a WMS GetMap of several layers adds 4 B for the image below, once (`acquire_layered_raster`) | `MC_RENDER_MEMORY_MB`, 1024 MiB ⇒ 33 554 432 px | 503 "Server busy, try again later" + `Retry-After: 1`: after queueing to the deadline, or at once if larger than the whole budget |
| `engine_geotiff::reader::MAX_MAP_PIXELS` | native source px of one full-resolution map read | 64 M | Maps/Tiles 400 "Invalid parameter: Map render source area N pixels exceeds maximum 64000000."; WMS red error tile, HTTP 200 `x-cache: ERROR` |
| `engine_geotiff::decode_budget::BUDGET` (a `ByteBudget`) | bytes per source tile fetched or decoded | `MC_GEOTIFF_DECODE_MEMORY_MB`, 1024 MiB | 503 in every API |
| engine-zarr `read_budget::BUDGET` (over a `ByteBudget`) | bytes of native reads + conversion buffers | `MC_ZARR_READ_MEMORY_MB`, 1024 MiB | 503 |
| engine-netcdf `decode_budget::BUDGET` (a `ByteBudget`) | bytes of one field read in progress: 3 × decoded + 3 × compressed, the decoded chunk bounding the compressed one (~24 MB per MEPS field); cache waiters and hits reserve nothing; a remote fill detached from its request and a poll's prewarm reserve it too | `MC_NETCDF_READ_MEMORY_MB`, 1024 MiB | 503 |
| engine-odim `read_budget::BUDGET` (a `ByteBudget`) | bytes a PVOL EDR walk (`walk_volumes`, GetFeatureInfo's position query included) holds per volume, reserved before its file is read: the bytes fetched (a read ahead's whole file, else the moments' byte ranges or the whole file) until decoded, and the moments decoded at 4 B/sample until the walk moves on; resident moments cost nothing; at most four volumes per walk; map, 3D Tiles and cell reads not counted | `MC_PVOL_READ_MEMORY_MB`, 1024 MiB: an all-quantity profile of a 41 MB FMI volume reserves ~196 MB, ~750 MB for a walk reading three ahead | 503 in EDR and GetFeatureInfo; a read ahead with no room is not started, not rejected |
| engine-netcdf `edr::QUERY_BYTES` | decoded bytes of the reads one EDR query plans (`edr::Job::bytes`), from the file metadata: a whole field `FieldRead::decoded_bytes` (4.06 MB for MEPS), an OPeNDAP window its cells × the stored levels of its hyperslab × 4 B; only fields read count (a trajectory each (step, level) its plan reads, a cube the levels its z positions take, a per-step amount's steps before) | `MC_NETCDF_EDR_QUERY_MB`, 544 MiB: read whole, one parameter over one MEPS run (67 leads) as the client sees it, including a derived wind speed, which reads both components (134 fields); not three variables over a run, nor one over a run at all 13 MEPS pressure levels (871 fields). OPeNDAP windows hardly count | 400 "Query too large: Query would decode N fields …", before any read |
| engine-netcdf `edr::QUERY_FIELD_READS` | reads one EDR query plans, any access: a field of one level is one read, an OPeNDAP request one, holding the windows of a step's variables, each over every level asked, up to `edr::MAX_REQUEST_PLANES` (16) planes | `MC_NETCDF_EDR_QUERY_READS`, 200: through OPeNDAP a six-variable surface meteogram over a MEPS run is 67, one variable at all 13 levels 67, a derived wind speed at all 13 134, three variables at all 13 201 (refused); ≤ 0.55 s per request of ≤ 16 planes (0.25 s + 0.016 s per plane, measured), four at once, ~27 s inside the 30 s deadline. Read whole, `QUERY_BYTES` binds first (140 MEPS fields) | 400 "Query too large: Query would make N field reads …", before any read |
| `ds_core::feature::MAX_AREA_DIM` / `MAX_AREA_VALUES` | EDR area output cells per axis / values per response | 256 / 1 M | coarsened / 400 "Query too large: …" |
| api-edr `response_budget` (a `ByteBudget`) | bytes of every EDR response body in memory, each buffer charged before it is allocated and until the body is delivered or dropped: CoverageJSON (serialised straight into it), EDR GeoJSON, CSV (its counted size), an HTML data page (its estimate, before it renders) and `/locations`; not PNG plots, `items`, metadata, the engine result or the HTML page's `Value` document | `MC_EDR_RESPONSE_MEMORY_MB`, 512 MiB; `/locations` also `MC_EDR_LOCATIONS_MAX_BYTES`, 16 MiB per body | 503 `ServerBusy` + `Retry-After: 1` at once; a `/locations` body over its cap 503 `ResponseLimit` |
| `engine_geotiff::STAC_REQUEST_MAX_COLD_ITEMS` | STAC items whose header one EDR query reads (items in its window with no metadata yet; without `datetime`, in the whole catalog) | 400 (a closed day of 5-minute frames is 289) | 400 "Query too large: …", counted from the STAC response before it is installed or anything loads (#889) |
| EDR `f=png` plot, WMS `GetLegendGraphic` | output px | 160–2000 × 120–2000 / ≤ 512 × 1024 | never: clamped silently |

- The engines' source byte budgets, and EDR's response budget (#1041), are
  each their own instance of `ds_executor::budget::ByteBudget`, with their
  own env var and static; a new engine's is one more instance, never a copy
  (#1067). Unlike
  `RENDER_MEMORY`, a waiting `RenderBudget`, they fail at once. A refused
  batch probe (`try_reserve`) or background reservation
  (`try_reserve_background`, at most half the budget) is not a counted
  rejection.
- At the default `MC_RENDER_MEMORY_MB` the API caps' full 64 M px is
  unreachable: anything over 33 554 432 output px (e.g. 5793 × 5793) is a 503
  on every retry. To serve larger images, raise the budget (README "Sizing
  render memory").
- The GeoTIFF source cap only trips on a full-resolution read: over it,
  `get_raster_tile` switches to the finest overview under it, else the
  coarsest, so in practice only COGs without overviews hit it.
- GeoTIFF EDR area answers at the native window, like GRIB (no
  `MAX_AREA_DIM` coarsening): `query_bbox` checks `MAX_AREA_VALUES` over
  `timesteps × ny × nx` before allocating the result or reading a file, so
  an over-budget area is the 400. Only a file that cannot be read becomes a
  logged timestep of nulls; client errors fail the query (#858). Its
  polygon mask has the shared `check_mask_budget` (#889). Both run before
  any data read; on a STAC source, after the window's metadata loads.
- A new source cap must name the pixel space it counts and fail with
  `InvalidParameter`/`QueryTooLarge` (400 with its message in Maps/Tiles/EDR).
  `Engine(..)` is an opaque 500 there, and WMS GetMap turns every engine
  error except `ResourceExhausted`/`DeadlineExceeded`/`Unavailable` (503)
  into the red tile.

## Shared Domain Machinery (ds-core, `crates/core/`)

- **`ds_core::web_mercator`** — the ONLY EPSG:3857↔WGS84 implementation
  (Critical Rule 4). `Crs::WebMercator` is its `Crs` form for a source grid
  (GMGSI): forward keeps longitude unwrapped, and a global grid wraps its
  own columns.
- **`ds_core::geo`** — CRS transforms (WGS84, TM, LAEA, LCC, Stereographic,
  Geostationary, WebMercator; TM, LAEA and Stereographic on a `figure:
  Ellipsoid`, WGS84/GRS80 by name, a sphere or any ellipsoid, #1192;
  `ds_core::cf`, engine-odim projdefs and engine-geotiff GeoKeys pass the
  figure their source states, an unknown or unrepresentable one an error;
  engine-querydata the one newbase master computes on, #1243),
  `GeoTransform`, `geometry_to_pixels`, `destination_point`,
  `geodetic_to_ecef`, `OutputCrs::footprint_pixel_window` (low-zoom ghost
  guard, #453). `Crs::WebMercator`'s northing is unbounded at the poles.
  `Crs::Geostationary` is the one **partial** transform both ways:
  `forward` is NaN on the far side of the Earth and `inverse` `None` off the
  disk, so code mapping many points must skip non-finite results; its
  `GeoTransform::bbox` comes from the limb and may be `west > east` (#819).
  `Crs::TransverseMercator` is Krüger's series to sixth order in Karney's
  2011 form, as PROJ's `tmerc` (#1130). Its **inverse** is partial too:
  `None` outside a square in projected metres, past a pole's northing or
  an easting k₀ times a quarter meridian (9 997 965 m for EPSG:3067) from
  the central meridian, where the series stops being exact (#1113; inside,
  under 1 mm off the exact projection). That spans the hemisphere within
  90° of the meridian except near the equator, where it ends 66.3° out.
  Its `forward` stays total: exact inside the area, unbounded near the
  equator 90° out. Edge samples miss the area's edge between them, so
  `wgs84_envelope` adds `area_edge_points` (where those edges enter and
  leave the box, and the equator or pole between) and a pole the box holds
  (`holds_pole`, projected from the central meridian). A source grid's
  extent is `GeoTransform::bbox_with_poles` → `grid_extent`, the one extent
  calculation of every gridded engine (#1194): longitude unrolled along the
  edges, so a grid across the antimeridian is `west > east` and a polar
  grid across its far meridian stays narrow; a pole its cells hold reached
  at every longitude; TM's area edges. Never use the plain `bbox()` for an
  extent. `footprint_envelope()` is its `east`-past-180 form for footprint
  guards; engine-geotiff, engine-querydata, engine-odim, engine-netcdf and
  `SourceGrid` take it (#1167). `polar_stereographic_k0(lat_ts, &figure)`
  is the one conversion of a polar standard parallel (PROJ `+lat_ts`, CF
  `standard_parallel`) to the `k0` of `Crs::Stereographic` on its figure
  (engine-odim, `ds_core::cf`); LAEA's polar
  aspect (EASE-Grid 2.0) takes its limit `β₀ = ±90°`, `D = 1`.
- **`ds_core::cf`** — CF semantics shared by NetCDF and Zarr readers,
  which pass plain values or an attribute lookup (no reader library here):
  - time units and `decode_times` (`… since 1970-01-01 00:00:00 +00:00`,
    `… UTC`, `…Z`, offsets moved to UTC);
  - `classify_axis` → `AxisRole`, including projected x/y (by
    `standard_name`) and ensemble (`realization` / `ensemble_member`);
  - `crs_from_grid_mapping`: `latitude_longitude`, `geostationary`,
    `lambert_conformal_conic` (one or two parallels, a sphere or WGS84),
    `polar_stereographic` (`standard_parallel` through
    `geo::polar_stereographic_k0`, or `scale_factor_at_projection_origin`;
    both must agree), `stereographic` (any origin, PROJ `stere`),
    `lambert_azimuthal_equal_area` and `transverse_mercator`, these four
    on the figure the mapping states, sphere or ellipsoid (#1192), each
    pinned to PROJ `cs2cs`. `coordinate_scale` takes projected x/y in m or
    km; `grid` scales km axes to metres and the mapping's
    `false_easting`/`false_northing` with them, since CF states those in
    the coordinates' units (#1194). Unsupported mappings, LCC ellipsoids
    other than WGS84, prolate figures, scale factors and missing earth
    figures are errors, never a WGS84 guess;
  - `CfAttr::Numbers` keeps a multi-element attribute whole, and a
    one-number parameter rejects it rather than reading its first element;
  - `grid(crs, &x, &y)` over `RegularAxis` values: units through
    `coordinate_scale`, a north-up `GeoTransform`, and `flipped` for an
    ascending `y` (row 0 south, as MEPS), which the reader reverses once at
    decode;
  - `PressureUnit`: a pressure vertical coordinate, known by its units
    (CF §4.3: `Pa`, `hPa`, `mbar`, …) once the reader has ruled out a
    parametric coordinate (`formula_terms`, a hybrid `standard_name`), and
    its levels in hPa to six significant digits (85000 Pa is exactly 850
    hPa; a `float` 0.85 bar is 850, not 850.0000238).
- **`ds_core::units`** — unit spellings, display conversions and vocabularies
  (#1067 PR 6). `canonical_unit` reduces every spelling of a product of
  powers to one (`kg/m^2`, `kg m**-2`, `kg.m-2` → `kg m-2`; `m/s` → `m s-1`;
  `degC` → `°C`); a scale factor or free text stays as written. Every
  unit-keyed match goes through it — `display_conversion` (engine-bufr;
  engine-grib's enum-keyed `default_display` is its twin, rule for rule:
  K → °C, Pa → hPa, kg m-2 → mm, kg m-2 s-1 → mm/h × 3600, proportion → %;
  a rule changes the unit, never the quantity, so geopotential's m2 s-2
  has none, #948), `cf_display_conversion` (engine-netcdf #1209 and
  engine-zarr #1213: the same rules, plus CF's `1` → % × 100 only when the
  variable's `standard_name` is `relative_humidity` or ends in `_area_fraction` or `_area_fraction_in_atmosphere_layer`; every other `1` — specific_humidity,
  land_binary_mask, no standard name — is served as stored),
  `qudt_unit` (EDR), ds-render's defaults table and `[[parameter_defaults]]`
  unit ranges, `same_unit` (wind u/v pairing, engine-netcdf's per-file
  check, the satellite recipes' K check) — never a raw string compare. The
  canonical spelling is a matching key and is never served: engines serve
  units as their source spells them, or as a display conversion names them.
- **`ds_core::temp_files`** — the names a data scan never reads (#1009): a
  hidden `.name` (written, then renamed into place) or a `PARTIAL_SUFFIXES`
  ending (`.tmp` / `.part`, the default `exclude_patterns`). An engine that
  enumerates a directory or prefix itself filters with `is_temporary` /
  `is_temporary_key`; the shared `ds_storage::discovery` scan skips hidden
  names always and leaves the suffixes to `ScanSpec.exclude`
  (`exclude_patterns`, or `partial_exclude_patterns()` for an engine
  without that setting). A listing that reaches below its base — every
  object-store `list`, including a local `data_path` through `ds-storage`,
  is recursive — also skips files in a hidden folder below that base
  (#1029; rsync `--delay-updates` stages files under their final names in
  `.~tmp~/`): `in_hidden_folder(base, key)`, or `is_temporary_below(base,
  key)` for both rules. The base's own segments are never judged, so a
  dot folder in the configured path or prefix is fine; the suffixes are
  file rules only. `discovery::scan_remote` applies the folder rule;
  `list_prefixes` and `DataStore::list` callers apply it themselves. A new
  engine's file listing does the same. A store-internal listing (Zarr v2's
  `.zarray`) is not a data scan.
- **`ds_core::instances`** — model-run (forecast reference time) machinery
  shared by ALL forecast engines (#337), so run selection, instance lists and
  instance-id encoding are identical everywhere:
  - `RunInfo { reference_time, valid_times }`; `instance_id()` = the RFC 3339
    UTC timestamp `2026-06-07T06:00:00Z` (MetOcean EDR profile, #947).
  - `format_instance_id` / `parse_instance_id` — URL ↔ reference-time codec
    (parse also accepts any RFC 3339 offset and the pre-#947 compact
    `20260607T0600Z` stamp, so old links resolve). Links carry the id's
    colons unencoded (RFC 3986 `pchar`). The API layer owns the string form;
    engines only see `Option<DateTime<Utc>>`.
  - `select_run(&BTreeMap<..>, Option<DateTime<Utc>>)` — `None` ⇒ latest,
    `Some(rt)` ⇒ exact run, absent ⇒ `None` → 404.
  - `build_instances(&runs, |rt, run| valid_times)`.
  - Engine contract: store runs in `BTreeMap<DateTime<Utc>, _>` keyed by
    reference time; implement `EdrEngine::get_instances()` (default empty =
    non-forecast), with `has_instances`, `find_instance` and
    `instance_reference_times` (the map's keys, #1006) from the same
    snapshot; honour the trailing `reference_time` parameter on query
    methods and `MapEngine::get_raster_tile` (`None` ⇒ latest); populate
    `RasterInfo.reference_times`. GRIB, QueryData, Zarr (forecast stores),
    NetCDF (`forecast_reference_time`) and Nowcast (generations = runs,
    #661) implement this; other engines accept-and-ignore.
  - API surface: EDR `/instances` (listed newest first: the handler sorts
    it, engines and `build_instances` stay ascending), `/instances/{id}`,
    `/instances/{id}/{position,area}` — gated on `get_instances()` being
    non-empty; no-instance routes default to the latest run. WMS exposes
    `DIM_REFERENCE_TIME` (see `crates/api-wms/CLAUDE.md`). Maps/Tiles still
    pass `reference_time: None` (follow-up).
- **`ds_core::time_axis`** — the one rule for long time axes (#1006): up to
  `MAX_LISTED_VALUES` (500) values a list, WMS byte-identical to before; past it,
  ranges of equally spaced values, lossless. WMS writes them as Annex C
  `min/max/res` (`time`, `reference_time`); EDR pages `/instances` past the
  same threshold and keeps the run axis out of collection metadata. See
  `crates/api-wms/CLAUDE.md` and `crates/api-edr/CLAUDE.md`.
- **Vertical dimension** — `ds_core::vertical::VerticalDimension`, surfaced
  on `RasterInfo.vertical` and `EdrEngine::get_vertical_extent`.
  `MapEngine::get_raster_tile` takes `z: Option<f64>` (one rendered layer);
  EDR query methods take `z: Option<&[f64]>` (one or more levels). WMS
  exposes it as `ELEVATION`, Maps/Tiles as `elevation`, EDR as `z`. Against
  a collection with no vertical extent the rule is per API: EDR ignores a
  well-formed `z` (EDR 1.2 `/req/edr/z-response` A makes that a SHALL;
  a malformed one is still a 400), while WMS `ELEVATION` and Maps/Tiles
  `elevation` reject it with HTTP 400. The ODIM PVOL engine uses it for
  radar elevation angle, GRIB's level views for pressure and model levels,
  and engine-netcdf for a CF pressure axis (#1067: hPa, bottom first, a map
  without `z` drawing the bottom level, EDR without `z` every level).
- **`ds_core::radar_sites`** — `RadarSiteInfo` + the data-only
  `RadarSiteSource` trait (one catalog snapshot per call, never I/O) that a
  polar-volume engine implements and the nowcast's per-cell beam-geometry
  join (#642) consumes. The 4/3-Earth beam model
  (`geo::{slant_to_ground_height, beam_height_at_ground,
  FOUR_THIRDS_EARTH_M}`) and `geo::great_circle_distance_m` live in
  `ds_core::geo` — one home; engine-odim re-exports them.
- **`ds_core::trajectory`** — the EDR along-path trajectory (#926): WKT
  `LINESTRING` / `Z` / `M` / `ZM` parsing, great-circle densification at the
  source grid spacing (≤ 2000 samples), the M-time and Z-level snapping and
  range rules, and the CoverageJSON `Trajectory` layout (`TrajectoryPlan`).
  A gridded engine selects its run/steps/levels like its position query and
  samples `plan.fields()` grouped per field or window — never re-implement
  these rules per engine, never read per sample. A radar volume instead
  declares `TrajectoryShape::CrossSection` (see `crates/api-edr/CLAUDE.md`).
- **`ds_core::cells`** — storm-cell segmentation and tracking over
  `VoxelGrid` (see `crates/engine-odim/CLAUDE.md`).
- **`ds_core::significance`** — domain-agnostic scoring/ranking: normalized
  `Term`s → a weighted mean with per-term `contributions` for
  explainability. Graded terms form the denominator and absent ones
  renormalize (so wiring a new data source needs no config flag day);
  bonus terms (`Term::flag` / `Term::bonus`) compose outside it, bounded by
  construction — a positive weight fills the remaining headroom, a negative
  weight is a multiplicative discount (fraction removed at full value) — so a
  signal that did not fire dilutes nothing and three firing at once cannot
  clamp the top of the list into a tie (#645). `WeightedScorer` is the baseline a
  learned ranker has to beat; a GBDT/ONNX model is another
  `impl Significance<T>` behind the same interface. Reusable for CAP alert
  urgency and impact-event priority — do not fork it per domain.
- **`ds_core::cell_facts`** — also `is_likely_clutter` (#614): a persistent
  near-stationary echo is flagged and demoted, because no other significance
  term asks whether an echo is meteorological at all.
- **`ds_core::cell_facts`** — `CellFactSheet`: the one wide description of a
  tracked storm cell, plus `Severity` (owned here so the tracker, the
  ranking and any narrative cannot drift) and `DEFAULT_CELL_WEIGHTS`. Built
  once per generation; read by feature properties, ranking, narratives and
  the #541 V2.4 model feature row.
- **`ds_core::simplify`** — the one Douglas–Peucker implementation, for
  closed rings and open lines, iterative so no input can deepen a worker's
  stack; the tolerance is in the caller's metric plane. Storm-cell
  footprints and the Features items map preview (#1135) use it; ds-mvt and
  any other simplifier should too, rather than add a `geo` dependency.
- **`ds_render::rasterize::fill_polygon`** — the shared vector→raster fill
  primitive (#397), fed by `ds_core::geo::geometry_to_pixels` (vertices
  projected via `OutputCrs::world_to_fraction`, never per pixel). Vector
  `MapEngine`s (engine-cap today) may depend on ds-render for it — an
  approved exception; engines still return `RasterTile` domain types and
  colorization stays in the API layer.
- **Station symbol layers** (#1063, #1204) — every station collection's
  marker layers share one implementation: `ds_core::station_map` (resolved
  `[<engine>.map]` settings, the `time_step` frame axis and
  `resolve_frame_end` that `get_raster_tile`/`resolve_time` share, the
  frame window, `LatestValues`, `MAX_FRAME_ROWS`) and
  `ds_render::station_symbols` (overlap order around the server-handed
  midpoint, `draw_markers` projecting per station, `NODATA_MARKER` for
  `SymbolColorMap`, the shared `MC_STATION_FRAME_CACHE_MB` frame cache keyed
  by a `next_frame_source()` id, `StationLayers` for in-memory engines).
  The server wires them with `admin::station_map_wiring`. Never fork it per
  engine.
- **`ds_render::resolve_layer_style`** — the one style lookup for WMS GetMap
  and GetLegendGraphic, and the Maps and Tiles map, tile and legend routes
  (#1133). A named parameter styles from `"{collection}/{param}"`, else the
  collection; on a multi-parameter collection (`RasterInfo.parameters`
  longer than one) a request naming none draws the engine's default
  parameter with its own layer's style, so `LAYERS=coll` and Maps/Tiles
  without `parameter-name` match `coll/<default>`. A single-parameter
  collection keeps its collection style, the one WMS advertises for its
  one layer. The rendered and meta-tile caches key on the `LayerStyle` it
  returns (layer, style, parameter), never the raw request. Never resolve
  the style a map, tile or legend is drawn with by hand; the capability
  and collection-metadata style listings read the style maps directly.
- **`ds_core::wind`** — wind speed and direction derived from u/v
  components (#897), the one home for the formulas (`hypot(u, v)`; the
  direction the wind blows **from**, `atan2(-u, -v)`, calm → null), the frame
  rules (`derivable`: unknown frame → no direction, speed only where the
  grid's axes are orthogonal; mixed frames → nothing; grid-relative →
  direction only on a north-aligned grid until rotation by the convergence
  angle exists) and the pairing in each source's own vocabulary
  (`WindPlan`: `10u`/`10v` → `10si`/`10wdir`, `u`/`v` → `ws`/`wdir`,
  `UGRD`/`VGRD` → `WIND`/`WDIR`, FMI → `WindSpeedMS`/`WindDirection`, CF
  stores by `standard_name`; skipped when the collection already has the
  quantity at that level). `ds_core::derive::Derived` (`DerivedWind` is its
  wind-only form) wraps an engine's `MapEngine` + `EdrEngine` in the
  server's registry wiring (`admin::derived_wrapped`, like the nowcast
  wraps its source): speed on every API, direction in EDR only
  (no cyclic palette). It derives **after sampling** — two component reads
  with identical bbox/size/time/z/run, combined per value, never a derived
  field interpolated — and resolves time/run/content as the components do
  (`resolve_parameters_time` and `resolve_parameters_reference_time` over
  u and v, intersected axes; #507/#521/#1026).
  Per collection `derive_wind = false` opts out. Engines report facts
  (`WindSource`); they never compute derived fields themselves.
- **`ds_core::derive`** — the one wrapper for every derived parameter
  (#897, #948): `Derived::with(id, engine, Derivations, DerivationLog)`
  plans each derivation from the engine's `WindFacts` snapshot (named for
  wind; it carries every parameter's GRIB triple, CF `standard_name`,
  level and served unit) into `DerivedParameter { inputs, formula }` and
  serves them over `MapEngine` + `EdrEngine`. A parameter derived from one
  input resolves time, run, level and axis through that input's own
  single-parameter methods; one from several through the plural ones over
  them, as wind does. A new derivation adds a `Formula`, a planner called
  from `Plan::build` and a `Derivations` flag with its config opt-out,
  never a second wrapper, as humidity did (#898). A formula that reads the
  level's pressure (`Formula::reads_level_pressure`) gets it, in Pa, as its
  last sample: on a map from `resolve_elevation` of its first input on an
  isobaric axis, in EDR from each value's level in the coverage's `z`
  (none ⇒ null values).
- **`ds_core::geopotential`** — geopotential height derived from
  geopotential (#948): `height` = Φ / `STANDARD_GRAVITY` (9.80665) in gpm.
  Geopotential is identified only by metadata (GRIB 0/3/4, CF
  `geopotential`) in m² s⁻²; a name alone never derives (`z` is often a
  vertical coordinate). Named in the source's vocabulary (`HeightPlan`:
  `z` → `gh`, wgrib2 `GP` → `HGT`, CF by inserting `height` after the
  `geopotential` token, `geopotential_pl` → `geopotential_height_pl`);
  skipped where the collection has geopotential height at that level
  (GRIB 0/3/5, CF `geopotential_height`, or a `gh`/`HGT` name without
  metadata). A scalar with the `gh` default style, served on every API. A
  single-level `z` gives the terrain height by the same rule. Per
  collection `derive_geopotential_height = false` opts out
  (`config::DERIVE_GEOPOTENTIAL_HEIGHT_ENGINES`: grib, zarr, netcdf).
- **`ds_core::moisture`** — relative humidity and dew point derived from
  their ingredients (#898). One saturation formula over liquid water at
  all temperatures (WMO-No. 8): IFS Tetens 611.21 Pa, 17.502, 32.19 K,
  T0 273.16 K, and its exact inverse for dew point. RH from T+Td, else T+q
  at an isobaric level's pressure (hPa × 100; never MSLP, never model
  levels); Td from q at an isobaric level (no assumption about a native
  RH's reference; ECMWF `r` is mixed phase), else T+RH (taken as over
  water). RH clamped to 0–100 %, Td ≤ T; q ≤ 0 or RH ≤ 0 ⇒ RH 0, Td null.
  Ingredients by GRIB 0/0/0, 0/0/6, 0/1/1, 0/1/0, CF `standard_name` or
  FMI 4/10/13 (a vocabulary name only without metadata), at one level
  (`ParameterFacts::level` + `vertical`), units from the served unit
  (K/°C, %/`1`, kg/kg/`1`, g/kg; unknown ⇒ refused and logged). Named in
  the source's vocabulary (`MoisturePlan`: `2t`/`2d` → `2r`, `t`/`q` →
  `r`/`dpt`, `t2m`/`d2m` → `r2`, wgrib2 `RH`/`DPT`, FMI
  `Humidity`/`DewPoint`, CF by swapping the `air_temperature` token);
  skipped where the quantity exists at the level. RH titled "Relative
  humidity over water", in %; Td in the temperature's served unit. Per
  collection `derive_humidity = false` opts out
  (`config::DERIVE_HUMIDITY_ENGINES`: grib, querydata, zarr, netcdf).
- **`ds_core::native_grid`** — `NativeGrid` (#1237) and `crs_wkt2`, the one
  WKT2 form per `Crs` variant, PROJ-checked and pinned in tests.
  `ogc_extent::build_extent` emits `storageCrsBbox` and a cell-centred
  `grid` only when the stated code is the advertised `storageCrs`, and
  `meteocore:nativeGrid` for every grid that is not lat/lon. Never add a
  `storageCrs` MeteoCore cannot render: Maps `/req/core/map-response` B
  makes it the default output.
- **`DataServerError::Unavailable`** (#1355): ds-storage types a store
  read as `Unavailable` when its last answer, after all retries, was
  429/502/503/504.
  - Every API answers it with 503, `Retry-After: 10` and the generic
    `UNAVAILABLE_DESCRIPTION` (JSON code `SourceUnavailable`, via
    `api_common::source_unavailable`), and logs it at WARN.
  - An engine wrapping a store error adds context with
    `DataServerError::with_storage_context`, never with
    `Storage(format!(…{e}))`, which turns the 503 into a 500.
  - GRIB position and trajectory queries fail with it, instead of sampling
    the field as null.
  - S3 stores retry per `S3_RETRY` in `crates/storage/src/lib.rs`.

## Engine Capabilities

| Engine | Traits | APIs |
|--------|--------|------|
| CAP | `FeatureEngine` + `MapEngine` | Features, WMS, Maps, Tiles (severity-shaded alert polygons) |
| CSV | `EdrEngine` + `FeatureEngine` + `MapEngine` | EDR (locations, area, radius), Features; WMS/Maps/Tiles station symbol layers when opted in (`[csv.map]`, or any map API in `apis`; #1204), drawn through `ds_render::station_symbols`, `content_version` 0 (static until reload) |
| GeoJSON | `FeatureEngine` | Features, Tiles (MVT) |
| GeoTIFF | `EdrEngine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| GRIB | `EdrEngine` + `MapEngine` | EDR (position, area, radius, trajectory incl. LINESTRING Z/M/ZM), WMS, Maps, Tiles; derived wind speed + direction from u/v (`ds_core::wind`); geopotential in m² s⁻² and its height derived where a view lacks one (`z` → `gh`, `ds_core::geopotential`, #948); relative humidity and dew point derived where a view lacks them (`2t`/`2d` → `2r`, pressure-level `t`/`q` → `r`/`dpt`, `ds_core::moisture`, #898); hour-window aggregates on their own time axes (#1005) |
| ODIM COMP | `EdrEngine` + `MapEngine` | EDR (position, area), WMS, Maps, Tiles |
| ODIM PVOL | `EdrEngine` + `MapEngine` + `VolumeEngine` (per-site views) + `FeatureEngine` (network engine) | EDR (position, locations, area, trajectory), WMS, Maps, Tiles, 3D Tiles, Features (site inventory) |
| QueryData | `EdrEngine` + `MapEngine` | EDR (position, area, radius, trajectory), WMS, Maps, Tiles; derived wind speed from u/v, direction on lat/lon areas (newbase's grid-relative convention); derived relative humidity and dew point from FMI 4/10/13 (#898) |
| Zarr | `EdrEngine` + `MapEngine` | EDR (position, area, radius, trajectory), WMS, Maps, Tiles; local + S3/HTTP; derived wind speed + direction from CF-identified u/v; derived geopotential height from CF `geopotential` (#948); derived relative humidity and dew point from CF-identified ingredients (`t2m`/`d2m` → `r2`, #898); values and units in display units from CF `units` and `standard_name` through `ds_core::units::cf_display_conversion` (°C, hPa, mm, mm/h, % for a CF fraction in unit 1; plain Zarr and Icechunk, #1213) |
| Nowcast | `MapEngine` + `FeatureEngine` + `EdrEngine` (derived: wraps another collection's engine) | WMS, Maps, Tiles — motion-extrapolated future frames; Features — tracked cell intelligence (severity, deviant movers, #544); EDR (area only) — the per-generation motion field as `motion_u`/`motion_v` m/s + `motion_quality` on a CoverageJSON Grid, generations as instances (#661). Reflectivity via EDR = #523 |
| PostGIS | `EdrEngine` + `FeatureEngine` + `MapEngine` | EDR (position, locations, area), Features; station shapes also WMS/Maps/Tiles when opted in (`[postgis.map]`, or `wms`/`maps` in `apis`): one symbol layer per parameter, each station a marker coloured by its latest value within `max_age` on `time_step` frames, most extreme on top (#1063); events shape: EDR (area) + WMS/Maps/Tiles (age-colored strike layer) |
| Satellite | `MapEngine` + `EdrEngine` | WMS, Maps, Tiles, EDR (position, area, radius) — geostationary imagery (GOES-R ABI NetCDF-4, Himawari-9 ISatSS tiles, GK2A AMI L1B and NOAA's hourly GMGSI global mosaic on AWS, or a local mirror); one parameter per band/product, each with its own time axis (`parameter_times`, `get_parameter_available_times`, #819). GMGSI serves 8-bit display counts (unit `"1"`) on a spherical-Mercator grid recognised from its 2-D lat/lon; RGB composites (`[[satellite.composites]]`) are layers of their own (`MapEngine::composites`), not parameters: WMS child layer `coll/<composite>`, Maps/Tiles `parameter-name=<composite>`, channel-list legend; EDR skips them |
| NetCDF | `MapEngine` + `EdrEngine` | WMS, Maps, Tiles, EDR (position, area, radius, trajectory incl. LINESTRING Z/M/ZM, cube) — CF-NetCDF gridded fields (MEPS per-lead files first) from a local directory, or read in place by HTTP range from a server found by template probing (thredds.met.no's fileServer: ≤ 8 requests per host, metadata blocks, compressed chunks, cold fills detached from the 3 s deadline); one layer per variable on one grid (1-D lat/lon, projected x/y with a CF `grid_mapping`, or a curvilinear grid of 2-D lat/lon placed by an inverse lookup over its nodes, #1174); runs from `forecast_reference_time` (`reference_times`, newest covering run, pinned runs never fall back); single-level fields, or fields on one pressure axis (`RasterInfo.vertical`: hPa, bottom first; WMS `ELEVATION`, Maps/Tiles `elevation`, EDR `z`; a map without one draws the bottom level, EDR without `z` reads every level; the MEPS `pl` family is a collection of its own). EDR samples each point, area or cube cell, or trajectory sample as the map draws it (one projection per point; CRS84 area and cube grids, a cube's native cells never coarsened), runs as instances, GetFeatureInfo through position at the level drawn; a query decodes at most `MC_NETCDF_EDR_QUERY_MB` and makes at most `MC_NETCDF_EDR_QUERY_READS` reads, both counted from its read plan before any read (a whole field its size, an OPeNDAP window its cells; a trajectory or cube counts only the fields it reads); through OPeNDAP a query reads only the windows it samples, a step's variables and levels in one request of up to 16 planes; derived wind speed from CF `x_wind`/`y_wind`, per pressure level (no direction on MEPS's Lambert grid); derived geopotential height from CF `geopotential`, `geopotential_pl` → `geopotential_height_pl` per level (#948); derived dew point and relative humidity from CF ingredients, `air_temperature_pl` + `relative_humidity_pl` → `dew_point_temperature_pl` per level, specific humidity on the pressure axis (#898); per-step amounts of run-start `_acc` accumulations in mm, e.g. `precipitation_amount` (#1165); values and units in display units from CF `units` and `standard_name` through `ds_core::units::cf_display_conversion` (°C, hPa, mm, % for a CF fraction in unit 1; geopotential as stored in m² s⁻²; #1209, #948) |
| BUFR | `EdrEngine` + `FeatureEngine` + `MapEngine` | EDR (locations, position, area, radius) over decoded SYNOP/SHIP station reports (in-memory, `retention` window); Features (station inventory: Point + last_report/report_count); WMS/Maps/Tiles station symbol layers when opted in (`[bufr.map]`, or any map API in `apis`; #1204), `content_version` = the metadata snapshot version. Sources: polled `data_path` or a WIS2 subscription (`[bufr.wis2]`; its store persists across restarts with `[server] state_dir`) |
| Station grid | `MapEngine` + `EdrEngine` + `FeatureEngine` (derived: wraps a CSV, BUFR or PostGIS stations collection's `EdrEngine`) | WMS, Maps, Tiles (one layer per parameter), EDR (position, area, radius, cube without `z`) — one time slice of the source's observations (latest per station within `max_age`, one area query) interpolated by IDW, ADW or nearest onto a CRS84 analysis grid (#1082); great-circle neighbours via a 3-D k-d tree (seam and poles); grids cached per (parameter, time, revision of its input) in `MC_STATION_GRID_CACHE_MB`, computed on a dedicated pool under the request deadline (503, never partial); `cadence` axis, or the source's `observation_instants` (CSV, BUFR; #1206); content_version moves with revised input fingerprints, which the poll re-checks for the latest `precompute` times and a bounded round-robin of older cached ones; EDR samples the grid the maps draw; Features (`features` in `apis`) — each station's leave-one-out residual per parameter and analysis time, computed with the grid and cached with it; `/items` never waits (a cold analysis is a 503 with `Retry-After`), no EDR `items`; optional lapse-rate correction over a terrain collection (`elevation_source`, −6.5 K/km by default only for a source-identified air temperature), a land/sea mask collection (`land_mask_source`, nodata outside in maps and EDR) and `exclude_where` station exclusions, the terrain and mask sampled once per engine by the poll (no analysis time until then) (#1206) |

## Config Format

```toml
[server]
host = "0.0.0.0"
port = 8000
# base_url = "https://api.example.com"  # optional, static fallback for absolute links behind a proxy
# trust_proxy_headers = true            # optional, per-request link base from proxy headers (default false)
# collections_dir = "collections.d"     # optional, directory of per-collection .toml files
# watch_collections_dir = true          # optional, auto-reload on collections_dir + colormaps_dir changes (default false)
# watch_debounce_ms = 500               # optional, coalesce-window for the watcher (default 500)
# render_concurrency = 24               # optional, render CPU slots shared by WMS/Maps/Tiles/3D Tiles
                                        # (default 2× CPUs, min 8; 1..=512). Fixed at boot: a reload
                                        # cannot resize the slots (logs a WARN), restart to change.
# rendered_cache_mb = 256               # optional, rendered-image cache (MB) shared by WMS/Maps/Tiles;
                                        # 0 disables. Unset: the largest deprecated per-collection
                                        # [wms] rendered_cache_mb among WMS/Maps/Tiles collections
                                        # (logs a WARN), else 256. Re-read on reload; an unchanged
                                        # size keeps the warm cache.
# colormaps_dir = "colormaps.d"  # optional, directory of palette files loaded as
                                 # named colormaps (name = file stem). Formats:
                                 # .toml (ColormapDef), GMT .cpt, GRLevelX .pal,
                                 # GDAL color-relief .txt/.clr, SLD .sld (ColorMap). Re-read on reload;
                                 # missing dir = hard error; other extensions skipped.
# state_dir = "/var/lib/meteocore"  # optional, engine state snapshots (#1000, #1002): a WIS2
                                 # CAP collection keeps its alert set in <state_dir>/<id>.cap.state,
                                 # a WIS2 BUFR collection its report store in <id>.bufr.state
                                 # (gzip JSON, ~8 MB global), across restarts. File-fed sources
                                 # re-read their files instead. Relative = to the config file's dir;
                                 # created at boot; needs a WRITABLE mount (a failed write is
                                 # a rate-limited WARN, never a load error). Unset = no
                                 # persistence. Fixed at boot (reload WARNs, restart to change).
                                 # Shared plumbing: ds_core::state — engines get an
                                 # Arc<dyn StateStore> + key "<id>.<kind>", never a path; the
                                 # StateWriter write policy is backend-independent — reuse
                                 # both, never hand-roll a writer (crates/server/CLAUDE.md).

# Optional Model Context Protocol endpoint at /mcp (off unless present and
# enabled). Top-level config.toml only. There is deliberately no default
# token: `enabled = true` with no resolvable token is a LOAD ERROR, because
# the alternative default would let a typo publish an unauthenticated tool
# surface over every Features collection.
[mcp]
enabled = true
token_env = "MCP_TOKEN"     # env var holding the bearer token (never inline)
# allowed_hosts = ["alt.example.com"]  # Host headers to accept BEYOND the
                            # loopback names and base_url's host. The
                            # transport 403s any Host it doesn't recognise
                            # (DNS-rebinding protection), so behind a proxy
                            # base_url must be correct or nothing works.
rate_limit_per_min = 60     # PER BUCKET — authenticated requests and failed
                            # auth attempts are budgeted separately, so the
                            # endpoint total is up to 2x this. 0 disables both.
# A reload can turn MCP OFF (a disabled endpoint 404s) and back on again, but
# cannot introduce it on a server that started without it — the route is built
# at boot. Enabling for the first time, and changing token_env or
# rate_limit_per_min, all need a restart.

# Optional named colormaps. Like [[style_bundles]], MUST live in top-level
# config.toml (rejected in per-collection files). Registered next to the
# built-ins; the name works anywhere a built-in colormap name does ([wms]
# colormap, styles, parameters, bundle defaults/extras). A user colormap may
# shadow a built-in name (replaces it deployment-wide, logged WARN);
# duplicate user names are a config error. Unknown colormap name references
# anywhere in config are a LOAD ERROR (no silent viridis fallback).
[[colormaps]]
name = "radar_nssl2"
title = "NSSL Reflectivity 2"
# interpolation = "step"        # "linear" (default) | "step" (discrete classes)
# nodata_color = "#00000000"    # optional
color_stops = [
  { value = 5.0,  color = "#414141" },
  { value = 60.0, color = "#FF0000" },
]

# Optional per-parameter default-style override rules, checked before the
# EMBEDDED defaults table (#320: parameters of multi-parameter collections,
# and a single-parameter collection's one layer, with no explicit style
# match built-in defaults — temperature palette for
# t2m/2t/TMP/dew point by unit K|C over one all-levels range, −90…50 °C =
# 183.15…323.15 K, pressure for msl by unit Pa|hPa, radar_dbz for
# DBZH, fraction twins of the humidity/cloud palettes for unit 1, etc. —
# BEFORE the collection-level colormap; opt out per collection with
# `[wms] parameter_defaults = false`). Top-level config.toml only.
[[parameter_defaults]]
names = ["td", "dewpoint_2m"]   # exact matches (normalized: lowercase alnum)
contains = ["dew_point"]        # substring matches vs name and title
colormap = "temperature"
[[parameter_defaults.unit_ranges]]
unit = "K"                      # any spelling (ds_core::units::canonical_unit):
                                # "kelvin" matches too, "m/s" matches "m s-1"
min = 183.15
max = 323.15

# Optional shared WMS style bundles. MUST live in top-level config.toml —
# a [[style_bundles]] block inside a collections_dir file is silently
# dropped by serde and any collection referencing it will fail validation.
[[style_bundles]]
id = "radar_multi"
[style_bundles.default]
colormap = "radar_dbz"
[[style_bundles.extras]]
colormap = "radar_fmi"   # name defaults to the colormap name, title to the
                         # palette's title; both can be set explicitly

[[collections]]
id = "weather"
title = "Finnish Weather Observations"
description = "Hourly weather observations"
data_path = "testdata/weather.csv"
apis = ["edr", "features"]     # defaults to ["edr"]; add wms/maps/tiles for station symbol layers
engine_type = "csv"             # defaults to "csv"
keywords = ["weather", "observations", "Finland"]   # optional discovery metadata
# [collections.csv.map]         # optional station symbol layers (#1204); same keys as
# max_age = "PT1H"              # [postgis.map]/[bufr.map]: max_age, time_step = "PT10M",
#                               # marker_size = 6, draw_missing = false
[collections.license]
title = "CC-BY-4.0"   # required; an SPDX id (no spaces). With no `url`, the
                      # link auto-resolves to https://spdx.org/licenses/<id>.html
# url = "https://creativecommons.org/licenses/by/4.0/"   # optional explicit override
# type = "application/pdf"   # optional media type of `url` (default text/html)

[[collections]]
id = "radar"
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[collections.geotiff]
filename_template = "radar_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
nodata = 255
endpoint = "https://s3.example.com"
bucket = "radar-data"
prefix_pattern = "%Y/%m/%d/"
time_window = "-PT2H"
max_files = 24

[collections.wms]
# Attach a shared style_bundle and/or set inline fields — they merge
# slot-wise (bundles v2), inline winning each slot it defines (palette
# source, min, max; named styles union with inline winning name clashes).
style_bundle = "radar_multi"
# colormap = "radar_dbz"
# webp_quality = 80      # default WebP quality for WMS/Maps/Tiles, 1-100:
                         # 1-99 lossy, 100 (= unset) lossless. A request's
                         # QUALITY / quality wins. For continuous-tone layers
                         # (satellite IR, NWP fields); keep discrete radar
                         # classes lossless.

# Optional /preview SPA tuning: cap the time slider's `values[]` to the most
# recent ISO 8601 duration before the latest timestep. Manifest-only; does
# NOT constrain the underlying engine.
[collections.preview]
time_window = "PT12H"

# grib / querydata / zarr / netcdf collections derive wind speed (and, where the
# source states the u/v frame, direction) from u/v components (#897).
# grib / zarr / netcdf collections derive geopotential height from
# geopotential where they have none at that level (#948).
# grib / querydata / zarr / netcdf collections derive relative humidity and
# dew point from their ingredients where they have none at that level (#898).
[[collections]]
id = "nwp"
engine_type = "grib"
apis = ["edr", "wms", "maps", "tiles"]
derive_wind = false     # opt out (default on); a load error on other engines
# derive_geopotential_height = false   # likewise; querydata rejects it
# derive_humidity = false              # likewise
[collections.grib]
data_path = "testdata/grib-local"

# Derived nowcast collection (#519): motion-extrapolates another collection's
# frames into the future. `source` must be a non-derived collection in the
# same config with wms/maps/tiles enabled.
[[collections]]
id = "radar-nowcast"
engine_type = "nowcast"
# "features" is what serves the tracked-cell layer — and what makes the
# collection visible to the MCP tools, which read the Features registry.
# "edr" serves the motion field itself (area query → motion_u/motion_v in
# m/s + motion_quality; one generation = one instance) for particle clients.
apis = ["wms", "maps", "tiles", "features", "edr"]

[collections.nowcast]
source = "radar"        # collection id to extrapolate
horizon = "PT2H"        # how far into the future (default PT2H)
# step = "PT5M"         # timestep spacing (default: source cadence)
# lightning_source = "lightning"  # events-shape postgis collection: per-cell
                                  # flash_count/rate + lightning_jump (#549)
# impact_source = "municipalities"     # any polygon `features` collection:
                                       # per-cell impact_over / _approaching /
                                       # _eta_minutes + the `impact` rank term
# impact_name_property = "name"        # display-name property (default "name")
# impact_weight_property = "population" # optional numeric property; log-weights
                                        # exposure. Omit ⇒ purely geometric
# radar_source = "fi-radar-pvol"  # odim-volume collection: per-cell nearest
                                  # radar, range, lowest-beam height (#642)

# Optional per-term significance weight overrides for tracked storm cells.
# Defaults live in ds_core::cell_facts::DEFAULT_CELL_WEIGHTS; omitted terms
# keep theirs. An unknown term name is a LOAD ERROR (no silent fallback).
# A term the cell has no data for drops out of the weighted mean entirely,
# so the same table works before and after a new source is wired.
[collections.nowcast.significance]
impact = 1.5        # deliberately the largest by default: a moderate cell
                    # closing on a town outranks a very severe cell at sea
severity = 1.0
lightning_jump = 0.9

[collections.wms]
colormap = "radar_dbz"

# CF-NetCDF gridded data (#1067): one file family, in a local directory or,
# with an http(s) data_path, read in place by HTTP range from a server found
# by template probing (collections.d/meps-netcdf-thredds.toml.disabled). Valid
# times come from each file's `time`, runs from `forecast_reference_time`. A
# family on pressure levels (MEPS `pl`, collections.d/meps-netcdf-pl-local.toml)
# is a collection of its own: its levels are the vertical axis.
# See crates/engine-netcdf/CLAUDE.md.
[[collections]]
id = "meps-netcdf-local"
engine_type = "netcdf"
apis = ["edr", "wms", "maps", "tiles"]

[collections.netcdf]
data_path = "testdata/meps-netcdf"
filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"  # default: *.nc
variables = ["air_temperature_2m", "precipitation_amount_acc"]  # default: all on the grid
max_runs = 4                      # newest runs kept
# max_files = 1024                # most files kept open; member = 0 (ensemble axis)
# Remote instead: data_path = "https://thredds.met.no/thredds/fileServer/meps25epsarchive/"
# prefix_pattern = "%Y/%m/%d/{run}/member_00/"   # {run}: the run hour
# run_hours = [0, 3, 6, 9, 12, 15, 18, 21]       # required
# leads = [0, 1, 2]                              # {lead}; the first is the sentinel
# time_window = "-PT12H"                         # required
# min_age = "PT10M"; run_grace = "PT30M"         # settling (defaults)
# prewarm_variables = []                         # newest run fetched ahead, ~100 MB/variable
[collections.netcdf.titles]
precipitation_amount_acc = "Precipitation accumulated since run start"

# BUFR surface observations (SYNOP/SHIP station reports) — polled directory or
# object-store prefix of *.bufr files; a WIS2 subscription ([bufr.wis2]) is
# the alternative source. See crates/engine-bufr/CLAUDE.md.
[[collections]]
id = "obs-bufr-local"
engine_type = "bufr"
apis = ["edr", "features"]

[collections.bufr]
data_path = "testdata/bufr-synop"   # XOR with [collections.bufr.wis2]
poll_interval_secs = 60
retention = "PT24H"            # rows older than this are pruned (default)
max_stations = 50000
stale_after = "PT2H"           # wis2 mode: no notification this long ⇒ degraded
position_radius_km = 25.0      # EDR position → nearest station within this
# builtin_parameters = true    # SYNOP essentials; [[collections.bufr.parameters]]
#                              # entries override by name or add (descriptors =
#                              # ["013011"], unit, period_hours for accumulations)
# [collections.bufr.wis2]      # push instead of poll; most SYNOP arrives inline
# topics = ["cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop"]
# warmup = "PT24H"             # default = the shorter of retention and PT24H: after a
#                              # cold start /health says "warming up after cold start:
#                              # N reports received" this long (a longer retention is
#                              # still filling its older part after it); with [server]
#                              # state_dir the store survives restarts (a snapshot older
#                              # than warmup keeps the rows still inside retention but
#                              # restarts the warm-up)
# [collections.bufr.map]       # station symbol layers (#1204) when apis lists a map API:
# max_age = "PT1H"             # max_age, time_step = "PT10M", marker_size = 6, draw_missing

# CAP warnings pushed over WMO WIS2 (third cap source mode, XOR with
# data_path / feed_url). `[….wis2]` is the shared ds-wis2 subscription
# block (same shape for the coming engine-bufr); see crates/ds-wis2/CLAUDE.md.
[[collections]]
id = "cap-meteoalarm-wis2"
engine_type = "cap"
apis = ["features", "wms", "maps", "tiles"]

[collections.cap]
poll_interval_secs = 60     # forced rebuild cadence in wis2 mode (min 5)
language = "en"
retention_grace = "PT1H"    # keep an alert this long past <expires>
max_alerts = 10000
geometry_links = true       # MeteoAlarm rel=geometry zone polygons (fetched on arrival)
bbox_fallback = false       # never draw the notification bbox as the area

[collections.cap.wis2]
topics = ["cache/a/wis2/eu-eumetnet-warnings/data/core/weather/advisories-warnings"]
# broker = "mqtts://globalbroker.meteo.fr:8883"   # default
# username = "everyone"; password = "everyone"   # defaults (public); or password_env
# download_allowlist = ["https://…/"]            # strict mode (recommended for origin/ topics)
# warmup = "PT24H"   # default; after a cold start (no state_dir snapshot restored)
                     # /health says "warming up after cold start: N alerts received"
                     # (degraded) this long after the subscription first comes up;
                     # a snapshot older than this restores its alerts but restarts
                     # the warm-up ("warming up after a long outage").
                     # [bufr.wis2] has it too, defaulting to the shorter of the
                     # collection's retention and PT24H.
```

See config struct definitions in each engine crate and `ds-core/src/config.rs`
for all fields.

### Per-file collection configs (`collections_dir`)

Collections can be individual `.toml` files in a directory (one collection per
file, no `[[collections]]` wrapper) instead of — or in addition to — inline
`[[collections]]` in `config.toml`:

```toml
# collections.d/radar-opera.toml
id = "radar-opera"                 # `id` is required (not derived from filename;
title = "OPERA Radar Composite"    #  a filename/id mismatch logs a warning)
engine_type = "geotiff"
apis = ["edr", "wms", "maps", "tiles"]

[geotiff]
filename_template = "OPERA@%Y%m%dT%H%M@0@ACRR.tiff"
parameter = "reflectivity"
unit = "dBZ"

[wms]
colormap = "radar_dbz"
```

Rules:

- Inline collections load first, then directory files sorted alphabetically by
  filename. Duplicate IDs across sources are rejected.
- Only `.toml` files load; rename to `.toml.disabled` to disable. Non-recursive.
- Missing directory = hard error; empty directory = valid but logs a warning.
- A single invalid file rejects the entire config (no partial loads).
- `[[style_bundles]]` is NOT allowed in per-collection files — only in
  `config.toml`. Referencing a bundle from a per-collection `[wms]` is fine;
  defining one is rejected with an explicit error.
- A `style_bundle` MERGES with inline `[wms]` fields (bundles v2): per
  parameter and per slot (palette source / min / max), the chain is inline
  `[[wms.parameters]]` → bundle `[[style_bundles.parameters]]` → inline
  `[wms]` fields → bundle default. Named styles are the union of bundle
  extras and inline `[[wms.styles]]`, inline winning name clashes. A bundle
  carries shared per-parameter defaults via `[[style_bundles.parameters]]`
  (same fields as `[[wms.parameters]]`; names must be unique per bundle).
- Inside a bundle, an `[[style_bundles.extras]]` entry with a `parameter`
  field is scoped to that parameter's layer only; untagged extras are shared
  across every parameter layer. A `parameters = [...]` list, in extras and
  in `[[wms.styles]]`, scopes one style to several parameter layers and
  keeps it off the base layer; `parameter` and `parameters` are mutually
  exclusive. Style names are unique per collection, so the list is how one
  shared bundle offers a style on every model's temperature layers.
- Hot reload (`POST /admin/collections/reload`) picks up added/removed/changed
  files. The optional filesystem watcher and its trust model are documented in
  `crates/server/CLAUDE.md`.

### Collection keywords & license

Two optional per-collection discovery fields (any engine), surfaced across
every API from the shared `CollectionConfig`:

- **`keywords`** — flat array of non-empty strings. Emitted as OGC Common
  Part 2 `"keywords"` in collection JSON, `<KeywordList>` in WMS
  GetCapabilities (after `<Abstract>`, WMS 1.3.0 schema order), chips on HTML
  pages, and matched by `/collections?q=` (whole-word or phrase).
- **`[collections.license]`** — `title` required (human name or SPDX id) +
  optional `url`. A plausible SPDX id with no `url` synthesizes
  `https://spdx.org/licenses/<id>.html`. Rendered as a `rel="license"` JSON
  link, `<Attribution>` in WMS (after `<Dimension>` elements), and an HTML
  link. A free-text title with no `url` shows its name but produces no JSON
  link (a link object requires an `href`). The JSON link's `type` is
  `text/html` or the optional `type` of a configured `url` (EDR 1.2 requires
  a `type` on every link, #964).

Both validated at config load: empty keyword entries, empty license title, a
non-http(s) license URL, or a license `type` that is not a media type or has
no `url` are rejected.

### Collection themes & discovery records (#1356)

An experiment for the OGC MetOcean Common profile: a WIS2 topic as a
`themes` concept, plus a link to the collection's WCMP2 discovery record.

- `[collections.discovery]` (`[discovery]` in a collections.d file):
  - `wis2_topic` becomes `themes`: one theme with scheme
    `https://codes.wmo.int/wis/topic-hierarchy/earth-system-discipline` and
    one concept per topic, at any level of the hierarchy.
  - `wcmp2_record` (`urn:wmo:md:…`) becomes one `describedby`
    `application/geo+json` link into the catalogue.
  - Topics are validated at load against
    `crates/core/assets/wis2-topic-hierarchy-<ver>/`, which is regenerated
    only by `scripts/vendor-wis2-topic-hierarchy.py`.
- WIS2-subscribed collections derive their topics from their subscriptions.
  An engine that gains a WIS2 source adds its subscriptions to
  `ds_core::discovery::topics`.
- Build theme JSON only through `ds_core::discovery::themes`, and descriptions
  only through `api_common::collection_metadata`, so the encoding stays in
  one place.
- The top-level `[discovery] catalogue` (GDC base URL, default
  `https://wis2-gdc.weather.gc.ca`) is never part of `CollectionConfig`. It
  reaches descriptions as the `Discovery` in each API state, a
  `load_collections` argument taken from `AcceptedLoad`. A reload that
  changes only the catalogue therefore rebuilds no engine.
- WIS2-fed collections learn the records their notifications name in
  `metadata_id`. They go into the server's one
  `ds_core::discovery::LearnedRecords`, held as
  `ServerState::learned_records` and in every load's `Discovery`.
  - Each id must parse as a `RecordId`. A collection keeps at most 64, keyed
    by its own id, and they are not persisted.
  - A configured `wcmp2_record` replaces the learned ones.
  - An accepted reload drops the records of collections it removed, or that
    no longer subscribe to WIS2.
  - A new WIS2 engine embeds `ds_wis2::RecordLearner` and calls `learn` on
    each notification it applies. `load_collections` connects it through
    `learn_records_into`, and its subscription goes in
    `discovery::subscriptions`.

## Admin & Operations

- **Reload:** `POST /admin/collections/reload` — re-reads config, atomically
  swaps engines. The shared core `admin::do_reload` is also driven by the
  optional `collections_dir` watcher. Reload is **incremental** (#574):
  collections whose `CollectionConfig` is unchanged keep their live engine
  (poll loop untouched, no re-bootstrap); only added/changed/removed ones
  rebuild, and their cached tiles are evicted per collection. See
  `crates/server/CLAUDE.md` for the rules.
- **Health:** `GET /health` — per-collection ready/degraded/failed status.
  HTTP 503 only when all collections failed. `data_age_secs` (and the
  `collection_data_age_seconds` gauge) is now minus the newest data a
  collection serves, never the time since its last poll (#1007); see
  `crates/server/CLAUDE.md`. A collection holding no data has no age and
  `collection_has_data` 0 (#1055).
- **Metrics:** `GET /metrics` — Prometheus format. Path labels use route
  patterns, never raw URLs (cardinality explosion).
- **Memory plan:** `GET /admin/cache-plan` (behind `ADMIN_TOKEN`) lists
  every cache and budget with its size and size source, the same list the
  INFO block at boot and after each accepted reload logs (#1306). Sizes
  from a config key with a serde default are inferred by
  `SizeSource::unless_default` until #1313 makes the keys `Option`.
  Memory outside the caches is metered too (#1307): each in-memory store
  keeps an estimate of its heap through `ds_core::heap` (`HeapSize`),
  computed when it changes and read O(1) by `/metrics`; a new store adds its
  `*_bytes` gauge and a test against `ds_core::heap::Counting` (within
  25 %). Budgets hand the plan a high-water mark that only `/metrics` takes
  (`plan::take_peaks` → `<budget>_reserved_peak_bytes`).
  Each cache row also carries `churn` and a `last_hour` label (#1308):
  `idle`, `thrashing`, `scan`, `churning` (no ghost ring to tell) or
  `healthy`, with the hit ratio at 1.5, 2 and 3 × the size; the Grafana Cache
  churn row mirrors it. A cache whose engines publish a demand (#1309) also
  carries `demand_bytes`, `demand_rule`, `demand_kind`, `suggested_bytes`
  and `demand_inputs`; `/metrics` has `cache_demand_bytes` and
  `cache_demand_ratio`, one series per such cache, labelled as
  `cache_size_source` is.
- **Grafana dashboards live in this repo:**
  `docker/grafana/dashboards/meteocore-overview.json`, provisioned via
  `docker/grafana/provisioning/`: a `dashboard.grafana.app/v2` resource
  that needs Grafana 13 or newer. Panels are `spec.elements`, named
  `panel-<id>`; `spec.layout` places each exactly once. Tabs: Overview
  (six stats and four time series; keep it that small), Requests &
  latency, Rendering & caches, Memory & admission, Storage & sources,
  Engines (a sub-tab per engine), Logs. Rows inside a tab stay open
  (`collapse: false`). Every PromQL query carries `job=~"$job"`;
  per-collection metrics also `collection=~"$collection"`. Queries name
  `${DS_PROMETHEUS}` or `${DS_LOKI}` as their datasource; a link into
  another tab carries `dtab=<slug>`.
  **When adding a new `/metrics` family, update the dashboard in the same
  PR**, in the tab and row its question belongs to. `python3
  scripts/check_grafana_dashboard.py` enforces this in CI: it fails on a
  registered family no panel queries, on a PromQL name that is not a
  family, on broken layout, variables, datasources, hosts or links, and
  on anything Grafana's v2 schema (vendored unchanged in
  `scripts/grafana/`) rejects or would silently drop. A cache
  named `<name>_cache_{hits_total,misses_total,bytes,capacity_bytes}`
  needs no panel: the `cache` variable feeds it to the cache table and
  the repeated card of hit ratio, fill and miss rate. Add a per-collection
  cache to the Engines › Gridded sources regex, and a memory budget
  `<name>_{reserved_bytes,capacity_bytes,rejected_total}` to the
  admission-budget panels in Memory & admission.
  A family labelled by `collection` registers through
  `admin::per_collection`, so a reload that removes the collection drops
  its series (`every_family_with_a_collection_label_is_pruned_on_reload`).
  The Helm chart (`charts/meteocore`, #1365) also ships the dashboard,
  compacted in a ConfigMap of at most 1 MiB; its workflow fails above
  900 KB rendered.
- **Alert rules live in this repo too:** `docker/prometheus/rules/meteocore.yml`
  (#1375), loaded by the compose stack and shipped by the chart as a
  PrometheusRule (`prometheusRule.enabled`, through a symlink; `helm.yml`
  checks the rendered rules equal the file). Every selector names
  `job="meteocore"`, which the chart replaces. Each alert has a firing and a
  quiet case in `meteocore.test.yml`; CI runs `promtool check rules` and
  `promtool test rules`. The README's Alerting section lists the alerts and
  thresholds: change both together.
- **State:** API state wrapped in `ArcSwap` for lock-free reads. Render
  semaphore (`[server] render_concurrency`, default 2× CPU cores, min 8;
  fixed at boot, so restart to change) shared across Maps/Tiles/WMS. Engine
  loading lives in `server/src/admin.rs`.

## Code Style

- Use `thiserror` for error types, not manual `impl Display`.
- Engine methods return `Result<T, DataServerError>`.
- Keep handlers thin — delegate logic to the engine, map errors to HTTP
  status codes.
- Use `serde_json::json!` for building JSON responses.
- Do not leak internal error details to clients — generic messages for 500s.
- Public ds-core domain structs (`RasterInfo`, `CollectionConfig`, …) are
  built via cross-crate struct literals by engines and tests. Do NOT add
  `#[non_exhaustive]` to them. Adding a field means patching every literal
  (grep the field name, then `cargo check --workspace --tests`).
