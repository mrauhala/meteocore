//! End-to-end tests for the 3D Tiles API over a mock `VolumeEngine`.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_3dtiles::TilesState3d;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::volume::{VolumeEngine, VolumeInfo, VolumePoint, VolumePointCloud};
use ds_render::{BuiltinColormap, LutColorMap};

/// Mock engine: a tiny fixed cloud, one quantity (`DBZH`), a coverage region.
struct MockVolume;

impl VolumeEngine for MockVolume {
    fn read_point_cloud(
        &self,
        quantity: Option<&str>,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        min_value: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        // Mirror the engine contract: unknown quantity → InvalidParameter.
        if let Some(q) = quantity {
            if q != "DBZH" {
                return Err(DataServerError::InvalidParameter(format!("unknown {q}")));
            }
        }
        let mut points = vec![
            VolumePoint {
                offset: [0.0, 0.0, 0.0],
                value: 10.0,
            },
            VolumePoint {
                offset: [100.0, -50.0, 250.0],
                value: 45.0,
            },
        ];
        if let Some(min) = min_value {
            points.retain(|p| p.value >= min);
        }
        if points.is_empty() {
            return Err(DataServerError::LocationNotFound("no echoes".into()));
        }
        Ok(VolumePointCloud {
            rtc_center: [3_000_000.0, 1_000_000.0, 5_000_000.0],
            region: [0.42, 1.05, 0.44, 1.07, 100.0, 12_000.0],
            points,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        })
    }

    fn read_voxel_grid(
        &self,
        _quantity: Option<&str>,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        dims: Option<[usize; 3]>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ds_core::volume::VoxelGrid, DataServerError> {
        let dims = dims.unwrap_or([2, 4, 2]);
        let total = dims[0] * dims[1] * dims[2];
        let mut values = vec![f32::NAN; total];
        values[0] = 30.0; // one sampled cell so valid_count() > 0
        Ok(ds_core::volume::VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 250_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 20_000.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        })
    }

    fn volume_info(&self) -> Arc<VolumeInfo> {
        Arc::new(VolumeInfo {
            quantities: vec![("DBZH".into(), "Reflectivity".into())],
            times: vec![],
            default_quantity: "DBZH".into(),
            default_unit: "dBZ".into(),
            region: Some([0.42, 1.05, 0.44, 1.07, 100.0, 25_000.0]),
        })
    }
}

fn collection_config(id: &str) -> CollectionConfig {
    // Build via TOML so this test doesn't depend on every CollectionConfig field.
    toml::from_str(&format!(
        r#"id = "{id}"
title = "Mock Radar Volume"
description = "test"
engine_type = "odim-volume"
apis = ["3dtiles"]
"#
    ))
    .expect("collection config parses")
}

fn router() -> axum::Router {
    let engine: Arc<dyn VolumeEngine> = Arc::new(MockVolume);
    let mut volume_engines = HashMap::new();
    volume_engines.insert("radar-fivih".to_string(), engine);
    let mut collections = HashMap::new();
    collections.insert("radar-fivih".to_string(), collection_config("radar-fivih"));

    let state = Arc::new(ArcSwap::from_pointee(TilesState3d {
        volume_engines,
        collections,
        colormap: Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::RadarDbz,
            -32.0,
            95.0,
        )),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
    }));
    api_3dtiles::router(state)
}

async fn get(uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let resp = router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, body, headers)
}

#[tokio::test]
async fn tileset_json_has_region_and_content_uri() {
    let (status, body, _h) = get("/collections/radar-fivih/tileset.json").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["geometricError"].as_f64().unwrap() > 0.0);
    assert_eq!(
        v["root"]["boundingVolume"]["region"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    // Content URI carries the resolved default quantity and is relative.
    let uri = v["root"]["content"]["uri"].as_str().unwrap();
    assert_eq!(uri, "content.pnts?quantity=DBZH");
}

#[tokio::test]
async fn content_pnts_is_valid_and_etagged() {
    let (status, body, headers) = get("/collections/radar-fivih/content.pnts?quantity=DBZH").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[0..4], b"pnts", "pnts magic");
    let byte_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
    assert_eq!(byte_len, body.len());
    assert_eq!(
        headers[axum::http::header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert!(headers.contains_key(axum::http::header::ETAG));
}

#[tokio::test]
async fn etag_round_trips_to_304() {
    let (_s, _b, headers) = get("/collections/radar-fivih/content.pnts").await;
    let etag = headers[axum::http::header::ETAG]
        .to_str()
        .unwrap()
        .to_string();
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/collections/radar-fivih/content.pnts")
                .header(axum::http::header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn unknown_collection_is_404() {
    let (status, _b, _h) = get("/collections/nope/tileset.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_quantity_is_400() {
    let (ts, _b, _h) = get("/collections/radar-fivih/tileset.json?quantity=FOOBAR").await;
    assert_eq!(
        ts,
        StatusCode::BAD_REQUEST,
        "tileset rejects unknown quantity"
    );
    let (cs, _b, _h) = get("/collections/radar-fivih/content.pnts?quantity=FOOBAR").await;
    assert_eq!(
        cs,
        StatusCode::BAD_REQUEST,
        "content rejects unknown quantity"
    );
}

#[tokio::test]
async fn datetime_offset_normalised_to_utc_z_in_content_uri() {
    // A `+hh:mm` offset must be re-emitted as `…Z` so the client's URL parser
    // doesn't decode the `+` as a space and 400 on the content fetch.
    let (status, body, _h) =
        get("/collections/radar-fivih/tileset.json?datetime=2024-01-01T12:00:00%2B05:30").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let uri = v["root"]["content"]["uri"].as_str().unwrap();
    assert!(!uri.contains('+'), "no raw + offset in content uri: {uri}");
    assert!(uri.contains("datetime=2024-01-01T06:30:00Z"), "got {uri}");
}

#[tokio::test]
async fn bad_datetime_is_400() {
    // Both routes parse the datetime.
    let (cs, _b, _h) = get("/collections/radar-fivih/content.pnts?datetime=notadate").await;
    assert_eq!(cs, StatusCode::BAD_REQUEST, "content rejects bad datetime");
    let (ts, _b, _h) = get("/collections/radar-fivih/tileset.json?datetime=notadate").await;
    assert_eq!(ts, StatusCode::BAD_REQUEST, "tileset rejects bad datetime");
}

#[tokio::test]
async fn if_none_match_wildcard_is_304() {
    // RFC 7232 §3.2: `If-None-Match: *` matches any current representation.
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/collections/radar-fivih/content.pnts")
                .header(axum::http::header::IF_NONE_MATCH, "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn viewer_page_is_served() {
    let (status, body, headers) = get("/viewer").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[axum::http::header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("Cesium"), "viewer loads CesiumJS");
    assert!(
        html.contains("/collections"),
        "viewer calls the collections API"
    );
}

#[tokio::test]
async fn non_finite_min_value_is_400() {
    for v in ["NaN", "inf", "-inf"] {
        let (ts, _b, _h) = get(&format!(
            "/collections/radar-fivih/tileset.json?min_value={v}"
        ))
        .await;
        assert_eq!(ts, StatusCode::BAD_REQUEST, "tileset rejects min_value={v}");
        let (cs, _b, _h) = get(&format!(
            "/collections/radar-fivih/content.pnts?min_value={v}"
        ))
        .await;
        assert_eq!(cs, StatusCode::BAD_REQUEST, "content rejects min_value={v}");
    }
}

#[tokio::test]
async fn tileset_carries_min_value_into_content_uri() {
    let (status, body, _h) =
        get("/collections/radar-fivih/tileset.json?quantity=DBZH&min_value=5").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let uri = v["root"]["content"]["uri"].as_str().unwrap();
    assert_eq!(uri, "content.pnts?quantity=DBZH&min_value=5");
}

#[tokio::test]
async fn collections_list_and_doc() {
    let (status, body, _h) = get("/collections").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["collections"][0]["id"], "radar-fivih");

    let (status, body, _h) = get("/collections/radar-fivih").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["quantities"][0]["id"], "DBZH");
}
