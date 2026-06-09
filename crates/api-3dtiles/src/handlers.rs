//! HTTP handlers for the 3D Tiles API.
//!
//! Serves OGC 3D Tiles from any collection implementing `VolumeEngine`, in three
//! representations (selected by `?representation=`):
//! - `GET /collections/{id}/tileset.json` — the tileset (bounding region from
//!   `VolumeInfo`; content points at `.pnts` for `points`, or `.glb` plus an
//!   antenna-ECEF `transform` for the `isosurface`/`echotop` mesh products).
//! - `GET /collections/{id}/content.pnts` — the point-cloud tile.
//! - `GET /collections/{id}/content.glb` — a mesh tile (`isosurface` shell, the
//!   default, or `echotop` columns). The mesh products take a `resolution`
//!   (`low`/`med`/`high`) detail tier; the point cloud is native-resolution.
//!
//! All content types are sampled on a blocking thread (bounded by the shared
//! render semaphore) and encoded by `ds-3dtiles`.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use chrono::{DateTime, Utc};
use ds_core::config::CollectionConfig;
use ds_core::geo::geodetic_to_ecef;
use ds_core::volume::VolumeEngine;
use ds_render::{ColorMap, ColorStop, LutColorMap};
use serde::Deserialize;
use serde_json::json;

use crate::error::Tiles3dError;

/// Default isosurface threshold (dBZ) when a request names none — a light-rain
/// reflectivity shell.
const ISOSURFACE_DEFAULT_THRESHOLD: f64 = 20.0;
/// Default echo-top reflectivity (dBZ) — the standard echo-top value.
const ECHO_TOP_DEFAULT_THRESHOLD: f64 = 18.0;

/// Voxel-grid resolution tier for the glTF mesh products (isosurface, echo-top):
/// `[radius, azimuth, height]`. Azimuth stays 360 (the native 1° ray spacing) at
/// every tier; radius/height trade detail for compute. The cell count drives both
/// the resample and the mesher, so the first-request cost scales with it (repeats
/// are ETag-cached). `High` ≈ the native 500 m range bins; `Low` is the engine's
/// historical default. All tiers stay under the engine's `MAX_VOXELS` cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    /// `[128, 360, 48]` ≈ 2.2 M cells — fast, coarse radial (~2 km bins).
    Low,
    /// `[256, 360, 56]` ≈ 5.2 M cells — the balanced default.
    Med,
    /// `[512, 360, 64]` ≈ 11.8 M cells — full radial detail; ~12 s first compute.
    High,
}

impl Resolution {
    /// Parse the `resolution` query value (`None`/empty → `Med`).
    fn parse(s: Option<&str>) -> Result<Self, Tiles3dError> {
        match s.map(str::trim) {
            None | Some("") | Some("med") => Ok(Self::Med),
            Some("low") => Ok(Self::Low),
            Some("high") => Ok(Self::High),
            Some(other) => Err(Tiles3dError::BadRequest(format!(
                "unknown resolution '{other}' (expected 'low', 'med', or 'high')"
            ))),
        }
    }

    /// The voxel-grid dimensions `[radius, azimuth, height]` for this tier.
    fn dims(self) -> [usize; 3] {
        match self {
            Self::Low => [128, 360, 48],
            Self::Med => [256, 360, 56],
            Self::High => [512, 360, 64],
        }
    }

    /// The canonical query-string token (round-trips through `parse`).
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Med => "med",
            Self::High => "high",
        }
    }
}

/// Reflectivity range (dBZ) sampled for the viewer's colour-scale legend. The
/// shared point colormap is sampled across this span so the legend reads the
/// same colours the points show. 0–70 dBZ is the conventional radar display
/// range — it covers the whole coloured ramp (the radar colormap is ~black
/// below a few dBZ, so a lower bound just adds an invisible band).
const LEGEND_MIN_DBZ: f64 = 0.0;
const LEGEND_MAX_DBZ: f64 = 70.0;
const LEGEND_STOPS: usize = 24;

/// Sample `colormap` into `(value, "#rrggbb")` stops across `[min, max]` for the
/// viewer legend. RGB only — `.pnts` colours are opaque, so alpha is dropped.
fn legend_stops(colormap: &dyn ColorMap, min: f64, max: f64, n: usize) -> Vec<serde_json::Value> {
    (0..n)
        .map(|i| {
            // Guard n == 1: `i/(n-1)` would be `0/0 = NaN`, which serde emits as
            // `null`. (LEGEND_STOPS is 24 today, but keep the invariant explicit.)
            let v = min
                + if n <= 1 {
                    0.0
                } else {
                    (max - min) * (i as f64) / ((n - 1) as f64)
                };
            let c = colormap.color(Some(v));
            json!({
                "value": (v * 10.0).round() / 10.0,
                "color": format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]),
            })
        })
        .collect()
}

/// The blue→red **height** colour ramp for the echo-top product (stops at height
/// values, 0–15 km) — a builtin colormap's stops are in its own units, so they
/// collapse over a height range; build it explicitly. Cheap to construct.
fn echo_top_height_colormap() -> LutColorMap {
    let stops = [
        (0.0_f64, [40u8, 70, 200, 255]),
        (3_000.0, [0, 200, 220, 255]),
        (6_000.0, [40, 200, 80, 255]),
        (9_000.0, [240, 230, 60, 255]),
        (12_000.0, [240, 140, 40, 255]),
        (15_000.0, [220, 40, 40, 255]),
    ]
    .map(|(value, color)| ColorStop { value, color });
    LutColorMap::from_stops(&stops, 0.0, 15_000.0)
}

/// Shared state for the 3D Tiles API. Wrapped in `ArcSwap` for lock-free reads
/// and atomic swap on config reload.
#[derive(Clone)]
pub struct TilesState3d {
    /// Collections that can produce volumetric point clouds, keyed by id.
    pub volume_engines: HashMap<String, Arc<dyn VolumeEngine>>,
    /// Per-collection config (title/description/etc.), keyed by id.
    pub collections: HashMap<String, CollectionConfig>,
    /// Colour ramp applied to point values when encoding `.pnts` RGB. v1 uses
    /// one shared ramp (reflectivity); per-collection/per-quantity colormaps
    /// from config are a follow-up.
    pub colormap: Arc<dyn ColorMap>,
    /// Bounds concurrent sampling/encoding, shared with the raster render path.
    pub render_semaphore: Arc<tokio::sync::Semaphore>,
    /// Public base URL for absolute links.
    pub base_url: String,
}

pub type AppState = Arc<ArcSwap<TilesState3d>>;

/// Look up a collection's volume engine + config, 404 if absent.
fn lookup<'a>(
    state: &'a TilesState3d,
    id: &str,
) -> Result<(&'a Arc<dyn VolumeEngine>, &'a CollectionConfig), Tiles3dError> {
    let engine = state
        .volume_engines
        .get(id)
        .ok_or_else(|| Tiles3dError::NotFound(format!("Collection '{id}' not found")))?;
    let config = state
        .collections
        .get(id)
        .ok_or_else(|| Tiles3dError::Internal(format!("config missing for '{id}'")))?;
    Ok((engine, config))
}

#[derive(Debug, Deserialize)]
pub struct TilesetParams {
    /// Quantity to render (`None` → the collection default).
    pub quantity: Option<String>,
    /// Valid time (RFC 3339). `None` → latest.
    pub datetime: Option<String>,
    /// Which 3D Tiles representation: `points` (the `.pnts` point cloud,
    /// default) or `isosurface` (a glTF `.glb` reflectivity-shell mesh).
    pub representation: Option<String>,
    /// `points` only: drop points below this physical value (e.g. a dBZ floor);
    /// carried into the tileset's `content.uri` so the `.pnts` fetch applies it.
    pub min_value: Option<f64>,
    /// `isosurface` only: the iso-value (e.g. dBZ) of the shell; carried into
    /// the `.glb` `content.uri`. `None` → [`ISOSURFACE_DEFAULT_THRESHOLD`].
    pub threshold: Option<f64>,
    /// Mesh products only (`isosurface`/`echotop`): voxel-grid detail tier
    /// (`low`/`med`/`high`); carried into the `.glb` `content.uri`. `None` →
    /// `med`. Ignored for `points` (the cloud is native-resolution).
    pub resolution: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContentParams {
    pub quantity: Option<String>,
    pub datetime: Option<String>,
    /// Drop points below this physical value (e.g. a dBZ floor).
    pub min_value: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct GlbContentParams {
    pub quantity: Option<String>,
    pub datetime: Option<String>,
    /// `isosurface`: iso-value of the shell; `echotop`: the reflectivity floor
    /// for the echo top. `None` → [`ISOSURFACE_DEFAULT_THRESHOLD`] /
    /// [`ECHO_TOP_DEFAULT_THRESHOLD`].
    pub threshold: Option<f64>,
    /// Which glTF product: `isosurface` (default) or `echotop`.
    pub representation: Option<String>,
    /// Voxel-grid detail tier (`low`/`med`/`high`). `None` → `med`.
    pub resolution: Option<String>,
}

/// The 3D Tiles representations of a volume — all valid OGC 3D Tiles tilesets,
/// differing in content (`.pnts` point cloud vs glTF `.glb` mesh) and, for the
/// two mesh products, in what the mesh *is*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Representation {
    /// `.pnts` point cloud — one point per echo cell.
    Points,
    /// `.glb` closed reflectivity shell at a threshold.
    Isosurface,
    /// `.glb` extruded echo-top columns (ground → echo-top height, by column).
    EchoTop,
}

impl Representation {
    /// Parse the `representation` query value (`None`/empty → `Points`).
    fn parse(s: Option<&str>) -> Result<Self, Tiles3dError> {
        match s.map(str::trim) {
            None | Some("") | Some("points") => Ok(Self::Points),
            Some("isosurface") => Ok(Self::Isosurface),
            Some("echotop") => Ok(Self::EchoTop),
            Some(other) => Err(Tiles3dError::BadRequest(format!(
                "unknown representation '{other}' (expected 'points', 'isosurface', or 'echotop')"
            ))),
        }
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved set passes
/// through). The `VolumeEngine` trait places no charset constraint on quantity
/// names, so a name containing `&`/`+`/`#`/`%`/… must not break the tileset's
/// `content.uri` — the same class of bug as the `+hh:mm` datetime offset.
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>, Tiles3dError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| Tiles3dError::BadRequest(format!("invalid datetime: {s:?}")))
}

/// Strong content-derived ETag — quoted hex with no `W/` prefix (the bytes are
/// exact, so byte-equal responses are equivalent per RFC 7232 §2.1). FNV-1a
/// 64-bit — stable across Rust versions and instances (unlike `DefaultHasher`),
/// so a toolchain upgrade or a mixed-version fleet doesn't silently invalidate
/// ETags.
fn etag_of(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

/// Build a binary content response with strong-ETag conditional handling: a
/// matching `If-None-Match` (or `*`) yields 304, otherwise 200 with the bytes.
///
/// Note the 304 saves the *network transfer* but not the recompute — the
/// caller already sampled + encoded + hashed to obtain `etag` (the
/// `If-None-Match` value can't be trusted to match without recomputing the
/// content). A CPU-cheap 304 needs an ETag cache keyed by the request params +
/// a data-version — a follow-up. RFC 7232 §3.2: `If-None-Match` may be `*` or a
/// comma-separated list.
fn binary_response(
    headers: &HeaderMap,
    content_type: &'static str,
    etag: &str,
    bytes: Vec<u8>,
) -> Response {
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| v == "*" || v.split(',').any(|t| t.trim() == etag));
    if not_modified {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "public, max-age=60"),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        bytes,
    )
        .into_response()
}

/// The bundled CesiumJS viewer page (collection + quantity + representation
/// picker), baked into the binary and served at `GET /3dtiles/viewer`.
const VIEWER_HTML: &str = include_str!("../viewer/index.html");

/// `GET /viewer` — interactive CesiumJS viewer. It calls this same API
/// (same-origin by default), so it works on any deployment.
pub async fn get_viewer() -> axum::response::Html<&'static str> {
    axum::response::Html(VIEWER_HTML)
}

// ---------------------------------------------------------------------------
// Tileset
// ---------------------------------------------------------------------------

/// `GET /collections/{id}/tileset.json`
pub async fn get_tileset(
    Path(id): Path<String>,
    Query(params): Query<TilesetParams>,
    State(state): State<AppState>,
) -> Result<Response, Tiles3dError> {
    let state = state.load_full();
    let (engine, _config) = lookup(&state, &id)?;
    let info = engine.volume_info();
    let representation = Representation::parse(params.representation.as_deref())?;

    // Resolve + validate the quantity against what the collection advertises.
    let quantity = match &params.quantity {
        Some(q) => {
            if !info.quantities.iter().any(|(qid, _)| qid == q) {
                return Err(Tiles3dError::BadRequest(format!(
                    "unknown quantity '{q}' for collection '{id}'"
                )));
            }
            q.clone()
        }
        None => info.default_quantity.clone(),
    };
    if quantity.is_empty() {
        return Err(Tiles3dError::NotFound(format!(
            "collection '{id}' has no quantities"
        )));
    }
    let region = info.region.ok_or_else(|| {
        Tiles3dError::NotFound(format!("collection '{id}' has no spatial coverage yet"))
    })?;

    // Carry the resolved quantity (and pinned time, if any) into the content
    // URI so the content fetch is deterministic. Re-format the parsed time as
    // UTC `…Z` — a raw `+hh:mm` offset would be decoded as a space by the
    // client's URL parser and 400 on the fetch.
    let mut query = format!("quantity={}", pct_encode(&quantity));
    if let Some(dt_str) = &params.datetime {
        let dt = parse_datetime(dt_str)?;
        query.push_str(&format!("&datetime={}", dt.format("%Y-%m-%dT%H:%M:%SZ")));
    }

    let tileset = match representation {
        Representation::Points => {
            if let Some(min) = params.min_value {
                // `serde_urlencoded` parses "NaN"/"inf" as a valid f64; a
                // non-finite threshold filters every point → a silently-empty
                // tile. Reject → 400. (f64 Display is URL-safe for a finite
                // value: digits, `.`, `-`.)
                if !min.is_finite() {
                    return Err(Tiles3dError::BadRequest("min_value must be finite".into()));
                }
                query.push_str(&format!("&min_value={min}"));
            }
            let content_uri = format!("content.pnts?{query}");
            ds_3dtiles::tileset_json_for_region(region, &content_uri)
                .map_err(|e| Tiles3dError::Internal(format!("tileset build failed: {e}")))?
        }
        Representation::Isosurface | Representation::EchoTop => {
            // Both glTF products need the voxel grid. `voxel_grid` couples the
            // capability with the origin, so such a collection always has the
            // origin — no separate `None` → 500 path.
            let caps = info.voxel_grid.as_ref().ok_or_else(|| {
                let name = if representation == Representation::EchoTop {
                    "echo-top"
                } else {
                    "isosurface"
                };
                Tiles3dError::BadRequest(format!(
                    "collection '{id}' does not support the {name} representation"
                ))
            })?;
            // The glTF `.glb` content has no embedded origin (unlike `.pnts`
            // `RTC_CENTER`), so the tileset places it via a `transform` = the
            // volume origin (antenna) in ECEF — taken from `VolumeInfo` so we
            // don't sample the grid just to emit the tileset.
            let [olon, olat, oh] = caps.origin;
            let rtc = geodetic_to_ecef(olon, olat, oh);
            if let Some(t) = params.threshold {
                if !t.is_finite() {
                    return Err(Tiles3dError::BadRequest("threshold must be finite".into()));
                }
                query.push_str(&format!("&threshold={t}"));
            }
            // Carry the detail tier so the content fetch matches the tileset.
            // Validate now (a bad value is a 400 here, not a surprise on fetch);
            // emit the canonical token even when the request omitted it, so the
            // `content.uri` is self-describing.
            let resolution = Resolution::parse(params.resolution.as_deref())?;
            query.push_str(&format!("&resolution={}", resolution.as_str()));
            // Tag the mesh product explicitly for *both* (not just echo-top), so
            // the `content.uri` is unambiguous and doesn't depend on the
            // content.glb handler's absent→isosurface default — a future default
            // change there can't then silently repoint old isosurface tilesets.
            query.push_str(match representation {
                Representation::EchoTop => "&representation=echotop",
                _ => "&representation=isosurface",
            });
            let content_uri = format!("content.glb?{query}");
            ds_3dtiles::tileset_json_glb(region, &content_uri, rtc)
                .map_err(|e| Tiles3dError::Internal(format!("tileset build failed: {e}")))?
        }
    };

    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        tileset,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Content (.pnts)
// ---------------------------------------------------------------------------

/// `GET /collections/{id}/content.pnts`
pub async fn get_content(
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<ContentParams>,
    State(state): State<AppState>,
) -> Result<Response, Tiles3dError> {
    let state = state.load_full();
    let (engine, _config) = lookup(&state, &id)?;

    let time = params.datetime.as_deref().map(parse_datetime).transpose()?;
    let quantity = params.quantity.clone();
    let min_value = params.min_value;
    // A non-finite threshold (parsed from "NaN"/"inf") would drop every point.
    if min_value.is_some_and(|m| !m.is_finite()) {
        return Err(Tiles3dError::BadRequest("min_value must be finite".into()));
    }

    // Sample + encode off the request worker: `read_point_cloud` does blocking
    // HDF5 I/O and a long CPU loop (CLAUDE.md concurrency rules), so bound it
    // with the shared render semaphore and run it on a blocking thread.
    let _permit = state
        .render_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Tiles3dError::Internal("render semaphore closed".into()))?;

    let engine = engine.clone();
    let colormap = state.colormap.clone();
    // Sample, encode, and hash all on the blocking thread (the ETag hash over a
    // multi-MB tile is real CPU — keep it off the async worker too).
    let (bytes, etag) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), Tiles3dError> {
            let cloud = engine.read_point_cloud(quantity.as_deref(), time, min_value, None)?;
            let bytes = ds_3dtiles::encode_pnts(&cloud, colormap.as_ref())
                .map_err(|e| Tiles3dError::Internal(format!("pnts encode failed: {e}")))?;
            let etag = etag_of(&bytes);
            Ok((bytes, etag))
        })
        .await
        .map_err(|e| Tiles3dError::Internal(format!("sample task failed: {e}")))??;

    Ok(binary_response(
        &headers,
        "application/octet-stream",
        &etag,
        bytes,
    ))
}

// ---------------------------------------------------------------------------
// Content (.glb isosurface)
// ---------------------------------------------------------------------------

/// `GET /collections/{id}/content.glb` — a glTF `.glb` mesh of the volume:
/// `representation=isosurface` (default) is a reflectivity shell (marching
/// tetrahedra); `representation=echotop` is extruded echo-top columns. Resampled
/// to a voxel grid and meshed on a blocking thread (bounded by the shared render
/// semaphore), encoded by `ds-3dtiles`.
pub async fn get_content_glb(
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<GlbContentParams>,
    State(state): State<AppState>,
) -> Result<Response, Tiles3dError> {
    let state = state.load_full();
    // `.glb` serves the mesh products. An *absent* representation defaults to
    // the isosurface (the isosurface tileset's `content.uri` omits the param),
    // but an *explicit* `points` is rejected below — the point cloud has its own
    // `content.pnts` route, and a tileset-following client never asks `.glb` for
    // it. (Distinguishing absent from explicit-points is why we don't lean on
    // `Representation::parse`'s `None → Points` default here.)
    let representation = match params.representation.as_deref().map(str::trim) {
        None | Some("") => Representation::Isosurface,
        other => Representation::parse(other)?,
    };
    let product = match representation {
        Representation::Points => {
            return Err(Tiles3dError::BadRequest(
                "use content.pnts for the point-cloud representation".into(),
            ))
        }
        Representation::EchoTop => "echo-top",
        Representation::Isosurface => "isosurface",
    };
    let (engine, _config) = lookup(&state, &id)?;
    let info = engine.volume_info();
    // Reject early if the engine can't produce a voxel grid (avoids a blocking
    // task just to return the trait's "unsupported" → 404).
    if info.voxel_grid.is_none() {
        return Err(Tiles3dError::BadRequest(format!(
            "collection '{id}' does not support the {product} representation"
        )));
    }
    // Validate the quantity against the advertised set *before* the blocking
    // task, like the `.pnts` handler — an unknown name shouldn't trigger a full
    // `read_voxel_grid` (HDF5 open + polar scan) just to be rejected.
    if let Some(q) = &params.quantity {
        if !info.quantities.iter().any(|(qid, _)| qid == q) {
            return Err(Tiles3dError::BadRequest(format!(
                "unknown quantity '{q}' for collection '{id}'"
            )));
        }
    }

    let time = params.datetime.as_deref().map(parse_datetime).transpose()?;
    let quantity = params.quantity.clone();
    // Detail tier (`low`/`med`/`high`, default `med`) → voxel-grid dims, applied
    // to both mesh products. A bad value is a 400 before the blocking task.
    let dims = Resolution::parse(params.resolution.as_deref())?.dims();
    let threshold = params.threshold.unwrap_or(match representation {
        Representation::EchoTop => ECHO_TOP_DEFAULT_THRESHOLD,
        _ => ISOSURFACE_DEFAULT_THRESHOLD,
    });
    if !threshold.is_finite() {
        return Err(Tiles3dError::BadRequest("threshold must be finite".into()));
    }
    // Both products lean on the engine's no-echo floor: the isosurface seals NaN
    // at it; an echo-top column needs its threshold above it (else clear air
    // would count as echo). (v1: the floor is dBZ for every quantity — #350.)
    let floor = f64::from(ds_core::volume::NO_ECHO_FLOOR_DBZ);
    if threshold <= floor {
        return Err(Tiles3dError::BadRequest(format!(
            "threshold must be above the no-echo floor ({floor})"
        )));
    }

    // read_voxel_grid + meshing do blocking HDF5 I/O + a long CPU loop, so bound
    // them with the shared render semaphore and run on a blocking thread (same
    // rule as the `.pnts` path / the raster `get_raster_tile`).
    let _permit = state
        .render_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Tiles3dError::Internal("render semaphore closed".into()))?;

    let engine = engine.clone();
    let colormap = state.colormap.clone(); // reflectivity ramp (isosurface)
    let id_for_err = id.clone();
    let (bytes, etag) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String), Tiles3dError> {
            let result = match representation {
                Representation::EchoTop => {
                    let grid =
                        engine.read_voxel_grid(quantity.as_deref(), time, Some(dims), None)?;
                    ds_3dtiles::encode_echo_top_columns_glb(
                        &grid,
                        threshold,
                        &echo_top_height_colormap(),
                    )
                }
                Representation::Isosurface => {
                    let grid =
                        engine.read_voxel_grid(quantity.as_deref(), time, Some(dims), None)?;
                    // Colour the shell at the threshold (v1: one colormap for all
                    // quantities — #350); seal NaN at the no-echo floor so it
                    // closes into solid blobs (`floor < threshold` guaranteed).
                    let color = colormap.color(Some(threshold));
                    ds_3dtiles::encode_isosurface_glb(&grid, threshold, color, Some(floor))
                }
                // Rejected before the blocking task (see the `product` match).
                Representation::Points => unreachable!("points rejected above"),
            };
            let bytes = result.map_err(|e| match e {
                // No surface / no columns (threshold above all echo) — 404.
                ds_3dtiles::Tiles3dError::Empty => Tiles3dError::NotFound(format!(
                    "no {product} at threshold {threshold} for collection '{id_for_err}'"
                )),
                // Client-driven (threshold too low for this grid) — 400.
                ds_3dtiles::Tiles3dError::TooLarge(_) => Tiles3dError::BadRequest(format!(
                    "threshold {threshold} produces too large a {product}; raise it"
                )),
                // Unreachable (floor < threshold above) — defensive 400. Only
                // the isosurface encoder emits this today, but this `map_err`
                // covers both products, so use `{product}` for consistency.
                ds_3dtiles::Tiles3dError::BackgroundNotBelowThreshold { .. } => {
                    Tiles3dError::BadRequest(format!(
                        "{product} sealing floor is not below the threshold"
                    ))
                }
                other => Tiles3dError::Internal(format!("{product} encode failed: {other}")),
            })?;
            let etag = etag_of(&bytes);
            Ok((bytes, etag))
        })
        .await
        .map_err(|e| Tiles3dError::Internal(format!("sample task failed: {e}")))??;

    // glTF binary media type (CesiumJS also keys off the `.glb` magic).
    Ok(binary_response(&headers, "model/gltf-binary", &etag, bytes))
}

// ---------------------------------------------------------------------------
// Landing / collections
// ---------------------------------------------------------------------------

/// `GET /` — API landing document.
pub async fn landing_page(State(state): State<AppState>) -> Json<serde_json::Value> {
    let state = state.load_full();
    let base = &state.base_url;
    Json(json!({
        "title": "MeteoCore — 3D Tiles",
        "description": "Volumetric weather data as OGC 3D Tiles (radar polar volumes)",
        "links": [
            { "href": format!("{base}/3dtiles/"), "rel": "self", "type": "application/json", "title": "This document" },
            { "href": format!("{base}/3dtiles/collections"), "rel": "data", "type": "application/json", "title": "Collections" },
            { "href": format!("{base}/3dtiles/viewer"), "rel": "alternate", "type": "text/html", "title": "Interactive 3D Tiles viewer" },
        ]
    }))
}

/// `GET /collections` — list 3D-Tiles-capable collections.
pub async fn collections(State(state): State<AppState>) -> Json<serde_json::Value> {
    let state = state.load_full();
    let base = &state.base_url;
    let mut ids: Vec<&String> = state.volume_engines.keys().collect();
    ids.sort();
    let items: Vec<_> = ids
        .iter()
        .map(|id| collection_doc(&state, id, base))
        .collect();
    Json(json!({ "collections": items }))
}

/// `GET /collections/{id}` — one collection document.
pub async fn collection(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, Tiles3dError> {
    let state = state.load_full();
    // 404 if not a volume collection.
    lookup(&state, &id)?;
    Ok(Json(collection_doc(&state, &id, &state.base_url)))
}

fn collection_doc(state: &TilesState3d, id: &str, base: &str) -> serde_json::Value {
    let cfg = state.collections.get(id);
    let title = cfg
        .map(|c| c.title.clone())
        .unwrap_or_else(|| id.to_string());
    let description = cfg.map(|c| c.description.clone()).unwrap_or_default();
    let info = state.volume_engines.get(id).map(|e| e.volume_info());
    let quantities: Vec<_> = info
        .as_ref()
        .map(|i| {
            i.quantities
                .iter()
                .map(|(qid, label)| json!({ "id": qid, "label": label }))
                .collect()
        })
        .unwrap_or_default();
    // Every volume collection serves a point cloud; those whose engine can also
    // produce a voxel grid additionally serve the two glTF mesh products
    // (isosurface, echo-top). The viewer reads this to populate its toggle.
    let supports_mesh = info.as_ref().is_some_and(|i| i.voxel_grid.is_some());
    let mut representations = vec!["points"];
    if supports_mesh {
        representations.push("isosurface");
        representations.push("echotop");
    }
    // A link per representation so a link-following client (not just one that
    // reads `representations` and builds URLs itself) can discover them.
    let mut links = vec![
        json!({ "href": format!("{base}/3dtiles/collections/{id}"), "rel": "self", "type": "application/json", "title": "This document" }),
        json!({ "href": format!("{base}/3dtiles/collections/{id}/tileset.json"), "rel": "3dtiles", "type": "application/json", "title": "3D Tiles tileset (point cloud)" }),
    ];
    if supports_mesh {
        links.push(json!({ "href": format!("{base}/3dtiles/collections/{id}/tileset.json?representation=isosurface"), "rel": "3dtiles", "type": "application/json", "title": "3D Tiles tileset (isosurface mesh)" }));
        links.push(json!({ "href": format!("{base}/3dtiles/collections/{id}/tileset.json?representation=echotop"), "rel": "3dtiles", "type": "application/json", "title": "3D Tiles tileset (echo-top columns)" }));
    }
    // Colour-scale legend for the point cloud: sample the shared reflectivity
    // colormap so the viewer can show a gradient bar whose colours match the
    // points (v1 uses one reflectivity ramp for all quantities — #350).
    let legend = json!({
        "title": "Reflectivity",
        "unit": "dBZ",
        "min": LEGEND_MIN_DBZ,
        "max": LEGEND_MAX_DBZ,
        "stops": legend_stops(state.colormap.as_ref(), LEGEND_MIN_DBZ, LEGEND_MAX_DBZ, LEGEND_STOPS),
    });
    json!({
        "id": id,
        "title": title,
        "description": description,
        "quantities": quantities,
        "representations": representations,
        "legend": legend,
        "links": links,
    })
}

#[cfg(test)]
mod tests {
    use super::pct_encode;

    #[test]
    fn pct_encode_passes_unreserved_and_escapes_specials() {
        assert_eq!(pct_encode("DBZH"), "DBZH");
        assert_eq!(pct_encode("a-b_c.d~e"), "a-b_c.d~e");
        // Chars that would break a query value are escaped.
        assert_eq!(pct_encode("a&b+c#d%e"), "a%26b%2Bc%23d%25e");
        assert_eq!(pct_encode("x y"), "x%20y");
    }
}
