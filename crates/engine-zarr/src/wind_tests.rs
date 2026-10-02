//! Wind derived from CF-identified Zarr components (#897): the frame comes
//! from each variable's `standard_name`, the derived names from the store's
//! own vocabulary.

use super::*;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::wind::{DerivedWind, OutcomeLog, PairOutcome, VectorFrame, WindSource};
use zarrs::array::{data_type, ArrayBuilder};
use zarrs::filesystem::FilesystemStore;
use zarrs::group::GroupBuilder;

/// Two timesteps on a 2×2 lat/lon grid. Each variable's four nodes (row
/// 59°N first, west first) and its CF attributes.
fn write_store(path: &std::path::Path, variables: &[(&str, Option<&str>, [f32; 4])]) {
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
    for (name, standard_name, values) in variables {
        let mut attributes = serde_json::json!({ "units": "m s-1" });
        if let Some(standard_name) = standard_name {
            attributes["standard_name"] = (*standard_name).into();
        }
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

fn wrap(path: &std::path::Path) -> (Arc<ZarrEngine>, DerivedWind, Vec<String>) {
    let config = ZarrConfig::auto_local(path.to_string_lossy().into());
    let engine = Arc::new(ZarrEngine::new("cf", &config).unwrap());
    let logged = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = logged.clone();
    let log: OutcomeLog = Arc::new(move |id: &str, outcome: &PairOutcome| {
        sink.lock().unwrap().push(format!("{id}: {outcome}"));
    });
    let wind = DerivedWind::new("cf", engine.clone(), log);
    let mut logged = logged.lock().unwrap().clone();
    logged.sort();
    (engine, wind, logged)
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

const U: [f32; 4] = [-3.0, 0.0, 4.0, 1.0];
const V: [f32; 4] = [-4.0, 0.0, 3.0, 1.0];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cf_components_give_speed_and_direction_in_the_store_vocabulary() {
    let dir = tempfile::tempdir().unwrap();
    write_store(
        dir.path(),
        &[
            ("wind_u_10m", Some("eastward_wind"), U),
            ("wind_v_10m", Some("northward_wind"), V),
            // Grid-relative, on a lat/lon grid: the same thing.
            ("x_wind_100m", Some("x_wind"), V),
            ("y_wind_100m", Some("y_wind"), U),
            // No standard name: no frame, and no pairing in this vocabulary.
            ("u_gust", None, U),
            ("v_gust", None, V),
        ],
    );
    let (engine, wind, logged) = wrap(dir.path());
    let facts = engine.wind_facts();
    let frame = |name: &str| {
        facts
            .parameters
            .iter()
            .find(|p| p.name == name)
            .unwrap()
            .frame
    };
    assert_eq!(frame("wind_u_10m"), VectorFrame::Earth);
    assert_eq!(frame("x_wind_100m"), VectorFrame::Grid);
    assert_eq!(frame("u_gust"), VectorFrame::Unknown);
    assert_eq!(
        logged,
        [
            "cf: wind_u_10m/wind_v_10m: derived speed 'wind_speed_10m' and direction \
             'wind_direction_10m'",
            "cf: x_wind_100m/y_wind_100m: derived speed 'wind_speed_100m' and direction \
             'wind_direction_100m'",
        ]
    );
    let names: Vec<String> = wind
        .raster_info_shared()
        .parameters
        .iter()
        .map(|p| p.name.clone())
        .filter(|n| n.starts_with("wind_"))
        .collect();
    assert_eq!(
        names,
        [
            "wind_u_10m",
            "wind_v_10m",
            "wind_speed_10m",
            "wind_speed_100m"
        ]
    );
    assert!(Arc::ptr_eq(
        &wind.raster_info_shared(),
        &wind.raster_info_shared()
    ));

    let components = series(engine.as_ref(), &["wind_u_10m", "wind_v_10m"]);
    let derived = series(&wind, &["wind_speed_10m", "wind_direction_10m"]);
    for t in 0..2 {
        let u = components.ranges["wind_u_10m"].values[t].unwrap();
        let v = components.ranges["wind_v_10m"].values[t].unwrap();
        let speed = derived.ranges["wind_speed_10m"].values[t].unwrap();
        let from = derived.ranges["wind_direction_10m"].values[t].unwrap();
        assert!((speed - u.hypot(v)).abs() < 1e-9);
        assert!((from - (-u).atan2(-v).to_degrees().rem_euclid(360.0)).abs() < 1e-9);
    }
    assert_eq!(
        derived.parameters["wind_direction_10m"]
            .standard_name
            .as_deref(),
        Some("wind_from_direction")
    );

    let (u, v) = (
        tile(engine.as_ref(), "wind_u_10m"),
        tile(engine.as_ref(), "wind_v_10m"),
    );
    let want: Vec<Option<f64>> = u
        .iter()
        .zip(&v)
        .map(|(u, v)| Some(f64::from(u.unwrap().hypot(v.unwrap()) as f32)))
        .collect();
    assert_eq!(tile(&wind, "wind_speed_10m"), want);
}
