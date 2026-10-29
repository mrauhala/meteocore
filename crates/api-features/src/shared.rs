//! OGC API - Features as a building block of the shared OGC API root (#789).
//!
//! Serves `/collections/{id}/items[/{featureId}]` from the per-API `/features`
//! service's state. The shared root lists its blocks as Maps, Tiles, Features,
//! so where several describe one collection (CAP, nowcast, lightning events,
//! vector-tiled GeoJSON) the earlier block's `extent`, `crs` and `dataType`
//! win, and a raster block's claim on `storageCrs` stands. Features adds
//! `itemType: "feature"`, `numberItems` and one `items` link per encoding.

use api_common::caching;
use api_common::shared::{BuildingBlock, Contribution, OpenApiFragment};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use ds_core::config::CollectionConfig;
use serde_json::{Map, Value};

use crate::handlers::{self, AppState, FeaturesState, Layout};

/// The Features building block over the per-API service's state.
pub struct FeaturesBlock {
    state: AppState,
}

impl FeaturesBlock {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

/// Component names at the shared root: Maps already defines a different
/// `bbox`, `datetime` and `link`.
fn shared_name(name: &str) -> String {
    format!("features-{name}")
}

/// `overview`: whether to say what the items are, which only the
/// collection's own HTML page shows (#1143); the catalog skips it.
fn contribution(
    state: &FeaturesState,
    config: &CollectionConfig,
    root: &str,
    overview: bool,
) -> Option<Contribution> {
    let engine = state.engines.get(&config.id)?;
    let (fields, links) = handlers::collection_parts(engine.as_ref(), config, root, Layout::Shared);
    Some(Contribution {
        config: config.clone(),
        fields,
        links,
        bbox: engine.spatial_extent(),
        time: engine.temporal_extent(),
        claims: &[],
        items: overview.then(|| handlers::items_overview(engine.as_ref())),
    })
}

impl BuildingBlock for FeaturesBlock {
    fn kind(&self) -> &'static str {
        "features"
    }

    fn conformance(&self) -> &'static [&'static str] {
        handlers::CONFORMANCE
    }

    fn base_url(&self, headers: &HeaderMap) -> String {
        handlers::request_base_url(&self.state.load(), headers)
    }

    fn discovery(&self) -> ds_core::discovery::Discovery {
        self.state.load().discovery.clone()
    }

    fn collections(&self, root: &str) -> Vec<Contribution> {
        let state = self.state.load();
        state
            .collections
            .values()
            .filter_map(|config| contribution(&state, config, root, false))
            .collect()
    }

    fn collection(&self, id: &str, root: &str) -> Option<Contribution> {
        let state = self.state.load();
        contribution(&state, state.collections.get(id)?, root, true)
    }

    fn serves(&self, id: &str) -> bool {
        self.state.load().collections.contains_key(id)
    }

    fn openapi(&self, mount: &str) -> OpenApiFragment {
        let state = self.state.load();
        let components = match handlers::openapi_components(shared_name) {
            Value::Object(components) => components,
            _ => Map::new(),
        };
        OpenApiFragment {
            paths: handlers::items_openapi_paths(&state, mount, shared_name),
            components,
        }
    }

    fn routes(&self) -> Router {
        Router::new()
            .route("/collections/{id}/items", get(handlers::items))
            .route("/collections/{id}/items/{feature_id}", get(handlers::item))
            // `/items` sets its own ETag with `timeStamp` blanked; the
            // middleware answers If-None-Match with 304 (#499).
            .layer(axum::middleware::from_fn(caching::conditional_get))
            .with_state(self.state.clone())
    }
}
