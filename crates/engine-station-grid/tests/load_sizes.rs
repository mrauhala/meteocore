//! #1313: a station grid checks its grid against the sizes the load being
//! built gives the grid cache and the computation budget (`[memory.caches]
//! station_grid` and `station_grid_compute`), which the server stages
//! before it builds the load. Its own test binary: what is staged is
//! process-wide.

use std::sync::Arc;

use ds_cache::plan::keys::{self, Size};
use ds_cache::MIB;
use ds_core::config::{StationGridConfig, StationGridGeometry};
use ds_core::edr_engine::EdrEngine;
use engine_station_grid::StationGridEngine;

fn fixture() -> Arc<dyn EdrEngine> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/weather.csv");
    let store = engine_csv::CsvDataStore::load(path).expect("weather.csv loads");
    Arc::new(engine_csv::CsvEngine::new(store))
}

/// A 1200 × 800 grid: 3.7 MiB.
fn config() -> StationGridConfig {
    StationGridConfig {
        source: "weather".into(),
        parameters: vec!["temperature".into()],
        elevation_source: None,
        elevation_parameter: None,
        land_mask_source: None,
        land_mask_parameter: None,
        exclude_where: Default::default(),
        grid: StationGridGeometry {
            crs: "EPSG:4326".into(),
            bbox: [19.0, 59.0, 25.0, 63.0],
            resolution: 0.005,
        },
        method: "idw".into(),
        power: None,
        correlation_length_km: None,
        max_neighbours: 8,
        search_radius_km: 60.0,
        max_age: "PT1H".into(),
        cadence: Some("PT1H".into()),
        poll_interval_secs: 60,
        precompute: 1,
    }
}

/// A live grid fits the load until its staged sizes no longer hold it,
/// and a grid built in that load fails with the constructor's error,
/// naming the staged size; staging resizes nothing.
#[test]
fn a_grid_checks_the_sizes_the_load_gives() {
    let engine = StationGridEngine::new("fits", "weather", fixture(), &config()).unwrap();
    assert!(engine.fits_the_load(), "nothing staged");
    let built = |id: &str| StationGridEngine::new(id, "weather", fixture(), &config());

    let cache = keys::by_key("station_grid").unwrap();
    keys::stage(&[(cache, Size::Pinned(2))], false);
    assert!(!engine.fits_the_load(), "a cache lowered below one grid");
    let error = built("lowered").err().unwrap().to_string();
    assert!(error.contains("more than the 2 MiB grid cache"), "{error}");
    keys::stage(&[(cache, Size::Pinned(64))], false);
    assert!(
        engine.fits_the_load(),
        "a cache lowered, still above one grid"
    );

    let budget = keys::by_key("station_grid_compute").unwrap();
    keys::stage(&[(budget, Size::Pinned(4))], false);
    assert!(
        !engine.fits_the_load(),
        "half the budget below one computation"
    );
    let error = built("starved").err().unwrap().to_string();
    assert!(error.contains("more than half the 4 MiB"), "{error}");

    keys::unstage();
    assert!(engine.fits_the_load());
    assert_eq!(
        engine_station_grid::cache_metrics().capacity_bytes,
        256 * MIB,
        "staging resized nothing"
    );
}
