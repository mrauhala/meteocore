//! GRIB hour-window aggregates (#1005) through WMS, Maps, Tiles and EDR,
//! end to end: an aggregate's child layer advertises only the steps that
//! carry it, with its own default, and a GetMap with TIME omitted or set to
//! a step without the field renders (and caches under) the nearest step
//! that has it, instead of the red error tile. With two runs, every map
//! API renders and keys the run that has the window at that time.
//!
//! The fields are the real ARPEGE 10 m wind messages of
//! `testdata/grib-arpege-wind`, written into GFS-style step files with
//! wgrib2 sidecars: u and v at every step, the wind speed as a GFS-style
//! average that restarts every six hours, never at the analysis. The engine
//! is wrapped in `DerivedWind`, as the server registers it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, GribConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::MapEngine;
use ds_core::wind::DerivedWind;
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};
use engine_grib::GribEngine;

const ID: &str = "gfs-windows";
/// The ARPEGE crop's domain.
const BBOX: &str = "-30,22,40,70";
/// Each ARPEGE message is 3257 bytes: 10wdir, 10si, 10u, 10v.
const MESSAGE: usize = 3257;

fn at(hours: i64) -> DateTime<Utc> {
    "2026-05-11T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + chrono::Duration::hours(hours)
}

fn rfc3339(hours: i64) -> String {
    at(hours).to_rfc3339()
}

/// f000: u and v. f003, f006, f009: u, v and the wind speed averaged since
/// the last six-hourly restart, `WIND_avg_3h` at f003 and f009,
/// `WIND_avg_6h` at f006.
fn write_steps(dir: &Path) {
    write_run(
        dir,
        "2026051100",
        &[
            (0, "anl", None),
            (3, "3 hour fcst", Some("0-3 hour ave fcst")),
            (6, "6 hour fcst", Some("0-6 hour ave fcst")),
            (9, "9 hour fcst", Some("6-9 hour ave fcst")),
        ],
    );
}

/// One run's step files, `date` as wgrib2 writes it (`YYYYMMDDHH`): u and v
/// at every step, and the wind speed under the step's window when it has one.
fn write_run(dir: &Path, date: &str, steps: &[(u32, &str, Option<&str>)]) {
    let source = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/grib-arpege-wind/arpege-10m-wind.grib2"),
    )
    .unwrap();
    let message = |index: usize| &source[index * MESSAGE..(index + 1) * MESSAGE];
    let (speed, u, v) = (message(1), message(2), message(3));
    for &(step, forecast, window) in steps {
        let mut records = vec![("UGRD", forecast, u), ("VGRD", forecast, v)];
        records.extend(window.map(|window| ("WIND", window, speed)));
        let (mut data, mut index) = (Vec::new(), String::new());
        for (i, (param, forecast, bytes)) in records.into_iter().enumerate() {
            index.push_str(&format!(
                "{}:{}:d={date}:{param}:10 m above ground:{forecast}:\n",
                i + 1,
                data.len()
            ));
            data.extend_from_slice(bytes);
        }
        let name = format!("{date}-f{step:03}");
        std::fs::write(dir.join(format!("{name}.grib2")), data).unwrap();
        std::fs::write(dir.join(format!("{name}.idx")), index).unwrap();
    }
}

fn grib_config(dir: &Path) -> GribConfig {
    serde_json::from_value(serde_json::json!({
        "data_path": dir.to_str().unwrap(),
        "index_format": "wgrib2", "index_suffix": ".idx", "grid_cache_mb": 16
    }))
    .unwrap()
}

/// The engine as the server registers it: wrapped for derived wind.
fn engine(dir: &Path) -> Arc<DerivedWind> {
    let grib = Arc::new(GribEngine::new(ID, &grib_config(dir)).unwrap());
    Arc::new(DerivedWind::new(
        ID,
        grib,
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    ))
}

fn collection(dir: &Path, api: &str) -> CollectionConfig {
    CollectionConfig {
        id: ID.into(),
        title: "GFS-style windows".into(),
        description: "Hour-window averages present at some steps".into(),
        data_path: None,
        apis: vec![api.into()],
        engine_type: "grib".into(),
        keywords: Vec::new(),
        license: None,
        geotiff: None,
        querydata: None,
        wms: None,
        grib: Some(grib_config(dir)),
        zarr: None,
        odim: None,
        cap: None,
        postgis: None,
        nowcast: None,
        bufr: None,
        satellite: None,
        preview: None,
        derive_wind: None,
    }
}

fn styles() -> HashMap<String, HashMap<String, StyleInfo>> {
    let style = StyleInfo {
        name: "default".into(),
        title: "Default".into(),
        palette: ds_render::builtin_palette_arc("viridis").unwrap(),
        colormap: Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::Viridis,
            0.0,
            30.0,
        )),
        min: 0.0,
        max: 30.0,
        parameter: None,
    };
    HashMap::from([(
        ID.to_string(),
        HashMap::from([("default".to_string(), style)]),
    )])
}

fn wms(engine: Arc<DerivedWind>, dir: &Path) -> axum::Router {
    api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, "wms"))]),
        styles: styles(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(64)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

fn maps(engine: Arc<DerivedWind>, dir: &Path) -> axum::Router {
    api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, "maps"))]),
        styles: styles(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    })))
}

fn tiles(engine: Arc<DerivedWind>, dir: &Path) -> axum::Router {
    api_tiles::router(Arc::new(ArcSwap::from_pointee(api_tiles::TilesState {
        map_engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, "tiles"))]),
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

fn edr(engine: Arc<DerivedWind>, dir: &Path) -> axum::Router {
    api_edr::router(Arc::new(ArcSwap::from_pointee(
        api_edr::handlers::EdrState {
            engines: HashMap::from([(ID.to_string(), engine as Arc<dyn EdrEngine>)]),
            feature_engines: HashMap::new(),
            collections: HashMap::from([(ID.to_string(), collection(dir, "edr"))]),
            styles: HashMap::new(),
            base_url: String::new(),
            trust_proxy_headers: false,
        },
    )))
}

struct Reply {
    status: StatusCode,
    x_cache: String,
    /// `OGCAPI-datetime`: the instant a map tile rendered.
    datetime: String,
    body: Bytes,
}

async fn get(app: &axum::Router, uri: &str) -> Reply {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    };
    let (x_cache, datetime) = (header("x-cache"), header("ogcapi-datetime"));
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        x_cache,
        datetime,
        body,
    }
}

fn json(reply: &Reply) -> Value {
    serde_json::from_slice(&reply.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wms_aggregate_layers_advertise_and_render_their_own_steps() {
    let dir = tempfile::tempdir().unwrap();
    write_steps(dir.path());
    let app = wms(engine(dir.path()), dir.path());

    let caps = get(&app, "/?SERVICE=WMS&REQUEST=GetCapabilities").await;
    assert_eq!(caps.status, StatusCode::OK);
    let caps = String::from_utf8(caps.body.to_vec()).unwrap();
    let layer = |name: &str| {
        let start = caps
            .find(&format!("<Name>{ID}/{name}</Name>"))
            .unwrap_or_else(|| panic!("no layer {name}: {caps}"));
        let end = caps[start..].find("</Layer>").unwrap();
        caps[start..start + end].to_string()
    };
    let own_time = |name: &str, default: i64, values: &[i64]| {
        let values: Vec<String> = values.iter().map(|&h| rfc3339(h)).collect();
        let dimension = format!(
            "default=\"{}\" nearestValue=\"1\">{}<",
            rfc3339(default),
            values.join(",")
        );
        let layer = layer(name);
        assert!(layer.contains(&dimension), "{name}: {layer}");
    };
    own_time("WIND_avg_3h", 9, &[3, 9]);
    own_time("WIND_avg_6h", 6, &[6]);
    // At every step: the parent's axis, inherited.
    for name in ["UGRD", "VGRD"] {
        let layer = layer(name);
        assert!(!layer.contains("<Dimension name=\"time\""), "{layer}");
    }

    let map = |name: &str, time: Option<i64>| {
        let time = time.map_or(String::new(), |h| {
            format!("&TIME={}", at(h).format("%Y-%m-%dT%H:%M:%SZ"))
        });
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={ID}/{name}&STYLES=\
             &FORMAT=image/png&CRS=CRS:84&BBOX={BBOX}&WIDTH=64&HEIGHT=64{time}"
        )
    };
    // TIME omitted: the layer's own default, f006, not the collection's
    // f009, which has no 6 h average. It used to be the red error tile.
    let first = get(&app, &map("WIND_avg_6h", None)).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.x_cache, "MISS");
    // Every TIME snaps to f006 and hits what was cached under it.
    for hours in [6, 0, 3, 9] {
        let reply = get(&app, &map("WIND_avg_6h", Some(hours))).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.x_cache, "HIT", "TIME = +{hours} h");
        assert_eq!(reply.body, first.body);
    }
    // The 3 h average: its own nearest step, f003 for the analysis.
    let reply = get(&app, &map("WIND_avg_3h", Some(0))).await;
    assert_eq!(reply.x_cache, "MISS");
    let reply = get(&app, &map("WIND_avg_3h", Some(3))).await;
    assert_eq!(reply.x_cache, "HIT");
    let reply = get(&app, &map("WIND_avg_3h", None)).await;
    assert_eq!(reply.x_cache, "MISS", "its default is f009");
    let reply = get(&app, &map("WIND_avg_3h", Some(8))).await;
    assert_eq!(reply.x_cache, "HIT");
    // The components keep the collection's steps.
    let reply = get(&app, &map("UGRD", Some(0))).await;
    assert_eq!(
        (reply.status, reply.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maps_and_edr_serve_an_aggregate_on_its_own_axis() {
    let dir = tempfile::tempdir().unwrap();
    write_steps(dir.path());
    let engine = engine(dir.path());

    let app = maps(engine.clone(), dir.path());
    let collection = json(&get(&app, &format!("/collections/{ID}")).await);
    let names = &collection["parameter_names"];
    assert_eq!(
        names["WIND_avg_6h"]["extent"]["temporal"]["interval"],
        serde_json::json!([[rfc3339(6), rfc3339(6)]])
    );
    assert_eq!(
        names["WIND_avg_3h"]["extent"]["temporal"]["interval"],
        serde_json::json!([[rfc3339(3), rfc3339(9)]])
    );
    assert!(names["UGRD"].get("extent").is_none(), "{}", names["UGRD"]);
    // The analysis has no average: the nearest step that has one renders,
    // where it used to be a 400 calling the parameter invalid.
    for datetime in ["2026-05-11T00:00:00Z", "2026-05-11T06:00:00Z"] {
        let reply = get(
            &app,
            &format!(
                "/collections/{ID}/map?bbox={BBOX}&width=64&height=64\
                 &parameter-name=WIND_avg_6h&datetime={datetime}"
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{datetime}");
        assert!(reply.body.starts_with(b"\x89PNG"));
    }

    let app = edr(engine, dir.path());
    let collection = json(&get(&app, &format!("/collections/{ID}")).await);
    let names = &collection["parameter_names"];
    assert_eq!(
        names["WIND_avg_3h"]["extent"]["temporal"]["values"],
        serde_json::json!([rfc3339(3), rfc3339(9)])
    );
    assert!(names["UGRD"].get("extent").is_none(), "{}", names["UGRD"]);
}

/// The API layers resolve the run with the parameter. At 06Z the newest
/// run's analysis has no 6 h average, but the 00Z run has one valid then:
/// that is what renders, and keys. Resolved without the parameter, the run
/// was the newest, whose nearest 6 h average is its f006, valid 12Z.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_run_rendered_is_the_parameters_on_every_map_api() {
    let dir = tempfile::tempdir().unwrap();
    write_steps(dir.path());
    write_run(
        dir.path(),
        "2026051106",
        &[
            (0, "anl", None),
            (3, "3 hour fcst", Some("0-3 hour ave fcst")),
            (6, "6 hour fcst", Some("0-6 hour ave fcst")),
        ],
    );
    let engine = engine(dir.path());
    assert_eq!(engine.raster_info_shared().reference_times, [at(0), at(6)]);
    assert_eq!(
        engine.resolve_reference_time(Some(at(6)), None),
        Some(at(6))
    );
    assert_eq!(
        engine.resolve_parameter_reference_time(Some("WIND_avg_6h"), Some(at(6)), None),
        Some(at(0))
    );

    let app = wms(engine.clone(), dir.path());
    let map = |time: i64, run: Option<i64>| {
        let stamp = |h: i64| at(h).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let run = run.map_or(String::new(), |h| {
            format!("&DIM_REFERENCE_TIME={}", stamp(h))
        });
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={ID}/WIND_avg_6h&STYLES=\
             &FORMAT=image/png&CRS=CRS:84&BBOX={BBOX}&WIDTH=64&HEIGHT=64\
             &TIME={}{run}",
            stamp(time)
        )
    };
    let first = get(&app, &map(6, None)).await;
    assert_eq!(
        (first.status, first.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    // A client echoing the advertised default run, the newest, and a pin of
    // the run rendered both hit the entry keyed on the 00Z run's f006.
    for run in [6, 0] {
        let reply = get(&app, &map(6, Some(run))).await;
        assert_eq!(reply.x_cache, "HIT", "DIM_REFERENCE_TIME = +{run} h");
        assert_eq!(reply.body, first.body);
    }
    // The 06Z run's own window is another entry.
    assert_eq!(get(&app, &map(12, None)).await.x_cache, "MISS");

    let app = maps(engine.clone(), dir.path());
    let map = |time: &str| {
        format!(
            "/collections/{ID}/map?bbox={BBOX}&width=64&height=64\
             &parameter-name=WIND_avg_6h&datetime={time}"
        )
    };
    let reply = get(&app, &map("2026-05-11T06:00:00Z")).await;
    assert_eq!(
        (reply.status, reply.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    let reply = get(&app, &map("2026-05-11T12:00:00Z")).await;
    assert_eq!(
        (reply.status, reply.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );

    let reply = get(
        &tiles(engine, dir.path()),
        &format!(
            "/collections/{ID}/tiles/WebMercatorQuad/2/1/2\
             ?parameter-name=WIND_avg_6h&datetime=2026-05-11T06:00:00Z"
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.datetime, "2026-05-11T06:00:00Z");
}
