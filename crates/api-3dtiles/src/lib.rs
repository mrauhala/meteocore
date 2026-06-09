//! OGC 3D Tiles HTTP API (#349).
//!
//! Serves volumetric weather data — radar polar volumes today — as OGC 3D
//! Tiles, from any collection implementing [`ds_core::volume::VolumeEngine`].
//! Like the other API crates, it depends only on `ds-core` + the encoder
//! (`ds-3dtiles`) and `ds-render` (for the colour ramp), never an engine crate;
//! the engine registry is keyed by collection id and swapped via `ArcSwap`.
//!
//! Routes (mounted under `/3dtiles` by the server):
//! - `GET /collections/{id}/tileset.json` (`?representation=points|isosurface`)
//! - `GET /collections/{id}/content.pnts` — point-cloud content
//! - `GET /collections/{id}/content.glb` — isosurface-mesh content
//! - `GET /` · `/collections` · `/collections/{id}` · `/viewer`

pub mod error;
pub mod handlers;

pub use error::Tiles3dError;
pub use handlers::{default_point_colormap, AppState, TilesState3d};

use axum::routing::get;
use axum::Router;

/// Build the 3D Tiles API router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::landing_page))
        .route("/viewer", get(handlers::get_viewer))
        .route("/collections", get(handlers::collections))
        .route("/collections/{id}", get(handlers::collection))
        .route("/collections/{id}/tileset.json", get(handlers::get_tileset))
        .route("/collections/{id}/content.pnts", get(handlers::get_content))
        .route(
            "/collections/{id}/content.glb",
            get(handlers::get_content_glb),
        )
        .with_state(state)
}
