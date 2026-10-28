# engine-grib crate — Claude Instructions

GRIB2 NWP data engine. Read the root `CLAUDE.md` first — Critical Rules 6–7
and 9 (the new-run probe once did 32 sequential blocking reads on one
thread) were learned here.

Only regular lat/lon grids (Template 0). Multi-parameter collections —
unlike GeoTIFF's one band per collection.

## Discovery & sources

- Discovers data via **index sidecar files** on S3/HTTP or a local
  directory; fetches messages via byte-range reads. The engine NEVER builds
  indexes itself. A listed index with a temporary name (e.g. a
  publisher's `.name.index`, #1009) or in a hidden folder below the listed
  prefix (the LIST is recursive; rsync `--delay-updates` stages files in
  `.~tmp~/`, #1029) is never fetched or marked known:
  `ds_core::temp_files::is_temporary_below`.
- **Data source (mutually exclusive):** remote
  `endpoint`+`bucket`+`prefix_pattern` (S3 with strftime/run-hour date
  templating), or local `data_path` (a directory of `.grib2` + index
  sidecars; also accepts an `s3://`/`http(s)://` fixed-prefix URL). For
  `data_path`, `prefix_pattern` is optional and literal (no date templating),
  appended to the URL's object prefix (or the local store root);
  index/data files must share a basename (`X.index` ↔ `X.grib2`).
- **Run prefixes** (remote) come from the shared
  `ds_storage::discovery::expand_run_prefixes` (#817), never a local copy:
  every run of today and yesterday (`SCAN_DAYS`) up to now, each day's
  strftime date formatted and each `run_hours` hour substituted into `{run}`
  as two digits. Runs come newest first: the `max_runs` early stop and
  `settle_completed_runs` depend on that order. A pattern without `{run}` is
  one prefix per day. A prefix repeated across runs (no day specifier) is
  listed once; a second copy would settle the newest run.
  `validate_run_prefix_pattern` runs at load: no `%H` (the run hour is
  `{run}`), no `{`/`}` outside `{run}`, and with `{run}` a non-empty
  `run_hours` of 0–23.
- **Index formats** via `index_format`: `"ecmwf-json"` (default, JSON-lines
  as shipped by ECMWF open data) and `"wgrib2"` (colon-separated text as
  shipped by NOAA GFS).
- Wgrib2 indexes carry only byte offsets — the last record's length is
  resolved via `DataStore::head()` on a full-field fetch. A failed HEAD/read is not cached
  and remains retryable; scanning does not issue a HEAD per index.
- Fetch new indexes through `DataStore::get_many` in chunks of at most eight.
  Bound the chunk as well as concurrency so raw sidecar bodies do not collect
  for the entire scan. Merge in sorted path order, regardless of completion
  order; mark only successfully parsed indexes as known. Failed reads/parses
  remain eligible when their prefix is next listed. Do not add per-index HEADs.
  The ignored `scan_tests::index_scan_latency_replay` test compares one versus
  eight concurrent reads over 120 indexes with 150 ms simulated GET latency.
  Run with `--ignored --nocapture`; timings are measurements, not CI gates.
- Parameter metadata populates lazily: `scan_once` probes ≤32 messages per
  scan, at most eight concurrently, each advertised parameter in the step
  files of the newest run that has it (#1026).
  Read headers with 4 KiB read-ahead, skip local-use/unsupported grid bodies, and cap each
  fetched metadata section at 64 KiB. Use the GRIB indicator's length for tail
  probes without HEAD. Never unpack values or fill the grid cache for discovery.
  Read the fixed template-3.0 grid header through the shared scan normalizer.
  Header discovery validates supported geometry and metadata, not packed values;
  a later query still validates/decodes the actual field. Unusually large or
  invalid headers remain unprobed until a successful read or full-field query.
  Pending probes continue even without new indexes, rotating past
  failures so later parameters are not starved. Time-window eviction also
  runs when discovery finds no new indexes.
  Preserve cursor order when applying concurrent results; metadata fallback
  representatives must not depend on completion order. Run on the dedicated
  poll runtime (shared fallback for CLI), propagate per-probe deadlines, and
  join all workers before returning. The ignored `metadata_probe_replay` test
  compares the old full-decode probes with header probes using a real global
  ECMWF field and simulated GET latency (`--ignored --nocapture`).

## Level collections

- Optional `level_types = ["single", "pressure", "model"]` expands a source
  into present/enabled `{id}-single`, `{id}-pressure`, `{id}-model` views.
  Omission retains the existing ID/canonical-level behavior.
- `GribSource` owns the single poll loop, storage and cache. Collection views
  share it; the server retains only the owner in `grib_engines`. Never spawn
  one poll loop or allocate a grid cache per view.
- Family catalogs, run-level lists and collection-level unions are built on
  the scan path and published with the source catalog. Drop empty steps/runs
  in each family: run selection and instances must reflect that family's data.
- Single: surface, fixed-height and named surface/layer products, no vertical
  axis; pressure: `pl`, hPa; model: `ml`, ordinal model/hybrid numbers.
  Soil/isentropic/fractional levels are excluded.
- Wgrib2 named levels retain distinct level types (ground, MSL, whole
  atmosphere, tropopause, cloud ceiling, etc.). Never alias them all to `sfc`.
  Ground/MSL/whole-atmosphere products outrank named upper-air products for
  canonical selection, independent of index order. Soil layers retain both
  metre boundaries in `sol:top-bottom`; their integer level is only a legacy
  ordering hint, not the layer identity or an exposed vertical coordinate.
- `StepFile` can combine several source files at one run/step. Always use
  `StepFile::message_url(entry)` for reads; offsets are local to that origin.
- Each catalog carries a nonzero content version, hashed on the scan path
  from its runs and message identities (`content_version()`). The map
  caches key on one frame's instead (`content_version_for`, #1334): a hash
  of the drawn message's origin, offset and indexed length, selected by
  `select_run_step` and `map_message`, which `render_field` shares
  (`map_level`, the step's default product, the canonical key at the map
  level). A scan adding other steps or runs keeps a frame's cache entries; a
  late file that changes the message a frame draws (another default or
  canonical level of its step) moves its version, and explicit-time HTTP
  images still revalidate. A failed selection returns the catalog's
  version. `map_message` runs on every map request, cache hits included:
  keep it free of key copies (no `ParameterKeys` clone; `map_level` reads
  the run's first level directly without `z`).
- Upper-air queries use exact levels, with no canonical-level fallback.
  Missing parameter/level pairs are null in EDR and errors in Maps. Pressure/
  model labels omit a fixed-level qualifier. The shared vertical descriptor
  feeds both EDR and Maps; single-level fields retain their label qualifiers.
- A map draws the requested level, else the selected run's own first level
  (`map_level`), which need not be the first of the advertised union.
  `render_field` and `MapEngine::resolve_elevation` share `map_level` and
  `select_run_step`, so WMS GetFeatureInfo asks the position query for the
  level the map shows (#1064).
- New families register through the server's background registration/recovery
  loop using the accepted config. Config reserves all enabled derived IDs.

## Unit conversion (source-driven — never hardcode parameter names)

- Conversions are driven by the WMO `(discipline, category,
  parameter_number)` triple read from message headers, not by
  short-name tables. Source units come from WMO Code Table 4.2
  (`src/units.rs`) plus per-center overlays for local parameter numbers
  192–254.
- Display conversions are mechanical: K→°C, Pa→hPa, kg m⁻²→mm,
  kg m⁻² s⁻¹→mm/h (× 3600, PRATE / ECMWF `tprate`, #1213),
  proportion→%. Geopotential (m² s⁻²) has no rule: ÷ g₀ it is geopotential
  height, another quantity, which `ds_core::geopotential` derives as a
  parameter of its own (#948); never serve it as a display unit of `z`.
  Colormap ranges use display units. The string-keyed twin
  of this table lives in `ds_core::units::display_conversion` (used by
  engine-bufr, engine-netcdf and engine-zarr); keep the two rule for rule.
  The units test
  `every_spelled_variant_has_ds_core_rule` checks each `SourceUnit` through
  its WMO spelling, and `display_rules_agree_with_ds_core_for_every_spelling`
  each spelling `parse_source_unit` reads (through
  `ds_core::units::canonical_unit`, as the twin; only tests call it). Two
  rules are GRIB-only, since no unit string identifies them: the code
  tables assign `KelvinDifference` (stays K) and ECMWF's `MetresOfWater`
  (× 1000 to mm) from the parameter. Served units come from the code table,
  already canonical (`m s-1`, `J kg-1`, `W m-2`), so unit
  normalisation (#1067 PR 6) changes no GRIB output.
- Distinguish absolute temperature from temperature differences using the
  WMO parameter semantics: dewpoint depression uses `KelvinDifference` and
  stays in K, with no Celsius zero-point offset.
- **Per-provider vocabularies are not needed.** A new provider only needs
  overlay entries if it uses local parameter numbers. ECMWF-`tcc` vs
  GFS-`TCDC`, `z` vs `HGT` are handled by construction (different triples).
- A rate accumulated over the product's interval is an amount, so the
  triple alone does not name it (#886): AIFS `cp`/`tp`/`sf` are the WMO
  rates 0/1/37, 0/1/52, 0/1/53 in template 4.8 with Code Table 4.10
  type 1, which eccodes reads as kg m⁻² (paramIds 228143/228228/228144).
  `MessageMetadata::statistic` (templates 4.8/4.11, one time range, read in
  `from_product` so probe and decode agree) feeds
  `units::lookup_with_statistic`, which maps those to the amount before
  the plain table; IFS `tprate` (0/1/52, template 4.0) stays mm/h. Add an
  `accumulated_lookup` entry only from eccodes' definition of the triple
  with `typeOfStatisticalProcessing = 1`, never from a short name.

## Model runs (#337)

Catalog keeps a `runs` map (`BTreeMap` keyed by reference time) and
implements the shared `ds_core::instances` contract (see root CLAUDE.md).
Two selections, never mixed (#967):
- **Maps** (`select_run_step`): without a parameter (the default layer;
  M trajectories use its `covering_run`) the newest run whose published
  valid-time extent covers the time, at its nearest step; with one, only
  the steps carrying it (#1005, below). `get_raster_tile(s)`, every
  `resolve_*` override and `content_version_for` go through it: they key the #507/#521 caches, so it
  must stay the render's own selection.
- **EDR** (`resolve_run`, `grid_steps`; position, area, radius, cube, 2-D
  and Z trajectories): by intersection (`/req/core/datetime-response` A, F).
  An instant takes only the step valid at exactly that time, never the
  nearest; a window every step inside it, open ends (`MIN_UTC`/`MAX_UTC`)
  included. The run: the newest covering the window's start with a step
  inside, else the newest with any step inside (`../end`). None inside is
  `LocationNotFound` (404), which a datetime list skips. A request naming
  parameters chooses among the runs that have them all first (#1026, below).

An incomplete newest run does not hide a covering older run; explicit run
pins do not fall back. No requested datetime selects the latest run, among
those with every named parameter first (#1026); an area, radius or cube
then reads its last step carrying them all (`grid_steps`), else its last
step.

Each run also has canonical `(parameter, level type, level)` selectors,
rebuilt with `Catalog::refresh_metadata` before publication. Metadata
probes and all query paths share these selectors. A missing canonical level
is null in a position series and an error in a map request or an area that
names the parameter (a defaulted area parameter is null); never
silently substitute an upper-air field. Metadata is cached by that full
identity, so historical runs with different selected levels keep their labels.
Pressure/model views may reuse metadata from a probed or decoded level of the same
parameter and level type until the exact level is decoded. Keep this fallback
indexed alongside the exact cache under one lock; never scan all cached levels
on the request path. Single-level and legacy views require exact metadata.

## v1 limitations (GFS)

- Only regular lat/lon grids (Template 0) — gaussian-grid products
  (`gdas.*`) fail loudly.
- All rectangular scan orders are normalized to eastward columns and
  north-to-south rows using the decoder's grid-index iterator. Basic-angle
  units are honored; unsupported staggered scan flags are rejected.
- Hour-window `acc fcst` and `ave fcst` records are preserved (#80).
  They use distinct keys (`APCP_acc_6h`, `DSWRF_avg_6h`), so two window
  lengths or an instantaneous field at the same valid time cannot collide.
  The `MessageEntry` retains the original start/end; EDR reports the window
  end and labels carry the duration. Config `parameters = ["APCP"]` includes
  its window variants; an exact key selects one duration. Discover keys
  from the collection before querying. Existing max/min keys remain unchanged.
  Repeated identical catalog keys at different offsets are ambiguous (the real
  GFS f006 fixture has repeated `APCP_acc_6h` and `ACPCP_acc_6h` surface records).
  After parameter/family filtering, scan emits at most one warning summarizing
  ambiguous files/records with an example and both offsets; full per-record
  details are DEBUG. Queries preserve first-record selection. The index alone
  cannot establish payload equivalence or expose an omitted product discriminator.
  Log the optional vertical level as `grib_level`, never `level`: flattened
  JSON reserves `level` for severity, and a duplicate key hides WARN in Loki.
- Source units still come from the decoded WMO triple: no automatic division
  by window length. Precipitation kg/m² displays as mm; already-averaged flux
  W/m² stays W/m², and energy in J/m² stays energy. ECMWF JSON sidecars retain
  their existing naming/semantics; this change does not infer missing windows.
- Parameter discovery and bounded metadata probes include every step of
  every retained run: the analysis step commonly has no aggregates. An aggregate
  absent from a time step is a null in a position series, never a zero.
  A wgrib2 index with mixed window-end times is rejected rather than assigning
  all its records the first record's time.
- Strongly advise a `parameters` filter with `index_format = "wgrib2"` — a
  single GFS 0.25° file has ~700 messages.
- CCSDS/AEC compression needs the `libaec` C library (via `libaec-sys`).

## Decoded-grid memory

- The GRIB decoder produces `f32`; retain those values in `DecodedGrid` to
  avoid doubling every cached buffer. Widen samples to `f64` before interpolation
  or display conversion. Coordinates and EDR outputs remain `f64`.
- Map tiles are `RasterValues::F32` (#475): `DecodedGrid::resample_f32` runs
  the same f64 sampling as `resample` (one walk, `resample_each`), fuses the
  display conversion, and narrows each result to f32 with NaN as nodata —
  4 B/px instead of a boxed 16. Values are exactly the old f64 ones `as f32`
  (`tests/local_source.rs` pins this on the fixture for all three CRS paths);
  keep `resample` returning f64 for the bench fingerprint.
- CRS:84 and EPSG:3857 map tiles are sampled separably (#1331). On these two
  outputs `project_node`'s longitude never reads `fy` and its latitude never
  reads `fx`, and `lonlat_to_src_px` is per axis too, so `resample_each` maps
  each output column to a source column and each output row to a source row
  once, then calls `bilinear_at` per pixel. Never call `project_node` per
  pixel there (Critical Rule 5). `cache/sampler_tests.rs` keeps the old
  per-pixel walk as the bit-for-bit reference. An `OutputCrs` whose longitude
  depends on the row, or latitude on the column, belongs on `ProjectionGrid`,
  not in this arm.
- This sampler is not `ds_core::resample::SourceGrid` (#1172), and moving to
  it changes pixels: EPSG:3857 maps each pixel exactly here, SourceGrid
  through its projection grid; a regional grid draws nothing west or north
  of its first node here, half a cell more there; a NaN neighbour falls back
  to the first valid corner here, the nearest cell there; and the display
  conversion runs before narrowing here. `cache/sampler_tests.rs` pins the
  two bit-equal where they agree on the committed global field: CRS84 and
  projected output, both forms of the seam box, the #323 Arctic tile. A
  sampler change that breaks it is a drift from the shared rules.
- Cache weights include allocated value capacity and key/structure overhead.
  Keep the entry-size estimate aligned with the 4 MiB typical global field.
- The grid cache's hot share leaves its cold ring room for a 0.1° field
  (`ds_cache::hot_allocation_holding` over `LARGEST_GRID_NODES`, #1332):
  0.894 at 256 MiB, 0.947 at 512 MiB, 0.97 from 1 GiB, and the default
  0.97 below 52 MiB, where no share both admits such a field and leaves it
  room. Once the hot ring is full quick_cache admits a new entry to the
  cold ring, and a single-flight fill links it there before evicting, so a
  fill heavier than that ring evicts itself unread: at 0.97 each 0.1°
  meta-tiled frame decoded its field again at its next tile, 46 decodes
  for a 24-frame loop. `LARGEST_GRID_NODES` is an assumption, since the
  cache is built before the first decode publishes the geometry; raise it
  before serving a finer global grid. Its cost: a small cache of coarser
  grids gives up hot ring, at 64 MiB a 0.25° one-lookup-per-frame loop
  longer than the cache decodes 15 instead of 9 on its first pass.
- Run `cargo run --release -p engine-grib --example bench_grid_cache` for a
  reproducible memory/cache replay and sampling benchmark using the committed
  ECMWF fixture. It prints a query-output fingerprint for before/after checks;
  timings are machine-dependent and are not CI thresholds.

## Compressed-message cache

- `message_cache_mb` is an optional additional per-source memory budget, default
  `0` (disabled). `GribSource` owns it alongside the decoded cache; level views
  and all query APIs share it. Keep metadata header probes out of both caches.
- A decoded-grid miss can reuse the compressed message and decode it again.
  On a compressed-cache miss, share the fetch through `ByteBoundedCache` and
  admit bytes only after decoding succeeds, or, for a CCSDS point read, after
  the index build or RSI decode succeeds (#1300). Reuse that cold decode;
  failed reads, invalid payloads and expired fills must remain retryable.
- The cache is one shard (`new_single_shard_with_hot_allocation`): a
  position query plans residency with `holds(messages, bytes)` (#1301),
  which counts each message's key and node overhead, not only its indexed
  length. Sharded, 1024 MiB was 64 shards of 16 MiB on 12 CPUs. Its hot
  share follows the grid cache's rule for a message as large as a decoded
  0.1° field, which bounds a packed one: 0.97 from 1 GiB.
- A window `holds` approved is filled with `get_with(.., window = true, ..)`,
  which `keep`s each fill (#1340). Marked read, quick_cache promotes it to
  the hot ring instead of evicting it from the cold one; a fill that a
  concurrent burst of fills evicted before the mark is inserted again,
  which quick_cache admits to the hot ring. Unkept, a full 1 GiB cache held
  only the last ~30 MiB of a window, and a repeated query took four runs to
  stick. Map fills (`read`) are never kept: one-off reads must leave first.
  `message_cache::tests::a_full_cache_keeps_a_cold_position_window_for_its_next_run`
  pins it through the real position path.
- Key by source-local path, offset and indexed length. Like the decoded cache,
  this assumes immutable files at a given path/offset; engine reconstruction
  clears both caches. Changing the cache configuration triggers reconstruction
  through normal config equality; unchanged reloads preserve the warm caches.
- Retained `Bytes` must own just the message, not a slice backed by an entire
  object. Count payload/key/node overhead; oversized messages are served but
  not retained. Range arithmetic must reject overflow/zero lengths and short
  reads before admission.
- `/metrics` exposes `grib_message_cache_{hits_total,misses_total,bytes,
  capacity_bytes}` under the source collection ID, once per owner. Message hits
  count reuse after decoded-grid misses, not every query field.
  The grid and message caches keep a ghost ring (#1308).
- The server enters the grid, message and point-index caches in the memory
  plan (`cache_plan::register_grib`, #1306) through the same accessors
  `/metrics` reads, scoped to the source's collection id. A new per-source
  cache adds its entry there.
- The ignored `message_cache::tests::repeated_forecast_latency_replay` replays
  two different coordinates over 120 real global fields with 150 ms simulated
  GET latency, comparing 256 MiB decoded-only caching with an additional
  128 MiB message cache. It checks identical samples and reports timings/read
  bytes; no wall-clock thresholds belong in CI. Its 120-field window exceeds
  the 256 MiB grid cache, so it reads around it (#1297); the message cache is
  what serves the second coordinate.

Default Maps parameter selection preserves the first previously supported
near-surface product before considering acc/ave records, regardless of index
ordering. Aggregate-only collections fall back to their first aggregate;
upper-air-only collections to their first message. Raster metadata and actual
rendering share `StepFile::default_message` so default labels/units agree.
EDR queries without `parameter-name` (position, trajectory, area/radius,
cube) return `default_parameters` instead, every parameter of the view
(EDR 1.2 `/req/edr/parameter-name-response` A, #966): every key of a
pressure/model view, else the products of the selected steps, near-surface
ones only in the legacy layout.

## Per-parameter time axes (#1005)

- Hour-window aggregates (`PRATE_avg_6h`) and maxima absent from the
  analysis (`TMAX`) exist at some steps only. `Catalog::refresh_metadata`
  records per run the steps carrying each parameter some steps lack
  (`partial_steps`, one pass over the messages) and each advertised
  parameter's valid times over the retained runs, kept where they differ
  from the collection's (`parameter_times`). A step carries a parameter at
  its run's canonical key; in a pressure/model family at any level.
- `MapEngine::parameter_times` and `EdrEngine::get_parameter_available_times`
  read that snapshot (an `Arc` clone). The API layers then give the WMS
  child layer its own `time` dimension and default, and Maps/Tiles/EDR
  `parameter_names` its own `extent.temporal`. `RasterInfo.times` stays the
  union. The advertised parameters are every retained run's (#1026, below).
- Map selection of a parameter (`select_run_step`, `CarriedSteps`), over
  the pinned run, else the runs newest first:
  1. the first whose carrying steps span the time, at the nearest of them;
  2. else, the time inside any run's steps, the carrying step nearest it
     in any of them, the newer run's on a tie (WMS `nearestValue`);
  3. else the time is outside the data: the default layer's 400.

  No carrying step is `LocationNotFound` (404); `None` is the newest
  carrying run's last carrying step. Several parameters
  (`get_raster_tiles`, `resolve_parameters_time`, DerivedWind's u/v) use
  the steps carrying all of them.
- The run depends on the parameter: an aggregate the newest run lacks at
  that time renders from an older run. Hence
  `MapEngine::resolve_parameter_reference_time`, which the API layers call
  instead of `resolve_reference_time`. Pinning the run it returns must
  re-select the same step at the request and at that step's time:
  `resolved_keys_render_the_same_pixels_as_the_request` sweeps it. So with
  a pin, rule 2 accepts a time outside the pinned run's own steps only when
  the unpinned selection chooses that run; any other pin outside its steps
  is the default layer's 400, never the pinned run's edge step.
- `fetch_grid`: a parameter the run lacks is `InvalidParameter`; a step
  without its field at the requested level (a pressure view's `z`) is
  `LocationNotFound`.
- Tests: `parameter_times_tests.rs` (two synthetic runs, and the real GFS
  2026-04-08 f000/f003/f006 sidecars in `testdata/gfs`), and
  `server/tests/grib_parameter_times.rs` (WMS capabilities and GetMap cache
  keys, Maps, EDR metadata, through `DerivedWind`; with two runs, the run
  WMS, Maps and Tiles render and key where the newest lacks the window).

## Parameters over retained runs (#1026)

- A new run's analysis, typically one scan cycle on its own, lacks every
  window and maximum, and a run may drop a parameter altogether.
  `Catalog::refresh_metadata` therefore advertises the union over every
  retained run, newest run first (its index order, then each older run's
  other names), so the order holds once the new run has them all.
  `refresh_families` calls it for each level view: the views union alike.
  WMS layers, Maps/Tiles and EDR `parameter_names` all read that list.
- `Catalog::advertised_keys`: each name's canonical key in the newest run
  that has it. Labels and units (`param_metadata`), the metadata probes and
  the wind facts read it, so an older run's parameter is never a bare name
  with an empty unit. `has_parameter` reads it too (O(log n)).
- Maps need no other change: `select_run_step` already walks every run, so a
  parameter the newest run lacks renders, and keys (#507/#521), an older
  run's step, `None` that run's last carrying step. Several parameters'
  run is `resolve_parameters_reference_time`, the same selection.
- EDR (`resolve_run`) and an M trajectory (`covering_run`) take the runs
  that have every named parameter first (`preferring_runs_with`), then,
  when none of those qualifies, every run as before. Without
  `parameter-name`, or naming only what the newest run has, nothing
  changes; an explicit run pin never falls back. Without a datetime, an
  area, radius or cube reads the last step of that run carrying every
  named parameter (`grid_steps`, as a map without `TIME` renders), so
  `TCDC_avg_3h` is not the 400 of a last step that lacks it.
- `DerivedWind::speed_tile` resolves an unpinned render's run over u and v
  (`resolve_parameters_reference_time`) before reading either component:
  `get_raster_tile` with `None` per component would pick each one's own
  run, the new run's u with the old run's v, unlike the keyed render.
- A parameter dropped from the newer runs stays advertised until
  `max_runs` (or the `time_window`) evicts the last run that has it.
- Tests: `retained_runs_tests.rs` (a complete 00Z run next to a 06Z run
  with only its analysis: advertisement and order, labels and units, map
  selection and the cache-key sweep, EDR position and area, an area of a
  window the last step lacks, an M trajectory, eviction, a pressure view's
  position and cube, and a v component only the older run has, with the
  derived speed's direct-versus-keyed sweep).

## Position queries

- `query_positions` samples every requested coordinate from each field once,
  even when the decoded-grid cache is disabled or smaller than the forecast
  window. Single-point, single-level and profile queries share this path;
  every parameter's fields go through one `run_field_jobs` run. Fields go
  through `position_samples`: CCSDS fields through the point index (see
  "CCSDS point reads"), others through `fetch_grid_by_entry`.
- Every EDR field query (position, trajectory, area/radius, cube) reads
  under one `GribEngine::field_plan` (#1297), built from the count of all
  its fields, the first geometry field included, and the view's published
  `RasterInfo::grid_size` (an `Arc` clone, never a decode):
  - concurrency: `runtime::field_concurrency`, `FIELD_MEMORY` (256 MiB of
    decoded f32) over one field, clamped to `FIELD_CONCURRENCY` (4) ..
    `MAX_FIELD_JOBS` (16): 16 at 0.25°, 10 at 0.1°, 4 without a published
    geometry. EDR admits at most `query_workers()` (≤ 8) queries, so GRIB
    EDR queries together hold ≤ 2 GiB decoded plus decode transients;
    there is no GRIB read budget. Change the constants with that sum in mind.
  - fill: `GridFill::Bypass` when fields × `GridCache::estimated_weight`
    exceed `grid_cache_mb` (`GridCache::holds`). `GridCache::get_or_join`
    uses a resident grid or awaits a fill in flight (no longer than the
    deadline), else the field is read without inserting; the message cache
    still fills. A window that fits is `GridFill::Insert`, cached as before;
    maps (`fetch_grid`) always insert. Never insert an oversized window: it
    evicts its own fields, and the maps' grids, before any reuse. The grid
    cache is one shard (`new_single_shard_with_hot_allocation`) because
    `holds` plans residency: at 256 MiB a sharded cache is two 128 MiB
    halves, so six 0.1° fields that "fit" could all hash to one half and
    evict each other. An `Insert` window's fills are not kept
    (`ByteBoundedCache::keep`), so in a full cache only what the cold ring
    holds, about one 0.1° field, survives to the next query.
    A field read around the cache (`get_or_join`, or `decode_in_full` under a
    Bypass plan) counts in `grid_cache_bypass_total`, never as a miss
    (#1308).
- Jobs run on the dedicated query runtime with `block_in_place`, including
  synchronous cache-fill waits; never move storage calls to `spawn_blocking`.
  Direct off-runtime callers share one fallback runtime.
- Propagate the absolute EDR deadline to each job and its storage calls. Stop
  dispatching at expiry and drain running jobs before releasing query admission.
- Validate all coordinates and the complete points × times × levels × parameters
  value budget before fetching. Preserve point/time/level ordering regardless of
  job completion order, and sample missing fields as null for every point.
- Tests: `field_plan_tests.rs` (an oversized window in every query kind
  leaves the cache's entries and bytes as they were and answers bit for bit
  as with no cache or a large one; resident fields are reused; the message
  cache still fills; a fitting window repeats as hits; 16 concurrent GETs on
  the fixture grid) and `runtime::tests` (the concurrency per grid size).
- Run the ignored `position::tests::position_latency_replay` test with
  `--ignored --nocapture` to compare serial and planned batch reads over 120
  steps (16 at a time on the fixture grid), with 150 ms simulated per-read
  latency and the grid cache disabled. It checks equal samples and prints
  timings/bytes; wall-clock times are not CI gates.

## CCSDS point reads (#1300)

- Position queries (POINT, MULTIPOINT) sample template 5.42 fields through
  `point_index.rs` (`position_samples`): a per-message `ccsds::CcsdsIndex`
  (Section 5 coding, lattice, `MessageMetadata`, payload position, one `u32`
  bit offset per RSI of 4096 values: 1.2 KB at 0.25°, 6.5 KB at 0.1°) in a
  per-source `ByteBoundedCache` keyed by `MessageKey` like the message cache,
  single-flight builds. `point_index_cache_mb` (unset: 25 % of
  `message_cache_mb`, at least 16, see Complex packing point reads; `0` =
  off, the kill switch for comparing paths). The cache is one shard (#1302): CCSDS
  and 5.3 indexes share it.
- First read: one libaec decode with offsets enabled, streamed through a
  reused 16 Ki-sample buffer (as fast as a whole-field buffer), picking the
  query's samples on the way. Later reads decode each needed RSI from its
  start to the last needed sample. Never build a grid or insert into the grid
  cache here; a resident grid is used through `GridCache::get_untracked`.
- Grid-cache counters measure grid loads: a resident grid counts a hit, a
  full decode (the fallback below) a miss, as the full path would; a field
  read through the index counts nothing. Never count a lookup that loads no
  grid, or the hit and miss rates stop sizing the cache.
- Bit identity with `decode_message` + `bilinear_value` is the contract: the
  same libaec stream, `grib`'s `NonZeroSimplePackingDecoder` scaling op for
  op (`(R + raw as f32 × 2^E) × 10^-D`; zero bits `R × 10^-D`), and one
  interpolation, `Lattice::bilinear_at` + `Bilinear::interpolate`, which
  `DecodedGrid` uses too. Change sampling there, never in one path.
- Readable: 5.42, bitmap indicator 255, template 3.0 scan mode 0, original
  field type 0, a byte layout `grib` reads alike (MSB, 24-bit samples in 3
  bytes, unsigned, no `AEC_PAD_RSI`), |E| ≤ 126. Template 5.3 goes to its
  own reader (#1302), never `Unsupported`. Else `Unsupported`: decoded
  in full from the same bytes (never a second GET), and the `ParameterKey`
  is marked unreadable until the catalog next publishes a snapshot: later
  fields of that run take `fetch_grid_by_entry` directly. `Invalid` (libaec
  error, short payload, offsets that do not match) also decodes in full from
  the same bytes, without marking: the full path's outcome, which may
  succeed (`grib` zero-fills a short payload). Deadlines stay fatal.
- Skipped when the plan would `Insert` into a grid cache and there is no
  message cache: the kept grid serves repeats with no request.
- Bytes come from `MessageCache::get_with` (admitted once the index build,
  the fallback decode or, for an evicted message with a warm index, the RSI
  decode succeeds) or one GET. Metadata and geometry come from the index's
  header and equal a decode's (`header_metadata_is_the_full_decode_metadata`).
- `ccsds/aec.rs` holds the crate's only `unsafe`, each with `SAFETY:`; check
  every length and offset before libaec. Never `aec_decode_range` (copies
  uninitialized memory on short input, leaks on error); `decode_rsi` seeks
  and decodes into the caller's buffer and requires every byte.
- `/metrics`: `grib_point_index_cache_{hits_total,misses_total,bytes,
  capacity_bytes}` and `grib_point_fields_total{collection,path}` (`partial`
  | `full`) per source.
- Tests: `ccsds/tests.rs`, `point_index_tests.rs`. Ignored, `--release
  --ignored --nocapture`, measurements only: `ccsds_files_are_bit_identical`
  (`MC_CCSDS_FILES=a:b`, every sample of every RSI, and the header metadata)
  and `point_index_replay` (`MC_CCSDS_REPLAY=file`, 41 steps);
  `repeated_forecast_latency_replay` compares the index on and off.
- Trajectory, area/radius and cube still decode in full. #1301: the index
  carries `payload_start` and the offsets; a partial index of the first m
  RSIs fails reads beyond them.

## Partial fetches (#1301)

- `FieldPlan::fetch`, one decision per query: a position query fetches only
  parts of its messages (`FieldFetch::Partial`) when the source has no
  message cache or the sum of its fields' `entry.length`, plus each
  message's cache overhead, exceeds `MessageCache::holds`; else whole,
  through the message cache, each fill kept for the next run. Other
  queries pass `None` (whole). No `entry.length` ⇒ whole. Partial bytes
  never enter the message cache; a resident message is used
  (`get_resident`: a hit, never a miss).
- Warm (`CcsdsIndex::covers`): ONE range GET, `rsi_bytes`, decoded by
  `sample_bytes` at the bit relative to the slice. Always one GET per
  field: multi-range returns the whole object and the ECMWF bucket 503s
  extra requests.
- Cold: ONE prefix GET from byte 0 to `predict_prefix` (≥ 4 KiB) from
  `PointIndex::sibling`: `Kin::SameRun` (+2 %), `OtherRun` (its layout's
  shape, +20 %), `OtherParameter` (linear in samples, +20 %). No index in
  the source yet ⇒ whole. `Header::parse_prefix` parses Sections 0–6 plus
  an empty 7 and 8 (grib rejects a truncated message); Section 7 must end
  the message at its indexed length.
- `Header::extend` decodes from the furthest known RSI start (or the first
  needed RSI) with offsets enabled, through the last needed RSI's end,
  merging RSI starts with `prior`'s (they must agree). Short ⇒ ONE
  continuation to max(sibling shape, `read_on_bits`: denser of the last 8
  RSIs' rate and the rest's average, ×1.6), or the message end past 80 %;
  still short ⇒ whole. The 1.6 is measured (the local files fell short by
  up to ×1.59 one RSI ahead).
- Beyond a partial index: one read-on GET from its furthest known start
  (`read_on`), never a longer prefix. libaec's offsets after a seek count
  from the byte after the sought one when the bit is mid-byte;
  `aec::decode_from` converts them. Tests compare every recorded start with
  a whole decode's, mid-byte starts included: keep them.
- Fallback to one whole GET (outside the message cache) on a header beyond
  the prefix, a short read, a mismatching index, a continuation still
  short. Unsupported in a prefix ⇒ fetch the rest, full decode, mark
  unreadable. Storage errors fail the field.
- Siblings are weak per (ParameterKey, run), pruned on catalog publish.
- `/metrics`: `grib_point_fetches_total` and `grib_point_fetch_bytes_total`
  `{collection, mode=whole|prefix|range|continuation}`.
- Tests: `ccsds/tests.rs` (`assert_partial_reads`), `point_fetch_tests.rs`
  (GETs and bytes over `TestStore`). Ignored, `--release --ignored
  --nocapture`: `ccsds_files_are_bit_identical` (now also partial reads,
  sibling accuracy, read-on shortfalls) and `point_fetch_replay`
  (`MC_CCSDS_REPLAY`, 41 steps × 2 runs).

## Complex packing point reads (#1302)

- Template 5.3, all of GFS 0.25°, reads through the same point index:
  `Index::Complex` beside `Index::Ccsds` in one cache, dispatched on Section
  5's template (`ccsds::data_template`). Decoder and index in `complex.rs`,
  the engine's reads in `point_index/complex_reads.rs`; `PointError`
  (`point_index.rs`) serves both.
- Every value is a running sum of all differences before it, so
  `ComplexIndex` keeps one 28 B `Checkpoint` per grid row: the decoder state
  before the row's first value (packed index, group, offset in it, the
  group's data bit, values seen, the last two undifferenced). It also keeps
  the message's packed group descriptors (refs, widths, lengths): a row's
  three descriptor slices lie about 90 KB apart, so keeping them is what
  holds a warm read to ONE GET. 100–142 KB per GFS 0.25° message, against
  1–6 KB for CCSDS; the weigher counts all of it. The descriptors dominate,
  so the size barely follows the message's: 13–20 % of a TMP message,
  20–23 % of a PRATE one, 18 % on average (48 real messages, #1333).
- The point-index cache is single shard (`new_single_shard`), so entries of
  1 KB to 142 KB share one budget. Sharded from a 4 KiB hint it was 64
  slices of 256 KiB and a window's indexes evicted each other;
  `point_index/tests.rs` pins 121 GFS-sized indexes resident in 16 MiB.
- Its size follows the message cache (#1333): `point_index_cache_mb` unset
  is 25 % of `message_cache_mb`, never below 16 MiB, which covers the
  largest ratio measured. Read the size through
  `GribConfig::effective_point_index_cache_mb`, never the raw `Option`; only
  `cache_plan::point_index_source` reads it, to name the plan source. A value
  set, `0` included, wins. At a fixed 16 MiB (about 145 GFS indexes) a warm
  query over 565 resident messages rebuilt 535–565 indexes at about 17 ms
  CPU each. Capacity fills only as positions are indexed; a CCSDS source
  uses a small part of it. The plan reports a derived size as
  `derived:[grib] message_cache_mb`, a set key as its key whatever the
  value. `point_index_tests.rs` pins 1024 MiB → room for the 115 KB index
  of every 0.81 MB message.
- Bit identity with `decode_message` is the contract: `grib`'s release-build
  integer arithmetic (wrapping), its missing values (management 1 and 2)
  and bitmap, `NonZeroSimplePackingDecoder` op for op, the shared
  `Bilinear`.
- Readable: order 1 or 2, group splitting 1, extra descriptors of 1–4
  octets, descriptor and group widths ≤ 31 bits, |E| ≤ 126, bitmap indicator
  0 or 255, canonical scan. Else `Unsupported`. Corrupt or truncated bytes
  are `Invalid`, never a panic: `grib` itself panics on a Section 7 shorter
  than its groups (#1325), so keep
  `malformed_messages_fail_or_agree_with_the_full_decode`.
- Whole message: the first read decodes every value once (about 5 ms
  against 16 ms for the full decode) and indexes every row; later reads
  decode from the first needed row's checkpoint, about two rows.
- Partial reads follow the #1301 rules. Warm: ONE range GET
  (`ComplexIndex::span`), the start checkpoint's data through the last
  needed value's byte, 1.3–2.3 KB on GFS; rows past the index read on from
  the last indexed row. Cold: ONE prefix predicted from a sibling
  (`predict_prefix`: its share through the row after the last needed one,
  with the kin margin, else linear); the prefix's own descriptors give the
  exact end, so a short prefix takes ONE exact continuation. A prefix ending
  inside the descriptors reads on once to `Header::predict_end`, else whole.
- Bitmapped fields (SOILW and other land fields): only the bitmap, before
  Section 7, places the needed values, so every partial read is a prefix; a
  warm one runs through `prefix_bound`, the row after the last needed one
  from the checkpoints. A decode records that row's checkpoint when the
  bytes hold it (`decode_end`).
- Tests: `complex/tests.rs` (every value of `testdata/grib-gfs-complex` and
  of `test_support::Complex` layouts, partial reads, malformed input),
  `complex_fetch_tests.rs` (GETs and bytes), `point_index/tests.rs`.
  Ignored, `--release --ignored --nocapture`: `complex_files_are_bit_identical`
  (`MC_COMPLEX_FILES=a:b`); `point_fetch_replay` takes 5.3 files too.

## Trajectory queries (#926)

- `trajectory.rs` selects the run (an M path's samples snap to the nearest
  step, so its run is `covering_run` of its first time, as a map's is,
  among the runs with the named parameters first, #1026),
  steps, levels and parameters with the position
  query's helpers (`resolve_run`, `position_parameters`, `selected_levels`)
  and leaves the path, time/level rules and layout to
  `ds_core::trajectory::TrajectoryPlan`. Densify at the published
  representative geometry's spacing (`RasterInfo` extent / grid cells;
  0.1° before the probe has run).
- One `run_field_jobs` job per (parameter, planned field = step × level),
  under the query's `FieldPlan`:
  fetch/decode once, sample every point that reads it with the position
  query's `bilinear_value` + display conversion. Missing or unreadable
  fields stay null; deadlines stay fatal. Never a job per sample.
- Z snaps to the run's available levels (`catalog.levels`), so a Z path
  never asks for a level the run lacks; `z` stays exact, like position.

## Area and radius queries

- Reuse the first geometry field's values even when the decoded cache is
  disabled. Select its axes without extracting values, check the combined
  output and mask budgets, then sample and release the grid before loading
  remaining fields.
- Use `runtime::run_field_jobs`, shared with positions, for the query's
  `FieldPlan` (4–16 parameter/level jobs per admitted query, the bypass rule
  of Position queries). It is a thin wrapper over
  `ds_executor::field_jobs::run_field_jobs` (moved there for engine-netcdf,
  #1067) with the GRIB I/O runtime as the off-runtime fallback. Keep deadline
  propagation, blocking cache waits and drain-on-error behavior in that
  shared scheduler. Area read failures
  are fatal; ordinary position field failures remain null samples.
- Workers return small masked/converted subsets into fixed
  step/parameter/level slots. Preserve requested level order and missing upper-air nulls regardless
  of completion order. Validate grid axes before extracting each subset.
- Radius delegates to the same area path via the default EDR trait method.
- `bbox_subset` rounds out to the nodes enclosing the polygon's bbox; the
  shared `QueryPolygon::mask_native_cells` nulls the nodes outside the
  polygon: for a rectangle those whose cell, half a spacing either side,
  misses it (#966), for any other polygon those centred outside it (#671).
  Pass the native spacing: a bbox clamped at a regional grid's edge leaves
  a lone node, whose cell only the spacing sizes. A rectangle no cell meets
  is a 404; the vertex fallback never applies to it. Keep
  `bbox_subset` enclosing (and `extract_bbox` with it): the cube trims its
  own axes from it instead.
- Steps come from `grid_steps`: no datetime is the run's last step and an
  instant its exact step, both a t-less `[z,]y,x` Grid; an interval every
  step inside it on a `t` axis (`[t, z, y, x]` like cube), even one step.
  The values budget counts steps × levels. A parameter some steps lack
  (an aggregate at the analysis step) is null there; one no step has is
  the canonical-level 400 when named, null when defaulted.
- Run `cargo test -p engine-grib area::tests::area_latency_replay -- --ignored
  --nocapture` for a 32-field replay with simulated 150 ms GET latency and no
  decoded cache. It compares serial storage with four concurrent reads, checks
  equal outputs/bytes and prints timings without CI timing thresholds.

## Cube queries

- `query_cube` (#925, `cube.rs`) is offered only by the pressure and
  model-level views (`supported_query_types` adds `cube` when
  `vertical_kind()` is set). Output shape is always `[t, z, y, x]`.
- Steps: `resolve_run` + `grid_steps`, the area query's: no datetime the
  run's last step, an instant its exact step, an interval every step of
  the run inside it. Levels are
  `selected_levels` (exact); parameters default to every key of the view.
- Check `check_cube_budget` before I/O with the known dimensions (both
  horizontal resolutions given ⇒ the full check), then again after the
  first field supplies the geometry, before allocating output or fetching
  another field. The first field's values are reused, like area.
- Remaining fields go through `runtime::run_field_jobs` (the query's `FieldPlan`);
  workers return the resampled `ny × nx` plane only. Skip levels no
  `resolution-z` position samples. Every field must have the first field's
  native subset axes, else the "different grid" 400.
- Resampling is `DecodedGrid::sample_subset` over nearest-index maps from
  `ds_core::cube::nearest_indices` (half a native cell of tolerance, so a
  position off a regional grid is null), reading the decoded grid in place.
  On a 360° grid an x position that misses retries one turn away, so the
  180° column of a global bbox samples −180°.
- At the native resolution the axes keep only the subset's nodes whose
  cell, half a spacing either side, intersects the bbox (`axis_map`; EDR
  1.2 `/req/edr/rc-bbox-response-cube` A, #966), the same rule as the area
  rectangle mask. The cells tile the grid, so a bbox between two nodes
  keeps the cell it lies in; none is a bbox off the grid.
- `west > east` bboxes are a 400 before any I/O (#667); a bbox off the grid
  is `LocationNotFound` (404).

## Wind components (#897)

- `ds_core::derive::Derived` (wired in `server/src/admin.rs`) derives
  speed and direction from u/v pairs, geopotential height from `z`
  (0/3/4) where the view has no `gh` (0/3/5) at that level (#948), and
  relative humidity and dew point from 0/0/0 with 0/0/6, 0/1/1 or, on a
  pressure view, 0/1/0 (#898); this engine only reports facts
  (`WindSource`: triple, level, served unit for every parameter, and
  `vertical` = the view's `vertical_kind()`, whose level a humidity from
  `q` reads as its pressure). Never compute a derived field here.
  `geopotential_tests` pins z in m² s⁻², `gh` = z / g₀ on EDR and the map,
  the skip when `gh` exists, and a single-level `z`'s terrain height;
  `moisture_tests` pins `2t`/`2d` → `2r`, a pressure view's `t`/`q` → `r`
  and `dpt` per level, and no second `RH`/`DPT` or single-level `SPFH`.
- Read GRIB2 flag table 3.3 bit 5 (`0x08`, Section 3 octet 55) from **every**
  message, in both the header probe (`metadata::read_metadata`) and the
  decode (`reader::uv_frame`), into `MessageMetadata::uv_frame` /
  `DecodedGrid::uv_frame` and the per-key `ParamMetadata` (with the triple).
  A key not yet probed reports `VectorFrame::Unknown`: speed only until its
  header is read, then direction too.
- `discovery::build_wind_facts` builds each view's `WindFacts` with its
  `RasterInfo` (`refresh_discovery`), so `wind_facts()` is an `Arc` clone.
  Single-level views give each parameter its canonical level identity
  (`hag:10`, `sfc`) and label; pressure/model views none (a request's `z`
  reads both components at that level). The canonical level of the newest
  run that has the parameter (`Catalog::advertised_keys`) is the pairing
  basis, so a component only an older run still has pairs (#1026). A
  derived speed's run is `resolve_parameters_reference_time` over u and v,
  through `select_run_step`: the run whose steps carry both, never the new
  run's u with the old run's v.
- The grid is reported `GridAxes::NorthAligned` because only Template 3.0
  decodes. A rotated or projected template must report its own axes there,
  or grid-relative components would silently pass as earth-relative.
- GRIB1 (code table 7, same bit) is not read: the engine is GRIB2 only.
- `wind_tests::derived_matches_native_arpege_speed_and_direction_over_the_domain`
  is the frame check against real data: `testdata/grib-arpege-wind` (13 KB,
  Météo-France ARPEGE 10 m u/v plus native 10si/10wdir, every 20th node of
  the 0.1° Europe grid; provenance in its README) served once as published
  and once as components only (`parameters = ["10u", "10v"]`). Derived and
  native agree within 0.02 m/s and 0.5° over the domain.
- Code Table 4.2 now has 0/2/0 wind direction (`°`) and 0/2/1 wind speed
  (`m s-1`), so native speeds and directions carry units.

## Discovery snapshots

- Publish `RasterInfo` snapshots after catalog changes and successful header/decoded
  metadata fills. `raster_info_shared` must only clone an Arc; Maps/Tiles/WMS use it.
- Precompute the sorted valid-time union during catalog publication. Geometry
  comes from one latest-run representative per view, with missing probes sharing
  the 32-message budget. Deduplicate parameter/geometry probes by source+offset.
- Keep only current representative geometries (including known unsupported ones);
  retry read failures. Never borrow another family's grid or advertise global
  bounds without a header. Native cells count intervals between GRIB nodes,
  including a cyclic closing cell, excluding duplicate seam nodes.
