pub mod handlers;
pub mod params;
pub(crate) mod plot_convert;
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
        .route(
            "/collections/{id}/trajectory",
            get(handlers::trajectory_query),
        )
        // OGC API - EDR instances (forecast model runs; #337). The `{instanceId}`
        // segment name matches the OpenAPI `api_definition()` path parameter.
        .route("/collections/{id}/instances", get(handlers::instances))
        .route(
            "/collections/{id}/instances/{instanceId}",
            get(handlers::instance),
        )
        .route(
            "/collections/{id}/instances/{instanceId}/position",
            get(handlers::instance_position_query),
        )
        .route(
            "/collections/{id}/instances/{instanceId}/area",
            get(handlers::instance_area_query),
        )
        .with_state(state)
}
