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
  bbox/limit/offset/datetime.
- **Auto-split happens in `server/src/admin.rs`** (`load_collections`, the
  `"odim-volume"` arm): build the engine once, enumerate `engine.sites()`
  (`(nod, label)` pairs), register one view per site (cloning the base
  `CollectionConfig` with per-site id/title). Site discovery is a scan
  snapshot — sites added later surface on the next reload.

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
- **Pixel pre-warm** (`[odim] prewarm_sweeps`, default `1`; #461/#472).
  Pixel arrays are read lazily per moment (#289), so a moment's first render
  does a cold whole-`.h5` read **while holding a render-semaphore permit** —
  a client animating N timesteps fires N concurrent cold reads and some
  requests get cut off by the front proxy (intermittent "missing timestep"
  frames; retry works because the backend finishes and caches). Fix: the
  poll loop already holds each new volume's bytes; `prewarm_pixels` (called
  from BOTH arms of `build_catalog` — remote AND local; local page cache is
  reclaimed off-peak, #472) decodes the lowest `prewarm_sweeps` sweeps'
  moments straight from those bytes on the background runtime into
  `PIXEL_CACHE`. Best-effort + additive (never marks known-bad;
  `PixelCache::contains` skips already-resident moments); bounded by the
  pixel-cache byte LRU (`MC_PVOL_PIXEL_CACHE_MB`). `0` disables; default `1`
  warms the base tilt (the standard reflectivity animation view).
- Undetect vs nodata: `RawPixels::sample_class` (`src/reader.rs`)
  distinguishes `Value`/`Undetect`/`Masked` — clear air (`undetect`) is a
  measurement, the cone of silence (`nodata`) is not. `voxel_grid_from_volume`
  fills `Undetect` with the finite `NO_ECHO_FLOOR` (−32 dBZ) and leaves
  `Masked` as NaN (#360).

## EDR specifics

- **Cross-sections:** `query_trajectory` returns a CoverageJSON `Section`
  (composite `[t,x,y]` axis + numeric `z` = height above antenna via the
  4/3-Earth beam model). `z` selects the elevation-angle band. Vertical axis
  is elevation angle (`VerticalKind::ElevationAngle`).
- **Coverage floor (#514):** every `Section` carries `coverage_floor` — per
  node, the height of the *effective* lowest surveyed beam (`window ∩` the
  volume's sweep envelope, so a `z`-narrowed request gets the floor of the
  band shown) via `beam_height_at_ground` (the same `ground/cos(el)`
  one-step `height_axis` uses). Raw metres — may dip below 0 near the radar
  and pierce the axis top far out; api-edr clips it in the PNG and emits it
  verbatim as the `meteocore:beamCoverage` domain foreign member in JSON.

## VolumeEngine (3D Tiles) implementation

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
  envelope-guarded `sample_polar_slant` — never fabricate data across the
  cone of silence. Unknown quantity ⇒ `InvalidParameter` (→ 400).
- `read_point_cloud`/`read_voxel_grid` are sync (blocking HDF5 I/O + long
  CPU loops); the API layer bounds them with the render semaphore and
  `spawn_blocking`.
- **`VOXEL_GRID_CACHE`**: `read_voxel_grid` returns `Arc<VoxelGrid>` from a
  global LRU keyed (file, quantity, dims) — isosurface/echo-top/voxels and
  threshold changes share one polar resample
  (`MC_PVOL_VOXEL_GRID_CACHE_MB`, default 512). The resample resolves each
  `(radius, height)` column once (`ColumnTarget`) instead of per-cell
  lookups.

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

## Retention & fixtures

- `[odim] max_files` (default unbounded) / `time_window` bound retained
  volumes per site; these set the 3D Tiles animation frame count. Operators
  should bound unbounded sources.
- Local test fixture: `testdata/radar-fmi-pvol/` (fivih = Vihti). Several
  radar fixtures are large and intentionally NOT committed — tests skip with
  an eprintln when a fixture is absent.
