//! Response-caching middleware: `Cache-Control` + ETag / `If-None-Match`
//! conditional GET for every 200 a router serves (#499).
//!
//! The pure pieces (hashing, matching, policy strings) live in
//! [`ds_core::http_cache`]; this module is the axum glue shared by EDR,
//! Features and the shared OGC API root's metadata routes (#789), and the
//! helpers every other API's 304 uses.
//!
//! The ETag is weak, `W/"<16 hex>"` ([`etag`]): the server's
//! `CompressionLayer` sits outside these routers and gzips the body after it
//! was tagged, so one tag names the plain and the gzipped bytes (#1500).
//! `If-None-Match` is compared weakly, as RFC 9110 §13.1.2 requires, over
//! every line of it ([`if_none_match`]).
//!
//! A 304 carries the headers its 200 would (RFC 9110 §15.4.5): the
//! handler's `Vary`, and the `Vary: accept-encoding` the compression layer
//! adds only to a body it would compress ([`compressed_by_server`]), never
//! to an empty 304. Nor a `Content-Length` (§8.6), which a gzipped 200
//! does not state: axum puts `content-length: 0` in an empty body's
//! headers, but hyper sends no length without a body to send (the
//! server's `middleware` tests check the wire).
//!
//! A handler may precompute its ETag with [`etag`], and the middleware then
//! honours it without touching the body: Features `items` hashes its body
//! with the per-request `timeStamp` excluded, since a body hash would never
//! match, and EDR hashes every data query's body on its query executor,
//! inside the query's admission, rather than on the request runtime here
//! (#1500). A 304 still runs the handler — what it saves is the transfer,
//! and `Cache-Control` saves the request entirely within the freshness
//! window.

use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use ds_core::http_cache::{if_none_match_matches, weak_etag_of, CACHE_CONTROL_SHORT};

/// The `ETag` header value of a 200 body behind [`conditional_get`]:
/// [`weak_etag_of`] its bytes. A handler that hashes its own body (off the
/// request runtime, or with a volatile field blanked) uses this, so every
/// tag these routers send has the one form.
pub fn etag(bytes: &[u8]) -> HeaderValue {
    HeaderValue::try_from(weak_etag_of(bytes)).expect("a weak quoted-hex ETag is a header value")
}

/// Whether the request's `If-None-Match` matches `etag`, weakly. A client
/// or proxy may send the field on several lines, which RFC 9110 §5.3 makes
/// one list: every line is read, never only the first.
pub fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| if_none_match_matches(value, etag))
}

/// Every line of the request's `If-None-Match` joined into the one list
/// they make (RFC 9110 §5.3), for a handler that compares it after it has
/// built its response; `None` without the field. A line that is not text
/// is skipped.
pub fn if_none_match_list(headers: &HeaderMap) -> Option<String> {
    let lines: Vec<&str> = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    (!lines.is_empty()).then(|| lines.join(", "))
}

/// Whether the server's `CompressionLayer` compresses a 200 typed
/// `content_type` (`None`: untyped) of `len` bytes (`None`: unknown), and
/// so adds `Vary: accept-encoding` to it, whatever the request accepts.
/// tower-http 0.7's default predicate, which main.rs uses: not a response
/// already encoded or a range (the caller checks those headers), not under
/// 32 bytes, not gRPC (but gRPC-Web), not an image (but SVG), not an event
/// stream. The server's `middleware` tests pin it against the layer.
pub fn compressed_by_server(content_type: Option<&str>, len: Option<u64>) -> bool {
    let content_type = content_type.unwrap_or_default();
    let excluded = |prefix: &str, except: Option<&str>| {
        content_type.starts_with(prefix) && except.is_none_or(|e| !content_type.starts_with(e))
    };
    len.is_none_or(|len| len >= 32)
        && !excluded("application/grpc", Some("application/grpc-web"))
        && !excluded("image/", Some("image/svg+xml"))
        && !excluded("text/event-stream", None)
}

/// Add `Vary: accept-encoding` to a 304 whose 200 the server's compression
/// layer compresses ([`compressed_by_server`]): the layer adds it to that
/// 200 but never to the empty 304, and RFC 9110 §15.4.5 wants the 304 to
/// repeat it. A `Vary` naming it already is kept as it is.
pub fn vary_as_compressed(headers: &mut HeaderMap) {
    let named = headers.get_all(header::VARY).iter().any(|value| {
        value
            .to_str()
            .is_ok_and(|v| v.to_ascii_lowercase().contains("accept-encoding"))
    });
    if !named {
        headers.append(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
}

/// Middleware wrapping every route: default `Cache-Control` to the short
/// policy when the handler didn't set one, attach a weak content-derived
/// ETag unless the handler set one (buffering the already in-memory body
/// only to hash it), and short-circuit a matching `If-None-Match` to 304.
pub async fn conditional_get(req: Request, next: Next) -> Response {
    // Owned copies of every line, so they survive the request being
    // consumed by `next`.
    let mut if_none_match_lines = HeaderMap::new();
    for value in req.headers().get_all(header::IF_NONE_MATCH) {
        if_none_match_lines.append(header::IF_NONE_MATCH, value.clone());
    }
    let resp = next.run(req).await;
    if resp.status() != StatusCode::OK {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    if !parts.headers.contains_key(header::CACHE_CONTROL) {
        parts.headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL_SHORT),
        );
    }
    let (etag, body) = match parts.headers.get(header::ETAG) {
        // The handler hashed its body: it passes through as it is.
        Some(precomputed) => (precomputed.clone(), body),
        None => {
            // Every 200 body behind this layer is a fully buffered
            // String/Vec (serde_json output or an encoded PNG), so this
            // await is a cheap move, not a streaming hazard.
            let bytes = match axum::body::to_bytes(body, usize::MAX).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::error!("failed to buffer response body for ETag: {e}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            let etag = etag(&bytes);
            parts.headers.insert(header::ETAG, etag.clone());
            (etag, Body::from(bytes))
        }
    };
    let matches = etag
        .to_str()
        .is_ok_and(|etag| if_none_match(&if_none_match_lines, etag));
    if matches {
        // The layer's `Vary`, from the 200 as the layer would see it.
        let compressed = !parts.headers.contains_key(header::CONTENT_ENCODING)
            && !parts.headers.contains_key(header::CONTENT_RANGE)
            && compressed_by_server(
                parts
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                body.size_hint().exact(),
            );
        // Keep the 200's headers (RFC 9110 §15.4.5 wants Cache-Control/ETag/
        // Vary repeated) but drop the payload-specific ones along with the
        // body.
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(header::CONTENT_TYPE);
        parts.headers.remove(header::CONTENT_LENGTH);
        if compressed {
            vary_as_compressed(&mut parts.headers);
        }
        return Response::from_parts(parts, Body::empty());
    }
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use super::*;

    /// A router of one 200 behind the middleware, its handler setting
    /// `preset` as its ETag when given.
    fn app(preset: Option<&'static str>) -> Router {
        Router::new()
            .route(
                "/",
                get(move || async move {
                    match preset {
                        Some(tag) => ([(header::ETAG, tag)], "body").into_response(),
                        None => "body".into_response(),
                    }
                }),
            )
            .layer(axum::middleware::from_fn(conditional_get))
    }

    async fn fetch(app: Router, if_none_match: Option<&str>) -> Response {
        let mut request = Request::builder().uri("/");
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: Response) -> axum::body::Bytes {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
    }

    /// The middleware's own tag is weak, over the body (#1500).
    #[tokio::test]
    async fn the_tag_is_weak_over_the_body() {
        let response = fetch(app(None), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], weak_etag_of(b"body"));
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            CACHE_CONTROL_SHORT
        );
        assert_eq!(body(response).await, "body");
    }

    /// `If-None-Match` compares weakly (RFC 9110 §13.1.2): the weak tag, the
    /// strong tag of the same bytes and a list holding either are a 304.
    #[tokio::test]
    async fn if_none_match_compares_weakly() {
        let weak = weak_etag_of(b"body");
        let strong = ds_core::http_cache::etag_of(b"body");
        for sent in [weak.clone(), strong, format!("\"other\", {weak}")] {
            let response = fetch(app(None), Some(&sent)).await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{sent}");
            assert_eq!(response.headers()[header::ETAG], weak.as_str());
            assert!(response.headers().get(header::CONTENT_TYPE).is_none());
            assert!(body(response).await.is_empty());
        }
        let response = fetch(app(None), Some(&weak_etag_of(b"other"))).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// `If-None-Match` sent on several lines is one list (RFC 9110 §5.3):
    /// a tag on any line matches.
    #[tokio::test]
    async fn every_if_none_match_line_is_read() {
        let weak = weak_etag_of(b"body");
        let request = Request::builder()
            .uri("/")
            .header(header::IF_NONE_MATCH, "\"other\"")
            .header(header::IF_NONE_MATCH, weak.as_str())
            .body(Body::empty())
            .unwrap();
        let response = app(None).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

        let mut headers = HeaderMap::new();
        headers.append(header::IF_NONE_MATCH, HeaderValue::from_static("\"a\""));
        assert!(!if_none_match(&headers, &weak));
        headers.append(header::IF_NONE_MATCH, HeaderValue::from_str(&weak).unwrap());
        assert!(if_none_match(&headers, &weak));
        assert!(!if_none_match(&HeaderMap::new(), &weak));
        let list = if_none_match_list(&headers).unwrap();
        assert_eq!(list, format!("\"a\", {weak}"));
        assert!(if_none_match_matches(&list, &weak));
        assert_eq!(if_none_match_list(&HeaderMap::new()), None);
    }

    /// The 304 repeats the `Vary: accept-encoding` the compression layer
    /// gives its 200 (RFC 9110 §15.4.5): for a body the layer compresses,
    /// never for an image or a body under 32 bytes. The handler's own
    /// `Vary` stays, and no `Content-Type` or `Content-Length` is left.
    #[tokio::test]
    async fn a_304_varies_as_its_compressed_200() {
        let json = "{\"conformsTo\": [\"http://www.opengis.net/spec\"]}";
        let app = |content_type: &'static str, body: &'static str| {
            Router::new()
                .route(
                    "/",
                    get(move || async move {
                        (
                            [
                                (header::CONTENT_TYPE, content_type),
                                (header::VARY, "accept"),
                            ],
                            body,
                        )
                    }),
                )
                .layer(axum::middleware::from_fn(conditional_get))
        };
        for (content_type, body, varies) in [
            ("application/json", json, true),
            ("text/html; charset=utf-8", json, true),
            ("image/svg+xml", json, true),
            ("image/png", json, false),
            ("application/json", "{}", false),
        ] {
            let sent = weak_etag_of(body.as_bytes());
            let request = Request::builder()
                .uri("/")
                .header(header::IF_NONE_MATCH, sent.as_str())
                .body(Body::empty())
                .unwrap();
            let response = app(content_type, body).oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_MODIFIED,
                "{content_type}"
            );
            let vary: Vec<_> = response.headers().get_all(header::VARY).iter().collect();
            let expected: &[&str] = if varies {
                &["accept", "accept-encoding"]
            } else {
                &["accept"]
            };
            assert_eq!(vary, expected, "{content_type} {body}");
            assert!(response.headers().get(header::CONTENT_TYPE).is_none());
        }
    }

    /// tower-http 0.7's default predicate, as `compressed_by_server` mirrors
    /// it; the server's `middleware` tests check the layer itself.
    #[test]
    fn the_server_compresses_text_above_32_bytes_but_no_image() {
        assert!(compressed_by_server(Some("application/json"), Some(32)));
        assert!(!compressed_by_server(Some("application/json"), Some(31)));
        assert!(compressed_by_server(Some("application/json"), None));
        assert!(compressed_by_server(None, Some(100)));
        assert!(!compressed_by_server(Some("image/png"), Some(100)));
        assert!(compressed_by_server(Some("image/svg+xml"), Some(100)));
        assert!(!compressed_by_server(Some("application/grpc"), Some(100)));
        assert!(compressed_by_server(
            Some("application/grpc-web"),
            Some(100)
        ));
        assert!(!compressed_by_server(Some("text/event-stream"), Some(100)));
        assert!(compressed_by_server(
            Some("application/vnd.mapbox-vector-tile"),
            Some(100)
        ));
    }

    /// A tag the handler set is kept, never replaced by a hash of the body,
    /// and answers `If-None-Match` in either form with a 304 (#1500).
    #[tokio::test]
    async fn a_preset_tag_is_honoured() {
        let preset = "W/\"0123456789abcdef\"";
        let response = fetch(app(Some(preset)), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], preset);
        assert_eq!(body(response).await, "body");
        for sent in [preset, "\"0123456789abcdef\""] {
            let response = fetch(app(Some(preset)), Some(sent)).await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{sent}");
            assert_eq!(response.headers()[header::ETAG], preset);
            assert!(body(response).await.is_empty());
        }
        // The body's own hash is not the tag.
        let response = fetch(app(Some(preset)), Some(&weak_etag_of(b"body"))).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
