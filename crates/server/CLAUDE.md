# server crate — Claude Instructions

The binary. Config loading, engine wiring (`src/admin.rs`), CLI parsing
(`src/main.rs`), auto-collections (`src/auto.rs`), the background poll
runtime, reload, health, metrics. Read the root `CLAUDE.md` first — Critical
Rules 6 and 7 (poll runtime, ds-storage bridge) are enforced here.

## CLI flags

Hand-rolled parsing (no `clap`) in `parse_cli_args` (`src/main.rs`). Every
flag also accepts `--flag=value`.

- `--collections <id1,id2,…>` — only load these collection IDs.
- `--host <HOST>` / `--port <PORT>` — override `[server].host`/`port` (CLI
  wins over config). `BASE_URL` env still wins for link generation.
- `--config <PATH>` — config file path (wins over `CONFIG_PATH` env, then
  `./config.toml`). A missing `--config` path is a hard error.
- `--auto-collections <DIR>` — auto-discover collections from a directory
  tree (repeatable); see below.

**No-config boot:** if the default config path is absent AND no `--config` is
given, the server starts from built-in defaults — host `127.0.0.1`, and it
auto-scans for the first free port at/above 8000 (up to 100 ports). A port
pinned by config or `--port` does NOT auto-scan: a bind conflict is fatal.
Combine with `--auto-collections` for a zero-config
`server --auto-collections ./data`.

## Auto-collections (`src/auto.rs`, #411 phase 1)

`--auto-collections <DIR>` synthesizes `CollectionConfig`s from data files on
disk, no TOML needed.

- **Mapping:** per-subdirectory + loose files. Each immediate subdir → one
  collection; loose files in the root → grouped under the root name. The root
  may itself be a Zarr store.
- **Detection (first match wins):** zarr store
  (`zarr.json`/`.zgroup`/`*.zarr` name) → `zarr`; `*.sqd` → `querydata`;
  `*.grib2` + index sidecars → `grib` (`.idx` → wgrib2, `.index` →
  ecmwf-json; **no index ⇒ skipped** — the engine never builds indexes);
  `*.tif`/`*.h5` → phase 2, skipped (need filename-template inference + ODIM
  COMP/PVOL probe); `*.geojson` and `*.csv` → one collection per file.
- **APIs:** each collection enables ALL APIs relevant to its type (mirrors
  the `engine_type → supported_apis` allowlist): raster/grid
  (zarr/grib/querydata) get edr+wms+maps+tiles, csv gets edr+features
  (its station symbol layers, #1204, stay opt-in although the allowlist
  accepts wms/maps/tiles), geojson gets features+tiles. Rendering works without a `[wms]` block: each
  parameter layer takes its built-in default when its name and unit match
  one, and a multi-parameter collection's root layer draws its default
  parameter's layer (#1161). A root layer on the collection's own style
  layer with no palette configured takes the built-in default of the
  engine's `RasterInfo.parameter`/`unit` (`default_generic_root_layers`,
  after every registry is built: the style builders don't see the engine).
  Otherwise generic viridis `0..1` until a colormap/min-max is set (#320);
  the load WARNs once per map collection only when the root layer, resolved
  as requests resolve it (`generic_root_layer`), still draws that fallback.
- Synthesized configs are appended to `config.collections` and run through
  the same `ServerConfig::validate()` as TOML (duplicate ids rejected).
- Resolved once at startup — reload does NOT re-scan auto roots (v1).
- Engine-config defaults come from `{QueryData,Grib,Zarr}Config::auto_*`
  constructors in `ds-core` — reuse the serde default fns, keep them DRY.
- **Symlinks are followed** (`is_dir`/`is_file` resolve them). Same trust
  model as `collections_dir`: whoever can write the scan root controls what
  is served.
- **Temporary names are skipped (#1055):** hidden folders (rsync's
  `.~tmp~`) and files `ds_core::temp_files::is_temporary` names, in the root
  and each folder; the root itself is never judged. A hidden `.name.geojson`
  would otherwise clash with `name.geojson`'s id and fail the config.

## Reload & the collections_dir watcher

- `POST /admin/collections/reload` re-reads config and atomically swaps
  engines. Shared core: `admin::do_reload`.
- **Reload is incremental (#574).** Validation stays all-or-nothing (any
  config error rejects the whole reload, live registry untouched), but the
  build phase diffs per collection id against the last accepted config
  (`ServerState::last_collections`, derived `PartialEq` on
  `CollectionConfig`; a GRIB or GeoTIFF config changed only in its cache
  sizes counts as unchanged when its engine can take them in place, below):
  - **Unchanged → the live engine `Arc` is reused verbatim** (from
    `ServerState::engine_handles` via `EngineReuse`): poll loop keeps
    running, catalog/caches stay warm, no remote re-bootstrap. Poll-loop
    rotation is by `Arc` identity (`diff_by_identity`): reused engines are
    neither `shutdown()` nor re-spawned (either would break — #442).
  - **Cache sizes only (#1311):** a GRIB config differing only in
    `grid_cache_mb` / `message_cache_mb` / `point_index_cache_mb`, or a
    GeoTIFF one only in `tile_cache_mb`, reuses its engine when
    `resizes_caches_in_place` (`cache_sizes_only`). Its `apply_cache_sizes`
    and its `cache_plan::register_*` are staged in `LoadResult::cache_resizes`,
    not run in `load_collections`: a rejected reload leaves its caches and
    the plan as they were. A cache turned on or off, or one that would stop
    behaving like a cache built at its new size, rebuilds as before.
  - **Render caches are always reused (#1311)**, rendered, meta-tile and
    MVT, when `resizes_in_place_to` the new size; their resize is staged
    too. Built at their configured size, never with a ceiling: quick_cache
    already gives the most shards that keep 32 entries each. A style
    fingerprint change still drops them, and a disabled cache grown, or a
    size too far from the old one, is built anew.
  - `apply_load` takes the staged resizes after the ready guard accepts the
    load and runs them after the per-collection eviction, so a shrink keeps
    the warm entries of reused collections, not those of rebuilt ones. A
    test calling `load_collections` directly applies them itself
    (`apply_cache_resizes`).
  - **Exceptions:** `csv`/`geojson` always rebuild to reread changed files.
    A station grid keeps its engine only when its source, `elevation_source`
    and `land_mask_source` collections keep theirs (#1082, #1206).
    A nowcast whose own collection config and raster source are unchanged can
    retain its live engine, forecasts, cell history and track IDs (#604).
    Auxiliary lightning/impact/radar sources are ALL revalidated against the
    new base registries, even on reuse. The engine also checks source identity
    and the retained geometry/product contract; a rebuilt raster source or a
    changed contract forces a fresh nowcast. Equal geometry alone is not proof
    of the same source data.
  - **Staged nowcast updates:** `LoadResult` holds `DependencyUpdate`s, applied
    only AFTER the ready-collection reload guard accepts the load and BEFORE
    rotating poll loops. Never mutate a live nowcast during `load_collections`:
    a rejected load must leave its dependencies untouched. Each generation
    captures one auxiliary snapshot; in-flight work finishes with the old
    snapshot, and the next generation uses the new one. Existing products are
    not rewritten at the same anchor. Missing/wrong-kind configured sources
    still fail load validation; removing an optional join requires updating
    the nowcast config too. Chaining and declaration-order restrictions stay.
    Cache eviction follows actual engine reuse, including a candidate that
    the engine's compatibility guard refused.
  - **Cache hygiene:** previously-live collections that were NOT reused get
    their entries evicted from the rendered/meta-tile/vector-tile caches
    (`evict_collection`; matches `{id}`, `{id}/param`, `{id}-derived`).
    Global style inputs ([[colormaps]], bundles, parameter_defaults,
    colormaps_dir bytes) still drop the rendered/meta-tile caches wholesale
    via `style_fingerprint`; per-collection `[wms]` edits are deliberately
    NOT in that fingerprint — they rebuild + evict just their collection.
  - Style-only global changes rebuild NO engines: styles re-resolve for every
    collection on each load, reused engines included.
- With `[server] watch_collections_dir = true` (default off), a filesystem
  watcher (`notify`) triggers the same reload on add/edit/remove — debounced
  (`watch_debounce_ms`, default 500), running on the background
  `poll_runtime`, keeping the live registry if the new config is
  invalid/empty. The same flag also watches `colormaps_dir` when set (#571):
  a palette edit runs the full reload, which rebuilds the palette registry
  and drops the rendered/meta-tile caches via `style_fingerprint`. Under
  `colormaps_dir` there is NO extension filter (palettes span
  .toml/.cpt/.pal/.txt/.clr/.sld and `.disabled` renames must retrigger);
  `collections_dir` events keep the `.toml`-name filter. Watch roots are
  resolved once at boot — re-pointing either dir needs a restart.
- **Filter watcher events by `event.kind`, not path alone.** notify's inotify
  mask includes IN_OPEN; do_reload itself reads every `.toml` and every
  palette file, so a path-only filter creates a self-sustaining reload loop
  (#424). Drop `Access(Open/Read/Close(Read))` and `Modify(Metadata)`; keep
  create/remove/rename/data-write/CLOSE_WRITE.
- **Trust model:** the watcher's reload is authorized by filesystem write
  access to the watched directories (`collections_dir`, `colormaps_dir`), NOT
  the HTTP `ADMIN_TOKEN` that gates the reload endpoint — different control
  planes. Anyone who can write a collection file already controls what the
  server serves; anyone who can write a palette file controls served styles.
  When the watcher is enabled and `ADMIN_TOKEN` is set, a startup WARN makes
  the asymmetry explicit, naming the watched dirs. Keep them writable only by
  trusted principals; avoid shared/NFS mounts for them.

## Derived parameters (#897, #948, #898)

`derived_wrapped` registers grib/querydata/zarr/netcdf engines (each GRIB
level view too) wrapped in `ds_core::derive::Derived::with`, with the
collection's `Derivations`: wind unless `derive_wind = false`, geopotential
height (grib/zarr/netcdf) unless `derive_geopotential_height = false`,
relative humidity and dew point unless `derive_humidity = false`; with all
off the engine registers unwrapped. Styles are resolved from the wrapper's
`RasterInfo`, so a derived speed layer gets the built-in `wind_speed`
default, a derived height the `gh` default, a derived relative humidity
`humidity` and a dew point the temperature default. The wrapper logs one
INFO line per candidate (`Collection 'x': wind u/v: …`,
`Collection 'x': geopotential height z: …`, `Collection 'x': humidity 2t:
…`) at load and when its outcome changes. Wrappers live in
`LoadResult::wind_by_id` / `ServerState::wind_handles` (configured id →
registered id): a reused engine keeps its wrappers through `EngineReuse::wind`,
so the registered `Arc` is the same after an incremental reload (a nowcast
compares its source by identity) and the load log is not repeated.

## Reverse-proxy base URL (`trust_proxy_headers`, #12)

Absolute self-links (landing pages, `collections`, GeoJSON `links`, WMS
GetCapabilities OnlineResource/legend URLs, 3D Tiles docs, …) are built from a
base URL. By default it is resolved once at startup: `BASE_URL` env >
`[server] base_url` > `http://{host}:{port}` — wrong behind a reverse proxy.

With `[server] trust_proxy_headers = true`, the base is resolved per request:

1. RFC 7239 `Forwarded` — `proto=`/`host=` of the **last** element (the
   closest trusted proxy; the first element is client-injectable via RFC 7239
   §4 append semantics),
2. `X-Forwarded-Proto` + `X-Forwarded-Host` (+ optional `X-Forwarded-Port`),
3. the static fallback above.

The pure resolver is `ds_core::proxy::resolve_base_url` (framework-free; axum
handlers pass header values via a closure). Each API crate calls it through a
small `request_base_url` helper.

**Security:** default false because forwarding headers are
client-controllable. Even when enabled: host values are sanitised
(whitespace/slashes/`@`/non-ASCII rejected), scheme restricted to
http/https, only the first value of a comma list used; malformed headers fall
through to the next source. Enable only when a trusted proxy sets/overwrites
these headers, the proxy strips `Forwarded`/`X-Forwarded-*` on untrusted
client requests, and clients cannot reach the backend directly — otherwise a
client could spoof the emitted self-links (open-redirect risk downstream).

## Request log (`request_logging_middleware`)

One INFO `request` line per HTTP request (`admin::request_logging_middleware`;
fields listed in its doc comment and the README's Structured Logging section).
`user_agent` and `referer` (#995) are client-controlled: each is decoded
lossily, cut to `MAX_LOGGED_HEADER_LEN` (256) bytes on a char boundary, and
logged as `""` when absent, like `query`. The referer drops everything from
the first `?` or `#`, since the query and fragment can carry tokens. Both stay
log-body fields: `docker/alloy/config.alloy` must never promote them to Loki
labels (unbounded cardinality). The client IP is deliberately not logged
pending a privacy decision (#995).

## Render admission and deadlines

`ds-executor` owns process-wide render slots and the waiting queue, preserved
across registry reloads. The slot count is `[server] render_concurrency`
(1..=512, validated at config load; default 2× CPUs, min 8), fixed by
`admin::init_render_concurrency` in `main` before the first
`load_collections`, which is the first reader of the slots — keep that order.
A reload cannot resize the slots: a changed value logs a restart WARN, and
`render_semaphore_total` keeps reporting the live count.
`MC_RENDER_QUEUE_CAPACITY` defaults to 3× slot count;
full queues shed immediately with 503 + Retry-After, and cached bytes bypass it.
`MC_RENDER_TIMEOUT_MS` defaults to 3000 for raster work and MVT encoding. The
absolute deadline includes memory and semaphore waits, blocking-pool dispatch, engine
read and normal encoding. 3D points/meshes share the queue with a 30 s compute
budget; dedicated voxel execution stays separate. Synchronous CPU work cannot
be preempted: it retains its CPU/memory permits after timeout/disconnect until
completion. Pending blocking jobs are aborted before they start.

`render_queue_depth`, `render_queue_capacity`, `render_queue_rejected_total`,
and `render_deadline_exceeded_total{stage}` are exposed in /metrics and
Grafana. Immediate 503s are `render_queue_rejected_total` plus
`render_budget_rejected_total{reason="oversize"}`; expiries are
`render_deadline_exceeded_total`, `stage` `queue` (waiting for memory or a
slot) or `render` (after admission), and an expired memory wait is also
`render_budget_rejected_total{reason="deadline"}`.
`render_duration_seconds{api,collection,outcome}` (#466) is fed by the
`ds_executor::RenderTiming` extension in `metrics_middleware`; `apply_load`
drops the series of `(api, collection)` pairs a reload removes (#446). The
same extension's phases feed `render_phase_seconds{api,phase}` (#147):
queue, engine, assemble (WMS meta-tiles), encode, one sample per phase a
served render ran. No `collection` label: queue wait belongs to the shared
slots, the per-collection tail is `render_duration_seconds`, and fixed labels
need no reload pruning.

## Memory plan (#1306)

- `cache_plan.rs` owns `/admin/cache-plan`, the INFO block and the plan
  gauges, all from `cache_plan::snapshot`. Both the endpoint and the block
  call `admin::refresh_metrics` first: a process-wide cache exists only
  once read. The snapshot drops collection- and source-scoped entries whose
  id is not in `last_collections` (a replaced engine a request still holds).
- `register_render_caches` runs after every accepted load (`main`,
  `apply_load`, the test `build_state`). Per-collection caches register in
  `load_collections` right after their engine is pushed, reused engines
  too, except a reused GRIB or GeoTIFF engine, which registers in its
  staged `cache_resizes` closure once the load is accepted (#1311);
  registering an owner again replaces its entry.
- A recovery-loop reload logs the block only when the plan changed.
- `cache_plan::snapshot` runs `plan::check_rows` and logs each new
  `Shortfall` at WARN (#1309), so a reload that shrinks a cache below its
  demand warns from the plan log; engines warn from their own publishes.
  `cache_demand_bytes` and `cache_demand_ratio` have one series per plan
  row with a demand, `collection` empty for a process-wide cache, reset
  every scrape; the ratio has none for a disabled cache.
- The plan test runs in a child process: the `MC_*` statics read the
  environment once.
- `memory_plan.rs` (#1312) fits every row to the memory total in
  `cache_plan::log`, so at boot, after every accepted reload and on a
  recovery reload: `ds_cache::plan::grant::plan` over the snapshot rows,
  the accepted `[memory] total_mb` (`AcceptedLoad::memory_total_mb`) and
  the facts `memory_plan::read_at_boot` reads once (`main` calls it before
  the first load, so `MemAvailable` is what boot leaves). The last report
  is a process-wide static that `/admin/cache-plan` (`planned_bytes`,
  `plan`) and the `/metrics` plan gauges read. It applies nothing: the
  plan is a report until `[memory] mode = "auto"` (#1312 part 2).
- Detection (`memory_plan/detect.rs`) reads `/proc` and `/sys` under a
  root, so its tests build fixture trees and run on any OS; without
  `<root>/proc/meminfo`, macOS asks `sysctlbyname` and Windows
  `GlobalMemoryStatusEx`.
- `memory_plan::unreachable` decides rule 2 (0 for a cache no collection
  can reach) from the accepted collections: a new cache or budget that some
  configs cannot reach needs an arm there, and a class in
  `ds_cache::plan::grant::class`, or it plans as `fixed`.
  `STORE_FAMILIES` is the dashboard's "stores" list (Memory outside
  caches): change both together.
- Without `ADMIN_TOKEN` every admin route answers 403 (`require_admin_token`).
- `metrics_handler` takes the budgets' high-water marks after
  `refresh_metrics` (`update_budget_peaks`); the plan endpoint and log block
  do not, or a burst would vanish before the next scrape (#1307).
- `apply_load` calls `prune_collection_series` with the config ids a reload
  removed, after the engine lists are swapped and under the scrape's
  counter-state lock, so no scrape re-creates the series; their counter
  state goes too.
- `storage_retryable_responses_total{collection,host,status}` (#1336) is fed
  from ds-storage's process-wide counts (`ds_storage::retryable_responses`),
  not from an engine list: the scrape exports only ids in `last_collections`
  (plus `collection=""`, stores no engine named), so a removed collection's
  in-flight request cannot re-create its series, and `prune_collection_series`
  also calls `ds_storage::forget_retryable_responses`. Each series follows the
  cumulative count (`inc_by(count - series.get())`) under the counter-state lock.
- `process_start_time_seconds` comes from `admin::record_process_start`,
  the first call in `main`.
- Churn families (#1308) come from `ChurnMetricSet`, a field of
  `CacheMetricSet` and `CollectionCacheMetricSet`, registered
  `per_collection` so reload pruning covers them.
  `CollectionCacheMetricSet::new(prefix, display, with_entries, with_bypass)`:
  only the grid cache has `grid_cache_bypass_total`. A cache without a ghost
  ring has no `*_ghost_hits_total` series. A new churn family goes in
  `CHURN_PARTS`/`CHURN_FAMILIES` in `scripts/check_grafana_dashboard.py`.

## Health: data age and poll age (#1007)

- `/health` `data_age_secs` and the `/metrics` gauge
  `collection_data_age_seconds{collection}` are one number from one source,
  `admin::collection_data_ages`: now minus the newest data the collection
  serves, in whole seconds. GeoTIFF: the newest catalog timestep. QueryData:
  the newest run's origin time, the header's reference time that keys the
  run and is its instance id. An analysis file's origin time is its analysis
  time, and a forecast's newest valid time lies in the future. BUFR: the
  newest report. Satellite: the newest scan start. NetCDF: the newest run's
  reference time, or the newest valid time of files without runs. Nowcast:
  the latest generation's anchor frame. Other engines report none. Negative when the
  newest data lies in the future.
- Never the time since a poll. A stalled feeder leaves its files in place,
  every poll still finds them, and a poll-stamped age stayed near the poll
  interval while the served latest got minutes older. An engine gaining a
  data age adds it to `collection_data_ages` with an `expected_interval()`
  over the same timeline (`ds_core::health::expected_interval`), and its
  type to `ds_core::config::DATA_AGE_ENGINES`; `/health` and the gauges
  follow.
- `poll_age_secs` (GeoTIFF, QueryData): time since the engine loaded or a
  poll last found data. It grows while the source is unreachable, empty or
  unloadable, the old data kept. Both growing: the source fails. Only
  `data_age_secs` growing: the feeder delivers nothing new.
- The gauges are set each scrape by `refresh_data_age_gauge`.
  `collection_data_ages` lists every collection of those engines, with no age
  while it holds none: never loaded, or all of it aged out. Such a collection
  has no `collection_data_age_seconds` series and `collection_has_data` 0;
  the others have 1 (#1055). A reloaded-away collection loses both series;
  only those are removed, never the family.
- `collection_expected_interval_seconds` (#1375) is the spacing of the
  timeline the age is measured on: `ds_core::health::expected_interval`,
  the median of the newest five gaps (the larger middle one of an even
  count), or the collection's `expected_interval` (`DATA_AGE_ENGINES` only).
  Below two timesteps without one it has no series, and `CollectionStale`
  never fires for the collection. Same lifecycle as the age series, in the
  same `REPORTED` lock.
- Alert rules live in `docker/prometheus/rules/meteocore.yml`, with a
  promtool unit test per alert in `meteocore.test.yml` (CI job `rules`).
  The chart ships the same file through its `rules/` symlink as a
  PrometheusRule, replacing `job="meteocore"`: every selector names that
  job. Renaming or relabelling a family a rule reads needs the rule and its
  test changed in the same PR.
- WMS GetMap's red error tile (HTTP 200, #280) carries an `api_wms::ErrorTile`;
  `metrics_middleware` counts it as `wms_error_tiles_total{collection,reason}`
  and `prune_render_duration` drops a removed WMS collection's series.

## Operational notes

- A brief "no images" period right after a deploy is the expected readiness
  blip: the port binds before collections finish loading (#247). Not a code
  bug.
- Engine poll loops are spawned on `poll_runtime()` at boot (`main.rs`) and
  on reload (`admin.rs`), with `shutdown()` called on the old engines at
  reload. When adding an engine with a poll loop, wire ALL THREE places:
  the boot spawn block in `main.rs`, `rotate_poll_loops!` in `admin.rs`,
  and the graceful-shutdown lists at the end of `main` (`signal_shutdown!`, or `flush_state!` for an engine whose `shutdown()` flushes state) (bug #442 was a
  missing boot-path spawn for `cap_engines`; the shutdown block had also
  silently skipped cap, postgis and nowcast). An engine whose push-fed
  state must survive its collection's rebuild implements
  `take_over(&self, predecessor: &Self)` and is rotated with
  `rotate_poll_loops!(<field>, take_over)`, as CAP and BUFR are (#1036).

- Raster memory admission uses the process-wide `ds_executor::budget::RENDER_MEMORY`,
  configured by `MC_RENDER_MEMORY_MB` (default 1024 MiB; restart to change).
  It deliberately survives reloads so old and new render tasks share one limit.
  The 32-byte/output-pixel estimate covers output buffers/scratch, not source
  decoding or resident caches. An RGB composite render adds 16 bytes per
  output pixel for each band past the first (`acquire_raster_planes`). A WMS
  GetMap of several layers adds 4 bytes per output pixel for the image of
  the layers below, once (`acquire_layered_raster`). At the
  default, the 8000×8000 format limit exceeds the memory budget and returns
  503; operators can raise the budget explicitly.
  Temporary memory exhaustion queues within the same deadline instead of failing
  immediately. Memory waiters consume queue capacity, not CPU slots.

- Preview temporal manifests may include `temporal_extent.default` from
  `MapEngine::default_time()`. The slider selects it rather than the last
  value; preview time-window filtering must preserve that default (CAP's
  active-now view can precede future warning boundaries).
- A parameter with its own time axis (`MapEngine::parameter_times`, e.g. a
  lagging satellite product) carries `parameters[].temporal_extent`, windowed
  the same way. The slider re-binds to it on a parameter switch, keeping the
  viewed instant or the new axis's nearest — the collection axis is the
  union, and scrubbing a lagging parameter across it would request instants
  it doesn't have.
- RGB composites (`MapEngine::composites`, #819) join `parameters[]` with
  `"composite": true`, an empty unit and their own `temporal_extent`. The SPA
  requests them on the unstyled tile route: a composite has only the
  `default` style.
- The manifest describes what is served, never the configured `apis`
  (#1134): an entry's `apis` are the registries holding its engine (a radar
  network's base id lists every API but serves Features only; its sites
  inherit `features` and serve everything else), `page_url` is its HTML
  page (shared root, else EDR; none for WMS only), and
  `parameters[].default` marks `raster_info().parameter`, which Maps and
  WMS render without `parameter-name` (QueryData names it by the full
  descriptor, its entry's title). The parameter list stays
  alphabetical; the SPA opens on the marked entry. An entry without a
  `tiles` block gets no layer switch, only "No map layer" and that link.
- First enabling a preview collection fits its extent without animation,
  before setting its layers visible. Keep that order: a visible source during
  the initial flight requests intermediate zoom levels and can exhaust cold
  Icechunk read admission. Re-enabling preserves the current view; the explicit
  "Zoom to extent" action remains animated.
- `GET /robots.txt` (`src/robots.rs`) lets crawlers reach the landing pages,
  `/api`, `/conformance`, the collection lists and each collection's page, at
  `/` and under `/edr`, and nothing below a collection or anywhere else. A new crawlable root or
  catalog needs its rules there; the tests check each path with Google's
  matcher (the `robotstxt` crate).

## Engine state store (`[server] state_dir`, #1000, #1002)

Engines that accumulate state from a push feed snapshot it into the
server's state store and restore it when the collection is built: the WIS2
CAP accumulator under `<collection id>.cap` (see
`crates/engine-cap/CLAUDE.md`) and the WIS2 BUFR report store under
`<collection id>.bufr` (gzip-compressed JSON, ~7.7 MB for a global 270k-
report store; see `crates/engine-bufr/CLAUDE.md`). File-fed collections
re-read their source and ignore the store. The plumbing is
`ds_core::state` (std only):

- `StateStore`, the object-safe backend trait: `load(key)`, `save(key,
  bytes)` (an atomic whole-blob replace), `describe(key)` for log lines.
  Engines receive an `Option<Arc<dyn StateStore>>` and form their key with
  `state::collection_key(id, kind)` = `<collection id>.<kind>` (`cap`,
  `bufr`); they never see a path. **Snapshots are whole blobs per
  collection key**: loaded once at build, rewritten whole, never appended
  to or patched — a backend needs nothing beyond get/set of one value.
- `FileStateStore`, the only backend: `<state_dir>/<escaped key>.state`
  (key bytes outside `[A-Za-z0-9_-]` percent-encoded, a leading `.`
  too, so no key escapes the directory or hides a file), saved through a
  temp file in the same directory + fsync + rename + directory fsync.
  `prepare()` creates the directory and sweeps temps a crash left behind
  (older than an hour) at boot.
- `StateWriter`, the write policy, backend-independent: write when the
  engine's revision moved, at most every `min_interval` (CAP and BUFR:
  5 min); rewrite an unchanged state every `refresh_interval` (both: a
  quarter of `warmup`, 5 min–1 h) so the snapshot's own timestamp says when
  the server was last alive; always write when forced (the shutdown flush);
  report failures at most every `warn_interval` (15 min).
  `WritePolicy::for_warmup(warmup)` is that timing (`WRITE_INTERVAL`,
  `REFRESH_CAP`, `WARN_INTERVAL`; `state::hours_minutes` for log lines):
  a new warm-up engine (#1023) uses it, never a copy.
- **Adding a backend** (Redis, …): implement `StateStore` and select it in
  `admin::init_state_store` by a NEW `[server]` key (e.g. a backend name
  plus its URL), keeping `state_dir` as the file backend. Engines and the
  write policy do not change. The backend must keep `save` atomic per key
  and must not be shared by two servers that serve the same collection ids.

Wiring:

- `state_dir` is resolved against the config file's directory
  (`ServerSettings::state_dir_path`); `admin::init_state_store` builds the
  store in `main` BEFORE the first `load_collections` (engines restore while
  being built); it creates the directory and only WARNs when it cannot.
  Engines get it through `admin::state_store()` — a process-global
  `OnceLock`, like the render slots, so the 20-odd `load_collections`
  callers need no new argument and tests (which never set it) run without
  persistence.
- A reload re-reads the key but cannot replace the store: a changed value
  logs `state_dir_reload_warning` ("restart to apply"). An unchanged
  collection keeps its live engine and state.
- **A rebuilt WIS2 collection takes over in memory (#1036).** The new
  engine is built while the old one still receives, so its build-time
  restore is only the last periodic snapshot. Once the reload is accepted,
  `rotate_poll_loops!(cap_engines, take_over)` (and `bufr_engines`) first
  shuts down every replaced engine: its `shutdown()` ends its ingest under
  the ingest lock, then flushes. Only then does the new engine of the same
  id get `take_over(&replaced)`, which copies the replaced engine's state
  and applies the new config as a restore does, before its poll loop is
  spawned and before the registry swap, so the swap serves the whole set.
  The new engine's next write replaces the flush. Keep that order: a
  take-over before the shutdown misses what arrives in between, and one
  after the spawn would replace a store the new pipeline already fills.
  A rejected reload returns before the rotation: nothing is shut down or
  taken over. The new engine is a new broker session, so what is
  published between the shutdown and its subscription is missed, as on a
  restart. No state store is needed for any of this.
- Graceful shutdown: at the end of `main`, `flush_state!` runs each CAP
  and BUFR engine's `shutdown()` concurrently on the blocking pool (each
  flushes its own key, so the slowest flush, not the sum, sets the stop
  time) while `signal_shutdown!` stops the other engines; then it waits
  for every flush, bounded only by the container's stop timeout. Each
  flush logs its duration and size at INFO. A redeploy loses nothing and
  the snapshot records when the server went down; a crash, or a flush the
  stop timeout kills, loses at most the last five minutes (the previous
  snapshot stays). A large BUFR store takes seconds to encode: size
  `stop_grace_period` / `docker stop -t` from that INFO line (#1043).
- Deployment: the directory needs a writable mount (the image's `/data` is
  the config mount, often read-only). The image creates
  `/var/lib/meteocore` owned by its non-root `dataserver` user, so a named
  volume mounted there is writable as is; a bind mount needs a host
  directory that user can write. Unwritable ⇒ every write fails with a
  WARN at most every 15 min and the server serves on, exactly as without
  persistence. A removed collection's snapshot stays on disk; delete it by
  hand. Never point two replicas at one `state_dir`.
- In the server's unit tests the state store is process-global: a test
  that needs it runs in a child process (`state_dir_reaches_wis2_engines`,
  which covers both engines).

## Remote radar startup recovery (#190)

Remote COMP/PVOL sources whose startup scan fails keep `failed` health (503
when every collection failed) but no longer require a manual reload to recover.
A serialized background task retries after 30 seconds, backing off to five
minutes while failures persist. An initially empty remote PVOL catalog also
gets its per-site routes registered once polling discovers sites. Rebuilding
registration also retries failed dependent nowcasts. Healthy engines, static
CSV/GeoJSON snapshots and warm render caches are reused; ordinary explicit
reloads still rebuild static sources as before.

Recovery uses the last accepted collections, palettes and server routing
settings, including startup CLI filters. It never rereads config/palette files;
an explicit reload or enabled watcher remains the configuration control plane.
The task shares the reload mutex and ds-poll shutdown handle lifecycle. Source
configuration errors remain visible and logged; remote failures may include
permanent endpoint/bucket mistakes, so repeated failed health needs operator
attention. The all-failed startup exception requires every configured collection to be
remote radar; mixed local/remote failures still exit. Local missing files and structurally missing source config retain
startup fail-fast behavior. Runtime scan errors still retain the good catalog.

## NetCDF layer registration (#1067)

A NetCDF collection's layers are its files' variables. `NetcdfEngine::new`
scans its directory once, as engine-grib does, so they and their styles
exist at load; a collection whose directory was empty or unreadable then
loads with none. `LoadResult::netcdf_parameters` records the parameters each
collection's styles were resolved for; the recovery loop's
`netcdf_registration_changed`, like `grib_registration_changed`, registers
the collection again (accepted config, engine reused) once they differ,
within 30 s of the poll that reads its first files, and `apply_load` evicts
that collection's rendered and meta-tile entries, drawn meanwhile with the
collection's style. The layout is fixed for an engine's life, so this
happens at most once per engine.

## GRIB level collections

A GRIB config with `level_types` expands into present/enabled `-single`,
`-pressure`, `-model` collections. Keep only the source owner in `grib_engines`
and `engine_handles`; cheap views share its poll loop/cache. Views inherit APIs,
styles and collection metadata. Config validation reserves derived IDs even
before data arrives. The background recovery loop also compares GRIB family
registration with the live catalog, reusing accepted config and warm engines
when new families appear. Reload eviction already covers `{base}-*` IDs.

## Allocator (jemalloc, #1310)

- `main` is a plain `fn`: it parses the CLI, starts tracing and calls
  `jemalloc::configure` (`src/jemalloc.rs`) before it builds the request
  runtime. Process-wide allocator settings go there, before any runtime
  starts, not in `serve()`.
- Background threads are turned on by a runtime `mallctl` write, never a
  build-time `malloc_conf`: `background_thread:true` compiled in makes
  jemalloc fail to initialise on macOS, which has no background threads.
  There the write fails with ENOENT, logged at INFO; any other failure is a
  WARN.
- jemalloc's symbols are prefixed, so it reads `_RJEM_MALLOC_CONF`, never
  `MALLOC_CONF`; a `background_thread` set there wins over the server's.
- `jemalloc_retained_bytes` is address space, not memory: keep it off any
  panel plotted against RSS.

## Container contract (Helm chart, #1365)

`charts/meteocore` runs the image as it is. A change to any of these needs
the chart, its README and `.github/workflows/helm.yml` changed in the same
PR:

- **Command:** the image has no ENTRYPOINT, so the chart sets
  `command: ["dataserver"]` and passes `--config /data/config.toml --host
  0.0.0.0 --port <containerPort>`.
- **User:** uid and gid 999, from `useradd -r` in the Dockerfile; the chart
  pins `runAsUser`, `runAsGroup` and `fsGroup` to 999. The root filesystem
  is read-only: the server may write only to `state_dir`
  (`/var/lib/meteocore`) and `/tmp`, both emptyDirs or volumes.
- **Paths:** `config.toml` at `/data`, `collections.d` and `colormaps.d`
  resolved against it.
- **ConfigMap volumes are symlinks:** kubelet mounts each key as a symlink
  into `..data/`. The `collections_dir` and `colormaps_dir` loaders take a
  file by `Path::is_file()`, which follows symlinks;
  `DirEntry::file_type().is_file()` does not, and would load nothing under
  the chart.
- **Required keys:** `[server] host` and `port`, and a collection's `title`
  and `description`. The chart's default config and sample collection
  carry them.
- **Health:** the startup and readiness probes read `/health` (503 only
  when every collection failed; 200 with no collections), and the startup
  probe allows 10 minutes for the load between binding the port and
  serving. Liveness is a TCP check.
- **Metrics:** `/metrics` on the main port, for the ServiceMonitor.
- **Shutdown:** the SIGTERM flush of `state_dir` snapshots must finish
  within `terminationGracePeriodSeconds`, 60 s.
- **Dashboard:** the chart ships `docker/grafana/dashboards/
  meteocore-overview.json` compacted in a ConfigMap, at most 1 MiB;
  `helm.yml` fails above 900 KB rendered.
