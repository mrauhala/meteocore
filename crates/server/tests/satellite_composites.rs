//! RGB composite layers (#819) of engine-satellite through WMS, Maps and
//! Tiles, end to end on the cropped GOES-19 fixtures: the composite child
//! layer and its time axis in GetCapabilities, the rendered pixels against
//! `ds_render::render_composite_tiles` over the engine's own band tiles, the
//! channel-list legend and the `parameter_names` entry.
//!
//! The IR and cloud top crops cover different parts of the disk, so a
//! composite reading both has no pixel with both bands: it renders empty.
//! `ir_grey`, which reads IR only, draws the IR crop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, SatelliteConfig};
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_render::{
    BuiltinColormap, CompositeSpec, ImageFormat, LutColorMap, RenderedCache, StyleInfo,
};
use engine_satellite::SatelliteEngine;

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
const ACHT: &str = "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";
const ID: &str = "goes19-fd";
/// Around the IR crop, which spans -25.4..3.7°E, 37.1..46.6°N.
const IR_BBOX: [f64; 4] = [-25.0, 37.5, 3.0, 46.5];

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// `fixture` republished as the scan starting at `hhmm` on 2026-09-25.
fn publish(dir: &Path, fixture: &str, hhmm: &str) {
    let nested = dir.join("ABI-L2/2026/268/19");
    std::fs::create_dir_all(&nested).unwrap();
    let from: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/goes19-abi")
        .join(fixture);
    let name = fixture.replace("_s20262681900199_", &format!("_s2026268{hhmm}199_"));
    std::fs::copy(from, nested.join(name)).unwrap();
}

fn satellite_config(dir: &Path) -> SatelliteConfig {
    toml::from_str(&format!(
        r#"
        data_path = '{}'

        [[products]]
        parameter = "ir_10_3"
        title = "IR 10.3 µm brightness temperature"
        unit = "K"
        product = "ABI-L2-CMIPF"
        band = 13
        variable = "CMI"

        [[products]]
        parameter = "cloud_top_temperature"
        title = "Cloud top temperature"
        unit = "K"
        product = "ABI-L2-ACHTF"
        variable = "TEMP"

        [[composites]]
        name = "ir_cloud"
        title = "IR and cloud top"
        red = {{ parameter = "ir_10_3", minus = "cloud_top_temperature", min = -10, max = 40 }}
        green = {{ parameter = "cloud_top_temperature", min = 330.0, max = 180.0 }}
        blue = {{ parameter = "ir_10_3", min = 330.0, max = 180.0 }}

        [[composites]]
        name = "ir_grey"
        title = "IR grey"
        red = {{ parameter = "ir_10_3", min = 330.0, max = 180.0 }}
        green = {{ parameter = "ir_10_3", min = 330.0, max = 200.0 }}
        blue = {{ parameter = "ir_10_3", min = 330.0, max = 220.0, gamma = 1.5 }}
        "#,
        dir.display()
    ))
    .unwrap()
}

/// IR at 19:00 and 19:10, cloud top at 19:00 only: `ir_cloud` has 19:00,
/// `ir_grey` both IR scans.
fn engine() -> (Arc<SatelliteEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    publish(dir.path(), C13, "1900");
    publish(dir.path(), C13, "1910");
    publish(dir.path(), ACHT, "1900");
    let engine = SatelliteEngine::new(ID, &satellite_config(dir.path())).unwrap();
    engine.poll_once();
    (Arc::new(engine), dir)
}

fn collection(dir: &Path, apis: &[&str]) -> CollectionConfig {
    CollectionConfig {
        id: ID.into(),
        title: "GOES-19 full disk".into(),
        description: "Cropped GOES-19 fixtures".into(),
        data_path: None,
        apis: apis.iter().map(|a| a.to_string()).collect(),
        engine_type: "satellite".into(),
        keywords: Vec::new(),
        license: None,
        geotiff: None,
        querydata: None,
        wms: None,
        grib: None,
        zarr: None,
        odim: None,
        cap: None,
        postgis: None,
        nowcast: None,
        bufr: None,
        satellite: Some(satellite_config(dir)),
        preview: None,
        derive_wind: None,
    }
}

/// The collection's one colormap style, for the band layers.
fn styles() -> HashMap<String, HashMap<String, StyleInfo>> {
    let style = StyleInfo {
        name: "default".into(),
        title: "Default".into(),
        palette: ds_render::builtin_palette_arc("grayscale").unwrap(),
        colormap: Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::Grayscale,
            180.0,
            330.0,
        )),
        min: 180.0,
        max: 330.0,
        parameter: None,
    };
    HashMap::from([(
        ID.to_string(),
        HashMap::from([("default".to_string(), style)]),
    )])
}

fn wms(engine: Arc<SatelliteEngine>, dir: &Path) -> axum::Router {
    api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, &["wms"]))]),
        styles: styles(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(64)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

fn maps(engine: Arc<SatelliteEngine>, dir: &Path) -> axum::Router {
    api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, &["maps"]))]),
        styles: styles(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    })))
}

fn tiles(engine: Arc<SatelliteEngine>, dir: &Path) -> axum::Router {
    api_tiles::router(Arc::new(ArcSwap::from_pointee(api_tiles::TilesState {
        map_engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, &["tiles"]))]),
        styles: styles(),
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(ds_mvt::VectorTileCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

struct Reply {
    status: StatusCode,
    x_cache: String,
    body: Bytes,
}

async fn get(app: &axum::Router, uri: &str) -> Reply {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let x_cache = resp
        .headers()
        .get("x-cache")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        x_cache,
        body,
    }
}

fn json(reply: &Reply) -> Value {
    serde_json::from_slice(&reply.body).unwrap()
}

/// `name`'s composite as the engine serves it.
fn spec(engine: &SatelliteEngine, name: &str) -> CompositeSpec {
    let composites = engine.composites();
    CompositeSpec::from(composites.iter().find(|c| c.name == name).unwrap())
}

/// `render_composite_tiles` over the engine's band tiles at `time`: what a
/// 64 × 64 CRS:84 render of the IR crop must return.
fn expected_png(engine: &SatelliteEngine, name: &str, time: &str) -> Vec<u8> {
    let spec = spec(engine, name);
    let bands: Vec<&str> = spec.parameters.iter().map(String::as_str).collect();
    let tiles = engine
        .get_raster_tiles(
            IR_BBOX,
            64,
            64,
            Some(at(time)),
            &OutputCrs::Wgs84,
            &bands,
            None,
            None,
        )
        .unwrap();
    ds_render::render_composite_tiles(&tiles, &spec, ImageFormat::Png, None)
        .unwrap()
        .expect("the IR crop has data")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wms_serves_composite_layers() {
    let (engine, dir) = engine();
    let app = wms(engine.clone(), dir.path());

    let caps = get(&app, "/?SERVICE=WMS&REQUEST=GetCapabilities").await;
    assert_eq!(caps.status, StatusCode::OK);
    let caps = String::from_utf8(caps.body.to_vec()).unwrap();
    let layer = |name: &str| {
        let start = caps.find(&format!("<Name>{name}</Name>")).unwrap();
        let end = caps[start..].find("</Layer>").unwrap();
        caps[start..start + end].to_string()
    };
    let (t0, t1) = (
        at("2026-09-25T19:00:00Z").to_rfc3339(),
        at("2026-09-25T19:10:00Z").to_rfc3339(),
    );
    let ir_cloud = layer("goes19-fd/ir_cloud");
    assert!(
        ir_cloud.contains(&format!("default=\"{t0}\" nearestValue=\"1\">{t0}<")),
        "{ir_cloud}"
    );
    let ir_grey = layer("goes19-fd/ir_grey");
    assert!(
        ir_grey.contains(&format!("default=\"{t1}\" nearestValue=\"1\">{t0},{t1}<")),
        "{ir_grey}"
    );
    assert!(ir_grey.contains("<Name>default</Name>"), "{ir_grey}");

    let map = |layer: &str, crs: &str, bbox: &str| {
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={ID}/{layer}&STYLES=\
             &FORMAT=image/png&CRS={crs}&BBOX={bbox}&WIDTH=64&HEIGHT=64"
        )
    };
    let [w, s, e, n] = IR_BBOX;
    let crs84 = format!("{w},{s},{e},{n}");

    // The latest IR scan, composed exactly as `ds_render` composes the
    // engine's own band tiles.
    let reply = get(&app, &map("ir_grey", "CRS:84", &crs84)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.x_cache, "MISS");
    assert_eq!(
        reply.body,
        expected_png(&engine, "ir_grey", "2026-09-25T19:10:00Z")
    );

    // Meta-tiled Web Mercator renders too.
    let x = |lon: f64| ds_core::web_mercator::lon_to_x(lon);
    let y = |lat: f64| ds_core::web_mercator::lat_to_y(lat);
    let mercator = format!("{},{},{},{}", x(w), y(s), x(e), y(n));
    let reply = get(&app, &map("ir_grey", "EPSG:3857", &mercator)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.x_cache, "MISS");

    // Where only IR has data, the two-band composite is empty, and no
    // error: it renders its shared 19:00 scan.
    let reply = get(&app, &map("ir_cloud", "CRS:84", &crs84)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.x_cache, "EMPTY");

    let legend = get(
        &app,
        &format!(
            "/?SERVICE=WMS&REQUEST=GetLegendGraphic&LAYER={ID}/ir_cloud&FORMAT=application/json"
        ),
    )
    .await;
    assert_eq!(legend.status, StatusCode::OK);
    let legend = json(&legend);
    assert_eq!(legend["parameter"], "ir_cloud");
    assert_eq!(
        legend["channels"][0]["label"],
        "ir_10_3 - cloud_top_temperature"
    );
    assert_eq!(legend["channels"][0]["unit"], "K");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maps_and_tiles_serve_composites_by_parameter_name() {
    let (engine, dir) = engine();

    let app = maps(engine.clone(), dir.path());
    let [w, s, e, n] = IR_BBOX;
    let reply = get(
        &app,
        &format!(
            "/collections/{ID}/map?bbox={w},{s},{e},{n}&width=64&height=64&parameter-name=ir_grey"
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        expected_png(&engine, "ir_grey", "2026-09-25T19:10:00Z")
    );

    let collection = json(&get(&app, &format!("/collections/{ID}")).await);
    let names = collection["parameter_names"].as_object().unwrap();
    assert_eq!(
        names.keys().map(String::as_str).collect::<Vec<_>>(),
        ["cloud_top_temperature", "ir_10_3", "ir_cloud", "ir_grey"]
    );
    assert!(names["ir_cloud"].get("unit").is_none());
    let t0 = at("2026-09-25T19:00:00Z").to_rfc3339();
    assert_eq!(
        names["ir_cloud"]["extent"]["temporal"]["interval"],
        serde_json::json!([[t0, t0]])
    );

    let legend = get(
        &app,
        &format!("/collections/{ID}/styles/default/legend?parameter-name=ir_grey&f=png"),
    )
    .await;
    assert_eq!(legend.status, StatusCode::OK);
    assert!(legend.body.starts_with(b"\x89PNG"));

    // A Web Mercator tile over the IR crop.
    let app = tiles(engine.clone(), dir.path());
    let reply = get(
        &app,
        &format!("/collections/{ID}/tiles/WebMercatorQuad/2/1/1?parameter-name=ir_grey"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.x_cache, "MISS");
    let reply = get(
        &app,
        &format!("/collections/{ID}/tiles/WebMercatorQuad/2/1/1?parameter-name=ir_cloud"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.x_cache, "EMPTY");
}
