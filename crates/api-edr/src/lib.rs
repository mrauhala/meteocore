pub mod handlers;
pub mod params;
pub mod plot_convert;
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
        .route("/collections/{id}/locations", get(handlers::locations))
        .route(
            "/collections/{id}/locations/{loc_id}",
            get(handlers::location_query),
        )
        .route("/collections/{id}/position", get(handlers::position_query))
        .route("/collections/{id}/area", get(handlers::area_query))
        .with_state(state)
}
