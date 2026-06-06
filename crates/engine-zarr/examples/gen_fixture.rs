//! Generate the committed Zarr V3 test fixture under
//! `testdata/zarr-era5-t2m`.
//!
//! Run with `cargo run -p engine-zarr --example gen_fixture`. The output is a
//! small, network-free, CF-conventions Zarr V3 store used by the engine's
//! integration tests and for manual `cargo run -p server` testing. It is
//! deterministic, so re-running it reproduces byte-for-byte the same store
//! (modulo codec library versions).
//!
//! Layout (an ERA5-like 2 m temperature snippet):
//! - root group `/`           — CF global attributes
//! - `/time`  int64   [4]     — "hours since 2026-01-01", values 0/6/12/18
//! - `/lat`   float64 [12]    — degrees_north, descending 60 → 49
//! - `/lon`   float64 [16]    — degrees_east, 0 → 15
//! - `/t2m`        float32 [4,12,16] chunk [4,6,8] gzip — units "K"
//! - `/t2m_packed` int16   [4,12,16] chunk [4,6,8] gzip — packed via
//!   scale_factor/add_offset, with a `_FillValue` sentinel cell
//!
//! Both data variables encode the same exact field, which is *linear* in lat
//! and lon so bilinear interpolation is exact and easy to assert:
//!
//! ```text
//! t2m[t, j, i] = 273.15 + 0.1 * lat[j] + 0.01 * lon[i] + 0.5 * t
//! ```

use std::sync::Arc;

use zarrs::array::{codec::GzipCodec, data_type, ArrayBuilder, ArraySubset};
use zarrs::filesystem::FilesystemStore;
use zarrs::group::GroupBuilder;

const NT: usize = 4;
const NY: usize = 12;
const NX: usize = 16;

const SCALE: f64 = 0.01;
const OFFSET: f64 = 273.15;
const FILL_I16: i16 = -9999;

fn attrs(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object()
        .expect("attribute literal is an object")
        .clone()
}

fn field(lat: f64, lon: f64, t: usize) -> f64 {
    273.15 + 0.1 * lat + 0.01 * lon + 0.5 * t as f64
}

fn main() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/zarr-era5-t2m");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create fixture dir");
    let store: Arc<FilesystemStore> =
        Arc::new(FilesystemStore::new(&out).expect("open filesystem store"));

    // Root group with CF global attributes.
    let group = GroupBuilder::new()
        .attributes(attrs(serde_json::json!({
            "Conventions": "CF-1.8",
            "title": "Synthetic ERA5-like 2 m temperature",
        })))
        .build(store.clone(), "/")
        .expect("build root group");
    group.store_metadata().expect("store root metadata");

    // --- Coordinate variables -------------------------------------------------
    let time_vals: Vec<i64> = vec![0, 6, 12, 18];
    write_i64_coord(
        &store,
        "/time",
        &time_vals,
        "time",
        serde_json::json!({
            "units": "hours since 2026-01-01 00:00:00",
            "calendar": "proleptic_gregorian",
            "standard_name": "time",
            "long_name": "time",
        }),
    );

    let lat_vals: Vec<f64> = (0..NY).map(|j| 60.0 - j as f64).collect(); // descending
    write_f64_coord(
        &store,
        "/lat",
        &lat_vals,
        "lat",
        serde_json::json!({
            "units": "degrees_north",
            "standard_name": "latitude",
            "long_name": "latitude",
        }),
    );

    let lon_vals: Vec<f64> = (0..NX).map(|i| i as f64).collect();
    write_f64_coord(
        &store,
        "/lon",
        &lon_vals,
        "lon",
        serde_json::json!({
            "units": "degrees_east",
            "standard_name": "longitude",
            "long_name": "longitude",
        }),
    );

    // --- t2m: float32, clean linear field ------------------------------------
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
        vec![NT as u64, 6, 8], // chunk: whole time axis, spatial split 2x2
        data_type::float32(),
        f32::NAN,
    )
    .dimension_names(Some(["time", "lat", "lon"]))
    .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(5).expect("gzip level"))])
    .attributes(attrs(serde_json::json!({
        "units": "K",
        "standard_name": "air_temperature",
        "long_name": "2 metre temperature",
    })))
    .build(store.clone(), "/t2m")
    .expect("build t2m");
    array.store_metadata().expect("store t2m metadata");
    let all = ArraySubset::new_with_shape(array.chunk_grid_shape().to_vec());
    array.store_chunks(&all, t2m).expect("store t2m chunks");

    // --- t2m_packed: int16 with scale_factor/add_offset + a fill sentinel ----
    let mut packed = Vec::with_capacity(NT * NY * NX);
    for t in 0..NT {
        for &lat in &lat_vals {
            for &lon in &lon_vals {
                let raw = ((field(lat, lon, t) - OFFSET) / SCALE).round() as i16;
                packed.push(raw);
            }
        }
    }
    // Mark a 2x2 block in the NW corner of the t=0 plane as missing, so a query
    // centred in it (POINT(0.5 59.5)) returns None for every neighbour and
    // exercises `_FillValue` → None end-to-end. lat[0]=60, lat[1]=59;
    // lon[0]=0, lon[1]=1.
    for j in 0..2 {
        for i in 0..2 {
            packed[j * NX + i] = FILL_I16;
        }
    }
    let array = ArrayBuilder::new(
        vec![NT as u64, NY as u64, NX as u64],
        vec![NT as u64, 6, 8],
        data_type::int16(),
        FILL_I16,
    )
    .dimension_names(Some(["time", "lat", "lon"]))
    .bytes_to_bytes_codecs(vec![Arc::new(GzipCodec::new(5).expect("gzip level"))])
    .attributes(attrs(serde_json::json!({
        "units": "K",
        "standard_name": "air_temperature",
        "long_name": "2 metre temperature (packed)",
        "scale_factor": SCALE,
        "add_offset": OFFSET,
        "_FillValue": FILL_I16,
    })))
    .build(store.clone(), "/t2m_packed")
    .expect("build t2m_packed");
    array.store_metadata().expect("store t2m_packed metadata");
    let all = ArraySubset::new_with_shape(array.chunk_grid_shape().to_vec());
    array
        .store_chunks(&all, packed)
        .expect("store t2m_packed chunks");

    println!("wrote fixture to {}", out.display());
}

fn write_f64_coord(
    store: &Arc<FilesystemStore>,
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
    .expect("build f64 coord");
    array.store_metadata().expect("store coord metadata");
    array
        .store_chunk(&[0], values.to_vec())
        .expect("store coord chunk");
}

fn write_i64_coord(
    store: &Arc<FilesystemStore>,
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
    .expect("build i64 coord");
    array.store_metadata().expect("store coord metadata");
    array
        .store_chunk(&[0], values.to_vec())
        .expect("store coord chunk");
}
