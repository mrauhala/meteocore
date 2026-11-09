//! Contour styles in OGC API - Maps (#1378): the built-in `contours` of a
//! pressure parameter is in the collection's style lists with the
//! parameters offering it and links naming one, its map draws the lines
//! alone, and its legend describes them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_maps::MapsState;
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

/// Mean sea-level pressure (hPa) and a wind speed as analytic fields.
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
                    1000.0 + 12.0 * (lon / 9.0).sin() + 10.0 * (lat / 7.0).cos()
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

fn app() -> axum::Router {
    let collection: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "NWP", "description": "A model",
        "apis": ["maps"], "engine_type": "grib",
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
    api_maps::router(Arc::new(ArcSwap::from_pointee(MapsState {
        engines: HashMap::from([(ID.to_string(), Arc::new(Model) as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection)]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: HashSet::new(),
        discovery: Default::default(),
    })))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

fn decode(png_bytes: &[u8]) -> Vec<u8> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let frame = reader.next_frame(&mut buf).unwrap();
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    buf.truncate(frame.buffer_size());
    buf
}

/// `…/styles` and the collection both list `contours` after the
/// collection's own styles, with the parameter offering it and map and
/// legend links naming it; the links resolve.
#[tokio::test]
async fn style_lists_name_contours_and_their_parameters() {
    let app = app();
    for uri in [
        format!("/collections/{ID}/styles"),
        format!("/collections/{ID}"),
    ] {
        let (status, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let doc: Value = serde_json::from_slice(&body).unwrap();
        let styles = doc["styles"].as_array().unwrap();
        assert_eq!(styles[0]["id"], "default", "{uri}");
        assert!(styles[0].get("parameters").is_none());
        let contours = styles
            .iter()
            .find(|s| s["id"] == "contours")
            .unwrap_or_else(|| panic!("{uri} lists contours"));
        assert_eq!(contours["title"], "Isobars");
        assert_eq!(contours["parameters"], serde_json::json!(["msl"]));
        let hrefs: Vec<&str> = contours["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["href"].as_str().unwrap())
            .collect();
        assert!(
            hrefs[0].ends_with("/collections/nwp/styles/contours/map?parameter-name=msl"),
            "{hrefs:?}"
        );
        assert!(hrefs[1].ends_with("/styles/contours/legend?parameter-name=msl"));
    }
}

/// The map draws lines alone (transparent between them, opaque on them)
/// where the default draws an opaque fill; the legend describes them.
#[tokio::test]
async fn contour_maps_and_legends() {
    let app = app();
    let map = |style: &str| {
        format!(
            "/collections/{ID}/styles/{style}/map?parameter-name=msl&bbox=0,40,40,70\
             &width=400&height=300&crs=CRS84&bbox-crs=CRS84"
        )
    };
    let (status, png) = get(&app, &map("contours")).await;
    assert_eq!(status, StatusCode::OK);
    let lines = decode(&png);
    let alpha: Vec<u8> = lines.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
    let drawn = alpha.iter().filter(|&&a| a > 0).count();
    assert!(
        drawn > 1_000 && drawn < alpha.len() / 4,
        "{drawn} line pixels"
    );
    assert!(alpha.contains(&255) && alpha.contains(&0));
    let (_, png) = get(&app, &map("default")).await;
    assert!(decode(&png).as_chunks::<4>().0.iter().all(|p| p[3] == 255));

    let (status, body) = get(
        &app,
        &format!("/collections/{ID}/styles/contours/legend?parameter-name=msl"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let legend: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(legend["contour"]["description"], "every 4 hPa");
    assert_eq!(legend["parameter"], "msl");
    let (status, png) = get(
        &app,
        &format!("/collections/{ID}/styles/contours/legend?parameter-name=msl&f=png"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(decode(&png)
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[..3] == [0, 0, 0]));

    // The wind speed offers no contours.
    let (status, _) = get(&app, &map("contours").replace("msl", "10si")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The `(time, reference_time)` a read asks for.
type Frame = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// A model with two runs, each with its own steps, recording the frame
/// every read asks for.
struct Runs {
    calls: std::sync::Mutex<Vec<Frame>>,
}

fn run(hour: i64) -> DateTime<Utc> {
    time() + chrono::Duration::hours(hour)
}

impl MapEngine for Runs {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.calls.lock().unwrap().push((time, reference_time));
        Model.get_raster_tile(
            bbox,
            width,
            height,
            time,
            output_crs,
            parameter,
            z,
            reference_time,
        )
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            times: vec![run(0), run(3), run(6), run(9)],
            reference_times: vec![run(0), run(6)],
            ..Model.raster_info()
        }
    }

    fn run_times(
        &self,
        _: Option<&str>,
        reference_time: DateTime<Utc>,
    ) -> Option<Arc<[DateTime<Utc>]>> {
        Some(Arc::from(vec![
            reference_time,
            reference_time + chrono::Duration::hours(3),
        ]))
    }

    fn resolve_reference_time(
        &self,
        _: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        reference_time.or(Some(run(6)))
    }
}

/// A contour map of a named run reads the frame the fill reads: the run
/// and that run's step, resolved before keying (#1450), never the latest.
#[tokio::test]
async fn contours_draw_the_named_run_as_the_fill_does() {
    let collection: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "NWP", "description": "A model",
        "apis": ["maps"], "engine_type": "grib",
    }))
    .unwrap();
    let ctx = StyleContext::with_builtins();
    let engine = Arc::new(Runs {
        calls: Default::default(),
    });
    let info = engine.raster_info();
    let mut styles = ctx
        .parameter_layer_styles(&collection, None, &info.parameters, &|_, cmap| cmap)
        .unwrap();
    styles.insert(
        ID.to_string(),
        ctx.collection_styles(&collection, None).unwrap(),
    );
    let app = api_maps::router(Arc::new(ArcSwap::from_pointee(MapsState {
        engines: HashMap::from([(ID.to_string(), engine.clone() as Arc<dyn MapEngine>)]),
        collections: HashMap::from([(ID.to_string(), collection)]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(0)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: HashSet::new(),
        discovery: Default::default(),
    })));
    let named = r#"&subset=reference_time("2026-11-01T00:00:00Z")"#;
    for (extra, run_drawn) in [
        (named.to_string(), Some(run(0))),
        (
            format!("{named}&datetime=2026-11-01T03:00:00Z"),
            Some(run(0)),
        ),
        (String::new(), Some(run(6))),
    ] {
        let mut frames = Vec::new();
        for style in ["default", "contours"] {
            let uri = format!(
                "/collections/{ID}/styles/{style}/map?parameter-name=msl&bbox=0,40,40,70\
                 &width=64&height=48{}",
                extra.replace('"', "%22")
            );
            let (status, _) = get(&app, &uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            frames.push(*engine.calls.lock().unwrap().last().unwrap());
        }
        assert_eq!(
            frames[0], frames[1],
            "contours and fill read one frame: {extra}"
        );
        assert_eq!(frames[1].1, run_drawn, "{extra}");
    }
    assert_eq!(engine.calls.lock().unwrap()[3].0, Some(run(3)));
}
