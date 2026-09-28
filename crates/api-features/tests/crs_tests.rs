//! OGC API - Features - Part 2: CRS by reference (OGC 18-058) on `/items`
//! and `/items/{featureId}` (#685).
//!
//! Expected projected coordinates come from PROJ 9 (`cs2cs EPSG:4326
//! EPSG:<code>`), which prints each CRS's EPSG axis order — never from the
//! code under test.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::*;
use ds_core::feature_engine::FeatureEngine;

const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const EPSG_4326: &str = "http://www.opengis.net/def/crs/EPSG/0/4326";
const EPSG_3857: &str = "http://www.opengis.net/def/crs/EPSG/0/3857";
const EPSG_3067: &str = "http://www.opengis.net/def/crs/EPSG/0/3067";
const EPSG_3035: &str = "http://www.opengis.net/def/crs/EPSG/0/3035";

struct Places(Vec<Feature>);

fn point(id: &str, x: f64, y: f64) -> Feature {
    Feature {
        id: id.into(),
        geometry: Geometry::Point { x, y }.into(),
        properties: HashMap::new().into(),
    }
}

impl Places {
    fn new() -> Self {
        Self(vec![
            point("helsinki", 24.9384, 60.1699),
            point("tampere", 23.7610, 61.4978),
            // Inside the TM35FIN box of `a_curved_projected_edge_…` but north
            // of its corners' latitudes.
            point("utsjoki", 27.0, 69.35),
            point("fiji", 178.4, -18.1),
            point("samoa", -171.8, -13.8),
            Feature {
                id: "area".into(),
                geometry: Geometry::Polygon {
                    exterior: vec![[24.8, 60.1], [25.1, 60.1], [25.1, 60.3], [24.8, 60.1]],
                    holes: vec![],
                }
                .into(),
                properties: HashMap::new().into(),
            },
        ])
    }
}

impl FeatureEngine for Places {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let matched: Vec<&Feature> = self
            .0
            .iter()
            .filter(|f| match (&query.bbox, f.geometry.bbox()) {
                (Some(bbox), Some(extent)) => bbox.intersects_bbox(&extent),
                (Some(_), None) => false,
                (None, _) => true,
            })
            .collect();
        let offset = query.offset.min(matched.len());
        let end = offset.saturating_add(query.limit).min(matched.len());
        Ok(FeaturePage {
            features: matched[offset..end].iter().map(|f| (*f).clone()).collect(),
            number_matched: matched.len(),
            number_returned: end - offset,
            next_offset: (end < matched.len()).then_some(end),
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        self.0
            .iter()
            .find(|f| f.id == feature_id)
            .cloned()
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.into()))
    }

    fn feature_count(&self) -> usize {
        self.0.len()
    }
}

fn router() -> axum::Router {
    let config: CollectionConfig = serde_json::from_value(json!({
        "id": "places",
        "title": "Places",
        "description": "Points and an area",
        "apis": ["features"],
        "engine_type": "mock"
    }))
    .unwrap();
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("places".into(), Arc::new(Places::new()));
    api_features::router(Arc::new(ArcSwap::from_pointee(FeaturesState {
        engines,
        collections: HashMap::from([("places".to_string(), config)]),
        base_url: String::new(),
        trust_proxy_headers: false,
        vector_tileset_ids: Default::default(),
    })))
}

/// `uri` may be a link the API emitted: those carry the `/features` mount,
/// which this bare router is not nested under.
async fn request(uri: &str) -> (StatusCode, HeaderMap, String) {
    let uri = uri.strip_prefix("/features").unwrap_or(uri);
    let resp = router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

async fn get(uri: &str) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, body) = request(uri).await;
    (status, headers, serde_json::from_str(&body).unwrap())
}

fn encoded(uri: &str) -> String {
    form_urlencoded::byte_serialize(uri.as_bytes()).collect()
}

fn content_crs(headers: &HeaderMap) -> &str {
    headers
        .get("content-crs")
        .expect("Content-Crs header")
        .to_str()
        .unwrap()
}

fn ids(doc: &Value) -> Vec<&str> {
    doc["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect()
}

fn link<'a>(doc: &'a Value, rel: &str) -> &'a str {
    doc["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rel"] == rel)
        .unwrap_or_else(|| panic!("no {rel} link"))["href"]
        .as_str()
        .unwrap()
}

fn assert_close(coordinates: &Value, expected: [f64; 2], tolerance: f64) {
    let got = [
        coordinates[0].as_f64().unwrap(),
        coordinates[1].as_f64().unwrap(),
    ];
    assert!(
        (got[0] - expected[0]).abs() <= tolerance && (got[1] - expected[1]).abs() <= tolerance,
        "{got:?} != {expected:?} (±{tolerance})"
    );
}

#[tokio::test]
async fn collection_advertises_exactly_the_crss_items_accepts() {
    let (_, _, collection) = get("/collections/places").await;
    assert_eq!(
        collection["crs"],
        json!([CRS84, EPSG_4326, EPSG_3857, EPSG_3067, EPSG_3035])
    );
    assert_eq!(collection["storageCrs"], CRS84);
    // Every advertised CRS works for `crs` on both feature resources and for
    // `bbox-crs`, and the response says which one it used.
    for crs in collection["crs"].as_array().unwrap() {
        let crs = crs.as_str().unwrap();
        for resource in [
            "/collections/places/items",
            "/collections/places/items/helsinki",
        ] {
            let (status, headers, _) = request(&format!("{resource}?crs={}", encoded(crs))).await;
            assert_eq!(status, StatusCode::OK, "{resource} {crs}");
            assert_eq!(content_crs(&headers), format!("<{crs}>"));
        }
        let (status, _, _) = request(&format!(
            "/collections/places/items?bbox-crs={}",
            encoded(crs)
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "bbox-crs {crs}");
    }
}

#[tokio::test]
async fn conformance_declares_part_2_crs() {
    let (_, _, doc) = get("/conformance").await;
    assert!(doc["conformsTo"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs"));
}

#[tokio::test]
async fn content_crs_is_crs84_by_default_in_every_representation() {
    for uri in [
        "/collections/places/items",
        "/collections/places/items?f=html",
        "/collections/places/items/helsinki",
        "/collections/places/items/helsinki?f=html",
    ] {
        let (status, headers, _) = request(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_crs(&headers), format!("<{CRS84}>"), "{uri}");
    }
    let (_, _, doc) = get("/collections/places/items/helsinki").await;
    assert_eq!(doc["geometry"]["coordinates"], json!([24.9384, 60.1699]));
}

#[tokio::test]
async fn epsg_4326_output_is_latitude_first() {
    let (_, _, doc) = get("/collections/places/items/helsinki?crs=EPSG%3A4326").await;
    assert_eq!(doc["geometry"]["coordinates"], json!([60.1699, 24.9384]));
    let (_, headers, doc) = get(&format!(
        "/collections/places/items?crs={}&bbox=24,60,26,61",
        encoded(EPSG_4326)
    ))
    .await;
    assert_eq!(content_crs(&headers), format!("<{EPSG_4326}>"));
    let area = doc["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "area")
        .unwrap();
    assert_eq!(
        area["geometry"]["coordinates"][0],
        json!([[60.1, 24.8], [60.1, 25.1], [60.3, 25.1], [60.1, 24.8]])
    );
}

#[tokio::test]
async fn projected_output_matches_proj() {
    for (crs, expected) in [
        // easting, northing
        (EPSG_3857, [2776129.9892, 8437661.7820]),
        (EPSG_3067, [385611.3167, 6672118.3802]),
        // northing, easting
        (EPSG_3035, [4206147.9718, 5145297.8805]),
    ] {
        let (status, _, doc) = get(&format!(
            "/collections/places/items/helsinki?crs={}",
            encoded(crs)
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_close(&doc["geometry"]["coordinates"], expected, 0.001);
    }
}

#[tokio::test]
async fn output_round_trips_through_bbox_crs() {
    for crs in [CRS84, EPSG_4326, EPSG_3857, EPSG_3067, EPSG_3035] {
        let (_, _, doc) = get(&format!(
            "/collections/places/items/helsinki?crs={}",
            encoded(crs)
        ))
        .await;
        let [a, b] = [0, 1].map(|i| doc["geometry"]["coordinates"][i].as_f64().unwrap());
        let d = if crs == CRS84 || crs == EPSG_4326 {
            1e-4
        } else {
            5.0
        };
        let (status, _, doc) = get(&format!(
            "/collections/places/items?bbox-crs={}&bbox={},{},{},{}",
            encoded(crs),
            a - d,
            b - d,
            a + d,
            b + d
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "{crs}");
        // The area polygon covers Helsinki too.
        assert_eq!(ids(&doc), ["helsinki", "area"], "{crs}");
    }
}

#[tokio::test]
async fn bbox_crs_axis_order_is_the_crss_own() {
    // EPSG:4326: south, west, north, east.
    let (_, _, doc) = get("/collections/places/items?bbox-crs=EPSG%3A4326&bbox=60,24,61,26").await;
    assert_eq!(ids(&doc), ["helsinki", "area"]);
    // The same numbers in CRS84 are longitudes 60–61, latitudes 24–26.
    let (_, _, doc) = get("/collections/places/items?bbox=60,24,61,26").await;
    assert!(ids(&doc).is_empty());
    // EPSG:3067 easting, northing around Helsinki (E 385611, N 6672118).
    let (_, _, doc) = get(&format!(
        "/collections/places/items?bbox-crs={}&bbox=380000,6660000,390000,6680000",
        encoded(EPSG_3067)
    ))
    .await;
    assert_eq!(ids(&doc), ["helsinki", "area"]);
    // EPSG:3035 northing, easting around Helsinki (N 4206148, E 5145298) …
    let (_, _, doc) = get(&format!(
        "/collections/places/items?bbox-crs={}&bbox=4200000,5140000,4210000,5150000",
        encoded(EPSG_3035)
    ))
    .await;
    assert_eq!(ids(&doc), ["helsinki", "area"]);
    // … while easting first names a box in the Norwegian Sea.
    let (_, _, doc) = get(&format!(
        "/collections/places/items?bbox-crs={}&bbox=5140000,4200000,5150000,4210000",
        encoded(EPSG_3035)
    ))
    .await;
    assert!(ids(&doc).is_empty());
}

#[tokio::test]
async fn a_curved_projected_edge_keeps_features_between_its_corners() {
    // The top edge of this TM35FIN box reaches 69.409°N at 27°E, but its
    // corners only 69.242°N (cs2cs EPSG:3067 EPSG:4326): Utsjoki at 69.35°N
    // is inside the box and outside a corners-only conversion.
    let (_, _, doc) = get(&format!(
        "/collections/places/items?bbox-crs={}&bbox=200000,6600000,800000,7700000",
        encoded(EPSG_3067)
    ))
    .await;
    assert_eq!(ids(&doc), ["helsinki", "tampere", "utsjoki", "area"]);
}

#[tokio::test]
async fn antimeridian_crossing_bboxes_still_filter() {
    // cs2cs EPSG:4326 EPSG:3857 of (-20°, 170°) and (-10°, -170°).
    let mercator = "18924313.4349,-2273030.9270,-18924313.4349,-1118889.9749";
    for query in [
        "bbox=170,-20,-170,-10".to_string(),
        "bbox=-20,170,-10,-170&bbox-crs=EPSG%3A4326".to_string(),
        format!("bbox={mercator}&bbox-crs={}", encoded(EPSG_3857)),
    ] {
        let (status, _, doc) = get(&format!("/collections/places/items?{query}")).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(ids(&doc), ["fiji", "samoa"], "{query}");
    }
    // The complement, west < east, holds neither.
    let (_, _, doc) = get("/collections/places/items?bbox=-170,-20,170,-10").await;
    assert!(ids(&doc).is_empty());
}

#[tokio::test]
async fn links_keep_the_crs_and_a_crs84_bbox() {
    let (_, _, first) = get(&format!(
        "/collections/places/items?crs={}&bbox-crs=EPSG%3A4326&bbox=60,24,61,26&limit=1",
        encoded(EPSG_3067)
    ))
    .await;
    let next = link(&first, "next");
    assert!(
        next.contains(&format!("crs={}", encoded(EPSG_3067))),
        "{next}"
    );
    assert!(
        next.contains("bbox=24,60,26,61") && !next.contains("bbox-crs"),
        "{next}"
    );
    let (status, headers, second) = get(next).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_crs(&headers), format!("<{EPSG_3067}>"));
    assert_eq!(ids(&second), ["area"]);
    assert_eq!(second["numberMatched"], first["numberMatched"]);
    // A feature's own link repeats the CRS; the HTML view is CRS84 only, so
    // the alternate link to it drops `crs` and still resolves.
    let feature_self = link(&first["features"][0], "self");
    assert!(feature_self.contains(&encoded(EPSG_3067)), "{feature_self}");
    let html = link(&first, "alternate");
    assert!(!html.contains("crs=") && html.contains("f=html"), "{html}");
    assert_eq!(request(html).await.0, StatusCode::OK);
}

#[tokio::test]
async fn html_is_crs84_only_but_takes_bbox_crs() {
    for uri in [
        "/collections/places/items?f=html&crs=EPSG%3A3067",
        "/collections/places/items/helsinki?f=html&crs=EPSG%3A4326",
    ] {
        let (status, _, doc) = get(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(doc["description"].as_str().unwrap().contains("CRS84"));
    }
    for uri in [
        "/collections/places/items?f=html&crs=OGC%3ACRS84",
        "/collections/places/items?f=html&bbox-crs=EPSG%3A4326&bbox=60,24,61,26",
    ] {
        let (status, headers, _) = request(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(content_crs(&headers), format!("<{CRS84}>"));
    }
}

#[tokio::test]
async fn unsupported_values_are_400_naming_the_valid_ones() {
    for query in [
        "crs=EPSG%3A32635",
        "bbox-crs=EPSG%3A2393&bbox=1,2,3,4",
        "crs=EPSG%3A4326&crs=EPSG%3A4326",
    ] {
        let (status, _, doc) = get(&format!("/collections/places/items?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        let message = doc["description"].as_str().unwrap();
        assert!(
            message.contains("duplicate") || message.contains(EPSG_3035),
            "{message}"
        );
    }
    // A projected box whose lower corner exceeds its upper one.
    let (status, _, _) = get(&format!(
        "/collections/places/items?bbox-crs={}&bbox=390000,6660000,380000,6680000",
        encoded(EPSG_3067)
    ))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn openapi_declares_crs_and_bbox_crs_as_part_2_does() {
    let (_, _, api) = get("/api").await;
    for name in ["crs", "bbox-crs"] {
        let p = &api["components"]["parameters"][name];
        assert_eq!(p["name"], name);
        assert_eq!(p["in"], "query");
        assert_eq!(p["required"], false);
        assert_eq!(p["schema"], json!({"type": "string", "format": "uri"}));
        assert_eq!(p["style"], "form");
        assert_eq!(p["explode"], false);
    }
    let refs = |path: &str| -> Vec<String> {
        api["paths"][path]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["$ref"].as_str().map(str::to_owned))
            .collect()
    };
    let items = refs("/features/collections/places/items");
    assert!(items.contains(&"#/components/parameters/crs".to_owned()));
    assert!(items.contains(&"#/components/parameters/bbox-crs".to_owned()));
    assert_eq!(
        refs("/features/collections/places/items/{featureId}"),
        ["#/components/parameters/crs"]
    );
}
