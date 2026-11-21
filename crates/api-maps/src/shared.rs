//! OGC API - Maps as a building block of the shared OGC API root (#789).
//!
//! Serves the same engines and styles as the per-API `/maps` service, from
//! the same state, with links built from the shared root.

use std::collections::HashMap;
use std::sync::Arc;

use api_common::shared::{BuildingBlock, Contribution, OpenApiFragment};
use api_common::JsonQuery;
use arc_swap::ArcSwap;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use ds_core::map_engine::MapEngine;
use ds_render::StyleInfo;
use serde_json::Map;

use crate::error::MapsError;
use crate::handlers::{self, AppState, MapsState};
use crate::params::{self, LegendQueryParams};

/// Style layers keyed as the server builds them: a collection's own under
/// its id, each parameter's under `{id}/{parameter}`.
type StyleLayers = HashMap<String, HashMap<String, StyleInfo>>;

/// The Maps building block over the per-API service's state.
pub struct MapsBlock {
    state: AppState,
    fallback: Option<StyleFallback>,
}

impl MapsBlock {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            fallback: None,
        }
    }

    /// Answer the shared root's legend of a raster collection Maps does
    /// not serve from `fallback`, the registry of the block that draws it
    /// (#792). The route is Maps' (one route per path across blocks), and
    /// Tiles links every style of a collection it draws to it.
    pub fn with_style_fallback(mut self, fallback: StyleFallback) -> Self {
        self.fallback = Some(fallback);
        self
    }
}

/// Another block's registry, which the shared root's legend reads a raster
/// collection from when Maps does not serve it (#792): Tiles', for a
/// collection whose `apis` list `tiles` and not `maps`. Its map tiles and
/// styled map tilesets are at the root, and its styles are the ones Maps
/// would offer (`collection_layer_styles` builds both), so its legends are
/// too, resolved as Maps resolves them.
#[derive(Clone)]
pub struct StyleFallback(Arc<dyn Fn() -> Arc<dyn StyleRegistry> + Send + Sync>);

impl StyleFallback {
    /// Over the current snapshot of `state` at each request: its raster
    /// engines by collection id (`engines`) and its style layers
    /// (`styles`), keyed as the server builds them.
    pub fn over<T: Send + Sync + 'static>(
        state: Arc<ArcSwap<T>>,
        engines: fn(&T) -> &HashMap<String, Arc<dyn MapEngine>>,
        styles: fn(&T) -> &StyleLayers,
    ) -> Self {
        Self(Arc::new(move || {
            Arc::new(Snapshot {
                state: state.load_full(),
                engines,
                styles,
            })
        }))
    }
}

/// What a legend reads of one registry snapshot.
trait StyleRegistry: Send + Sync {
    fn engine(&self, id: &str) -> Option<&Arc<dyn MapEngine>>;
    fn styles(&self) -> &StyleLayers;
}

struct Snapshot<T> {
    state: Arc<T>,
    engines: fn(&T) -> &HashMap<String, Arc<dyn MapEngine>>,
    styles: fn(&T) -> &StyleLayers,
}

impl<T: Send + Sync> StyleRegistry for Snapshot<T> {
    fn engine(&self, id: &str) -> Option<&Arc<dyn MapEngine>> {
        (self.engines)(&self.state).get(id)
    }

    fn styles(&self) -> &StyleLayers {
        (self.styles)(&self.state)
    }
}

/// The shared root's legend route state: Maps' registry, then the
/// fallback's.
#[derive(Clone)]
struct SharedLegend {
    maps: AppState,
    fallback: Option<StyleFallback>,
}

/// GET /collections/{id}/styles/{styleId}/legend at the shared root: Maps'
/// legend, or for a collection only the fallback draws, the same legend
/// from the fallback's engine and styles (#792). A collection neither
/// draws is Maps' 404.
async fn shared_style_legend(
    Path((id, style_id)): Path<(String, String)>,
    JsonQuery(legend): JsonQuery<LegendQueryParams>,
    RawQuery(query): RawQuery,
    State(state): State<SharedLegend>,
) -> Result<Response, MapsError> {
    // As on every legend route, a parameter it does not take is a 400
    // (#1073), before the collection is looked up.
    params::reject_unknown_parameters(query.as_deref(), params::LEGEND_PARAMETERS)?;
    let maps = state.maps.load_full();
    if !maps.engines.contains_key(&id) {
        if let Some(registry) = state.fallback.as_ref().map(|fallback| (fallback.0)()) {
            if let Some(engine) = registry.engine(&id) {
                return handlers::legend(engine, registry.styles(), &id, &style_id, &legend).await;
            }
        }
    }
    let (engine, _config) = handlers::lookup_engine(&maps, &id)?;
    handlers::legend(engine, &maps.styles, &id, &style_id, &legend).await
}

fn contribution(
    state: &MapsState,
    config: &ds_core::config::CollectionConfig,
    root: &str,
) -> Option<Contribution> {
    let engine = state.engines.get(&config.id)?;
    let info = engine.raster_info_shared();
    let (fields, links) =
        handlers::collection_parts(config, engine.as_ref(), &info, &state.styles, root);
    Some(Contribution {
        config: config.clone(),
        fields,
        links,
        bbox: info.spatial_extent,
        time: info.times.first().copied().zip(info.times.last().copied()),
        // Maps omits a native CRS with no OGC URI rather than mislabel it.
        claims: &["storageCrs"],
        items: None,
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

    fn discovery(&self) -> ds_core::discovery::Discovery {
        self.state.load().discovery.clone()
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

    fn serves(&self, id: &str) -> bool {
        self.state.load().collections.contains_key(id)
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
        let legend = SharedLegend {
            maps: self.state.clone(),
            fallback: self.fallback.clone(),
        };
        Router::new()
            .route("/collections/{id}/map", get(handlers::get_map))
            // Served only as JSON: `f=html` is a 400 naming the format (#1132).
            .route(
                "/collections/{id}/styles",
                api_common::errors::json_only_get(handlers::styles, "a style list"),
            )
            .route(
                "/collections/{id}/styles/{styleId}/map",
                get(handlers::get_styled_map),
            )
            // The legend of every raster collection the root draws: Maps',
            // then the fallback's (#792).
            .route(
                "/collections/{id}/styles/{styleId}/legend",
                get(shared_style_legend).with_state(legend),
            )
            .with_state(self.state.clone())
    }
}
