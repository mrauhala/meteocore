pub mod error;
pub mod handlers;
pub mod params;
pub mod shared;
pub mod tilematrixset;

use axum::routing::get;
use axum::{Extension, Router};

pub use handlers::{AppState, TilesState};
pub use shared::TilesBlock;

/// The Tiles service router, mounted at [`api_common::mounts::TILES`].
pub fn router(state: AppState) -> Router {
    router_at(state, api_common::mounts::TILES)
}

/// The Tiles router for a `mount` path below the external base URL. Every
/// link the handlers emit is built from base URL + `mount`.
pub fn router_at(state: AppState, mount: &'static str) -> Router {
    let router = Router::new()
        .route("/", get(handlers::landing_page))
        .route("/api", get(handlers::api_definition))
        .route("/api/docs", get(handlers::api_docs))
        .route("/api/docs/{asset}", get(handlers::api_docs_asset))
        .route("/conformance", get(handlers::conformance))
        .route(
            "/tileMatrixSets",
            api_common::errors::json_only_get(handlers::tile_matrix_sets, "tile matrix sets"),
        )
        .route(
            "/tileMatrixSets/{tileMatrixSetId}",
            api_common::errors::json_only_get(handlers::tile_matrix_set, "a tile matrix set"),
        )
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route(
            "/collections/{id}/tiles",
            api_common::errors::json_only_get(handlers::collection_tilesets, "a tileset list"),
        )
        .route(
            "/collections/{id}/tiles/{tileMatrixSetId}",
            api_common::errors::json_only_get(handlers::collection_tileset, "a tileset"),
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
        .with_state(state.clone());
    // A request that asked for HTML gets its error as a page (#1132).
    let catalog = state.clone();
    let pages = api_common::errors::pages(
        move |headers| handlers::request_base_url(&state.load(), headers),
        move |id| handlers::tile_sources(&catalog.load(), id).is_ok(),
    );
    api_common::errors::html_errors(router, "tiles", mount, pages)
        .layer(Extension(api_common::Mount(mount)))
}
