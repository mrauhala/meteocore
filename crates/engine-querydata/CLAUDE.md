# engine-querydata crate — Claude Instructions

FMI QueryData (`.sqd`) binary-format engine. Memory-mapped file access via
`memmap2`. Read the root `CLAUDE.md` first (poll loop on the background
runtime).

- **Multi-parameter:** exposes all parameters from the file; `wms_parameter`
  config (name / short name / ID) selects which to render, reported by its
  map name.
- **Model runs (#337):** polls the directory and retains the most recent
  `max_runs` `.sqd` files as model runs, keyed by each file's **origin
  (analysis) time** (`RunSet: BTreeMap<DateTime<Utc>, _>`), atomically
  swapped via `ArcSwap`. Already-loaded files are reused on poll (not
  re-parsed). Each run is an EDR instance / `RasterInfo.reference_times`
  entry; the latest run is the default for un-pinned queries. Implements the
  shared `ds_core::instances` contract (root CLAUDE.md).
  `list_sqd_files` skips temporary names (`ds_core::temp_files::is_temporary`):
  a publisher writes `.name.sqd` and renames it, and parsing it mid-write
  logged a spurious ERROR and read up to 1 GB (#1009).
- **Grids:** WGS84, Rotated Lat-Lon, Stereographic, Lambert Conformal Conic
  (MEPS: tangent cone, `lat1 == lat2`). `GridInfo::new` derives the
  GeoTransform once; per-pixel code reads it, never re-projects corners.
  It samples the WGS84 extents once too (`lonlat_extent`, `bbox`, from
  `bbox_with_poles`: a held pole at every longitude, `west > east` across
  the antimeridian, #1194); the
  per-request metadata reads them, never re-projects edges (#881).
- **Corner gotchas (`GridInfo::new`):** the stored corners are grid-point
  *centres* (spacing = span / (n − 1)). A lat/lon area starts at its first
  stored corner even when that is the north edge (ECMWF Kenya). A projected
  area starts at the south-west corner of its projected rectangle whichever
  two corners it stores — MEPS stores the NW and SE ones — so the projected
  corners are min/max-normalised. Both orientations are pinned against the
  fixtures' geography (`grid_lonlat_corners`, `meps_rows_run_south_to_north`);
  a corner-coordinate test alone cannot catch a flipped row order.
- The LCC `radius` line (e.g. 6371220 m) becomes `Crs::LambertConformalConic
  { radius: Some(r) }`: the grid is defined on that sphere, and projecting it
  on WGS84 misplaced the full MEPS interior by ~1.2 km mean (#800). The
  stored corners have only six significant digits, so corner anchoring lands
  within ~5 m of the file's world rect — pinned in `parse_meps_lcc`. The
  stereographic record carries no radius line and still projects on WGS84 (`figure:
  Ellipsoid::WGS84`; #1243).
- The render path projected per node (every source but a lat/lon grid
  under CRS:84 / Web Mercator) takes the shared sampler's map (#1190):
  `GridInfo::source` is a `ds_core::resample::SourceGrid` built once at
  load, and a render samples the memory map at
  `source().output_map_quiet(..).position(ox, oy)`, checking the request
  deadline per row, and logs `capped_error()` through `tracing`. The map's
  rules are ds-core's: a projected grid's longitudes within half a turn of
  its centre (`Lcc::forward` is not periodic, θ = n·Δλ, #1114), its
  footprint window, no grid when the footprint misses the image (#1129);
  a lat/lon grid's map periodic in longitude, so EPSG:3035's 170°W cut and
  the pole stay registered (#1188, #323). `sample_grid_bilinear` keeps the
  sampler's rules on the memory map, a global grid's columns wrapping
  (`SourceGrid::global_period`) on every path, EDR included.
  `engine/sampler_tests.rs` pins the render to `SourceGrid::render_f32`
  value for value, world copies and the seam box included. The engine does
  not render through `render_f32` because QueryData's param → location →
  level → time layout strides one field by levels × times, so a decoded
  `&[f32]` field would cost a full-field strided read per tile.
- A global lat/lon grid on a tile holding a pole still refines its map to
  the 256-cell density, about 1.49 M projections (~0.4 s): beside the pole
  the bilinear column map converges only linearly. It meets the budget and
  no longer caps; the cost is `ProjectionGrid`'s, shared by engine-grib and
  every `SourceGrid` (see `engine/global_tests.rs`).
- EDR position queries and map rendering use bilinear interpolation. EDR area (and radius via the shared default) returns a
  CRS84 `Grid` over the polygon bbox at native resolution (≤ 256 cells per
  axis, 1M-value budget across time × cells × parameters), every cell
  bilinearly interpolated and cells outside the polygon masked to null
  (`QueryPolygon::sample_grid` in ds-core, #671). One `t` axis when the
  datetime window selects several steps.
- EDR trajectory (#926): the path, time rules and layout come from
  `ds_core::trajectory::TrajectoryPlan`, densified at the area query's mean
  cell size (`lonlat_extent` / grid dims). Each sample is projected once
  (`world_to_grid_px`) and reused for every parameter and step, then
  `sample_grid_bilinear` — the position query's own interpolation. The run
  is memory mapped, so there is no I/O to group. No vertical axis: a Z
  coordinate is ignored.
- Missing-value sentinel: `32700.0`.
- **Units (#1211):** a `.sqd` descriptor has no unit string; the FMI newbase
  parameter number fixes it. `src/units.rs` is the one table (number → unit),
  each entry sourced from FMI code (newbase `NFmiTotalWind`, smartmet-qdtools
  nctoqd `apply_unit_conversion` and gribtoqd `cnf/grib.conf`; newbase itself
  keeps no unit table). Values are stored in these units: never convert. A
  number not in the table reports `""`, never a unit guessed from a name;
  Max/MinTemperature (5/6) stay out because gribtoqd copies DWD Kelvin
  into them unconverted (`grib.conf` `121;5`, `122;6`), and
  PrecipitationRate (49) because FMI's converters write it in both mm/h
  and kg m-2 s-1. Every unit-reporting path reads it: EDR descriptions
  (`description()`), `RasterInfo.parameters`/`unit`, and `wind_facts`
  (derived speed takes the components' `m s-1`).
- **Names (#1215):** the map APIs name a parameter by the short name in
  parentheses at the end of its descriptor (`2t` for
  `2 Metre Temperature (2t)`, `map_name`), and so does `RasterInfo.parameter`,
  so the root layer draws `{collection}/{default}`'s style (#1161). EDR
  advertises the full descriptor (`get_parameters`, descriptions) and
  `select_parameters` also takes the short name as its alias on every query
  type, descriptor first (the map's `param_index_by_name` order), keying the
  coverage by the name asked for: WMS GetFeatureInfo asks for the map name.
  Both names of one parameter are two ranges, which `DerivedWind` relies on
  (it asks for components by descriptor). Errors stay the shared
  `ds_core::edr_engine::select_parameters` 400.
- **Wind (#897):** `wind_facts` reports each parameter's FMI number and both
  names, built with the run set. `ds_core::wind` pairs `kFmiWindUMS` /
  `kFmiWindVMS` (23/24), or vocabulary short names, into `WindSpeedMS` (or
  e.g. `10si`), plus `WindDirection` where the frame allows. The format
  carries **no u/v frame flag** (not in the header, the parameter
  descriptors or the area classes), so the frame is FMI newbase's
  convention: **u/v are relative to the data's own grid**
  (`VectorFrame::Grid`). Its basis is smartmet-library-newbase
  `newbase/NFmiFastQueryInfo.cpp`, `NFmiFastQueryInfo::DoWindComponentFix`:
  reprojecting `kFmiWindUMS`/`kFmiWindVMS` onto another grid, it turns them
  by the difference of the two areas' `NFmiArea::TrueNorthAzimuth`
  (`GridValues`/`PressureValues`/`HeightValues` with `relative_uv`).
  `gribtoqd` copies GRIB components without reading their flag. On a
  lat/lon area grid-relative is earth-relative: speed and direction. On
  LCC, stereographic and rotated lat/lon areas (`GridAxes::Rotated`) speed
  only, until `ds_core::wind` can turn components to true north.
- Config: `wms_parameter`, `poll_interval_secs` (default 30), `max_runs`
  (default 4; set 1 for latest-only).
