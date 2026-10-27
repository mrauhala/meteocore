# engine-netcdf crate — Claude Instructions

CF-NetCDF gridded forecasts and analyses (#1067), first for MET Norway's
MEPS. Read the root `CLAUDE.md` first — Critical Rules 5, 6, 7, 9 and 10
shape this crate. Today: local directories and remote files read in place
by HTTP range or through OPeNDAP (thredds.met.no; OPeNDAP only for MEPS
since 2026-10-22, #1264), WMS/Maps/Tiles, EDR position/area/radius/
trajectory/cube with runs as instances, GetFeatureInfo, derived wind speed, and a CF
pressure axis as the collection's vertical axis (PR 8, the last of the
#1067 plan), per-step amounts of run-start accumulations (#1165), and
curvilinear grids placed by 2-D latitude/longitude (#1174).

## Model

- **One collection is one file family** (MEPS `sfc` per-lead files): every
  file carries the collection's variables on one grid. The first file a
  poll ingests fixes the `Layout` (variables in allowlist or file order,
  grid); a later file missing a variable, on another grid, or with another
  level or units is skipped with a WARN. Units, and level units, compare in
  any spelling (`Variable::same_quantity`, `ds_core::units::same_unit`):
  `kg m-2` in a later file is the first file's `kg/m^2`. The first file's
  unit and standard name decide the display conversion (see "Display
  units"), and its spelling is served where none applies. The layout lives as long as the
  engine.
- **Time from the file, never its name:** valid times from the fields'
  `time` axis (`ds_core::cf::decode_times`, which reads MEPS's
  `… 00:00:00 +00:00`), the run from `forecast_reference_time` (scalar, or
  1-D holding one value). Files without one form one timeline without runs;
  a directory mixing both serves the runs (WARN once). The global
  `min_time`/`max_time` attributes are wrong in MEPS and never read.
  Non-standard calendars are refused, not approximated.
- **Variables:** `(y, x)` last, on 1-D latitude/longitude or 1-D projected
  x/y in m or km with a `grid_mapping` (`ds_core::cf::{classify_axis,
  crs_from_grid_mapping, RegularAxis, grid}`), or placed by 2-D
  latitude/longitude auxiliaries (see "Curvilinear grids"); one time axis; an ensemble
  axis read at `member`; every other axis of size 1, whose coordinate value
  and units are the level identity (MEPS puts 10 m wind on `height6`,
  `height7` or `height2` by family: never compare dimension names). At most
  one axis longer than 1 may be a **pressure axis** (its coordinate's units
  are a pressure, `ds_core::cf::PressureUnit`, and it is no parametric
  coordinate; see "Pressure levels"); any
  other axis longer than 1 (MEPS `hl` height above ground, `ml` hybrid
  levels) skips the variable with a WARN.
  Numeric types are read through the matching typed slice; CF
  `scale_factor`/`add_offset`, `_FillValue`, `missing_value` (compared in
  the stored type), netCDF's default float fill without a `_FillValue`,
  and NaN apply. `valid_range` is not applied.
- **2-D latitude/longitude auxiliaries are read only for a curvilinear
  grid** (#1174, `ingest::placement`): when a field's `(y, x)` have no 1-D
  latitude/longitude, nor projected x/y with a `grid_mapping`. MEPS carries
  them as Float64 next to every field, beside its Lambert axes and
  `grid_mapping`: there the axes win and the arrays stay unread
  (`ingest::tests::stated_axes_win_over_auxiliaries_and_names_decide_nothing`).

## Pressure levels (#1067 PR 8)

- **Identity:** a dimension is a pressure axis when its 1-D coordinate's
  `units` are a pressure (`PressureUnit::parse`: `Pa`, `hPa`, `mbar`, …;
  CF 1.11 §4.3 needs no more), by value, never by name (MEPS's is
  `pressure`, ERA5's `level`), unless it is a parametric coordinate
  (`ingest::pressure_unit`): one with `formula_terms` or a `standard_name`
  other than `air_pressure` is not, whatever its units (CF 4.3.3; CESM/CAM's
  hybrid `lev` is in hPa). A size-1 pressure axis stays a single
  level's identity, as before. `ingest::classify` gives it `Dim::Level`.
- **Levels:** `Variable::pressure`, in hPa (`PressureUnit::to_hpa`: Pa ÷ 100,
  so 85000 Pa is exactly 850, rounded to six significant digits, so a
  `float` 0.85 bar is 850, not 850.0000238), **bottom first** (largest
  pressure first, as engine-grib's pressure views), finite, positive and
  distinct, and
  `FieldRead::levels` the stored index of each served level (MEPS stores
  50…1000 hPa ascending, so served 0 = stored 12). `same_quantity` compares
  them: a later file on other levels is skipped.
- **One vertical axis per collection** (`ingest::one_vertical_axis`): the
  layout's variables are all on the same levels, or none has any. With an
  allowlist the first listed decides and a mismatch fails the file naming
  the variable; without, the fields are grouped by their levels (one
  level, or each distinct level set) and the largest group wins,
  single-level on a tie (so an sfc file stays as before), else the first
  group found, and the rest are skipped with why. MEPS's
  `pl` family is its own collection.
- **The axis:** `Layout::vertical`, `VerticalDimension(Pressure, levels)`,
  is `RasterInfo.vertical` (built once per poll) and
  `EdrEngine::get_vertical_extent` (from the same snapshot).
- **Map level:** `Layout::map_level(z)`, the one selection behind
  `get_raster_tile(s)`: `z` matched exactly (else `InvalidParameter` naming
  the levels), `None` ⇒ index 0, the bottom level. That is exactly the
  default `MapEngine::resolve_elevation` (`z`, else `levels[0]`), so there
  is no override and GetFeatureInfo queries the level GetMap draws; the
  rendered caches key on the requested `z`, one level per key. Pinned by
  `tests/integration.rs` `a_map_without_elevation_draws_the_level_get_feature_info_queries`
  and the server's `wms_get_feature_info`
  `a_netcdf_pressure_level_pixel_reads_the_level_get_map_draws` (GetMap
  bytes and GetFeatureInfo's reported level). Exact, as GRIB, though WMS
  capabilities say `nearestValue="1"` on every `ELEVATION` dimension (a
  radar volume does draw the nearest sweep).
- **EDR levels:** `Layout::query_levels(z)`: `z`'s levels kept in request
  order, each once, a missing one dropped, none ⇒ 400; without `z`, every
  level (clause F, as GRIB). Responses: one level ⇒ `PointSeries` with a
  one-value `z`; several ⇒ a `VerticalProfile` per step; area ⇒ a `z` axis
  even for one (`[t, z, y, x]`).
- **Per-level everything:** `FieldKey.level` is the served index, so the
  field cache, single-flight, detached fills and the decode budget work per
  level. The one exception: an EDR query through OPeNDAP reads the levels it
  asks of a variable at one step as one window over their stored span
  (`ingest::Span`, see "OPeNDAP", EDR windows). Prewarm fetches index 0 only
  (13 MEPS levels would be 1.3 GB per variable and run).
- **Not served:** height above ground (`VerticalKind::Height` is above
  MSL) and hybrid levels (`ModelLevel` expects ordinal levels).

## Curvilinear grids (#1174)

- **Identity:** `ingest::placement`. A field's CF `coordinates` names
  2-D variables on exactly its `(y, x)`, one a latitude and one a
  longitude by `standard_name` or `degrees_north`/`degrees_east` units
  (`ingest::stated_role`: `classify_axis` without the name fallback),
  never by name; exactly one of each (`ingest::auxiliaries`). Where the
  field also has 1-D axes, they win only as their own coordinate
  variables' attributes state them: latitude/longitude, or projected x/y
  with a `grid_mapping` (MEPS). A bare index axis named `x`/`y`, which
  `classify_axis` reads as longitude/latitude by name, loses to the
  auxiliaries; without auxiliaries the old rule stands unchanged.
  `classify` then skips its `(y, x)` role check (`curvilinear`).
- **The grid:** `FileGrid::Curvilinear(Arc<CurvilinearNodes>)`. Each
  file's two arrays are read whole (`ingest::read_whole`: slice read,
  packing, `_FillValue`/`missing_value`/NaN as a node without a position)
  once per file, into `f32` unit vectors (12 B/node); the arrays are not
  kept. They are the grid identity: `FileGrid::same` compares node by
  node, to a thousandth of each node's spacing (at least 1e-6 rad), so a
  later file on other nodes is skipped. Remote cost: a curvilinear remote
  family reads both arrays per file opened. Rows are as stored, never
  flipped; `transform()` is `None`, `size()` `(nx, ny)`. At most
  `ds_core::resample::curvilinear::MAX_NODES` (4096²), checked from the
  dimensions before any read (`CurvilinearNodes::check_size`): an
  oversized file is skipped unread. The read reserves its transient peak
  from `decode_budget::BUDGET` (`ingest::curvilinear_peak_bytes`: 16 B for
  both `f64` arrays + 12 B of vectors + 3 × the stored size, 52 B/node for
  `double`); a full budget is `ingest::Unservable::Busy`, which
  `open_inspect` turns into `Opened::Retry`: the file is opened again next
  poll, never remembered as skipped. Packing is applied in place
  (`mapv_inplace`, then the array's own buffer), so no second full-size
  `f64` buffer exists.
- **The lookup:** `Layout::new` builds `CurvilinearGrid` (k-d tree over
  the nodes, 5 B/node, and the extent) once per layout, inside
  `SourceGrid::from_curvilinear`. Everything downstream is the shared
  sampler: `SourceGrid::output_map` evaluates `CurvilinearGrid::locate`
  at the coarse `ProjectionGrid` nodes only (Critical Rule 5), with the
  footprint guard over the extent; `SourceGrid::position` is the same
  lookup for EDR, so a map pixel and an EDR value at one point stay the
  same number. `locate`: nearest node from the tree (chord on unit
  vectors: no antimeridian or pole case), then the cell around it
  inverted on the gnomonic plane at the point (Newton), walking at most
  `MAX_WALK` cells; NaN when the nearest node is beyond `REACH` (3) of its
  own neighbour spacing or the edge cell's extrapolation lands more than
  `EDGE_MARGIN` (2) nodes past the outer nodes. The sampler's usual rules
  then apply (bilinear, nearest at a nodata neighbour, drawn up to one
  node past the outer nodes, the half cell past the outer cell edges).
- **Metadata:** `native_crs` `"curvilinear"` (no `storageCrs`), no
  `native_grid`, `grid_size` `[nx, ny]`, `spatial_extent`
  `CurvilinearGrid::extent` (outer nodes moved half a spacing out, walked
  as a ring through `ds_core::geo::ring_extent`; a pole the grid holds,
  found by locating it, makes −180…180 up to it). `engine.grid()` is
  `None`. Wind: `GridAxes::Skewed`, so grid-relative components give
  nothing, earth-relative ones speed and direction.
- **EDR:** unchanged code; area, cube and trajectory spacing use
  `SourceGrid::size()` over the extent's degrees, so a grid around a pole
  samples longitude at 360° / nx, as the projected polar grids do.

## Discovery

`discover::list`, over `data_path`, non-recursive, symlinks to files
followed, temporary names (`ds_core::temp_files`) skipped:

- with `filename_template` (ds-storage's vocabulary, as
  `meps_sfc_{lead}_%Y%m%dT{run}Z.nc`): `ds_storage::discovery::scan_local`
  with its `FilenameMatcher`, which reads `{run}` as the timestamp's hour
  and `{lead}` as a lead and keeps one file per (timestamp, lead), so every
  lead of a run; ordered newest name timestamp first, then by lead. A
  template the matcher rejects (no strftime codes, two `{lead}`s) fails the
  collection at load (`Config`).
- without one: every `*.nc`, newest modification first.

The name only selects the file and orders the opening: valid times and the
run always come from the file.

**Remote** (`data_path` = `http(s)://…`, `NetcdfConfig::is_remote`):
ds-storage's `TemplateProber` over `Schedule::Runs` (`prefix_pattern` with
`{run}`, `filename_template`, `run_hours`, `leads`, `time_window`, settling
`Stable { min_age }` default 10 min, `run_grace` default 30 min, not
`immutable`: published sentinels are re-HEADed every 30 min). The prober
HEADs through the host's background share. Its published files of the
newest `max_runs` run slots, newest run first, lead order, are the listing
(an older run's files would be opened only for the retention to close
them: ~80 MB a MEPS run), plus the open files of an older slot that serves,
until `max_runs` newer slots serve (#1290); a key is the `Known` map's path. A run is
published whole (or after its grace), so the engine never sees a run
half-uploaded. Every request of a probe failing (`ProbeReport::unreachable`,
HEADs and THREDDS catalogs) is an `Err` (the catalog stays, `/health`
degrades after five polls).

**Remote with `catalog_url`** (#1170): `Finder::Catalog`, ds-storage's
`CatalogProber` (`discovery::thredds`): the same `TemplateSpec`, its checks
answered by THREDDS catalogs through `TemplateProber::poll_with` instead of
HEADs, so it lists exactly the files probing lists, settled and published by
the same rules over the catalog's rounded size and `date type="modified"`.
The walk from `catalog_url` follows only `catalogRef`s toward directories of
slots in the window (never another origin or outside the root), ≤ 64
catalogs per poll, and keeps each found directory's URL while a slot lies
under it. Catalog sizes have four significant digits, so each listed file is
HEADed once for its exact `ObjectVersion` (what range reads check), again
when its catalog entry changes or `recheck` is called. Catalogs go through
`remote::host(collection_id, catalog_url)`'s background share. THREDDS
streams catalogs chunked without `Content-Length`; they are read with
`DataStore::get_documents` (ds-storage `document.rs`: the store's own
throttled transport under all its layers, the 16 MiB cap enforced while
streaming). With `access = "opendap"` (#1264) the prober is built
`with_service(FileService::OpenDap)`: keys lie under the catalogs'
`OpenDAP` base, and nothing is HEADed, a file's `ObjectVersion` being its
catalog entry's (rounded size, `date type="modified"`); see "OPeNDAP".

The poll (`poll_once`, on `poll_runtime()`, Critical Rule 6) remembers every
listed file by path and identity (`catalog::Identity`, an `ObjectVersion`:
size, modification time or `Last-Modified`, ETag):

- new or changed files are opened, newest first, at most
  `MAX_OPENS_PER_POLL` (128) per poll (Critical Rule 9);
- a catalogued file whose identity changes is read again. Its entry, and
  its old handle in the catalog, stay until the new version is open
  (`record`): past the cap, or when the open must be retried, the old
  version keeps drawing and the next poll finds the change again. Only
  then does `content_version` move (#1067 decision 7), with the catalog
  that serves the new version; a new file moves nothing
  (`scan_tests`, a cap of one);
- runs count once a file of theirs is open (#1290): the newest `max_runs`
  runs with an open file are kept, as many as `max_files` holds (without
  runs the `max_files` newest files). A closed-only run among the newest
  `max_runs` is kept after them, in the room they leave, to be reopened.
  With the probe's listing above, a run listed but not yet open never
  drops a serving one (`catch_up::served_runs_stay_until_a_newer_run_serves`).
  Files outside are closed
  (`Known::Closed`, run and newest time remembered, never reopened unless
  they come back into the kept set); unservable files are remembered as
  `Skipped` and not opened again until they change.

Opens happen in two phases so remote files open in parallel (Critical
Rule 9): `open_inspect` (no poll state; `REMOTE_OPEN_CONCURRENCY` = 8
scoped threads for a remote source, inline for a local one), then `admit`
in listing order (`fits_layout`). Without an allowlist the first servable
file is opened alone, since it fixes which variables the rest are read
for. A remote open that failed on a read (network, deadline, replaced) is
`Opened::Retry`: forgotten, tried next poll; any other failure is
`Skipped`. Once `MAX_THROTTLED_OPENS` (3) opens of a scan fail while the
host answered 429 (`ds_storage::throttled_answers` before and after the
open), the scan starts no more opens; those in flight finish. Reopens go
newest run first, like new files. A scan logs one line for its opens: WARN
if a read failed, INFO if its time limit cut it; the per-file line is
DEBUG. A remote poll may open `max(MAX_OPENS_PER_POLL, leads)` files,
so at least one whole run.

`NetcdfEngine::new` scans once, synchronously, as engine-grib does, so the
layers and their styles exist when the server registers the collection; a
failed scan is a WARN ("initial NetCDF scan failed (will retry on poll)").
Every catalog is built by a scan and swapped through `ArcSwap`.

- **Local:** the whole directory; the poll loop's first tick is skipped.
- **Remote, bounded:** up to `INITIAL_PROBE_ROUNDS` (3) probes until a run
  is published (round 1 finds the sentinels, round 2 HEADs the found runs'
  other leads, which settle at once when older than `min_age`), then at
  most one run's leads (the newest run's files) under a deadline of
  `INITIAL_SCAN_BUDGET` (15 s) from the start, entered on every open
  thread. Why the bound: `load_collections` builds collections one after
  another and the server answers nothing until all are built (main.rs
  "Socket bound — loading collections, not yet serving", and the same on
  reload, on the poll runtime), so a remote `new()` delays every collection
  after it and the whole boot. 67 MEPS opens are ~67 MB at the background
  share of 2: seconds from a server's network, tens of seconds over a slow
  link. The worst case is the budget plus one wave of HEADs (30 s each).
  The rest is the poll loop's: its first tick fires at once
  (`FirstTick::Immediate`), and `netcdf_registration_changed` registers the
  layers if `new()` found none.

## Open files and metadata — measured

Each catalogued file keeps its reader (`catalog::OpenFile::reader`, a
`source::Reader`: its `NcFile`, or through OPeNDAP its `DapFile`) for as
long as it is catalogued; request paths never open a file. Local files are opened
over `FileStorage` (positional reads) for NetCDF-4 and classic alike, never
`NcFile::open`'s mmap, which a file truncated in place would turn into
SIGBUS. One descriptor per catalogued local file: `max_files` bounds them.
Remote files are opened over `remote::RemoteFile` (below). The reader's
chunk cache is off (`chunk_cache_bytes = 0`) either way.

- **Metadata trees:** `NcFile::variables()` and `variable(name)` build two
  separate trees. Ingest reads only `variables()`; nothing calls
  `variable(name)`. The request read, `read_variable_slice`, uses neither: it
  resolves `Hdf5File::dataset(name)` (a template cache per path) and looks
  up the chunk index for the exact chunk bounds (a 32-entry LRU per
  dataset). Ingest warms the dataset lookup and walks each served
  variable's whole index once through `iter_variable_chunks`, which reads no
  chunk. The bounded lookup a request makes still walks the B-tree once per
  bounds (locally a positional read; PR 5's block cache covers it remotely).
- **Reads, MEPS fixture file (18 variables), counted with a storage
  wrapper** (`ingest::tests::an_ingested_field_reads_only_its_index_and_chunk`
  pins the last three): open 19 reads / 11 KB; `variables()` 268 / 213 KB
  (no block cache under the reader); chunk shape through a second
  `Hdf5File` 17 / 3 KB; the rest of `inspect` 66 / 24 KB; warming one
  variable 8 / 1.5 KB. A first field read after warming: 3 reads (index +
  chunk), against 9 without; a repeat 1 read (the chunk). `variable(name)`
  after ingest would cost another 175 reads / 200 KB.
- **Chunk shapes** come from a second `hdf5_reader::Hdf5File` over the same
  storage at ingest: the NetCDF metadata does not carry them. They size the
  decode budget, and a chunk spanning several fields logs a WARN (every
  field read then inflates them all).
- **Heap held per open file** (`cargo run -p engine-netcdf --example
  handle_memory`): 140 KB per MEPS fixture file serving 8 variables, 100 KB
  serving 1, 61 KB for the global fixture (2 variables). It grows with the
  variables in the file, all of which `variables()` parses, so a real MEPS
  sfc file (221 variables, 190 MB) holds far more: **820 KB** per kept
  per-lead sfc handle serving one variable (`tests/live_thredds.rs`,
  2026-10-17, heap allocated by a second open of the same three files from
  the block cache). Handles alone hold 820 KB × 67 leads × `max_runs`:
  **110 MB** at the remote example's `max_runs = 2`, 220 MB at 4.

## Remote files (`remote.rs`, `io.rs`) — #1067 PR 5

Read in place by HTTP range, never downloaded whole (decision 4).

- **Storage:** `RemoteFile` implements `hdf5_reader::Storage` over
  `DataStore::get_range_if_unmodified_on`. Every GET is conditional on the
  version the prober listed (`If-Unmodified-Since`, plus the response's
  size from `Content-Range`, `Last-Modified` and ETag compared:
  ds-storage's `range_tests::a_conditional_read_over_http_sends_and_checks_the_version`
  pins that a 206 carries the whole object's size).
- **Runtime (Critical Rule 7, a tested exception):** the callback is
  synchronous; `io::blocking` runs the GET on the persistent `netcdf-io`
  runtime (`Handle::block_on`): `block_in_place` on a multi-thread worker
  (a render on a request worker, the poll loop), directly on a blocking or
  plain thread (`RenderJob`, a detached fill, ingest's open threads), a
  scoped thread on a current-thread runtime. The deadline is the calling
  thread's (`ds_storage` captures it). Pinned by
  `io::tests::every_calling_thread_reaches_the_runtime` and
  `remote::tests::reads_run_on_the_poll_runtime_render_jobs_and_async_workers`;
  EDR queries on the shared query runtime's workers and GetFeatureInfo's
  `RenderJob::run_query` by
  `remote::tests::queries_read_on_the_query_runtime_for_edr_and_get_feature_info`.
- **Fetch modes** (`io::Mode`, a thread-local like `ds_core::deadline`):
  `Request` (default: the host's whole limit, `FetchBudget::Request`, the
  request's deadline), `Detached` (a fill a request handed off: whole
  limit, `Background` budget, no deadline), `Background` (every read a
  poll makes, ingest and prewarm: the host's background share,
  `Background` budget). `scan` and every open/prewarm thread enter it.
- **Host limit:** `remote::host` → `ds_storage::build_http_store_limited`
  (`HOST_REQUEST_LIMIT` = 8, one limit per host for the process, over
  every collection) and a `BACKGROUND_SHARE` (2) stacked on it, also one
  per host (`with_request_limit`). The prober's HEADs take the share too.
  `remote::host(collection_id, …)` returns that host's stores named with
  `DataStore::for_collection` (one `Host` per collection over the shared
  limit, share and `gets` counter), so a retry line names the collection
  (#1233); find a host by `DataStore::same_store`, never `Arc::ptr_eq` on
  `inner()`.
  The 8 adapt (#1235): a 429 halves them, at most once a second, never
  below 1; each quiet 30 s adds one back. A 429's `Retry-After` (≤ 60 s)
  pauses every request to the host. After a 429 the host's requests
  start at least the pace apart (#1290): 250 ms, doubling at most once a
  second to 2 s, halving per quiet 30 s, off below 250 ms. A 429 without
  `Retry-After` is retried after 2 s. Both are shared by every store on
  the host; `storage_host_throttled_total{host}` counts the 429s. Permit,
  pace and `Retry-After` waits count against the deadline. Tests against
  a throttling `TestServer` use `ds_storage::test_util::quick_backoff`. `ds_storage::host_requests` reports the current
  limit, in flight and queued per host.
- **Blocks** (`BLOCKS`, `MC_NETCDF_BLOCK_CACHE_MB`, default 64 MiB, floor
  8): reads ≤ `Tuning::bypass_over` (256 KiB) come from 64 KiB blocks keyed
  by `FileId` (URL + version) and index, `approx_entry_bytes` 64 KiB. A
  read's missing blocks are claimed with zero-timeout guards and fetched
  in one GET (copied per block, so no block pins a span); blocks another
  reader is fetching are waited for (single-flight). `prefetch` fetches the
  first 1 MiB in one GET before the reader parses it (MEPS keeps all its
  metadata there, measured). Keyed by identity, not ingest: a second open
  of the same version (reload, a closed file kept again) makes 0 GETs.
- **Chunks** (`CHUNKS`, `MC_NETCDF_CHUNK_CACHE_MB`, default 512 MiB): reads
  over 256 KiB, cached whole by (identity, offset, length), single-flight:
  8 renders of a cold field make one GET. Equivalent to the issue's
  (identity, variable, time, level) key without passing a field context
  down to the storage callback. An EDR query's fields land here, never in
  `FIELDS` (see EDR below).
- **Replaced file** (decision 7): `RangeRead::Changed` (412, 404, or another
  size/`Last-Modified`/ETag) marks the `RemoteFile` replaced (every later
  read fails at once), drops its blocks and chunks (`retain` by `FileId`)
  and its ingest's decoded fields (`field::evict_file`), queues
  (key, version) on the engine's `Changes`, and, from a request or detached
  fill, bumps `content_version` at once (`content_version()` =
  catalog's + `Changes::bumps`). The read is `ReadFailure::Replaced`,
  which a request gets as `ResourceExhausted` (503). The next poll
  forgets the file (bumping again if it was open), calls
  `TemplateProber::recheck(key)` so that poll HEADs it first, and opens the
  new version once it settles; the same version is opened again. A
  replacement or removal the prober finds itself (its 30-min sentinel
  recheck) withdraws a file whose run is still in the window: the poll
  bumps `content_version` for it too. A run aging out of the window bumps
  nothing.
- **Errors:** `ReadFailure` travels through hdf5-reader as an `io::Error`;
  `ReadFailure::find` walks the error chain (through `Context` and the
  `io::Error`'s inner value) so a deadline stays `DeadlineExceeded`.
- **Request counts — measured** on testdata/netcdf-remote (62 KB files,
  production sizes ÷ 64, `remote::tests::TUNING`): an open is 4 GETs (the
  first read, which covers all `variables()` metadata, and three block
  reads: two chunk indexes and the time axis), a cold field 1 GET (its
  chunk), a repeat 0, a second open of the same identity 0.
- **MEPS on thredds.met.no — measured 2026-10-17 over a laptop link, debug
  build** (`tests/live_thredds.rs`, newest sfc run 09Z, leads 00-02, one
  variable): first scan 4.0 s, 5 HEADs, 13 GETs, 3.71 MB (about 4 GETs
  and 1.24 MB per file: the 1 MiB first read and three 64 KiB blocks); a
  second open of the same files 0 GETs, 0.2 s; a cold field 1 GET,
  1.57 MB, 3.5 s (over the 3 s render deadline: detached fills matter on
  such a link); the same field again 0 GETs, 34 ms. A 206 for bytes
  0-1023 named the whole file's size (167 786 505), as HEAD did. A server
  smoke test (two variables, the same three files) loaded the collection
  in 1.7-2.0 s with 1.31 MB per file, drew a cold GetMap in about 1 s
  (384 × 512 EPSG:3857 over Norway; 355 × 600 EPSG:3067 over Finland,
  every pixel drawn) and map tiles of the same field in 25-30 ms. By range
  not measured from a server's network: thredds.met.no serves MEPS through
  OPeNDAP only since 2026-10-22. Its OPeNDAP numbers from a server's
  network, and the cache defaults kept, are under "OPeNDAP" (#1171).

## OPeNDAP (`opendap.rs`, `dap.rs`, `source.rs`) — #1264

thredds.met.no binds `meps25epsarchive` and `mepslatest` to `dapOnly` since
2026-10-22: every fileServer HEAD or range read answers 403. `[netcdf]
access = "opendap"` reads a remote family through DAP2.

- **Config** (`ds_core::config::NetcdfAccess`): `"http"` (default) or
  `"opendap"`, remote only (a load error on a local `data_path`, as is
  any value but these); `opendap` needs a `catalog_url` and a `data_path`
  without query or fragment (the query is the constraint). `data_path` is
  the OPeNDAP directory (`…/thredds/dodsC/<dir>/`); a file is read at
  `origin/key` plus `.dds`, `.das`, `.dods`.
- **One seam, `source::Reader`:** `Nc { nc, storage, remote }` or
  `Dap(DapFile)`, with `variables()`, `hdf5()` (chunk shapes, NetCDF-4
  only), `read_all(var)` (a whole variable as stored, `f64`),
  `read_field(read, time, level)` (a `Raw` in the stored type), `warm`,
  `is_remote`, `read_failed`. Ingest and `field::decode` read only
  through it, and `Raw::decode` applies packing, fill and the flip for
  both: never branch ingest, field, EDR or render logic on the backend.
- **Metadata** (`dap::metadata`): the DDS's top-level arrays, scalars and
  Grids (by the Grid's name, its array's type and dimensions) become
  `NcVariable`s with their DAS container's attributes, typed as the DAS
  says. THREDDS prints a `Float32` in its shortest round-trip form, so
  `9.96921E36` parsed as `f32` is netCDF's default fill exactly. DAP2's
  `Byte` is unsigned: `_Unsigned = "false"` makes it `NcType::Byte`, its
  `UBytes` attributes reinterpreted. `DODS_EXTRA.Unlimited_Dimension`
  marks the record dimension. Structures and sequences are skipped. A
  numeric attribute that does not parse fails the file: a dropped
  `_FillValue` would serve fill as data. The parsers recurse once per
  nesting level, so structures, Grids and DAS containers nest at most
  `dap::MAX_DEPTH` (32) levels: deeper is an error, never a stack
  overflow. Every size a DDS states is the server's word: size arithmetic
  saturates (`dap::xdr_bytes`, `FieldRead::field_bytes`).
- **Open** (`DapFile::open`): `.dds` and `.das` in one
  `get_documents_on` round, then one `.dods` of every numeric scalar and
  1-D coordinate variable (`dap::whole`), kept as `f64` for `read_all`;
  any other variable read whole (a curvilinear grid's 2-D latitude and
  longitude) is one more `.dods`. Measured 2026-10-23: 3 requests, 158 KB
  for a MEPS sfc file (DDS 50 KB, DAS 98 KB, coordinates 9.5 KB), 23 KB
  for a pl file; 254 KB of heap per kept sfc handle serving one variable
  (`tests/live_thredds.rs`), against 820 KB read by range. A failed
  request is `OpenError::Read` (`Opened::Retry`), an unreadable answer
  `OpenError::Format` (`Skip`). A reply's cap follows the sizes the DDS
  declares, so every whole read is checked against a hard cap before it
  is asked for: a file's scalars and 1-D coordinates together, or one
  whole scalar or 1-D read, at most `opendap::MAX_COORDINATE_BYTES`
  (16 MiB in XDR; over it the open is `Format`), a 2-D one
  `CurvilinearNodes::check_size`, nothing else. A whole read reserves its
  peak, the reply buffered plus its values decoded and widened to `f64`,
  from the decode budget first: the coordinates' at open (no room is
  `OpenError::Busy`, `Opened::Retry`), a scalar or 1-D one in `read_all`
  (no room marks the file failed, so `Retry`); a 2-D read's is its
  caller's, `ingest::file_grid`'s `curvilinear_peak_bytes`, as by range.
  `DapFile` holds the budget it reserves from (`decode_budget::BUDGET`;
  tests pass their own). No block cache: a second open of the same
  file reads its metadata again.
- **Fields:** one `.dods` per field (one level of a pressure axis; an EDR
  read may span several levels and variables, below), its
  constraint `dap::constraint`: `grid.array` for a Grid's array (THREDDS
  wraps it in a `Structure`), the name for a plain array, `[i]` per fixed
  axis and `[start:stride:stop]` (stop inclusive) per grid axis, brackets
  sent as `%5B`/`%5D`. `dap::decode` follows the reply's own DDS:
  arrays carry their length twice, `Int16`/`UInt16` are widened to 32
  bits, a `Byte` array is padded to four bytes, scalars are bare, and
  variables come in dataset order, not the constraint's. The reply is
  kept in `remote::CHUNKS` as `Part::Dap(constraint)`
  (`remote::dap_reply`, single-flight), where a range read keeps its
  compressed chunk, so prewarm, detached fills, an EDR query's retries
  and a render and a query of one field behave as by range; a reply
  without `Data:` is never cached (`dap::check`). `FieldRead.chunk_bytes`
  is `field_bytes` (no HDF5 view), so the decode budget and the EDR byte
  cap count a whole field as a contiguous one.
- **EDR windows (#1171):** an EDR or GetFeatureInfo query through OPeNDAP
  reads only the cells it samples. `edr::NetcdfEngine::plan_reads` joins each
  field's positions' `SourceGrid::cells_read` (the four cells `value_at`
  reads, the nearest included; ds-core `resample::sampler`) into one
  `CellWindow` per field, each distinct positions list's once;
  `FieldRead::window_selection` makes it the stored hyperslab (rows
  reversed for a south-up grid), and `field::QueryField::value_at`
  samples it through `SourceGrid::value_in`: the same cells in the same
  arithmetic as `value_at` on the whole field, bit for bit
  (`sampler::tests::a_window_samples_what_the_whole_field_samples`).
  `Reader::reads_windows` is true for `Dap` only: a NetCDF file reads a
  whole chunk whatever the window, and the range path stays as it is. A
  window over half the field's cells (`edr::worth_a_window`) reads the
  field whole: a whole reply serves every later query and map of the
  field, a window only itself, so past half a window then the field
  costs more than the field.
- **Levels in one request:** the levels a query asks of one variable at one
  step are one job: one `.dods` over the stored levels from the lowest asked
  to the highest (`FieldRead::span`; a contiguous range, never a DAP stride;
  `[i]` for one level, so a one-level constraint is as before), over the
  window joining theirs, while `worth_a_window(window, span levels, asked,
  …)` holds (cells × span levels × 2 ≤ field cells × asked). Position, area
  and cube levels share one window; a Z trajectory's levels each sample
  their own cells and join only while that test holds, else each level is
  its own window or whole field. The reply decodes to one plane per stored
  level (`Raw::decode_plane`, each flipped alone), each level sampled
  through `value_in` bit for bit
  (`ingest::tests::a_window_over_several_levels_holds_each_levels_window`,
  `opendap::tests::windows` over testdata/netcdf-pressure-13). Measured
  2026-10-24 from a server near thredds.met.no: a 2 × 2 window 0.27 s at one
  level, 0.53 s at all 13, so a 13-level series over a 67-lead run is 67
  requests (~9 s), not 871 (~60 s).
- **Variables in one request (#1274):** `edr::requests_of` packs one step's
  windows (one file and time index) into requests, first fit (`edr::pack`)
  over the members sorted by variable index then first stored level: a
  variable at most once a request, at most `edr::MAX_REQUEST_PLANES` (16)
  planes, a member's planes its span's stored levels; one past 16 is a
  request alone; whole-field reads stay single. An `edr::Job` is one request
  of `edr::Member`s (variable, levels, window, asked).
  `DapFile::read_fields` joins the hyperslabs' constraints with `,` in
  member order and finds each in the reply by its leaf, the k-th of a leaf
  for the k-th member in dataset order: THREDDS answers in dataset order,
  not the constraint's (testdata/netcdf-opendap `…_rh_t2m_2x2.dods`,
  captured 2026-10-24), as `test_server` does. Measured 2026-10-24 from a
  server near thredds.met.no, 2 × 2 MEPS `pl` windows: about 0.25 s per
  request plus 0.016 s per plane (1 × 1 0.23-0.34 s, 3 × 1 0.35-0.42 s, 1 ×
  13 0.49-0.59 s, 5 × 13 1.1-1.6 s), so 16 planes stay near the 0.55 s the
  read cap was sized for: a six-variable meteogram over a 67-lead run is 67
  requests, not 402, a derived 10 m wind speed 67, not 134. One variable's
  windows at one step (a Z path's per-level ones) never share a request. The
  bound counts planes, not cells: large area or cube windows group too.
- **Caching windows:** `read_uncached(file, time, members)` leaves out of
  the request a member every level of which is held whole (`FIELDS` hits or
  whole replies in `CHUNKS`: `Reader::holds_field`, `remote::dap_cached`, no
  request, no recency bump) and decodes it from those; the other members'
  windows are one read (`read_parts`). A request is a reply of its own in
  `CHUNKS` (keyed by its whole constraint, variables and levels included,
  weighted by its bytes and its constraint's length, an error reply never
  cached by `dap::check`), a `QUERY_IN_FLIGHT` key of its own (`QueryKey {
  file, time, parts }`, each part a variable's name, span and window in
  planned order), and reserves the sum of its members' levels' shares of the
  whole field's decode peak (`field::read_peak_bytes`). A query of a subset
  of a request's variables misses and makes its own requests. Fan-out:
  `run_field_jobs`, four jobs at a time, detached on `field::QUERY_READS`.
  Measured 2026-10-24 from a server's network: a whole MEPS field 4 066 402
  bytes in 0.25 s, a 2 × 2 window 428 bytes in 0.10 s, so a 67-lead series
  is about 29 KB per variable instead of 272 MB. Cache defaults kept
  (512/256/64 MB): EDR no longer churns `CHUNKS` with whole fields, while
  renders and prewarm still fill it with them.
- **Transport:** `DataStore::get_documents_on` (ds-storage `document.rs`):
  `DocumentRequest { path, query }`, the query appended by the host's
  `Documents` layer (after the base URL's own), which marks the result
  `Sent`; any other store fails a queried request. `FetchedDocument`
  carries `Last-Modified` and ETag; a 404 is `Ok(None)`. Replies stream
  chunked without `Content-Length` and are read whole, capped at
  `dap::xdr_bytes` + 64 KiB per field and 16 MiB per DDS or DAS. Driven on
  `io::handle()` through `io::blocking` under the thread's `Mode`, the
  host's limit, share, back-off and `storage` span: Critical Rule 7 as by
  range.
- **Measured 2026-10-23** (thredds.met.no, laptop on a mobile link):
  `.dods` comes uncompressed although asked `Accept-Encoding: deflate,
  gzip`, so a MEPS field is 4.06 MB against a 1.5 MB chunk by range; no
  `Last-Modified` or ETag on any DAP reply; every reply chunked. Live
  test: first scan of two files 2.2 s (10 catalogs, 6 GETs, 668 KB with
  the catalogs), a cold field 1.4 s. Server smoke test: collections
  loaded in 1.6 s (sfc) and 0.7 s (pl); a cold field outlasted two 3 s
  render deadlines (503, its detached fill completing) and was then drawn
  in 0.1 s; an EDR series over two leads with one cold field 4.6 s.
- **Replaced files:** with no version in DAP replies, a file replaced in
  place is found by the poll once its catalog entry changes (its run's
  30 min recheck included); until then reads return the new file's values
  under the old ingest. A 404 on any DAP request, or a reply whose
  `Last-Modified` differs from the `.dds`'s (a server that sends one), is
  `remote::forget_replaced`: chunks and fields dropped, `Changes` told,
  the content version moved from a request, the read
  `ReadFailure::Replaced` (503).
- **Not served:** DAP4, OPeNDAP for local files, string variables. MEPS
  stores one lead per file, so a series is one request per lead for up to
  16 planes of its variables and levels: the EDR windows above, never one
  request per series.
- **Tests:** `dap::tests` (THREDDS's own MEPS DDS, DAS, coordinates and
  4 × 4 replies, testdata/netcdf-opendap; every XDR type, DAS escapes,
  types and signed bytes, constraints, a server's error);
  `opendap::tests` (`test_server`'s DAP2 datasets of the MEPS sfc and pl
  fixtures, as Grids and as plain arrays, against the files read locally,
  every parameter, level, run and time and the EDR series; the requests
  an open and a field cost; replaced and removed files; THREDDS's
  metadata ingested; the shipped examples; `opendap::tests::windows`,
  the EDR windows of #1171: a series reading one 2 × 2 window per lead,
  every query shape against the files read locally, the half-field
  fallback and a whole field cached sampled whole, packing, fill and the
  global seam, and THREDDS's own MEPS metadata and 4 × 4 reply answering
  a window's exact constraint; over testdata/netcdf-pressure-13, one
  request per variable and step at every level, both caps refusing before
  any request, non-adjacent `z`, a Z path batched or per level, whole
  fields counted whole, a batch outliving its deadline; #1274: a series of
  several variables one request per step, its constraint in reverse
  dataset order, the 16-plane bound, a request of several variables
  outliving its deadline, a subset's own requests and a member held whole
  left out, THREDDS's own two-variable reply and the mock answering
  alike); ds-storage
  `document::tests::a_query_reaches_the_server_and_the_version_comes_back`
  and `thredds::tests::opendap_files_take_the_catalogs_version_without_a_head`;
  ds-core `opendap_access_needs_a_catalog`; `tests/live_thredds.rs`
  `meps_through_opendap_from_thredds` (about 5 MB).

## Reads (`field.rs`)

- **Only `read_variable_slice::<T>` with an explicit hyperslab** (`FieldRead::
  selection`), never `read_variable*`/`read_array`: those keep every
  decoded dataset ≤ 32 MiB in a `OnceLock` on the dataset template, never
  evicted, outside every budget. Full reads are for 1-D coordinates at
  ingest only (slice reads too); a scalar `forecast_reference_time` is the
  one `read_variable_as_f64` (8 bytes).
- Never call netcdf-reader/hdf5-reader `*_parallel`/`*_in_pool` (rayon;
  Critical Rule 7). netcdf-reader enables hdf5-reader's rayon feature even
  with default features off; only those calls use it.
- **One engine-wide `ByteBoundedCache`** (`FIELDS`, `MC_NETCDF_FIELD_CACHE_MB`,
  default 256), process-global across collections, keyed by the file's
  ingest id (unique in the process: a replaced file never reads the old
  file's fields), variable, time index and level (the served pressure-level
  index, 0 for a single-level field).
  Values are `Arc<[f32]>`, north-up, NaN for nodata: a flipped (south-up)
  grid's rows are reversed once, while decoding.
- **Single-flight:** `get_or_insert_with_timeout` inside `io::blocking`
  (`block_in_place` on a Tokio worker, directly elsewhere), as engine-zarr's
  store does. Only the fill owner reserves the transient peak from the
  decode budget
  (`decode_budget::BUDGET`, `MC_NETCDF_READ_MEMORY_MB`, default 1024):
  3 × decoded + 3 × compressed, where hdf5-reader exposes no chunk's stored
  size before reading it, so the decoded chunk bounds the compressed one
  (~24 MB for a MEPS field). Waiters and hits reserve nothing. Exhaustion is
  `ResourceExhausted` (503). Waits observe the caller's deadline (30 s
  without one); a fill that completes after it stays cached. The budget is
  a `ds_executor::budget::ByteBudget` (`decode_budget.rs` keeps only the
  static), never a copy of its own.
- **Detached fills (remote files, the 3 s render deadline):** a remote
  cold field's fill is claimed with `get_value_or_guard_untracked(ZERO)`
  and the guard moved into a task on the I/O runtime's blocking pool
  (`Mode::Detached`: no deadline, `FetchBudget::Background`), which
  reserves the budget, decodes and inserts. The request waits for the value
  until its own deadline only. A fetch slower than the deadline fails that
  request (503) and still fills the cache. A failed detached fill drops
  the guard: a waiting reader then fills inline and gets the error itself.
  At most `MAX_DETACHED_FILLS` (16) run at once, over every collection;
  past that the fill runs inline under the deadline. Local files fill
  inline. EDR and GetFeatureInfo reads (`read_uncached`) detach from a pool
  of their own, `QUERY_READS` (`MAX_QUERY_DETACHED_READS`, 8), never the
  render fills': one cold series holds four reads for as long as a 1.5 MB
  fetch takes, and a few would otherwise push the renders' cold fills
  inline, where a fetch cut off by the 3 s deadline caches nothing and
  retries never progress. Both pools' fetches still share the host's
  request limit (`Mode::Detached`). A derived wind speed tile (`DerivedWind`) reads u, then v: on a
  cold remote pair the second fetch starts once the first has landed, so
  a slow link may take a retry or two, each making progress.
- **Prewarm** (`prewarm_variables`, remote only, off by default): after a
  poll publishes, the newest run's fields of those variables are read in
  `Mode::Background` on `BACKGROUND_SHARE` threads, until the next poll is
  due. `field::prewarm` decodes and drops: the compressed chunk lands in
  `CHUNKS`, the field cache keeps the frames being drawn. Each (ingest,
  variable, step) once (`PollState::prewarmed`); the first failure
  (budget, network) stops it until the next poll. ~100 MB per MEPS
  variable and run.
- **Metrics** (server `metrics_handler`, while a NetCDF collection is
  loaded): `netcdf_{field,block,chunk}_cache_{hits_total,misses_total,
  bytes,capacity_bytes}` (the chunk cache's include OPeNDAP field replies),
  `netcdf_read_{reserved_bytes,capacity_bytes,
  rejected_total}`, `netcdf_detached_fills_{pending,total,inline_total}`
  (render fills only: EDR reads count in neither these nor the field
  cache's hits and misses, which measure the renders; they show in the
  chunk cache and the per-host request gauges);
  and, for every host behind `build_http_store_limited`,
  `storage_host_{requests_in_flight,requests_queued,request_limit}{host}`.
  Grafana: the Caches row of Rendering & caches, the `netcdf_read` budget
  in Memory & admission ("Admission budgets — use", "Admission budgets",
  "Admission rejections"), "Remote requests per host" in Storage &
  sources and "NetCDF detached field fills" in Engines › Gridded sources.
- **A failed read is an `Err` (`Storage`)**, never a tile of nodata, and is
  not cached.
- `block_in_place` panics on a current-thread runtime: tests that render
  from async code use a multi-thread runtime.

## Rendering

`get_raster_tile` → `Catalog::select` → `field::read` →
`SourceGrid::output_map` → `OutputMap::sample_f32_with` (the variable's
display conversion, #1209) → `RasterValues::F32`. The
`SourceGrid` is built once per layout (its WGS84 envelope is
inverse-projected once; a curvilinear grid's index and extent are built
once, #1174). Code that needs the grid's size uses `SourceGrid::size()`,
never `transform()`, which a curvilinear grid has not. `get_raster_tiles` builds the output map once and
samples every band from it, all at one level. `z` selects the level through
`Layout::map_level` (ignored without a vertical axis, where the API layers
refuse it first).

`RasterInfo` is built once per poll (`raster_info_shared` is O(1), Critical
Rule 10): `native_crs` `CRS:84` only for latitude/longitude axes, else
`projected` (`geos` for geostationary, `curvilinear` for 2-D
latitude/longitude, #1174), so the Common `storageCrs` and
degree grid are omitted for MEPS; `spatial_extent` from
`CurvilinearGrid::extent` on a curvilinear grid, else from
`GeoTransform::bbox_with_poles`, so a polar grid's reaches the pole it
holds, one across the antimeridian is `west > east`, and an EDR area
there is not a 404 (#1167, #1194); `grid_size` `[nx, ny]`;
`reference_times`
ascending; `times` the union over runs; titles from `titles`, else
`long_name`; units the served ones (`Layout::unit`, below). Style defaults
and every other unit-keyed match read them through
`ds_core::units::canonical_unit`, so the layers need no
`[[wms.parameters]]` for the defaults ds-render's table gives MEPS names
(PR 6).

## Display units (#1209)

- **One rule per variable:** `Layout::display`, built once with the layout
  from each variable's CF `units` and `standard_name` through
  `ds_core::units::cf_display_conversion`: the rules engine-bufr uses and
  engine-grib's `default_display` twins (K → °C, Pa → hPa, kg m-2 → mm
  identity, kg m-2 s-1 → mm/h × 3600 #1213, in any spelling; geopotential's
  m2 s-2 is served as stored, #948), plus CF's `1` → % × 100 when the standard name is
  `relative_humidity` or ends in `_area_fraction` or `_area_fraction_in_atmosphere_layer` (MEPS relative humidity and cloud cover). A unit without a rule
  (m/s, `%`, `1` with any other standard name or none) is `None` and served
  as stored. Never a name table: the standard name is source metadata, the
  variable name decides nothing.
- **Sample, then convert, in `f64`:** maps through
  `OutputMap::sample_f32_with` (`lib.rs` `sample_display`; an identity rule
  takes plain `sample_f32`), EDR position/area/radius and GetFeatureInfo by
  `Layout::to_display` on `SourceGrid::value_at`'s result. A map pixel and
  an EDR value at the same point are the same number before the pixel is
  narrowed to `f32`. The field cache holds the stored values; nothing is
  converted before sampling.
- **Units reported converted everywhere:** `Layout::unit` feeds
  `RasterInfo.parameters` (styles, legends, `parameter_names`), EDR
  `ParameterDescription.unit` (CoverageJSON, GetFeatureInfo's `unit`), and
  `WindFacts` (the derived speed takes the components' served unit).
  `Variable::units` stays the file's spelling: `fits_layout` and
  `same_quantity` compare stored units.
- **Not converted:** the pressure axis, already hPa from the coordinate
  (`PressureUnit::to_hpa`): a variable's unit and a level's unit are
  separate.
- **Styles:** MEPS takes the defaults by served unit: temperature
  `temperature` −90…50 °C, MSLP `pressure` 950–1050 hPa, the run-start
  accumulation `precipitation_accumulation` 0–200 mm, geopotential
  `viridis` 0–21 000 gpm × g₀ in m^2/s^2 and its derived
  `geopotential_height_pl` 0–21 000 gpm (#948), relative humidity and cloud cover `humidity` and
  `cloud_cover` 0–100 (served in %). The shipped examples keep only the ±20 m/s
  surface wind components and a collection-level `temperature` palette with
  no range.
- Pinned by `tests/integration.rs` `values_are_served_in_display_units`
  (relative humidity and cloud cover in % included), `catalog/tests.rs`
  `unit_1_is_percent_only_for_a_fraction_standard_name`,
  ds-core's `a_converting_render_converts_each_sample_before_narrowing`,
  the server's `wms_get_feature_info` (values and `unit` per variable),
  `netcdf_edr` (QUDT `DEG_C`/`HectoPA`) and the admin example-style tests.

## EDR (`edr.rs`) — #1067 PR 7

`EdrEngine`: position (one point, or `query_positions`' several), area and
radius (the trait default: a 64-vertex polygon through area); trajectory
and cube (#1169, `edr/trajectory.rs`, `edr/cube.rs`; see their module
docs: an M path reads `Catalog::covering_run` of its earliest time; a
cube is an area's steps and cells, never coarsened, sampled bilinearly,
`resolution-z` taking the nearest level); no locations.

- **Run and steps** (`Catalog::query_run`, engine-grib's `resolve_run`
  rule): a pinned run is exactly that run (absent ⇒ 404
  `ReferenceTimeNotFound`); unpinned with a `datetime` window, the newest
  run with a step inside it that also spans the window's start, else the
  newest with any step inside; none ⇒ `LocationNotFound` (404), so a
  `datetime` list skips the instant (#968). An instant needs a step at
  exactly it. Without a window: a position series is every step of the
  newest run, an area the newest run's last step (the map's default). An
  interval puts an area's steps on a `t` axis, even one (grib's rule).
  GetFeatureInfo passes the run and step its map resolved
  (`resolve_reference_time`, `resolve_time`), so it reads that field.
- **Sampling:** `ds_core::resample::SourceGrid::position` projects a point,
  or an area cell centre, once (Critical Rule 5); `value_at` samples it as
  the map does (bilinear in x/y, nearest at a nodata neighbour, the half
  cell past the edges), and `Layout::to_display` converts it as the map
  converts a pixel (#1209). A point off the grid is null, not an error. An
  area is a CRS84 grid over the polygon's bbox at the extent's mean degree
  size per cell (`crs84_bbox_spans` ÷ cells), ≤ `MAX_AREA_DIM`, masked by
  `cell_mask`, as engine-querydata's; a polygon missing the extent is 404.
  A point or cell off the grid (`SourceGrid::covers`, the rule `value_at`
  applies) is dropped before sampling, and a query with none on the grid
  reads no field: `POINT(-100 10)` on remote MEPS costs no fetch.
- **Field jobs:** `NetcdfEngine::plan_reads` plans every read before any
  (`edr::Plan`): each (variable, step, level) field `sample_fields` needs,
  read once however many parameters and `Sampling`s need it (a
  per-step amount adds its accumulation at the run's step before each
  `Sampling.valid`, #1165), sampled once per distinct positions list
  (position, area and cube share one position set, step-major `[t][z]`, a
  trajectory gives each field its samples; a field with no position on the
  grid is not read), through
  `ds_executor::field_jobs::run_field_jobs` (engine-grib's scheduler, moved
  there): four at once as tasks on the query runtime (fallback
  `ds_executor::query_runtime()` outside any runtime), each re-entering
  the query's deadline. One `edr::Job` per read: a field of one level, or
  through OPeNDAP one request of the windows of a step's variables, its
  `edr::Member`s, each over its levels (see "OPeNDAP", Variables in one
  request). Each
  job returns only the samples at every point: one read per field for all
  points.
- **Reads:** `field::read_uncached` (a job's members, one `QueryField`
  per member and level): a `FIELDS` hit (a frame a render just
  drew) is used; a miss is decoded for the query and **not inserted**, so
  a series over a run never evicts the frames being drawn; the lookup is
  not counted in `netcdf_field_cache_*`. A remote miss runs detached on
  the I/O runtime (`Mode::Detached`, a `QUERY_READS` slot, inline past
  them) and the job waits until its deadline: a query that misses the
  30 s `QUERY_TIMEOUT` is a `DeadlineExceeded` (EDR 504, GetFeatureInfo
  503) while its fetches go on into `CHUNKS`, so a retry makes progress
  (`remote::tests::a_query_past_its_deadline_leaves_its_chunks_cached`).
  A query read of a field another query's detached read is still reading
  joins it (`field::QUERY_IN_FLIGHT`): no slot, no decode, no GET; a failed
  read leaves the joiner to read for its own error. Retries used to fill
  the 8-slot pool and push cold reads inline, where the deadline cut their
  GETs off (`retries_join_the_reads_a_late_query_left_running`). Tests
  that pin GETs under a deadline take `remote::tests::query_pool()` (the
  pool alone, a `RwLock` write); the other OPeNDAP query tests take
  `query_pool_shared()`.
  Every read reserves the decode budget; a failed read fails the query,
  never a null: a replaced file or the budget is `ResourceExhausted` (503),
  the deadline `DeadlineExceeded` (504), any other remote failure
  `Storage` (an opaque 500 in EDR). Through OPeNDAP the read is the
  window of the field the query samples (see "OPeNDAP", EDR windows).
- **Query caps** (from the plan's `edr::Tally`, before any read, bytes
  checked first; over either a 400 `QueryTooLarge`): `QUERY_BYTES`
  (`MC_NETCDF_EDR_QUERY_MB`, 544 MiB) on what the planned reads decode,
  `edr::Member::bytes` (a whole field `FieldRead::decoded_bytes`, the `f32`
  field or its chunk when larger; a window its cells × span levels × 4 B, a
  request the sum of its members'); `QUERY_FIELD_READS`
  (`MC_NETCDF_EDR_QUERY_READS`, 200; not `field::QUERY_READS`, the pool) on
  the jobs, a request of several variables' windows of several levels being
  one. Only fields read count (a trajectory's planned pairs, a cube's steps
  × the levels its z positions take, a per-step amount's steps before,
  nothing off the grid). The byte message names the fields, `edr::shape`
  (timesteps × levels × variables, pairs for a trajectory, "of" when the
  reads are not their product: a per-step amount's steps before, or
  variables sharing requests) and the sizes; the read message asks for fewer
  parameters only when a step makes more than one read, then for fewer
  levels too on several levels (read one by one each is a read, read
  together each a plane), else only for fewer steps. Read whole, the byte
  default admits one parameter over one MEPS run as the client sees it,
  including a derived wind speed (decided on #1067: `DerivedWind` reads both
  components, 134 × 4.06 MB, 519 MiB); three variables over a run (201
  fields) are a 400, as is one variable at all 13 levels read whole (871
  fields, `edr::tests::the_cap_counts_pressure_levels`); 140 MEPS fields at
  most. Through OPeNDAP windows hardly count and the read cap binds: a
  six-variable meteogram over a run is 67 reads, 13 levels × a run 67, a
  derived wind speed at every level 134, three variables at every level 201
  (a 400); ≤ 0.55 s per request of ≤ 16 planes, four at once, 200 reads ~27
  s. Every doc of the caps says so (root and engine READMEs, api-edr's
  matrix, the pixel-budget rows, the thredds and OPeNDAP examples).
  `check_area_budget` and, for areas, `check_mask_budget` come first: the
  plan needs the positions. Measured live 2026-10-17 (laptop link, debug
  build, `tests/live_thredds.rs`): a 3-lead series whose first field a map
  had drawn, 2 GETs, 3.1 MB, 3.6 s.
- **Metadata:** parameters from the layout and the catalog's `RasterInfo`
  (titles, served units: `Layout::unit`), CF `standard_name` →
  `observedProperty.id`;
  temporal extent and available times are the union over runs; instances
  from the run map (`get_instances`, O(1) `has_instances`,
  `instance_reference_times` from its keys, `find_instance` one run); a
  run-less timeline has none.

## Derived wind

`WindSource::wind_facts` is the layout's `Arc<WindFacts>`, built once with
the layout (`catalog::wind_facts`): per variable its CF `standard_name`,
the frame `VectorFrame::from_standard_name` gives (`x_wind`/`y_wind` ⇒
`Grid`), the served unit (`Layout::unit`: `DerivedWind` combines served
values), and the level (`"10 m"`: the single level's value and
units) as the pairing level; the grid's `GridAxes::of(crs)`. MEPS's Lambert
grid is `Rotated`, so `x_wind_10m`/`y_wind_10m` give `wind_speed_10m` and no
direction (decision 6; rotation by the convergence angle is a follow-up on
#897). The server wraps the engine in `ds_core::derive::Derived`
(`derived_wrapped`), which derives on every API from the components' own
reads; a `geopotential` variable gives its height the same way
(`geopotential_pl` → `geopotential_height_pl`, #948). Components on the
pressure axis (`x_wind_pl`/`y_wind_pl`) have no pairing level, as
`ParameterFacts::level` asks of a parameter spanning the vertical axis:
they pair into `wind_speed_pl`, and `DerivedWind` reads both at the one `z`
of each request, so the speed is per level
(`tests/integration.rs` `derived_wind_pairs_the_components_level_by_level`;
a Pa file with earth-relative `u`/`v` on lat/lon gives `ws` and `wdir` per
level, `pressure_in_pa_is_served_in_hpa_with_wind_direction_per_level`).
The same facts carry `vertical: Some(Pressure)` for a variable on the
pressure axis, so `ds_core::moisture` (#898) pairs `air_temperature_*`
with `relative_humidity_*`, `dew_point_temperature_*` or, on that axis,
`specific_humidity_*` by swapping the token: MEPS gains
`dew_point_temperature_2m` and `dew_point_temperature_pl`
(`derived_dew_point_from_temperature_and_relative_humidity`).

## Per-step amounts (#1165)

`<name>_acc` served in mm (`Layout::per_step`, `per_step_amounts`) also
gives `<name>`: MEPS `precipitation_amount_acc` → `precipitation_amount`,
titled with the step, "(1 h accumulation)". It is not derived for another
unit (the clamp would corrupt signed accumulations), when a variable
already has `<name>`, or with `[netcdf] derive_per_step = false`.
Parameters are `catalog::Param` (`Stored`/`PerStep`), served after the
variables (`Layout::info_index`). Value = `step_amount(now, before)`: both
fields sampled and converted in f64, differenced, clamped ≥ 0, never a
difference interpolated; the run's first step (`Selected::previous` /
`Run::previous` = None) is its own accumulation. Map: `sample_step_amount`
(`OutputMap::position` + `SourceGrid::value_at`). EDR position, area,
trajectory and cube: `sample_fields` reads each (variable, step) once,
including the run's step before each sample's own step, and the cap counts
it. Title from `step_lengths`: one length `(1 h accumulation)`, else
`(min to max accumulation per forecast step)`. Cache keys don't name the
step before, so `build_catalog` bumps `content_version` when
`steps_before_changed`. Pinned by `tests/integration.rs` (`per_step_*`,
`a_decreasing_accumulation_is_no_amount`,
`the_first_lead_present_is_its_own_accumulation`,
`a_lead_filled_in_moves_the_content_version`,
`derive_per_step_false_serves_the_variables_only`), `tests/trajectory_cube.rs`
(`a_per_step_amount_*`), `catalog::tests`,
`edr::tests::the_cap_counts_the_step_before_a_per_step_amount`, and the
server's `netcdf_edr` and `wms_get_feature_info`.

## Runs (`catalog.rs`)

`BTreeMap<Option<DateTime<Utc>>, Run>`, `None` the run-less timeline.
`Catalog::select` is the ONE selection behind `get_raster_tile(s)`,
`resolve_time` and `resolve_reference_time` (#507, #521), engine-grib's
default-layer rule: a pinned run is exactly that run (absent ⇒
`ReferenceTimeNotFound`, 404) and never falls back; unpinned, the newest run
whose steps span the time, so an incomplete newer run does not hide an
older one; without a time, the newest run's last step. Within the run, the
nearest step, the earlier on a tie. Every variable, and every per-step
amount, shares the run's time axis (decision 3), so the per-parameter
resolvers keep their defaults; `Selected::previous` is the run's step
before, never another run's.

`content_version` starts at the process start's Unix seconds (nonzero, new
per restart) and moves when a catalogued file is replaced in place: found
by a poll (local identity, or a remote file the prober lists at another
version), or by a remote read at once (`Changes::bumps`). It also moves
when a per-step layout's step gets another step before it (a lead filled
in or removed inside a run, `catalog::steps_before_changed`, #1165);
appended leads and new runs move nothing.

## Server wiring

`engine_type = "netcdf"`, allowlist `["edr", "wms", "maps", "tiles"]` (the
default `apis` works). Registered through `derived_wrapped` (in
`DERIVE_WIND_ENGINES`, `DERIVE_GEOPOTENTIAL_HEIGHT_ENGINES` and
`DERIVE_HUMIDITY_ENGINES`; `derive_wind = false`,
`derive_geopotential_height = false`, `derive_humidity = false` opt out), its EDR side
through `register_point_engine` for GetFeatureInfo whether or not it lists
`edr`. `netcdf_parameters` keeps the engine's own variables, which the
recovery loop compares, while the styles are resolved for the wrapper's
(with the derived speed). Poll loop in all
three places (boot spawn, `rotate_poll_loops!`, shutdown); a remote
source's first tick is immediate. Health: live
status from `live_health`; data age = the newest run's reference time, or
the newest valid time without runs. Layer styles resolve at load from
`raster_info()`, which the initial scan has filled. When it found no files
(an empty or unreadable directory), the server's recovery loop
(`netcdf_registration_changed`, ≤ 30 s, mirroring
`grib_registration_changed`) registers the collection again once a poll
has read them, reusing the engine and evicting any tiles drawn meanwhile.

## Tests and fixtures

- `testdata/meps-netcdf/`: seven real MEPS sfc files cropped to every 100th
  point (11 × 10, south-up), two runs (06Z leads 0–3, 09Z leads 0–2: 09Z is
  in both), with MET Norway's 2-D lat/lon; built by `make_cdl.py` from
  OPeNDAP ASCII subsets, provenance in its README.
- `testdata/meps-netcdf-pl/`: five real MEPS pl files cropped the same way
  (11 × 10), two runs (06Z leads 0–2, 09Z leads 0–1), five variables at
  1000/850/500/300 hPa (stored top first, as the source), one level per
  chunk; built by its `make_cdl.py` from OPeNDAP ASCII subsets, provenance
  in its README (fetched 2026-10-18, ~265 KB).
- `testdata/netcdf-pressure-pa/`: synthetic, a Pa pressure axis (50000,
  85000, 100000) on a 1° lat/lon grid, two steps, no run, earth-relative
  `u`/`v` and `t`, each value a formula of its indices (`make_cdl.py`).
- `testdata/netcdf-pressure-13/`: synthetic, MEPS's 13 pressure levels in
  hPa stored top first, an 8 × 10 0.5° lat/lon grid stored south-up, one run
  of six hourly steps in one file, `air_temperature_pl` (one fill cell) and
  `relative_humidity_pl` as formulas of their indices (`make_cdl.py`);
  `opendap::tests::windows` reads its levels' windows through the mock DAP2
  server.
- `testdata/netcdf-global/`: synthetic 5° global grid, north-up, three
  steps, no run; a packed `short` variable with a fill box.
- `testdata/netcdf-remote/`: two synthetic MEPS-like per-lead files (one
  run, leads 00 and 01, LCC, south-up, 64 × 96, two variables chunked one
  field per chunk with shuffle + deflate 3 and noisy values, so a chunk is
  ~15 KB), written by `make_fixture.py` (needs `ncgen`).
- `testdata/netcdf-thredds-catalog/`: MET Norway's `catalog.xml` of one MEPS
  day directory (2026-10-19), as served (21 KB). ds-storage
  `discovery::thredds::tests` pin the catalog route against the template
  route on its `meps_det_2_5km` files; `tests/integration.rs`
  `a_thredds_catalog_finds_what_probing_finds` serves it, chunked, as the
  run's day from `src/test_server.rs` (its `thredds_catalog` writes the
  other catalogs with `quick_xml::Writer`);
  `remote::tests::a_file_replaced_under_a_catalog_is_headed_again`.
- `testdata/netcdf-opendap/`: what thredds.met.no's OPeNDAP served on
  2026-10-23 for `meps_pl_00` (DDS and DAS whole) and `meps_sfc_00`
  (trimmed to the example's variables) of the 12Z run, each file's
  coordinates reply, and a 4 × 4 hyperslab of 2 m temperature (48 KB in
  all, provenance in its README; `opendap::tests::windows` answers EDR
  windows of the sfc file with it, #1171). `src/test_server.rs` also serves any
  NetCDF fixture as a DAP2 dataset (`TestServer::put_opendap`: `.dds`,
  `.das`, `.dods` with constraints, chunked, Grids or plain arrays,
  `Last-Modified` optional).
- `testdata/netcdf-projections/`: one synthetic file per CF mapping of
  #1167, each its own collection directory (`polar-stereographic/` with
  EPSG:3413's parameters around the North Pole, `laea/` with EPSG:3035's
  on GRS80 and rows south-up, `transverse-mercator/` UTM 33N,
  `stereographic-km/` the oblique CF `stereographic` on a 6371 km sphere
  with x/y and its 600 km, −200 km false origin in km, #1194), one step,
  no run; the fields `easting`/`northing` are each cell centre's own
  projected x/y, so a read returns where the server puts a point
  (`make_cdl.py`). `tests/grid_mappings.rs` reads them through EDR
  position and a one-pixel map at points pinned to PROJ `cs2cs` (forward,
  and cell centres through `-I`), within the 1 m their `f32` rounding
  allows, and the polar grid's extent and an EDR area at the pole.
- `testdata/netcdf-curvilinear/`: synthetic, 2-D `lat`/`lon` only (no 1-D
  coordinates on `y`/`x`, no grid mapping), 19 × 28 nodes of a spherical
  polar stereographic plane 100 km apart around the North Pole, over
  Svalbard and across the antimeridian, one step, no run; its fields
  `column` and `row` are each node's own indices (`make_cdl.py`, cs2cs to
  the ninth decimal). `tests/curvilinear.rs` pins EDR positions and map
  pixels (CRS84 across the antimeridian, EPSG:3857) to
  `tests/curvilinear_reference.py`, a stdlib brute force over the CDL's
  arrays (closed-form cell inversion, nearest node past the edge), plus a
  one-pixel map equal to EDR, a meta-tile against a direct render, a
  world map drawing only the cap, and area/radius/cube/trajectory reading
  what position reads. ds-core `resample::curvilinear::tests` pins the
  lookup against the exact projection, the footprint, the extent and
  `same`; `ingest::tests` the recognition rules.
- `remote::tests` (in the lib test binary): a local HTTP/1.1 server
  (`test_server.rs`: HEAD without ETag, 206 with `Content-Range`, 412 on
  `If-Unmodified-Since`, a request log, slow chunk GETs on demand) and the
  scaled `TUNING`. Pins the request counts above, ranged reads drawing
  what local reads draw (every GET a 206), one GET for 8 concurrent cold
  renders, a detached fill outliving its request's deadline and leaving
  the cache warm, a replaced file (412 → 503, content version, re-probe,
  new file), prewarm, the bounded first scan opening only the newest run,
  the runtime contexts, EDR and GetFeatureInfo queries on the query
  runtime, a query past its deadline leaving its chunks cached, two
  pressure levels' detached fills each its own chunk GET and decode
  (`each_pressure_level_is_a_detached_fill_and_a_decode_of_its_own`), and
  the shipped thredds example (offline).
- `tests/live_thredds.rs`: `#[ignore]`, against thredds.met.no: leads
  00-02 of the newest run, `air_temperature_2m`, one field and an EDR
  position series over the three leads, ~9 MB; leads 00-01 of the newest
  pl run and one position query at 850 and 500 hPa (measured 2026-10-18,
  laptop link, debug build: first scan 4.2 s, 4 HEADs, 8 GETs, 2.46 MB,
  1.23 MB per pl file serving one variable; the query 4 GETs, 5.68 MB,
  1.1 s; the levels 1000…50 hPa bottom first); and a 1 KiB ranged GET of
  an archived file. Run by hand only (`cargo test -p
  engine-netcdf --test live_thredds -- --ignored --nocapture`); it prints
  requests, bytes, timings and the heap per kept handle
  (`NetcdfEngine::remote_traffic`).
- `scan_tests`: a file replaced in place under a cap of one open per poll.
- `tests/integration.rs`: MET Norway's lat/lon against the grid (measured
  within 6 cm), north-up drawing of south-up rows, run selection and
  fallback with pixel values, `max_runs`/`max_files`, the seam box
  170,10,-170,20, packing and fill, a failed read, a replaced file, the
  initial scan and a poll picking up new files, an unusable
  `filename_template`, and renders on `RenderJob` blocking workers and an
  async worker. Its EDR tests (`edr_*`, `derived_wind_*`): position values
  pinned to the stored cells in the run EDR selects, the 404s a `datetime`
  list skips, position and area sampling what the map draws, several
  points sharing each field read and nothing cached, area and radius
  masking and `t` axes, instances, the derived speed against the stored
  components, and queries on the query runtime and a GetFeatureInfo job.
  `edr::tests` pin both query caps; the server's
  `wms_get_feature_info` test reads a pixel at a fixture cell. Pressure
  levels: `ingest::tests` (the axis by units, hPa bottom first, Pa
  converted, one vertical axis per collection), `tests/integration.rs`
  (`pressure_levels_are_the_vertical_axis`, each level's stored field and
  one decode per level, the default-level parity, EDR `z` shapes and
  values, derived wind per level, the Pa fixture), `edr::tests` (the cap
  counting levels, `z` selection), the server's `netcdf_edr`
  `netcdf_pressure_level_responses_validate` (every `z` form, CoverageJSON
  and EDR schemas) and `admin::tests`
  `meps_netcdf_pressure_level_example_serves_its_levels_and_styles`.
  `field::tests` pins single-flight with a slow storage (8 readers, one
  decode). `ingest::tests`, `catalog::tests`, `discover::tests` are unit
  tests; `server` has
  `netcdf_serves_edr_and_derived_wind_and_is_reused_across_reloads` and
  `netcdf_recovery_registers_layer_styles_once_variables_are_found`.
- `crates/server/tests/netcdf_edr.rs`: every query shape on the committed
  MEPS fixture through the EDR router (position, an instant, MULTIPOINT, a
  `datetime` list, area, radius, instance queries, the derived wind
  speed, the 404s), each CoverageJSON body validated against
  `schemas/coveragejson.json`, and the collection and instances documents
  against the EDR 1.1 and 1.2 schemas.
- Regenerate a fixture's `.nc` with `ncgen -k nc4 -o <name>.nc <name>.cdl`.
