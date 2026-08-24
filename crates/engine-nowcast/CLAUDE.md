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
- **Lightning join (#549, part 2):** `[collections.nowcast]
  lightning_source = "<id>"` names an events-shape engine-postgis
  collection in the same config (wired second-pass via the
  `ds_core::events::EventSource` trait — engine-nowcast never depends on
  engine-postgis; a missing/non-events id FAILS the collection at load).
  Each generation makes ONE bounded window fetch `(prev anchor, anchor]`
  on the poll runtime (the postgis sync bridge is legal there and only
  there), bins strikes onto cells via the label map (unlabeled strikes
  fall back to the nearest centroid within `LIGHTNING_JOIN_RADIUS_KM`),
  and updates per-track flash stats + the Schultz-style 2σ jump flag
  (`MIN_JUMP_RATE_PER_MIN` floor — revisit against Nordic storm data if
  jumps never fire). Feature properties `flash_count` /
  `flash_rate_per_min` / `lightning_jump` exist ONLY when a source is
  wired; null means "join skipped this generation" (source error — the
  generation itself never fails), 0 means measured-quiet.

## Lightning metrics (#616)

- **`jump_sigma` replaces a bare boolean as the scoring input.** The 2σ test
  already computed the magnitude and discarded it; a 5σ surge and a 2.1σ
  nudge both set `lightning_jump` but are not the same fact. The bool stays,
  now DERIVED from the magnitude so the two cannot disagree.
- `jump_sigma` is `None` until there are ≥2 generations of history — **not
  0.0**, which would claim "measured, no anomaly". A jump with no baseline
  still scores (it happened; it just can't be graded).
- A perfectly flat history has zero spread, making the true sigma infinite.
  Clamped to `JUMP_SIGMA_FLAT_HISTORY` so it stays a renderable number.
- `flash_density_per_km2` normalizes for cell size; guarded against a
  degenerate zero-area cell producing `inf`.
- `first_flash` is the track's FIRST ever, carried across generations, never
  overwritten by a later one — electrification age, not "most recent".
- Scoring ramps the jump between `JUMP_SIGMA_FLOOR` (2.0, the test
  threshold) and `JUMP_SIGMA_CEILING` (6.0); beyond that the difference
  shouldn't decide a ranking.
### IC/CG split and polarity (part 2)

- `EventPoint.attrs` (`ds_core::events::EventAttrs`) carries the per-event
  scalars. **Flat and `Copy`** — up to `MAX_JOIN_STRIKES` (200k) events cross
  this seam per generation, so a map or `Vec` per event would allocate 200k
  times per cycle on the poll runtime.
- The columns are **opt-in per source**: `[postgis.events]`
  `cloud_indicator_col` / `peak_current_col`. A network that doesn't report
  them declares nothing and nothing is selected. Do NOT add a column here
  without a consumer — `multiplicity_col` was wired end-to-end in the first
  draft of #618 and read by nothing, paying SQL and decode cost per strike
  for a config knob that silently did nothing.
- **A three-way distinction, not two.** `cg_count`/`ic_count`/
  `positive_cg_fraction` are absent when no source is wired, `null` when the
  source reports no discriminator, and a number when measured. "This network
  doesn't say" and "no CG flashes" are different facts and only one of them
  licenses a statement.
- **Presence flags must be exactly as fine-grained as the fact they gate.**
  `Tallies` carries `saw_split` and `saw_polarity` PER TRACK. Review on #618
  found the same mistake twice at two granularities:
  - One flag for both facts made a split-only network report
    `positive_cg_fraction: 0.0` for every CG-producing cell — "we checked and
    found no positive flashes" about a question it never asked.
  - A generation-global flag let a cell whose OWN strikes were all
    unclassified report `Some(0)` because some OTHER cell's strikes were
    classified. Degraded detections cluster by cell, so this is not exotic.
- **`positive_cg_fraction`'s denominator is `cg_polarity_known`, not
  `cg_count`.** Peak-current estimation fails on weak signals, so a network
  can classify only part of its CG population; dividing 4 positives by all 10
  CG flashes reports 0.4 where the measured share is 0.8. The denominator is
  served as its own property so the sample size behind the share is visible —
  "3 of 4" and "300 of 400" are the same fraction and not the same evidence.
- **`positive_cg_fraction` is `None` when `cg_count == 0`**, never 0.0 — 0/0
  is not 0%. A cell with only IC flashes has no CG polarity to report.
- Polarity comes from the SIGN of `peak_current`, not its magnitude. A zero
  current yields `None` (no polarity) rather than "positive".
- `positive_cg` is weighted at 0.6 and ramps `POSITIVE_CG_FLOOR` 0.05 →
  `POSITIVE_CG_CEILING` 0.5: a few +CG flashes are normal background, and a
  CG population half positive is already the severe signature, so the term
  saturates there rather than reserving its top half for shares that
  essentially never occur. Test the RAMP, not just the ordering — a relative
  `assert!(a > b)` passes with no ramp at all, which is how the missing one
  reached review.

## Fact sheets + significance ranking

- **`ds_core::cell_facts::CellFactSheet` is the one description of a cell.**
  `score_cells()` builds it ONCE per generation from `CellTrack` + grid
  geometry and stores it (with its score) in `CellSnapshot.cells` as
  `ScoredCell`. Four consumers read it — served feature properties, the
  ranking, any future narrative, and the #541 V2.4 learned-model feature row
  — so all four see identical numbers by construction. Do NOT reconstruct
  cell attributes at request time; that is how two of them drift.
- Rounding to meaningful precision happens in `score_cells`, not in
  `cell_feature`: the working grid is ~500 m, so 5 lon/lat decimals ≈ 1 m,
  and raw f64s roughly double the GeoJSON payload to carry noise.
- **Ranking is `ds_core::significance`** (domain-agnostic: it sees normalized
  `Term`s, never a storm cell). Weights come from
  `cell_facts::DEFAULT_CELL_WEIGHTS`, overridable per collection via
  `[nowcast.significance]`; an unknown term name FAILS the collection at
  load rather than silently keeping a default nobody chose.
- Served as `significance` (0..=1), `significance_rank` (1-based within the
  snapshot) and `significance_reasons` (top 3 contributing terms). The
  reasons field is load-bearing: a weight table with no ground truth has to
  be arguable to be tunable.
- **Absent terms renormalize.** A cell with no volume/impact/lightning data
  simply omits those terms. That is why wiring a new source later needs no
  config flag day — but it also means `measured-quiet` ranks BELOW
  `unknown`, which is correct (measured zero is information) and worth
  remembering when a newly wired source appears to demote everything.
- Term normalization ceilings live in `cell_facts` (`DBZ_CEILING` 60,
  `VIL_CEILING_KG_M2` 50, …). Volume-derived terms are scaled by
  `beam_coverage`, so a far-range cell cannot ride an inflated VIL to the
  top of the list.
- **Sortable via `?sortby=`** (#605). Base set: `significance`,
  `significance_rank`, `max_dbz`, `area_km2`, `track_age`, `speed_ms`,
  `bearing_deg`, `intensity_trend_dbz_min`. `flash_count` /
  `flash_rate_per_min` are added only with `lightning_source` wired, and
  `impact_eta_minutes` only with `impact_source` — **`sortables()` is
  instance-dependent**. A property absent from every feature sorts to a
  no-op (`sort_features` sees it missing everywhere and falls through to
  the id tie-break), so advertising an unwired one would answer 200 in id
  order — the silently-ignored-parameter failure the whole surface exists
  to remove. Resolved once by `recompute_sortables()` whenever a source is
  wired and stored on the engine, so the accessor is a borrow and there is
  one list to maintain rather than one per source combination.
## Clutter mitigation (#614)

- A **persistent, near-stationary** echo is flagged `likely_clutter` and
  demoted by a negative `clutter` term. Reported from production: wind
  turbine clutter near Oulu ranked #1 on a quiet day, because every
  significance term measures intensity, size, trend or impact and **none
  asks whether the echo is meteorological**. Clutter is bright, compact,
  persistent and usually over a town, so it scores well on nearly
  everything.
- Thresholds in `ds_core::cell_facts`: speed < `CLUTTER_MAX_SPEED_MS` (3.0,
  matching `DEVIANT_MIN_CELL_SPEED_MS`) **and** age ≥ `CLUTTER_MIN_AGE` (6
  frames ≈ 30 min). Both conditions matter — a real cell can crawl briefly
  in weak flow, but one holding position *and* high reflectivity for half an
  hour is a fixed object.
- **A newborn track is never flagged.** `speed_ms` is `None` until the
  second observation; reading that as "stationary" would flag every cell for
  the first frames after a reload.
- **Demoted, never dropped.** The flag is a served property, so a client can
  say "persistent stationary echo, probably a wind farm" instead of either
  "severe storm" or nothing. Excluding would let a false positive delete
  real weather with no trace — the opposite of the absent/null/value
  discipline everywhere else here.
- **Mitigation, not detection.** Wind turbine clutter is a hard upstream QC
  problem; this only stops a fixed echo dominating a ranking. A known-site
  clutter mask (option B in #614) is the precise complement and is not
  built.

- `severity` is deliberately NOT sortable — as a string it orders
  `moderate < severe < very_severe < weak`, which looks almost right and
  buries the weakest cells at the end; use `significance`, which already
  incorporates it.
- Sorting happens in `get_features` **before** the offset/limit slice, via
  `ds_core::feature::sort_features`. Do not reorder those two steps.
- NOT yet wired: a `min_significance` filter, and the `volume` /
  `environment` fact groups.

## Impact context (`impact.rs`)

- `[nowcast] impact_source = "<id>"` names any polygon **Features**
  collection (municipalities, catchments, service regions). Wired
  second-pass in `admin.rs`; a missing id or one not wired to the Features
  API FAILS the collection at load.
- **A geojson impact source makes its nowcast rebuild on every reload.**
  `csv`/`geojson` are always rebuilt (reload is the only way they re-read a
  changed file), and `reusable_collections` requires every second-pass
  dependency to be reused — so the typical `municipalities` source costs a
  nowcast re-bootstrap per reload. Correct, but not free; use a postgis
  areas collection if that matters.
- Unlike `lightning_source` (postgis only, all built in the first pass),
  impact sources are resolved against a **snapshot of `feature_engines`
  taken before the nowcast pass** (`base_feature_engines`). Nowcast engines
  are themselves `FeatureEngine`s and get inserted as that loop runs, so
  resolving against the live map would make `impact_source = "<another
  nowcast>"` succeed or fail purely on config declaration order. Pointing at
  a nowcast collection now fails deterministically, with a message that says
  why.
- `impact_name_property` (default `"name"`) is the display name;
  `impact_weight_property` is an optional numeric property (population,
  households, insured value) that log-weights exposure. Without it, scoring
  is purely geometric — honest, but it cannot rank a city above a village.
- **ONE bounded `get_features` call per generation**, then all
  point-in-polygon locally — never one call per cell (an impact source may be
  a sync bridge over a database; the `EventSource` contract, same reasoning).
  A source error degrades to "no impact context this generation", never a
  failed generation.
- The fetch bbox is the working grid **padded by `pad_for_lookahead`** —
  `MAX_CELL_SPEED_MS × LOOKAHEAD_MIN` (~126 km), longitude widened by
  latitude and capped at 20°. Without the pad, a cell at the composite edge
  moving outward — which is what coastal and border radars produce
  constantly — reports `approaching: null` for a real imminent arrival,
  which is worse than reporting nothing because it looks like an answer.
- A malformed grid bbox **fails the join**, it does not fall back to an
  unfiltered query. Dropping the filter would pull the source's whole
  catalog every generation, silently.
- Arrival is decided on the source feature **id, not the display name**.
  Names are not unique in every plausible source (service areas, postal
  regions), and a name comparison would suppress a genuine transition
  between two same-named polygons.
- Resolution: `over` = the area under the centroid; `approaching` = the
  first *different* area along the motion vector within `LOOKAHEAD_MIN`
  (60 min, sampled every `STEP_MIN` = 2 min — 30 probes per cell, bbox
  prefiltered). A track with no velocity gets `over` only; inventing an ETA
  from no velocity is worse than no ETA.
- Exposure: `over` ⇒ the area's weight; `approaching` ⇒ weight × linear ETA
  decay × `APPROACHING_FACTOR` (0.7). Weight is log-scaled between
  `WEIGHT_FLOOR` (100) and `WEIGHT_REFERENCE` (700 000 ≈ a capital) —
  **linear population weighting would collapse everything but the capital
  to ~0**, which is why the log ramp is load-bearing rather than cosmetic.
- A feature missing the weight property falls back to weight 1.0, NOT 0.0 —
  treating a missing value as zero would silently erase that area from
  every ranking.
- Feature properties `impact_over` / `impact_approaching` /
  `impact_eta_minutes` exist ONLY when a source is wired (same tri-state
  discipline as the flash properties); `null` inside the group means
  "nothing there".
- `testdata/municipalities.geojson` carries a `population` property joined
  from Statistics Finland (`vaestoalue:kunta_vaki2025`, 308 municipalities,
  2025 figures) — that's what makes the example config's weighting real.

## Gotchas

- `growth_decay = true` (experimental, default OFF — gate verdict on #546
  says it stays off) adds a SECOND full-grid `sample_u8` per lead (the
  advected label map, in both the U8 and f32 arms) — roughly 2× per-lead
  sampling cost, against the grain of #528's linear-cheap-leads work.
  Budget for it before ever flipping the flag on.
- Advection is Lagrangian persistence: no growth/decay; 35+ dBZ convective
  cores lose skill beyond ~1 h (phase-4 territory). Inflow boundaries
  become nodata — never echo.
- A TIME-less request resolves to `times.last()` = the FURTHEST forecast
  (API-layer convention). Clients should send explicit TIME; /preview does.
- `min_echo` is in the source's physical display units (dBZ for radar
  composites). A unit-converted source (e.g. K) needs it overridden.
- Config: `horizon` (default PT2H), `step` (default source cadence),
  `history_frames` ≥ 2, `poll_interval_secs` (30), `max_generations` (6),
  `max_pixels` (4 M), `min_echo` (10.0), `[nowcast.significance]` weight
  overrides (all optional; unknown names rejected), `impact_source` +
  `impact_name_property` (default `"name"`) + `impact_weight_property`.
