//! Block-matching motion estimation between two consecutive frames.
//!
//! pySTEPS-style Lucas–Kanade-lite: cross-correlate coarse blocks (sum of
//! absolute differences), refine the integer minimum to sub-pixel precision,
//! keep only vectors measured where the target frame actually has echo
//! (which also stops stationary ground clutter and empty sky from anchoring
//! the field), reject outliers against the robust median of each vector's
//! nearest measured neighbours, then fill unmeasured blocks from their
//! neighbours and smooth the filled ones into a continuous field. Measured
//! vectors are never smoothed. The result samples bilinearly at any pixel
//! position.

use crate::Grid;

/// Knobs for [`estimate_motion`]. Defaults fit ~1 km composite pixels at a
/// 5-minute cadence (40 m/s ≈ 12 km ≈ 12 px per interval).
#[derive(Debug, Clone)]
pub struct MotionOptions {
    /// Block edge in pixels.
    pub block: usize,
    /// Search radius in pixels (max displacement per frame interval).
    pub search_radius: i32,
    /// Coarse search stride; a ±(stride) step-1 refinement follows.
    pub coarse_step: i32,
    /// Physical echo threshold (e.g. dBZ) a pixel must reach to count as echo.
    pub min_echo: f32,
    /// Fraction of a block's pixels that must be echo for the block to yield
    /// a vector.
    pub min_echo_frac: f32,
    /// Fraction of a block's pixels that must overlap valid source pixels for
    /// a candidate displacement to be scored.
    pub min_overlap_frac: f32,
    /// Robust outlier gate: vectors farther than
    /// `max(outlier_sigmas * 1.4826 * MAD, 2 px)` from the median of their
    /// `outlier_neighbours` nearest measured blocks are dropped.
    pub outlier_sigmas: f32,
    /// Neighbourhood size of the local outlier gate (pySTEPS' `k_outlier`).
    pub outlier_neighbours: usize,
    /// 3×3 box-smoothing passes applied after fill, to filled blocks only.
    pub smooth_passes: usize,
}

impl Default for MotionOptions {
    fn default() -> Self {
        Self {
            block: 32,
            search_radius: 20,
            coarse_step: 2,
            min_echo: 10.0,
            min_echo_frac: 0.02,
            min_overlap_frac: 0.5,
            outlier_sigmas: 3.0,
            outlier_neighbours: 30,
            smooth_passes: 2,
        }
    }
}

/// A block-level motion field over a frame, bilinear-sampled per pixel.
///
/// `u`/`v` are pixels per frame interval; `measured[i]` marks blocks whose
/// vector came from an actual block match (as opposed to fill/smoothing).
#[derive(Debug, Clone)]
pub struct MotionField {
    pub block: usize,
    /// Blocks per row.
    pub bw: usize,
    /// Blocks per column.
    pub bh: usize,
    pub u: Vec<f32>,
    pub v: Vec<f32>,
    pub measured: Vec<bool>,
}

impl MotionField {
    /// Temporal EMA with the previous generation's field (#524): per block,
    /// `self = alpha·self + (1−alpha)·prev`, where `alpha` is
    /// `alpha_measured` for blocks this generation actually measured and the
    /// (lower) `alpha_filled` for blocks whose vector came from fill/
    /// smoothing — a filled block carries no new information, so it should
    /// lean harder on history. No-op when the grids don't match (working
    /// geometry changed, e.g. a config edit) — blending across different
    /// grids would be nonsense.
    ///
    /// This is what stops nowcast animations rubber-banding between
    /// generations: single-pair block matching re-reads convective
    /// growth/decay as motion noise every generation, and the EMA gives the
    /// field ~1/(1−alpha) generations of memory.
    pub fn blend_with_previous(
        &mut self,
        prev: &MotionField,
        alpha_measured: f32,
        alpha_filled: f32,
    ) {
        if (self.bw, self.bh, self.block) != (prev.bw, prev.bh, prev.block) {
            return;
        }
        for j in 0..self.u.len() {
            let alpha = if self.measured[j] {
                alpha_measured
            } else {
                alpha_filled
            };
            self.u[j] = alpha * self.u[j] + (1.0 - alpha) * prev.u[j];
            self.v[j] = alpha * self.v[j] + (1.0 - alpha) * prev.v[j];
        }
    }

    /// Bilinear sample of (u, v) at pixel position (x, y), clamped at edges.
    pub fn sample(&self, x: f32, y: f32) -> (f32, f32) {
        if self.bw == 0 || self.bh == 0 {
            return (0.0, 0.0);
        }
        // Block centres sit at (b + 0.5) * block; express the query in block
        // coordinates relative to centre spacing.
        let fx = (x / self.block as f32 - 0.5).clamp(0.0, (self.bw - 1) as f32);
        let fy = (y / self.block as f32 - 0.5).clamp(0.0, (self.bh - 1) as f32);
        let ix = (fx.floor() as usize).min(self.bw.saturating_sub(2));
        let iy = (fy.floor() as usize).min(self.bh.saturating_sub(2));
        let (ix1, iy1) = ((ix + 1).min(self.bw - 1), (iy + 1).min(self.bh - 1));
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let idx = |bx: usize, by: usize| by * self.bw + bx;
        let lerp2 = |g: &[f32]| {
            let top = g[idx(ix, iy)] * (1.0 - tx) + g[idx(ix1, iy)] * tx;
            let bot = g[idx(ix, iy1)] * (1.0 - tx) + g[idx(ix1, iy1)] * tx;
            top * (1.0 - ty) + bot * ty
        };
        (lerp2(&self.u), lerp2(&self.v))
    }
}

/// Estimate the motion field that carries `prev` into `next`.
///
/// A vector `(u, v)` for a block means `next(x, y) ≈ prev(x - u, y - v)` for
/// pixels in that block: echoes moved by `(u, v)` over one frame interval.
pub fn estimate_motion(prev: &Grid, next: &Grid, opts: &MotionOptions) -> MotionField {
    let mut field = measure_pair(prev, next, opts);
    postprocess(&mut field, opts);
    field
}

/// Multi-pair motion estimation (#524): measure each consecutive frame pair,
/// scale pair *i*'s vectors by `interval_scales[i]` (converting them to the
/// reference interval's unit — pass `1.0` for a uniform cadence), average the
/// per-block measurements, then run the shared outlier/fill/smooth pipeline
/// ONCE on the combined field. Averaging measured vectors across pairs damps
/// the single-pair noise that convective growth/decay masquerades as —
/// the dominant source of generation-to-generation nowcast rubber-banding.
///
/// `frames` is oldest → newest, length ≥ 2; `interval_scales.len()` must be
/// `frames.len() - 1`.
pub fn estimate_motion_multi(
    frames: &[&Grid],
    interval_scales: &[f32],
    opts: &MotionOptions,
) -> MotionField {
    assert!(frames.len() >= 2, "multi-pair estimation needs >= 2 frames");
    assert_eq!(
        interval_scales.len(),
        frames.len() - 1,
        "one interval scale per consecutive pair"
    );

    let mut combined: Option<MotionField> = None;
    let mut counts: Vec<u32> = Vec::new();
    for (i, scale) in interval_scales.iter().enumerate() {
        if !scale.is_finite() || *scale <= 0.0 {
            continue; // degenerate pair interval — skip the pair
        }
        let pair = measure_pair(frames[i], frames[i + 1], opts);
        match &mut combined {
            None => {
                counts = pair.measured.iter().map(|&m| u32::from(m)).collect();
                let mut first = pair;
                for (j, m) in first.measured.iter().enumerate() {
                    if *m {
                        first.u[j] *= scale;
                        first.v[j] *= scale;
                    }
                }
                combined = Some(first);
            }
            Some(acc) => {
                debug_assert_eq!((acc.bw, acc.bh), (pair.bw, pair.bh));
                for (j, count) in counts.iter_mut().enumerate() {
                    if pair.measured[j] {
                        acc.u[j] += pair.u[j] * scale;
                        acc.v[j] += pair.v[j] * scale;
                        acc.measured[j] = true;
                        *count += 1;
                    }
                }
            }
        }
    }

    let mut field = combined
        .unwrap_or_else(|| measure_pair(frames[frames.len() - 2], frames[frames.len() - 1], opts));
    for (j, &n) in counts.iter().enumerate() {
        if n > 1 {
            field.u[j] /= n as f32;
            field.v[j] /= n as f32;
        }
    }
    postprocess(&mut field, opts);
    field
}

/// The raw block-matching stage: per-block SAD search, no outlier rejection,
/// no fill, no smoothing. Shared by the single-pair and multi-pair paths so
/// the measurement itself cannot drift between them.
fn measure_pair(prev: &Grid, next: &Grid, opts: &MotionOptions) -> MotionField {
    assert_eq!(
        (prev.width, prev.height),
        (next.width, next.height),
        "motion estimation needs equally sized frames"
    );
    let block = opts.block.max(4);
    let bw = next.width.div_ceil(block);
    let bh = next.height.div_ceil(block);
    let mut field = MotionField {
        block,
        bw,
        bh,
        u: vec![0.0; bw * bh],
        v: vec![0.0; bw * bh],
        measured: vec![false; bw * bh],
    };

    for by in 0..bh {
        for bx in 0..bw {
            let x0 = bx * block;
            let y0 = by * block;
            let x1 = (x0 + block).min(next.width);
            let y1 = (y0 + block).min(next.height);
            let area = (x1 - x0) * (y1 - y0);

            let echo_pixels = (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let v = next.at(x, y);
                    v.is_finite() && v >= opts.min_echo
                })
                .count();
            if (echo_pixels as f32) < opts.min_echo_frac * area as f32 {
                continue;
            }

            let min_overlap = (opts.min_overlap_frac * area as f32) as usize;
            let cost_of = |dx: i32, dy: i32| -> Option<f32> {
                let mut sum = 0.0f64;
                let mut n = 0usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let a = next.at(x, y);
                        if !a.is_finite() {
                            continue;
                        }
                        let sx = x as i64 - dx as i64;
                        let sy = y as i64 - dy as i64;
                        if sx < 0 || sy < 0 || sx >= prev.width as i64 || sy >= prev.height as i64 {
                            continue;
                        }
                        let b = prev.at(sx as usize, sy as usize);
                        if !b.is_finite() {
                            continue;
                        }
                        sum += (a - b).abs() as f64;
                        n += 1;
                    }
                }
                if n < min_overlap.max(1) {
                    return None;
                }
                // Tiny displacement penalty breaks ties on flat cost surfaces
                // toward "no motion" instead of an arbitrary search corner.
                let bias = 1e-4 * ((dx * dx + dy * dy) as f32).sqrt();
                Some(sum as f32 / n as f32 + bias)
            };

            let step = opts.coarse_step.max(1);
            let r = opts.search_radius.max(step);
            let mut best: Option<(f32, i32, i32)> = None;
            let mut dy = -r;
            while dy <= r {
                let mut dx = -r;
                while dx <= r {
                    keep_better(&mut best, cost_of(dx, dy), dx, dy);
                    dx += step;
                }
                dy += step;
            }
            if let Some((_, cx, cy)) = best {
                for dy in (cy - step)..=(cy + step) {
                    for dx in (cx - step)..=(cx + step) {
                        if dx.abs() <= r && dy.abs() <= r {
                            keep_better(&mut best, cost_of(dx, dy), dx, dy);
                        }
                    }
                }
            }

            if let Some((c0, dx, dy)) = best {
                // Integer displacements alone quantise speed to ±0.5 px per
                // interval — ±12 px after 24 leads (#651).
                let fx = subpixel_offset(cost_of(dx - 1, dy), c0, cost_of(dx + 1, dy));
                let fy = subpixel_offset(cost_of(dx, dy - 1), c0, cost_of(dx, dy + 1));
                let i = by * bw + bx;
                field.u[i] = dx as f32 + fx;
                field.v[i] = dy as f32 + fy;
                field.measured[i] = true;
            }
        }
    }

    field
}

/// The shared post-measurement pipeline: robust outlier rejection, fill,
/// smoothing of the filled blocks.
fn postprocess(field: &mut MotionField, opts: &MotionOptions) {
    reject_outliers(field, opts);
    fill_unmeasured(field);
    for _ in 0..opts.smooth_passes {
        box_smooth(field);
    }
}

/// Sub-pixel offset of the cost minimum along one axis: the vertex of the
/// parabola through the costs one pixel either side of the integer minimum
/// `c0`, clamped to ±0.5 px. Zero when a neighbour has too little overlap to
/// score or the surface is not convex there.
fn subpixel_offset(minus: Option<f32>, c0: f32, plus: Option<f32>) -> f32 {
    let (Some(m), Some(p)) = (minus, plus) else {
        return 0.0;
    };
    let curvature = m - 2.0 * c0 + p;
    if !curvature.is_finite() || curvature <= 0.0 {
        return 0.0;
    }
    (0.5 * (m - p) / curvature).clamp(-0.5, 0.5)
}

/// Keep `(cost, dx, dy)` in `best` if it beats the current candidate.
fn keep_better(best: &mut Option<(f32, i32, i32)>, cost: Option<f32>, dx: i32, dy: i32) {
    if let Some(c) = cost {
        if best.map(|(bc, _, _)| c < bc).unwrap_or(true) {
            *best = Some((c, dx, dy));
        }
    }
}

/// Drop measured vectors far from the robust (median/MAD) consensus of their
/// `outlier_neighbours` nearest measured blocks (itself excluded). A local
/// gate, as in pySTEPS: one global median over a composite-wide domain is a
/// rigid-body prior that clips genuine differential flow — a front steered
/// differently north and south, a right-mover (#651).
fn reject_outliers(field: &mut MotionField, opts: &MotionOptions) {
    let measured: Vec<usize> = (0..field.measured.len())
        .filter(|&i| field.measured[i])
        .collect();
    if measured.len() < 4 {
        return;
    }
    let k = opts.outlier_neighbours.clamp(1, measured.len() - 1);
    let bw = field.bw;
    let median_of = |vals: &mut [f32]| -> f32 {
        vals.sort_by(|a, b| a.total_cmp(b));
        vals[vals.len() / 2]
    };
    // Brute-force k-NN: O(measured²) block pairs, a few million at most on
    // the pixel-budgeted working grid.
    let mut near: Vec<(usize, usize)> = Vec::with_capacity(measured.len());
    let (mut us, mut vs) = (Vec::with_capacity(k), Vec::with_capacity(k));
    let mut rejected = Vec::new();
    for &i in &measured {
        let (xi, yi) = (i % bw, i / bw);
        near.clear();
        near.extend(measured.iter().filter(|&&j| j != i).map(|&j| {
            let (dx, dy) = ((j % bw).abs_diff(xi), (j / bw).abs_diff(yi));
            (dx * dx + dy * dy, j)
        }));
        // Ties break on block index, so the neighbourhood is deterministic.
        near.select_nth_unstable(k - 1);
        us.clear();
        vs.clear();
        us.extend(near[..k].iter().map(|&(_, j)| field.u[j]));
        vs.extend(near[..k].iter().map(|&(_, j)| field.v[j]));
        let (mu, mv) = (median_of(&mut us), median_of(&mut vs));
        us.iter_mut().for_each(|u| *u = (*u - mu).abs());
        vs.iter_mut().for_each(|v| *v = (*v - mv).abs());
        let (mad_u, mad_v) = (median_of(&mut us), median_of(&mut vs));
        let gate_u = (opts.outlier_sigmas * 1.4826 * mad_u).max(2.0);
        let gate_v = (opts.outlier_sigmas * 1.4826 * mad_v).max(2.0);
        if (field.u[i] - mu).abs() > gate_u || (field.v[i] - mv).abs() > gate_v {
            rejected.push(i);
        }
    }
    // Gate against the unfiltered neighbourhoods, then drop all at once, so
    // the verdict does not depend on block order.
    for i in rejected {
        field.measured[i] = false;
        field.u[i] = 0.0;
        field.v[i] = 0.0;
    }
}

/// Grow vectors outward from measured blocks until the whole field is
/// covered; a field with no measured vectors at all stays zero.
fn fill_unmeasured(field: &mut MotionField) {
    let (bw, bh) = (field.bw, field.bh);
    let mut known = field.measured.clone();
    if !known.iter().any(|&k| k) {
        return;
    }
    // Each pass fills cells adjacent to already-known cells; bounded by the
    // grid diameter.
    for _ in 0..(bw + bh) {
        if known.iter().all(|&k| k) {
            break;
        }
        let snapshot = known.clone();
        for by in 0..bh {
            for bx in 0..bw {
                let i = by * bw + bx;
                if snapshot[i] {
                    continue;
                }
                let mut su = 0.0f32;
                let mut sv = 0.0f32;
                let mut n = 0u32;
                for ny in by.saturating_sub(1)..(by + 2).min(bh) {
                    for nx in bx.saturating_sub(1)..(bx + 2).min(bw) {
                        let j = ny * bw + nx;
                        if snapshot[j] {
                            su += field.u[j];
                            sv += field.v[j];
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    field.u[i] = su / n as f32;
                    field.v[i] = sv / n as f32;
                    known[i] = true;
                }
            }
        }
    }
}

/// One 3×3 box-smoothing pass over the FILLED blocks. A measured vector is
/// kept as matched: averaging it with filled neighbours would dilute exactly
/// the isolated edge cell whose motion differs from the ambient flow (#651).
fn box_smooth(field: &mut MotionField) {
    let (bw, bh) = (field.bw, field.bh);
    let mut nu = field.u.clone();
    let mut nv = field.v.clone();
    for by in 0..bh {
        for bx in 0..bw {
            let i = by * bw + bx;
            if field.measured[i] {
                continue;
            }
            let mut su = 0.0f32;
            let mut sv = 0.0f32;
            let mut n = 0u32;
            for ny in by.saturating_sub(1)..(by + 2).min(bh) {
                for nx in bx.saturating_sub(1)..(bx + 2).min(bw) {
                    let j = ny * bw + nx;
                    su += field.u[j];
                    sv += field.v[j];
                    n += 1;
                }
            }
            nu[i] = su / n as f32;
            nv[i] = sv / n as f32;
        }
    }
    field.u = nu;
    field.v = nv;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hard disc of `value` centred at (cx, cy) on a zero background.
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
    fn multi_pair_matches_single_pair_on_uniform_motion() {
        // Three frames of the same translation: averaging the two pairs must
        // agree with the last pair alone (each pair measures the same shift).
        let t0 = disc_frame(200, 200, 74.0, 124.0, 12.0, 40.0);
        let t1 = disc_frame(200, 200, 80.0, 120.0, 12.0, 40.0);
        let t2 = disc_frame(200, 200, 86.0, 116.0, 12.0, 40.0);
        let opts = MotionOptions {
            search_radius: 10,
            ..MotionOptions::default()
        };
        let single = estimate_motion(&t1, &t2, &opts);
        let multi = estimate_motion_multi(&[&t0, &t1, &t2], &[1.0, 1.0], &opts);
        let (su, sv) = single.sample(86.0, 116.0);
        let (mu, mv) = multi.sample(86.0, 116.0);
        assert!(
            (su - mu).abs() <= 1.0 && (sv - mv).abs() <= 1.0,
            "uniform motion: multi ({mu},{mv}) must agree with single ({su},{sv})"
        );
    }

    #[test]
    fn multi_pair_skips_degenerate_interval_scales() {
        let t0 = disc_frame(160, 160, 60.0, 80.0, 10.0, 40.0);
        let t1 = disc_frame(160, 160, 66.0, 80.0, 10.0, 40.0);
        let opts = MotionOptions {
            search_radius: 10,
            ..MotionOptions::default()
        };
        // First pair carries a non-finite scale (degenerate interval) — it
        // must be skipped, leaving the second pair's measurement intact.
        let multi = estimate_motion_multi(&[&t0, &t0, &t1], &[f32::INFINITY, 1.0], &opts);
        let (u, v) = multi.sample(66.0, 80.0);
        assert!((u - 6.0).abs() <= 1.0, "u = {u}, expected ~6");
        assert!(v.abs() <= 1.0, "v = {v}, expected ~0");
    }

    #[test]
    fn blend_with_previous_applies_per_block_alpha() {
        let frame = disc_frame(128, 128, 60.0, 60.0, 10.0, 35.0);
        let moved = disc_frame(128, 128, 66.0, 60.0, 10.0, 35.0);
        let mut new = estimate_motion(&frame, &moved, &MotionOptions::default());
        let mut prev = new.clone();
        // Previous generation thought everything moved (0, 8).
        for j in 0..prev.u.len() {
            prev.u[j] = 0.0;
            prev.v[j] = 8.0;
        }
        let before_u = new.u.clone();
        let before_v = new.v.clone();
        new.blend_with_previous(&prev, 0.7, 0.4);
        for j in 0..new.u.len() {
            let a = if new.measured[j] { 0.7 } else { 0.4 };
            assert!((new.u[j] - a * before_u[j]).abs() < 1e-4);
            assert!((new.v[j] - (a * before_v[j] + (1.0 - a) * 8.0)).abs() < 1e-4);
        }
    }

    #[test]
    fn blend_with_previous_is_noop_on_dimension_mismatch() {
        let a0 = disc_frame(128, 128, 60.0, 60.0, 10.0, 35.0);
        let mut a = estimate_motion(&a0, &a0, &MotionOptions::default());
        let b0 = disc_frame(64, 64, 30.0, 30.0, 8.0, 35.0);
        let b = estimate_motion(&b0, &b0, &MotionOptions::default());
        let before = a.u.clone();
        a.blend_with_previous(&b, 0.7, 0.4);
        assert_eq!(a.u, before, "mismatched grids must not blend");
    }

    #[test]
    fn recovers_pure_translation() {
        let prev = disc_frame(200, 200, 80.0, 120.0, 12.0, 40.0);
        let next = disc_frame(200, 200, 86.0, 116.0, 12.0, 40.0); // moved (+6, -4)
        let opts = MotionOptions {
            search_radius: 10,
            ..MotionOptions::default()
        };
        let field = estimate_motion(&prev, &next, &opts);
        let (u, v) = field.sample(86.0, 116.0);
        assert!((u - 6.0).abs() <= 1.0, "u = {u}, expected ~6");
        assert!((v + 4.0).abs() <= 1.0, "v = {v}, expected ~-4");
    }

    #[test]
    fn identical_frames_yield_zero_motion() {
        let frame = disc_frame(128, 128, 60.0, 60.0, 10.0, 35.0);
        let field = estimate_motion(&frame, &frame, &MotionOptions::default());
        for (&u, &v) in field.u.iter().zip(&field.v) {
            assert!(
                u.abs() < 0.5 && v.abs() < 0.5,
                "expected ~zero, got ({u},{v})"
            );
        }
    }

    #[test]
    fn empty_frames_yield_zero_field_not_noise() {
        let a = Grid::filled_nodata(96, 96);
        let b = Grid::filled_nodata(96, 96);
        let field = estimate_motion(&a, &b, &MotionOptions::default());
        assert!(field.measured.iter().all(|&m| !m));
        assert!(field.u.iter().chain(&field.v).all(|&c| c == 0.0));
    }

    /// Frame sampled from `f` at pixel centres, so a synthetic displacement
    /// can be any real number of pixels.
    fn frame_of(w: usize, h: usize, f: impl Fn(f32, f32) -> f32) -> Grid {
        let data = (0..w * h)
            .map(|i| f((i % w) as f32 + 0.5, (i / w) as f32 + 0.5))
            .collect();
        Grid::new(w, h, data)
    }

    /// Smooth isotropic echo: a Gaussian of `peak` dBZ, sigma `s` px.
    fn gaussian(x: f32, y: f32, cx: f32, cy: f32, s: f32, peak: f32) -> f32 {
        peak * (-((x - cx).powi(2) + (y - cy).powi(2)) / (2.0 * s * s)).exp()
    }

    /// Textured echo, every pixel above `min_echo`, so every block it covers
    /// yields a vector.
    fn texture(x: f32, y: f32) -> f32 {
        30.0 + 8.0 * (0.31 * x + 0.17 * y).sin()
            + 7.0 * (0.13 * x - 0.29 * y + 1.3).sin()
            + 5.0 * (0.47 * x + 0.41 * y + 0.7).sin()
    }

    #[test]
    fn recovers_sub_pixel_translation() {
        // Integer matching alone lands 0.3 / 0.4 px off this shift — about
        // 7 / 10 px of drift after 24 leads. Asserted where the echo is well
        // resolved: faint-tail blocks carry the SAD fit's own bias.
        let (su, sv) = (3.3f32, -2.6f32);
        // Centred in block (2, 2) of a 5×5-block frame.
        let blob = |x: f32, y: f32| gaussian(x, y, 80.0, 80.0, 10.0, 50.0);
        let prev = frame_of(160, 160, blob);
        let next = frame_of(160, 160, |x, y| blob(x - su, y - sv));
        let opts = MotionOptions {
            search_radius: 8,
            ..MotionOptions::default()
        };
        let field = estimate_motion(&prev, &next, &opts);
        let centre = 2 * field.bw + 2;
        assert!(field.measured[centre]);
        let (u, v) = (field.u[centre], field.v[centre]);
        assert!(
            (u - su).abs() < 0.1 && (v - sv).abs() < 0.1,
            "({u}, {v}) vs true ({su}, {sv})"
        );
        let (u, v) = field.sample(80.0, 80.0);
        assert!(
            (u - su).abs() < 0.1 && (v - sv).abs() < 0.1,
            "sampled ({u}, {v})"
        );
    }

    #[test]
    fn subpixel_offset_is_the_parabola_vertex_and_clamped() {
        // Symmetric → no offset; costs of (x - 0.25)² at -1, 0, 1 → +0.25.
        assert_eq!(subpixel_offset(Some(2.0), 1.0, Some(2.0)), 0.0);
        let c = |x: f32| (x - 0.25).powi(2);
        assert!((subpixel_offset(Some(c(-1.0)), c(0.0), Some(c(1.0))) - 0.25).abs() < 1e-6);
        // A neighbour below the "minimum" (search-window edge) clamps.
        assert_eq!(subpixel_offset(Some(5.0), 1.0, Some(0.5)), 0.5);
        // Unscorable neighbour or a non-convex surface: stay on the integer.
        assert_eq!(subpixel_offset(None, 1.0, Some(2.0)), 0.0);
        assert_eq!(subpixel_offset(Some(1.0), 1.0, Some(1.0)), 0.0);
    }

    #[test]
    fn measured_edge_block_keeps_its_vector() {
        // Ambient echo on the left moves (+4, 0); an isolated cell at the
        // echo edge moves (+5, +1) — inside the outlier gate, so it stays
        // measured. Smoothing used to average it with the ambient flow.
        let ambient = |x: f32, y: f32| if x < 96.0 { texture(x, y) } else { 0.0 };
        let cell = |x: f32, y: f32| gaussian(x, y, 144.0, 48.0, 4.0, 50.0);
        let prev = frame_of(256, 128, |x, y| ambient(x, y).max(cell(x, y)));
        let next = frame_of(256, 128, |x, y| {
            ambient(x - 4.0, y).max(cell(x - 5.0, y - 1.0))
        });
        let opts = MotionOptions {
            search_radius: 8,
            ..MotionOptions::default()
        };
        let field = estimate_motion(&prev, &next, &opts);
        let edge = field.bw + 4; // block (4, 1) holds the cell
        assert!(field.measured[edge], "the cell's block must be measured");
        assert!(
            field.measured[field.bw + 2],
            "precondition: ambient echo is measured next door"
        );
        assert!(
            (field.u[edge] - 5.0).abs() < 0.1 && (field.v[edge] - 1.0).abs() < 0.1,
            "edge vector diluted to ({}, {})",
            field.u[edge],
            field.v[edge]
        );
    }

    #[test]
    fn local_gate_keeps_differential_flow() {
        // Most of the domain moves (+6, 0); the lower-right blocks move
        // (-2, 0). One global median/MAD saw MAD 0 and a 2 px gate, and
        // rejected the whole minority flow as outliers.
        let minority = |x: f32, y: f32| x >= 224.0 && y >= 128.0;
        let prev = frame_of(384, 256, texture);
        let next = frame_of(384, 256, |x, y| {
            let u = if minority(x, y) { -2.0 } else { 6.0 };
            texture(x - u, y)
        });
        let opts = MotionOptions {
            search_radius: 8,
            ..MotionOptions::default()
        };
        let field = estimate_motion(&prev, &next, &opts);
        // Block (9, 6): the middle of the minority blocks (7..12 × 4..8).
        let centre = 6 * field.bw + 9;
        assert!(field.measured[centre], "minority flow rejected as outlier");
        assert!(
            (field.u[centre] + 2.0).abs() < 0.1,
            "u = {}",
            field.u[centre]
        );
        // The majority keeps its own vector.
        let far = field.bw + 1;
        assert!(field.measured[far] && (field.u[far] - 6.0).abs() < 0.1);
    }
}
