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
/// Shortest path (km) that counts as a path at all for
/// [`CellTrack::path_straightness`].
///
/// Below this the cell has not gone anywhere and the net/path ratio is noise
/// divided by noise. `None` is the honest answer, and
/// [`CellTrack::net_displacement_km`] already says the useful thing about a
/// cell that never moved.
pub const MIN_PATH_FOR_STRAIGHTNESS_KM: f32 = 1.0;
/// Straightness a track needs before its motion is trusted enough to raise
/// the deviant-mover flag (#629).
///
/// A track whose own displacement is incoherent has motion estimates that
/// measure association noise rather than storm behaviour. Observed
/// 2026-08-24: a track ping-ponging between two fixed echoes 6.3 km apart
/// scored ~0.2 straightness, produced 20 m/s speed spikes on the jump frames,
/// and raised `deviant_mover` — which then appeared in
/// `significance_reasons` and inflated its rank.
pub const DEVIANT_MIN_STRAIGHTNESS: f32 = 0.5;
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

/// dBZ the peak must fall BELOW a step, beyond the step itself, before the
/// severity is allowed to drop back through it (#623).
///
/// Frame-to-frame `max_dbz` noise on a real cell runs about ±2 dB while the
/// bins are hard steps at 45/50/55, so a cell sitting near a boundary crosses
/// it repeatedly without changing in any physical sense. Observed 2026-08-24:
/// one coherent 50-minute track, growing monotonically the whole time,
/// reported nine severity changes — a client animates that as a storm
/// exploding and collapsing every five minutes.
pub const SEVERITY_DOWNGRADE_DEADBAND_DBZ: f32 = 2.0;
/// Same idea for the area point, relative because cell areas span orders of
/// magnitude and a fixed km² slack would be meaningless at both ends.
pub const SEVERITY_DOWNGRADE_DEADBAND_AREA: f64 = 0.15;

/// Points behind the severity rank, with per-criterion slack applied.
///
/// `slack` is only ever non-zero when testing whether an ALREADY severe cell
/// may drop; it makes each criterion easier to keep than to earn.
fn severity_points(blob: &CellBlob, area_km2: f64, dbz_slack: f32, area_slack: f64) -> u32 {
    let mut points = 0u32;
    for step in [45.0f32, 50.0, 55.0] {
        if blob.max_value >= step - dbz_slack {
            points += 1;
        }
    }
    if area_km2 >= 50.0 * (1.0 - area_slack) {
        points += 1;
    }
    points
}

fn severity_from_points(points: u32) -> Severity {
    match points {
        0 => Severity::Weak,
        1 => Severity::Moderate,
        2 => Severity::Severe,
        _ => Severity::VerySevere,
    }
}

/// Rank a cell from max intensity and area: one point per crossed max-dBZ
/// step (45/50/55) plus one for area ≥ 50 km² — 0 ⇒ weak … 3+ ⇒ very
/// severe. Deliberately simple and monotone; a tree-based model replaces
/// this in V2.4.
///
/// This is the memoryless form, used for a cell with no history. Tracked
/// cells go through [`severity_hysteretic`].
pub fn severity(blob: &CellBlob, area_km2: f64) -> Severity {
    severity_from_points(severity_points(blob, area_km2, 0.0, 0.0))
}

/// Severity for a tracked cell, damped against boundary flapping (#623).
///
/// **Deliberately asymmetric.** Rising is immediate: a strengthening storm
/// must never be under-called while a filter waits for confirmation. Falling
/// requires the peak to clear [`SEVERITY_DOWNGRADE_DEADBAND_DBZ`] below the
/// step it earned, so noise alone cannot walk a cell back down.
///
/// The cost is that severity becomes path-dependent — two cells with
/// identical current pixels can report different severity if they arrived
/// from different directions. That is inherent to hysteresis and is the
/// intended trade: the alternative is a value that changes every frame for
/// reasons that are not about the weather.
pub fn severity_hysteretic(blob: &CellBlob, area_km2: f64, prev: Option<Severity>) -> Severity {
    let raw = severity(blob, area_km2);
    let Some(prev) = prev else {
        return raw;
    };
    if raw.rank() >= prev.rank() {
        return raw;
    }
    // Falling. Re-test with the criteria relaxed; hold the old rank if the
    // cell still clears them, and otherwise drop only as far as the relaxed
    // test allows — a genuine collapse still registers in one frame.
    let held = severity_from_points(severity_points(
        blob,
        area_km2,
        SEVERITY_DOWNGRADE_DEADBAND_DBZ,
        SEVERITY_DOWNGRADE_DEADBAND_AREA,
    ));
    if held.rank() >= prev.rank() {
        prev
    } else {
        held
    }
}

/// Relative volume change needed to flip the growing/decaying verdict (#623).
///
/// The old test was a bare `volume >= prev.volume`, so a cell that had not
/// meaningfully changed still had to answer growing or decaying, and noise
/// decided which. The same 50-minute track that flapped severity alternated
/// this six times while growing monotonically.
pub const TREND_FLIP_DEADBAND: f32 = 0.10;

/// Absolute floor on the change, in the volume proxy's own units
/// (summed dBZ above the cell threshold).
///
/// The deadband is relative, so on a marginal cell — ten pixels barely above
/// 35 dBZ, volume near zero — ten percent is also near zero and any wobble
/// flips the verdict. That would reintroduce the flapping this fix exists to
/// remove, just for weak cells instead of ones near a severity edge.
///
/// One pixel-dBZ is below anything physically meaningful and binds only on
/// cells whose volume is single digits; a real cell of any size clears it on
/// the relative test first. **Uncalibrated** — chosen from the units rather
/// than from measured marginal-cell traces, so revisit against real data.
pub const TREND_MIN_VOLUME_CHANGE: f32 = 1.0;

/// Growing / decaying, measured against the volume at which the current
/// verdict was last confirmed rather than against the previous frame.
///
/// **The anchor is what makes this correct.** Comparing to the previous frame
/// means a real trend whose per-frame change never clears the deadband can
/// never flip the verdict, however large the cumulative change: 5% growth per
/// frame for twenty frames is a 165% increase that would still report
/// "decaying" if that is what the verdict happened to be. Anchoring accrues
/// those small steps until they clear the band together, so slow-but-real
/// drift eventually wins while genuine noise — which oscillates around the
/// anchor instead of accumulating — never does.
///
/// Returns the verdict and the anchor to carry forward: unchanged while
/// holding, reset to the current volume whenever the verdict is confirmed.
///
/// `None` propagates only when there was no previous verdict AND the change
/// is too small to call — "no trend established yet" rather than a coin flip.
fn volume_trend(volume_now: f32, anchor: f32, prev_verdict: Option<bool>) -> (Option<bool>, f32) {
    let base = anchor.abs().max(f32::EPSILON);
    let delta = volume_now - anchor;
    let relative = delta / base;
    // Both gates must clear: relative keeps large cells from tripping on
    // trivial fractions, absolute keeps small ones from tripping on noise.
    let significant = relative.abs() > TREND_FLIP_DEADBAND && delta.abs() > TREND_MIN_VOLUME_CHANGE;
    if !significant {
        return (prev_verdict, anchor);
    }
    (Some(delta > 0.0), volume_now)
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
    /// Centroid (pixels) where this track was first detected.
    pub first_centroid: (f32, f32),
    /// Straight-line distance from first detection to now, km.
    ///
    /// The honest answer to "has this thing actually gone anywhere". A fixed
    /// echo sits near zero however long it is tracked, and unlike
    /// `track_age` it cannot be inflated by an association failure.
    pub net_displacement_km: f32,
    /// Distance travelled summed frame by frame, km. Always ≥
    /// [`Self::net_displacement_km`].
    pub path_length_km: f32,
    pub severity: Severity,
    /// Volume-proxy trend vs the previous observation: `Some(true)` =
    /// growing, `Some(false)` = decaying, `None` = newborn (#546 iter 1).
    pub growing: Option<bool>,
    /// Volume at which [`Self::growing`] was last confirmed — the reference
    /// the deadband is measured from, NOT the previous frame's volume.
    ///
    /// Carrying this is what lets a slow, sustained trend accumulate past the
    /// deadband instead of being held forever by steps that are each
    /// individually too small. Reset whenever the verdict is confirmed.
    pub trend_anchor_volume: f32,
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
    /// CG flashes whose polarity was reported — the DENOMINATOR for the
    /// positive share. Not the same as `cg_count`: peak-current estimation
    /// fails on weak signals, so a network can classify only part of its CG
    /// population, and dividing by the full count would understate the share.
    pub cg_polarity_known_count: Option<u32>,
    /// When this track was first attributed a flash. Electrification age is
    /// context a raw count cannot carry: a cell producing its first flash
    /// now is a different situation from one that has been active an hour.
    pub first_flash: Option<DateTime<Utc>>,
}

impl CellTrack {
    /// Sustained deviant mover?
    /// Net displacement divided by path-integrated distance, 0..=1.
    ///
    /// Real advection sits near 1: a storm that travels 30 km gets 30 km from
    /// where it started. A track that wanders without arriving anywhere — the
    /// signature of an association failure swapping between co-located fixed
    /// echoes — sits near 0.
    ///
    /// `None` when the path is shorter than
    /// [`MIN_PATH_FOR_STRAIGHTNESS_KM`]: a cell that never moved has no
    /// direction to be straight in, and 0/0 is not 0. Read
    /// [`Self::net_displacement_km`] for that case — it is the field that
    /// distinguishes a stationary echo, while this one distinguishes a
    /// wandering one.
    pub fn path_straightness(&self) -> Option<f32> {
        (self.path_length_km > MIN_PATH_FOR_STRAIGHTNESS_KM)
            .then(|| (self.net_displacement_km / self.path_length_km).clamp(0.0, 1.0))
    }

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
            // Severity is hysteretic for a tracked cell (#623), so it can only
            // be computed once the predecessor is known — see each arm below.
            match prev_idx {
                None => CellTrack {
                    id: next_id(),
                    first_centroid: blob.centroid,
                    net_displacement_km: 0.0,
                    path_length_km: 0.0,
                    severity: severity(&blob, area_km2),
                    trend_anchor_volume: blob.volume,
                    blob,
                    age: 1,
                    velocity_kms: None,
                    deviant_streak: 0,
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
                    cg_polarity_known_count: None,
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
                    // Path accrues frame by frame; net is always measured
                    // from the origin, so an association jump inflates the
                    // path without inflating the net — which is exactly the
                    // asymmetry that exposes it (#629).
                    let path_length_km =
                        prev.path_length_km + scale.distance(blob.centroid, prev.blob.centroid);
                    let net_displacement_km = scale.distance(blob.centroid, prev.first_centroid);
                    let straightness = (path_length_km > MIN_PATH_FOR_STRAIGHTNESS_KM)
                        .then(|| (net_displacement_km / path_length_km).clamp(0.0, 1.0));
                    // An incoherent track's motion estimate measures
                    // association noise, so it must not earn the deviant
                    // bonus. Unknown straightness does not qualify either:
                    // this awards a bonus, and an unverifiable claim should
                    // not get one.
                    let coherent = straightness.is_some_and(|s| s >= DEVIANT_MIN_STRAIGHTNESS);
                    let deviant_now = residual_ms > DEVIANT_RESIDUAL_MS
                        && cell_speed_ms > DEVIANT_MIN_CELL_SPEED_MS
                        && coherent;

                    // Measured from the anchor — the volume where the verdict
                    // was last confirmed — not from the previous frame, so a
                    // slow sustained trend accumulates instead of being held
                    // forever by individually-small steps.
                    let (growing, trend_anchor_volume) =
                        volume_trend(blob.volume, prev.trend_anchor_volume, prev.growing);
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
                        first_centroid: prev.first_centroid,
                        net_displacement_km,
                        path_length_km,
                        severity: severity_hysteretic(&blob, area_km2, Some(prev.severity)),
                        blob,
                        age: prev.age + 1,
                        growing,
                        trend_anchor_volume,
                        intensity_tendency,
                        velocity_kms: Some((vx_km, vy_km)),
                        deviant_streak: if deviant_now {
                            prev.deviant_streak + 1
                        } else {
                            0
                        },
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
                        cg_polarity_known_count: None,
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
/// Per-track attribute tallies accumulated over one lightning join.
///
/// Every field is PER TRACK, including the two "was this ever reported"
/// flags. Two separate mistakes live here, both found in review on #618, and
/// both the same shape — a presence flag coarser than the fact it gates:
///
/// 1. The split and the polarity are tracked separately, because
///    `cloud_indicator_col` and `peak_current_col` are independently optional.
///    One shared flag made a split-only network report "no positive flashes"
///    for a question it never asked.
/// 2. The flags are per track, not per generation. One batch can mix strikes
///    that carry a discriminator with strikes that don't — degraded detections
///    cluster by cell. A generation-global flag let a cell whose OWN strikes
///    were all unclassified report `Some(0)` because some other cell's strikes
///    were classified.
struct Tallies {
    cg: Vec<u32>,
    ic: Vec<u32>,
    cg_pos: Vec<u32>,
    /// CG flashes whose polarity was ACTUALLY reported — the denominator for
    /// the positive share, and deliberately not `cg`.
    ///
    /// Coverage can be partial within one network: peak-current estimation
    /// fails on weak signals, so a cell can have 10 CG flashes of which only
    /// 5 carry a current. Dividing 4 positives by all 10 would report 0.4
    /// where the measured share is 0.8, halving the term for every deployment
    /// with imperfect coverage and giving a consumer no way to see it.
    cg_polarity_known: Vec<u32>,
    /// Did THIS track see any strike carrying the IC/CG discriminator?
    saw_split: Vec<bool>,
    /// Did THIS track see any strike carrying a usable peak current?
    saw_polarity: Vec<bool>,
}

impl Tallies {
    fn new(n: usize) -> Self {
        Self {
            cg: vec![0; n],
            ic: vec![0; n],
            cg_pos: vec![0; n],
            cg_polarity_known: vec![0; n],
            saw_split: vec![false; n],
            saw_polarity: vec![false; n],
        }
    }

    /// Add one strike to track `idx`.
    fn add(&mut self, idx: usize, attrs: EventAttrs) {
        let is_cg = attrs.is_cloud_to_ground();
        // Polarity is independent of the IC/CG split. Any strike with a
        // current answers "is polarity reported for this cell" — but only a
        // KNOWN cloud-to-ground flash joins the share, since the quantity is
        // the positive share OF CG flashes.
        if let Some(positive) = attrs.is_positive() {
            self.saw_polarity[idx] = true;
            if is_cg == Some(true) {
                self.cg_polarity_known[idx] += 1;
                if positive {
                    self.cg_pos[idx] += 1;
                }
            }
        }
        let Some(is_cg) = is_cg else {
            return;
        };
        self.saw_split[idx] = true;
        if is_cg {
            self.cg[idx] += 1;
        } else {
            self.ic[idx] += 1;
        }
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
    let mut tallies = Tallies::new(tracks.len());

    for &((sx, sy), attrs) in strikes_px {
        // In-grid by contract; `as usize` saturates negatives to 0 and
        // `labels.get` bounds the rest.
        let idx = (sy as usize) * width + sx as usize;
        let label = labels.get(idx).copied().unwrap_or(0) as usize;
        if (1..=tracks.len()).contains(&label) {
            counts[label - 1] += 1;
            tallies.add(label - 1, attrs);
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
            tallies.add(k, attrs);
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
        // Only surfaced when THIS track's own strikes carried the
        // discriminator; otherwise these stay None, so "not reported" never
        // reads as zero. Gated per fact and per track — see `Tallies`.
        //
        // A cell with NO strikes is the exception, and it is not a special
        // case so much as arithmetic: the split of zero flashes is zero
        // flashes, whether or not the network could have classified them.
        // Gating on `saw_split` alone reported `flash_count: 0` beside
        // `cg_count: null`, which claims ignorance about a total the same
        // response asserts is zero.
        //
        // (This says nothing about coverage. Outside the lightning network's
        // range `flash_count` should itself be null rather than 0 — #621 —
        // and these follow it there.)
        let nothing_to_classify = n == 0;
        if nothing_to_classify || tallies.saw_split[idx] {
            t.cg_count = Some(tallies.cg[idx]);
            t.ic_count = Some(tallies.ic[idx]);
        }
        if nothing_to_classify || tallies.saw_polarity[idx] {
            t.cg_positive_count = Some(tallies.cg_pos[idx]);
            t.cg_polarity_known_count = Some(tallies.cg_polarity_known[idx]);
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
            first_centroid: (cx, cy),
            net_displacement_km: 0.0,
            path_length_km: 0.0,
            velocity_kms: None,
            deviant_streak: 0,
            severity: Severity::Weak,
            growing: None,
            trend_anchor_volume: 0.0,
            intensity_tendency: 0.0,
            flash_count: None,
            flash_rate_per_min: None,
            flash_history: Vec::new(),
            lightning_jump: false,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_positive_count: None,
            cg_polarity_known_count: None,
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
            first_centroid: blob.centroid,
            net_displacement_km: 0.0,
            path_length_km: 0.0,
            blob: blob.clone(),
            age: 1,
            velocity_kms: None,
            deviant_streak: 0,
            severity: Severity::Weak,
            growing: None,
            trend_anchor_volume: 0.0,
            intensity_tendency: 0.0,
            flash_count: None,
            flash_rate_per_min: None,
            flash_history: Vec::new(),
            lightning_jump: false,
            jump_sigma: None,
            cg_count: None,
            ic_count: None,
            cg_positive_count: None,
            cg_polarity_known_count: None,
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

    #[test]
    fn a_split_only_network_reports_no_polarity_rather_than_zero() {
        use ds_core::events::EventAttrs;
        // cloud_indicator_col and peak_current_col are independently optional.
        // A network reporting the split but no current must leave the positive
        // count unreported — 0 would claim "we looked and found none".
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        let split_only = |cloud| EventAttrs {
            cloud_indicator: Some(cloud),
            peak_current_ka: None,
        };
        let strikes = vec![
            ((0.5, 0.5), split_only(0)),
            ((0.5, 0.5), split_only(0)),
            ((0.5, 0.5), split_only(1)),
        ];
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].cg_count, Some(2), "the split IS reported");
        assert_eq!(tracks[0].ic_count, Some(1));
        assert_eq!(
            tracks[0].cg_positive_count, None,
            "polarity was never reported, so it must not read as zero"
        );
    }

    #[test]
    fn a_polarity_only_network_reports_polarity_without_the_split() {
        use ds_core::events::EventAttrs;
        // The mirror case: current but no discriminator. The positive count is
        // gated on a KNOWN cloud-to-ground flash, so it stays 0 here — but it
        // is reported, because the network does answer the polarity question.
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        let strikes = vec![
            (
                (0.5, 0.5),
                EventAttrs {
                    cloud_indicator: None,
                    peak_current_ka: Some(30.0),
                },
            );
            3
        ];
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].cg_count, None);
        assert_eq!(tracks[0].ic_count, None);
        assert_eq!(tracks[0].cg_positive_count, Some(0));
    }

    #[test]
    fn partial_polarity_coverage_divides_by_what_was_classified() {
        use ds_core::events::EventAttrs;
        // 4 CG with known polarity (3 positive) + 6 CG whose current was NULL.
        // The share is 3/4, not 3/10: a network whose current estimation fails
        // on weak signals must not have its positives divided by flashes it
        // never classified.
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        let cg = |current: Option<f32>| EventAttrs {
            cloud_indicator: Some(0),
            peak_current_ka: current,
        };
        let mut strikes = vec![((0.5, 0.5), cg(Some(20.0))); 3];
        strikes.push(((0.5, 0.5), cg(Some(-20.0))));
        strikes.extend(vec![((0.5, 0.5), cg(None)); 6]);
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(
            tracks[0].cg_count,
            Some(10),
            "every CG flash is still counted"
        );
        assert_eq!(
            tracks[0].cg_polarity_known_count,
            Some(4),
            "only 4 carried a current"
        );
        assert_eq!(tracks[0].cg_positive_count, Some(3));
    }

    #[test]
    fn one_cells_unclassified_strikes_do_not_borrow_anothers_discriminator() {
        use ds_core::events::EventAttrs;
        // Degraded detections cluster by cell. Track A's own strikes carry no
        // discriminator; track B's do. A generation-global presence flag would
        // hand A a "measured zero" built from B's evidence.
        let mut tracks = vec![bare_track(1, 0.5, 0.5), bare_track(2, 1.5, 0.5)];
        for t in &mut tracks {
            t.flash_history = vec![1.0];
        }
        let strikes = vec![
            // Track A (label 1): no discriminator at all.
            ((0.5, 0.5), EventAttrs::default()),
            ((0.5, 0.5), EventAttrs::default()),
            // Track B (label 2): fully classified.
            (
                (1.5, 0.5),
                EventAttrs {
                    cloud_indicator: Some(0),
                    peak_current_ka: Some(25.0),
                },
            ),
        ];
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1, 2],
            2,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].flash_count, Some(2), "A still counts its strikes");
        assert_eq!(
            tracks[0].cg_count, None,
            "A saw no discriminator, so it must report nothing — not zero"
        );
        assert_eq!(tracks[0].ic_count, None);
        assert_eq!(tracks[0].cg_positive_count, None);
        // B is unaffected and reports what it actually measured.
        assert_eq!(tracks[1].cg_count, Some(1));
        assert_eq!(tracks[1].cg_positive_count, Some(1));
    }

    #[test]
    fn a_cell_with_no_strikes_reports_a_zero_split_not_an_unknown_one() {
        // Reported from the 2026-08-24 deploy: cells carried `flash_count: 0`
        // beside `cg_count: null`, claiming ignorance about a total the same
        // response asserted was zero. The split of zero flashes is zero.
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        apply_lightning(
            &mut tracks,
            &[],
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].flash_count, Some(0));
        assert_eq!(tracks[0].cg_count, Some(0), "zero of zero is zero");
        assert_eq!(tracks[0].ic_count, Some(0));
        assert_eq!(tracks[0].cg_polarity_known_count, Some(0));
        assert_eq!(tracks[0].cg_positive_count, Some(0));
    }

    #[test]
    fn strikes_that_carry_no_discriminator_still_report_an_unknown_split() {
        // The distinction the zero case must not erase: strikes were seen and
        // could not be classified, which is different from no strikes.
        use ds_core::events::EventAttrs;
        let mut t = bare_track(1, 0.5, 0.5);
        t.flash_history = vec![1.0];
        let mut tracks = vec![t];
        let strikes = vec![((0.5, 0.5), EventAttrs::default()); 4];
        apply_lightning(
            &mut tracks,
            &strikes,
            &[1],
            1,
            PixelScale { x: 1.0, y: 1.0 },
            60.0,
            test_instant(),
        );
        assert_eq!(tracks[0].flash_count, Some(4));
        assert_eq!(tracks[0].cg_count, None, "seen but unclassifiable");
        assert_eq!(tracks[0].ic_count, None);
        assert_eq!(tracks[0].cg_positive_count, None);
    }

    // ---- #623 severity hysteresis / trend deadband -----------------------

    fn blob_at(max_dbz: f32, area: usize) -> CellBlob {
        CellBlob {
            centroid: (0.5, 0.5),
            area,
            volume: max_dbz * area as f32,
            max_value: max_dbz,
        }
    }

    #[test]
    fn severity_rises_immediately_but_falls_only_past_the_deadband() {
        // Rising: never damped. Under-calling a strengthening storm while a
        // filter waits for confirmation is the one failure worth avoiding.
        let strengthening = blob_at(50.5, 10);
        assert_eq!(
            severity_hysteretic(&strengthening, 10.0, Some(Severity::Weak)),
            Severity::Severe,
            "a jump upward is reported the frame it happens"
        );

        // Falling by less than the deadband: held.
        let jitter = blob_at(49.4, 10);
        assert_eq!(
            severity_hysteretic(&jitter, 10.0, Some(Severity::Severe)),
            Severity::Severe,
            "0.6 dB under the step is noise, not a downgrade"
        );

        // Falling clear past it: drops, in one frame.
        let collapsed = blob_at(47.0, 10);
        assert_eq!(
            severity_hysteretic(&collapsed, 10.0, Some(Severity::Severe)),
            Severity::Moderate,
            "a genuine collapse still registers immediately"
        );
    }

    #[test]
    fn a_cell_jittering_across_a_bin_edge_reports_one_severity() {
        // The reported defect: max_dbz noise of about +/-1 dB either side of the
        // 50 step produced moderate/severe/moderate/severe on a track that was
        // not changing. Replay it.
        let noise = [50.3f32, 49.6, 50.4, 49.5, 50.1, 49.7, 50.2, 49.4];
        let mut sev = severity(&blob_at(noise[0], 10), 10.0);
        let first = sev;
        let mut changes = 0;
        for &dbz in &noise[1..] {
            let next = severity_hysteretic(&blob_at(dbz, 10), 10.0, Some(sev));
            if next != sev {
                changes += 1;
            }
            sev = next;
        }
        assert_eq!(changes, 0, "a cell that is not changing must not flap");
        assert_eq!(sev, first);

        // Without hysteresis the same sequence flaps on every sample, which is
        // what makes this a fix rather than a coincidence.
        let bare: Vec<Severity> = noise
            .iter()
            .map(|&d| severity(&blob_at(d, 10), 10.0))
            .collect();
        let bare_changes = bare.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(bare_changes, 7, "precondition: the raw binner flaps here");
    }

    /// Walk a volume series the way the tracker does, carrying the anchor.
    fn walk_trend(start_volume: f32, series: &[f32]) -> Vec<Option<bool>> {
        let mut verdict = None;
        let mut anchor = start_volume;
        series
            .iter()
            .map(|&v| {
                let (nv, na) = volume_trend(v, anchor, verdict);
                verdict = nv;
                anchor = na;
                verdict
            })
            .collect()
    }

    #[test]
    fn volume_trend_holds_through_noise_but_follows_real_change() {
        // Inside the deadband: keep the previous answer rather than letting
        // noise pick one.
        assert_eq!(volume_trend(102.0, 100.0, Some(true)).0, Some(true));
        assert_eq!(volume_trend(98.0, 100.0, Some(true)).0, Some(true));
        assert_eq!(volume_trend(102.0, 100.0, Some(false)).0, Some(false));

        // Past it: follow the data, both directions, and re-anchor.
        assert_eq!(volume_trend(130.0, 100.0, Some(false)), (Some(true), 130.0));
        assert_eq!(volume_trend(70.0, 100.0, Some(true)), (Some(false), 70.0));

        // No previous verdict and too small to call is honestly unknown —
        // not a coin flip dressed as a measurement.
        assert_eq!(volume_trend(101.0, 100.0, None).0, None);

        // A zero-volume predecessor must not divide by zero.
        assert_eq!(volume_trend(5.0, 0.0, None).0, Some(true));
    }

    #[test]
    fn a_slow_sustained_reversal_eventually_flips_the_verdict() {
        // The gap found in review on #627. Measuring against the PREVIOUS
        // FRAME meant a real trend whose per-frame change never cleared the
        // deadband could never flip the verdict, however large the cumulative
        // change. Here: an established "decaying", then twenty frames of 5%
        // growth — a 165% increase, every step individually under the band.
        let mut series = vec![88.0f32]; // -12%: establishes decaying
        let mut v = 88.0f32;
        for _ in 0..20 {
            v *= 1.05;
            series.push(v);
        }
        let verdicts = walk_trend(100.0, &series);
        assert_eq!(verdicts[0], Some(false), "precondition: decaying is set");
        assert_eq!(
            verdicts.last().copied().flatten(),
            Some(true),
            "sustained growth must win: {verdicts:?}"
        );
        // And it should not take anywhere near all twenty frames — two steps
        // of 5% clear 10% together.
        let flipped_at = verdicts.iter().position(|v| *v == Some(true)).unwrap();
        assert!(flipped_at <= 3, "flipped only at frame {flipped_at}");
    }

    #[test]
    fn oscillation_around_the_anchor_never_accumulates_into_a_verdict() {
        // The other half: noise must NOT accumulate. Wobbling +/-4% around a
        // fixed level forever stays undecided, because each sample is
        // measured from the same anchor rather than from its predecessor.
        let series: Vec<f32> = (0..40)
            .map(|i| if i % 2 == 0 { 104.0 } else { 96.0 })
            .collect();
        let verdicts = walk_trend(100.0, &series);
        assert!(
            verdicts.iter().all(|v| v.is_none()),
            "noise produced a verdict: {verdicts:?}"
        );
    }

    #[test]
    fn a_marginal_cell_does_not_hair_trigger_on_a_tiny_absolute_change() {
        // Secondary review finding: the deadband is relative, so a cell with
        // volume near zero has a threshold near zero and any wobble flips it.
        // 0.01 -> 0.02 is +100% relative and physically nothing.
        assert_eq!(volume_trend(0.02, 0.01, None).0, None);
        assert_eq!(volume_trend(0.005, 0.01, Some(true)).0, Some(true));
        // A real cell still clears both gates.
        assert_eq!(volume_trend(130.0, 100.0, None).0, Some(true));
    }

    #[test]
    fn a_monotonically_growing_cell_never_reports_decaying() {
        // Cell 156 in the field report: 3.6 -> 18.9 km2 over ten frames, which
        // flapped growing/decaying six times under the bare comparison.
        let series = [4.9f32, 6.2, 8.0, 9.1, 11.4, 13.0, 15.2, 16.8, 18.9];
        let verdicts = walk_trend(3.6, &series);
        assert!(
            !verdicts.contains(&Some(false)),
            "monotonic growth reported as decaying: {verdicts:?}"
        );
        assert_eq!(verdicts.last().copied().flatten(), Some(true));
    }

    // ---- #629 path straightness ------------------------------------------

    #[test]
    fn straightness_separates_advection_from_wandering() {
        let mut t = bare_track(1, 0.0, 0.0);

        // A real cell: 30 km travelled, 30 km from where it started.
        t.path_length_km = 30.0;
        t.net_displacement_km = 30.0;
        assert_eq!(t.path_straightness(), Some(1.0));

        // The reported track: ping-ponging 6.3 km apart for an hour ends up
        // ~6.4 km from the origin having covered ~30 km.
        t.path_length_km = 30.0;
        t.net_displacement_km = 6.4;
        let s = t.path_straightness().unwrap();
        assert!(
            (0.15..0.25).contains(&s),
            "expected the reported ~0.2, got {s}"
        );

        // A cell that never moved has no direction to be straight in. `None`,
        // not 0 — and `net_displacement_km` is what speaks for this case.
        t.path_length_km = 0.3;
        t.net_displacement_km = 0.05;
        assert_eq!(t.path_straightness(), None);
    }

    #[test]
    fn an_association_failure_inflates_the_path_but_not_the_net() {
        // The asymmetry the metric relies on. Two tracks end the same distance
        // from their origin; one got there directly, the other by ping-ponging.
        // Only the path length tells them apart, so only the ratio does.
        let scale = PixelScale { x: 1.0, y: 1.0 };
        let straight: Vec<(f32, f32)> = (0..=10).map(|i| (i as f32 * 3.0, 0.0)).collect();
        let pingpong: Vec<(f32, f32)> = (0..=10)
            .map(|i| if i % 2 == 0 { (0.0, 0.0) } else { (6.3, 0.0) })
            .collect();

        let measure = |path: &[(f32, f32)]| {
            let mut length = 0.0f32;
            for w in path.windows(2) {
                length += scale.distance(w[1], w[0]);
            }
            let net = scale.distance(*path.last().unwrap(), path[0]);
            (net, length)
        };

        let (net_s, len_s) = measure(&straight);
        let (net_p, len_p) = measure(&pingpong);
        assert!((net_s / len_s - 1.0).abs() < 1e-5, "advection is straight");
        assert!(
            net_p / len_p < DEVIANT_MIN_STRAIGHTNESS,
            "wandering must fall below the coherence gate: {}",
            net_p / len_p
        );
        // Both are "long tracks" by age; only this distinguishes them.
        assert!(len_p > 30.0 && net_p < 1.0);
    }

    #[test]
    fn an_incoherent_track_cannot_raise_the_deviant_flag() {
        // A track jumping between two fixed echoes produces a large residual
        // against the ambient flow on the jump frames. Before #629 that
        // raised `deviant_mover`, which then appeared in
        // `significance_reasons` and inflated the cell's rank.
        let mut t = bare_track(1, 0.0, 0.0);
        t.deviant_streak = DEVIANT_STREAK + 1;
        // The flag itself is still a pure function of the streak...
        assert!(t.deviant());
        // ...but the streak can no longer accumulate on an incoherent track,
        // which is enforced where `deviant_now` is computed. Pin the gate
        // value so the two cannot drift apart.
        t.path_length_km = 30.0;
        t.net_displacement_km = 6.4;
        assert!(
            t.path_straightness().unwrap() < DEVIANT_MIN_STRAIGHTNESS,
            "the reported track must be below the gate"
        );
    }

    #[test]
    fn a_tracked_cell_accumulates_path_and_net_from_its_origin() {
        let scale = PixelScale { x: 1.0, y: 1.0 };
        let frame = |x: f32| disc(300, 120, x, 60.0, 12.0, 50.0);
        let field = estimate_motion(&frame(100.0), &frame(104.0), &MotionOptions::default());
        let mut counter = 0u64;
        let mut id_gen = || {
            counter += 1;
            counter
        };
        let mut tracks = advance_tracks(
            &[],
            segment_cells(&frame(100.0), CELL_THRESHOLD_DBZ, CELL_MIN_AREA_PX),
            scale,
            &field,
            300.0,
            300.0,
            &mut id_gen,
        );
        assert_eq!(tracks[0].path_length_km, 0.0, "a newborn has gone nowhere");
        assert_eq!(tracks[0].net_displacement_km, 0.0);
        assert_eq!(tracks[0].path_straightness(), None);

        for x in [104.0f32, 108.0, 112.0] {
            tracks = advance_tracks(
                &tracks,
                segment_cells(&frame(x), CELL_THRESHOLD_DBZ, CELL_MIN_AREA_PX),
                scale,
                &field,
                300.0,
                300.0,
                &mut id_gen,
            );
        }
        let t = &tracks[0];
        assert!(
            t.path_length_km > 10.0,
            "path accrued: {}",
            t.path_length_km
        );
        // Pure advection: the two agree, so straightness is ~1.
        let s = t.path_straightness().expect("path is long enough");
        assert!(s > 0.95, "straight-line advection scored {s}");
    }
}
