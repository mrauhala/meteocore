pub mod error;
pub mod handlers;
pub mod params;
pub mod shared;

use axum::routing::get;
use axum::{Extension, Router};

pub use handlers::{AppState, MapsState};
pub use shared::MapsBlock;

/// The Maps service router, mounted at [`api_common::mounts::MAPS`].
pub fn router(state: AppState) -> Router {
    router_at(state, api_common::mounts::MAPS)
}

/// The Maps router for a `mount` path below the external base URL. Every link
/// the handlers emit is built from base URL + `mount`.
pub fn router_at(state: AppState, mount: &'static str) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/api/docs/{asset}", get(handlers::api_docs_asset))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/map", get(handlers::get_map))
        .route("/collections/{id}/styles", get(handlers::styles))
        .route(
            "/collections/{id}/styles/{styleId}/map",
            get(handlers::get_styled_map),
        )
        .route(
            "/collections/{id}/styles/{styleId}/legend",
            get(handlers::style_legend),
        )
        .with_state(state)
        .layer(Extension(api_common::Mount(mount)))
}
