//! `PolarVolumeEngine::catalog_bytes`, the PVOL catalog's estimated heap,
//! against the heap a catalog build leaves allocated, measured with a
//! counting allocator (#1307): within 25 %. One test: every thread's
//! allocations count, so nothing may run alongside.
//!
//! The committed fixture is the only PVOL in the tree, so the catalog is
//! 400 copies of it under different names: one site, 400 parsed volumes,
//! each with its two sweeps of two moments and their byte spans. Capped, it
//! is 40 of them, and what the scan remembers of the other 360.
//!
//! The site's map metadata snapshot (#1485) is under 1 KB of that, too
//! little for the 25 % band to tell whether it is counted at all. So it is
//! measured on its own: its views share it, reading it allocates nothing,
//! and the part of the estimate that counts it is exactly what freeing the
//! one snapshot gives back.

use std::path::PathBuf;
use std::sync::Arc;

use ds_core::config::OdimConfig;
use ds_core::heap::Counting;
use ds_core::map_engine::MapEngine;
use engine_odim::PolarVolumeEngine;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn config(max_files: Option<usize>) -> OdimConfig {
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
        max_files,
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
    drop(PolarVolumeEngine::new("pvol-heap", Some(&path), &config(None)).unwrap());

    // The whole catalog, then one that trims all but 40 volumes and holds
    // only what it remembers of the rest (#1398).
    for max_files in [None, Some(40)] {
        let config = config(max_files);
        let before = Counting::live();
        let engine = PolarVolumeEngine::new("pvol-heap", Some(&path), &config).unwrap();
        let held = (Counting::live() - before) as f64;
        let estimate = engine.catalog_bytes() as f64;
        let error = (estimate - held) / held;
        println!(
            "catalog, max_files {max_files:?}: held {held} bytes, estimated {estimate} ({:+.1} %)",
            100.0 * error
        );
        assert!(
            error.abs() <= 0.25,
            "max_files {max_files:?}: the estimate {estimate} is {:.1} % off the {held} bytes held",
            100.0 * error
        );

        // Two views of the site read the one snapshot the estimate counts:
        // a hundred reads hold no more heap than the `Vec` holding them.
        let (nod, _) = engine.sites().into_iter().next().expect("one site");
        let views = [
            engine.site_view(&nod, "pvol-heap-site"),
            engine.site_view(&nod, "pvol-heap-site-again"),
        ];
        let mut infos = Vec::with_capacity(100);
        let before_reads = Counting::live();
        for i in 0..100 {
            infos.push(views[i % 2].raster_info_shared());
        }
        assert_eq!(
            Counting::live() - before_reads,
            0,
            "max_files {max_files:?}: a read allocates"
        );
        assert!(infos.iter().all(|info| Arc::ptr_eq(info, &infos[0])));
        assert!(!infos[0].parameters.is_empty());
        assert_eq!(engine.catalog_bytes() as f64, estimate, "built once");

        // Keep one handle on the snapshot and drop everything else: freeing
        // it then gives back exactly the bytes the estimate counted for it,
        // once for the one site.
        let counted = engine.site_snapshot_bytes();
        let snapshot = infos[0].clone();
        drop(infos);
        drop(views);
        drop(engine);
        let before_free = Counting::live();
        drop(snapshot);
        let freed = before_free - Counting::live();
        println!("site snapshot, max_files {max_files:?}: freed {freed} bytes, counted {counted}");
        assert!(
            freed > 0,
            "max_files {max_files:?}: the snapshot outlived the catalog"
        );
        assert_eq!(
            freed as u64, counted,
            "max_files {max_files:?}: the estimate counts the site's snapshot once"
        );
    }
}
