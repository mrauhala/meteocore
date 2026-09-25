//! OGC API - Maps as a building block of the shared OGC API root (#789).
//!
//! Serves the same engines and styles as the per-API `/maps` service, from
//! the same state, with links built from the shared root.

use api_common::shared::{BuildingBlock, Contribution, OpenApiFragment};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use serde_json::Map;

use crate::handlers::{self, AppState, MapsState};

/// The Maps building block over the per-API service's state.
pub struct MapsBlock {
    state: AppState,
}

impl MapsBlock {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

fn contribution(
    state: &MapsState,
    config: &ds_core::config::CollectionConfig,
    root: &str,
) -> Option<Contribution> {
    let engine = state.engines.get(&config.id)?;
    let info = engine.raster_info_shared();
    let (fields, links) =
        handlers::collection_parts(config, &info, state.styles.get(&config.id), root);
    Some(Contribution {
        config: config.clone(),
        fields,
        links,
        bbox: info.spatial_extent,
        time: info.times.first().copied().zip(info.times.last().copied()),
        // Maps omits a native CRS with no OGC URI rather than mislabel it.
        claims: &["storageCrs"],
    })
}

impl BuildingBlock for MapsBlock {
    fn kind(&self) -> &'static str {
        "maps"
    }

    fn conformance(&self) -> &'static [&'static str] {
        handlers::CONFORMANCE
    }

    fn base_url(&self, headers: &HeaderMap) -> String {
        handlers::request_base_url(&self.state.load(), headers)
    }

    fn collections(&self, root: &str) -> Vec<Contribution> {
        let state = self.state.load();
        state
            .collections
            .values()
            .filter_map(|config| contribution(&state, config, root))
            .collect()
    }

    fn collection(&self, id: &str, root: &str) -> Option<Contribution> {
        let state = self.state.load();
        contribution(&state, state.collections.get(id)?, root)
    }

    fn openapi(&self, mount: &str) -> OpenApiFragment {
        let state = self.state.load();
        let components = match handlers::openapi_components() {
            serde_json::Value::Object(components) => components,
            _ => Map::new(),
        };
        OpenApiFragment {
            paths: handlers::collection_openapi_paths(&state, mount),
            components,
        }
    }

    fn routes(&self) -> Router {
        Router::new()
            .route("/collections/{id}/map", get(handlers::get_map))
            .route("/collections/{id}/styles", get(handlers::styles))
            .route(
                "/collections/{id}/styles/{styleId}/map",
                get(handlers::get_styled_map),
            )
            .route(
                "/collections/{id}/styles/{styleId}/legend",
                get(handlers::style_legend),
            )
            .with_state(self.state.clone())
    }
}
