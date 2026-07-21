//! Semi-Lagrangian backward advection along a motion field.
//!
//! For each output pixel the departure trajectory is integrated *backward*
//! through the motion field in substeps, then the analysis frame is sampled
//! **once, nearest-neighbour** at the departure point. One nearest sample
//! keeps sparse far-range echoes and their raw values intact — iterated
//! bilinear resampling smears exactly the echoes the GeoTIFF downsampling bug
//! (#456) taught us not to lose. Pixels whose trajectory leaves the domain
//! (inflow boundary) become nodata; nodata never turns into echo.

use crate::motion::MotionField;
use crate::Grid;

/// Incrementally extended backward trajectories — one per output pixel.
///
/// A generation advects the same analysis frame to a *schedule* of leads.
/// Re-integrating each lead's trajectory from scratch costs
/// `Σ leadᵢ·substeps·pixels` — quadratic in the lead count (67 s per prod
/// generation at 2.27 Mpx × 24 leads, #528). Extending the stored departure
/// points by one lead-delta at a time walks the SAME piecewise trajectory
/// (identical step density and step sequence when the schedule is uniform)
/// in `Σ substeps·pixels` — linear.
///
/// Departure points keep integrating even outside the domain (the motion
/// field's edge-clamped sample keeps them moving) exactly like the one-shot
/// integration did; bounds are only checked when sampling a frame.
pub struct TrajectoryIntegrator<'a> {
    field: &'a MotionField,
    width: usize,
    height: usize,
    /// Current departure position of each output pixel's trajectory.
    px: Vec<f32>,
    py: Vec<f32>,
}

impl<'a> TrajectoryIntegrator<'a> {
    /// Trajectories at lead 0: every pixel departs from its own centre.
    pub fn new(width: usize, height: usize, field: &'a MotionField) -> Self {
        let mut px = Vec::with_capacity(width * height);
        let mut py = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                px.push(x as f32 + 0.5);
                py.push(y as f32 + 0.5);
            }
        }
        Self {
            field,
            width,
            height,
            px,
            py,
        }
    }

    /// Extend every trajectory backward by `delta` frame intervals, in
    /// `ceil(delta × substeps)` integration steps (≥ 1).
    pub fn advance(&mut self, delta: f32, substeps: usize) {
        let steps = ((delta * substeps.max(1) as f32).ceil() as usize).max(1);
        let h = delta / steps as f32;
        for (px, py) in self.px.iter_mut().zip(self.py.iter_mut()) {
            for _ in 0..steps {
                let (u, v) = self.field.sample(*px, *py);
                *px -= u * h;
                *py -= v * h;
            }
        }
    }

    /// Source-pixel index each trajectory currently departs from; `None`
    /// outside the domain (inflow boundary).
    #[inline]
    fn source_index(&self, i: usize) -> Option<usize> {
        let (px, py) = (self.px[i], self.py[i]);
        if px < 0.0 || py < 0.0 || px >= self.width as f32 || py >= self.height as f32 {
            None
        } else {
            Some(py as usize * self.width + px as usize)
        }
    }

    /// Sample `frame` (f32, NaN nodata) at the current departure points.
    pub fn sample(&self, frame: &Grid) -> Grid {
        assert_eq!((frame.width, frame.height), (self.width, self.height));
        let mut out = Grid::filled_nodata(self.width, self.height);
        for (i, cell) in out.data.iter_mut().enumerate() {
            if let Some(src) = self.source_index(i) {
                *cell = frame.data[src];
            }
        }
        out
    }

    /// Sample a raw-`u8` frame at the current departure points; inflow
    /// pixels get the `nodata` byte.
    pub fn sample_u8(&self, frame: &[u8], nodata: u8) -> Vec<u8> {
        assert_eq!(frame.len(), self.width * self.height);
        let mut out = vec![nodata; frame.len()];
        for (i, cell) in out.iter_mut().enumerate() {
            if let Some(src) = self.source_index(i) {
                *cell = frame[src];
            }
        }
        out
    }
}

/// Extrapolate `frame` forward by `lead` frame intervals (one-shot form —
/// a single-lead wrapper over [`TrajectoryIntegrator`], so the one-shot and
/// incremental paths share the same integration core and cannot drift).
///
/// `substeps` is the number of trajectory-integration steps per interval;
/// values around 4 track spatially varying fields well. `lead` may be
/// fractional.
pub fn advect(frame: &Grid, field: &MotionField, lead: f32, substeps: usize) -> Grid {
    let mut integrator = TrajectoryIntegrator::new(frame.width, frame.height, field);
    integrator.advance(lead, substeps);
    integrator.sample(frame)
}

/// Raw-byte advection: extrapolate an 8-bit frame without decoding it, so a
/// `RasterValues::U8`-shaped source stays 1 byte/pixel end to end. Inflow
/// pixels get the `nodata` byte.
pub fn advect_u8(
    frame: &[u8],
    width: usize,
    height: usize,
    nodata: u8,
    field: &MotionField,
    lead: f32,
    substeps: usize,
) -> Vec<u8> {
    assert_eq!(frame.len(), width * height, "frame length must be w*h");
    let mut integrator = TrajectoryIntegrator::new(width, height, field);
    integrator.advance(lead, substeps);
    integrator.sample_u8(frame, nodata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::{estimate_motion, MotionOptions};
    use crate::skill::score;

    fn disc_frame(w: usize, h: usize, cx: f32, cy: f32, r: f32, value: f32) -> Grid {
        let mut data = vec![0.0f32; w * h];
        for (i, cell) in data.iter_mut().enumerate() {
            let (x, y) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
            if (x - cx).powi(2) + (y - cy).powi(2) <= r * r {
                *cell = value;
            }
        }
        Grid::new(w, h, data)
    }

    #[test]
    fn zero_field_is_identity_for_finite_and_nodata_pixels() {
        let mut frame = disc_frame(64, 64, 30.0, 30.0, 8.0, 40.0);
        frame.data[5] = f32::NAN;
        let field = estimate_motion(&frame, &frame, &MotionOptions::default());
        let out = advect(&frame, &field, 1.0, 4);
        for (a, b) in frame.data.iter().zip(&out.data) {
            assert!(
                (a.is_nan() && b.is_nan()) || a == b,
                "identity advection changed a pixel: {a} -> {b}"
            );
        }
    }

    #[test]
    fn extrapolation_beats_persistence_on_translating_disc() {
        // The phase-0 gate in miniature: a disc translating (+6, -4) per
        // interval. Estimate motion from (t0, t1), extrapolate t1 one
        // interval, score against the true t2 — and against persistence.
        let t0 = disc_frame(200, 200, 74.0, 124.0, 12.0, 40.0);
        let t1 = disc_frame(200, 200, 80.0, 120.0, 12.0, 40.0);
        let t2 = disc_frame(200, 200, 86.0, 116.0, 12.0, 40.0);
        let opts = MotionOptions {
            search_radius: 10,
            ..MotionOptions::default()
        };
        let field = estimate_motion(&t0, &t1, &opts);
        let forecast = advect(&t1, &field, 1.0, 4);

        let nowcast = score(&forecast, &t2, 20.0).csi().unwrap();
        let persistence = score(&t1, &t2, 20.0).csi().unwrap();
        assert!(
            nowcast > persistence,
            "nowcast CSI {nowcast} must beat persistence {persistence}"
        );
        assert!(
            nowcast > 0.9,
            "nowcast CSI {nowcast} should be near-perfect"
        );
    }

    /// The incremental path must walk the exact trajectory the one-shot path
    /// does: N advances of one interval == one advance of N intervals, down
    /// to the bit (same step size, same step sequence). This is what makes
    /// the O(leads) generation loop (#528) safe to substitute for per-lead
    /// from-scratch integration.
    #[test]
    fn incremental_advances_match_one_shot_bitwise() {
        let t0 = disc_frame(160, 120, 60.0, 60.0, 11.0, 40.0);
        let t1 = disc_frame(160, 120, 66.0, 56.0, 11.0, 40.0);
        let field = estimate_motion(&t0, &t1, &MotionOptions::default());

        let one_shot = advect(&t1, &field, 3.0, 4);
        let mut integrator = TrajectoryIntegrator::new(160, 120, &field);
        for _ in 0..3 {
            integrator.advance(1.0, 4);
        }
        let incremental = integrator.sample(&t1);
        for (a, b) in one_shot.data.iter().zip(&incremental.data) {
            assert!(
                (a.is_nan() && b.is_nan()) || a == b,
                "incremental and one-shot advection diverged: {a} vs {b}"
            );
        }
    }

    /// The raw-u8 path must move exactly the same source pixels as the f32
    /// path — they share the `TrajectoryIntegrator` core, pinning that
    /// contract.
    #[test]
    fn advect_u8_matches_f32_path_pixel_for_pixel() {
        let t0 = disc_frame(120, 90, 50.0, 40.0, 9.0, 40.0);
        let t1 = disc_frame(120, 90, 56.0, 36.0, 9.0, 40.0);
        let field = estimate_motion(&t0, &t1, &MotionOptions::default());

        // Encode t1 as raw u8: value 40.0 → raw 80 (gain 0.5), background 0,
        // nodata byte 255.
        let raw: Vec<u8> = t1
            .data
            .iter()
            .map(|v| if v.is_nan() { 255 } else { (v * 2.0) as u8 })
            .collect();

        let f32_out = advect(&t1, &field, 1.0, 4);
        let u8_out = advect_u8(&raw, 120, 90, 255, &field, 1.0, 4);
        for (a, b) in f32_out.data.iter().zip(&u8_out) {
            let expected = if a.is_nan() { 255 } else { (a * 2.0) as u8 };
            assert_eq!(expected, *b, "u8 and f32 advection diverged");
        }
    }

    #[test]
    fn inflow_boundary_becomes_nodata_not_echo() {
        // Uniform rightward motion: the left edge has no upstream data, so it
        // must become nodata rather than repeat or invent echo.
        let t0 = disc_frame(100, 100, 40.0, 50.0, 10.0, 40.0);
        let t1 = disc_frame(100, 100, 48.0, 50.0, 10.0, 40.0);
        let field = estimate_motion(&t0, &t1, &MotionOptions::default());
        let out = advect(&t1, &field, 1.0, 4);
        for y in 0..100 {
            let v = out.at(0, y);
            assert!(
                v.is_nan() || v < 20.0,
                "left inflow column should be nodata/background, got {v}"
            );
        }
        assert!(
            out.data.iter().filter(|v| v.is_nan()).count() > 0,
            "expected a nodata inflow strip"
        );
    }
}
