//! Relative humidity and dew point derived from CF-identified Zarr
//! variables (#898): the `standard_name` identifies each ingredient, the
//! served unit (#1213: K as °C, a `relative_humidity` in `1` as %) is what
//! the formulas convert from, and the derived names follow the store's own
//! vocabulary.

use std::sync::Mutex;

use super::*;
use ds_core::derive::{DerivationLog, Derivations, Derived, Outcome};
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::moisture::{dew_point_from_relative_humidity, relative_humidity_from_dew_point};
use zarrs::array::{data_type, ArrayBuilder};
use zarrs::filesystem::FilesystemStore;
use zarrs::group::GroupBuilder;

const K: f64 = 273.15;

/// Two timesteps on a 2×2 lat/lon grid; each variable's four nodes, its
/// `standard_name` and its `units`.
fn write_store(path: &std::path::Path, variables: &[(&str, &str, &str, [f32; 4])]) {
    let store = Arc::new(FilesystemStore::new(path).unwrap());
    GroupBuilder::new()
        .build(store.clone(), "/")
        .unwrap()
        .store_metadata()
        .unwrap();
    for (name, values, units) in [
        ("time", vec![0., 1.], "hours since 2026-01-01"),
        ("lat", vec![59., 60.], "degrees_north"),
        ("lon", vec![24., 25.], "degrees_east"),
    ] {
        let array = ArrayBuilder::new(
            vec![values.len() as u64],
            vec![values.len() as u64],
            data_type::float64(),
            f64::NAN,
        )
        .dimension_names(Some([name]))
        .attributes(
            serde_json::json!({ "units": units })
                .as_object()
                .unwrap()
                .clone(),
        )
        .build(store.clone(), &format!("/{name}"))
        .unwrap();
        array.store_metadata().unwrap();
        array.store_chunk(&[0], values).unwrap();
    }
    for (name, standard_name, units, values) in variables {
        let attributes = serde_json::json!({ "units": units, "standard_name": standard_name });
        let array = ArrayBuilder::new(vec![2, 2, 2], vec![1, 2, 2], data_type::float32(), f32::NAN)
            .dimension_names(Some(["time", "lat", "lon"]))
            .attributes(attributes.as_object().unwrap().clone())
            .build(store.clone(), &format!("/{name}"))
            .unwrap();
        array.store_metadata().unwrap();
        for t in 0..2 {
            array.store_chunk(&[t, 0, 0], values.to_vec()).unwrap();
        }
    }
}

fn wrap(path: &std::path::Path) -> (Arc<ZarrEngine>, Derived, Vec<String>) {
    let config = ZarrConfig::auto_local(path.to_string_lossy().into());
    let engine = Arc::new(ZarrEngine::new("cf", &config).unwrap());
    let logged = Arc::new(Mutex::new(Vec::new()));
    let sink = logged.clone();
    let log: DerivationLog = Arc::new(move |id: &str, outcome: &Outcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    let derived = Derived::with("cf", engine.clone(), Derivations::ALL, log);
    let mut logged = logged.lock().unwrap().clone();
    logged.sort();
    (engine, derived, logged)
}

fn series(engine: &dyn EdrEngine, names: &[&str]) -> QueryResult {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    match engine
        .query_position("POINT(24.5 59.5)", None, Some(&names), None, None)
        .unwrap()
    {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected one series"),
    }
}

fn tile(engine: &dyn MapEngine, parameter: &str) -> Vec<Option<f64>> {
    engine
        .get_raster_tile(
            [24.0, 59.0, 25.0, 60.0],
            4,
            4,
            None,
            &OutputCrs::Wgs84,
            Some(parameter),
            None,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

/// ERA5's `t2m` and `d2m` (K) give `r2`, each value the formula over the
/// samples of the same request in their served °C.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn era5_t2m_d2m_give_r2() {
    let dir = tempfile::tempdir().unwrap();
    write_store(
        dir.path(),
        &[
            ("t2m", "air_temperature", "K", [283.0, 290.0, 270.0, 300.0]),
            (
                "d2m",
                "dew_point_temperature",
                "K",
                [280.0, 281.0, 265.0, 299.0],
            ),
        ],
    );
    let (engine, derived, logged) = wrap(dir.path());
    assert_eq!(
        logged,
        [
            "cf: humidity t2m: derived relative humidity 'r2' from 't2m' and 'd2m'; no dew \
          point: the collection already has 'd2m'"
        ]
    );
    let info = derived.raster_info_shared();
    let unit = |name: &str| {
        info.parameters
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.unit.clone())
    };
    assert_eq!(unit("t2m").as_deref(), Some("°C"));
    assert_eq!(unit("r2").as_deref(), Some("%"));

    let inputs = series(engine.as_ref(), &["t2m", "d2m"]);
    let result = series(&derived, &["r2"]);
    let values = |name: &str| inputs.ranges[name].values.clone();
    let want: Vec<Option<f64>> = values("t2m")
        .iter()
        .zip(values("d2m"))
        .map(|(t, td)| relative_humidity_from_dew_point((*t)? + K, td? + K))
        .collect();
    assert!(want.iter().all(Option::is_some));
    assert_eq!(result.ranges["r2"].values, want);

    let (t, td) = (tile(engine.as_ref(), "t2m"), tile(engine.as_ref(), "d2m"));
    let want: Vec<Option<f64>> = t
        .iter()
        .zip(&td)
        .map(|(t, td)| {
            relative_humidity_from_dew_point((*t)? + K, (*td)? + K).map(|v| f64::from(v as f32))
        })
        .collect();
    assert!(want.iter().any(Option::is_some));
    assert_eq!(tile(&derived, "r2"), want);

    // The opt-out.
    let off = Derivations {
        humidity: false,
        ..Derivations::ALL
    };
    let plain = Derived::with(
        "cf",
        engine.clone(),
        off,
        Arc::new(|_: &str, _: &Outcome| {}),
    );
    assert!(!plain.get_parameters().contains(&"r2".to_string()));
}

/// MEPS-style `air_temperature_2m` (K) and `relative_humidity_2m` (CF `1`,
/// served in %) give `dew_point_temperature_2m` in the °C the temperature
/// is served in; a relative humidity of 0 gives none, and one over 100 %
/// the temperature.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cf_temperature_and_relative_humidity_give_dew_point() {
    let dir = tempfile::tempdir().unwrap();
    write_store(
        dir.path(),
        &[
            (
                "air_temperature_2m",
                "air_temperature",
                "K",
                [283.0, 290.0, 270.0, 300.0],
            ),
            (
                "relative_humidity_2m",
                "relative_humidity",
                "1",
                [0.9, 0.5, 0.0, 1.03],
            ),
        ],
    );
    let (engine, derived, logged) = wrap(dir.path());
    assert_eq!(
        logged,
        [
            "cf: humidity air_temperature_2m: no relative humidity: the collection already has \
          'relative_humidity_2m'; derived dew point 'dew_point_temperature_2m' from \
          'air_temperature_2m' and 'relative_humidity_2m'"
        ]
    );
    let description = &derived.get_parameter_descriptions()["dew_point_temperature_2m"];
    assert_eq!(description.unit, "°C");
    assert_eq!(
        description.standard_name.as_deref(),
        Some("dew_point_temperature")
    );
    // Every node: the corners of the grid, each step.
    for point in [
        "POINT(24 59)",
        "POINT(25 59)",
        "POINT(24 60)",
        "POINT(25 60)",
    ] {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let query = |engine: &dyn EdrEngine, n: &[&str]| match engine
            .query_position(point, None, Some(&names(n)), None, None)
            .unwrap()
        {
            CoverageResponse::Single(result) => result,
            CoverageResponse::Collection(_) => panic!("expected one series"),
        };
        let inputs = query(
            engine.as_ref(),
            &["air_temperature_2m", "relative_humidity_2m"],
        );
        let result = query(&derived, &["dew_point_temperature_2m"]);
        let (t, rh) = (
            &inputs.ranges["air_temperature_2m"].values,
            &inputs.ranges["relative_humidity_2m"].values,
        );
        let want: Vec<Option<f64>> = t
            .iter()
            .zip(rh)
            .map(|(t, rh)| dew_point_from_relative_humidity((*t)? + K, (*rh)?).map(|td| td - K))
            .collect();
        assert_eq!(
            result.ranges["dew_point_temperature_2m"].values, want,
            "{point}"
        );
        match rh[0] {
            Some(rh) if rh <= 0.0 => assert_eq!(want[0], None, "{point}"),
            Some(rh) if rh > 100.0 => assert_eq!(want[0], t[0], "{point}"),
            _ => assert!(want[0].is_some_and(|td| td < t[0].unwrap()), "{point}"),
        }
    }
}
