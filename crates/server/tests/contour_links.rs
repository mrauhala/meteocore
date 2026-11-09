//! Every link to a style resolves (#1378): a crawler over a collection's
//! description on the shared OGC API root and on the per-API Maps and Tiles
//! services, and over WMS GetCapabilities, gets no 404. A parameter's
//! built-in `contours` lives only on that parameter's style layer, so its
//! links, the tilesets they list and their tile templates name the
//! parameter, and the style routes resolve it there.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{
    MapEngine, OutputCrs, ParameterInfo, RasterInfo, RasterTile, RasterValues,
};
use ds_render::{RenderedCache, StyleContext};

const BASE: &str = "https://example.test";
const ID: &str = "nwp";

/// A model with isobars and isotherms among its built-in styles, and a
/// wind speed with none: one value everywhere, which no style objects to.
struct Model;

impl MapEngine for Model {
    fn get_raster_tile(
        &self,
        _: [f64; 4],
        width: u32,
        height: u32,
        _: Option<DateTime<Utc>>,
        _: &OutputCrs,
        _: Option<&str>,
        _: Option<f64>,
        _: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        Ok(RasterTile {
            width,
            height,
            values: RasterValues::F32 {
                data: vec![1000.5; width as usize * height as usize],
                nodata: None,
            },
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([-30.0, 30.0, 40.0, 70.0]),
            times: vec!["2026-11-01T00:00:00Z".parse().unwrap()],
            parameter: "10si".into(),
            unit: "m/s".into(),
            parameters: [
                ("10si", "10 metre wind speed", "m/s"),
                ("msl", "Mean sea level pressure", "hPa"),
                ("2t", "2 metre temperature", "K"),
            ]
            .map(|(name, title, unit)| ParameterInfo {
                name: name.into(),
                title: title.into(),
                unit: unit.into(),
            })
            .to_vec(),
            vertical: None,
            grid_size: Some([280, 160]),
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

type Styles = HashMap<String, HashMap<String, ds_render::StyleInfo>>;
/// The collections, map engines and style layers every service registers.
type Registries = (
    HashMap<String, CollectionConfig>,
    HashMap<String, Arc<dyn MapEngine>>,
    Styles,
);

fn registries() -> Registries {
    let config: CollectionConfig = serde_json::from_value(serde_json::json!({
        "id": ID, "title": "NWP", "description": "A model",
        "apis": ["maps", "tiles", "wms"], "engine_type": "grib",
    }))
    .unwrap();
    let ctx = StyleContext::with_builtins();
    let mut styles = ctx
        .parameter_layer_styles(&config, None, &Model.raster_info().parameters, &|_, c| c)
        .unwrap();
    styles.insert(ID.into(), ctx.collection_styles(&config, None).unwrap());
    (
        HashMap::from([(ID.to_string(), config)]),
        HashMap::from([(ID.to_string(), Arc::new(Model) as Arc<dyn MapEngine>)]),
        styles,
    )
}

fn maps_state() -> api_maps::AppState {
    let (collections, engines, styles) = registries();
    Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
        engines,
        map_tileset_ids: collections.keys().cloned().collect::<HashSet<_>>(),
        collections,
        discovery: Default::default(),
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(4)),
        base_url: BASE.into(),
        trust_proxy_headers: false,
    }))
}

fn tiles_state() -> api_tiles::AppState {
    let (collections, map_engines, styles) = registries();
    Arc::new(ArcSwap::from_pointee(api_tiles::TilesState {
        map_engines,
        collections,
        discovery: Default::default(),
        styles,
        feature_engines: HashMap::new(),
        feature_collections: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(4)),
        vector_tile_cache: Arc::new(ds_mvt::VectorTileCache::new(4)),
        base_url: BASE.into(),
        trust_proxy_headers: false,
    }))
}

fn wms_state() -> api_wms::AppState {
    let (collections, engines, styles) = registries();
    Arc::new(ArcSwap::from_pointee(api_wms::WmsState {
        engines,
        point_engines: HashMap::new(),
        collections,
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(4)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(4)),
        base_url: BASE.into(),
        trust_proxy_headers: false,
    }))
}

/// The shared root, and the per-API Maps, Tiles and WMS services at their
/// mounts, as the server serves them.
fn app() -> Router {
    let shared = api_common::shared::SharedApi::new(
        "",
        vec![
            Arc::new(api_maps::MapsBlock::new(maps_state())),
            Arc::new(api_tiles::TilesBlock::new(tiles_state())),
        ],
        vec![],
    );
    Router::new()
        .nest(api_common::mounts::MAPS, api_maps::router(maps_state()))
        .nest(api_common::mounts::TILES, api_tiles::router(tiles_state()))
        .nest(api_common::mounts::WMS, api_wms::router(wms_state()))
        .merge(api_common::shared::router(shared))
}

async fn get(app: &Router, href: &str) -> (StatusCode, Vec<u8>) {
    let uri = href.strip_prefix(BASE).unwrap_or(href);
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

/// A tile template filled in: a zoom-3 tile over Europe.
fn tile(template: &str) -> String {
    template
        .replace("{tileMatrix}", "3")
        .replace("{tileRow}", "2")
        .replace("{tileCol}", "4")
}

/// Follow `href` and every style resource the JSON it returns links (a
/// path through `/styles/`: maps, legends, tileset lists, tilesets, tiles),
/// to `depth` levels, recording each status. Tile templates are filled in;
/// a non-JSON response ends the walk there.
async fn crawl(
    app: &Router,
    href: String,
    depth: usize,
    seen: &mut BTreeSet<String>,
    statuses: &mut Vec<(String, StatusCode)>,
) {
    let href = tile(&href);
    if !seen.insert(href.clone()) {
        return;
    }
    let (status, body) = get(app, &href).await;
    statuses.push((href.clone(), status));
    if depth == 0 {
        return;
    }
    let Ok(doc) = serde_json::from_slice::<Value>(&body) else {
        return;
    };
    let mut links = Vec::new();
    collect_links(&doc, &mut links);
    for link in links {
        if link.starts_with(BASE) && link.contains("/styles/") {
            Box::pin(crawl(app, link, depth - 1, seen, statuses)).await;
        }
    }
}

fn collect_links(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(href) = map.get("href").and_then(Value::as_str) {
                out.push(href.to_string());
            }
            map.values().for_each(|v| collect_links(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| collect_links(v, out)),
        _ => {}
    }
}

/// Every style link of the collection, on the shared root and the per-API
/// Maps and Tiles services, and every link of the tilesets those name,
/// resolves; the built-in `contours` among them names its parameter.
#[tokio::test]
async fn every_advertised_style_link_resolves() {
    let app = app();
    for collection in [
        format!("{BASE}/collections/{ID}"),
        format!("{BASE}/maps/collections/{ID}"),
        format!("{BASE}/maps/collections/{ID}/styles"),
        format!("{BASE}/tiles/collections/{ID}"),
    ] {
        let (status, body) = get(&app, &collection).await;
        assert_eq!(status, StatusCode::OK, "{collection}");
        let doc: Value = serde_json::from_slice(&body).unwrap();
        let styles = doc["styles"].as_array().unwrap();
        let contours = styles
            .iter()
            .find(|s| s["id"] == "contours")
            .unwrap_or_else(|| panic!("{collection} lists contours"));
        assert_eq!(contours["parameters"], serde_json::json!(["msl", "2t"]));
        let mut links = Vec::new();
        for style in styles {
            collect_links(&style["links"], &mut links);
        }
        assert!(
            links
                .iter()
                .filter(|l| l.contains("/styles/contours/"))
                .all(|l| l.ends_with("?parameter-name=msl")),
            "{collection}: {links:?}"
        );
        let (mut seen, mut statuses) = (BTreeSet::new(), Vec::new());
        for link in links {
            crawl(&app, link, 3, &mut seen, &mut statuses).await;
        }
        assert!(
            statuses
                .iter()
                .any(|(href, _)| href.contains("/styles/contours/")),
            "{collection}: nothing crawled for contours"
        );
        let broken: Vec<_> = statuses
            .iter()
            .filter(|(_, status)| !status.is_success())
            .collect();
        assert!(broken.is_empty(), "{collection}: {broken:?}");
    }

    // WMS: every LegendURL of GetCapabilities.
    let (status, body) = get(
        &app,
        &format!("{BASE}/wms?SERVICE=WMS&REQUEST=GetCapabilities"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let xml = String::from_utf8(body).unwrap();
    let legends: Vec<String> = xml
        .split("<LegendURL")
        .skip(1)
        .filter_map(|block| {
            let href = block.split("xlink:href=\"").nth(1)?.split('"').next()?;
            Some(href.replace("&amp;", "&"))
        })
        .collect();
    assert!(legends.iter().any(|l| l.contains("STYLE=contours")));
    for legend in legends {
        let (status, _) = get(&app, &legend).await;
        assert_eq!(status, StatusCode::OK, "{legend}");
    }
}

/// The styled tileset of a style only a parameter's layer offers is that
/// parameter's: without it, or with a parameter that does not offer it,
/// there is no such style (404); a parameter the collection lacks is a 400.
#[tokio::test]
async fn styled_tilesets_resolve_the_parameter_layer() {
    let app = app();
    let list = format!("{BASE}/collections/{ID}/styles/contours/map/tiles");
    for (query, status) in [
        ("?parameter-name=msl", StatusCode::OK),
        ("?parameter-name=2t", StatusCode::OK),
        ("", StatusCode::NOT_FOUND),
        ("?parameter-name=10si", StatusCode::NOT_FOUND),
        ("?parameter-name=nope", StatusCode::BAD_REQUEST),
        ("?other=1", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            get(&app, &format!("{list}{query}")).await.0,
            status,
            "{query}"
        );
    }
    let (_, body) = get(&app, &format!("{list}?parameter-name=2t")).await;
    let doc: Value = serde_json::from_slice(&body).unwrap();
    let mut links = Vec::new();
    collect_links(&doc, &mut links);
    assert!(links
        .iter()
        .all(|l| l.contains("parameter-name=2t") || !l.contains("/contours/")));
    let item = doc["tilesets"][0]["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "item")
        .unwrap()["href"]
        .as_str()
        .unwrap();
    assert!(
        item.ends_with("/{tileMatrix}/{tileRow}/{tileCol}?parameter-name=2t"),
        "{item}"
    );
    // A repeated `parameter-name` is the same JSON 400 every Tiles route
    // gives a malformed query, on the list and on one tileset.
    for path in [list.clone(), format!("{list}/WebMercatorQuad")] {
        let (status, body) = get(
            &app,
            &format!("{path}?parameter-name=msl&parameter-name=2t"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert!(
            serde_json::from_slice::<Value>(&body).is_ok(),
            "{path}: not JSON"
        );
    }
    // The collection's own styles keep links without a parameter.
    let (status, _) = get(
        &app,
        &format!("{BASE}/collections/{ID}/styles/default/map/tiles"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
