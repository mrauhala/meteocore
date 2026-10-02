//! Wind derived from GRIB u/v components (#897): the engine reports each
//! message's GRIB2 component flag, `ds_core::wind::DerivedWind` does the rest.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::wind::{DerivedWind, OutcomeLog, PairOutcome, VectorFrame, WindSource};

/// Flag table 3.3 as ECMWF and NCEP write it: increments given, earth-relative.
const EARTH: u8 = 0x30;
/// The same with bit 5: components relative to the grid's x and y.
const GRID: u8 = 0x38;

fn reference() -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse().unwrap()
}

/// u and v at the 2×2 nodes (NW, NE, SW, SE): opposite winds, calm, and a
/// south-westerly. `shift` moves u at every node, for a second level.
fn u_message(flags: u8, surface: u8, level: u32, shift: u8) -> Vec<u8> {
    parameter_message(2, 2, flags, -5.0, [shift, 10, 5, 8], surface, level)
}

fn v_message(flags: u8, surface: u8, level: u32) -> Vec<u8> {
    parameter_message(2, 3, flags, -4.0, [0, 8, 4, 10], surface, level)
}

type Logged = Arc<Mutex<Vec<String>>>;

fn wrap(id: &str, engine: &Arc<GribEngine>) -> (DerivedWind, Logged) {
    let logged: Logged = Arc::default();
    let sink = logged.clone();
    let log: OutcomeLog = Arc::new(move |id: &str, outcome: &PairOutcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    (DerivedWind::new(id, engine.clone(), log), logged)
}

fn position(engine: &dyn EdrEngine, names: &[&str], z: Option<&[f64]>) -> QueryResult {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    match engine
        .query_position("POINT(0.5 0.5)", None, Some(&names), z, None)
        .unwrap()
    {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one series"),
    }
}

fn tile(engine: &dyn MapEngine, parameter: &str, z: Option<f64>) -> Vec<Option<f64>> {
    engine
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            2,
            2,
            Some(reference()),
            &OutputCrs::Wgs84,
            Some(parameter),
            z,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

/// Independent of `ds_core::wind`: speed and the direction the wind blows
/// from, degrees clockwise from north.
fn expected(u: f64, v: f64) -> (f64, f64) {
    (
        (u * u + v * v).sqrt(),
        (-u).atan2(-v).to_degrees().rem_euclid(360.0),
    )
}

/// Bit 5 of the resolution-and-component flags, read by the header probe
/// at load (no values decoded) and by a full decode alike.
#[test]
fn the_component_flag_is_read_from_the_header_and_the_decode() {
    for (flags, frame) in [(EARTH, VectorFrame::Earth), (GRID, VectorFrame::Grid)] {
        let bytes = u_message(flags, 103, 10, 0);
        assert_eq!(
            reader::decode_message(&bytes, "UGRD").unwrap().uv_frame,
            frame
        );
        let source = TestSource::new();
        source.write("f000", &[("UGRD", "10 m above ground", bytes)], 0);
        let engine = GribEngine::new("flag", &source.config()).unwrap();
        assert_eq!(engine.source.grid_cache.as_ref().unwrap().len(), 0);
        let facts = engine.wind_facts();
        assert_eq!(facts.grid, ds_core::wind::GridAxes::NorthAligned);
        let u = &facts.parameters[0];
        assert_eq!(u.name, "UGRD");
        assert_eq!(u.frame, frame);
        assert_eq!(u.grib, Some((0, 2, 2)));
        assert_eq!(u.unit, "m s-1");
        assert_eq!(u.level.as_deref(), Some("hag:10"));
        assert_eq!(u.level_label.as_deref(), Some("10 m above ground"));
        // O(1): one snapshot until the next refresh.
        assert!(Arc::ptr_eq(&facts, &engine.wind_facts()));
    }
}

/// wgrib2 `UGRD`/`VGRD` → `WIND`/`WDIR`, sampled from the components with
/// the engine's own interpolation and only then combined.
#[test]
fn ugrd_vgrd_give_wind_and_wdir_derived_after_sampling() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("UGRD", "10 m above ground", u_message(EARTH, 103, 10, 0)),
            ("VGRD", "10 m above ground", v_message(EARTH, 103, 10)),
            ("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
        ],
        0,
    );
    let engine = Arc::new(GribEngine::new("gfs", &source.config()).unwrap());
    let (wind, logged) = wrap("gfs", &engine);
    assert_eq!(
        *logged.lock().unwrap(),
        ["gfs: UGRD/VGRD: derived speed 'WIND' and direction 'WDIR'"]
    );

    // Map APIs: the speed only, styled as wind speed by its name and title.
    let info = wind.raster_info_shared();
    let speed = info.parameters.iter().find(|p| p.name == "WIND").unwrap();
    assert_eq!(speed.title, "Wind speed (10 m above ground)");
    assert_eq!(speed.unit, "m s-1");
    assert!(!info.parameters.iter().any(|p| p.name == "WDIR"));

    // EDR: speed and direction, equal to the formula on the component query.
    assert!(wind
        .get_parameters()
        .ends_with(&["WIND".into(), "WDIR".into()]));
    let components = position(engine.as_ref(), &["UGRD", "VGRD"], None);
    let (u, v) = (
        components.ranges["UGRD"].values[0].unwrap(),
        components.ranges["VGRD"].values[0].unwrap(),
    );
    assert!((u - 0.75).abs() < 1e-9);
    assert!((v - 1.5).abs() < 1e-9);
    let derived = position(&wind, &["WIND", "WDIR"], None);
    let (speed, direction) = expected(u, v);
    assert!((derived.ranges["WIND"].values[0].unwrap() - speed).abs() < 1e-9);
    assert!((derived.ranges["WDIR"].values[0].unwrap() - direction).abs() < 1e-9);
    assert_eq!(derived.ranges.len(), 2, "the components are not returned");
    assert_eq!(derived.parameters["WDIR"].unit, "°");
    // Derived after sampling: the mean of the four node speeds is not it.
    let node_mean = [(-5.0, -4.0), (5.0, 4.0), (0.0, 0.0), (3.0, 6.0)]
        .iter()
        .map(|&(u, v): &(f64, f64)| u.hypot(v))
        .sum::<f64>()
        / 4.0;
    assert!((node_mean - speed).abs() > 1.0);

    // A speed tile is the hypotenuse of the two component tiles.
    let (u, v) = (
        tile(engine.as_ref(), "UGRD", None),
        tile(engine.as_ref(), "VGRD", None),
    );
    let want: Vec<Option<f64>> = u
        .iter()
        .zip(&v)
        .map(|(u, v)| Some(f64::from(expected(u.unwrap(), v.unwrap()).0 as f32)))
        .collect();
    assert_eq!(tile(&wind, "WIND", None), want);
    assert!(wind
        .get_raster_tile(
            [0.0, 0.0, 1.0, 1.0],
            2,
            2,
            None,
            &OutputCrs::Wgs84,
            Some("WDIR"),
            None,
            None
        )
        .is_err());
    // The cache-key time and run are the components'.
    assert_eq!(
        wind.resolve_parameter_time(Some("WIND"), Some(reference()), None),
        engine.resolve_time(Some(reference()), None)
    );
    assert_eq!(wind.content_version(), engine.content_version());
}

/// ECMWF `10u`/`10v` → `10si`/`10wdir`; a collection that has `10si` gets
/// the direction only, never a second `10si`.
#[test]
fn ecmwf_10u_10v_give_10si_and_10wdir_without_duplicating_10si() {
    for with_speed in [false, true] {
        let source = TestSource::new();
        let mut records = vec![
            ("10u", "10 m above ground", u_message(EARTH, 103, 10, 0)),
            ("10v", "10 m above ground", v_message(EARTH, 103, 10)),
        ];
        if with_speed {
            let speed = parameter_message(2, 1, EARTH, 0.0, [1, 2, 3, 4], 103, 10);
            records.push(("10si", "10 m above ground", speed));
        }
        source.write("f000", &records, 0);
        let engine = Arc::new(GribEngine::new("ifs", &source.config()).unwrap());
        let (wind, logged) = wrap("ifs", &engine);
        let names = wind.get_parameters();
        let count = |name: &str| names.iter().filter(|n| *n == name).count();
        assert_eq!((count("10si"), count("10wdir")), (1, 1), "{names:?}");
        let map: Vec<String> = wind
            .raster_info_shared()
            .parameters
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(map.iter().filter(|n| *n == "10si").count(), 1);
        let log = logged.lock().unwrap().clone();
        if with_speed {
            assert_eq!(
                log,
                ["ifs: 10u/10v: derived direction '10wdir' only; no speed: \
                  the collection already has '10si'"]
            );
            // The native speed is served as it is.
            let native = position(&wind, &["10si"], None);
            assert!((native.ranges["10si"].values[0].unwrap() - 2.5).abs() < 1e-9);
            assert_eq!(native.parameters["10si"].unit, "m s-1");
        } else {
            assert_eq!(
                log,
                ["ifs: 10u/10v: derived speed '10si' and direction '10wdir'"]
            );
            let components = position(engine.as_ref(), &["10u", "10v"], None);
            let derived = position(&wind, &["10si"], None);
            let (speed, _) = expected(
                components.ranges["10u"].values[0].unwrap(),
                components.ranges["10v"].values[0].unwrap(),
            );
            assert!((derived.ranges["10si"].values[0].unwrap() - speed).abs() < 1e-9);
        }
    }
}

/// A pressure-level collection: `ws`/`wdir` at a level are computed from
/// `u`/`v` at that level, in EDR and on the map.
#[test]
fn pressure_level_ws_and_wdir_match_the_components_at_each_level() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("u", "850 mb", u_message(EARTH, 100, 85000, 0)),
            ("v", "850 mb", v_message(EARTH, 100, 85000)),
            ("u", "500 mb", u_message(EARTH, 100, 50000, 20)),
            ("v", "500 mb", v_message(EARTH, 100, 50000)),
        ],
        0,
    );
    let mut config = source.config();
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let owner = GribEngine::new("ifs", &config).unwrap();
    let pressure = Arc::new(owner.level_collections().remove(0));
    let (wind, logged) = wrap("ifs-pressure", &pressure);
    assert_eq!(
        *logged.lock().unwrap(),
        ["ifs-pressure: u/v: derived speed 'ws' and direction 'wdir'"]
    );
    let speed = wind
        .raster_info_shared()
        .parameters
        .iter()
        .find(|p| p.name == "ws")
        .cloned()
        .unwrap();
    assert_eq!(speed.title, "Wind speed", "the level is the request's z");
    for level in [850.0, 500.0] {
        let z = [level];
        let components = position(pressure.as_ref(), &["u", "v"], Some(&z));
        let derived = position(&wind, &["ws", "wdir"], Some(&z));
        let (speed, direction) = expected(
            components.ranges["u"].values[0].unwrap(),
            components.ranges["v"].values[0].unwrap(),
        );
        assert!((derived.ranges["ws"].values[0].unwrap() - speed).abs() < 1e-9);
        assert!((derived.ranges["wdir"].values[0].unwrap() - direction).abs() < 1e-9);
        let (u, v) = (
            tile(pressure.as_ref(), "u", Some(level)),
            tile(pressure.as_ref(), "v", Some(level)),
        );
        let want: Vec<Option<f64>> = u
            .iter()
            .zip(&v)
            .map(|(u, v)| Some(f64::from(expected(u.unwrap(), v.unwrap()).0 as f32)))
            .collect();
        assert_eq!(tile(&wind, "ws", Some(level)), want, "{level} hPa");
    }
    // The two levels differ, so each was read at its own level.
    let at = |z: f64| position(&wind, &["ws"], Some(&[z])).ranges["ws"].values[0];
    assert_ne!(at(850.0), at(500.0));
    // A whole profile: one value per level, combined level by level.
    let CoverageResponse::Collection(profiles) = wind
        .query_position("POINT(0.5 0.5)", None, Some(&["ws".into()]), None, None)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(profiles[0].ranges["ws"].values, [at(850.0), at(500.0)]);
}

/// On a regular lat/lon grid (the only grid this engine reads) grid-relative
/// components are earth-relative; components in different frames give
/// nothing at all.
#[test]
fn grid_relative_flags_on_lat_lon_give_direction_and_mixed_flags_nothing() {
    for (u_flags, v_flags, want) in [
        (
            GRID,
            GRID,
            "ifs: 10u/10v: derived speed '10si' and direction '10wdir'",
        ),
        (
            EARTH,
            GRID,
            "ifs: 10u/10v: nothing derived: u and v are relative to different frames",
        ),
    ] {
        let source = TestSource::new();
        source.write(
            "f000",
            &[
                ("10u", "10 m above ground", u_message(u_flags, 103, 10, 0)),
                ("10v", "10 m above ground", v_message(v_flags, 103, 10)),
            ],
            0,
        );
        let engine = Arc::new(GribEngine::new("ifs", &source.config()).unwrap());
        let (wind, logged) = wrap("ifs", &engine);
        assert_eq!(*logged.lock().unwrap(), [want]);
        let derived = wind.get_parameters().len() - engine.get_parameters().len();
        assert_eq!(derived, if u_flags == v_flags { 2 } else { 0 });
    }
}

/// The ARPEGE fixture (`testdata/grib-arpege-wind`): 10 m u/v and
/// Météo-France's own 10 m speed and direction from one analysis, every
/// 20th node of the 0.1° Europe grid. `parameters` serves the components
/// alone, so the wrapper derives `10si`/`10wdir`; without it the native ones
/// stop the derivation.
fn arpege(parameters: Option<&[&str]>) -> Arc<GribEngine> {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/grib-arpege-wind"
    );
    let config: GribConfig = serde_json::from_value(serde_json::json!({
        "data_path": dir, "grid_cache_mb": 16, "parameters": parameters,
    }))
    .unwrap();
    Arc::new(GribEngine::new("arpege", &config).unwrap())
}

fn area(engine: &dyn EdrEngine, names: &[&str]) -> QueryResult {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    match engine
        .query_area("-32,20,42,72", None, Some(&names), None, None)
        .unwrap()
    {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one grid"),
    }
}

/// Derived against native over the whole domain (#897): speed within the
/// source's packing precision, direction within half a degree wherever the
/// wind is not calm, across the 0/360 wrap. A frame mistake (a rotation, a
/// swapped or negated component) would show as a direction error that
/// varies across the domain.
#[test]
fn derived_matches_native_arpege_speed_and_direction_over_the_domain() {
    let native = arpege(None);
    let (_, logged) = wrap("arpege", &native);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "arpege: 10u/10v: nothing derived; no speed: the collection already has '10si'; \
          no direction: the collection already has '10wdir'"
        ]
    );
    let components = arpege(Some(&["10u", "10v"]));
    let facts = components.wind_facts();
    assert!(facts
        .parameters
        .iter()
        .all(|p| p.frame == VectorFrame::Earth));
    let (wind, logged) = wrap("arpege-uv", &components);
    assert_eq!(
        *logged.lock().unwrap(),
        ["arpege-uv: 10u/10v: derived speed '10si' and direction '10wdir'"]
    );

    let native = area(native.as_ref(), &["10si", "10wdir"]);
    let derived = area(&wind, &["10si", "10wdir"]);
    let (DomainDescription::Grid { x, y, .. }, DomainDescription::Grid { x: dx, y: dy, .. }) =
        (&native.domain, &derived.domain)
    else {
        panic!("expected grids")
    };
    assert_eq!((x, y), (dx, dy), "one domain");
    assert_eq!((x.len(), y.len()), (38, 27), "every node of the fixture");
    assert_eq!(native.parameters["10wdir"].unit, "°");
    assert_eq!(derived.parameters["10wdir"].unit, "°");
    assert_eq!(
        derived.parameters["10si"].unit,
        native.parameters["10si"].unit
    );

    let values = |r: &QueryResult, name: &str| r.ranges[name].values.clone();
    let (speed, native_speed) = (values(&derived, "10si"), values(&native, "10si"));
    let (from, native_from) = (values(&derived, "10wdir"), values(&native, "10wdir"));
    let (mut compared, mut near_north, mut worst) = (0, 0, 0.0f64);
    for i in 0..speed.len() {
        let (s, n) = (speed[i].unwrap(), native_speed[i].unwrap());
        assert!((s - n).abs() < 0.02, "speed at node {i}: {s} vs {n}");
        if n <= 1.0 {
            continue; // near calm the direction is ill-conditioned
        }
        let (d, nd) = (from[i].unwrap(), native_from[i].unwrap());
        let error = (d - nd + 180.0).rem_euclid(360.0) - 180.0;
        worst = worst.max(error.abs());
        compared += 1;
        if !(5.0..=355.0).contains(&nd) {
            near_north += 1;
        }
    }
    assert!(worst < 0.5, "worst direction error {worst}°");
    assert!(compared > 900, "{compared} nodes compared");
    assert!(near_north > 20, "{near_north} nodes across the 0/360 wrap");
}
