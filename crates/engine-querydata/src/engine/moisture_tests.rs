//! QueryData humidity ingredients (#898): FMI's numbers identify
//! temperature (4), dew point (10) and relative humidity (13) and fix their
//! units (°C, °C, %), `ds_core::derive::Derived` does the rest. No committed
//! fixture carries a dew point or a humidity: these tests rename the Kenya
//! fixture's precipitation descriptor to one. The values are precipitation,
//! but the derivation only needs a field in the stated unit.

use std::sync::Mutex;

use super::wind_tests::{Fixture, KENYA};
use super::*;
use ds_core::derive::{DerivationLog, Derivations, Derived, Outcome};
use ds_core::map_engine::OutputCrs;
use ds_core::moisture::{dew_point_from_relative_humidity, relative_humidity_from_dew_point};

const K: f64 = 273.15;
const RR1H: &[u8] = b"\n353\n4 rr1h\n";
const DEW_POINT: &[u8] = b"\n10\n8 DewPoint\n";
const HUMIDITY: &[u8] = b"\n13\n8 Humidity\n";
/// The temperature's EDR name: its descriptor (#1215).
const T2M: &str = "2 Metre Temperature (2t)";

fn wrap(fixture: &Fixture) -> (Arc<QueryDataEngine>, Derived, Vec<String>) {
    let engine = Arc::new(QueryDataEngine::new(&fixture.0, "qd", None, 30, 4).unwrap());
    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = logged.clone();
    let log: DerivationLog = Arc::new(move |id: &str, outcome: &Outcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    let derived = Derived::with("qd", engine.clone(), Derivations::ALL, log);
    let logged = logged.lock().unwrap().clone();
    (engine, derived, logged)
}

fn series(engine: &dyn EdrEngine, names: &[&str]) -> QueryResult {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    match engine
        .query_position("POINT(36.8 -1.3)", None, Some(&names), None, None)
        .unwrap()
    {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one series"),
    }
}

fn tile(engine: &dyn MapEngine, parameter: &str) -> Vec<Option<f64>> {
    engine
        .get_raster_tile(
            [35.0, -4.0, 40.0, 3.0],
            8,
            8,
            None,
            &OutputCrs::WebMercator,
            Some(parameter),
            None,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

/// `2t` (FMI 4) with a dew point (FMI 10) gives a relative humidity named
/// in the temperature's vocabulary, `2r`, its dew point found by number.
#[test]
fn temperature_and_dew_point_give_relative_humidity() {
    let fixture = Fixture::new(KENYA, &[(RR1H, DEW_POINT)]);
    let (engine, derived, logged) = wrap(&fixture);
    assert_eq!(
        logged,
        [
            "qd: humidity 2t: derived relative humidity '2r' from '2t' and 'DewPoint'; no dew \
          point: the collection already has 'DewPoint'"
        ]
    );
    let t = engine
        .wind_facts()
        .parameters
        .iter()
        .find(|p| p.name == "2t")
        .map(|p| (p.fmi_param, p.unit.clone()));
    assert_eq!(t, Some((Some(4), "°C".to_string())));
    assert!(derived.get_parameters().contains(&"2r".to_string()));
    let description = &derived.get_parameter_descriptions()["2r"];
    assert_eq!(description.unit, "%");
    assert_eq!(
        description.standard_name.as_deref(),
        Some("relative_humidity")
    );

    let inputs = series(engine.as_ref(), &[T2M, "DewPoint"]);
    let result = series(&derived, &["2r"]);
    let (t, td) = (
        &inputs.ranges[T2M].values,
        &inputs.ranges["DewPoint"].values,
    );
    // The first step has no precipitation, here no dew point: no value.
    assert!(td.iter().any(Option::is_some) && td.iter().any(Option::is_none));
    let want: Vec<Option<f64>> = t
        .iter()
        .zip(td)
        .map(|(t, td)| relative_humidity_from_dew_point((*t)? + K, (*td)? + K))
        .collect();
    assert_eq!(result.ranges["2r"].values, want);

    let (t, td) = (
        tile(engine.as_ref(), "2t"),
        tile(engine.as_ref(), "DewPoint"),
    );
    let want: Vec<Option<f64>> = t
        .iter()
        .zip(&td)
        .map(|(t, td)| {
            relative_humidity_from_dew_point((*t)? + K, (*td)? + K).map(|v| f64::from(v as f32))
        })
        .collect();
    assert!(
        want.iter().all(Option::is_some),
        "the view is inside the grid"
    );
    assert_eq!(tile(&derived, "2r"), want);
}

/// A relative humidity (FMI 13, %) gives the dew point, `2d`, in °C; a
/// relative humidity of 0 gives none.
#[test]
fn temperature_and_relative_humidity_give_dew_point() {
    let fixture = Fixture::new(KENYA, &[(RR1H, HUMIDITY)]);
    let (engine, derived, logged) = wrap(&fixture);
    assert_eq!(
        logged,
        [
            "qd: humidity 2t: no relative humidity: the collection already has 'Humidity'; \
          derived dew point '2d' from '2t' and 'Humidity'"
        ]
    );
    let inputs = series(engine.as_ref(), &[T2M, "Humidity"]);
    let result = series(&derived, &["2d"]);
    assert_eq!(result.parameters["2d"].unit, "°C");
    let (t, rh) = (
        &inputs.ranges[T2M].values,
        &inputs.ranges["Humidity"].values,
    );
    let want: Vec<Option<f64>> = t
        .iter()
        .zip(rh)
        .map(|(t, rh)| dew_point_from_relative_humidity((*t)? + K, (*rh)?).map(|td| td - K))
        .collect();
    assert!(want.iter().any(Option::is_some));
    assert_eq!(result.ranges["2d"].values, want);
}
