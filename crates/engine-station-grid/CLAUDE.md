# engine-station-grid crate — Claude Instructions

A derived collection (`engine_type = "station-grid"`, #1082): one time slice
of a station collection's observations interpolated on the fly onto a
regular CRS84 analysis grid, served through WMS, Maps, Tiles and EDR. Read
the root `CLAUDE.md` first — Critical Rules 5–7 and 9–10 apply.

## Architecture: a DERIVED collection

- The engine wraps the `Arc<dyn EdrEngine>` of its **source**
  (`[collections.station_grid] source = "<id>"`), a CSV, BUFR or PostGIS
  stations collection (`EngineHandle::station_source` in `server/src/
  admin.rs`, whatever APIs the source lists). The server wires it in a
  third pass, after the nowcast pass; it never grids a derived collection.
  It is reused across a reload only when its config is unchanged AND its
  source is reused (a CSV source never is), because it holds the source's
  `Arc`.
- A slice is ONE EDR area query over the grid's search box
  (`GridGeom::query_bbox`: the grid widened by the search radius, across the
  seam or to every longitude at a pole) with the window `[t - max_age, t]`;
  each station's latest finite value in it is used (`source.rs`). A PostGIS
  source has already turned the values its parameters' `valid_range` and
  `missing_values` reject into nulls (#1210), so a missing-value code never
  reaches a slice; do not filter again here. That is
  the set-based "every station at one time" read (#1062). CSV caps an area
  query at 500 stations — a demo source; BUFR and PostGIS have no station
  ceiling, only their value budgets.
- Analysis times: every `cadence`, or without one the source's
  `EdrEngine::observation_instants` (#1206): CSV and BUFR list every
  distinct instant from their in-memory snapshots, off-hour reports
  included; PostGIS lists none, so it needs a `cadence` (load error). The
  hook is an O(1) `Arc` read by the station grid alone. Never switch it to
  `get_available_times`: the API layers publish that as the source's
  temporal `values`, which would change the CSV/BUFR collection JSON.
- `geometry`, `interpolate` and `kdtree` are dependency-free pure functions —
  keep them that way. `compute` owns the cache, budget and pool; `engine`
  owns the axis, the version, the poll loop and both traits.

## Methods (interpolate.rs)

- Distances are great-circle on `EARTH_RADIUS_M`, from a 3-D k-d tree over
  unit vectors: chord order is great-circle order, so the antimeridian and
  the poles need no special case. `chord_to_m` is the haversine distance
  (pinned against `ds_core::geo::great_circle_distance_m`).
- IDW `d^-p`; ADW `exp(-d/x0)^m × (1 + a_k)`, the angular term computed from
  tangent-plane directions at the target (defined at the pole); nearest.
  Weights are scaled by the nearest station's, which leaves the estimate
  unchanged and keeps ADW from underflowing. A station within 1 mm of the
  target is its value. Ties in the neighbour search go to the lower index;
  the slice is sorted by position first, so the source's order never
  matters.
- `tests/reference/idw_adw_reference.py` is an independent stdlib
  implementation (haversine, compass bearings) over `testdata/weather.csv`;
  `tests/integration.rs` pins its output to 1e-4 °C (the grid is `f32`).
  Re-run it when changing a method and update the pinned values only with a
  reason.

## Cache-correctness contracts (do not weaken)

- `get_raster_tile` and `resolve_time` both snap with `select_time`
  (latest-not-after, #507). No analysis time ⇒ an empty tile AND
  `resolve_time` = `None`, so no no-TTL cache keeps it.
- **content_version** moves when an analysis's input fingerprint differs
  from the one recorded for that (parameter, time): every computation
  records it (`Inner::observe`), and the poll re-reads the latest
  `precompute` times each cycle. The version is collection-wide (the trait
  has one value): a revision re-keys every time's map tiles. It starts at
  1, never 0, so explicit-`TIME` responses revalidate. The grid cache keys
  on the time's own revision instead, so only the revised time's grid is
  computed again and the other tiles re-render from cached grids.
- **Older times** (#1206, `engine/recheck.rs`): after the pre-compute, each
  poll re-reads at most `RECHECK_PER_CYCLE` (8) times older than the
  `precompute` window whose grid the cache holds, newest first and
  round-robin across cycles (Critical Rule 9: a cap per cycle, never one
  read per cached grid). A revision moves the time's revision and the
  version through `observe` and computes the new grid ahead (background
  admission); an unchanged one touches nothing. Selection uses
  `contains_key`, never a get, so a re-check never keeps a grid warm. With
  K such grids a revision is found within ⌈K / 8⌉ polls. No request reads
  the source to look for a revision. An older time whose grid left the
  cache is not re-checked: its next request reads the source, but map tiles
  rendered from it stay in the no-TTL caches until the version next moves.
- Grid cache key: (engine epoch, parameter, time, revision). The
  epoch is unique per engine construction and stands for the source and the
  method config; `shutdown` evicts its grids. A grid lives under the
  revision of the input it was computed from (`compute::store`): a
  computation that finds a revision itself fills its claimed key's waiters,
  then moves the entry to the new revision, so the next request hits
  (#1206). Never key a stored grid on the revision read before the compute.

## Execution (compute.rs) — never on a request worker

- Request misses: claim the grid's fill guard (single-flight), reserve its
  bytes (`MC_STATION_GRID_MEMORY_MB`, fail-fast `ResourceExhausted`), queue
  the job on the station-grid pool (its own multi-thread runtime,
  `MC_STATION_GRID_WORKERS` permits, job inside `block_in_place` so a
  PostGIS source's sync bridge finds a runtime), and wait until the request
  deadline. Not ready ⇒ `ResourceExhausted` (503, `Retry-After`); the job
  keeps running and fills the cache for the retry. Never a partial grid.
- An EDR query queues every grid it needs before waiting for the first
  (`Inner::grids`), at most `MAX_QUERY_GRIDS` (96) analyses.
- The poll (background runtime, `block_in_place`) reads the source itself
  and hands the slice to a pool job with BACKGROUND admission (half the
  budget stays for requests). It waits for each job: pre-compute is
  sequential per collection.
- Interpolation is one pass per grid on one pool thread — never per output
  pixel, never a per-request rayon fan-out. Maps sample the grid
  nearest-cell: separable axes for CRS84/Web Mercator (one `project_node`
  per row and column), `ProjectionGrid` for projected output
  (`build_2d_periodic` with the column period for a global grid, else
  `build_2d_within` over the footprint window with an unwrapped envelope).
- Load-time limits: ≤ 16 777 216 cells; one grid must fit the cache and
  one computation half the budget, else the collection fails at load
  (otherwise every tile would recompute it, or no request could).

## EDR

- Every query samples the cached grid the maps draw, at the cell under the
  point (documented in the api-edr README): position and GetFeatureInfo
  answer the value the map shows. Values are the stored `f32`'s shortest
  decimal (`value()`), still the same number.
- No `datetime` = the latest analysis time; an instant must be an analysis
  time (404 otherwise); an interval = every analysis time in it.
- Area: `QueryPolygon::sample_grid` at the analysis resolution (≤ 256/axis),
  masked; cube: native cells in the bbox ordered west to east from the
  bbox's west edge (across the seam), or `resolution-x/-y` positions; both
  `[t, y, x]` with the `t` axis always present. No `z`.

## Metrics

`station_grid_stations{collection,parameter}` (the stations behind the
latest analysis — the first thing to check), `station_grid_analyses_total`,
`station_grid_failures_total`, `station_grid_last_analysis_ms`, the
`station_grid_cache_*` family and `station_grid_compute_*` admission; panels
on the dashboard's Engines › Station grids tab. Keep them in step.

## Not in phase 1 (follow-up issue)

Lapse-rate correction with a DEM, a land/sea mask, leave-one-out residuals,
station exclusions and quality flags, projected analysis grids, a per-time
content version (a `MapEngine` trait change), kriging/natural neighbour,
parallel interpolation of one large grid across the pool.
