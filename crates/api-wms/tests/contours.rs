//! Contour styles in WMS (#1378): GetCapabilities lists a parameter
//! layer's built-in `contours` with its legend, GetLegendGraphic describes
//! the lines, and a meta-tiled view draws the lines a direct render of the
//! same view draws: the meta-tiles meet across their edges.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{
    MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use ds_render::{RenderedCache, StyleContext};

const ID: &str = "nwp";

fn time() -> DateTime<Utc> {
    "2026-11-01T00:00:00Z".parse().unwrap()
}

/// Mean sea-level pressure (hPa) and a wind speed, analytic fields of the
/// pixel centre, mapped through the output CRS as every engine maps it.
struct Model;

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
        let wind = parameter == Some("10si");
        let mut data = Vec::with_capacity(width as usize * height as usize);
        for y in 0..height {
            for x in 0..width {
                let (lon, lat) = output_crs.project_node(
                    bbox,
                    (f64::from(x) + 0.5) / f64::from(width),
                    (f64::from(y) + 0.5) / f64::from(height),
                );
                let value = if wind {
                    5.0 + (lon / 3.0).sin()
                } else {
                    1000.0 + 12.0 * (lon / 9.0).sin() + 10.0 * (lat / 7.0).cos() + 0.02 * lon * lat
                };
                data.push(value as f32);
            }
        }
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([-180.0, -90.0, 180.0, 90.0]),
            times: vec![time()],
            parameter: "msl".into(),
            unit: "hPa".into(),
            parameters: [
                ("msl", "Mean sea level pressure", "hPa"),
                ("10si", "10 metre wind speed", "m/s"),
            ]
            .map(|(name, title, unit)| ParameterInfo {
                name: name.into(),
                title: title.into(),
                unit: unit.into(),
            })
            .to_vec(),
            vertical: None,
            grid_size: Some([1440, 720]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

/// The WMS router over the model, with the style layers the server
/// resolves for it; `metatile_cache_mb = 0` renders every view directly.
fn app(metatile_cache_mb: u64) -> axum::Router {
    let collection: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "NWP", "description": "A model",
        "apis": ["wms"], "engine_type": "grib",
    }))
    .unwrap();
    let ctx = StyleContext::with_builtins();
    let info = Model.raster_info();
    let mut styles = ctx
        .parameter_layer_styles(&collection, None, &info.parameters, &|_, cmap| cmap)
        .unwrap();
    styles.insert(
        ID.to_string(),
        ctx.collection_styles(&collection, None).unwrap(),
    );
    api_wms::router(Arc::new(ArcSwap::from_pointee(WmsState {
        engines: HashMap::from([(ID.to_string(), Arc::new(Model) as Arc<dyn MapEngine>)]),
        point_engines: HashMap::new(),
        collections: HashMap::from([(ID.to_string(), collection)]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(0)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(metatile_cache_mb)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

async fn get(app: &axum::Router, query: &str) -> (StatusCode, String, Vec<u8>) {
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
    let status = response.status();
    let x_cache = response
        .headers()
        .get("x-cache")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, x_cache, body.to_vec())
}

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

/// The parameter layer offers `contours` titled for its quantity, with a
/// LegendURL; the wind speed layer does not; the collection's own layer
/// keeps its own styles.
#[tokio::test]
async fn capabilities_list_the_contour_style() {
    let (status, _, body) = get(&app(16), "REQUEST=GetCapabilities").await;
    assert_eq!(status, StatusCode::OK);
    let xml = String::from_utf8(body).unwrap();
    let layer = |name: &str| {
        let start = xml
            .find(&format!("<Name>{name}</Name>"))
            .unwrap_or_else(|| panic!("layer {name}"));
        let end = start + xml[start..].find("</Layer>").unwrap();
        xml[start..end].to_string()
    };
    let msl = layer("nwp/msl");
    assert!(
        msl.contains("<Style><Name>contours</Name><Title>Isobars</Title>"),
        "{msl}"
    );
    assert!(
        msl.contains("STYLE=contours"),
        "the style's LegendURL: {msl}"
    );
    assert!(!layer("nwp/10si").contains("<Name>contours</Name>"));
}

/// GetLegendGraphic describes the lines: the JSON's contour member names
/// the interval in hPa, the image is the line swatch.
#[tokio::test]
async fn legend_graphic_describes_the_lines() {
    let app = app(16);
    let (status, _, body) = get(
        &app,
        "REQUEST=GetLegendGraphic&LAYER=nwp/msl&STYLE=contours&FORMAT=application/json",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let legend: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(legend["style"], "contours");
    assert_eq!(legend["contour"]["description"], "every 4 hPa");
    assert_eq!(legend["contour"]["color"], "#000000");
    assert!(legend.get("stops").is_none());

    let (status, _, png) = get(
        &app,
        "REQUEST=GetLegendGraphic&LAYER=nwp/msl&STYLE=contours&FORMAT=image/png",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, pixels) = decode(&png);
    // White canvas, black swatch and text, and no colour bar.
    assert!(pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[..3] == [0, 0, 0]));
    assert!(pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|p| p[0] == p[1] && p[1] == p[2]));
}

/// A view on the Web Mercator meta-tile ladder (zoom 3, 2 × 2 meta-tiles)
/// draws the isobars a direct render of it draws, to within one level of
/// anti-aliasing: each meta-tile reads past its edges, so lines meet.
#[tokio::test]
async fn meta_tiled_isobars_match_a_direct_render() {
    let quarter = std::f64::consts::PI * ds_core::web_mercator::EARTH_RADIUS / 2.0;
    let query = format!(
        "REQUEST=GetMap&LAYERS=nwp/msl&STYLES=contours&CRS=EPSG:3857\
         &BBOX=0,0,{quarter},{quarter}&WIDTH=512&HEIGHT=512&FORMAT=image/png\
         &TRANSPARENT=TRUE&TIME=2026-11-01T00:00:00Z"
    );
    let (status, _, meta) = get(&app(64), &query).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, direct) = get(&app(0), &query).await;
    assert_eq!(status, StatusCode::OK);
    let ((w, h, meta), (_, _, direct)) = (decode(&meta), decode(&direct));
    assert_eq!((w, h), (512, 512));
    let worst = meta
        .iter()
        .zip(&direct)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(
        worst <= 1,
        "meta-tiles differ from the direct render by {worst}"
    );
    let drawn = |x: usize, y: usize| meta[(y * 512 + x) * 4 + 3] > 0;
    let across = (0..512).filter(|&y| drawn(255, y) && drawn(256, y)).count();
    assert!(across > 3, "{across} line pixels on the meta-tile seam");

    // The default fill of the same view is another image.
    let (_, _, fill) = get(&app(64), &query.replace("STYLES=contours", "STYLES=")).await;
    assert_ne!(decode(&fill).2, meta);
}

/// The render outcome of a GetMap: meta-tiled views whose tiles are all
/// cached are `Assembled`, direct renders always `Cold`.
async fn outcome(app: &axum::Router, query: &str) -> ds_executor::RenderOutcome {
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
    response
        .extensions()
        .get::<ds_executor::RenderTiming>()
        .expect("a render timing")
        .outcome
}

/// Off the meta-tile ladder, a view would be assembled from tiles drawn at
/// the finer step and downsampled by 1.1 to 1.41, nearest, losing line
/// pixels. Such a contour view is rendered directly instead: every pixel a
/// direct render covers fully stays covered. On a step, contours stay
/// meta-tiled.
#[tokio::test]
async fn contours_off_the_ladder_render_directly() {
    let level_6 = std::f64::consts::PI * ds_core::web_mercator::EARTH_RADIUS / 2.0 / 512.0;
    let query = |factor: f64| {
        let side = 512.0 * level_6 * factor;
        format!(
            "REQUEST=GetMap&LAYERS=nwp/msl&STYLES=contours&CRS=EPSG:3857\
             &BBOX=0,0,{side},{side}&WIDTH=512&HEIGHT=512&FORMAT=image/png\
             &TRANSPARENT=TRUE&TIME=2026-11-01T00:00:00Z"
        )
    };
    for factor in [1.1, 1.2, 1.41] {
        let (status, _, meta) = get(&app(64), &query(factor)).await;
        assert_eq!(status, StatusCode::OK);
        let (_, _, direct) = get(&app(0), &query(factor)).await;
        let ((_, _, meta), (_, _, direct)) = (decode(&meta), decode(&direct));
        let faded = direct
            .as_chunks::<4>()
            .0
            .iter()
            .zip(meta.as_chunks::<4>().0)
            .filter(|(d, m)| d[3] == 255 && m[3] < 255)
            .count();
        assert_eq!(faded, 0, "factor {factor}: {faded} core line pixels faded");
        let worst = meta
            .iter()
            .zip(&direct)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(
            worst <= 1,
            "factor {factor}: differs from the direct render by {worst}"
        );
        // Nothing to reuse: every repeat reads the engine again.
        let app = app(64);
        get(&app, &query(factor)).await;
        assert_eq!(
            outcome(&app, &query(factor)).await,
            ds_executor::RenderOutcome::Cold
        );
    }
    // On the step the second view is assembled from cached meta-tiles.
    let app = app(64);
    get(&app, &query(1.0)).await;
    assert_eq!(
        outcome(&app, &query(1.0)).await,
        ds_executor::RenderOutcome::Assembled
    );
}
