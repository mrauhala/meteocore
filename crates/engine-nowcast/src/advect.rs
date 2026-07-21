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

/// Walk every output pixel's backward trajectory and hand the caller the
/// source-pixel index it departs from (`None` when the trajectory leaves the
/// domain). Shared core for the `f32` and raw-`u8` advection variants so
/// their trajectory math cannot drift apart.
fn for_each_departure(
    width: usize,
    height: usize,
    field: &MotionField,
    lead: f32,
    substeps: usize,
    mut emit: impl FnMut(usize, Option<usize>),
) {
    let steps = ((lead * substeps.max(1) as f32).ceil() as usize).max(1);
    let h = lead / steps as f32;
    for y in 0..height {
        for x in 0..width {
            let mut px = x as f32 + 0.5;
            let mut py = y as f32 + 0.5;
            for _ in 0..steps {
                let (u, v) = field.sample(px, py);
                px -= u * h;
                py -= v * h;
            }
            let src = if px < 0.0 || py < 0.0 || px >= width as f32 || py >= height as f32 {
                None // departure outside the domain: inflow boundary
            } else {
                Some(py as usize * width + px as usize)
            };
            emit(y * width + x, src);
        }
    }
}

/// Extrapolate `frame` forward by `lead` frame intervals.
///
/// `substeps` is the number of trajectory-integration steps per interval;
/// values around 4 track spatially varying fields well. `lead` may be
/// fractional.
pub fn advect(frame: &Grid, field: &MotionField, lead: f32, substeps: usize) -> Grid {
    let mut out = Grid::filled_nodata(frame.width, frame.height);
    for_each_departure(
        frame.width,
        frame.height,
        field,
        lead,
        substeps,
        |dst, src| {
            if let Some(src) = src {
                out.data[dst] = frame.data[src];
            }
        },
    );
    out
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
    let mut out = vec![nodata; frame.len()];
    for_each_departure(width, height, field, lead, substeps, |dst, src| {
        if let Some(src) = src {
            out[dst] = frame[src];
        }
    });
    out
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

    /// The raw-u8 path must move exactly the same source pixels as the f32
    /// path — they share `for_each_departure`, and this pins that contract.
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
