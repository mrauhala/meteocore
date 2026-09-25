# engine-querydata crate — Claude Instructions

FMI QueryData (`.sqd`) binary-format engine. Memory-mapped file access via
`memmap2`. Read the root `CLAUDE.md` first (poll loop on the background
runtime).

- **Multi-parameter:** exposes all parameters from the file; `wms_parameter`
  config (name / short name / ID) selects which to render.
- **Model runs (#337):** polls the directory and retains the most recent
  `max_runs` `.sqd` files as model runs, keyed by each file's **origin
  (analysis) time** (`RunSet: BTreeMap<DateTime<Utc>, _>`), atomically
  swapped via `ArcSwap`. Already-loaded files are reused on poll (not
  re-parsed). Each run is an EDR instance / `RasterInfo.reference_times`
  entry; the latest run is the default for un-pinned queries. Implements the
  shared `ds_core::instances` contract (root CLAUDE.md).
- **Grids:** WGS84, Rotated Lat-Lon, Stereographic, Lambert Conformal Conic
  (MEPS: tangent cone, `lat1 == lat2`). `GridInfo::new` derives the
  GeoTransform once; per-pixel code reads it, never re-projects corners.
- **Corner gotchas (`GridInfo::new`):** the stored corners are grid-point
  *centres* (spacing = span / (n − 1)). A lat/lon area starts at its first
  stored corner even when that is the north edge (ECMWF Kenya). A projected
  area starts at the south-west corner of its projected rectangle whichever
  two corners it stores — MEPS stores the NW and SE ones — so the projected
  corners are min/max-normalised. Both orientations are pinned against the
  fixtures' geography (`grid_lonlat_corners`, `meps_rows_run_south_to_north`);
  a corner-coordinate test alone cannot catch a flipped row order.
- The LCC `radius` line (a sphere, e.g. 6371220 m) is ignored: projection is
  on WGS84 through the stored corners, so corners are exact and the interior
  differs slightly from the producer's sphere grid.
- EDR position queries and map rendering use bilinear interpolation. EDR area (and radius via the shared default) returns a
  CRS84 `Grid` over the polygon bbox at native resolution (≤ 256 cells per
  axis, 1M-value budget across time × cells × parameters), every cell
  bilinearly interpolated and cells outside the polygon masked to null
  (`QueryPolygon::sample_grid` in ds-core, #671). One `t` axis when the
  datetime window selects several steps.
- Missing-value sentinel: `32700.0`.
- Config: `wms_parameter`, `poll_interval_secs` (default 30), `max_runs`
  (default 4; set 1 for latest-only).
