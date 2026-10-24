//! Shared OGC API root (#789): OGC API standards composed as building blocks
//! under one landing page, conformance declaration, OpenAPI document and
//! collection catalog.
//!
//! Each [`BuildingBlock`] (Maps, Tiles, Features, …) contributes its
//! conformance classes, landing links, OpenAPI paths, data-access routes and —
//! per collection — its access links and standard fields. The composer owns
//! the Common resources and merges a collection's contributions into one
//! description: links are concatenated in block order, the first block to
//! describe a field wins (unless an earlier block [claims](Contribution::claims)
//! it), `styles` merge by style id so each block can add its per-style
//! links, and `crs` lists union in block order. Common Part 2 §6.2: "the available data access
//! mechanisms supported for a specific collection are typically advertised by
//! including links … in the links array of the collection description."
//!
//! The per-API services (`/maps`, `/tiles`, …) remain routed, though only
//! EDR is advertised; blocks reuse their engine registries, so a reload
//! reaches both surfaces at once.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use ds_core::config::CollectionConfig;
use ds_core::html::{FormatParams, LinkView, Wanted};
use serde_json::{json, map::Entry, Map, Value};

use crate::workbench::{self, Surface};
use crate::{
    caching, openapi_tags, rel, tag_api_kind, CollectionEntry, CollectionRequest, JsonQuery, Mount,
};

/// API kind of the shared root's own Common resources in logs and metrics.
pub const COMMON: &str = "common";

/// HTML workspace kind of the shared root.
pub const WORKSPACE: &str = "ogc";

/// One collection as a building block serves it.
pub struct Contribution {
    pub config: CollectionConfig,
    /// Standard fields this block describes (`extent`, `crs`, `styles`, …).
    pub fields: Map<String, Value>,
    /// Data-access links; the composer adds `self`, `alternate` and license.
    pub links: Vec<Value>,
    pub bbox: Option<[f64; 4]>,
    pub time: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Fields this block describes authoritatively even when it leaves them
    /// out: no later block may fill them. A raster block claims `storageCrs`
    /// because it omits a native CRS with no OGC URI rather than mislabel it,
    /// and a later Features block's CRS84 must not stand in for it.
    pub claims: &'static [&'static str],
    /// What the collection's HTML overview says of its feature items
    /// (#1143), from the block serving them.
    pub items: Option<workbench::ItemsOverview>,
}

/// OpenAPI paths and components a block adds to the shared definition.
#[derive(Default)]
pub struct OpenApiFragment {
    /// Path keys include the shared root's mount.
    pub paths: Map<String, Value>,
    /// Components by section (`parameters`, `schemas`, …), then by name.
    pub components: Map<String, Value>,
}

/// One OGC API standard as served at the shared root.
pub trait BuildingBlock: Send + Sync + 'static {
    /// API kind reported in request logs and metrics.
    fn kind(&self) -> &'static str;
    /// Conformance classes this block implements on its per-API service and
    /// at the shared root alike.
    fn conformance(&self) -> &'static [&'static str];
    /// Classes this block implements only in the shared root's layout, where
    /// it composes with the other blocks: Tiles serves map tiles under Maps'
    /// map resources there (`{map}/tiles`), which OGC API - Maps "Map
    /// Tilesets" requires and neither per-API service has. Declared once at
    /// the shared root, after [`conformance`](Self::conformance).
    fn shared_conformance(&self) -> &'static [&'static str] {
        &[]
    }
    /// Landing-page links beyond the Common ones, for the absolute API root.
    fn landing_links(&self, _root: &str) -> Vec<Value> {
        Vec::new()
    }
    /// External base URL of this request, per the block's configuration.
    fn base_url(&self, headers: &HeaderMap) -> String;
    /// Every collection this block serves, from one registry snapshot.
    fn collections(&self, root: &str) -> Vec<Contribution>;
    /// One collection, or `None` when this block does not serve it.
    fn collection(&self, id: &str, root: &str) -> Option<Contribution>;
    /// Whether this block serves collection `id`, without assembling its
    /// description (an error page's link back, #1132). The default builds
    /// the contribution; a block with a registry overrides it with a lookup.
    fn serves(&self, id: &str) -> bool {
        self.collection(id, "").is_some()
    }
    /// OpenAPI paths (keys prefixed with `mount`) and components.
    fn openapi(&self, mount: &str) -> OpenApiFragment;
    /// Data-access routes, relative to the shared root.
    fn routes(&self) -> Router;
}

/// A landing-page link to a service outside the shared API (EDR, WMS,
/// health). Paths are relative to the external base URL.
pub struct RelatedLink {
    pub path: String,
    pub rel: &'static str,
    pub media_type: &'static str,
    pub title: String,
}

/// What the services outside the shared root serve of one collection
/// (#1228): EDR with the query types it offers, WMS as the layer named by
/// the collection's id, and 3D Tiles. The collection document names none
/// of them; its HTML page links each and builds example requests for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServedBy {
    /// EDR's query types for the collection, `None` when EDR does not serve
    /// it.
    pub edr: Option<Vec<String>>,
    pub wms: bool,
    pub tiles_3d: bool,
}

/// [`ServedBy`] per collection id, built once per load from the server's
/// registries.
pub type ServedByMap = std::collections::HashMap<String, Arc<ServedBy>>;

/// Where a collection page reads [`ServedBy`]: the server's snapshot of its
/// registries, swapped on reload. A lookup, never a computation: the page
/// is rendered per request (root CLAUDE.md rule 10).
pub trait CollectionServices: Send + Sync + 'static {
    /// The services outside the shared root serving collection `id`;
    /// `None` when none does.
    fn served_by(&self, id: &str) -> Option<Arc<ServedBy>>;
}

/// The shared root: its mount below the external base URL, its blocks in
/// precedence order, and related services for the landing page.
pub struct SharedApi {
    mount: &'static str,
    blocks: Vec<Arc<dyn BuildingBlock>>,
    related: Vec<RelatedLink>,
    /// Common classes plus every block's (its per-API classes, then its
    /// shared-root ones), de-duplicated once.
    conformance: Vec<&'static str>,
    /// What the services outside the shared root serve of each collection;
    /// without it a page reads them from the collection's `apis`.
    services: Option<Arc<dyn CollectionServices>>,
}

impl SharedApi {
    /// Blocks are listed in precedence order; at least one is required.
    pub fn new(
        mount: &'static str,
        blocks: Vec<Arc<dyn BuildingBlock>>,
        related: Vec<RelatedLink>,
    ) -> Self {
        assert!(!blocks.is_empty(), "a shared OGC API root needs a block");
        let mut conformance: Vec<&'static str> = crate::CONFORMANCE_CLASSES.to_vec();
        let classes = blocks.iter().flat_map(|b| b.conformance());
        let shared_only = blocks.iter().flat_map(|b| b.shared_conformance());
        for class in classes.chain(shared_only) {
            if !conformance.contains(class) {
                conformance.push(class);
            }
        }
        Self {
            mount,
            blocks,
            related,
            conformance,
            services: None,
        }
    }

    /// Read what EDR, WMS and 3D Tiles serve of a collection from
    /// `services`, the server's registries, rather than from the
    /// collection's `apis` (#1228): an EDR query type, or a service the
    /// collection's id does not reach, is then never offered.
    pub fn with_services(mut self, services: Arc<dyn CollectionServices>) -> Self {
        self.services = Some(services);
        self
    }

    fn base(&self, headers: &HeaderMap) -> String {
        self.blocks[0].base_url(headers)
    }
}

type AppState = Arc<SharedApi>;

/// The shared root router: Common resources plus every block's routes.
/// Mount it at `api.mount` below the external base URL.
pub fn router(api: SharedApi) -> Router {
    let mount = api.mount;
    let api: AppState = Arc::new(api);
    let common = Router::new()
        .route("/", get(landing_page))
        .route("/api", crate::errors::format_only_get(api_definition))
        .route("/api/docs", get(api_docs))
        .route("/api/docs/{asset}", get(api_docs_asset))
        .route("/conformance", get(conformance))
        .route("/collections", get(collections))
        .route("/collections/{id}", get(collection))
        .with_state(api.clone())
        // Metadata only: data routes keep their own content-derived ETags.
        .layer(axum::middleware::from_fn(caching::conditional_get));
    let mut router = tag_api_kind(common, COMMON);
    for block in &api.blocks {
        router = router.merge(tag_api_kind(block.routes(), block.kind()));
    }
    // Every block's errors, too, are pages for a browser (#1132).
    let router = crate::errors::html_errors(router, WORKSPACE, mount, Arc::new(Pages(api)));
    router.layer(Extension(Mount(mount)))
}

/// What the shared root's error pages know: its base URL and catalog.
struct Pages(AppState);

impl crate::errors::ErrorPages for Pages {
    fn base_url(&self, headers: &HeaderMap) -> String {
        self.0.base(headers)
    }

    fn has_collection(&self, id: &str) -> bool {
        self.0.blocks.iter().any(|block| block.serves(id))
    }
}

fn error(status: StatusCode, code: &str, description: &str) -> Response {
    crate::JsonError(
        status,
        Json(json!({"code": code, "description": description})),
    )
    .into_response()
}

/// The requested representation, or the 400 to send for an unsupported one.
fn negotiate(format: &FormatParams, headers: &HeaderMap) -> Result<Wanted, Box<Response>> {
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    ds_core::html::negotiate(format.f.as_deref(), accept)
        .map_err(|e| Box::new(error(StatusCode::BAD_REQUEST, "BadRequest", &e.to_string())))
}

fn with_vary(mut response: Response) -> Response {
    response
        .headers_mut()
        .append(header::VARY, axum::http::HeaderValue::from_static("accept"));
    response
}

fn link(href: String, rel: &str, media_type: &str, title: &str) -> Value {
    json!({"href": href, "rel": rel, "type": media_type, "title": title})
}

/// GET {mount}/ — Landing page
async fn landing_page(
    State(api): State<AppState>,
    JsonQuery(format): JsonQuery<FormatParams>,
    headers: HeaderMap,
) -> Response {
    let wanted = match negotiate(&format, &headers) {
        Ok(wanted) => wanted,
        Err(response) => return *response,
    };
    let base = &api.base(&headers);
    let root = &Mount(api.mount).root(base);
    let title = "MeteoCore";
    let description = "Metocean Data Server — OGC API building blocks (Maps, Tiles, \
         Features) over one collection catalog";
    let json = "application/json";
    let mut links = vec![
        link(format!("{root}/"), "self", json, "This document"),
        link(
            format!("{root}/api"),
            "service-desc",
            "application/vnd.oai.openapi+json;version=3.0",
            "API definition",
        ),
        link(
            format!("{root}/api/docs"),
            "service-doc",
            "text/html",
            "API documentation",
        ),
        link(
            format!("{root}/conformance"),
            "conformance",
            json,
            "Conformance classes",
        ),
        link(
            format!("{root}/conformance"),
            rel::CONFORMANCE,
            json,
            "Conformance classes",
        ),
        link(format!("{root}/collections"), "data", json, "Collections"),
        link(
            format!("{root}/collections"),
            rel::DATA,
            json,
            "Collections",
        ),
    ];
    links.extend(api.blocks.iter().flat_map(|b| b.landing_links(root)));
    links.extend(
        api.related
            .iter()
            .map(|r| link(format!("{base}{}", r.path), r.rel, r.media_type, &r.title)),
    );
    with_vary(match wanted {
        Wanted::Json => Json(json!({"title": title, "description": description, "links": links}))
            .into_response(),
        Wanted::Html => {
            let mut views: Vec<LinkView> = links
                .iter()
                .map(|l| {
                    let view = LinkView::new(
                        l["href"].as_str().unwrap_or_default(),
                        l["rel"].as_str().unwrap_or_default(),
                        l["title"].as_str(),
                    );
                    match l["type"].as_str() {
                        Some(media_type) => view.with_type(media_type),
                        None => view,
                    }
                })
                .collect();
            views.push(
                LinkView::new(
                    format!("{root}/?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                )
                .with_type(json),
            );
            Html(workbench::landing_html(
                Surface {
                    base,
                    root,
                    api: WORKSPACE,
                },
                title,
                description,
                &views,
            ))
            .into_response()
        }
    })
}

/// GET {mount}/conformance — the union of the blocks' classes
async fn conformance(
    State(api): State<AppState>,
    JsonQuery(format): JsonQuery<FormatParams>,
    headers: HeaderMap,
) -> Response {
    let wanted = match negotiate(&format, &headers) {
        Ok(wanted) => wanted,
        Err(response) => return *response,
    };
    let classes = &api.conformance;
    with_vary(match wanted {
        Wanted::Json => Json(json!({"conformsTo": classes})).into_response(),
        Wanted::Html => {
            let base = &api.base(&headers);
            let root = &Mount(api.mount).root(base);
            let nav = [
                LinkView::new(format!("{root}/"), "up", Some("Landing page")),
                LinkView::new(
                    format!("{root}/conformance?f=json"),
                    "alternate",
                    Some("This document as JSON"),
                ),
            ];
            Html(workbench::conformance_html(
                Surface {
                    base,
                    root,
                    api: WORKSPACE,
                },
                classes,
                &nav,
            ))
            .into_response()
        }
    })
}

fn format_parameter() -> Value {
    json!({"name": "f", "in": "query", "required": false, "schema": {"type": "string", "enum": ["json", "html"]},
           "description": "Output format. 'json' (default) or 'html'; overrides the Accept header."})
}

/// The shared OpenAPI document: Common operations plus every block's paths
/// and components. Blocks earlier in precedence order win name clashes; the
/// crates keep overlapping definitions identical (tested).
pub fn openapi_document(api: &SharedApi) -> Value {
    let m = api.mount;
    let mut paths = Map::new();
    for (path, summary, operation_id) in [
        (format!("{m}/"), "Landing page", "getLandingPage"),
        (
            format!("{m}/conformance"),
            "Conformance classes",
            "getConformance",
        ),
    ] {
        paths.insert(
            path,
            json!({"get": {"summary": summary, "operationId": operation_id,
                "tags": [openapi_tags::DISCOVERY],
                "parameters": [format_parameter()],
                "responses": {"200": {"description": summary},
                              "400": crate::format_error_response()}}}),
        );
    }
    paths.insert(
        format!("{m}/collections"),
        json!({"get": crate::collection_operation()}),
    );
    paths.insert(
        format!("{m}/collections/{{collectionId}}"),
        json!({"get": {"summary": "Describe a collection and its data access mechanisms",
            "operationId": "getCollection",
            "tags": [openapi_tags::DISCOVERY],
            "parameters": [
                {"name": "collectionId", "in": "path", "required": true, "schema": {"type": "string"},
                 "description": "Collection identifier"},
                format_parameter()
            ],
            "responses": {"200": {"description": "Collection description"},
                          "400": crate::format_error_response(),
                          "404": {"description": "Collection not found"}}}}),
    );
    let mut components = Map::new();
    for block in &api.blocks {
        let fragment = block.openapi(m);
        for (path, item) in fragment.paths {
            paths.entry(path).or_insert(item);
        }
        for (section, entries) in fragment.components {
            let Value::Object(entries) = entries else {
                continue;
            };
            let target = components
                .entry(section)
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(target) = target {
                for (name, definition) in entries {
                    target.entry(name).or_insert(definition);
                }
            }
        }
    }
    // Collection titles describe the per-collection tags. The contributions'
    // links are unused here, so the mount stands in for the request root.
    let titles: BTreeMap<String, String> = api
        .blocks
        .iter()
        .flat_map(|block| block.collections(m))
        .map(|c| (c.config.id, c.config.title))
        .collect();
    let tags = document_tags(&paths, &titles);
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "MeteoCore - OGC API",
            "version": "1.0.0",
            "description": "OGC API building blocks over one collection catalog (#789)"
        },
        "tags": tags,
        "paths": paths,
        "components": components
    })
}

/// The document's tag list, in the order API docs show them: discovery first,
/// then other shared groups (tiling schemes) as they first appear, then one
/// tag per collection by id, described by its title.
fn document_tags(paths: &Map<String, Value>, titles: &BTreeMap<String, String>) -> Vec<Value> {
    let mut used: Vec<&str> = Vec::new();
    for operation in paths
        .values()
        .filter_map(Value::as_object)
        .flat_map(|i| i.values())
    {
        for tag in operation["tags"].as_array().into_iter().flatten() {
            if let Some(tag) = tag.as_str().filter(|t| !used.contains(t)) {
                used.push(tag);
            }
        }
    }
    let (mut collections, mut groups): (Vec<&str>, Vec<&str>) =
        used.into_iter().partition(|t| titles.contains_key(*t));
    groups.sort_by_key(|t| *t != openapi_tags::DISCOVERY);
    collections.sort_unstable();
    let described = |name: &str| match name {
        openapi_tags::DISCOVERY => Some(openapi_tags::DISCOVERY_DESCRIPTION),
        openapi_tags::TILING_SCHEMES => Some(openapi_tags::TILING_SCHEMES_DESCRIPTION),
        _ => None,
    };
    groups
        .into_iter()
        .map(|name| match described(name) {
            Some(description) => json!({"name": name, "description": description}),
            None => json!({"name": name}),
        })
        .chain(
            collections
                .into_iter()
                .map(|id| json!({"name": id, "description": titles[id]})),
        )
        .collect()
}

/// GET {mount}/api — OpenAPI 3.0.3 definition
async fn api_definition(State(api): State<AppState>) -> Response {
    Json(openapi_document(&api)).into_response()
}

/// GET {mount}/api/docs — Swagger UI
async fn api_docs(State(api): State<AppState>, headers: HeaderMap) -> Response {
    let spec_url = format!("{}/api", Mount(api.mount).root(&api.base(&headers)));
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                ds_core::openapi::SWAGGER_UI_CSP,
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(ds_core::openapi::swagger_ui_html(
            "MeteoCore - OGC API",
            &spec_url,
        )),
    )
        .into_response()
}

/// Pinned Swagger assets embedded in ds-core.
async fn api_docs_asset(Path(asset): Path<String>) -> Response {
    match ds_core::openapi::swagger_ui_asset(&asset) {
        Some((content_type, bytes)) => (
            [
                (header::CONTENT_TYPE, content_type),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A collection description merged from its blocks' contributions.
struct Merged {
    config: CollectionConfig,
    metadata: Value,
    items: Option<workbench::ItemsOverview>,
    bbox: Option<[f64; 4]>,
    time: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Merge one collection's contributions (in block order) into a description.
fn merge(contributions: Vec<Contribution>, root: &str) -> Option<Merged> {
    let mut contributions = contributions.into_iter();
    let first = contributions.next()?;
    let (config, mut fields, mut links, mut bbox, mut time, mut items) = (
        first.config,
        first.fields,
        first.links,
        first.bbox,
        first.time,
        first.items,
    );
    let mut claimed: Vec<&str> = first.claims.to_vec();
    // Discovery bounds follow the advertised extent: they come from the block
    // whose `extent` is kept, so search never matches bounds the description
    // does not show (api-common CLAUDE.md).
    let mut has_extent = fields.contains_key("extent");
    for contribution in contributions {
        if !has_extent && contribution.fields.contains_key("extent") {
            bbox = contribution.bbox;
            time = contribution.time;
            has_extent = true;
        }
        for (key, value) in contribution.fields {
            if claimed.contains(&key.as_str()) {
                continue;
            }
            match fields.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(value);
                }
                Entry::Occupied(mut entry) if entry.key() == "styles" => {
                    merge_styles(entry.get_mut(), value);
                }
                Entry::Occupied(mut entry) if entry.key() == "crs" => {
                    merge_crs(entry.get_mut(), value);
                }
                // The first block to describe a field wins.
                Entry::Occupied(_) => {}
            }
        }
        links.extend(contribution.links);
        claimed.extend(contribution.claims);
        items = items.or(contribution.items);
    }
    let mut all = vec![json!({
        "href": format!("{root}/collections/{}", config.id),
        "rel": "self",
        "type": "application/json",
        "title": config.title
    })];
    all.extend(links);
    let metadata = crate::collection_metadata(&config, Value::Object(fields), all);
    Some(Merged {
        config,
        metadata,
        items,
        bbox,
        time,
    })
}

/// Union `crs` lists, keeping the earlier block's order (its first entry is
/// the default). The collection offers every CRS one of its access
/// mechanisms serves, and Features Part 2 requires `/items` to accept
/// exactly its collection's list, so a vector-tiled feature collection
/// cannot advertise only its tile matrix sets' CRSs (#685).
fn merge_crs(existing: &mut Value, incoming: Value) {
    let (Value::Array(existing), Value::Array(incoming)) = (existing, incoming) else {
        return;
    };
    for crs in incoming {
        if !existing.contains(&crs) {
            existing.push(crs);
        }
    }
}

/// Merge style entries by `id`: a later block's links join the earlier
/// block's entry for the same style; new styles are appended.
fn merge_styles(existing: &mut Value, incoming: Value) {
    let (Value::Array(existing), Value::Array(incoming)) = (existing, incoming) else {
        return;
    };
    for style in incoming {
        let Some(entry) = existing.iter_mut().find(|s| s["id"] == style["id"]) else {
            existing.push(style);
            continue;
        };
        let Value::Array(new_links) = style["links"].clone() else {
            continue;
        };
        if !entry["links"].is_array() {
            entry["links"] = json!([]);
        }
        let links = entry["links"].as_array_mut().expect("links is an array");
        for new in new_links {
            if !links
                .iter()
                .any(|l| l["href"] == new["href"] && l["rel"] == new["rel"])
            {
                links.push(new);
            }
        }
    }
}

/// GET {mount}/collections — every collection any block serves
async fn collections(
    State(api): State<AppState>,
    request: CollectionRequest,
    headers: HeaderMap,
) -> Response {
    let base = &api.base(&headers);
    let root = &Mount(api.mount).root(base);
    let mut by_id: BTreeMap<String, Vec<Contribution>> = BTreeMap::new();
    for block in &api.blocks {
        for contribution in block.collections(root) {
            by_id
                .entry(contribution.config.id.clone())
                .or_default()
                .push(contribution);
        }
    }
    let (configs, parts): (Vec<_>, Vec<_>) = by_id
        .into_values()
        .filter_map(|contributions| merge(contributions, root))
        .map(|m| (m.config, (m.metadata, m.bbox, m.time)))
        .unzip();
    let entries = configs
        .iter()
        .zip(parts)
        .map(|(config, (metadata, bbox, time))| CollectionEntry {
            config,
            metadata,
            bbox,
            time,
        })
        .collect();
    crate::collections_response(
        Surface {
            base,
            root,
            api: WORKSPACE,
        },
        request,
        entries,
    )
}

/// The WMS endpoint of a collection the WMS serves, for its page's map to
/// draw with GetMap (#1220). The collection's `apis`, which the server
/// enforces, wire it to WMS as the layer named by its id; nothing in the
/// collection document says so.
fn wms_endpoint(config: &CollectionConfig, base: &str) -> Option<String> {
    config
        .apis
        .iter()
        .any(|api| api == "wms")
        .then(|| format!("{base}{}", crate::mounts::WMS))
}

/// The services outside the shared root that a collection's `apis` wire it
/// to (#1228), as [`wms_endpoint`] reads WMS: the page's fallback when the
/// server gives no [`CollectionServices`]. The EDR query types are not
/// known from them, so the page offers no EDR example.
fn services_from_apis<'a>(config: &CollectionConfig, base: &str) -> workbench::Services<'a> {
    let lists = |name: &str| config.apis.iter().any(|api| api == name);
    workbench::Services {
        edr: lists("edr").then(|| workbench::EdrService {
            href: collection_url(base, crate::mounts::EDR, &config.id),
            queries: &[],
        }),
        wms: wms_endpoint(config, base),
        tiles_3d: lists("3dtiles")
            .then(|| collection_url(base, crate::mounts::TILES_3D, &config.id)),
    }
}

/// The services [`ServedBy`] names, at their mounts below `base`.
fn services_served<'a>(
    served: Option<&'a ServedBy>,
    id: &str,
    base: &str,
) -> workbench::Services<'a> {
    let Some(served) = served else {
        return workbench::Services::default();
    };
    workbench::Services {
        edr: served.edr.as_deref().map(|queries| workbench::EdrService {
            href: collection_url(base, crate::mounts::EDR, id),
            queries,
        }),
        wms: served.wms.then(|| format!("{base}{}", crate::mounts::WMS)),
        tiles_3d: served
            .tiles_3d
            .then(|| collection_url(base, crate::mounts::TILES_3D, id)),
    }
}

/// A collection's URL in the API mounted at `mount` below `base`.
fn collection_url(base: &str, mount: &str, id: &str) -> String {
    format!("{base}{mount}/collections/{}", workbench::path_segment(id))
}

/// GET {mount}/collections/{id} — one collection, merged from every block
async fn collection(
    Path(id): Path<String>,
    State(api): State<AppState>,
    JsonQuery(format): JsonQuery<FormatParams>,
    headers: HeaderMap,
) -> Response {
    let wanted = match negotiate(&format, &headers) {
        Ok(wanted) => wanted,
        Err(response) => return *response,
    };
    let base = &api.base(&headers);
    let root = &Mount(api.mount).root(base);
    let contributions = api
        .blocks
        .iter()
        .filter_map(|b| b.collection(&id, root))
        .collect();
    let Some(merged) = merge(contributions, root) else {
        return error(
            StatusCode::NOT_FOUND,
            "NotFound",
            &format!("Collection '{id}' not found"),
        );
    };
    with_vary(match wanted {
        Wanted::Json => Json(merged.metadata).into_response(),
        Wanted::Html => {
            // The services outside the shared root, from the server's
            // registries when it gives them (#1228).
            let served = api.services.as_ref().map(|s| s.served_by(&id));
            let services = match &served {
                Some(served) => services_served(served.as_deref(), &id, base),
                None => services_from_apis(&merged.config, base),
            };
            Html(workbench::collection_page(
                Surface {
                    base,
                    root,
                    api: WORKSPACE,
                },
                &merged.metadata,
                merged.config.license.as_ref(),
                merged.items.as_ref(),
                &services,
            ))
            .into_response()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contribution(id: &str, fields: Value, links: Vec<Value>) -> Contribution {
        Contribution {
            config: serde_json::from_value(json!({"id": id, "title": id, "description": ""}))
                .unwrap(),
            fields: fields.as_object().unwrap().clone(),
            links,
            bbox: None,
            time: None,
            claims: &[],
            items: None,
        }
    }

    #[test]
    fn the_page_draws_with_wms_only_for_a_collection_wired_to_it() {
        let config = |apis: Value| -> CollectionConfig {
            serde_json::from_value(
                json!({"id": "c", "title": "c", "description": "", "apis": apis}),
            )
            .unwrap()
        };
        assert_eq!(
            wms_endpoint(&config(json!(["maps", "wms"])), "https://x/base"),
            Some("https://x/base/wms".to_owned())
        );
        assert_eq!(
            wms_endpoint(&config(json!(["maps", "tiles"])), "https://x"),
            None
        );
    }

    /// #1228: the services outside the shared root come from the server's
    /// registries when it gives them: EDR with its query types, WMS and 3D
    /// Tiles at their mounts, and nothing for an id none serves (a radar
    /// volume's network inventory, though its config lists them all).
    /// Without them, from the collection's `apis`, as the WMS draw mode
    /// was (#1220), with EDR's queries unknown.
    #[test]
    fn page_services_follow_the_registries_else_the_apis() {
        let config = |apis: Value| -> CollectionConfig {
            serde_json::from_value(
                json!({"id": "r d", "title": "c", "description": "", "apis": apis}),
            )
            .unwrap()
        };
        let every = config(json!([
            "edr", "wms", "maps", "tiles", "3dtiles", "features"
        ]));
        let fallback = services_from_apis(&every, "https://x/base");
        assert_eq!(
            fallback,
            workbench::Services {
                edr: Some(workbench::EdrService {
                    href: "https://x/base/edr/collections/r%20d".into(),
                    queries: &[],
                }),
                wms: Some("https://x/base/wms".into()),
                tiles_3d: Some("https://x/base/3dtiles/collections/r%20d".into()),
            }
        );
        assert_eq!(
            services_from_apis(&config(json!(["maps", "tiles"])), "https://x"),
            workbench::Services::default()
        );
        let served = ServedBy {
            edr: Some(vec!["area".into(), "radius".into()]),
            wms: true,
            tiles_3d: false,
        };
        let services = services_served(Some(&served), "r d", "https://x");
        assert_eq!(
            services
                .edr
                .as_ref()
                .map(|edr| (edr.href.as_str(), edr.queries)),
            Some((
                "https://x/edr/collections/r%20d",
                &["area".to_owned(), "radius".to_owned()][..]
            ))
        );
        assert_eq!(services.wms.as_deref(), Some("https://x/wms"));
        assert_eq!(services.tiles_3d, None);
        // The network inventory: served by none of them, whatever its
        // `apis` say.
        assert_eq!(
            services_served(None, "r d", "https://x"),
            workbench::Services::default()
        );
    }

    #[test]
    fn discovery_bounds_follow_the_advertised_extent() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let span = Some((t("2026-01-01T00:00:00Z"), t("2026-01-02T00:00:00Z")));
        let mut maps = contribution("c", json!({"extent": {"spatial": {}}}), vec![]);
        maps.bbox = Some([0.0, 0.0, 1.0, 1.0]);
        let mut tiles = contribution("c", json!({"extent": {"temporal": {}}}), vec![]);
        tiles.time = span;
        let merged = merge(vec![maps, tiles], "https://x").unwrap();
        assert_eq!(merged.time, None, "Maps' extent advertises no time");
        assert_eq!(merged.bbox, Some([0.0, 0.0, 1.0, 1.0]));
        // Without an extent of its own, the first block defers to the next.
        let first = contribution("c", json!({}), vec![]);
        let mut second = contribution("c", json!({"extent": {"temporal": {}}}), vec![]);
        second.time = span;
        assert_eq!(merge(vec![first, second], "https://x").unwrap().time, span);
    }

    #[test]
    fn a_claimed_field_is_never_filled_by_a_later_block() {
        let crs84 = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
        let features = || {
            contribution(
                "c",
                json!({"storageCrs": crs84, "itemType": "feature"}),
                vec![],
            )
        };
        // A raster whose native CRS has no URI: Maps omits and claims it.
        let mut maps = contribution("c", json!({"dataType": "map"}), vec![]);
        maps.claims = &["storageCrs"];
        let merged = merge(vec![maps, features()], "https://x").unwrap().metadata;
        assert!(merged.get("storageCrs").is_none(), "{merged}");
        assert_eq!(
            merged["itemType"], "feature",
            "unclaimed fields still merge"
        );
        // Vector tiles claim nothing, so Features' CRS84 describes the storage.
        let tiles = contribution("c", json!({"dataType": "vector"}), vec![]);
        let merged = merge(vec![tiles, features()], "https://x")
            .unwrap()
            .metadata;
        assert_eq!(merged["storageCrs"], crs84);
    }

    /// The items overview comes from the block that serves the items
    /// (#1143), whatever its place in block order.
    #[test]
    fn merge_keeps_the_items_overview_of_the_block_serving_items() {
        let overview = workbench::ItemsOverview {
            filterables: vec!["name".into()],
            ..Default::default()
        };
        let mut features = contribution("a", json!({}), vec![]);
        features.items = Some(overview.clone());
        let merged = merge(
            vec![contribution("a", json!({}), vec![]), features],
            "https://x",
        )
        .unwrap();
        assert_eq!(merged.items, Some(overview));
        let merged = merge(vec![contribution("a", json!({}), vec![])], "https://x").unwrap();
        assert_eq!(merged.items, None);
    }

    #[test]
    fn merge_keeps_first_fields_concatenates_links_and_joins_styles_by_id() {
        let maps = contribution(
            "radar",
            json!({"crs": ["crs84", "maps"], "dataType": "map", "styles": [
                {"id": "default", "links": [{"rel": "map", "href": "/m/default"}]},
                {"id": "rain", "links": [{"rel": "map", "href": "/m/rain"}]}]}),
            vec![json!({"rel": "map", "href": "/map"})],
        );
        let tiles = contribution(
            "radar",
            json!({"crs": ["crs84", "tiles"], "dataType": "vector", "styles": [
                {"id": "default", "links": [
                    {"rel": "map", "href": "/m/default"},
                    {"rel": rel::TILESETS_MAP, "href": "/t/default"}]},
                {"id": "extra", "links": []}]}),
            vec![json!({"rel": rel::TILESETS_MAP, "href": "/map/tiles"})],
        );
        let merged = merge(vec![maps, tiles], "https://x").unwrap().metadata;
        assert_eq!(
            merged["crs"],
            json!(["crs84", "maps", "tiles"]),
            "crs lists union, earlier order first"
        );
        assert_eq!(merged["dataType"], "map", "the first block wins");
        let rels: Vec<_> = merged["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["rel"].as_str().unwrap())
            .collect();
        assert_eq!(rels[0], "self");
        assert!(rels.contains(&"map") && rels.contains(&rel::TILESETS_MAP));
        let styles = merged["styles"].as_array().unwrap();
        assert_eq!(styles.len(), 3);
        let default_links = styles[0]["links"].as_array().unwrap();
        assert_eq!(default_links.len(), 2, "duplicate link collapses");
        assert_eq!(styles[2]["id"], "extra");
    }
}
