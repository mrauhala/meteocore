//! OGC API - EDR `cube` query machinery shared by the API layer and the
//! engines (#925): the output resolution a request asks for, the evenly
//! spaced positions it names along an axis, the nearest-neighbour index map
//! that resamples a native axis onto them, and the response budget across
//! timesteps × levels × cells × parameters.

use crate::error::DataServerError;
use crate::feature::MAX_AREA_VALUES;

/// The output sampling a cube query asks for along each axis: EDR 1.2
/// `resolution-x`, `resolution-y` and `resolution-z`.
///
/// `None` is the native resolution: the parameter was absent, or `0`, which
/// the standard defines as "all available data at the stored resolution".
/// `Some(n)` asks for `n` evenly spaced positions from the axis minimum to
/// its maximum, both included ([`axis_positions`]), each taking the value of
/// the nearest native coordinate ([`nearest_indices`]). The API layer never
/// passes `Some(0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CubeResolution {
    pub x: Option<usize>,
    pub y: Option<usize>,
    pub z: Option<usize>,
}

/// `n` evenly spaced positions from `min` to `max`, both included. EDR's
/// `resolution-x=10` "would retrieve 10 values along the x-axis from the
/// minimum x coordinate to maximum x coordinate (i.e. a value at both the
/// minimum x and maximum x coordinates and 8 values between)". The last
/// position is exactly `max`; `n == 1` is `[min]` and `n == 0` is empty.
pub fn axis_positions(min: f64, max: f64, n: usize) -> Vec<f64> {
    match n {
        0 => Vec::new(),
        1 => vec![min],
        _ => {
            let step = (max - min) / (n - 1) as f64;
            (0..n)
                .map(|i| {
                    if i == n - 1 {
                        max
                    } else {
                        min + i as f64 * step
                    }
                })
                .collect()
        }
    }
}

/// For each of `positions`, the index of the nearest `native` coordinate,
/// or `None` when even that one is farther than `tolerance`: a position past
/// the edge of the data is missing, never the edge value repeated. `native`
/// may be in any order (a requested level list is); ties go to the lower
/// index, and non-finite coordinates are never chosen.
///
/// Engines pass half their native spacing (plus rounding slack) as the
/// horizontal tolerance, and `f64::INFINITY` for levels, whose positions lie
/// between the selected levels by construction.
pub fn nearest_indices(native: &[f64], positions: &[f64], tolerance: f64) -> Vec<Option<usize>> {
    let mut sorted: Vec<(f64, usize)> = native
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .map(|(i, &v)| (v, i))
        .collect();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    positions
        .iter()
        .map(|&p| {
            if !p.is_finite() {
                return None;
            }
            // The nearest coordinate is the last one below `p` or the first
            // one at or above it.
            let k = sorted.partition_point(|&(v, _)| v < p);
            [k.checked_sub(1), Some(k)]
                .into_iter()
                .flatten()
                .filter_map(|c| sorted.get(c))
                .map(|&(v, i)| ((v - p).abs(), i))
                .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
                .filter(|&(d, _)| d <= tolerance)
                .map(|(_, i)| i)
        })
        .collect()
}

/// Enforce [`MAX_AREA_VALUES`], the shared gridded response budget, for a
/// cube response of `timesteps × levels × ny × nx × parameters` values.
/// Engines call it before reading or allocating the output.
pub fn check_cube_budget(
    timesteps: usize,
    levels: usize,
    ny: usize,
    nx: usize,
    parameters: usize,
) -> Result<(), DataServerError> {
    let total = timesteps
        .saturating_mul(levels)
        .saturating_mul(ny)
        .saturating_mul(nx)
        .saturating_mul(parameters);
    if total > MAX_AREA_VALUES {
        return Err(DataServerError::QueryTooLarge(format!(
            "Cube query would return {total} values ({timesteps} timesteps × {levels} levels × \
             {ny} × {nx} cells × {parameters} parameters); the limit is {MAX_AREA_VALUES} — \
             narrow the datetime window, z, the bbox, the resolution or the parameters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_include_both_ends() {
        assert_eq!(
            axis_positions(0.0, 9.0, 10),
            (0..10).map(f64::from).collect::<Vec<_>>()
        );
        assert_eq!(axis_positions(1000.0, 500.0, 3), [1000.0, 750.0, 500.0]);
        assert_eq!(axis_positions(2.0, 4.0, 1), [2.0]);
        assert!(axis_positions(2.0, 4.0, 0).is_empty());
        // The end is exact even where the step does not add up to it.
        let p = axis_positions(0.1, 0.7, 7);
        assert_eq!(p[6], 0.7);
    }

    #[test]
    fn nearest_is_tolerant_ordered_and_breaks_ties_low() {
        let native = [0.0, 1.0, 2.0];
        assert_eq!(
            nearest_indices(&native, &[-0.4, 0.5, 1.6, 2.5, f64::NAN], 0.5),
            [Some(0), Some(0), Some(2), Some(2), None]
        );
        // Past the edge by more than the tolerance: missing, not the edge.
        assert_eq!(nearest_indices(&native, &[-0.6, 2.51], 0.5), [None, None]);
        // Any order: a requested level list.
        let levels = [500.0, 1000.0, 850.0];
        assert_eq!(
            nearest_indices(&levels, &[500.0, 750.0, 900.0, 1000.0], f64::INFINITY),
            [Some(0), Some(2), Some(2), Some(1)]
        );
        assert_eq!(nearest_indices(&[], &[1.0], f64::INFINITY), [None]);
    }

    #[test]
    fn budget_counts_every_axis() {
        assert!(check_cube_budget(1, 10, 100, 1000, 1).is_ok());
        let err = check_cube_budget(2, 10, 100, 1000, 1).unwrap_err();
        assert!(matches!(err, DataServerError::QueryTooLarge(_)));
        assert!(err.to_string().contains("10 levels"), "{err}");
        assert!(check_cube_budget(usize::MAX, usize::MAX, 2, 2, 2).is_err());
    }
}
