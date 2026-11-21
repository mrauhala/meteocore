//! Response-caching middleware: `Cache-Control` + ETag / `If-None-Match`
//! conditional GET for every 200 a router serves (#499).
//!
//! The pure pieces (hashing, matching, policy strings) live in
//! [`ds_core::http_cache`]; this module is the axum glue shared by EDR,
//! Features and the shared OGC API root's metadata routes (#789).
//!
//! The ETag is weak, `W/"<16 hex>"` ([`etag`]): the server's
//! `CompressionLayer` sits outside these routers and gzips the body after it
//! was tagged, so one tag names the plain and the gzipped bytes (#1500).
//! `If-None-Match` is compared weakly, as RFC 9110 §13.1.2 requires.
//!
//! A handler may precompute its ETag with [`etag`], and the middleware then
//! honours it without touching the body: Features `items` hashes its body
//! with the per-request `timeStamp` excluded, since a body hash would never
//! match, and EDR hashes every data query's body on its query executor,
//! inside the query's admission, rather than on the request runtime here
//! (#1500). A 304 still runs the handler — what it saves is the transfer,
//! and `Cache-Control` saves the request entirely within the freshness
//! window.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
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

/// Middleware wrapping every route: default `Cache-Control` to the short
/// policy when the handler didn't set one, attach a weak content-derived
/// ETag unless the handler set one (buffering the already in-memory body
/// only to hash it), and short-circuit a matching `If-None-Match` to 304.
pub async fn conditional_get(req: Request, next: Next) -> Response {
    // Owned copy so it survives the request being consumed by `next`.
    let if_none_match = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
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
    let matches = if_none_match
        .as_deref()
        .zip(etag.to_str().ok())
        .is_some_and(|(inm, etag)| if_none_match_matches(inm, etag));
    if matches {
        // Keep the 200's headers (RFC 9110 §15.4.5 wants Cache-Control/ETag/
        // Vary repeated) but drop the payload-specific ones along with the
        // body.
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(header::CONTENT_TYPE);
        parts.headers.remove(header::CONTENT_LENGTH);
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
