pub mod error;
pub mod handlers;
pub mod params;

use axum::routing::get;
use axum::Router;

pub use handlers::{AppState, MapsState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/map", get(handlers::get_map))
        .route("/collections/{id}/styles", get(handlers::styles))
        .route(
            "/collections/{id}/styles/{styleId}/map",
            get(handlers::get_styled_map),
        )
        .with_state(state)
}
