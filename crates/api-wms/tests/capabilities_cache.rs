//! The GetCapabilities document cache (#1482), through the router: one
//! build per base URL within the window, the document's bytes unchanged,
//! links that never cross hosts, a reload shown at once and new time steps
//! within the window, and the document gzipped once, with its ETag.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_wms::{AppState, WmsState};
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

/// A radar whose time steps a test can extend, as a poll does, and which
/// counts its metadata reads: building the document reads them, serving a
/// cached one does not.
struct Radar {
    times: RwLock<Vec<DateTime<Utc>>>,
    reads: AtomicUsize,
}

impl Radar {
    fn new(times: &[&str]) -> Arc<Self> {
        Arc::new(Radar {
            times: RwLock::new(times.iter().map(|t| t.parse().unwrap()).collect()),
            reads: AtomicUsize::new(0),
        })
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl MapEngine for Radar {
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
            values: vec![None; (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        self.reads.fetch_add(1, Ordering::SeqCst);
        RasterInfo {
            native_crs: "EPSG:4326".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: self.times.read().unwrap().clone(),
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

const T0: &str = "2026-11-21T10:00:00Z";
const T1: &str = "2026-11-21T10:05:00Z";
const T2: &str = "2026-11-21T10:10:00Z";

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "title": id,
        "description": "Capabilities cache fixture",
        "apis": ["wms"],
    }))
    .unwrap()
}

fn style() -> HashMap<String, StyleInfo> {
    HashMap::from([(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Viridis,
                0.0,
                1.0,
            )),
            min: 0.0,
            max: 1.0,
            parameter: None,
            contour: None,
        },
    )])
}

/// A state serving `engines` under their ids, with links below `base_url`.
fn wms_state(engines: &[(&str, Arc<Radar>)], base_url: &str, trust_proxy: bool) -> WmsState {
    WmsState {
        engines: engines
            .iter()
            .map(|(id, engine)| (id.to_string(), engine.clone() as Arc<dyn MapEngine>))
            .collect(),
        point_engines: HashMap::new(),
        collections: engines
            .iter()
            .map(|(id, _)| (id.to_string(), collection(id)))
            .collect(),
        styles: engines
            .iter()
            .map(|(id, _)| (id.to_string(), style()))
            .collect(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: base_url.to_string(),
        trust_proxy_headers: trust_proxy,
    }
}

fn app_state(state: WmsState) -> AppState {
    Arc::new(ArcSwap::from_pointee(state))
}

const BASE: &str = "http://localhost:8000";
const CAPABILITIES: &str = "/?SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.3.0";

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }

    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

async fn get(app: &axum::Router, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder().uri(CAPABILITIES);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: body.to_vec(),
    }
}

/// The document as the builder writes it for the router's current state.
fn built(state: &AppState, base_url: &str) -> Vec<u8> {
    let state = state.load();
    api_wms::capabilities::get_capabilities_xml(
        &state.engines,
        &state.collections,
        &state.styles,
        &state.point_engines,
        base_url,
    )
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// Requests inside the window are served the first request's document
/// without building it again: the engine's metadata is read for the first
/// only. The bytes are the builder's, unchanged.
#[tokio::test]
async fn repeated_requests_within_the_window_build_once() {
    let radar = Radar::new(&[T0, T1]);
    let state = app_state(wms_state(&[("radar", radar.clone())], BASE, false));
    let app = api_wms::router(state.clone());

    let first = get(&app, &[]).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.header(header::CONTENT_TYPE), Some("text/xml"));
    assert_eq!(
        first.header(header::X_CONTENT_TYPE_OPTIONS),
        Some("nosniff")
    );
    let reads = radar.reads();
    assert!(reads > 0, "the first request builds");

    for _ in 0..5 {
        let again = get(&app, &[]).await;
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(again.body, first.body);
        assert_eq!(again.header(header::ETAG), first.header(header::ETAG));
    }
    assert_eq!(radar.reads(), reads, "no request built again");
    assert_eq!(first.body, built(&state, BASE), "the builder's bytes");
}

/// With `trust_proxy_headers`, each forwarded host gets a document with its
/// own links, built once, and never another host's.
#[tokio::test]
async fn documents_are_keyed_by_the_requests_base_url() {
    let radar = Radar::new(&[T0]);
    let state = app_state(wms_state(&[("radar", radar.clone())], BASE, true));
    let app = api_wms::router(state.clone());
    let host = |name: &'static str| [("x-forwarded-proto", "https"), ("x-forwarded-host", name)];

    let a = get(&app, &host("a.example.org")).await;
    let one = radar.reads();
    let b = get(&app, &host("b.example.org")).await;
    let two = radar.reads();
    assert!(two > one && one > 0, "each host's first request builds");
    assert!(a.text().contains("https://a.example.org/wms"));
    assert!(b.text().contains("https://b.example.org/wms"));
    assert!(!a.text().contains("b.example.org"));
    assert!(!b.text().contains("a.example.org"));
    assert!(!a.text().contains(BASE) && !b.text().contains(BASE));

    // Alternating hosts inside the window builds neither again.
    for _ in 0..3 {
        assert_eq!(get(&app, &host("a.example.org")).await.body, a.body);
        assert_eq!(get(&app, &host("b.example.org")).await.body, b.body);
    }
    assert_eq!(radar.reads(), two);
    assert_eq!(a.body, built(&state, "https://a.example.org"));
    assert_eq!(b.body, built(&state, "https://b.example.org"));
}

/// A reload's state is served at once, inside the window: its new
/// collection is listed by the next request.
#[tokio::test]
async fn a_reload_shows_at_once() {
    let radar = Radar::new(&[T0]);
    let state = app_state(wms_state(&[("radar", radar.clone())], BASE, false));
    let app = api_wms::router(state.clone());
    let before = get(&app, &[]).await.text();
    assert!(!before.contains("<Name>added</Name>"));

    state.store(Arc::new(wms_state(
        &[("radar", radar), ("added", Radar::new(&[T0]))],
        BASE,
        false,
    )));
    let after = get(&app, &[]).await;
    assert!(
        after.text().contains("<Name>added</Name>"),
        "{}",
        after.text()
    );
    assert_eq!(after.body, built(&state, BASE));
}

/// A time step an engine adds without a reload shows once the window,
/// `CAPABILITIES_TTL`, ends, and not before: every request inside it gets
/// the same bytes. On tokio's paused clock, to the millisecond.
#[tokio::test(start_paused = true)]
async fn new_time_steps_show_within_the_window() {
    let radar = Radar::new(&[T0, T1]);
    let state = app_state(wms_state(&[("radar", radar.clone())], BASE, false));
    let app = api_wms::router(state.clone());
    let first = get(&app, &[]).await;
    assert!(first
        .text()
        .contains(&format!("default=\"{}\"", rfc3339(T1))));

    radar.times.write().unwrap().push(T2.parse().unwrap());
    assert_eq!(get(&app, &[]).await.body, first.body, "as the window opens");
    tokio::time::advance(api_wms::CAPABILITIES_TTL - Duration::from_millis(1)).await;
    assert_eq!(get(&app, &[]).await.body, first.body, "as the window ends");

    tokio::time::advance(Duration::from_millis(1)).await;
    let later = get(&app, &[]).await;
    assert!(later
        .text()
        .contains(&format!("default=\"{}\"", rfc3339(T2))));
    assert_eq!(later.body, built(&state, BASE));

    assert!(api_wms::CAPABILITIES_TTL <= Duration::from_secs(10));
}

fn rfc3339(t: &str) -> String {
    t.parse::<DateTime<Utc>>().unwrap().to_rfc3339()
}

/// A client taking gzip gets the document gzipped, once per build, with
/// `Content-Encoding: gzip`; one that does not gets it as is. Both carry
/// `Vary: Accept-Encoding` and one ETag.
#[tokio::test]
async fn the_document_is_gzipped_for_clients_that_take_it() {
    let radar = Radar::new(&[T0]);
    let state = app_state(wms_state(&[("radar", radar)], BASE, false));
    let app = api_wms::router(state.clone());
    let plain = get(&app, &[]).await;
    assert_eq!(plain.header(header::CONTENT_ENCODING), None);
    assert_eq!(plain.header(header::VARY), Some("accept-encoding"));

    for accept in ["gzip", "gzip, deflate, br", "*"] {
        let zipped = get(&app, &[("accept-encoding", accept)]).await;
        assert_eq!(zipped.status, StatusCode::OK, "{accept}");
        assert_eq!(
            zipped.header(header::CONTENT_ENCODING),
            Some("gzip"),
            "{accept}"
        );
        assert_eq!(zipped.header(header::VARY), Some("accept-encoding"));
        assert_eq!(zipped.header(header::CONTENT_TYPE), Some("text/xml"));
        assert_eq!(zipped.header(header::ETAG), plain.header(header::ETAG));
        assert_eq!(gunzip(&zipped.body), plain.body, "{accept}");
        assert!(zipped.body.len() < plain.body.len());
    }
    for accept in ["identity", "gzip;q=0", "br"] {
        let reply = get(&app, &[("accept-encoding", accept)]).await;
        assert_eq!(reply.header(header::CONTENT_ENCODING), None, "{accept}");
        assert_eq!(reply.body, plain.body, "{accept}");
    }
}

/// The ETag hashes the document: a matching `If-None-Match` is 304 with no
/// body, and a document that changes, after a reload, has another ETag.
#[tokio::test]
async fn if_none_match_is_answered_with_304_until_the_document_changes() {
    let radar = Radar::new(&[T0]);
    let state = app_state(wms_state(&[("radar", radar.clone())], BASE, false));
    let app = api_wms::router(state.clone());
    let first = get(&app, &[]).await;
    let etag = first.header(header::ETAG).unwrap().to_string();
    assert!(etag.starts_with("W/\""), "{etag}");
    assert_eq!(etag, ds_core::http_cache::weak_etag_of(&first.body));

    for accept in [None, Some("gzip")] {
        let mut headers = vec![("if-none-match", etag.as_str())];
        headers.extend(accept.map(|a| ("accept-encoding", a)));
        let not_modified = get(&app, &headers).await;
        assert_eq!(not_modified.status, StatusCode::NOT_MODIFIED, "{accept:?}");
        assert!(not_modified.body.is_empty());
        assert_eq!(not_modified.header(header::ETAG), Some(etag.as_str()));
        assert_eq!(not_modified.header(header::VARY), Some("accept-encoding"));
    }
    // The strong form a client may send matches too (weak comparison).
    let strong = etag.trim_start_matches("W/").to_string();
    assert_eq!(
        get(&app, &[("if-none-match", &strong)]).await.status,
        StatusCode::NOT_MODIFIED
    );
    assert_eq!(
        get(&app, &[("if-none-match", "\"other\"")]).await.status,
        StatusCode::OK
    );

    state.store(Arc::new(wms_state(
        &[("radar", radar), ("added", Radar::new(&[T0]))],
        BASE,
        false,
    )));
    let changed = get(&app, &[("if-none-match", etag.as_str())]).await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_ne!(changed.header(header::ETAG), Some(etag.as_str()));
}
