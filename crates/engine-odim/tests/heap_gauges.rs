//! `PolarVolumeEngine::catalog_bytes`, the PVOL catalog's estimated heap,
//! against the heap a catalog build leaves allocated, measured with a
//! counting allocator (#1307): within 25 %. One test: every thread's
//! allocations count, so nothing may run alongside.
//!
//! The committed fixture is the only PVOL in the tree, so the catalog is
//! 400 copies of it under different names: one site, 400 parsed volumes,
//! each with its two sweeps of two moments and their byte spans.

use std::path::PathBuf;

use ds_core::config::OdimConfig;
use ds_core::heap::Counting;
use engine_odim::PolarVolumeEngine;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn config() -> OdimConfig {
    OdimConfig {
        filename_template: None,
        filename_pattern: None,
        timestamp_format: None,
        parameter: None,
        unit: None,
        nodata: None,
        gain: None,
        offset: None,
        poll_interval_secs: 30,
        max_files: None,
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        discovery: None,
        cadence_secs: None,
        resampling: Default::default(),
        // Pre-warming fills the process-wide pixel cache, not the catalog.
        prewarm_sweeps: 0,
        prewarm_quantities: None,
    }
}

#[test]
fn catalog_estimate_matches_the_heap_held() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/pvol-cold-batch.h5");
    let dir = tempfile::tempdir().unwrap();
    for i in 0..400 {
        std::fs::copy(&fixture, dir.path().join(format!("pvol_{i:04}.h5"))).unwrap();
    }
    let path = dir.path().to_str().unwrap().to_string();

    // A first build initialises the process-wide statics a scan touches.
    drop(PolarVolumeEngine::new("pvol-heap", Some(&path), &config()).unwrap());

    let before = Counting::live();
    let engine = PolarVolumeEngine::new("pvol-heap", Some(&path), &config()).unwrap();
    let held = (Counting::live() - before) as f64;
    let estimate = engine.catalog_bytes() as f64;
    let error = (estimate - held) / held;
    println!(
        "catalog: held {held} bytes, estimated {estimate} ({:+.1} %)",
        100.0 * error
    );
    assert!(
        error.abs() <= 0.25,
        "the estimate {estimate} is {:.1} % off the {held} bytes held",
        100.0 * error
    );
}
