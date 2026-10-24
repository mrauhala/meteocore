pub mod caching;
pub mod crs;
pub mod handlers;
mod html;
mod map_preview;
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
    let router = Router::new()
        .route("/", get(handlers::landing_page))
        .route(
            "/api",
            api_common::errors::format_only_get(handlers::api_definition),
        )
        .route("/api/docs", get(handlers::api_docs))
        .route("/api/docs/{asset}", get(handlers::api_docs_asset))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/items", get(handlers::items))
        .route("/collections/{id}/items/{feature_id}", get(handlers::item))
        // Cache-Control + ETag/If-None-Match on every 200 (#499).
        .layer(axum::middleware::from_fn(caching::conditional_get))
        .with_state(state.clone());
    // A request that asked for HTML gets its error as a page (#1132).
    let catalog = state.clone();
    let pages = api_common::errors::pages(
        move |headers| handlers::request_base_url(&state.load(), headers),
        move |id| catalog.load().engines.contains_key(id),
    );
    api_common::errors::html_errors(router, "features", mount, pages)
        .layer(Extension(api_common::Mount(mount)))
}
