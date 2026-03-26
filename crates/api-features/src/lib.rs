pub mod handlers;
pub mod params;
pub mod response;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use handlers::FeaturesState;

pub fn router(state: Arc<FeaturesState>) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/conformance", get(handlers::conformance))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/items", get(handlers::items))
        .route("/collections/{id}/items/{feature_id}", get(handlers::item))
        .with_state(state)
}
