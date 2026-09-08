//! The per-generation motion field as a served data product (#661):
//! block-centre samples of the estimated precipitation motion in physical
//! units, on the regular WGS84 grid the field was estimated on.
//!
//! [`crate::motion::MotionField`] vectors are working-grid **pixels per
//! source interval** with the row axis pointing south. A particle client
//! wants east/north **m/s** so one renderer can consume this and a model
//! wind field alike. The conversion needs only the grid geometry and the
//! interval — no I/O — so it lives here as a pure function the engine's
//! EDR `area` query calls.
//!
//! This is echo motion (steering-level storm motion plus propagation), not
//! surface wind. The parameter names and labels say so; keep them honest.

use ds_core::geo::EARTH_RADIUS_M;

use crate::motion::MotionField;

/// EDR parameter name: eastward component of precipitation motion, m/s.
pub const PARAM_U: &str = "motion_u";
/// EDR parameter name: northward component of precipitation motion, m/s.
pub const PARAM_V: &str = "motion_v";
/// EDR parameter name: 1 where the block was measured by block matching,
/// 0 where its vector came from neighbour fill / smoothing.
pub const PARAM_QUALITY: &str = "motion_quality";

/// All served parameters, in a stable order.
pub const PARAMS: [&str; 3] = [PARAM_U, PARAM_V, PARAM_QUALITY];

/// Metres per degree of latitude on the sphere [`EARTH_RADIUS_M`] uses —
/// the same Earth as `great_circle_distance_m`, so a client that checks a
/// served speed against two served centres agrees to the rounding.
fn metres_per_degree() -> f64 {
    EARTH_RADIUS_M * std::f64::consts::PI / 180.0
}

/// Geometry of the working WGS84 grid a field was estimated on: the
/// north-west corner and the (positive) cell sizes in degrees.
#[derive(Debug, Clone, Copy)]
pub struct GridSpec {
    pub west: f64,
    pub north: f64,
    /// Degrees of longitude per working-grid pixel.
    pub dlon: f64,
    /// Degrees of latitude per working-grid pixel (positive; rows go south).
    pub dlat: f64,
}

/// A block-centre subset of a motion field in physical units. `u`, `v` and
/// `quality` are row-major over `y` × `x` (row 0 = `y[0]`).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionGrid {
    /// Block-centre longitudes, ascending.
    pub x: Vec<f64>,
    /// Block-centre latitudes, **descending** (north first, matching the
    /// field's row order — the CoverageJSON Grid axis carries the values,
    /// so the order is free and this keeps `values` a plain copy).
    pub y: Vec<f64>,
    /// Eastward m/s.
    pub u: Vec<f64>,
    /// Northward m/s.
    pub v: Vec<f64>,
    /// 1.0 measured, 0.0 filled.
    pub quality: Vec<f64>,
}

/// Block centres of `field` whose centre lies inside `bbox`
/// (`[west, south, east, north]`, WGS84), converted to east/north m/s over
/// `interval_secs`. `None` when no centre falls inside the bbox.
///
/// A block's centre is at working-grid pixel `((b + 0.5) * block)` on each
/// axis — the same convention [`MotionField::sample`] interpolates
/// between, so a client bilinear-sampling these centres reproduces the
/// engine's own advection field.
pub fn motion_grid(
    field: &MotionField,
    spec: &GridSpec,
    interval_secs: f64,
    bbox: [f64; 4],
) -> Option<MotionGrid> {
    if field.bw == 0 || field.bh == 0 || interval_secs <= 0.0 || interval_secs.is_nan() {
        return None;
    }
    let centre_lon = |bx: usize| spec.west + (bx as f64 + 0.5) * field.block as f64 * spec.dlon;
    let centre_lat = |by: usize| spec.north - (by as f64 + 0.5) * field.block as f64 * spec.dlat;

    let cols: Vec<usize> = (0..field.bw)
        .filter(|&bx| {
            let lon = centre_lon(bx);
            lon >= bbox[0] && lon <= bbox[2]
        })
        .collect();
    let rows: Vec<usize> = (0..field.bh)
        .filter(|&by| {
            let lat = centre_lat(by);
            lat >= bbox[1] && lat <= bbox[3]
        })
        .collect();
    if cols.is_empty() || rows.is_empty() {
        return None;
    }

    let m_per_deg = metres_per_degree();
    let n = cols.len() * rows.len();
    let mut u = Vec::with_capacity(n);
    let mut v = Vec::with_capacity(n);
    let mut quality = Vec::with_capacity(n);
    for &by in &rows {
        let lat = centre_lat(by);
        // Metres per degree of longitude shrink with latitude; the row's
        // centre latitude is exact for the row's own centres.
        let m_per_deg_lon = m_per_deg * lat.to_radians().cos();
        for &bx in &cols {
            let j = by * field.bw + bx;
            let east = field.u[j] as f64 * spec.dlon * m_per_deg_lon / interval_secs;
            // Row index grows southward: a positive `v` is southward motion.
            let north = -(field.v[j] as f64) * spec.dlat * m_per_deg / interval_secs;
            u.push(east);
            v.push(north);
            quality.push(if field.measured[j] { 1.0 } else { 0.0 });
        }
    }
    Some(MotionGrid {
        x: cols.into_iter().map(centre_lon).collect(),
        y: rows.into_iter().map(centre_lat).collect(),
        u,
        v,
        quality,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3×2 blocks of 10 px on a 0.01°/px grid anchored at (20°E, 61°N).
    fn field() -> (MotionField, GridSpec) {
        let field = MotionField {
            block: 10,
            bw: 3,
            bh: 2,
            u: vec![2.0, 0.0, -1.0, 0.0, 0.0, 0.0],
            v: vec![0.0, 3.0, 0.0, 0.0, 0.0, 0.0],
            measured: vec![true, false, true, false, false, false],
        };
        let spec = GridSpec {
            west: 20.0,
            north: 61.0,
            dlon: 0.01,
            dlat: 0.01,
        };
        (field, spec)
    }

    #[test]
    fn block_centres_and_units() {
        let (field, spec) = field();
        let g = motion_grid(&field, &spec, 300.0, [-180.0, -90.0, 180.0, 90.0]).unwrap();
        assert_eq!(g.x, vec![20.05, 20.15, 20.25]);
        assert_eq!(g.y.len(), 2);
        assert!((g.y[0] - 60.95).abs() < 1e-9 && (g.y[1] - 60.85).abs() < 1e-9);
        assert_eq!(g.u.len(), 6);

        // +2 px east over 300 s: 2 × 0.01° × (m/° at 60.95°N) / 300 s.
        let m_per_deg = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
        let expect_u = 2.0 * 0.01 * m_per_deg * 60.95f64.to_radians().cos() / 300.0;
        assert!((g.u[0] - expect_u).abs() < 1e-9, "{} vs {expect_u}", g.u[0]);
        assert_eq!(g.v[0], 0.0);

        // +3 px down the rows = SOUTHWARD ⇒ negative northward m/s.
        let expect_v = -3.0 * 0.01 * m_per_deg / 300.0;
        assert!((g.v[1] - expect_v).abs() < 1e-9, "{} vs {expect_v}", g.v[1]);
        assert!(g.v[1] < 0.0);

        assert_eq!(g.quality, vec![1.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn bbox_selects_centres_and_empty_is_none() {
        let (field, spec) = field();
        // Only the middle column, only the top row.
        let g = motion_grid(&field, &spec, 300.0, [20.1, 60.9, 20.2, 61.0]).unwrap();
        assert_eq!(g.x, vec![20.15]);
        assert_eq!(g.y.len(), 1);
        assert_eq!(g.u.len(), 1);
        assert_eq!(g.quality, vec![0.0]);
        assert!(motion_grid(&field, &spec, 300.0, [0.0, 0.0, 1.0, 1.0]).is_none());
        assert!(motion_grid(&field, &spec, 0.0, [-180.0, -90.0, 180.0, 90.0]).is_none());
    }

    #[test]
    fn speed_is_consistent_with_great_circle_between_centres() {
        // Sanity: a vector of exactly one block eastward per interval should
        // report the great-circle distance between neighbouring centres per
        // interval, to well under a percent (haversine vs local cos scaling).
        let (mut field, spec) = field();
        field.u = vec![10.0; 6];
        field.v = vec![0.0; 6];
        let g = motion_grid(&field, &spec, 60.0, [-180.0, -90.0, 180.0, 90.0]).unwrap();
        let d = ds_core::geo::great_circle_distance_m(g.x[0], g.y[0], g.x[1], g.y[0]);
        let expect = d / 60.0;
        assert!(
            ((g.u[0] - expect) / expect).abs() < 1e-3,
            "{} vs {expect}",
            g.u[0]
        );
    }
}
