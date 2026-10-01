//! Map request parameters of OGC API - Maps Part 1 (OGC 20-058) and the map
//! view they select.
//!
//! [`MapQueryParams::validate`] checks the request on its own: syntax, the
//! CRS parameters, and the combinations the standard forbids. [`MapRequest::view`]
//! then resolves the area and size against the collection:
//!
//! - **Output CRS.** `crs`, else the collection's storage CRS when it is one
//!   of [`MapCrs::ALL`], else CRS84 (`/req/core/map-response` B).
//! - **Area.** At most one of `bbox` (in `bbox-crs`), a spatial `subset` (in
//!   `subset-crs`) and `center` (in `center-crs`); each CRS parameter is
//!   CRS84 by default and ignored without its parameter. Without any, the
//!   map covers the collection's whole spatial extent (`/rec/core/map-op`).
//!   A `subset` naming one axis keeps the extent on the other, and `*` is the
//!   extent's edge.
//! - **Size.** `width` and `height` resample a map over a given area; an
//!   omitted one keeps square pixels, and with both omitted the longer side
//!   is [`map_frame::DEFAULT_MAP_SIZE`] pixels. With `center`, or with
//!   `scale-denominator` and no area, they size the map and the area follows
//!   from the scale — `scale-denominator`, else the collection's native
//!   resolution — around the centre (default: the extent's); omitted ones
//!   are the other's value, or [`map_frame::DEFAULT_MAP_SIZE`] for both.
//!   `scale-denominator` with an area and no size derives the size from the
//!   scale. Every size, given or derived, is held to [`MAX_MAP_DIMENSION`]
//!   and [`MAX_MAP_PIXELS`].
//! - **Time.** `datetime` (an instant or interval) or `subset=time(…)`, not
//!   both. An instant snaps as it always has; an interval renders the latest
//!   time inside it.

use api_common::map_frame::{self, Frame, MapCrs};
use api_common::subset::{self, Axis, SubsetRange, SubsetValue, TimeSelection};
use ds_core::map_engine::{OutputCrs, RasterInfo};
use ds_core::web_mercator;
use serde::Deserialize;

use crate::error::MapsError;

/// Output-pixel cap of one map: `width × height` of the returned image.
///
/// It bounds the output only, so passing it guarantees neither of the budgets
/// behind it ("Pixel budgets" in the root CLAUDE.md, #120): engine-geotiff's
/// `reader::MAX_MAP_PIXELS` has the same value but counts native *source*
/// pixels, and render admission charges 32 B per output pixel against
/// `MC_RENDER_MEMORY_MB`, whose 1024 MiB default admits at most 33 554 432.
///
/// Enforced in [`MapRequest::view`] on the given or derived size: HTTP 400
/// `BadRequest` "width * height (N) exceeds maximum of 64000000".
/// [`MAX_MAP_DIMENSION`]² equals this cap, so the per-side check always fires
/// first today; this one only guards a future per-side increase.
pub const MAX_MAP_PIXELS: u64 = 64_000_000;

/// Output-pixel cap per side (width or height). 8000 chosen so 8000 × 8000
/// equals MAX_MAP_PIXELS — a square at the per-dim cap doesn't trip the
/// pixel cap with a confusing second error. Tripping it is HTTP 400
/// `BadRequest` "width and height must not exceed 8000".
pub const MAX_MAP_DIMENSION: u32 = 8000;

/// Supported output formats.
///
/// `image/png` auto-selects an 8-bit indexed-palette encoding ("PNG8") when
/// the rendered image carries ≤256 distinct colours (every colormap layer);
/// the encoder falls back to 32-bit RGBA above that. Content-type is
/// `image/png` either way — clients can't tell, and no second `f=` value is
/// needed.
const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// `subset` axes of the map routes (`/req/spatial-subsetting/subset-definition`
/// B with the aliases of `/rec/spatial-subsetting/subset-crs-axis-names`, and
/// `/req/datetime/axis` with the aliases of `/req/datetime/subset-definition`
/// C). Names match without regard to case.
const LON: Axis<'static> = Axis {
    name: "Lon",
    aliases: &["Long", "Longitude"],
};
const LAT: Axis<'static> = Axis {
    name: "Lat",
    aliases: &["Latitude"],
};
const EASTING: Axis<'static> = Axis {
    name: "E",
    aliases: &["X", "Easting"],
};
const NORTHING: Axis<'static> = Axis {
    name: "N",
    aliases: &["Y", "Northing"],
};
const TIME: Axis<'static> = Axis {
    name: "time",
    aliases: &["t"],
};
const SUBSET_AXES: [Axis<'static>; 5] = [LON, LAT, EASTING, NORTHING, TIME];

/// Query parameters for OGC API Maps get_map / get_styled_map endpoints.
/// `subset` may repeat, so the handler collects it from the raw query.
#[derive(Debug, Default, Deserialize)]
pub struct MapQueryParams {
    pub bbox: Option<String>,
    /// Strings, so a bad value gets a JSON 400 naming the rule.
    pub width: Option<String>,
    pub height: Option<String>,
    pub crs: Option<String>,
    pub datetime: Option<String>,
    pub transparent: Option<String>,
    #[serde(rename = "f")]
    pub format: Option<String>,
    #[serde(rename = "bbox-crs")]
    pub bbox_crs: Option<String>,
    pub center: Option<String>,
    #[serde(rename = "center-crs")]
    pub center_crs: Option<String>,
    #[serde(rename = "subset-crs")]
    pub subset_crs: Option<String>,
    #[serde(rename = "scale-denominator")]
    pub scale_denominator: Option<String>,
    /// EDR-style parameter selector for multi-parameter raster engines
    /// (GRIB, multi-param QueryData). Non-OGC for now — a standardised
    /// path/query form is on the OGC Maps roadmap and will replace this.
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    /// Vertical level selector (e.g. a radar elevation angle). Rejected
    /// with HTTP 400 for collections with no vertical dimension.
    pub elevation: Option<String>,
    /// Encoder quality (MeteoCore extension): 1–100 for `image/webp`
    /// (100 = lossless) and `image/jpeg`; rejected for PNG. A string so a
    /// bad value gets the range message instead of a deserialize error.
    pub quality: Option<String>,
}

/// Query parameters for the style legend endpoint.
#[derive(Debug, Deserialize)]
pub struct LegendQueryParams {
    #[serde(rename = "f")]
    pub format: Option<String>,
    /// Selects the per-parameter style layer, matching `parameter-name` on
    /// the map routes — without it the legend would describe the
    /// collection-level colormap while the map renders the parameter's own.
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
}

/// Representation a legend request asked for.
pub enum LegendFormat {
    /// Machine-readable description: palette stops, range, interpolation.
    Json,
    /// The rendered legend image.
    Png,
}

impl LegendQueryParams {
    /// Resolve `?f=`; JSON is the default because the legend endpoint exists
    /// for clients that draw their own legend. Both the short token and the
    /// media type are accepted, mirroring how `f=` is spelled elsewhere in
    /// this API (`image/png`) and in the other OGC metadata endpoints (`json`).
    pub fn validate(&self) -> Result<LegendFormat, MapsError> {
        match self
            .format
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None | Some("json") | Some("application/json") => Ok(LegendFormat::Json),
            Some("png") | Some("image/png") => Ok(LegendFormat::Png),
            Some(other) => Err(MapsError::BadRequest(format!(
                "Format '{other}' is not supported for a legend. Supported: json, png"
            ))),
        }
    }
}

/// One spatial `subset` interval; `None` is `*`, the extent's edge.
pub type SubsetInterval = (Option<f64>, Option<f64>);

/// The area a map request selects.
#[derive(Debug, Clone, PartialEq)]
pub enum Area {
    /// No `bbox`, spatial `subset` or `center`: the collection's extent.
    Default,
    /// `bbox` as a plane box of its `bbox-crs`: `[min_x, min_y, max_x,
    /// max_y]`, longitude first, a box crossing the antimeridian unwrapped.
    Bbox { crs: MapCrs, plane: [f64; 4] },
    /// Spatial `subset` intervals along the plane axes of `subset-crs`.
    Subset {
        crs: MapCrs,
        x: Option<SubsetInterval>,
        y: Option<SubsetInterval>,
    },
    /// `center` as a plane point of its `center-crs`.
    Center { crs: MapCrs, x: f64, y: f64 },
}

/// The time a map request selects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapTime {
    pub selection: TimeSelection,
    /// From `subset=time(…)`, where an instant outside the time axis is a
    /// 404 (`/req/datetime/subset-definition` D) instead of snapping.
    pub from_subset: bool,
}

/// A validated map request: everything but what needs the collection.
#[derive(Debug)]
pub struct MapRequest {
    /// `crs`; `None` is the collection's storage CRS, else CRS84.
    pub crs: Option<MapCrs>,
    pub area: Area,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub scale_denominator: Option<f64>,
    pub time: Option<MapTime>,
    /// Output format at its default quality (JPEG 85, lossless WebP); the
    /// handler applies [`Self::quality`] and the collection's `webp_quality`.
    pub format: ds_render::ImageFormat,
    /// The `quality` parameter, 1–100, when supplied. Always `None` for PNG,
    /// which rejects it.
    pub quality: Option<u8>,
    pub parameter_name: Option<String>,
    /// Vertical level, parsed from the `elevation` query parameter.
    pub z: Option<f64>,
}

/// The map a request renders over a collection.
#[derive(Debug)]
pub struct MapView {
    pub frame: Frame,
    pub width: u32,
    pub height: u32,
    /// The `get_raster_tile` viewport (longitude/latitude) and output CRS.
    pub bbox: [f64; 4],
    pub output_crs: OutputCrs,
    /// The `Content-Bbox` header value.
    pub content_bbox: String,
}

fn bad(message: impl Into<String>) -> MapsError {
    MapsError::BadRequest(message.into())
}

/// Parse a CRS parameter, a 400 naming the valid values otherwise.
fn parse_crs(name: &str, value: &str) -> Result<MapCrs, MapsError> {
    MapCrs::parse(value).ok_or_else(|| {
        let codes: Vec<&str> = MapCrs::ALL.into_iter().map(MapCrs::code).collect();
        bad(format!(
            "{name} '{value}' is not supported. Supported: {} (or their URIs: {})",
            codes.join(", "),
            MapCrs::uris().join(", ")
        ))
    })
}

/// A present, non-blank parameter value.
fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// `width`/`height`: a positive integer up to [`MAX_MAP_DIMENSION`]
/// (`/req/scaling/width-definition` C).
fn parse_dimension(name: &str, value: Option<&str>) -> Result<Option<u32>, MapsError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let n = raw
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| bad(format!("{name} '{raw}' must be a positive integer")))?;
    if n > MAX_MAP_DIMENSION {
        return Err(bad(format!(
            "width and height must not exceed {MAX_MAP_DIMENSION}"
        )));
    }
    Ok(Some(n))
}

/// Comma-separated finite numbers.
fn numbers(name: &str, value: &str) -> Result<Vec<f64>, MapsError> {
    value
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| bad(format!("Invalid {name} value: '{s}'")))
        })
        .collect()
}

impl MapQueryParams {
    /// Validate the request on its own. `subsets` are the `subset` values in
    /// request order.
    pub fn validate<S: AsRef<str>>(&self, subsets: &[S]) -> Result<MapRequest, MapsError> {
        // FORMAT — default image/png
        let format_str = self.format.as_deref().unwrap_or("image/png");
        if !SUPPORTED_FORMATS.contains(&format_str) {
            return Err(bad(format!(
                "Format '{format_str}' is not supported. Supported: {}",
                SUPPORTED_FORMATS.join(", ")
            )));
        }
        let format = match format_str {
            "image/jpeg" => ds_render::ImageFormat::JPEG,
            "image/webp" => ds_render::ImageFormat::WEBP,
            _ => ds_render::ImageFormat::Png,
        };
        // QUALITY — JPEG/WebP only; a value on PNG is an error, not ignored.
        let quality = ds_render::parse_quality("quality", self.quality.as_deref(), format)
            .map_err(MapsError::BadRequest)?;

        let crs = present(&self.crs)
            .map(|value| parse_crs("CRS", value))
            .transpose()?;
        let width = parse_dimension("width", present(&self.width))?;
        let height = parse_dimension("height", present(&self.height))?;
        let scale_denominator = present(&self.scale_denominator)
            .map(|raw| {
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .ok_or_else(|| {
                        bad(format!(
                            "scale-denominator '{raw}' must be a positive number"
                        ))
                    })
            })
            .transpose()?;

        // SUBSET — spatial axes and `time`; any other axis is a 400.
        let ranges = subset::by_axis(
            subset::parse(subsets).map_err(MapsError::BadRequest)?,
            &SUBSET_AXES,
        )
        .map_err(MapsError::BadRequest)?;
        let spatial_subset = parse_spatial_subset(&ranges, present(&self.subset_crs))?;

        // The area: at most one of bbox, a spatial subset and center
        // (`/req/spatial-subsetting/bbox-definition` C, `center-definition`
        // B, `subset-definition` G). Each CRS parameter is ignored without
        // its parameter (`bbox-crs` F, `center-crs` F, `subset-crs` F).
        let bbox = present(&self.bbox);
        let center = present(&self.center);
        match (bbox.is_some(), spatial_subset.is_some(), center.is_some()) {
            (true, true, _) => {
                return Err(bad(
                    "bbox and a spatial subset cannot be used together: both set the map area",
                ))
            }
            (true, _, true) => {
                return Err(bad(
                    "bbox and center cannot be used together: both set the map area",
                ))
            }
            (_, true, true) => {
                return Err(bad(
                    "center and a spatial subset cannot be used together: both set the map area",
                ))
            }
            _ => {}
        }
        // `/req/scaling/scale-denominator-definition` D, `width-definition`
        // F, `height-definition` F.
        if scale_denominator.is_some()
            && (width.is_some() || height.is_some())
            && (bbox.is_some() || spatial_subset.is_some())
        {
            return Err(bad(
                "scale-denominator cannot be combined with width or height when bbox or a \
                 spatial subset sets the map area",
            ));
        }
        let area = if let Some(value) = bbox {
            let crs = present(&self.bbox_crs)
                .map(|v| parse_crs("bbox-crs", v))
                .transpose()?
                .unwrap_or(MapCrs::Crs84);
            Area::Bbox {
                crs,
                plane: parse_bbox(value, crs)?,
            }
        } else if let Some(value) = center {
            let crs = present(&self.center_crs)
                .map(|v| parse_crs("center-crs", v))
                .transpose()?
                .unwrap_or(MapCrs::Crs84);
            let (x, y) = parse_center(value, crs)?;
            Area::Center { crs, x, y }
        } else if let Some(area) = spatial_subset {
            area
        } else {
            Area::Default
        };

        // TIME — `datetime` or `subset=time(…)`.
        let datetime = present(&self.datetime)
            .map(|v| TimeSelection::from_datetime(v).map_err(MapsError::BadRequest))
            .transpose()?;
        let time_subset = ranges
            .get(TIME.name)
            .map(|range| TimeSelection::from_subset(range).map_err(MapsError::BadRequest))
            .transpose()?;
        let time = match (datetime, time_subset) {
            (Some(_), Some(_)) => {
                return Err(bad(
                    "datetime and subset=time cannot be used together: both select the time",
                ))
            }
            (Some(selection), None) => Some(MapTime {
                selection,
                from_subset: false,
            }),
            (None, Some(selection)) => Some(MapTime {
                selection,
                from_subset: true,
            }),
            (None, None) => None,
        };

        // parameter-name — validation that the name is in the engine's list
        // happens in the handler (we don't have the engine here). Just trim and
        // reject empty/blank to keep handler logic simple.
        let parameter_name = present(&self.parameter_name).map(str::to_string);

        // ELEVATION — a single vertical level. Multi-value selection is an
        // EDR concern; a map renders exactly one layer.
        let z = match present(&self.elevation) {
            Some(raw) => Some(
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| bad(format!("elevation '{raw}' is not a finite number")))?,
            ),
            None => None,
        };

        Ok(MapRequest {
            crs,
            area,
            width,
            height,
            scale_denominator,
            time,
            format,
            quality,
            parameter_name,
            z,
        })
    }
}

/// The spatial part of `subset`, as an [`Area::Subset`] in `subset-crs`;
/// `None` when it names no spatial axis (`subset-crs` is then ignored).
fn parse_spatial_subset(
    ranges: &std::collections::BTreeMap<&str, SubsetRange>,
    subset_crs: Option<&str>,
) -> Result<Option<Area>, MapsError> {
    let used: Vec<&str> = [LON.name, LAT.name, EASTING.name, NORTHING.name]
        .into_iter()
        .filter(|axis| ranges.contains_key(axis))
        .collect();
    if used.is_empty() {
        return Ok(None);
    }
    let crs = subset_crs
        .map(|v| parse_crs("subset-crs", v))
        .transpose()?
        .unwrap_or(MapCrs::Crs84);
    let (x_axis, y_axis) = crs.axis_abbreviations();
    // `/req/spatial-subsetting/subset-definition` D: only the axes of the
    // subsetting CRS.
    if let Some(other) = used.iter().find(|a| **a != x_axis && **a != y_axis) {
        return Err(bad(format!(
            "subset axis '{other}' is not an axis of subset-crs {}: use {x_axis} and {y_axis}",
            crs.code()
        )));
    }
    let interval = |axis: &str| -> Result<Option<SubsetInterval>, MapsError> {
        let Some(range) = ranges.get(axis) else {
            return Ok(None);
        };
        let bound = |value: &SubsetValue| match value {
            SubsetValue::Number(v) => Ok(Some(*v)),
            SubsetValue::Star => Ok(None),
            SubsetValue::Text(_) => Err(bad(format!(
                "subset {axis} takes numbers or '*', not quoted text"
            ))),
        };
        match range {
            SubsetRange::Interval(low, high) => Ok(Some((bound(low)?, bound(high)?))),
            // A 2D map trims its spatial axes, never slices them (the
            // standard's note to `subset-definition`).
            SubsetRange::Single(_) => Err(bad(format!(
                "subset {axis} needs an interval low:high; a map cannot be a single slice of a \
                 spatial axis"
            ))),
        }
    };
    Ok(Some(Area::Subset {
        crs,
        x: interval(x_axis)?,
        y: interval(y_axis)?,
    }))
}

/// Parse `bbox` given in `crs` (its own axis order) into a plane box of
/// `crs`: `[min_x, min_y, max_x, max_y]`, longitude or easting first.
///
/// For longitude/latitude, `west > east` is a box crossing the antimeridian,
/// as OGC API Maps and Features define it: `170,10,-170,20` is the 20°-wide
/// box over the seam, not a 340°-wide one. It is unwrapped to a continuous
/// viewport, `east + 360`, so the engine receives `[170, 10, 190, 20]`: the
/// same request as `170,10,190,20`, which was already accepted, and the form
/// WMS sends for a CRS:84 box past 180°. Each engine maps such longitudes to
/// the meridian they show; one that does not renders nodata past 180°, never
/// an error. Both longitudes of a crossing box must lie in `[-180, 180]`,
/// the domain where the convention is defined. A `west < east` viewport is
/// passed through as is, never clamped, including one reaching past ±180°
/// (root CLAUDE.md Critical Rule 4).
///
/// A projected CRS has no such reading: its minimum must lie below its
/// maximum on both axes. Six values (a three-dimensional box) are rejected:
/// a map here is two-dimensional.
fn parse_bbox(bbox_str: &str, crs: MapCrs) -> Result<[f64; 4], MapsError> {
    let parts = numbers("bbox", bbox_str)?;
    if parts.len() == 6 {
        return Err(bad(
            "bbox: six values describe a three-dimensional box; these maps are \
             two-dimensional, give four values",
        ));
    }
    let [a0, b0, a1, b1] = parts[..] else {
        return Err(bad(
            "bbox must have exactly 4 values: west,south,east,north",
        ));
    };
    let (west, south) = crs.to_plane(a0, b0);
    let (mut east, north) = crs.to_plane(a1, b1);

    if !crs.is_geographic() {
        if west >= east || south >= north {
            return Err(bad(format!(
                "bbox: in {} the lower corner must lie below and left of the upper corner",
                crs.code()
            )));
        }
        return Ok([west, south, east, north]);
    }

    if south >= north {
        return Err(bad("bbox: south must be less than north"));
    }

    if west > east {
        if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) {
            return Err(bad(
                "bbox: west greater than east is a box crossing the antimeridian, \
                 which needs both longitudes within [-180, 180]",
            ));
        }
        east += 360.0;
    }

    // Equal longitudes, or `180,…,-180,…` once unwrapped: a box of no width.
    if west >= east {
        return Err(bad(
            "bbox: west and east must differ; west greater than east is a box \
             crossing the antimeridian",
        ));
    }

    Ok([west, south, east, north])
}

/// Parse `center` given in `crs` (its own axis order) into a plane point.
fn parse_center(value: &str, crs: MapCrs) -> Result<(f64, f64), MapsError> {
    let [a, b] = numbers("center", value)?[..] else {
        return Err(bad(
            "center must be two comma-separated coordinates in center-crs",
        ));
    };
    let (x, y) = crs.to_plane(a, b);
    if crs.is_geographic() && !(-90.0..=90.0).contains(&y) {
        return Err(bad("center: latitude must be within [-90, 90]"));
    }
    Ok((x, y))
}

/// The collection's advertised spatial extent as a longitude/latitude
/// viewport (a box crossing the antimeridian unwrapped), `None` when it has
/// no area.
fn default_extent(info: &RasterInfo) -> Option<[f64; 4]> {
    let [west, south, mut east, north] = ds_core::geo::crs84_extent(info.spatial_extent?)?;
    if west > east {
        east += 360.0;
    }
    (west < east && south < north).then_some([west, south, east, north])
}

/// The extent's frame in `crs`. A Web Mercator frame keeps to the square
/// world, ±[`web_mercator::LAT_LIMIT_DEG`]: Mercator has no northing at a
/// pole. This bounds a frame the server chooses and reports in
/// `Content-Bbox`, never a requested viewport (Critical Rule 4).
fn extent_frame(extent: [f64; 4], crs: MapCrs) -> Result<Frame, MapsError> {
    let [west, mut south, east, mut north] = extent;
    if crs == MapCrs::Epsg3857 {
        south = south.max(-web_mercator::LAT_LIMIT_DEG);
        north = north.min(web_mercator::LAT_LIMIT_DEG);
        if south >= north {
            return Err(MapsError::NotFound(
                "the collection's extent lies outside the Web Mercator world".into(),
            ));
        }
    }
    Frame::from_box(MapCrs::Crs84, [west, south, east, north], crs).map_err(|_| {
        MapsError::NotFound(format!(
            "the collection's extent lies outside the valid area of {}",
            crs.code()
        ))
    })
}

/// Round a size to whole pixels and hold it to the caps: `cause` names what
/// set it, for the message when a derived size is too large.
fn pixels((width, height): (f64, f64), cause: &str) -> Result<(u32, u32), MapsError> {
    let (width, height) = (width.round().max(1.0), height.round().max(1.0));
    if !(width.is_finite() && height.is_finite()) {
        return Err(bad(format!("{cause} gives no finite map size")));
    }
    if width > f64::from(MAX_MAP_DIMENSION) || height > f64::from(MAX_MAP_DIMENSION) {
        return Err(bad(format!(
            "width and height must not exceed {MAX_MAP_DIMENSION}: {cause} gives a \
             {width}x{height} map"
        )));
    }
    let (width, height) = (width as u32, height as u32);
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_MAP_PIXELS {
        return Err(bad(format!(
            "width * height ({pixels}) exceeds maximum of {MAX_MAP_PIXELS}"
        )));
    }
    Ok((width, height))
}

/// The size of a map whose area follows from its size: the given sides, a
/// missing one equal to the other, [`map_frame::DEFAULT_MAP_SIZE`] for both
/// (`/rec/scaling/dimensions` A).
fn square(width: Option<u32>, height: Option<u32>) -> (u32, u32) {
    match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(side), None) | (None, Some(side)) => (side, side),
        (None, None) => (map_frame::DEFAULT_MAP_SIZE, map_frame::DEFAULT_MAP_SIZE),
    }
}

/// Plane units per pixel at the collection's native resolution: its extent
/// frame over its grid, or over the default size when it has none.
fn native_resolution(extent: &Frame, grid: Option<[u32; 2]>) -> (f64, f64) {
    let [x0, y0, x1, y1] = extent.plane();
    let (columns, rows) = match grid {
        Some([columns, rows]) if columns > 0 && rows > 0 => (f64::from(columns), f64::from(rows)),
        _ => map_frame::size_for_aspect(extent.aspect(), None, None),
    };
    ((x1 - x0) / columns, (y1 - y0) / rows)
}

impl MapRequest {
    /// The CRS the map renders in: `crs`, else the storage CRS when a map
    /// can be rendered in it, else CRS84 (`/req/core/map-response` B).
    pub fn output_crs(&self, info: &RasterInfo) -> MapCrs {
        self.crs
            .or_else(|| MapCrs::from_storage_label(&info.native_crs))
            .unwrap_or(MapCrs::Crs84)
    }

    /// Resolve the map's frame and size over a collection.
    pub fn view(&self, info: &RasterInfo, collection_id: &str) -> Result<MapView, MapsError> {
        let out = self.output_crs(info);
        let extent = || {
            default_extent(info).ok_or_else(|| {
                MapsError::NotFound(format!(
                    "Collection '{collection_id}' advertises no spatial extent yet; \
                     request an explicit bbox"
                ))
            })
        };
        let scale_error = || {
            bad(
                "scale-denominator cannot be applied at the map centre: the CRS has no local \
                 scale there",
            )
        };
        // A frame the request (or the extent) sets, sized from its aspect or
        // from `scale-denominator`.
        let sized = |frame: Frame| -> Result<(Frame, u32, u32), MapsError> {
            let (width, height) = match self.scale_denominator {
                Some(scale) => {
                    let (x, y) = frame.center();
                    let (rx, ry) =
                        map_frame::resolution_at(out, x, y, scale).ok_or_else(scale_error)?;
                    pixels(
                        map_frame::size_for_resolution(&frame, rx, ry),
                        "scale-denominator over this area",
                    )?
                }
                None => pixels(
                    map_frame::size_for_aspect(frame.aspect(), self.width, self.height),
                    "the area's aspect ratio",
                )?,
            };
            Ok((frame, width, height))
        };
        // A frame the size sets, around a plane point of the output CRS.
        let around = |(x, y): (f64, f64)| -> Result<(Frame, u32, u32), MapsError> {
            let resolution = match self.scale_denominator {
                Some(scale) => {
                    map_frame::resolution_at(out, x, y, scale).ok_or_else(scale_error)?
                }
                None => native_resolution(&extent_frame(extent()?, out)?, info.grid_size),
            };
            let (width, height) = square(self.width, self.height);
            Ok((
                map_frame::frame_around(out, (x, y), resolution, width, height),
                width,
                height,
            ))
        };

        let (frame, width, height) = match &self.area {
            Area::Bbox { crs, plane } => {
                sized(Frame::from_box(*crs, *plane, out).map_err(|_| {
                    bad(format!(
                        "bbox is outside the valid area of bbox-crs {}",
                        crs.code()
                    ))
                })?)?
            }
            Area::Subset { crs, x, y } => {
                let plane =
                    subset_plane(*crs, *x, *y, || Ok(extent_frame(extent()?, *crs)?.plane()))?;
                sized(Frame::from_box(*crs, plane, out).map_err(|_| {
                    MapsError::NotFound(format!(
                        "the subset lies entirely outside the valid area of subset-crs {}",
                        crs.code()
                    ))
                })?)?
            }
            Area::Center { crs, x, y } => {
                let (lon, lat) = crs.inverse(*x, *y).ok_or_else(|| {
                    bad(format!(
                        "center is outside the valid area of center-crs {}",
                        crs.code()
                    ))
                })?;
                let (x, y) = out.forward(lon, lat);
                if !(x.is_finite() && y.is_finite()) {
                    return Err(bad(format!("center has no coordinates in {}", out.code())));
                }
                around((x, y))?
            }
            Area::Default => match self.scale_denominator {
                Some(_) => around(extent_frame(extent()?, out)?.center())?,
                None => sized(extent_frame(extent()?, out)?)?,
            },
        };

        let content_bbox = frame.content_bbox().ok_or_else(|| {
            bad(format!(
                "the map area has no valid coordinates in {}: Web Mercator needs latitudes \
                 strictly between -90 and 90",
                out.code()
            ))
        })?;
        // None means the projected frame is entirely outside the CRS's valid
        // domain — reject (400) rather than reading a global window.
        let (bbox, output_crs) = frame
            .render_target()
            .ok_or_else(|| bad("bbox is outside the valid area of the requested crs"))?;
        Ok(MapView {
            frame,
            width,
            height,
            bbox,
            output_crs,
            content_bbox,
        })
    }
}

/// The plane box a spatial subset selects in its `crs`: each given interval,
/// the extent's (`base`) on an axis the subset leaves out or at a `*`.
///
/// Longitude follows `bbox`'s antimeridian rule (`low > high` crosses it,
/// both within [-180, 180]; `/req/spatial-subsetting/subset-definition` F).
/// An interval entirely outside its axis' valid values — latitude beyond
/// ±90°, longitude beyond ±180°, Web Mercator beyond its square world — is a
/// 404 (`subset-definition` E).
fn subset_plane(
    crs: MapCrs,
    x: Option<SubsetInterval>,
    y: Option<SubsetInterval>,
    base: impl FnOnce() -> Result<[f64; 4], MapsError>,
) -> Result<[f64; 4], MapsError> {
    let complete = |r: Option<SubsetInterval>| matches!(r, Some((Some(_), Some(_))));
    let base = if complete(x) && complete(y) {
        [f64::NAN; 4]
    } else {
        base()?
    };
    let pick = |r: Option<SubsetInterval>, low: f64, high: f64| match r {
        None => (low, high),
        Some((l, h)) => (l.unwrap_or(low), h.unwrap_or(high)),
    };
    let (x0, mut x1) = pick(x, base[0], base[2]);
    let (y0, y1) = pick(y, base[1], base[3]);
    let (x_axis, y_axis) = crs.axis_abbreviations();
    let outside = |axis: &str, low: f64, high: f64, limit: f64| {
        MapsError::NotFound(format!(
            "subset {axis}({low}:{high}) lies entirely outside the valid values \
             {}..{limit} of the axis",
            -limit
        ))
    };

    if crs.is_geographic() {
        if y0 >= y1 {
            return Err(bad(format!("subset {y_axis}: low must be less than high")));
        }
        if y1 < -90.0 || y0 > 90.0 {
            return Err(outside(y_axis, y0, y1, 90.0));
        }
        if x0 > x1 {
            if !(-180.0..=180.0).contains(&x0) || !(-180.0..=180.0).contains(&x1) {
                return Err(bad(format!(
                    "subset {x_axis}: low greater than high crosses the antimeridian, which \
                     needs both values within [-180, 180]"
                )));
            }
            x1 += 360.0;
        }
        if x0 >= x1 {
            return Err(bad(format!("subset {x_axis}: low and high must differ")));
        }
        if x1 < -180.0 || x0 > 180.0 {
            return Err(outside(x_axis, x0, x1, 180.0));
        }
    } else {
        if x0 >= x1 || y0 >= y1 {
            return Err(bad(format!(
                "subset {x_axis} and {y_axis}: low must be less than high"
            )));
        }
        if crs == MapCrs::Epsg3857 {
            // Half the square world's side: π·R.
            let limit = std::f64::consts::PI * web_mercator::EARTH_RADIUS;
            if x1 < -limit || x0 > limit {
                return Err(outside(x_axis, x0, x1, limit));
            }
            if y1 < -limit || y0 > limit {
                return Err(outside(y_axis, y0, y1, limit));
            }
        }
    }
    Ok([x0, y0, x1, y1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crs84(bbox: &str) -> Result<[f64; 4], MapsError> {
        parse_bbox(bbox, MapCrs::Crs84)
    }

    #[test]
    fn test_parse_bbox_valid() {
        let bbox = crs84("10,55,30,70").unwrap();
        assert_eq!(bbox, [10.0, 55.0, 30.0, 70.0]);
    }

    #[test]
    fn test_parse_bbox_invalid_count() {
        assert!(crs84("10,55,30").is_err());
        // Six values are a 3D box: not for a 2D map.
        assert!(crs84("10,55,0,30,70,100").is_err());
    }

    /// #828: `west > east` is a CRS84 box crossing the antimeridian, unwrapped
    /// to a continuous viewport past 180°. The seam test box first.
    #[test]
    fn test_parse_bbox_antimeridian_is_unwrapped() {
        assert_eq!(crs84("170,10,-170,20").unwrap(), [170.0, 10.0, 190.0, 20.0]);
        // GOES-West's advertised fixture extent, requested back as is.
        let [w, s, e, n] = crs84("173.9,11.2,-174.8,16.3").unwrap();
        assert_eq!([w, s, n], [173.9, 11.2, 16.3]);
        assert!((e - 185.2).abs() < 1e-9, "{e}");
        // The domain edges: a box from 180° across to 179°W, and 30°..10° is
        // the 340°-wide box the long way round, not an inverted one.
        assert_eq!(crs84("180,0,-179,1").unwrap(), [180.0, 0.0, 181.0, 1.0]);
        assert_eq!(crs84("30,55,10,70").unwrap(), [30.0, 55.0, 370.0, 70.0]);
        // EPSG:4326 is latitude first; the same seam box.
        assert_eq!(
            parse_bbox("10,170,20,-170", MapCrs::Epsg4326).unwrap(),
            [170.0, 10.0, 190.0, 20.0]
        );
    }

    /// A `west < east` viewport is never clamped or wrapped, even past ±180°.
    #[test]
    fn test_parse_bbox_past_180_is_passed_through() {
        assert_eq!(crs84("170,10,190,20").unwrap(), [170.0, 10.0, 190.0, 20.0]);
        assert_eq!(
            crs84("-200,-10,200,10").unwrap(),
            [-200.0, -10.0, 200.0, 10.0]
        );
    }

    #[test]
    fn test_parse_bbox_rejects_degenerate_and_inverted_boxes() {
        // Zero width, directly or once unwrapped.
        assert!(crs84("10,55,10,70").is_err());
        assert!(crs84("180,55,-180,70").is_err());
        // south >= north stays an error, with or without a crossing.
        assert!(crs84("10,70,30,55").is_err());
        assert!(crs84("10,55,30,55").is_err());
        assert!(crs84("170,20,-170,10").is_err());
        // A crossing needs both longitudes in the CRS84 domain.
        assert!(crs84("190,10,-170,20").is_err());
        assert!(crs84("170,10,-190,20").is_err());
        assert!(crs84("200,10,100,20").is_err());
        // A projected box has no crossing: min > max is an error.
        assert!(parse_bbox("2000000,1000000,-2000000,2000000", MapCrs::Epsg3857).is_err());
        assert!(parse_bbox("300000,6900000,500000,6700000", MapCrs::Epsg3067).is_err());
        // EPSG:3035 is northing first.
        assert_eq!(
            parse_bbox("3000000,4000000,3500000,4500000", MapCrs::Epsg3035).unwrap(),
            [4_000_000.0, 3_000_000.0, 4_500_000.0, 3_500_000.0]
        );
    }

    #[test]
    fn test_parse_bbox_nan() {
        assert!(crs84("NaN,0,1,1").is_err());
    }

    fn query_with_crs(crs: &str) -> MapQueryParams {
        MapQueryParams {
            // CRS:84 bbox over Finland (bbox-crs defaults to CRS:84).
            bbox: Some("19,59,32,70".to_string()),
            width: Some("256".into()),
            height: Some("256".into()),
            crs: Some(crs.to_string()),
            ..Default::default()
        }
    }

    const NONE: [&str; 0] = [];

    fn info() -> RasterInfo {
        RasterInfo {
            native_crs: "CRS:84".into(),
            spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
            times: Vec::new(),
            parameter: String::new(),
            unit: String::new(),
            parameters: Vec::new(),
            vertical: None,
            grid_size: Some([2000, 1500]),
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }

    fn view(query: MapQueryParams, subsets: &[&str]) -> Result<MapView, MapsError> {
        query.validate(subsets)?.view(&info(), "radar")
    }

    #[test]
    fn validate_projected_output_crs_3067() {
        // #160: a projected output CRS must produce OutputCrs::Projected, not a
        // silent Wgs84 fallback. The bbox stays CRS:84 (bbox-crs), but the
        // engine read window widens to the projected frame's WGS84 envelope.
        let view = view(query_with_crs("EPSG:3067"), &NONE).unwrap();
        match view.output_crs {
            OutputCrs::Projected { ref crs, bbox } => {
                assert!(matches!(crs, ds_core::geo::Crs::TransverseMercator { .. }));
                // Projected envelope of the CRS:84 box: easting near the 500 km
                // false-easting band, northing in the millions of metres.
                assert!(bbox[1] > 5_000_000.0 && bbox[3] > 6_000_000.0, "{bbox:?}");
            }
            other => panic!("expected Projected, got {other:?}"),
        }
        // The read window stays in plausible WGS84 degrees.
        let [w, s, e, n] = view.bbox;
        assert!(
            w > 10.0 && e < 40.0 && s > 55.0 && n < 75.0,
            "{:?}",
            view.bbox
        );
    }

    #[test]
    fn quality_is_parsed_for_webp_and_jpeg() {
        for (f, q) in [
            ("image/webp", "75"),
            ("image/jpeg", "40"),
            ("image/webp", "100"),
        ] {
            let mut query = query_with_crs("CRS:84");
            query.format = Some(f.into());
            query.quality = Some(q.into());
            let validated = query.validate(&NONE).unwrap();
            assert_eq!(validated.quality, Some(q.parse().unwrap()), "{f}");
            assert_eq!(validated.format, validated.format.with_default_quality());
        }
        let mut query = query_with_crs("CRS:84");
        query.format = Some("image/webp".into());
        assert_eq!(query.validate(&NONE).unwrap().quality, None);
    }

    #[test]
    fn quality_out_of_range_or_on_png_is_400() {
        let bad = |f: &str, q: &str| {
            let mut query = query_with_crs("CRS:84");
            query.format = Some(f.into());
            query.quality = Some(q.into());
            match query.validate(&NONE) {
                Err(MapsError::BadRequest(msg)) => msg,
                Err(other) => panic!("{f} {q}: expected BadRequest, got {other:?}"),
                Ok(_) => panic!("{f} {q}: expected an error"),
            }
        };
        for q in ["0", "101", "x", "7.5"] {
            assert_eq!(
                bad("image/webp", q),
                format!("quality '{q}' must be an integer from 1 to 100")
            );
        }
        assert_eq!(
            bad("image/png", "80"),
            "quality applies only to image/jpeg and image/webp, not image/png"
        );
        // f defaults to PNG, so a bare quality is rejected too.
        let mut query = query_with_crs("CRS:84");
        query.quality = Some("80".into());
        assert!(matches!(
            query.validate(&NONE),
            Err(MapsError::BadRequest(_))
        ));
    }

    #[test]
    fn validate_wgs84_and_webmercator_unchanged() {
        assert_eq!(
            view(query_with_crs("CRS:84"), &NONE).unwrap().output_crs,
            OutputCrs::Wgs84
        );
        assert_eq!(
            view(query_with_crs("EPSG:3857"), &NONE).unwrap().output_crs,
            OutputCrs::WebMercator
        );
        // The URI names the same CRS.
        assert_eq!(
            view(
                query_with_crs("http://www.opengis.net/def/crs/EPSG/0/3857"),
                &NONE
            )
            .unwrap()
            .output_crs,
            OutputCrs::WebMercator
        );
    }

    /// No area: the extent, longer side 1024 at square pixels.
    #[test]
    fn default_map_is_the_extent_at_the_default_size() {
        let view = view(MapQueryParams::default(), &NONE).unwrap();
        assert_eq!(view.bbox, [10.0, 55.0, 30.0, 70.0]);
        assert_eq!((view.width, view.height), (1024, 768));
        assert_eq!(view.content_bbox, "10,55,30,70");
        // One side given: the other keeps the aspect.
        let query = MapQueryParams {
            height: Some("300".into()),
            ..Default::default()
        };
        let view = super::tests::view(query, &NONE).unwrap();
        assert_eq!((view.width, view.height), (400, 300));
    }

    /// A storage CRS the map routes render in is the default output CRS.
    #[test]
    fn storage_crs_is_the_default_output_crs() {
        let info = RasterInfo {
            native_crs: "EPSG:3067".into(),
            ..info()
        };
        let request = MapQueryParams::default().validate(&NONE).unwrap();
        assert_eq!(request.output_crs(&info), MapCrs::Epsg3067);
        let view = request.view(&info, "radar").unwrap();
        assert!(matches!(view.output_crs, OutputCrs::Projected { .. }));
        let unlabelled = RasterInfo {
            native_crs: "TM".into(),
            ..info
        };
        assert_eq!(request.output_crs(&unlabelled), MapCrs::Crs84);
    }

    #[test]
    fn subset_selects_and_completes_the_area() {
        let view = |subsets: &[&str]| view(MapQueryParams::default(), subsets);
        let both = view(&["Lon(12:20),Lat(60:65)"]).unwrap();
        assert_eq!(both.bbox, [12.0, 60.0, 20.0, 65.0]);
        assert_eq!(both.content_bbox, "12,60,20,65");
        // Repeated parameters are one subset.
        assert_eq!(view(&["Lon(12:20)", "Lat(60:65)"]).unwrap().bbox, both.bbox);
        // A missing axis and `*` take the extent's.
        assert_eq!(
            view(&["Lat(60:65)"]).unwrap().bbox,
            [10.0, 60.0, 30.0, 65.0]
        );
        assert_eq!(
            view(&["Long(*:20)"]).unwrap().bbox,
            [10.0, 55.0, 20.0, 70.0]
        );
        // The seam test box.
        assert_eq!(
            view(&["Lon(170:-170),Lat(10:20)"]).unwrap().bbox,
            [170.0, 10.0, 190.0, 20.0]
        );
        // Entirely outside the axis: 404; inverted or sliced: 400.
        assert!(matches!(view(&["Lat(91:95)"]), Err(MapsError::NotFound(_))));
        assert!(matches!(
            view(&["Lon(181:190)"]),
            Err(MapsError::NotFound(_))
        ));
        for bad in [
            "Lat(65:60)",
            "Lat(60)",
            "Lon(190:-170)",
            "Lat(\"a\":2)",
            "E(1:2)",
        ] {
            assert!(
                matches!(view(&[bad]), Err(MapsError::BadRequest(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn subset_in_a_projected_subset_crs() {
        let x = web_mercator::lon_to_x(12.0);
        let y = web_mercator::lat_to_y(60.0);
        let query = MapQueryParams {
            subset_crs: Some("[EPSG:3857]".into()),
            ..Default::default()
        };
        let subset = format!("E({x}:{}),N({y}:*)", web_mercator::lon_to_x(20.0));
        let view = view(query, &[subset.as_str()]).unwrap();
        let [w, s, e, n] = view.bbox;
        assert!(
            (w - 12.0).abs() < 1e-9 && (s - 60.0).abs() < 1e-9,
            "{:?}",
            view.bbox
        );
        assert!(
            (e - 20.0).abs() < 1e-9 && (n - 70.0).abs() < 1e-9,
            "{:?}",
            view.bbox
        );
        // Lat is no axis of EPSG:3857.
        let query = MapQueryParams {
            subset_crs: Some("EPSG:3857".into()),
            ..Default::default()
        };
        assert!(matches!(
            query.validate(&["Lat(60:65)"]),
            Err(MapsError::BadRequest(_))
        ));
        // subset-crs is ignored without a spatial axis.
        let query = MapQueryParams {
            subset_crs: Some("EPSG:9999".into()),
            ..Default::default()
        };
        assert!(query.validate(&["time(*)"]).is_ok());
    }

    #[test]
    fn center_sizes_the_area_at_the_native_or_requested_scale() {
        // The mock grid is 0.01° per cell: 200 × 100 pixels span 2° × 1°.
        let query = MapQueryParams {
            center: Some("20,60".into()),
            width: Some("200".into()),
            height: Some("100".into()),
            ..Default::default()
        };
        let view = view(query, &NONE).unwrap();
        let [w, s, e, n] = view.bbox;
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(
            close(w, 19.0) && close(e, 21.0) && close(s, 59.5) && close(n, 60.5),
            "{:?}",
            view.bbox
        );
        assert_eq!((view.width, view.height), (200, 100));
        // Omitted sizes are square.
        let query = MapQueryParams {
            center: Some("20,60".into()),
            width: Some("300".into()),
            ..Default::default()
        };
        let view = super::tests::view(query, &NONE).unwrap();
        assert_eq!((view.width, view.height), (300, 300));
        // EPSG:4326 is latitude first.
        let query = MapQueryParams {
            center: Some("60,20".into()),
            center_crs: Some("EPSG:4326".into()),
            width: Some("200".into()),
            height: Some("100".into()),
            ..Default::default()
        };
        assert!(close(
            super::tests::view(query, &NONE).unwrap().bbox[0],
            19.0
        ));
        // 1:1 000 000 is 280 m a pixel: 100 pixels of latitude ≈ 0.25°.
        let query = MapQueryParams {
            center: Some("20,60".into()),
            width: Some("100".into()),
            height: Some("100".into()),
            scale_denominator: Some("1000000".into()),
            ..Default::default()
        };
        let [_, s, _, n] = super::tests::view(query, &NONE).unwrap().bbox;
        assert!(((n - s) - 0.2518).abs() < 1e-3, "{}", n - s);
    }

    #[test]
    fn scale_denominator_sizes_a_given_area() {
        // 1° of latitude ≈ 111.2 km ≈ 397 pixels of 280 m.
        let query = MapQueryParams {
            bbox: Some("20,60,22,61".into()),
            scale_denominator: Some("1000000".into()),
            ..Default::default()
        };
        let view = view(query, &NONE).unwrap();
        assert_eq!(view.height, 397);
        // At 60.5°N the 2° of longitude are ≈ 2 × 0.49 × 397 pixels.
        assert!((view.width as i32 - 391).abs() <= 1, "{}", view.width);
        // Too fine a scale for the area: the derived size is capped.
        let query = MapQueryParams {
            bbox: Some("10,55,30,70".into()),
            scale_denominator: Some("1000".into()),
            ..Default::default()
        };
        assert!(matches!(
            super::tests::view(query, &NONE),
            Err(MapsError::BadRequest(_))
        ));
    }

    #[test]
    fn forbidden_combinations_are_400() {
        let cases: [(MapQueryParams, &[&str]); 6] = [
            (
                MapQueryParams {
                    bbox: Some("10,55,30,70".into()),
                    center: Some("20,60".into()),
                    ..Default::default()
                },
                &[],
            ),
            (
                MapQueryParams {
                    bbox: Some("10,55,30,70".into()),
                    ..Default::default()
                },
                &["Lat(60:65)"],
            ),
            (
                MapQueryParams {
                    center: Some("20,60".into()),
                    ..Default::default()
                },
                &["Lon(10:20)"],
            ),
            (
                MapQueryParams {
                    bbox: Some("10,55,30,70".into()),
                    width: Some("100".into()),
                    scale_denominator: Some("1000000".into()),
                    ..Default::default()
                },
                &[],
            ),
            (
                MapQueryParams {
                    height: Some("100".into()),
                    scale_denominator: Some("1000000".into()),
                    ..Default::default()
                },
                &["Lat(60:65)"],
            ),
            (
                MapQueryParams {
                    datetime: Some("2024-01-01T00:00:00Z".into()),
                    ..Default::default()
                },
                &["time(*)"],
            ),
        ];
        for (i, (query, subsets)) in cases.into_iter().enumerate() {
            assert!(
                matches!(query.validate(subsets), Err(MapsError::BadRequest(_))),
                "case {i}"
            );
        }
        // A time subset is no spatial subset: it goes with bbox and a scale.
        let query = MapQueryParams {
            bbox: Some("10,55,30,70".into()),
            ..Default::default()
        };
        assert!(query.validate(&["time(*)"]).is_ok());
        // An unknown axis is a 400, never ignored.
        assert!(matches!(
            MapQueryParams::default().validate(&["foo(1)"]),
            Err(MapsError::BadRequest(_))
        ));
    }

    #[test]
    fn dimensions_must_be_positive_integers() {
        for bad in ["0", "-1", "1.5", "x", "9000"] {
            let query = MapQueryParams {
                width: Some(bad.into()),
                ..Default::default()
            };
            assert!(
                matches!(query.validate(&NONE), Err(MapsError::BadRequest(_))),
                "{bad}"
            );
        }
        for bad in ["0", "-5", "x", "inf"] {
            let query = MapQueryParams {
                scale_denominator: Some(bad.into()),
                ..Default::default()
            };
            assert!(
                matches!(query.validate(&NONE), Err(MapsError::BadRequest(_))),
                "{bad}"
            );
        }
    }
}
