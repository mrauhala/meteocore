# engine-odim crate — Claude Instructions

ODIM_H5 weather-radar engine: COMP (Cartesian composites) and PVOL (polar
volumes). Read the root `CLAUDE.md` first — the poll loop runs on the
background runtime and the HDF5 scan may use `spawn_blocking`, but never
around `ds-storage` calls (Critical Rules 6–7).

## PVOL per-site collection model (#281)

- **One source → N collections.** A single `engine_type = "odim-volume"`
  config scans a directory / S3 prefix of `.h5` polar volumes spanning a
  radar *network*, then expands into one OGC collection per radar site
  (ODIM `nod`), id `{base_id}-{nod}` (e.g. `radar-fi-volume-local-h5-fivih`).
  There is no network-level aggregate raster collection.
- **The engine owns the source; views serve the per-site APIs.**
  `PolarVolumeEngine` (`src/volume_engine.rs`) does the scan, parse cache,
  and poll loop. Each site is served by a cheap `PolarVolumeSiteView` over
  the engine's shared `Arc<ArcSwap<Catalog>>`, so all views see poll
  refreshes for free. The engine implements only `FeatureEngine` (the
  inventory below); per-site `EdrEngine`/`MapEngine`/`VolumeEngine` live on
  the views.
- **Base id = site inventory (Features).** When the source's `apis` includes
  `"features"`, the owning engine is registered as a `FeatureEngine` under
  the base id — one Point Feature per radar site (id = NOD, geometry =
  antenna, properties = `name`/`wmo`/`quantities`/`elevation_angles`/
  `coverage_radius_m`/`latest_volume_time`/`volume_count`/`collection`).
  `quantities`/`elevation_angles` use `PropertyValue::List`.
  `GET /features/collections/{base}/items[/{nod}]` supports
  bbox/limit/offset/datetime (`datetime` = a volume inside the interval,
  `any_time_in_interval`). Each site view's EDR `/locations?datetime=`
  (`location_time_filter`, #932) lists its site by the same rule over the
  same volume times, not by its temporal extent: a window between two
  volumes matches nothing, as `/locations/{nod}` would answer 404 there.
- **Auto-split happens in `server/src/admin.rs`** (`load_collections`, the
  `"odim-volume"` arm): build the engine once, enumerate `engine.sites()`
  (`(nod, label)` pairs), register one view per site (cloning the base
  `CollectionConfig` with per-site id/title). Site discovery is a scan
  snapshot — sites added later surface on the next reload. An initially
  empty remote source is re-expanded automatically by server startup recovery
  once its poll finds sites (#190).

## Parameters & labels

- **Parameter = bare ODIM quantity** (`DBZH`, `VRADH`, `ZDR`, `RHOHV`, …) —
  NEVER `<nod>:<quantity>`. The site is the collection, so the site has no
  place in the parameter name. This also lets WMS styling key off the
  quantity: `[[wms.parameters]]` (name = bare quantity) on the source config
  is inherited by every site.
- **Labels come from the quantity dictionary** (`src/quantities.rs`): the
  bare quantity stays the parameter *id*; the human label and unit come from
  the dictionary (e.g. `DBZH` → "DBZH — Reflectivity (horizontal)", unit
  dBZ); unknown codes fall back to the bare string. WMS layer `<Title>`s are
  prefixed with the site place name via `RasterInfo.layer_subtitle` (ODIM
  `/what` PLC).
- **COMP projdef earth figure (#1243).** `proj::figure` follows PROJ
  9.8.1's CRS rules, each pinned to `cs2cs`: `+R=` sphere overrules all;
  `+ellps=` (all 46 `proj -le` names, `NAMED_ELLIPSOIDS`, case-sensitive)
  or `+datum=` (10 PROJ datums; `ire65` = EPSG Airy Modified 1849, not
  `mod_airy`); `+a=` + first of rf/f/es/e/b, `+a` alone = sphere; nothing
  = WGS84. Unknown names, shape without `+a`, prolate figures, `+R_*`,
  and a name beside axes (PROJ resolves that per parameter) are errors —
  never a WGS84 guess. Datum shifts are not applied. `stated_epsg` still
  requires literal `+ellps=GRS80` + exact parameters; GRS80 by axes or
  datum is the figure but no EPSG claim. SMHI's composite is Bessel: its
  corners match the file's to ~20 m on Bessel vs ~200 m on WGS84.

## Rendering

- **Resampling is configurable** (`[odim] resampling`,
  `OdimConfig.resampling`). The per-site Cartesian render (`polar_sample`;
  WMS/Maps/Tiles) defaults to **`nearest`** — each output pixel takes its
  single enclosing polar cell, preserving every gate's value.
  `resampling = "bilinear"` blends the four surrounding `(ray, bin)` cells
  (`bilinear_cell`), smoothing the radial wedge structure far from the radar
  (#186) at the cost of softening peaks. The flag flows engine → every view.
  It does NOT affect: the COMP composite render (always nearest), EDR
  position/area queries (always nearest — a point query wants the
  measurement), or the 3D Tiles products (those sample nearest, then
  `ds-3dtiles` applies its own 3-D `smooth_grid` blur; the point cloud is
  raw).
- **The sweep a map draws is `drawn_sweep`**: among the sweeps carrying the
  quantity, the one nearest `z`, else the lowest. `polar_sample` and the
  site view's `MapEngine::resolve_elevation` share it, so WMS GetFeatureInfo
  asks the position query for that sweep's rounded angle and reads the
  sweep the map shows (#1064). The position query matches `z` to one sweep
  exactly (`resolve_levels`), so the advertised first angle would read
  nothing on a split-cut scan whose lowest sweep lacks the quantity.
- **Pixel pre-warm** (`[odim] prewarm_sweeps`, default `1`; #461/#472).
  Pixel arrays are read lazily per moment (#289), so a moment's first render
  does a cold read (its byte ranges, #1047, else the whole `.h5`) **while
  holding a render-semaphore permit** —
  a client animating N timesteps fires N concurrent cold reads and some
  requests get cut off by the front proxy (intermittent "missing timestep"
  frames; retry works because the backend finishes and caches). Fix: the
  poll loop already holds each new volume's bytes; `prewarm_pixels` (called
  from BOTH arms of `build_catalog` — remote AND local; local page cache is
  reclaimed off-peak, #472) decodes the lowest `prewarm_sweeps` sweeps'
  **default quantity only** (`prewarm_requests`: `DBZH`, else `TH`, else
  the first — `default_quantity_of`, the same rule as the advertised
  `default_quantity` and the bare-layer render, #992), or the quantities
  `[odim] prewarm_quantities` lists (#1046, `Prewarm`: one a volume lacks
  is skipped, an empty name is `EmptyPrewarmQuantity`), straight from
  those bytes on the background runtime into `PIXEL_CACHE`. Warming every moment
  of the base sweep wrote ~a dozen arrays per volume and evicted the
  reflectivity frames; other quantities and EDR read lazily. Best-effort +
  additive (never marks known-bad; `PixelCache::contains` skips
  already-resident moments); bounded by the pixel-cache byte budget
  (`MC_PVOL_PIXEL_CACHE_MB`). `0` disables; default `1` warms the base tilt
  (the standard reflectivity animation view).
- **Pixel cache policy (#992).** `PIXEL_CACHE` is filled ahead of reads, so
  it is built with `ByteBoundedCache::new_with_hot_allocation(…, 0.5)`
  (`PIXEL_CACHE_HOT_ALLOCATION`), not quick_cache's default 0.97. Under the
  default the hot ring kept whatever was inserted first after start, and
  every pre-warmed sweep shared a 3 % cold ring: evicted within minutes, so
  animation frames were cold multi-second reads against the 3 s render
  deadline. At 0.5 unread inserts get half the budget, first in, first out;
  a read promotes an entry to hot. Do not go below ~0.4: the hot target
  also caps the largest admitted entry (`0.5 × capacity / shards`, shards
  floored at ~31 × the 256 KiB entry estimate ⇒ ≥ ~3.9 MiB from 8 MiB up),
  and a 720 × 1000 f32 sweep (2.75 MiB) must fit — pinned by
  `full_f32_sweep_is_admitted_at_every_sensible_size`.
- **Multi-moment walks prefetch per file (#994).** Every walk over several
  moments of one volume file — EDR profiles (`profile_targets`),
  `level_series` (volume-major: one volume's batch at a time, all levels
  and quantities), the cross-section (`section_plan`: gates computed once
  per cell, the sweeps they hit prefetched), the point cloud and voxel grid
  (`volume_moments`) — first resolves exactly the moments it will read with
  `Pixels::prefetch`/`load_moments` (one read, of the moments' byte ranges
  when they have a plan (#1047) else one whole-file GET, and one HDF5 open
  through `PixelCache::load_many`, #993 classification intact) and samples
  through
  `Pixels::batched`. Never loop `Pixels::moment` cold over quantities ×
  sweeps: each first miss of a quantity downloads the whole file again
  (V × Q GETs per request). A batch read must name every moment the walk
  samples — anything left out reads as nodata — so share the target helper
  (`level_target`, `slant_cell_moment`) between prefetch and sampler.
  Pinned by the `*_fetches_*_volume_file_once` tests (`file_fetches`).
  The batch's arrays outlive LRU eviction until the walk moves on to the
  next volume, so an EDR walk over every quantity (no `parameter-name`:
  `resolve_quantities` returns them all) holds one decoded volume per
  request, every quantity × sweep it samples, while it samples: 78 MB of
  arrays for a 41 MB FMI volume (160 moments), times the EDR queries
  running at once. The PVOL read budget counts it (next bullet).
- **EDR walks reserve their reads (#1041).** `read_budget::BUDGET`
  (`MC_PVOL_READ_MEMORY_MB`, default 1024 MiB, a `ByteBudget`) admits
  every volume of `walk_volumes` before its file is read. A batch with
  nothing read ahead reserves inside `load_moments_from`'s loader, once
  `load_many` has named the missing moments: their arrays at `sweep_bytes`
  (4 B/sample, twice FMI's u16 arrays: 155 MB for that volume), held with
  the batch until the walk moves on, and the bytes it fetches
  (`ReadPlan::bytes`, else `file_len`, the whole file again when a ranged
  read falls back) until they are decoded. Resident moments and held
  bytes cost nothing. A read ahead (`start_read`) reserves what it
  downloads (its cold targets' `ReadPlan` ranges, else the whole file,
  #1261) and their arrays with `try_reserve` when it starts, so its batch
  needs nothing more; ranges that turn out unusable reserve the whole-file
  read made instead. One with no room is not
  started, counts no rejection, and stops the topping up until the
  volumes before it are sampled: a volume behind never holds what the one
  before it needs, and the front then reserves as it reads. A walk thus
  reserves at most four volumes, about 750 MB for an all-quantity FMI walk
  reading ahead. A joined download (#1127) counts again for each holder.
  A reservation that does not fit fails the batch with `ResourceExhausted`
  (503) before it reads, marking nothing bad. Only EDR walks charge it,
  GetFeatureInfo's position query included: `volume_moments` (3D Tiles)
  and `Pixels::moment` (maps) pass no budget. Pinned by
  `a_batch_reserves_its_arrays_and_file_before_reading_them`,
  `a_ranged_batch_reserves_its_ranges_not_the_file`,
  `a_ranged_read_ahead_reserves_its_ranges_not_the_file`,
  `read_ahead_walks_read_no_further_ahead_than_the_budget_holds` and
  `edr_walks_charge_the_budget_mc_pvol_read_memory_mb_sizes` (a child
  process at `MC_PVOL_READ_MEMORY_MB=0`).
- **EDR walks over several volumes download ahead (#999).** The profiles
  and `level_series` of `site_coverages` and the sections of
  `site_trajectory` go through `walk_volumes`: `prepare` names each
  volume's moments first (`profile_targets`, `level_target`,
  `section_plan`), and for up to `MAX_WALK_VOLUMES_IN_FLIGHT` (4) volumes,
  the one being sampled included, what their batch will read of the
  remote files (a target neither resident nor known bad, no held bytes)
  downloads concurrently: the cold targets' `ReadPlan` ranges through
  `DataStore::start_get_ranges_or_refusal` (#1261), else the whole file
  through `DataStore::start_get_or_refusal`. A batch uses ranges read
  ahead only when
  they hold every moment it misses, else it reads as if nothing were read
  ahead; unusable ranges (file length changed, decode outside them) fall
  back to the whole file. Reads ahead are async tasks on the
  runtime that runs the query (`Pixels::handle` for a cross-section on the
  blocking pool, else the query runtime worker's own), never a rayon pool,
  a thread per file or a new `Runtime` (root Critical Rules 7 and 9). Each
  volume is then decoded and sampled in time order on the walk's thread,
  from the same batch the sequential walk reads (`Pixels::prefetch` with
  the downloaded bytes), and dropped before the next download starts: at
  most four volumes fetched or decoded per request, so up to
  4 × `MAX_REMOTE_FILE_SIZE` (64 MiB) of downloaded bytes per query, times
  the EDR queries admitted at once (`ds_executor::query_workers()`), plus
  about 3.3 MB of section cell geometry per volume (`SectionPlan`: 1024
  nodes × 100 levels × 32 B). The cap is a maintainer decision, pinned by a
  const assert. Errors map as before: the deadline (checked per volume,
  bounding every wait for a download) or a transient failure ends the walk
  and drops the downloads in flight; a missing object is nodata and known
  bad. A local source, a one-volume walk (GetFeatureInfo, a single
  `datetime`) and a thread without a multi-thread runtime read each file
  as the walk reaches it, inside the per-file lock. Never
  loop `Pixels::prefetch` over remote volumes outside `walk_volumes`.
  Pinned by `walks_download_volumes_concurrently_at_most_four_at_once`
  (wall time ≈ ceil(V/4) GET delays, each file once),
  `read_ahead_walks_answer_as_the_sequential_walk`,
  `read_ahead_walk_stops_at_the_deadline`,
  `walks_read_ahead_only_their_moments_byte_ranges` and
  `a_batch_answers_unusable_ranges_read_ahead_from_its_own_read`.
- **Concurrent requests share a volume file's download (#1127).** Every
  remote whole-file read of the pixel path, a walk's whole-file read ahead
  and a miss inside the per-file lock alike (`fetch_file_bytes`, on a thread with a
  multi-thread runtime), is a `RemoteRead`: it joins the download of the
  file another request holds, registered by source-qualified cache id in
  `PixelCache` (`join_or_start_read`), or starts one that others join. The
  registry is weak: a read lives while a request holds it, and each holder
  keeps it until the file's moments are decoded, so a request arriving
  meanwhile joins instead of downloading again; the last holder dropping
  it aborts it, and no bytes outlive their holders. A started read
  (`DataStore::start_get_or_refusal`) is bounded by `FetchBudget::Request`
  alone
  and each wait by its own thread's deadline, so one request's deadline
  never cuts off a read another waits on. A read that failed is not
  joined, and a request whose joined read failed reads once more, its own
  read or one started since: the failure is the starter's (503/504), not
  the joiner's, and nothing is marked bad. Pinned by
  `concurrent_cold_walks_download_each_volume_once` (GETs counted at the
  store) and `a_joined_download_that_fails_is_read_again`. Ranged reads
  (`fetch_spans`, a walk's ranged read ahead) are not shared.
- **A request waiting for the per-file lock leaves its runtime running
  (#1200).** `PixelCache::with_file_load` takes a free lock at once. A
  request that finds another's load of the file in flight waits inside
  `block_in_place` on a multi-thread runtime (`off_core`), because that load
  may be downloading the file as a task on the same runtime: requests
  parked on every query-runtime worker left nothing to run the download or
  the deadline timers, and the runtime stalled for good. The wait ends at
  the request deadline with `DeadlineExceeded`: its load never runs and
  nothing is marked bad. The lock's mutex guards a flag, never the load.
  Pinned by `one_volume_walks_waiting_on_a_cold_file_leave_the_runtime_running`
  (more walks than workers), `walks_waiting_on_a_cold_file_stop_at_their_deadline`
  and `file_load_waiter_gives_up_at_its_deadline`.
- **Cold reads by byte range (#1047).** `parse_and_cache` runs
  `layout::probe` on the bytes the scan holds: per moment, a fresh
  `Hdf5File` over a recording storage resolves the dataset and walks
  `iter_chunks` with filters that fail at once (`probe_filters`), so each
  chunk's raw read is recorded and nothing is decompressed. The reads,
  merged within `PLAN_GAP` (64 KiB), are `PolarMoment::spans` (u32, next to
  `PolarVolume::file_len`). A cold miss with no held and no read-ahead
  bytes builds a `ReadPlan` over the moments it needs and fetches only
  those ranges (`fetch_spans`: local reads after a length check; remote
  `DataStore::start_get_ranges_or_refusal` as a task on `shared_read_runtime`,
  else `get_ranges_or_refusal[_on]`, a GET per range, at most ten at once,
  without object_store's 1 MiB merge), then decodes through `SpanStorage`.
  No plan means the whole file: a moment not stored in chunks (contiguous
  or compact, like `testdata/pvol-cold-batch.h5`), more than
  `MAX_PLAN_RANGES` (10) ranges, or more than `MAX_PLAN_FRACTION` (half)
  of the file. A decode that reads outside the fetched ranges
  (`SpanStorage::missed`, logged at WARN) or a file whose length changed
  also reads the whole file; neither is ever bad data. #993 holds: a
  missing object is `Bad`, a deadline or other failure `Abort`, with no
  whole-file retry. A ranged single-moment miss decodes no sibling. Walks
  over several remote volumes read their batches' plans ahead the same way
  (#1261). A cold
  base-sweep DBZH miss reads 0.46 % of the 41.4 MB FMI volume in 7 ranges,
  1.37 % of a 25.5 MB SMHI volume in 4 and 3.9 % of a 1.2 MB DMI volume in
  4. Pinned by `layout::tests`, `cold_miss_reads_only_the_moments_byte_ranges`,
  `ranged_miss_is_classified_as_the_whole_file_read`,
  `misses_without_a_usable_plan_read_the_whole_file`,
  `a_batch_reads_byte_ranges_or_the_whole_file` and, when the uncommitted
  radar fixtures are present, `fixture_volumes_decode_identically_from_their_byte_ranges`,
  which decodes every moment from exactly its recorded reads.
- **Fetched files decode in place (#1261).** Open fetched `Bytes` with
  `reader::open_bytes` (`BytesStorage`), never `Hdf5File::from_bytes`,
  which copies the whole file twice (`to_vec`, then `Arc<[u8]>`).
  hdf5-reader 0.9 has no public zero-copy `StorageBuffer`, so each read
  copies only its range: a metadata parse or a one-moment decode copies
  kilobytes, not 20–40 MB. Pinned by
  `shared_bytes_decode_as_a_copy_and_copy_only_what_is_read`.
- **Cold single-moment misses (#293, #994):** only a map render reads one
  moment at a time. A `Pixels::moment` miss reads the moment's byte ranges
  (previous bullet) or else fetches the source-qualified file once, decodes
  **only the requested moment** on the caller's thread and returns it.
  After a whole-file fetch only, speculation (`sibling_requests`) is the requested quantity
  at the `MAX_SIBLING_SWEEPS` (2) nearest other sweeps — what a viewer
  stepping `ELEVATION` reads next — within one sweep's worth of bytes (the
  requested sweep's moment count × its array at four bytes/sample, at most
  capacity/16); other quantities are not decoded ahead. A fixed-`ELEVATION`
  animation never reads those siblings: two wasted inserts per cold frame
  is the price, watch `pvol_pixel_cache_inserts_total`. It runs off the
  request path (`spawn_sibling_decode`) as `spawn_blocking` on the
  background poll runtime, which `PolarVolumeEngine::poll_loop` registers
  (`set_background_runtime`); before any poll loop starts, nothing is
  decoded ahead. While it runs, the downloaded bytes are registered in
  `PixelCache::held_bytes` (single flight per file, at most
  `MAX_HELD_FILES` files, shared with finished reads) and a miss on that
  file — single
  or batch — decodes from them instead of downloading again; the guard is
  dropped only after the last insert, so a miss that finds no held bytes
  then sees every decoded sibling. Both paths snapshot held bytes BEFORE
  their cache recheck (`load_many` passes its snapshot to the loader). A
  miss on a sibling the background task has not reached yet decodes it
  again on the request thread (bounded: at most two arrays). Per-file locks
  still coalesce concurrent misses. A sibling that fails to decode is
  skipped, never marked bad. Whole-volume speculation (the old capacity/4
  batch) flooded the cache's insert ring and evicted pre-warmed sweeps — do
  not reintroduce it on the request path.
- **A read that fails is not bad data (#993).** `Pixels::moment` is
  `Ok(None)` only for unusable data (missing file/object, HDF5 open or decode
  failure), which `mark_bad` negatively caches and samplers draw as nodata.
  A request deadline (`DeadlineExceeded`) or any other storage failure
  (`ResourceExhausted`, detail logged) is `Err`: never `mark_bad`, never
  nodata. Every sampler propagates it (`?`), so WMS/Maps/Tiles answer 503 and
  the meta-tile / rendered caches store nothing (they cache only `Ok` tiles),
  and EDR walks over volumes stop at the first such error (504/503). A
  missing remote object is told apart through
  `DataStore::get_or_refusal[_on]` and the other `_or_refusal` reads
  (`RangeRefusal::NotFound`), never by matching error strings. A refusal
  (#1046: `Forbidden`, a 401/403; a local
  `PermissionDenied`/`IsADirectory`/`NotADirectory`) is not the data's fault
  either: `Abort(ResourceExhausted)`, never `mark_bad`, and
  `PixelCache::refuse` remembers it for `REFUSAL_RETRY_AFTER` (60 s), so
  reads of the file meanwhile fail at once with no store request or log line
  (`check_refusal`). Pinned by
  `a_refused_volume_fails_at_once_until_the_refusal_expires`.
- **A read the deadline cuts off is finished (#1046).** A started read whose
  wait the request deadline ends (`RemoteRead::wait`, `fetch_spans`,
  `AheadRead::wait`) is handed with the moments the request wanted
  (`Finish`) to `finish_in_background`: a blocking task on the background
  runtime waits for it and decodes them into `PIXEL_CACHE`, a whole file's
  bytes held in `held_bytes` and its read kept joinable until the last
  insert, so a retry joins the download or hits the cache. One per file
  (`claim_finish`), `MAX_FINISHING_READS` (4) at once, nothing without a
  registered runtime; a failed or refused read decodes nothing and marks
  nothing bad. Finished reads are not charged to the PVOL read budget; the
  cap bounds them. Pinned by
  `a_read_the_deadline_cuts_off_is_finished_for_the_retry`,
  `a_retry_joins_the_read_the_deadline_cut_off`,
  `a_ranged_read_the_deadline_cuts_off_is_finished_for_the_retry` and
  `a_batch_read_the_deadline_cuts_off_is_finished_for_the_retry`.
- **Aborted reads are counted (#1046).** `aborted()` counts each read whose
  error becomes the request's (`moment_with`, `batch_failure`, and a
  deadline while waiting for another's file load) by `AbortReason`:
  `pvol_pixel_read_aborts_total{reason}`, plotted on the PVOL pixel cache
  churn panel next to `pvol_pixel_read_failures_total` (nodata reads only).
- Undetect vs nodata: `RawPixels::sample_class` (`src/reader.rs`)
  distinguishes `Value`/`Undetect`/`Masked` — clear air (`undetect`) is a
  measurement, the cone of silence (`nodata`) is not. `voxel_grid_from_volume`
  fills `Undetect` with the finite `NO_ECHO_FLOOR` (−32 dBZ) and leaves
  `Masked` as NaN (#360).

## EDR specifics

- **Cross-sections:** `query_trajectory` returns a CoverageJSON `Section`
  (composite `[t,x,y]` axis + numeric `z` = height above antenna via the
  4/3-Earth beam model). `z` selects sweeps exactly as position/area do
  (`resolve_levels`, `SWEEP_MATCH_TOLERANCE_DEG`; none matching → 400), and
  `section_plan` keeps only the cells whose nearest sweep is selected —
  never the nearest *selected* sweep, and never a ±1° band around `z`
  (#983, EDR 1.2 `/req/edr/z-response` B). Vertical axis is elevation angle
  (`VerticalKind::ElevationAngle`). The per-site view
  declares `TrajectoryShape::CrossSection` (#926): api-edr then accepts a
  2-D `LINESTRING` only, offers the PNG heatmap, and dispatches on the
  blocking pool that `blocking_pixel_handle` expects. Not the gridded
  engines' along-path `Trajectory`.
- **Coverage floor (#514):** every `Section` carries `coverage_floor` — per
  node, the height of the *effective* lowest surveyed beam (`window ∩` the
  volume's sweep envelope, so a `z`-narrowed request gets the floor of the
  band shown) via `beam_height_at_ground` (the same `ground/cos(el)`
  one-step `height_axis` uses). Raw metres — may dip below 0 near the radar
  and pierce the axis top far out; api-edr clips it in the PNG and emits it
  verbatim as the `meteocore:beamCoverage` domain foreign member in JSON.

## VolumeEngine (3D Tiles) implementation

- Cold 3D point/grid reads batch all missing sweeps of the requested quantity
  through one file fetch and HDF5 open. `PixelCache::load_many` serializes
  same-file batches and rechecks the shared cache before decoding. The render
  retains decoded Arcs so eviction or a disabled cache cannot cause rereads
  inside its sampling loop. Unusable moments keep the known-bad policy; a
  deadline or storage failure fails `volume_moments` (#993), so the voxel-grid
  cache never keeps an empty grid for it. `volume_point_cloud` and
  `resolve_column` propagate a read `Err` too (#1046), so a caller without
  the batch cannot turn a deadline into nodata.


Encoder-side rules live in `crates/ds-3dtiles/CLAUDE.md`; API routes/caching
in `crates/api-3dtiles/CLAUDE.md`.

- `PolarVolumeSiteView` implements
  `read_point_cloud(quantity, time, min_value, reference_time)` →
  `VolumePointCloud` (one point per echo cell at its true ECEF position via
  the 4/3-Earth beam model), `read_voxel_grid(quantity, time, dims,
  reference_time)` → `VoxelGrid` (regular cylindrical grid
  `radius`×`angle`×`height`, NaN = nodata), and `volume_info()` →
  `Arc<VolumeInfo>` (O(1) cached snapshot, #211 — quantities, times, default
  quantity, coverage region).
- Bounds: point cloud ≤ `MAX_POINTS` (8M); voxel grid ≤ `MAX_VOXELS` (32M
  cells).
- Both samplers share `select_entry_and_quantity` and resample via the
  envelope guards of `slant_cell` (the voxel grid's `resolve_column` mirrors
  them guard for guard) — never fabricate data across the
  cone of silence. Unknown quantity ⇒ `InvalidParameter` (→ 400).
- `read_point_cloud`/`read_voxel_grid` are sync (blocking HDF5 I/O + long
  CPU loops); the API layer bounds them with its 3D Tiles content pool and
  `spawn_blocking`.
- **`VOXEL_GRID_CACHE`**: `read_voxel_grid` returns `Arc<VoxelGrid>` from a
  global LRU keyed (file, quantity, dims) — isosurface/echo-top/voxels and
  threshold changes share one polar resample
  (`MC_PVOL_VOXEL_GRID_CACHE_MB`, default 512). The resample resolves each
  `(radius, height)` column once (`ColumnTarget`) instead of per-cell
  lookups.

## Quantitative voxel validation (#641)

`examples/voxel_validation.rs` compares the production grid against all native
gates of the exact requested quantity. `voxel_diagnostics` contains the offline
range-band comparison and declared beam-support integration model, with
fixture-free regression tests. See [method and measured baseline](../../docs/odim-voxel-validation.md).
The beam width is an assumption; its VIL-like integrals are not ground truth.
Nearest sampling can miss peaks and extend echoes vertically; larger grids do
not guarantee monotonic improvement. This baseline does not enable #642.

## Storm cells (#367)

- `ds_core::cells` segments a `VoxelGrid` into `StormCell`s on the
  **column-maximum 2-D projection**: mask = any voxel in the
  `(radius, azimuth)` column ≥ threshold (default 35 dBZ); one-cell
  morphological closing bridges speckle gaps; 4-neighbour connected
  components; the azimuth seam wraps. This is the TITAN-style operational
  convention — footprints never nest or overlap, and vertically-split echo
  is ONE cell. (Full-3-D CC rendered as nested-ring spaghetti — rejected.)
- Attributes come from the 3-D member voxels (max dBZ, linear-Z centroid,
  echo top/base, volume, area, column-max VIL with 56 dBZ hail cap) + a
  deterministic CCW footprint ring. `track_cells` matches centroids across
  scans (gated greedy with constant-velocity prediction; `Track.motion_ms
  (u,v)` seeds future motion products).
- Generic surface: `VolumeEngine::read_cells(CellQuery)` (default impl over
  `read_voxel_grid`; window clamped to `MAX_TRACK_SCANS`; an empty target
  scan is a valid empty result, NOT 404). The PVOL view overrides it with
  the byte-bounded per-volume `CELL_SET_CACHE` (`MC_PVOL_CELL_SET_CACHE_MB`,
  default 64; volumes are immutable ⇒ never stale) and gates to dBZ-unit
  quantities (VIL/linear-Z is reflectivity physics → else 400).
- **A cold `CELLS` window (#1155).** A tile segments the scan it draws and
  the `CELLS_TRACK_SCANS` (6) behind it: seven whole volumes, each a GET,
  an HDF5 decode of every reflectivity sweep (most of the ~50 ms CPU), a
  voxel grid and a segmentation. `cell_product` claims each volume's
  `CELL_SET_CACHE` fill with a zero-timeout probe and builds only volumes
  no other caller is building, so a viewport's tiles build a cold window in
  parallel, one GET per volume; built in turn, seven 0.5 s GETs outlast the
  3 s render deadline. It waits for the rest through
  `ds_executor::wait_without_slot`, so a waiting tile holds no render slot.
  A claim a failed builder hands over is let go before that wait for a
  slot and re-probed: never hold a fill claim while queueing for a slot.
  The poll loop builds cell sets ahead of the tiles, on the background
  runtime: a downloaded volume newer than the site's newest from the bytes
  in hand (`warm_from_bytes`, no second GET), and the newest windows newest
  volume first, at most `MAX_WARMS_PER_POLL` (32) per poll and once as the
  loop starts (`warm_windows`, four downloads at a time, a volume claimed
  only after its download so no tile waits on a download without a
  deadline). Warm builds decode into memory of their own
  (`voxel_grid_from_bytes`), never `PIXEL_CACHE` or `VOXEL_GRID_CACHE`
  (#992), key entries as tiles do (`layer_query`, `cell_set_key`), and count
  no hit or miss. Pinned by `tests/cells_viewport.rs` and the warm-up tests
  in `cells::tests`.
- **WMS/Maps/Tiles `CELLS` layer:** each reflectivity-capable site
  advertises a derived `CELLS` parameter (`engine_odim::cells::
  CELLS_PARAMETER`; appended to `SiteMeta.parameters` only — never an EDR
  quantity or 3D Tiles quantity) rendering footprint outlines + centroid
  markers (at the cell's max dBZ) + track trails into the `RasterTile` via
  `ds_core::raster_paint::Canvas` (Bresenham + Liang–Barsky clip,
  value-space pre-colorize). Vertices are projected per-VERTEX via
  `OutputCrs::world_to_fraction` — never per pixel (#203).
- **Trails** are drawn only for cells present in the *rendered* scan (a
  trail must terminate at a visible outline; drawing every windowed track
  painted orphan lines). Trails paint at the reserved `CELLS_TRACK_SENTINEL`
  value, which `ds_render::OverlayColorMap` (wrapping whatever colormap
  CELLS resolves to) renders as the neutral `CELLS_TRACK_COLOR` (dark grey)
  so trails stay visually distinct from dBZ-coloured outlines.
- Styling: outlines inherit the collection colormap (dBZ) or an explicit
  `[[wms.parameters]] name = "CELLS"`. `time` selects the scan (TIME
  animation works); `z` is ignored. 3D Tiles `representation=cells` +
  Features output are follow-ups of #367.
- Queryable by its features (#1100): `layer_shows_values` is `false` for
  `CELLS` and `layer_picks_features` is `true`. WMS GetFeatureInfo calls
  `pick_features` → `pick_cells`: the target scan's cell whose outline holds
  the pixel centre, from `cell_product(&layer_query(..), None)`, the tiles'
  product and cache, with no `spawn_blocking` handle since it runs on the
  query runtime. It serves what the per-site product has, never the
  nowcast's severity or significance. Every moment layer is queryable by
  value.

## File discovery

- COMP filenames go through the shared
  `ds_storage::discovery::FilenameMatcher` (#817), built by `build_matcher`
  in `src/engine.rs` from `filename_template` or `filename_pattern` +
  `timestamp_format`, and shared with engine-geotiff — never re-implement
  it here. A template is anchored `^…$`, so `.tmp`/`.part` partial uploads
  never match; an explicit pattern is used as written and logged at WARN
  when unanchored.
- COMP `scan_local_directory` / `scan_remote` are thin wrappers over the
  shared catalog scan, `ds_storage::discovery::{scan_local, scan_remote}`
  (#817): prefixes listed concurrently (at most `MAX_CONCURRENT_LISTS`),
  window, one entry per timestamp (greatest key or path wins, logged), cap.
  The local scan follows symlinks to files (`Symlinks::Follow`). The engine
  keeps only its error policy: all prefixes failing with nothing kept is an
  error, some failing is a WARN.
- COMP `discovery = "template"` (#287) runs the shared
  `ds_storage::discovery::TemplateProber` (#1067), configured by
  `template_prober` in `src/engine.rs`: `Schedule::Cadence` of
  `cadence_secs`, `max_files` as `max_slots`, at most `MAX_TEMPLATE_SLOTS`
  HEADs per poll, `Settling::OnSight` and `immutable`, so a found composite
  is catalogued at once and never HEADed again, and objects over
  `MAX_REMOTE_FILE_SIZE` are skipped. `Source::TemplateHttp` keeps the
  prober, and with it what it found, between polls. Never re-implement the
  probing here.
- PVOL `enumerate_remote` uses no template: `parse_key_timestamp` reads the
  basename's first 12-digit run, only as a pre-fetch window filter. It lists
  through the shared bounded-concurrent `discovery::list_prefixes`, never a
  `list` per prefix in a loop, but not the matcher scan: many sites share a
  timestamp, so dedup and cap would drop volumes. Its bootstrap
  newest-per-stream reduction stays here.
- Both PVOL enumerations (`enumerate_local`, `enumerate_remote`) keep a
  basename only when `is_volume_name` accepts it: `.h5`, and not a
  temporary name (`ds_core::temp_files`), so a volume written as `.name.h5`
  is never HDF5-parsed truncated (#1009). `enumerate_remote`'s LIST is
  recursive, so it also skips a key in a hidden folder below its prefix
  (`temp_files::in_hidden_folder`, #1029: rsync `--delay-updates` stages
  files under their final names in `.~tmp~/`); `enumerate_local` never
  descends. The COMP scans get the hidden-name and, remote, hidden-folder
  skips from the shared catalog scan and pass
  `temp_files::partial_exclude_patterns()` as `ScanSpec.exclude`: COMP has
  no `exclude_patterns` setting, and an unanchored explicit pattern matches
  inside `….h5.part`.

## Retention & fixtures

- `[odim] max_files` (default unbounded) / `time_window` bound retained
  volumes per site; these set the 3D Tiles animation frame count. Operators
  should bound unbounded sources.
- Local test fixture: `testdata/radar-fmi-pvol/` (fivih = Vihti). Several
  radar fixtures are large and intentionally NOT committed — tests skip with
  an eprintln when a fixture is absent. The committed polar volume is
  `testdata/pvol-cold-batch.h5` (contiguous, so it has no byte-range plan),
  pinned end to end by `reads_the_committed_pvol_fixture`.

## Startup errors (#190)

COMP/PVOL constructors still report scan/configuration errors; poll failures
still preserve the last good catalog. The server retries failed remote startup
construction with backoff and re-registers per-site APIs when an initially empty
remote catalog becomes populated. This recovery reuses accepted configuration
and existing healthy engines; it does not turn scan errors into empty catalogs.
