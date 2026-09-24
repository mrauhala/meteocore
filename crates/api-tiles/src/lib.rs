pub mod error;
pub mod handlers;
pub mod params;
pub mod tilematrixset;

use axum::routing::get;
use axum::{Extension, Router};

pub use handlers::{AppState, TilesState};

/// The Tiles service router, mounted at [`api_common::mounts::TILES`].
pub fn router(state: AppState) -> Router {
    router_at(state, api_common::mounts::TILES)
}

/// The Tiles router for a `mount` path below the external base URL. Every
/// link the handlers emit is built from base URL + `mount`.
pub fn router_at(state: AppState, mount: &'static str) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/api/docs/{asset}", get(handlers::api_docs_asset))
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
            "/collections/{id}/tiles/{tileMatrixSetId}",
            get(handlers::collection_tileset),
        )
        .route(
            "/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
            get(handlers::get_tile),
        )
        .route(
            "/collections/{id}/styles/{styleId}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
            get(handlers::get_styled_tile),
        )
        .route(
            "/collections/{id}/styles/{styleId}/legend",
            get(handlers::style_legend),
        )
        .with_state(state)
        .layer(Extension(api_common::Mount(mount)))
}
