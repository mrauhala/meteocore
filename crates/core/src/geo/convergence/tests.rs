//! Grid convergence pinned against PROJ 9.8.1's `proj -V`, which prints the
//! convergence it finds by differentiating the projection numerically:
//!
//! ```text
//! echo "25 60" | proj -V +proj=lcc +lat_0=63.3 +lat_1=63.3 +lat_2=63.3 +lon_0=15 +R=6371000
//!   Convergence : 8d56'1.37" [ 8.93371388 ]
//! ```
//!
//! and its sign: positive where grid north lies east of true north.

use super::*;
use crate::geo::{polar_stereographic_k0, Ellipsoid};

/// PROJ prints eight decimals; its derivative is numerical.
const PROJ_TOLERANCE: f64 = 1e-6;

fn assert_pins(crs: &Crs, cases: &[(f64, f64, f64)]) {
    let convergence = Convergence::of(crs).expect("a conformal grid");
    for &(lon, lat, want) in cases {
        let got = convergence.degrees(lon, lat).unwrap();
        assert!(
            (got - want).abs() < PROJ_TOLERANCE,
            "{crs:?} at ({lon}, {lat}): {got} ≠ {want}"
        );
    }
}

fn lcc(lat1: f64, lat2: f64, lat0: f64, lon0: f64, radius: Option<f64>) -> Crs {
    Crs::LambertConformalConic {
        lat1: lat1.to_radians(),
        lat2: lat2.to_radians(),
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        false_e: 0.0,
        false_n: 0.0,
        radius,
    }
}

/// MEPS's grid mapping: one standard parallel, 63.3°, on the 6 371 km
/// sphere.
fn meps() -> Crs {
    lcc(63.3, 63.3, 63.3, 15.0, Some(6_371_000.0))
}

fn stere(lat0: f64, lon0: f64, k0: f64, figure: Ellipsoid) -> Crs {
    Crs::Stereographic {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        k0,
        false_e: 0.0,
        false_n: 0.0,
        figure,
    }
}

fn tm(lon0: f64, k0: f64, false_e: f64, figure: Ellipsoid) -> Crs {
    Crs::TransverseMercator {
        lat0: 0.0,
        lon0: lon0.to_radians(),
        k0,
        false_e,
        false_n: 0.0,
        figure,
    }
}

/// CF's `rotated_latitude_longitude` with its grid north pole at 40°N,
/// 170°W: the south pole at 40°S, 10°E.
fn rotated() -> Crs {
    Crs::RotatedLatLon {
        south_pole_lat: (-40f64).to_radians(),
        south_pole_lon: 10f64.to_radians(),
    }
}

/// The tangent cone's constant is sin φ₁, so γ = n (λ − λ₀) worked by hand:
/// at 25°E on MEPS's grid, 10° east of its central meridian,
/// γ = 10° · sin 63.3° ≈ 8.934°. A secant cone's n comes from both
/// parallels, on the ellipsoid it is drawn on, and a southern cone turns
/// the other way.
#[test]
fn lambert_conic_turns_by_the_cone_constant() {
    let hand = 10.0 * 63.3f64.to_radians().sin();
    assert!((hand - 8.933_713_88).abs() < 1e-8);
    let convergence = Convergence::of(&meps()).unwrap();
    // Every latitude alike: γ depends on longitude only.
    for lat in [55.0, 60.0, 70.0] {
        let got = convergence.degrees(25.0, lat).unwrap();
        assert!((got - hand).abs() < 1e-12, "{lat}: {got}");
    }
    // PROJ, both sides of the meridian.
    assert_pins(
        &meps(),
        &[(25.0, 60.0, 8.933_713_88), (0.0, 55.0, -13.400_570_82)],
    );
    // +proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +ellps=WGS84
    assert_pins(
        &lcc(33.0, 45.0, 39.0, -96.0, None),
        &[(-80.0, 40.0, 10.087_940_02), (-110.0, 30.0, -8.826_947_52)],
    );
    // +proj=lcc +lat_1=-18 +lat_2=-36 +lat_0=-27 +lon_0=133 +ellps=WGS84
    assert_pins(
        &lcc(-18.0, -36.0, -27.0, 133.0, None),
        &[(150.0, -30.0, -7.750_605_22)],
    );
    // A longitude written a turn away is the same meridian.
    let east = convergence.degrees(25.0, 60.0).unwrap();
    assert!((convergence.degrees(385.0, 60.0).unwrap() - east).abs() < 1e-9);
    assert!((convergence.degrees(-335.0, 60.0).unwrap() - east).abs() < 1e-9);
}

/// A polar stereographic plane is the cone of its pole, n = ±1: γ is the
/// longitude from its meridian, the other way round at the south pole,
/// whatever the scale or figure.
#[test]
fn polar_stereographic_turns_by_the_longitude() {
    // +proj=stere +lat_0=90 +lat_ts=60 +lon_0=-45 +ellps=WGS84
    let wgs84 = Ellipsoid::WGS84;
    let north = stere(
        90.0,
        -45.0,
        polar_stereographic_k0(60f64.to_radians(), &wgs84),
        wgs84,
    );
    assert_pins(&north, &[(10.0, 70.0, 55.0)]);
    // +proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 +ellps=WGS84
    let south = stere(
        -90.0,
        0.0,
        polar_stereographic_k0(71f64.to_radians(), &wgs84),
        wgs84,
    );
    assert_pins(&south, &[(30.0, -70.0, -30.0)]);
}

/// Oblique stereographic, PROJ's `stere` (not `sterea`): DMI's composite
/// grid on WGS 84, and an oblique grid on newbase's sphere.
#[test]
fn oblique_stereographic_pins_proj() {
    // +proj=stere +lat_0=56 +lon_0=10.5666 +k_0=1 +ellps=WGS84
    assert_pins(
        &stere(56.0, 10.5666, 1.0, Ellipsoid::WGS84),
        &[(14.0, 58.0, 2.874_438_47), (2.0, 52.0, -6.923_155_16)],
    );
    // +proj=stere +lat_0=60 +lon_0=20 +k_0=1 +R=6371220
    assert_pins(
        &stere(60.0, 20.0, 1.0, Ellipsoid::sphere(6_371_220.0)),
        &[(40.0, 50.0, 16.499_690_15)],
    );
}

/// Transverse Mercator: TM35FIN (EPSG:3067, UTM zone 35 on GRS 1980), near
/// and 33° from its meridian, where Krüger's series turns γ by 0.07° from
/// the conformal sphere's; on a sphere, the hand-worked `tan γ = sin φ tan
/// Δλ`.
#[test]
fn transverse_mercator_pins_proj() {
    // +proj=utm +zone=35 +ellps=GRS80, and +proj=tmerc +lon_0=27 +k=0.9996
    // +x_0=500000 +ellps=GRS80 for the far point.
    assert_pins(
        &tm(27.0, 0.9996, 500_000.0, Ellipsoid::GRS80),
        &[
            (30.0, 65.0, 2.719_368_63),
            (20.0, 61.0, -6.129_525_01),
            (60.0, 50.0, 26.458_094_33),
        ],
    );
    // +proj=tmerc +lat_0=0 +lon_0=27 +k=1 +R=6371000
    let sphere = tm(27.0, 1.0, 0.0, Ellipsoid::sphere(6_371_000.0));
    let hand = (8f64.to_radians().tan() * 50f64.to_radians().sin())
        .atan()
        .to_degrees();
    assert!((hand - 6.144_825_54).abs() < 1e-8);
    assert_pins(&sphere, &[(35.0, 50.0, hand)]);
}

/// Rotated latitude/longitude, against PROJ's `ob_tran`. PROJ measures its
/// convergence in the plane of rotated longitude and latitude, which keeps
/// angles only on the rotated equator, so the points are there: rotated
/// (0.2, 0), (−0.3, 0) and (0.25, 0) radians, found by `invproj`.
#[test]
fn rotated_lat_lon_pins_proj_on_the_rotated_equator() {
    let crs = rotated();
    // The premise: the same rotation as
    // +proj=ob_tran +o_proj=longlat +o_lon_p=0 +o_lat_p=40 +lon_0=10 +R=6371000
    // which takes (20°E, 60°N) to (0.088372013190, 0.180444776095) rad.
    let (x, y) = crs.forward(20.0, 60.0);
    assert!((x.to_radians() - 0.088_372_013_190).abs() < 1e-11, "{x}");
    assert!((y.to_radians() - 0.180_444_776_095).abs() < 1e-11, "{y}");
    assert_pins(
        &crs,
        &[
            (27.503_233_321_097, 48.657_516_919_568, 13.320_342_00),
            (-15.698_803_237_033, 47.040_047_146_886, -19.401_614_73),
            (31.665_030_203_136, 47.921_723_406_637, 16.427_865_68),
        ],
    );
}

/// The direction true north takes on the grid, `atan2(∂x/∂φ, ∂y/∂φ)` by a
/// central difference of `Crs::forward`, is −γ: each formula turns by what
/// the projection the grid is drawn in turns, across each grid. A rotated
/// grid's x is its longitude, in degrees along its parallel of radius
/// cos φ′.
#[test]
fn every_formula_is_the_turn_of_its_own_projection() {
    let h = 1e-5;
    let grids: [(Crs, [f64; 4]); 9] = [
        (meps(), [-10.0, 50.0, 40.0, 75.0]),
        (
            lcc(33.0, 45.0, 39.0, -96.0, None),
            [-130.0, 20.0, -60.0, 55.0],
        ),
        (
            lcc(-18.0, -36.0, -27.0, 133.0, None),
            [110.0, -45.0, 155.0, -10.0],
        ),
        (
            stere(90.0, -45.0, 0.97, Ellipsoid::WGS84),
            [-180.0, 40.0, 180.0, 88.0],
        ),
        (
            stere(-90.0, 0.0, 0.97, Ellipsoid::WGS84),
            [-180.0, -88.0, 180.0, -40.0],
        ),
        (
            stere(56.0, 10.5666, 1.0, Ellipsoid::WGS84),
            [-10.0, 45.0, 35.0, 70.0],
        ),
        (
            stere(60.0, 20.0, 1.0, Ellipsoid::sphere(6_371_220.0)),
            [-10.0, 40.0, 60.0, 80.0],
        ),
        (
            tm(27.0, 0.9996, 500_000.0, Ellipsoid::GRS80),
            [0.0, -60.0, 60.0, 80.0],
        ),
        (rotated(), [-40.0, 20.0, 60.0, 80.0]),
    ];
    for (crs, [west, south, east, north]) in grids {
        let convergence = Convergence::of(&crs).unwrap();
        for i in 0..=8 {
            for j in 0..=8 {
                let lon = west + (east - west) * f64::from(i) / 8.0;
                let lat = south + (north - south) * f64::from(j) / 8.0;
                let (x0, y0) = crs.forward(lon, lat - h);
                let (x1, y1) = crs.forward(lon, lat + h);
                let (mut dx, dy) = (x1 - x0, y1 - y0);
                if let Crs::RotatedLatLon { .. } = crs {
                    dx = ((dx + 180.0).rem_euclid(360.0) - 180.0)
                        * crs.forward(lon, lat).1.to_radians().cos();
                }
                let north_on_grid = dx.atan2(dy).to_degrees();
                let gamma = convergence.degrees(lon, lat).unwrap();
                // Modulo a turn: 180° east and west of the meridian is one.
                let off = (gamma + north_on_grid + 180.0).rem_euclid(360.0) - 180.0;
                assert!(
                    off.abs() < 1e-5,
                    "{crs:?} at ({lon}, {lat}): γ {gamma}, north on the grid {north_on_grid}"
                );
            }
        }
    }
}

/// No convergence where there is none to compute: a north-aligned grid, a
/// grid whose axes are not orthogonal on the ground, and parameters that
/// make no cone. At a pole there is no north, and none at a rotated grid's
/// own pole.
#[test]
fn no_convergence_without_a_conformal_turn() {
    assert_eq!(Convergence::of(&Crs::Wgs84), None);
    assert_eq!(Convergence::of(&Crs::WebMercator), None);
    let laea = Crs::LambertAzimuthalEqualArea {
        lat0: 0.9,
        lon0: 0.17,
        false_e: 0.0,
        false_n: 0.0,
        figure: Ellipsoid::WGS84,
    };
    assert_eq!(Convergence::of(&laea), None);
    // Parallels symmetric about the equator: n = 0, a cylinder, no cone.
    assert_eq!(Convergence::of(&lcc(30.0, -30.0, 0.0, 15.0, None)), None);
    assert_eq!(
        Convergence::of(&lcc(f64::NAN, 60.0, 60.0, 15.0, None)),
        None
    );

    let convergence = Convergence::of(&meps()).unwrap();
    for lat in [90.0, -90.0] {
        assert_eq!(convergence.degrees(25.0, lat), None);
    }
    assert_eq!(convergence.degrees(f64::NAN, 60.0), None);
    // The rotated grid's own north pole, 40°N 170°W.
    assert_eq!(
        Convergence::of(&rotated()).unwrap().degrees(-170.0, 40.0),
        None
    );
}
