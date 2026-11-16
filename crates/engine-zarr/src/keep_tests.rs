//! The keep floor a plain store publishes on its object cache (#1314),
//! over the committed ERA5 fixture.

use super::*;
use ds_core::map_engine::{MapEngine, OutputCrs};

fn engine() -> ZarrEngine {
    let config = ZarrConfig {
        data_path: Some(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/zarr-era5-t2m").to_string(),
        ),
        endpoint: None,
        bucket: None,
        path: None,
        zarr_version: Some(3),
        parameters: None,
        poll_interval_secs: 300,
        cache_mb: Some(ds_core::config::CacheSize::Mib(256)),
        icechunk: None,
    };
    ZarrEngine::new("zarr-keep", &config).expect("open fixture")
}

/// A plain store keeps its first variable's chunks at the default time:
/// once a map without a parameter or a time has read them, the object
/// cache holds them within the floor, which weighs them at their decoded
/// size as the cache weighs an object; a quiet day drops it.
#[test]
fn a_plain_store_keeps_its_default_chunks() {
    let engine = engine();
    let keep = engine.chunk_cache_keep().expect("published at load");
    assert_eq!((keep.keep, keep.quiet_bytes), (true, 0));
    let chunks = keep.inputs[0];
    assert_eq!(chunks.0, "chunks");
    assert!(chunks.1 > 0);
    let catalog = engine.catalog.load();
    let name = catalog.vars[0].name.clone();
    let extent = catalog.extent;
    drop(catalog);
    engine
        .get_raster_tile(extent, 16, 16, None, &OutputCrs::Wgs84, None, None, None)
        .unwrap();
    let prefix = format!("{name}/");
    let held = engine
        .source
        .cached_weight(|path| path.starts_with(&prefix) && !path.ends_with("zarr.json"));
    assert!(held > 0, "the default chunks read");
    assert!(held <= keep.bytes, "{held} > {keep:?}");
}
