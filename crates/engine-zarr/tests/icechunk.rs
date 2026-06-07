//! End-to-end Icechunk test (feature `icechunk`, issue #335): generate a local
//! Icechunk repository with a tiny CF Zarr dataset, then read it back through
//! the engine. Network-free.
//!
//! Run with: `cargo test -p engine-zarr --features icechunk --test icechunk`

#![cfg(feature = "icechunk")]

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use ds_core::config::{IcechunkConfig, ZarrConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::model::CoverageResponse;
use engine_zarr::ZarrEngine;

use icechunk::Repository;
use zarrs::array::{data_type, ArrayBuilder, ArraySubset};
use zarrs::group::GroupBuilder;
use zarrs_icechunk::AsyncIcechunkStore;

const NT: usize = 3;
const NY: usize = 4;
const NX: usize = 5;

fn attrs(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

fn field(lat: f64, lon: f64, t: usize) -> f64 {
    273.15 + 0.1 * lat + 0.01 * lon + 0.5 * t as f64
}

/// Write a tiny CF Zarr V3 dataset into a fresh local Icechunk repo at `dir`
/// and commit it on branch `main`.
async fn write_repo(dir: &std::path::Path) {
    let storage = icechunk::new_local_filesystem_storage(dir)
        .await
        .expect("local icechunk storage");
    let repo = Repository::create(None, storage, HashMap::new(), Default::default(), true)
        .await
        .expect("create repo");
    let store = Arc::new(AsyncIcechunkStore::new(
        repo.writable_session("main")
            .await
            .expect("writable session"),
    ));

    // Root group.
    let group = GroupBuilder::new()
        .attributes(attrs(serde_json::json!({"Conventions": "CF-1.8"})))
        .build(store.clone(), "/")
        .expect("group builder");
    group.async_store_metadata().await.expect("group metadata");

    // Coordinates.
    let time_vals: Vec<i64> = (0..NT as i64).map(|i| i * 6).collect();
    write_coord_i64(
        &store,
        "/time",
        &time_vals,
        "time",
        serde_json::json!({
            "units": "hours since 2026-01-01 00:00:00",
            "calendar": "proleptic_gregorian",
            "standard_name": "time",
        }),
    )
    .await;
    let lat_vals: Vec<f64> = (0..NY).map(|j| 60.0 - j as f64).collect(); // descending
    write_coord_f64(
        &store,
        "/lat",
        &lat_vals,
        "lat",
        serde_json::json!({
            "units": "degrees_north", "standard_name": "latitude",
        }),
    )
    .await;
    let lon_vals: Vec<f64> = (0..NX).map(|i| i as f64).collect();
    write_coord_f64(
        &store,
        "/lon",
        &lon_vals,
        "lon",
        serde_json::json!({
            "units": "degrees_east", "standard_name": "longitude",
        }),
    )
    .await;

    // Data variable t2m (float32), single chunk.
    let mut t2m = Vec::with_capacity(NT * NY * NX);
    for t in 0..NT {
        for &lat in &lat_vals {
            for &lon in &lon_vals {
                t2m.push(field(lat, lon, t) as f32);
            }
        }
    }
    let array = ArrayBuilder::new(
        vec![NT as u64, NY as u64, NX as u64],
        vec![NT as u64, NY as u64, NX as u64],
        data_type::float32(),
        f32::NAN,
    )
    .dimension_names(Some(["time", "lat", "lon"]))
    .attributes(attrs(serde_json::json!({
        "units": "K", "long_name": "2 metre temperature",
    })))
    .build(store.clone(), "/t2m")
    .expect("t2m builder");
    array.async_store_metadata().await.expect("t2m metadata");
    let all = ArraySubset::new_with_shape(array.chunk_grid_shape().to_vec());
    array
        .async_store_chunks(&all, t2m)
        .await
        .expect("t2m chunks");

    // Commit the snapshot to branch main.
    store
        .session()
        .write()
        .await
        .commit("fixture")
        .execute()
        .await
        .expect("commit");
}

async fn write_coord_f64(
    store: &Arc<AsyncIcechunkStore>,
    path: &str,
    values: &[f64],
    dim: &str,
    attributes: serde_json::Value,
) {
    let array = ArrayBuilder::new(
        vec![values.len() as u64],
        vec![values.len() as u64],
        data_type::float64(),
        f64::NAN,
    )
    .dimension_names(Some([dim]))
    .attributes(attrs(attributes))
    .build(store.clone(), path)
    .expect("coord builder");
    array.async_store_metadata().await.expect("coord metadata");
    array
        .async_store_chunks(
            &ArraySubset::new_with_shape(array.chunk_grid_shape().to_vec()),
            values.to_vec(),
        )
        .await
        .expect("coord chunks");
}

async fn write_coord_i64(
    store: &Arc<AsyncIcechunkStore>,
    path: &str,
    values: &[i64],
    dim: &str,
    attributes: serde_json::Value,
) {
    let array = ArrayBuilder::new(
        vec![values.len() as u64],
        vec![values.len() as u64],
        data_type::int64(),
        0i64,
    )
    .dimension_names(Some([dim]))
    .attributes(attrs(attributes))
    .build(store.clone(), path)
    .expect("coord builder");
    array.async_store_metadata().await.expect("coord metadata");
    array
        .async_store_chunks(
            &ArraySubset::new_with_shape(array.chunk_grid_shape().to_vec()),
            values.to_vec(),
        )
        .await
        .expect("coord chunks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_local_icechunk_repo() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_repo(dir.path()).await;

    // The engine opens read-only at branch main HEAD. `ZarrEngine::new` is sync
    // and bridges to async via block_in_place — valid here on the multi-thread
    // test runtime.
    let config = ZarrConfig {
        data_path: Some(dir.path().to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        path: None,
        zarr_version: Some(3),
        parameters: None,
        poll_interval_secs: 300,
        cache_mb: 64,
        icechunk: Some(IcechunkConfig {
            branch: Some("main".into()),
            tag: None,
            snapshot: None,
            region: None,
        }),
    };
    let engine = ZarrEngine::new("ic-test", &config).expect("open icechunk repo");

    assert_eq!(engine.get_parameters(), vec!["t2m".to_string()]);
    let (first, last) = engine.get_temporal_extent().unwrap();
    assert_eq!(first, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    assert_eq!(last, Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());

    // Position at an exact grid point (lon=2, lat=58): 273.15 + 5.8 + 0.02 =
    // 278.97 at t=0, +0.5/step.
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let resp = engine
        .query_position("POINT(2 58)", Some((t0, t0)), None, None)
        .expect("position query");
    let qr = match resp {
        CoverageResponse::Single(qr) => qr,
        CoverageResponse::Collection(_) => panic!("expected Single"),
    };
    let v = qr.ranges.get("t2m").unwrap().values[0].expect("value");
    assert!((v - 278.97).abs() < 0.02, "icechunk t2m value {v}");
}
