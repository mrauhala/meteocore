//! A wind speed derived from u/v components (#897, `ds_core::wind`) through
//! the WMS: its GetMap image is the colorized hypotenuse of the two
//! component tiles, on the direct and the meta-tiled render path, and the
//! direction is no layer.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::{
    MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use ds_core::model::{CoverageResponse, Location};
use ds_core::wind::{
    DerivedWind, GridAxes, OutcomeLog, ParameterFacts, VectorFrame, WindFacts, WindSource,
};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

/// u varies west to east, v north to south; every seventh pixel has no u.
fn u_at(i: usize, width: usize) -> Option<f64> {
    (!i.is_multiple_of(7)).then(|| ((i % width) as f64 / width as f64 - 0.5) * 50.0)
}

fn v_at(i: usize, width: usize, height: usize) -> Option<f64> {
    Some(((i / width) as f64 / height as f64 - 0.5) * 40.0)
}

fn info(parameters: &[&str]) -> RasterInfo {
    RasterInfo {
        native_crs: "CRS:84".into(),
        spatial_extent: Some([0.0, 40.0, 40.0, 80.0]),
        times: vec!["2026-10-01T00:00:00Z".parse().unwrap()],
        parameter: parameters[0].into(),
        unit: "m s-1".into(),
        parameters: parameters
            .iter()
            .map(|&p| ParameterInfo {
                name: p.into(),
                title: p.into(),
                unit: "m s-1".into(),
            })
            .collect(),
        vertical: None,
        grid_size: None,
        layer_subtitle: None,
        reference_times: Vec::new(),
    }
}

/// An NWP collection publishing wind only as earth-relative u and v.
struct Components;

impl MapEngine for Components {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let (w, h) = (width as usize, height as usize);
        let values = (0..w * h)
            .map(|i| match parameter {
                Some("u") => u_at(i, w),
                Some("v") => v_at(i, w, h),
                other => panic!("unexpected parameter {other:?}"),
            })
            .collect::<Vec<_>>();
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        info(&["u", "v"])
    }
}

impl EdrEngine for Components {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        Ok(Vec::new())
    }

    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        unreachable!()
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["u".into(), "v".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        None
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }
}

impl WindSource for Components {
    fn wind_facts(&self) -> Arc<WindFacts> {
        static FACTS: std::sync::LazyLock<Arc<WindFacts>> = std::sync::LazyLock::new(|| {
            let component = |name: &str, number| ParameterFacts {
                grib: Some((0, 2, number)),
                frame: VectorFrame::Earth,
                unit: "m s-1".into(),
                ..ParameterFacts::new(name)
            };
            Arc::new(WindFacts {
                grid: GridAxes::NorthAligned,
                parameters: vec![component("u", 2), component("v", 3)],
            })
        });
        FACTS.clone()
    }
}

/// The reference: a native speed field equal to the hypotenuse of the
/// same component fields, as a float engine serves it.
struct NativeSpeed;

impl MapEngine for NativeSpeed {
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
        let (w, h) = (width as usize, height as usize);
        let data = (0..w * h)
            .map(|i| match (u_at(i, w), v_at(i, w, h)) {
                (Some(u), Some(v)) => u.hypot(v) as f32,
                _ => f32::NAN,
            })
            .collect();
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 { data, nodata: None },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        info(&["ws"])
    }
}

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "apis": ["wms"], "engine_type": "grib",
    }))
    .unwrap()
}

/// The built-in `wind_speed` default: 0–40 m/s.
fn wind_speed_style() -> HashMap<String, StyleInfo> {
    HashMap::from([(
        "default".to_string(),
        StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            palette: ds_render::builtin_palette_arc("wind_speed").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::WindSpeed,
                0.0,
                40.0,
            )),
            min: 0.0,
            max: 40.0,
            parameter: None,
        },
    )])
}

fn router() -> axum::Router {
    let log: OutcomeLog = Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {});
    let derived: Arc<dyn MapEngine> = Arc::new(DerivedWind::new("uv", Arc::new(Components), log));
    let native: Arc<dyn MapEngine> = Arc::new(NativeSpeed);
    let mut styles = HashMap::new();
    for layer in ["uv", "uv/ws", "ref", "ref/ws"] {
        styles.insert(layer.to_string(), wind_speed_style());
    }
    api_wms::router(Arc::new(ArcSwap::from_pointee(WmsState {
        engines: HashMap::from([("uv".to_string(), derived), ("ref".to_string(), native)]),
        collections: HashMap::from([
            ("uv".to_string(), collection("uv")),
            ("ref".to_string(), collection("ref")),
        ]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, bytes::Bytes) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}

fn get_map(layer: &str, crs: &str, bbox: &str) -> String {
    format!(
        "/?SERVICE=WMS&REQUEST=GetMap&VERSION=1.3.0&LAYERS={layer}&STYLES=\
         &CRS={crs}&BBOX={bbox}&WIDTH=64&HEIGHT=64&FORMAT=image/png"
    )
}

/// Decode a PNG (indexed or truecolour, with or without alpha) to RGBA.
fn png_pixels(bytes: &[u8]) -> Vec<[u8; 4]> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png buffer size")];
    let frame = reader.next_frame(&mut buf).unwrap();
    let data = &buf[..frame.buffer_size()];
    match frame.color_type {
        png::ColorType::Rgba => data.as_chunks::<4>().0.to_vec(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        other => panic!("unexpected decoded PNG colour type: {other:?}"),
    }
}

#[tokio::test]
async fn a_derived_speed_tile_is_the_colorized_hypotenuse_of_its_components() {
    let app = router();
    // The direct path, and the meta-tiled one.
    for (crs, bbox) in [
        ("CRS:84", "0,40,40,80"),
        ("EPSG:3857", "0,4865942,4452780,13580978"),
    ] {
        let (status, derived) = get(&app, &get_map("uv/ws", crs, bbox)).await;
        assert_eq!(status, StatusCode::OK, "{crs}");
        let (status, native) = get(&app, &get_map("ref/ws", crs, bbox)).await;
        assert_eq!(status, StatusCode::OK, "{crs}");
        let (derived, native) = (png_pixels(&derived), png_pixels(&native));
        assert_eq!(derived, native, "{crs}");
        assert!(
            derived.iter().any(|p| p[3] == 0),
            "{crs}: nodata stays clear"
        );
        let colours: std::collections::HashSet<_> = derived.iter().collect();
        assert!(colours.len() > 10, "{crs}: a field, not a flat tile");
    }
}

#[tokio::test]
async fn the_speed_is_a_layer_and_the_direction_is_not() {
    let app = router();
    let (_, capabilities) = get(&app, "/?SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.3.0").await;
    let capabilities = String::from_utf8(capabilities.to_vec()).unwrap();
    assert!(
        capabilities.contains("<Name>uv/ws</Name>"),
        "{capabilities}"
    );
    assert!(!capabilities.contains("uv/wdir"));
    let (status, body) = get(&app, &get_map("uv/wdir", "CRS:84", "0,40,40,80")).await;
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("LayerNotDefined") && body.contains("Available: u, v, ws"),
        "{body}"
    );
}
