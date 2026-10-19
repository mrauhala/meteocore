//! Axis helpers for the Zarr engine's reads.
//!
//! Pure functions, no I/O: the bad-chunking heuristic, and 1-D coordinate
//! location for bilinear sampling. CF semantics (time decoding, axis roles)
//! live in [`ds_core::cf`].

/// Whether a variable's chunk shape is pathological for point / time-series
/// access: each timestep is stored as a single full-domain spatial chunk, so a
/// one-point time series must decode the entire field for every timestep
/// (e.g. MUR-SST's `time=1, lat=full, lon=full`). Issue #125 asks us to warn
/// on this at startup.
pub fn is_bad_timeseries_chunking(
    time_chunk: u64,
    lat_chunk: u64,
    lon_chunk: u64,
    ny: u64,
    nx: u64,
    n_times: u64,
) -> bool {
    n_times > 1 && time_chunk <= 1 && lat_chunk >= ny && lon_chunk >= nx
}

/// Whether a 1-D axis is monotonic (non-strictly ascending or descending).
/// [`locate`] assumes this; a non-monotonic coordinate axis would otherwise
/// produce silently wrong bilinear interpolation, so callers validate up front.
pub fn is_monotonic(vals: &[f64]) -> bool {
    if vals.len() < 2 {
        return true;
    }
    let ascending = vals[1] >= vals[0];
    vals.windows(2).all(|w| {
        if ascending {
            w[1] >= w[0]
        } else {
            w[1] <= w[0]
        }
    })
}

/// Locate `target` within a monotonic (ascending or descending) 1-D
/// coordinate axis. Returns `(lo, hi, w)` such that the interpolated value is
/// `data[lo] * (1 - w) + data[hi] * w`, with `lo`/`hi` the bracketing indices
/// and `w ∈ [0, 1]` the weight toward `hi`.
///
/// Returns `None` when `target` lies outside the axis range.
///
/// A single-element axis always returns `(0, 0, 0.0)`, snapping *any* in-bounds
/// target to the sole cell regardless of distance. This is intentional: a
/// collapsed/degenerate axis — e.g. a zonal- or meridional-mean field stored
/// with a single lat or lon point — should return its one value for every
/// position along that axis, not nodata.
pub fn locate(axis: &[f64], target: f64) -> Option<(usize, usize, f64)> {
    let n = axis.len();
    if n == 0 || !target.is_finite() {
        return None;
    }
    if n == 1 {
        return Some((0, 0, 0.0));
    }
    // Monotonic axis (ascending or descending) → O(log n) bracket via binary
    // search. Both endpoints are inclusive, matching a fence-post linear scan
    // (a query exactly at the first or last coordinate is in-range).
    let ascending = axis[n - 1] >= axis[0];
    let (min, max) = if ascending {
        (axis[0], axis[n - 1])
    } else {
        (axis[n - 1], axis[0])
    };
    if target < min || target > max {
        return None;
    }
    // `partition_point` returns the first index whose value is >= target
    // (ascending) / <= target (descending). Clamp into a valid bracket: `pos`
    // is 0 only when target == the first element and `n` only when target ==
    // the last — both in range here, so the clamp picks the edge cell.
    let pos = axis.partition_point(|&v| if ascending { v < target } else { v > target });
    let j = pos.clamp(1, n - 1);
    let i = j - 1;
    let (a, b) = (axis[i], axis[j]);
    let w = if (b - a).abs() < f64::EPSILON {
        0.0
    } else {
        (target - a) / (b - a)
    };
    Some((i, j, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_ascending_and_descending() {
        let asc = [0.0, 1.0, 2.0, 3.0];
        let (lo, hi, w) = locate(&asc, 1.5).unwrap();
        assert_eq!((lo, hi), (1, 2));
        assert!((w - 0.5).abs() < 1e-9);

        let desc = [60.0, 59.0, 58.0, 57.0];
        let (lo, hi, w) = locate(&desc, 58.25).unwrap();
        assert_eq!((lo, hi), (1, 2)); // between 59 and 58
        assert!((w - 0.75).abs() < 1e-9); // 0.75 of the way from 59 toward 58

        assert!(locate(&asc, -1.0).is_none());
        assert!(locate(&asc, 9.0).is_none());
    }

    #[test]
    fn locate_single_element_snaps_any_target() {
        // A collapsed axis (one point — e.g. a zonal/meridional mean) returns
        // its only cell for any target, near or far. Documented, intentional.
        let one = [50.0];
        assert_eq!(locate(&one, 50.0), Some((0, 0, 0.0)));
        assert_eq!(locate(&one, -120.0), Some((0, 0, 0.0)));
        assert_eq!(locate(&one, 9999.0), Some((0, 0, 0.0)));
        // Non-finite is still rejected.
        assert!(locate(&one, f64::NAN).is_none());
    }

    #[test]
    fn locate_inclusive_endpoints() {
        // Exact endpoints must stay in range (the subtle case a naive
        // partition_point bracket gets wrong at the upper edge).
        let asc = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(locate(&asc, 0.0), Some((0, 1, 0.0))); // first element, w=0
        assert_eq!(locate(&asc, 3.0), Some((2, 3, 1.0))); // last element, w=1

        let desc = [60.0, 59.0, 58.0, 57.0];
        assert_eq!(locate(&desc, 60.0), Some((0, 1, 0.0)));
        assert_eq!(locate(&desc, 57.0), Some((2, 3, 1.0)));
    }

    #[test]
    fn is_monotonic_detects_both_directions() {
        assert!(is_monotonic(&[1.0, 2.0, 3.0]));
        assert!(is_monotonic(&[3.0, 2.0, 1.0]));
        assert!(is_monotonic(&[5.0])); // trivially
        assert!(!is_monotonic(&[1.0, 3.0, 2.0]));
    }

    #[test]
    fn bad_chunking_fires_on_full_spatial_chunk() {
        // time=1, lat=full, lon=full → pathological for time series.
        assert!(is_bad_timeseries_chunking(1, 1800, 3600, 1800, 3600, 24));
        // time spans many steps in one chunk, spatial split → fine.
        assert!(!is_bad_timeseries_chunking(24, 100, 100, 1800, 3600, 24));
        // single timestep → not a time-series concern.
        assert!(!is_bad_timeseries_chunking(1, 1800, 3600, 1800, 3600, 1));
    }
}
