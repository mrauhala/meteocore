# engine-netcdf crate — Claude Instructions

CF-NetCDF gridded forecasts and analyses (#1067), first for MET Norway's
MEPS. Read the root `CLAUDE.md` first — Critical Rules 5, 6, 7, 9 and 10
shape this crate. Today: local directories and remote files read in place
by HTTP range (thredds.met.no), WMS/Maps/Tiles. EDR and derived wind
(PR 7) and pressure levels (PR 8) come later; keep the seams below where
they are.

## Model

- **One collection is one file family** (MEPS `sfc` per-lead files): every
  file carries the collection's variables on one grid. The first file a
  poll ingests fixes the `Layout` (variables in allowlist or file order,
  grid); a later file missing a variable, on another grid, or with another
  level or units is skipped with a WARN. The layout lives as long as the
  engine.
- **Time from the file, never its name:** valid times from the fields'
  `time` axis (`ds_core::cf::decode_times`, which reads MEPS's
  `… 00:00:00 +00:00`), the run from `forecast_reference_time` (scalar, or
  1-D holding one value). Files without one form one timeline without runs;
  a directory mixing both serves the runs (WARN once). The global
  `min_time`/`max_time` attributes are wrong in MEPS and never read.
  Non-standard calendars are refused, not approximated.
- **Variables:** `(y, x)` last, on 1-D latitude/longitude or 1-D projected
  x/y with a `grid_mapping` (`ds_core::cf::{classify_axis,
  crs_from_grid_mapping, RegularAxis, grid}`); one time axis; an ensemble
  axis read at `member`; every other axis of size 1, whose coordinate value
  and units are the level identity (MEPS puts 10 m wind on `height6`,
  `height7` or `height2` by family: never compare dimension names). Another
  axis longer than 1 skips the variable with a WARN (PR 8 adds pressure).
  Numeric types are read through the matching typed slice; CF
  `scale_factor`/`add_offset`, `_FillValue`, `missing_value` (compared in
  the stored type), netCDF's default float fill without a `_FillValue`,
  and NaN apply. `valid_range` is not applied.
- **2-D latitude/longitude auxiliaries are never read** (MEPS carries them as
  Float64 next to every field): the grid is the projected axes.

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
them: ~80 MB a MEPS run); a key is the `Known` map's path. A run is
published whole (or after its grace), so the engine never sees a run
half-uploaded. Every HEAD of a probe failing is an `Err` (the catalog stays,
`/health` degrades after five polls).

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
- the newest `max_runs` runs are kept, as many as `max_files` holds
  (without runs the `max_files` newest files); files outside are closed
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
`Skipped`. A remote poll may open `max(MAX_OPENS_PER_POLL, leads)` files,
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

Each catalogued file keeps its `NcFile` (`catalog::OpenFile`) for as long as
it is catalogued; request paths never open a file. Local files are opened
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
  `remote::tests::reads_run_on_the_poll_runtime_render_jobs_and_async_workers`.
  EDR and GetFeatureInfo query workers: PR 7 adds their tests.
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
  A permit wait counts against the deadline. `ds_storage::host_requests`
  reports in flight and queued per host.
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
  down to the storage callback; PR 7's EDR series inserts here.
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
  every pixel drawn) and map tiles of the same field in 25-30 ms. Not yet measured from a
  server's network: a follow-up after deploy, which sets the cache and
  budget defaults.

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
  file's fields), variable, time index and level index (0 until PR 8).
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
  inline. Note for PR 7: derived wind reads u and v one after the other.
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
  bytes,capacity_bytes}`, `netcdf_read_{reserved_bytes,capacity_bytes,
  rejected_total}`, `netcdf_detached_fills_{pending,total,inline_total}`;
  and, for every host behind `build_http_store_limited`,
  `storage_host_{requests_in_flight,requests_queued,request_limit}{host}`.
  Grafana: the cache-family panels, the two "NetCDF field-read" panels,
  "Remote requests per host" and "NetCDF detached field fills".
- **A failed read is an `Err` (`Storage`)**, never a tile of nodata, and is
  not cached.
- `block_in_place` panics on a current-thread runtime: tests that render
  from async code use a multi-thread runtime.

## Rendering

`get_raster_tile` → `Catalog::select` → `field::read` →
`ds_core::resample::SourceGrid::render_f32` → `RasterValues::F32`. The
`SourceGrid` is built once per layout (its WGS84 envelope is
inverse-projected once). `get_raster_tiles` builds the output map once and
samples every band from it. `z` is ignored (no vertical axis yet).

`RasterInfo` is built once per poll (`raster_info_shared` is O(1), Critical
Rule 10): `native_crs` `CRS:84` only for latitude/longitude axes, else
`projected` (`geos` for geostationary), so the Common `storageCrs` and
degree grid are omitted for MEPS; `grid_size` `[nx, ny]`; `reference_times`
ascending; `times` the union over runs; titles from `titles`, else
`long_name`; units as the file spells them (`kg/m^2`: PR 6 normalises).

## Runs (`catalog.rs`)

`BTreeMap<Option<DateTime<Utc>>, Run>`, `None` the run-less timeline.
`Catalog::select` is the ONE selection behind `get_raster_tile(s)`,
`resolve_time` and `resolve_reference_time` (#507, #521), engine-grib's
default-layer rule: a pinned run is exactly that run (absent ⇒
`ReferenceTimeNotFound`, 404) and never falls back; unpinned, the newest run
whose steps span the time, so an incomplete newer run does not hide an
older one; without a time, the newest run's last step. Within the run, the
nearest step, the earlier on a tie. Every variable shares the run's time
axis (decision 3), so the per-parameter resolvers keep their defaults.

`content_version` starts at the process start's Unix seconds (nonzero, new
per restart) and moves when a catalogued file is replaced in place: found
by a poll (local identity, or a remote file the prober lists at another
version), or by a remote read at once (`Changes::bumps`).

## Server wiring

`engine_type = "netcdf"`, allowlist `["wms", "maps", "tiles"]` (an `edr`
collection is rejected until PR 7; examples set `apis`). Poll loop in all
three places (boot spawn, `rotate_poll_loops!`, shutdown); a remote
source's first tick is immediate. Health: live
status from `live_health`; data age = the newest run's reference time, or
the newest valid time without runs. Layer styles resolve at load from
`raster_info()`, which the initial scan has filled. When it found no files
(an empty or unreadable directory), the server's recovery loop
(`netcdf_registration_changed`, ≤ 30 s, mirroring
`grib_registration_changed`) registers the collection again once a poll
has read them, reusing the engine and evicting any tiles drawn meanwhile.
Not in `DERIVE_WIND_ENGINES` (PR 7).

## Tests and fixtures

- `testdata/meps-netcdf/`: seven real MEPS sfc files cropped to every 100th
  point (11 × 10, south-up), two runs (06Z leads 0–3, 09Z leads 0–2: 09Z is
  in both), with MET Norway's 2-D lat/lon; built by `make_cdl.py` from
  OPeNDAP ASCII subsets, provenance in its README.
- `testdata/netcdf-global/`: synthetic 5° global grid, north-up, three
  steps, no run; a packed `short` variable with a fill box.
- `testdata/netcdf-remote/`: two synthetic MEPS-like per-lead files (one
  run, leads 00 and 01, LCC, south-up, 64 × 96, two variables chunked one
  field per chunk with shuffle + deflate 3 and noisy values, so a chunk is
  ~15 KB), written by `make_fixture.py` (needs `ncgen`).
- `remote::tests` (in the lib test binary): a local HTTP/1.1 server
  (`test_server.rs`: HEAD without ETag, 206 with `Content-Range`, 412 on
  `If-Unmodified-Since`, a request log, slow chunk GETs on demand) and the
  scaled `TUNING`. Pins the request counts above, ranged reads drawing
  what local reads draw (every GET a 206), one GET for 8 concurrent cold
  renders, a detached fill outliving its request's deadline and leaving
  the cache warm, a replaced file (412 → 503, content version, re-probe,
  new file), prewarm, the bounded first scan opening only the newest run,
  the runtime contexts, and the shipped thredds example (offline).
- `tests/live_thredds.rs`: `#[ignore]`, against thredds.met.no: leads
  00-02 of the newest run, `air_temperature_2m`, one field, ~5 MB, and a
  1 KiB ranged GET of an archived file. Run by hand only (`cargo test -p
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
  async worker.
  `field::tests` pins single-flight with a slow storage (8 readers, one
  decode). `ingest::tests`, `catalog::tests`, `discover::tests` are unit
  tests; `server` has `netcdf_is_a_map_engine_reused_across_reloads` and
  `netcdf_recovery_registers_layer_styles_once_variables_are_found`.
- Regenerate a fixture's `.nc` with `ncgen -k nc4 -o <name>.nc <name>.cdl`.
