//! Generate the committed Zarr V3 test fixture under
//! `testdata/zarr-era5-t2m`.
//!
//! Run with `cargo run -p engine-zarr --example gen_fixture`. The output is a
//! small, network-free, CF-conventions Zarr V3 store used by the engine's
//! integration tests and for manual `cargo run -p server` testing. It is
//! deterministic, so re-running it reproduces byte-for-byte the same store
//! (modulo codec library versions).
//!
//! Layout (an ERA5-like snippet of surface fields):
//! - root group `/`           — CF global attributes
//! - `/time`  int64   [4]     — "hours since 2026-01-01", values 0/6/12/18
//! - `/lat`   float64 [12]    — degrees_north, descending 60 → 49
//! - `/lon`   float64 [16]    — degrees_east, 0 → 15
//! - `/msl`        float32 [4,12,16] chunk [4,6,8] gzip — units "Pa"
//! - `/mtpr`       float32 [4,12,16] chunk [4,6,8] gzip — units
//!   "kg m**-2 s**-1" (ERA5's spelling), a precipitation rate
//! - `/t2m`        float32 [4,12,16] chunk [4,6,8] gzip — units "K"
//! - `/t2m_packed` int16   [4,12,16] chunk [4,6,8] gzip — packed via
//!   scale_factor/add_offset, with a `_FillValue` sentinel cell
//!
//! Every field is *linear* in lat and lon, so bilinear interpolation is
//! exact and easy to assert. In the units stored, and as the engine serves
//! them (#1213: °C, hPa, mm/h):
//!
//! ```text
//! t2m[t, j, i]  = 273.15 + 0.1 * lat[j] + 0.01 * lon[i] + 0.5 * t   K
//!               =          0.1 * lat[j] + 0.01 * lon[i] + 0.5 * t   °C
//! msl[t, j, i]  = 101325 + 100 * (lat[j] - 55) - 20 * lon[i] + 50 * t   Pa
//!               = 1013.25 +      (lat[j] - 55) - 0.2 * lon[i] + 0.5 * t  hPa
//! mtpr[t, j, i] = (0.5 + 0.1 * (60 - lat[j]) + 0.05 * lon[i] + 0.25 * t) / 3600
//!                                                         kg m-2 s-1
//!               =  0.5 + 0.1 * (60 - lat[j]) + 0.05 * lon[i] + 0.25 * t  mm/h
//! ```
//!
//! `t2m_packed` encodes the same field as `t2m`. Re-running keeps the
//! directory's `test-config.toml`.

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

/// Mean sea level pressure in Pa.
fn msl(lat: f64, lon: f64, t: usize) -> f64 {
    101_325.0 + 100.0 * (lat - 55.0) - 20.0 * lon + 50.0 * t as f64
}

/// Mean total precipitation rate in kg m-2 s-1: the mm/h rate over 3600.
fn mtpr(lat: f64, lon: f64, t: usize) -> f64 {
    (0.5 + 0.1 * (60.0 - lat) + 0.05 * lon + 0.25 * t as f64) / 3600.0
}

fn main() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/zarr-era5-t2m");
    // Replace the store, keeping the example config that lives beside it.
    if let Ok(entries) = std::fs::read_dir(&out) {
        for entry in entries {
            let path = entry.expect("fixture dir entry").path();
            if path.file_name() == Some("test-config.toml".as_ref()) {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path).expect("remove fixture array");
            } else {
                std::fs::remove_file(&path).expect("remove fixture file");
            }
        }
    }
    std::fs::create_dir_all(&out).expect("create fixture dir");
    let store: Arc<FilesystemStore> =
        Arc::new(FilesystemStore::new(&out).expect("open filesystem store"));

    // Root group with CF global attributes.
    let group = GroupBuilder::new()
        .attributes(attrs(serde_json::json!({
            "Conventions": "CF-1.8",
            "title": "Synthetic ERA5-like surface fields",
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

    // --- float32 fields, clean and linear -------------------------------------
    write_f32_field(
        &store,
        "/t2m",
        &lat_vals,
        &lon_vals,
        field,
        serde_json::json!({
            "units": "K",
            "standard_name": "air_temperature",
            "long_name": "2 metre temperature",
        }),
    );
    write_f32_field(
        &store,
        "/msl",
        &lat_vals,
        &lon_vals,
        msl,
        serde_json::json!({
            "units": "Pa",
            "standard_name": "air_pressure_at_mean_sea_level",
            "long_name": "Mean sea level pressure",
        }),
    );
    write_f32_field(
        &store,
        "/mtpr",
        &lat_vals,
        &lon_vals,
        mtpr,
        serde_json::json!({
            "units": "kg m**-2 s**-1",
            "standard_name": "precipitation_flux",
            "long_name": "Mean total precipitation rate",
        }),
    );

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

/// A float32 `[time, lat, lon]` variable holding `value(lat, lon, t)`,
/// chunked over the whole time axis and split 2 × 2 in space, gzipped.
fn write_f32_field(
    store: &Arc<FilesystemStore>,
    path: &str,
    lat_vals: &[f64],
    lon_vals: &[f64],
    value: fn(f64, f64, usize) -> f64,
    attributes: serde_json::Value,
) {
    let mut values = Vec::with_capacity(NT * NY * NX);
    for t in 0..NT {
        for &lat in lat_vals {
            for &lon in lon_vals {
                values.push(value(lat, lon, t) as f32);
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
    .attributes(attrs(attributes))
    .build(store.clone(), path)
    .expect("build float32 field");
    array.store_metadata().expect("store field metadata");
    let all = ArraySubset::new_with_shape(array.chunk_grid_shape().to_vec());
    array
        .store_chunks(&all, values)
        .expect("store field chunks");
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
