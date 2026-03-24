pub mod handlers;
pub mod params;
pub mod response;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;

use ds_core::engine::Engine;

pub fn router(engine: Arc<dyn Engine>) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/locations", get(handlers::locations))
        .route(
            "/collections/{id}/locations/{loc_id}",
            get(handlers::location_query),
        )
        .layer(CorsLayer::permissive())
        .with_state(engine)
}
