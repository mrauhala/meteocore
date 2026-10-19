//! Interpolation of scattered station values onto a grid: IDW, ADW and
//! nearest neighbour, over great-circle distances on the sphere of
//! [`ds_core::geo::EARTH_RADIUS_M`].
//!
//! Every method uses at most `max_neighbours` stations within `radius_m` of
//! the target; a target with none is `None` (nodata). A station within
//! [`COINCIDENT_M`] of the target is its value exactly (the mean, should
//! several share the point), so a cell centred on a station shows that
//! station's value.
//!
//! - **IDW** (Shepard 1968): weight `d^-p`.
//! - **ADW** (Shepard 1968's angular term, as in New et al. 2000 and Caesar
//!   et al. 2006): distance weight `w_k = exp(-d_k / x0)^m`, then
//!   `W_k = w_k (1 + a_k)` with `a_k = Σ_{l≠k} w_l (1 − cos θ_kl) /
//!   Σ_{l≠k} w_l`, where `θ_kl` is the angle at the target between the
//!   directions to stations `k` and `l`. A station in a direction no other
//!   station covers gains up to three times the weight of one inside a
//!   cluster, so a cluster counts less than its station count.
//! - **Nearest**: the nearest station's value, for categorical fields.
//!
//! Weights are scaled by the nearest station's (IDW: `(d_min / d)^p`, ADW:
//! `exp(-m (d - d_min) / x0)`), which leaves every ratio, and so the
//! estimate, unchanged and keeps them from underflowing.

use ds_core::geo::EARTH_RADIUS_M;

use crate::geometry::GridGeom;
use crate::kdtree::KdTree;

/// A station within this many metres of the target is the target's value.
pub const COINCIDENT_M: f64 = 1e-3;

/// How a value is estimated from the neighbouring stations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Method {
    /// Inverse distance weighting, `w = d^-power`.
    Idw { power: f64 },
    /// Angular distance weighting: `exp(-d / decay_m)^exponent`, times the
    /// angular-separation term.
    Adw { decay_m: f64, exponent: f64 },
    /// The nearest station's value.
    Nearest,
}

impl Method {
    /// Short name, as configured.
    pub fn name(&self) -> &'static str {
        match self {
            Method::Idw { .. } => "idw",
            Method::Adw { .. } => "adw",
            Method::Nearest => "nearest",
        }
    }
}

/// A method with its neighbourhood.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub method: Method,
    /// Stations a target is estimated from at most.
    pub max_neighbours: usize,
    /// Stations farther than this (great-circle metres) are not used.
    pub radius_m: f64,
}

/// One station's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Station {
    pub lon: f64,
    pub lat: f64,
    pub value: f64,
}

/// Stations indexed for neighbour search.
pub struct Stations {
    tree: KdTree,
    points: Vec<[f64; 3]>,
    values: Vec<f64>,
}

impl Stations {
    pub fn new(stations: &[Station]) -> Self {
        let points: Vec<[f64; 3]> = stations.iter().map(|s| unit(s.lon, s.lat)).collect();
        Self {
            tree: KdTree::new(&points),
            values: stations.iter().map(|s| s.value).collect(),
            points,
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Reusable buffers for [`estimate_at`].
#[derive(Default)]
pub struct Scratch {
    found: Vec<(f64, u32)>,
    neighbours: Vec<(f64, usize)>,
    weights: Vec<f64>,
    directions: Vec<[f64; 3]>,
}

/// The unit vector of `(lon, lat)` degrees.
pub fn unit(lon: f64, lat: f64) -> [f64; 3] {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

/// Great-circle metres from a chord of the unit sphere: `2R asin(c / 2)`,
/// the haversine distance ([`ds_core::geo::great_circle_distance_m`])
/// computed from the chord the neighbour search already has.
fn chord_to_m(chord2: f64) -> f64 {
    2.0 * EARTH_RADIUS_M * (chord2.sqrt() / 2.0).clamp(0.0, 1.0).asin()
}

/// The squared chord of a great-circle distance.
fn m_to_chord2(metres: f64) -> f64 {
    let half = (metres / (2.0 * EARTH_RADIUS_M)).min(std::f64::consts::FRAC_PI_2);
    let chord = 2.0 * half.sin();
    chord * chord
}

/// The estimate at `(lon, lat)`; `None` with no station within the radius.
pub fn estimate_at(
    stations: &Stations,
    settings: &Settings,
    lon: f64,
    lat: f64,
    scratch: &mut Scratch,
) -> Option<f64> {
    estimate(
        stations,
        settings,
        unit(lon, lat),
        m_to_chord2(settings.radius_m),
        scratch,
    )
}

fn estimate(
    stations: &Stations,
    settings: &Settings,
    target: [f64; 3],
    max_chord2: f64,
    scratch: &mut Scratch,
) -> Option<f64> {
    let k = match settings.method {
        Method::Nearest => 1,
        _ => settings.max_neighbours,
    };
    stations
        .tree
        .nearest(target, k, max_chord2, &mut scratch.found);
    if scratch.found.is_empty() {
        return None;
    }
    scratch.neighbours.clear();
    scratch.neighbours.extend(
        scratch
            .found
            .iter()
            .map(|&(c2, i)| (chord_to_m(c2), i as usize)),
    );
    let neighbours = &scratch.neighbours;
    let coincident: Vec<f64> = neighbours
        .iter()
        .take_while(|(d, _)| *d < COINCIDENT_M)
        .map(|&(_, i)| stations.values[i])
        .collect();
    if !coincident.is_empty() {
        return Some(coincident.iter().sum::<f64>() / coincident.len() as f64);
    }
    let d_min = neighbours[0].0;
    let weights = &mut scratch.weights;
    weights.clear();
    match settings.method {
        Method::Nearest => return Some(stations.values[neighbours[0].1]),
        Method::Idw { power } => {
            weights.extend(neighbours.iter().map(|&(d, _)| (d_min / d).powf(power)));
        }
        Method::Adw { decay_m, exponent } => {
            weights.extend(
                neighbours
                    .iter()
                    .map(|&(d, _)| (-exponent * (d - d_min) / decay_m).exp()),
            );
            // Directions from the target to each station in the target's
            // tangent plane: defined everywhere, poles included.
            let directions = &mut scratch.directions;
            directions.clear();
            directions.extend(neighbours.iter().map(|&(_, i)| {
                let s = stations.points[i];
                let along = s[0] * target[0] + s[1] * target[1] + s[2] * target[2];
                let t = [
                    s[0] - along * target[0],
                    s[1] - along * target[1],
                    s[2] - along * target[2],
                ];
                let norm = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
                if norm > 0.0 {
                    [t[0] / norm, t[1] / norm, t[2] / norm]
                } else {
                    [0.0; 3]
                }
            }));
            let total: f64 = weights.iter().sum();
            let angular: Vec<f64> = (0..weights.len())
                .map(|k| {
                    let others = total - weights[k];
                    if others <= 0.0 {
                        return 0.0;
                    }
                    let spread: f64 = (0..weights.len())
                        .filter(|&l| l != k)
                        .map(|l| {
                            let (a, b) = (directions[k], directions[l]);
                            weights[l] * (1.0 - (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]))
                        })
                        .sum();
                    spread / others
                })
                .collect();
            for (w, a) in weights.iter_mut().zip(angular) {
                *w *= 1.0 + a;
            }
        }
    }
    let total: f64 = weights.iter().sum();
    let weighted: f64 = weights
        .iter()
        .zip(neighbours)
        .map(|(w, &(_, i))| w * stations.values[i])
        .sum();
    (total > 0.0).then(|| weighted / total)
}

/// Every cell of `geom` estimated at its centre, row-major, NaN for nodata.
/// One pass over the cells on the calling thread; never per output pixel of
/// a map request.
pub fn interpolate_grid(stations: &Stations, settings: &Settings, geom: &GridGeom) -> Vec<f32> {
    let mut values = vec![f32::NAN; geom.cells()];
    if stations.is_empty() {
        return values;
    }
    let max_chord2 = m_to_chord2(settings.radius_m);
    let cols: Vec<(f64, f64)> = (0..geom.nx)
        .map(|c| {
            let lon = geom.lon(c).to_radians();
            (lon.cos(), lon.sin())
        })
        .collect();
    let mut scratch = Scratch::default();
    for row in 0..geom.ny {
        let lat = geom.lat(row).to_radians();
        let (cos_lat, sin_lat) = (lat.cos(), lat.sin());
        let out = &mut values[row as usize * geom.nx as usize..][..geom.nx as usize];
        for (cell, &(cos_lon, sin_lon)) in out.iter_mut().zip(&cols) {
            let target = [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat];
            if let Some(v) = estimate(stations, settings, target, max_chord2, &mut scratch) {
                *cell = v as f32;
            }
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::geo::great_circle_distance_m;

    fn idw() -> Settings {
        Settings {
            method: Method::Idw { power: 2.0 },
            max_neighbours: 8,
            radius_m: 500_000.0,
        }
    }

    fn adw() -> Settings {
        Settings {
            method: Method::Adw {
                decay_m: 300_000.0,
                exponent: 4.0,
            },
            ..idw()
        }
    }

    fn nearest() -> Settings {
        Settings {
            method: Method::Nearest,
            ..idw()
        }
    }

    fn st(lon: f64, lat: f64, value: f64) -> Station {
        Station { lon, lat, value }
    }

    fn at(stations: &[Station], settings: &Settings, lon: f64, lat: f64) -> Option<f64> {
        estimate_at(
            &Stations::new(stations),
            settings,
            lon,
            lat,
            &mut Scratch::default(),
        )
    }

    /// The chord form is the shared haversine distance.
    #[test]
    fn chord_distance_is_the_haversine_distance() {
        for (a, b) in [
            ((24.9, 60.2), (25.7, 62.2)),
            ((179.9, 10.0), (-179.9, 10.0)),
            ((0.0, 89.9), (180.0, 89.9)),
            ((10.0, -45.0), (-170.0, 44.0)),
        ] {
            let (pa, pb) = (unit(a.0, a.1), unit(b.0, b.1));
            let c2 = (0..3).map(|i| (pa[i] - pb[i]).powi(2)).sum::<f64>();
            let haversine = great_circle_distance_m(a.0, a.1, b.0, b.1);
            assert!((chord_to_m(c2) - haversine).abs() < 1e-6, "{a:?} {b:?}");
        }
        assert!((m_to_chord2(1000.0).sqrt() - 1000.0 / EARTH_RADIUS_M).abs() < 1e-12);
    }

    /// Every method reproduces a constant field, wherever a station is in
    /// reach, and leaves the rest nodata.
    #[test]
    fn constant_field_is_reproduced() {
        let stations = [
            st(20.0, 60.0, 7.5),
            st(22.0, 61.0, 7.5),
            st(25.0, 60.5, 7.5),
            st(21.0, 63.0, 7.5),
        ];
        let geom = GridGeom::new([18.0, 58.0, 28.0, 66.0], 0.25).unwrap();
        for mut settings in [idw(), adw(), nearest()] {
            settings.radius_m = 150_000.0;
            let grid = interpolate_grid(&Stations::new(&stations), &settings, &geom);
            let (filled, empty): (Vec<f32>, Vec<f32>) = grid.iter().partition(|v| v.is_finite());
            assert!(!filled.is_empty() && !empty.is_empty(), "{settings:?}");
            assert!(
                filled.iter().all(|&v| (v - 7.5).abs() < 1e-5),
                "{settings:?}"
            );
        }
    }

    /// IDW at a station's own location is that station's value, however
    /// close the other stations are; co-located stations average.
    #[test]
    fn a_station_location_is_its_value() {
        let stations = [
            st(24.0, 61.0, 1.0),
            st(24.05, 61.0, 100.0),
            st(23.9, 61.1, -50.0),
        ];
        for s in &stations {
            assert_eq!(at(&stations, &idw(), s.lon, s.lat), Some(s.value));
            assert_eq!(at(&stations, &adw(), s.lon, s.lat), Some(s.value));
        }
        let twins = [
            st(24.0, 61.0, 2.0),
            st(24.0, 61.0, 4.0),
            st(25.0, 61.0, 9.0),
        ];
        assert_eq!(at(&twins, &idw(), 24.0, 61.0), Some(3.0));
        // Just beside a station, IDW is close to but not exactly its value.
        let near = at(&stations, &idw(), 24.0001, 61.0).unwrap();
        assert!(near != 1.0 && (near - 1.0).abs() < 1.0, "{near}");
    }

    /// A layout symmetric about a target gives the mean at the target, and
    /// mirror-image targets get mirror-image estimates.
    #[test]
    fn symmetric_layouts_give_symmetric_fields() {
        let stations = [
            st(10.0, 1.0, 3.0),
            st(10.0, -1.0, 3.0),
            st(11.0, 0.0, 9.0),
            st(9.0, 0.0, 1.0),
        ];
        for settings in [idw(), adw()] {
            let centre = at(&stations, &settings, 10.0, 0.0).unwrap();
            assert!((centre - 4.0).abs() < 1e-9, "{settings:?}: {centre}");
            let north = at(&stations, &settings, 10.3, 0.4).unwrap();
            let south = at(&stations, &settings, 10.3, -0.4).unwrap();
            assert!(
                (north - south).abs() < 1e-9,
                "{settings:?}: {north} {south}"
            );
        }
        let geom = GridGeom::new([8.0, -2.0, 12.0, 2.0], 0.1).unwrap();
        let grid = interpolate_grid(&Stations::new(&stations), &idw(), &geom);
        let nx = geom.nx as usize;
        for row in 0..geom.ny as usize {
            let mirror = geom.ny as usize - 1 - row;
            for col in 0..nx {
                let (a, b) = (grid[row * nx + col], grid[mirror * nx + col]);
                assert!((a - b).abs() < 1e-4, "row {row} col {col}: {a} {b}");
            }
        }
    }

    /// ADW's angular term: a lone station opposite a cluster of three at the
    /// same distance keeps 3/8 of the weight (IDW gives it 1/4). With the
    /// cluster tight, each cluster station's term tends to 2/3 and the lone
    /// one's to 2: `3 / (3 + 3 × 5/3)`.
    #[test]
    fn adw_down_weights_a_cluster() {
        let lone = st(0.0, 1.0, 1.0);
        let cluster = [
            st(0.0, -1.0, 0.0),
            st(0.001, -1.0, 0.0),
            st(-0.001, -1.0, 0.0),
        ];
        let stations = [lone, cluster[0], cluster[1], cluster[2]];
        let idw_share = at(&stations, &idw(), 0.0, 0.0).unwrap();
        let adw_share = at(&stations, &adw(), 0.0, 0.0).unwrap();
        assert!((idw_share - 0.25).abs() < 1e-3, "{idw_share}");
        assert!((adw_share - 0.375).abs() < 1e-3, "{adw_share}");
    }

    /// Stations either side of the antimeridian neighbour each other: a cell
    /// at the seam sees both at their true, short distance.
    #[test]
    fn seam_neighbours_across_the_antimeridian() {
        let stations = [st(179.5, 15.0, 10.0), st(-179.5, 15.0, 20.0)];
        let mid = at(&stations, &idw(), 180.0, 15.0).unwrap();
        assert!((mid - 15.0).abs() < 1e-9, "{mid}");
        let geom = GridGeom::new([170.0, 10.0, -170.0, 20.0], 0.5).unwrap();
        let grid = interpolate_grid(&Stations::new(&stations), &idw(), &geom);
        let row = geom.row_at(geom.row_frac(15.1)).unwrap() as usize;
        let nx = geom.nx as usize;
        let west = grid[row * nx + geom.col_at(geom.col_frac(179.75)).unwrap() as usize];
        let east = grid[row * nx + geom.col_at(geom.col_frac(-179.75)).unwrap() as usize];
        assert!(
            west > 10.0 && west < 15.0 && east > 15.0 && east < 20.0,
            "{west} {east}"
        );
        assert!(
            (west + east - 30.0).abs() < 1e-3,
            "mirror pair: {west} {east}"
        );
    }

    /// Near a pole every longitude is close: stations on opposite meridians
    /// both reach a cell between them, and a cell at the pole itself, where
    /// every direction is south, is the mean of equidistant stations.
    #[test]
    fn near_the_pole() {
        let stations = [st(0.0, 89.0, 1.0), st(180.0, 89.0, 3.0)];
        for settings in [idw(), adw()] {
            let between = at(&stations, &settings, 90.0, 89.5).unwrap();
            assert!((between - 2.0).abs() < 1e-9, "{settings:?}: {between}");
            let pole = at(&stations, &settings, 37.0, 90.0).unwrap();
            assert!((pole - 2.0).abs() < 1e-9, "{settings:?}: {pole}");
        }
        let geom = GridGeom::new([-180.0, 80.0, 180.0, 90.0], 1.0).unwrap();
        let grid = interpolate_grid(&Stations::new(&stations), &idw(), &geom);
        let top: Vec<f32> = grid[..geom.nx as usize].to_vec();
        assert!(top.iter().all(|v| v.is_finite() && *v > 1.0 && *v < 3.0));
    }

    #[test]
    fn nothing_in_reach_is_nodata() {
        let stations = [st(0.0, 0.0, 5.0)];
        assert_eq!(at(&stations, &idw(), 10.0, 0.0), None);
        assert_eq!(at(&[], &idw(), 0.0, 0.0), None);
        assert_eq!(at(&stations, &nearest(), 1.0, 0.0), Some(5.0));
        let lots = [st(0.0, 0.0, 1.0), st(0.5, 0.0, 2.0), st(3.0, 0.0, 3.0)];
        let mut one = idw();
        one.max_neighbours = 1;
        assert_eq!(at(&lots, &one, 0.4, 0.0), Some(2.0));
    }
}
