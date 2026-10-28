//! GRIB hour-window aggregates (#1005) through WMS, Maps, Tiles and EDR,
//! end to end: an aggregate's child layer advertises only the steps that
//! carry it, with its own default, and a GetMap with TIME omitted or set to
//! a step without the field renders (and caches under) the nearest step
//! that has it, instead of the red error tile. With two runs, every map
//! API renders and keys the run that has the window at that time.
//!
//! A scan that adds steps a frame does not draw from keeps its cached
//! image and meta-tiles (#1334).
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

/// The ARPEGE messages: 10wdir, 10si, 10u, 10v.
fn arpege() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/grib-arpege-wind/arpege-10m-wind.grib2"),
    )
    .unwrap()
}

/// The ARPEGE message at `index`.
fn arpege_message(source: &[u8], index: usize) -> &[u8] {
    &source[index * MESSAGE..(index + 1) * MESSAGE]
}

/// One run's step files, `date` as wgrib2 writes it (`YYYYMMDDHH`): u and v
/// at every step, and the wind speed under the step's window when it has one.
fn write_run(dir: &Path, date: &str, steps: &[(u32, &str, Option<&str>)]) {
    let source = arpege();
    let message = |index| arpege_message(&source, index);
    let (speed, u, v) = (message(1), message(2), message(3));
    let level = "10 m above ground";
    for &(step, forecast, window) in steps {
        let mut records = vec![("UGRD", level, forecast, u), ("VGRD", level, forecast, v)];
        records.extend(window.map(|window| ("WIND", level, window, speed)));
        write_file(dir, &format!("{date}-f{step:03}"), date, &records);
    }
}

/// The step file `name` of the run `date` and its wgrib2 sidecar: each
/// record's parameter, level, forecast field and message.
fn write_file(dir: &Path, name: &str, date: &str, records: &[(&str, &str, &str, &[u8])]) {
    let (mut data, mut index) = (Vec::new(), String::new());
    for (i, (param, level, forecast, bytes)) in records.iter().enumerate() {
        index.push_str(&format!(
            "{}:{}:d={date}:{param}:{level}:{forecast}:\n",
            i + 1,
            data.len()
        ));
        data.extend_from_slice(bytes);
    }
    std::fs::write(dir.join(format!("{name}.grib2")), data).unwrap();
    std::fs::write(dir.join(format!("{name}.idx")), index).unwrap();
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
    wrapped(Arc::new(GribEngine::new(ID, &grib_config(dir)).unwrap()))
}

fn wrapped(grib: Arc<GribEngine>) -> Arc<DerivedWind> {
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
        station_grid: None,
        bufr: None,
        satellite: None,
        netcdf: None,
        csv: None,
        preview: None,
        derive_wind: None,
        derive_geopotential_height: None,
        derive_humidity: None,
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
    wms_with(engine, dir, Arc::new(ds_render::TilePixelCache::new(64)))
}

/// [`wms`] over the meta-tile cache `tile_cache`.
fn wms_with(
    engine: Arc<DerivedWind>,
    dir: &Path,
    tile_cache: Arc<ds_render::TilePixelCache>,
) -> axum::Router {
    api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, "wms"))]),
        styles: styles(),
        point_engines: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache,
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

/// A new run with only its analysis next to the complete 00Z run (#1026):
/// the windows the analysis lacks stay WMS layers, Maps parameters and EDR
/// parameter names, and render and query from the 00Z run, where they used
/// to drop out of every list until the new run's f003.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_analysis_only_run_keeps_the_windows_on_every_api() {
    let dir = tempfile::tempdir().unwrap();
    write_steps(dir.path());
    write_run(dir.path(), "2026051106", &[(0, "anl", None)]);
    let engine = engine(dir.path());
    assert_eq!(engine.raster_info_shared().reference_times, [at(0), at(6)]);

    let app = wms(engine.clone(), dir.path());
    let caps = get(&app, "/?SERVICE=WMS&REQUEST=GetCapabilities").await;
    let caps = String::from_utf8(caps.body.to_vec()).unwrap();
    for name in ["UGRD", "VGRD", "WIND_avg_3h", "WIND_avg_6h"] {
        assert!(
            caps.contains(&format!("<Name>{ID}/{name}</Name>")),
            "{name}"
        );
    }
    let map = |time: Option<i64>| {
        let time = time.map_or(String::new(), |h| {
            format!("&TIME={}", at(h).format("%Y-%m-%dT%H:%M:%SZ"))
        });
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={ID}/WIND_avg_6h&STYLES=\
             &FORMAT=image/png&CRS=CRS:84&BBOX={BBOX}&WIDTH=64&HEIGHT=64{time}"
        )
    };
    // TIME omitted: the 00Z run's f006, never the error tile; the same
    // step requested by its time, or the analysis without a window, hits.
    let first = get(&app, &map(None)).await;
    assert_eq!(
        (first.status, first.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    for hours in [6, 0] {
        let reply = get(&app, &map(Some(hours))).await;
        assert_eq!(reply.x_cache, "HIT", "TIME = +{hours} h");
        assert_eq!(reply.body, first.body);
    }

    let app = maps(engine.clone(), dir.path());
    let collection = json(&get(&app, &format!("/collections/{ID}")).await);
    assert_eq!(
        collection["parameter_names"]["WIND_avg_3h"]["extent"]["temporal"]["interval"],
        serde_json::json!([[rfc3339(3), rfc3339(9)]])
    );
    let reply = get(
        &app,
        &format!("/collections/{ID}/map?bbox={BBOX}&width=64&height=64&parameter-name=WIND_avg_6h"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.starts_with(b"\x89PNG"));

    let app = edr(engine, dir.path());
    let collection = json(&get(&app, &format!("/collections/{ID}")).await);
    assert!(
        collection["parameter_names"]["WIND_avg_6h"].is_object(),
        "{}",
        collection["parameter_names"]
    );
    // No datetime: the run that has the window, not the new analysis.
    let reply = get(
        &app,
        &format!("/collections/{ID}/position?coords=POINT(10%2050)&parameter-name=WIND_avg_6h"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let coverage = json(&reply);
    assert_eq!(
        coverage["domain"]["axes"]["t"]["values"]
            .as_array()
            .unwrap()
            .len(),
        4,
        "{coverage}"
    );
    let values = coverage["ranges"]["WIND_avg_6h"]["values"]
        .as_array()
        .unwrap();
    assert!(values[2].is_number(), "{values:?}");
    assert!(values[0].is_null() && values[1].is_null() && values[3].is_null());
}

/// GRIB map caches key on the message a frame draws (#1334). A scan that
/// adds another step of the frame's run and a newer run leaves the frame's
/// image and meta-tiles hitting, in WMS, Maps and Tiles, where keyed on the
/// catalog's version every cached frame missed; a late file that changes
/// the message the frame draws misses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scan_adding_other_steps_keeps_a_frames_cached_tiles() {
    let dir = tempfile::tempdir().unwrap();
    write_run(
        dir.path(),
        "2026051100",
        &[
            (0, "anl", None),
            (3, "3 hour fcst", Some("0-3 hour ave fcst")),
        ],
    );
    let grib = Arc::new(GribEngine::new(ID, &grib_config(dir.path())).unwrap());
    let tile_cache = Arc::new(ds_render::TilePixelCache::new(64));
    let app = wms_with(wrapped(grib.clone()), dir.path(), tile_cache.clone());
    // Web Mercator: drawn from meta-tiles.
    let map = |extra: &str| {
        format!(
            "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={ID}/UGRD&STYLES=\
             &FORMAT=image/png&CRS=EPSG:3857&BBOX=0,5000000,1000000,6000000\
             &WIDTH=64&HEIGHT=64&TIME=2026-05-11T03:00:00Z{extra}"
        )
    };
    let first = get(&app, &map("")).await;
    assert_eq!(
        (first.status, first.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    let (_, misses) = tile_cache.stats();
    assert!(misses > 0, "drawn from meta-tiles");
    let maps = maps(wrapped(grib.clone()), dir.path());
    let tiles = tiles(wrapped(grib.clone()), dir.path());
    let frame = "parameter-name=UGRD&datetime=2026-05-11T03:00:00Z";
    let others = [
        (
            &maps,
            format!("/collections/{ID}/map?bbox={BBOX}&width=64&height=64&{frame}"),
        ),
        (
            &tiles,
            format!("/collections/{ID}/tiles/WebMercatorQuad/2/1/2?{frame}"),
        ),
    ];
    for (app, uri) in &others {
        let reply = get(app, uri).await;
        assert_eq!(
            (reply.status, reply.x_cache.as_str()),
            (StatusCode::OK, "MISS"),
            "{uri}"
        );
    }

    // Another step of the frame's run, and a newer run whose analysis does
    // not cover the frame's valid time.
    write_run(
        dir.path(),
        "2026051100",
        &[(6, "6 hour fcst", Some("0-6 hour ave fcst"))],
    );
    write_run(dir.path(), "2026051106", &[(0, "anl", None)]);
    let catalog = grib.content_version();
    grib.scan_once().unwrap();
    assert_eq!(grib.raster_info_shared().reference_times, [at(0), at(6)]);
    assert_ne!(grib.content_version(), catalog);

    let reply = get(&app, &map("")).await;
    assert_eq!(
        (reply.status, reply.x_cache.as_str()),
        (StatusCode::OK, "HIT")
    );
    assert_eq!(reply.body, first.body);
    // A transparent view of the frame is another image, from the same
    // meta-tiles: every one of them hits.
    let (hits, _) = tile_cache.stats();
    let reply = get(&app, &map("&TRANSPARENT=TRUE")).await;
    assert_eq!(
        (reply.status, reply.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    let (now_hits, now_misses) = tile_cache.stats();
    assert!(now_hits > hits);
    assert_eq!(now_misses, misses, "no meta-tile rendered again");
    for (app, uri) in &others {
        assert_eq!(get(app, uri).await.x_cache, "HIT", "{uri}");
    }

    // A late file of the frame's own step with u at 2 m, the run's new
    // canonical level: the frame draws another message.
    let source = arpege();
    write_file(
        dir.path(),
        "2026051100-f003-late",
        "2026051100",
        &[(
            "UGRD",
            "2 m above ground",
            "3 hour fcst",
            arpege_message(&source, 2),
        )],
    );
    grib.scan_once().unwrap();
    let reply = get(&app, &map("")).await;
    assert_eq!(
        (reply.status, reply.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    assert_eq!(get(&app, &map("")).await.x_cache, "HIT");
    for (app, uri) in &others {
        assert_eq!(get(app, uri).await.x_cache, "MISS", "{uri}");
    }
}
