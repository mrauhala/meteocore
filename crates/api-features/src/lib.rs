pub mod caching;
pub mod handlers;
pub mod params;
pub mod response;

use axum::routing::get;
use axum::Router;

use handlers::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/items", get(handlers::items))
        .route("/collections/{id}/items/{feature_id}", get(handlers::item))
        // Cache-Control + ETag/If-None-Match on every 200 (#499).
        .layer(axum::middleware::from_fn(caching::conditional_get))
        .with_state(state)
}
