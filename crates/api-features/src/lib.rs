pub mod caching;
pub mod handlers;
mod html;
pub mod params;
pub mod response;
pub mod shared;

use axum::routing::get;
use axum::{Extension, Router};

pub use handlers::{AppState, FeaturesState};
pub use shared::FeaturesBlock;

/// The Features service router, mounted at [`api_common::mounts::FEATURES`].
pub fn router(state: AppState) -> Router {
    router_at(state, api_common::mounts::FEATURES)
}

/// The Features router for a `mount` path below the external base URL. Every
/// link the handlers emit is built from base URL + `mount`.
pub fn router_at(state: AppState, mount: &'static str) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/api/docs/{asset}", get(handlers::api_docs_asset))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/items", get(handlers::items))
        .route("/collections/{id}/items/{feature_id}", get(handlers::item))
        // Cache-Control + ETag/If-None-Match on every 200 (#499).
        .layer(axum::middleware::from_fn(caching::conditional_get))
        .with_state(state)
        .layer(Extension(api_common::Mount(mount)))
}
