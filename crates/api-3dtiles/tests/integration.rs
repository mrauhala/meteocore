//! End-to-end tests for the 3D Tiles API over a mock `VolumeEngine`.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_3dtiles::TilesState3d;
use chrono::{TimeZone, Utc};
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::volume::{
    VolumeEngine, VolumeInfo, VolumePoint, VolumePointCloud, VoxelGrid, VoxelGridCaps,
};
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
        quantity: Option<&str>,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        dims: Option<[usize; 3]>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VoxelGrid, DataServerError> {
        if let Some(q) = quantity {
            if q != "DBZH" {
                return Err(DataServerError::InvalidParameter(format!("unknown {q}")));
            }
        }
        let dims = dims.unwrap_or([4, 8, 4]);
        let [n_r, n_a, n_h] = dims;
        // Clear air is the finite no-echo floor (matching the post-#360 engine
        // fill), with a finite >threshold echo core in the middle — so the
        // isosurface (which the handler seals with `background=Some(floor)`)
        // produces a non-empty mesh.
        let mut values = vec![ds_core::volume::NO_ECHO_FLOOR_DBZ; n_r * n_a * n_h];
        for i_r in 1..n_r.min(3) {
            for i_a in 0..n_a {
                for i_h in 1..n_h.min(3) {
                    values[VoxelGrid::index_of(dims, i_r, i_a, i_h)] = 40.0;
                }
            }
        }
        Ok(VoxelGrid {
            origin_lon: 24.5,
            origin_lat: 60.5,
            origin_height: 100.0,
            dims,
            radius_range: [0.0, 100_000.0],
            angle_range: [0.0, std::f64::consts::TAU],
            height_range: [0.0, 10_000.0],
            values,
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        })
    }

    fn volume_info(&self) -> Arc<VolumeInfo> {
        Arc::new(VolumeInfo {
            quantities: vec![("DBZH".into(), "Reflectivity".into())],
            // Two volume timesteps — the time axis a client scrubs. Ascending +
            // deduped, matching the engine's `VolumeInfo.times` contract (the API
            // emits them directly, no re-sort).
            times: vec![
                Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 5, 15, 0, 5, 0).unwrap(),
            ],
            default_quantity: "DBZH".into(),
            default_unit: "dBZ".into(),
            region: Some([0.42, 1.05, 0.44, 1.07, 100.0, 25_000.0]),
            voxel_grid: Some(VoxelGridCaps {
                origin: [24.5, 60.5, 100.0],
                radius_m: 250_000.0,
                height_m: 20_000.0,
            }),
        })
    }
}

/// A volume engine that serves a point cloud but NOT a voxel grid (uses the
/// default `read_voxel_grid` → unsupported), like a non-PVOL volume source.
struct MockNoVoxel;

impl VolumeEngine for MockNoVoxel {
    fn read_point_cloud(
        &self,
        _quantity: Option<&str>,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _min_value: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        Ok(VolumePointCloud {
            rtc_center: [3_000_000.0, 1_000_000.0, 5_000_000.0],
            region: [0.42, 1.05, 0.44, 1.07, 100.0, 12_000.0],
            points: vec![VolumePoint {
                offset: [0.0, 0.0, 0.0],
                value: 10.0,
            }],
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        })
    }

    // read_voxel_grid uses the trait default (unsupported).

    fn volume_info(&self) -> Arc<VolumeInfo> {
        Arc::new(VolumeInfo {
            quantities: vec![("DBZH".into(), "Reflectivity".into())],
            times: vec![],
            default_quantity: "DBZH".into(),
            default_unit: "dBZ".into(),
            region: Some([0.42, 1.05, 0.44, 1.07, 100.0, 25_000.0]),
            voxel_grid: None,
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
    // The available volume timestamps are advertised (RFC 3339 Z, ascending per
    // the engine's `VolumeInfo.times` contract) — the time axis a client scrubs,
    // each value reusable as `?datetime=`.
    let times: Vec<&str> = v["times"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(times, vec!["2026-05-15T00:00:00Z", "2026-05-15T00:05:00Z"]);
    // The mock supports voxel grids, so all three representations are advertised.
    let reps: Vec<&str> = v["representations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert_eq!(reps, vec!["points", "isosurface", "echotop", "voxels"]);
    // A colour-scale legend is advertised for the point cloud: a unit, a range,
    // and sampled `#rrggbb` stops the viewer renders as a gradient bar.
    let lg = &v["legend"];
    assert_eq!(lg["unit"], "dBZ");
    assert!(lg["min"].as_f64().unwrap() < lg["max"].as_f64().unwrap());
    let stops = lg["stops"].as_array().unwrap();
    assert!(stops.len() >= 2, "legend has multiple stops");
    let c0 = stops[0]["color"].as_str().unwrap();
    assert!(
        c0.starts_with('#') && c0.len() == 7,
        "stop colour is #rrggbb: {c0}"
    );
    // …and a link-following client can discover the isosurface tileset too.
    let hrefs: Vec<&str> = v["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l["href"].as_str())
        .collect();
    assert!(
        hrefs.iter().any(|h| h.ends_with("/tileset.json")),
        "point-cloud tileset link present: {hrefs:?}"
    );
    assert!(
        hrefs
            .iter()
            .any(|h| h.contains("tileset.json?representation=isosurface")),
        "isosurface tileset link present: {hrefs:?}"
    );
    assert!(
        hrefs
            .iter()
            .any(|h| h.contains("tileset.json?representation=echotop")),
        "echo-top tileset link present: {hrefs:?}"
    );
}

#[tokio::test]
async fn isosurface_on_unsupported_collection_is_400() {
    // A collection whose engine can't voxel-grid advertises only `points` and
    // rejects isosurface requests at both the tileset and content routes.
    let engine: Arc<dyn VolumeEngine> = Arc::new(MockNoVoxel);
    let mut volume_engines = HashMap::new();
    volume_engines.insert("radar-novox".to_string(), engine);
    let mut collections = HashMap::new();
    collections.insert("radar-novox".to_string(), collection_config("radar-novox"));
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
    let app = api_3dtiles::router(state);

    let send = |uri: &str| {
        let app = app.clone();
        let uri = uri.to_string();
        async move {
            app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
        }
    };

    assert_eq!(
        send("/collections/radar-novox/tileset.json?representation=isosurface")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send("/collections/radar-novox/content.glb").await.status(),
        StatusCode::BAD_REQUEST
    );
    // The point-cloud representation still works.
    assert_eq!(
        send("/collections/radar-novox/tileset.json").await.status(),
        StatusCode::OK
    );
    // Collection doc advertises only `points`.
    let resp = send("/collections/radar-novox").await;
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reps: Vec<&str> = v["representations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert_eq!(reps, vec!["points"]);
}

#[tokio::test]
async fn echotop_tileset_and_content_is_valid_glb() {
    // The tileset advertises echo-top glb content tagged with the representation,
    // plus the antenna-ECEF transform (like the isosurface).
    let (status, body, _h) =
        get("/collections/radar-fivih/tileset.json?representation=echotop&quantity=DBZH").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["root"]["transform"].as_array().unwrap().len(), 16);
    let uri = v["root"]["content"]["uri"].as_str().unwrap();
    assert!(uri.starts_with("content.glb?"), "glb content: {uri}");
    assert!(
        uri.contains("representation=echotop"),
        "echotop-tagged: {uri}"
    );
    // Both mesh products embed the (defaulted) resolution tier — the tileset
    // handler shares the code path, but assert it here so the two can't diverge.
    assert!(uri.contains("resolution="), "resolution tier in uri: {uri}");

    // The content itself is a valid glTF binary.
    let (status, body, headers) =
        get("/collections/radar-fivih/content.glb?representation=echotop&quantity=DBZH").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[0..4], b"glTF", "glb magic");
    assert_eq!(
        u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize,
        body.len()
    );
    assert_eq!(
        headers[axum::http::header::CONTENT_TYPE],
        "model/gltf-binary"
    );
}

#[tokio::test]
async fn tileset_carries_resolution_tier_into_glb_content_uri() {
    // An explicit resolution is validated and echoed into the content.uri so the
    // content fetch matches the tileset.
    let (status, body, _h) = get(
        "/collections/radar-fivih/tileset.json?representation=isosurface&quantity=DBZH&threshold=20&resolution=high",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let uri = v["root"]["content"]["uri"].as_str().unwrap();
    assert!(uri.contains("resolution=high"), "tier in uri: {uri}");

    // A bad tier is a 400 at the tileset, not a surprise on the content fetch.
    let (status, _b, _h) = get(
        "/collections/radar-fivih/tileset.json?representation=isosurface&quantity=DBZH&resolution=ultra",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // …and on the content route too.
    let (status, _b, _h) =
        get("/collections/radar-fivih/content.glb?quantity=DBZH&resolution=ultra").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A bad tier is rejected even on a `points` tileset (which ignores the value)
    // — the param is validated up front regardless of representation.
    let (status, _b, _h) = get(
        "/collections/radar-fivih/tileset.json?representation=points&quantity=DBZH&resolution=ultra",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn glb_rejects_points_representation() {
    // `.glb` serves only the mesh products — asking it for `points` is a 400
    // (the point cloud has its own `content.pnts` route), not a silently
    // mislabelled isosurface.
    let (status, _body, _h) =
        get("/collections/radar-fivih/content.glb?representation=points&quantity=DBZH").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn voxel_tileset_subtree_and_content_are_well_formed() {
    // Tileset: cylinder bounding volume + content_voxels + implicit OCTREE +
    // a content URI carrying the resolved selection.
    let (status, body, _h) =
        get("/collections/radar-fivih/voxel/tileset.json?quantity=DBZH&resolution=low").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let bv = &v["root"]["boundingVolume"]["extensions"]["3DTILES_bounding_volume_cylinder"];
    assert!(bv["maxRadius"].as_f64().unwrap() > 0.0);
    assert_eq!(
        v["root"]["content"]["extensions"]["3DTILES_content_voxels"]["class"],
        "voxel"
    );
    assert_eq!(v["root"]["implicitTiling"]["subdivisionScheme"], "OCTREE");
    let curi = v["root"]["content"]["uri"].as_str().unwrap();
    assert!(
        curi.starts_with("content/{level}/{x}/{y}/{z}.glb?"),
        "{curi}"
    );
    assert!(curi.contains("quantity=DBZH") && curi.contains("resolution=low"));

    // Subtree: constant single-tile availability. `contentAvailability` is an
    // ARRAY per the 3D Tiles 1.1 implicit-tiling spec (one entry per layer).
    let (status, body, _h) = get("/collections/radar-fivih/voxel/subtrees/0/0/0/0.json").await;
    assert_eq!(status, StatusCode::OK);
    let s: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s["tileAvailability"]["constant"], 1);
    assert_eq!(s["contentAvailability"][0]["constant"], 1);

    // Content: a valid EXT_primitive_voxels glb (cylinder mode 2147483650).
    let (status, body, headers) =
        get("/collections/radar-fivih/voxel/content/0/0/0/0.glb?quantity=DBZH&resolution=low")
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[0..4], b"glTF", "glb magic");
    assert_eq!(
        headers[axum::http::header::CONTENT_TYPE],
        "model/gltf-binary"
    );
    // Parse the JSON chunk and check the voxel extension + cylinder mode.
    let jlen = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
    let j: serde_json::Value = serde_json::from_slice(&body[20..20 + jlen]).unwrap();
    assert_eq!(j["meshes"][0]["primitives"][0]["mode"], 2_147_483_650u64);
    assert!(j["extensionsRequired"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e == "EXT_primitive_voxels"));
}

#[tokio::test]
async fn isosurface_tileset_has_transform_and_glb_content() {
    let (status, body, _h) =
        get("/collections/radar-fivih/tileset.json?representation=isosurface&quantity=DBZH&threshold=20")
            .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["geometricError"].as_f64().unwrap() > 0.0);
    // glTF content needs the antenna-ECEF tile transform (16-element matrix).
    let t = v["root"]["transform"].as_array().unwrap();
    assert_eq!(t.len(), 16);
    // Content points at the .glb, carrying the resolved quantity + threshold +
    // the (defaulted) resolution tier + the explicit representation, so the
    // content fetch is deterministic and unambiguous.
    let uri = v["root"]["content"]["uri"].as_str().unwrap();
    assert_eq!(
        uri,
        "content.glb?quantity=DBZH&threshold=20&resolution=med&representation=isosurface"
    );
    // The region bounding volume is still present (unaffected by the transform).
    assert_eq!(
        v["root"]["boundingVolume"]["region"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
}

#[tokio::test]
async fn content_glb_is_valid_gltf_and_etagged() {
    let (status, body, headers) =
        get("/collections/radar-fivih/content.glb?quantity=DBZH&threshold=20").await;
    assert_eq!(status, StatusCode::OK);
    // glTF binary magic "glTF" + version 2.
    assert_eq!(&body[0..4], b"glTF", "glb magic");
    assert_eq!(
        u32::from_le_bytes(body[4..8].try_into().unwrap()),
        2,
        "glTF version"
    );
    assert_eq!(
        u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize,
        body.len(),
        "header length == actual length"
    );
    assert_eq!(
        headers[axum::http::header::CONTENT_TYPE],
        "model/gltf-binary"
    );
    assert!(headers.contains_key(axum::http::header::ETAG));
}

#[tokio::test]
async fn isosurface_threshold_at_or_below_floor_is_400() {
    // A threshold at/below the no-echo floor would place clear-air floor cells
    // inside the surface (all clear air renders as echo) — rejected.
    let floor = f64::from(ds_core::volume::NO_ECHO_FLOOR_DBZ);
    for t in [floor - 8.0, floor] {
        let (cs, _b, _h) = get(&format!(
            "/collections/radar-fivih/content.glb?threshold={t}"
        ))
        .await;
        assert_eq!(
            cs,
            StatusCode::BAD_REQUEST,
            "threshold {t} <= floor must 400"
        );
    }
    // Just above the floor is accepted.
    let (cs, _b, _h) = get(&format!(
        "/collections/radar-fivih/content.glb?threshold={}",
        floor + 2.0
    ))
    .await;
    assert_eq!(cs, StatusCode::OK, "threshold above floor is fine");
}

#[tokio::test]
async fn unknown_representation_is_400() {
    let (status, _b, _h) =
        get("/collections/radar-fivih/tileset.json?representation=hologram").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn non_finite_threshold_is_400() {
    for v in ["NaN", "inf"] {
        let (ts, _b, _h) = get(&format!(
            "/collections/radar-fivih/tileset.json?representation=isosurface&threshold={v}"
        ))
        .await;
        assert_eq!(ts, StatusCode::BAD_REQUEST, "tileset rejects threshold={v}");
        let (cs, _b, _h) = get(&format!(
            "/collections/radar-fivih/content.glb?threshold={v}"
        ))
        .await;
        assert_eq!(cs, StatusCode::BAD_REQUEST, "content rejects threshold={v}");
    }
}
