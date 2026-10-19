//! WMS GetFeatureInfo (#1064) on real engines: the value at a pixel is the
//! EDR position query's value at that pixel's centre, at the timestep and
//! run the GetMap of the same map draws.
//!
//! - GeoTIFF: `testdata/radar`, five 5-minute radar frames on an EPSG:4326
//!   grid. The value at a source pixel's centre is pinned to the file's own
//!   (read with GDAL).
//! - GRIB: `testdata/grib-arpege-wind`, ARPEGE 10 m wind on a 2° grid,
//!   served as components only and wrapped in `DerivedWind` as the server
//!   registers it, so the speed is derived; GRIB samples bilinearly between
//!   nodes.
//! - NetCDF: `testdata/meps-netcdf`, a crop of MET Norway's MEPS files on
//!   their Lambert grid, two runs, wrapped in `DerivedWind` for the speed of
//!   its grid-relative 10 m wind (#1067); and `testdata/meps-netcdf-pl`, the
//!   same grid on four pressure levels, where GetMap and GetFeatureInfo
//!   without `ELEVATION` use one level, and OGC API Maps' `elevation`
//!   selects the same levels.
//! - Zarr: `testdata/zarr-era5-t2m`, a synthetic ERA5-like store whose
//!   fields are linear in lat/lon and stored in K, Pa and kg m-2 s-1,
//!   served in °C, hPa and mm/h (#1213).
//!
//! No collection lists `edr` for the WMS: the WMS state carries the
//! engine's EDR side in `point_engines`, as the server wires it.
//!
//! A probe engine checks that both APIs run their engine queries on the one
//! shared query runtime (#1103).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use chrono::{DateTime, Utc};
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult,
};
use ds_core::wind::DerivedWind;
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};
use engine_geotiff::GeoTiffEngine;
use engine_grib::GribEngine;
use engine_netcdf::NetcdfEngine;
use engine_zarr::ZarrEngine;

fn testdata(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(dir)
}

fn collection(id: &str, engine_type: &str, api: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "engine_type": engine_type, "apis": [api],
    }))
    .unwrap()
}

/// One style for every layer: the data-valued wind-speed palette, whose
/// stops (0-50) separate the values GetMap draws.
fn style() -> HashMap<String, StyleInfo> {
    HashMap::from([(
        "default".to_string(),
        StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            palette: ds_render::builtin_palette_arc("wind_speed").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::WindSpeed,
                0.0,
                70.0,
            )),
            min: 0.0,
            max: 70.0,
            parameter: None,
        },
    )])
}

/// A WMS of one collection, whose engine also answers GetFeatureInfo, and
/// an EDR of it.
struct Apis {
    wms: axum::Router,
    edr: axum::Router,
}

fn apis<E>(id: &str, engine_type: &str, engine: Arc<E>, layers: &[&str]) -> Apis
where
    E: MapEngine + EdrEngine + 'static,
{
    let mut styles = HashMap::from([(id.to_string(), style())]);
    for layer in layers {
        styles.insert(format!("{id}/{layer}"), style());
    }
    let edr_engine: Arc<dyn EdrEngine> = engine.clone();
    assert!(api_wms::feature_info::answers_position(edr_engine.as_ref()));
    let wms = api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(id.to_string(), engine as Arc<dyn MapEngine>)]),
        point_engines: HashMap::from([(id.to_string(), edr_engine.clone())]),
        collections: HashMap::from([(id.to_string(), collection(id, engine_type, "wms"))]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(64)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    let edr = api_edr::router(Arc::new(ArcSwap::from_pointee(
        api_edr::handlers::EdrState {
            engines: HashMap::from([(id.to_string(), edr_engine)]),
            feature_engines: HashMap::new(),
            collections: HashMap::from([(id.to_string(), collection(id, engine_type, "edr"))]),
            styles: HashMap::new(),
            base_url: String::new(),
            trust_proxy_headers: false,
        },
    )));
    Apis { wms, edr }
}

struct Reply {
    status: StatusCode,
    x_cache: String,
    body: String,
}

async fn get(app: &axum::Router, uri: &str) -> Reply {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let x_cache = response
        .headers()
        .get("x-cache")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        x_cache,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn json(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
}

/// A GetFeatureInfo answer: the value and its unit, the reported time and
/// run, and the sampled point.
struct Info {
    value: Value,
    unit: Value,
    time: Value,
    reference_time: Value,
    lon: f64,
    lat: f64,
}

/// GetFeatureInfo as JSON at pixel (`i`, `j`) of a `width` × `height` map
/// of `layer` over `bbox` in `crs`, with `extra` parameters.
#[allow(clippy::too_many_arguments)]
async fn feature_info(
    wms: &axum::Router,
    layer: &str,
    crs: &str,
    bbox: &str,
    (width, height): (u32, u32),
    (i, j): (u32, u32),
    extra: &str,
) -> Info {
    let reply = get(
        wms,
        &format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS={layer}&STYLES=\
             &CRS={crs}&BBOX={bbox}&WIDTH={width}&HEIGHT={height}&QUERY_LAYERS={layer}\
             &I={i}&J={j}&INFO_FORMAT=application/json{extra}"
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let feature = json(&reply)["features"][0].clone();
    let properties = &feature["properties"];
    assert_eq!(properties["layer"], layer);
    let coordinates = &feature["geometry"]["coordinates"];
    Info {
        value: properties["value"].clone(),
        unit: properties["unit"].clone(),
        time: properties["time"].clone(),
        reference_time: properties
            .get("reference_time")
            .cloned()
            .unwrap_or(Value::Null),
        lon: coordinates[0].as_f64().unwrap(),
        lat: coordinates[1].as_f64().unwrap(),
    }
}

/// The EDR position value of `parameter` at `(lon, lat)` and `datetime`.
async fn edr_position(
    edr: &axum::Router,
    id: &str,
    parameter: &str,
    (lon, lat): (f64, f64),
    datetime: &str,
) -> Value {
    let reply = get(
        edr,
        &format!(
            "/collections/{id}/position?coords=POINT({lon}%20{lat})\
             &parameter-name={parameter}&datetime={datetime}"
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let coverage = json(&reply);
    let values = coverage["ranges"][parameter]["values"]
        .as_array()
        .unwrap_or_else(|| panic!("no {parameter} values: {coverage}"));
    assert_eq!(values.len(), 1, "{coverage}");
    values[0].clone()
}

fn radar() -> Apis {
    let config = serde_json::from_value(serde_json::json!({
        "filename_template": "radar_%Y%m%dT%H%MZ.tif",
        "parameter": "reflectivity",
        "unit": "dBZ",
        "poll_interval_secs": 3600,
    }))
    .unwrap();
    let dir = testdata("radar");
    let engine = Arc::new(GeoTiffEngine::new("radar", dir.to_str(), &config).unwrap());
    apis("radar", "geotiff", engine, &[])
}

/// The centre of source pixel (921, 597) of the radar frames: origin
/// (0.419004205443889°E, 74.810202268866689°N), 0.011565390529645° pixels.
/// At 23:35 its value is 40.5 dBZ (`gdallocationinfo`).
const RADAR_PIXEL: (f64, f64) = (11.076_511_578_511_981, 67.899_881_427_403_66);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_geotiff_pixel_reads_the_edr_position_value() {
    let Apis { wms, edr } = radar();
    let (lon, lat) = RADAR_PIXEL;
    // A 3 × 3 map centred on the source pixel's centre; (1, 1) is its
    // middle pixel. CRS:84 is lon/lat, EPSG:4326 lat/lon: the same map.
    let d = 0.003;
    let crs84 = format!("{},{},{},{}", lon - d, lat - d, lon + d, lat + d);
    let epsg4326 = format!("{},{},{},{}", lat - d, lon - d, lat + d, lon + d);
    let mut answers = Vec::new();
    for (crs, bbox) in [("CRS:84", &crs84), ("EPSG:4326", &epsg4326)] {
        let info = feature_info(&wms, "radar", crs, bbox, (3, 3), (1, 1), "").await;
        // TIME omitted: the latest frame, which GetMap draws.
        assert_eq!(info.time, "2026-03-24T23:35:00Z", "{crs}");
        assert_eq!(info.reference_time, Value::Null, "{crs}");
        assert!(
            (info.lon - lon).abs() < 1e-9 && (info.lat - lat).abs() < 1e-9,
            "{crs}: ({}, {})",
            info.lon,
            info.lat
        );
        let position = edr_position(
            &edr,
            "radar",
            "reflectivity",
            (info.lon, info.lat),
            "2026-03-24T23:35:00Z",
        )
        .await;
        assert_eq!(info.value, position, "{crs}");
        answers.push(info.value);
    }
    assert_eq!(answers, [40.5, 40.5]);

    // The EPSG:4326 BBOX read as lon/lat is a map 67.9°E, 11.1°N, outside
    // the radar's coverage: no value, and no error.
    let info = feature_info(&wms, "radar", "CRS:84", &epsg4326, (3, 3), (1, 1), "").await;
    assert_eq!(info.value, Value::Null);
}

/// A TIME between frames snaps to the frame the engine draws, latest not
/// after it: GetMap keys that frame, and GetFeatureInfo reports and reads
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_geotiff_time_between_frames_is_the_frame_get_map_keys() {
    let Apis { wms, edr } = radar();
    let (lon, lat) = RADAR_PIXEL;
    let d = 0.003;
    let bbox = format!("{},{},{},{}", lon - d, lat - d, lon + d, lat + d);
    let map = |time: &str| {
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=radar&STYLES=&CRS=CRS:84\
             &BBOX={bbox}&WIDTH=3&HEIGHT=3&FORMAT=image/png&TIME={time}"
        )
    };
    let first = get(&wms, &map("2026-03-24T23:22:00Z")).await;
    assert_eq!(
        (first.status, first.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    assert_eq!(get(&wms, &map("2026-03-24T23:20:00Z")).await.x_cache, "HIT");

    let info = feature_info(
        &wms,
        "radar",
        "CRS:84",
        &bbox,
        (3, 3),
        (1, 1),
        "&TIME=2026-03-24T23:22:00Z",
    )
    .await;
    assert_eq!(info.time, "2026-03-24T23:20:00Z");
    let position = edr_position(
        &edr,
        "radar",
        "reflectivity",
        (info.lon, info.lat),
        "2026-03-24T23:20:00Z",
    )
    .await;
    assert_eq!(info.value, position);
    assert!(info.value.is_number(), "{}", info.value);
}

fn arpege() -> Apis {
    let config = serde_json::from_value(serde_json::json!({
        "data_path": testdata("grib-arpege-wind").to_str().unwrap(),
        "parameters": ["10u", "10v"],
        "grid_cache_mb": 16,
    }))
    .unwrap();
    let grib = Arc::new(GribEngine::new("arpege-uv", &config).unwrap());
    let engine = Arc::new(DerivedWind::new(
        "arpege-uv",
        grib,
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    ));
    apis("arpege-uv", "grib", engine, &["10u", "10v", "10si"])
}

/// Between grid nodes GRIB samples bilinearly, in EDR position and so in
/// GetFeatureInfo; the derived speed is the wrapper's, in both.
///
/// The requests are served by a current-thread runtime on purpose. GRIB
/// spawns its field jobs onto the runtime it runs on, and each job
/// `block_in_place`s, which panics on a current-thread runtime. So the query
/// must run on the dedicated query runtime (`RenderJob::run_query`), never
/// on the serving one (root CLAUDE.md Critical Rule 7); a `spawn_blocking`
/// worker of the serving runtime sent the jobs back to it. The engine is
/// built outside any runtime, as the server builds engines, since its scan
/// bridges storage the same way.
#[test]
fn a_grib_pixel_reads_the_edr_position_value() {
    let Apis { wms, edr } = arpege();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(grib_pixels_read_the_edr_position_value(wms, edr));
}

async fn grib_pixels_read_the_edr_position_value(wms: axum::Router, edr: axum::Router) {
    for parameter in ["10u", "10v", "10si"] {
        let layer = format!("arpege-uv/{parameter}");
        // Pixel (1, 0) of a 2 × 2 map: 11.35°E, 47.75°N, between nodes.
        let info = feature_info(
            &wms,
            &layer,
            "CRS:84",
            "11.2,47.6,11.4,47.8",
            (2, 2),
            (1, 0),
            "",
        )
        .await;
        assert!((info.lon - 11.35).abs() < 1e-9, "{}", info.lon);
        assert!((info.lat - 47.75).abs() < 1e-9, "{}", info.lat);
        // The analysis, of the one run.
        assert_eq!(info.time, "2026-05-11T00:00:00Z", "{parameter}");
        assert_eq!(info.reference_time, "2026-05-11T00:00:00Z", "{parameter}");
        let position = edr_position(
            &edr,
            "arpege-uv",
            parameter,
            (info.lon, info.lat),
            "2026-05-11T00:00:00Z",
        )
        .await;
        assert!(info.value.is_number(), "{parameter}: {}", info.value);
        assert_eq!(info.value, position, "{parameter}");
    }
}

fn meps() -> Apis {
    let config = serde_json::from_value(serde_json::json!({
        "data_path": testdata("meps-netcdf").to_str().unwrap(),
        "filename_template": "meps_sfc_{lead}_%Y%m%dT{run}Z.nc",
        "variables": ["air_temperature_2m", "x_wind_10m", "y_wind_10m",
                      "air_pressure_at_sea_level", "precipitation_amount_acc",
                      "relative_humidity_2m"],
    }))
    .unwrap();
    let netcdf = Arc::new(NetcdfEngine::new("meps", &config).unwrap());
    let engine = Arc::new(DerivedWind::new(
        "meps",
        netcdf,
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    ));
    apis(
        "meps",
        "netcdf",
        engine,
        &[
            "air_temperature_2m",
            "x_wind_10m",
            "y_wind_10m",
            "air_pressure_at_sea_level",
            "precipitation_amount_acc",
            "relative_humidity_2m",
            "wind_speed_10m",
        ],
    )
}

/// Fixture cell (row 5 from the south, column 4) of
/// `testdata/meps-netcdf`: MET Norway's latitude and longitude of it, and
/// its 11Z 10 m wind along the grid's x and y in m/s in
/// `meps_sfc_02_20261015T09Z`, as the CDL holds them.
const MEPS_CELL: (f64, f64) = (13.827_727_538_778_511, 62.552_981_178_508_49);
const MEPS_CELL_11Z: (f64, f64) = (1.137_076_4, 5.762_552_3);

/// [`MEPS_CELL`]'s other 11Z values, as the CDL holds them in the file's
/// unit, converted to what GetFeatureInfo serves and named in the unit it
/// reports (#1209): 2 m temperature from K to °C, MSLP from Pa to hPa, the
/// accumulated precipitation's kg/m^2 as mm, and relative humidity, a
/// fraction in unit 1 whose standard name is `relative_humidity`, as %.
const MEPS_CELL_11Z_DISPLAY: [(&str, f64, &str); 4] = [
    ("air_temperature_2m", 278.38034 - 273.15, "°C"),
    ("air_pressure_at_sea_level", 100_751.086 / 100.0, "hPa"),
    ("precipitation_amount_acc", 0.021_972_656, "mm"),
    ("relative_humidity_2m", 1.000_006 * 100.0, "%"),
];

/// NetCDF (#1067): MEPS on its Lambert grid, south-up rows. The pixel over
/// a grid cell's centre reads the cell's stored value in its display unit
/// (#1209); every pixel reads the EDR position value at its centre, from
/// the step and run GetMap draws: the newest run without `TIME`, the run
/// that has a time the newest lacks. The derived speed is the wrapper's in
/// both. Served by a current-thread runtime, as GRIB's above: the field
/// jobs run on the query runtime.
#[test]
fn a_netcdf_pixel_reads_the_edr_position_value() {
    let Apis { wms, edr } = meps();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(netcdf_pixels_read_the_edr_position_value(wms, edr));
}

async fn netcdf_pixels_read_the_edr_position_value(wms: axum::Router, edr: axum::Router) {
    let (lon, lat) = MEPS_CELL;
    let d = 0.01;
    let bbox = format!("{},{},{},{}", lon - d, lat - d, lon + d, lat + d);
    let (u, v) = MEPS_CELL_11Z;
    let speed = ("wind_speed_10m", u.hypot(v), "m/s");
    for (parameter, served, unit) in MEPS_CELL_11Z_DISPLAY.into_iter().chain([speed]) {
        let layer = format!("meps/{parameter}");
        let info = feature_info(&wms, &layer, "CRS:84", &bbox, (3, 3), (1, 1), "").await;
        assert_eq!(info.time, "2026-10-15T11:00:00Z", "{parameter}");
        assert_eq!(info.reference_time, "2026-10-15T09:00:00Z", "{parameter}");
        let value = info.value.as_f64().unwrap();
        assert!(
            (value - served).abs() < 1e-3,
            "{parameter}: {value} {served}"
        );
        assert_eq!(info.unit, unit, "{parameter}");
        let position = edr_position(
            &edr,
            "meps",
            parameter,
            (info.lon, info.lat),
            "2026-10-15T11:00:00Z",
        )
        .await;
        assert_eq!(info.value, position, "{parameter}");
    }

    // 08Z only the 06Z run has; a pixel between cell centres, in EPSG:3857.
    for parameter in ["air_temperature_2m", "wind_speed_10m"] {
        let info = feature_info(
            &wms,
            &format!("meps/{parameter}"),
            "EPSG:3857",
            "2700000,8400000,2800000,8500000",
            (4, 4),
            (1, 2),
            "&TIME=2026-10-15T08:00:00Z",
        )
        .await;
        assert_eq!(info.time, "2026-10-15T08:00:00Z");
        assert_eq!(info.reference_time, "2026-10-15T06:00:00Z");
        let position = edr_position(
            &edr,
            "meps",
            parameter,
            (info.lon, info.lat),
            "2026-10-15T08:00:00Z",
        )
        .await;
        assert!(info.value.is_number(), "{parameter}: {}", info.value);
        assert_eq!(info.value, position, "{parameter}");
    }
}

/// `testdata/meps-netcdf-pl`'s temperature and wind, wrapped in
/// `DerivedWind` as the server registers it.
fn meps_pl_engine() -> Arc<DerivedWind> {
    let config = serde_json::from_value(serde_json::json!({
        "data_path": testdata("meps-netcdf-pl").to_str().unwrap(),
        "filename_template": "meps_pl_{lead}_%Y%m%dT{run}Z.nc",
        "variables": ["air_temperature_pl", "x_wind_pl", "y_wind_pl"],
    }))
    .unwrap();
    let netcdf = Arc::new(NetcdfEngine::new("meps-pl", &config).unwrap());
    Arc::new(DerivedWind::new(
        "meps-pl",
        netcdf,
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    ))
}

fn meps_pl() -> Apis {
    apis(
        "meps-pl",
        "netcdf",
        meps_pl_engine(),
        &[
            "air_temperature_pl",
            "x_wind_pl",
            "y_wind_pl",
            "wind_speed_pl",
        ],
    )
}

/// `testdata/meps-netcdf-pl` at [`MEPS_CELL`] in `meps_pl_01_20261015T09Z`
/// (10Z, the default step), as the CDL holds them: per level (1000, 850,
/// 500, 300 hPa) the temperature in K, served in °C (#1209), and the wind
/// along the grid's x and y in m/s.
const MEPS_PL_CELL_10Z: [(f64, f64, f64, f64); 4] = [
    (1000.0, 281.9298, 0.607_873_9, 5.187_544),
    (850.0, 277.650_57, 20.274_448, 14.588_396),
    (500.0, 260.222, 20.465_487, 18.797_607),
    (300.0, 231.872_45, 36.897_503, 15.632_816),
];

/// Pressure levels (#1067 PR 8): GetMap and GetFeatureInfo without
/// `ELEVATION` use one level, the first advertised (1000 hPa): the GetMap
/// image is the one `ELEVATION=1000` draws, and GetFeatureInfo reports
/// that level and reads its stored value, as the EDR position query at it
/// does. With `ELEVATION` each reads its own level, the derived speed from
/// both components at that level. Capabilities advertise the levels.
#[test]
fn a_netcdf_pressure_level_pixel_reads_the_level_get_map_draws() {
    let Apis { wms, edr } = meps_pl();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(netcdf_pressure_level_pixels(wms, edr));
}

async fn netcdf_pressure_level_pixels(wms: axum::Router, edr: axum::Router) {
    let (lon, lat) = MEPS_CELL;
    let d = 0.01;
    let bbox = format!("{},{},{},{}", lon - d, lat - d, lon + d, lat + d);
    let layer = "meps-pl/air_temperature_pl";
    // The image of the wind speed, which the test style's palette
    // separates by level, as bytes.
    let get_map = |extra: &str| {
        let uri = format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=meps-pl/wind_speed_pl&STYLES=\
             &CRS=EPSG:3857&BBOX=1300000,8500000,2300000,9500000&WIDTH=32&HEIGHT=32\
             &FORMAT=image/png{extra}"
        );
        let wms = wms.clone();
        async move {
            let response = wms
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_ne!(
                response
                    .headers()
                    .get("x-cache")
                    .map(|v| v.to_str().unwrap()),
                Some("ERROR")
            );
            response.into_body().collect().await.unwrap().to_bytes()
        }
    };
    let default = get_map("").await;
    let bottom = get_map("&ELEVATION=1000").await;
    let top = get_map("&ELEVATION=300").await;
    assert_eq!(default, bottom, "GetMap draws 1000 hPa by default");
    assert_ne!(default, top);

    let info = feature_info(&wms, layer, "CRS:84", &bbox, (3, 3), (1, 1), "").await;
    let reply = get(
        &wms,
        &format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS={layer}&STYLES=\
             &CRS=CRS:84&BBOX={bbox}&WIDTH=3&HEIGHT=3&QUERY_LAYERS={layer}&I=1&J=1\
             &INFO_FORMAT=application/json"
        ),
    )
    .await;
    assert_eq!(
        json(&reply)["features"][0]["properties"]["elevation"],
        serde_json::json!(1000.0),
        "GetFeatureInfo reports the level GetMap draws"
    );
    assert_eq!(info.time, "2026-10-15T10:00:00Z");
    assert_eq!(info.reference_time, "2026-10-15T09:00:00Z");
    for (hpa, t, u, v) in MEPS_PL_CELL_10Z {
        // 1000 hPa without ELEVATION: the level GetMap draws by default.
        let extra = if hpa == 1000.0 {
            String::new()
        } else {
            format!("&ELEVATION={hpa}")
        };
        let info = feature_info(&wms, layer, "CRS:84", &bbox, (3, 3), (1, 1), &extra).await;
        let value = info.value.as_f64().unwrap();
        let celsius = t - 273.15;
        assert!((value - celsius).abs() < 1e-3, "{hpa}: {value} {celsius}");
        assert_eq!(info.unit, "°C", "{hpa}");
        let speed = feature_info(
            &wms,
            "meps-pl/wind_speed_pl",
            "CRS:84",
            &bbox,
            (3, 3),
            (1, 1),
            &format!("&ELEVATION={hpa}"),
        )
        .await;
        let value = speed.value.as_f64().unwrap();
        assert!((value - u.hypot(v)).abs() < 1e-3, "{hpa}: {value}");
        let position = get(
            &edr,
            &format!(
                "/collections/meps-pl/position?coords=POINT({}%20{})\
                 &parameter-name=air_temperature_pl&datetime=2026-10-15T10:00:00Z&z={hpa}",
                info.lon, info.lat
            ),
        )
        .await;
        assert_eq!(position.status, StatusCode::OK, "{}", position.body);
        assert_eq!(
            json(&position)["ranges"]["air_temperature_pl"]["values"][0],
            info.value,
            "{hpa}"
        );
    }

    let capabilities = get(&wms, "/?SERVICE=WMS&REQUEST=GetCapabilities").await;
    assert!(
        capabilities.body.contains(
            "<Dimension name=\"elevation\" units=\"hPa\" default=\"1000\" \
             nearestValue=\"1\">1000,850,500,300</Dimension>"
        ),
        "{}",
        capabilities.body
    );
}

/// OGC API Maps `elevation` on pressure levels (#1067 PR 8): a map without
/// it draws the bottom level, as `elevation=1000` does, `elevation=300`
/// another image, and a level the collection lacks is a 400.
#[test]
fn a_netcdf_pressure_level_map_draws_the_elevation_asked_for() {
    let id = "meps-pl";
    let maps = api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines: HashMap::from([(id.to_string(), meps_pl_engine() as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(id.to_string(), collection(id, "netcdf", "maps"))]),
        styles: HashMap::from([(id.to_string(), style())]),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    })));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let map = |extra: &str| {
                let uri = format!(
                    "/collections/{id}/map?bbox=11.7,60.6,20.7,64.9&width=32&height=32\
                     &parameter-name=wind_speed_pl{extra}"
                );
                let maps = maps.clone();
                async move {
                    let response = maps
                        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    let status = response.status();
                    (
                        status,
                        response.into_body().collect().await.unwrap().to_bytes(),
                    )
                }
            };
            let (status, default) = map("").await;
            assert_eq!(status, StatusCode::OK);
            assert!(default.starts_with(b"\x89PNG"));
            let (status, bottom) = map("&elevation=1000").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(default, bottom, "Maps draws 1000 hPa by default");
            let (status, top) = map("&elevation=300").await;
            assert_eq!(status, StatusCode::OK);
            assert_ne!(default, top, "300 hPa is another image");
            let (status, body) = map("&elevation=600").await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
        });
}

fn era5() -> Apis {
    let config = ds_core::config::ZarrConfig::auto_local(
        testdata("zarr-era5-t2m").to_string_lossy().into_owned(),
    );
    let engine = Arc::new(ZarrEngine::new("era5", &config).unwrap());
    apis(
        "era5",
        "zarr",
        engine,
        &["msl", "mtpr", "t2m", "t2m_packed"],
    )
}

/// Zarr (#1213): at a grid cell's centre, 8°E 54°N, the latest step
/// (18Z, the fixture's fourth), GetFeatureInfo reads each field in its
/// display unit, converted from the store's, and names that unit; the EDR
/// position value there is the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zarr_pixel_reads_the_display_unit_value() {
    let Apis { wms, edr } = era5();
    let (lon, lat) = (8.0, 54.0);
    let d = 0.003;
    let bbox = format!("{},{},{},{}", lon - d, lat - d, lon + d, lat + d);
    for (parameter, served, unit, tolerance) in [
        // 273.15 + 5.4 + 0.08 + 1.5 K.
        ("t2m", 5.4 + 0.08 + 1.5, "°C", 1e-4),
        // The packed copy holds the same field to 0.01 K.
        ("t2m_packed", 5.4 + 0.08 + 1.5, "°C", 6e-3),
        // 101 325 + 100 × (54 - 55) - 20 × 8 + 50 × 3 Pa.
        ("msl", 1013.25 - 1.0 - 1.6 + 1.5, "hPa", 1e-3),
        // (0.5 + 0.6 + 0.4 + 0.75) / 3600 kg m-2 s-1.
        ("mtpr", 0.5 + 0.6 + 0.4 + 0.75, "mm/h", 1e-5),
    ] {
        let layer = format!("era5/{parameter}");
        let info = feature_info(&wms, &layer, "CRS:84", &bbox, (3, 3), (1, 1), "").await;
        assert_eq!(info.time, "2026-01-01T18:00:00Z", "{parameter}");
        assert_eq!(info.reference_time, Value::Null, "{parameter}");
        let value = info.value.as_f64().unwrap();
        assert!(
            (value - served).abs() < tolerance,
            "{parameter}: {value}, expected {served} {unit}"
        );
        assert_eq!(info.unit, unit, "{parameter}");
        let position = edr_position(
            &edr,
            "era5",
            parameter,
            (info.lon, info.lat),
            "2026-01-01T18:00:00Z",
        )
        .await;
        assert_eq!(info.value, position, "{parameter}");
    }
}

/// The layers GetCapabilities marks queryable are the ones answered above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_mark_the_layers_queryable() {
    for (wms, layers) in [
        (radar().wms, &["radar"][..]),
        (
            arpege().wms,
            &["arpege-uv/10u", "arpege-uv/10v", "arpege-uv/10si"][..],
        ),
        (
            meps().wms,
            &["meps/air_temperature_2m", "meps/wind_speed_10m"][..],
        ),
    ] {
        let reply = get(&wms, "/?SERVICE=WMS&REQUEST=GetCapabilities").await;
        assert_eq!(reply.status, StatusCode::OK);
        for layer in layers {
            assert!(
                reply.body.contains(&format!(
                    "<Layer queryable=\"1\" opaque=\"1\"><Name>{layer}</Name>"
                )),
                "{layer}: {}",
                reply.body
            );
        }
        assert!(reply
            .body
            .contains("<GetFeatureInfo><Format>text/plain</Format>"));
    }
}

/// Where a position query ran, and the deadline it ran under.
struct Ran {
    runtime: tokio::runtime::Id,
    thread: Option<String>,
    deadline: Option<Instant>,
}

/// The one step of the probe's position answers.
fn probe_time() -> DateTime<Utc> {
    "2026-10-17T00:00:00Z".parse().unwrap()
}

/// A one-parameter engine over 0–10°E, 0–10°N that records where each
/// position query runs and answers 1.0.
#[derive(Default)]
struct Probe {
    ran: Mutex<Vec<Ran>>,
}

impl MapEngine for Probe {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(1.0); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([0.0, 0.0, 10.0, 10.0]),
            times: Vec::new(),
            parameter: "v".into(),
            unit: "1".into(),
            parameters: vec![ParameterInfo {
                name: "v".into(),
                title: "v".into(),
                unit: "1".into(),
            }],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

impl EdrEngine for Probe {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound("none".into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["v".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((probe_time(), probe_time()))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([0.0, 0.0, 10.0, 10.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into()]
    }

    fn query_position(
        &self,
        coords: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.ran.lock().unwrap().push(Ran {
            runtime: tokio::runtime::Handle::current().id(),
            thread: std::thread::current().name().map(str::to_owned),
            deadline: ds_core::deadline::current(),
        });
        let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: vec![probe_time()],
                z: None,
            },
            parameters: HashMap::from([(
                "v".to_string(),
                ParameterDescription {
                    label: "v".into(),
                    unit: "1".into(),
                    observed_property: "v".into(),
                    standard_name: None,
                },
            )]),
            ranges: HashMap::from([(
                "v".to_string(),
                NdArray {
                    shape: vec![1],
                    axis_names: vec!["t".into()],
                    values: vec![Some(1.0)],
                },
            )]),
        }))
    }
}

/// EDR and WMS GetFeatureInfo run their engine queries on one shared query
/// runtime (#1103), ds-executor's, never on the runtime serving the
/// request, and each under its own deadline: an EDR query keeps its 30 s.
/// Before #1103 each API had a runtime of its own.
#[tokio::test]
async fn edr_and_get_feature_info_share_one_query_runtime() {
    let probe = Arc::new(Probe::default());
    let Apis { wms, edr } = apis("probe", "geotiff", probe.clone(), &[]);
    let serving = tokio::runtime::Handle::current().id();

    let info = feature_info(&wms, "probe", "CRS:84", "0,0,10,10", (2, 2), (0, 0), "").await;
    assert_eq!(info.value, 1.0);
    let start = Instant::now();
    let position = edr_position(&edr, "probe", "v", (2.5, 7.5), "2026-10-17T00:00:00Z").await;
    assert_eq!(position, 1.0);

    let ran = probe.ran.lock().unwrap();
    let [get_feature_info, edr] = &ran[..] else {
        panic!("{} queries", ran.len());
    };
    assert_eq!(get_feature_info.runtime, edr.runtime);
    assert_eq!(edr.runtime, ds_executor::query_runtime().id());
    assert_ne!(edr.runtime, serving);
    for query in [get_feature_info, edr] {
        assert_eq!(
            query.thread.as_deref(),
            Some(ds_executor::QUERY_THREAD_NAME)
        );
    }
    let budget = edr.deadline.expect("an EDR query runs under its deadline") - start;
    assert!(
        Duration::from_secs(30) <= budget && budget < Duration::from_secs(31),
        "{budget:?}"
    );
    assert!(get_feature_info.deadline.is_some());
}
