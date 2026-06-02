use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::*;
use ds_core::feature_engine::FeatureEngine;

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockFeatureEngine {
    features: Vec<Feature>,
    extent: Option<[f64; 4]>,
}

impl MockFeatureEngine {
    fn new() -> Self {
        let features = vec![
            Feature {
                id: "helsinki".into(),
                geometry: Geometry::Point {
                    x: 24.9384,
                    y: 60.1699,
                }
                .into(),
                properties: {
                    let mut m = HashMap::new();
                    m.insert("name".into(), PropertyValue::String("Helsinki".into()));
                    m.insert("population".into(), PropertyValue::Integer(658457));
                    m.into()
                },
            },
            Feature {
                id: "tampere".into(),
                geometry: Geometry::Point {
                    x: 23.7610,
                    y: 61.4978,
                }
                .into(),
                properties: {
                    let mut m = HashMap::new();
                    m.insert("name".into(), PropertyValue::String("Tampere".into()));
                    m.insert("population".into(), PropertyValue::Integer(244315));
                    m.into()
                },
            },
            Feature {
                id: "no-location".into(),
                geometry: Geometry::Null.into(),
                properties: {
                    let mut m = HashMap::new();
                    m.insert("name".into(), PropertyValue::String("Unknown".into()));
                    m.into()
                },
            },
        ];
        Self {
            extent: Some([23.7610, 60.1699, 24.9384, 61.4978]),
            features,
        }
    }
}

impl FeatureEngine for MockFeatureEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let mut filtered: Vec<&Feature> = self.features.iter().collect();

        if let Some(bbox) = &query.bbox {
            filtered.retain(|f| {
                if let Some(b) = f.geometry.bbox() {
                    bbox.intersects_bbox(&b)
                } else {
                    false
                }
            });
        }

        let number_matched = filtered.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);
        let page: Vec<Feature> = filtered[offset..end].iter().map(|f| (*f).clone()).collect();
        let number_returned = page.len();
        let next_offset = if end < number_matched {
            Some(end)
        } else {
            None
        };

        Ok(FeaturePage {
            features: page,
            number_matched,
            number_returned,
            next_offset,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        self.features
            .iter()
            .find(|f| f.id == feature_id)
            .cloned()
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.into()))
    }

    fn feature_count(&self) -> usize {
        self.features.len()
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.extent
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_router() -> axum::Router {
    let engine: Arc<dyn FeatureEngine> = Arc::new(MockFeatureEngine::new());
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();

    engines.insert("cities".to_string(), engine);
    collections.insert(
        "cities".to_string(),
        CollectionConfig {
            id: "cities".to_string(),
            title: "Finnish Cities".to_string(),
            description: "City points for testing".to_string(),
            data_path: None,
            apis: vec!["features".to_string()],
            engine_type: "mock".to_string(),
            geotiff: None,
            querydata: None,
            wms: None,
            grib: None,
            odim: None,
            postgis: None,
            preview: None,
        },
    );

    let state = Arc::new(ArcSwap::from_pointee(FeaturesState {
        engines,
        collections,
        base_url: String::new(),
    }));
    api_features::router(state)
}

async fn get(uri: &str) -> (StatusCode, Value) {
    let app = build_router();
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    (status, json)
}

async fn get_with_headers(uri: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let app = build_router();
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    (status, headers, json)
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
        assert!(links.iter().any(|l| l["rel"] == "service-desc"));
        assert!(links.iter().any(|l| l["rel"] == "conformance"));
        assert!(links.iter().any(|l| l["rel"] == "data"));
    }

    #[tokio::test]
    async fn links_have_href_and_rel() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        for link in links {
            assert!(link["href"].is_string(), "link missing href: {link}");
            assert!(link["rel"].is_string(), "link missing rel: {link}");
        }
    }

    #[tokio::test]
    async fn service_desc_link_has_openapi_type() {
        let (_, json) = get("/").await;
        let links = json["links"].as_array().unwrap();
        let desc = links.iter().find(|l| l["rel"] == "service-desc").unwrap();
        assert!(
            desc["type"].as_str().unwrap().contains("openapi"),
            "service-desc link should have OpenAPI type"
        );
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
    async fn declares_geojson() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/geojson")));
    }

    #[tokio::test]
    async fn declares_oas30() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes
            .iter()
            .any(|c| c.as_str().unwrap().contains("conf/oas30")));
    }
}

// ---------------------------------------------------------------------------
// API definition tests
// ---------------------------------------------------------------------------

mod api_definition {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/api").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn has_openapi_version() {
        let (_, json) = get("/api").await;
        assert!(json["openapi"].as_str().unwrap().starts_with("3.0"));
    }

    #[tokio::test]
    async fn has_info() {
        let (_, json) = get("/api").await;
        assert!(json["info"]["title"].is_string());
        assert!(json["info"]["version"].is_string());
    }

    #[tokio::test]
    async fn has_paths() {
        let (_, json) = get("/api").await;
        assert!(json["paths"].is_object());
    }

    #[tokio::test]
    async fn has_collection_item_paths() {
        let (_, json) = get("/api").await;
        let paths = json["paths"].as_object().unwrap();
        assert!(paths.contains_key("/features/collections/cities/items"));
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
    }

    #[tokio::test]
    async fn collections_not_empty() {
        let (_, json) = get("/collections").await;
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
    async fn collection_has_links() {
        let (_, json) = get("/collections").await;
        for c in json["collections"].as_array().unwrap() {
            let links = c["links"].as_array().unwrap();
            assert!(links.iter().any(|l| l["rel"] == "self"));
            assert!(links.iter().any(|l| l["rel"] == "items"));
        }
    }

    #[tokio::test]
    async fn collection_has_crs() {
        let (_, json) = get("/collections").await;
        for c in json["collections"].as_array().unwrap() {
            assert!(c["crs"].is_array());
            let crs = c["crs"].as_array().unwrap();
            assert!(crs.iter().any(|v| v.as_str().unwrap().contains("CRS84")));
        }
    }

    #[tokio::test]
    async fn collection_has_extent() {
        let (_, json) = get("/collections").await;
        let c = &json["collections"][0];
        assert!(c["extent"]["spatial"]["bbox"].is_array());
        assert!(c["extent"]["spatial"]["crs"].is_string());
    }

    #[tokio::test]
    async fn collection_detail_returns_200() {
        let (status, _) = get("/collections/cities").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn collection_detail_has_id() {
        let (_, json) = get("/collections/cities").await;
        assert_eq!(json["id"], "cities");
    }

    #[tokio::test]
    async fn collection_detail_omits_nonstandard_apis_field() {
        // `apis` is a vendor extension with no OGC schema; it must not leak
        // into the standard collection JSON.
        let (_, json) = get("/collections/cities").await;
        assert!(
            json.get("apis").is_none(),
            "apis must not be present in the standard collection JSON"
        );
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

// ---------------------------------------------------------------------------
// Vector-tile discovery tests
// ---------------------------------------------------------------------------

mod vector_tile_discovery {
    use super::*;

    fn build_router_with_tiles() -> axum::Router {
        // Router seeded with a collection that advertises both Features and
        // Tiles, mirroring how server/admin.rs would wire a real geojson
        // collection that the operator opted into vector tiles for.
        let engine: Arc<dyn FeatureEngine> = Arc::new(MockFeatureEngine::new());
        let mut engines = HashMap::new();
        let mut collections = HashMap::new();
        engines.insert("cities".to_string(), engine);
        collections.insert(
            "cities".to_string(),
            CollectionConfig {
                id: "cities".to_string(),
                title: "Finnish Cities".to_string(),
                description: "City points for testing".to_string(),
                data_path: None,
                apis: vec!["features".to_string(), "tiles".to_string()],
                engine_type: "mock".to_string(),
                geotiff: None,
                querydata: None,
                wms: None,
                grib: None,
                odim: None,
                postgis: None,
                preview: None,
            },
        );
        let state = Arc::new(ArcSwap::from_pointee(FeaturesState {
            engines,
            collections,
            base_url: String::new(),
        }));
        api_features::router(state)
    }

    async fn fetch(uri: &str) -> Value {
        let app = build_router_with_tiles();
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn collection_with_tiles_api_still_omits_apis_array() {
        // Even when the operator wires both Features and Tiles, the vendor
        // `apis` field stays out of the standard collection JSON — discovery
        // is via the `tilesets-vector` link asserted below.
        let json = fetch("/collections/cities").await;
        assert!(
            json.get("apis").is_none(),
            "apis must not be present in the standard collection JSON"
        );
    }

    #[tokio::test]
    async fn collection_with_tiles_api_emits_tileset_link() {
        let json = fetch("/collections/cities").await;
        let links = json["links"].as_array().expect("links must be present");
        let tileset_link = links
            .iter()
            .find(|l| {
                l["rel"].as_str() == Some("http://www.opengis.net/def/rel/ogc/1.0/tilesets-vector")
            })
            .expect("collection with apis=[tiles] must advertise a vector tileset link");
        // Per OGC API – Tiles 1.0 §7.1 the `tilesets-vector` rel targets the
        // tilesets-list resource (returns JSON), not a tile URL template, so
        // the type is `application/json` and there is no `templated` flag.
        assert_eq!(tileset_link["type"].as_str().unwrap(), "application/json");
        assert!(
            tileset_link.get("templated").is_none(),
            "tilesets-vector link must not carry the `templated` HAL-ism: \
             it points at a concrete tilesets-list URL, not a template"
        );
        let href = tileset_link["href"].as_str().unwrap();
        // No WebMercatorQuad hardcode — the list enumerates every supported
        // TileMatrixSet, so the discovery link stays correct if another TMS
        // is added later.
        assert!(
            href.ends_with("/tiles/collections/cities/tiles"),
            "tilesets-vector href must be the tilesets-list endpoint, got: {href}"
        );
        assert!(!href.contains("WebMercatorQuad"));
        assert!(!href.contains("f=mvt"));
    }

    #[tokio::test]
    async fn collection_without_tiles_api_does_not_emit_tileset_link() {
        // Reuse the standard `build_router()` fixture (via `get`), which
        // seeds `cities` with `apis: ["features"]` only — exactly the
        // config this assertion needs. The conditional in
        // `build_collection_metadata` must omit the `tilesets-vector`
        // link when `tiles` is absent.
        let (_, json) = get("/collections/cities").await;
        let links = json["links"].as_array().expect("links must be present");
        assert!(
            !links.iter().any(|l| {
                l["rel"].as_str() == Some("http://www.opengis.net/def/rel/ogc/1.0/tilesets-vector")
            }),
            "collection without `tiles` in apis must not emit a tilesets-vector link"
        );
    }
}

// ---------------------------------------------------------------------------
// Items tests
// ---------------------------------------------------------------------------

mod items {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections/cities/items").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn returns_geojson_content_type() {
        let (_, headers, _) = get_with_headers("/collections/cities/items").await;
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert!(
            ct.contains("application/geo+json"),
            "Expected geo+json content type, got: {ct}"
        );
    }

    #[tokio::test]
    async fn is_feature_collection() {
        let (_, json) = get("/collections/cities/items").await;
        assert_eq!(json["type"], "FeatureCollection");
    }

    #[tokio::test]
    async fn has_features_array() {
        let (_, json) = get("/collections/cities/items").await;
        assert!(json["features"].is_array());
    }

    #[tokio::test]
    async fn has_number_matched() {
        let (_, json) = get("/collections/cities/items").await;
        assert!(json["numberMatched"].is_number());
        assert_eq!(json["numberMatched"], 3);
    }

    #[tokio::test]
    async fn has_number_returned() {
        let (_, json) = get("/collections/cities/items").await;
        assert!(json["numberReturned"].is_number());
    }

    #[tokio::test]
    async fn has_timestamp() {
        let (_, json) = get("/collections/cities/items").await;
        assert!(json["timeStamp"].is_string());
    }

    #[tokio::test]
    async fn has_links() {
        let (_, json) = get("/collections/cities/items").await;
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
    }

    #[tokio::test]
    async fn features_have_required_structure() {
        let (_, json) = get("/collections/cities/items").await;
        for f in json["features"].as_array().unwrap() {
            assert_eq!(f["type"], "Feature");
            assert!(f["id"].is_string());
            assert!(f.get("geometry").is_some());
            assert!(f.get("properties").is_some());
        }
    }

    #[tokio::test]
    async fn null_geometry_serialized_as_null() {
        let (_, json) = get("/collections/cities/items").await;
        let features = json["features"].as_array().unwrap();
        let no_loc = features.iter().find(|f| f["id"] == "no-location").unwrap();
        assert!(no_loc["geometry"].is_null());
    }

    #[tokio::test]
    async fn feature_links_include_self_and_collection() {
        let (_, json) = get("/collections/cities/items").await;
        let f = &json["features"][0];
        let links = f["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == "collection"));
    }

    #[tokio::test]
    async fn pagination_limit() {
        let (_, json) = get("/collections/cities/items?limit=1").await;
        assert_eq!(json["numberReturned"], 1);
        assert_eq!(json["numberMatched"], 3);
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "next"));
    }

    #[tokio::test]
    async fn pagination_offset() {
        let (_, json) = get("/collections/cities/items?limit=1&offset=1").await;
        assert_eq!(json["numberReturned"], 1);
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "prev"));
    }

    #[tokio::test]
    async fn bbox_filter() {
        // Bbox covering only Helsinki area
        let (_, json) = get("/collections/cities/items?bbox=24.5,60.0,25.5,60.5").await;
        assert_eq!(json["numberMatched"], 1);
        let features = json["features"].as_array().unwrap();
        assert_eq!(features[0]["id"], "helsinki");
    }

    #[tokio::test]
    async fn bbox_no_match() {
        let (_, json) = get("/collections/cities/items?bbox=0.0,0.0,1.0,1.0").await;
        assert_eq!(json["numberMatched"], 0);
    }

    #[tokio::test]
    async fn invalid_bbox_returns_400() {
        let (status, _) = get("/collections/cities/items?bbox=not,valid,bbox,here").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_collection_returns_404() {
        let (status, _) = get("/collections/nonexistent/items").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn limit_clamped_to_max() {
        // limit > MAX_LIMIT (1000) should be clamped, not error
        let (status, _) = get("/collections/cities/items?limit=9999").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn limit_zero_clamped_to_one() {
        // limit=0 clamped to 1 per spec minimum
        let (status, json) = get("/collections/cities/items?limit=0").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["numberReturned"].as_u64().unwrap() >= 1);
    }
}

// ---------------------------------------------------------------------------
// Single item tests
// ---------------------------------------------------------------------------

mod single_item {
    use super::*;

    #[tokio::test]
    async fn returns_200() {
        let (status, _) = get("/collections/cities/items/helsinki").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn returns_geojson_content_type() {
        let (_, headers, _) = get_with_headers("/collections/cities/items/helsinki").await;
        let ct = headers.get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("application/geo+json"));
    }

    #[tokio::test]
    async fn is_feature() {
        let (_, json) = get("/collections/cities/items/helsinki").await;
        assert_eq!(json["type"], "Feature");
        assert_eq!(json["id"], "helsinki");
    }

    #[tokio::test]
    async fn has_geometry() {
        let (_, json) = get("/collections/cities/items/helsinki").await;
        assert_eq!(json["geometry"]["type"], "Point");
    }

    #[tokio::test]
    async fn has_properties() {
        let (_, json) = get("/collections/cities/items/helsinki").await;
        assert_eq!(json["properties"]["name"], "Helsinki");
    }

    #[tokio::test]
    async fn has_links() {
        let (_, json) = get("/collections/cities/items/helsinki").await;
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == "collection"));
    }

    #[tokio::test]
    async fn not_found_returns_404() {
        let (status, _) = get("/collections/cities/items/nonexistent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn null_geometry_feature() {
        let (status, json) = get("/collections/cities/items/no-location").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["geometry"].is_null());
    }
}

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
    async fn bad_bbox_returns_400_with_error_body() {
        let (status, json) = get("/collections/cities/items?bbox=invalid").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["code"].is_string());
    }

    #[tokio::test]
    async fn invalid_datetime_returns_400() {
        let (status, json) = get("/collections/cities/items?datetime=not-a-date").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["code"].is_string());
    }
}

// ---------------------------------------------------------------------------
// OGC API - Common - Part 4: Searchable Collections — handler wiring smoke
// ---------------------------------------------------------------------------

mod part4_searchable {
    use super::*;

    #[tokio::test]
    async fn conformance_declares_searchable_collections() {
        let (_, json) = get("/conformance").await;
        let classes = json["conformsTo"].as_array().unwrap();
        assert!(classes.iter().any(|c| c
            .as_str()
            .unwrap()
            .contains("common-4/1.0/conf/searchable-collections")));
    }

    #[tokio::test]
    async fn collections_has_match_counts() {
        let (status, json) = get("/collections").await;
        assert_eq!(status, StatusCode::OK);
        assert!(json["numberMatched"].is_number());
        assert!(json["numberReturned"].is_number());
    }

    #[tokio::test]
    async fn invalid_bbox_is_400() {
        let (status, _) = get("/collections?bbox=1,2,3").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

// ---------------------------------------------------------------------------
// Content negotiation — ?f=json|html (OGC API Common Part 2 conf/html, #296)
// ---------------------------------------------------------------------------
mod content_negotiation {
    use super::*;

    async fn get_raw(uri: &str) -> (StatusCode, String, String) {
        let resp = build_router()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, ct, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn f_html_serves_html() {
        let (status, ct, body) = get_raw("/collections?f=html").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
        assert!(body.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn collection_detail_serves_html() {
        let (status, ct, _) = get_raw("/collections/cities?f=html").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.starts_with("text/html"), "content-type was {ct}");
    }

    #[tokio::test]
    async fn unknown_f_is_400() {
        let (status, _, _) = get_raw("/collections?f=xml").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
