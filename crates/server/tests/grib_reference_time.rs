//! Model-run selection on OGC API - Maps maps and map tiles (#337), end to
//! end over a GRIB engine wrapped for derived wind, as the server registers
//! it: `subset=reference_time("<run>")` draws that run's pixels and keys its
//! own cache entry, and selects the time on the run's own valid times: an
//! interval, `*` or a partial date draws the run's latest step inside it, a
//! time outside the run is no data, the latest run named included.
//! `reference_time(*)` is the latest run as omitting it is, an unknown run
//! is a 404, and the collection's `extent` lists the runs as the
//! `reference_time` dimension, by their EDR instance ids.
//!
//! The fields are the real ARPEGE 10 m wind messages of
//! `testdata/grib-arpege-wind`, written into two GFS-style runs with wgrib2
//! sidecars: 00Z with f000–f009 and 06Z with f000–f006, so 06Z and 09Z are
//! valid times of both. `UGRD` is the u message in the 00Z run and the v
//! message in the 06Z run, so the two runs draw different pixels at one
//! valid time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ds_core::config::{CollectionConfig, GribConfig};
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::MapEngine;
use ds_core::wind::DerivedWind;
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};
use engine_grib::GribEngine;

const ID: &str = "gfs-runs";
/// The ARPEGE crop's domain.
const BBOX: &str = "-30,22,40,70";
/// Each ARPEGE message is 3257 bytes: 10wdir, 10si, 10u, 10v.
const MESSAGE: usize = 3257;
const OLD_RUN: &str = "2026-05-11T00:00:00Z";
const NEW_RUN: &str = "2026-05-11T06:00:00Z";
/// A valid time of both runs: the 00Z run's f009, the 06Z run's f003.
const BOTH: &str = "2026-05-11T09:00:00Z";

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// The ARPEGE messages: 10wdir, 10si, 10u, 10v.
fn arpege() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/grib-arpege-wind/arpege-10m-wind.grib2"),
    )
    .unwrap()
}

/// One run's step files, `date` as wgrib2 writes it (`YYYYMMDDHH`): `UGRD`
/// from the ARPEGE message `u` and `VGRD` from `v` at every step.
fn write_run(dir: &Path, date: &str, steps: &[(u32, &str)], (u, v): (usize, usize)) {
    let source = arpege();
    let message = |index: usize| &source[index * MESSAGE..(index + 1) * MESSAGE];
    let level = "10 m above ground";
    for &(step, forecast) in steps {
        let records = [("UGRD", message(u)), ("VGRD", message(v))];
        let (mut data, mut index) = (Vec::new(), String::new());
        for (i, (param, bytes)) in records.iter().enumerate() {
            index.push_str(&format!(
                "{}:{}:d={date}:{param}:{level}:{forecast}:\n",
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

/// The two runs: u and v as they are at 00Z, swapped at 06Z.
fn write_runs(dir: &Path) {
    write_run(
        dir,
        "2026051100",
        &[
            (0, "anl"),
            (3, "3 hour fcst"),
            (6, "6 hour fcst"),
            (9, "9 hour fcst"),
        ],
        (2, 3),
    );
    write_run(
        dir,
        "2026051106",
        &[(0, "anl"), (3, "3 hour fcst"), (6, "6 hour fcst")],
        (3, 2),
    );
}

fn grib_config(dir: &Path) -> GribConfig {
    serde_json::from_value(json!({
        "data_path": dir.to_str().unwrap(),
        "index_format": "wgrib2", "index_suffix": ".idx", "grid_cache_mb": 16
    }))
    .unwrap()
}

/// The engine as the server registers it: wrapped for derived wind.
fn engine(dir: &Path) -> Arc<DerivedWind> {
    Arc::new(DerivedWind::new(
        ID,
        Arc::new(GribEngine::new(ID, &grib_config(dir)).unwrap()),
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    ))
}

fn collection(dir: &Path, api: &str) -> CollectionConfig {
    CollectionConfig {
        id: ID.into(),
        title: "GFS-style runs".into(),
        description: "Two forecast runs covering the same valid times".into(),
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
        discovery: None,
        derive_humidity: None,
        expected_interval: None,
    }
}

/// Wind components either side of zero, so u and v draw apart.
fn styles() -> HashMap<String, HashMap<String, StyleInfo>> {
    let style = StyleInfo {
        name: "default".into(),
        title: "Default".into(),
        palette: ds_render::builtin_palette_arc("viridis").unwrap(),
        colormap: Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::Viridis,
            -30.0,
            30.0,
        )),
        min: -30.0,
        max: 30.0,
        parameter: None,
        contour: None,
    };
    HashMap::from([(
        ID.to_string(),
        HashMap::from([("default".to_string(), style)]),
    )])
}

fn maps(engine: Arc<DerivedWind>, dir: &Path) -> axum::Router {
    api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines: HashMap::from([(ID.to_string(), engine as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection(dir, "maps"))]),
        discovery: Default::default(),
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
        discovery: Default::default(),
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
            discovery: Default::default(),
            styles: HashMap::new(),
            base_url: String::new(),
            trust_proxy_headers: false,
        },
    )))
}

struct Reply {
    status: StatusCode,
    x_cache: String,
    /// `Content-Datetime` of a map, `OGCAPI-datetime` of a map tile: the
    /// instant drawn.
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
    let x_cache = header("x-cache");
    let datetime = [header("content-datetime"), header("ogcapi-datetime")].concat();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        x_cache,
        datetime,
        body,
    }
}

fn json_of(reply: &Reply) -> Value {
    serde_json::from_slice(&reply.body).unwrap()
}

/// `subset=reference_time("<run>")`, URL-encoded.
fn run(value: &str) -> String {
    format!("subset=reference_time(%22{value}%22)")
}

/// One route's run selection: `uri` builds the request from its extra
/// query, `time` is the route's time `subset` axis and `no_data` its answer
/// for a time that selects no step, a Maps 404 or an empty map tile. Both
/// routes behave the same.
async fn assert_run_selection(
    app: &axum::Router,
    time: &str,
    no_data: StatusCode,
    uri: impl Fn(&str) -> String,
) {
    let datetime = format!("datetime={BOTH}");
    // No run named: the latest run, 06Z.
    let latest = get(app, &uri(&datetime)).await;
    assert_eq!(
        (latest.status, latest.x_cache.as_str()),
        (StatusCode::OK, "MISS")
    );
    assert_eq!(latest.datetime, BOTH);
    // The 00Z run at the same valid time: its own pixels, its own entry.
    let old = get(app, &uri(&format!("{datetime}&{}", run(OLD_RUN)))).await;
    assert_eq!((old.status, old.x_cache.as_str()), (StatusCode::OK, "MISS"));
    assert_ne!(
        old.body, latest.body,
        "the 00Z run drew the 06Z run's pixels"
    );
    assert_eq!(old.datetime, BOTH);
    // The same frame, however the request names it.
    for query in [
        format!("{datetime}&{}", run(OLD_RUN)),
        // The compact instance id served before #947.
        format!("{datetime}&{}", run("20260511T0000Z")),
        format!("subset={time}(%22{BOTH}%22),reference_time(%22{OLD_RUN}%22)"),
        // No time: the run's own latest step, f009, not the collection's
        // 12Z, which the 00Z run does not have.
        run(OLD_RUN),
        // An interval, `*` and a partial date select on the run's own
        // times: their latest is f009, where the collection's axis would
        // pick 12Z, which only the 06Z run has.
        format!("datetime={OLD_RUN}/..&{}", run(OLD_RUN)),
        format!("datetime=../2026-05-11T23:00:00Z&{}", run(OLD_RUN)),
        format!("subset={time}(*),reference_time(%22{OLD_RUN}%22)"),
        format!("subset={time}(%222026-05-11%22),reference_time(%22{OLD_RUN}%22)"),
        format!("subset={time}(%22{OLD_RUN}%22:*),reference_time(%22{OLD_RUN}%22)"),
    ] {
        let reply = get(app, &uri(&query)).await;
        assert_eq!(
            (reply.status, reply.x_cache.as_str()),
            (StatusCode::OK, "HIT"),
            "{query}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(reply.body, old.body, "{query}");
        assert_eq!(reply.datetime, BOTH, "{query}");
    }
    // A step only the 00Z run has: drawn from it when it is named.
    let early = get(
        app,
        &uri(&format!(
            "subset={time}(%222026-05-11T03:00:00Z%22),reference_time(%22{OLD_RUN}%22)"
        )),
    )
    .await;
    assert_eq!(early.status, StatusCode::OK);
    assert_eq!(early.datetime, "2026-05-11T03:00:00Z");
    // `*` and the latest run named are the latest run, as omitting it is.
    for query in [
        format!("{datetime}&subset=reference_time(*)"),
        format!("{datetime}&{}", run(NEW_RUN)),
    ] {
        let reply = get(app, &uri(&query)).await;
        assert_eq!(
            (reply.status, reply.x_cache.as_str()),
            (StatusCode::OK, "HIT"),
            "{query}"
        );
        assert_eq!(reply.body, latest.body, "{query}");
    }
    // A named run is drawn exactly: a time outside it is no data, the
    // latest run named included, never another run's frame or a 400. With
    // no run named, 03Z falls back to the 00Z run.
    for query in [
        format!("datetime=2026-05-11T12:00:00Z&{}", run(OLD_RUN)),
        format!("datetime=2026-05-11T03:00:00Z&{}", run(NEW_RUN)),
        format!("subset={time}(%222026-05-11T12:00:00Z%22),reference_time(%22{OLD_RUN}%22)"),
        format!("datetime=2026-05-11T10:00:00Z/..&{}", run(OLD_RUN)),
    ] {
        let reply = get(app, &uri(&query)).await;
        assert_eq!(reply.status, no_data, "{query}");
    }
    let fallback = get(app, &uri("datetime=2026-05-11T03:00:00Z")).await;
    assert_eq!(fallback.status, StatusCode::OK);
    assert_eq!(fallback.datetime, "2026-05-11T03:00:00Z");
    // A run the collection does not have; an interval of runs.
    let unknown = get(
        app,
        &uri(&format!("{datetime}&{}", run("2026-05-10T18:00:00Z"))),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(
        json_of(&unknown)["description"]
            .as_str()
            .unwrap()
            .contains("is not a model run"),
        "{}",
        json_of(&unknown)
    );
    let interval = get(
        app,
        &uri(&format!(
            "{datetime}&subset=reference_time(%22{OLD_RUN}%22:%22{NEW_RUN}%22)"
        )),
    )
    .await;
    assert_eq!(interval.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_named_run_draws_and_keys_that_runs_frame_on_maps_and_map_tiles() {
    let dir = tempfile::tempdir().unwrap();
    write_runs(dir.path());
    let engine = engine(dir.path());
    assert_eq!(
        engine.raster_info_shared().reference_times,
        [at(OLD_RUN), at(NEW_RUN)]
    );

    let app = maps(engine.clone(), dir.path());
    assert_run_selection(&app, "time", StatusCode::NOT_FOUND, |query| {
        format!("/collections/{ID}/map?bbox={BBOX}&width=64&height=64&parameter-name=UGRD&{query}")
    })
    .await;
    let app = tiles(engine, dir.path());
    assert_run_selection(&app, "datetime", StatusCode::NO_CONTENT, |query| {
        format!("/collections/{ID}/tiles/WebMercatorQuad/2/1/2?parameter-name=UGRD&{query}")
    })
    .await;
}

/// The runs are the `reference_time` dimension of the Maps and Tiles
/// collection, each by the id EDR names its instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_extent_lists_the_runs_by_their_instance_ids() {
    let dir = tempfile::tempdir().unwrap();
    write_runs(dir.path());
    let engine = engine(dir.path());
    let expected = json!({
        "interval": [[OLD_RUN, NEW_RUN]],
        "description": "Model run (forecast reference time), as RFC 3339; the collection's EDR instance ids where it serves EDR",
        "definition": "https://vocab.nerc.ac.uk/standard_name/forecast_reference_time/",
        "grid": {"cellsCount": 2, "coordinates": [OLD_RUN, NEW_RUN]}
    });
    for app in [
        maps(engine.clone(), dir.path()),
        tiles(engine.clone(), dir.path()),
    ] {
        let collection = json_of(&get(&app, &format!("/collections/{ID}")).await);
        assert_eq!(collection["extent"]["reference_time"], expected);
    }
    let instances = json_of(
        &get(
            &edr(engine, dir.path()),
            &format!("/collections/{ID}/instances"),
        )
        .await,
    );
    let ids: Vec<&str> = instances["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, [OLD_RUN, NEW_RUN], "{instances}");
}
