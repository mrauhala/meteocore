//! Cross-API discovery contracts over equivalent catalogs. No storage or render
//! calls: exercise each real router/adapter, including vector-only Tiles.
use std::{collections::HashMap, sync::Arc};

use arc_swap::ArcSwap;
use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use chrono::{DateTime, Utc};
use ds_core::{
    config::CollectionConfig,
    edr_engine::EdrEngine,
    error::DataServerError,
    feature::{Feature, FeaturePage, FeatureQuery},
    feature_engine::FeatureEngine,
    map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile},
    model::{CoverageResponse, Location},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const BASE: &str = "https://example.test/base";
const SURFACES: &[&str] = &["edr", "maps", "tiles", "vector-tiles", "features"];
const FILTERS: &str =
    "bbox=20,60,30,70&bbox-crs=CRS84&datetime=2024-01-01T00%3A30%3A00Z&q=RaDaR%20%26%20hail&query=%2Bweather%20-wind";

struct Fixture {
    bbox: Option<[f64; 4]>,
    time: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl EdrEngine for Fixture {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        panic!("metadata must not query data")
    }
    fn query_location(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        panic!("metadata must not query data")
    }
    fn get_parameters(&self) -> Vec<String> {
        vec![]
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.time
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.bbox
    }
}

impl FeatureEngine for Fixture {
    fn get_features(&self, _: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        panic!("metadata must not query data")
    }
    fn get_feature(&self, _: &str) -> Result<Feature, DataServerError> {
        panic!("metadata must not query data")
    }
    fn feature_count(&self) -> usize {
        0
    }
    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.bbox
    }
    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.time
    }
}

impl MapEngine for Fixture {
    fn get_raster_tile(
        &self,
        _: [f64; 4],
        _: u32,
        _: u32,
        _: Option<DateTime<Utc>>,
        _: &OutputCrs,
        _: Option<&str>,
        _: Option<f64>,
        _: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        panic!("metadata must not render")
    }
    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: self.bbox,
            times: self.time.map(|(a, b)| vec![a, b]).unwrap_or_default(),
            parameter: "rain".into(),
            unit: "mm".into(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: vec![],
        }
    }
}

fn instant(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn app(surface: &str) -> (Router, String) {
    let api = if surface == "vector-tiles" {
        "tiles"
    } else {
        surface
    };
    let mut configs = HashMap::new();
    let mut edr: HashMap<String, Arc<dyn EdrEngine>> = HashMap::new();
    let mut maps: HashMap<String, Arc<dyn MapEngine>> = HashMap::new();
    let mut features: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    // Deliberately not ID order. Two early IDs fail different predicates; the
    // unknown extent stays eligible, so filtering after paging cannot pass.
    for id in [
        "f-wind",
        "d-match",
        "a-outside",
        "e-unknown",
        "b-old",
        "c-match",
    ] {
        let time = if id == "e-unknown" {
            None
        } else if id == "b-old" {
            Some((
                instant("2023-01-01T00:00:00Z"),
                instant("2023-01-01T01:00:00Z"),
            ))
        } else {
            Some((
                instant("2024-01-01T00:00:00Z"),
                instant("2024-01-01T01:00:00Z"),
            ))
        };
        let bbox = match id {
            "e-unknown" => None,
            "a-outside" => Some([0.0, 0.0, 1.0, 1.0]),
            _ => Some([21.0, 61.0, 29.0, 69.0]),
        };
        let fixture = Arc::new(Fixture { bbox, time });
        edr.insert(id.into(), fixture.clone());
        maps.insert(id.into(), fixture.clone());
        features.insert(id.into(), fixture);
        let config: CollectionConfig = serde_json::from_value(json!({
            "id": id, "title": if id == "f-wind" { "Wind" } else { "Radar & hail" },
            "description": format!("Discovery fixture {id}"), "keywords": ["weather"],
            "license": {"title": "CC-BY-4.0"}, "apis": ["edr", "maps", "tiles", "features"]
        }))
        .unwrap();
        configs.insert(id.into(), config);
    }
    let router = match surface {
        "edr" => api_edr::router(Arc::new(ArcSwap::from_pointee(
            api_edr::handlers::EdrState {
                engines: edr,
                collections: configs,
                styles: HashMap::new(),
                base_url: BASE.into(),
                trust_proxy_headers: false,
            },
        ))),
        "features" => api_features::router(Arc::new(ArcSwap::from_pointee(
            api_features::handlers::FeaturesState {
                engines: features,
                collections: configs,
                base_url: BASE.into(),
                trust_proxy_headers: false,
            },
        ))),
        "maps" => api_maps::router(Arc::new(ArcSwap::from_pointee(api_maps::MapsState {
            engines: maps,
            collections: configs,
            styles: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            rendered_cache: Arc::new(ds_render::RenderedCache::new(1)),
            base_url: BASE.into(),
            trust_proxy_headers: false,
        }))),
        "tiles" | "vector-tiles" => {
            let vector_only = surface == "vector-tiles";
            api_tiles::router(Arc::new(ArcSwap::from_pointee(api_tiles::TilesState {
                map_engines: if vector_only { HashMap::new() } else { maps },
                collections: if vector_only {
                    HashMap::new()
                } else {
                    configs.clone()
                },
                // Raster+vector duplicates must not duplicate catalog entries.
                feature_engines: features,
                feature_collections: configs,
                styles: HashMap::new(),
                render_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
                rendered_cache: Arc::new(ds_render::RenderedCache::new(1)),
                vector_tile_cache: Arc::new(ds_mvt::VectorTileCache::new(1)),
                base_url: BASE.into(),
                trust_proxy_headers: false,
            })))
        }
        _ => unreachable!(),
    };
    let prefix = format!("/base/{api}");
    (Router::new().nest(&prefix, router), prefix)
}

async fn get(app: &Router, url: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let path = url.strip_prefix("https://example.test").unwrap_or(url);
    let mut req = Request::builder().uri(path);
    if let Some(accept) = accept {
        req = req.header(header::ACCEPT, accept);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

async fn get_json(app: &Router, url: &str) -> Value {
    let (status, _, body) = get(app, url, None).await;
    assert_eq!(status, StatusCode::OK, "{url}: {body}");
    serde_json::from_str(&body).unwrap()
}

fn link<'a>(doc: &'a Value, rel: &str) -> &'a str {
    doc["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel)
        .unwrap()["href"]
        .as_str()
        .unwrap()
}

#[tokio::test]
async fn equivalent_catalogs_filter_before_paging_and_preserve_every_filter() {
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        let doc = get_json(
            &app,
            &format!("{prefix}/collections?{FILTERS}&limit=1&offset=1"),
        )
        .await;
        assert_eq!(doc["numberMatched"], 3, "{surface}");
        for rel in ["self", "next", "prev", "alternate"] {
            assert!(link(&doc, rel).contains("query=%2Bweather%20-wind"));
        }
        assert_eq!(doc["numberReturned"], 1);
        assert_eq!(doc["collections"][0]["id"], "d-match");
        let next = get_json(&app, link(&doc, "next")).await;
        assert_eq!(next["numberMatched"], 3);
        assert_eq!(next["collections"][0]["id"], "e-unknown");
        assert!(!next["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["rel"] == "next"));
        let prev = get_json(&app, link(&doc, "prev")).await;
        assert_eq!(prev["collections"][0]["id"], "c-match");
        let alternate = link(&doc, "alternate");
        assert!(alternate.contains("f=html"));
        let (status, headers, html) = get(&app, alternate, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert!(html.contains("d-match?f=html"));
        assert!(!html.contains("c-match?f=html"));
        assert!(html.contains("RaDaR%20%26%20hail"));
        assert!(html.contains("offset=2"));
    }
}

#[tokio::test]
async fn query_text_semantics_and_encoding_agree_on_every_surface() {
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        for (query, expected) in [
            (
                "query=radar%20%2Bweather%20-old",
                vec!["a-outside", "c-match", "d-match", "e-unknown"],
            ),
            (
                "query=radar%20%2Bweather%20-old,wind",
                vec!["a-outside", "c-match", "d-match", "e-unknown", "f-wind"],
            ),
            ("query=-radar", vec!["f-wind"]),
            (
                "query=weather%20%2BradAR%20-old",
                vec!["a-outside", "c-match", "d-match", "e-unknown"],
            ),
            ("query=c-match", vec!["c-match"]),
            (
                "query=radar+%2Bweather+-old",
                vec!["a-outside", "c-match", "d-match", "e-unknown"],
            ),
            // An unencoded + means a space in a form query, not an operator.
            ("query=radar+weather", vec![]),
            ("q=radar&query=wind", vec![]),
            (
                "q=radar%09%26%20%20hail",
                vec!["a-outside", "b-old", "c-match", "d-match", "e-unknown"],
            ),
            (
                "query=radar%09%26%20%20hail",
                vec!["a-outside", "b-old", "c-match", "d-match", "e-unknown"],
            ),
            ("q=adar%20%26%20hail", vec![]),
            ("query=adar%20%26%20hail", vec![]),
        ] {
            let doc = get_json(&app, &format!("{prefix}/collections?{query}")).await;
            let ids: Vec<_> = doc["collections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, expected, "{surface}: {query}");
            assert_eq!(doc["numberMatched"], expected.len());
            let replay = get_json(&app, link(&doc, "self")).await;
            assert_eq!(replay["collections"], doc["collections"]);
        }
    }
}

#[tokio::test]
async fn unsupported_duplicate_and_invalid_parameters_return_structured_400() {
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        for query in [
            "parent=x",
            "descendants=immediate",
            "sortby=-id",
            "filter=true",
            "query=radar&query=wind",
            "query=radar&%71uery=wind",
            "query=",
            "query=radar,,wind",
            "query=radar%20%2B",
            "query=radar%20-%20hail",
            "sd=1000",
            "resolution=1",
            "typo=1",
            "limit=1&limit=2",
            "q=radar&%71=hail",
            "limit=0",
            "offset=-1",
            "bbox=1,2,3",
            "bbox-crs=EPSG:3857",
            "datetime=invalid",
            "f=xml",
        ] {
            let (status, headers, body) =
                get(&app, &format!("{prefix}/collections?{query}"), None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{surface} {query}: {body}");
            assert!(headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json"));
            let error: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(error["code"], "BadRequest");
            assert!(!error["description"].as_str().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn accept_negotiated_html_has_explicit_format_navigation() {
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        let (status, headers, html) = get(
            &app,
            &format!("{prefix}/collections?{FILTERS}&limit=1"),
            Some("text/html"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers
            .get_all(header::VARY)
            .iter()
            .any(|v| v.to_str().unwrap().eq_ignore_ascii_case("accept")));
        // Follow real rendered links with no Accept header.
        let marker = "href=\"";
        let links: Vec<_> = html
            .split(marker)
            .skip(1)
            .map(|s| s.split('"').next().unwrap().replace("&amp;", "&"))
            .collect();
        let next = links
            .iter()
            .find(|l| l.contains("offset=1") && l.contains("f=html"))
            .unwrap();
        let (_, headers, next_html) = get(&app, next, None).await;
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert!(next_html.contains("d-match?f=html"));
        let json_link = links.iter().find(|l| l.contains("f=json")).unwrap();
        let doc = get_json(&app, json_link).await;
        assert_eq!(doc["numberMatched"], 3);
        assert_eq!(doc["collections"][0]["id"], "c-match");
    }
}

#[tokio::test]
async fn metadata_and_search_use_the_same_temporal_extent() {
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        let detail = get_json(&app, &format!("{prefix}/collections/b-old")).await;
        assert!(detail["extent"]["temporal"]["interval"][0][0]
            .as_str()
            .unwrap()
            .starts_with("2023-"));
        if ["features", "vector-tiles"].contains(surface) {
            assert!(detail["extent"]["temporal"].get("grid").is_none());
        }
        let doc = get_json(
            &app,
            &format!("{prefix}/collections?datetime=2024-01-01T00%3A30%3A00Z"),
        )
        .await;
        assert!(
            !doc["collections"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == "b-old"),
            "{surface}"
        );
        assert!(doc["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "e-unknown"));
        let listed = get_json(&app, &format!("{prefix}/collections")).await;
        let old = listed["collections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "b-old")
            .unwrap();
        assert_eq!(old, &detail);
        assert_eq!(detail["keywords"], json!(["weather"]));
        assert_eq!(
            link(&detail, "license"),
            "https://spdx.org/licenses/CC-BY-4.0.html"
        );
        let (status, headers, _) = get(&app, link(&detail, "alternate"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
    }
}

#[tokio::test]
async fn openapi_and_conformance_are_consistent_across_surfaces() {
    let mut baseline = None;
    let mut classes_baseline = None;
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        let api = get_json(&app, &format!("{prefix}/api")).await;
        let path = format!("{}/collections", prefix.strip_prefix("/base").unwrap());
        let operation = &api["paths"][&path]["get"];
        let success = &operation["responses"]["200"]["content"];
        assert!(success["application/json"].is_object());
        assert!(success["text/html"].is_object());
        assert_eq!(
            operation["responses"]["400"]["content"]["application/json"]["schema"]["required"],
            json!(["code", "description"])
        );
        let params = &operation["parameters"];
        let parameters = params.as_array().unwrap();
        assert_eq!(parameters.len(), 8);
        let limit = parameters.iter().find(|p| p["name"] == "limit").unwrap();
        assert_eq!(limit["schema"]["default"], 1000);
        assert_eq!(limit["schema"]["maximum"], 1000);
        for name in ["bbox", "q", "query"] {
            let p = parameters.iter().find(|p| p["name"] == name).unwrap();
            assert_eq!(p["schema"]["type"], "array");
            assert_eq!(p["style"], "form");
            assert_eq!(p["explode"], false);
        }
        if let Some(ref baseline) = baseline {
            assert_eq!(params, baseline);
        }
        baseline = Some(params.clone());
        let conformance = get_json(&app, &format!("{prefix}/conformance")).await;
        let common: Vec<_> = conformance["conformsTo"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| s.contains("ogcapi-common-"))
            .map(str::to_owned)
            .collect();
        assert_eq!(common.len(), 6);
        assert!(!common.iter().any(|c| c.contains("common-4")));
        if let Some(ref baseline) = classes_baseline {
            assert_eq!(&common, baseline);
        }
        classes_baseline = Some(common);
        let doc = get_json(&app, &format!("{prefix}/collections?limit=9999")).await;
        assert!(link(&doc, "self").contains("limit=1000"));
        assert_eq!(doc["numberReturned"], 6);
        let empty = get_json(&app, &format!("{prefix}/collections?offset=999")).await;
        assert_eq!(empty["numberMatched"], 6);
        assert_eq!(empty["numberReturned"], 0);
    }
}

#[tokio::test]
async fn workbench_exposes_supported_queries_and_same_resource_json_on_every_surface() {
    for surface in SURFACES {
        let (app, prefix) = app(surface);
        let (status, _, html) = get(
            &app,
            &format!("{prefix}/collections?{FILTERS}&limit=1&offset=1&f=html"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for control in ["q", "query", "bbox", "datetime", "limit", "offset"] {
            assert!(
                html.contains(&format!("name=\"{control}\""))
                    || html.contains(&format!("data-param=\"{control}\"")),
                "{surface}: {control}"
            );
        }
        for unsupported in ["sortby", "sd", "resolution", "parent", "depth"] {
            assert!(!html.contains(&format!("name=\"{unsupported}\"")));
            assert!(!html.contains(&format!("data-param=\"{unsupported}\"")));
        }
        let target = html
            .split("id=\"json-link\" href=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .replace("&amp;", "&");
        let doc = get_json(&app, &target).await;
        assert_eq!(doc["collections"][0]["id"], "d-match");
        assert_eq!(doc["numberMatched"], 3);
        assert!(target.starts_with(BASE));
        assert!(html.contains("id=\"theme\""));
        let (_, _, detail) = get(&app, &format!("{prefix}/collections/d-match?f=html"), None).await;
        assert!(detail.contains("Coverage &amp; metadata"));
        assert!(detail.contains("2024-01-01"));
        assert!(detail.contains("Back to collections"));
    }
}
