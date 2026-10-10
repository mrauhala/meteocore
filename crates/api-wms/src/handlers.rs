use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;

use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_executor::{RenderOutcome, RenderPhase, RenderPhases, RenderTiming};
use ds_render::{CacheKey, ColorMap, CompositeSpec, RenderedCache, StyleInfo};

use crate::error::WmsError;
use crate::params::{WmsQuery, WmsRequestType};

/// Log a phase breakdown for any GetMap render at or above this wall-clock time,
/// so production tells us where a slow render's time actually goes (queue wait
/// vs tile render vs assemble vs encode). Diagnostic for the cold-render tail.
const SLOW_RENDER_LOG_MS: u64 = 400;

/// Which path a GetMap render took, for the slow-render diagnostic log. Keeps the
/// four cases distinct (a meta render, an all-nodata meta render, a meta render
/// that *fell back* to direct, and a genuine non-meta render) rather than
/// conflating the last three under one "direct" label.
enum RenderPath {
    /// Web Mercator meta-tiling, with per-phase stats.
    Meta(ds_render::MetaTileStats),
    /// Meta-tiling path, every covered pixel nodata — carries the tile-loop
    /// timing (assemble/encode skipped).
    MetaEmpty(ds_render::MetaTileStats),
    /// Meta-tiling declined (degenerate bbox / over tile budget / extreme
    /// zoom) → direct, with its engine and encode phases.
    Fallback(RenderPhases),
    /// Genuine non-meta path (non-3857 CRS, or meta cache disabled).
    Direct(RenderPhases),
}

impl RenderPath {
    /// The render-latency outcome (#466): a meta-tiled view whose covering
    /// tiles were all cached only assembled and encoded; every other path
    /// read the engine.
    fn outcome(&self) -> RenderOutcome {
        match self {
            RenderPath::Meta(stats) | RenderPath::MetaEmpty(stats) if stats.misses == 0 => {
                RenderOutcome::Assembled
            }
            _ => RenderOutcome::Cold,
        }
    }

    /// The worker's render phases (#147), from the same measurements the
    /// slow-render log reads. A meta-tiled view's `engine` is the uncached
    /// tiles' engine reads, and its `encode` also counts colorizing them, as
    /// the direct path's `render_tile` does. Phases a path skipped stay unset.
    fn phases(&self) -> RenderPhases {
        match self {
            RenderPath::Meta(stats) | RenderPath::MetaEmpty(stats) => {
                let mut phases = RenderPhases::default();
                if stats.misses > 0 {
                    phases.add(RenderPhase::Engine, stats.engine);
                }
                // An all-nodata view skips assembly and encoding.
                if matches!(self, RenderPath::Meta(_)) {
                    phases.add(RenderPhase::Assemble, stats.assemble);
                    phases.add(RenderPhase::Encode, stats.colorize + stats.encode);
                }
                phases
            }
            RenderPath::Fallback(phases) | RenderPath::Direct(phases) => *phases,
        }
    }
}

/// How a GetMap turns engine output into pixels.
enum Paint {
    /// One parameter through its style's colormap.
    Colormap(Arc<dyn ColorMap>),
    /// An RGB composite layer (#819): its bands from `get_raster_tiles`,
    /// composed by its channels. It has one style, `default`.
    Composite(Arc<CompositeSpec>),
}

impl Paint {
    /// Value planes a render holds at once, for memory admission.
    fn planes(&self) -> usize {
        match self {
            Paint::Colormap(_) => 1,
            Paint::Composite(spec) => spec.parameters.len(),
        }
    }
}

/// The RGB composite `layer` names, when it is `{collection}/{composite}`
/// of an engine that serves one (#819).
fn composite_layer(engine: &dyn MapEngine, layer: &str) -> Option<Arc<CompositeSpec>> {
    let (_, name) = layer.split_once('/')?;
    engine
        .composites()
        .iter()
        .find(|c| c.name == name)
        .map(|def| Arc::new(CompositeSpec::from(def)))
}

#[derive(Clone)]
pub struct WmsState {
    pub engines: HashMap<String, Arc<dyn MapEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
    /// Map of layer → style name → StyleInfo. Every layer has at least "default".
    pub styles: HashMap<String, HashMap<String, StyleInfo>>,
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    pub rendered_cache: Arc<RenderedCache>,
    /// Decoded-RGBA meta-tile cache for the Web Mercator GetMap path (#202).
    pub tile_cache: Arc<ds_render::TilePixelCache>,
    /// Static fallback base URL for absolute links. Used as-is unless
    /// `trust_proxy_headers` resolves a per-request value.
    pub base_url: String,
    /// Honour reverse-proxy forwarding headers when generating self-links (#12).
    pub trust_proxy_headers: bool,
}

pub type AppState = Arc<ArcSwap<WmsState>>;

/// Resolve the absolute base URL for the current request, honouring reverse-proxy
/// forwarding headers when `trust_proxy_headers` is enabled (#12).
fn request_base_url(state: &WmsState, headers: &HeaderMap) -> String {
    ds_core::proxy::resolve_base_url(&state.base_url, state.trust_proxy_headers, |name| {
        headers.get(name).and_then(|v| v.to_str().ok())
    })
}

/// Render a semi-transparent red error tile to make failed areas visible,
/// composited over `background` when the request asked for opaque output.
fn render_error_tile(
    width: u32,
    height: u32,
    background: Option<[u8; 3]>,
) -> Result<Vec<u8>, WmsError> {
    let pixel_count = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for _ in 0..pixel_count {
        rgba.extend_from_slice(&[255, 0, 0, 100]);
    }
    if let Some(bg) = background {
        ds_render::flatten_onto(&mut rgba, bg);
    }
    ds_render::encode_png(&rgba, width, height)
        .map_err(|e| WmsError::Internal(format!("Failed to encode error tile: {e}")))
}

/// Cache-Control header value for a WMS response.
///
/// - An explicit TIME that resolved to a timestep, over immutable content
///   (`content_version == 0`): the pixels for that instant never change —
///   cache for 24 hours, `immutable` (no revalidation at all).
/// - Otherwise — no TIME ("latest" moves), a TIME the engine has nothing to
///   render for yet (resolved to `None`: its catalog is still empty after a
///   start or reload), or content the engine revises in place under a fixed
///   instant (`content_version != 0`, e.g. a push-fed alert set): short
///   cache (60 s) and revalidate, so a browser/CDN that holds a pre-revision
///   tile for the same URL asks again (the ETag is content-derived, so an
///   unchanged tile is a cheap 304). The red error tile is always short too.
fn cache_control_value(pinned_time: bool, content_version: u64) -> &'static str {
    if pinned_time && content_version == 0 {
        "public, max-age=86400, immutable"
    } else {
        "public, max-age=60, must-revalidate"
    }
}

/// Main WMS handler — dispatches on REQUEST parameter.
pub async fn wms_handler(
    headers: HeaderMap,
    Query(query): Query<WmsQuery>,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, WmsError> {
    let state = state.load_full();

    match query.request_type()? {
        WmsRequestType::GetCapabilities => {
            let xml = crate::capabilities::get_capabilities_xml(
                &state.engines,
                &state.collections,
                &state.styles,
                &request_base_url(&state, &headers),
            );
            Ok((
                [
                    (header::CONTENT_TYPE, "text/xml"),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                ],
                xml,
            )
                .into_response())
        }
        WmsRequestType::GetMap => {
            let params = query.validate_get_map()?;

            // Parse layer name: "collection-id" or "collection-id/parameter"
            let (collection_id, layer_parameter) =
                if let Some((cid, param)) = params.layer.split_once('/') {
                    (cid.to_string(), Some(param.to_string()))
                } else {
                    (params.layer.clone(), None)
                };

            // Look up engine by collection ID
            let engine = state
                .engines
                .get(&collection_id)
                .ok_or_else(|| WmsError::layer_not_found(&params.layer))?;

            // An RGB composite layer (#819) has no style map: its colours
            // come from its channels, and its one style is `default`.
            let composite = composite_layer(engine.as_ref(), &params.layer);

            // Look up style: try full layer name first (e.g., "ecmwf-kenya/2t" for
            // per-parameter defaults), then fall back to collection ID
            let (paint, style_name, style_parameter) = match &composite {
                Some(spec) => {
                    if params.style != ds_render::COMPOSITE_STYLE {
                        return Err(WmsError::StyleNotDefined(format!(
                            "Style '{}' not defined for layer '{}'. Available: {}",
                            params.style,
                            params.layer,
                            ds_render::COMPOSITE_STYLE
                        )));
                    }
                    (
                        Paint::Composite(spec.clone()),
                        ds_render::COMPOSITE_STYLE.to_string(),
                        None,
                    )
                }
                None => {
                    let layer_styles = state
                        .styles
                        .get(&params.layer)
                        .or_else(|| state.styles.get(&collection_id))
                        .ok_or_else(|| WmsError::layer_not_found(&params.layer))?;

                    let style_info = layer_styles.get(&params.style).ok_or_else(|| {
                        WmsError::StyleNotDefined(format!(
                            "Style '{}' not defined for layer '{}'. Available: {}",
                            params.style,
                            params.layer,
                            layer_styles.keys().cloned().collect::<Vec<_>>().join(", ")
                        ))
                    })?;
                    (
                        Paint::Colormap(style_info.colormap.clone()),
                        style_info.name.clone(),
                        style_info.parameter.clone(),
                    )
                }
            };

            // Share one metadata snapshot across parameter, ELEVATION and
            // reference_time validation below.
            let info = engine.raster_info_shared();

            // Validate `LAYERS=collection/parameter` against the engine's
            // advertised list (mirroring Maps + Tiles). Without this, an
            // unknown parameter would silently render whatever the engine
            // defaults to and cache that result under the invalid name —
            // ServiceException is the correct OGC response here.
            if let Some(pname) = layer_parameter.as_deref() {
                if composite.is_none()
                    && !info.parameters.is_empty()
                    && !info.parameters.iter().any(|p| p.name == pname)
                {
                    let composites = engine.composites();
                    let mut supported: Vec<&str> = info
                        .parameters
                        .iter()
                        .map(|p| p.name.as_str())
                        .chain(composites.iter().map(|c| c.name.as_str()))
                        .collect();
                    supported.sort_unstable();
                    return Err(WmsError::LayerNotDefined(format!(
                        "Parameter '{pname}' is not available for layer \
                         '{collection_id}'. Available: {}",
                        supported.join(", ")
                    )));
                }
            }

            // Reject an `ELEVATION` against a layer with no vertical axis.
            if params.elevation.is_some() && info.vertical.is_none() {
                return Err(WmsError::invalid_parameter(&format!(
                    "Layer '{collection_id}' has no ELEVATION dimension"
                )));
            }

            // Validate `DIM_REFERENCE_TIME` against the layer's advertised model
            // runs. The engine requires an exact run match (`select_run` →
            // `ReferenceTimeNotFound`, which the GetMap render path would turn
            // into a red 200 tile); surfacing `InvalidDimensionValue` here is the
            // correct WMS response — mirroring the parameter/ELEVATION checks.
            if let Some(rt) = params.reference_time {
                if info.reference_times.is_empty() {
                    return Err(WmsError::InvalidDimensionValue(format!(
                        "Layer '{collection_id}' has no reference_time dimension"
                    )));
                }
                // Ascending by the `RasterInfo` contract; an archive keeps
                // thousands of runs (#1006).
                if info.reference_times.binary_search(&rt).is_err() {
                    return Err(WmsError::InvalidDimensionValue(format!(
                        "reference_time '{}' is not an available model run for layer \
                         '{collection_id}'",
                        rt.to_rfc3339()
                    )));
                }
            }

            // The format as encoded: an explicit QUALITY, else for WebP the
            // collection's `[wms] webp_quality`, else the format default
            // (JPEG 85, lossless WebP). Resolved before keying, so the
            // rendered cache never serves a lossy image for a lossless
            // request or the reverse.
            let format = params.format.with_quality(
                params.quality,
                state
                    .collections
                    .get(&collection_id)
                    .and_then(CollectionConfig::webp_quality),
            );
            let content_type = format.content_type();
            let has_explicit_time = params.time.is_some();

            // WMS picks the parameter via `LAYERS=collection/param` (parsed
            // into layer_parameter) and then `style_info.parameter`. Settle it
            // first: a parameter can have its own time axis, and so can a
            // composite, whose name the engine resolves like a parameter's.
            let parameter = layer_parameter.clone().or(style_parameter);

            // Engine-owned default first (CAP: active now), then the
            // parameter's (else the collection's) latest time. Resolve before
            // keying so an omitted TIME tracks catalog updates.
            let time = params.time.or_else(|| {
                ds_core::map_engine::default_request_time(
                    engine.as_ref(),
                    &info,
                    parameter.as_deref(),
                )
            });

            // Normalise an explicit pin of the *current* latest run to `None`
            // BEFORE resolution, so it gets the same fallback-tolerant run
            // selection as an omitted dimension: the common client flow
            // echoes the GetCapabilities `default=` (the latest run) on
            // every request — including animation frames OLDER than that
            // run's reference time, which an exact pin would refuse (GRIB's
            // cross-run fallback applies only to `None`). A pin of an
            // *older* run stays explicit/exact — that is user intent.
            let requested_run = params
                .reference_time
                .filter(|&rt| info.reference_times.last().copied() != Some(rt));

            // #521: resolve the run axis to the CONCRETE run the engine will
            // render — the run-axis mirror of what #508 does for TIME below.
            // The no-TTL caches keyed on `None` ("latest at render time")
            // would freeze whichever run was latest at first render: when a
            // newer run re-covers the same valid times with different pixels
            // (every few hours for NWP, every ~5 min for a nowcast
            // generation), the stale entries would keep serving forever.
            // Asking the ENGINE (rather than picking
            // `reference_times.last()` here) preserves engine-specific
            // selection such as GRIB's cross-run fallback — a valid time the
            // newest run doesn't cover resolves to (and keys) the older run
            // actually rendered. Engines without runs keep the identity
            // default (`None` stays `None`).
            let reference_time = engine.resolve_reference_time(time, requested_run);

            // #507: snap that instant to the exact timestep the engine will
            // actually render, BEFORE any cache key is built. Engines that
            // select latest-not-after (geotiff) would otherwise render T−1
            // pixels for a not-yet-ingested TIME=T and cache them under T's
            // key — permanently poisoning frame T for the covered tiles once
            // T's file lands (the partial one-step-behind animation regions
            // seen under storm load). Resolving here also pins one timestep
            // for the whole request, so a catalog swap mid-render can no
            // longer mix timesteps within a single response. Exact-match
            // engines keep the identity default; a parameter with its own
            // time axis snaps on that axis.
            let time = engine.resolve_parameter_time(parameter.as_deref(), time, reference_time);
            // Content revised in place under the same instant (a push-fed
            // alert set) must not hit a stale entry: the engine's content
            // version is part of every rendered/meta-tile key.
            let content_version = engine.content_version();

            // Build cache key
            let cache_key = CacheKey {
                layer: params.layer.clone(),
                style: params.style.clone(),
                // Quality included; the meta-tile key below needs none, its
                // tiles are RGBA encoded per request.
                format,
                crs: params.crs.clone(),
                // For a projected output CRS the rendered pixels are laid out
                // over the projected-metres bbox (carried in `output_crs`), not
                // the WGS84 envelope in `params.bbox` — two requests with
                // different projected bboxes can share an envelope, so key on the
                // metres to avoid serving one's tile for the other (#267 review).
                bbox: match &params.output_crs {
                    OutputCrs::Projected { bbox, .. } => ds_render::quantize_bbox(bbox),
                    _ => ds_render::quantize_bbox(&params.bbox),
                },
                width: params.width,
                height: params.height,
                time,
                // The parameter settled above (also folded into
                // `style_parameter` below), so the rendered cache
                // distinguishes parameters.
                parameter: parameter.clone(),
                z: params.elevation.map(ds_render::quantize_z),
                // The forecast run pinned via the `reference_time` dimension
                // (None ⇒ latest), so runs don't collide in the rendered cache.
                reference_time,
                content_version,
                // TRANSPARENT=FALSE / BGCOLOR (and JPEG) change the encoded
                // bytes; the meta-tile key below stays background-free, so
                // opaque and transparent views share cached RGBA tiles.
                background: params.background,
            };

            let cache_control =
                cache_control_value(has_explicit_time && time.is_some(), content_version);
            // Read If-None-Match into an owned String so it survives the move
            // into spawn_blocking and the cache-hit/miss branches below.
            let if_none_match = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string);

            // Cache lookup runs BEFORE the If-None-Match check. The ETag is
            // content-derived (see `CachedRendered::new`), so a key-derived
            // 304 short-circuit would be wrong: it would return 304 for any
            // request matching the cache key, even after a server-side fix
            // produces different pixels. Mirror the MVT path in
            // `render_vector_tile` (the bug #145 fixed for raster tiles).
            // The render-latency clock (#466) starts at this lookup, so hits,
            // assembled views and cold renders each report their own tail.
            let render_start = std::time::Instant::now();
            if let Some(cached) = state.rendered_cache.get(&cache_key) {
                if let Some(ref inm) = if_none_match {
                    if ds_render::etag_matches(inm, cached.etag()) {
                        // 304 from the cache-HIT branch. The `x-cache: HIT`
                        // header lets the regression test (and curious
                        // clients) distinguish this from a post-render
                        // MISS→304, which the handler also serves.
                        return Ok(axum::response::Response::builder()
                            .status(StatusCode::NOT_MODIFIED)
                            .header(header::ETAG, cached.etag())
                            .header(header::CACHE_CONTROL, cache_control)
                            .header(header::HeaderName::from_static("x-cache"), "HIT")
                            .extension(RenderTiming::since(
                                &collection_id,
                                RenderOutcome::Hit,
                                render_start,
                            ))
                            .body(axum::body::Body::empty())
                            .unwrap()
                            .into_response());
                    }
                }
                return Ok(axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, content_type)
                    .header(header::ETAG, cached.etag())
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    )
                    .header(header::HeaderName::from_static("x-cache"), "HIT")
                    .extension(RenderTiming::since(
                        &collection_id,
                        RenderOutcome::Hit,
                        render_start,
                    ))
                    .body(axum::body::Body::from(cached.into_bytes()))
                    .unwrap()
                    .into_response());
            }

            // Acquire render semaphore (with timeout to shed load under
            // pressure). A composite holds one value plane per band.
            let t_sem = std::time::Instant::now();
            let (job, memory_permit) = ds_executor::RenderJob::acquire_raster_planes(
                state.render_semaphore.clone(),
                params.width,
                params.height,
                paint.planes(),
            )
            .await
            .map_err(WmsError::from)?;
            let worker_memory = memory_permit.clone();

            // Admission wait: the `queue` render phase (#147) and the slow
            // log's `sem_wait_ms`.
            let queue_wait = t_sem.elapsed();
            let sem_wait_ms = queue_wait.as_millis() as u64;

            // Render on a blocking thread
            let engine = engine.clone();
            let bbox = params.bbox;
            let width = params.width;
            let height = params.height;
            // `time` (latest-resolved above) is `Copy`; it flows into both the
            // direct and meta-tile render closures below.
            let output_crs = params.output_crs.clone();
            let background = params.background;
            let elevation = params.elevation;
            // `reference_time` (resolved to a concrete run above, #521) is
            // `Copy`; it flows into both the direct and meta-tile render
            // closures below.
            let z_q = elevation.map(ds_render::quantize_z);
            let layer = params.layer.clone();
            // Key meta-tiles on the *resolved* style name, not the raw STYLES
            // param: `STYLES=` (empty → default) and `STYLES=default` resolve to
            // the same StyleInfo, so they must share cached tiles.
            let style = style_name;
            let rendered_cache = state.rendered_cache.clone();
            let tile_cache = state.tile_cache.clone();

            // Layer parameter (from "collection/param") takes priority over
            // style parameter; for a composite it is the composite's name.
            let style_parameter = parameter;

            // Spans spawn_blocking *dispatch* + execution, so `render_ms` includes
            // any wait for a free blocking-pool thread (itself a useful signal: if
            // render_ms greatly exceeds the internal phase sum
            // tile_render_ms+assemble_ms+encode_ms, the gap is scheduling latency).
            let t_render = std::time::Instant::now();
            let render_outcome = job
                .run(
                    move || -> Result<(Option<Vec<u8>>, RenderPath), DataServerError> {
                        let _memory_permit = worker_memory;

                        // A composite's bands, in the plane order its spec reads.
                        let bands: Vec<&str> = match &paint {
                            Paint::Composite(spec) => {
                                spec.parameters.iter().map(String::as_str).collect()
                            }
                            Paint::Colormap(_) => Vec::new(),
                        };
                        // Bands that share no scan resolve no time (#819):
                        // nothing to draw, and nothing to cache under a key
                        // that names no timestep.
                        if matches!(paint, Paint::Composite(_)) && time.is_none() {
                            return Ok((None, RenderPath::Direct(RenderPhases::default())));
                        }

                        // Direct single-shot render: one get_raster_tile →
                        // colorize → encode, or a composite's bands → compose →
                        // encode.
                        let direct = || {
                            let mut phases = RenderPhases::default();
                            let engine_start = std::time::Instant::now();
                            let bytes = match &paint {
                                Paint::Colormap(colormap) => {
                                    let tile = engine.get_raster_tile(
                                        bbox,
                                        width,
                                        height,
                                        time,
                                        &output_crs,
                                        style_parameter.as_deref(),
                                        elevation,
                                        reference_time,
                                    )?;
                                    phases.add(RenderPhase::Engine, engine_start.elapsed());
                                    // If every pixel is nodata, skip colorization + encoding entirely.
                                    if tile.is_empty() {
                                        return Ok((None, phases));
                                    }
                                    let encode_start = std::time::Instant::now();
                                    let bytes = ds_render::render_tile_with_background(
                                        &tile,
                                        colormap.as_ref(),
                                        format,
                                        background,
                                    )?;
                                    phases.add(RenderPhase::Encode, encode_start.elapsed());
                                    bytes
                                }
                                Paint::Composite(spec) => {
                                    // Every band from the one timestep the
                                    // cache key names (#507).
                                    let tiles = engine.get_raster_tiles(
                                        bbox,
                                        width,
                                        height,
                                        time,
                                        &output_crs,
                                        &bands,
                                        elevation,
                                        reference_time,
                                    )?;
                                    phases.add(RenderPhase::Engine, engine_start.elapsed());
                                    let encode_start = std::time::Instant::now();
                                    let Some(bytes) = ds_render::render_composite_tiles(
                                        &tiles, spec, format, background,
                                    )?
                                    else {
                                        return Ok((None, phases));
                                    };
                                    phases.add(RenderPhase::Encode, encode_start.elapsed());
                                    bytes
                                }
                            };
                            Ok::<_, DataServerError>((Some(bytes), phases))
                        };

                        // Supported projected CRSs: cache 256×256 meta-tiles and
                        // resample to the exact viewport (#202). The expensive
                        // per-tile work is cached and reused across overlapping
                        // fullscreen views; geographic requests render directly. A zero-byte
                        // tile cache (`metatile_cache_mb = 0`) is the kill switch:
                        // it bypasses meta-tiling so an operator can revert to the
                        // direct path via config reload, no redeploy.
                        if output_crs != OutputCrs::Wgs84 && tile_cache.capacity() > 0 {
                            let prefix = ds_render::TileKeyPrefix {
                                layer,
                                parameter: style_parameter.clone(),
                                style,
                                time,
                                z: z_q,
                                reference_time,
                                content_version,
                            };
                            // Projected output retains its exact metre rectangle;
                            // the helper supplies each tile's own OutputCrs and
                            // WGS84 source-read envelope to the engine. A
                            // composite's tiles cache its composed RGBA.
                            let outcome = match &paint {
                                Paint::Colormap(colormap) => ds_render::render_metatiled(
                                    bbox,
                                    &output_crs,
                                    width,
                                    height,
                                    &prefix,
                                    colormap.as_ref(),
                                    format,
                                    background,
                                    tile_cache.as_ref(),
                                    |tbbox, tw, th, tile_output| {
                                        engine.get_raster_tile(
                                            tbbox,
                                            tw,
                                            th,
                                            time,
                                            tile_output,
                                            style_parameter.as_deref(),
                                            elevation,
                                            reference_time,
                                        )
                                    },
                                )?,
                                Paint::Composite(spec) => ds_render::render_metatiled_composite(
                                    bbox,
                                    &output_crs,
                                    width,
                                    height,
                                    &prefix,
                                    spec,
                                    format,
                                    background,
                                    tile_cache.as_ref(),
                                    |tbbox, tw, th, tile_output| {
                                        engine.get_raster_tiles(
                                            tbbox,
                                            tw,
                                            th,
                                            time,
                                            tile_output,
                                            &bands,
                                            elevation,
                                            reference_time,
                                        )
                                    },
                                )?,
                            };
                            match outcome {
                                ds_render::MetaTile::Image { bytes, stats } => {
                                    Ok((Some(bytes), RenderPath::Meta(stats)))
                                }
                                ds_render::MetaTile::Empty { stats } => {
                                    Ok((None, RenderPath::MetaEmpty(stats)))
                                }
                                ds_render::MetaTile::Fallback => {
                                    direct().map(|(o, phases)| (o, RenderPath::Fallback(phases)))
                                }
                            }
                        } else {
                            direct().map(|(o, phases)| (o, RenderPath::Direct(phases)))
                        }
                    },
                )
                .await
                .map_err(WmsError::from)?;
            let render_ms = t_render.elapsed().as_millis() as u64;

            // Split the render outcome: bytes flow into the existing response
            // match below; the path + timing are logged for slow renders so prod
            // pinpoints the cost (queue wait vs tile render vs assemble vs encode).
            // `render_path` is irrelevant on error (the log is gated on success).
            // `render_path` is `None` on error (no fabricated placeholder); the
            // slow-log is gated on success anyway, so it's never read on error.
            let (render_result, render_path): (
                Result<Option<Vec<u8>>, DataServerError>,
                Option<RenderPath>,
            ) = match render_outcome {
                Ok((bytes, path)) => (Ok(bytes), Some(path)),
                Err(e) => (Err(e), None),
            };
            // `None` exactly for a failed render: an error tile is not a
            // render outcome and records no latency.
            let outcome = render_path.as_ref().map(|path| {
                let mut phases = path.phases();
                phases.add(RenderPhase::Queue, queue_wait);
                (path.outcome(), phases)
            });
            // Only log *successful* slow renders (the 200-status tail we're
            // diagnosing); errors are surfaced by the WmsError render warn arm
            // below. The arms stay distinct so a meta render that fell back to
            // direct isn't conflated with a genuine non-meta render.
            if render_ms >= SLOW_RENDER_LOG_MS && render_result.is_ok() {
                match render_path {
                    Some(RenderPath::Meta(s)) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        tiles = s.tiles,
                        misses = s.misses,
                        tile_loop_ms = s.tile_loop.as_millis() as u64,
                        assemble_ms = s.assemble.as_millis() as u64,
                        encode_ms = s.encode.as_millis() as u64,
                        width = params.width,
                        height = params.height,
                        format = content_type,
                        quality = ?format.quality(),
                        "slow WMS meta-tile render"
                    ),
                    Some(RenderPath::MetaEmpty(s)) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        tiles = s.tiles,
                        misses = s.misses,
                        tile_loop_ms = s.tile_loop.as_millis() as u64,
                        width = params.width,
                        height = params.height,
                        format = content_type,
                        quality = ?format.quality(),
                        "slow WMS meta-tile render (all nodata)"
                    ),
                    Some(RenderPath::Fallback(_)) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        width = params.width,
                        height = params.height,
                        format = content_type,
                        quality = ?format.quality(),
                        "slow WMS render (meta-tiling fell back to direct)"
                    ),
                    // `Direct` covers geographic output and any request
                    // with meta-tiling disabled (metatile_cache_mb
                    // = 0), so the label stays generic rather than claiming a CRS.
                    Some(RenderPath::Direct(_)) => tracing::info!(
                        layer = %params.layer,
                        sem_wait_ms,
                        render_ms,
                        width = params.width,
                        height = params.height,
                        format = content_type,
                        quality = ?format.quality(),
                        "slow WMS direct render"
                    ),
                    None => {}
                }
            }

            // The EMPTY and ERROR fast paths skip the format-aware encoder and
            // emit PNG bytes directly. Track the *actual* Content-Type per
            // branch so the header never lies about the payload (#162). Wrap
            // every branch in `CachedRendered` so the response ETag is
            // FNV-1a over the actual bytes — different pixels, different
            // ETag — regardless of which exit we take (#145).
            // Each arm produces a `CachedRendered` ready to serve. Only the
            // populated `Ok(Some(_))` path inserts into the rendered cache;
            // the EMPTY and ERROR fast-paths intentionally don't (their
            // bytes are deterministic for fixed dimensions and the
            // engine error case shouldn't poison the cache).
            let (cached, x_cache, response_content_type) = match render_result {
                Ok(Some(bytes)) => {
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(bytes));
                    rendered_cache.insert(cache_key, cached.clone());
                    (cached, "MISS", content_type)
                }
                Ok(None) => {
                    // Empty tile: a transparent PNG — or, for opaque output, a
                    // solid background one — encoded once per (w,h,background)
                    // and shared across WMS/Maps/Tiles (#171). Not inserted into
                    // the rendered cache — the shared empty-tile cache already
                    // serves the deterministic empty response.
                    let cached =
                        ds_render::background_tile(params.width, params.height, params.background)
                            .map_err(|e| {
                                WmsError::Internal(format!("Failed to encode empty tile: {e}"))
                            })?;
                    (cached, "EMPTY", "image/png")
                }
                Err(
                    e @ (DataServerError::ResourceExhausted | DataServerError::DeadlineExceeded),
                ) => {
                    return Err(WmsError::ServiceUnavailable(e.to_string()));
                }
                Err(e) => {
                    tracing::warn!("WMS render error for layer '{}': {e}", params.layer);
                    let png = render_error_tile(params.width, params.height, params.background)?;
                    let cached = ds_render::CachedRendered::new(bytes::Bytes::from(png));
                    (cached, "ERROR", "image/png")
                }
            };
            // A failed render is transient: never pin its error tile for a
            // day under an explicit TIME.
            let cache_control = if x_cache == "ERROR" {
                cache_control_value(false, content_version)
            } else {
                cache_control
            };

            // Now that we have the content-derived ETag, do the
            // `If-None-Match` comparison. Same flow as `render_vector_tile`
            // in api-tiles: cache lookup → revalidate against cached ETag,
            // miss → encode → revalidate against fresh ETag.
            let not_modified = if_none_match
                .as_deref()
                .is_some_and(|inm| ds_render::etag_matches(inm, cached.etag()));
            let mut response = if not_modified {
                // 304 from the post-render branch. Forward the same
                // `x_cache` label the 200 response would carry — `"MISS"`,
                // `"EMPTY"`, or `"ERROR"` — so revalidations look the
                // same on dashboards as initial fetches. A client
                // revalidating a cached transparent-tile response sees
                // `304 x-cache: EMPTY`, not a misleading `MISS`.
                axum::response::Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, cached.etag())
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(header::HeaderName::from_static("x-cache"), x_cache)
                    .body(axum::body::Body::empty())
                    .unwrap()
                    .into_response()
            } else {
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, response_content_type)
                    .header(header::ETAG, cached.etag())
                    .header(header::CACHE_CONTROL, cache_control)
                    .header(
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    )
                    .header(header::HeaderName::from_static("x-cache"), x_cache)
                    .body(axum::body::Body::from(cached.into_bytes()))
                    .unwrap()
                    .into_response()
            };
            if let Some((outcome, phases)) = outcome {
                response.extensions_mut().insert(
                    RenderTiming::since(&collection_id, outcome, render_start).with_phases(phases),
                );
            }
            Ok(response)
        }
        WmsRequestType::GetLegendGraphic => {
            let layer_name = query
                .layers
                .as_deref()
                .or(query.layer.as_deref())
                .ok_or(WmsError::missing_parameter("LAYER"))?;

            // Also accept singular STYLE= — SLD standard for GetLegendGraphic (#165).
            let style_name = query
                .styles
                .as_deref()
                .or(query.style.as_deref())
                .unwrap_or("default");
            let style_name = if style_name.is_empty() {
                "default"
            } else {
                style_name
            };

            // An RGB composite layer's legend is its channel list (#819).
            let legend_engine = state
                .engines
                .get(layer_name.split('/').next().unwrap_or(layer_name));
            if let Some((engine, spec)) = legend_engine.and_then(|engine| {
                composite_layer(engine.as_ref(), layer_name).map(|s| (engine, s))
            }) {
                return composite_legend(&query, layer_name, style_name, engine.as_ref(), spec)
                    .await;
            }

            // Support "collection/parameter" layer names for legend. Resolve
            // the FULL layer key first, exactly as GetMap does: a
            // "{collection}/{param}" layer carries its own style map (the
            // parameter's colormap from `[[wms.parameters]]`, a bundle
            // parameter entry, or a built-in default), and the collection map
            // only has the collection default. Falling straight through to the
            // collection made the legend describe a different palette than the
            // GetMap of the same LAYER rendered.
            let legend_collection_id = layer_name.split('/').next().unwrap_or(layer_name);
            let layer_styles = state
                .styles
                .get(layer_name)
                .or_else(|| state.styles.get(legend_collection_id))
                .ok_or_else(|| WmsError::layer_not_found(layer_name))?;

            let style_info = layer_styles.get(style_name).ok_or_else(|| {
                WmsError::StyleNotDefined(format!(
                    "Style '{style_name}' not defined for layer '{layer_name}'"
                ))
            })?;

            let format = crate::params::parse_legend_format(query.format.as_deref())?;

            // Resolve the parameter + unit the legend describes from the
            // engine's raster metadata (#371). Match GetMap precedence: the
            // "collection/param" layer segment, then the style's configured
            // parameter, then the engine's default. Resolve the unit only
            // after selecting the parameter.
            // Both feed the rendered legend's title and the JSON legend, so the
            // two representations describe the same thing.
            let info = state
                .engines
                .get(legend_collection_id)
                .map(|e| e.raster_info_shared());
            let param = layer_name
                .split('/')
                .nth(1)
                .map(str::to_string)
                .or_else(|| style_info.parameter.clone())
                .or_else(|| {
                    info.as_ref()
                        .map(|i| i.parameter.clone())
                        .filter(|p| !p.is_empty())
                });
            let unit = info
                .as_ref()
                .and_then(|i| i.parameter_unit(param.as_deref()))
                .map(str::to_string);

            // Machine-readable legend: palette stops + range, for clients that
            // draw their own legend.
            let format = match format {
                crate::params::LegendFormat::Json => {
                    let body =
                        ds_render::legend_json(style_info, param.as_deref(), unit.as_deref());
                    let mut response = axum::Json(body).into_response();
                    let headers = response.headers_mut();
                    headers.insert(
                        header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static(ds_render::LEGEND_CACHE_CONTROL),
                    );
                    headers.insert(
                        header::HeaderName::from_static("x-content-type-options"),
                        axum::http::HeaderValue::from_static("nosniff"),
                    );
                    return Ok(response);
                }
                crate::params::LegendFormat::Image(format) => format,
            };

            let (width, height) = legend_size(&query);

            let colormap = style_info.colormap.clone();
            let min = style_info.min;
            let max = style_info.max;

            // Title shared with the Maps/Tiles legend endpoints so the three
            // services label the same style identically.
            let title = ds_render::legend_title(style_info, param.as_deref(), unit.as_deref());

            let legend_bytes = tokio::task::spawn_blocking(move || {
                ds_render::render_legend(
                    colormap.as_ref(),
                    min,
                    max,
                    width,
                    height,
                    format,
                    title.as_deref(),
                )
            })
            .await
            .map_err(|e| WmsError::Internal(format!("Legend render failed: {e}")))?
            .map_err(|e| WmsError::Internal(format!("Legend render error: {e}")))?;

            Ok((
                [
                    (header::CONTENT_TYPE, format.content_type()),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff",
                    ),
                    // Legends are static — cache for 24h
                    (header::CACHE_CONTROL, ds_render::LEGEND_CACHE_CONTROL),
                ],
                legend_bytes,
            )
                .into_response())
        }
    }
}

/// The legend image size a GetLegendGraphic asks for. The default fits the
/// value-tick labels + title (#371); a client can still request a smaller
/// thumbnail, where the renderer degrades to a bare swatch.
fn legend_size(query: &WmsQuery) -> (u32, u32) {
    let width: u32 = query
        .width
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ds_render::LEGEND_DEFAULT_WIDTH);
    let height: u32 = query
        .height
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ds_render::LEGEND_DEFAULT_HEIGHT);
    (width.clamp(1, 512), height.clamp(1, 1024))
}

/// GetLegendGraphic for an RGB composite layer (#819): the channel list,
/// each channel's bands, range, gamma and unit, with no colour bar. The
/// units are the bands' own. Its one style is `default`.
async fn composite_legend(
    query: &WmsQuery,
    layer_name: &str,
    style_name: &str,
    engine: &dyn MapEngine,
    spec: Arc<CompositeSpec>,
) -> Result<axum::response::Response, WmsError> {
    if style_name != ds_render::COMPOSITE_STYLE {
        return Err(WmsError::StyleNotDefined(format!(
            "Style '{style_name}' not defined for layer '{layer_name}'"
        )));
    }
    let format = crate::params::parse_legend_format(query.format.as_deref())?;
    let info = engine.raster_info_shared();
    let units: Vec<Option<String>> = ds_render::composite_units(&spec, &info)
        .into_iter()
        .map(|unit| unit.map(str::to_string))
        .collect();
    let format = match format {
        crate::params::LegendFormat::Json => {
            let units: Vec<Option<&str>> = units.iter().map(Option::as_deref).collect();
            let mut response =
                axum::Json(ds_render::composite_legend_json(&spec, &units)).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static(ds_render::LEGEND_CACHE_CONTROL),
            );
            headers.insert(
                header::HeaderName::from_static("x-content-type-options"),
                axum::http::HeaderValue::from_static("nosniff"),
            );
            return Ok(response);
        }
        crate::params::LegendFormat::Image(format) => format,
    };
    let (width, height) = legend_size(query);
    let legend_bytes = tokio::task::spawn_blocking(move || {
        let units: Vec<Option<&str>> = units.iter().map(Option::as_deref).collect();
        ds_render::render_composite_legend(&spec, &units, width, height, format)
    })
    .await
    .map_err(|e| WmsError::Internal(format!("Legend render failed: {e}")))?
    .map_err(|e| WmsError::Internal(format!("Legend render error: {e}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, format.content_type()),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
            (header::CACHE_CONTROL, ds_render::LEGEND_CACHE_CONTROL),
        ],
        legend_bytes,
    )
        .into_response())
}

impl From<ds_executor::ExecutionError> for WmsError {
    fn from(error: ds_executor::ExecutionError) -> Self {
        match error {
            ds_executor::ExecutionError::Task(e) => Self::Internal(e.to_string()),
            other => Self::ServiceUnavailable(other.to_string()),
        }
    }
}
