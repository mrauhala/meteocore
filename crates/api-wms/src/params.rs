use chrono::{DateTime, Utc};
use ds_core::map_engine::OutputCrs;
use serde::Deserialize;

use crate::error::WmsError;

/// Output-pixel cap of one GetMap: `WIDTH × HEIGHT` of the returned image.
///
/// It bounds the output only, so passing it guarantees neither of the budgets
/// behind it ("Pixel budgets" in the root CLAUDE.md, #120): engine-geotiff's
/// `reader::MAX_MAP_PIXELS` has the same value but counts native *source*
/// pixels, and render admission charges 32 B per output pixel against
/// `MC_RENDER_MEMORY_MB`, whose 1024 MiB default admits at most 33 554 432.
///
/// Enforced in [`WmsQuery::validate_get_map`]: HTTP 400 `InvalidParameterValue`
/// "WIDTH * HEIGHT (N) exceeds maximum of 64000000". [`MAX_MAP_DIMENSION`]²
/// equals this cap, so the per-side check always fires first today; this one
/// only guards a future per-side increase.
pub const MAX_MAP_PIXELS: u64 = 64_000_000;

/// Output-pixel cap per side (WIDTH or HEIGHT). 8000 chosen so 8000 × 8000
/// equals MAX_MAP_PIXELS — a square at the per-dim cap doesn't trip the
/// pixel cap with a confusing second error. Tripping it is HTTP 400
/// `InvalidParameterValue` "WIDTH and HEIGHT must not exceed 8000".
/// GetCapabilities advertises it as the `<Service>`'s `<MaxWidth>` and
/// `<MaxHeight>` (#1012).
pub const MAX_MAP_DIMENSION: u32 = 8000;

/// Most layers one GetMap may name in `LAYERS`, advertised as the
/// `<Service>`'s `<LayerLimit>`. GetMap renders exactly one layer — it reads
/// only the first name — so raise this only together with layer composition.
/// The handler refuses more only after checking every named layer and
/// style, so an undefined one is `LayerNotDefined` / `StyleNotDefined`.
pub const LAYER_LIMIT: usize = 1;

/// Supported CRS identifiers.
const SUPPORTED_CRS: &[&str] = &["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"];

/// Supported output formats for `GetMap` and `GetLegendGraphic`.
///
/// Single source of truth: capabilities iterates this slice when emitting
/// `<Format>` children, and `parse_image_format` mirrors it. If you add a
/// format here, extend the match in `parse_image_format` to match.
///
/// `image/png` auto-selects an 8-bit indexed-palette encoding ("PNG8") when
/// the rendered image carries ≤256 distinct colours (every colormap layer);
/// the encoder falls back to 32-bit RGBA above that. Content-type is
/// `image/png` either way — no second `FORMAT=` value is needed because the
/// choice is invisible to clients.
pub const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// Parse a WMS `FORMAT=` query parameter into the corresponding `ImageFormat`.
///
/// `None` (parameter omitted) defaults to PNG, matching the WMS 1.3.0
/// convention. JPEG and WebP come at their default quality (85, lossless);
/// GetMap then applies `QUALITY` and the collection's `webp_quality`. Any value outside `SUPPORTED_FORMATS` yields
/// `WmsError::InvalidFormat`. Used by both `validate_get_map` and the
/// `GetLegendGraphic` handler so the two paths can't drift.
pub fn parse_image_format(format: Option<&str>) -> Result<ds_render::ImageFormat, WmsError> {
    match format {
        None | Some("image/png") => Ok(ds_render::ImageFormat::Png),
        Some("image/jpeg") => Ok(ds_render::ImageFormat::JPEG),
        Some("image/webp") => Ok(ds_render::ImageFormat::WEBP),
        Some(other) => Err(WmsError::invalid_format(other)),
    }
}

/// MIME type selecting the machine-readable legend from `GetLegendGraphic`.
pub const LEGEND_JSON_FORMAT: &str = "application/json";

/// What a `GetLegendGraphic` request asked for: a rendered image, or the
/// machine-readable legend description (palette stops + value range) that lets
/// a client draw its own legend.
pub enum LegendFormat {
    Image(ds_render::ImageFormat),
    Json,
}

/// Parse `FORMAT=` for `GetLegendGraphic`.
///
/// `application/json` (case-insensitive) selects the machine-readable legend;
/// every other value — including an omitted one — goes through
/// [`parse_image_format`] unchanged, so image requests and their errors behave
/// exactly as before. `GetMap` keeps using `parse_image_format` directly and
/// therefore still rejects `application/json`.
pub fn parse_legend_format(format: Option<&str>) -> Result<LegendFormat, WmsError> {
    if let Some(f) = format {
        if f.trim().eq_ignore_ascii_case(LEGEND_JSON_FORMAT) {
            return Ok(LegendFormat::Json);
        }
    }
    parse_image_format(format).map(LegendFormat::Image)
}

/// The formats GetFeatureInfo answers in, its `INFO_FORMAT` values, in the
/// order GetCapabilities lists them.
///
/// Single source of truth: capabilities iterates this slice, and
/// [`InfoFormat::parse`] mirrors it.
pub const INFO_FORMATS: &[&str] = &["text/plain", "application/json", "text/html"];

/// A GetFeatureInfo response format (`INFO_FORMAT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoFormat {
    /// `text/plain`: one `name: value` line per field.
    Text,
    /// `application/json`: a GeoJSON FeatureCollection, one Point feature
    /// per queried layer at the sampled position.
    Json,
    /// `text/html`: one table per queried layer.
    Html,
}

impl InfoFormat {
    /// Parse `INFO_FORMAT`. MIME types are case-insensitive, so it is
    /// matched in any case. Omitted or empty is `text/plain`, as GeoServer
    /// and MapServer answer; WMS 1.3.0 makes the parameter mandatory, and
    /// a client that leaves it out still gets a value. Anything else is
    /// `InvalidFormat` (WMS 1.3.0 Table E.1).
    pub fn parse(raw: Option<&str>) -> Result<Self, WmsError> {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(InfoFormat::Text);
        };
        match raw.to_ascii_lowercase().as_str() {
            "text/plain" => Ok(InfoFormat::Text),
            "application/json" => Ok(InfoFormat::Json),
            "text/html" => Ok(InfoFormat::Html),
            _ => Err(WmsError::invalid_info_format(raw)),
        }
    }

    /// The response's `Content-Type`.
    pub fn content_type(self) -> &'static str {
        match self {
            InfoFormat::Text => "text/plain; charset=utf-8",
            InfoFormat::Json => "application/json",
            InfoFormat::Html => "text/html; charset=utf-8",
        }
    }
}

/// Largest legend image `GetLegendGraphic` draws, per side: a larger
/// `WIDTH`/`HEIGHT` is clamped to it.
const MAX_LEGEND_WIDTH: u32 = 512;
const MAX_LEGEND_HEIGHT: u32 = 1024;

/// The size of the legend image `GetLegendGraphic` returns for its `WIDTH`
/// and `HEIGHT` values: [`ds_render::LEGEND_DEFAULT_WIDTH`] ×
/// [`ds_render::LEGEND_DEFAULT_HEIGHT`], large enough for the value-tick
/// labels and the title (#371), for an absent or unparseable side, and every
/// side clamped to 1..=512 × 1..=1024. A client can still ask for a small
/// thumbnail, where the renderer degrades to a bare swatch.
pub fn legend_size(width: Option<&str>, height: Option<&str>) -> (u32, u32) {
    let side = |raw: Option<&str>, default: u32, max: u32| {
        raw.and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(default)
            .clamp(1, max)
    };
    (
        side(width, ds_render::LEGEND_DEFAULT_WIDTH, MAX_LEGEND_WIDTH),
        side(height, ds_render::LEGEND_DEFAULT_HEIGHT, MAX_LEGEND_HEIGHT),
    )
}

/// WMS request parameters.
///
/// WMS 1.3.0 §6.8.1 makes parameter names case-insensitive: `REQUEST`,
/// `request` and `ReQuEsT` are one parameter. serde field names and aliases
/// match case-sensitively, so the query string is read as its decoded
/// `(name, value)` pairs and keyed here by [`WmsQuery::from_pairs`]; never
/// add serde aliases for a spelling. Values are kept exactly as sent, since
/// layer and style names are case-sensitive. A name the server does not use
/// is ignored, as WMS requires, and a parameter given more than once, in any
/// mix of cases, takes its first value.
#[derive(Debug, Default)]
pub struct WmsQuery {
    pub service: Option<String>,
    pub request: Option<String>,
    pub version: Option<String>,
    pub layers: Option<String>,
    pub layer: Option<String>,
    pub styles: Option<String>,
    pub style: Option<String>,
    pub crs: Option<String>,
    pub bbox: Option<String>,
    pub width: Option<String>,
    pub height: Option<String>,
    pub format: Option<String>,
    pub transparent: Option<String>,
    pub bgcolor: Option<String>,
    pub time: Option<String>,
    pub elevation: Option<String>,
    /// Forecast model run selector — the custom `reference_time` dimension
    /// (de-facto ncWMS/THREDDS convention). Per WMS, a dimension `foo` is
    /// requested via `DIM_FOO`.
    pub dim_reference_time: Option<String>,
    /// Encoder quality vendor parameter (GetMap only): 1–100 for
    /// `image/webp` (100 = lossless) and `image/jpeg`; rejected for PNG.
    pub quality: Option<String>,
    /// GetFeatureInfo: the `LAYERS` layers to read a value from.
    pub query_layers: Option<String>,
    /// GetFeatureInfo: the column of the queried pixel, 0 at the left.
    pub i: Option<String>,
    /// GetFeatureInfo: the row of the queried pixel, 0 at the top.
    pub j: Option<String>,
    /// GetFeatureInfo: the response's MIME type, one of [`INFO_FORMATS`].
    pub info_format: Option<String>,
    /// GetFeatureInfo: the most features to report per layer.
    pub feature_count: Option<String>,
}

impl<'de> Deserialize<'de> for WmsQuery {
    /// Reads the query string as its `(name, value)` pairs in request order,
    /// both already percent-decoded, so names can be matched in any case and
    /// "first value wins" is well defined.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let pairs = Vec::<(String, String)>::deserialize(deserializer)?;
        Ok(Self::from_pairs(pairs))
    }
}

/// Validated GetMap parameters.
pub struct GetMapParams {
    /// The first `LAYERS` name: the layer GetMap renders.
    pub layer: String,
    /// Its style, `default` for an empty `STYLES` value.
    pub style: String,
    /// The further `(layer, style)` pairs of a request naming several layers,
    /// in `LAYERS` order. GetMap renders one layer ([`LAYER_LIMIT`]): the
    /// handler checks each pair names a layer and style it serves, so an
    /// undefined one gets its WMS 1.3.0 Table E.1 code, then refuses the
    /// request.
    pub other_layers: Vec<(String, String)>,
    pub crs: String,
    /// Bbox in WGS84 [west, south, east, north], normalized from CRS axis order.
    /// For EPSG:3067/3035 the inverse-projected envelope of the projected box
    /// the engine reads, and NaN for a box wholly outside the projection,
    /// whose `output_crs.domain_rows` is `None` (#1089): read a projected
    /// frame's positions from `output_crs`, never from this.
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    /// Opaque background to composite the image over, from `TRANSPARENT` /
    /// `BGCOLOR` and the format: `None` keeps the alpha channel
    /// (`TRANSPARENT=TRUE`, for PNG/WebP); `Some(rgb)` for `TRANSPARENT=FALSE`,
    /// the default, and for JPEG, which cannot carry alpha.
    pub background: Option<[u8; 3]>,
    pub time: Option<DateTime<Utc>>,
    /// Output CRS for pixel-to-coordinate mapping.
    pub output_crs: OutputCrs,
    /// Output image format, at its default quality (JPEG 85, lossless WebP).
    /// The handler applies [`Self::quality`] and the collection's
    /// `webp_quality` with [`ds_render::ImageFormat::with_quality`].
    pub format: ds_render::ImageFormat,
    /// The `QUALITY` vendor parameter, 1–100, when supplied. Always `None`
    /// for PNG, which rejects it.
    pub quality: Option<u8>,
    /// Vertical level from the WMS `ELEVATION` dimension, when supplied.
    pub elevation: Option<f64>,
    /// Forecast model run from the custom `reference_time` dimension
    /// (`DIM_REFERENCE_TIME`), when supplied. `None` ⇒ the engine's latest run.
    pub reference_time: Option<DateTime<Utc>>,
}

/// Validated GetFeatureInfo parameters (WMS 1.3.0 §7.4.3).
pub struct GetFeatureInfoParams {
    /// The map the client shows: the GetMap parameters, validated by
    /// [`WmsQuery::validate_get_map`] so the two requests cannot read them
    /// differently.
    pub map: GetMapParams,
    /// The `QUERY_LAYERS` names, in request order, each once. The handler
    /// checks each is one of the map's `LAYERS`.
    pub query_layers: Vec<String>,
    /// The queried pixel's column, `0..map.width`.
    pub i: u32,
    /// The queried pixel's row, `0..map.height`, 0 at the top.
    pub j: u32,
    pub info_format: InfoFormat,
    /// `FEATURE_COUNT`, 1 when omitted. Every queried layer has one value
    /// at a point, so any count reports it.
    pub feature_count: u32,
}

impl WmsQuery {
    /// Key a request's decoded `(name, value)` pairs, in request order.
    ///
    /// Names match whatever their case; values are kept as sent. A name no
    /// field reads is skipped, and a repeated parameter keeps its first value:
    /// `LAYERS=a&layers=b` names layer `a`.
    pub fn from_pairs<N, V>(pairs: impl IntoIterator<Item = (N, V)>) -> Self
    where
        N: AsRef<str>,
        V: Into<String>,
    {
        let mut query = Self::default();
        for (name, value) in pairs {
            if let Some(slot) = query.slot(name.as_ref()) {
                slot.get_or_insert_with(|| value.into());
            }
        }
        query
    }

    /// The field parameter `name` fills, matched case-insensitively, or
    /// `None` for a name the server does not use.
    fn slot(&mut self, name: &str) -> Option<&mut Option<String>> {
        let slot = match name.to_ascii_lowercase().as_str() {
            "service" => &mut self.service,
            "request" => &mut self.request,
            "version" => &mut self.version,
            "layers" => &mut self.layers,
            "layer" => &mut self.layer,
            "styles" => &mut self.styles,
            "style" => &mut self.style,
            "crs" => &mut self.crs,
            "bbox" => &mut self.bbox,
            "width" => &mut self.width,
            "height" => &mut self.height,
            "format" => &mut self.format,
            "transparent" => &mut self.transparent,
            "bgcolor" => &mut self.bgcolor,
            "time" => &mut self.time,
            "elevation" => &mut self.elevation,
            "dim_reference_time" => &mut self.dim_reference_time,
            "quality" => &mut self.quality,
            "query_layers" => &mut self.query_layers,
            "i" => &mut self.i,
            "j" => &mut self.j,
            "info_format" => &mut self.info_format,
            "feature_count" => &mut self.feature_count,
            _ => return None,
        };
        Some(slot)
    }

    /// Extract and validate the REQUEST type.
    pub fn request_type(&self) -> Result<WmsRequestType, WmsError> {
        let request = self
            .request
            .as_deref()
            .ok_or(WmsError::missing_parameter("REQUEST"))?;

        match request {
            "GetCapabilities" => Ok(WmsRequestType::GetCapabilities),
            "GetMap" => Ok(WmsRequestType::GetMap),
            "GetLegendGraphic" => Ok(WmsRequestType::GetLegendGraphic),
            "GetFeatureInfo" => Ok(WmsRequestType::GetFeatureInfo),
            other => Err(WmsError::operation_not_supported(other)),
        }
    }

    /// Validate and extract GetMap parameters.
    pub fn validate_get_map(&self) -> Result<GetMapParams, WmsError> {
        // VERSION is mandatory and must be 1.3.0. Only GetCapabilities
        // negotiates a version (WMS 1.3.0 §6.2.4); GetMap and GetFeatureInfo
        // without one are refused, as ets-wms13's getmap:no-version expects.
        let version = self
            .version
            .as_deref()
            .ok_or(WmsError::missing_parameter("VERSION"))?;
        if version != "1.3.0" {
            return Err(WmsError::invalid_parameter(&format!(
                "Unsupported VERSION '{version}'. Only 1.3.0 is supported."
            )));
        }

        // LAYERS and STYLES. `split` yields at least one name. More layers
        // than `LAYER_LIMIT` are refused by the handler, once it has checked
        // that each names a layer and style it serves.
        let layers_str = self
            .layers
            .as_deref()
            .ok_or(WmsError::missing_parameter("LAYERS"))?;
        let layers: Vec<&str> = layers_str.split(',').collect();
        let styles_str = self
            .styles
            .as_deref()
            .or(self.style.as_deref())
            .unwrap_or("");
        // One layer takes the whole STYLES value as its style, as it always
        // has. Several pair with STYLES' values one to one (WMS 1.3.0
        // §7.3.3.4); an empty STYLES gives each its default.
        let styles: Vec<&str> = if layers.len() == 1 || styles_str.is_empty() {
            vec![styles_str; layers.len()]
        } else {
            styles_str.split(',').collect()
        };
        if styles.len() != layers.len() {
            return Err(WmsError::invalid_parameter(&format!(
                "STYLES has {} values for {} LAYERS: give one per layer, or leave STYLES \
                 empty for every layer's default",
                styles.len(),
                layers.len()
            )));
        }
        // An empty style is the layer's default.
        let mut pairs = layers.iter().zip(&styles).map(|(&layer, &style)| {
            let style = if style.is_empty() { "default" } else { style };
            (layer.to_string(), style.to_string())
        });
        let (layer, style) = pairs.next().expect("split yields at least one layer");
        let other_layers: Vec<(String, String)> = pairs.collect();

        // CRS
        let crs = self
            .crs
            .as_deref()
            .ok_or(WmsError::missing_parameter("CRS"))?;
        if !SUPPORTED_CRS.contains(&crs) {
            return Err(WmsError::invalid_crs(crs));
        }
        let crs = crs.to_string();

        // BBOX — also resolves the output CRS (axis order + reprojection both
        // depend on the CRS, so they're determined together).
        let bbox_str = self
            .bbox
            .as_deref()
            .ok_or(WmsError::missing_parameter("BBOX"))?;
        let (bbox, output_crs) = parse_bbox(bbox_str, &crs)?;

        // WIDTH
        let width: u32 = self
            .width
            .as_deref()
            .ok_or(WmsError::missing_parameter("WIDTH"))?
            .parse()
            .map_err(|_| WmsError::invalid_parameter("WIDTH must be a positive integer"))?;

        // HEIGHT
        let height: u32 = self
            .height
            .as_deref()
            .ok_or(WmsError::missing_parameter("HEIGHT"))?
            .parse()
            .map_err(|_| WmsError::invalid_parameter("HEIGHT must be a positive integer"))?;

        // Validate dimensions
        if width == 0 || height == 0 {
            return Err(WmsError::invalid_parameter(
                "WIDTH and HEIGHT must be greater than 0",
            ));
        }
        if width > MAX_MAP_DIMENSION || height > MAX_MAP_DIMENSION {
            return Err(WmsError::invalid_parameter(&format!(
                "WIDTH and HEIGHT must not exceed {MAX_MAP_DIMENSION}"
            )));
        }
        if (width as u64) * (height as u64) > MAX_MAP_PIXELS {
            return Err(WmsError::invalid_parameter(&format!(
                "WIDTH * HEIGHT ({}) exceeds maximum of {MAX_MAP_PIXELS}",
                width as u64 * height as u64
            )));
        }

        // FORMAT, and the QUALITY vendor parameter for JPEG/WebP (1–100; for
        // WebP 100 = lossless). A value on PNG is an error, not ignored.
        let image_format = parse_image_format(self.format.as_deref())?;
        let quality = ds_render::parse_quality("QUALITY", self.quality.as_deref(), image_format)
            .map_err(|msg| WmsError::invalid_parameter(&msg))?;

        // TRANSPARENT + BGCOLOR (WMS 1.3.0 §7.3.3.9–10). With TRANSPARENT=FALSE,
        // the default, or a format without alpha (JPEG), nodata pixels take
        // the BGCOLOR value (default white) and the image is opaque. An
        // overlay client sends TRANSPARENT=TRUE, as OpenLayers' WMS sources
        // do unless told otherwise, and keeps nodata transparent.
        let transparent = parse_transparent(self.transparent.as_deref())?;
        let bgcolor = parse_bgcolor(self.bgcolor.as_deref())?;
        let background = (!transparent
            || matches!(image_format, ds_render::ImageFormat::Jpeg { .. }))
        .then_some(bgcolor);

        // TIME
        let time = self.time.as_deref().map(parse_time).transpose()?;

        // ELEVATION — a single vertical level. WMS 1.3.0 permits a
        // comma-separated list, but this server renders one layer per
        // request, so a list is rejected with an explicit message. Like a
        // malformed TIME, a value that is no single level is an invalid
        // sample dimension value: `InvalidDimensionValue` (Table E.1).
        let elevation = match self
            .elevation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) if raw.contains(',') => {
                return Err(WmsError::InvalidDimensionValue(
                    "ELEVATION must be a single value; comma-separated lists are not supported"
                        .to_string(),
                ));
            }
            Some(raw) => Some(
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| {
                        WmsError::InvalidDimensionValue(format!(
                            "ELEVATION '{raw}' is not a finite number"
                        ))
                    })?,
            ),
            None => None,
        };

        // DIM_REFERENCE_TIME — the forecast model run. A single value; the
        // handler validates it against the layer's advertised runs (and rejects
        // it for a non-forecast layer). Empty/whitespace ⇒ latest run (None).
        let reference_time = self
            .dim_reference_time
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(parse_reference_time)
            .transpose()?;

        Ok(GetMapParams {
            layer,
            style,
            other_layers,
            crs,
            bbox,
            width,
            height,
            background,
            time,
            output_crs,
            format: image_format,
            quality,
            elevation,
            reference_time,
        })
    }

    /// Validate and extract GetFeatureInfo parameters (WMS 1.3.0 §7.4.3).
    ///
    /// The map part is [`Self::validate_get_map`]'s, with its exceptions.
    /// Then: `QUERY_LAYERS` is mandatory; `INFO_FORMAT` is one of
    /// [`INFO_FORMATS`], else `InvalidFormat`; `I` and `J` are mandatory,
    /// and one that is no pixel of the `WIDTH` × `HEIGHT` map, a negative or
    /// non-integer value included, is `InvalidPoint`; `FEATURE_COUNT`, when
    /// given, is a positive integer.
    pub fn validate_get_feature_info(&self) -> Result<GetFeatureInfoParams, WmsError> {
        let map = self.validate_get_map()?;

        let mut query_layers: Vec<String> = Vec::new();
        for name in self
            .query_layers
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !query_layers.iter().any(|q| q == name) {
                query_layers.push(name.to_string());
            }
        }
        if query_layers.is_empty() {
            return Err(WmsError::missing_parameter("QUERY_LAYERS"));
        }

        let info_format = InfoFormat::parse(self.info_format.as_deref())?;

        let pixel = |name: &str, raw: Option<&str>, size: u32| -> Result<u32, WmsError> {
            let raw = raw
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| WmsError::missing_parameter(name))?;
            raw.parse::<u32>()
                .ok()
                .filter(|&v| v < size)
                .ok_or_else(|| {
                    WmsError::InvalidPoint(format!(
                        "{name} '{raw}' is no pixel of the map: use an integer from 0 to {}",
                        size - 1
                    ))
                })
        };
        let i = pixel("I", self.i.as_deref(), map.width)?;
        let j = pixel("J", self.j.as_deref(), map.height)?;

        let feature_count = match self
            .feature_count
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => 1,
            Some(raw) => raw.parse::<u32>().ok().filter(|&n| n > 0).ok_or_else(|| {
                WmsError::invalid_parameter(&format!(
                    "FEATURE_COUNT '{raw}' must be a positive integer"
                ))
            })?,
        };

        Ok(GetFeatureInfoParams {
            map,
            query_layers,
            i,
            j,
            info_format,
            feature_count,
        })
    }
}

pub enum WmsRequestType {
    GetCapabilities,
    GetMap,
    GetLegendGraphic,
    GetFeatureInfo,
}

/// Parse a BBOX string and resolve the output CRS, handling WMS 1.3.0 axis
/// order and reprojection.
///
/// WMS 1.3.0 axis order depends on CRS:
/// - CRS:84    → lon,lat order: BBOX=west,south,east,north
/// - EPSG:4326 → lat,lon order: BBOX=south,west,north,east (swapped)
/// - EPSG:3857/3067/3035 → easting,northing: BBOX=minx,miny,maxx,maxy
///
/// Returns the WGS84 bounding box `[west, south, east, north]` the engine reads
/// against, paired with the [`OutputCrs`] that tells the engine how output
/// pixels map to coordinates:
/// - CRS:84 / EPSG:4326 → bbox is already WGS84 degrees; [`OutputCrs::Wgs84`].
/// - EPSG:3857 → bbox metres reprojected to a WGS84 box; [`OutputCrs::WebMercator`].
/// - EPSG:3067 / EPSG:3035 → bbox is projected metres; [`OutputCrs::Projected`]
///   carries it, and the returned WGS84 box is its inverse-projected envelope so
///   the engine reads the right source window (#160/#251). Previously these two
///   codes fell through to `Wgs84` and the engine read projected metres as
///   degrees → fully-transparent tiles. A box wholly outside the projection's
///   valid area has no read window: [`NO_READ_WINDOW`] (#1089).
fn parse_bbox(bbox_str: &str, crs: &str) -> Result<([f64; 4], OutputCrs), WmsError> {
    let parts: Vec<f64> = bbox_str
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<f64>()
                .map_err(|_| WmsError::invalid_parameter(&format!("Invalid BBOX value: '{s}'")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if parts.len() != 4 {
        return Err(WmsError::invalid_parameter(
            "BBOX must have exactly 4 values",
        ));
    }

    // Check for NaN/Infinity
    for v in &parts {
        if !v.is_finite() {
            return Err(WmsError::invalid_parameter(
                "BBOX values must be finite numbers",
            ));
        }
    }

    // WMS 1.3.0 axis order: EPSG:4326 uses lat/lon, everything else x/y.
    let [x1, y1, x2, y2] = match crs {
        "EPSG:4326" => {
            // BBOX = south, west, north, east (lat/lon order) → normalize to x/y
            [parts[1], parts[0], parts[3], parts[2]]
        }
        _ => {
            // CRS:84, EPSG:3857, EPSG:3067, EPSG:3035 use x/y order
            [parts[0], parts[1], parts[2], parts[3]]
        }
    };

    // Basic validation (in source CRS units)
    if x1 >= x2 || y1 >= y2 {
        return Err(WmsError::invalid_parameter(
            "BBOX: min values must be less than max values",
        ));
    }

    match crs {
        "EPSG:3857" => {
            // Web Mercator metres → WGS84 degrees; pixel Y stays Mercator-spaced.
            let (lon1, lat1) = epsg3857_to_wgs84(x1, y1);
            let (lon2, lat2) = epsg3857_to_wgs84(x2, y2);
            Ok(([lon1, lat1, lon2, lat2], OutputCrs::WebMercator))
        }
        "EPSG:3067" | "EPSG:3035" => {
            // Projected metres: keep them in the OutputCrs so the engine lays
            // output pixels out in the projection, and pass its inverse-
            // projected WGS84 envelope as the read window.
            let proj_crs = ds_core::geo::projected_output_crs(crs).ok_or_else(|| {
                WmsError::invalid_parameter(&format!("CRS '{crs}' has no projection definition"))
            })?;
            let proj_bbox = [x1, y1, x2, y2];
            // Wholly outside the projection's valid area: WMS 1.3.0 §7.3.3.6
            // empty content (#1089). `OutputCrs::domain_rows` is `None` for
            // this frame, so GetMap serves the empty image without reading
            // the engine. Asked before the envelope, which such a box has
            // none of.
            let wgs84 = if ds_core::geo::outside_projection_domain(&proj_crs, proj_bbox) {
                NO_READ_WINDOW
            } else {
                // `None`: every edge sample misses the valid area, which
                // reaches into the box: a box wider than the LAEA disk, or a
                // tall or wide one whose edges cross the area between
                // samples. The edges give no read window, and a global one
                // would read whole datasets: reject.
                ds_core::geo::wgs84_envelope(&proj_crs, proj_bbox).ok_or_else(|| {
                    WmsError::invalid_parameter(&format!(
                        "BBOX edges miss the valid area of {crs} inside the box: request a \
                         smaller BBOX"
                    ))
                })?
            };
            Ok((
                wgs84,
                OutputCrs::Projected {
                    crs: proj_crs,
                    bbox: proj_bbox,
                },
            ))
        }
        _ => {
            // CRS:84 and EPSG:4326 are already in WGS84 degrees.
            Ok(([x1, y1, x2, y2], OutputCrs::Wgs84))
        }
    }
}

/// The WGS84 read window of a projected BBOX wholly outside its CRS's valid
/// area, which has none: NaN, which names no place. Never read: the frame's
/// [`OutputCrs::domain_rows`] is `None`, so GetMap serves the empty image
/// without calling the engine (#1089). The rendered cache keys a projected
/// frame on its metres, not on this.
const NO_READ_WINDOW: [f64; 4] = [f64::NAN; 4];

/// Convert EPSG:3857 (Web Mercator) metres to WGS84 (lon/lat degrees) via the
/// shared [`ds_core::web_mercator`] conversions (single source of truth, #452).
/// Unclamped — a zoomed-out request bbox legitimately reaches past ±85°.
fn epsg3857_to_wgs84(x: f64, y: f64) -> (f64, f64) {
    (
        ds_core::web_mercator::x_to_lon(x),
        ds_core::web_mercator::y_to_lat(y),
    )
}

/// `BGCOLOR` default: white (WMS 1.3.0 §7.3.3.10).
const DEFAULT_BGCOLOR: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// Parse `TRANSPARENT`: `TRUE` or `FALSE`, case-insensitive; omitted or empty
/// ⇒ `FALSE`, as WMS 1.3.0 §7.3.3.9 specifies (#1065). Anything else is
/// rejected rather than read as either, since a typo would silently pick
/// the opaque or the transparent image.
fn parse_transparent(raw: Option<&str>) -> Result<bool, WmsError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(false),
        Some(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Some(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        Some(s) => Err(WmsError::invalid_parameter(&format!(
            "TRANSPARENT '{s}' must be TRUE or FALSE"
        ))),
    }
}

/// Parse `BGCOLOR` strictly as `0xRRGGBB` — a lower-case `0x` prefix and six
/// hex digits of either case (WMS 1.3.0 §7.3.3.10); omitted or empty ⇒ white.
fn parse_bgcolor(raw: Option<&str>) -> Result<[u8; 3], WmsError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_BGCOLOR);
    };
    s.strip_prefix("0x")
        .filter(|hex| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .map(|rgb| {
            let [_, r, g, b] = rgb.to_be_bytes();
            [r, g, b]
        })
        .ok_or_else(|| {
            WmsError::invalid_parameter(&format!("BGCOLOR '{s}' must be of the form 0xRRGGBB"))
        })
}

/// Parse an ISO 8601 timestamp for the TIME parameter.
fn parse_time(s: &str) -> Result<DateTime<Utc>, WmsError> {
    // Try RFC 3339 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Try basic ISO 8601 without timezone (assume UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Ok(dt.and_utc());
    }
    Err(WmsError::InvalidDimensionValue(format!(
        "Cannot parse TIME '{s}' as ISO 8601"
    )))
}

/// Parse a `DIM_REFERENCE_TIME` value (the forecast model run / reference time).
///
/// Accepts the same ISO 8601 forms as `TIME` (the values GetCapabilities
/// advertises for the `reference_time` dimension are RFC 3339, as is an EDR
/// instance id) and, through the EDR instance-id codec, the compact stamp
/// EDR served as instance id before #947 (`20260607T0600Z`), so a client can
/// echo either the WMS dimension value or an EDR instance id.
fn parse_reference_time(s: &str) -> Result<DateTime<Utc>, WmsError> {
    if let Ok(dt) = parse_time(s) {
        return Ok(dt);
    }
    ds_core::instances::parse_instance_id(s).ok_or_else(|| {
        WmsError::InvalidDimensionValue(format!(
            "Cannot parse DIM_REFERENCE_TIME '{s}' as ISO 8601 or a run id"
        ))
    })
}

/// Supported CRS list for GetCapabilities.
pub fn supported_crs_list() -> &'static [&'static str] {
    SUPPORTED_CRS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bbox_crs84() {
        let (bbox, crs) = parse_bbox("10,55,30,70", "CRS:84").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
        assert_eq!(crs, OutputCrs::Wgs84);
    }

    #[test]
    fn test_bbox_epsg4326_axis_swap() {
        // EPSG:4326 uses lat/lon order: south,west,north,east
        let (bbox, crs) = parse_bbox("55,10,70,30", "EPSG:4326").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
        assert_eq!(crs, OutputCrs::Wgs84);
    }

    #[test]
    fn test_bbox_invalid() {
        assert!(parse_bbox("10,55,5,70", "CRS:84").is_err()); // west > east
        assert!(parse_bbox("NaN,0,1,1", "CRS:84").is_err());
    }

    #[test]
    fn test_bbox_epsg3857_reprojection() {
        // Web Mercator bbox covering roughly lon 1.5-14.3, lat 53.0-63.0
        let (bbox, crs) =
            parse_bbox("171318.93,6897641.62,2528475.00,9153471.98", "EPSG:3857").unwrap();
        // Should be reprojected to WGS84 degrees
        assert!((bbox[0] - 1.539).abs() < 0.01); // west ≈ 1.54°
        assert!((bbox[1] - 52.536).abs() < 0.01); // south ≈ 52.54°
        assert!((bbox[2] - 22.714).abs() < 0.01); // east ≈ 22.71°
        assert!((bbox[3] - 63.216).abs() < 0.01); // north ≈ 63.22°
        assert_eq!(crs, OutputCrs::WebMercator);
    }

    #[test]
    fn test_bbox_epsg3067_projected() {
        // Regression for #251/#160: FMI's native TM35FIN extent. The bbox is in
        // EPSG:3067 metres and must NOT be passed through as WGS84 degrees.
        let (bbox, crs) = parse_bbox(
            "-118331.366408,6335621.167014,875567.731907,7907751.537264",
            "EPSG:3067",
        )
        .unwrap();

        // OutputCrs carries the projected request rectangle verbatim.
        match &crs {
            OutputCrs::Projected {
                bbox: proj,
                crs: proj_crs,
            } => {
                assert_eq!(
                    *proj,
                    [
                        -118331.366408,
                        6335621.167014,
                        875567.731907,
                        7907751.537264
                    ]
                );
                // EPSG:3067 is Transverse Mercator (TM35FIN).
                assert!(matches!(
                    proj_crs,
                    ds_core::geo::Crs::TransverseMercator { .. }
                ));
            }
            other => panic!("expected Projected, got {other:?}"),
        }

        // The returned WGS84 envelope must be plausible Nordic degrees, NOT the
        // metres treated as degrees (a metres-as-degrees bug puts these in the
        // millions). FMI's composite is wide, so the lon span reaches ~10–37°E.
        let [west, south, east, north] = bbox;
        assert!(
            (-30.0..60.0).contains(&west) && (-30.0..60.0).contains(&east),
            "envelope lon {west}..{east} should be degrees"
        );
        assert!(
            (45.0..75.0).contains(&south) && (45.0..75.0).contains(&north),
            "envelope lat {south}..{north} should be degrees"
        );
        assert!(west < east && south < north);
    }

    #[test]
    fn test_bbox_epsg3035_projected() {
        // EPSG:3035 (ETRS89-LAEA) is also projected metres → Projected, not Wgs84.
        let (_bbox, crs) = parse_bbox("4200000,3200000,5300000,5000000", "EPSG:3035").unwrap();
        assert!(matches!(crs, OutputCrs::Projected { .. }));
    }

    /// A projected box wholly outside its projection keeps its metres in the
    /// output CRS, and its read window names no place: NaN, never a real
    /// position such as 0°, 0° (#1089). EPSG:3067's lie past a pole, and
    /// past 50° east of its meridian, where its inverse once gave places
    /// anywhere (#1113).
    #[test]
    fn projected_bbox_wholly_outside_has_no_read_window() {
        for (crs, metres) in [
            ("EPSG:3035", [2.0e7, 2.0e7, 2.1e7, 2.1e7]),
            ("EPSG:3067", [0.0, 2.1e7, 1.0e6, 2.2e7]),
            ("EPSG:3067", [0.0, -2.2e7, 1.0e6, -2.1e7]),
            ("EPSG:3067", [5.0e6, 6.5e6, 6.0e6, 7.0e6]),
        ] {
            let bbox = metres.map(|v| v.to_string()).join(",");
            let (read, output) = parse_bbox(&bbox, crs).unwrap();
            assert!(read.iter().all(|v| v.is_nan()), "{crs} {bbox}: {read:?}");
            assert!(
                matches!(output, OutputCrs::Projected { bbox, .. } if bbox == metres),
                "{crs} {bbox}"
            );
        }
    }

    #[test]
    fn test_parse_time() {
        assert!(parse_time("2024-01-01T00:00:00Z").is_ok());
        assert!(parse_time("2024-01-01T00:00:00+00:00").is_ok());
        assert!(parse_time("2024-01-01T00:00").is_ok());
        assert!(parse_time("not-a-time").is_err());
    }

    #[test]
    fn parse_bgcolor_accepts_0xrrggbb_in_either_hex_case() {
        assert_eq!(parse_bgcolor(Some("0x336699")).unwrap(), [0x33, 0x66, 0x99]);
        assert_eq!(parse_bgcolor(Some("0xAbCdEf")).unwrap(), [0xAB, 0xCD, 0xEF]);
        assert_eq!(parse_bgcolor(Some("0x000000")).unwrap(), [0, 0, 0]);
        // Omitted or empty ⇒ the spec default, white.
        assert_eq!(parse_bgcolor(None).unwrap(), [0xFF, 0xFF, 0xFF]);
        assert_eq!(parse_bgcolor(Some("")).unwrap(), [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn parse_bgcolor_rejects_malformed_values() {
        for bogus in [
            "#336699",
            "336699",
            "0X336699",
            "0x3366",
            "0x33669",
            "0x3366990",
            "0x33669G",
            "0x+33669",
            "0x",
            "white",
        ] {
            let err = parse_bgcolor(Some(bogus)).expect_err(bogus);
            assert!(
                matches!(err, WmsError::InvalidParameterValue(_)),
                "expected InvalidParameterValue for {bogus:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn parse_transparent_is_true_false_case_insensitive() {
        // Omitted or empty: FALSE, WMS 1.3.0 §7.3.3.9's default (#1065).
        assert!(!parse_transparent(None).unwrap());
        assert!(!parse_transparent(Some("")).unwrap());
        assert!(!parse_transparent(Some("  ")).unwrap());
        assert!(parse_transparent(Some("TRUE")).unwrap());
        assert!(parse_transparent(Some("true")).unwrap());
        assert!(!parse_transparent(Some("FALSE")).unwrap());
        assert!(!parse_transparent(Some("False")).unwrap());
        for bogus in ["1", "0", "yes", "no", "t"] {
            let err = parse_transparent(Some(bogus)).expect_err(bogus);
            assert!(matches!(err, WmsError::InvalidParameterValue(_)));
        }
    }

    #[test]
    fn parse_image_format_accepts_all_supported_types() {
        assert!(matches!(
            parse_image_format(Some("image/png")).unwrap(),
            ds_render::ImageFormat::Png
        ));
        assert_eq!(
            parse_image_format(Some("image/jpeg")).unwrap(),
            ds_render::ImageFormat::JPEG
        );
        // WebP defaults to lossless; QUALITY / webp_quality change it later.
        assert_eq!(
            parse_image_format(Some("image/webp")).unwrap(),
            ds_render::ImageFormat::WEBP
        );
    }

    #[test]
    fn parse_image_format_defaults_to_png_when_absent() {
        assert!(matches!(
            parse_image_format(None).unwrap(),
            ds_render::ImageFormat::Png
        ));
    }

    #[test]
    fn parse_image_format_rejects_unknown_with_invalid_format() {
        // Locks in the fix for #161: any value outside SUPPORTED_FORMATS
        // must return InvalidFormat, not silently fall back to PNG. The
        // pre-fix handler returned PNG for image/gif and any other typo
        // with no error, leaving capabilities-trusting clients confused.
        for bogus in ["image/gif", "image/avif", "", "png", "image/webp;q=1"] {
            let err = parse_image_format(Some(bogus)).expect_err(bogus);
            assert!(
                matches!(err, WmsError::InvalidFormat(_)),
                "expected InvalidFormat for {bogus:?}, got {err:?}"
            );
        }
    }

    fn get_map_query(format: &str, quality: Option<&str>) -> WmsQuery {
        WmsQuery {
            service: Some("WMS".into()),
            request: Some("GetMap".into()),
            version: Some("1.3.0".into()),
            layers: Some("radar".into()),
            layer: None,
            styles: None,
            style: None,
            crs: Some("CRS:84".into()),
            bbox: Some("10,55,30,70".into()),
            width: Some("64".into()),
            height: Some("64".into()),
            format: Some(format.into()),
            transparent: None,
            bgcolor: None,
            time: None,
            elevation: None,
            dim_reference_time: None,
            quality: quality.map(str::to_string),
            query_layers: None,
            i: None,
            j: None,
            info_format: None,
            feature_count: None,
        }
    }

    #[test]
    fn quality_is_parsed_for_webp_and_jpeg() {
        let p = get_map_query("image/webp", Some("80"))
            .validate_get_map()
            .unwrap();
        assert_eq!(p.quality, Some(80));
        // The format stays at its default; the handler applies the quality
        // together with the collection's webp_quality.
        assert_eq!(p.format, ds_render::ImageFormat::WEBP);
        let p = get_map_query("image/jpeg", Some("100"))
            .validate_get_map()
            .unwrap();
        assert_eq!(p.quality, Some(100));
        for absent in [None, Some(""), Some("  ")] {
            let p = get_map_query("image/webp", absent)
                .validate_get_map()
                .unwrap();
            assert_eq!(p.quality, None);
        }
    }

    #[test]
    fn quality_out_of_range_or_not_an_integer_is_invalid_parameter_value() {
        for bogus in ["0", "101", "-5", "80.5", "high"] {
            let err = get_map_query("image/webp", Some(bogus))
                .validate_get_map()
                .err()
                .expect(bogus);
            match err {
                WmsError::InvalidParameterValue(msg) => assert_eq!(
                    msg,
                    format!("QUALITY '{bogus}' must be an integer from 1 to 100")
                ),
                other => panic!("{bogus}: expected InvalidParameterValue, got {other:?}"),
            }
        }
    }

    #[test]
    fn quality_on_png_is_invalid_parameter_value() {
        let err = get_map_query("image/png", Some("80"))
            .validate_get_map()
            .err()
            .unwrap();
        match err {
            WmsError::InvalidParameterValue(msg) => assert_eq!(
                msg,
                "QUALITY applies only to image/jpeg and image/webp, not image/png"
            ),
            other => panic!("expected InvalidParameterValue, got {other:?}"),
        }
        // Without QUALITY, PNG is unaffected.
        assert!(get_map_query("image/png", None).validate_get_map().is_ok());
    }

    /// A GetFeatureInfo on a 64 × 32 CRS:84 map, with `pairs` added or, as
    /// a repeated name keeps its first value, overriding.
    fn get_feature_info(pairs: &[(&str, &str)]) -> Result<GetFeatureInfoParams, WmsError> {
        let base = [
            ("REQUEST", "GetFeatureInfo"),
            ("VERSION", "1.3.0"),
            ("LAYERS", "radar"),
            ("CRS", "CRS:84"),
            ("BBOX", "10,55,30,70"),
            ("WIDTH", "64"),
            ("HEIGHT", "32"),
            ("QUERY_LAYERS", "radar"),
            ("I", "63"),
            ("J", "31"),
        ];
        WmsQuery::from_pairs(pairs.iter().chain(&base).copied()).validate_get_feature_info()
    }

    #[test]
    fn get_feature_info_reads_the_map_part_and_its_own_parameters() {
        let p = get_feature_info(&[("QUERY_LAYERS", "radar, radar")]).unwrap();
        assert_eq!(p.map.layer, "radar");
        assert_eq!((p.map.width, p.map.height), (64, 32));
        assert_eq!(p.query_layers, ["radar"], "named once");
        assert_eq!((p.i, p.j), (63, 31));
        assert_eq!(p.info_format, InfoFormat::Text, "INFO_FORMAT omitted");
        assert_eq!(p.feature_count, 1);
        let p = get_feature_info(&[("INFO_FORMAT", "Application/JSON"), ("FEATURE_COUNT", "5")])
            .unwrap();
        assert_eq!(
            p.info_format,
            InfoFormat::Json,
            "MIME types match in any case"
        );
        assert_eq!(p.feature_count, 5);
        // The map part's exceptions are GetMap's.
        assert!(matches!(
            get_feature_info(&[("CRS", "EPSG:9999")]),
            Err(WmsError::InvalidCrs(_))
        ));
    }

    /// WMS 1.3.0 Table E.1: an `I` or `J` that is no pixel of the map is
    /// `InvalidPoint`, whatever makes it so.
    #[test]
    fn get_feature_info_point_outside_the_map_is_invalid_point() {
        for (name, value) in [
            ("I", "64"),
            ("J", "32"),
            ("I", "-1"),
            ("J", "1.5"),
            ("I", "left"),
        ] {
            match get_feature_info(&[(name, value)]) {
                Err(WmsError::InvalidPoint(msg)) => assert!(msg.contains(value), "{msg}"),
                Err(other) => panic!("{name}={value}: expected InvalidPoint, got {other:?}"),
                Ok(_) => panic!("{name}={value}: expected InvalidPoint"),
            }
        }
        for name in ["VERSION", "I", "J", "QUERY_LAYERS"] {
            let query = WmsQuery::from_pairs(
                [
                    ("REQUEST", "GetFeatureInfo"),
                    ("VERSION", "1.3.0"),
                    ("LAYERS", "radar"),
                    ("CRS", "CRS:84"),
                    ("BBOX", "10,55,30,70"),
                    ("WIDTH", "64"),
                    ("HEIGHT", "32"),
                    ("QUERY_LAYERS", "radar"),
                    ("I", "1"),
                    ("J", "1"),
                ]
                .into_iter()
                .filter(|(n, _)| *n != name),
            );
            // The message names the omitted parameter, so another missing
            // one cannot pass for it.
            assert!(
                matches!(
                    query.validate_get_feature_info(),
                    Err(WmsError::MissingParameterValue(msg)) if msg.ends_with(name)
                ),
                "{name} omitted"
            );
        }
    }

    #[test]
    fn get_map_without_version_is_missing_parameter_value() {
        let mut query = get_map_query("image/png", None);
        query.version = None;
        match query.validate_get_map() {
            Err(WmsError::MissingParameterValue(msg)) => {
                assert_eq!(msg, "Missing required parameter: VERSION")
            }
            Err(other) => panic!("expected MissingParameterValue, got {other:?}"),
            Ok(_) => panic!("a GetMap without VERSION was accepted"),
        }
        query.version = Some("1.1.1".into());
        assert!(matches!(
            query.validate_get_map(),
            Err(WmsError::InvalidParameterValue(_))
        ));
    }

    #[test]
    fn get_feature_info_rejects_other_formats_and_feature_counts() {
        for format in ["text/xml", "application/vnd.ogc.gml", "image/png"] {
            assert!(
                matches!(
                    get_feature_info(&[("INFO_FORMAT", format)]),
                    Err(WmsError::InvalidFormat(_))
                ),
                "{format}"
            );
        }
        for count in ["0", "-1", "many"] {
            assert!(
                matches!(
                    get_feature_info(&[("FEATURE_COUNT", count)]),
                    Err(WmsError::InvalidParameterValue(_))
                ),
                "{count}"
            );
        }
    }

    /// Capabilities lists `INFO_FORMATS`: each must parse, and to the
    /// format whose Content-Type names it.
    #[test]
    fn info_formats_all_parse_to_their_own_content_type() {
        for format in INFO_FORMATS {
            let parsed = InfoFormat::parse(Some(format)).unwrap();
            assert!(parsed.content_type().starts_with(format), "{format}");
        }
    }

    /// Every parameter name `WmsQuery` reads, lower-case, in the order
    /// [`fields`] lists the fields.
    const NAMES: [&str; 23] = [
        "service",
        "request",
        "version",
        "layers",
        "layer",
        "styles",
        "style",
        "crs",
        "bbox",
        "width",
        "height",
        "format",
        "transparent",
        "bgcolor",
        "time",
        "elevation",
        "dim_reference_time",
        "quality",
        "query_layers",
        "i",
        "j",
        "info_format",
        "feature_count",
    ];

    /// The fields of `q`, in [`NAMES`] order, written out so the test pins
    /// each name to its own field rather than reusing `WmsQuery::slot`.
    /// The destructuring names every field and has no `..`: a new
    /// `WmsQuery` field fails to compile here until it joins `NAMES`, and
    /// then the case test fails until `slot` reads it, so no parameter can
    /// be silently never read.
    fn fields(q: &WmsQuery) -> [Option<&str>; 23] {
        let WmsQuery {
            service,
            request,
            version,
            layers,
            layer,
            styles,
            style,
            crs,
            bbox,
            width,
            height,
            format,
            transparent,
            bgcolor,
            time,
            elevation,
            dim_reference_time,
            quality,
            query_layers,
            i,
            j,
            info_format,
            feature_count,
        } = q;
        [
            service,
            request,
            version,
            layers,
            layer,
            styles,
            style,
            crs,
            bbox,
            width,
            height,
            format,
            transparent,
            bgcolor,
            time,
            elevation,
            dim_reference_time,
            quality,
            query_layers,
            i,
            j,
            info_format,
            feature_count,
        ]
        .map(Option::as_deref)
    }

    /// `name` with its letters alternately upper and lower case, the way
    /// the OGC test suite spells them: `ReQuEsT`, or `rEqUeSt`.
    fn alternating_case(name: &str, upper_first: bool) -> String {
        name.chars()
            .enumerate()
            .map(|(i, c)| {
                if (i % 2 == 0) == upper_first {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    }

    /// WMS 1.3.0 §6.8.1: parameter names are case-insensitive. Every name
    /// fills its field whatever its case, not only the lower, upper and
    /// capitalised spellings the serde aliases used to list.
    #[test]
    fn every_parameter_name_matches_in_any_case() {
        let spellings: [fn(&str) -> String; 4] = [
            str::to_ascii_lowercase,
            str::to_ascii_uppercase,
            |name| alternating_case(name, true),
            |name| alternating_case(name, false),
        ];
        for spell in spellings {
            let query = WmsQuery::from_pairs(NAMES.map(|name| (spell(name), name)));
            assert_eq!(
                fields(&query),
                NAMES.map(Some),
                "spelled like {:?}",
                spell("request")
            );
        }
    }

    /// Values are kept as sent (layer and style names are case-sensitive),
    /// and a name the server does not use is ignored, as WMS requires.
    #[test]
    fn values_keep_their_case_and_unknown_names_are_ignored() {
        let query = WmsQuery::from_pairs([
            ("BoGuS", "ignored"),
            ("LaYeRs", "Radar-FI/DBZH"),
            ("sTyLeS", "Radar_FMI"),
            ("EXCEPTIONS", "XML"),
        ]);
        assert_eq!(query.layers.as_deref(), Some("Radar-FI/DBZH"));
        assert_eq!(query.styles.as_deref(), Some("Radar_FMI"));
        let set = fields(&query).iter().filter(|f| f.is_some()).count();
        assert_eq!(set, 2, "only LAYERS and STYLES are read: {query:?}");
    }

    /// A parameter given more than once, in one case or several, takes its
    /// first value, an empty one included.
    #[test]
    fn a_repeated_parameter_keeps_its_first_value() {
        let query = WmsQuery::from_pairs([
            ("LAYERS", "first"),
            ("layers", "second"),
            ("LaYeRs", "third"),
            ("styles", ""),
            ("STYLES", "named"),
        ]);
        assert_eq!(query.layers.as_deref(), Some("first"));
        assert_eq!(query.styles.as_deref(), Some(""));
    }

    /// The query string the handler's `Query<WmsQuery>` extractor parses:
    /// names in any case, names and values percent-decoded, `+` a space, a
    /// repeated name answered with its first value instead of the
    /// "duplicate field" rejection serde's derive gave, and unknown names
    /// skipped.
    #[test]
    fn query_string_is_decoded_then_keyed_case_insensitively() {
        let uri: axum::http::Uri = "/?ReQuEsT=%47%65%74%4D%61%70&%4C%61%59%65%52%73=radar%2FDBZH\
             &cRs=CRS%3A84&StYlEs=a+b&BoGuS=ignored&layers=other"
            .parse()
            .unwrap();
        let axum::extract::Query(query) =
            axum::extract::Query::<WmsQuery>::try_from_uri(&uri).expect("query parses");
        assert_eq!(query.request.as_deref(), Some("GetMap"));
        assert_eq!(query.layers.as_deref(), Some("radar/DBZH"));
        assert_eq!(query.crs.as_deref(), Some("CRS:84"));
        assert_eq!(query.styles.as_deref(), Some("a b"));
        assert!(matches!(query.request_type(), Ok(WmsRequestType::GetMap)));
    }

    /// An absent or unparseable side takes the default size and every side
    /// is clamped, as GetLegendGraphic sized legends before the size moved
    /// here for capabilities to share.
    #[test]
    fn legend_size_defaults_and_clamps_each_side() {
        let default = (
            ds_render::LEGEND_DEFAULT_WIDTH,
            ds_render::LEGEND_DEFAULT_HEIGHT,
        );
        assert_eq!(legend_size(None, None), default);
        assert_eq!(legend_size(Some("wide"), Some("")), default);
        assert_eq!(legend_size(Some("20"), Some("120")), (20, 120));
        assert_eq!(legend_size(Some("0"), Some("0")), (1, 1));
        assert_eq!(legend_size(Some("9999"), Some("9999")), (512, 1024));
    }

    /// Adding an entry to `SUPPORTED_FORMATS` without extending the match in
    /// `parse_image_format` will cause this test to fail — update both
    /// together. (Introduced to prevent the same capabilities-vs-handler
    /// drift that caused #161.)
    #[test]
    fn parse_image_format_covers_every_supported_format() {
        for fmt in SUPPORTED_FORMATS {
            let parsed = parse_image_format(Some(fmt)).unwrap_or_else(|_| {
                panic!("SUPPORTED_FORMATS entry {fmt:?} not handled by parse_image_format")
            });
            assert_eq!(
                parsed.content_type(),
                *fmt,
                "parse_image_format({fmt:?}) → ImageFormat with wrong content_type {}",
                parsed.content_type()
            );
        }
    }
}
