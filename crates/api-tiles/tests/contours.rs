//! Contour styles on map tiles (#1378): the built-in `contours` style of a
//! pressure parameter is listed with its parameters, its tiles meet across
//! tile edges (a 2 × 2 block of tiles is one tile of the zoom above drawn
//! at twice the size), the caches key on the style, and its legend
//! describes the lines.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_tiles::TilesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_mvt::VectorTileCache;
use ds_render::{RenderedCache, StyleContext};

const ID: &str = "nwp";

fn time() -> DateTime<Utc> {
    "2026-11-01T00:00:00Z".parse().unwrap()
}

/// A model on a 0.025° grid: mean sea-level pressure in hPa, 2 m
/// temperature in K and a wind speed, each an analytic field of the pixel
/// centre, mapped through the output CRS as every engine maps it.
struct Model;

fn pressure(lon: f64, lat: f64) -> f64 {
    1000.0 + 12.0 * (lon / 9.0).sin() + 10.0 * (lat / 7.0).cos() + 0.02 * lon * lat
}

impl MapEngine for Model {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let parameter = parameter.unwrap_or("msl");
        let mut data = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                let (lon, lat) = output_crs.project_node(
                    bbox,
                    (f64::from(x) + 0.5) / f64::from(width),
                    (f64::from(y) + 0.5) / f64::from(height),
                );
                let value = match parameter {
                    "msl" => pressure(lon, lat),
                    "2t" => 273.15 + 15.0 - 0.6 * lat.abs() + 3.0 * (lon / 5.0).sin(),
                    _ => 5.0 + (lon / 3.0).sin(),
                };
                data.push(value as f32);
            }
        }
        Ok(RasterTile {
            width,
            height,
            values: ds_core::map_engine::RasterValues::F32 { data, nodata: None },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([-180.0, -90.0, 180.0, 90.0]),
            times: vec![time()],
            parameter: "msl".into(),
            unit: "hPa".into(),
            parameters: params(),
            vertical: None,
            grid_size: Some([14_400, 7_200]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

fn params() -> Vec<ParameterInfo> {
    [
        ("msl", "Mean sea level pressure", "hPa"),
        ("2t", "2 metre temperature", "K"),
        ("10si", "10 metre wind speed", "m/s"),
    ]
    .map(|(name, title, unit)| ParameterInfo {
        name: name.into(),
        title: title.into(),
        unit: unit.into(),
    })
    .to_vec()
}

fn collection() -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": ID, "title": "NWP", "description": "A model",
        "apis": ["tiles"], "engine_type": "grib",
    }))
    .unwrap()
}

/// The router, with the style layers the server resolves for the
/// collection: its own and one per parameter, built-in defaults included.
fn app() -> axum::Router {
    let ctx = StyleContext::with_builtins();
    let collection = collection();
    let mut styles = ctx
        .parameter_layer_styles(&collection, None, &params(), &|_, cmap| cmap)
        .unwrap();
    styles.insert(
        ID.to_string(),
        ctx.collection_styles(&collection, None).unwrap(),
    );
    api_tiles::router(Arc::new(ArcSwap::from_pointee(TilesState {
        map_engines: HashMap::from([(ID.to_string(), Arc::new(Model) as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection)]),
        styles,
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        vector_tile_cache: Arc::new(VectorTileCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        discovery: Default::default(),
    })))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
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
    (status, x_cache, body.to_vec())
}

/// `(width, height, RGBA)` of a PNG, whichever colour type it was written in.
fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut buf).unwrap();
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    buf.truncate(frame.buffer_size());
    (frame.width, frame.height, buf)
}

fn tile_uri(style: &str, z: u32, row: u32, col: u32, extra: &str) -> String {
    format!(
        "/collections/{ID}/styles/{style}/tiles/WebMercatorQuad/{z}/{row}/{col}\
         ?parameter-name=msl&datetime=2026-11-01T00:00:00Z{extra}"
    )
}

/// The four zoom-3 tiles under zoom-2 tile (1, 2), stitched, are the zoom-2
/// tile drawn at 512 × 512: every isobar leaving one tile meets itself in
/// the next. The fill-only default draws other pixels, and each style keys
/// its own cache entry.
#[tokio::test]
async fn isobar_tiles_meet_across_tile_edges() {
    let app = app();
    let (status, _, whole) = get(
        &app,
        &tile_uri("contours", 2, 1, 2, "&width=512&height=512"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (w, h, whole) = decode(&whole);
    assert_eq!((w, h), (512, 512));

    let mut stitched = vec![0u8; whole.len()];
    for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        let (status, _, tile) = get(&app, &tile_uri("contours", 3, 2 + dr, 4 + dc, "")).await;
        assert_eq!(status, StatusCode::OK);
        let (tw, th, tile) = decode(&tile);
        assert_eq!((tw, th), (256, 256));
        for y in 0..256usize {
            let out = ((dr as usize * 256 + y) * 512 + dc as usize * 256) * 4;
            stitched[out..out + 256 * 4].copy_from_slice(&tile[y * 256 * 4..(y + 1) * 256 * 4]);
        }
    }
    let worst = whole
        .iter()
        .zip(&stitched)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(
        worst <= 1,
        "tiles differ from one render of the block by {worst}"
    );

    // Lines alone, crossing both seams.
    let drawn = |x: usize, y: usize| whole[(y * 512 + x) * 4 + 3] > 0;
    let lines = (0..512 * 512).filter(|&i| drawn(i % 512, i / 512)).count();
    assert!(
        lines > 2_000 && lines < 512 * 512 / 3,
        "{lines} line pixels"
    );
    let across = (0..512).filter(|&y| drawn(255, y) && drawn(256, y)).count();
    let down = (0..512).filter(|&x| drawn(x, 255) && drawn(x, 256)).count();
    assert!(
        across > 3 && down > 3,
        "{across} / {down} pixels on the seams"
    );

    // The default fill is another image under another key; a repeat of the
    // contour tile is a cache hit of the same bytes.
    let (_, _, fill) = get(&app, &tile_uri("default", 3, 2, 4, "")).await;
    let (_, x_cache, again) = get(&app, &tile_uri("contours", 3, 2, 4, "")).await;
    let (_, _, first) = get(&app, &tile_uri("contours", 3, 2, 4, "")).await;
    assert_eq!(x_cache, "HIT");
    assert_eq!(again, first);
    assert_ne!(decode(&fill).2, decode(&first).2);
    let (_, _, fill) = decode(&fill);
    assert!(
        fill.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
        "the fill is opaque"
    );
}

/// The collection lists `contours` after its own styles, with the
/// parameter layers that offer it; a parameter without contours (the wind
/// speed) is not among them. Its legend names the interval in the
/// parameter's unit.
#[tokio::test]
async fn contours_are_listed_with_their_parameters_and_legend() {
    let app = app();
    let (status, _, body) = get(&app, &format!("/collections/{ID}")).await;
    assert_eq!(status, StatusCode::OK);
    let collection: Value = serde_json::from_slice(&body).unwrap();
    let styles = collection["styles"].as_array().unwrap();
    assert_eq!(styles[0]["id"], "default");
    let contours = styles
        .iter()
        .find(|s| s["id"] == "contours")
        .expect("contours listed");
    assert_eq!(contours["parameters"], serde_json::json!(["msl", "2t"]));
    // Isobars on one layer, isotherms on the other: the list names it.
    assert_eq!(contours["title"], "contours");
    let legend = contours["links"][0]["href"].as_str().unwrap();
    assert!(
        legend.ends_with("/styles/contours/legend?parameter-name=msl"),
        "{legend}"
    );

    let (status, _, body) = get(
        &app,
        &format!("/collections/{ID}/styles/contours/legend?parameter-name=msl"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let legend: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(legend["title"], "Isobars");
    assert_eq!(legend["unit"], "hPa");
    assert_eq!(legend["contour"]["description"], "every 4 hPa");
    assert_eq!(legend["contour"]["interval"], 4.0);
    assert!(legend.get("stops").is_none());

    let (_, _, body) = get(
        &app,
        &format!("/collections/{ID}/styles/contours/legend?parameter-name=2t"),
    )
    .await;
    let legend: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        legend["contour"]["description"],
        "every 2 K through 273.15 K"
    );

    let (status, _, png) = get(
        &app,
        &format!("/collections/{ID}/styles/contours/legend?parameter-name=msl&f=png"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, pixels) = decode(&png);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[..3] == [0, 0, 0] && p[3] == 255)
            .count()
            > 24,
        "a black line swatch and its label"
    );

    // The wind speed has no contours.
    let (status, _, _) = get(
        &app,
        &tile_uri("contours", 3, 2, 4, "").replace("msl", "10si"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
