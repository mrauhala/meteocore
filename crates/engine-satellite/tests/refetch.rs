//! With zero-size caches every scan is fetched again on each use, as after
//! an eviction. The fetch must work from both call sites the server has: an
//! async request worker (EDR queries run there) and a blocking render worker
//! (WMS/Maps/Tiles render jobs) — `DataStore::get`'s bridge serves both,
//! where an explicit `get_on` handle would panic on the async worker.
//!
//! A separate test binary: the cache sizes are read once per process.

use std::path::Path;
use std::sync::Arc;

use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_satellite::SatelliteEngine;

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evicted_scans_are_fetched_from_async_and_blocking_workers() {
    // Only this test runs in this binary, before either cache is first used.
    std::env::set_var("MC_SATELLITE_FRAME_CACHE_MB", "0");
    std::env::set_var("MC_SATELLITE_STRIP_CACHE_MB", "0");

    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/goes19-abi")
            .join(C13),
        dir.path().join(C13),
    )
    .unwrap();
    let config = SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        products: vec![SatelliteProductConfig {
            parameter: "ir_10_3".into(),
            title: "IR 10.3 µm brightness temperature".into(),
            unit: "K".into(),
            product: "ABI-L2-CMIPF".into(),
            band: Some(13),
            variable: "CMI".into(),
        }],
    };
    let engine = Arc::new(SatelliteEngine::new("goes19-refetch", &config).unwrap());
    // The poll loop's call site: a task on a multi-thread runtime.
    engine.poll_once();
    let extent = engine.raster_info().spatial_extent.unwrap();
    let [w, s, e, n] = extent;
    let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);

    // EDR: called directly on this async worker, as the EDR handlers do.
    let series = engine
        .query_position(&format!("POINT({lon} {lat})"), None, None, None, None)
        .expect("EDR query fetches the evicted scan from an async worker");
    let ds_core::model::CoverageResponse::Single(series) = series else {
        panic!("one coverage");
    };
    assert_eq!(series.ranges["ir_10_3"].values.len(), 1);

    // Rendering: on a blocking worker, as render jobs do.
    let render = engine.clone();
    let tile = tokio::task::spawn_blocking(move || {
        render.get_raster_tile(extent, 16, 16, None, &OutputCrs::Wgs84, None, None, None)
    })
    .await
    .unwrap()
    .expect("a render fetches the evicted scan from a blocking worker");
    assert!((0..tile.values.len()).any(|i| tile.values.value_at(i).is_some()));
}
