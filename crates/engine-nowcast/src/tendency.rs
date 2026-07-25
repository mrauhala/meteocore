//! Growth/decay tendency support (#546).
//!
//! HISTORY / GATE VERDICT (full trail on #546): three classical tendency
//! estimators were implemented and hindcast-gated in July 2026 —
//! scene-wide per-band profiles (drained whole stratiform fields),
//! class-conditioned per-band profiles (gated to zero: single-digit
//! samples per band × class on real cells), and per-cell EMA'd intensity
//! trends (lost at 35 dBZ on a convective FMI day; decaying-cell POD
//! 0.90 → 0.76 at +30 min). Cell intensity trends decorrelate in ~10 min
//! (consistent with published hail correlation timescales), so ANY
//! integration over nowcast leads overshoots. The rendered field therefore
//! stays pure advection; the measured per-cell trend ships as the
//! `intensity_trend_dbz_min` feature property instead, and rendered
//! growth/decay waits for the learned Lagrangian residual (V2.5, #541).
//! The dead estimator code was removed with the verdict — it lives in git
//! history (#547 / PR #548 rounds) if ever needed again.

/// e-folding damping (in source intervals) used when a per-cell tendency
/// is experimentally applied along leads: `delta(lead) = tendency × efold
/// × (1 − exp(−lead/efold))`. Retained for the `growth_decay` experiment
/// flag (default off — see the gate verdict above).
pub const EFOLD_INTERVALS: f32 = 3.0;
