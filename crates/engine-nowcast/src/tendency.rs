//! Intensity-based growth/decay profiles (#546, v2.3) — the NowPrecip-2
//! lineage (Sideris et al., QJRMS 2026): classical, per-intensity-band
//! tendencies measured in the Lagrangian (storm-following) frame.
//!
//! Measurement: advect the previous frame one interval along the motion
//! field, compare with the actual frame — the per-band mean difference is
//! growth/decay net of motion. Application: adjust each advected value by
//! its band's tendency integrated over the lead with an e-folding damping,
//! so short leads evolve and long leads relax toward pure advection
//! instead of running away. This attacks pure advection's biggest lie:
//! cells never die.

use crate::Grid;

/// Intensity band width (physical units, dBZ for radar).
pub const BAND_WIDTH: f32 = 2.0;
/// Number of bands above the profile base (covers base .. base+64 dBZ).
pub const N_BANDS: usize = 32;
/// Minimum pixels in a band for its tendency to be trusted; sparser bands
/// keep tendency 0 (pure advection).
pub const MIN_BAND_SAMPLES: u32 = 200;
/// Per-interval tendency clamp — a band never brightens/dims faster than
/// this, keeping one noisy scene from painting extremes.
pub const MAX_TENDENCY: f32 = 2.0;
/// e-folding of the applied tendency, in source intervals (3 × 5 min =
/// 15 min): `delta(lead) = tendency × efold × (1 − exp(−lead/efold))`.
///
/// GATE STATUS (#546, 2026-07-25): scene-wide profiles FAILED the V2.1
/// harness on the stratiform SMHI hindcast — worse than pure advection at
/// every lead/threshold (e.g. +60 min/20 dBZ CSI 0.305 vs 0.339; a 45-min
/// e-fold was far worse still, 0.118). The mechanism therefore ships
/// OFF by default (`growth_decay = false`) pending localization
/// (NowPrecip 2 applies profiles regionally, not scene-wide) or the
/// learned route (V2.5). Do not enable in prod until a gate run passes.
pub const EFOLD_INTERVALS: f32 = 3.0;
/// Cross-generation EMA weight for the new profile (same spirit as the
/// motion-field EMA — damps single-scene noise).
pub const PROFILE_EMA_ALPHA: f32 = 0.6;

/// Per-band mean intensity tendency (physical units per source interval).
#[derive(Debug, Clone)]
pub struct GrowthProfile {
    /// Band 0 starts here (typically `min_echo`).
    pub base: f32,
    pub tendency: [f32; N_BANDS],
}

impl GrowthProfile {
    pub fn zero(base: f32) -> Self {
        Self {
            base,
            tendency: [0.0; N_BANDS],
        }
    }

    #[inline]
    fn band(&self, value: f32) -> Option<usize> {
        if !value.is_finite() || value < self.base {
            return None;
        }
        let b = ((value - self.base) / BAND_WIDTH) as usize;
        Some(b.min(N_BANDS - 1))
    }

    /// Measure the profile from an advected-previous vs actual frame pair
    /// (both on the same grid). Pixels are binned by the ADVECTED value —
    /// the same key later used at application time — and each band's mean
    /// `(actual − advected)` becomes its tendency, clamped to
    /// [`MAX_TENDENCY`]. Bands with fewer than [`MIN_BAND_SAMPLES`] pixels
    /// stay 0.
    pub fn measure(advected_prev: &Grid, actual: &Grid, base: f32) -> Self {
        let mut sums = [0f64; N_BANDS];
        let mut counts = [0u32; N_BANDS];
        let profile = Self::zero(base);
        for (&a, &o) in advected_prev.data.iter().zip(&actual.data) {
            if !a.is_finite() || !o.is_finite() {
                continue;
            }
            if let Some(b) = profile.band(a) {
                // Floor sub-base actuals one band BELOW base (not at base
                // itself) so decay out of the lowest band still registers
                // as roughly one band's worth of loss instead of zero.
                sums[b] += f64::from(o.max(base - BAND_WIDTH) - a);
                counts[b] += 1;
            }
        }
        let mut tendency = [0f32; N_BANDS];
        let mut measured = [false; N_BANDS];
        for b in 0..N_BANDS {
            if counts[b] >= MIN_BAND_SAMPLES {
                tendency[b] =
                    ((sums[b] / f64::from(counts[b])) as f32).clamp(-MAX_TENDENCY, MAX_TENDENCY);
                measured[b] = true;
            }
        }
        // Fill unmeasured bands from the nearest measured neighbour (≤ 2
        // bands away): measurement keys by the PREVIOUS value, application
        // by the CURRENT one — in an evolving scene those routinely sit in
        // adjacent bands, and an unmeasured hole there would silently
        // disable the profile exactly where it matters.
        let filled = tendency;
        for b in 0..N_BANDS {
            if measured[b] {
                continue;
            }
            for d in 1..=2usize {
                let lower = b.checked_sub(d).filter(|&i| measured[i]);
                let upper = (b + d < N_BANDS && measured[b + d]).then_some(b + d);
                if let Some(src) = lower.or(upper) {
                    tendency[b] = filled[src];
                    break;
                }
            }
        }
        Self { base, tendency }
    }

    /// Cross-generation EMA: `self = alpha·self + (1−alpha)·prev`. No-op on
    /// a base mismatch (config change).
    pub fn blend_with_previous(&mut self, prev: &GrowthProfile, alpha: f32) {
        if (self.base - prev.base).abs() > f32::EPSILON {
            return;
        }
        for (t, p) in self.tendency.iter_mut().zip(&prev.tendency) {
            *t = alpha * *t + (1.0 - alpha) * p;
        }
    }

    /// Adjusted value at `lead` intervals: the band tendency integrated
    /// under e-folding damping. Values below the base (or nodata) pass
    /// through untouched.
    #[inline]
    pub fn apply(&self, value: f32, lead: f32) -> f32 {
        if self.band(value).is_none() {
            return value;
        }
        // Linear interpolation between band centres removes the
        // discretization cliff at band boundaries.
        let f = ((value - self.base) / BAND_WIDTH - 0.5).clamp(0.0, (N_BANDS - 1) as f32 - 1e-3);
        let (lo, frac) = (f as usize, f.fract());
        let hi = (lo + 1).min(N_BANDS - 1);
        let t = self.tendency[lo] * (1.0 - frac) + self.tendency[hi] * frac;
        value + t * EFOLD_INTERVALS * (1.0 - (-lead / EFOLD_INTERVALS).exp())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: usize, h: usize, v: f32) -> Grid {
        Grid::new(w, h, vec![v; w * h])
    }

    #[test]
    fn measures_uniform_decay_and_applies_with_efold() {
        // Advected says 40 dBZ everywhere; reality is 38.5 → tendency −1.5.
        let advected = flat(200, 200, 40.0);
        let actual = flat(200, 200, 38.5);
        let p = GrowthProfile::measure(&advected, &actual, 10.0);
        let b = ((40.0 - 10.0) / BAND_WIDTH) as usize;
        assert!((p.tendency[b] + 1.5).abs() < 1e-3);

        // Lead 1 ≈ raw tendency; long leads saturate at tendency × efold.
        let v1 = p.apply(40.0, 1.0);
        assert!(
            (v1 - (40.0 - 1.5 * EFOLD_INTERVALS * (1.0 - (-1.0f32 / EFOLD_INTERVALS).exp()))).abs()
                < 1e-3
        );
        let v_inf = p.apply(40.0, 1000.0);
        assert!((v_inf - (40.0 - 1.5 * EFOLD_INTERVALS)).abs() < 1e-2);
        assert!(v1 > v_inf, "damped integral is monotone in lead");
        // Below-base values untouched.
        assert_eq!(p.apply(5.0, 3.0), 5.0);
    }

    #[test]
    fn sparse_bands_and_clamps_stay_conservative() {
        // Tiny frame: under MIN_BAND_SAMPLES → tendency 0.
        let p = GrowthProfile::measure(&flat(10, 10, 40.0), &flat(10, 10, 20.0), 10.0);
        assert!(p.tendency.iter().all(|&t| t == 0.0));
        // Huge change clamps to MAX_TENDENCY.
        let p = GrowthProfile::measure(&flat(200, 200, 40.0), &flat(200, 200, 10.0), 10.0);
        let b = ((40.0 - 10.0) / BAND_WIDTH) as usize;
        assert_eq!(p.tendency[b], -MAX_TENDENCY);
    }

    #[test]
    fn ema_blends_and_skips_base_mismatch() {
        let mut a = GrowthProfile::zero(10.0);
        a.tendency[3] = 1.0;
        let mut b = GrowthProfile::zero(10.0);
        b.tendency[3] = -1.0;
        a.blend_with_previous(&b, 0.6);
        assert!((a.tendency[3] - 0.2).abs() < 1e-6);
        let c = GrowthProfile::zero(20.0);
        let before = a.tendency[3];
        a.blend_with_previous(&c, 0.6);
        assert_eq!(a.tendency[3], before);
    }
}
