//! Geopotential is served in m² s⁻² and its height derived where a view
//! lacks one (#948): the engine reports each message's triple, level and
//! unit, `ds_core::derive::Derived` does the rest.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::derive::{DerivationLog, Derivations, Derived, Outcome};
use ds_core::geopotential::STANDARD_GRAVITY;

fn reference() -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse().unwrap()
}

/// Geopotential (0/3/4) at the 2×2 nodes: `base` plus 0, 10, 20 and 30.
fn z_message(surface: u8, level: u32, base: f32) -> Vec<u8> {
    parameter_message(3, 4, 0x30, base, [0, 10, 20, 30], surface, level)
}

/// Geopotential height (0/3/5), in gpm.
fn gh_message(surface: u8, level: u32) -> Vec<u8> {
    parameter_message(3, 5, 0x30, 5_870.0, [0, 1, 2, 3], surface, level)
}

type Logged = Arc<Mutex<Vec<String>>>;

fn wrap(id: &str, engine: &Arc<GribEngine>, derive: Derivations) -> (Derived, Logged) {
    let logged: Logged = Arc::default();
    let sink = logged.clone();
    let log: DerivationLog = Arc::new(move |id: &str, outcome: &Outcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    (Derived::with(id, engine.clone(), derive, log), logged)
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

fn names(info: &RasterInfo) -> Vec<&str> {
    info.parameters.iter().map(|p| p.name.as_str()).collect()
}

/// A pressure view with `z` and no `gh` (AIFS-like `z` alone).
fn pressure_view(records: &[(&str, &str, Vec<u8>)]) -> (TestSource, Arc<GribEngine>) {
    let source = TestSource::new();
    source.write("f000", records, 0);
    let mut config = source.config();
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let owner = GribEngine::new("aifs", &config).unwrap();
    let pressure = Arc::new(owner.level_collections().remove(0));
    (source, pressure)
}

/// `z` is geopotential, in m² s⁻², on every API: the map pixel, the EDR
/// position value and the reported units agree. It used to be served
/// divided by g₀, as gpm, under the label "Geopotential".
#[test]
fn z_is_served_in_m2_s2() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[("z", "500 mb", z_message(100, 50000, 57_600.0))],
        0,
    );
    let engine = GribEngine::new("aifs", &source.config()).unwrap();
    let info = engine.raster_info_shared();
    assert_eq!(info.parameter_unit(Some("z")), Some("m2 s-2"));
    let description = &engine.get_parameter_descriptions()["z"];
    assert_eq!(description.unit, "m2 s-2");
    assert!(description.label.starts_with("Geopotential"));

    // Sampled between the four nodes as packed, 57 600–57 630: no
    // division anywhere.
    let pixels: Vec<f64> = tile(&engine, "z", None).into_iter().flatten().collect();
    assert_eq!(pixels.len(), 4);
    assert!(
        pixels.iter().all(|p| (57_600.0..=57_630.0).contains(p)),
        "{pixels:?}"
    );
    assert!((pixels.iter().sum::<f64>() / 4.0 - 57_615.0).abs() < 1e-3);

    let result = position(&engine, &["z"], None);
    assert_eq!(result.parameters["z"].unit, "m2 s-2");
    // The grid's centre: the mean of its four nodes.
    let value = result.ranges["z"].values[0].unwrap();
    assert!((value - 57_615.0).abs() < 1e-6, "{value}");
}

/// A pressure view with geopotential and no height gets `gh` = z / g₀,
/// sampled from `z` at each level on EDR and on the map, in gpm and
/// styled by the existing `gh` default.
#[test]
fn pressure_level_z_gives_gh_derived_after_sampling() {
    let (_source, pressure) = pressure_view(&[
        ("z", "850 mb", z_message(100, 85000, 14_000.0)),
        ("z", "500 mb", z_message(100, 50000, 57_600.0)),
    ]);
    let (derived, logged) = wrap("aifs-pressure", &pressure, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        ["aifs-pressure: geopotential height z: derived 'gh'"]
    );
    let info = derived.raster_info_shared();
    assert_eq!(names(&info), ["z", "gh"]);
    let gh = &info.parameters[1];
    assert_eq!(
        (gh.title.as_str(), gh.unit.as_str()),
        ("Geopotential height", "gpm"),
        "the level is the request's z"
    );
    let description = &derived.get_parameter_descriptions()["gh"];
    assert_eq!(description.unit, "gpm");
    assert_eq!(
        description.standard_name.as_deref(),
        Some("geopotential_height")
    );
    for level in [850.0, 500.0] {
        let at = [level];
        let z = position(pressure.as_ref(), &["z"], Some(&at)).ranges["z"].values[0].unwrap();
        let result = position(&derived, &["gh"], Some(&at));
        let gh = result.ranges["gh"].values[0].unwrap();
        assert!((gh - z / STANDARD_GRAVITY).abs() < 1e-9, "{level}: {gh}");
        assert_eq!(result.parameters["gh"].unit, "gpm");
        assert!(!result.ranges.contains_key("z"), "z only fed the height");
        let want: Vec<Option<f64>> = tile(pressure.as_ref(), "z", Some(level))
            .into_iter()
            .map(|z| Some(f64::from((z.unwrap() / STANDARD_GRAVITY) as f32)))
            .collect();
        assert_eq!(tile(&derived, "gh", Some(level)), want, "{level} hPa");
    }
    // 500 hPa geopotential of 57 615 m² s⁻² is about 5875 gpm.
    let gh = position(&derived, &["gh"], Some(&[500.0])).ranges["gh"].values[0].unwrap();
    assert!((gh - 5875.1).abs() < 0.1, "{gh}");
    // Time, run and level resolve as z's.
    for t in [None, Some(reference())] {
        assert_eq!(
            derived.resolve_parameter_time(Some("gh"), t, None),
            pressure.resolve_parameter_time(Some("z"), t, None)
        );
        assert_eq!(
            derived.resolve_parameter_reference_time(Some("gh"), t, None),
            pressure.resolve_parameter_reference_time(Some("z"), t, None)
        );
    }
    assert_eq!(
        derived.resolve_elevation(Some("gh"), None, None, None),
        pressure.resolve_elevation(Some("z"), None, None, None)
    );
}

/// A view that ships `gh` at the level (the ECMWF pressure-level
/// collections) gets no second one.
#[test]
fn an_existing_gh_is_not_derived_again() {
    let (_source, pressure) = pressure_view(&[
        ("z", "500 mb", z_message(100, 50000, 57_600.0)),
        ("gh", "500 mb", gh_message(100, 50000)),
    ]);
    let (derived, logged) = wrap("ifs-pressure", &pressure, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        ["ifs-pressure: geopotential height z: nothing derived: the collection already has 'gh'"]
    );
    assert_eq!(names(&derived.raster_info_shared()), ["z", "gh"]);
    assert_eq!(
        derived.get_parameter_descriptions()["gh"].unit,
        "gpm",
        "the native height"
    );
}

/// A single-level collection's `z` is the surface geopotential: the same
/// rule derives the surface's height, terrain in gpm. Wind is derived as
/// before next to it, and each derivation is its own opt-out.
#[test]
fn surface_z_gives_the_terrain_height_next_to_wind() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("z", "surface", z_message(1, 0, 1_000.0)),
            (
                "10u",
                "10 m above ground",
                parameter_message(2, 2, 0x30, -5.0, [0, 10, 5, 8], 103, 10),
            ),
            (
                "10v",
                "10 m above ground",
                parameter_message(2, 3, 0x30, -4.0, [0, 8, 4, 10], 103, 10),
            ),
            ("2t", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
        ],
        0,
    );
    let engine = Arc::new(GribEngine::new("ifs", &source.config()).unwrap());
    let (derived, logged) = wrap("ifs", &engine, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "ifs: wind 10u/10v: derived speed '10si' and direction '10wdir'",
            "ifs: geopotential height z: derived 'gh'",
        ]
    );
    let info = derived.raster_info_shared();
    let gh = info.parameters.iter().find(|p| p.name == "gh").unwrap();
    assert_eq!(gh.title, "Geopotential height (surface)");
    let z = position(engine.as_ref(), &["z"], None).ranges["z"].values[0].unwrap();
    let terrain = position(&derived, &["gh"], None).ranges["gh"].values[0].unwrap();
    assert!((terrain - z / STANDARD_GRAVITY).abs() < 1e-9);
    assert!((terrain - 103.5).abs() < 0.1, "{terrain} gpm");
    let speed = position(&derived, &["10si"], None).ranges["10si"].values[0];
    assert!(speed.is_some());

    // Wind only: no height, and the wind as before.
    let (wind, logged) = wrap("ifs", &engine, Derivations::WIND);
    assert_eq!(
        *logged.lock().unwrap(),
        ["ifs: wind 10u/10v: derived speed '10si' and direction '10wdir'"]
    );
    assert!(!wind.get_parameters().contains(&"gh".to_string()));
    let height_only = Derivations {
        wind: false,
        geopotential_height: true,
    };
    let (height, _) = wrap("ifs", &engine, height_only);
    let names = height.get_parameters();
    assert!(names.contains(&"gh".to_string()));
    assert!(!names.contains(&"10si".to_string()));
}
