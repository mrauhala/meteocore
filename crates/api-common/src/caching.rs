//! Response-caching middleware: `Cache-Control` + ETag / `If-None-Match`
//! conditional GET for every 200 a router serves (#499).
//!
//! The pure pieces (hashing, matching, policy strings) live in
//! [`ds_core::http_cache`]; this module is the axum glue shared by EDR,
//! Features and the shared OGC API root's metadata routes (#789).
//!
//! A handler may precompute its ETag (Features `items` hashes its body with
//! the per-request `timeStamp` excluded, since a body hash would never match);
//! the middleware honours a handler-set ETag instead of hashing. A 304 still
//! runs the handler — what it saves is the transfer, and `Cache-Control`
//! saves the request entirely within the freshness window.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use ds_core::http_cache::{etag_of, if_none_match_matches, CACHE_CONTROL_SHORT};

/// Middleware wrapping every route: buffer the (already in-memory) body of a
/// 200, default `Cache-Control` to the short policy when the handler didn't
/// set one, attach a strong content-derived ETag (honouring one the handler
/// precomputed), and short-circuit a matching `If-None-Match` to 304.
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
    // Every 200 body behind this layer is a fully buffered String/Vec (serde_json
    // output or an encoded PNG), so this await is a cheap move, not a streaming
    // hazard.
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("failed to buffer response body for ETag: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !parts.headers.contains_key(header::CACHE_CONTROL) {
        parts.headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL_SHORT),
        );
    }
    let etag = match parts
        .headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
    {
        Some(precomputed) => precomputed.to_owned(),
        None => {
            let etag = etag_of(&bytes);
            parts.headers.insert(
                header::ETAG,
                HeaderValue::from_str(&etag).expect("quoted-hex etag is a valid header value"),
            );
            etag
        }
    };
    if if_none_match
        .as_deref()
        .is_some_and(|inm| if_none_match_matches(inm, &etag))
    {
        // Keep the 200's headers (RFC 7232 §4.1 wants Cache-Control/ETag/Vary
        // repeated) but drop the payload-specific ones along with the body.
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(header::CONTENT_TYPE);
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::empty());
    }
    Response::from_parts(parts, Body::from(bytes))
}
