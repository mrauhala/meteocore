//! `LayerNotDefined` hints (#118): a name that is no layer of this server
//! says which layer it most likely meant, by one fixed rewrite (its first
//! `:` as `/`, for clients that name `collection:parameter`), or lists the
//! layers of the collection it names, bounded. Never by similarity, and
//! never naming a layer GetCapabilities does not list.

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
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

/// A collection publishing `parameters` (none: one layer, the collection).
struct Grid(Vec<String>);

impl MapEngine for Grid {
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
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(0.5); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([10.0, 50.0, 30.0, 60.0]),
            times: vec!["2026-11-08T00:00:00Z".parse().unwrap()],
            parameter: self.0.first().cloned().unwrap_or_default(),
            unit: String::new(),
            parameters: self
                .0
                .iter()
                .map(|name| ParameterInfo {
                    name: name.clone(),
                    title: name.clone(),
                    unit: String::new(),
                })
                .collect(),
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": id, "apis": ["wms"], "engine_type": "grib",
    }))
    .unwrap()
}

fn default_style() -> HashMap<String, StyleInfo> {
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

/// `nwp` (`t2m`, `msl`), `big` (25 parameters, `p00`…`p24`) and the
/// single-layer `radar`.
fn router() -> axum::Router {
    let big: Vec<String> = (0..25).rev().map(|i| format!("p{i:02}")).collect();
    let collections: [(&str, Vec<String>); 3] = [
        ("nwp", vec!["t2m".into(), "msl".into()]),
        ("big", big),
        ("radar", Vec::new()),
    ];
    let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut configs = HashMap::new();
    let mut styles = HashMap::new();
    for (id, parameters) in collections {
        engines.insert(id.to_string(), Arc::new(Grid(parameters)));
        configs.insert(id.to_string(), collection(id));
        styles.insert(id.to_string(), default_style());
    }
    api_wms::router(Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections: configs,
        styles,
        point_engines: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

fn get_map(layers: &str) -> String {
    format!(
        "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={layers}&STYLES=\
         &CRS=CRS:84&BBOX=10,50,30,60&WIDTH=20&HEIGHT=10&FORMAT=image/png"
    )
}

fn get_legend(layer: &str) -> String {
    format!("/?SERVICE=WMS&REQUEST=GetLegendGraphic&LAYER={layer}&FORMAT=image/png")
}

fn get_feature_info(layers: &str, query_layers: &str) -> String {
    format!(
        "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS={layers}&STYLES=\
         &CRS=CRS:84&BBOX=10,50,30,60&WIDTH=20&HEIGHT=10&QUERY_LAYERS={query_layers}\
         &I=0&J=0"
    )
}

/// The request's ServiceExceptionReport: its raw XML, and the
/// `<ServiceException>`'s code and text, read with quick-xml.
struct Exception {
    xml: String,
    code: String,
    message: String,
}

async fn exception(app: &axum::Router, uri: &str) -> Exception {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let xml = String::from_utf8(body.to_vec()).unwrap();
    let (code, message) = service_exception(&xml);
    Exception { xml, code, message }
}

/// The code and the unescaped text of the report's `<ServiceException>`.
/// quick-xml reports `&lt;` and its kin as `GeneralRef` events, between
/// the text events.
fn service_exception(xml: &str) -> (String, String) {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut code, mut message, mut inside) = (None, String::new(), false);
    loop {
        match reader.read_event().unwrap() {
            Event::Start(e) if e.local_name().as_ref() == "ServiceException" => {
                let attr = e.try_get_attribute("code").unwrap().unwrap();
                code = Some(attr.value.into_owned());
                inside = true;
            }
            Event::Text(text) if inside => message.push_str(&text.xml10_content()),
            Event::GeneralRef(entity) if inside => match entity.resolve_char_ref().unwrap() {
                Some(c) => message.push(c),
                None => message.push_str(
                    quick_xml::escape::resolve_predefined_entity(entity.as_ref()).unwrap(),
                ),
            },
            Event::End(e) if e.local_name().as_ref() == "ServiceException" => inside = false,
            Event::Eof => break,
            _ => {}
        }
    }
    (code.expect("a ServiceException"), message)
}

/// `LAYERS=nwp:t2m` (as sent, and percent-encoded) names `nwp/t2m`: the
/// error says so, in GetMap, a GetMap layer list and GetLegendGraphic.
#[tokio::test]
async fn a_colon_name_is_told_the_slash_layer_it_means() {
    let app = router();
    let hint = "Did you mean 'nwp/t2m'? Layer names use '/' between collection and parameter.";
    for uri in [
        get_map("nwp:t2m"),
        get_map("nwp%3At2m"),
        get_map("radar,nwp:t2m"),
        get_legend("nwp:t2m"),
    ] {
        let reply = exception(&app, &uri).await;
        assert_eq!(reply.code, "LayerNotDefined", "{uri}");
        assert_eq!(
            reply.message,
            format!("Layer 'nwp:t2m' does not exist. {hint}"),
            "{uri}"
        );
    }
}

/// A colon name whose `/` form is no layer is suggested nothing: not the
/// nearest parameter, not a parameter of a single-layer collection, which
/// GetCapabilities lists only as the collection. When the name's
/// collection is listed, its layers are; when it is not, or the name names
/// no collection, nothing is.
#[tokio::test]
async fn a_colon_name_without_its_layer_is_suggested_nothing() {
    let app = router();
    for (layer, message) in [
        (
            "nwp:t2",
            "Layer 'nwp:t2' does not exist. Layers of collection 'nwp': nwp/msl, nwp/t2m.",
        ),
        (
            "radar:reflectivity",
            "Layer 'radar:reflectivity' does not exist. Layers of collection 'radar': radar.",
        ),
        ("nope:t2m", "Layer 'nope:t2m' does not exist"),
        ("nwpx", "Layer 'nwpx' does not exist"),
    ] {
        for uri in [get_map(layer), get_legend(layer)] {
            let reply = exception(&app, &uri).await;
            assert_eq!(reply.code, "LayerNotDefined", "{uri}");
            assert_eq!(reply.message, message, "{uri}");
        }
    }
}

/// An unknown parameter of a listed collection lists the collection's
/// layers, sorted, at most 20 of them and how many more.
#[tokio::test]
async fn an_unknown_parameter_lists_its_collections_layers_bounded() {
    let app = router();
    let first: Vec<String> = (0..20).map(|i| format!("big/p{i:02}")).collect();
    for (layer, message) in [
        (
            "big/nope",
            format!(
                "Layer 'big/nope' does not exist. Layers of collection 'big': {} and 5 more.",
                first.join(", ")
            ),
        ),
        (
            "big:nope",
            format!(
                "Layer 'big:nope' does not exist. Layers of collection 'big': {} and 5 more.",
                first.join(", ")
            ),
        ),
        (
            "nwp/nope",
            "Layer 'nwp/nope' does not exist. Layers of collection 'nwp': nwp/msl, nwp/t2m."
                .to_string(),
        ),
    ] {
        let reply = exception(&app, &get_map(layer)).await;
        assert_eq!(reply.code, "LayerNotDefined", "{layer}");
        assert_eq!(reply.message, message, "{layer}");
    }
}

/// The name the client sent is echoed in the hint's message through the
/// report writer's escaping, never as markup.
#[tokio::test]
async fn a_hinted_name_is_escaped_in_the_exception_report() {
    let app = router();
    // `nwp:<b>&"x"`, percent-encoded.
    for uri in [
        get_map("nwp%3A%3Cb%3E%26%22x%22"),
        get_legend("nwp%3A%3Cb%3E%26%22x%22"),
    ] {
        let reply = exception(&app, &uri).await;
        assert_eq!(reply.code, "LayerNotDefined", "{uri}");
        assert_eq!(
            reply.message,
            "Layer 'nwp:<b>&\"x\"' does not exist. Layers of collection 'nwp': nwp/msl, nwp/t2m.",
            "{uri}"
        );
        assert!(!reply.xml.contains("<b>"), "{}", reply.xml);
        assert!(reply.xml.contains("&lt;b&gt;&amp;"), "{}", reply.xml);
    }
}

/// A `QUERY_LAYERS` name in the colon form of one of `LAYERS` is told so.
#[tokio::test]
async fn a_colon_query_layer_is_told_the_map_layer_it_means() {
    let app = router();
    let reply = exception(&app, &get_feature_info("nwp/t2m", "nwp:t2m")).await;
    assert_eq!(reply.code, "LayerNotDefined");
    assert_eq!(
        reply.message,
        "QUERY_LAYERS layer 'nwp:t2m' is not one of LAYERS. Did you mean 'nwp/t2m'? \
         Layer names use '/' between collection and parameter."
    );
    let other = exception(&app, &get_feature_info("nwp/t2m", "nwp:msl")).await;
    assert_eq!(
        other.message,
        "QUERY_LAYERS layer 'nwp:msl' is not one of LAYERS"
    );
}
