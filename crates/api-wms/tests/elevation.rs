//! WMS `ELEVATION` between a layer's levels (#1477). Capabilities declare
//! every elevation dimension `nearestValue="1"`, so WMS 1.3.0 C.4.3 makes
//! the server draw the nearest level, "rounded, not merely truncated".
//! The engine names the level it draws (`MapEngine::resolve_elevation`);
//! WMS resolves each layer's level after its run and time, passes that
//! level to the render, and keys the rendered and meta-tile caches on it,
//! as #507 did for `TIME`. An `ELEVATION` the engine has no level for is
//! `InvalidDimensionValue`, never the error tile.
//!
//! The engine here draws only its own levels exactly and fails any other
//! `z`, so a request reaching it with the `ELEVATION` as sent would be an
//! error tile. The real engines, NetCDF, GRIB and an ODIM volume, are
//! `crates/server/tests/wms_elevation.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_core::model::{
    CoverageResponse, DomainDescription, LocationSnapshot, NdArray, ParameterDescription,
    QueryResult, ValuePrecision, VerticalCoord,
};
use ds_core::vertical::{VerticalDimension, VerticalKind};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

/// The one valid time.
const TIME: &str = "2026-10-01T00:00:00Z";

/// A parameter the engine has no level to draw at for any `ELEVATION`, as
/// a radar volume none of whose sweeps carries a quantity.
const NO_LEVEL: &str = "nolevel";

/// A pressure-level collection on `levels`, bottom first, drawing each
/// level as one value, `level / 1000`, and only its own levels: any other
/// `z` fails. It selects the nearest level through the shared rule, as
/// NetCDF and GRIB do. Counts its renders and records each `z` it is
/// passed and each position query's.
struct Levels {
    levels: Vec<f64>,
    renders: AtomicUsize,
    drawn: Mutex<Vec<Option<f64>>>,
    queried: Mutex<Vec<Option<Vec<f64>>>>,
}

impl Levels {
    fn new(levels: &[f64]) -> Arc<Self> {
        Arc::new(Self {
            levels: levels.to_vec(),
            renders: AtomicUsize::new(0),
            drawn: Mutex::new(Vec::new()),
            queried: Mutex::new(Vec::new()),
        })
    }

    fn renders(&self) -> usize {
        self.renders.load(Ordering::Relaxed)
    }

    fn drawn(&self) -> Vec<Option<f64>> {
        self.drawn.lock().unwrap().clone()
    }
}

impl MapEngine for Levels {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.renders.fetch_add(1, Ordering::Relaxed);
        self.drawn.lock().unwrap().push(z);
        let level = match z {
            None => self.levels[0],
            Some(z) if parameter != Some(NO_LEVEL) && self.levels.contains(&z) => z,
            Some(z) => {
                return Err(DataServerError::InvalidParameter(format!(
                    "{z} is not a level"
                )))
            }
        };
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(level / 1000.0); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let parameter = |name: &str| ParameterInfo {
            name: name.into(),
            title: name.into(),
            unit: "K".into(),
        };
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([-20.0, 30.0, 40.0, 80.0]),
            times: vec![TIME.parse().unwrap()],
            parameter: "t".into(),
            unit: "K".into(),
            parameters: vec![parameter("t"), parameter(NO_LEVEL)],
            vertical: Some(VerticalDimension::new(
                VerticalKind::Pressure,
                self.levels.clone(),
            )),
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }

    /// The level `get_raster_tile` would draw if it drew the nearest: the
    /// level nearest `z`, else the bottom one; none for [`NO_LEVEL`].
    fn resolve_elevation(
        &self,
        parameter: Option<&str>,
        z: Option<f64>,
        _time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<f64> {
        if parameter == Some(NO_LEVEL) {
            return None;
        }
        match z {
            None => self.levels.first().copied(),
            Some(z) => ds_core::vertical::nearest_level(&self.levels, z).map(|i| self.levels[i]),
        }
    }
}

impl EdrEngine for Levels {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }

    fn query_location(
        &self,
        _location_id: &str,
        _datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound("none".into()))
    }

    fn get_parameters(&self) -> Vec<String> {
        vec!["t".into()]
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let time = TIME.parse().unwrap();
        Some((time, time))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([-20.0, 30.0, 40.0, 80.0])
    }

    fn supported_query_types(&self) -> Vec<String> {
        vec!["position".into()]
    }

    /// The level as the value, at a level the engine has, exactly.
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _parameters: Option<&[String]>,
        z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        self.queried.lock().unwrap().push(z.map(<[f64]>::to_vec));
        let level = z.map_or(self.levels[0], |z| z[0]);
        if !self.levels.contains(&level) {
            return Err(DataServerError::InvalidParameter(format!(
                "{level} is not a level"
            )));
        }
        let (lat, lon) = ds_core::feature::parse_point_coords(coords)?;
        Ok(CoverageResponse::Single(QueryResult {
            domain: DomainDescription::PointSeries {
                x: lon,
                y: lat,
                t: vec![datetime.map_or(TIME.parse().unwrap(), |(start, _)| start)],
                z: Some(VerticalCoord {
                    kind: VerticalKind::Pressure,
                    values: vec![level],
                }),
            },
            parameters: HashMap::from([(
                "t".to_string(),
                ParameterDescription {
                    label: "Temperature".into(),
                    unit: "K".into(),
                    observed_property: "t".into(),
                    standard_name: None,
                },
            )]),
            ranges: HashMap::from([(
                "t".to_string(),
                NdArray {
                    shape: vec![1],
                    axis_names: vec!["t".into()],
                    values: vec![Some(level)],
                    precision: ValuePrecision::F64,
                },
            )]),
        }))
    }
}

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "apis": ["wms"], "engine_type": "netcdf",
    }))
    .unwrap()
}

fn style() -> HashMap<String, StyleInfo> {
    HashMap::from([(
        "default".to_string(),
        StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Viridis,
                0.0,
                1.0,
            )),
            min: 0.0,
            max: 1.0,
            parameter: None,
            contour: None,
        },
    )])
}

/// A WMS of `p`, on 1000, 850, 500 and 300 hPa, and `q`, on 700 and
/// 500 hPa, both answering position queries.
fn app() -> (axum::Router, Arc<Levels>, Arc<Levels>) {
    let p = Levels::new(&[1000.0, 850.0, 500.0, 300.0]);
    let q = Levels::new(&[700.0, 500.0]);
    let mut styles = HashMap::new();
    for layer in ["p", "p/t", "p/nolevel", "q", "q/t", "q/nolevel"] {
        styles.insert(layer.to_string(), style());
    }
    let app = api_wms::router(Arc::new(ArcSwap::from_pointee(WmsState {
        engines: HashMap::from([
            ("p".to_string(), p.clone() as Arc<dyn MapEngine>),
            ("q".to_string(), q.clone() as Arc<dyn MapEngine>),
        ]),
        point_engines: HashMap::from([
            ("p".to_string(), p.clone() as Arc<dyn EdrEngine>),
            ("q".to_string(), q.clone() as Arc<dyn EdrEngine>),
        ]),
        collections: HashMap::from([
            ("p".to_string(), collection("p")),
            ("q".to_string(), collection("q")),
        ]),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    (app, p, q)
}

struct Reply {
    status: StatusCode,
    x_cache: String,
    etag: String,
    cache_control: String,
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
    let (status, x_cache, etag, cache_control) = (
        response.status(),
        header("x-cache"),
        header("etag"),
        header("cache-control"),
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        x_cache,
        etag,
        cache_control,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// The `code` of a ServiceExceptionReport.
fn exception_code(reply: &Reply) -> &str {
    let code = reply
        .body
        .split_once("<ServiceException code=\"")
        .unwrap_or_else(|| panic!("no ServiceException: {}", reply.body))
        .1;
    &code[..code.find('"').unwrap()]
}

/// The direct path and the meta-tiled one.
const VIEWS: [(&str, &str); 2] = [
    ("CRS:84", "10,55,30,70"),
    ("EPSG:3857", "1113194,7361866,3339584,11068715"),
];

fn get_map(layers: &str, crs: &str, bbox: &str, extra: &str) -> String {
    format!(
        "REQUEST=GetMap&LAYERS={layers}&STYLES=&CRS={crs}&BBOX={bbox}&WIDTH=64&HEIGHT=64\
         &TIME={TIME}{extra}"
    )
}

/// `ELEVATION=851` draws 850 hPa, from the rendered entry `ELEVATION=850`
/// filled, with its ETag, and the engine is not read again; so do the
/// other values nearer 850 hPa than any other level. Midway between 1000
/// and 850 hPa is the bottom level, the one drawn without `ELEVATION`. The
/// engine is only ever passed its own levels. On both render paths.
#[tokio::test]
async fn an_elevation_between_levels_draws_and_keys_the_nearest_level() {
    for (crs, bbox) in VIEWS {
        let (app, p, _) = app();
        let map = |extra: &str| get_map("p/t", crs, bbox, &format!("&FORMAT=image/png{extra}"));
        let at_850 = get(&app, &map("&ELEVATION=850")).await;
        assert_eq!(at_850.status, StatusCode::OK, "{crs}: {}", at_850.body);
        assert_eq!(at_850.x_cache, "MISS", "{crs}");
        assert!(
            at_850.cache_control.contains("immutable"),
            "{crs}: {}",
            at_850.cache_control
        );
        let renders = p.renders();
        assert!(renders > 0);
        for z in ["851", "849.5", "924", "700"] {
            let reply = get(&app, &map(&format!("&ELEVATION={z}"))).await;
            assert_eq!(reply.status, StatusCode::OK, "{crs} {z}");
            assert_eq!(reply.x_cache, "HIT", "{crs} {z}");
            assert_eq!(reply.etag, at_850.etag, "{crs} {z}");
            // Another level may land under this URL (a GRIB run still
            // filling in): no fixed frame.
            assert!(
                !reply.cache_control.contains("immutable"),
                "{crs} {z}: {}",
                reply.cache_control
            );
        }
        assert_eq!(p.renders(), renders, "{crs}: the engine is not read again");

        let tie = get(&app, &map("&ELEVATION=925")).await;
        assert_eq!(tie.x_cache, "MISS", "{crs}");
        assert_ne!(tie.etag, at_850.etag, "{crs}");
        let renders = p.renders();
        for extra in ["", "&ELEVATION=1000", "&ELEVATION=1100"] {
            let reply = get(&app, &map(extra)).await;
            assert_eq!(
                (reply.x_cache.as_str(), reply.etag.as_str()),
                ("HIT", tie.etag.as_str()),
                "{crs} {extra:?}"
            );
        }
        assert_eq!(p.renders(), renders, "{crs}");
        assert!(
            p.drawn()
                .iter()
                .all(|z| matches!(z, Some(z) if *z == 850.0 || *z == 1000.0)),
            "{crs}: {:?}",
            p.drawn()
        );
    }
}

/// The meta-tiles are keyed on the level drawn too: a JPEG at 851 hPa,
/// another rendered entry than the PNG at 850 hPa, is assembled from the
/// PNG's tiles without an engine read.
#[tokio::test]
async fn meta_tiles_are_shared_by_the_elevations_drawing_one_level() {
    let (crs, bbox) = VIEWS[1];
    let (app, p, _) = app();
    let png = get(
        &app,
        &get_map("p/t", crs, bbox, "&FORMAT=image/png&ELEVATION=850"),
    )
    .await;
    assert_eq!(png.status, StatusCode::OK, "{}", png.body);
    let renders = p.renders();
    let jpeg = get(
        &app,
        &get_map("p/t", crs, bbox, "&FORMAT=image/jpeg&ELEVATION=851"),
    )
    .await;
    assert_eq!(jpeg.status, StatusCode::OK, "{}", jpeg.body);
    assert_eq!(jpeg.x_cache, "MISS", "another rendered entry");
    assert_eq!(p.renders(), renders, "the 850 hPa meta-tiles");
    let top = get(
        &app,
        &get_map("p/t", crs, bbox, "&FORMAT=image/jpeg&ELEVATION=310"),
    )
    .await;
    assert_eq!(top.status, StatusCode::OK);
    assert!(p.renders() > renders, "300 hPa is tiles of its own");
}

/// Each listed layer draws the nearest of its own levels: at 800 hPa `p`
/// draws 850 and `q` 700; at 600 hPa `p` draws 500, and `q`, midway between
/// its levels, the bottom one, 700.
#[tokio::test]
async fn each_listed_layer_draws_the_nearest_of_its_own_levels() {
    for (crs, bbox) in VIEWS {
        let (app, p, q) = app();
        for (z, at_p, at_q) in [(800.0, 850.0, 700.0), (600.0, 500.0, 700.0)] {
            let reply = get(
                &app,
                &get_map(
                    "p/t,q/t",
                    crs,
                    bbox,
                    &format!("&FORMAT=image/png&ELEVATION={z}"),
                ),
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK, "{crs}: {}", reply.body);
            assert_ne!(reply.x_cache, "ERROR", "{crs} {z}");
            assert_eq!(p.drawn().last(), Some(&Some(at_p)), "{crs} {z}");
            assert_eq!(q.drawn().last(), Some(&Some(at_q)), "{crs} {z}");
        }
    }
}

/// An `ELEVATION` the engine has no level to draw at is the exception
/// `InvalidDimensionValue`, in GetMap and GetFeatureInfo, alone or in a
/// list, never the error tile, and nothing is read. Without `ELEVATION`
/// the layer draws its default.
#[tokio::test]
async fn an_elevation_without_a_level_to_draw_is_invalid_dimension_value() {
    let (app, p, _) = app();
    let (crs, bbox) = VIEWS[0];
    for layers in ["p/nolevel", "p/t,p/nolevel"] {
        let reply = get(
            &app,
            &get_map(layers, crs, bbox, "&FORMAT=image/png&ELEVATION=850"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{layers}");
        assert_eq!(exception_code(&reply), "InvalidDimensionValue", "{layers}");
        // The layer, and what it has no level of: the parameter, at the
        // frame drawn.
        assert!(reply.body.contains("p/nolevel"), "{}", reply.body);
        assert!(
            reply.body.contains("nolevel")
                && reply.body.contains("has none at 2026-10-01T00:00:00Z"),
            "{}",
            reply.body
        );
    }
    assert_eq!(p.renders(), 0);
    let gfi = get(
        &app,
        "REQUEST=GetFeatureInfo&LAYERS=p/nolevel&QUERY_LAYERS=p/nolevel&STYLES=&CRS=CRS:84\
         &BBOX=10,55,30,70&WIDTH=20&HEIGHT=10&I=5&J=5&INFO_FORMAT=application/json\
         &ELEVATION=850",
    )
    .await;
    assert_eq!(gfi.status, StatusCode::BAD_REQUEST, "{}", gfi.body);
    assert_eq!(exception_code(&gfi), "InvalidDimensionValue");
    assert!(p.queried.lock().unwrap().is_empty());

    let default = get(&app, &get_map("p/nolevel", crs, bbox, "&FORMAT=image/png")).await;
    assert_eq!(default.status, StatusCode::OK);
    assert_eq!(default.x_cache, "MISS");
}

/// GetFeatureInfo at `ELEVATION=851` queries and reports 850 hPa, the
/// level GetMap draws there, and answers 850 hPa's value; without
/// `ELEVATION`, the bottom level.
#[tokio::test]
async fn get_feature_info_between_levels_reads_the_level_drawn() {
    let (app, p, _) = app();
    for (extra, level) in [
        ("&ELEVATION=851", 850.0),
        ("&ELEVATION=850", 850.0),
        ("&ELEVATION=925", 1000.0),
        ("&ELEVATION=10", 300.0),
        ("", 1000.0),
    ] {
        let reply = get(
            &app,
            &format!(
                "REQUEST=GetFeatureInfo&LAYERS=p/t&QUERY_LAYERS=p/t&STYLES=&CRS=CRS:84\
                 &BBOX=10,55,30,70&WIDTH=20&HEIGHT=10&I=5&J=5\
                 &INFO_FORMAT=application/json{extra}"
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{extra}: {}", reply.body);
        let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let properties = &body["features"][0]["properties"];
        assert_eq!(properties["elevation"], level, "{extra}");
        assert_eq!(properties["value"], level, "{extra}");
        assert_eq!(
            p.queried.lock().unwrap().pop(),
            Some(Some(vec![level])),
            "{extra}"
        );
    }
}
