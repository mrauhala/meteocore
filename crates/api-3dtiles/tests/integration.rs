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
    ) -> Result<Arc<VoxelGrid>, DataServerError> {
        if let Some(q) = quantity {
            if q != "DBZH" {
                return Err(DataServerError::InvalidParameter(format!("unknown {q}")));
            }
        }
        let dims = dims.unwrap_or([16, 16, 16]);
        let [n_r, n_a, n_h] = dims;
        // Clear air is the finite no-echo floor (matching the post-#360 engine
        // fill), with a finite >threshold echo core spanning the middle half of
        // the radius/height axes (full angle ring) — so the isosurface (which
        // the handler seals with `background=Some(floor)` and then smooths
        // before marching, #381) produces a non-empty mesh: the core scales
        // with the requested grid and its interior survives the blur against
        // the floor, like a physical echo spanning many cells (a fixed
        // 2-cell-wide ring would smooth entirely below the threshold).
        let mut values = vec![ds_core::volume::NO_ECHO_FLOOR_DBZ; n_r * n_a * n_h];
        for i_r in n_r / 4..(n_r * 3 / 4) {
            for i_a in 0..n_a {
                for i_h in n_h / 4..(n_h * 3 / 4) {
                    values[VoxelGrid::index_of(dims, i_r, i_a, i_h)] = 40.0;
                }
            }
        }
        Ok(Arc::new(VoxelGrid {
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
        }))
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

/// A healthy voxel engine that has ingested no volumes yet: `volume_info`
/// carries `voxel_grid = Some` but with `radius_m = 0` (no coverage). The
/// `voxels` representation must NOT be advertised (its tileset 404s), while the
/// mesh products (which 404 on the *content* route, not the tileset) still are.
struct MockZeroRadiusVoxel;

impl VolumeEngine for MockZeroRadiusVoxel {
    fn read_point_cloud(
        &self,
        _quantity: Option<&str>,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _min_value: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        Err(DataServerError::LocationNotFound("no data yet".into()))
    }

    // read_voxel_grid uses the trait default (unsupported) — the test never
    // reaches it (the tileset 404s on the `radius_m == 0` gate first).

    fn volume_info(&self) -> Arc<VolumeInfo> {
        Arc::new(VolumeInfo {
            quantities: vec![("DBZH".into(), "Reflectivity".into())],
            times: vec![],
            default_quantity: "DBZH".into(),
            default_unit: "dBZ".into(),
            region: Some([0.42, 1.05, 0.44, 1.07, 100.0, 25_000.0]),
            voxel_grid: Some(VoxelGridCaps {
                origin: [24.5, 60.5, 100.0],
                radius_m: 0.0,
                height_m: 0.0,
            }),
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
        quantity_styles: HashMap::new(),
        collection_pages: HashMap::new(),
        content_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
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
    assert!(html.contains(r#"<script src="viewer.js"></script>"#));
    let (status, body, _) = get("/viewer.js").await;
    assert_eq!(status, StatusCode::OK);
    let script = String::from_utf8(body).unwrap();
    assert!(
        script.contains("/Build/Cesium/Cesium.js"),
        "viewer loads CesiumJS"
    );
    assert!(
        script.contains("/collections"),
        "viewer calls the collections API"
    );
}

/// The directive `name` of a CSP, as its source list.
fn csp_sources<'a>(csp: &'a str, name: &str) -> Vec<&'a str> {
    csp.split(';')
        .map(str::trim)
        .find_map(|d| {
            let mut words = d.split_whitespace();
            (words.next() == Some(name)).then(|| words.collect())
        })
        .unwrap_or_else(|| panic!("CSP has no {name}: {csp}"))
}

/// The viewer runs under a Content-Security-Policy (#1137): scripts from this
/// origin and the pinned CesiumJS release only, no inline script, and every
/// CDN URL the page loads is inside the allow-list — a version bump that
/// forgets the CSP fails here, not in production. (`'unsafe-eval'` is
/// allowed: CesiumJS does not load without it.) The page's script inserts
/// CesiumJS, so the CDN URLs are read from it.
#[tokio::test]
async fn viewer_page_runs_under_a_csp_matching_its_cdn() {
    let (_, body, headers) = get("/viewer").await;
    let html = String::from_utf8(body).unwrap();
    let (_, script, _) = get("/viewer.js").await;
    let script = String::from_utf8(script).unwrap();
    let csp = headers["content-security-policy"].to_str().unwrap();
    let scripts = csp_sources(csp, "script-src");
    assert!(scripts.contains(&"'self'"), "{csp}");
    for unsafe_source in ["'unsafe-inline'", "*", "https:", "data:", "blob:"] {
        assert!(
            !scripts.contains(&unsafe_source),
            "{unsafe_source} in {csp}"
        );
    }
    assert_eq!(csp_sources(csp, "default-src"), ["'none'"]);
    assert_eq!(csp_sources(csp, "frame-ancestors"), ["'none'"]);
    // Every external script and stylesheet the page loads sits under a source
    // its directive lists.
    let mut external = 0;
    for (attr, directive) in [("src: \"", "script-src"), ("href: \"", "style-src")] {
        for url in script
            .split(attr)
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
        {
            if !url.starts_with("https://") {
                continue;
            }
            external += 1;
            let allowed = csp_sources(csp, directive);
            assert!(
                allowed
                    .iter()
                    .any(|source| source.ends_with('/') && url.starts_with(source)),
                "{url} is outside {directive} {allowed:?}"
            );
            // CesiumJS fetches its workers and assets from the same release.
            let release = &url[..url.find("/Build/Cesium/").unwrap() + "/Build/Cesium/".len()];
            for fetched in ["connect-src", "img-src", "worker-src"] {
                assert!(
                    csp_sources(csp, fetched).contains(&release),
                    "{fetched}: {csp}"
                );
            }
        }
    }
    assert_eq!(external, 2, "Cesium.js and widgets.css");
    assert!(csp_sources(csp, "worker-src").contains(&"blob:"));
    // No inline script: the only <script> without a src would be one the CSP blocks.
    assert_eq!(
        html.matches("<script").count(),
        html.matches("<script src=").count()
    );
}

/// The viewer script revalidates by ETag, and is also served where a
/// trailing-slash page URL resolves its relative `viewer.js`.
#[tokio::test]
async fn viewer_script_is_etagged_and_served_under_both_page_urls() {
    let (status, body, headers) = get("/viewer.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[axum::http::header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/javascript"));
    let etag = headers[axum::http::header::ETAG]
        .to_str()
        .unwrap()
        .to_string();
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/viewer.js")
                .header(axum::http::header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let (status, nested, _) = get("/viewer/viewer.js").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(nested, body);
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

/// The landing document and the collection list are JSON only: `f=html`
/// is a 400 naming the format they have, as the other APIs' JSON-only
/// resources answer it (#1132); `f=json` and no `f` serve them.
#[tokio::test]
async fn json_only_documents_reject_other_formats() {
    for (path, what) in [
        ("/", "the 3D Tiles landing document"),
        ("/collections", "the 3D Tiles collection list"),
    ] {
        for f in ["html", "text%2Fhtml"] {
            let (status, body, _) = get(&format!("{path}?f={f}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path} {f}");
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                v["error"],
                format!(
                    "Format '{}' is not supported for {what}. Supported: json",
                    f.replace("%2F", "/")
                )
            );
        }
        for query in ["", "?f=json", "?f=application%2Fjson"] {
            let (status, _, _) = get(&format!("{path}{query}")).await;
            assert_eq!(status, StatusCode::OK, "{path}{query}");
        }
    }
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
    // The viewer opened on this volume; a 3D-Tiles-only collection has no
    // other API page to link.
    assert!(hrefs.contains(&"/3dtiles/viewer?collection=radar-fivih"));
    assert!(!v["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["rel"] == "related"));
    // With no resolved style, the default quantity keeps the shared
    // reflectivity ramp, legend and floor.
    let q = &v["quantities"][0];
    assert_eq!(q["unit"], "dBZ");
    assert_eq!(q["min_value"], 5.0);
    assert_eq!(q["legend"], v["legend"]);
}

#[tokio::test]
async fn voxels_not_advertised_without_coverage() {
    // A site whose engine supports voxels but has no scanned volume yet
    // (`radius_m = 0`) must NOT advertise the `voxels` representation — its
    // tileset 404s, which a `Cesium3DTilesVoxelProvider` can't distinguish from
    // "unsupported". The advertisement gate must match the tileset's gate.
    let engine: Arc<dyn VolumeEngine> = Arc::new(MockZeroRadiusVoxel);
    let mut volume_engines = HashMap::new();
    volume_engines.insert("radar-nocov".to_string(), engine);
    let mut collections = HashMap::new();
    collections.insert("radar-nocov".to_string(), collection_config("radar-nocov"));
    let state = Arc::new(ArcSwap::from_pointee(TilesState3d {
        volume_engines,
        collections,
        colormap: Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::RadarDbz,
            -32.0,
            95.0,
        )),
        quantity_styles: HashMap::new(),
        collection_pages: HashMap::new(),
        content_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
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

    // Collection doc: mesh products advertised, `voxels` NOT.
    let resp = send("/collections/radar-nocov").await;
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reps: Vec<&str> = v["representations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(
        reps.contains(&"isosurface") && reps.contains(&"echotop"),
        "mesh products still advertised: {reps:?}"
    );
    assert!(
        !reps.contains(&"voxels"),
        "voxels must NOT be advertised without coverage: {reps:?}"
    );
    // And the voxel tileset itself is 404 (the gate the advertisement now mirrors).
    assert_eq!(
        send("/collections/radar-nocov/voxel/tileset.json")
            .await
            .status(),
        StatusCode::NOT_FOUND
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
        quantity_styles: HashMap::new(),
        collection_pages: HashMap::new(),
        content_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
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

    // Single-tile implicit set: only the root 0/0/0/0 exists — a non-root
    // subtree/content coordinate is 404, not the root's constant data.
    let (status, _b, _h) = get("/collections/radar-fivih/voxel/subtrees/1/0/0/0.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _b, _h) =
        get("/collections/radar-fivih/voxel/content/0/0/0/1.glb?quantity=DBZH&resolution=low")
            .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

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
    // inside the surface (all clear air renders as echo) — rejected, on the
    // tileset route too (a bad threshold must 400 up front, not surprise the
    // client on the content fetch the tileset points at).
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
        let (ts, _b, _h) = get(&format!(
            "/collections/radar-fivih/tileset.json?representation=isosurface&threshold={t},20"
        ))
        .await;
        assert_eq!(
            ts,
            StatusCode::BAD_REQUEST,
            "tileset with lowest threshold {t} <= floor must 400"
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

#[tokio::test]
async fn nested_isosurface_thresholds_yield_blended_primitives() {
    // Tileset: an unsorted, duplicated list is canonicalised (sorted ascending,
    // deduped) into the content.uri so spelling variants share one content URL.
    let (status, body, _h) = get(
        "/collections/radar-fivih/tileset.json?representation=isosurface&quantity=DBZH&threshold=35,20,35",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["root"]["content"]["uri"].as_str().unwrap(),
        "content.glb?quantity=DBZH&threshold=20,35&resolution=med&representation=isosurface"
    );

    // Content: the mock grid's echo core is 40 dBZ, so 20 and 35 both yield a
    // shell while 50 is empty (skipped, not an error — a weak storm without a
    // 50 dBZ core must still show its envelope). One primitive + material per
    // surviving shell, innermost first; translucent shells are alpha-BLENDed.
    let (status, body, _h) =
        get("/collections/radar-fivih/content.glb?quantity=DBZH&threshold=20,35,50").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[0..4], b"glTF");
    let jlen = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
    let j: serde_json::Value = serde_json::from_slice(&body[20..20 + jlen]).unwrap();
    let prims = j["meshes"][0]["primitives"].as_array().unwrap();
    assert_eq!(prims.len(), 2, "50 dBZ shell is empty and skipped");
    let materials = j["materials"].as_array().unwrap();
    assert_eq!(materials.len(), 2);
    // Both surviving shells are translucent (the opaque slot belonged to the
    // empty 50 dBZ core) — alpha keyed to the REQUESTED list, so a frame where
    // the core vanishes doesn't re-shade the envelope.
    for m in materials {
        assert_eq!(m["alphaMode"], "BLEND");
        assert!(m["doubleSided"].as_bool().unwrap());
        let a = m["pbrMetallicRoughness"]["baseColorFactor"][3]
            .as_f64()
            .unwrap();
        assert!((0.0..1.0).contains(&a), "translucent alpha, got {a}");
    }
    // Innermost (35) first: its alpha is the higher of the two.
    let alpha = |i: usize| {
        materials[prims[i]["material"].as_u64().unwrap() as usize]["pbrMetallicRoughness"]
            ["baseColorFactor"][3]
            .as_f64()
            .unwrap()
    };
    assert!(alpha(0) > alpha(1), "inner shell more opaque than outer");

    // A single threshold keeps the classic fully-opaque shell (no alphaMode).
    let (status, body, _h) =
        get("/collections/radar-fivih/content.glb?quantity=DBZH&threshold=20").await;
    assert_eq!(status, StatusCode::OK);
    let jlen = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
    let j: serde_json::Value = serde_json::from_slice(&body[20..20 + jlen]).unwrap();
    assert_eq!(j["meshes"][0]["primitives"].as_array().unwrap().len(), 1);
    assert!(j["materials"][0].get("alphaMode").is_none());
}

#[tokio::test]
async fn bad_threshold_lists_are_400() {
    // Echo-top takes exactly one threshold — a list is isosurface-only.
    for path in [
        "/collections/radar-fivih/tileset.json?representation=echotop&threshold=18,30",
        "/collections/radar-fivih/content.glb?representation=echotop&threshold=18,30",
    ] {
        let (status, _b, _h) = get(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    // The shell cap counts DEDUPED values: six repeats collapse to one shell
    // and pass, six distinct values are over the cap (asserted below).
    let (status, _b, _h) = get(
        "/collections/radar-fivih/tileset.json?representation=isosurface&threshold=20,20,20,20,20,20",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "repeats collapse under the cap");
    // Junk inside a list, an empty slot, and an over-long list are all 400 on
    // both routes.
    for t in ["20,abc", "20,,35", "5,10,15,20,25,30"] {
        let (ts, _b, _h) = get(&format!(
            "/collections/radar-fivih/tileset.json?representation=isosurface&threshold={t}"
        ))
        .await;
        assert_eq!(ts, StatusCode::BAD_REQUEST, "tileset rejects {t:?}");
        let (cs, _b, _h) = get(&format!(
            "/collections/radar-fivih/content.glb?threshold={t}"
        ))
        .await;
        assert_eq!(cs, StatusCode::BAD_REQUEST, "content rejects {t:?}");
    }
    // A list whose LOWEST member is at/below the no-echo floor is rejected even
    // when other members are fine.
    let floor = f64::from(ds_core::volume::NO_ECHO_FLOOR_DBZ);
    let (cs, _b, _h) = get(&format!(
        "/collections/radar-fivih/content.glb?threshold={},20",
        floor
    ))
    .await;
    assert_eq!(cs, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Content cache + cache-control policy (hot-path audit, 2026-06)
// ---------------------------------------------------------------------------

/// `MockVolume` that counts engine reads, to prove the content cache short-
/// circuits the recompute (not just the transfer).
struct CountingVolume {
    reads: Arc<std::sync::atomic::AtomicU64>,
}

impl VolumeEngine for CountingVolume {
    fn read_point_cloud(
        &self,
        quantity: Option<&str>,
        time: Option<chrono::DateTime<chrono::Utc>>,
        min_value: Option<f64>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MockVolume.read_point_cloud(quantity, time, min_value, reference_time)
    }

    fn read_voxel_grid(
        &self,
        quantity: Option<&str>,
        time: Option<chrono::DateTime<chrono::Utc>>,
        dims: Option<[usize; 3]>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Arc<VoxelGrid>, DataServerError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MockVolume.read_voxel_grid(quantity, time, dims, reference_time)
    }

    fn volume_info(&self) -> Arc<VolumeInfo> {
        MockVolume.volume_info()
    }
}

/// A router over one named collection + engine (the shared `router()` is
/// fixed to `radar-fivih`/`MockVolume`). Cache-sensitive tests use unique
/// collection ids so the process-global content cache can't leak state
/// between tests.
fn router_with(id: &str, engine: Arc<dyn VolumeEngine>) -> axum::Router {
    let mut volume_engines = HashMap::new();
    volume_engines.insert(id.to_string(), engine);
    let mut collections = HashMap::new();
    collections.insert(id.to_string(), collection_config(id));
    let state = Arc::new(ArcSwap::from_pointee(TilesState3d {
        volume_engines,
        collections,
        colormap: Arc::new(LutColorMap::from_builtin(
            BuiltinColormap::RadarDbz,
            -32.0,
            95.0,
        )),
        quantity_styles: HashMap::new(),
        collection_pages: HashMap::new(),
        content_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_3dtiles::router(state)
}

async fn get_on(router: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let resp = router
        .clone()
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
async fn repeated_pnts_request_is_served_from_cache_without_recompute() {
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let app = router_with(
        "radar-cache-pnts",
        Arc::new(CountingVolume {
            reads: reads.clone(),
        }),
    );
    let uri = "/collections/radar-cache-pnts/content.pnts?quantity=DBZH";
    let (s1, b1, h1) = get_on(&app, uri).await;
    let (s2, b2, h2) = get_on(&app, uri).await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(b1, b2, "identical bytes");
    assert_eq!(
        h1[axum::http::header::ETAG],
        h2[axum::http::header::ETAG],
        "identical ETag"
    );
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "second request must hit the content cache, not the engine"
    );
}

/// An unknown quantity is a 400 before the content cache, the render slot
/// and the engine: distinct bogus names would otherwise each take a
/// scarce slot only to be rejected by the engine (#536).
#[tokio::test]
async fn unknown_quantity_never_reaches_the_engine() {
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let app = router_with(
        "radar-bogus-quantity",
        Arc::new(CountingVolume {
            reads: reads.clone(),
        }),
    );
    for uri in [
        "/collections/radar-bogus-quantity/content.pnts?quantity=BOGUS1",
        "/collections/radar-bogus-quantity/content.pnts?quantity=BOGUS2",
    ] {
        let (status, _, _) = get_on(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[tokio::test]
async fn glb_revalidation_304_does_not_recompute() {
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let app = router_with(
        "radar-cache-glb",
        Arc::new(CountingVolume {
            reads: reads.clone(),
        }),
    );
    let uri = "/collections/radar-cache-glb/content.glb?quantity=DBZH&resolution=low";
    let (s1, _b, h1) = get_on(&app, uri).await;
    assert_eq!(s1, StatusCode::OK);
    let etag = h1[axum::http::header::ETAG].to_str().unwrap().to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(axum::http::header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the 304 must be served from the content cache"
    );
}

#[tokio::test]
async fn pinned_datetime_gets_immutable_cache_control() {
    // Pinned to an advertised volume time → long-lived immutable.
    let (s, _b, h) =
        get("/collections/radar-fivih/content.pnts?quantity=DBZH&datetime=2026-05-15T00:05:00Z")
            .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h[axum::http::header::CACHE_CONTROL],
        "public, max-age=86400, immutable"
    );
    // Latest (no datetime) → short revalidation window.
    let (s, _b, h) = get("/collections/radar-fivih/content.pnts?quantity=DBZH").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[axum::http::header::CACHE_CONTROL], "public, max-age=60");
    // The tileset follows the same policy.
    let (s, _b, h) =
        get("/collections/radar-fivih/tileset.json?datetime=2026-05-15T00:05:00Z").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        h[axum::http::header::CACHE_CONTROL],
        "public, max-age=86400, immutable"
    );
}

#[tokio::test]
async fn non_manifest_datetime_is_not_immutable() {
    // A datetime BETWEEN advertised volume times resolves by nearest-volume
    // selection, which can change when a closer volume arrives — it must NOT
    // get the immutable policy (PR #378 review).
    let (s, _b, h) =
        get("/collections/radar-fivih/content.pnts?quantity=DBZH&datetime=2026-05-15T00:02:30Z")
            .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[axum::http::header::CACHE_CONTROL], "public, max-age=60");
}

/// Engine whose time manifest can grow mid-test — simulates a live collection
/// ingesting a new volume between requests.
struct GrowingVolume {
    reads: Arc<std::sync::atomic::AtomicU64>,
    times: std::sync::Mutex<Vec<chrono::DateTime<Utc>>>,
}

impl VolumeEngine for GrowingVolume {
    fn read_point_cloud(
        &self,
        quantity: Option<&str>,
        time: Option<chrono::DateTime<chrono::Utc>>,
        min_value: Option<f64>,
        reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MockVolume.read_point_cloud(quantity, time, min_value, reference_time)
    }

    fn volume_info(&self) -> Arc<VolumeInfo> {
        let base = MockVolume.volume_info();
        Arc::new(VolumeInfo {
            times: self.times.lock().unwrap().clone(),
            ..(*base).clone()
        })
    }
}

#[tokio::test]
async fn exact_time_entry_survives_new_volume_arrival() {
    // A new volume arriving must NOT evict pinned exact-time entries (they
    // are immutable) — only "latest"/non-exact entries re-key (PR #378 r2).
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let t0 = Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap();
    let t1 = Utc.with_ymd_and_hms(2026, 5, 15, 0, 5, 0).unwrap();
    let engine = Arc::new(GrowingVolume {
        reads: reads.clone(),
        times: std::sync::Mutex::new(vec![t0, t1]),
    });
    let app = router_with("radar-grow", engine.clone());

    let pinned_uri =
        "/collections/radar-grow/content.pnts?quantity=DBZH&datetime=2026-05-15T00:00:00Z";
    let latest_uri = "/collections/radar-grow/content.pnts?quantity=DBZH";
    let (s, _b, _h) = get_on(&app, pinned_uri).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _b, _h) = get_on(&app, latest_uri).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);

    // A new volume arrives.
    engine
        .times
        .lock()
        .unwrap()
        .push(Utc.with_ymd_and_hms(2026, 5, 15, 0, 10, 0).unwrap());

    // The pinned exact-time frame is still served from the cache…
    let (s, _b, _h) = get_on(&app, pinned_uri).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "exact-time entry must survive the manifest change"
    );
    // …while "latest" re-keys (data_version changed) and recomputes.
    let (s, _b, _h) = get_on(&app, latest_uri).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "latest entry must re-key on a new volume"
    );
}

/// A real blocking engine call that can outlive its HTTP waiter. Point reads
/// block until released; voxel reads are only counted.
struct GatedVolume {
    started: Arc<tokio::sync::Notify>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    reads: std::sync::atomic::AtomicU64,
    voxel_reads: std::sync::atomic::AtomicU64,
}
impl GatedVolume {
    fn new() -> (Arc<Self>, std::sync::mpsc::Sender<()>) {
        let (release, receiver) = std::sync::mpsc::channel();
        let engine = Arc::new(Self {
            started: Arc::new(tokio::sync::Notify::new()),
            release: std::sync::Mutex::new(receiver),
            reads: std::sync::atomic::AtomicU64::new(0),
            voxel_reads: std::sync::atomic::AtomicU64::new(0),
        });
        (engine, release)
    }
}
impl VolumeEngine for GatedVolume {
    fn volume_info(&self) -> Arc<VolumeInfo> {
        MockVolume.volume_info()
    }
    fn read_point_cloud(
        &self,
        quantity: Option<&str>,
        time: Option<chrono::DateTime<Utc>>,
        min_value: Option<f64>,
        reference_time: Option<chrono::DateTime<Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.started.notify_one();
        self.release.lock().unwrap().recv().unwrap();
        MockVolume.read_point_cloud(quantity, time, min_value, reference_time)
    }
    fn read_voxel_grid(
        &self,
        quantity: Option<&str>,
        time: Option<chrono::DateTime<Utc>>,
        dims: Option<[usize; 3]>,
        reference_time: Option<chrono::DateTime<Utc>>,
    ) -> Result<Arc<VoxelGrid>, DataServerError> {
        self.voxel_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        MockVolume.read_voxel_grid(quantity, time, dims, reference_time)
    }
}

/// A router over one collection whose content computes draw from `pool`.
fn router_with_pool(
    id: &str,
    engine: Arc<dyn VolumeEngine>,
    pool: Arc<tokio::sync::Semaphore>,
) -> axum::Router {
    api_3dtiles::router(Arc::new(ArcSwap::from_pointee(TilesState3d {
        volume_engines: [(id.into(), engine)].into(),
        collections: [(id.into(), collection_config(id))].into(),
        colormap: api_3dtiles::default_point_colormap(),
        quantity_styles: HashMap::new(),
        collection_pages: HashMap::new(),
        content_semaphore: pool,
        base_url: String::new(),
        trust_proxy_headers: false,
    })))
}

#[tokio::test]
async fn canceled_http_waiter_keeps_content_permit_and_populates_cache() {
    let (engine, release) = GatedVolume::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let app = router_with_pool("radar-cancel", engine.clone(), semaphore.clone());
    let uri = "/collections/radar-cancel/content.pnts";
    let first = app.clone();
    let request = tokio::spawn(async move { get_on(&first, uri).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), engine.started.notified())
        .await
        .unwrap();
    request.abort();
    let _ = request.await;
    let held = semaphore.available_permits();
    release.send(()).unwrap();
    assert_eq!(held, 0, "blocking encode must still own the permit");
    let (status, _, _) = tokio::time::timeout(std::time::Duration::from_secs(2), get_on(&app, uri))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(engine.reads.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(semaphore.available_permits(), 1);
}

/// #380: points and mesh computes run on the 3D content pool, so they complete
/// with every raster render slot taken — and a cold 3D preload never occupies
/// one. Wired like the server: `CONTENT_SLOTS` in the state, while the
/// process-wide `RENDER_SLOTS` WMS/Maps/Tiles share are all held.
#[tokio::test]
async fn content_computes_while_every_raster_render_slot_is_taken() {
    let raster = ds_executor::RENDER_SLOTS.clone();
    let _busy = raster
        .clone()
        .try_acquire_many_owned(ds_executor::render_concurrency() as u32)
        .expect("no other test in this binary takes raster slots");
    let app = router_with_pool(
        "radar-raster-busy",
        Arc::new(MockVolume),
        api_3dtiles::CONTENT_SLOTS.clone(),
    );
    for uri in [
        "/collections/radar-raster-busy/content.pnts",
        "/collections/radar-raster-busy/content.glb?resolution=low",
        "/collections/radar-raster-busy/content.glb?representation=echotop&resolution=low",
    ] {
        // Far inside the 30 s volume deadline a raster-slot wait would burn.
        let (status, _, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), get_on(&app, uri))
                .await
                .unwrap_or_else(|_| panic!("{uri} waited for a raster render slot"));
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    assert_eq!(raster.available_permits(), 0);
}

/// #380: voxels share the one content pool with points and meshes, so total
/// 3D CPU has one bound. While a point compute holds the only permit, a voxel
/// compute waits instead of starting.
#[tokio::test]
async fn voxel_content_waits_on_the_shared_content_pool() {
    let (engine, release) = GatedVolume::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let app = router_with_pool("radar-pool-share", engine.clone(), semaphore.clone());
    let spawn = |uri: &'static str| {
        let app = app.clone();
        tokio::spawn(async move { get_on(&app, uri).await })
    };
    let points = spawn("/collections/radar-pool-share/content.pnts");
    tokio::time::timeout(std::time::Duration::from_secs(2), engine.started.notified())
        .await
        .unwrap();
    let voxels = spawn("/collections/radar-pool-share/voxel/content/0/0/0/0.glb?resolution=low");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        engine
            .voxel_reads
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "voxel compute must wait for the point compute's permit"
    );
    release.send(()).unwrap();
    for task in [points, voxels] {
        let (status, _, _) = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(
        engine
            .voxel_reads
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(semaphore.available_permits(), 1);
}

// ---------------------------------------------------------------------------
// Per-quantity drawing (#1137)
// ---------------------------------------------------------------------------

/// A dual-pol volume: reflectivity, radial velocity and a correlation
/// coefficient. Each point cloud is one point at a value that sits on a stop
/// of the quantity's style below.
struct MockMoments;

const MOMENT_VALUES: [(&str, f64); 3] = [("DBZH", 50.0), ("VRADH", 48.0), ("RHOHV", 1.0)];

impl VolumeEngine for MockMoments {
    fn read_point_cloud(
        &self,
        quantity: Option<&str>,
        _time: Option<chrono::DateTime<chrono::Utc>>,
        _min_value: Option<f64>,
        _reference_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<VolumePointCloud, DataServerError> {
        let quantity = quantity.unwrap_or("DBZH");
        let value = MOMENT_VALUES
            .iter()
            .find(|(q, _)| *q == quantity)
            .map(|(_, v)| *v)
            .expect("advertised quantity");
        Ok(VolumePointCloud {
            rtc_center: [3_000_000.0, 1_000_000.0, 5_000_000.0],
            region: [0.42, 1.05, 0.44, 1.07, 100.0, 12_000.0],
            points: vec![VolumePoint {
                offset: [0.0, 0.0, 0.0],
                value,
            }],
            quantity: quantity.into(),
            unit: String::new(),
        })
    }

    fn volume_info(&self) -> Arc<VolumeInfo> {
        Arc::new(VolumeInfo {
            quantities: moment_parameters()
                .into_iter()
                .map(|p| (p.name, p.title))
                .collect(),
            times: vec![],
            default_quantity: "DBZH".into(),
            default_unit: "dBZ".into(),
            region: Some([0.42, 1.05, 0.44, 1.07, 100.0, 25_000.0]),
            voxel_grid: None,
        })
    }
}

fn moment_parameters() -> Vec<ds_core::map_engine::ParameterInfo> {
    [
        ("DBZH", "DBZH — Reflectivity (horizontal)", "dBZ"),
        ("VRADH", "VRADH — Radial velocity (horizontal)", "m/s"),
        ("RHOHV", "RHOHV — Correlation coefficient", ""),
    ]
    .map(|(name, title, unit)| ds_core::map_engine::ParameterInfo {
        name: name.into(),
        title: title.into(),
        unit: unit.into(),
    })
    .into()
}

/// A PVOL-style collection: reflectivity on the dBZ palette, velocity on its
/// own diverging stops ending at `velocity_top`, the correlation coefficient
/// on viridis over 0–1 — the per-parameter `[wms]` styling of the shipped
/// example configs.
fn moments_config(id: &str, velocity_top: &str) -> CollectionConfig {
    toml::from_str(&format!(
        r##"id = "{id}"
title = "Dual-pol volume"
description = "test"
engine_type = "odim-volume"
apis = ["edr", "maps", "3dtiles"]

[wms]
colormap = "radar_dbz"

[[wms.parameters]]
name = "VRADH"
min = -48.0
max = 48.0
[[wms.parameters.color_stops]]
value = -48.0
color = "#1a1aff"
[[wms.parameters.color_stops]]
value = 0.0
color = "#f5f5f5"
[[wms.parameters.color_stops]]
value = 48.0
color = "{velocity_top}"

[[wms.parameters]]
name = "RHOHV"
colormap = "viridis"
min = 0.0
max = 1.0
"##
    ))
    .expect("collection config parses")
}

/// A router over `MockMoments` whose quantity styles are resolved the way the
/// server resolves them: the shared parameter-layer style path, then
/// `quantity_styles`.
fn moments_router(id: &str, velocity_top: &str) -> (axum::Router, api_3dtiles::QuantityStyles) {
    let config = moments_config(id, velocity_top);
    let layers = ds_render::StyleContext::with_builtins()
        .parameter_layer_styles(&config, None, &moment_parameters(), &|_, c| c)
        .expect("styles resolve");
    let styles = api_3dtiles::quantity_styles(id, &moment_parameters(), &layers);
    let state = Arc::new(ArcSwap::from_pointee(TilesState3d {
        volume_engines: HashMap::from([(
            id.to_string(),
            Arc::new(MockMoments) as Arc<dyn VolumeEngine>,
        )]),
        collections: HashMap::from([(id.to_string(), config)]),
        colormap: api_3dtiles::default_point_colormap(),
        quantity_styles: HashMap::from([(id.to_string(), styles.clone())]),
        // As the server resolves it for a collection Maps serves.
        collection_pages: HashMap::from([(id.to_string(), format!("/collections/{id}"))]),
        content_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    (api_3dtiles::router(state), styles)
}

/// The RGB of a one-point `.pnts`: header, feature-table JSON, then POSITION
/// (12 bytes) and RGB in the feature-table binary.
fn pnts_rgb(bytes: &[u8]) -> [u8; 3] {
    assert_eq!(&bytes[..4], b"pnts");
    let ft_json = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let rgb = 28 + ft_json + 12;
    [bytes[rgb], bytes[rgb + 1], bytes[rgb + 2]]
}

/// The collection document gives each quantity its unit, the legend of its own
/// ramp and a suggested floor (reflectivity only); the top-level legend stays
/// the default quantity's.
#[tokio::test]
async fn collection_doc_carries_unit_and_legend_per_quantity() {
    let (app, _) = moments_router("moments-doc", "#cc0000");
    let (status, body, _) = get_on(&app, "/collections/moments-doc").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let q = |id: &str| {
        v["quantities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["id"] == id)
            .unwrap()
            .clone()
    };
    let (dbzh, vradh, rhohv) = (q("DBZH"), q("VRADH"), q("RHOHV"));
    // Reflectivity: dBZ, the radar palette's 0–70 dBZ, the 5 dBZ floor.
    assert_eq!(dbzh["unit"], "dBZ");
    assert_eq!(dbzh["min_value"], 5.0);
    assert_eq!(dbzh["legend"]["unit"], "dBZ");
    assert_eq!(dbzh["legend"]["title"], "DBZH — Reflectivity (horizontal)");
    assert_eq!(
        (
            dbzh["legend"]["min"].as_f64(),
            dbzh["legend"]["max"].as_f64()
        ),
        (Some(0.0), Some(70.0))
    );
    // Velocity: m/s over its configured ±48, no floor (approaching flow is
    // negative and must not be dropped).
    assert_eq!(vradh["unit"], "m/s");
    assert!(vradh["min_value"].is_null());
    assert_eq!(vradh["legend"]["unit"], "m/s");
    assert_eq!(
        (
            vradh["legend"]["min"].as_f64(),
            vradh["legend"]["max"].as_f64()
        ),
        (Some(-48.0), Some(48.0))
    );
    let stops = vradh["legend"]["stops"].as_array().unwrap();
    assert_eq!(stops.first().unwrap()["color"], "#1a1aff");
    assert_eq!(stops.last().unwrap()["color"], "#cc0000");
    // Correlation coefficient: unitless 0–1, no floor.
    assert_eq!(rhohv["unit"], "");
    assert!(rhohv["min_value"].is_null());
    assert_eq!(
        (
            rhohv["legend"]["min"].as_f64(),
            rhohv["legend"]["max"].as_f64()
        ),
        (Some(0.0), Some(1.0))
    );
    assert_eq!(v["legend"], dbzh["legend"]);
    // The collection is served by Maps too: its page on the shared root, and
    // the viewer opened on it.
    let links = v["links"].as_array().unwrap();
    let link = |rel: &str| {
        links
            .iter()
            .find(|l| l["rel"] == rel && l["type"] == "text/html")
            .map(|l| l["href"].as_str().unwrap().to_string())
    };
    assert_eq!(
        link("related").as_deref(),
        Some("/collections/moments-doc?f=html")
    );
    assert_eq!(
        link("alternate").as_deref(),
        Some("/3dtiles/viewer?collection=moments-doc")
    );

    // The collection list keeps each quantity's unit and floor but leaves the
    // legends to the collection's own document; the default quantity's stays.
    let (_, body, _) = get_on(&app, "/collections").await;
    let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entry = &list["collections"][0];
    let quantities = entry["quantities"].as_array().unwrap();
    assert_eq!(quantities.len(), 3);
    assert!(quantities.iter().all(|q| q.get("legend").is_none()));
    assert_eq!(quantities[1]["unit"], "m/s");
    assert_eq!(entry["legend"], dbzh["legend"]);
    assert_eq!(entry["links"], v["links"]);
}

/// Points are painted with their quantity's ramp, the one the legend shows —
/// not the reflectivity ramp for every quantity.
#[tokio::test]
async fn points_are_painted_with_their_quantity_ramp() {
    let (app, styles) = moments_router("moments-paint", "#cc0000");
    for (quantity, value) in MOMENT_VALUES {
        let (status, body, _) = get_on(
            &app,
            &format!("/collections/moments-paint/content.pnts?quantity={quantity}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{quantity}");
        let [r, g, b, _] = styles[quantity].colormap().color(Some(value));
        assert_eq!(pnts_rgb(&body), [r, g, b], "{quantity}");
    }
    let (_, body, _) = get_on(
        &app,
        "/collections/moments-paint/content.pnts?quantity=VRADH",
    )
    .await;
    assert_eq!(
        pnts_rgb(&body),
        [0xcc, 0x00, 0x00],
        "the velocity ramp's top"
    );
}

/// Restyling a quantity (a reload with new `[wms]` styling) repaints its
/// points: the content cache keys on the ramp, so the old colours are not
/// served from it.
#[tokio::test]
async fn a_restyled_quantity_is_not_served_from_the_content_cache() {
    let uri = "/collections/moments-restyle/content.pnts?quantity=VRADH";
    let (before, _) = moments_router("moments-restyle", "#cc0000");
    let (_, body, _) = get_on(&before, uri).await;
    assert_eq!(pnts_rgb(&body), [0xcc, 0x00, 0x00]);
    let (after, _) = moments_router("moments-restyle", "#00ff00");
    let (_, body, _) = get_on(&after, uri).await;
    assert_eq!(pnts_rgb(&body), [0x00, 0xff, 0x00]);
}

/// Isosurface shells are painted with the quantity's ramp too (#1137): a
/// reflectivity styled flat magenta draws a magenta 20 dBZ shell, where the
/// shared radar ramp would not.
#[tokio::test]
async fn isosurface_shells_are_painted_with_their_quantity_ramp() {
    let id = "radar-iso-style";
    let config: CollectionConfig = toml::from_str(&format!(
        r##"id = "{id}"
title = "Mock Radar Volume"
description = "test"
engine_type = "odim-volume"
apis = ["3dtiles"]

[wms]
colormap = "radar_dbz"

[[wms.parameters]]
name = "DBZH"
min = 0.0
max = 70.0
[[wms.parameters.color_stops]]
value = 0.0
color = "#ff00ff"
[[wms.parameters.color_stops]]
value = 70.0
color = "#ff00ff"
"##
    ))
    .expect("collection config parses");
    let parameters = vec![ds_core::map_engine::ParameterInfo {
        name: "DBZH".into(),
        title: "Reflectivity".into(),
        unit: "dBZ".into(),
    }];
    let layers = ds_render::StyleContext::with_builtins()
        .parameter_layer_styles(&config, None, &parameters, &|_, c| c)
        .expect("styles resolve");
    let styles = api_3dtiles::quantity_styles(id, &parameters, &layers);
    let app = api_3dtiles::router(Arc::new(ArcSwap::from_pointee(TilesState3d {
        volume_engines: HashMap::from([(
            id.to_string(),
            Arc::new(MockVolume) as Arc<dyn VolumeEngine>,
        )]),
        collections: HashMap::from([(id.to_string(), config)]),
        colormap: api_3dtiles::default_point_colormap(),
        quantity_styles: HashMap::from([(id.to_string(), styles)]),
        collection_pages: HashMap::new(),
        content_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        base_url: String::new(),
        trust_proxy_headers: false,
    })));
    let (status, body, _) = get_on(
        &app,
        &format!("/collections/{id}/content.glb?quantity=DBZH&threshold=20"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let jlen = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
    let j: serde_json::Value = serde_json::from_slice(&body[20..20 + jlen]).unwrap();
    let rgb: Vec<f64> = j["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"]
        .as_array()
        .unwrap()[..3]
        .iter()
        .map(|c| c.as_f64().unwrap())
        .collect();
    assert_eq!(rgb, [1.0, 0.0, 1.0]);
}
