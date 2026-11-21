//! Every link to a style resolves (#1378): a crawler over a collection's
//! description on the shared OGC API root and on the per-API Maps and Tiles
//! services, and over WMS GetCapabilities, gets no 404. A parameter's
//! built-in `contours` lives only on that parameter's style layer, so its
//! links, the tilesets they list and their tile templates name the
//! parameter, and the style routes resolve it there. A collection only
//! Tiles draws has its legends at the shared root too (#792).

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

#[path = "../../api-common/tests/support/openapi_schema.rs"]
mod openapi_schema;

const BASE: &str = "https://example.test";
const ID: &str = "nwp";
/// A collection only Tiles draws (#792): its `apis` list `tiles` and no
/// other map API, so neither Maps nor WMS registers it.
const TILES_ONLY: &str = "nwp-tiles";
/// The map APIs a collection lists unless it is [`TILES_ONLY`].
const EVERY_MAP_API: &[&str] = &["maps", "tiles", "wms"];

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
    registries_over(&[(ID, &[], EVERY_MAP_API), (TILES_ONLY, &["a/b"], &["tiles"])])
}

/// Collections over [`Model`], each `(id, styles, apis)` with its `styles`
/// as `[[wms.styles]]` over a palette, and the style layers the server
/// builds from them.
fn registries_over(collections: &[(&str, &[&str], &[&str])]) -> Registries {
    let ctx = StyleContext::with_builtins();
    let (mut configs, mut engines, mut styles) = (HashMap::new(), HashMap::new(), HashMap::new());
    for &(id, named, apis) in collections {
        let mut config = serde_json::json!({
            "id": id, "title": "NWP", "description": "A model",
            "apis": apis, "engine_type": "grib",
        });
        if !named.is_empty() {
            config["wms"] = serde_json::json!({
                "styles": named
                    .iter()
                    .map(|name| serde_json::json!({"name": name, "colormap": "viridis"}))
                    .collect::<Vec<_>>(),
            });
        }
        let config: CollectionConfig = serde_json::from_value(config).unwrap();
        styles.extend(
            ctx.parameter_layer_styles(&config, None, &Model.raster_info().parameters, &|_, c| c)
                .unwrap(),
        );
        styles.insert(id.into(), ctx.collection_styles(&config, None).unwrap());
        engines.insert(id.to_string(), Arc::new(Model) as Arc<dyn MapEngine>);
        configs.insert(id.to_string(), config);
    }
    (configs, engines, styles)
}

/// What the `api` service registers of `registries`: the collections whose
/// `apis` list it, as the server enforces, with their engines and style
/// layers (`{id}` and `{id}/{parameter}`).
fn registered(api: &str, (mut configs, mut engines, mut styles): Registries) -> Registries {
    configs.retain(|_, config| config.apis.iter().any(|listed| listed == api));
    engines.retain(|id, _| configs.contains_key(id));
    styles.retain(|layer, _| {
        configs.keys().any(|id| {
            layer
                .strip_prefix(id.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    });
    (configs, engines, styles)
}

fn maps_state(registries: Registries) -> api_maps::AppState {
    let (collections, engines, styles) = registered("maps", registries);
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

fn tiles_state(registries: Registries) -> api_tiles::AppState {
    let (collections, map_engines, styles) = registered("tiles", registries);
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

fn wms_state(registries: Registries) -> api_wms::AppState {
    let (collections, engines, styles) = registered("wms", registries);
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
    app_over(registries)
}

/// [`app`] over the collections `registries` builds, once per service.
/// The shared root's Maps block reads a collection only Tiles draws from
/// the Tiles block's registry, as the server's `admin::shared_maps_block`
/// wires it (#792).
fn app_over(registries: impl Fn() -> Registries) -> Router {
    let tiles = tiles_state(registries());
    let shared = api_common::shared::SharedApi::new(
        "",
        vec![
            Arc::new(
                api_maps::MapsBlock::new(maps_state(registries())).with_style_fallback(
                    api_maps::StyleFallback::over(
                        tiles.clone(),
                        |tiles| &tiles.map_engines,
                        |tiles| &tiles.styles,
                    ),
                ),
            ),
            Arc::new(api_tiles::TilesBlock::new(tiles)),
        ],
        vec![],
    );
    Router::new()
        .nest(
            api_common::mounts::MAPS,
            api_maps::router(maps_state(registries())),
        )
        .nest(
            api_common::mounts::TILES,
            api_tiles::router(tiles_state(registries())),
        )
        .nest(
            api_common::mounts::WMS,
            api_wms::router(wms_state(registries())),
        )
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
/// resolves; the built-in `contours` among them names its parameter. So do
/// those of a collection only Tiles draws, its legends included (#792).
#[tokio::test]
async fn every_advertised_style_link_resolves() {
    let app = app();
    for collection in [
        format!("{BASE}/collections/{ID}"),
        format!("{BASE}/maps/collections/{ID}"),
        format!("{BASE}/maps/collections/{ID}/styles"),
        format!("{BASE}/tiles/collections/{ID}"),
        format!("{BASE}/collections/{TILES_ONLY}"),
        format!("{BASE}/tiles/collections/{TILES_ONLY}"),
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

/// Style names a path segment cannot carry as they are (#1517): every Maps
/// and Tiles style link, on the shared root and the per-API services,
/// percent-encodes the style name and the collection id, and every link it
/// leads to answers 200. The `/` of `a/b` reaches its route inside one
/// segment and draws that style; unencoded it names another resource. A
/// name of unreserved characters keeps its links byte for byte.
#[tokio::test]
async fn style_links_encode_names_a_path_segment_cannot_carry() {
    const PLAIN: &str = "Plain-1.0_x~y";
    const NAMES: [&str; 4] = ["a b", "a#b", "a/b", PLAIN];
    const SPACED: &str = "nwp eu";
    let app = app_over(|| {
        registries_over(&[
            (ID, &NAMES, EVERY_MAP_API),
            (SPACED, &["a/b"], EVERY_MAP_API),
        ])
    });
    fn segment(name: &str) -> &str {
        match name {
            "a b" => "a%20b",
            "a#b" => "a%23b",
            "a/b" => "a%2Fb",
            "nwp eu" => "nwp%20eu",
            plain => plain,
        }
    }
    for (id, names) in [(ID, &NAMES[..]), (SPACED, &["a/b"])] {
        let collection = segment(id);
        for (root, description) in [
            (BASE.to_string(), format!("{BASE}/collections/{collection}")),
            (
                format!("{BASE}/maps"),
                format!("{BASE}/maps/collections/{collection}"),
            ),
            (
                format!("{BASE}/maps"),
                format!("{BASE}/maps/collections/{collection}/styles"),
            ),
            (
                format!("{BASE}/tiles"),
                format!("{BASE}/tiles/collections/{collection}"),
            ),
        ] {
            let (status, body) = get(&app, &description).await;
            assert_eq!(status, StatusCode::OK, "{description}");
            let doc: Value = serde_json::from_slice(&body).unwrap();
            let (mut seen, mut statuses) = (BTreeSet::new(), Vec::new());
            for &name in names {
                let style = doc["styles"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["id"] == name)
                    .unwrap_or_else(|| panic!("{description} lists {name:?}"));
                let mut links = Vec::new();
                collect_links(&style["links"], &mut links);
                let prefix = format!("{root}/collections/{collection}/styles/{}/", segment(name));
                assert!(
                    !links.is_empty() && links.iter().all(|l| l.starts_with(&prefix)),
                    "{description} {name:?}: {links:?} not below {prefix}"
                );
                for link in links {
                    crawl(&app, link, 3, &mut seen, &mut statuses).await;
                }
            }
            let broken: Vec<_> = statuses
                .iter()
                .filter(|(_, status)| !status.is_success())
                .collect();
            assert!(broken.is_empty(), "{description}: {broken:?}");
        }
    }

    // The links of a plain name are the ones an unencoded name gave.
    let (_, body) = get(&app, &format!("{BASE}/collections/{ID}")).await;
    let doc: Value = serde_json::from_slice(&body).unwrap();
    let plain = doc["styles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == PLAIN)
        .unwrap();
    let mut links = Vec::new();
    collect_links(&plain["links"], &mut links);
    links.sort();
    let style = format!("{BASE}/collections/{ID}/styles/{PLAIN}");
    assert_eq!(
        links,
        [
            format!("{style}/legend"),
            format!("{style}/map"),
            format!("{style}/map/tiles"),
        ]
    );

    // `a%2Fb` is one segment, the style `a/b`, down to its tiles; `a/b`
    // unencoded is no style route at all.
    let (status, body) = get(
        &app,
        &format!("{BASE}/collections/{ID}/styles/a%2Fb/map/tiles"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["tilesets"][0]["style"]["id"], "a/b");
    let item = doc["tilesets"][0]["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == "item")
        .unwrap()["href"]
        .as_str()
        .unwrap();
    assert!(
        item.starts_with(&format!("{BASE}/collections/{ID}/styles/a%2Fb/map/tiles/")),
        "{item}"
    );
    let (status, body) = get(&app, &tile(item)).await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert!(body.starts_with(b"\x89PNG"), "{item}: not a PNG");
    for path in ["maps/collections", "tiles/collections", "collections"] {
        let raw = format!("{BASE}/{path}/{ID}/styles/a/b/legend");
        assert_eq!(get(&app, &raw).await.0, StatusCode::NOT_FOUND, "{raw}");
    }
}

/// `href` with `query` added to its query string.
fn with_query(href: &str, query: &str) -> String {
    let separator = if href.contains('?') { '&' } else { '?' };
    format!("{href}{separator}{query}")
}

/// A raster collection only Tiles draws has its map tiles and styled map
/// tilesets at the shared root, so its legends are there too (#792): each
/// style links the root's legend route once, `parameter-name` included
/// where only a parameter's layer offers it, and the link serves the JSON
/// and PNG legend the per-API Tiles service gives. The legend route is
/// Maps', and answers from Tiles' registry; nothing else of Maps is.
#[tokio::test]
async fn a_tiles_only_collection_has_its_legends_at_the_shared_root() {
    let app = app();
    let (status, body) = get(&app, &format!("{BASE}/collections/{TILES_ONLY}")).await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&body).unwrap();
    // Maps does not draw it: no map, no style list.
    for relation in [api_common::rel::MAP, api_common::rel::STYLES] {
        assert!(
            !doc["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["rel"] == relation),
            "{relation}: {doc}"
        );
    }
    let styles = doc["styles"].as_array().unwrap();
    let ids: Vec<&str> = styles.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert!(
        ["default", "a/b", "contours"]
            .iter()
            .all(|id| ids.contains(id)),
        "{ids:?}"
    );
    for style in styles {
        let id = style["id"].as_str().unwrap();
        let relations: Vec<&str> = style["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["rel"].as_str().unwrap())
            .collect();
        assert_eq!(
            relations,
            [api_common::rel::TILESETS_MAP, api_common::rel::LEGEND],
            "{id}"
        );
        let legend = style["links"][1]["href"].as_str().unwrap();
        let query = style["parameters"][0]
            .as_str()
            .map(|p| format!("?parameter-name={p}"))
            .unwrap_or_default();
        let segment = api_common::workbench::path_segment(id);
        assert_eq!(
            legend,
            format!("{BASE}/collections/{TILES_ONLY}/styles/{segment}/legend{query}"),
            "{id}"
        );
        assert_eq!(style["links"][1]["type"], "application/json", "{id}");
        let per_api = legend.replacen(
            &format!("{BASE}/collections/"),
            &format!("{BASE}/tiles/collections/"),
            1,
        );
        let (status, json) = get(&app, legend).await;
        assert_eq!(status, StatusCode::OK, "{legend}");
        let parsed: Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            get(&app, &per_api).await,
            (StatusCode::OK, json),
            "{legend}"
        );
        if id == "contours" {
            assert_eq!(parsed["contour"]["description"], "every 4 hPa");
        }
        let png = with_query(legend, "f=png");
        let (status, image) = get(&app, &png).await;
        assert_eq!(status, StatusCode::OK, "{png}");
        assert!(image.starts_with(b"\x89PNG"), "{png}: not a PNG");
        assert_eq!(
            get(&app, &with_query(&per_api, "f=png")).await,
            (StatusCode::OK, image),
            "{png}"
        );
    }

    // Its page's map shows that legend, not "This style advertises no
    // legend."
    let (status, page) = get(&app, &format!("{BASE}/collections/{TILES_ONLY}?f=html")).await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8(page).unwrap();
    let legend = format!("{BASE}/collections/{TILES_ONLY}/styles/default/legend");
    assert!(
        page.contains(&format!("&quot;legend&quot;:&quot;{legend}&quot;")),
        "the page's map has no legend for the default style"
    );

    // The legend route keeps its rules for the fallback's collections.
    let legend = format!("{BASE}/collections/{TILES_ONLY}/styles");
    for (path, status) in [
        (format!("{legend}/contours/legend"), StatusCode::NOT_FOUND),
        (format!("{legend}/nope/legend"), StatusCode::NOT_FOUND),
        (
            format!("{legend}/default/legend?parameter-name=nope"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("{legend}/default/legend?other=1"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("{legend}/default/legend?f=svg"),
            StatusCode::BAD_REQUEST,
        ),
        // Nothing else of Maps answers for it: not the style list, not a
        // styled map, not the per-API Maps legend.
        (legend.clone(), StatusCode::NOT_FOUND),
        (format!("{legend}/default/map"), StatusCode::NOT_FOUND),
        (
            format!("{BASE}/maps/collections/{TILES_ONLY}/styles/default/legend"),
            StatusCode::NOT_FOUND,
        ),
        // A collection neither registry draws is Maps' 404.
        (
            format!("{BASE}/collections/nope/styles/default/legend"),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let (got, body) = get(&app, &path).await;
        assert_eq!(got, status, "{path}");
        assert!(
            serde_json::from_slice::<Value>(&body).is_ok(),
            "{path}: not JSON"
        );
    }

    // A collection Maps draws keeps Maps' style links, one legend each
    // (the Tiles block's is the same link, merged away), and Maps' legend.
    let (_, body) = get(&app, &format!("{BASE}/collections/{ID}")).await;
    let doc: Value = serde_json::from_slice(&body).unwrap();
    for style in doc["styles"].as_array().unwrap() {
        let relations: Vec<&str> = style["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["rel"].as_str().unwrap())
            .collect();
        assert_eq!(
            relations,
            [
                api_common::rel::MAP,
                api_common::rel::LEGEND,
                api_common::rel::TILESETS_MAP
            ],
            "{style}"
        );
    }
    let maps_legend = format!("{BASE}/maps/collections/{ID}/styles/default/legend");
    assert_eq!(
        get(
            &app,
            &format!("{BASE}/collections/{ID}/styles/default/legend")
        )
        .await,
        get(&app, &maps_legend).await
    );
}

/// The shared root's OpenAPI document describes the legend of a collection
/// only Tiles draws, once, and keeps the Maps block's description of the
/// legend of a collection Maps draws (#792).
#[tokio::test]
async fn the_shared_definition_describes_every_legend_the_root_serves() {
    let app = app();
    let (status, body) = get(&app, &format!("{BASE}/api")).await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&body).unwrap();
    openapi_schema::assert_valid(&doc, "shared root");
    let paths = doc["paths"].as_object().unwrap();
    let legend = &paths[&format!("/collections/{TILES_ONLY}/styles/{{styleId}}/legend")]["get"];
    assert_eq!(
        legend["operationId"],
        format!("getStyleLegend_{TILES_ONLY}")
    );
    assert_eq!(
        legend["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/legend"
    );
    assert!(doc["components"]["schemas"]["legend"].is_object());
    let operations: Vec<&str> = paths
        .values()
        .flat_map(|item| item.as_object().unwrap().values())
        .filter_map(|operation| operation["operationId"].as_str())
        .collect();
    let unique: HashSet<&str> = operations.iter().copied().collect();
    assert_eq!(unique.len(), operations.len(), "duplicate operationId");
    let (_, body) = get(&app, &format!("{BASE}/maps/api")).await;
    let maps: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        paths[&format!("/collections/{ID}/styles/{{styleId}}/legend")],
        maps["paths"][&format!("/maps/collections/{ID}/styles/{{styleId}}/legend")]
    );
}
