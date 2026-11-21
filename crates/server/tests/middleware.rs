//! Server-level integration tests for cross-cutting middleware concerns.
//!
//! These tests assemble the full router with mock engines and verify
//! gzip compression, conditional requests (304), and load shedding (503).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;

use api_wms::WmsState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::{BuiltinColormap, LutColorMap, RenderedCache, StyleInfo};

// ---------------------------------------------------------------------------
// Mock engine
// ---------------------------------------------------------------------------

struct MockMapEngine;

impl MapEngine for MockMapEngine {
    #[allow(clippy::too_many_arguments)]
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
        let n = (width * height) as usize;
        Ok(RasterTile {
            width,
            height,
            values: (0..n)
                .map(|i| Some(i as f64 / n as f64))
                .collect::<Vec<_>>()
                .into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        RasterInfo {
            native_crs: "EPSG:4326".to_string(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: vec![chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)],
            parameter: "reflectivity".to_string(),
            unit: "dBZ".to_string(),
            parameters: vec![],
            vertical: None,
            grid_size: None,
            layer_subtitle: None,
            reference_times: Vec::new(),
            native_grid: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_collection(id: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: "Test".to_string(),
        description: "Test data".to_string(),
        data_path: None,
        apis: vec!["wms".to_string()],
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
        station_grid: None,
        period: None,
        bufr: None,
        satellite: None,
        netcdf: None,
        csv: None,
        preview: None,
        derive_wind: None,
        derive_geopotential_height: None,
        discovery: None,
        derive_humidity: None,
        expected_interval: None,
    }
}

fn make_styles() -> HashMap<String, StyleInfo> {
    let mut m = HashMap::new();
    m.insert(
        "default".to_string(),
        StyleInfo {
            name: "default".to_string(),
            title: "Default".to_string(),
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
    );
    m
}

fn build_wms_router() -> axum::Router {
    let engine: Arc<dyn MapEngine> = Arc::new(MockMapEngine);
    let mut engines = HashMap::new();
    let mut collections = HashMap::new();
    let mut styles = HashMap::new();

    engines.insert("radar".to_string(), engine);
    collections.insert("radar".to_string(), make_collection("radar"));
    styles.insert("radar".to_string(), make_styles());

    let state = Arc::new(ArcSwap::from_pointee(WmsState {
        engines,
        collections,
        styles,
        point_engines: HashMap::new(),
        render_semaphore: Arc::new(tokio::sync::Semaphore::new(4)),
        rendered_cache: Arc::new(RenderedCache::new(16)),
        tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
        base_url: String::new(),
        trust_proxy_headers: false,
    }));
    api_wms::router(state)
}

/// Build the WMS router wrapped in CompressionLayer (like main.rs does).
fn build_compressed_router() -> axum::Router {
    build_wms_router().layer(CompressionLayer::new())
}

const WMS_GETMAP: &str = "/?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap\
    &LAYERS=radar&CRS=CRS:84&STYLES=default\
    &BBOX=10,55,30,70&WIDTH=64&HEIGHT=64&FORMAT=image/png";

const WMS_GETCAP: &str = "/?SERVICE=WMS&REQUEST=GetCapabilities";

async fn wms_request(
    router: axum::Router,
    uri: &str,
    headers: Vec<(&str, &str)>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().uri(uri);
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, resp_headers, body)
}

// ---------------------------------------------------------------------------
// Gzip compression tests
// ---------------------------------------------------------------------------

mod compression {
    use super::*;

    #[tokio::test]
    async fn getcapabilities_compressed_when_accepted() {
        let app = build_compressed_router();
        let (status, headers, _body) =
            wms_request(app, WMS_GETCAP, vec![("accept-encoding", "gzip")]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-encoding").map(|v| v.to_str().unwrap()),
            Some("gzip")
        );
    }

    #[tokio::test]
    async fn getcapabilities_uncompressed_without_header() {
        let app = build_compressed_router();
        let (status, headers, _body) = wms_request(app, WMS_GETCAP, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn png_not_double_compressed() {
        let app = build_compressed_router();
        let (status, _headers, body) =
            wms_request(app, WMS_GETMAP, vec![("accept-encoding", "gzip")]).await;
        assert_eq!(status, StatusCode::OK);
        // PNG magic bytes should be present (not wrapped in gzip)
        // CompressionLayer skips image/* content types
        let is_png = body.starts_with(&[0x89, b'P', b'N', b'G']);
        let is_gzip = body.starts_with(&[0x1f, 0x8b]);
        assert!(
            is_png || !is_gzip,
            "PNG response should not be gzip-compressed"
        );
    }

    /// GetCapabilities is gzipped by the WMS handler, once per cached
    /// document (#1482), and the layer passes its answer through as it is:
    /// never compressed twice, never left uncompressed where the layer
    /// would have compressed it, and with one `Vary: Accept-Encoding`. The
    /// handler reads `Accept-Encoding` as the layer does, a coding listed
    /// twice included, so with and without the layer every request gets the
    /// same bytes and encoding.
    #[tokio::test]
    async fn getcapabilities_passes_the_layer_as_the_handler_encoded_it() {
        for accept in [
            None,
            Some("gzip"),
            Some("gzip, deflate, br"),
            Some("br;q=1.0, gzip;q=0.8, *;q=0.1"),
            Some("gzip;q=0.5"),
            Some("*"),
            Some("identity"),
            Some("gzip;q=0"),
            Some("identity;q=1, gzip;q=0.5"),
            Some("*;q=0.5, identity"),
            Some("deflate"),
            Some("gzip;q=0, gzip"),
            Some("gzip;q=0.5, identity;q=0, identity"),
        ] {
            let headers: Vec<(&str, &str)> =
                accept.iter().map(|a| ("accept-encoding", *a)).collect();
            let (status, layered, layered_body) =
                wms_request(build_compressed_router(), WMS_GETCAP, headers.clone()).await;
            let (_, bare, bare_body) = wms_request(build_wms_router(), WMS_GETCAP, headers).await;
            assert_eq!(status, StatusCode::OK, "{accept:?}");
            assert_eq!(layered_body, bare_body, "{accept:?}");
            let encodings = |h: &axum::http::HeaderMap| -> Vec<String> {
                h.get_all(header::CONTENT_ENCODING)
                    .iter()
                    .map(|v| v.to_str().unwrap().to_string())
                    .collect()
            };
            assert_eq!(encodings(&layered), encodings(&bare), "{accept:?}");
            assert!(encodings(&layered).len() <= 1, "{accept:?}");
            let vary: Vec<_> = layered.get_all(header::VARY).iter().collect();
            assert_eq!(vary, ["accept-encoding"], "{accept:?}");
            let gzipped = layered_body.starts_with(&[0x1f, 0x8b]);
            assert_eq!(
                gzipped,
                encodings(&layered) == ["gzip"],
                "{accept:?}: the body is gzipped exactly when it says so"
            );
        }
    }
}

/// An EDR router with no collections, wrapped in CompressionLayer as
/// main.rs wraps every router: its metadata is JSON the layer compresses.
fn build_compressed_edr_router() -> axum::Router {
    let state = Arc::new(ArcSwap::from_pointee(api_edr::handlers::EdrState {
        engines: HashMap::new(),
        feature_engines: HashMap::new(),
        collections: HashMap::new(),
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
        discovery: Default::default(),
    }));
    api_edr::router(state).layer(CompressionLayer::new())
}

mod edr_compression {
    use super::*;

    /// The EDR (and Features) ETag is set inside the router and the body
    /// gzipped outside it, so the plain and the gzipped responses carry one
    /// tag: it is weak, `W/"…"`, since a strong tag would name two
    /// different byte sequences as one (#1500). Either response's tag
    /// revalidates the other, in either form.
    #[tokio::test]
    async fn gzip_and_identity_carry_one_weak_tag() {
        const URI: &str = "/conformance?f=json";
        let (status, plain, plain_body) =
            wms_request(build_compressed_edr_router(), URI, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(plain.get(header::CONTENT_ENCODING).is_none());
        let (status, gzipped, gzipped_body) = wms_request(
            build_compressed_edr_router(),
            URI,
            vec![("accept-encoding", "gzip")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(gzipped[header::CONTENT_ENCODING], "gzip");
        assert_ne!(plain_body, gzipped_body);
        let etag = plain[header::ETAG].to_str().unwrap().to_string();
        assert_eq!(gzipped[header::ETAG], etag.as_str());
        assert_eq!(etag, ds_core::http_cache::weak_etag_of(&plain_body));

        let strong = etag.trim_start_matches("W/").to_string();
        for accept in ["identity", "gzip"] {
            for sent in [&etag, &strong] {
                let (status, headers, body) = wms_request(
                    build_compressed_edr_router(),
                    URI,
                    vec![("accept-encoding", accept), ("if-none-match", sent)],
                )
                .await;
                assert_eq!(status, StatusCode::NOT_MODIFIED, "{accept} {sent}");
                assert_eq!(headers[header::ETAG], etag.as_str(), "{accept} {sent}");
                assert!(body.is_empty(), "{accept} {sent}");
            }
        }
    }
}

/// The wire: `app` served on a local port as main.rs serves its router,
/// through hyper, which a `oneshot` skips.
mod wire {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// One GET over a fresh connection: the status and every header line,
    /// names lowercased.
    pub async fn get(
        addr: std::net::SocketAddr,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (u16, Vec<(String, String)>) {
        let exchange = async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut request =
                format!("GET {uri} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n");
            for (name, value) in headers {
                request.push_str(&format!("{name}: {value}\r\n"));
            }
            request.push_str("\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await.unwrap();
            raw
        };
        let raw = tokio::time::timeout(std::time::Duration::from_secs(10), exchange)
            .await
            .expect("the server answered");
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(raw[..end].to_vec()).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap().split(' ').nth(1).unwrap();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.trim().to_ascii_lowercase(), value.trim().to_string())
            })
            .collect();
        (status.parse().unwrap(), headers)
    }

    /// Every value of `name`, each line split at its commas, sorted.
    pub fn values(headers: &[(String, String)], name: &str) -> Vec<String> {
        let mut values: Vec<String> = headers
            .iter()
            .filter(|(n, _)| n == name)
            .flat_map(|(_, v)| v.split(',').map(|v| v.trim().to_ascii_lowercase()))
            .collect();
        values.sort();
        values
    }
}

mod not_modified_on_the_wire {
    use super::*;

    /// A 304 carries the `Vary` its 200 does (RFC 9110 §15.4.5): the
    /// handler's `accept`, and the `accept-encoding` the compression layer
    /// gives a body it compresses, in neither case for an image. And it
    /// states no `Content-Length` (§8.6), which a gzipped 200 does not
    /// state either: axum puts `content-length: 0` in the empty body's
    /// headers, and hyper sends no length without a body. Nor a
    /// `Content-Encoding` (#1500).
    #[tokio::test]
    async fn a_304_varies_as_its_200_and_states_no_length() {
        let edr = wire::serve(build_compressed_edr_router()).await;
        let wms = wire::serve(build_compressed_router()).await;
        for (addr, uri, compressed) in [
            (edr, "/conformance?f=json", true),
            (edr, "/conformance", true),
            (wms, WMS_GETMAP, false),
        ] {
            for accept in ["gzip", "identity"] {
                let (status, ok) = wire::get(addr, uri, &[("accept-encoding", accept)]).await;
                assert_eq!(status, 200, "{uri} {accept}");
                let vary = wire::values(&ok, "vary");
                assert_eq!(
                    vary.contains(&"accept-encoding".to_string()),
                    compressed,
                    "{uri} {accept}: {vary:?}"
                );
                let etag = ok.iter().find(|(n, _)| n == "etag").unwrap().1.clone();
                let (status, not_modified) = wire::get(
                    addr,
                    uri,
                    &[("accept-encoding", accept), ("if-none-match", &etag)],
                )
                .await;
                assert_eq!(status, 304, "{uri} {accept}");
                assert_eq!(wire::values(&not_modified, "vary"), vary, "{uri} {accept}");
                let tags: Vec<_> = not_modified.iter().filter(|(n, _)| n == "etag").collect();
                assert_eq!(
                    tags,
                    [&("etag".to_string(), etag.clone())],
                    "{uri} {accept}"
                );
                for absent in ["content-length", "content-encoding", "content-type"] {
                    assert!(
                        wire::values(&not_modified, absent).is_empty(),
                        "{uri} {accept}: {absent} in {not_modified:?}"
                    );
                }
            }
        }
    }

    /// `If-None-Match` on several lines is one list: a tag on its second
    /// line is a 304 (RFC 9110 §5.3).
    #[tokio::test]
    async fn every_if_none_match_line_counts() {
        let edr = wire::serve(build_compressed_edr_router()).await;
        let (_, ok) = wire::get(edr, "/conformance", &[]).await;
        let etag = ok.iter().find(|(n, _)| n == "etag").unwrap().1.clone();
        let (status, _) = wire::get(
            edr,
            "/conformance",
            &[("if-none-match", "\"other\""), ("if-none-match", &etag)],
        )
        .await;
        assert_eq!(status, 304);
    }
}

// ---------------------------------------------------------------------------
// Conditional requests (304 Not Modified)
// ---------------------------------------------------------------------------

mod conditional_requests {
    use super::*;

    #[tokio::test]
    async fn returns_304_for_matching_etag() {
        let app = build_wms_router();

        // First request — get the ETag
        let (status, headers, _) = wms_request(app, WMS_GETMAP, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        let etag = headers
            .get(header::ETAG)
            .expect("response should have ETag")
            .to_str()
            .unwrap()
            .to_string();

        // Second request with If-None-Match
        let app = build_wms_router();
        let (status, _, body) = wms_request(app, WMS_GETMAP, vec![("if-none-match", &etag)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty(), "304 body should be empty");
    }

    #[tokio::test]
    async fn returns_200_for_non_matching_etag() {
        let app = build_wms_router();
        let (status, _, body) =
            wms_request(app, WMS_GETMAP, vec![("if-none-match", "\"wrongetag\"")]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn returns_304_for_wildcard() {
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", "*")]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn returns_304_for_etag_in_list() {
        let app = build_wms_router();

        // Get the real ETag first
        let (_, headers, _) = wms_request(app, WMS_GETMAP, vec![]).await;
        let etag = headers.get(header::ETAG).unwrap().to_str().unwrap();

        // Send it in a comma-separated list
        let multi = format!("\"aaa\", {etag}, \"zzz\"");
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", &multi)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn returns_304_for_weak_etag() {
        let app = build_wms_router();

        let (_, headers, _) = wms_request(app, WMS_GETMAP, vec![]).await;
        let etag = headers.get(header::ETAG).unwrap().to_str().unwrap();

        // Wrap in W/ weak prefix
        let weak = format!("W/{etag}");
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", &weak)]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn not_modified_includes_etag_and_cache_control() {
        let app = build_wms_router();
        let (status, _, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", "*")]).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);

        // Rebuild for the assertion (oneshot consumes)
        let app = build_wms_router();
        let (_, headers, _) = wms_request(app, WMS_GETMAP, vec![("if-none-match", "*")]).await;
        assert!(headers.get(header::ETAG).is_some(), "304 must include ETag");
        assert!(
            headers.get(header::CACHE_CONTROL).is_some(),
            "304 must include Cache-Control"
        );
    }
}

// ---------------------------------------------------------------------------
// Load shedding (503 Service Unavailable)
// ---------------------------------------------------------------------------

mod load_shedding {
    use super::*;

    #[tokio::test]
    async fn returns_503_when_semaphore_exhausted() {
        // Build router with 0 permits — every render request will timeout
        let engine: Arc<dyn MapEngine> = Arc::new(MockMapEngine);
        let mut engines = HashMap::new();
        let mut collections = HashMap::new();
        let mut styles = HashMap::new();

        engines.insert("radar".to_string(), engine);
        collections.insert("radar".to_string(), make_collection("radar"));
        styles.insert("radar".to_string(), make_styles());

        let state = Arc::new(ArcSwap::from_pointee(WmsState {
            engines,
            collections,
            styles,
            point_engines: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(0)), // 0 permits!
            rendered_cache: Arc::new(RenderedCache::new(16)),
            tile_cache: Arc::new(ds_render::TilePixelCache::new(16)),
            base_url: String::new(),
            trust_proxy_headers: false,
        }));
        let app = api_wms::router(state);

        // This should timeout and return 503
        // Use a short timeout by temporarily checking the behavior
        let (status, _, body) = wms_request(app, WMS_GETMAP, vec![]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        // WMS returns XML error
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains("ServiceException"),
            "503 should return WMS ServiceExceptionReport XML"
        );
    }
}
