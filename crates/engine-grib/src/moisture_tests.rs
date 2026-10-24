//! Relative humidity and dew point derived where a view lacks them (#898):
//! the engine reports each message's triple, served unit and level (or the
//! pressure axis it spans), `ds_core::derive::Derived` does the rest.
//!
//! No committed GRIB fixture carries temperature, specific humidity and
//! native relative humidity on pressure levels, so the issue's cross-check
//! of a derived `r` against ECMWF's native mixed-phase one is not run here;
//! these messages are built by `test_support`.

use super::*;
use crate::test_support::{message, parameter_message, TestSource};
use ds_core::derive::{DerivationLog, Derivations, Derived, Outcome};
use ds_core::moisture::{
    dew_point_from_specific_humidity, relative_humidity_from_dew_point,
    relative_humidity_from_specific_humidity,
};
use ds_core::vertical::VerticalKind;
use ds_core::wind::WindSource;

const K: f64 = 273.15;

fn reference() -> DateTime<Utc> {
    "2026-04-05T00:00:00Z".parse().unwrap()
}

/// Temperature (0/0/0) in K at the 2×2 nodes: `base` plus 0, 2, 4, 6.
fn t_message(surface: u8, level: u32, base: f32) -> Vec<u8> {
    message(0, base, [0, 2, 4, 6], surface, level)
}

/// Dew point (0/0/6) in K: `base` plus 0, 1, 2, 3.
fn td_message(surface: u8, level: u32, base: f32) -> Vec<u8> {
    parameter_message(0, 6, 0x30, base, [0, 1, 2, 3], surface, level)
}

/// Specific humidity (0/1/0) in kg/kg, the same at every node.
fn q_message(surface: u8, level: u32, q: f32) -> Vec<u8> {
    parameter_message(1, 0, 0x30, q, [0; 4], surface, level)
}

/// Relative humidity (0/1/1) in %.
fn rh_message(surface: u8, level: u32) -> Vec<u8> {
    parameter_message(1, 1, 0x30, 60.0, [0, 5, 10, 15], surface, level)
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

/// ECMWF's 2 m `2t` and `2d` (served in °C from their K) give `2r` on
/// every API: a map pixel is the formula over the two tiles of the same
/// request, an EDR value the formula over the two sampled values.
#[test]
fn single_level_2t_2d_give_2r() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("2t", "2 m above ground", t_message(103, 2, 280.0)),
            ("2d", "2 m above ground", td_message(103, 2, 272.0)),
        ],
        0,
    );
    let engine = Arc::new(GribEngine::new("ifs", &source.config()).unwrap());
    let facts = engine.wind_facts();
    let t = facts.parameters.iter().find(|p| p.name == "2t").unwrap();
    assert_eq!(
        (t.grib, t.unit.as_str(), t.level.as_deref(), t.vertical),
        (Some((0, 0, 0)), "°C", Some("hag:2"), None)
    );
    let (derived, logged) = wrap("ifs", &engine, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "ifs: humidity 2t: derived relative humidity '2r' from '2t' and '2d'; no dew \
          point: the collection already has '2d'"
        ]
    );
    let info = derived.raster_info_shared();
    assert_eq!(names(&info), ["2t", "2d", "2r"]);
    let rh = &info.parameters[2];
    assert_eq!(
        (rh.title.as_str(), rh.unit.as_str()),
        ("Relative humidity over water (2 m above ground)", "%")
    );

    let (t, td) = (
        tile(engine.as_ref(), "2t", None),
        tile(engine.as_ref(), "2d", None),
    );
    let want: Vec<Option<f64>> = t
        .iter()
        .zip(&td)
        .map(|(t, td)| {
            relative_humidity_from_dew_point(t.unwrap() + K, td.unwrap() + K)
                .map(|rh| f64::from(rh as f32))
        })
        .collect();
    assert!(want
        .iter()
        .all(|rh| rh.is_some_and(|rh| rh > 30.0 && rh < 100.0)));
    assert_eq!(tile(&derived, "2r", None), want);

    let inputs = position(engine.as_ref(), &["2t", "2d"], None);
    let (t, td) = (
        inputs.ranges["2t"].values[0].unwrap(),
        inputs.ranges["2d"].values[0].unwrap(),
    );
    let result = position(&derived, &["2r"], None);
    assert_eq!(
        result.ranges["2r"].values[0],
        relative_humidity_from_dew_point(t + K, td + K)
    );
    let description = &result.parameters["2r"];
    assert_eq!(description.unit, "%");
    assert_eq!(
        description.standard_name.as_deref(),
        Some("relative_humidity")
    );
    assert!(description.label.contains("over water"));
    // The opt-out: no humidity, the rest as before.
    let off = Derivations {
        humidity: false,
        ..Derivations::ALL
    };
    let (plain, logged) = wrap("ifs", &engine, off);
    assert!(logged.lock().unwrap().is_empty());
    assert_eq!(plain.get_parameters(), ["2t", "2d"]);
}

/// A pressure view with `t` and `q` and no `r` (AIFS-like): `r` and `dpt`
/// from each level's own pressure, on the map at the drawn level and in
/// EDR at the queried one.
#[test]
fn pressure_level_t_q_give_r_and_dpt() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("t", "850 mb", t_message(100, 85000, 275.0)),
            ("t", "500 mb", t_message(100, 50000, 250.0)),
            ("q", "850 mb", q_message(100, 85000, 0.004)),
            ("q", "500 mb", q_message(100, 50000, 0.0005)),
        ],
        0,
    );
    let mut config = source.config();
    config.level_types = Some(vec![GribLevelType::Pressure]);
    let owner = GribEngine::new("aifs", &config).unwrap();
    let pressure = Arc::new(owner.level_collections().remove(0));
    let facts = pressure.wind_facts();
    assert!(facts
        .parameters
        .iter()
        .all(|p| p.level.is_none() && p.vertical == Some(VerticalKind::Pressure)));
    let (derived, logged) = wrap("aifs-pressure", &pressure, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "aifs-pressure: humidity t: derived relative humidity 'r' from 't', 'q' and the \
          level's pressure; derived dew point 'dpt' from 't', 'q' and the level's pressure"
        ]
    );
    assert_eq!(names(&derived.raster_info_shared()), ["t", "q", "r", "dpt"]);

    for level in [850.0, 500.0] {
        let p = level * 100.0;
        let (t, q) = (
            tile(pressure.as_ref(), "t", Some(level)),
            tile(pressure.as_ref(), "q", Some(level)),
        );
        let want = |f: fn(f64, f64, f64) -> Option<f64>, offset: f64| -> Vec<Option<f64>> {
            t.iter()
                .zip(&q)
                .map(|(t, q)| {
                    f(t.unwrap() + K, q.unwrap(), p).map(|v| f64::from((v - offset) as f32))
                })
                .collect()
        };
        assert_eq!(
            tile(&derived, "r", Some(level)),
            want(relative_humidity_from_specific_humidity, 0.0),
            "{level} hPa"
        );
        assert_eq!(
            tile(&derived, "dpt", Some(level)),
            want(dew_point_from_specific_humidity, K),
            "{level} hPa"
        );

        let at = [level];
        let inputs = position(pressure.as_ref(), &["t", "q"], Some(&at));
        let (t, q) = (
            inputs.ranges["t"].values[0].unwrap(),
            inputs.ranges["q"].values[0].unwrap(),
        );
        let result = position(&derived, &["r", "dpt"], Some(&at));
        assert_eq!(
            result.ranges["r"].values[0],
            relative_humidity_from_specific_humidity(t + K, q, p),
            "{level} hPa"
        );
        assert_eq!(
            result.ranges["dpt"].values[0],
            dew_point_from_specific_humidity(t + K, q, p).map(|td| td - K),
            "{level} hPa"
        );
        assert_eq!(result.parameters["dpt"].unit, "°C");
    }
    // Without ELEVATION the map draws the view's first level, 850 hPa, and
    // the humidity takes that level's pressure.
    assert_eq!(tile(&derived, "r", None), tile(&derived, "r", Some(850.0)));
}

/// GFS's 2 m `TMP`, `DPT` and `RH` are all native: nothing is derived, and
/// the reason is logged; a q at a single level never pairs with MSLP.
#[test]
fn a_view_with_native_humidity_gets_no_second_one() {
    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("TMP", "2 m above ground", t_message(103, 2, 280.0)),
            ("DPT", "2 m above ground", td_message(103, 2, 272.0)),
            ("RH", "2 m above ground", rh_message(103, 2)),
        ],
        0,
    );
    let engine = Arc::new(GribEngine::new("gfs", &source.config()).unwrap());
    let (derived, logged) = wrap("gfs", &engine, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "gfs: humidity TMP: no relative humidity: the collection already has 'RH'; no dew \
          point: the collection already has 'DPT'"
        ]
    );
    assert_eq!(derived.get_parameters(), ["TMP", "DPT", "RH"]);

    let source = TestSource::new();
    source.write(
        "f000",
        &[
            ("TMP", "2 m above ground", t_message(103, 2, 280.0)),
            ("SPFH", "2 m above ground", q_message(103, 2, 0.004)),
            (
                "PRMSL",
                "mean sea level",
                parameter_message(3, 1, 0x30, 101_000.0, [0; 4], 101, 0),
            ),
        ],
        0,
    );
    let engine = Arc::new(GribEngine::new("gfs", &source.config()).unwrap());
    let (derived, logged) = wrap("gfs", &engine, Derivations::ALL);
    assert_eq!(
        *logged.lock().unwrap(),
        [
            "gfs: humidity TMP: nothing derived: 'SPFH' is not on pressure levels, and a single \
          level's pressure would be the surface pressure, which is not supported"
        ]
    );
    assert_eq!(derived.get_parameters(), ["TMP", "SPFH", "PRMSL"]);
}
