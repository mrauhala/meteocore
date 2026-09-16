//! Offline comparisons for the nearest-sweep voxel sampler (#641).
//!
//! Native gate maxima/beam-centre heights are observations. The vertical
//! integral reference is an explicit beam-support MODEL, not ground-truth VIL:
//! each tilt has constant reflectivity across a supplied beam width, clipped
//! at adjacent-tilt midpoints so overlapping beams are not counted twice.
//! Gaps and masked gates contribute no measured depth (not measured zero).
//! No interpolation, resampling changes, or serving-path work lives here.

use ds_core::geo::{beam_height_at_ground, slant_to_ground_height};
use ds_core::volume::{VoxelGrid, NO_ECHO_FLOOR_DBZ};

use crate::pvol::{PolarMoment, Sweep};
use crate::reader::{PixelClass, RawPixels};

pub const TOP_THRESHOLDS_DBZ: [f64; 3] = [18.0, 45.0, 50.0];
const BAND_STARTS_M: [f64; 4] = [0.0, 50_000.0, 100_000.0, 150_000.0];

/// One decoded requested quantity. No fallback to another moment is allowed.
pub struct NativeSweep<'a> {
    pub sweep: &'a Sweep,
    pub moment: &'a PolarMoment,
    pub pixels: &'a RawPixels,
}

#[derive(Debug, Default, Clone)]
pub struct BandComparison {
    pub native_echo_gates: u64,
    pub native_peak_dbz: Option<f64>,
    /// Native peak retaining only the first requested-quantity sweep per tilt.
    pub first_tilt_peak_dbz: Option<f64>,
    /// Highest native beam centre above the antenna at 18/45/50 dBZ.
    pub native_top_m: [Option<f64>; 3],
    pub voxel_peak_dbz: Option<f64>,
    /// Highest voxel centre, not its top face, above the antenna.
    pub voxel_top_m: [Option<f64>; 3],
    pub finite_voxels: u64,
    /// Finite voxel centres outside all valid native beam segments at that
    /// column, including native nodata/quantity gaps. Count-weighted, not volume.
    pub finite_without_beam_support: u64,
    pub columns: u64,
    pub max_reference_integral_kg_m2: f64,
    pub max_voxel_integral_kg_m2: f64,
    /// Integral restricted to >=35 dBZ, exposing the cell-member cutoff.
    pub max_voxel_member_integral_kg_m2: f64,
    /// Paired-column difference; the two band maxima above may occur elsewhere.
    pub max_abs_integral_difference_kg_m2: f64,
}

fn band(ground_m: f64) -> usize {
    BAND_STARTS_M
        .iter()
        .rposition(|r| ground_m >= *r)
        .unwrap_or(0)
}

fn max_into(dst: &mut Option<f64>, value: f64) {
    if value.is_finite() {
        *dst = Some(dst.map_or(value, |old| old.max(value)));
    }
}

/// Liquid-water-content proxy used in VIL: 3.44e-6 * Z^(4/7), with
/// reflectivity capped at 56 dBZ as in the cell product. Multiplying by metres
/// gives kg/m². The explicit no-echo floor contributes zero, never rainfall.
fn water_content(dbz: f64) -> f64 {
    if !dbz.is_finite() || dbz <= NO_ECHO_FLOOR_DBZ as f64 {
        0.0
    } else {
        3.44e-6 * 10f64.powf(dbz.min(56.0) * 4.0 / 70.0)
    }
}

struct BeamSegment {
    low: f64,
    high: f64,
    density: f64,
}

/// Exact integration of a piecewise-constant beam-support model. Height
/// boundaries use the same core 4/3-Earth helper as cross-section coverage
/// floors (ground/cos(elevation) approximation). This is one declared
/// reference policy, not a substitute for radar beam/blockage quality metadata.
fn segments(
    native: &[NativeSweep<'_>],
    ground: f64,
    azimuth: f64,
    heights: [f64; 2],
    beamwidth_deg: f64,
) -> Vec<BeamSegment> {
    let mut out = Vec::with_capacity(native.len());
    for (i, n) in native.iter().enumerate() {
        let s = n.sweep;
        // Duplicate tilts are separate measurements, not extra beam depth.
        // Stable first-tilt ownership matches nearest_sweep's tie policy when
        // the requested quantity is present on that first sweep.
        if i > 0 && native[i - 1].sweep.elangle == s.elangle {
            continue;
        }
        let mut low_el = s.elangle - beamwidth_deg / 2.0;
        let mut high_el = s.elangle + beamwidth_deg / 2.0;
        if i > 0 {
            low_el = low_el.max((native[i - 1].sweep.elangle + s.elangle) / 2.0);
        }
        if let Some(next) = native[i + 1..].iter().find(|n| n.sweep.elangle > s.elangle) {
            high_el = high_el.min((next.sweep.elangle + s.elangle) / 2.0);
        }
        let low = beam_height_at_ground(low_el, ground).max(heights[0]);
        let high = beam_height_at_ground(high_el, ground).min(heights[1]);
        if high <= low {
            continue;
        }
        let slant = ground / s.elangle.to_radians().cos();
        let bin = ((slant - s.rstart) / s.rscale).floor();
        if bin < 0.0 || bin >= s.nbins as f64 {
            continue;
        }
        let ray = (azimuth / std::f64::consts::TAU * s.nrays as f64).floor() as usize % s.nrays;
        let density = match n.pixels.sample_class(
            ray,
            bin as usize,
            n.moment.gain,
            n.moment.offset,
            n.moment.nodata,
            Some(n.moment.undetect),
        ) {
            PixelClass::Value(v) if v.is_finite() => water_content(v),
            PixelClass::Undetect => 0.0,
            _ => continue,
        };
        out.push(BeamSegment { low, high, density });
    }
    out
}

/// Compare the actual engine grid with all decoded native gates and the beam
/// model, in ground-range bands 0–50, 50–100, 100–150, 150+ km. Native peaks/tops
/// are restricted to the grid's radius/height domain for a fair comparison.
/// All native sweeps must have the same requested reflectivity quantity,
/// valid geometry, and nondecreasing elevations. Repeated tilts contribute to
/// native maxima but only the first requested-quantity sweep owns beam depth. Reject ambiguous or
/// unreadable input instead of producing a plausible partial report.
pub fn compare(
    grid: &VoxelGrid,
    native: &[NativeSweep<'_>],
    beamwidth_deg: f64,
) -> Result<[BandComparison; 4], String> {
    if grid.unit != "dBZ" || native.is_empty() {
        return Err("comparison needs dBZ and at least one decoded sweep".into());
    }
    if !beamwidth_deg.is_finite() || !(0.0..=10.0).contains(&beamwidth_deg) || beamwidth_deg == 0.0
    {
        return Err("beam width must be finite and in (0, 10] degrees".into());
    }
    let [nr, na, nh] = grid.dims;
    if nr == 0
        || na == 0
        || nh == 0
        || nr.checked_mul(na).and_then(|v| v.checked_mul(nh)) != Some(grid.values.len())
        || grid.radius_range[0] != 0.0
        || grid.angle_range != [0.0, std::f64::consts::TAU]
        || !grid.radius_range[1].is_finite()
        || grid.radius_range[1] <= 0.0
        || !grid.height_range.iter().all(|v| v.is_finite())
        || grid.height_range[1] <= grid.height_range[0]
    {
        return Err("invalid full-cylinder voxel grid".into());
    }
    for (i, n) in native.iter().enumerate() {
        let s = n.sweep;
        if n.moment.quantity != grid.quantity
            || s.nrays == 0
            || s.nbins == 0
            || n.pixels.shape() != (s.nrays, s.nbins)
            || !s.rscale.is_finite()
            || s.rscale <= 0.0
            || !s.rstart.is_finite()
            || s.rstart < 0.0
            || !s.elangle.is_finite()
            || s.elangle.abs() + beamwidth_deg / 2.0 >= 90.0
            || !n.moment.gain.is_finite()
            || !n.moment.offset.is_finite()
            || (i > 0 && native[i - 1].sweep.elangle > s.elangle)
        {
            return Err(format!("invalid native sweep {i}: elevation={} previous={:?} quantity={} rays={} bins={} pixel_shape={:?} rstart={} rscale={}; need matching quantity/shape, valid geometry and nondecreasing elevations", s.elangle, i.checked_sub(1).map(|j| native[j].sweep.elangle), n.moment.quantity, s.nrays, s.nbins, n.pixels.shape(), s.rstart, s.rscale));
        }
    }
    let mut bands: [BandComparison; 4] = std::array::from_fn(|_| BandComparison::default());
    for (i, n) in native.iter().enumerate() {
        let s = n.sweep;
        for bin in 0..s.nbins {
            let slant = s.rstart + (bin as f64 + 0.5) * s.rscale;
            let (ground, height) = slant_to_ground_height(slant, s.elangle);
            if ground < 0.0
                || ground >= grid.radius_range[1]
                || height < grid.height_range[0]
                || height >= grid.height_range[1]
            {
                continue;
            }
            let b = &mut bands[band(ground)];
            for ray in 0..s.nrays {
                if let PixelClass::Value(v) = n.pixels.sample_class(
                    ray,
                    bin,
                    n.moment.gain,
                    n.moment.offset,
                    n.moment.nodata,
                    Some(n.moment.undetect),
                ) {
                    if !v.is_finite() {
                        continue;
                    }
                    b.native_echo_gates += 1;
                    max_into(&mut b.native_peak_dbz, v);
                    if i == 0 || native[i - 1].sweep.elangle != s.elangle {
                        max_into(&mut b.first_tilt_peak_dbz, v);
                    }
                    for (j, threshold) in TOP_THRESHOLDS_DBZ.iter().enumerate() {
                        if v >= *threshold {
                            max_into(&mut b.native_top_m[j], height);
                        }
                    }
                }
            }
        }
    }
    let dr = grid.radius_range[1] / nr as f64;
    let dh = (grid.height_range[1] - grid.height_range[0]) / nh as f64;
    for ir in 0..nr {
        let ground = (ir as f64 + 0.5) * dr;
        let b = &mut bands[band(ground)];
        for ia in 0..na {
            b.columns += 1;
            let azimuth = (ia as f64 + 0.5) * std::f64::consts::TAU / na as f64;
            let support = segments(native, ground, azimuth, grid.height_range, beamwidth_deg);
            let reference: f64 = support.iter().map(|s| s.density * (s.high - s.low)).sum();
            let (mut voxel, mut member) = (0.0, 0.0);
            for ih in 0..nh {
                let v = grid.values[grid.index(ir, ia, ih)] as f64;
                if !v.is_finite() {
                    continue;
                }
                let h = grid.height_range[0] + (ih as f64 + 0.5) * dh;
                b.finite_voxels += 1;
                if !support.iter().any(|s| h >= s.low && h < s.high) {
                    b.finite_without_beam_support += 1;
                }
                // Clear-air floor is measured support, not an echo peak.
                if v > NO_ECHO_FLOOR_DBZ as f64 {
                    max_into(&mut b.voxel_peak_dbz, v);
                }
                for (j, threshold) in TOP_THRESHOLDS_DBZ.iter().enumerate() {
                    if v >= *threshold {
                        max_into(&mut b.voxel_top_m[j], h);
                    }
                }
                let contribution = water_content(v) * dh;
                voxel += contribution;
                if v >= 35.0 {
                    member += contribution;
                }
            }
            b.max_reference_integral_kg_m2 = b.max_reference_integral_kg_m2.max(reference);
            b.max_voxel_integral_kg_m2 = b.max_voxel_integral_kg_m2.max(voxel);
            b.max_voxel_member_integral_kg_m2 = b.max_voxel_member_integral_kg_m2.max(member);
            b.max_abs_integral_difference_kg_m2 = b
                .max_abs_integral_difference_kg_m2
                .max((voxel - reference).abs());
        }
    }
    Ok(bands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;

    fn sweep(elangle: f64) -> Sweep {
        Sweep {
            elangle,
            nbins: 2,
            nrays: 2,
            rscale: 1000.0,
            rstart: 0.0,
            a1gate: 0,
            moments: vec![PolarMoment {
                quantity: "DBZH".into(),
                gain: 1.0,
                offset: 0.0,
                nodata: 65535.0,
                undetect: 65534.0,
                dataset_path: "test".into(),
            }],
        }
    }

    fn grid(values: Vec<f32>) -> VoxelGrid {
        VoxelGrid {
            origin_lon: 25.0,
            origin_lat: 60.0,
            origin_height: 100.0,
            dims: [1, 2, 2],
            radius_range: [0.0, 2000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 40.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        }
    }

    #[test]
    fn constant_beam_integral_and_unsupported_voxels() {
        let s = sweep(0.5);
        let p = RawPixels::U16(Array2::from_elem((2, 2), 40));
        let native = [NativeSweep {
            sweep: &s,
            moment: &s.moments[0],
            pixels: &p,
        }];
        let result = compare(&grid(vec![40.0; 4]), &native, 1.0).unwrap();
        let b = &result[0];
        assert_eq!(b.native_peak_dbz, Some(40.0));
        assert_eq!(b.native_echo_gates, 4);
        assert_eq!(b.finite_voxels, 4);
        assert_eq!(b.finite_without_beam_support, 2); // 30 m centres above beam
        assert_eq!(b.voxel_top_m, [Some(30.0), None, None]);
        // At 1 km a 1-degree-wide beam spans approximately 17.45 m.
        let density = 3.44e-6 * 10f64.powf(16.0 / 7.0);
        assert!((b.max_reference_integral_kg_m2 / density - 17.455).abs() < 0.01);
        assert!((b.max_voxel_integral_kg_m2 - density * 40.0).abs() < 1e-12);
        assert_eq!(
            b.max_voxel_member_integral_kg_m2,
            b.max_voxel_integral_kg_m2
        );
        assert!(
            (b.max_abs_integral_difference_kg_m2
                - (density * 40.0 - b.max_reference_integral_kg_m2))
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn undetect_is_supported_zero_but_nodata_is_unobserved() {
        let s = sweep(0.5);
        let p = RawPixels::U16(
            Array2::from_shape_vec((2, 2), vec![65534, 65534, 65535, 65535]).unwrap(),
        );
        let native = [NativeSweep {
            sweep: &s,
            moment: &s.moments[0],
            pixels: &p,
        }];
        let b = &compare(&grid(vec![NO_ECHO_FLOOR_DBZ; 4]), &native, 1.0).unwrap()[0];
        assert_eq!(b.finite_without_beam_support, 3);
        assert_eq!(b.native_echo_gates, 0);
        assert_eq!(b.native_peak_dbz, None);
        assert_eq!(b.voxel_peak_dbz, None);
        assert_eq!(b.max_reference_integral_kg_m2, 0.0);
        assert_eq!(b.max_voxel_integral_kg_m2, 0.0);
    }

    #[test]
    fn beam_model_preserves_gaps_and_does_not_double_count_overlap() {
        let low = sweep(0.5);
        let mut high = sweep(2.5);
        let p = RawPixels::U16(Array2::from_elem((2, 2), 40));
        let make = |s: &Sweep| {
            segments(
                &[
                    NativeSweep {
                        sweep: &low,
                        moment: &low.moments[0],
                        pixels: &p,
                    },
                    NativeSweep {
                        sweep: s,
                        moment: &s.moments[0],
                        pixels: &p,
                    },
                ],
                1000.0,
                0.0,
                [0.0, 100.0],
                1.0,
            )
        };
        let separated = make(&high);
        assert_eq!(separated.len(), 2);
        assert!(separated[1].low - separated[0].high > 17.0);
        high.elangle = 1.0;
        let overlapping = make(&high);
        assert_eq!(overlapping.len(), 2);
        assert_eq!(overlapping[0].high, overlapping[1].low);
    }

    #[test]
    fn repeated_tilts_keep_all_native_peaks_but_use_first_beam_once() {
        let s = sweep(0.5);
        let p = RawPixels::U16(Array2::from_elem((2, 2), 20));
        let strong = RawPixels::U16(Array2::from_elem((2, 2), 60));
        let native = [
            NativeSweep {
                sweep: &s,
                moment: &s.moments[0],
                pixels: &p,
            },
            NativeSweep {
                sweep: &s,
                moment: &s.moments[0],
                pixels: &strong,
            },
        ];
        let g = grid(vec![20.0; 4]);
        let both = compare(&g, &native, 1.0).unwrap();
        let first = compare(&g, &native[..1], 1.0).unwrap();
        assert_eq!(both[0].native_peak_dbz, Some(60.0));
        assert_eq!(both[0].first_tilt_peak_dbz, Some(20.0));
        assert_eq!(both[0].native_echo_gates, 8);
        assert_eq!(
            both[0].max_reference_integral_kg_m2,
            first[0].max_reference_integral_kg_m2
        );
        assert_eq!(both[0].max_voxel_member_integral_kg_m2, 0.0);
    }

    #[test]
    fn rejects_ambiguous_quantity_geometry_and_grid() {
        let mut s = sweep(0.5);
        let p = RawPixels::U16(Array2::from_elem((2, 2), 40));
        let g = grid(vec![40.0; 4]);
        let check = |s: &Sweep, g: &VoxelGrid, width| {
            compare(
                g,
                &[NativeSweep {
                    sweep: s,
                    moment: &s.moments[0],
                    pixels: &p,
                }],
                width,
            )
        };
        assert!(check(&s, &g, 1.0).is_ok());
        for width in [0.0, -1.0, f64::NAN, 11.0] {
            assert!(check(&s, &g, width).is_err());
        }
        assert!(check(&s, &grid(vec![40.0; 3]), 1.0).is_err());
        s.moments[0].quantity = "TH".into();
        assert!(check(&s, &g, 1.0).is_err());
        s.moments[0].quantity = "DBZH".into();
        s.nrays = 3;
        assert!(check(&s, &g, 1.0).is_err());
        s.nrays = 2;
        s.rscale = 0.0;
        assert!(check(&s, &g, 1.0).is_err());
    }
}
