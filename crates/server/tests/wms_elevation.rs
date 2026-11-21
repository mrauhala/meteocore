//! WMS `ELEVATION` between levels on real engines (#1477). Capabilities
//! declare every elevation dimension `nearestValue="1"`, so a GetMap draws
//! the nearest level the engine can draw (WMS 1.3.0 C.4.3), keyed on that
//! level, and GetFeatureInfo reads and reports it:
//!
//! - NetCDF: `testdata/meps-netcdf-pl`, MEPS on 1000, 850, 500 and
//!   300 hPa, wrapped in `DerivedWind` as the server registers it, so the
//!   wind speed is derived from both components at the one level drawn.
//! - GRIB: `testdata/grib-local`, ECMWF specific humidity at 150 hPa, the
//!   one level of its one run, served as a pressure-level collection. The
//!   per-run selection over several runs is engine-grib's
//!   `a_map_draws_the_selected_runs_nearest_level`.
//! - ODIM PVOL: `testdata/pvol-cold-batch.h5`, a volume of two sweeps,
//!   0.5° and 1.0°, whose DBZH differs between them; the `ELEVATION`s
//!   drawing one sweep share one rendered entry.
//!
//! The mock-engine twin, counting renders and meta-tile reuse, the layer
//! lists and `InvalidDimensionValue`, is `crates/api-wms/tests/elevation.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, GribConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::MapEngine;
use ds_core::wind::DerivedWind;
use ds_render::{ColorStop, LutColorMap, RenderedCache, StyleInfo};
use engine_grib::GribEngine;
use engine_netcdf::NetcdfEngine;

/// A current-thread runtime to serve requests on, the strictest: a
/// storage bridge that blocks on it panics (root CLAUDE.md Critical Rule
/// 7). Engines are built outside it, as the server builds them, since their
/// scans bridge storage.
fn serve(requests: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(requests);
}

fn testdata(dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(dir)
}

fn collection(id: &str, engine_type: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "engine_type": engine_type, "apis": ["wms"],
    }))
    .unwrap()
}

/// One style for every layer: a grey ramp from black at `min` to white at
/// `max`, so images of different levels differ.
fn style(min: f64, max: f64) -> HashMap<String, StyleInfo> {
    let stops = [
        ColorStop {
            value: min,
            color: [0, 0, 0, 255],
        },
        ColorStop {
            value: max,
            color: [255, 255, 255, 255],
        },
    ];
    HashMap::from([(
        "default".to_string(),
        StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: Arc::new(LutColorMap::from_stops(&stops, min, max)),
            min,
            max,
            parameter: None,
            contour: None,
        },
    )])
}

/// A WMS of the one collection `id`, whose engine answers GetFeatureInfo
/// too, as the server wires it, styling `layers` over `range`.
fn wms<E>(
    id: &str,
    engine_type: &str,
    engine: Arc<E>,
    layers: &[&str],
    (min, max): (f64, f64),
) -> axum::Router
where
    E: MapEngine + EdrEngine + 'static,
{
    let mut styles = HashMap::from([(id.to_string(), style(min, max))]);
    for layer in layers {
        styles.insert(format!("{id}/{layer}"), style(min, max));
    }
    api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(id.to_string(), engine.clone() as Arc<dyn MapEngine>)]),
        point_engines: HashMap::from([(id.to_string(), engine as Arc<dyn EdrEngine>)]),
        collections: HashMap::from([(id.to_string(), collection(id, engine_type))]),
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
    x_cache: String,
    etag: String,
    body: String,
}

async fn get(app: &axum::Router, query: &str) -> Reply {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/?SERVICE=WMS&VERSION=1.3.0&{query}"))
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
            .unwrap_or_default()
    };
    let (status, x_cache, etag) = (response.status(), header("x-cache"), header("etag"));
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        x_cache,
        etag,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// GetMaps of `layer` at each `(ELEVATION, x-cache)` in turn: each draws
/// an image, never the error tile, with the `x-cache` given. Their ETags.
async fn get_maps(
    app: &axum::Router,
    layer: &str,
    view: &str,
    elevations: &[(&str, &str)],
) -> Vec<String> {
    let mut etags = Vec::new();
    for (z, x_cache) in elevations {
        let reply = get(
            app,
            &format!("REQUEST=GetMap&LAYERS={layer}&STYLES=&{view}&FORMAT=image/png&ELEVATION={z}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{layer} {z}: {}", reply.body);
        assert_eq!(reply.x_cache, *x_cache, "{layer} at {z}");
        etags.push(reply.etag);
    }
    etags
}

/// The properties of GetFeatureInfo's first feature, as JSON, at the
/// centre pixel of a 3 × 3 CRS:84 map of `layer` over `bbox`.
async fn feature_info(app: &axum::Router, layer: &str, bbox: &str, extra: &str) -> Value {
    let reply = get(
        app,
        &format!(
            "REQUEST=GetFeatureInfo&LAYERS={layer}&QUERY_LAYERS={layer}&STYLES=&CRS=CRS:84\
             &BBOX={bbox}&WIDTH=3&HEIGHT=3&I=1&J=1&INFO_FORMAT=application/json{extra}"
        ),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{layer}{extra}: {}",
        reply.body
    );
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    body["features"][0]["properties"].clone()
}

/// The issue's own map of `meps-netcdf-pl-local`.
const MEPS_VIEW: &str = "CRS=CRS:84&BBOX=0,55,30,70&WIDTH=128&HEIGHT=128";

/// A cell of the MEPS fixture: MET Norway's longitude and latitude of row
/// 5 from the south, column 4.
const MEPS_CELL: (f64, f64) = (13.827_727_538_778_511, 62.552_981_178_508_49);

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

fn meps_pl() -> axum::Router {
    wms(
        "meps-pl",
        "netcdf",
        meps_pl_engine(),
        &["air_temperature_pl", "wind_speed_pl"],
        (-60.0, 40.0),
    )
}

/// The issue's reproduction: `ELEVATION=851` draws the 850 hPa image, from
/// the entry `ELEVATION=850` filled, with its ETag, where it drew the error
/// tile; 600 hPa the 500 hPa image; midway between 1000 and 850 hPa the
/// bottom level, the default. So does the wind speed derived from both
/// components at the level drawn. GetFeatureInfo at 851 reads and reports
/// 850 hPa.
#[test]
fn a_netcdf_elevation_between_levels_draws_the_nearest_level() {
    let app = meps_pl();
    serve(netcdf_elevations(app));
}

async fn netcdf_elevations(app: axum::Router) {
    for layer in ["meps-pl/air_temperature_pl", "meps-pl/wind_speed_pl"] {
        let etags = get_maps(
            &app,
            layer,
            MEPS_VIEW,
            &[
                ("850", "MISS"),
                ("851", "HIT"),
                ("500", "MISS"),
                ("600", "HIT"),
                ("925", "MISS"),
                ("1000", "HIT"),
            ],
        )
        .await;
        assert_eq!(etags[1], etags[0], "{layer}: 851 is 850 hPa");
        assert_ne!(etags[2], etags[0], "{layer}: 500 hPa is another image");
        assert_eq!(etags[3], etags[2], "{layer}: 600 is 500 hPa");
        assert_eq!(etags[5], etags[4], "{layer}: 925 is 1000 hPa");
        let default = get(
            &app,
            &format!("REQUEST=GetMap&LAYERS={layer}&STYLES=&{MEPS_VIEW}&FORMAT=image/png"),
        )
        .await;
        assert_eq!(
            (default.x_cache.as_str(), default.etag.as_str()),
            ("HIT", etags[4].as_str()),
            "{layer}: the default is the bottom level"
        );
    }

    let (lon, lat) = MEPS_CELL;
    let bbox = format!(
        "{},{},{},{}",
        lon - 0.01,
        lat - 0.01,
        lon + 0.01,
        lat + 0.01
    );
    for layer in ["meps-pl/air_temperature_pl", "meps-pl/wind_speed_pl"] {
        let exact = feature_info(&app, layer, &bbox, "&ELEVATION=850").await;
        let between = feature_info(&app, layer, &bbox, "&ELEVATION=851").await;
        assert_eq!(between["elevation"], 850.0, "{layer}: {between}");
        assert!(between["value"].is_number(), "{layer}: {between}");
        assert_eq!(between["value"], exact["value"], "{layer}");
        let top = feature_info(&app, layer, &bbox, "&ELEVATION=10").await;
        assert_eq!(top["elevation"], 300.0, "{layer}: {top}");
        assert_ne!(top["value"], exact["value"], "{layer}");
    }
}

fn grib_config() -> GribConfig {
    serde_json::from_value(serde_json::json!({
        "level_types": ["pressure"],
        "data_path": testdata("grib-local").to_str().unwrap(),
        "poll_interval_secs": 3600,
        "index_format": "ecmwf-json",
    }))
    .unwrap()
}

/// A GRIB pressure-level collection draws the selected run's nearest
/// level: the fixture's run has 150 hPa only, so 200 and 100 hPa draw it,
/// from its rendered entry, and GetFeatureInfo reads and reports it.
#[test]
fn a_grib_elevation_between_levels_draws_the_runs_nearest_level() {
    let owner = GribEngine::new("forecast", &grib_config()).unwrap();
    let view = owner
        .level_collections()
        .into_iter()
        .find(|v| v.collection_id() == "forecast-pressure")
        .expect("the fixture's 150 hPa field makes a pressure collection");
    assert_eq!(
        view.raster_info_shared().vertical.as_ref().unwrap().levels,
        [150.0]
    );
    let id = "forecast-pressure";
    let app = wms(id, "grib", Arc::new(view), &["q"], (0.0, 1e-4));
    serve(grib_elevations(app, id));
}

async fn grib_elevations(app: axum::Router, id: &str) {
    let etags = get_maps(
        &app,
        &format!("{id}/q"),
        "CRS=CRS:84&BBOX=0,40,40,70&WIDTH=64&HEIGHT=48",
        &[("150", "MISS"), ("200", "HIT"), ("100", "HIT")],
    )
    .await;
    assert!(etags.iter().all(|etag| *etag == etags[0]), "{etags:?}");

    let exact = feature_info(&app, &format!("{id}/q"), "20,50,21,51", "&ELEVATION=150").await;
    let between = feature_info(&app, &format!("{id}/q"), "20,50,21,51", "&ELEVATION=200").await;
    assert_eq!(between["elevation"], 150.0, "{between}");
    assert!(between["value"].is_number(), "{between}");
    assert_eq!(between["value"], exact["value"]);
}

/// A radar volume draws the sweep nearest `ELEVATION` (#1064); now every
/// `ELEVATION` drawing one sweep keys that sweep's angle, so they share one
/// rendered entry: 0.6° and 0.75°, midway, are the 0.5° sweep's image, 0.9°
/// and 3° the 1.0° sweep's, whose DBZH differs. GetFeatureInfo reports the
/// sweep drawn.
#[test]
fn pvol_elevations_drawing_one_sweep_share_their_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        testdata("pvol-cold-batch.h5"),
        dir.path().join("202609161200_test_PVOL.h5"),
    )
    .unwrap();
    let config = serde_json::from_value(serde_json::json!({
        "poll_interval_secs": 3600,
        "prewarm_sweeps": 0,
        "max_files": 4,
    }))
    .unwrap();
    let engine = engine_odim::PolarVolumeEngine::new("pvol", dir.path().to_str(), &config).unwrap();
    let view = engine.site_view("test", "pvol-test");
    assert_eq!(
        view.raster_info_shared().vertical.as_ref().unwrap().levels,
        [0.5, 1.0]
    );
    let id = "pvol-test";
    // The fixture's DBZH: -26.5 dBZ on the 0.5° sweep, -21.5 on the 1.0°.
    let app = wms(id, "odim-volume", Arc::new(view), &["DBZH"], (-30.0, -20.0));
    serve(pvol_elevations(app, format!("{id}/DBZH")));
}

async fn pvol_elevations(app: axum::Router, layer: String) {
    let view = "CRS=CRS:84&BBOX=24.4,60.25,24.6,60.35&WIDTH=32&HEIGHT=32";
    let etags = get_maps(
        &app,
        &layer,
        view,
        &[
            ("0.5", "MISS"),
            ("0.6", "HIT"),
            ("0.75", "HIT"),
            ("0.9", "MISS"),
            ("1.0", "HIT"),
            ("3", "HIT"),
        ],
    )
    .await;
    assert_eq!(etags[1], etags[0]);
    assert_eq!(etags[2], etags[0]);
    assert_ne!(etags[3], etags[0], "the 1.0° sweep is another image");
    assert_eq!(etags[4], etags[3]);
    assert_eq!(etags[5], etags[3]);

    let info = feature_info(&app, &layer, "24.49,60.29,24.51,60.31", "&ELEVATION=0.9").await;
    assert_eq!(info["elevation"], 1.0, "{info}");
    assert_eq!(info["value"], -21.5, "{info}");
    let info = feature_info(&app, &layer, "24.49,60.29,24.51,60.31", "&ELEVATION=0.6").await;
    assert_eq!(info["elevation"], 0.5, "{info}");
    assert_eq!(info["value"], -26.5, "{info}");
}

/// OGC API Maps `elevation` resolves and keys its level as WMS does
/// (`api_common::subset::resolve_frame`, #1477): 851 is 850 hPa's map, from
/// its rendered entry; 600 is 500 hPa's; the default is 1000 hPa's, as 925,
/// midway, is.
#[test]
fn a_maps_elevation_between_levels_keys_the_nearest_level() {
    let id = "meps-pl";
    let maps = api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines: HashMap::from([(id.to_string(), meps_pl_engine() as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(id.to_string(), collection(id, "netcdf"))]),
        discovery: Default::default(),
        styles: HashMap::from([(id.to_string(), style(-60.0, 40.0))]),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    })));
    serve(async move {
        let map = |extra: &str| {
            let uri = format!(
                "/collections/{id}/map?bbox=0,55,30,70&width=64&height=64\
                 &parameter-name=air_temperature_pl{extra}"
            );
            let maps = maps.clone();
            async move {
                let response = maps
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default()
                };
                (header("x-cache"), header("etag"))
            }
        };
        let (x_cache, at_850) = map("&elevation=850").await;
        assert_ne!(x_cache, "HIT");
        assert_eq!(
            map("&elevation=851").await,
            ("HIT".to_string(), at_850.clone())
        );
        let (x_cache, at_500) = map("&elevation=500").await;
        assert_ne!(x_cache, "HIT");
        assert_ne!(at_500, at_850);
        assert_eq!(map("&elevation=600").await, ("HIT".to_string(), at_500));
        let (x_cache, default) = map("").await;
        assert_ne!(x_cache, "HIT");
        assert_eq!(
            map("&elevation=925").await,
            ("HIT".to_string(), default.clone())
        );
        assert_eq!(map("&elevation=1000").await, ("HIT".to_string(), default));
    });
}

/// OGC API Tiles `elevation` resolves and keys its level as Maps does
/// (`api_common::subset::resolve_frame`, #1477): a map tile at 851 hPa is
/// 850 hPa's, from its rendered entry; 600 is 500 hPa's; the default is
/// 1000 hPa's, as 925, midway, is.
#[test]
fn a_tiles_elevation_between_levels_keys_the_nearest_level() {
    let id = "meps-pl";
    let tiles = api_tiles::router(Arc::new(ArcSwap::from_pointee(api_tiles::TilesState {
        map_engines: HashMap::from([(id.to_string(), meps_pl_engine() as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(id.to_string(), collection(id, "netcdf"))]),
        discovery: Default::default(),
        styles: HashMap::from([(id.to_string(), style(-60.0, 40.0))]),
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(ds_mvt::VectorTileCache::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    serve(async move {
        // The WebMercatorQuad tile (row 8, column 17 at level 5) over
        // 11.25–22.5°E, 62.9–66.5°N.
        let tile = |extra: &str| {
            let uri = format!(
                "/collections/{id}/tiles/WebMercatorQuad/5/8/17\
                 ?parameter-name=air_temperature_pl{extra}"
            );
            let tiles = tiles.clone();
            async move {
                let response = tiles
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default()
                };
                (header("x-cache"), header("etag"))
            }
        };
        let (x_cache, at_850) = tile("&elevation=850").await;
        assert_ne!(x_cache, "HIT");
        assert_eq!(
            tile("&elevation=851").await,
            ("HIT".to_string(), at_850.clone())
        );
        let (x_cache, at_500) = tile("&elevation=500").await;
        assert_ne!(x_cache, "HIT");
        assert_ne!(at_500, at_850);
        assert_eq!(tile("&elevation=600").await, ("HIT".to_string(), at_500));
        let (x_cache, default) = tile("").await;
        assert_ne!(x_cache, "HIT");
        assert_eq!(
            tile("&elevation=925").await,
            ("HIT".to_string(), default.clone())
        );
        assert_eq!(tile("&elevation=1000").await, ("HIT".to_string(), default));
    });
}
