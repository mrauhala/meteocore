//! Cell intelligence (#544, v2.2 part 1): tracked 2D composite cells with a
//! TRT-lite severity rank and a deviant-mover flag.
//!
//! Built on the VERIFIED 2D objects machinery (#543) — deliberately NOT on
//! the 3D `VoxelGrid` attributes (beta, non-verified, slow as of 2026-07);
//! echo-top/VIL enrichment of the rank waits for verified voxels.
//!
//! The deviant-mover detector is the estimator-disagreement idea: the block
//! motion field measures the ambient echo flow, the cell track measures the
//! storm's own displacement — a sustained residual between them marks
//! deviant movers (right-movers etc.), the storms pure advection misplaces
//! first.

use chrono::{DateTime, Utc};
use ds_core::events::EventAttrs;

use crate::motion::MotionField;
use crate::objects::{match_cells, CellBlob, PixelScale};

/// Severity lives in ds-core so the tracker, the significance ranking and the
/// narrative cannot drift to different meanings of "severe". Re-exported here
/// because this module owns the heuristic that assigns it.
pub use ds_core::cell_facts::Severity;

/// Cell threshold (dBZ) for intelligence tracking — the Ritvanen-style
/// convective contour, matching the verification harness default.
pub const CELL_THRESHOLD_DBZ: f32 = 35.0;
/// Minimum component size in pixels. 10 px ≈ 2.5 km² on the FMI 500 m
/// grid — below that, 35 dBZ specks churn between generations and flood
/// the Features layer with unmatched one-generation "cells".
pub const CELL_MIN_AREA_PX: usize = 10;
/// Speed-based matching gates (like `ds_core::cells::track_cells`):
/// `gate_km = speed × elapsed + BASE_GATE_KM`, so the implied velocity of
/// ANY accepted match is physically bounded. The old flat 20 km / 5 min
/// gate allowed 66 m/s implied cell speeds — gate-edge mismatches read as
/// 200+ km/h storms on the client (observed live 2026-07-26).
///
/// Fastest real cell motion (extreme bow echoes) ≈ 35 m/s — the raw-
/// position gate uses [`MAX_CELL_SPEED_MS`]. After pass-1 motion
/// compensation the prediction has already absorbed advection, so only a
/// small residual is legitimate ([`COMPENSATED_RESIDUAL_SPEED_MS`]).
/// [`BASE_GATE_KM`] absorbs centroid jitter from footprint changes at
/// small elapsed times.
pub const MAX_CELL_SPEED_MS: f32 = 35.0;
pub const COMPENSATED_RESIDUAL_SPEED_MS: f32 = 10.0;
pub const BASE_GATE_KM: f32 = 3.0;
/// Residual speed (m/s) between cell track and ambient field that counts
/// as deviant, provided the cell itself moves faster than
/// [`DEVIANT_MIN_CELL_SPEED_MS`].
pub const DEVIANT_RESIDUAL_MS: f32 = 5.0;
pub const DEVIANT_MIN_CELL_SPEED_MS: f32 = 3.0;
/// Consecutive deviant generations before the flag is raised (single-scan
/// residuals are mostly track noise).
pub const DEVIANT_STREAK: u32 = 2;
/// Per-second clamp on a cell's measured intensity tendency (±2 dBZ per
/// 5-minute interval at the usual cadence).
pub const MAX_CELL_TENDENCY_PER_S: f32 = 2.0 / 300.0;
/// Lightning join (#549): a strike outside every cell footprint joins the
/// nearest cell centroid within this many km (anvil and adjacent flashes
/// belong to the storm even when they miss the 35 dBZ contour).
pub const LIGHTNING_JOIN_RADIUS_KM: f32 = 10.0;
/// Per-generation flash-rate history depth for the jump baseline
/// (6 generations ≈ 30 min at 5-min cadence — the scale of the Schultz
/// lightning-jump verification window).
pub const FLASH_HISTORY_LEN: usize = 6;
/// Absolute flash-rate floor (flashes/min) for the jump flag — the
/// published Schultz operational threshold. Suppresses 2σ triggers on
/// near-zero baselines, where a single extra strike is "2σ". Revisit
/// against live Nordic storm data if jumps never fire.
pub const MIN_JUMP_RATE_PER_MIN: f32 = 10.0;
/// Sigma reported when the baseline has zero spread and the rate rose.
///
/// A perfectly flat history makes the true value infinite; clamping keeps it
/// a number a client can render and compare, while still reading as extreme.
pub const JUMP_SIGMA_FLAT_HISTORY: f32 = 10.0;

/// Rank a cell from max intensity and area: one point per crossed max-dBZ
/// step (45/50/55) plus one for area ≥ 50 km² — 0 ⇒ weak … 3+ ⇒ very
/// severe. Deliberately simple and monotone; a tree-based model replaces
/// this in V2.4.
pub fn severity(blob: &CellBlob, area_km2: f64) -> Severity {
    let mut points = 0u32;
    for step in [45.0f32, 50.0, 55.0] {
        if blob.max_value >= step {
            points += 1;
        }
    }
    if area_km2 >= 50.0 {
        points += 1;
    }
    match points {
        0 => Severity::Weak,
        1 => Severity::Moderate,
        2 => Severity::Severe,
        _ => Severity::VerySevere,
    }
}

/// One tracked cell (current generation's snapshot plus track state).
#[derive(Debug, Clone)]
pub struct CellTrack {
    pub id: u64,
    pub blob: CellBlob,
    /// Generations this track has been observed in (1 = newborn).
    pub age: u32,
    /// EMA of the track's own velocity, km/SECOND in grid axes
    /// (+x east, +y south) — time-base free, so lead-step configs and
    /// skipped generations cannot skew it. `None` until the second
    /// observation.
    pub velocity_kms: Option<(f32, f32)>,
    /// Consecutive generations the track-vs-field residual exceeded the
    /// deviant gates.
    pub deviant_streak: u32,
    pub severity: Severity,
    /// Volume-proxy trend vs the previous observation: `Some(true)` =
    /// growing, `Some(false)` = decaying, `None` = newborn (#546 iter 1).
    pub growing: Option<bool>,
    /// EMA'd mean-intensity trend of THIS cell (physical units per second,
    /// clamped): the tracker-level growth/decay signal — immune to the
    /// per-pixel motion-residual contamination that poisoned field-level
    /// profiles. 0.0 until the second observation.
    pub intensity_tendency: f32,
    /// Lightning join (#549): strikes attributed to this cell over the
    /// last inter-generation window. `None` = no event source configured,
    /// or the join was skipped this generation (source error).
    pub flash_count: Option<u32>,
    /// The same window's strikes per minute.
    pub flash_rate_per_min: Option<f32>,
    /// Recent per-generation flash rates (ascending age, newest LAST,
    /// ≤ [`FLASH_HISTORY_LEN`]) — the jump detector's baseline, carried
    /// across generations by the track id.
    pub flash_history: Vec<f32>,
    /// Schultz-style 2σ lightning jump fired this generation.
    ///
    /// Derived from [`Self::jump_sigma`] so the two cannot disagree; kept as
    /// a field because existing consumers read it.
    pub lightning_jump: bool,
    /// How far above the baseline this generation's flash rate sits, in
    /// standard deviations of the recent history.
    ///
    /// The bool above answers "did it cross 2σ"; this answers "by how much",
    /// which a 4σ surge and a 2.1σ nudge do not share. `None` until there is
    /// enough history (≥ 2 generations) to have a baseline at all — not 0.0,
    /// which would read as "measured, no anomaly".
    pub jump_sigma: Option<f32>,
    /// Cloud-to-ground flashes this window, when the source reports the
    /// discriminator. `None` = not reported, distinct from zero measured.
    pub cg_count: Option<u32>,
    /// Intra-cloud flashes this window. Total lightning rises before CG in
    /// developing storms, so the split is an intensification cue a single
    /// count cannot express.
    pub ic_count: Option<u32>,
    /// Positive-polarity cloud-to-ground flashes. A high positive fraction is
    /// a well-established severe-storm signal.
    pub cg_positive_count: Option<u32>,
    /// When this track was first attributed a flash. Electrification age is
    /// context a raw count cannot carry: a cell producing its first flash
    /// now is a different situation from one that has been active an hour.
    pub first_flash: Option<DateTime<Utc>>,
}

impl CellTrack {
    /// Sustained deviant mover?
    pub fn deviant(&self) -> bool {
        self.deviant_streak >= DEVIANT_STREAK
    }

    /// Ground speed in m/s.
    pub fn speed_ms(&self) -> Option<f32> {
        self.velocity_kms
            .map(|(vx, vy)| (vx * vx + vy * vy).sqrt() * 1000.0)
    }

    /// Compass bearing (degrees, 0 = north, clockwise) the cell moves toward.
    pub fn bearing_deg(&self) -> Option<f64> {
        self.velocity_kms.map(|(vx, vy)| {
            // +y is SOUTH in grid coordinates.
            (f64::from(vx).atan2(f64::from(-vy)).to_degrees() + 360.0) % 360.0
        })
    }
}

/// Advance tracks by one generation: match previous tracks to newly
/// segmented blobs (anisotropic km gate), carry ids/age, update the
/// velocity EMA, and re-evaluate severity and the deviant streak against
/// the generation's motion `field` (vectors in px/interval).
///
/// `next_id` supplies ids for newborn tracks.
#[allow(clippy::too_many_arguments)]
pub fn advance_tracks(
    previous: &[CellTrack],
    blobs: Vec<CellBlob>,
    scale: PixelScale,
    field: &MotionField,
    // Wall-clock seconds the tracked DISPLACEMENT spans (previous
    // generation's anchor → this anchor — 2× cadence after a skipped
    // generation), distinct from the seconds one FIELD vector spans (the
    // source interval motion was estimated over).
    displacement_secs: f32,
    field_interval_secs: f32,
    mut next_id: impl FnMut() -> u64,
) -> Vec<CellTrack> {
    // Two-hypothesis matching: pass 1 uses TITAN-style motion-compensated
    // first guesses (fixes fast movers pairing with the wrong upstream
    // cell — the against-flow client bug, 2026-07-25); pass 2 rematches the
    // leftovers at their RAW positions, so a genuine counter-flow cell —
    // whose first guess is displaced the WRONG way and may leave the gate —
    // still finds its true successor instead of being dropped by the very
    // detector built to flag it.
    let ratio = displacement_secs.max(1.0) / field_interval_secs.max(1.0);
    let displaced: Vec<CellBlob> = previous
        .iter()
        .map(|t| {
            let mut b = t.blob.clone();
            let (fu, fv) = field.sample(b.centroid.0, b.centroid.1);
            b.centroid.0 += fu * ratio;
            b.centroid.1 += fv * ratio;
            b
        })
        .collect();
    let ds = displacement_secs.max(1.0);
    // Pass 1 gate: residual around the motion-compensated prediction.
    let gate_compensated = BASE_GATE_KM + COMPENSATED_RESIDUAL_SPEED_MS * ds / 1000.0;
    // Pass 2 gate: full physical speed bound around the raw position.
    let gate_raw = BASE_GATE_KM + MAX_CELL_SPEED_MS * ds / 1000.0;
    let mut matched_prev: Vec<Option<usize>> = vec![None; blobs.len()];
    let mut prev_taken = vec![false; previous.len()];
    for (pi, ci) in match_cells(&displaced, &blobs, scale, gate_compensated) {
        matched_prev[ci] = Some(pi);
        prev_taken[pi] = true;
    }
    {
        // Pass 2 on leftovers, raw positions.
        let free_prev: Vec<usize> = (0..previous.len()).filter(|&i| !prev_taken[i]).collect();
        let free_cur: Vec<usize> = (0..blobs.len())
            .filter(|&i| matched_prev[i].is_none())
            .collect();
        let prev_raw: Vec<CellBlob> = free_prev
            .iter()
            .map(|&i| previous[i].blob.clone())
            .collect();
        let cur_raw: Vec<CellBlob> = free_cur.iter().map(|&i| blobs[i].clone()).collect();
        for (a, b) in match_cells(&prev_raw, &cur_raw, scale, gate_raw) {
            matched_prev[free_cur[b]] = Some(free_prev[a]);
        }
    }

    blobs
        .into_iter()
        .zip(matched_prev)
        .map(|(blob, prev_idx)| {
            let area_km2 = blob.area as f64 * f64::from(scale.x) * f64::from(scale.y);
            let severity = severity(&blob, area_km2);
            match prev_idx {
                None => CellTrack {
                    id: next_id(),
                    blob,
                    age: 1,
                    velocity_kms: None,
                    deviant_streak: 0,
                    severity,
                    growing: None,
                    intensity_tendency: 0.0,
                    flash_count: None,
                    flash_rate_per_min: None,
                    flash_history: Vec::new(),
                    lightning_jump: false,
                    jump_sigma: None,
                    cg_count: None,
                    ic_count: None,
                    cg_positive_count: None,
                    first_flash: None,
                },
                Some(pi) => {
                    let prev = &previous[pi];
                    // Track displacement over one interval, km in grid axes.
                    let ds = displacement_secs.max(1.0);
                    let mut dx = (blob.centroid.0 - prev.blob.centroid.0) * scale.x / ds;
                    let mut dy = (blob.centroid.1 - prev.blob.centroid.1) * scale.y / ds;
                    // Physical clamp before the EMA fold: a centroid jump
                    // past MAX_CELL_SPEED_MS (merge/split shifting the
                    // intensity-weighted centroid, or a residual mismatch)
                    // is not cell MOTION — cap the magnitude, keep the
                    // direction, and let the EMA absorb the remainder.
                    // Without this, one bad displacement reads as 200+ km/h
                    // and pollutes speed/bearing for several generations.
                    let speed_kms = (dx * dx + dy * dy).sqrt();
                    let max_kms = MAX_CELL_SPEED_MS / 1000.0;
                    if speed_kms > max_kms {
                        let f = max_kms / speed_kms;
                        dx *= f;
                        dy *= f;
                    }
                    let (vx_km, vy_km) = match prev.velocity_kms {
                        // EMA keeps single-scan centroid jitter out of the
                        // deviant residual.
                        Some((px, py)) => (0.5 * dx + 0.5 * px, 0.5 * dy + 0.5 * py),
                        None => (dx, dy),
                    };

                    // Ambient flow at the cell: px/field-interval → km/s.
                    let fs = field_interval_secs.max(1.0);
                    let (fu, fv) = field.sample(blob.centroid.0, blob.centroid.1);
                    let (fx_km, fy_km) = (fu * scale.x / fs, fv * scale.y / fs);
                    let to_ms = 1000.0;
                    let residual_ms =
                        (((vx_km - fx_km).powi(2) + (vy_km - fy_km).powi(2)).sqrt()) * to_ms;
                    let cell_speed_ms = (vx_km * vx_km + vy_km * vy_km).sqrt() * to_ms;
                    let deviant_now = residual_ms > DEVIANT_RESIDUAL_MS
                        && cell_speed_ms > DEVIANT_MIN_CELL_SPEED_MS;

                    let growing = Some(blob.volume >= prev.blob.volume);
                    // Mean-exceedance trend, per second, EMA'd with the
                    // track's history (newborns start at 0 ⇒ first
                    // measurement is halved — mild warm-up damping).
                    let mean_now = blob.volume / blob.area.max(1) as f32;
                    let mean_prev = prev.blob.volume / prev.blob.area.max(1) as f32;
                    let raw_tendency = ((mean_now - mean_prev) / ds)
                        .clamp(-MAX_CELL_TENDENCY_PER_S, MAX_CELL_TENDENCY_PER_S);
                    let intensity_tendency = 0.5 * raw_tendency + 0.5 * prev.intensity_tendency;
                    CellTrack {
                        id: prev.id,
                        blob,
                        age: prev.age + 1,
                        growing,
                        intensity_tendency,
                        velocity_kms: Some((vx_km, vy_km)),
                        deviant_streak: if deviant_now {
                            prev.deviant_streak + 1
                        } else {
                            0
                        },
                        severity,
                        // The join (apply_lightning) fills this generation's
                        // stats after matching; the baseline history rides
                        // the track.
                        flash_count: None,
                        flash_rate_per_min: None,
                        flash_history: prev.flash_history.clone(),
                        lightning_jump: false,
                        jump_sigma: None,
                        cg_count: None,
                        ic_count: None,
                        cg_positive_count: None,
                        // Carried, not reset: it is the track's first flash
                        // ever, not its first this generation.
                        first_flash: prev.first_flash,
                    }
                }
            }
        })
        .collect()
}

/// Join one generation's lightning strikes onto the tracked cells (#549)
/// and update each track's flash statistics and jump flag.
///
/// `strikes_px` are strike positions in WORKING-GRID pixel coordinates —
/// the caller projects lon/lat and drops off-grid strikes. `labels` is the
/// analysis label map on the same grid (`0` background, `k+1` ⇔
/// `tracks[k]`, the `segment_cells_labeled` contract — `advance_tracks`
/// returns tracks in blob order, which preserves it). A strike lands on
/// its footprint's cell when labeled, else on the nearest cell centroid
/// within [`LIGHTNING_JOIN_RADIUS_KM`] (anisotropic km), else nowhere.
///
/// The jump flag is the Schultz-style detector: the window's rate exceeds
/// the track's recent baseline mean by 2σ, with ≥ 2 baseline rates and the
/// [`MIN_JUMP_RATE_PER_MIN`] absolute floor.
///
/// The radius fallback is a deliberate brute-force O(unmatched strikes ×
/// tracks) scan: the worst real Nordic window (~10⁴ strikes × ~150 cells)
/// is ~10⁶ distance checks once per generation on the background runtime
/// — milliseconds. Even the MAX_JOIN_STRIKES cap × the 255-track label
/// ceiling stays in the tens of ms. A spatial index earns its complexity
/// only if either bound grows by orders of magnitude.
/// Add one strike to a track's attribute tallies.
///
/// `any_attrs` records whether ANY strike carried a discriminator, which is
/// what decides between reporting zeros and reporting nothing.
#[allow(clippy::too_many_arguments)]
fn tally(
    cg: &mut [u32],
    ic: &mut [u32],
    cg_pos: &mut [u32],
    any_attrs: &mut bool,
    idx: usize,
    attrs: EventAttrs,
) {
    let Some(is_cg) = attrs.is_cloud_to_ground() else {
        return;
    };
    *any_attrs = true;
    if is_cg {
        cg[idx] += 1;
        // Polarity is independent of the IC/CG split and may be missing even
        // when the split is present.
        if attrs.is_positive() == Some(true) {
            cg_pos[idx] += 1;
        }
    } else {
        ic[idx] += 1;
    }
}

pub fn apply_lightning(
    tracks: &mut [CellTrack],
    // Position plus the reported attributes, parallel per strike. A slice of
    // pairs rather than a richer per-strike type keeps this allocation-free
    // at MAX_JOIN_STRIKES.
    strikes_px: &[((f32, f32), EventAttrs)],
    labels: &[u32],
    width: usize,
    scale: PixelScale,
    window_secs: f32,
    // The generation's analysis instant, stamped on a track's first flash.
    observed: DateTime<Utc>,
) {
    let mut counts = vec![0u32; tracks.len()];
    // Attribute tallies run alongside the plain count, indexed like `counts`.
    let mut cg = vec![0u32; tracks.len()];
    let mut ic = vec![0u32; tracks.len()];
    let mut cg_pos = vec![0u32; tracks.len()];
    let mut any_attrs = false;

    for &((sx, sy), attrs) in strikes_px {
        // In-grid by contract; `as usize` saturates negatives to 0 and
        // `labels.get` bounds the rest.
        let idx = (sy as usize) * width + sx as usize;
        let label = labels.get(idx).copied().unwrap_or(0) as usize;
        if (1..=tracks.len()).contains(&label) {
            counts[label - 1] += 1;
            tally(
                &mut cg,
                &mut ic,
                &mut cg_pos,
                &mut any_attrs,
                label - 1,
                attrs,
            );
            continue;
        }
        let gate2 = LIGHTNING_JOIN_RADIUS_KM * LIGHTNING_JOIN_RADIUS_KM;
        let mut best: Option<(usize, f32)> = None;
        for (k, t) in tracks.iter().enumerate() {
            let dx = (sx - t.blob.centroid.0) * scale.x;
            let dy = (sy - t.blob.centroid.1) * scale.y;
            let d2 = dx * dx + dy * dy;
            if d2 <= gate2 && best.is_none_or(|(_, b)| d2 < b) {
                best = Some((k, d2));
            }
        }
        if let Some((k, _)) = best {
            counts[k] += 1;
            tally(&mut cg, &mut ic, &mut cg_pos, &mut any_attrs, k, attrs);
        }
    }

    let window_min = (window_secs / 60.0).max(f32::EPSILON);
    for (idx, (t, &n)) in tracks.iter_mut().zip(&counts).enumerate() {
        let rate = n as f32 / window_min;
        // Keep the magnitude instead of collapsing it to the threshold test.
        // None while there is no baseline to measure against — 0.0 would
        // read as "measured, no anomaly", which is a different claim.
        let sigma = if t.flash_history.len() >= 2 {
            let count = t.flash_history.len() as f32;
            let mean = t.flash_history.iter().sum::<f32>() / count;
            let sd = (t
                .flash_history
                .iter()
                .map(|r| (r - mean).powi(2))
                .sum::<f32>()
                / count)
                .sqrt();
            // A flat history has zero spread, so any increase is infinitely
            // many sigmas. Report the rise without letting it be inf.
            Some(if sd > f32::EPSILON {
                (rate - mean) / sd
            } else if rate > mean {
                JUMP_SIGMA_FLAT_HISTORY
            } else {
                0.0
            })
        } else {
            None
        };
        // The bool stays derived from the magnitude, so they cannot disagree.
        let jump = rate >= MIN_JUMP_RATE_PER_MIN && sigma.is_some_and(|s| s > 2.0);

        if n > 0 && t.first_flash.is_none() {
            t.first_flash = Some(observed);
        }
        // Only surfaced when the source actually reports the discriminator;
        // otherwise these stay None, so "not reported" never reads as zero.
        if any_attrs {
            t.cg_count = Some(cg[idx]);
            t.ic_count = Some(ic[idx]);
            t.cg_positive_count = Some(cg_pos[idx]);
        }
        t.flash_count = Some(n);
        t.flash_rate_per_min = Some(rate);
        t.jump_sigma = sigma;
        t.lightning_jump = jump;
        t.flash_history.push(rate);
        if t.flash_history.len() > FLASH_HISTORY_LEN {
            t.flash_history.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_instant() -> DateTime<Utc> {
        "2026-08-24T12:00:00Z".parse().unwrap()
    }
    use crate::motion::{estimate_motion, MotionField, MotionOptions};
    use crate::objects::segment_cells;
    use crate::Grid;

    fn bare_track(id: u64, cx: f32, cy: f32) -> CellTrack {
        CellTrack {
            id,
            blob: CellBlob {
                centroid: (cx, cy),
                area: 10,
                volume: 400.0,
                max_value: 40.0,
            },
            age: 1,
            velocity_kms: None,
            deviant_streak: 0,
            severity: Severity::Weak,
            growing: None,
            intensity_tendency: 0.0,
            flash_count: None,
            flash_rate_per_min: None,
            flash_history: Vec::new(),
            lightning_jump: false,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_positive_count: None,
            first_flash: None,
        }
    }

    #[test]
    fn lightning_attributes_by_label_then_radius_and_ignores_far_strikes() {
        // 40×20 grid at 1 km/px. Track 1 has a labeled footprint around
        // (3,3); track 2 sits at (30,10) with no labeled pixels (its
        // strikes must come via the radius fallback).
        let (w, h) = (40usize, 20usize);
        let mut labels = vec![0u32; w * h];
        for y in 2..5 {
            for x in 2..5 {
                labels[y * w + x] = 1;
            }
        }
        let mut tracks = vec![bare_track(1, 3.0, 3.0), bare_track(2, 30.0, 10.0)];
        let scale = PixelScale { x: 1.0, y: 1.0 };
        let strikes: Vec<((f32, f32), EventAttrs)> = [
            (3.5, 3.5),   // labeled footprint → track 1
            (30.0, 10.0), // unlabeled px → nearest centroid (track 2, 0 km)
            (30.0, 2.0),  // 8 km north of track 2 — inside the 10 km gate
            (39.5, 19.5), // ~13.4 km from track 2 — outside the gate, dropped
        ]
        .into_iter()
        .map(|p| (p, EventAttrs::default()))
        .collect();
        apply_lightning(
            &mut tracks,
            &strikes,
            &labels,
            w,
            scale,
            300.0,
            test_instant(),
        );
        assert_eq!(tracks[0].flash_count, Some(1));
        assert_eq!(tracks[1].flash_count, Some(2));
        let r0 = tracks[0].flash_rate_per_min.unwrap();
        let r1 = tracks[1].flash_rate_per_min.unwrap();
        assert!(
            (r0 - 0.2).abs() < 1e-6,
            "1 strike / 5 min = 0.2/min, got {r0}"
        );
        assert!((r1 - 0.4).abs() < 1e-6);
        assert!(!tracks[0].lightning_jump, "no baseline yet");
        assert_eq!(tracks[0].flash_history.len(), 1);
    }

    #[test]
    fn lightning_jump_needs_baseline_floor_and_two_sigma() {
        let (w, h) = (10usize, 10usize);
        let labels = vec![0u32; w * h];
        let scale = PixelScale { x: 1.0, y: 1.0 };
        let strike = (5.0f32, 5.0f32); // radius-joins the only track
        let mut tracks = vec![bare_track(1, 5.0, 5.0)];

        // A burst with NO baseline never jumps (history < 2). Fresh track:
        // a burst entering the history would raise the later 2σ bar (the
        // detector deliberately distrusts cells that JUST burst).
        let burst: Vec<((f32, f32), EventAttrs)> = vec![(strike, EventAttrs::default()); 60]; // 12 fl/min
        apply_lightning(
            &mut tracks,
            &burst,
            &labels,
            w,
            scale,
            300.0,
            test_instant(),
        );
        assert!(!tracks[0].lightning_jump, "no jump without a baseline");

        // Quiet-baseline track: quiet, quiet, sub-floor uptick (4 fl/min
        // < the 10 fl/min floor — no jump, baseline [0,0,4]), then a real
        // burst: 12 ≥ floor and > mean+2σ ≈ 5.1 ⇒ jump.
        let mut tracks = vec![bare_track(2, 5.0, 5.0)];
        apply_lightning(&mut tracks, &[], &labels, w, scale, 300.0, test_instant());
        apply_lightning(&mut tracks, &[], &labels, w, scale, 300.0, test_instant());
        let uptick: Vec<((f32, f32), EventAttrs)> = vec![(strike, EventAttrs::default()); 20]; // 4 fl/min < floor
        apply_lightning(
            &mut tracks,
            &uptick,
            &labels,
            w,
            scale,
            300.0,
            test_instant(),
        );
        assert!(!tracks[0].lightning_jump, "below the absolute floor");
        apply_lightning(
            &mut tracks,
            &burst,
            &labels,
            w,
            scale,
            300.0,
            test_instant(),
        );
        assert!(tracks[0].lightning_jump, "burst over quiet baseline");

        // History stays bounded.
        for _ in 0..10 {
            apply_lightning(&mut tracks, &[], &labels, w, scale, 300.0, test_instant());
        }
        assert!(tracks[0].flash_history.len() <= FLASH_HISTORY_LEN);
    }

    #[test]
    fn gates_and_clamp_bound_implied_track_velocity() {
        // The 2026-07-26 client bug: gate-edge mismatches read as 200+
        // km/h storms. With speed-based gates, a blob whose association
        // would imply > MAX_CELL_SPEED_MS (+ base) never matches; and a
        // legal-but-fast displacement folds into the EMA magnitude-
        // clamped, direction preserved.
        let still = MotionField {
            block: 16,
            bw: 2,
            bh: 2,
            u: vec![0.0; 4],
            v: vec![0.0; 4],
            measured: vec![true; 4],
        };
        let scale = PixelScale { x: 1.0, y: 1.0 };

        // 15 km jump in 300 s (50 m/s implied): outside the 13.5 km raw
        // gate — the track dies and the blob is born fresh.
        let previous = vec![bare_track(1, 0.0, 0.0)];
        let far = CellBlob {
            centroid: (15.0, 0.0),
            area: 20,
            volume: 800.0,
            max_value: 42.0,
        };
        let mut next = 100u64;
        let tracks = advance_tracks(&previous, vec![far], scale, &still, 300.0, 300.0, || {
            next += 1;
            next
        });
        assert_eq!(tracks[0].age, 1, "50 m/s implied match must be rejected");
        assert_ne!(tracks[0].id, 1);

        // 12 km in 300 s (40 m/s): inside the 13.5 km raw gate, but the
        // folded velocity is clamped to MAX_CELL_SPEED_MS with the
        // direction (due east) preserved.
        let near = CellBlob {
            centroid: (12.0, 0.0),
            area: 20,
            volume: 800.0,
            max_value: 42.0,
        };
        let previous = vec![bare_track(1, 0.0, 0.0)];
        let tracks = advance_tracks(&previous, vec![near], scale, &still, 300.0, 300.0, || {
            next += 1;
            next
        });
        assert_eq!(tracks[0].id, 1);
        let speed = tracks[0].speed_ms().unwrap();
        assert!(
            (speed - MAX_CELL_SPEED_MS).abs() < 0.01,
            "clamped to the physical cap, got {speed}"
        );
        assert!((tracks[0].bearing_deg().unwrap() - 90.0).abs() < 0.5);
    }

    #[test]
    fn counter_flow_cell_matches_via_raw_position_pass() {
        // The against-flow fix (two-hypothesis matching): a strong ambient
        // flow displaces the pass-1 hypothesis of a STATIONARY cell far
        // outside the gate. Pass 2 must still match it at its raw position —
        // otherwise the matcher drops exactly the counter-flow cells the
        // deviant-mover detector exists to flag.
        let blob = CellBlob {
            centroid: (50.0, 50.0),
            area: 20,
            volume: 800.0,
            max_value: 42.0,
        };
        let previous = vec![CellTrack {
            id: 7,
            blob: blob.clone(),
            age: 1,
            velocity_kms: None,
            deviant_streak: 0,
            severity: Severity::Weak,
            growing: None,
            intensity_tendency: 0.0,
            flash_count: None,
            flash_rate_per_min: None,
            flash_history: Vec::new(),
            lightning_jump: false,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_positive_count: None,
            first_flash: None,
        }];
        // Uniform 30 px/interval eastward flow; at 1 km/px the compensated
        // hypothesis lands 30 km from the stationary successor — far
        // outside the pass-1 residual gate (6 km at 300 s), so pass 1
        // alone would orphan the track; the raw-position pass rescues it
        // (0 km ≤ the 13.5 km raw gate).
        let field = MotionField {
            block: 16,
            bw: 2,
            bh: 2,
            u: vec![30.0; 4],
            v: vec![0.0; 4],
            measured: vec![true; 4],
        };
        let scale = PixelScale { x: 1.0, y: 1.0 };
        let mut next = 100u64;
        let tracks = advance_tracks(&previous, vec![blob], scale, &field, 300.0, 300.0, || {
            next += 1;
            next
        });
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].id, 7,
            "raw-position pass must rescue the counter-flow match"
        );
        assert_eq!(tracks[0].age, 2);
        // Velocity comes from ORIGINAL (non-displaced) centroids: the cell
        // is stationary, so the track velocity must be ~zero, not the flow.
        let (vx, vy) = tracks[0].velocity_kms.unwrap();
        assert!(vx.abs() < 1e-6 && vy.abs() < 1e-6);
    }

    fn disc(w: usize, h: usize, cx: f32, cy: f32, r: f32, v: f32) -> Grid {
        let mut data = vec![0.0f32; w * h];
        for (i, cell) in data.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
            if (x - cx).powi(2) + (y - cy).powi(2) <= r * r {
                *cell = v;
            }
        }
        Grid::new(w, h, data)
    }

    #[test]
    fn severity_rank_is_monotone_in_intensity_and_area() {
        let weak = CellBlob {
            centroid: (0.0, 0.0),
            area: 10,
            volume: 10.0,
            max_value: 40.0,
        };
        assert_eq!(severity(&weak, 10.0), Severity::Weak);
        let mut b = weak.clone();
        b.max_value = 47.0;
        assert_eq!(severity(&b, 10.0), Severity::Moderate);
        b.max_value = 52.0;
        assert_eq!(severity(&b, 60.0), Severity::VerySevere);
        b.max_value = 57.0;
        assert_eq!(severity(&b, 60.0), Severity::VerySevere);
    }

    #[test]
    fn tracks_carry_id_age_velocity_and_flag_deviant_movers() {
        // Ambient field: everything (the big disc) moves +4 px/interval in x.
        // The small disc moves -4 px/interval — against the flow.
        let scale = PixelScale { x: 1.0, y: 1.0 }; // 1 km/px, interval 300 s
        let f0 = |big_x: f32, small_x: f32| {
            let mut g = disc(300, 120, big_x, 60.0, 25.0, 45.0);
            let s = disc(300, 120, small_x, 30.0, 5.0, 50.0);
            for (a, b) in g.data.iter_mut().zip(&s.data) {
                *a = a.max(*b);
            }
            g
        };
        let frame_a = f0(150.0, 250.0);
        let frame_b = f0(154.0, 246.0);
        let field = estimate_motion(&frame_a, &frame_b, &MotionOptions::default());

        let mut counter = 0u64;
        let mut id_gen = || {
            counter += 1;
            counter
        };
        let t0 = advance_tracks(
            &[],
            segment_cells(&frame_a, CELL_THRESHOLD_DBZ, CELL_MIN_AREA_PX),
            scale,
            &field,
            300.0,
            300.0,
            &mut id_gen,
        );
        assert_eq!(t0.len(), 2);
        assert!(t0.iter().all(|t| t.age == 1 && t.velocity_kms.is_none()));

        let mut tracks = t0;
        for step in 1..=2 {
            let fa = f0(150.0 + 4.0 * step as f32, 250.0 - 4.0 * step as f32);
            tracks = advance_tracks(
                &tracks,
                segment_cells(&fa, CELL_THRESHOLD_DBZ, CELL_MIN_AREA_PX),
                scale,
                &field,
                300.0,
                300.0,
                &mut id_gen,
            );
        }
        assert_eq!(tracks.len(), 2);
        assert!(tracks.iter().all(|t| t.age == 3), "ids persisted");
        let small = tracks.iter().find(|t| t.blob.area < 200).unwrap();
        let big = tracks.iter().find(|t| t.blob.area >= 200).unwrap();
        assert!(
            small.deviant(),
            "counter-flow cell must be flagged (streak {})",
            small.deviant_streak
        );
        assert!(!big.deviant(), "with-flow cell must not be flagged");
        // ~4 km / 300 s ≈ 13 m/s, moving due west vs due east.
        assert!((small.speed_ms().unwrap() - 13.3).abs() < 4.0);
        assert!((big.bearing_deg().unwrap() - 90.0).abs() < 20.0);
        assert!((small.bearing_deg().unwrap() - 270.0).abs() < 20.0);
    }

    #[test]
    fn jump_sigma_is_none_without_a_baseline_and_a_value_with_one() {
        let mut t = bare_track(1, 10.0, 10.0);
        // No history: the magnitude is unknown, not zero.
        apply_lightning(
            &mut [t.clone()][..],
            &[],
            &[],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        let mut tracks = vec![t.clone()];
        apply_lightning(
            &mut tracks,
            &[],
            &[],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].jump_sigma, None, "no baseline yet");

        // With a flat, quiet history a surge is extreme but finite.
        t.flash_history = vec![0.0, 0.0, 0.0];
        let mut tracks = vec![t.clone()];
        let strikes: Vec<((f32, f32), EventAttrs)> = (0..50)
            .map(|_| ((0.5, 0.5), EventAttrs::default()))
            .collect();
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        let sigma = tracks[0].jump_sigma.expect("history exists");
        assert!(sigma.is_finite(), "a flat baseline must not yield inf");
        assert!(sigma > 2.0, "a surge from nothing is a large jump: {sigma}");
        assert!(
            tracks[0].lightning_jump,
            "and the bool follows the magnitude"
        );
    }

    #[test]
    fn first_flash_is_stamped_once_and_carried() {
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0, 1.0];
        let mut tracks = vec![t];
        let scale = PixelScale { x: 1.0, y: 1.0 };
        // No strikes: nothing to stamp.
        apply_lightning(&mut tracks, &[], &[1], 1, scale, 60.0, test_instant());
        assert_eq!(tracks[0].first_flash, None);

        let first = test_instant();
        apply_lightning(
            &mut tracks,
            &[((0.5, 0.5), EventAttrs::default())],
            &[1],
            1,
            scale,
            60.0,
            first,
        );
        assert_eq!(tracks[0].first_flash, Some(first));

        // A later flash must NOT overwrite it — it is the first ever.
        let later = first + chrono::Duration::minutes(30);
        apply_lightning(
            &mut tracks,
            &[((0.5, 0.5), EventAttrs::default())],
            &[1],
            1,
            scale,
            60.0,
            later,
        );
        assert_eq!(
            tracks[0].first_flash,
            Some(first),
            "first_flash is the first, not the latest"
        );
    }

    #[test]
    fn ic_cg_and_polarity_are_tallied_per_cell() {
        use ds_core::events::EventAttrs;
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        let scale = PixelScale { x: 1.0, y: 1.0 };
        let at = |cloud, current| EventAttrs {
            cloud_indicator: Some(cloud),
            peak_current_ka: Some(current),
            multiplicity: None,
        };
        let strikes = vec![
            ((0.5, 0.5), at(0, -15.0)), // CG negative
            ((0.5, 0.5), at(0, 22.0)),  // CG positive
            ((0.5, 0.5), at(0, 30.0)),  // CG positive
            ((0.5, 0.5), at(1, -5.0)),  // IC
        ];
        apply_lightning(&mut tracks, &strikes, &[1], 1, scale, 60.0, test_instant());
        assert_eq!(tracks[0].flash_count, Some(4));
        assert_eq!(tracks[0].cg_count, Some(3));
        assert_eq!(tracks[0].ic_count, Some(1));
        assert_eq!(tracks[0].cg_positive_count, Some(2));
    }

    #[test]
    fn a_source_reporting_no_discriminator_leaves_the_split_unreported() {
        use ds_core::events::EventAttrs;
        // Not zero: "this network doesn't say" and "no CG flashes" are
        // different facts, and only one of them licenses a statement.
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        let strikes = vec![((0.5, 0.5), EventAttrs::default()); 3];
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].flash_count, Some(3), "the count still works");
        assert_eq!(tracks[0].cg_count, None);
        assert_eq!(tracks[0].ic_count, None);
        assert_eq!(tracks[0].cg_positive_count, None);
    }
}
