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

use crate::motion::MotionField;
use crate::objects::{match_cells, CellBlob, PixelScale};

/// Cell threshold (dBZ) for intelligence tracking — the Ritvanen-style
/// convective contour, matching the verification harness default.
pub const CELL_THRESHOLD_DBZ: f32 = 35.0;
/// Minimum component size in pixels. 10 px ≈ 2.5 km² on the FMI 500 m
/// grid — below that, 35 dBZ specks churn between generations and flood
/// the Features layer with unmatched one-generation "cells".
pub const CELL_MIN_AREA_PX: usize = 10;
/// Matching gate (km) for track continuity between generations, per
/// [`TRACK_GATE_BASE_SECS`] of elapsed time — the gate scales linearly
/// with the actual span (a skipped generation doubles the distance a
/// storm legitimately covers), never below one base gate.
pub const TRACK_GATE_KM: f32 = 20.0;
pub const TRACK_GATE_BASE_SECS: f32 = 300.0;
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

/// TRT-lite severity rank from 2D attributes (documented heuristic v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Weak,
    Moderate,
    Severe,
    VerySevere,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Weak => "weak",
            Severity::Moderate => "moderate",
            Severity::Severe => "severe",
            Severity::VerySevere => "very_severe",
        }
    }
}

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
    let gate = TRACK_GATE_KM * (displacement_secs / TRACK_GATE_BASE_SECS).max(1.0);
    let mut matched_prev: Vec<Option<usize>> = vec![None; blobs.len()];
    let mut prev_taken = vec![false; previous.len()];
    for (pi, ci) in match_cells(&displaced, &blobs, scale, gate) {
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
        for (a, b) in match_cells(&prev_raw, &cur_raw, scale, gate) {
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
                },
                Some(pi) => {
                    let prev = &previous[pi];
                    // Track displacement over one interval, km in grid axes.
                    let ds = displacement_secs.max(1.0);
                    let dx = (blob.centroid.0 - prev.blob.centroid.0) * scale.x / ds;
                    let dy = (blob.centroid.1 - prev.blob.centroid.1) * scale.y / ds;
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
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::{estimate_motion, MotionField, MotionOptions};
    use crate::objects::segment_cells;
    use crate::Grid;

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
        }];
        // Uniform 30 px/interval eastward flow; at 1 km/px the compensated
        // hypothesis lands 30 km from the stationary successor — outside
        // the 20 km base gate, so pass 1 alone would orphan the track.
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
}
