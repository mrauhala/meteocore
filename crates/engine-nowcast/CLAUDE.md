# engine-nowcast crate — Claude Instructions

Radar nowcasting: motion-extrapolated frames from another collection's
raster data, served with TIME values in the future (epic #519). Read the
root `CLAUDE.md` first — Critical Rules 5–7 apply, and this engine is the
reason `MapEngine::resolve_reference_time` exists (#521).

## Architecture: a DERIVED collection

- The engine wraps an `Arc<dyn MapEngine>` of its **source** collection
  (`[collections.nowcast] source = "<id>"`). `server/src/admin.rs` wires it
  in a **second pass** after all base engines exist; nowcast-of-nowcast is
  rejected at load. The source must have at least one of wms/maps/tiles in
  its `apis` (that's the registry the lookup snapshots).
- Poll loop (background runtime, both boot and reload paths — #442 lesson)
  watches the source's `raster_info().times`; each new latest frame triggers
  a **generation**: fetch the last frames at the source's native cell count
  on a regular WGS84 grid, estimate motion, advect the analysis frame
  `horizon` far at `step` spacing (default: source cadence).
- The algorithm modules (`motion`, `advect`, `skill`) are dependency-free
  pure functions — keep them that way; the engine module owns all I/O and
  state. `examples/skill_spike.rs` is the phase-0 hindcast harness
  (extrapolation must beat persistence; see #520 for the gate results).

## Cache-correctness contracts (do not weaken)

- **Every generation is a model run** (`reference_time` = the source anchor
  frame). `RasterInfo.reference_times` is populated and
  `resolve_reference_time` is overridden — this is LOAD-BEARING: the no-TTL
  rendered/meta caches key the run axis on it, and a nowcast rewrites all
  future valid times every ~5 minutes (#521).
- `resolve_time`, `resolve_reference_time`, and `get_raster_tile` share
  `select_generation` + `select_time` (#507/#521 — one selection
  implementation, cannot drift).

## Rendering & storage

- Internal frames live on a **regular WGS84 grid** (source native cell
  count, halved to fit `max_pixels`, default 4 Mpx), so the output→source
  map is affine; Projected output goes through `ProjectionGrid` +
  `footprint_pixel_window` exactly like engine-zarr. Sampling is
  nearest-neighbour (raw values; discrete radar palettes + PNG8 survive).
- A `RasterValues::U8` source with a nodata byte stays raw bytes end to end
  (1 B/px; `advect_u8` moves bytes); anything else falls back to f32
  (4 B/px — the FMI S3 COG path lands here until #475-style typed paths).
- Motion is estimated on a coarsened grid sized so the physical search
  window (40 m/s × source interval) fits `TARGET_SEARCH_PX` (48 — keeps
  the FMI 500 m grid uncoarsened), then the field is scaled back —
  deliberate scale handling; do NOT rely on the pixel budget to do this
  implicitly.
- **Motion stabilization (#524 part 1)** — two mechanisms against the
  rubber-band animation artifact (single-pair block matching re-reads
  convective growth/decay as motion noise every generation):
  1. multi-pair estimation: every consecutive history pair contributes
     measurements (scaled to the last interval's unit), averaged per
     block, then ONE shared outlier/fill/smooth pass;
  2. temporal EMA with the previous generation's field
     (`Generation.field`, weights `EMA_ALPHA_MEASURED = 0.7` /
     `EMA_ALPHA_FILLED = 0.4`; auto-skipped when the block grid changes).
  Weakening either brings the between-generation oscillation back —
  verify with an animation loop, not stills.

## Memory sizing (retention multiplies!)

Resident bytes ≈ `max_pixels × (leads + 1) × max_generations × bytes/px`
(1 B/px for the U8 path, 4 B/px for the f32 fallback). At the defaults
(4 Mpx, 24 leads, 6 generations) that is ~600 MB per U8 collection and
~2.4 GB for an f32-fallback source — PVOL-max_files territory (#493).
Until phase 2 (#523) makes deep retention useful (EDR `/instances`; today
only an explicit `DIM_REFERENCE_TIME` pin reads old generations), set
`max_generations = 2` in production configs. A generation-thinning
follow-up (full frames only for the latest generation) is scoped in #523.

## Verification (V2.1, #542)

- `objects` module = dependency-free 2D cell segmentation (threshold
  contour, 8-connected, min-area), Hungarian centroid matching with a
  distance gate, growing/decaying classification by volume-proxy change —
  the Ritvanen et al. (GMD 2025) object framework. `skill_spike` prints the
  object CSI-by-lead table (overall + per class + centroid error) next to
  the pixel table; every v2 quality change gates on BOTH.
- Each generation scores the previous one's lead-1 prediction against the
  fresh analysis (pixel CSI at `min_echo`) and the persistence baseline —
  exported as `nowcast_lead1_csi_permille` /
  `nowcast_lead1_persistence_csi_permille`. A persistent gap collapse in
  prod = motion or data regression; check before blaming the client.

## Cell intelligence (V2.2, #544)

- `cells2d` tracks the analysis frame's 35 dBZ cells across generations
  (anisotropic km matching): TRT-lite severity (max dBZ 45/50/55 steps +
  area ≥ 50 km² — deliberately NOT VoxelGrid attributes while voxels are
  beta/unverified), velocity EMA, and the deviant-mover flag = sustained
  ≥5 m/s residual between the track and the ambient motion field (the
  estimator-disagreement right-mover detector). Served as Point features
  via `FeatureEngine` when the collection lists `features` in `apis`.
- **Cell-snapshot HISTORY (#548):** one snapshot per generation is
  retained, `CELL_HISTORY_SNAPSHOTS = 48` deep (~4 h at 5-min cadence,
  ~100 B/track). `?datetime=` on the items endpoint selects the NEWEST
  snapshot inside the interval — animating clients query the exact cell
  situation per rendered frame. No `datetime` ⇒ latest snapshot; instants
  before the retained range (or in the future — cells are analysis-only,
  never forecast) ⇒ 0 features. The collection's temporal extent
  advertises the retained span; by-id GET serves the latest snapshot's
  version of a track.
- Lightning join (flash counts/jump per track) is part 2 — needs a ds-core
  event-source trait + engine-postgis impl.

## Gotchas

- Advection is Lagrangian persistence: no growth/decay; 35+ dBZ convective
  cores lose skill beyond ~1 h (phase-4 territory). Inflow boundaries
  become nodata — never echo.
- A TIME-less request resolves to `times.last()` = the FURTHEST forecast
  (API-layer convention). Clients should send explicit TIME; /preview does.
- `min_echo` is in the source's physical display units (dBZ for radar
  composites). A unit-converted source (e.g. K) needs it overridden.
- Config: `horizon` (default PT2H), `step` (default source cadence),
  `history_frames` ≥ 2, `poll_interval_secs` (30), `max_generations` (6),
  `max_pixels` (4 M), `min_echo` (10.0).
