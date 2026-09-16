//! Production motion estimation shared by the live engine and hindcast harness.
//!
//! Frames and pair intervals are oldest first. Output vectors are working-grid
//! pixels per last source interval. This module owns no I/O or retained state;
//! callers pass the previous generation explicitly for the temporal EMA.

use crate::motion::{estimate_motion_multi, MotionField, MotionOptions};
use crate::Grid;

/// Fastest cell motion the search window must cover (m/s). 40 m/s ≈ 144 km/h
/// matches the cell-tracker gate in `ds_core::cells`.
const MAX_SPEED_MS: f64 = 40.0;
/// Target search radius (px) on the motion-estimation grid; frames are
/// coarsened until the physical search window fits. 48 preserves typical
/// kilometre-scale working grids; actual resolution depends on the source
/// extent and pixel budget, not a fixed 500 m assumption.
const TARGET_SEARCH_PX: i32 = 48;
/// Temporal EMA weights for blending each generation's motion field with
/// the previous one (#524): the new field keeps this share, per block.
/// Measured blocks carry fresh information; filled blocks are inferred and
/// lean harder on history. 0.7 ≈ one-and-a-half generations of memory —
/// enough to damp single-pair convective noise without lagging a genuine
/// wind shift by more than a couple of cadence intervals.
const EMA_ALPHA_MEASURED: f32 = 0.7;
const EMA_ALPHA_FILLED: f32 = 0.4;

/// Shared ceiling on source history fetched per generation.
pub const MAX_HISTORY_FRAMES: usize = 8;

/// Motion result plus the scale choices needed to interpret harness output.
pub struct MotionEstimate {
    pub field: MotionField,
    pub interval_secs: f32,
    pub coarsening: usize,
    pub search_radius: i32,
}

/// Fit the native working grid into the configured pixel budget, preserving
/// nonzero axes even for elongated source grids. Inputs must be nonzero.
pub fn working_grid_size([mut w, mut h]: [u32; 2], max_pixels: usize) -> [u32; 2] {
    assert!(w > 0 && h > 0 && max_pixels > 0);
    while (w as usize) * (h as usize) > max_pixels && w.max(h) > 1 {
        if w >= h {
            w = (w / 2).max(1);
        } else {
            h = (h / 2).max(1);
        }
    }
    [w, h]
}

/// Estimate with the production physical search radius, coarsening, multi-pair
/// averaging and cadence-rescaled temporal EMA. Frames must share a nonempty
/// grid, with one interval per pair and a positive, finite last interval.
/// Invalid older intervals are skipped by the multi-pair estimator.
/// `previous` carries its own source interval, not the forecast lead spacing.
pub fn estimate_production_motion(
    frames: &[&Grid],
    pair_intervals_secs: &[f32],
    px_meters: f64,
    min_echo: f32,
    previous: Option<(&MotionField, f32)>,
) -> MotionEstimate {
    assert!(frames.len() >= 2);
    assert_eq!(pair_intervals_secs.len(), frames.len() - 1);
    let interval_secs = *pair_intervals_secs.last().expect("at least one pair");
    assert!(interval_secs.is_finite() && interval_secs >= 1.0);
    let (w, h) = (frames[0].width, frames[0].height);
    assert!(w > 0 && h > 0);
    assert!(frames.iter().all(|g| (g.width, g.height) == (w, h)));

    let max_shift_px = MAX_SPEED_MS * interval_secs as f64 / px_meters.max(1.0);
    let mut factor = 1usize;
    while max_shift_px / factor as f64 > TARGET_SEARCH_PX as f64
        && w / (factor * 2) >= 128
        && h / (factor * 2) >= 128
    {
        factor *= 2;
    }
    let search_radius =
        ((max_shift_px / factor as f64).ceil() as i32).clamp(4, TARGET_SEARCH_PX * 2);
    let opts = MotionOptions {
        search_radius,
        min_echo,
        ..MotionOptions::default()
    };
    let scales: Vec<f32> = pair_intervals_secs
        .iter()
        .map(|dt| interval_secs / dt)
        .collect();
    let mut field = if factor > 1 {
        let coarse: Vec<Grid> = frames.iter().map(|g| downsample(g, factor)).collect();
        let refs: Vec<&Grid> = coarse.iter().collect();
        let mut f = estimate_motion_multi(&refs, &scales, &opts);
        f.block *= factor;
        for v in f.u.iter_mut().chain(f.v.iter_mut()) {
            *v *= factor as f32;
        }
        f
    } else {
        estimate_motion_multi(frames, &scales, &opts)
    };

    if let Some((previous, previous_interval_secs)) = previous {
        let ratio = interval_secs / previous_interval_secs;
        if (ratio - 1.0).abs() > 1e-6 && ratio.is_finite() {
            let mut rescaled = previous.clone();
            for v in rescaled.u.iter_mut().chain(rescaled.v.iter_mut()) {
                *v *= ratio;
            }
            field.blend_with_previous(&rescaled, EMA_ALPHA_MEASURED, EMA_ALPHA_FILLED);
        } else {
            field.blend_with_previous(previous, EMA_ALPHA_MEASURED, EMA_ALPHA_FILLED);
        }
    }
    MotionEstimate {
        field,
        interval_secs,
        coarsening: factor,
        search_radius,
    }
}

/// Nearest-neighbour 1/f downsample (motion estimation only — stored frames
/// stay full resolution).
fn downsample(grid: &Grid, factor: usize) -> Grid {
    let w = (grid.width / factor).max(1);
    let h = (grid.height / factor).max(1);
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            data.push(grid.at(x * factor, y * factor));
        }
    }
    Grid::new(w, h, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo(w: usize, h: usize, shift: usize) -> Grid {
        let mut g = Grid::new(w, h, vec![0.0; w * h]);
        for y in h / 3..2 * h / 3 {
            for x in w / 3..2 * w / 3 {
                g.data[y * w + x + shift] = 20.0 + ((x * 17 + y * 11) % 29) as f32;
            }
        }
        g
    }

    #[test]
    fn irregular_pairs_use_last_interval_units() {
        let frames = [echo(128, 128, 0), echo(128, 128, 2), echo(128, 128, 6)];
        let result = estimate_production_motion(
            &frames.iter().collect::<Vec<_>>(),
            &[300.0, 600.0],
            2000.0,
            10.0,
            None,
        );
        assert_eq!(result.search_radius, 12);
        assert_eq!(result.interval_secs, 600.0);
        assert_eq!(result.coarsening, 1);
        assert!(result.field.measured.iter().any(|m| *m));
        let (u, v) = result.field.sample(64.0, 64.0);
        assert!((u - 4.0).abs() < 0.2, "expected 4px/600s, got {u}");
        assert!(v.abs() < 0.2);
    }

    #[test]
    fn coarsened_vectors_return_to_working_grid_units() {
        let frames = [echo(256, 256, 0), echo(256, 256, 8)];
        let result =
            estimate_production_motion(&[&frames[0], &frames[1]], &[300.0], 125.0, 10.0, None);
        assert_eq!(result.coarsening, 2);
        assert_eq!(result.search_radius, 48);
        assert_eq!(result.field.block, 64);
        let (u, v) = result.field.sample(128.0, 128.0);
        assert!((u - 8.0).abs() < 0.2, "expected 8 working px, got {u}");
        assert!(v.abs() < 0.2);
    }

    #[test]
    fn ema_rescales_previous_interval_for_measured_and_filled_blocks() {
        let frames = [echo(128, 128, 0), echo(128, 128, 4)];
        let raw =
            estimate_production_motion(&[&frames[0], &frames[1]], &[600.0], 2000.0, 10.0, None);
        let mut previous = raw.field.clone();
        previous.u.fill(10.0);
        previous.v.fill(-5.0);
        let blended = estimate_production_motion(
            &[&frames[0], &frames[1]],
            &[600.0],
            2000.0,
            10.0,
            Some((&previous, 300.0)),
        );
        assert!(raw.field.measured.iter().any(|m| *m));
        assert!(raw.field.measured.iter().any(|m| !*m));
        for i in 0..raw.field.u.len() {
            let weight = if raw.field.measured[i] { 0.7 } else { 0.4 };
            assert!(
                (blended.field.u[i] - (weight * raw.field.u[i] + (1.0 - weight) * 20.0)).abs()
                    < 1e-5
            );
            assert!(
                (blended.field.v[i] - (weight * raw.field.v[i] - (1.0 - weight) * 10.0)).abs()
                    < 1e-5
            );
        }
        previous.block *= 2;
        let incompatible = estimate_production_motion(
            &[&frames[0], &frames[1]],
            &[600.0],
            2000.0,
            10.0,
            Some((&previous, 300.0)),
        );
        assert_eq!(incompatible.field.u, raw.field.u);
        assert_eq!(incompatible.field.v, raw.field.v);
    }

    #[test]
    fn working_grid_budget_handles_elongated_sources() {
        assert_eq!(working_grid_size([4096, 512], 1_048_576), [2048, 512]);
        assert_eq!(working_grid_size([1, 1_000_000], 500_000), [1, 500_000]);
    }
}
