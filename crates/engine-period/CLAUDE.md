# engine-period crate — Claude Instructions

Rolling-window statistics over another collection's frames (#1379), radar
rain accumulation first. Read the root `CLAUDE.md` first — Critical Rules
5–7, 9 and 10 apply. The README holds the semantics a client sees.

## Architecture: a DERIVED collection

- The engine wraps the `Arc<dyn MapEngine>` of its **source**
  (`[collections.period] source = "<id>"`): `EngineHandle::period_source`
  in `server/src/admin.rs`, a non-derived raster (`raster_source`) or a
  station grid, whatever APIs it lists. The server wires it in a FOURTH
  pass, after the station grids. A nowcast (future frames) or another
  period collection is refused; a source with model runs is refused by
  `PeriodEngine::new` and, should runs appear later, by every poll.
- Reused across a reload only when its config is unchanged AND its source
  is reused (`admin::reusable_collections`): it holds the source's `Arc`
  and the windows summed from it.
- `zr`, `plan`, `accum` and `geometry` are dependency-free pure functions —
  keep them that way. `rolling` is the poll-side bookkeeping (no I/O of its
  own: it is handed the frame times and a read function); `engine` owns
  config validation, the poll loop, the snapshot, `MapEngine`, `EdrEngine`.

## The memory rule (do not weaken)

- Per window ONE `Grids`: `f32` accumulator + `u16` valid-frame count,
  6 B/cell. No per-frame grid is ever resident. A re-sum or rebuild holds a
  second `Grids` for its window; at most ONE re-sum of a live window runs
  at a time (`resumming` in `Roller::poll`), so the peak is windows + 1.
  A new or broken window's rebuild replaces its live grids, so it does not
  count against that.
- A leaving frame is READ AGAIN from the source and subtracted with the
  same contribution, bit for bit (`Grids::apply`, `Op::Remove`). Removing
  needs exactly the values added: `read` refuses a frame unless the source
  still resolves exactly that time before AND after the read — a source
  snaps a missing time to another frame (latest-not-after), whose values
  must never be subtracted as this one's.
- **Strips (maintainer's decision).** A frame is NEVER read whole: in
  strips of whole working-grid rows, at most `read_strip_cells` cells
  (default 4 Mi, at least one row; `rolling::Strips`), each strip's box on
  the grid's own row edges (`GridGeom::strip_bbox`, the grid's north and
  south exactly at its ends). Each strip is folded into every live window
  and rebuild that takes the frame and dropped: a poll holds ONE strip of
  ONE frame, about 84 MB at the default even from a source that boxes
  `f64` (every GeoTIFF read from object storage does: `read_bbox_u8` is
  local-only). Never collect a frame's strips into a whole frame, and
  never read a strip for one window and again for another: live updates
  and rebuilds share each strip read.
- Frames are folded in time order, each frame's strips top to bottom, so a
  cell gets the same operations in the same order whatever the strip size:
  strip-wise grids equal whole-frame grids bit for bit
  (`strips_fold_bit_for_bit_like_whole_frames`, and on the real fixture
  `strips_of_the_real_source_equal_the_whole_frame_read`, native and
  overview). Keep that order.
- A frame failing part way through its strips breaks every live window it
  reached and restarts the rebuilds that took part of it (part of a frame
  cannot be taken out again); a frame failing at its first strip is just
  left out (an add) or breaks the windows that must take it out.
- A cell's count reaching 0 resets its accumulator to exactly 0, so no
  rounding remainder of frames long gone survives.
- **Resolution (maintainer's decision): native when it fits.** The working
  grid is the source's own cell counts while they fit `max_pixels`
  (default 40 M, cap `geometry::MAX_CELLS` = 64 M, the GeoTIFF engine's
  full-resolution read cap), so a 36 M-cell composite (4963 × 7316, 218 MB
  per window) stays native. Past it, `GridGeom::over` halves the longer
  axis, and frames are read as the source renders that size (a GeoTIFF's
  overview) — keep that, never resample frames here. `log_grid` logs the
  grid and its bytes at load (and at the first poll when the source had no
  grid then); `period_grid_bytes` feeds the memory plan's store families.
  The sizing table lives in the README; keep it in step with any change to
  the per-cell cost or the defaults.
- **Latest end only (v1, maintainer's decision).** One grid per window, so
  a window serves its end at the source's latest frame and nothing else:
  earlier ends are neither kept nor recomputed. Do not "fix" an animating
  client by keeping a ring of past grids (one grid each) or by summing on a
  request (reads frames on a request worker, Critical Rule 7); computing
  past ends on demand is a follow-up with its own design.

## Poll semantics (`rolling.rs`)

- Window ending at the source's latest frame `T`: frames in `[T − L, T)`,
  each for `min(next − t, cadence)` seconds (`plan::target`). The newest
  frame enters when its successor fixes its duration — the maintainer's
  "spacing to the next frame".
- A live window is brought to its target in ONE poll or it BREAKS (grids
  dropped, unserved, rebuilt): a frame to take out that left the source or
  fails to read, or more changes than `max_reads_per_cycle`. Its update is
  applied strip by strip, each strip under the window's write lock, so a
  request missing the cache mid-poll can draw a partly updated window — see
  the in-flux version below for why that is never cached under a live key.
- A frame that left the source while in a window sets `needs_margin`: that
  window is then built only while the source holds the window plus one
  frame (`plan::holds`), so a source retaining exactly one window does not
  trigger a rebuild every poll. It breaks once and says what it needs.
- Failed reads: the frame is left out of every target for
  `FAILED_RETRY_POLLS` polls (coverage shows it), then read again.
- Re-sum after `RESUM_AFTER_LENGTHS` (4) window lengths of grid operations,
  or when the source's `content_version` moves (frames revised in place).
  A drift-only swap keeps the version; any other swap moves it.

## Cache-correctness contracts

- `get_raster_tile`, `resolve_time` and `resolve_parameter_time` select
  through `engine::select_time` over the parameter's window axis (`[end]`
  or empty). Empty ⇒ `None` and an empty tile, so nothing is cached under a
  requested time (#507).
- Windows have their own axes (`parameter_times`), so `get_raster_tiles` is
  overridden: every band at exactly the given time, else an error.
- `content_version_for(parameter)` is the parameter's window's SERVED
  version, one atomic load (`rolling::Versions`); it moves whenever that
  window's grids or ledger change (new frame, late frame, retime, break,
  rebuild swap). `content_version` is the highest handed out. Versions only
  grow, come from one counter, survive a grid-geometry reset (the engine
  owns `Versions`, the new `Roller` takes a fresh one) and are never 0.
- **In-flux versions (no cache poisoning mid-fold).** A fold changes a
  window in place over a whole poll, strip by strip, while requests render.
  Before the FIRST strip a poll applies to a window, before its grids are
  dropped (break, grid reset), and before a content-changing rebuild
  replaces them, the window moves to a fresh in-flux version
  (`Versions::flux`). A render that misses the cache mid-fold is keyed
  (end, in-flux version): the pre-fold key's entries are never written with
  a partial image. After the poll, the engine stores the snapshot and THEN
  sets each window's next version (`publish`), another fresh value; no
  request asks for the in-flux one again. Keep this order: setting the
  version before the snapshot would key new grids with the old snapshot's
  coverage (a late frame keeps the end) under the live key. Every flux is
  followed by a bump. A drift-only re-sum swap keeps the version (the
  values differ by rounding). Pinned by
  `a_render_mid_fold_never_lands_under_a_live_key`, which fails without
  the flux.

## Tests

- `tests/rolling.rs`: a synthetic source (closed-form frames, raw-byte and
  `f32`), every product against a direct `f64` computation; 300 rolling
  steps against a fresh engine (≤ 1e-6 of the field) with the reads per
  step and the frame bytes held measured; coverage gating; late frames and
  versions; retention; unreadable frames; EDR = map; bad configs; memory.
- `tests/integration.rs`: the committed `testdata/radar` frames through the
  GeoTIFF engine. At the default `max_pixels` the grid is native
  (3249 × 1750) and the wettest cell equals the Marshall–Palmer sum of the
  source's own EDR position series there. On the native and a coarse grid,
  the whole field equals the frames read from the source's map path at the
  same size. On a coarse grid compare with that map path, never the
  position query: it reads a GeoTIFF's overview (smoke test on the
  fixture: 4.246 mm at a cell whose native pixel sums to 4.118 mm).
- `zr.rs` pins Marshall–Palmer against the published table values.

## Not in v1 (#1379 follow-ups)

Past window ends computed on demand; `max` / `min` (a rolling extreme
cannot take a leaving frame out of one grid); windows over a forecast run's
valid times; clock-aligned windows (00–24 UTC).
