pub mod caching;
pub(crate) mod csv;
mod datetime_list;
mod error_pages;
mod executor;
pub(crate) mod geojson;
pub mod handlers;
mod html;
pub mod items;
pub mod params;
pub(crate) mod plot_convert;
mod query_form;
pub mod response;
mod response_budget;

use axum::routing::get;
use axum::Router;

use handlers::AppState;

/// The EDR response memory budget's (reserved bytes, capacity bytes,
/// rejected responses), for `/metrics` (#1041).
pub use response_budget::metrics as response_budget_metrics;

pub fn router(state: AppState) -> Router {
    // Reads the budget's environment at startup, where a retired variable
    // is reported.
    response_budget::init();
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
        .route("/collections/{id}/locations", get(handlers::locations))
        .route(
            "/collections/{id}/locations/{loc_id}",
            get(handlers::location_query),
        )
        .route("/collections/{id}/position", get(handlers::position_query))
        .route("/collections/{id}/area", get(handlers::area_query))
        .route("/collections/{id}/radius", get(handlers::radius_query))
        .route("/collections/{id}/cube", get(handlers::cube_query))
        .route(
            "/collections/{id}/trajectory",
            get(handlers::trajectory_query),
        )
        // Items (#928): the collection's FeatureEngine, where it has one.
        .route("/collections/{id}/items", get(items::items))
        .route("/collections/{id}/items/{item_id}", get(items::item))
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
        .route(
            "/collections/{id}/instances/{instanceId}/radius",
            get(handlers::instance_radius_query),
        )
        .route(
            "/collections/{id}/instances/{instanceId}/cube",
            get(handlers::instance_cube_query),
        )
        // A data query asked for as HTML without its coordinates is
        // answered with its form (#1138), cached like any page.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            query_form::bare_query_form,
        ))
        // Cache-Control + ETag/If-None-Match on every 200 (#499).
        .layer(axum::middleware::from_fn(caching::conditional_get))
        // What an HTML error page adds for a data query (#1132).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            error_pages::error_detail,
        ))
        .with_state(state.clone());
    // A request that asked for HTML gets its error as a page (#1132).
    error_pages::wrap(router, state, api_common::mounts::EDR)
}
