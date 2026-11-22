//! A WMS EPSG:3857 view a whole turn of longitude away, a world copy, is
//! the same PNG as the view in place, byte for byte, through GetMap's
//! meta-tiled path (#1189), on real engines that draw every copy alike:
//!
//! - NetCDF: `testdata/netcdf-global`, a global 5° lat/lon grid, so a view
//!   across ±180° has data on both sides.
//! - QueryData: `testdata/meps`, MEPS on its Lambert conformal conic grid,
//!   the issue's collection.
//!
//! Each view is drawn on an odd ladder level, whose turn is not a whole
//! number of tiles, and on an even one, and its copies a turn and two turns
//! east and west. The synthetic twin, which also checks that a copy's tiles
//! are the view's a turn away, is ds-render's
//! `a_world_copy_assembles_the_pixels_of_the_view_in_place`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ds_core::config::CollectionConfig;
use ds_core::map_engine::MapEngine;
use ds_render::{ColorStop, LutColorMap, RenderedCache, StyleInfo, TilePixelCache};
use engine_netcdf::NetcdfEngine;
use engine_querydata::QueryDataEngine;

/// A turn of longitude in EPSG:3857 metres.
const TURN: f64 = 2.0 * std::f64::consts::PI * ds_core::web_mercator::EARTH_RADIUS;
/// The ground resolution of ladder level 0, zoom 0, in metres per pixel.
const Z0_RES: f64 = TURN / 256.0;
/// Width and height of every view.
const SIZE: u32 = 512;
/// 512 px of zoom 5, an even ladder step.
const ZOOM_5: f64 = 512.0 * Z0_RES / 32.0;

/// A current-thread runtime to serve requests on, as the other WMS suites
/// do; engines are built outside it, as the server builds them.
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

/// A grey ramp from black at `min` to white at `max`, in the served unit.
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

/// A WMS of the one collection `id`, meta-tiling on and the rendered cache
/// off, so every GetMap assembles its view from `tiles`.
struct Wms {
    app: axum::Router,
    tiles: Arc<TilePixelCache>,
}

fn wms(
    id: &str,
    engine_type: &str,
    engine: Arc<dyn MapEngine>,
    layer: &str,
    (min, max): (f64, f64),
) -> Wms {
    let collection: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "engine_type": engine_type, "apis": ["wms"],
    }))
    .unwrap();
    let tiles = Arc::new(TilePixelCache::new(64));
    let app = api_wms::router(Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines: HashMap::from([(id.to_string(), engine)]),
        point_engines: HashMap::new(),
        collections: HashMap::from([(id.to_string(), collection)]),
        styles: HashMap::from([
            (id.to_string(), style(min, max)),
            (format!("{id}/{layer}"), style(min, max)),
        ]),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(0)),
        tile_cache: tiles.clone(),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    Wms { app, tiles }
}

impl Wms {
    /// The PNG of a `SIZE`² GetMap of `layer` over the EPSG:3857 `bbox`,
    /// checked to be drawn and meta-tiled.
    async fn get_map(&self, layer: &str, [minx, miny, maxx, maxy]: [f64; 4]) -> Vec<u8> {
        let (hits, misses) = self.tiles.stats();
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={layer}&STYLES=\
                         &CRS=EPSG:3857&BBOX={minx},{miny},{maxx},{maxy}\
                         &WIDTH={SIZE}&HEIGHT={SIZE}&FORMAT=image/png&TRANSPARENT=TRUE"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let x_cache = response.headers()["x-cache"].to_str().unwrap().to_string();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(status, StatusCode::OK, "{layer} {minx},{maxx}");
        assert_eq!(
            x_cache, "MISS",
            "{layer} {minx},{maxx}: drawn, not the error tile"
        );
        let (now_hits, now_misses) = self.tiles.stats();
        assert!(
            now_hits + now_misses > hits + misses,
            "{layer} {minx},{maxx}: not meta-tiled"
        );
        body.to_vec()
    }
}

/// The RGBA pixels of a PNG GetMap returned, indexed or not.
fn decode(bytes: &[u8]) -> Vec<[u8; 4]> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let info = reader.info().clone();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut buf).unwrap();
    let used = &buf[..frame.buffer_size()];
    match frame.color_type {
        png::ColorType::Rgba => used.as_chunks::<4>().0.to_vec(),
        png::ColorType::Indexed => {
            let plte = info.palette.as_deref().unwrap();
            let trns = info.trns.as_deref().unwrap_or(&[]);
            used.iter()
                .map(|&i| {
                    let p = usize::from(i);
                    let alpha = trns.get(p).copied().unwrap_or(255);
                    [plte[p * 3], plte[p * 3 + 1], plte[p * 3 + 2], alpha]
                })
                .collect()
        }
        other => panic!("unexpected PNG colour type {other:?}"),
    }
}

/// The ladder level of a view: the coarsest half-octave step no coarser
/// than its resolution, as meta-tiling snaps it.
fn level(view: [f64; 4]) -> i32 {
    let res = (view[2] - view[0]) / f64::from(SIZE);
    (2.0 * (Z0_RES / res).log2() - 1e-6).ceil() as i32
}

/// Every view of `views` draws data, at the level it names, and its copies
/// a turn and two turns east and west are its PNG, byte for byte.
async fn copies_draw_the_view_in_place(wms: &Wms, layer: &str, views: &[(&str, [f64; 4], i32)]) {
    for &(case, view, expected_level) in views {
        assert_eq!(level(view), expected_level, "{layer} {case}");
        let in_place = wms.get_map(layer, view).await;
        let pixels = decode(&in_place);
        let drawn = pixels.iter().filter(|p| p[3] != 0).count();
        let colours = pixels
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(
            drawn > 50_000 && colours > 10,
            "{layer} {case}: {drawn} pixels drawn in {colours} colours"
        );
        for turns in [1.0, -1.0, 2.0, -2.0] {
            let [minx, miny, maxx, maxy] = view;
            let copy = [minx + turns * TURN, miny, maxx + turns * TURN, maxy];
            let bytes = wms.get_map(layer, copy).await;
            if bytes != in_place {
                let differ = decode(&bytes)
                    .iter()
                    .zip(&pixels)
                    .filter(|(a, b)| a != b)
                    .count();
                panic!("{layer} {case}, {turns} turns away: {differ} pixels differ");
            }
        }
    }
}

#[test]
fn a_world_copy_of_a_lat_lon_grid_is_the_view_in_place() {
    let config = serde_json::from_value(serde_json::json!({
        "data_path": testdata("netcdf-global").to_str().unwrap(),
        "variables": ["air_temperature"],
    }))
    .unwrap();
    let engine = Arc::new(NetcdfEngine::new("global", &config).unwrap());
    // Served in °C: 30 cos(lat) + 20 sin(lon) + the step.
    let wms = wms("global", "netcdf", engine, "air_temperature", (-30.0, 60.0));
    serve(async move {
        copies_draw_the_view_in_place(
            &wms,
            "global/air_temperature",
            &[
                ("off the ladder", [-4.0e6, -4.0e6, 4.0e6, 4.0e6], 7),
                (
                    "on zoom 5",
                    [-1.0e6, -1.0e6, -1.0e6 + ZOOM_5, -1.0e6 + ZOOM_5],
                    10,
                ),
                ("across 180°", [18.0e6, -3.0e6, 22.0e6, 1.0e6], 9),
                ("across -180°", [-22.0e6, -3.0e6, -18.0e6, 1.0e6], 9),
            ],
        )
        .await;
    });
}

#[test]
fn a_world_copy_of_a_lambert_grid_is_the_view_in_place() {
    let engine = Arc::new(QueryDataEngine::new(&testdata("meps"), "meps", None, 3600, 4).unwrap());
    // The default parameter, Temperature, in °C.
    let wms = wms("meps", "querydata", engine, "Temperature", (-20.0, 20.0));
    serve(async move {
        copies_draw_the_view_in_place(
            &wms,
            "meps",
            &[
                ("off the ladder", [0.6e6, 8.0e6, 2.6e6, 10.0e6], 11),
                (
                    "on zoom 6",
                    [0.6e6, 8.0e6, 0.6e6 + ZOOM_5 / 2.0, 8.0e6 + ZOOM_5 / 2.0],
                    12,
                ),
            ],
        )
        .await;
    });
}
