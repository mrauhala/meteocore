use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_maps::MapsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockMapEngine;

impl MockMapEngine {
    fn new() -> Self {
        Self
    }

    fn make_info() -> RasterInfo {
        RasterInfo {
            // CRS:84 (lon-first) is what real WGS84 engines emit.
            native_crs: "CRS:84".to_string(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![
                chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                chrono::DateTime::parse_from_rfc3339("2024-01-01T01:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ],
            parameter: "reflectivity".to_string(),
            unit: "dBZ".to_string(),
            parameters: vec![],
            vertical: None,
            // bbox [10,55,30,70] over 2000x1500 cells => 0.01° per cell.
            grid_size: Some([2000, 1500]),
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

impl MapEngine for MockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        let values: Vec<Option<f64>> = (0..pixel_count)
            .map(|i| Some(i as f64 / pixel_count as f64))
            .collect();
        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        Self::make_info()
    }
}

/// A `MapEngine` whose `get_raster_tile` always returns `InvalidParameter`
/// — mirrors a multi-parameter PVOL collection rendered without a
/// `<site>:<quantity>` selection. Used to verify the handler classifies a
/// client mistake as 400, not 500.
struct InvalidParamEngine;

impl MapEngine for InvalidParamEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        _width: u32,
        _height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        Err(DataServerError::InvalidParameter(
            "collection requires a `<site>:<quantity>` parameter (e.g. `fivih:DBZH`)".into(),
        ))
    }

    fn raster_info(&self) -> RasterInfo {
        MockMapEngine::make_info()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_router() -> axum::Router {
    build_router_with_apis(vec!["maps".to_string()])
}

/// Build a Maps router backed by a caller-supplied engine (otherwise
/// identical to `build_router`), for exercising engine error paths.
fn build_router_with_engine(engine: Arc<dyn MapEngine>) -> axum::Router {
    build_router_with_engine_and_wms(engine, None)
}

/// [`build_router_with_engine`] with the collection's `[wms]` config, for
/// the per-collection settings the handler reads (e.g. `webp_quality`).
fn build_router_with_engine_and_wms(
    engine: Arc<dyn MapEngine>,
    wms: Option<ds_core::config::WmsConfig>,
) -> axum::Router {
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();
    engines.insert("radar".to_string(), engine);
    collections.insert(
        "radar".to_string(),
        CollectionConfig {
            id: "radar".to_string(),
            title: "Test Radar".to_string(),
            description: "Test radar data".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "odim-volume".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );
    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("radar".to_string(), layer_styles);
    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    }));
    api_maps::router(state)
}

/// `apis` is the collection config; the Tiles service registers the
/// collection for raster tiles exactly when `apis` lists it, as at load time.
fn build_router_with_apis(apis: Vec<String>) -> axum::Router {
    let map_tilesets = apis.iter().any(|a| a == "tiles");
    build_router_with_tilesets(apis, map_tilesets)
}

fn build_router_with_tilesets(apis: Vec<String>, map_tilesets: bool) -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MockMapEngine::new());
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("radar".to_string(), engine);
    collections.insert(
        "radar".to_string(),
        CollectionConfig {
            id: "radar".to_string(),
            title: "Test Radar".to_string(),
            description: "Test radar data".to_string(),
            data_path: None,
            apis,
            engine_type: "geotiff".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );

    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap.clone(),
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    layer_styles.insert(
        "grayscale".to_string(),
        StyleInfo {
            name: "grayscale".to_string(),
            title: "Grayscale".to_string(),
            palette: ds_render::builtin_palette_arc("grayscale").unwrap(),
            colormap: Arc::new(LutColorMap::from_builtin(
                BuiltinColormap::Grayscale,
                0.0,
                1.0,
            )),
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("radar".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: if map_tilesets {
            HashSet::from(["radar".to_string()])
        } else {
            HashSet::new()
        },
    }));
    api_maps::router(state)
}

async fn get(uri: &str) -> (StatusCode, Value) {
    get_on(build_router(), uri).await
}

async fn get_with_apis(uri: &str, apis: Vec<String>) -> (StatusCode, Value) {
    get_on(build_router_with_apis(apis), uri).await
}

async fn get_on(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

async fn get_raw(uri: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let app = build_router();
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

/// Fetch the collection JSON for an ad-hoc single-engine router. Used by the
/// extent edge-case tests that need a bespoke `RasterInfo`.
async fn fetch_collection_json(engine: Arc<dyn MapEngine>, id: &str, apis: Vec<String>) -> Value {
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert(id.to_string(), engine);
    collections.insert(
        id.to_string(),
        CollectionConfig {
            id: id.to_string(),
            title: "Test".to_string(),
            description: "Test".to_string(),
            data_path: None,
            apis,
            engine_type: "geotiff".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );
    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    }));
    let (_, json) = get_on(api_maps::router(state), &format!("/collections/{id}")).await;
    json
}

/// Fetch a collection's JSON with explicit keywords + license configured.
fn router_with(
    keywords: Vec<String>,
    license: Option<ds_core::config::LicenseConfig>,
) -> axum::Router {
    let id = "radar";
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    engines.insert(
        id.to_string(),
        Arc::new(MockMapEngine::new()) as Arc<dyn MapEngine>,
    );
    collections.insert(
        id.to_string(),
        CollectionConfig {
            id: id.to_string(),
            title: "Test".to_string(),
            description: "Test".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "geotiff".to_string(),
            keywords,
            license,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );
    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    }));
    api_maps::router(state)
}

async fn fetch_collection_json_with(
    keywords: Vec<String>,
    license: Option<ds_core::config::LicenseConfig>,
) -> Value {
    let (_, json) = get_on(router_with(keywords, license), "/collections/radar").await;
    json
}

/// Fetch the HTML collection-detail page as a raw string.
async fn fetch_collection_html_with(license: Option<ds_core::config::LicenseConfig>) -> String {
    let app = router_with(Vec::new(), license);
    let req = Request::builder()
        .uri("/collections/radar?f=html")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(body.to_vec()).unwrap()
}

mod metadata_extras {
    use super::*;

    #[tokio::test]
    async fn keywords_appear_in_collection_json() {
        let json = fetch_collection_json_with(vec!["radar".into(), "weather".into()], None).await;
        assert_eq!(json["keywords"], serde_json::json!(["radar", "weather"]));
    }

    #[tokio::test]
    async fn keyword_is_matched_by_q_search() {
        // End-to-end: config.keywords -> rows tuple -> CollectionMatch.keywords.
        // "thunderstorm" is in neither title nor description, so a match proves
        // the keyword wiring (a wrong tuple index would silently return 0).
        let (_, hit) = get_on(
            router_with(vec!["thunderstorm".into()], None),
            "/collections?q=thunderstorm",
        )
        .await;
        assert_eq!(hit["numberMatched"].as_u64(), Some(1));
        let (_, miss) = get_on(
            router_with(vec!["thunderstorm".into()], None),
            "/collections?q=zzznotaword",
        )
        .await;
        assert_eq!(miss["numberMatched"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn freetext_license_shows_name_in_html_but_no_json_link() {
        // A free-text license (no resolvable URL) must surface its name on the
        // HTML page (as plain text, no <a>) yet produce no JSON `rel="license"`
        // link — the cross-output behavior the docs promise (review on PR #324).
        let lic = ds_core::config::LicenseConfig {
            title: "All rights reserved".into(),
            url: None,
        };
        let html = fetch_collection_html_with(Some(lic.clone())).await;
        assert!(html.contains("License: All rights reserved"));
        assert!(!html.contains("License: <a"));

        let json = fetch_collection_json_with(Vec::new(), Some(lic)).await;
        assert!(json["links"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["rel"] != "license"));
    }

    #[tokio::test]
    async fn no_keywords_field_when_empty() {
        let json = fetch_collection_json_with(Vec::new(), None).await;
        assert!(json.get("keywords").is_none());
    }

    #[tokio::test]
    async fn license_link_uses_explicit_url() {
        let lic = ds_core::config::LicenseConfig {
            title: "CC-BY 4.0".into(),
            url: Some("https://example/lic".into()),
        };
        let json = fetch_collection_json_with(Vec::new(), Some(lic)).await;
        let link = json["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["rel"] == "license")
            .expect("a rel=license link");
        assert_eq!(link["href"], "https://example/lic");
        assert_eq!(link["title"], "CC-BY 4.0");
    }

    #[tokio::test]
    async fn license_link_synthesizes_spdx_url() {
        let lic = ds_core::config::LicenseConfig {
            title: "Apache-2.0".into(),
            url: None,
        };
        let json = fetch_collection_json_with(Vec::new(), Some(lic)).await;
        let link = json["links"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["rel"] == "license")
            .expect("a rel=license link");
        assert_eq!(link["href"], "https://spdx.org/licenses/Apache-2.0.html");
    }

    #[tokio::test]
    async fn no_license_link_without_license() {
        let json = fetch_collection_json_with(Vec::new(), None).await;
        assert!(json["links"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["rel"] != "license"));
    }
}

// ---------------------------------------------------------------------------
// Landing page tests
// ---------------------------------------------------------------------------

mod landing_page {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_title() {
        let (_, json) = get("/").await;
        assert!(json["title"].is_string());
    }

    #[tokio::test]
    async fn has_required_links() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == "conformance"));
        assert!(links.iter().any(|l| l["rel"] == "data"));
    }
}

// ---------------------------------------------------------------------------
// Conformance tests
// ---------------------------------------------------------------------------

mod conformance {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/conformance").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_conforms_to() {
        let (_, json) = get("/conformance").await;
        assert!(json["conformsTo"].is_array());
    }

    #[tokio::test]
    async fn declares_core() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/core")));
    }

    #[tokio::test]
    async fn declares_collection_map() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/collection-map")));
    }

    #[tokio::test]
    async fn declares_styled_map() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/styled-map")));
    }

    #[tokio::test]
    async fn declares_png() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/png")));
    }

    #[tokio::test]
    async fn omits_map_tilesets_conformance_class() {
        // We link to map tilesets (tilesets-map rel) but do NOT implement the
        // Map Tilesets class's /map/tiles endpoints, so the class must not be
        // declared — that would be a false conformance claim. (Tiles are
        // served by the standalone OGC API Tiles service.)
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(!classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/tilesets")));
    }

    #[tokio::test]
    async fn declares_ogcapi_common_part1_and_part2() {
        // OGC API - Common Part 1 (Core) + Part 2 (Collections, JSON) — the
        // landing page / conformance / collections resources satisfy them, so
        // they are advertised for discovery (#291).
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        let has = |needle: &str| classes.iter().any(|c| c.as_str().unwrap().contains(needle));
        assert!(has("ogcapi-common-1/1.0/conf/core"), "Common Part 1 Core");
        assert!(
            has("ogcapi-common-2/1.0/conf/collections"),
            "Common Part 2 Collections"
        );
        assert!(has("ogcapi-common-2/1.0/conf/json"), "Common Part 2 JSON");
    }

    #[tokio::test]
    async fn declares_common_html_class() {
        // HTML representation of the metadata endpoints is now served via
        // `?f=html` / Accept, so the Common Part 2 HTML class is declared (#296).
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("common-2/1.0/conf/html")));
    }
}

// ---------------------------------------------------------------------------
// Collections tests
// ---------------------------------------------------------------------------

mod collections {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_collections_array() {
        let (_, json) = get("/collections").await;
        assert!(json["collections"].is_array());
        assert!(!json["collections"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn each_collection_has_id_and_title() {
        let (_, json) = get("/collections").await;
        for c in json["collections"].as_array().unwrap() {
            assert!(c["id"].is_string());
            assert!(c["title"].is_string());
        }
    }

    #[tokio::test]
    async fn collection_has_crs_and_styles() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["crs"].is_array());
        assert!(c["styles"].is_array());
    }

    #[tokio::test]
    async fn collection_has_extent() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["extent"]["spatial"]["bbox"].is_array());
    }

    #[tokio::test]
    async fn collection_has_temporal_extent() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["extent"]["temporal"]["interval"].is_array());
    }

    #[tokio::test]
    async fn collection_has_links() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        let links = c["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == api_common::rel::MAP));
        assert!(links.iter().any(|l| l["rel"] == api_common::rel::STYLES));
    }

    #[tokio::test]
    async fn collection_detail_returns_200() {
        let (status, _) = get("/collections/radar").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn collection_detail_has_id() {
        let (_, json) = get("/collections/radar").await;
        assert_eq!(json["id"], "radar");
    }

    #[tokio::test]
    async fn collection_omits_nonstandard_apis_field() {
        // `apis` is a vendor extension with no OGC schema; it must not leak
        // into the standard collection JSON.
        let (_, json) = get("/collections/radar").await;
        assert!(
            json.get("apis").is_none(),
            "apis must not be present in the standard collection JSON"
        );
    }

    #[tokio::test]
    async fn collection_advertises_storage_crs() {
        let (_, json) = get("/collections/radar").await;
        // WGS84 data is lon-first -> CRS84 URI, not the lat-first EPSG:4326 one.
        assert_eq!(
            json["storageCrs"],
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        );
    }

    #[tokio::test]
    async fn spatial_extent_has_grid_resolution() {
        let (_, json) = get("/collections/radar").await;
        let grid = json["extent"]["spatial"]["grid"]
            .as_array()
            .expect("spatial.grid must be present");
        assert_eq!(grid.len(), 2, "one grid axis per spatial dimension");
        // bbox [10,55,30,70] over 2000x1500 cells => 0.01° per cell.
        assert_eq!(grid[0]["cellsCount"], 2000);
        assert_eq!(grid[1]["cellsCount"], 1500);
        assert!((grid[0]["resolution"].as_f64().unwrap() - 0.01).abs() < 1e-9);
        assert!((grid[1]["resolution"].as_f64().unwrap() - 0.01).abs() < 1e-9);
    }

    #[tokio::test]
    async fn temporal_extent_has_regular_grid_resolution() {
        let (_, json) = get("/collections/radar").await;
        let grid = &json["extent"]["temporal"]["grid"];
        // Two timestamps one hour apart => regular PT1H step.
        assert_eq!(grid["cellsCount"], 2);
        assert_eq!(grid["resolution"], "PT1H");
    }

    #[tokio::test]
    async fn collection_omits_tilesets_map_link_without_tiles_api() {
        let (_, json) = get("/collections/radar").await;
        let links = json["links"].as_array().unwrap();
        assert!(!links
            .iter()
            .any(|l| l["rel"] == "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map"));
    }

    #[tokio::test]
    async fn collection_links_carry_registered_relations() {
        // Maps Req 46 (collection → map), Req 53 (styled maps) and the legend
        // recommendation name registered relations; the Maps test suite only
        // finds maps through them. The unregistered short forms are gone.
        let (_, json) = get("/collections/radar").await;
        let href = |links: &Value, rel: &str| {
            links
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["rel"] == rel)
                .map(|l| l["href"].as_str().unwrap().to_owned())
        };
        let links = &json["links"];
        assert!(href(links, api_common::rel::MAP).is_some());
        assert!(href(links, api_common::rel::STYLES).is_some());
        assert_eq!(href(links, "map"), None);
        assert_eq!(href(links, "styles"), None);
        let styles = json["styles"].as_array().unwrap();
        assert!(!styles.is_empty());
        for style in styles {
            let links = &style["links"];
            assert!(href(links, api_common::rel::MAP).is_some());
            assert!(href(links, api_common::rel::LEGEND).is_some());
            assert_eq!(href(links, "map"), None);
            assert_eq!(href(links, "legend"), None);
        }
    }

    #[tokio::test]
    async fn collection_omits_tilesets_map_link_when_tiles_did_not_register_it() {
        // `apis` may list tiles for a collection the Tiles service does not
        // render as map tiles; the link must follow the Tiles registry (#789).
        let app = build_router_with_tilesets(vec!["maps".into(), "tiles".into()], false);
        let (_, json) = get_on(app, "/collections/radar").await;
        assert!(!json["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map"));
    }

    #[tokio::test]
    async fn collection_advertises_tilesets_map_link_with_tiles_api() {
        let (_, json) =
            get_with_apis("/collections/radar", vec!["maps".into(), "tiles".into()]).await;
        let links = json["links"].as_array().unwrap();
        let tileset_link = links
            .iter()
            .find(|l| l["rel"] == "http://www.opengis.net/def/rel/ogc/1.0/tilesets-map")
            .expect("tilesets-map link must be present when tiles API is enabled");
        assert!(tileset_link["href"]
            .as_str()
            .unwrap()
            .ends_with("/tiles/collections/radar/tiles"));
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Styles tests
// ---------------------------------------------------------------------------

mod styles_endpoint {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections/radar/styles").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_styles_array() {
        let (_, json) = get("/collections/radar/styles").await;
        let styles = json["styles"].as_array().unwrap();
        assert!(!styles.is_empty());
    }

    #[tokio::test]
    async fn default_style_present() {
        let (_, json) = get("/collections/radar/styles").await;
        let styles = json["styles"].as_array().unwrap();
        assert!(styles.iter().any(|s| s["id"] == "default"));
    }

    #[tokio::test]
    async fn grayscale_style_present() {
        let (_, json) = get("/collections/radar/styles").await;
        let styles = json["styles"].as_array().unwrap();
        assert!(styles.iter().any(|s| s["id"] == "grayscale"));
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/styles").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Every style advertises its machine-readable legend, so a client that
    /// listed the styles can fetch the palette without guessing the URL.
    #[tokio::test]
    async fn every_style_links_its_legend() {
        let (_, json) = get("/collections/radar/styles").await;
        for style in json["styles"].as_array().unwrap() {
            let id = style["id"].as_str().unwrap();
            let legend = style["links"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["rel"] == api_common::rel::LEGEND)
                .unwrap_or_else(|| panic!("style {id} has no legend link"));
            assert_eq!(
                legend["href"],
                format!("/maps/collections/radar/styles/{id}/legend")
            );
            assert_eq!(legend["type"], "application/json");
        }
    }

    /// The collection metadata carries the same legend links as the styles
    /// listing — the two representations must not drift.
    #[tokio::test]
    async fn collection_metadata_styles_link_their_legends() {
        let (_, json) = get("/collections/radar").await;
        for style in json["styles"].as_array().unwrap() {
            let id = style["id"].as_str().unwrap();
            assert!(
                style["links"].as_array().unwrap().iter().any(|l| {
                    l["rel"] == api_common::rel::LEGEND
                        && l["href"] == format!("/maps/collections/radar/styles/{id}/legend")
                }),
                "style {id} in collection metadata has no legend link"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Style legend tests
// ---------------------------------------------------------------------------

mod style_legend {
    use super::*;

    /// `?f=` defaults to the machine-readable legend: value range,
    /// interpolation mode and one entry per palette stop.
    #[tokio::test]
    async fn defaults_to_json_description() {
        let (status, json) = get("/collections/radar/styles/default/legend").await;
        assert_eq!(status, StatusCode::OK);

        assert_eq!(json["style"], "default");
        assert_eq!(json["title"], "Default");
        // Resolved from the engine's raster_info().
        assert_eq!(json["parameter"], "reflectivity");
        assert_eq!(json["unit"], "dBZ");
        assert_eq!(json["min"], 0.0);
        assert_eq!(json["max"], 1.0);
        assert_eq!(json["interpolation"], "linear");

        let palette = ds_render::builtin_palette("viridis").unwrap();
        let stops = json["stops"].as_array().unwrap();
        assert_eq!(stops.len(), palette.stops.len());
        assert_eq!(stops[0]["value"], palette.stops[0].value);
        assert_eq!(stops[0]["color"], "#440154");
        // Builtin palettes define no explicit nodata colour — omitted, not null.
        assert!(json.get("nodataColor").is_none());
    }

    /// A named style describes its own palette, not the default's.
    #[tokio::test]
    async fn named_style_describes_its_own_palette() {
        let (status, json) = get("/collections/radar/styles/grayscale/legend").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["style"], "grayscale");
        assert_eq!(json["title"], "Grayscale");
        let stops = json["stops"].as_array().unwrap();
        assert_eq!(
            stops.len(),
            ds_render::builtin_palette("grayscale").unwrap().stops.len()
        );
        assert_eq!(stops[0]["color"], "#000000");
        assert_eq!(stops[stops.len() - 1]["color"], "#FFFFFF");
    }

    #[tokio::test]
    async fn png_returns_a_rendered_legend_image() {
        let (status, headers, body) =
            get_raw("/collections/radar/styles/default/legend?f=png").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("content-type").unwrap(), "image/png");
        // Cacheable for a day, but NOT immutable — palettes are hot-reloadable.
        assert_eq!(
            headers.get("cache-control").unwrap(),
            "public, max-age=86400"
        );
        assert!(
            body.starts_with(&[0x89, b'P', b'N', b'G']),
            "not a PNG: {:?}",
            &body[..4.min(body.len())]
        );
    }

    #[tokio::test]
    async fn unknown_style_returns_404() {
        let (status, _) = get("/collections/radar/styles/nope/legend").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/styles/default/legend").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unsupported_format_returns_400() {
        let (status, _) = get("/collections/radar/styles/default/legend?f=image/jpeg").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// The OpenAPI document advertises the endpoint (repo rule: every new
    /// endpoint updates `api_definition()`).
    #[tokio::test]
    async fn is_advertised_in_the_api_definition() {
        let (_, json) = get("/api").await;
        let path = &json["paths"]["/maps/collections/radar/styles/{styleId}/legend"]["get"];
        assert!(
            path.is_object(),
            "legend path missing from the OpenAPI spec"
        );
        assert_eq!(
            path["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/legend"
        );
        assert!(json["components"]["schemas"]["legend"].is_object());
    }
}

// ---------------------------------------------------------------------------
// GetMap tests
// ---------------------------------------------------------------------------

mod get_map {
    use super::*;

    #[tokio::test]
    async fn returns_png() {
        let (status, headers, body) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert_eq!(ct, "image/png");
        // Check PNG magic bytes
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn has_cache_headers() {
        let (_, headers, _) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        assert!(headers.contains_key("cache-control"));
        assert!(headers.contains_key("etag"));
    }

    /// `elevation` against a collection with no vertical dimension
    /// (`MockMapEngine.raster_info().vertical` is `None`) is a 400.
    #[tokio::test]
    async fn elevation_against_non_vertical_collection_returns_400() {
        let (status, _, _) = get_raw("/collections/radar/map?bbox=10,55,30,70&elevation=0.5").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn explicit_time_immutable_cache() {
        let (_, headers, _) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&datetime=2024-01-01T00:00:00Z").await;
        let cc = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cc.contains("immutable"));
    }

    #[tokio::test]
    async fn no_time_short_cache() {
        let (_, headers, _) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        let cc = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cc.contains("must-revalidate"));
    }

    #[tokio::test]
    async fn custom_dimensions() {
        let (status, _, body) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&width=128&height=128").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn jpeg_format() {
        let (status, headers, body) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&f=image/jpeg").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert_eq!(ct, "image/jpeg");
        assert!(body[0] == 0xFF && body[1] == 0xD8);
    }

    #[tokio::test]
    async fn missing_bbox_returns_400() {
        let (status, json) = get("/collections/radar/map").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["code"].is_string());
    }

    #[tokio::test]
    async fn invalid_bbox_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=invalid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// An engine `InvalidParameter` from `get_raster_tile` (e.g. a
    /// multi-parameter PVOL collection rendered without a
    /// `<site>:<quantity>` selection) is a **400 with the engine's
    /// message**, not a 500 that hides it. Regression for the reported
    /// "internal server error" on a parameterless PVOL maps request.
    #[tokio::test]
    async fn render_invalid_parameter_is_400_with_message() {
        let app = build_router_with_engine(Arc::new(InvalidParamEngine));
        let (status, json) = get_on(
            app,
            "/collections/radar/map?bbox=10,55,30,70&width=64&height=48",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["code"], "BadRequest");
        assert!(
            json["description"]
                .as_str()
                .unwrap_or_default()
                .contains("parameter"),
            "the helpful engine message must reach the client, got {json}"
        );
    }

    #[tokio::test]
    async fn unsupported_crs_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&crs=EPSG:9999").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unsupported_format_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&f=image/gif").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dimension_too_large_returns_400() {
        let (status, _) = get("/collections/radar/map?bbox=10,55,30,70&width=9000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn parameter_name_query_is_accepted_for_single_param_engine() {
        // `MockMapEngine.raster_info().parameters` is empty (single-band).
        // The handler accepts any `parameter-name=` and forwards it; the
        // engine ignores the value at render time per the trait contract.
        let (status, _, body) =
            get_raw("/collections/radar/map?bbox=10,55,30,70&parameter-name=anything").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    /// Regression for #145: the response `ETag` must be FNV-1a over the
    /// rendered bytes — not over the cache key — so a server-side fix that
    /// produces different pixels under the same key surfaces a fresh ETag
    /// and clients holding the stale entry refetch instead of receiving an
    /// infinite 304. Direct check: build the same `CachedRendered` the
    /// handler does and verify the header matches.
    #[tokio::test]
    async fn etag_is_content_derived_over_response_body() {
        let (status, headers, body) = get_raw("/collections/radar/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        let actual_etag = headers.get("etag").unwrap().to_str().unwrap();
        let expected_etag = ds_render::CachedRendered::new(bytes::Bytes::from(body))
            .etag()
            .to_string();
        assert_eq!(
            actual_etag, expected_etag,
            "ETag header must be FNV-1a over the response body (content-derived), \
             not derived from the CacheKey — see #145"
        );
    }

    /// Pin the cache-HIT→304 branch specifically. The handler returns 304
    /// from two places: the cache-HIT branch (this test, asserted via
    /// `x-cache: HIT`) and the post-render MISS branch (which also
    /// compares `If-None-Match` against the freshly-computed ETag and
    /// returns 304 when matched). A fresh router would still 304 — just
    /// via the MISS path — so the `x-cache` assertion is what makes
    /// "we exercised the HIT branch" testable. Sharing one router across
    /// both calls is how we warm the cache for that branch.
    #[tokio::test]
    async fn if_none_match_after_cache_warm_returns_304_via_cache_hit() {
        let app = build_router();
        // First request populates the rendered cache.
        let resp_a = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/collections/radar/map?bbox=10,55,30,70")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = resp_a
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Cache is warm. Same key + matching If-None-Match → cache HIT →
        // ETag compare against `cached.etag()` → 304 with `x-cache: HIT`.
        let req = Request::builder()
            .uri("/collections/radar/map?bbox=10,55,30,70")
            .header("If-None-Match", &etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("HIT"),
            "304 must come from the cache-HIT branch, not post-render MISS"
        );
    }

    /// Pin the post-render MISS → 304 branch. Use a fresh router (no
    /// cache-warm) so the first `If-None-Match`-bearing request must
    /// go through the full render path; assert the 304 carries
    /// `x-cache: MISS` rather than the cache-HIT branch's `HIT`.
    #[tokio::test]
    async fn if_none_match_against_fresh_router_returns_304_via_miss_branch() {
        // Step 1: render once on a separate fresh router to learn the ETag.
        let etag = {
            let warm = build_router();
            let resp = warm
                .oneshot(
                    Request::builder()
                        .uri("/collections/radar/map?bbox=10,55,30,70")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            resp.headers()
                .get("etag")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        // Step 2: brand-new router with an empty cache. Handler must
        // render, compute the same content-derived ETag, match the
        // header, and 304 via the post-render branch.
        let app = build_router();
        let req = Request::builder()
            .uri("/collections/radar/map?bbox=10,55,30,70")
            .header("If-None-Match", &etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("MISS"),
            "304 must come from the post-render MISS branch, not the cache-HIT branch"
        );
    }

    /// Each map carries the render timing the server records per collection
    /// and outcome (#466): the first render is cold, the repeat a hit.
    #[tokio::test]
    async fn map_reports_render_outcome_per_collection() {
        use ds_executor::{RenderOutcome, RenderTiming};
        let app = build_router();
        for expected in [RenderOutcome::Cold, RenderOutcome::Hit] {
            let req = Request::builder()
                .uri("/collections/radar/map?bbox=10,55,30,70")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let timing = resp.extensions().get::<RenderTiming>().unwrap();
            assert_eq!(
                (timing.collection.as_str(), timing.outcome),
                ("radar", expected)
            );
        }
    }

    /// A cold map reports its admission, engine read and encode phases
    /// (#147); the cached repeat ran none of them.
    #[tokio::test]
    async fn map_reports_render_phases() {
        use ds_executor::RenderTiming;
        let app = build_router();
        let mut recorded = Vec::new();
        for _ in 0..2 {
            let req = Request::builder()
                .uri("/collections/radar/map?bbox=10,55,30,70")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let timing = resp.extensions().get::<RenderTiming>().unwrap();
            let phases: Vec<_> = timing.phases.iter().map(|(p, _)| p.as_str()).collect();
            recorded.push(phases);
        }
        assert_eq!(recorded[0], ["queue", "engine", "encode"]);
        assert!(recorded[1].is_empty(), "a hit runs no phase");
    }

    /// Cross-parameter staleness protection: different `parameter-name`
    /// values must produce different rendered bytes (because the
    /// `MultiParamMockEngine` varies its output by parameter), which under
    /// content-derived ETags (#145) means different ETags. Combined with
    /// `parameter` being part of the cache key, a client switching
    /// parameters can't get a 304 against the previous parameter's entry.
    #[tokio::test]
    async fn parameter_name_changes_content_etag_on_multi_param_engine() {
        let app = build_multi_param_router();
        let resp_2t = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=2t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp_10u = app
            .oneshot(
                Request::builder()
                    .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=10u")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag_2t = resp_2t.headers().get("etag").unwrap().to_str().unwrap();
        let etag_10u = resp_10u.headers().get("etag").unwrap().to_str().unwrap();
        assert_ne!(
            etag_2t, etag_10u,
            "parameter-name varies the rendered pixels, so the content-derived \
             ETag must differ — otherwise a client switching from 2t to 10u \
             would get a stale 304"
        );
    }

    #[tokio::test]
    async fn unknown_parameter_name_returns_400_for_multi_param_engine() {
        let app = build_multi_param_router();
        let req = Request::builder()
            .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=nope")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        // Sorted alphabetically — "10u" < "2t" by lexicographic comparison.
        assert!(
            text.contains("Available: 10u, 2t"),
            "error body should list available parameters in sorted order; got: {text}"
        );
    }

    #[tokio::test]
    async fn known_parameter_name_is_accepted_by_multi_param_engine() {
        let app = build_multi_param_router();
        let req = Request::builder()
            .uri("/collections/wx/map?bbox=10,55,30,70&parameter-name=2t")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Regression for #162: when the engine produces an all-nodata tile,
    /// the fast path emits PNG bytes without going through the
    /// format-aware encoder. Before this fix the response carried the
    /// *requested* Content-Type (e.g. image/jpeg) over PNG bytes,
    /// breaking decoders that trust the header. Both header and body
    /// must agree.
    #[tokio::test]
    async fn empty_tile_forces_png_content_type_even_when_jpeg_requested() {
        let app = build_empty_router();
        let req = Request::builder()
            .uri("/collections/empty/map?bbox=10,55,30,70&f=image/jpeg")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(
            headers.get("content-type").unwrap(),
            "image/png",
            "empty-tile response must self-declare PNG, not the requested image/jpeg"
        );
        assert_eq!(headers.get("x-cache").unwrap(), "EMPTY");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(
            body.starts_with(&[0x89, b'P', b'N', b'G']),
            "body must be a real PNG, got first bytes {:?}",
            &body[..4.min(body.len())]
        );
    }

    #[tokio::test]
    async fn empty_tile_forces_png_content_type_even_when_webp_requested() {
        // Same regression as above, second format for symmetry — WebP
        // takes a different `ImageFormat::Webp` branch in the cache
        // key, so this exercises a distinct code path.
        let app = build_empty_router();
        let req = Request::builder()
            .uri("/collections/empty/map?bbox=10,55,30,70&f=image/webp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(headers.get("content-type").unwrap(), "image/png");
        assert_eq!(headers.get("x-cache").unwrap(), "EMPTY");
    }

    /// Revalidating a cached empty-tile response must round-trip the
    /// `EMPTY` label. Empty tiles bypass `rendered_cache`, so an
    /// `If-None-Match` request always falls through to the post-render
    /// branch — exactly the branch the round-7 fix targets.
    #[tokio::test]
    async fn if_none_match_on_empty_tile_returns_304_with_x_cache_empty() {
        let app = build_empty_router();
        let uri = "/collections/empty/map?bbox=10,55,30,70&f=image/png";

        let etag = {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.headers().get("x-cache").unwrap(), "EMPTY");
            resp.headers()
                .get("etag")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("If-None-Match", &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get("etag").unwrap().to_str().unwrap(), etag);
        assert_eq!(
            resp.headers().get("x-cache").map(|v| v.to_str().unwrap()),
            Some("EMPTY"),
            "post-render 304 must forward the `x_cache` label from the \
             match arm, not hard-code `MISS` — otherwise revalidating an \
             empty tile silently changes its dashboard category"
        );
    }
}

/// Mock engine that returns an all-`None` (all-nodata) `RasterTile`.
/// Used to exercise the empty-tile fast path that bypasses the
/// format-aware encoder. The regression test for #162 lives in
/// `mod get_map` above.
struct EmptyMockMapEngine;

impl MapEngine for EmptyMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let pixel_count = (width * height) as usize;
        Ok(RasterTile {
            width,
            height,
            values: vec![None; pixel_count].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        MockMapEngine::make_info()
    }
}

fn build_empty_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(EmptyMockMapEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("empty".to_string(), engine);
    collections.insert(
        "empty".to_string(),
        CollectionConfig {
            id: "empty".to_string(),
            title: "Empty".to_string(),
            description: "All-nodata fixture for #162".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "geotiff".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );

    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("empty".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    }));
    api_maps::router(state)
}

struct MultiParamMockEngine;

impl MapEngine for MultiParamMockEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &ds_core::map_engine::OutputCrs,
        parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ds_core::map_engine::RasterTile, ds_core::error::DataServerError> {
        // Vary the pixel values by parameter so the `parameter` field on the
        // cache key actually changes the rendered bytes — the cross-parameter
        // ETag test depends on this. Fold the parameter name into a value
        // in [0, 1] (within the style's min/max range) so the colormap
        // produces a different uniform fill per parameter.
        let fill: f64 = parameter
            .map(|p| {
                let mut h: u64 = 0xcbf29ce484222325;
                for &b in p.as_bytes() {
                    h ^= b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                ((h & 0xff) as f64) / 255.0
            })
            .unwrap_or(0.0);
        let pixel_count = (width * height) as usize;
        let values: Vec<Option<f64>> = vec![Some(fill); pixel_count];
        Ok(ds_core::map_engine::RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> ds_core::map_engine::RasterInfo {
        ds_core::map_engine::RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([0.0, 0.0, 10.0, 10.0]),
            times: vec![],
            parameter: "2t".into(),
            unit: "K".into(),
            parameters: vec![
                ds_core::map_engine::ParameterInfo {
                    name: "2t".into(),
                    title: "Temperature".into(),
                    unit: "°C".into(),
                },
                ds_core::map_engine::ParameterInfo {
                    name: "10u".into(),
                    title: "U Wind".into(),
                    unit: "".into(),
                },
            ],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

fn build_multi_param_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MultiParamMockEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles_map = HashMap::new();

    engines.insert("wx".to_string(), engine);
    collections.insert(
        "wx".to_string(),
        CollectionConfig {
            id: "wx".to_string(),
            title: "Forecast".to_string(),
            description: "Test multi-param".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "grib".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );

    let cmap = Arc::new(LutColorMap::from_builtin(
        BuiltinColormap::Viridis,
        0.0,
        1.0,
    ));
    let mut layer_styles = HashMap::new();
    layer_styles.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
            palette: ds_render::builtin_palette_arc("viridis").unwrap(),
            colormap: cmap,
            min: 0.0,
            max: 1.0,
            parameter: None,
        },
    );
    styles_map.insert("wx".to_string(), layer_styles);

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles: styles_map,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    }));
    api_maps::router(state)
}

// ---------------------------------------------------------------------------
// Parameter discovery (#279)
// ---------------------------------------------------------------------------

mod parameter_discovery {
    use super::*;

    /// The collection advertises the valid `parameter-name` values, in EDR's
    /// `parameter_names` shape, sorted by name.
    #[tokio::test]
    async fn multi_parameter_collection_lists_its_parameters() {
        let (status, json) = get_on(build_multi_param_router(), "/collections/wx").await;
        assert_eq!(status, StatusCode::OK);
        let parameters = json["parameter_names"].as_object().unwrap();
        assert_eq!(parameters.keys().collect::<Vec<_>>(), ["10u", "2t"]);
        let t2 = &parameters["2t"];
        assert_eq!(t2["type"], "Parameter");
        assert_eq!(t2["observedProperty"]["label"]["en"], "Temperature");
        assert_eq!(t2["unit"]["symbol"]["value"], "°C");
        assert_eq!(
            parameters["10u"]["observedProperty"]["label"]["en"],
            "U Wind"
        );
        assert!(
            parameters["10u"].get("unit").is_none(),
            "unknown unit omitted"
        );

        let (_, list) = get_on(build_multi_param_router(), "/collections").await;
        assert_eq!(
            list["collections"][0]["parameter_names"],
            json["parameter_names"]
        );
    }

    /// A single-parameter collection ignores `parameter-name`: nothing to list.
    #[tokio::test]
    async fn single_parameter_collection_lists_no_parameters() {
        let (status, json) = get("/collections/radar").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.get("parameter_names").is_none());
    }

    /// Both render routes declare the selector they accept.
    #[tokio::test]
    async fn render_routes_declare_parameter_name() {
        let (_, api) = get_on(build_multi_param_router(), "/api").await;
        assert_eq!(
            api["components"]["parameters"]["parameter-name"]["name"],
            "parameter-name"
        );
        for path in [
            "/maps/collections/wx/map",
            "/maps/collections/wx/styles/{styleId}/map",
        ] {
            let parameters = api["paths"][path]["get"]["parameters"].as_array().unwrap();
            assert!(
                parameters
                    .iter()
                    .any(|p| p["$ref"] == "#/components/parameters/parameter-name"),
                "{path}: {parameters:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Styled map tests
// ---------------------------------------------------------------------------

mod styled_map {
    use super::*;

    #[tokio::test]
    async fn default_style_returns_png() {
        let (status, _, body) =
            get_raw("/collections/radar/styles/default/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn grayscale_style_returns_png() {
        let (status, _, body) =
            get_raw("/collections/radar/styles/grayscale/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[tokio::test]
    async fn unknown_style_returns_404() {
        let (status, _) = get("/collections/radar/styles/nonexistent/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/styles/default/map?bbox=10,55,30,70").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// TileMatrixSets tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Error response tests
// ---------------------------------------------------------------------------

mod errors {
    use super::*;

    #[tokio::test]
    async fn error_responses_have_code_and_description() {
        let (_, json) = get("/collections/nonexistent").await;
        assert!(json["code"].is_string());
        assert!(json["description"].is_string());
    }

    #[tokio::test]
    async fn bad_request_has_error_body() {
        let (status, json) = get("/collections/radar/map?bbox=invalid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["code"].is_string());
    }
}

// ---------------------------------------------------------------------------
// Vertical extent tests (radar elevation angle, OGC API Common Part 2 form)
// ---------------------------------------------------------------------------

mod vertical_extent {
    use super::*;
    use ds_core::vertical::{VerticalDimension, VerticalKind};

    struct VerticalMockEngine;

    impl MapEngine for VerticalMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![],
                parameter: "DBZH".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: Some(VerticalDimension::new(
                    VerticalKind::ElevationAngle,
                    vec![0.5, 1.5, 5.0],
                )),
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    async fn fetch_collection() -> Value {
        fetch_collection_json(
            Arc::new(VerticalMockEngine),
            "pvol",
            vec!["maps".to_string()],
        )
        .await
    }

    #[tokio::test]
    async fn vertical_extent_has_common_part2_form() {
        let json = fetch_collection().await;
        let v = &json["extent"]["vertical"];
        // Back-compat fields retained.
        assert_eq!(v["values"], serde_json::json!([0.5, 1.5, 5.0]));
        assert_eq!(v["interval"], serde_json::json!([[0.5, 5.0]]));
        // OGC API Common Part 2 additive form.
        assert_eq!(v["unit"], "deg");
        assert_eq!(v["grid"]["coordinates"], serde_json::json!([0.5, 1.5, 5.0]));
        // A Uniform Additional Dimension needs a reference system and a grid
        // cell count (Common Part 2); radar elevation angle has no registered
        // URI, so its vrs is the inline WKT2 EDR also advertises.
        assert_eq!(
            v["vrs"],
            ds_core::vertical::VerticalKind::ElevationAngle.vrs()
        );
        assert_eq!(v["grid"]["cellsCount"], 3);
    }

    /// A `VerticalDimension` with no levels must not emit `"interval": null`
    /// (invalid per OGC API Common Part 2) — the whole vertical extent is
    /// omitted instead.
    struct EmptyVerticalMockEngine;

    impl MapEngine for EmptyVerticalMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![],
                parameter: "DBZH".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: Some(VerticalDimension::new(VerticalKind::ElevationAngle, vec![])),
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn empty_vertical_dimension_omits_extent() {
        let json = fetch_collection_json(
            Arc::new(EmptyVerticalMockEngine),
            "empty-z",
            vec!["maps".to_string()],
        )
        .await;
        assert!(
            json["extent"].get("vertical").is_none(),
            "vertical extent must be omitted when there are no levels, got {:?}",
            json["extent"].get("vertical")
        );
    }
}

// ---------------------------------------------------------------------------
// Extent edge cases (storageCrs omission, temporal-grid jitter)
// ---------------------------------------------------------------------------

mod extent_edge_cases {
    use super::*;

    /// Engine whose native CRS is a projection with no canonical OGC URI
    /// (engines label these "TM"/"LAEA"/"projected"/…).
    struct ProjectedMockEngine;

    impl MapEngine for ProjectedMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "LAEA".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![],
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: Some([2000, 1500]),
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    /// Engine whose timestamps are genuinely hourly but carry ±1 s jitter on
    /// the first interval — the regression case for the gap-spread check.
    struct JitteredTimesMockEngine;

    impl MapEngine for JitteredTimesMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            // Gaps: 3599 s, 3601 s, 3600 s -> spread 2, mean 3600 -> PT1H.
            let t = |s: &str| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            };
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: vec![
                    t("2024-01-01T00:00:00Z"),
                    t("2024-01-01T00:59:59Z"),
                    t("2024-01-01T02:00:00Z"),
                    t("2024-01-01T03:00:00Z"),
                ],
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn storage_crs_and_spatial_grid_omitted_for_projected_native_crs() {
        let json =
            fetch_collection_json(Arc::new(ProjectedMockEngine), "proj", vec!["maps".into()]).await;
        // Mislabelling a projected grid as CRS84 is worse than omitting it.
        assert!(
            json.get("storageCrs").is_none(),
            "storageCrs must be absent for a native CRS with no OGC URI, got {:?}",
            json.get("storageCrs")
        );
        // The bbox is still advertised...
        assert!(json["extent"]["spatial"]["bbox"].is_array());
        // ...but not a CRS84-degree grid: a projected grid isn't degree-regular.
        assert!(
            json["extent"]["spatial"].get("grid").is_none(),
            "projected grids must not advertise a CRS84-degree spatial.grid"
        );
    }

    #[tokio::test]
    async fn temporal_grid_treats_jittered_series_as_regular() {
        let json = fetch_collection_json(
            Arc::new(JitteredTimesMockEngine),
            "jit",
            vec!["maps".into()],
        )
        .await;
        let grid = &json["extent"]["temporal"]["grid"];
        assert_eq!(grid["cellsCount"], 4);
        // Despite the ±1 s jitter on the first interval, the series is regular.
        assert_eq!(grid["resolution"], "PT1H");
        assert!(
            grid.get("coordinates").is_none(),
            "regular series must not fall back to a coordinates list"
        );
    }

    /// Engine whose bbox crosses the anti-meridian (east < west).
    struct AntiMeridianMockEngine;

    impl MapEngine for AntiMeridianMockEngine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            Ok(RasterTile {
                width,
                height,
                values: vec![None; (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                native_crs: "CRS:84".into(),
                // 20°-wide box straddling 180°: east (-170) < west (170).
                spatial_extent: Some([170.0, 60.0, -170.0, 70.0]),
                times: vec![],
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: Some([2000, 1000]),
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn spatial_grid_resolution_positive_across_antimeridian() {
        let json =
            fetch_collection_json(Arc::new(AntiMeridianMockEngine), "am", vec!["maps".into()])
                .await;
        let grid = json["extent"]["spatial"]["grid"].as_array().unwrap();
        // 20° / 2000 cells = 0.01, positive — not the -340°/2000 a naive
        // (east - west) would give.
        let lon_res = grid[0]["resolution"].as_f64().unwrap();
        assert!(lon_res > 0.0, "resolution must be positive, got {lon_res}");
        assert!((lon_res - 0.01).abs() < 1e-9);
    }
}

// ---------------------------------------------------------------------------
// OGC API - Common - Part 4: Searchable Collections (?bbox/datetime/q/limit)
// ---------------------------------------------------------------------------

mod searchable {
    use super::*;

    /// Build a Maps router with two collections so pagination links can be
    /// exercised end-to-end. Both are backed by the same mock engine
    /// (bbox [10,55,30,70], times 2024-01-01T00..01Z, title "Test Radar").
    fn build_router_two() -> axum::Router {
        let mut engines: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
        let mut collections = HashMap::new();
        for id in ["radar-a", "radar-b"] {
            engines.insert(id.to_string(), Arc::new(MockMapEngine::new()));
            collections.insert(
                id.to_string(),
                CollectionConfig {
                    id: id.to_string(),
                    title: "Test Radar".to_string(),
                    description: "Test radar data".to_string(),
                    data_path: None,
                    apis: vec!["maps".to_string()],
                    engine_type: "geotiff".to_string(),
                    keywords: Vec::new(),
                    license: None,
                    geotiff: None,
                    querydata: None,
                    wms: None,
                    grib: None,
                    zarr: None,
                    odim: None,
                    cap: None,
                    postgis: None,
                    nowcast: None,
                    bufr: None,
                    satellite: None,
                    preview: None,
                },
            );
        }
        let state = Arc::new(ArcSwap::from_pointee(MapsState {
            engines,
            collections,
            styles: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
            map_tileset_ids: Default::default(),
        }));
        api_maps::router(state)
    }

    #[tokio::test]
    async fn conformance_does_not_claim_incomplete_searchable_collections() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(!classes.iter().any(|c| c
            .as_str()
            .unwrap()
            .contains("common-4/1.0/conf/searchable-collections")));
    }

    #[tokio::test]
    async fn unfiltered_has_match_counts() {
        let (status, json) = get("/collections").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["numberMatched"], 1);
        assert_eq!(json["numberReturned"], 1);
        assert_eq!(json["collections"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn q_matches_title_word() {
        let (_, json) = get("/collections?q=radar").await;
        assert_eq!(json["numberMatched"], 1);
    }

    #[tokio::test]
    async fn q_no_match_excludes() {
        let (_, json) = get("/collections?q=zzznotaword").await;
        assert_eq!(json["numberMatched"], 0);
        assert_eq!(json["numberReturned"], 0);
        assert!(json["collections"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bbox_intersecting_includes() {
        let (_, json) = get("/collections?bbox=0,50,15,60").await;
        assert_eq!(json["numberMatched"], 1);
    }

    #[tokio::test]
    async fn bbox_disjoint_excludes() {
        let (_, json) = get("/collections?bbox=-50,-50,-40,-40").await;
        assert_eq!(json["numberMatched"], 0);
    }

    #[tokio::test]
    async fn datetime_within_extent_includes() {
        let (_, json) = get("/collections?datetime=2024-01-01T00:30:00Z").await;
        assert_eq!(json["numberMatched"], 1);
    }

    #[tokio::test]
    async fn datetime_outside_extent_excludes() {
        let (_, json) = get("/collections?datetime=2025-06-01T00:00:00Z").await;
        assert_eq!(json["numberMatched"], 0);
    }

    #[tokio::test]
    async fn invalid_limit_is_400() {
        let (status, _) = get("/collections?limit=0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_bbox_is_400() {
        let (status, _) = get("/collections?bbox=1,2,3").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn non_crs84_bbox_crs_is_400() {
        let (status, _) = get("/collections?bbox=0,0,1,1&bbox-crs=EPSG:3857").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pagination_first_page_has_next_not_prev() {
        let app = build_router_two();
        let (_, json) = get_on(app, "/collections?limit=1").await;
        assert_eq!(json["numberMatched"], 2);
        assert_eq!(json["numberReturned"], 1);
        let links = json["links"].as_array().unwrap();
        let next = links
            .iter()
            .find(|l| l["rel"] == "next")
            .expect("next link");
        assert!(next["href"].as_str().unwrap().contains("offset=1"));
        assert!(!links.iter().any(|l| l["rel"] == "prev"));
    }

    #[tokio::test]
    async fn pagination_second_page_has_prev_not_next() {
        let app = build_router_two();
        let (_, json) = get_on(app, "/collections?limit=1&offset=1").await;
        assert_eq!(json["numberReturned"], 1);
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "prev"));
        assert!(!links.iter().any(|l| l["rel"] == "next"));
    }

    #[tokio::test]
    async fn self_link_preserves_query() {
        let app = build_router_two();
        let (_, json) = get_on(app, "/collections?q=radar&limit=1").await;
        let links = json["links"].as_array().unwrap();
        let self_link = links
            .iter()
            .find(|l| l["rel"] == "self")
            .expect("self link");
        let href = self_link["href"].as_str().unwrap();
        assert!(href.contains("q=radar"));
        assert!(href.contains("limit=1"));
    }
}

// ---------------------------------------------------------------------------
// Content negotiation — ?f=json|html (OGC API Common Part 2 conf/html, #296)
// ---------------------------------------------------------------------------
mod content_negotiation {
    use super::*;

    #[tokio::test]
    async fn f_html_serves_html() {
        let (status, headers, body) = get_raw("/collections?f=html").await;
        assert_eq!(status, StatusCode::OK);
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
        assert!(String::from_utf8_lossy(&body).contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn collection_detail_serves_html() {
        let (status, headers, _) = get_raw("/collections/radar?f=html").await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));
    }

    #[tokio::test]
    async fn unknown_f_is_400() {
        let (status, _, _) = get_raw("/collections?f=xml").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accept_header_selects_html() {
        let app = build_router();
        let req = Request::builder()
            .uri("/collections")
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
    }

    /// Negotiated responses carry `Vary: Accept` so shared caches don't serve
    /// the wrong representation.
    #[tokio::test]
    async fn negotiated_responses_set_vary_accept() {
        for uri in [
            "/collections",
            "/collections?f=html",
            "/collections/radar",
            "/conformance",
            "/",
        ] {
            let (_, headers, _) = get_raw(uri).await;
            let vary = headers
                .get("vary")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(
                vary.to_ascii_lowercase().contains("accept"),
                "{uri}: missing Vary: Accept (got {vary:?})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Run-less map must track the engine's latest model run (#521)
// ---------------------------------------------------------------------------

/// Mock forecast engine whose run list can be advanced mid-test — the Maps
/// twin of the WMS `RunSwapMockMapEngine`. Maps never pins a run (the
/// `reference_time` query parameter is a #337 Phase 4 follow-up), so every
/// request exercises the `resolve_reference_time(time, None)` path.
struct RunSwapMockMapEngine {
    runs: Arc<std::sync::RwLock<Vec<chrono::DateTime<chrono::Utc>>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl MapEngine for RunSwapMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let idx = self.runs.read().unwrap().len().min(2);
        let v = 0.2 + 0.3 * idx as f64;
        Ok(RasterTile {
            width,
            height,
            values: vec![Some(v); (width * height) as usize].into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec!["2026-07-11T20:00:00Z".parse().unwrap()],
            parameter: "reflectivity".into(),
            unit: "dBZ".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: self.runs.read().unwrap().clone(),
        }
    }

    fn resolve_reference_time(
        &self,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        // Run-retaining engine contract (#521): None ⇒ the concrete run the
        // render would use (latest here).
        reference_time.or_else(|| self.runs.read().unwrap().last().copied())
    }
}

/// The #521 stale-run replay on the Maps path: the no-TTL rendered cache must
/// key on the concrete latest run so a new run (or nowcast generation)
/// re-renders instead of serving the first run's pixels forever.
#[tokio::test]
async fn map_re_renders_when_a_new_run_lands() {
    let runs = Arc::new(std::sync::RwLock::new(vec!["2026-07-11T12:00:00Z"
        .parse()
        .unwrap()]));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = build_router_with_engine(Arc::new(RunSwapMockMapEngine {
        runs: runs.clone(),
        calls: calls.clone(),
    }));
    let uri = "/collections/radar/map?bbox=10,55,30,70&width=128&height=128";

    // 1. Cold render under run 1.
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

    // 2. Repeat: pure cache hit under the concrete run-1 key.
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

    // 3. A new run supersedes the latest → the same request must re-render.
    //    Pre-#521 (key reference_time: None) this served the stale hit.
    runs.write()
        .unwrap()
        .push("2026-07-11T18:00:00Z".parse().unwrap());
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "new latest run must miss the run-1 cache entry and re-render"
    );
}

// ---------------------------------------------------------------------------
// Per-parameter style layers
// ---------------------------------------------------------------------------

/// A multi-parameter engine, so `parameter-name=` passes the handler's
/// validation against `raster_info().parameters` and the request exercises the
/// real multi-parameter shape. Pixels are the same 0..1 ramp `MockMapEngine`
/// produces, independent of the parameter — the point of these tests is which
/// COLORMAP the handler picks, not which data.
struct MultiParamMockMapEngine;

impl MapEngine for MultiParamMockMapEngine {
    fn get_raster_tile(
        &self,
        _bbox: [f64; 4],
        width: u32,
        height: u32,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        Ok(ramp_tile(width, height))
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            parameters: vec![
                ds_core::map_engine::ParameterInfo {
                    name: "T".to_string(),
                    title: "Temperature".to_string(),
                    unit: "°C".into(),
                },
                ds_core::map_engine::ParameterInfo {
                    name: "RH".to_string(),
                    title: "Relative humidity".to_string(),
                    unit: "%".into(),
                },
            ],
            ..MockMapEngine::make_info()
        }
    }
}

/// The tile `MultiParamMockMapEngine` returns, rebuilt so a test can render a
/// reference image through `ds-render` and compare bytes.
fn ramp_tile(width: u32, height: u32) -> RasterTile {
    let pixel_count = (width * height) as usize;
    let values: Vec<Option<f64>> = (0..pixel_count)
        .map(|i| Some(i as f64 / pixel_count as f64))
        .collect();
    RasterTile {
        width,
        height,
        values: values.into(),
    }
}

/// A `StyleInfo` over a named built-in palette, resolved through the same
/// `StyleContext` the server uses so `colormap` and `palette` agree.
fn palette_style(name: &str, palette: &str, parameter: Option<&str>) -> StyleInfo {
    let resolved = ds_render::StyleContext::with_builtins()
        .build_colormap(&ds_render::StyleSpec {
            colormap: Some(palette),
            ..Default::default()
        })
        .expect("built-in palette resolves");
    StyleInfo {
        name: name.to_string(),
        title: format!("{palette} over {name}"),
        colormap: resolved.colormap,
        palette: resolved.palette,
        min: resolved.min,
        max: resolved.max,
        parameter: parameter.map(str::to_string),
    }
}

/// The PNG a map/tile request must return when rendered with `palette`.
fn reference_png(width: u32, height: u32, palette: &str) -> Vec<u8> {
    let style = palette_style("ref", palette, None);
    ds_render::render_tile(
        &ramp_tile(width, height),
        style.colormap.as_ref(),
        ds_render::ImageFormat::Png,
    )
    .expect("reference render succeeds")
}

/// Router whose "radar" collection registers a per-parameter style layer for
/// `T` (as the server does for `[[wms.parameters]]`, bundle parameter entries
/// and built-in parameter defaults) but not for `RH`.
fn build_param_layer_router() -> axum::Router {
    build_router_with_styles(param_layer_styles())
}

/// The style registry the server builds for a multi-parameter collection: the
/// collection-level map plus a per-parameter map for `T` only, so `RH` has to
/// fall back to the collection map.
fn param_layer_styles() -> HashMap<String, HashMap<String, StyleInfo>> {
    HashMap::from([
        (
            "radar".to_string(),
            HashMap::from([
                (
                    "default".to_string(),
                    palette_style("default", "viridis", None),
                ),
                ("alt".to_string(), palette_style("alt", "radar_dbz", None)),
            ]),
        ),
        (
            "radar/T".to_string(),
            HashMap::from([
                (
                    "default".to_string(),
                    palette_style("default", "grayscale", Some("T")),
                ),
                (
                    "alt".to_string(),
                    palette_style("alt", "temperature", Some("T")),
                ),
            ]),
        ),
    ])
}

fn build_router_with_styles(styles: HashMap<String, HashMap<String, StyleInfo>>) -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MultiParamMockMapEngine);
    let mut engines = HashMap::new();
    engines.insert("radar".to_string(), engine);

    let mut collections = HashMap::new();
    collections.insert(
        "radar".to_string(),
        CollectionConfig {
            id: "radar".to_string(),
            title: "Test Radar".to_string(),
            description: "Test radar data".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "grib".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        },
    );

    let state = Arc::new(ArcSwap::from_pointee(MapsState {
        engines,
        collections,
        styles,
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
        map_tileset_ids: Default::default(),
    }));
    api_maps::router(state)
}

mod parameter_styles {
    use super::*;

    const SIZE: (u32, u32) = (32, 16);

    async fn fetch(uri: &str) -> (StatusCode, Vec<u8>) {
        let app = build_param_layer_router();
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body)
    }

    async fn fetch_json(uri: &str) -> (StatusCode, Value) {
        let (status, body) = fetch(uri).await;
        (status, serde_json::from_slice(&body).unwrap())
    }

    const MAP: &str = "/collections/radar/map?bbox=10,55,30,70&width=32&height=16";

    /// The gap this fixes: `parameter-name=T` renders with the `radar/T`
    /// layer's colormap, not the collection-level default. Before the fix the
    /// per-parameter palette was unreachable through Maps entirely.
    #[tokio::test]
    async fn map_with_parameter_name_uses_the_parameter_layer_style() {
        let (status, body) = fetch(&format!("{MAP}&parameter-name=T")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            reference_png(SIZE.0, SIZE.1, "grayscale"),
            "parameter-name=T must render with the radar/T layer's grayscale palette"
        );
        assert_ne!(
            body,
            reference_png(SIZE.0, SIZE.1, "viridis"),
            "the collection-level palette must not win over the parameter layer's"
        );
    }

    /// No `parameter-name` → the collection-level style, unchanged.
    #[tokio::test]
    async fn map_without_parameter_name_keeps_the_collection_style() {
        let (status, body) = fetch(MAP).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, reference_png(SIZE.0, SIZE.1, "viridis"));
    }

    /// A parameter with no registered style layer falls back to the
    /// collection-level map (single-parameter engines rely on this too).
    #[tokio::test]
    async fn parameter_without_a_style_layer_falls_back_to_the_collection() {
        let (status, body) = fetch(&format!("{MAP}&parameter-name=RH")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, reference_png(SIZE.0, SIZE.1, "viridis"));
    }

    /// The styled route resolves the same way — a named style is taken from
    /// the parameter layer when the request selects that parameter.
    #[tokio::test]
    async fn styled_map_route_resolves_the_parameter_layer() {
        let (status, body) = fetch(
            "/collections/radar/styles/alt/map\
             ?bbox=10,55,30,70&width=32&height=16&parameter-name=T",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, reference_png(SIZE.0, SIZE.1, "temperature"));
        assert_ne!(body, reference_png(SIZE.0, SIZE.1, "radar_dbz"));
    }

    /// A style that exists only on the collection layer is NOT silently
    /// substituted for the parameter layer — matching WMS GetMap, which 404s
    /// a style the requested layer does not define.
    #[tokio::test]
    async fn style_missing_from_the_parameter_layer_is_404() {
        // The T layer omits "alt" — what the resolver does with a style
        // scoped to a different parameter.
        let mut styles = param_layer_styles();
        styles.get_mut("radar/T").unwrap().remove("alt");
        let app = build_router_with_styles(styles);
        let req = Request::builder()
            .uri(
                "/collections/radar/styles/alt/map\
                 ?bbox=10,55,30,70&width=32&height=16&parameter-name=T",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    /// The legend follows the same resolution, so a client's drawn legend
    /// matches the pixels it gets for that parameter.
    #[tokio::test]
    async fn legend_with_unknown_parameter_name_is_a_400() {
        // Mirrors render_map: an unknown parameter must not fall back and
        // emit a legend labelled with a parameter that doesn't exist.
        let (status, json) =
            fetch_json("/collections/radar/styles/default/legend?parameter-name=BOGUS").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["description"]
            .as_str()
            .unwrap_or_default()
            .contains("BOGUS"));
    }

    #[tokio::test]
    async fn legend_with_parameter_name_describes_the_parameter_style() {
        let (status, json) =
            fetch_json("/collections/radar/styles/default/legend?parameter-name=T").await;
        assert_eq!(status, StatusCode::OK);
        let grayscale = ds_render::builtin_palette("grayscale").unwrap();
        let stops = json["stops"].as_array().unwrap();
        assert_eq!(stops.len(), grayscale.stops.len());
        assert_eq!(stops[0]["color"], "#000000");
        assert_eq!(stops[stops.len() - 1]["color"], "#FFFFFF");
        // The parameter comes from the style layer, not the engine default.
        assert_eq!(json["parameter"], "T");
        assert_eq!(json["unit"], "°C");
    }

    #[tokio::test]
    async fn legend_fallback_uses_requested_parameters_unit() {
        // RH has no dedicated style layer: the collection's dBZ unit must
        // not leak into its legend when falling back to the collection style.
        let (status, json) =
            fetch_json("/collections/radar/styles/default/legend?parameter-name=RH").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["parameter"], "RH");
        assert_eq!(json["unit"], "%");
    }

    #[tokio::test]
    async fn legend_without_parameter_name_describes_the_collection_style() {
        let (status, json) = fetch_json("/collections/radar/styles/default/legend").await;
        assert_eq!(status, StatusCode::OK);
        let viridis = ds_render::builtin_palette("viridis").unwrap();
        assert_eq!(json["stops"].as_array().unwrap().len(), viridis.stops.len());
        assert_eq!(json["stops"][0]["color"], "#440154");
        assert_eq!(json["parameter"], "reflectivity");
    }

    /// The new legend query parameter is advertised (repo rule: every new
    /// query param updates `api_definition()`).
    #[tokio::test]
    async fn legend_parameter_name_is_advertised_in_the_api_definition() {
        let app = build_param_layer_router();
        let req = Request::builder().uri("/api").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let params = json["paths"]["/maps/collections/radar/styles/{styleId}/legend"]["get"]
            ["parameters"]
            .as_array()
            .unwrap();
        assert!(
            params.iter().any(|p| p["name"] == "parameter-name"),
            "legend must advertise parameter-name; got {params:?}"
        );
    }
}

#[tokio::test]
async fn oversized_render_is_rejected_by_memory_admission() {
    let (status, headers, _) =
        get_raw("/collections/radar/map?bbox=20,60,30,70&width=8000&height=8000").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "1");
}

#[tokio::test]
async fn swagger_docs_and_local_assets_obey_security_policy() {
    // Nest under a prefix to catch asset URLs that only work at the root.
    let app = axum::Router::new().nest("/prefix/service", build_router());
    let docs = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/prefix/service/api/docs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(docs.status(), StatusCode::OK);
    assert_eq!(
        docs.headers()["content-security-policy"],
        ds_core::openapi::SWAGGER_UI_CSP
    );
    assert_eq!(docs.headers()["x-content-type-options"], "nosniff");
    let bytes = axum::body::to_bytes(docs.into_body(), 100_000)
        .await
        .unwrap();
    let html = std::str::from_utf8(&bytes).unwrap();
    assert!(!html.contains("unpkg.com"));
    assert!(!html.contains("<script>"));
    for name in [
        "swagger-ui-5.33.0.js",
        "swagger-ui-5.33.0.css",
        "init.js",
        "layout.css",
    ] {
        assert!(html.contains(&format!("docs/{name}")));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/prefix/service/api/docs/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let (mime, embedded) = ds_core::openapi::swagger_ui_asset(name).unwrap();
        assert_eq!(response.headers()["content-type"], mime);
        let bytes = axum::body::to_bytes(response.into_body(), 2_000_000)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), embedded);
    }
    let missing = app
        .oneshot(
            Request::builder()
                .uri("/prefix/service/api/docs/not-vendored.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Per-parameter time axes (#819)
// ---------------------------------------------------------------------------

mod per_parameter_times {
    use super::*;
    use chrono::{DateTime, Utc};

    const T0: &str = "2026-09-25T19:00:00Z";
    const T1: &str = "2026-09-25T19:10:00Z";
    const T2: &str = "2026-09-25T19:20:00Z";

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Two parameters on different time axes, as in a satellite collection:
    /// `a` has three scans; `b`, a product that lands later, only the first
    /// two. The collection's times are the union. Records every render's
    /// `(parameter, time)`.
    /// One render's `(parameter, time)`.
    type Render = (Option<String>, Option<DateTime<Utc>>);

    #[derive(Default)]
    struct Engine {
        renders: std::sync::Mutex<Vec<Render>>,
    }

    fn times(parameter: Option<&str>) -> Vec<DateTime<Utc>> {
        match parameter {
            Some("b") => vec![t(T0), t(T1)],
            _ => vec![t(T0), t(T1), t(T2)],
        }
    }

    impl MapEngine for Engine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            self.renders
                .lock()
                .unwrap()
                .push((parameter.map(str::to_string), time));
            Ok(RasterTile {
                width,
                height,
                values: vec![Some(0.5); (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            let parameter = |name: &str| ds_core::map_engine::ParameterInfo {
                name: name.to_string(),
                title: format!("Parameter {name}"),
                unit: "K".into(),
            };
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([-180.0, -90.0, 180.0, 90.0]),
                times: times(None),
                parameter: "a".into(),
                unit: "K".into(),
                parameters: vec![parameter("a"), parameter("b")],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }

        fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
            Some(times(Some(parameter)).into())
        }

        fn resolve_parameter_time(
            &self,
            parameter: Option<&str>,
            time: Option<DateTime<Utc>>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Option<DateTime<Utc>> {
            let times = times(parameter);
            match time {
                Some(time) => times.into_iter().rev().find(|&ts| ts <= time),
                None => times.last().copied(),
            }
        }
    }

    /// A map of parameter `p` over area `n` (0 or 1), at `time` if given.
    #[allow(non_snake_case)]
    fn URI(p: &str, n: u32, time: Option<&str>) -> String {
        let bbox = if n == 0 { "0,40,10,50" } else { "10,40,20,50" };
        let time = time.map(|t| format!("&datetime={t}")).unwrap_or_default();
        format!("/collections/radar/map?bbox={bbox}&width=64&height=64&parameter-name={p}{time}")
    }

    async fn status(app: &axum::Router, uri: &str) -> StatusCode {
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// An omitted `datetime` renders the requested parameter's latest time,
    /// and a time it lacks snaps on its own axis before the cache key is
    /// built — so the snapped request hits what the default one cached.
    #[tokio::test]
    async fn datetime_resolves_per_parameter() {
        let engine = Arc::new(Engine::default());
        let app = build_router_with_engine(engine.clone());
        let renders = |engine: &Engine| engine.renders.lock().unwrap().clone();
        let last = |engine: &Engine| renders(engine).last().cloned().unwrap();

        assert_eq!(status(&app, &URI("b", 0, None)).await, StatusCode::OK);
        assert_eq!(last(&engine), (Some("b".into()), Some(t(T1))));
        assert_eq!(status(&app, &URI("a", 0, None)).await, StatusCode::OK);
        assert_eq!(last(&engine), (Some("a".into()), Some(t(T2))));

        // T2 exists for `a` only: `b` resolves to T1, already cached above.
        let rendered = renders(&engine).len();
        assert_eq!(status(&app, &URI("b", 0, Some(T2))).await, StatusCode::OK);
        assert_eq!(renders(&engine).len(), rendered);
        // Elsewhere, the same request renders `b` at T1.
        assert_eq!(status(&app, &URI("b", 1, Some(T2))).await, StatusCode::OK);
        assert!(renders(&engine).len() > rendered);
        assert_eq!(last(&engine), (Some("b".into()), Some(t(T1))));
    }

    /// A datetime the parameter has nothing for yet resolves to `None` (a
    /// satellite product before its first scan): the response is revalidated
    /// rather than pinned `immutable` for a day. A resolved one stays pinned.
    #[tokio::test]
    async fn unresolved_datetime_is_not_immutable() {
        let app = build_router_with_engine(Arc::new(Engine::default()));
        let cache_control = |uri: String| {
            let app = app.clone();
            async move {
                let resp = app
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                resp.headers()["cache-control"]
                    .to_str()
                    .unwrap()
                    .to_string()
            }
        };
        let cc = cache_control(URI("a", 0, Some(T1))).await;
        assert!(cc.contains("immutable"), "{cc}");
        let cc = cache_control(URI("a", 0, Some("2026-09-25T18:50:00Z"))).await;
        assert!(
            cc.contains("must-revalidate") && !cc.contains("immutable"),
            "{cc}"
        );
    }

    /// Each parameter advertises its own time axis next to the collection's
    /// union (#279), so a client knows `b` ends a timestep earlier.
    #[tokio::test]
    async fn parameters_advertise_their_own_time_axis() {
        let app = build_router_with_engine(Arc::new(Engine::default()));
        let (status, json) = get_on(app, "/collections/radar").await;
        assert_eq!(status, StatusCode::OK);
        let rfc3339 = |s: &str| t(s).to_rfc3339();
        let interval =
            |p: &str| json["parameter_names"][p]["extent"]["temporal"]["interval"].clone();
        assert_eq!(
            json["extent"]["temporal"]["interval"],
            serde_json::json!([[rfc3339(T0), rfc3339(T2)]])
        );
        assert_eq!(
            interval("a"),
            serde_json::json!([[rfc3339(T0), rfc3339(T2)]])
        );
        assert_eq!(
            interval("b"),
            serde_json::json!([[rfc3339(T0), rfc3339(T1)]])
        );
    }
}

// ---------------------------------------------------------------------------
// Antimeridian-crossing bbox (#828)
// ---------------------------------------------------------------------------

mod antimeridian {
    use super::*;

    /// Records the `(bbox, output_crs)` of every render.
    #[derive(Default)]
    struct Engine {
        renders: std::sync::Mutex<Vec<([f64; 4], OutputCrs)>>,
    }

    impl MapEngine for Engine {
        fn get_raster_tile(
            &self,
            bbox: [f64; 4],
            width: u32,
            height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            self.renders
                .lock()
                .unwrap()
                .push((bbox, output_crs.clone()));
            Ok(RasterTile {
                width,
                height,
                values: vec![Some(0.5); (width * height) as usize].into(),
            })
        }

        fn raster_info(&self) -> RasterInfo {
            RasterInfo {
                // GOES-West's fixture extent: it crosses the antimeridian.
                spatial_extent: Some([173.9, 11.2, -174.8, 16.3]),
                grid_size: None,
                ..MockMapEngine::make_info()
            }
        }
    }

    async fn request(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>) {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let x_cache = resp
            .headers()
            .get("x-cache")
            .map(|v| v.to_str().unwrap().to_string());
        (resp.status(), x_cache)
    }

    /// The seam test box `170,10,-170,20` renders (200) instead of a 400,
    /// and the engine receives it unwrapped to a continuous viewport past
    /// 180°, on both map routes and in CRS:84 and Web Mercator output.
    #[tokio::test]
    async fn crossing_bbox_renders_unwrapped_past_180() {
        let engine = Arc::new(Engine::default());
        let app = build_router_with_engine(engine.clone());
        let last = || engine.renders.lock().unwrap().last().cloned().unwrap();

        for (uri, output_crs) in [
            (
                "/collections/radar/map?bbox=170,10,-170,20&width=64&height=32",
                OutputCrs::Wgs84,
            ),
            (
                "/collections/radar/styles/default/map?bbox=170,10,-170,20&width=32&height=32",
                OutputCrs::Wgs84,
            ),
            (
                "/collections/radar/map?bbox=170,10,-170,20&bbox-crs=CRS:84&crs=EPSG:3857&width=64&height=64",
                OutputCrs::WebMercator,
            ),
            (
                "/collections/radar/map?bbox=170,10,-170,20&bbox-crs=http://www.opengis.net/def/crs/OGC/1.3/CRS84&width=16&height=16",
                OutputCrs::Wgs84,
            ),
        ] {
            let (status, _) = request(&app, uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(last(), ([170.0, 10.0, 190.0, 20.0], output_crs), "{uri}");
        }
    }

    /// A crossing box is the same viewport as its unwrapped spelling, so
    /// the two share one cached render.
    #[tokio::test]
    async fn crossing_bbox_and_its_unwrapped_spelling_share_the_cache() {
        let engine = Arc::new(Engine::default());
        let app = build_router_with_engine(engine.clone());
        let uri = |bbox: &str| format!("/collections/radar/map?bbox={bbox}&width=64&height=32");

        assert_eq!(
            request(&app, &uri("170,10,-170,20")).await,
            (StatusCode::OK, Some("MISS".to_string()))
        );
        assert_eq!(
            request(&app, &uri("170,10,190,20")).await,
            (StatusCode::OK, Some("HIT".to_string()))
        );
        assert_eq!(engine.renders.lock().unwrap().len(), 1);
    }

    /// Still 400, and never reaching the engine: `west > east` in a
    /// projected `bbox-crs`, south >= north across the seam, a crossing
    /// with a longitude outside [-180, 180], and a box of no width.
    #[tokio::test]
    async fn invalid_boxes_are_still_rejected() {
        let engine = Arc::new(Engine::default());
        let app = build_router_with_engine(engine.clone());
        for bbox in [
            "2000000,1000000,-2000000,2000000&bbox-crs=EPSG:3857",
            "170,20,-170,10",
            "170,10,-170,10",
            "190,10,-170,20",
            "180,10,-180,20",
            "170,10,170,20",
        ] {
            let uri = format!("/collections/radar/map?bbox={bbox}");
            let (status, _) = request(&app, &uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
        assert!(engine.renders.lock().unwrap().is_empty());
    }
}

// --- RGB composites (#819) --------------------------------------------------

mod composites {
    use super::*;
    use chrono::{DateTime, Utc};
    use ds_core::map_engine::{select_common_time, CompositeChannel, CompositeDef};
    use ds_render::{CompositeSpec, ImageFormat};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    const T0: &str = "2026-09-25T19:00:00Z";
    const T1: &str = "2026-09-25T19:10:00Z";
    const T2: &str = "2026-09-25T19:20:00Z";
    const RGB: &str = "rgb";

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Red `a - b` over -10..40, green `b` over 0..100, blue `a` inverted
    /// over 100..0.
    fn rgb() -> CompositeDef {
        let channel = |parameter: &str, minus: Option<&str>, min, max| CompositeChannel {
            parameter: parameter.into(),
            minus: minus.map(String::from),
            min,
            max,
            gamma: 1.0,
        };
        CompositeDef {
            name: RGB.into(),
            title: "A and B".into(),
            channels: [
                channel("a", Some("b"), -10.0, 40.0),
                channel("b", None, 0.0, 100.0),
                channel("a", None, 100.0, 0.0),
            ],
        }
    }

    /// One band's tile: 50 (`a`) or 20 (`b`) plus the scan's ten-minute
    /// step, so each scan draws its own pixels.
    fn band(parameter: &str, time: DateTime<Utc>, w: u32, h: u32) -> RasterTile {
        let step = ((time - t(T0)).num_minutes() / 10) as f64;
        let value = if parameter == "a" { 50.0 } else { 20.0 } + step;
        RasterTile {
            width: w,
            height: h,
            values: vec![Some(value); (w * h) as usize].into(),
        }
    }

    /// One `get_raster_tiles` call: its bands and time.
    type Call = (Vec<String>, Option<DateTime<Utc>>);

    /// Two bands on their own time axes, like a satellite collection, and
    /// one composite over both. The axes can change under a running router.
    struct Engine {
        axes: Mutex<BTreeMap<&'static str, Vec<DateTime<Utc>>>>,
        calls: Mutex<Vec<Call>>,
    }

    impl Engine {
        fn new(a: &[&str], b: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                axes: Mutex::new(BTreeMap::from([
                    ("a", a.iter().map(|s| t(s)).collect()),
                    ("b", b.iter().map(|s| t(s)).collect()),
                ])),
                calls: Mutex::default(),
            })
        }

        fn add_scan(&self, band: &'static str, time: &str) {
            let mut axes = self.axes.lock().unwrap();
            let axis = axes.get_mut(band).unwrap();
            axis.push(t(time));
            axis.sort();
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn axis(&self, band: &str) -> Vec<DateTime<Utc>> {
            self.axes.lock().unwrap()[band].clone()
        }
    }

    impl MapEngine for Engine {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<RasterTile, DataServerError> {
            let parameter = parameter.unwrap_or("a");
            if parameter == RGB {
                return Err(DataServerError::InvalidParameter("not a band".into()));
            }
            let time = time.unwrap_or_else(|| *self.axis(parameter).last().unwrap());
            Ok(band(parameter, time, width, height))
        }

        fn get_raster_tiles(
            &self,
            _bbox: [f64; 4],
            width: u32,
            height: u32,
            time: Option<DateTime<Utc>>,
            _output_crs: &OutputCrs,
            parameters: &[&str],
            _z: Option<f64>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Result<Vec<RasterTile>, DataServerError> {
            self.calls
                .lock()
                .unwrap()
                .push((parameters.iter().map(|p| p.to_string()).collect(), time));
            let time = time.expect("a composite renders the time it was keyed on");
            Ok(parameters
                .iter()
                .map(|p| band(p, time, width, height))
                .collect())
        }

        fn raster_info(&self) -> RasterInfo {
            let parameter = |name: &str| ds_core::map_engine::ParameterInfo {
                name: name.to_string(),
                title: format!("Band {name}"),
                unit: "K".into(),
            };
            let mut times: Vec<DateTime<Utc>> = self
                .axes
                .lock()
                .unwrap()
                .values()
                .flatten()
                .copied()
                .collect();
            times.sort();
            times.dedup();
            RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: Some([-180.0, -85.0, 180.0, 85.0]),
                times,
                parameter: "a".into(),
                unit: "K".into(),
                parameters: vec![parameter("a"), parameter("b")],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
            }
        }

        fn parameter_times(&self, parameter: &str) -> Option<Arc<[DateTime<Utc>]>> {
            if parameter == RGB {
                let b = self.axis("b");
                let shared: Vec<DateTime<Utc>> = self
                    .axis("a")
                    .into_iter()
                    .filter(|t| b.contains(t))
                    .collect();
                return Some(shared.into());
            }
            Some(self.axis(parameter).into())
        }

        fn resolve_parameter_time(
            &self,
            parameter: Option<&str>,
            time: Option<DateTime<Utc>>,
            _reference_time: Option<DateTime<Utc>>,
        ) -> Option<DateTime<Utc>> {
            let parameter = parameter.unwrap_or("a");
            if parameter == RGB {
                let (a, b) = (self.axis("a"), self.axis("b"));
                return select_common_time(&[&a, &b], time);
            }
            select_common_time(&[&self.axis(parameter)], time)
        }

        fn composites(&self) -> Arc<[CompositeDef]> {
            Arc::from([rgb()])
        }
    }

    struct Reply {
        status: StatusCode,
        x_cache: String,
        body: bytes::Bytes,
    }

    async fn fetch(app: &axum::Router, uri: &str) -> Reply {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let x_cache = resp
            .headers()
            .get("x-cache")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            x_cache,
            body,
        }
    }

    fn json(reply: &Reply) -> Value {
        serde_json::from_slice(&reply.body).unwrap()
    }

    /// The composite's expected image at `time`: its bands composed.
    fn expected_png(w: u32, h: u32, time: &str) -> Vec<u8> {
        let tiles = vec![band("a", t(time), w, h), band("b", t(time), w, h)];
        ds_render::render_composite_tiles(
            &tiles,
            &CompositeSpec::from(&rgb()),
            ImageFormat::Png,
            None,
        )
        .unwrap()
        .unwrap()
    }

    /// A Maps router over `engine` as collection `radar`, with a `default`
    /// and an `alt` style.
    fn router(engine: Arc<Engine>) -> axum::Router {
        let engine: Arc<dyn MapEngine> = engine;
        let styles = HashMap::from([(
            "radar".to_string(),
            HashMap::from([
                (
                    "default".to_string(),
                    palette_style("default", "viridis", None),
                ),
                ("alt".to_string(), palette_style("alt", "radar_dbz", None)),
            ]),
        )]);
        let config = CollectionConfig {
            id: "radar".to_string(),
            title: "Satellite".to_string(),
            description: "Two bands and an RGB composite".to_string(),
            data_path: None,
            apis: vec!["maps".to_string()],
            engine_type: "satellite".to_string(),
            keywords: Vec::new(),
            license: None,
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            zarr: None,
            odim: None,
            cap: None,
            postgis: None,
            nowcast: None,
            bufr: None,
            satellite: None,
            preview: None,
        };
        let state = Arc::new(ArcSwap::from_pointee(MapsState {
            engines: HashMap::from([("radar".to_string(), engine)]),
            collections: HashMap::from([("radar".to_string(), config)]),
            styles,
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
            rendered_cache: Arc::new(RenderedCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
            map_tileset_ids: Default::default(),
        }));
        api_maps::router(state)
    }

    fn map_uri(extra: &str) -> String {
        format!(
            "/collections/radar/map?bbox=0,40,10,50&width=64&height=64&parameter-name=rgb{extra}"
        )
    }

    /// `parameter-name=<composite>` renders the bands composed at the time
    /// they share (T1, not `a`'s newer T2), and caches on it: a newer scan
    /// of one band leaves the frame alone (#507).
    #[tokio::test]
    async fn map_renders_the_composite_at_the_shared_time() {
        let engine = Engine::new(&[T0, T1, T2], &[T0, T1]);
        let app = router(engine.clone());
        let first = fetch(&app, &map_uri("")).await;
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(first.x_cache, "MISS");
        assert_eq!(first.body, expected_png(64, 64, T1));
        assert_eq!(
            engine.calls(),
            [(vec!["a".to_string(), "b".to_string()], Some(t(T1)))]
        );

        engine.add_scan("a", "2026-09-25T19:30:00Z");
        assert_eq!(fetch(&app, &map_uri("")).await.x_cache, "HIT");
        let pinned = fetch(&app, &map_uri(&format!("&datetime={T2}"))).await;
        assert_eq!(pinned.x_cache, "HIT");
        assert_eq!(pinned.body, first.body);
        assert_eq!(engine.calls().len(), 1);

        engine.add_scan("b", T2);
        let moved = fetch(&app, &map_uri("")).await;
        assert_eq!(moved.x_cache, "MISS");
        assert_eq!(moved.body, expected_png(64, 64, T2));
        assert_eq!(engine.calls()[1].1, Some(t(T2)));
    }

    /// Bands that share no scan give an empty image without an engine call.
    #[tokio::test]
    async fn no_shared_scan_is_an_empty_map() {
        let engine = Engine::new(&[T0], &[T1]);
        let app = router(engine.clone());
        let reply = fetch(&app, &map_uri(&format!("&datetime={T1}"))).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.x_cache, "EMPTY");
        assert!(engine.calls().is_empty());
    }

    /// Only the `default` style renders a composite; unknown names list
    /// the composite with the parameters.
    #[tokio::test]
    async fn composites_have_only_the_default_style() {
        let app = router(Engine::new(&[T0, T1], &[T0, T1]));
        let styled = fetch(
            &app,
            "/collections/radar/styles/alt/map?bbox=0,40,10,50&width=64&height=64&parameter-name=rgb",
        )
        .await;
        assert_eq!(styled.status, StatusCode::NOT_FOUND);
        assert!(json(&styled)["description"]
            .as_str()
            .unwrap()
            .contains("Available: default"));
        let named_default = fetch(
            &app,
            "/collections/radar/styles/default/map?bbox=0,40,10,50&width=64&height=64&parameter-name=rgb",
        )
        .await;
        assert_eq!(named_default.status, StatusCode::OK);

        let unknown = fetch(
            &app,
            "/collections/radar/map?bbox=0,40,10,50&width=64&height=64&parameter-name=nope",
        )
        .await;
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
        assert!(json(&unknown)["description"]
            .as_str()
            .unwrap()
            .contains("Available: a, b, rgb"));
    }

    /// The collection lists the composite among `parameter_names`: no unit,
    /// its channels described, its own time axis.
    #[tokio::test]
    async fn collection_lists_the_composite_parameter() {
        let app = router(Engine::new(&[T0, T1, T2], &[T0, T1]));
        let reply = fetch(&app, "/collections/radar").await;
        assert_eq!(reply.status, StatusCode::OK);
        let collection = json(&reply);
        let names = &collection["parameter_names"];
        assert_eq!(
            names.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["a", "b", "rgb"]
        );
        let composite = &names["rgb"];
        assert_eq!(composite["observedProperty"]["label"]["en"], "A and B");
        assert_eq!(
            composite["description"],
            "RGB composite: red a - b, green b, blue a"
        );
        assert!(composite.get("unit").is_none());
        assert_eq!(
            composite["extent"]["temporal"]["interval"],
            serde_json::json!([[t(T0).to_rfc3339(), t(T1).to_rfc3339()]])
        );
    }

    /// The legend of a composite is its channel list, JSON or PNG; another
    /// style is a 404.
    #[tokio::test]
    async fn legend_lists_the_channels() {
        let app = router(Engine::new(&[T0, T1], &[T0, T1]));
        let reply = fetch(
            &app,
            "/collections/radar/styles/default/legend?parameter-name=rgb",
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let legend = json(&reply);
        assert_eq!(legend["style"], "default");
        assert_eq!(legend["parameter"], "rgb");
        assert_eq!(legend["title"], "A and B");
        let channels = legend["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 3);
        assert_eq!(channels[0]["label"], "a - b");
        assert_eq!(channels[0]["unit"], "K");
        assert!(legend.get("stops").is_none());

        let png = fetch(
            &app,
            "/collections/radar/styles/default/legend?parameter-name=rgb&f=png",
        )
        .await;
        assert_eq!(png.status, StatusCode::OK);
        let expected = ds_render::render_composite_legend(
            &CompositeSpec::from(&rgb()),
            &[Some("K"), Some("K")],
            ds_render::LEGEND_DEFAULT_WIDTH,
            ds_render::LEGEND_DEFAULT_HEIGHT,
            ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(png.body, expected);

        let other = fetch(
            &app,
            "/collections/radar/styles/alt/legend?parameter-name=rgb",
        )
        .await;
        assert_eq!(other.status, StatusCode::NOT_FOUND);
    }

    /// The OpenAPI legend schema covers both legend shapes.
    #[tokio::test]
    async fn openapi_legend_schema_covers_composites() {
        let app = router(Engine::new(&[T0, T1], &[T0, T1]));
        let api = json(&fetch(&app, "/api").await);
        let variants = api["components"]["schemas"]["legend"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(variants.len(), 2);
        assert!(variants[1]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("channels")));
    }
}

// ---------------------------------------------------------------------------
// `quality` parameter and `[wms] webp_quality`
// ---------------------------------------------------------------------------

mod quality {
    use super::*;

    /// The first RIFF chunk of a WebP body: `VP8L` is lossless.
    fn webp_chunk(body: &[u8]) -> &[u8] {
        assert_eq!(&body[..4], b"RIFF", "not a WebP body");
        &body[12..16]
    }

    struct Fetched {
        status: StatusCode,
        x_cache: String,
        etag: String,
        body: Vec<u8>,
    }

    async fn fetch(app: &axum::Router, query: &str) -> Fetched {
        let uri = format!(
            "/collections/radar/map?bbox=10,55,30,70&width=64&height=64\
             &datetime=2024-01-01T00:00:00Z{query}"
        );
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default()
        };
        let (status, x_cache, etag) = (resp.status(), header("x-cache"), header("etag"));
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Fetched {
            status,
            x_cache,
            etag,
            body,
        }
    }

    fn app() -> axum::Router {
        build_router_with_engine(Arc::new(MockMapEngine::new()))
    }

    #[tokio::test]
    async fn lossy_and_lossless_webp_never_alias_in_the_cache() {
        let app = app();
        let lossless = fetch(&app, "&f=image/webp").await;
        assert_eq!(lossless.status, StatusCode::OK);
        assert_eq!(lossless.x_cache, "MISS");
        assert_eq!(webp_chunk(&lossless.body), b"VP8L", "default is lossless");

        let lossy = fetch(&app, "&f=image/webp&quality=80").await;
        assert_eq!(lossy.status, StatusCode::OK);
        assert_eq!(
            lossy.x_cache, "MISS",
            "a lossless entry must not serve lossy"
        );
        assert_ne!(webp_chunk(&lossy.body), b"VP8L");
        assert_ne!(lossy.etag, lossless.etag);

        let explicit = fetch(&app, "&f=image/webp&quality=100").await;
        assert_eq!(explicit.x_cache, "HIT", "100 is the lossless request");
        assert_eq!(explicit.body, lossless.body);
        assert_eq!(fetch(&app, "&f=image/webp&quality=80").await.x_cache, "HIT");
        assert_eq!(
            fetch(&app, "&f=image/webp&quality=60").await.x_cache,
            "MISS"
        );
    }

    #[tokio::test]
    async fn collection_webp_quality_is_the_default_and_quality_overrides_it() {
        let wms: ds_core::config::WmsConfig =
            serde_json::from_value(serde_json::json!({ "webp_quality": 70 })).unwrap();
        let configured =
            build_router_with_engine_and_wms(Arc::new(MockMapEngine::new()), Some(wms));
        let plain = app();
        let default = fetch(&configured, "&f=image/webp").await;
        assert_eq!(default.status, StatusCode::OK);
        assert_ne!(
            webp_chunk(&default.body),
            b"VP8L",
            "collection default is lossy"
        );
        let seventy = fetch(&configured, "&f=image/webp&quality=70").await;
        assert_eq!(seventy.x_cache, "HIT", "the default is quality 70");
        let lossless = fetch(&configured, "&f=image/webp&quality=100").await;
        assert_eq!(lossless.x_cache, "MISS");
        assert_eq!(
            webp_chunk(&lossless.body),
            b"VP8L",
            "explicit 100 is lossless"
        );
        assert_eq!(lossless.body, fetch(&plain, "&f=image/webp").await.body);
        // JPEG keeps its own default.
        assert_eq!(
            fetch(&configured, "&f=image/jpeg").await.body,
            fetch(&plain, "&f=image/jpeg").await.body
        );
    }

    #[tokio::test]
    async fn quality_sets_the_jpeg_quality() {
        let app = app();
        let default = fetch(&app, "&f=image/jpeg").await;
        assert_eq!(fetch(&app, "&f=image/jpeg&quality=85").await.x_cache, "HIT");
        let low = fetch(&app, "&f=image/jpeg&quality=20").await;
        assert_eq!(low.x_cache, "MISS");
        assert!(low.body.len() < default.body.len());
    }

    #[tokio::test]
    async fn bad_quality_is_400_naming_the_range_or_formats() {
        let app = app();
        for (query, description) in [
            (
                "&f=image/webp&quality=0",
                "quality '0' must be an integer from 1 to 100",
            ),
            (
                "&f=image/webp&quality=101",
                "quality '101' must be an integer from 1 to 100",
            ),
            (
                "&f=image/jpeg&quality=4.5",
                "quality '4.5' must be an integer from 1 to 100",
            ),
            (
                "&f=image/png&quality=80",
                "quality applies only to image/jpeg and image/webp, not image/png",
            ),
            (
                "&quality=80",
                "quality applies only to image/jpeg and image/webp, not image/png",
            ),
        ] {
            let resp = fetch(&app, query).await;
            assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{query}");
            let json: Value = serde_json::from_slice(&resp.body).unwrap();
            assert_eq!(json["description"], description, "{query}");
        }
        // The styled route validates the same way.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(
                        "/collections/radar/styles/default/map?bbox=10,55,30,70\
                         &f=image/png&quality=50",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Both map routes declare `quality` (repo rule: every new parameter
    /// updates `api_definition()`), as a bounded integer.
    #[tokio::test]
    async fn quality_is_in_the_api_definition() {
        let (_, api) = get_on(app(), "/api").await;
        for path in [
            "/maps/collections/radar/map",
            "/maps/collections/radar/styles/{styleId}/map",
        ] {
            let parameters = api["paths"][path]["get"]["parameters"].as_array().unwrap();
            assert!(
                parameters
                    .contains(&serde_json::json!({"$ref": "#/components/parameters/quality"})),
                "{path}"
            );
        }
        let quality = &api["components"]["parameters"]["quality"];
        assert_eq!(quality["in"], "query");
        assert_eq!(
            quality["schema"],
            serde_json::json!({"type": "integer", "minimum": 1, "maximum": 100})
        );
    }
}
