# engine-grib crate — Claude Instructions

GRIB2 NWP data engine. Read the root `CLAUDE.md` first — Critical Rules 6–7
and 9 (the new-run probe once did 32 sequential blocking reads on one
thread) were learned here.

Only regular lat/lon grids (Template 0). Multi-parameter collections —
unlike GeoTIFF's one band per collection.

## Discovery & sources

- Discovers data via **index sidecar files** on S3/HTTP or a local
  directory; fetches messages via byte-range reads. The engine NEVER builds
  indexes itself.
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
  scan, at most eight concurrently, across the newest run's step files.
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
- Each catalog carries a nonzero render content version, hashed on the scan
  path from its runs and message identities. Late files can change default
  levels at an existing time: Maps/Tiles/WMS caches must follow that revision,
  and explicit-time HTTP images must revalidate. No-op rebuilds and changes
  confined to other families preserve a view's version.
- Upper-air queries use exact levels, with no canonical-level fallback.
  Missing parameter/level pairs are null in EDR and errors in Maps. Pressure/
  model labels omit a fixed-level qualifier. The shared vertical descriptor
  feeds both EDR and Maps; single-level fields retain their label qualifiers.
- New families register through the server's background registration/recovery
  loop using the accepted config. Config reserves all enabled derived IDs.

## Unit conversion (source-driven — never hardcode parameter names)

- Conversions are driven by the WMO `(discipline, category,
  parameter_number)` triple read from message headers, not by
  short-name tables. Source units come from WMO Code Table 4.2
  (`src/units.rs`) plus per-center overlays for local parameter numbers
  192–254.
- Display conversions are mechanical: K→°C, Pa→hPa, kg m⁻²→mm, m² s⁻²→gpm,
  proportion→%. Colormap ranges use display units. The string-keyed twin
  of this table lives in `ds_core::units` (used by engine-bufr); keep the
  two rule for rule.
- Distinguish absolute temperature from temperature differences using the
  WMO parameter semantics: dewpoint depression uses `KelvinDifference` and
  stays in K, with no Celsius zero-point offset.
- **Per-provider vocabularies are not needed.** A new provider only needs
  overlay entries if it uses local parameter numbers. ECMWF-`tcc` vs
  GFS-`TCDC`, `z` vs `HGT` are handled by construction (different triples).

## Model runs (#337)

Catalog keeps a `runs` map (`BTreeMap` keyed by reference time) and
implements the shared `ds_core::instances` contract (see root CLAUDE.md).
Two selections, never mixed (#967):
- **Maps** (`select_run_step`): without a parameter (the default layer;
  M trajectories use its `covering_run`) the newest run whose published
  valid-time extent covers the time, at its nearest step; with one, only
  the steps carrying it (#1005, below). `get_raster_tile(s)` and every
  `resolve_*` override go through it: they key the #507/#521 caches, so it
  must stay the render's own selection.
- **EDR** (`resolve_run`, `grid_steps`; position, area, radius, cube, 2-D
  and Z trajectories): by intersection (`/req/core/datetime-response` A, F).
  An instant takes only the step valid at exactly that time, never the
  nearest; a window every step inside it, open ends (`MIN_UTC`/`MAX_UTC`)
  included. The run: the newest covering the window's start with a step
  inside, else the newest with any step inside (`../end`). None inside is
  `LocationNotFound` (404), which a datetime list skips.

An incomplete newest run does not hide a covering older run; explicit run
pins do not fall back. No requested datetime still selects the latest run.

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
- Parameter discovery and bounded metadata probes include every step in the
  newest run: the analysis step commonly has no aggregates. An aggregate
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
- Cache weights include allocated value capacity and key/structure overhead.
  Keep the entry-size estimate aligned with the 4 MiB typical global field.
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
  admit bytes only after decoding succeeds. Reuse that cold decode; failed
  reads, invalid payloads and expired fills must remain retryable.
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
- The ignored `message_cache::tests::repeated_forecast_latency_replay` replays
  two different coordinates over 120 real global fields with 150 ms simulated
  GET latency, comparing 256 MiB decoded-only caching with an additional
  128 MiB message cache. It checks identical samples and reports timings/read
  bytes; no wall-clock thresholds belong in CI.

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
  union. The advertised parameters are still the newest run's: while it
  has only its analysis, its windows are no layer and a request naming one
  is a 400, although older runs could serve it.
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
  `resolved_keys_render_the_same_pixels_as_the_request` sweeps it. That is
  why rule 2 checks "inside the data" against every run, not the pinned one.
- `fetch_grid`: a parameter the run lacks is `InvalidParameter`; a step
  without its field at the requested level (a pressure view's `z`) is
  `LocationNotFound`.
- Tests: `parameter_times_tests.rs` (two synthetic runs, and the real GFS
  2026-04-08 f000/f003/f006 sidecars in `testdata/gfs`), and
  `server/tests/grib_parameter_times.rs` (WMS capabilities and GetMap cache
  keys, Maps, EDR metadata, through `DerivedWind`; with two runs, the run
  WMS, Maps and Tiles render and key where the newest lacks the window).

## Position queries

- `query_positions` samples every requested coordinate from each field once,
  even when the decoded-grid cache is disabled or smaller than the forecast
  window. Single-point, single-level and profile queries share this path.
- Keep at most four field fetch/decode jobs in flight per admitted EDR query.
  Jobs run on the dedicated query runtime with `block_in_place`, including
  synchronous cache-fill waits; never move storage calls to `spawn_blocking`.
  Direct off-runtime callers share one fallback runtime.
- Propagate the absolute EDR deadline to each job and its storage calls. Stop
  dispatching at expiry and drain running jobs before releasing query admission.
- Validate all coordinates and the complete points × times × levels × parameters
  value budget before fetching. Preserve point/time/level ordering regardless of
  job completion order, and sample missing fields as null for every point.
- Run the ignored `position::tests::position_latency_replay` test with
  `--ignored --nocapture` to compare serial and batched reads over 120 steps,
  with 150 ms simulated per-read latency and the grid cache disabled. It checks
  equal samples and prints timings/bytes; wall-clock times are not CI gates.

## Trajectory queries (#926)

- `trajectory.rs` selects the run (an M path's samples snap to the nearest
  step, so its run is `covering_run` of its first time, as a map's is),
  steps, levels and parameters with the position
  query's helpers (`resolve_run`, `position_parameters`, `selected_levels`)
  and leaves the path, time/level rules and layout to
  `ds_core::trajectory::TrajectoryPlan`. Densify at the published
  representative geometry's spacing (`RasterInfo` extent / grid cells;
  0.1° before the probe has run).
- One `run_field_jobs` job per (parameter, planned field = step × level):
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
- Use `runtime::run_field_jobs`, shared with positions, for at most four
  parameter/level jobs per admitted query. Keep deadline propagation, blocking
  cache waits and drain-on-error behavior in that scheduler. Area read failures
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
- Remaining fields go through `runtime::run_field_jobs` (four in flight);
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

- `ds_core::wind::DerivedWind` (wired in `server/src/admin.rs`) derives
  speed and direction from u/v pairs; this engine only reports facts
  (`WindSource`). Never compute a derived field here.
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
  reads both components at that level). The latest run's canonical level is
  the pairing basis.
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
