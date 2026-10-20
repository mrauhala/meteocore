//! WMS GetMap with several `LAYERS` (#1181): each layer drawn with its
//! style through the path one layer takes, bottom first (WMS 1.3.0
//! §7.3.3.3), `TRANSPARENT` and `BGCOLOR` applied once to the whole image;
//! `STYLES` pairing; the dimension rules; meta-tile reuse; the render
//! budget and deadline; and a single layer's image unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{
    CompositeChannel, CompositeDef, MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile,
};
use ds_core::vertical::{VerticalDimension, VerticalKind};
use ds_render::{ColorMap, ImageFormat, RenderedCache, StyleInfo};

fn at(time: &str) -> DateTime<Utc> {
    time.parse().unwrap()
}

const T0: &str = "2026-10-01T00:00:00Z";
const T1: &str = "2026-10-01T01:00:00Z";
const T2: &str = "2026-10-01T02:00:00Z";
/// Model runs, ascending.
const R0: &str = "2026-09-30T12:00:00Z";
const R1: &str = "2026-10-01T00:00:00Z";

const BLUE: [u8; 4] = [0, 0, 255, 255];
const YELLOW: [u8; 4] = [255, 255, 0, 255];
const RED_HALF: [u8; 4] = [255, 0, 0, 128];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const GREY: [u8; 4] = [128, 128, 128, 255];

/// How long a `slow` layer's read takes.
const SLOW: std::time::Duration = std::time::Duration::from_millis(250);

/// A colormap painting every value one colour, nodata transparent.
struct Solid([u8; 4]);

impl ColorMap for Solid {
    fn color(&self, value: Option<f64>) -> [u8; 4] {
        match value {
            Some(v) if v.is_finite() => self.0,
            _ => [0; 4],
        }
    }
}

/// What one `get_raster_tile` call asked for.
#[derive(Debug, Clone, PartialEq)]
struct Call {
    parameter: Option<String>,
    time: Option<DateTime<Utc>>,
    z: Option<f64>,
    reference_time: Option<DateTime<Utc>>,
}

/// How an engine answers.
#[derive(Clone, Copy)]
enum Answer {
    Data,
    Fails,
    Busy,
}

/// A layer's data: 1.0 at the pixels whose centre lies west of `west_of`
/// and east of `east_of` degrees of longitude (each bound optional), nodata
/// elsewhere. Output pixels are uniform in longitude in CRS:84 and
/// EPSG:3857, the CRSs these tests draw in. Records every call.
struct Region {
    west_of: Option<f64>,
    east_of: Option<f64>,
    times: Vec<DateTime<Utc>>,
    /// Per-parameter time axes (#819): `(parameter, times)`.
    parameter_times: Vec<(&'static str, Vec<DateTime<Utc>>)>,
    vertical: bool,
    runs: Vec<DateTime<Utc>>,
    composite: bool,
    answer: Answer,
    /// How long each `get_raster_tile` call takes.
    delay: std::time::Duration,
    calls: Mutex<Vec<Call>>,
}

impl Region {
    fn new() -> Self {
        Self {
            west_of: None,
            east_of: None,
            times: vec![at(T0)],
            parameter_times: Vec::new(),
            vertical: false,
            runs: Vec::new(),
            composite: false,
            answer: Answer::Data,
            delay: std::time::Duration::ZERO,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn last_call(&self) -> Call {
        self.calls().pop().expect("the engine was asked")
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl MapEngine for Region {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        _output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.calls.lock().unwrap().push(Call {
            parameter: parameter.map(str::to_string),
            time,
            z,
            reference_time,
        });
        std::thread::sleep(self.delay);
        match self.answer {
            Answer::Data => {}
            Answer::Fails => return Err(DataServerError::Engine("source unreachable".into())),
            Answer::Busy => return Err(DataServerError::ResourceExhausted),
        }
        let [west, _, east, _] = bbox;
        let mut values = Vec::with_capacity((width * height) as usize);
        for _ in 0..height {
            for x in 0..width {
                let lon = west + (f64::from(x) + 0.5) * (east - west) / f64::from(width);
                let inside =
                    self.west_of.is_none_or(|w| lon < w) && self.east_of.is_none_or(|e| lon > e);
                values.push(inside.then_some(1.0));
            }
        }
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        let parameters = self
            .parameter_times
            .iter()
            .map(|(name, _)| ParameterInfo {
                name: (*name).into(),
                title: (*name).into(),
                unit: "1".into(),
            })
            .collect();
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([-180.0, -85.0, 180.0, 85.0]),
            times: self.times.clone(),
            parameter: "value".into(),
            unit: "1".into(),
            parameters,
            vertical: self
                .vertical
                .then(|| VerticalDimension::new(VerticalKind::Pressure, vec![850.0, 500.0])),
            grid_size: None,
            layer_subtitle: None,
            reference_times: self.runs.clone(),
            native_grid: None,
        }
    }

    fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
        self.parameter_times
            .iter()
            .find(|(name, _)| *name == parameter)
            .map(|(_, times)| Arc::from(times.clone()))
    }

    /// Latest-not-after on the parameter's own axis, else the collection's.
    fn resolve_parameter_time(
        &self,
        parameter: Option<&str>,
        time: Option<DateTime<Utc>>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        let times = parameter
            .and_then(|p| self.parameter_times(p))
            .map_or_else(|| self.times.clone(), |times| times.to_vec());
        match time {
            None => times.last().copied(),
            Some(time) if times.is_empty() => Some(time),
            Some(time) => times.into_iter().rev().find(|&t| t <= time),
        }
    }

    fn composites(&self) -> Arc<[CompositeDef]> {
        if !self.composite {
            return Arc::from([]);
        }
        let channel = |min: f64| CompositeChannel {
            parameter: "value".into(),
            minus: None,
            min,
            max: 2.0,
            gamma: 1.0,
        };
        Arc::from([CompositeDef {
            name: "rgb".into(),
            title: "RGB".into(),
            channels: [channel(0.0), channel(1.0), channel(-2.0)],
        }])
    }
}

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "apis": ["wms"], "engine_type": "geotiff",
    }))
    .unwrap()
}

fn style(name: &str, colour: [u8; 4]) -> StyleInfo {
    StyleInfo {
        name: name.into(),
        title: name.into(),
        palette: ds_render::builtin_palette_arc("viridis").unwrap(),
        colormap: Arc::new(Solid(colour)),
        min: 0.0,
        max: 2.0,
        parameter: None,
    }
}

/// The collections the tests draw:
///
/// - `base`: data everywhere, `default` blue, `alt` yellow;
/// - `west`: data west of 20°E, translucent red;
/// - `east`: data east of 20°E, green;
/// - `timed` (T0, T1), `later` (T0..T2), `untimed` (no times),
///   `levels` (850 and 500 hPa), `runs` (R0, R1), all grey;
/// - `params`: parameters `a` (T0) and `b` (T1) on their own time axes;
/// - `sat`: an RGB composite layer `sat/rgb`;
/// - `failing` and `busy`: an engine that fails, one that is overloaded;
/// - `slow` and `slower`: reads taking [`SLOW`] and twice that.
struct Fixture {
    app: axum::Router,
    engines: HashMap<&'static str, Arc<Region>>,
    tile_cache: Arc<ds_render::TilePixelCache>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_tile_cache(16)
    }

    fn with_tile_cache(metatile_cache_mb: u64) -> Self {
        let region = |f: &dyn Fn(&mut Region)| {
            let mut region = Region::new();
            f(&mut region);
            Arc::new(region)
        };
        let engines: HashMap<&'static str, Arc<Region>> = HashMap::from([
            ("base", region(&|_| {})),
            ("west", region(&|r| r.west_of = Some(20.0))),
            ("east", region(&|r| r.east_of = Some(20.0))),
            ("timed", region(&|r| r.times = vec![at(T0), at(T1)])),
            ("later", region(&|r| r.times = vec![at(T0), at(T1), at(T2)])),
            ("untimed", region(&|r| r.times = Vec::new())),
            ("levels", region(&|r| r.vertical = true)),
            ("runs", region(&|r| r.runs = vec![at(R0), at(R1)])),
            (
                "params",
                region(&|r| {
                    r.times = vec![at(T0), at(T1)];
                    r.parameter_times = vec![("a", vec![at(T0)]), ("b", vec![at(T1)])];
                }),
            ),
            ("sat", region(&|r| r.composite = true)),
            ("failing", region(&|r| r.answer = Answer::Fails)),
            ("busy", region(&|r| r.answer = Answer::Busy)),
            ("slow", region(&|r| r.delay = SLOW)),
            ("slower", region(&|r| r.delay = 2 * SLOW)),
        ]);
        let mut styles = HashMap::new();
        for id in engines.keys() {
            let colour = match *id {
                "base" => BLUE,
                "west" => RED_HALF,
                "east" => GREEN,
                _ => GREY,
            };
            let mut layer_styles =
                HashMap::from([("default".to_string(), style("default", colour))]);
            if *id == "base" {
                layer_styles.insert("alt".to_string(), style("alt", YELLOW));
            }
            styles.insert(id.to_string(), layer_styles);
        }
        let tile_cache = Arc::new(ds_render::TilePixelCache::new(metatile_cache_mb));
        let app = api_wms::router(Arc::new(ArcSwap::from_pointee(WmsState {
            engines: engines
                .iter()
                .map(|(id, engine)| (id.to_string(), engine.clone() as Arc<dyn MapEngine>))
                .collect(),
            point_engines: HashMap::new(),
            collections: engines
                .keys()
                .map(|id| (id.to_string(), collection(id)))
                .collect(),
            styles,
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            tile_cache: tile_cache.clone(),
            base_url: String::new(),
            trust_proxy_headers: false,
        })));
        Self {
            app,
            engines,
            tile_cache,
        }
    }

    fn engine(&self, id: &str) -> &Region {
        &self.engines[id]
    }

    async fn get(&self, query: &str) -> Reply {
        let response = self
            .app
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
        let (content_type, cache_control, x_cache) = (
            header("content-type"),
            header("cache-control"),
            header("x-cache"),
        );
        let timing = response
            .extensions()
            .get::<ds_executor::RenderTiming>()
            .cloned();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            content_type,
            cache_control,
            x_cache,
            timing,
            body: body.to_vec(),
        }
    }

    /// A GetMap of `layers` with `styles` and `extra` parameters, 40 × 20
    /// pixels over lon 10–30°, lat 50–60° in CRS:84.
    async fn get_map(&self, layers: &str, styles: &str, extra: &str) -> Reply {
        self.get(&format!(
            "REQUEST=GetMap&LAYERS={layers}&STYLES={styles}&CRS=CRS:84&BBOX=10,50,30,60\
             &WIDTH=40&HEIGHT=20&FORMAT=image/png{extra}"
        ))
        .await
    }
}

struct Reply {
    status: StatusCode,
    content_type: String,
    cache_control: String,
    x_cache: String,
    timing: Option<ds_executor::RenderTiming>,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The image's RGBA pixels, after checking it is a served PNG.
    fn pixels(&self) -> Vec<[u8; 4]> {
        assert_eq!(self.status, StatusCode::OK, "{}", self.text());
        assert_eq!(self.content_type, "image/png");
        png_pixels(&self.body)
    }

    fn exception_code(&self) -> String {
        let xml = self.text();
        let code = xml
            .split_once("<ServiceException code=\"")
            .unwrap_or_else(|| panic!("no ServiceException: {xml}"))
            .1;
        code[..code.find('"').unwrap()].to_string()
    }
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

/// `images` drawn bottom first with `ds_render::draw_over`, then flattened
/// onto `background` when there is one: what a GetMap of their layers must
/// show.
fn drawn(images: &[Vec<[u8; 4]>], background: Option<[u8; 3]>) -> Vec<[u8; 4]> {
    let mut below: Vec<u8> = images[0].concat();
    for above in &images[1..] {
        ds_render::draw_over(&mut below, &above.concat()).unwrap();
    }
    if let Some(background) = background {
        ds_render::flatten_onto(&mut below, background);
    }
    below.as_chunks::<4>().0.to_vec()
}

/// WMS 1.3.0 §7.3.3.3: the first layer listed is drawn first, at the
/// bottom, and each next one over it. The image is the layers' own
/// transparent images drawn over each other in that order.
#[tokio::test]
async fn layers_are_drawn_bottom_up_in_list_order() {
    let fixture = Fixture::new();
    let transparent = "&TRANSPARENT=TRUE";
    let own = |layer: &'static str| {
        let fixture = &fixture;
        async move { fixture.get_map(layer, "", transparent).await.pixels() }
    };
    let (base, west, east) = (own("base").await, own("west").await, own("east").await);

    let image = fixture
        .get_map("base,west,east", "", transparent)
        .await
        .pixels();
    assert_eq!(
        image,
        drawn(&[base.clone(), west.clone(), east.clone()], None)
    );
    // West of 20°E: translucent red over blue; east of it: green over blue.
    assert_eq!(image[0], [128, 0, 127, 255]);
    assert_eq!(image[39], GREEN);

    // Listed last, the opaque base covers the others.
    let image = fixture
        .get_map("east,west,base", "", transparent)
        .await
        .pixels();
    assert_eq!(image, base);

    // Two layers, as ets-wms13's two-layers sends them: the same layer twice.
    let image = fixture.get_map("west,west", "", transparent).await.pixels();
    assert_eq!(image, drawn(&[west.clone(), west], None));
    assert_eq!(image[0], [255, 0, 0, 192]);
    assert_eq!(image[39], [0; 4]);
}

/// `TRANSPARENT` and `BGCOLOR` apply once, to the whole image, not to each
/// layer: a layer's nodata shows the layers below it, never BGCOLOR.
#[tokio::test]
async fn transparent_and_bgcolor_apply_to_the_whole_image() {
    let fixture = Fixture::new();
    let own = |layer: &'static str| {
        let fixture = &fixture;
        async move {
            fixture
                .get_map(layer, "", "&TRANSPARENT=TRUE")
                .await
                .pixels()
        }
    };
    let (west, east) = (own("west").await, own("east").await);

    // Opaque by default, over white.
    let image = fixture.get_map("east,west", "", "").await.pixels();
    assert_eq!(
        image,
        drawn(&[east.clone(), west.clone()], Some([255, 255, 255]))
    );
    assert!(image.iter().all(|p| p[3] == 255), "no transparent pixel");

    let image = fixture
        .get_map("west,west", "", "&TRANSPARENT=FALSE&BGCOLOR=0x0000FF")
        .await
        .pixels();
    assert_eq!(
        image,
        drawn(&[west.clone(), west.clone()], Some([0, 0, 255]))
    );
    // Red at 192/255 over the blue BGCOLOR, and BGCOLOR where neither
    // layer has data: once, not per layer, which would have turned the
    // top layer's nodata opaque blue over the red below.
    assert_eq!(image[0], [192, 0, 63, 255]);
    assert_eq!(image[39], [0, 0, 255, 255]);

    // JPEG, which has no alpha, is the same image over BGCOLOR.
    let jpeg = fixture
        .get(
            "REQUEST=GetMap&LAYERS=west,east&STYLES=&CRS=CRS:84&BBOX=10,50,30,60\
             &WIDTH=40&HEIGHT=20&FORMAT=image/jpeg",
        )
        .await;
    assert_eq!(jpeg.status, StatusCode::OK, "{}", jpeg.text());
    assert_eq!(jpeg.content_type, "image/jpeg");
    assert!(jpeg.body.starts_with(&[0xFF, 0xD8, 0xFF]));
}

/// `STYLES` pairs with `LAYERS` by position (§7.3.3.4): every form of
/// "all defaults" draws the same image, a named style draws its layer only,
/// and `STYLES` of another length is refused.
#[tokio::test]
async fn styles_pair_with_layers_by_position() {
    let fixture = Fixture::new();
    let layers = "base,west,west,east";
    let defaults = fixture.get_map(layers, "", "").await.pixels();
    for styles in [
        ",,,",
        "default,default,default,default",
        "default,,,default",
    ] {
        assert_eq!(
            fixture.get_map(layers, styles, "").await.pixels(),
            defaults,
            "STYLES={styles}"
        );
    }

    // The named style draws the base yellow; the layers over it keep theirs.
    let image = fixture.get_map("base,west", "alt,", "").await.pixels();
    assert_eq!(image[0], [255, 127, 0, 255], "red over yellow");
    assert_eq!(image[39], YELLOW);
    let image = fixture.get_map("west,base", ",alt", "").await.pixels();
    assert!(image.iter().all(|&p| p == YELLOW));

    for (layers, styles, code) in [
        ("base,west", "alt", "InvalidParameterValue"),
        ("base,west", "alt,,", "InvalidParameterValue"),
        ("base,west", "default,alt", "StyleNotDefined"),
        ("base,nope", "", "LayerNotDefined"),
    ] {
        let reply = fixture.get_map(layers, styles, "").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{layers} {styles}");
        assert_eq!(reply.exception_code(), code, "{layers} {styles}");
    }
}

/// GetCapabilities advertises `LayerLimit` 5. A list of five is drawn; six
/// are refused, once every name and style has been checked.
#[tokio::test]
async fn layer_limit_is_five_and_names_are_checked_first() {
    let fixture = Fixture::new();
    let caps = fixture.get("REQUEST=GetCapabilities").await.text();
    assert!(caps.contains("<LayerLimit>5</LayerLimit>"), "{caps}");
    assert_eq!(api_wms::params::LAYER_LIMIT, 5);

    let five = fixture.get_map("base,west,east,base,west", "", "").await;
    assert_eq!(five.status, StatusCode::OK, "{}", five.text());

    for (layers, styles, code) in [
        ("base,west,east,base,west,east", "", "InvalidParameterValue"),
        ("base,west,east,base,west,nope", "", "LayerNotDefined"),
        (
            "base,west,east,base,west,base",
            ",,,,,bogus",
            "StyleNotDefined",
        ),
    ] {
        let reply = fixture.get_map(layers, styles, "").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{layers}");
        assert_eq!(reply.exception_code(), code, "{layers}");
    }
}

/// `TIME` applies to the listed layers with a time dimension; a layer
/// without one ignores it. When no listed layer has one, every layer takes
/// it, as one layer without times always did. The cache headers follow
/// the layers' own: `immutable` only when every layer draws the explicit
/// TIME.
#[tokio::test]
async fn time_applies_to_the_layers_with_a_time_dimension() {
    const IMMUTABLE: &str = "public, max-age=86400, immutable";
    const SHORT: &str = "public, max-age=60, must-revalidate";
    let fixture = Fixture::new();
    let time = format!("&TIME={T0}");

    let reply = fixture.get_map("timed,untimed", "", &time).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(fixture.engine("timed").last_call().time, Some(at(T0)));
    assert_eq!(fixture.engine("untimed").last_call().time, None);
    assert_eq!(
        reply.cache_control, SHORT,
        "the untimed layer is not pinned"
    );

    let reply = fixture.get_map("untimed", "", &time).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(fixture.engine("untimed").last_call().time, Some(at(T0)));

    let reply = fixture.get_map("untimed,untimed", "", &time).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let calls = fixture.engine("untimed").calls();
    assert!(calls[calls.len() - 2..]
        .iter()
        .all(|c| c.time == Some(at(T0))));

    let reply = fixture.get_map("timed,later", "", &time).await;
    assert_eq!(reply.cache_control, IMMUTABLE, "both layers draw T0");

    // A malformed TIME is still the request's error.
    let reply = fixture
        .get_map("timed,untimed", "", "&TIME=yesterday")
        .await;
    assert_eq!(reply.exception_code(), "InvalidDimensionValue");
}

/// Without `TIME`, each layer draws its own default, a parameter with its
/// own time axis its own latest (#819), and keys its caches on it.
#[tokio::test]
async fn each_layer_defaults_its_own_time() {
    let fixture = Fixture::new();
    let reply = fixture
        .get_map("timed,later,params/a,params/b", "", "")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(fixture.engine("timed").last_call().time, Some(at(T1)));
    assert_eq!(fixture.engine("later").last_call().time, Some(at(T2)));
    let params: Vec<_> = fixture
        .engine("params")
        .calls()
        .into_iter()
        .map(|c| (c.parameter, c.time))
        .collect();
    assert_eq!(
        params,
        [
            (Some("a".to_string()), Some(at(T0))),
            (Some("b".to_string()), Some(at(T1)))
        ]
    );
    // A TIME the parameters snap differently: each draws its own step.
    let reply = fixture
        .get_map("params/a,params/b", "", &format!("&TIME={T2}"))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let calls = fixture.engine("params").calls();
    assert_eq!(calls[2].time, Some(at(T0)));
    assert_eq!(calls[3].time, Some(at(T1)));
}

/// `ELEVATION` applies to the listed layers with an elevation dimension and
/// the others ignore it; when no listed layer has one it is
/// `InvalidDimensionValue`, as for one layer.
#[tokio::test]
async fn elevation_applies_to_the_layers_with_an_elevation_dimension() {
    let fixture = Fixture::new();
    let reply = fixture
        .get_map("levels,untimed", "", "&ELEVATION=500")
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(fixture.engine("levels").last_call().z, Some(500.0));
    assert_eq!(fixture.engine("untimed").last_call().z, None);

    for layers in ["untimed,base", "base"] {
        let reply = fixture.get_map(layers, "", "&ELEVATION=500").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{layers}");
        assert_eq!(reply.exception_code(), "InvalidDimensionValue", "{layers}");
    }
}

/// `DIM_REFERENCE_TIME` applies to the listed layers with model runs. A run
/// a layer with runs does not have is that layer's `InvalidDimensionValue`;
/// one no listed layer has runs for is the request's.
#[tokio::test]
async fn reference_time_applies_to_the_layers_with_runs() {
    let fixture = Fixture::new();
    let run = format!("&DIM_REFERENCE_TIME={R0}");
    let reply = fixture.get_map("runs,untimed", "", &run).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        fixture.engine("runs").last_call().reference_time,
        Some(at(R0))
    );
    assert_eq!(fixture.engine("untimed").last_call().reference_time, None);

    for (layers, run) in [
        ("untimed,runs", "&DIM_REFERENCE_TIME=2026-09-30T00:00:00Z"),
        ("untimed,base", run.as_str()),
        ("base", run.as_str()),
    ] {
        let reply = fixture.get_map(layers, "", run).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{layers} {run}");
        assert_eq!(reply.exception_code(), "InvalidDimensionValue", "{layers}");
    }
}

/// A layer list is drawn from each layer's own meta-tiles: the tiles a
/// layer's own GetMap cached serve it in a list, and the other way round.
/// The list itself is never in the rendered cache, so a repeat is
/// assembled again from cached tiles, without an engine read.
#[tokio::test]
async fn layers_reuse_their_own_cached_meta_tiles() {
    use ds_executor::RenderOutcome::{Assembled, Cold, Hit};
    let fixture = Fixture::new();
    let view = |layers: &str| {
        format!(
            "REQUEST=GetMap&LAYERS={layers}&STYLES=&CRS=EPSG:3857\
             &BBOX=1113194,6446276,3339584,8399737&WIDTH=256&HEIGHT=256\
             &FORMAT=image/png&TRANSPARENT=TRUE"
        )
    };
    let reads = |id: &str| fixture.engine(id).call_count();

    let base = fixture.get(&view("base")).await;
    assert_eq!(base.status, StatusCode::OK, "{}", base.text());
    assert_eq!(base.timing.unwrap().outcome, Cold);
    let base_reads = reads("base");
    assert!(base_reads > 0);

    let list = fixture.get(&view("base,west")).await;
    assert_eq!(list.x_cache, "MISS");
    assert_eq!(reads("base"), base_reads, "base came from its cached tiles");
    let west_reads = reads("west");
    assert!(west_reads > 0);
    let timing = list.timing.unwrap();
    assert_eq!(timing.collection, "base", "labelled by the bottom layer");
    assert_eq!(timing.outcome, Cold, "west was read");

    let again = fixture.get(&view("base,west")).await;
    assert_eq!(again.x_cache, "MISS", "a list is not in the rendered cache");
    assert_eq!(again.timing.unwrap().outcome, Assembled);
    assert_eq!((reads("base"), reads("west")), (base_reads, west_reads));
    assert_eq!(again.body, list.body);

    // West's own GetMap is assembled from the tiles the list cached.
    let west = fixture.get(&view("west")).await;
    assert_eq!(west.timing.unwrap().outcome, Assembled);
    assert_eq!(reads("west"), west_reads);
    // And the list is those images drawn over each other.
    assert_eq!(
        png_pixels(&list.body),
        drawn(&[png_pixels(&base.body), png_pixels(&west.body)], None)
    );
    // A single layer is still in the rendered cache.
    assert_eq!(
        fixture.get(&view("base")).await.timing.unwrap().outcome,
        Hit
    );
    assert!(!fixture.tile_cache.is_empty());
}

/// One layer's image is byte for byte what it was before layer lists:
/// the direct path's `render_tile_with_background` over the engine's tile,
/// and the meta-tiled path's `render_metatiled` over the engine's tiles.
#[tokio::test]
async fn a_single_layer_image_is_unchanged() {
    let fixture = Fixture::new();
    let colormap = Solid(RED_HALF);
    let engine = fixture.engine("west");

    let reply = fixture.get_map("west", "", "&BGCOLOR=0x336699").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let tile = engine
        .get_raster_tile(
            [10.0, 50.0, 30.0, 60.0],
            40,
            20,
            Some(at(T0)),
            &OutputCrs::Wgs84,
            None,
            None,
            None,
        )
        .unwrap();
    let expected = ds_render::render_tile_with_background(
        &tile,
        &colormap,
        ImageFormat::Png,
        Some([0x33, 0x66, 0x99]),
    )
    .unwrap();
    assert_eq!(reply.body, expected, "direct path");

    let reply = fixture
        .get(
            "REQUEST=GetMap&LAYERS=west&STYLES=&CRS=EPSG:3857\
             &BBOX=1113194,6446276,3339584,8399737&WIDTH=300&HEIGHT=200&FORMAT=image/png",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let prefix = ds_render::TileKeyPrefix {
        layer: "west".into(),
        parameter: None,
        style: "default".into(),
        time: Some(at(T0)),
        z: None,
        reference_time: None,
        content_version: 0,
    };
    let wgs84 = [
        ds_core::web_mercator::x_to_lon(1113194.0),
        ds_core::web_mercator::y_to_lat(6446276.0),
        ds_core::web_mercator::x_to_lon(3339584.0),
        ds_core::web_mercator::y_to_lat(8399737.0),
    ];
    let ds_render::MetaTile::Image { bytes, .. } = ds_render::render_metatiled(
        wgs84,
        &OutputCrs::WebMercator,
        300,
        200,
        &prefix,
        &colormap,
        ImageFormat::Png,
        Some([255, 255, 255]),
        &ds_render::TilePixelCache::new(16),
        |bbox, w, h, output| {
            engine.get_raster_tile(bbox, w, h, Some(at(T0)), output, None, None, None)
        },
    )
    .unwrap() else {
        panic!("expected a meta-tiled image");
    };
    assert_eq!(reply.body, bytes, "meta-tiled path");
}

/// A layer that fails makes the map what one failing layer gives: the
/// error tile, or the 503 for an overloaded engine. Layers listed after a
/// failed one are not drawn.
#[tokio::test]
async fn a_failing_layer_fails_the_map() {
    let fixture = Fixture::new();
    let reply = fixture.get_map("base,failing,east", "", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.x_cache, "ERROR");
    assert_eq!(reply.content_type, "image/png");
    assert!(reply.timing.is_none(), "an error tile records no latency");
    assert_eq!(fixture.engine("east").call_count(), 0);

    let reply = fixture.get_map("base,busy", "", "").await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
}

/// An RGB composite layer is drawn in a list like any layer: its composed
/// pixels over the layers below it.
#[tokio::test]
async fn composite_layers_draw_in_a_list() {
    let fixture = Fixture::new();
    let own = |layer: &'static str| {
        let fixture = &fixture;
        async move {
            fixture
                .get_map(layer, "", "&TRANSPARENT=TRUE")
                .await
                .pixels()
        }
    };
    let (west, sat) = (own("west").await, own("sat/rgb").await);
    assert!(sat.iter().all(|p| p[3] == 255), "the composite is opaque");
    let image = fixture
        .get_map("sat/rgb,west", "", "&TRANSPARENT=TRUE")
        .await
        .pixels();
    assert_eq!(image, drawn(&[sat, west], None));
}

/// Several layers are admitted for what they hold at once: one layer's
/// render, 32 B per output pixel, plus the 4 B RGBA image of the layers
/// below, whatever their number. With `MC_RENDER_MEMORY_MB=1` one layer of
/// 181 × 181 fits and two do not (36 × 32 761 B > 1 MiB, the immediate
/// 503); five of 170 × 170 fit (36 × 28 900 B), where a charge per layer
/// would need 4.6 MB. The budget is fixed by its first read, so the test
/// runs in a child process.
#[test]
fn layers_are_admitted_for_one_layer_and_the_image_below() {
    const TEST: &str = "layers_are_admitted_for_one_layer_and_the_image_below";
    const CHILD: &str = "MC_TEST_LAYERED_BUDGET_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("MC_RENDER_MEMORY_MB", "1")
            .status()
            .unwrap();
        assert!(status.success(), "{TEST} failed in its child process");
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new();
        let get_map = |layers: &str, size: u32| {
            format!(
                "REQUEST=GetMap&LAYERS={layers}&STYLES=&CRS=CRS:84&BBOX=10,50,30,60\
                 &WIDTH={size}&HEIGHT={size}&FORMAT=image/png"
            )
        };
        for (layers, size, status) in [
            ("base", 181, StatusCode::OK),
            ("base,west", 181, StatusCode::SERVICE_UNAVAILABLE),
            ("base,west,east,west,base", 170, StatusCode::OK),
        ] {
            let reply = fixture.get(&get_map(layers, size)).await;
            assert_eq!(reply.status, status, "{layers} at {size}: {}", reply.text());
        }
    });
}

/// The request shapes of ets-wms13's multi-layer GetMap tests (#1065): the
/// same layer two, three and four times with `STYLES` empty, of commas, or
/// named, and five layers with some styles named and some default
/// (`styles-some-default`), are each an image in the requested format.
#[tokio::test]
async fn ets_multi_layer_get_maps_are_images() {
    let fixture = Fixture::new();
    for (layers, styles) in [
        ("base,base", ""),
        ("base,base,base", ""),
        ("base,base", "default,default"),
        ("base,base,base", "default,default,default"),
        ("base,base,base,base", ""),
        ("base,base,base,base", ",,,"),
        ("west,base,west,base,base", "default,,default,,"),
    ] {
        let reply = fixture
            .get(&format!(
                "ReQuEsT=GetMap&WiDtH=100&HeIgHt=100&BbOx=-0.0025,-0.0025,0.0025,0.0025\
                 &CrS=CRS%3A84&StYlEs={styles}&FoRmAt=image%2Fpng&LaYeRs={layers}"
            ))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{layers} {styles}: {}",
            reply.text()
        );
        assert_eq!(reply.content_type, "image/png", "{layers} {styles}");
    }
}

/// A list of N layers renders under the render deadline N times (#1181):
/// each layer gets the time it gets alone. With `MC_RENDER_TIMEOUT_MS=400`,
/// one 250 ms layer fits and so do two, 500 ms; one 500 ms layer does not,
/// nor do two, 1 s against 800 ms. The deadline is fixed by its first read,
/// so the test runs in a child process.
#[test]
fn a_list_renders_under_the_render_deadline_once_per_layer() {
    const TEST: &str = "a_list_renders_under_the_render_deadline_once_per_layer";
    const CHILD: &str = "MC_TEST_LAYERED_DEADLINE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("MC_RENDER_TIMEOUT_MS", "400")
            .status()
            .unwrap();
        assert!(status.success(), "{TEST} failed in its child process");
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new();
        for (layers, status) in [
            ("slow", StatusCode::OK),
            ("slow,slow", StatusCode::OK),
            ("slower", StatusCode::SERVICE_UNAVAILABLE),
            ("slower,slower", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let reply = fixture.get_map(layers, "", "").await;
            assert_eq!(reply.status, status, "{layers}: {}", reply.text());
        }
    });
}
