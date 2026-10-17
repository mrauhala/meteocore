# engine-netcdf crate — Claude Instructions

CF-NetCDF gridded forecasts and analyses (#1067), first for MET Norway's
MEPS. Read the root `CLAUDE.md` first — Critical Rules 5, 6, 7 and 10 shape
this crate. Today: local directories, WMS/Maps/Tiles. Remote files by HTTP
range (#1067 PR 5), EDR and derived wind (PR 7) and pressure levels (PR 8)
come later; keep the seams below where they are.

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
run always come from the file. PR 5 adds remote discovery with the same
`TemplateSpec` vocabulary (`prefix_pattern`, `run_hours`, `leads`,
`time_window`) next to these fields, without renaming them.

The poll (`poll_once`, on `poll_runtime()`, Critical Rule 6) remembers every
listed file by path and identity (size + modification time):

- new or changed files are opened, newest first, at most
  `MAX_OPENS_PER_POLL` (128) per poll (Critical Rule 9);
- a catalogued file whose identity changes is read again and bumps
  `content_version` (#1067 decision 7); a new file does not;
- the newest `max_runs` runs are kept, as many as `max_files` holds
  (without runs the `max_files` newest files); files outside are closed
  (`Known::Closed`, run and newest time remembered, never reopened unless
  they come back into the kept set); unservable files are remembered as
  `Skipped` and not opened again until they change.

`NetcdfEngine::new` scans once, synchronously, as engine-grib does, so the
layers and their styles exist when the server registers the collection; a
failed scan is a WARN ("initial NetCDF scan failed (will retry on poll)")
and the poll loop, whose first tick is skipped, retries. Every catalog is
built by that scan and swapped through `ArcSwap`. PR 5 bounds the remote
first scan.

## Open files and metadata — measured

Each catalogued file keeps its `NcFile` (`catalog::OpenFile`) for as long as
it is catalogued; request paths never open a file. Files are opened over
`FileStorage` (positional reads) for NetCDF-4 and classic alike, never
`NcFile::open`'s mmap, which a file truncated in place would turn into
SIGBUS. One descriptor per catalogued file: `max_files` bounds them.
The reader's chunk cache is off (`chunk_cache_bytes = 0`).

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
  sfc file (221 variables, 190 MB) holds far more. **Not measured here:**
  a real file was deliberately not downloaded; PR 5 measures one per-lead
  sfc handle × 67 leads × `max_runs` and records it here.

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
- **Single-flight:** `get_or_insert_with_timeout` inside `block_in_place` on
  a Tokio worker, directly elsewhere, as engine-zarr's store does. Only the
  fill owner reserves the transient peak from the decode budget
  (`decode_budget::BUDGET`, `MC_NETCDF_READ_MEMORY_MB`, default 1024):
  3 × decoded + 3 × compressed, where hdf5-reader exposes no chunk's stored
  size before reading it, so the decoded chunk bounds the compressed one
  (~24 MB for a MEPS field). Waiters and hits reserve nothing. Exhaustion is
  `ResourceExhausted` (503). Waits observe the caller's deadline (30 s
  without one); a fill that completes after it stays cached. The budget is
  a `ds_executor::budget::ByteBudget` (`decode_budget.rs` keeps only the
  static), never a copy of its own.
- **Metrics** (server `metrics_handler`, while a NetCDF collection is
  loaded): `netcdf_field_cache_{hits_total,misses_total,bytes,
  capacity_bytes}` and `netcdf_read_{reserved_bytes,capacity_bytes,
  rejected_total}`, with Grafana panels (the cache-family panels, and the
  two "NetCDF field-read" panels).
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
per restart) and moves when a catalogued file is replaced in place.

## Server wiring

`engine_type = "netcdf"`, allowlist `["wms", "maps", "tiles"]` (an `edr`
collection is rejected until PR 7; examples set `apis`). Poll loop in all
three places (boot spawn, `rotate_poll_loops!`, shutdown). Health: live
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
