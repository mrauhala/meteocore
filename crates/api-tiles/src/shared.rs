//! OGC API - Tiles as a building block of the shared OGC API root (#789).
//!
//! Uses the Tiles Table 8 layout: map tiles under
//! `/collections/{id}/map/tiles`, styled map tiles under
//! `/collections/{id}/styles/{styleId}/map/tiles`, and vector tiles (MVT)
//! under `/collections/{id}/tiles`, each list with its own tileset resources.
//! Engines, styles and caches are the per-API `/tiles` service's.

use api_common::rel;
use api_common::shared::{BuildingBlock, Contribution, OpenApiFragment};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Map, Value};

use crate::handlers::{self, AppState, Layout, TileSources, TilesState, MVT_CONTENT_TYPE};
use crate::params;
use crate::tilematrixset::SUPPORTED_TILE_MATRIX_SETS;

/// The Tiles building block over the per-API service's state.
pub struct TilesBlock {
    state: AppState,
}

impl TilesBlock {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

fn contribution(state: &TilesState, sources: &TileSources<'_>, root: &str) -> Contribution {
    let (fields, links) = handlers::collection_parts(
        sources,
        state.styles.get(&sources.config.id),
        Layout::Shared,
        root,
    );
    Contribution {
        config: sources.config.clone(),
        fields,
        links,
        bbox: sources.spatial_extent(),
        time: sources.time(),
    }
}

impl BuildingBlock for TilesBlock {
    fn kind(&self) -> &'static str {
        "tiles"
    }

    fn conformance(&self) -> &'static [&'static str] {
        handlers::CONFORMANCE
    }

    fn landing_links(&self, root: &str) -> Vec<Value> {
        let href = format!("{root}/tileMatrixSets");
        [("tiling-schemes"), (rel::TILING_SCHEMES)]
            .into_iter()
            .map(|relation| {
                json!({"href": href, "rel": relation, "type": "application/json",
                       "title": "Tile matrix sets"})
            })
            .collect()
    }

    fn base_url(&self, headers: &HeaderMap) -> String {
        handlers::request_base_url(&self.state.load(), headers)
    }

    fn collections(&self, root: &str) -> Vec<Contribution> {
        let state = self.state.load();
        handlers::tile_collections(&state)
            .iter()
            .map(|sources| contribution(&state, sources, root))
            .collect()
    }

    fn collection(&self, id: &str, root: &str) -> Option<Contribution> {
        let state = self.state.load();
        let sources = handlers::tile_sources(&state, id).ok()?;
        Some(contribution(&state, &sources, root))
    }

    fn openapi(&self, mount: &str) -> OpenApiFragment {
        let state = self.state.load();
        let mut paths = handlers::tile_matrix_set_openapi_paths(mount);
        paths.extend(collection_openapi_paths(&state, mount));
        // The shared layout references only these per-API components; its
        // tile-format parameters are inline (they differ from Maps' `f`).
        let per_api = handlers::openapi_components();
        let parameters: Map<String, Value> = ["datetime", "elevation"]
            .into_iter()
            .filter_map(|name| {
                let definition = per_api["parameters"].get(name)?.clone();
                Some((name.to_owned(), definition))
            })
            .collect();
        let mut components = Map::new();
        components.insert("parameters".into(), Value::Object(parameters));
        OpenApiFragment { paths, components }
    }

    fn routes(&self) -> Router {
        Router::new()
            .route("/tileMatrixSets", get(handlers::tile_matrix_sets))
            .route(
                "/tileMatrixSets/{tileMatrixSetId}",
                get(handlers::tile_matrix_set),
            )
            .route("/collections/{id}/map/tiles", get(handlers::map_tilesets))
            .route(
                "/collections/{id}/map/tiles/{tileMatrixSetId}",
                get(handlers::map_tileset),
            )
            .route(
                "/collections/{id}/map/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
                get(handlers::map_tile),
            )
            .route(
                "/collections/{id}/styles/{styleId}/map/tiles",
                get(handlers::styled_map_tilesets),
            )
            .route(
                "/collections/{id}/styles/{styleId}/map/tiles/{tileMatrixSetId}",
                get(handlers::styled_map_tileset),
            )
            .route(
                "/collections/{id}/styles/{styleId}/map/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
                get(handlers::get_styled_tile),
            )
            .route("/collections/{id}/tiles", get(handlers::vector_tilesets))
            .route(
                "/collections/{id}/tiles/{tileMatrixSetId}",
                get(handlers::vector_tileset),
            )
            .route(
                "/collections/{id}/tiles/{tileMatrixSetId}/{tileMatrix}/{tileRow}/{tileCol}",
                get(handlers::vector_tile),
            )
            .with_state(self.state.clone())
    }
}

fn path_parameter(name: &str, schema: Value, description: &str) -> Value {
    json!({"name": name, "in": "path", "required": true, "schema": schema,
           "description": description})
}

fn tile_matrix_set_parameter() -> Value {
    path_parameter(
        "tileMatrixSetId",
        json!({"type": "string", "enum": SUPPORTED_TILE_MATRIX_SETS}),
        "Tile matrix set identifier",
    )
}

fn tile_parameters() -> Vec<Value> {
    vec![
        tile_matrix_set_parameter(),
        path_parameter(
            "tileMatrix",
            json!({"type": "integer", "minimum": 0, "maximum": params::MAX_ZOOM_LEVEL}),
            "Zoom level",
        ),
        path_parameter(
            "tileRow",
            json!({"type": "integer", "minimum": 0}),
            "Row index",
        ),
        path_parameter(
            "tileCol",
            json!({"type": "integer", "minimum": 0}),
            "Column index",
        ),
    ]
}

fn style_parameter() -> Value {
    path_parameter("styleId", json!({"type": "string"}), "Style identifier")
}

fn binary(media_types: &[&str]) -> Value {
    Value::Object(
        media_types
            .iter()
            .map(|t| {
                (
                    t.to_string(),
                    json!({"schema": {"type": "string", "format": "binary"}}),
                )
            })
            .collect(),
    )
}

/// The list, tileset and tile operations below one `…/tiles` path.
fn tileset_paths(
    paths: &mut Map<String, Value>,
    list: String,
    operation: &str,
    summary: &str,
    extra: &[Value],
    tile_query: Vec<Value>,
    tile_content: Value,
) {
    let with = |params: Vec<Value>| -> Vec<Value> { extra.iter().cloned().chain(params).collect() };
    paths.insert(
        list.clone(),
        json!({"get": {"summary": format!("{summary} tilesets"),
            "operationId": format!("get{operation}Tilesets"),
            "parameters": with(vec![]),
            "responses": {"200": {"description": "Tileset list"},
                          "404": {"description": "Not found"}}}}),
    );
    paths.insert(
        format!("{list}/{{tileMatrixSetId}}"),
        json!({"get": {"summary": format!("{summary} tileset"),
            "operationId": format!("get{operation}Tileset"),
            "parameters": with(vec![tile_matrix_set_parameter()]),
            "responses": {"200": {"description": "Tileset metadata"},
                          "404": {"description": "Not found"}}}}),
    );
    let mut parameters = with(tile_parameters());
    parameters.extend(tile_query);
    paths.insert(
        format!("{list}/{{tileMatrixSetId}}/{{tileMatrix}}/{{tileRow}}/{{tileCol}}"),
        json!({"get": {"summary": format!("{summary} tile"),
            "operationId": format!("get{operation}Tile"),
            "parameters": parameters,
            "responses": {"200": {"description": "Tile", "content": tile_content},
                          "400": {"description": "Bad request"},
                          "404": {"description": "Not found"},
                          "422": {"description": "Tile too dense (feature count exceeds per-tile cap)"},
                          "500": {"description": "Server error"},
                          "503": {"description": "Render capacity exhausted; retry"}}}}),
    );
}

/// Per-collection tile paths of the shared layout, keyed below the mount.
fn collection_openapi_paths(state: &TilesState, m: &str) -> Map<String, Value> {
    let mut paths = Map::new();
    let raster_formats = ["image/png", "image/jpeg", "image/webp"];
    let raster_query = || {
        vec![
            json!({"$ref": "#/components/parameters/datetime"}),
            json!({"$ref": "#/components/parameters/elevation"}),
            json!({"name": "parameter-name", "in": "query", "required": false,
                   "schema": {"type": "string"},
                   "description": "Render this parameter of a multi-parameter collection."}),
            json!({"name": "f", "in": "query", "required": false,
                   "schema": {"type": "string", "default": "image/png", "enum": raster_formats},
                   "description": "Image format. `image/png` emits an 8-bit palette PNG for colormap layers when possible."}),
        ]
    };
    for sources in handlers::tile_collections(state) {
        let id = &sources.config.id;
        if sources.raster_info.is_some() {
            tileset_paths(
                &mut paths,
                format!("{m}/collections/{id}/map/tiles"),
                &format!("Map_{id}_"),
                &format!("{} map", sources.config.title),
                &[],
                raster_query(),
                binary(&raster_formats),
            );
            tileset_paths(
                &mut paths,
                format!("{m}/collections/{id}/styles/{{styleId}}/map/tiles"),
                &format!("StyledMap_{id}_"),
                &format!("{} styled map", sources.config.title),
                &[style_parameter()],
                raster_query(),
                binary(&raster_formats),
            );
        }
        if sources.has_vector {
            tileset_paths(
                &mut paths,
                format!("{m}/collections/{id}/tiles"),
                &format!("Vector_{id}_"),
                &format!("{} vector", sources.config.title),
                &[],
                vec![json!({"name": "f", "in": "query", "required": false,
                    "schema": {"type": "string", "enum": crate::params::MVT_FORMAT_TOKENS},
                    "description": "Optional; vector tiles are always Mapbox Vector Tiles."})],
                binary(&[MVT_CONTENT_TYPE]),
            );
        }
    }
    paths
}
