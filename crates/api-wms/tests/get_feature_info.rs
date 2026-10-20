//! WMS 1.3.0 GetFeatureInfo (#1064) against mock engines: the value is the
//! collection's EDR position query at the centre of pixel (I, J), at the
//! timestep, run and level the GetMap of the same map draws; the WMS 1.3.0
//! exceptions; the formats; and what GetCapabilities advertises.
//!
//! The real-engine twin, the value at a pixel of a GeoTIFF and a GRIB
//! fixture against their EDR position answers, is
//! `crates/server/tests/wms_get_feature_info.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{
    CompositeChannel, CompositeDef, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile,
};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, VerticalCoord,
};
use ds_core::vertical::{VerticalDimension, VerticalKind};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

fn at(time: &str) -> DateTime<Utc> {
    time.parse().unwrap()
}

/// The valid times, hourly.
const TIMES: [&str; 3] = [
    "2026-10-01T00:00:00Z",
    "2026-10-01T01:00:00Z",
    "2026-10-01T02:00:00Z",
];
/// The model runs, ascending.
const RUNS: [&str; 2] = ["2026-09-30T12:00:00Z", "2026-10-01T00:00:00Z"];
/// The levels, the first the default.
const LEVELS: [f64; 2] = [850.0, 500.0];

/// What a mock position query answers.
#[derive(Clone, Copy)]
enum Answer {
    Value(f64),
    Nodata,
    /// `LocationNotFound`: no data at the point.
    NotFound,
    /// `ReferenceTimeNotFound`: the run was evicted after the map resolved.
    RunEvicted,
    /// `ResourceExhausted`.
    Busy,
    /// An internal failure whose message must not reach the client.
    Fails,
}

/// One position query the engine received.
#[derive(Debug, Clone)]
struct Call {
    coords: String,
    datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
    parameters: Option<Vec<String>>,
    z: Option<Vec<f64>>,
    reference_time: Option<DateTime<Utc>>,
}

/// An NWP collection: a temperature `t2m`, an overlay layer `outline` that
/// no position query answers, and an RGB composite `rgb`; hourly valid
/// times, two runs, two pressure levels. Times snap latest-not-after, as a
/// file catalog's do.
struct Nwp {
    answer: Answer,
    unit: &'static str,
    content_version: u64,
    /// How long a position query takes.
    delay: Duration,
    calls: Mutex<Vec<Call>>,
}

impl Nwp {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            unit: "K",
            content_version: 0,
            delay: Duration::ZERO,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// An engine whose position query takes `delay`, then answers 280 K.
    fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            answer: Answer::Value(280.0),
            unit: "K",
            content_version: 0,
            delay,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl MapEngine for Nwp {
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
            values: vec![Some(280.0); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let parameter = |name: &str| ParameterInfo {
            name: name.into(),
            title: name.into(),
            unit: "K".into(),
        };
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([0.0, 40.0, 40.0, 80.0]),
            times: TIMES.iter().map(|t| at(t)).collect(),
            parameter: "t2m".into(),
            unit: "K".into(),
            parameters: vec![parameter("t2m"), parameter("outline")],
            vertical: Some(VerticalDimension::new(
                VerticalKind::Pressure,
                LEVELS.to_vec(),
            )),
            grid_size: None,
            layer_subtitle: None,
            reference_times: RUNS.iter().map(|t| at(t)).collect(),
            native_grid: None,
        }
    }

    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let times: Vec<_> = TIMES.iter().map(|t| at(t)).collect();
        match time {
            None => times.last().copied(),
            Some(time) => times.into_iter().rev().find(|&t| t <= time),
        }
    }

    fn resolve_reference_time(
        &self,
        _time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        reference_time.or(Some(at(RUNS[1])))
    }

    fn content_version(&self) -> u64 {
        self.content_version
    }

    fn composites(&self) -> Arc<[CompositeDef]> {
        let channel = || CompositeChannel {
            parameter: "t2m".into(),
            minus: None,
            min: 250.0,
            max: 300.0,
            gamma: 1.0,
        };
        Arc::from([CompositeDef {
            name: "rgb".into(),
            title: "RGB".into(),
            channels: [channel(), channel(), channel()],
        }])
    }

    fn layer_shows_values(&self, parameter: &str) -> bool {
        parameter != "outline"
    }

    /// The level a map draws: the nearest level to `ELEVATION`, else the
    /// first; the older run has only 500 hPa, which its maps draw.
    fn resolve_elevation(
        &self,
        _parameter: Option<&str>,
        z: Option<f64>,
        _time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<f64> {
        if reference_time == Some(at(RUNS[0])) {
            return Some(500.0);
        }
        let z = z.unwrap_or(LEVELS[0]);
        LEVELS
            .into_iter()
            .min_by(|a, b| (a - z).abs().total_cmp(&(b - z).abs()))
    }
}

impl EdrEngine for Nwp {
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
        vec!["t2m".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((at(TIMES[0]), at(TIMES[2])))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([0.0, 40.0, 40.0, 80.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into(), "area".into()]
    }

    /// A one-step series at the requested time and level.
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.calls.lock().unwrap().push(Call {
            coords: coords.into(),
            datetime,
            parameters: parameters.map(<[String]>::to_vec),
            z: z.map(<[f64]>::to_vec),
            reference_time,
        });
        std::thread::sleep(self.delay);
        let value = match self.answer {
            Answer::Value(value) => Some(value),
            Answer::Nodata => None,
            Answer::NotFound => {
                return Err(DataServerError::LocationNotFound("outside".into()));
            }
            Answer::RunEvicted => {
                return Err(DataServerError::ReferenceTimeNotFound("evicted".into()));
            }
            Answer::Busy => return Err(DataServerError::ResourceExhausted),
            Answer::Fails => {
                return Err(DataServerError::Storage(
                    "connection refused at 10.0.0.5:9000/secret-bucket".into(),
                ));
            }
        };
        let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
        let level = z.map_or(LEVELS[0], |z| z[0]);
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: vec![datetime.map_or(at(TIMES[2]), |(start, _)| start)],
                z: Some(VerticalCoord {
                    kind: VerticalKind::Pressure,
                    values: vec![level],
                }),
            },
            parameters: HashMap::from([(
                "t2m".to_string(),
                ParameterDescription {
                    label: "Temperature".into(),
                    unit: self.unit.into(),
                    observed_property: "t2m".into(),
                    standard_name: None,
                },
            )]),
            ranges: HashMap::from([(
                "t2m".to_string(),
                NdArray {
                    shape: vec![1],
                    axis_names: vec!["t".into()],
                    values: vec![value],
                },
            )]),
        }))
    }
}

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "apis": ["wms"], "engine_type": "grib",
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
                250.0,
                300.0,
            )),
            min: 250.0,
            max: 300.0,
            parameter: None,
        },
    )])
}

/// A WMS serving `nwp` (`engine`, which answers position queries) and
/// `plain`, a map-only collection with no position engine.
fn router(engine: Arc<Nwp>) -> axum::Router {
    router_with(engine, Nwp::new(Answer::Value(300.0)))
}

/// [`router`] with `nwp2` too, a second collection whose engine, `nwp2`,
/// answers position queries.
fn router_with(engine: Arc<Nwp>, nwp2: Arc<Nwp>) -> axum::Router {
    let plain: Arc<dyn MapEngine> = Nwp::new(Answer::Value(0.0));
    let mut styles = HashMap::new();
    for layer in [
        "nwp",
        "nwp/t2m",
        "nwp/outline",
        "nwp2",
        "nwp2/t2m",
        "plain",
        "plain/t2m",
    ] {
        styles.insert(layer.to_string(), style());
    }
    let point_engines: HashMap<String, Arc<dyn EdrEngine>> =
        [("nwp", engine.clone()), ("nwp2", nwp2.clone())]
            .into_iter()
            .filter(|(_, engine)| api_wms::feature_info::answers_position(engine.as_ref()))
            .map(|(id, engine)| (id.to_string(), engine as Arc<dyn EdrEngine>))
            .collect();
    api_wms::router(Arc::new(ArcSwap::from_pointee(WmsState {
        engines: HashMap::from([
            ("nwp".to_string(), engine as Arc<dyn MapEngine>),
            ("nwp2".to_string(), nwp2 as Arc<dyn MapEngine>),
            ("plain".to_string(), plain),
        ]),
        point_engines,
        collections: HashMap::from([
            ("nwp".to_string(), collection("nwp")),
            ("nwp2".to_string(), collection("nwp2")),
            ("plain".to_string(), collection("plain")),
        ]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

struct Reply {
    status: StatusCode,
    content_type: String,
    cache_control: String,
    x_cache: String,
    retry_after: Option<String>,
    body: String,
}

async fn get(app: &axum::Router, query: &str) -> Reply {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let (content_type, cache_control, x_cache, retry_after) = (
        header("content-type").unwrap_or_default(),
        header("cache-control").unwrap_or_default(),
        header("x-cache").unwrap_or_default(),
        header("retry-after"),
    );
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        cache_control,
        x_cache,
        retry_after,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// A GetFeatureInfo on `layer`, a 20 × 10 map of lon 10–30°, lat 50–60°
/// in CRS:84, at pixel (`i`, `j`), plus `extra` parameters.
fn gfi(layer: &str, i: u32, j: u32, extra: &str) -> String {
    format!(
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS={layer}&STYLES=\
         &CRS=CRS:84&BBOX=10,50,30,60&WIDTH=20&HEIGHT=10&QUERY_LAYERS={layer}\
         &I={i}&J={j}{extra}"
    )
}

/// [`gfi`] on a map of `layers`, reading `query_layers`.
fn gfi_layers(layers: &str, query_layers: &str, extra: &str) -> String {
    gfi(layers, 0, 0, extra).replacen(
        &format!("QUERY_LAYERS={layers}"),
        &format!("QUERY_LAYERS={query_layers}"),
        1,
    )
}

fn json(reply: &Reply) -> serde_json::Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
}

/// The `code` of a ServiceExceptionReport, read with quick-xml.
fn exception_code(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "ServiceException" => {
                let attr = e.try_get_attribute("code").ok()??;
                return Some(attr.value.into_owned());
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// `POINT(lon lat)` as numbers.
fn point(coords: &str) -> (f64, f64) {
    let (lat, lon) = ds_core::feature::parse_point_coords(coords).unwrap();
    (lon, lat)
}

fn assert_close(actual: (f64, f64), expected: (f64, f64)) {
    assert!(
        (actual.0 - expected.0).abs() < 1e-9 && (actual.1 - expected.1).abs() < 1e-9,
        "{actual:?} != {expected:?}"
    );
}

#[tokio::test]
async fn the_value_is_the_position_query_at_the_frame_drawn() {
    let engine = Nwp::new(Answer::Value(271.5));
    let app = router(engine.clone());
    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/json")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.content_type, "application/json");

    // The pixel's centre, at the default time, the latest run and the
    // default level: what GetMap draws without TIME, DIM_REFERENCE_TIME
    // and ELEVATION.
    let calls = engine.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_close(point(&call.coords), (10.5, 59.5));
    assert_eq!(call.datetime, Some((at(TIMES[2]), at(TIMES[2]))));
    assert_eq!(call.parameters.as_deref(), Some(&["t2m".to_string()][..]));
    assert_eq!(call.z.as_deref(), Some(&[850.0][..]));
    assert_eq!(call.reference_time, Some(at(RUNS[1])));

    let body = json(&reply);
    assert_eq!(body["type"], "FeatureCollection");
    let feature = &body["features"][0];
    assert_eq!(feature["type"], "Feature");
    assert_eq!(feature["geometry"]["type"], "Point");
    assert_eq!(
        feature["geometry"]["coordinates"],
        serde_json::json!([10.5, 59.5])
    );
    assert_eq!(
        feature["properties"],
        serde_json::json!({
            "layer": "nwp/t2m",
            "parameter": "t2m",
            "value": 271.5,
            "unit": "K",
            "time": "2026-10-01T02:00:00Z",
            "reference_time": "2026-10-01T00:00:00Z",
            "elevation": 850.0,
        })
    );

    // ELEVATION and a pinned older run reach the query as given.
    let reply = get(
        &app,
        &gfi(
            "nwp/t2m",
            0,
            0,
            "&INFO_FORMAT=application/json&ELEVATION=500\
             &DIM_REFERENCE_TIME=2026-09-30T12:00:00Z",
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let call = engine.calls().pop().unwrap();
    assert_eq!(call.z.as_deref(), Some(&[500.0][..]));
    assert_eq!(call.reference_time, Some(at(RUNS[0])));
    let properties = &json(&reply)["features"][0]["properties"];
    assert_eq!(properties["elevation"], 500.0);
    assert_eq!(properties["reference_time"], "2026-09-30T12:00:00Z");
}

/// The level queried and reported is the one GetMap draws, as the engine
/// resolves it (`MapEngine::resolve_elevation`), not `ELEVATION` or the
/// first advertised level as such. Here an `ELEVATION` between levels
/// draws the nearest, and the older run, which lacks the first level,
/// draws its own.
#[tokio::test]
async fn the_level_is_the_one_get_map_draws() {
    let engine = Nwp::new(Answer::Value(250.0));
    let app = router(engine.clone());
    for (extra, level) in [
        ("", 850.0),
        ("&ELEVATION=600", 500.0),
        ("&ELEVATION=800", 850.0),
        ("&DIM_REFERENCE_TIME=2026-09-30T12:00:00Z", 500.0),
    ] {
        let reply = get(
            &app,
            &gfi(
                "nwp/t2m",
                0,
                0,
                &format!("&INFO_FORMAT=application/json{extra}"),
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{extra}: {}", reply.body);
        let call = engine.calls().pop().unwrap();
        assert_eq!(call.z.as_deref(), Some(&[level][..]), "{extra}");
        let properties = &json(&reply)["features"][0]["properties"];
        assert_eq!(properties["elevation"], level, "{extra}");
    }
}

/// WMS 1.3.0 axis order: EPSG:4326's BBOX is lat/lon, CRS:84's lon/lat,
/// and EPSG:3857's is metres; each names the same pixel centre here. J
/// counts rows from the top.
#[tokio::test]
async fn the_pixel_centre_follows_each_crs_axis_order() {
    let engine = Nwp::new(Answer::Value(1.0));
    let app = router(engine.clone());
    let base = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=nwp/t2m\
                &QUERY_LAYERS=nwp/t2m&WIDTH=20&HEIGHT=10&I=3&J=7";
    let (x0, x1) = (
        ds_core::web_mercator::lon_to_x(10.0),
        ds_core::web_mercator::lon_to_x(30.0),
    );
    let (y0, y1) = (
        ds_core::web_mercator::lat_to_y(50.0),
        ds_core::web_mercator::lat_to_y(60.0),
    );
    for (crs, bbox) in [
        ("CRS:84", "10,50,30,60".to_string()),
        ("EPSG:4326", "50,10,60,30".to_string()),
        ("EPSG:3857", format!("{x0},{y0},{x1},{y1}")),
    ] {
        let reply = get(&app, &format!("{base}&CRS={crs}&BBOX={bbox}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{crs}: {}", reply.body);
        let (lon, lat) = point(&engine.calls().pop().unwrap().coords);
        // Column 3 of 20 over 20°: 13.5°E in every CRS.
        assert!((lon - 13.5).abs() < 1e-9, "{crs}: lon {lon}");
        let expected_lat = match crs {
            // Row 7 of 10 over 10° of latitude.
            "CRS:84" | "EPSG:4326" => 52.5,
            // Row 7 of 10 over the Mercator northings.
            _ => ds_core::web_mercator::y_to_lat(y1 - 0.75 * (y1 - y0)),
        };
        assert!((lat - expected_lat).abs() < 1e-9, "{crs}: lat {lat}");
    }
    // The BBOX that is lat/lon for EPSG:4326 read as lon/lat is another
    // map, 20° of latitude by 10° of longitude: another point.
    let reply = get(&app, &format!("{base}&CRS=CRS:84&BBOX=50,10,60,30")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_close(point(&engine.calls().pop().unwrap().coords), (51.75, 15.0));

    // EPSG:3067 is inverse-projected. Pixel (9, 7) of a 20 × 10 map over
    // 490500–510500 E, 6640000–6660000 N is centred at (500000, 6645000):
    // 27°E, the central meridian, and 59.942431988°N (PROJ's cs2cs).
    let projected =
        format!("{base}&CRS=EPSG:3067&BBOX=490500,6640000,510500,6660000").replace("I=3", "I=9");
    let reply = get(&app, &projected).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let (lon, lat) = point(&engine.calls().pop().unwrap().coords);
    assert!((lon - 27.0).abs() < 1e-9, "EPSG:3067: lon {lon}");
    assert!((lat - 59.942_431_988).abs() < 1e-7, "EPSG:3067: lat {lat}");
}

/// The time reported, and queried, is the one GetMap keys its caches on:
/// the default for an omitted TIME, and a TIME between steps snapped to
/// the step the engine draws.
#[tokio::test]
async fn an_omitted_or_snapped_time_is_the_one_get_map_keys_on() {
    let engine = Nwp::new(Answer::Value(280.0));
    let app = router(engine.clone());
    let map = |time: &str| {
        format!(
            "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=nwp/t2m&STYLES=&CRS=CRS:84\
             &BBOX=10,50,30,60&WIDTH=20&HEIGHT=10&FORMAT=image/png{time}"
        )
    };
    // GetMap keys TIME=01:30 on 01:00: the 01:00 map is a hit.
    assert_eq!(
        get(&app, &map("&TIME=2026-10-01T01:30:00Z")).await.x_cache,
        "MISS"
    );
    assert_eq!(
        get(&app, &map("&TIME=2026-10-01T01:00:00Z")).await.x_cache,
        "HIT"
    );
    // And an omitted TIME on 02:00.
    assert_eq!(get(&app, &map("")).await.x_cache, "MISS");
    assert_eq!(
        get(&app, &map("&TIME=2026-10-01T02:00:00Z")).await.x_cache,
        "HIT"
    );

    for (time, resolved) in [("&TIME=2026-10-01T01:30:00Z", TIMES[1]), ("", TIMES[2])] {
        let reply = get(
            &app,
            &gfi(
                "nwp/t2m",
                5,
                5,
                &format!("&INFO_FORMAT=application/json{time}"),
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        assert_eq!(json(&reply)["features"][0]["properties"]["time"], resolved);
        let call = engine.calls().pop().unwrap();
        assert_eq!(call.datetime, Some((at(resolved), at(resolved))), "{time}");
    }
}

#[tokio::test]
async fn a_point_outside_the_map_is_invalid_point() {
    let engine = Nwp::new(Answer::Value(1.0));
    let app = router(engine.clone());
    for (i, j) in [(20, 0), (0, 10), (99, 99)] {
        let reply = get(&app, &gfi("nwp/t2m", i, j, "")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(exception_code(&reply.body).as_deref(), Some("InvalidPoint"));
    }
    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "").replace("I=0", "I=-1")).await;
    assert_eq!(exception_code(&reply.body).as_deref(), Some("InvalidPoint"));
    assert!(engine.calls().is_empty());
}

/// An overlay, an RGB composite and a collection without a position query
/// are not queryable.
#[tokio::test]
async fn layers_no_position_query_answers_are_layer_not_queryable() {
    let engine = Nwp::new(Answer::Value(1.0));
    let app = router(engine.clone());
    for layer in ["nwp/outline", "nwp/rgb", "plain", "plain/t2m"] {
        let reply = get(&app, &gfi(layer, 0, 0, "")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{layer}");
        assert_eq!(
            exception_code(&reply.body).as_deref(),
            Some("LayerNotQueryable"),
            "{layer}: {}",
            reply.body
        );
    }
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn other_requests_get_their_wms_exceptions() {
    let app = router(Nwp::new(Answer::Value(1.0)));
    for (query, code) in [
        (
            gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=text/xml"),
            "InvalidFormat",
        ),
        (
            gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/vnd.ogc.gml"),
            "InvalidFormat",
        ),
        // A layer of the map part that the server does not serve.
        (gfi("nope", 0, 0, ""), "LayerNotDefined"),
        // A queried layer the map does not show.
        (
            gfi("nwp/t2m", 0, 0, "").replace("QUERY_LAYERS=nwp/t2m", "QUERY_LAYERS=nwp/outline"),
            "LayerNotDefined",
        ),
        (
            gfi("nwp/t2m", 0, 0, "").replace("STYLES=&", "STYLES=bogus&"),
            "StyleNotDefined",
        ),
        (
            gfi("nwp/t2m", 0, 0, "").replace("CRS=CRS:84", "CRS=EPSG:2393"),
            "InvalidCRS",
        ),
        (
            gfi("nwp/t2m", 0, 0, "&DIM_REFERENCE_TIME=2020-01-01T00:00:00Z"),
            "InvalidDimensionValue",
        ),
        (
            gfi("nwp/t2m", 0, 0, "").replace("&I=0", ""),
            "MissingParameterValue",
        ),
    ] {
        let reply = get(&app, &query).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(
            exception_code(&reply.body).as_deref(),
            Some(code),
            "{query}: {}",
            reply.body
        );
    }
}

/// `text/plain` and `text/html` carry the same fields; HTML escapes every
/// one, and nodata is written as such.
#[tokio::test]
async fn text_formats_list_the_fields_and_html_escapes_them() {
    let engine = Arc::new(Nwp {
        answer: Answer::Value(271.25),
        unit: "<b>K</b> & co",
        content_version: 0,
        delay: Duration::ZERO,
        calls: Mutex::new(Vec::new()),
    });
    let app = router(engine);
    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type, "text/plain; charset=utf-8");
    assert_eq!(
        reply.body,
        "layer: nwp/t2m\nparameter: t2m\nvalue: 271.25\nunit: <b>K</b> & co\n\
         time: 2026-10-01T02:00:00Z\nreference_time: 2026-10-01T00:00:00Z\n\
         elevation: 850\nlon: 10.5\nlat: 59.5\n"
    );

    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=TEXT/HTML")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type, "text/html; charset=utf-8");
    assert!(reply.body.starts_with("<!DOCTYPE html>"), "{}", reply.body);
    assert!(
        reply.body.contains("<caption>nwp/t2m</caption>"),
        "{}",
        reply.body
    );
    assert!(
        reply
            .body
            .contains("<tr><th>unit</th><td>&lt;b&gt;K&lt;/b&gt; &amp; co</td></tr>"),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("<b>"), "{}", reply.body);

    let app = router(Nwp::new(Answer::Nodata));
    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "")).await;
    assert!(reply.body.contains("\nvalue: nodata\n"), "{}", reply.body);
    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/json")).await;
    assert_eq!(
        json(&reply)["features"][0]["properties"]["value"],
        serde_json::Value::Null
    );
}

/// Outside the data, or past a pole, the answer is empty, not an error:
/// a null value, and past the pole no position and no engine call.
#[tokio::test]
async fn no_data_at_the_point_is_an_empty_answer() {
    let engine = Nwp::new(Answer::NotFound);
    let app = router(engine.clone());
    let reply = get(&app, &gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/json")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let feature = &json(&reply)["features"][0];
    assert_eq!(feature["properties"]["value"], serde_json::Value::Null);
    assert_eq!(
        feature["geometry"]["coordinates"],
        serde_json::json!([10.5, 59.5])
    );
    assert_eq!(engine.calls().len(), 1);

    // Row 0 of a map reaching from 80°N to 100°N is centred at 99°N.
    let past_pole = gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/json")
        .replace("BBOX=10,50,30,60", "BBOX=10,80,30,100");
    let reply = get(&app, &past_pole).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let feature = &json(&reply)["features"][0];
    assert_eq!(feature["geometry"], serde_json::Value::Null);
    assert_eq!(feature["properties"]["value"], serde_json::Value::Null);
    assert_eq!(engine.calls().len(), 1, "no query past the pole");

    // A run evicted after the map resolved has no data either.
    let reply = get(
        &router(Nwp::new(Answer::RunEvicted)),
        &gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/json"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        json(&reply)["features"][0]["properties"]["value"],
        serde_json::Value::Null
    );
}

/// A projected map never queries a point that is not on the Earth (#1089).
/// A BBOX wholly outside its projection, which GetMap draws as the empty
/// image, has no position at any pixel, and so has a pixel centre past a
/// pole or past the eastings where Transverse Mercator's series stops being
/// exact, where its inverse names no place (#1113, #1130): the empty
/// answer, without an engine call. A point on the Earth still answers.
#[tokio::test]
async fn a_projected_pixel_off_the_earth_is_an_empty_answer() {
    let engine = Nwp::new(Answer::Value(280.0));
    let app = router(engine.clone());
    let map = |crs: &str, bbox: &str, height: u32| {
        format!(
            "SERVICE=WMS&VERSION=1.3.0&LAYERS=nwp/t2m&STYLES=&CRS={crs}&BBOX={bbox}\
             &WIDTH=20&HEIGHT={height}"
        )
    };
    let info_at = |crs: &str, bbox: &str, height: u32, (i, j): (u32, u32)| {
        format!(
            "{}&REQUEST=GetFeatureInfo&QUERY_LAYERS=nwp/t2m&I={i}&J={j}\
             &INFO_FORMAT=application/json",
            map(crs, bbox, height)
        )
    };
    let assert_empty = |reply: &Reply, context: &str| {
        assert_eq!(reply.status, StatusCode::OK, "{context}: {}", reply.body);
        let feature = &json(reply)["features"][0];
        assert_eq!(feature["geometry"], serde_json::Value::Null, "{context}");
        assert_eq!(
            feature["properties"]["value"],
            serde_json::Value::Null,
            "{context}"
        );
    };

    // Wholly outside: past TM's poles, north and south, past its area's
    // east edge, and past the LAEA disk. Every pixel is empty, the corners
    // and the centre alike.
    for (crs, bbox) in [
        ("EPSG:3067", "0,21000000,1000000,22000000"),
        ("EPSG:3067", "0,-22000000,1000000,-21000000"),
        ("EPSG:3067", "11000000,6500000,12000000,7000000"),
        ("EPSG:3035", "20000000,20000000,21000000,21000000"),
    ] {
        let reply = get(
            &app,
            &format!("{}&REQUEST=GetMap&FORMAT=image/png", map(crs, bbox, 10)),
        )
        .await;
        assert_eq!(reply.x_cache, "EMPTY", "GetMap {crs} {bbox}");
        for pixel in [(0, 0), (9, 5), (19, 9)] {
            assert_empty(
                &get(&app, &info_at(crs, bbox, 10, pixel)).await,
                &format!("{crs} {bbox} {pixel:?}"),
            );
        }
    }
    assert!(engine.calls().is_empty(), "{:?}", engine.calls());

    // A TM map from 9 000 km north of the equator, 81°N, over the pole and
    // on, 12 rows of 1 000 km: rows 0 (20 500 km) and 5 (15 500 km) are
    // centred past the pole, empty; row 11 (9 500 km) is on the Earth and
    // answers.
    let across = "0,9000000,1000000,21000000";
    for j in [0, 5] {
        assert_empty(
            &get(&app, &info_at("EPSG:3067", across, 12, (9, j))).await,
            &format!("row {j}"),
        );
    }
    // A TM map across its area's east edge on the equator, easting
    // 10 497 964.943, 20 columns of 50 km: columns 10 to 19 are centred
    // past it, where the series stops being exact (#1130): none is asked.
    let far_east = "10000000,-1000000,11000000,1000000";
    for i in 10..20 {
        assert_empty(
            &get(&app, &info_at("EPSG:3067", far_east, 4, (i, 0))).await,
            &format!("far east ({i}, 0)"),
        );
    }
    assert!(engine.calls().is_empty(), "{:?}", engine.calls());
    let reply = get(&app, &info_at("EPSG:3067", across, 12, (9, 11))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let (lon, lat) = point(&engine.calls().pop().unwrap().coords);
    assert!((80.0..90.0).contains(&lat), "lat {lat}");
    assert!(lon.is_finite(), "lon {lon}");
    assert_eq!(json(&reply)["features"][0]["properties"]["value"], 280.0);

    // A map in Finland answers at its pixel centre, as ever.
    let reply = get(
        &app,
        &info_at("EPSG:3067", "490500,6640000,510500,6660000", 10, (9, 7)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let (lon, lat) = point(&engine.calls().pop().unwrap().coords);
    assert!((lon - 27.0).abs() < 1e-9, "lon {lon}");
    assert!((lat - 59.942_431_988).abs() < 1e-7, "lat {lat}");
    assert_eq!(json(&reply)["features"][0]["properties"]["value"], 280.0);
}

#[tokio::test]
async fn an_overloaded_engine_is_503_and_a_failing_one_leaks_nothing() {
    let reply = get(&router(Nwp::new(Answer::Busy)), &gfi("nwp/t2m", 0, 0, "")).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.retry_after.as_deref(), Some("1"));

    let reply = get(&router(Nwp::new(Answer::Fails)), &gfi("nwp/t2m", 0, 0, "")).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        reply.body.contains("Internal server error"),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("secret"), "{}", reply.body);
    assert!(!reply.body.contains("10.0.0.5"), "{}", reply.body);
}

/// GetFeatureInfo's deadline is its own (#1103): `MC_FEATURE_INFO_TIMEOUT_MS`,
/// 10 s by default, not GetMap's render deadline of 3 s. Scaled down here
/// to 1 s and 100 ms: a 300 ms query, past the render deadline, answers,
/// and a 1.5 s one, past its own, is the 503. The deadlines are fixed by
/// their first read, so the test runs in a child process.
#[test]
fn get_feature_info_has_a_deadline_of_its_own() {
    const TEST: &str = "get_feature_info_has_a_deadline_of_its_own";
    const CHILD: &str = "MC_TEST_FEATURE_INFO_DEADLINE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("MC_RENDER_TIMEOUT_MS", "100")
            .env("MC_FEATURE_INFO_TIMEOUT_MS", "1000")
            .status()
            .unwrap();
        assert!(status.success(), "{TEST} failed in its child process");
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let query = gfi("nwp/t2m", 0, 0, "&INFO_FORMAT=application/json");
        let past_render = get(&router(Nwp::slow(Duration::from_millis(300))), &query).await;
        assert_eq!(past_render.status, StatusCode::OK, "{}", past_render.body);
        assert_eq!(
            json(&past_render)["features"][0]["properties"]["value"],
            280.0
        );

        let past_own = get(&router(Nwp::slow(Duration::from_millis(1500))), &query).await;
        assert_eq!(
            past_own.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            past_own.body
        );
        assert_eq!(past_own.retry_after.as_deref(), Some("1"));
    });
}

/// As GetMap: `immutable` only for an explicit TIME over content the
/// engine never revises in place. A null value is never pinned: an engine
/// may answer a transient read failure with one.
#[tokio::test]
async fn cache_control_is_get_maps() {
    const IMMUTABLE: &str = "public, max-age=86400, immutable";
    const SHORT: &str = "public, max-age=60, must-revalidate";
    let app = router(Nwp::new(Answer::Value(1.0)));
    let pinned = gfi("nwp/t2m", 0, 0, "&TIME=2026-10-01T01:00:00Z");
    assert_eq!(get(&app, &pinned).await.cache_control, IMMUTABLE);
    assert_eq!(
        get(&app, &gfi("nwp/t2m", 0, 0, "")).await.cache_control,
        SHORT
    );
    assert_eq!(
        get(&router(Nwp::new(Answer::Nodata)), &pinned)
            .await
            .cache_control,
        SHORT
    );

    let revised = Arc::new(Nwp {
        answer: Answer::Value(1.0),
        unit: "K",
        content_version: 7,
        delay: Duration::ZERO,
        calls: Mutex::new(Vec::new()),
    });
    assert_eq!(get(&router(revised), &pinned).await.cache_control, SHORT);
}

/// The layers GetCapabilities marks queryable are exactly those
/// GetFeatureInfo answers, and `<Request>` offers it, in schema order,
/// with exactly its three formats.
#[tokio::test]
async fn capabilities_advertise_get_feature_info_and_queryable_layers() {
    let reply = get(
        &router(Nwp::new(Answer::Value(1.0))),
        "SERVICE=WMS&REQUEST=GetCapabilities",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let xml = reply.body;

    let layers = queryable_layers(&xml);
    for (layer, queryable) in [
        ("nwp/t2m", "1"),
        ("nwp/outline", "0"),
        ("nwp/rgb", "0"),
        ("plain/t2m", "0"),
        ("plain/outline", "0"),
    ] {
        assert_eq!(
            layers.get(layer).map(String::as_str),
            Some(queryable),
            "{layer}: {layers:?}"
        );
    }

    let operations = request_operations(&xml);
    let names: Vec<&str> = operations.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "GetCapabilities",
            "GetMap",
            "GetFeatureInfo",
            "sld:GetLegendGraphic"
        ]
    );
    assert_eq!(
        operations[2].1,
        ["text/plain", "application/json", "text/html"]
    );
    assert_eq!(api_wms::params::INFO_FORMATS, &operations[2].1[..]);
}

/// Every named `<Layer>`'s `queryable` attribute, by name.
fn queryable_layers(xml: &str) -> HashMap<String, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    // One entry per open <Layer>: its queryable attribute.
    let mut stack: Vec<Option<String>> = Vec::new();
    let mut in_name = false;
    let mut layers = HashMap::new();
    loop {
        match reader.read_event().unwrap() {
            Event::Start(e) if e.local_name().as_ref() == "Layer" => stack.push(
                e.try_get_attribute("queryable")
                    .unwrap()
                    .map(|a| a.value.into_owned()),
            ),
            Event::End(e) if e.local_name().as_ref() == "Layer" => {
                stack.pop();
            }
            Event::Start(e) if e.local_name().as_ref() == "Name" => in_name = true,
            Event::End(e) if e.local_name().as_ref() == "Name" => in_name = false,
            Event::Text(text) if in_name && !stack.is_empty() => {
                let name = String::from(&*text);
                if let Some(Some(queryable)) = stack.last() {
                    layers.insert(name, queryable.clone());
                }
            }
            Event::Eof => return layers,
            _ => {}
        }
    }
}

/// `<Request>`'s operations, in order, each with its `<Format>`s.
fn request_operations(xml: &str) -> Vec<(String, Vec<String>)> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut in_request, mut depth, mut in_format) = (false, 0, false);
    let mut operations: Vec<(String, Vec<String>)> = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            Event::Start(e) => {
                let name = e.name().as_ref().to_string();
                if name == "Request" {
                    in_request = true;
                } else if in_request {
                    depth += 1;
                    if depth == 1 {
                        operations.push((name, Vec::new()));
                    } else if depth == 2 && name == "Format" {
                        in_format = true;
                    }
                }
            }
            Event::End(e) => {
                if e.name().as_ref() == "Request" {
                    return operations;
                }
                if in_request {
                    depth -= 1;
                    in_format = false;
                }
            }
            Event::Text(text) if in_format => {
                let format = String::from(&*text);
                operations.last_mut().unwrap().1.push(format);
            }
            Event::Eof => return operations,
            _ => {}
        }
    }
}

/// An engine answering area queries only, as a nowcast's motion field and
/// a PostGIS events shape do.
struct AreaOnly;

impl EdrEngine for AreaOnly {
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
        Vec::new()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        None
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["area".into(), "radius".into()]
    }
}

/// The server registers a collection's engine for GetFeatureInfo only when
/// it answers position queries: an area-only engine's layers stay
/// `queryable="0"`.
#[test]
fn only_a_position_engine_answers_get_feature_info() {
    assert!(api_wms::feature_info::answers_position(
        Nwp::new(Answer::Nodata).as_ref()
    ));
    assert!(!api_wms::feature_info::answers_position(&AreaOnly));
}

// ---------------------------------------------------------------------------
// Several QUERY_LAYERS (#1181)
// ---------------------------------------------------------------------------

/// One answer per query layer, in `QUERY_LAYERS` order, each at its own
/// layer's time, run and level: a text block, a feature, an HTML table each.
#[tokio::test]
async fn several_query_layers_answer_in_query_layers_order() {
    let (nwp, nwp2) = (
        Nwp::new(Answer::Value(271.5)),
        Nwp::new(Answer::Value(300.0)),
    );
    let app = router_with(nwp.clone(), nwp2.clone());
    let query = |format: &str| {
        gfi_layers(
            "nwp/t2m,plain/t2m,nwp2/t2m",
            "nwp2/t2m,nwp/t2m",
            &format!("&ELEVATION=500&INFO_FORMAT={format}"),
        )
    };

    let reply = get(&app, &query("application/json")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let features = json(&reply)["features"].clone();
    let answers: Vec<_> = features
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["properties"]["layer"].as_str().unwrap().to_string(),
                f["properties"]["value"].as_f64().unwrap(),
                f["properties"]["elevation"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        answers,
        [
            ("nwp2/t2m".to_string(), 300.0, 500.0),
            ("nwp/t2m".to_string(), 271.5, 500.0)
        ]
    );
    for engine in [&nwp, &nwp2] {
        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].z.as_deref(), Some(&[500.0][..]));
        assert_eq!(calls[0].coords, nwp.calls()[0].coords, "one point");
    }

    let reply = get(&app, &query("text/plain")).await;
    assert_eq!(reply.content_type, "text/plain; charset=utf-8");
    let blocks: Vec<&str> = reply.body.split("\n\n").collect();
    assert_eq!(blocks.len(), 2, "{}", reply.body);
    assert!(blocks[0].starts_with("layer: nwp2/t2m\n"), "{}", reply.body);
    assert!(blocks[0].contains("\nvalue: 300\n"), "{}", reply.body);
    assert!(blocks[1].starts_with("layer: nwp/t2m\n"), "{}", reply.body);
    assert!(blocks[1].contains("\nvalue: 271.5\n"), "{}", reply.body);

    let reply = get(&app, &query("text/html")).await;
    assert_eq!(reply.content_type, "text/html; charset=utf-8");
    let second = reply.body.find("<caption>nwp/t2m</caption>");
    let first = reply.body.find("<caption>nwp2/t2m</caption>");
    assert!(first.is_some() && first < second, "{}", reply.body);
    assert_eq!(reply.body.matches("<table>").count(), 2, "{}", reply.body);
}

/// Every query layer must be a layer of the map and queryable, checked
/// before any engine is asked.
#[tokio::test]
async fn every_query_layer_is_shown_and_queryable() {
    let nwp = Nwp::new(Answer::Value(1.0));
    let app = router(nwp.clone());
    for (layers, query_layers, code) in [
        ("nwp/t2m,plain/t2m", "nwp/t2m,nwp2/t2m", "LayerNotDefined"),
        (
            "nwp/t2m,plain/t2m",
            "nwp/t2m,plain/t2m",
            "LayerNotQueryable",
        ),
        ("nwp/t2m,nwp/rgb", "nwp/t2m,nwp/rgb", "LayerNotQueryable"),
    ] {
        let reply = get(&app, &gfi_layers(layers, query_layers, "")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query_layers}");
        assert_eq!(
            exception_code(&reply.body).as_deref(),
            Some(code),
            "{query_layers}: {}",
            reply.body
        );
    }
    assert!(nwp.calls().is_empty());
}

/// The request shapes of ets-wms13's `getfeatureinfo:two-query_layers`,
/// `three-query_layers` and `less-query_layers`: one layer listed two or
/// three times, all or some of them queried. A layer queried twice is
/// answered once.
#[tokio::test]
async fn ets_multi_layer_get_feature_infos_answer() {
    let nwp = Nwp::new(Answer::Value(271.5));
    let app = router(nwp.clone());
    for (layers, query_layers) in [
        ("nwp/t2m,nwp/t2m", "nwp/t2m,nwp/t2m"),
        ("nwp/t2m,nwp/t2m,nwp/t2m", "nwp/t2m,nwp/t2m,nwp/t2m"),
        ("nwp/t2m,nwp/t2m,nwp/t2m", "nwp/t2m,nwp/t2m"),
    ] {
        let reply = get(
            &app,
            &gfi_layers(
                layers,
                query_layers,
                "&INFO_FORMAT=text/plain&FORMAT=image/png",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{layers}: {}", reply.body);
        assert_eq!(reply.content_type, "text/plain; charset=utf-8");
        assert_eq!(reply.body.matches("layer: ").count(), 1, "{}", reply.body);
    }
}

/// A query layer that fails fails the request, as one layer does, and the
/// layers after it are not queried; one with no data at the point is an
/// empty answer among the others. `immutable` needs every value.
#[tokio::test]
async fn a_failing_query_layer_fails_the_request() {
    const IMMUTABLE: &str = "public, max-age=86400, immutable";
    const SHORT: &str = "public, max-age=60, must-revalidate";
    let both = |extra: &str| gfi_layers("nwp/t2m,nwp2/t2m", "nwp2/t2m,nwp/t2m", extra);

    let nwp = Nwp::new(Answer::Value(1.0));
    let app = router_with(nwp.clone(), Nwp::new(Answer::Fails));
    let reply = get(&app, &both("")).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!reply.body.contains("secret"), "{}", reply.body);
    assert!(nwp.calls().is_empty(), "the layer after it is not queried");

    let app = router_with(Nwp::new(Answer::Busy), Nwp::new(Answer::Value(1.0)));
    let reply = get(&app, &both("")).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);

    let app = router_with(Nwp::new(Answer::Value(1.0)), Nwp::new(Answer::NotFound));
    let pinned = both("&INFO_FORMAT=application/json&TIME=2026-10-01T01:00:00Z");
    let reply = get(&app, &pinned).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let values: Vec<_> = json(&reply)["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["properties"]["value"].clone())
        .collect();
    assert_eq!(values, [serde_json::Value::Null, serde_json::json!(1.0)]);
    assert_eq!(reply.cache_control, SHORT, "a null value is never pinned");

    let app = router_with(Nwp::new(Answer::Value(1.0)), Nwp::new(Answer::Value(2.0)));
    assert_eq!(get(&app, &pinned).await.cache_control, IMMUTABLE);
}

/// GetFeatureInfo's deadline is the whole request's: the query layers are
/// read one after another under it. With 1 s, one 600 ms query answers and
/// two are the 503. The deadline is fixed by its first read, so the test
/// runs in a child process.
#[test]
fn several_query_layers_share_one_deadline() {
    const TEST: &str = "several_query_layers_share_one_deadline";
    const CHILD: &str = "MC_TEST_FEATURE_INFO_LAYERS_DEADLINE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("MC_FEATURE_INFO_TIMEOUT_MS", "1000")
            .status()
            .unwrap();
        assert!(status.success(), "{TEST} failed in its child process");
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let slow = || Nwp::slow(Duration::from_millis(600));
        let app = router_with(slow(), slow());
        let one = get(&app, &gfi_layers("nwp/t2m,nwp2/t2m", "nwp/t2m", "")).await;
        assert_eq!(one.status, StatusCode::OK, "{}", one.body);
        let two = get(
            &app,
            &gfi_layers("nwp/t2m,nwp2/t2m", "nwp/t2m,nwp2/t2m", ""),
        )
        .await;
        assert_eq!(two.status, StatusCode::SERVICE_UNAVAILABLE, "{}", two.body);
    });
}
