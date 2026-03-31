pub mod error;
pub mod handlers;
pub mod params;
pub mod tilematrixset;

use axum::routing::get;
use axum::Router;

pub use handlers::{AppState, TilesState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/conformance", get(handlers::conformance))
        .route("/tileMatrixSets", get(handlers::tile_matrix_sets))
        .route(
            "/tileMatrixSets/{tileMatrixSetId}",
            get(handlers::tile_matrix_set),
        )
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route(
            "/collections/{id}/tiles",
            get(handlers::collection_tilesets),
        )
        .route(
            "/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
            get(handlers::get_tile),
        )
        .route(
            "/collections/{id}/styles/{styleId}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
            get(handlers::get_styled_tile),
        )
        .with_state(state)
}
