pub mod handlers;
pub mod params;
pub mod response;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use handlers::EdrState;

pub fn router(state: Arc<EdrState>) -> Router {
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
        .route(
            "/collections/{id}/position",
            get(handlers::position_query),
        )
        .with_state(state)
}
