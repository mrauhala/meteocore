use super::*;
use ds_core::map_engine::{MapEngine, OutputCrs};
use zarrs::array::{codec::GzipCodec, data_type, ArrayBuilder};
use zarrs::filesystem::FilesystemStore;
use zarrs::group::GroupBuilder;

fn write_store(path: &std::path::Path, times: &[f64], longitude: f64, value: f32) {
    let store = Arc::new(FilesystemStore::new(path).unwrap());
    GroupBuilder::new()
        .build(store.clone(), "/")
        .unwrap()
        .store_metadata()
        .unwrap();
    for (name, values, units) in [
        ("time", times.to_vec(), "hours since 2026-01-01"),
        ("lat", vec![59., 60.], "degrees_north"),
        ("lon", vec![longitude, longitude + 1.], "degrees_east"),
    ] {
        let array = ArrayBuilder::new(
            vec![values.len() as u64],
            vec![values.len() as u64],
            data_type::float64(),
            f64::NAN,
        )
        .dimension_names(Some([name]))
        .attributes(
            serde_json::json!({"units": units})
                .as_object()
                .unwrap()
                .clone(),
        )
        .build(store.clone(), &format!("/{name}"))
        .unwrap();
        array.store_metadata().unwrap();
        array.store_chunk(&[0], values).unwrap();
    }
    let array = ArrayBuilder::new(
        vec![times.len() as u64, 2, 2],
        vec![1, 2, 2],
        data_type::float32(),
        f32::NAN,
    )
    .dimension_names(Some(["time", "lat", "lon"]))
    .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(1).unwrap())])
    .build(store, "/temp")
    .unwrap();
    array.store_metadata().unwrap();
    for t in 0..times.len() {
        array
            .store_chunk(&[t as u64, 0, 0], vec![value; 4])
            .unwrap();
    }
}

fn engine(path: &std::path::Path, cache_mb: u64) -> ZarrEngine {
    let mut config = ZarrConfig::auto_local(path.to_string_lossy().into());
    config.cache_mb = cache_mb;
    ZarrEngine::new("refresh", &config).unwrap()
}

// Independent V2 fixture: exercise .zarray/.zattrs and period-separated keys,
// including the absent zarr.json probes cached by the initial catalog build.
fn write_v2_store(path: &std::path::Path, times: &[f64], longitude: f64, value: f32) {
    std::fs::write(path.join(".zgroup"), br#"{"zarr_format":2}"#).unwrap();
    for (name, values, units) in [
        ("time", times.to_vec(), "hours since 2026-01-01"),
        ("lat", vec![59., 60.], "degrees_north"),
        ("lon", vec![longitude, longitude + 1.], "degrees_east"),
    ] {
        let dir = path.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".zarray"),
            serde_json::to_vec(&serde_json::json!({
                "zarr_format":2, "shape":[values.len()], "chunks":[values.len()],
                "dtype":"<f8", "compressor":null, "fill_value":"NaN", "order":"C",
                "filters":null, "dimension_separator":"."
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join(".zattrs"),
            serde_json::to_vec(&serde_json::json!({
                "_ARRAY_DIMENSIONS":[name], "units":units
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("0"),
            values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
    }
    let dir = path.join("temp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".zarray"),
        serde_json::to_vec(&serde_json::json!({
            "zarr_format":2, "shape":[times.len(),2,2], "chunks":[1,2,2],
            "dtype":"<f4", "compressor":null, "fill_value":"NaN", "order":"C",
            "filters":null, "dimension_separator":"."
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".zattrs"),
        br#"{"_ARRAY_DIMENSIONS":["time","lat","lon"]}"#,
    )
    .unwrap();
    for t in 0..times.len() {
        std::fs::write(dir.join(format!("{t}.0.0")), value.to_le_bytes().repeat(4)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_metadata_and_coordinate_updates_are_visible() {
    let dir = tempfile::tempdir().unwrap();
    write_v2_store(dir.path(), &[0.], 10., 1.);
    let engine = engine(dir.path(), 1);
    assert_eq!(render(&engine, 10.), vec![Some(1.); 4]);
    let previous = engine.content_version();
    write_v2_store(dir.path(), &[0., 6.], 20., 2.);
    std::fs::write(
        dir.path().join("time/.zattrs"),
        br#"{"_ARRAY_DIMENSIONS":["time"],"units":"days since 2026-01-01"}"#,
    )
    .unwrap();
    engine.poll_once();
    assert_ne!(engine.content_version(), previous);
    let times = engine.get_available_times().unwrap();
    assert_eq!(times.len(), 2);
    assert_eq!(times[1] - times[0], chrono::Duration::days(6));
    assert_eq!(engine.catalog.load().extent[0], 19.5);
    assert_eq!(render(&engine, 20.), vec![Some(2.); 4]);
}

fn render(engine: &ZarrEngine, longitude: f64) -> Vec<Option<f64>> {
    engine
        .get_raster_tile(
            [longitude, 59., longitude + 1., 60.],
            2,
            2,
            None,
            &OutputCrs::Wgs84,
            Some("temp"),
            None,
            None,
        )
        .unwrap()
        .values
        .iter_values()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polling_refreshes_metadata_coordinates_and_payloads() {
    for cache_mb in [0, 1] {
        let dir = tempfile::tempdir().unwrap();
        write_store(dir.path(), &[0.], 10., 1.);
        let engine = engine(dir.path(), cache_mb);
        let old = engine.catalog.load_full();
        assert!(
            old.revision.is_none(),
            "plain generations are not Icechunk snapshots"
        );
        assert_ne!(engine.content_version(), 0);
        assert_eq!(render(&engine, 10.), vec![Some(1.); 4]);

        write_store(dir.path(), &[0., 6.], 20., 2.);
        engine.poll_once();
        let current = engine.catalog.load_full();
        assert_ne!(current.content_version, old.content_version);
        assert_eq!(current.times.len(), 2);
        assert_eq!(current.extent[0], 19.5);
        assert_eq!(render(&engine, 20.), vec![Some(2.); 4]);
        assert_eq!(old.times.len(), 1);
        assert_eq!(old.extent[0], 9.5);
        let old_read = old.read_window(&old.vars[0], None, 0, old.extent);
        if cache_mb == 0 {
            assert!(
                old_read.is_err(),
                "retired readers cannot refill from new data"
            );
        } else {
            let window = old_read.unwrap().unwrap();
            assert_eq!(window.sample(10.5, 59.5), Some(1.));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_only_corrections_and_noop_polls_advance_render_version() {
    let dir = tempfile::tempdir().unwrap();
    write_store(dir.path(), &[0.], 10., 1.);
    let engine = engine(dir.path(), 1);
    assert_eq!(render(&engine, 10.), vec![Some(1.); 4]);
    let old = engine.catalog.load_full();
    // Leave every metadata and coordinate object untouched.
    let store = Arc::new(FilesystemStore::new(dir.path()).unwrap());
    let array = zarrs::array::Array::open(store, "/temp").unwrap();
    array.store_chunk(&[0, 0, 0], vec![9.0f32; 4]).unwrap();
    engine.poll_once();
    assert_eq!(engine.get_available_times().unwrap(), old.times);
    assert_ne!(engine.content_version(), old.content_version);
    assert_eq!(render(&engine, 10.), vec![Some(9.); 4]);
    let version = engine.content_version();
    engine.poll_once();
    assert_ne!(
        engine.content_version(),
        version,
        "without a manifest, a no-op metadata scan cannot prove payloads unchanged"
    );
    assert_eq!(render(&engine, 10.), vec![Some(9.); 4]);
    assert_eq!(engine.decoded_cache_metrics().bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_keeps_the_previous_generation_readable_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    write_store(dir.path(), &[0.], 10., 1.);
    // With caching disabled, continuing to read proves a failed candidate did
    // not retire the old generation, rather than merely hitting its cache.
    let engine = engine(dir.path(), 0);
    let old = engine.catalog.load_full();
    let path = dir.path().join("temp/zarr.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"invalid metadata").unwrap();
    engine.poll_once();
    assert!(Arc::ptr_eq(&old, &engine.catalog.load_full()));
    assert_eq!(engine.content_version(), old.content_version);
    assert_eq!(render(&engine, 10.), vec![Some(1.); 4]);
    std::fs::write(path, original).unwrap();
    engine.poll_once();
    assert!(!Arc::ptr_eq(&old, &engine.catalog.load_full()));
    assert_ne!(engine.content_version(), old.content_version);
    assert_eq!(render(&engine, 10.), vec![Some(1.); 4]);
}
