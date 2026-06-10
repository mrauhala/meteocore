//! Separable 3-D smoothing of a cylindrical voxel field in its native
//! `[radius, angle, height]` index order (#381) — shared by the voxel encoder
//! (which dissolves the cell lattice before GPU trilinear interpolation) and
//! the isosurface mesher (which otherwise inherits the lattice as stair-step
//! artifacts along the extracted shell).
//!
//! Radar echo is **cellular**: each ~native-resolution cell is a local
//! reflectivity maximum, so any C0 reconstruction (trilinear sampling, linear
//! edge interpolation in marching tetrahedra) shows the cell boundaries.
//! Repeated passes of a `[0.25, 0.5, 0.25]` kernel along each axis widen the
//! effective Gaussian (`sigma ≈ sqrt(passes/2)` cells), dropping cell-to-cell
//! contrast until the field reads as a continuous volume. Angle wraps (full
//! circle); radius/height clamp at the ends.

use ds_core::volume::VoxelGrid;

/// Smooth a **dense** (all-finite) field: `passes` applications of the
/// `[0.25, 0.5, 0.25]` kernel along each axis. Callers fill `NaN` cells first
/// (the voxel encoder's no-echo floor, the isosurface's `background` seal).
///
/// Each axis sweep ping-pongs `src`↔`dst`, so the result is always in `src`
/// regardless of the pass count.
pub(crate) fn smooth_grid(vals: Vec<f32>, dims: [usize; 3], passes: usize) -> Vec<f32> {
    // A NaN slipping in would propagate to every touched neighbour and silently
    // empty the output (debug-only: a full scan of up to MAX_VOXELS cells).
    debug_assert!(
        vals.iter().all(|v| v.is_finite()),
        "smooth_grid expects a dense (all-finite) field; seal NaN first or use smooth_grid_nan_aware"
    );
    smooth_with(vals, dims, passes, |lo, mid, hi| {
        0.25 * lo + 0.5 * mid + 0.25 * hi
    })
}

/// Smooth a field that still contains `NaN` (unmeasured) cells **without
/// eroding them**: a `NaN` cell stays `NaN`, and a finite cell averages only
/// its finite neighbours (the kernel weights renormalize over what's present).
/// This keeps the isosurface's open-boundary semantics (`background = None`
/// skips any tetrahedron touching a `NaN` corner) intact — the smoothing
/// neither fabricates values inside the unmeasured region nor lets the
/// sentinel bleed into real echo.
pub(crate) fn smooth_grid_nan_aware(vals: Vec<f32>, dims: [usize; 3], passes: usize) -> Vec<f32> {
    smooth_with(vals, dims, passes, |lo, mid, hi| {
        if !mid.is_finite() {
            return f32::NAN;
        }
        let mut sum = 0.5 * mid;
        let mut weight = 0.5_f32;
        if lo.is_finite() {
            sum += 0.25 * lo;
            weight += 0.25;
        }
        if hi.is_finite() {
            sum += 0.25 * hi;
            weight += 0.25;
        }
        sum / weight
    })
}

/// The shared separable sweep: applies `blur(lo, mid, hi)` along height
/// (clamped), angle (periodic), and radius (clamped), `passes` times.
/// Monomorphized per blur kernel, so the dense path keeps its tight inner
/// loops. The loop orders are cache-driven: `h` (stride 1) is always
/// innermost and `r` (stride `n_a·n_h`) outermost, so every sweep walks the
/// three touched rows stride-1; the angle sweep hoists its periodic neighbour
/// indices out of the `h` loop.
fn smooth_with<F>(vals: Vec<f32>, dims: [usize; 3], passes: usize, blur: F) -> Vec<f32>
where
    F: Fn(f32, f32, f32) -> f32 + Copy,
{
    let [n_r, n_a, n_h] = dims;
    let idx = |r: usize, a: usize, h: usize| VoxelGrid::index_of(dims, r, a, h);

    let mut src = vals;
    let mut dst = vec![0.0f32; src.len()];

    for _ in 0..passes {
        // Height (clamp).
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 0..n_h {
                    let lo = src[idx(r, a, h.saturating_sub(1))];
                    let hi = src[idx(r, a, (h + 1).min(n_h - 1))];
                    dst[idx(r, a, h)] = blur(lo, src[idx(r, a, h)], hi);
                }
            }
        }
        std::mem::swap(&mut src, &mut dst);
        // Angle (periodic).
        for r in 0..n_r {
            for a in 0..n_a {
                let a_lo = (a + n_a - 1) % n_a;
                let a_hi = (a + 1) % n_a;
                for h in 0..n_h {
                    let lo = src[idx(r, a_lo, h)];
                    let hi = src[idx(r, a_hi, h)];
                    dst[idx(r, a, h)] = blur(lo, src[idx(r, a, h)], hi);
                }
            }
        }
        std::mem::swap(&mut src, &mut dst);
        // Radius (clamp).
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 0..n_h {
                    let lo = src[idx(r.saturating_sub(1), a, h)];
                    let hi = src[idx((r + 1).min(n_r - 1), a, h)];
                    dst[idx(r, a, h)] = blur(lo, src[idx(r, a, h)], hi);
                }
            }
        }
        std::mem::swap(&mut src, &mut dst);
    }
    src
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIMS: [usize; 3] = [5, 8, 7];

    fn fill<F: Fn(usize, usize, usize) -> f32>(f: F) -> Vec<f32> {
        let [n_r, n_a, n_h] = DIMS;
        let mut v = vec![0.0f32; n_r * n_a * n_h];
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 0..n_h {
                    v[VoxelGrid::index_of(DIMS, r, a, h)] = f(r, a, h);
                }
            }
        }
        v
    }

    #[test]
    fn linear_field_is_invariant_away_from_clamped_ends() {
        // The [0.25, 0.5, 0.25] kernel preserves linear functions exactly; only
        // the clamped boundary cells shift (the boundary perturbation travels
        // one cell inward per pass). value = h, two passes ⇒ cells 2..n_h−3 of
        // every column must be untouched.
        let vals = fill(|_, _, h| h as f32);
        let out = smooth_grid(vals, DIMS, 2);
        let [n_r, n_a, n_h] = DIMS;
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 2..n_h - 2 {
                    let v = out[VoxelGrid::index_of(DIMS, r, a, h)];
                    assert!((v - h as f32).abs() < 1e-5, "({r},{a},{h}) = {v}");
                }
            }
        }
    }

    #[test]
    fn angle_axis_wraps_periodically() {
        // value = f(angle) with a single spike at a = 0: after one pass the
        // spike must leak into BOTH angular neighbours, including a = n_a−1
        // (the wrap) — a clamped angle axis would leave the far side at 0.
        let [_, n_a, _] = DIMS;
        let vals = fill(|_, a, _| if a == 0 { 8.0 } else { 0.0 });
        let out = smooth_grid(vals, DIMS, 1);
        let at = |a: usize| out[VoxelGrid::index_of(DIMS, 2, a, 3)];
        assert!(at(1) > 0.0, "forward neighbour gets a share");
        assert!(
            (at(n_a - 1) - at(1)).abs() < 1e-6,
            "wrap neighbour gets the same share: {} vs {}",
            at(n_a - 1),
            at(1)
        );
    }

    #[test]
    fn nan_aware_preserves_nan_and_never_bleeds_it() {
        // A finite plateau bordered by NaN: the NaN cells must stay NaN (no
        // fabrication), and the finite cells must stay finite at the plateau
        // value (renormalized weights — averaging 40 with 40 is 40, regardless
        // of how many NaN neighbours were dropped).
        let vals = fill(|r, _, _| if r < 3 { 40.0 } else { f32::NAN });
        let out = smooth_grid_nan_aware(vals, DIMS, 2);
        let [n_r, n_a, n_h] = DIMS;
        for r in 0..n_r {
            for a in 0..n_a {
                for h in 0..n_h {
                    let v = out[VoxelGrid::index_of(DIMS, r, a, h)];
                    if r < 3 {
                        assert!((v - 40.0).abs() < 1e-4, "({r},{a},{h}) = {v}");
                    } else {
                        assert!(v.is_nan(), "({r},{a},{h}) = {v} must stay NaN");
                    }
                }
            }
        }
    }

    #[test]
    fn nan_aware_renormalizes_weights_exactly() {
        // Non-uniform values against a NaN boundary pin the renormalization
        // arithmetic itself (a uniform plateau can't — any weighting of equal
        // inputs returns the input). One pass over a [1, 1, 3] column
        // [NaN, 10, 30]: only the height sweep does real work (the radius and
        // angle axes are size 1, so their clamped/periodic neighbours are the
        // cell itself and blur(v, v, v) = v).
        //   cell 1: NaN neighbour dropped → (0.5·10 + 0.25·30) / 0.75 = 50/3
        //   cell 2: clamped end (hi = self) → 0.25·10 + 0.75·30 = 25
        //   cell 0: NaN stays NaN
        let dims = [1, 1, 3];
        let out = smooth_grid_nan_aware(vec![f32::NAN, 10.0, 30.0], dims, 1);
        assert!(out[0].is_nan(), "NaN cell must stay NaN: {}", out[0]);
        assert!(
            (out[1] - 50.0 / 3.0).abs() < 1e-4,
            "renormalized over finite neighbours only: {}",
            out[1]
        );
        assert!((out[2] - 25.0).abs() < 1e-4, "clamped end: {}", out[2]);
    }

    #[test]
    fn zero_passes_is_identity() {
        let vals = fill(|r, a, h| (r * 100 + a * 10 + h) as f32);
        assert_eq!(smooth_grid(vals.clone(), DIMS, 0), vals);
    }
}
