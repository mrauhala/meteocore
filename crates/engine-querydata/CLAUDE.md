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
- **Grids:** WGS84, Stereographic, Rotated Lat-Lon.
- EDR position queries use bilinear interpolation; map rendering uses
  nearest-neighbour.
- Missing-value sentinel: `32700.0`.
- Config: `wms_parameter`, `poll_interval_secs` (default 30), `max_runs`
  (default 4; set 1 for latest-only).
