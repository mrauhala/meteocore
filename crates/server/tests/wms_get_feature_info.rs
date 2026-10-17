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
//!
//! Neither collection lists `edr` for the WMS: the WMS state carries the
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

fn style() -> HashMap<String, StyleInfo> {
    HashMap::from([(
        "default".to_string(),
        StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Viridis,
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

/// A GetFeatureInfo answer: the value, the reported time and run, and the
/// sampled point.
struct Info {
    value: Value,
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

/// The layers GetCapabilities marks queryable are the ones answered above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_mark_the_layers_queryable() {
    for (wms, layers) in [
        (radar().wms, &["radar"][..]),
        (
            arpege().wms,
            &["arpege-uv/10u", "arpege-uv/10v", "arpege-uv/10si"][..],
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
