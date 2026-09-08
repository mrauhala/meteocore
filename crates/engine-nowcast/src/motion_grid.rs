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

use crate::motion::MotionField;
use crate::KM_PER_DEG;

/// EDR parameter name: eastward component of precipitation motion, m/s.
pub const PARAM_U: &str = "motion_u";
/// EDR parameter name: northward component of precipitation motion, m/s.
pub const PARAM_V: &str = "motion_v";
/// EDR parameter name: 1 where the block was measured by block matching,
/// 0 where its vector came from neighbour fill / smoothing.
pub const PARAM_QUALITY: &str = "motion_quality";

/// Static description of one served parameter. Labels say "precipitation
/// motion" on purpose: radar echo motion is steering-level storm motion
/// plus propagation, not surface wind, and a client must not present it
/// as wind.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub observed_property: &'static str,
}

/// The served parameters, in a stable order — the ONE table both the
/// parameter list and the descriptions derive from, so a name cannot exist
/// without a description (or vice versa).
pub const PARAM_SPECS: [ParamSpec; 3] = [
    ParamSpec {
        name: PARAM_U,
        label: "Precipitation motion, eastward component",
        unit: "m/s",
        observed_property: "precipitation_motion_eastward",
    },
    ParamSpec {
        name: PARAM_V,
        label: "Precipitation motion, northward component",
        unit: "m/s",
        observed_property: "precipitation_motion_northward",
    },
    ParamSpec {
        name: PARAM_QUALITY,
        label: "Motion vector quality (1 = block-matched, 0 = filled from neighbours)",
        unit: "",
        observed_property: "precipitation_motion_quality",
    },
];

/// Look up a served parameter by name.
pub fn param_spec(name: &str) -> Option<&'static ParamSpec> {
    PARAM_SPECS.iter().find(|p| p.name == name)
}

/// Metres per degree of latitude — [`KM_PER_DEG`], the crate's single
/// named Earth constant, so the served m/s and the tracker's km-per-pixel
/// motion scale (`lonlat_grid_km_per_px`) agree to the constant. The one
/// deliberate difference: the tracker scales longitude by the grid's
/// MID-latitude cosine (a per-grid scalar), this product by each row's own
/// latitude — a 60–70°N grid spans a ~15% cos range and a served speed
/// should be right where it is served.
const METRES_PER_DEG: f64 = KM_PER_DEG * 1000.0;

/// Geometry of the working WGS84 grid a field was estimated on: the
/// north-west corner, the (positive) cell sizes in degrees, and the pixel
/// counts (a field's block grid is `div_ceil`, so its last block may hang
/// past the grid edge — the pixel counts are what the served centres are
/// clamped to).
#[derive(Debug, Clone, Copy)]
pub struct GridSpec {
    pub west: f64,
    pub north: f64,
    /// Degrees of longitude per working-grid pixel.
    pub dlon: f64,
    /// Degrees of latitude per working-grid pixel (positive; rows go south).
    pub dlat: f64,
    /// Working-grid pixels across.
    pub width: u32,
    /// Working-grid pixels down.
    pub height: u32,
}

/// A block-centre subset of a motion field in physical units. `u`, `v` and
/// `quality` are row-major over `y` × `x` (row 0 = `y[0]`).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionGrid {
    /// Block-centre longitudes, ascending.
    pub x: Vec<f64>,
    /// Block-centre latitudes, **ascending** (south first — the GRIB area
    /// query's convention, so a client that already consumes `10u`/`10v`
    /// indexes this identically; ODIM/GeoTIFF emit north-first, a
    /// pre-existing workspace inconsistency this product does not join).
    pub y: Vec<f64>,
    /// Eastward m/s.
    pub u: Vec<f64>,
    /// Northward m/s.
    pub v: Vec<f64>,
    /// 1.0 measured, 0.0 filled.
    pub quality: Vec<f64>,
}

/// Block centres of `field` for every block whose footprint overlaps
/// `bbox` (`[west, south, east, north]`, WGS84), plus one block of padding
/// on each side, converted to east/north m/s over `interval_secs`.
/// Overlap, not centre-inside: a bbox smaller than one block spacing still
/// gets the block it sits in. The padding is what lets a client bilinear-
/// sample right up to the bbox edge — the same enclosing-cell selection
/// GRIB's `extract_bbox` does. `None` when the bbox misses the grid.
///
/// A block's centre is at working-grid pixel `((b + 0.5) * block)` on each
/// axis — the same convention [`MotionField::sample`] interpolates
/// between, so a client bilinear-sampling these centres reproduces the
/// engine's own advection field. A trailing partial block's centre is
/// clamped to the last pixel so served coordinates never leave the
/// collection's spatial extent.
pub fn motion_grid(
    field: &MotionField,
    spec: &GridSpec,
    interval_secs: f64,
    bbox: [f64; 4],
) -> Option<MotionGrid> {
    if field.bw == 0 || field.bh == 0 || interval_secs <= 0.0 || interval_secs.is_nan() {
        return None;
    }
    let block = field.block as f64;
    let (w, h) = (spec.width as f64, spec.height as f64);
    let centre_lon =
        |bx: usize| spec.west + ((bx as f64 + 0.5) * block).min(w - 0.5).max(0.0) * spec.dlon;
    let centre_lat =
        |by: usize| spec.north - ((by as f64 + 0.5) * block).min(h - 0.5).max(0.0) * spec.dlat;
    // Block footprints, clipped to the grid; half-open overlap so a bbox
    // edge exactly on a block boundary does not pull in the neighbour.
    let cols: Vec<usize> = (0..field.bw)
        .filter(|&bx| {
            let x0 = spec.west + (bx as f64 * block) * spec.dlon;
            let x1 = spec.west + ((bx as f64 + 1.0) * block).min(w) * spec.dlon;
            x0 < bbox[2] && x1 > bbox[0]
        })
        .collect();
    let rows: Vec<usize> = (0..field.bh)
        .filter(|&by| {
            let y1 = spec.north - (by as f64 * block) * spec.dlat;
            let y0 = spec.north - ((by as f64 + 1.0) * block).min(h) * spec.dlat;
            y0 < bbox[3] && y1 > bbox[1]
        })
        .collect();
    if cols.is_empty() || rows.is_empty() {
        return None;
    }
    // Overlap selection is contiguous; pad by one block each side.
    let pad = |sel: &[usize], n: usize| -> Vec<usize> {
        let lo = sel[0].saturating_sub(1);
        let hi = (sel[sel.len() - 1] + 1).min(n - 1);
        (lo..=hi).collect()
    };
    let cols = pad(&cols, field.bw);
    // Rows come out south→north (ascending latitude): walk them reversed.
    let mut rows = pad(&rows, field.bh);
    rows.reverse();

    let m_per_deg = METRES_PER_DEG;
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
            width: 30,
            height: 20,
        };
        (field, spec)
    }

    #[test]
    fn block_centres_and_units() {
        let (field, spec) = field();
        let g = motion_grid(&field, &spec, 300.0, [-180.0, -90.0, 180.0, 90.0]).unwrap();
        assert_eq!(g.x, vec![20.05, 20.15, 20.25]);
        // y ascends: the field's row 0 (north, 60.95) is served LAST.
        assert_eq!(g.y.len(), 2);
        assert!((g.y[0] - 60.85).abs() < 1e-9 && (g.y[1] - 60.95).abs() < 1e-9);
        assert_eq!(g.u.len(), 6);

        // +2 px east over 300 s: 2 × 0.01° × (m/° at 60.95°N) / 300 s —
        // field row 0 ⇒ served row 1 ⇒ flat index 3.
        let m_per_deg = KM_PER_DEG * 1000.0;
        let expect_u = 2.0 * 0.01 * m_per_deg * 60.95f64.to_radians().cos() / 300.0;
        assert!((g.u[3] - expect_u).abs() < 1e-9, "{} vs {expect_u}", g.u[3]);
        assert_eq!(g.v[3], 0.0);

        // +3 px down the rows = SOUTHWARD ⇒ negative northward m/s.
        let expect_v = -3.0 * 0.01 * m_per_deg / 300.0;
        assert!((g.v[4] - expect_v).abs() < 1e-9, "{} vs {expect_v}", g.v[4]);
        assert!(g.v[4] < 0.0);

        assert_eq!(g.quality, vec![0.0, 0.0, 0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn bbox_selects_centres_and_empty_is_none() {
        let (field, spec) = field();
        // Exactly the middle column and the top row overlap; one block of
        // padding each side pulls in the neighbours (clamped at the grid).
        let g = motion_grid(&field, &spec, 300.0, [20.1, 60.9, 20.2, 61.0]).unwrap();
        assert_eq!(g.x, vec![20.05, 20.15, 20.25]);
        assert_eq!(g.y.len(), 2);
        assert_eq!(g.u.len(), 6);
        // A bbox touching only the west column pads east only.
        let g = motion_grid(&field, &spec, 300.0, [19.0, 60.0, 20.05, 62.0]).unwrap();
        assert_eq!(g.x, vec![20.05, 20.15]);
        assert!(motion_grid(&field, &spec, 300.0, [0.0, 0.0, 1.0, 1.0]).is_none());
        assert!(motion_grid(&field, &spec, 0.0, [-180.0, -90.0, 180.0, 90.0]).is_none());
    }

    #[test]
    fn sub_block_bbox_gets_the_block_it_sits_in() {
        // A bbox far smaller than one block spacing (a particle client's
        // tile, say) contains no block CENTRE but sits inside block (2, 1).
        let (field, spec) = field();
        let g = motion_grid(&field, &spec, 300.0, [20.26, 60.81, 20.27, 60.82]).unwrap();
        assert_eq!(g.x, vec![20.15, 20.25]);
        assert_eq!(g.y.len(), 2);
        assert!((g.y[0] - 60.85).abs() < 1e-9 && (g.y[1] - 60.95).abs() < 1e-9);
    }

    #[test]
    fn trailing_partial_block_centre_is_clamped_into_the_grid() {
        // 25 px wide with 10 px blocks: bw = 3, the last block covers px
        // 20..25 and its nominal centre (px 25) lies outside the grid.
        let (field, mut spec) = field();
        spec.width = 25;
        let g = motion_grid(&field, &spec, 300.0, [-180.0, -90.0, 180.0, 90.0]).unwrap();
        assert_eq!(g.x.len(), 3);
        assert!((g.x[2] - (20.0 + 24.5 * 0.01)).abs() < 1e-9, "{}", g.x[2]);
        assert!(g.x[2] < spec.west + 25.0 * spec.dlon);
    }

    #[test]
    fn speed_is_consistent_with_great_circle_between_centres() {
        // Sanity: a vector of exactly one block eastward per interval should
        // report the great-circle distance between neighbouring centres per
        // interval, to well under a percent. The residual is the deliberate
        // KM_PER_DEG (111.32) vs mean-sphere haversine (111.19) difference,
        // ~0.11%, plus local-cos vs great-circle — both far inside the
        // estimator's noise.
        let (mut field, spec) = field();
        field.u = vec![10.0; 6];
        field.v = vec![0.0; 6];
        let g = motion_grid(&field, &spec, 60.0, [-180.0, -90.0, 180.0, 90.0]).unwrap();
        let d = ds_core::geo::great_circle_distance_m(g.x[0], g.y[0], g.x[1], g.y[0]);
        let expect = d / 60.0;
        assert!(
            ((g.u[0] - expect) / expect).abs() < 2e-3,
            "{} vs {expect}",
            g.u[0]
        );
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[test]
    fn every_param_has_a_spec_and_names_are_unique() {
        for p in &PARAM_SPECS {
            assert!(param_spec(p.name).is_some());
        }
        let mut names: Vec<&str> = PARAM_SPECS.iter().map(|p| p.name).collect();
        names.dedup();
        assert_eq!(names.len(), PARAM_SPECS.len());
        assert!(param_spec("wind_u").is_none());
    }
}
