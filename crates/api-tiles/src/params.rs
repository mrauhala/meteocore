use api_common::map_frame::{self, Frame};
use api_common::subset::{self, Axis, RequestedTime, TimeSelection};
use serde::Deserialize;

use crate::error::TilesError;

/// Absolute maximum zoom level.
pub const MAX_ZOOM_LEVEL: u32 = 24;

/// Default per-collection maximum zoom level.
pub const DEFAULT_MAX_ZOOM: u32 = 18;

/// Standard tile size in pixels: the tileWidth and tileHeight of every tile
/// matrix. A map tile renders `TILE_SIZE × TILE_SIZE` output pixels unless
/// `width`, `height` or `scale-denominator` sizes it, up to the Maps caps
/// (`api_common::map_frame::MAX_MAP_DIMENSION`/`MAX_MAP_PIXELS`); render
/// admission and the engine's source budget still apply ("Pixel budgets" in
/// the root CLAUDE.md).
pub const TILE_SIZE: u32 = 256;

/// Maximum number of features a single MVT tile is allowed to carry. At
/// z=0 of a 100k-feature dataset the whole world lands in one tile —
/// returning a multi-megabyte MVT would harm the cache and the client.
/// Exceeding this cap surfaces as HTTP 422 (Unprocessable Content): the
/// request is well-formed, but the data can't be served at that scale.
/// The fix is to raise the collection's `minzoom`, not to silently encode.
pub const MAX_FEATURES_PER_TILE: usize = 50_000;

/// Supported raster output formats for map tiles.
///
/// `image/png` auto-selects an 8-bit indexed-palette encoding ("PNG8") when
/// the rendered tile carries ≤256 distinct colours (every colormap layer);
/// the encoder falls back to 32-bit RGBA above that. Content-type is
/// `image/png` either way — no second `f=` value is needed.
const SUPPORTED_FORMATS: &[&str] = &["image/png", "image/jpeg", "image/webp"];

/// MVT format aliases. Either form works in `?f=` for clients that prefer
/// a short token or the canonical MIME.
pub const MVT_FORMAT_TOKENS: &[&str] = &["mvt", "application/vnd.mapbox-vector-tile"];

/// Query parameters for tile requests. `subset` may repeat, so the handlers
/// collect it from the raw query.
#[derive(Debug, Default, Deserialize)]
pub struct TileQueryParams {
    /// An instant or interval (OGC API - Tiles DateTime class).
    pub datetime: Option<String>,
    #[serde(rename = "f")]
    pub format: Option<String>,
    /// EDR-style parameter selector for multi-parameter raster engines.
    /// Non-OGC for now; a standardised form is on the OGC Tiles roadmap.
    /// Ignored for MVT (`?f=mvt`) responses.
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
    /// Vertical level selector (e.g. a radar elevation angle). Rejected
    /// with HTTP 400 for collections with no vertical dimension; ignored
    /// for MVT responses.
    pub elevation: Option<String>,
    /// Encoder quality (MeteoCore extension): 1–100 for `image/webp`
    /// (100 = lossless) and `image/jpeg`. Rejected for PNG and for vector
    /// tiles. A string so a bad value gets the range message.
    pub quality: Option<String>,
    /// The Maps Scaling parameters map tiles take (Map Tilesets
    /// `/req/tilesets/tiles-parameters`): `width` and `height` override the
    /// tile matrix's tileWidth and tileHeight, which still set the tile's
    /// area. Strings, so a bad value gets a JSON 400 naming the rule.
    pub width: Option<String>,
    pub height: Option<String>,
    /// The scale the tile image is drawn at, on the standard 0.28 mm pixel:
    /// it sets the image's size over the tile's area.
    #[serde(rename = "scale-denominator")]
    pub scale_denominator: Option<String>,
}

/// Every query parameter a map tile accepts; any other is a 400 naming
/// these (root CLAUDE.md: never silently ignore a parameter).
pub const MAP_TILE_PARAMETERS: &[&str] = &[
    "f",
    "datetime",
    "subset",
    "width",
    "height",
    "scale-denominator",
    "parameter-name",
    "elevation",
    "quality",
];

/// Every query parameter a vector tile accepts.
pub const VECTOR_TILE_PARAMETERS: &[&str] = &["f", "datetime", "subset"];

/// The `subset` axis tiles accept, map and vector alike: OGC API - Tiles
/// names its time axis `datetime` (`/req/datetime/axis`). There is no
/// other. The tile matrix
/// set partitions the plane, and the vertical `h`/`z` axis that Maps
/// Spatial Subsetting adds to map tiles needs a three-dimensional spatial
/// extent, which no collection here has; levels are selected by `elevation`.
pub const DATETIME_AXIS: Axis<'static> = Axis {
    name: "datetime",
    aliases: &[],
};

/// A 400 naming the accepted parameters when `raw_query` holds another one.
pub fn reject_unknown_parameters(
    raw_query: Option<&str>,
    accepted: &[&str],
) -> Result<(), TilesError> {
    match subset::unknown_parameter(raw_query, accepted) {
        Some(name) => Err(TilesError::BadRequest(format!(
            "Unknown query parameter '{name}'. Supported: {}",
            accepted.join(", ")
        ))),
        None => Ok(()),
    }
}

impl TileQueryParams {
    /// Whether the client requested a Mapbox Vector Tile via `?f=mvt`.
    pub fn is_mvt(&self) -> bool {
        self.format
            .as_deref()
            .map(|f| MVT_FORMAT_TOKENS.contains(&f))
            .unwrap_or(false)
    }
}

/// Query parameters for the style legend endpoint.
#[derive(Debug, Deserialize)]
pub struct LegendQueryParams {
    #[serde(rename = "f")]
    pub format: Option<String>,
    /// Selects the per-parameter style layer, matching `parameter-name` on
    /// the tile routes — without it the legend would describe the
    /// collection-level colormap while the tile renders the parameter's own.
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
    /// this API (`image/png`, `mvt`) and in the OGC metadata endpoints (`json`).
    pub fn validate(&self) -> Result<LegendFormat, TilesError> {
        match self
            .format
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None | Some("json") | Some("application/json") => Ok(LegendFormat::Json),
            Some("png") | Some("image/png") => Ok(LegendFormat::Png),
            Some(other) => Err(TilesError::BadRequest(format!(
                "Format '{other}' is not supported for a legend. Supported: json, png"
            ))),
        }
    }
}

/// How a map tile's image is sized over the tile's area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TileScaling {
    /// `width` and `height`, either or both omitted: both omitted is the
    /// tile matrix's tileWidth × tileHeight, one omitted keeps square pixels.
    Size {
        width: Option<u32>,
        height: Option<u32>,
    },
    /// `scale-denominator`: the size that scale gives over the tile's area.
    Scale(f64),
}

/// Validated tile query parameters.
pub struct ValidatedTileParams {
    /// `datetime` or `subset=datetime(…)`, not both.
    pub time: Option<RequestedTime>,
    /// Output format at its default quality (JPEG 85, lossless WebP); the
    /// handler applies [`Self::quality`] and the collection's `webp_quality`.
    pub format: ds_render::ImageFormat,
    /// The `quality` parameter, 1–100, when supplied. Always `None` for PNG,
    /// which rejects it.
    pub quality: Option<u8>,
    pub parameter_name: Option<String>,
    /// Vertical level, parsed from the `elevation` query parameter.
    pub z: Option<f64>,
    pub scaling: TileScaling,
}

fn bad(message: impl Into<String>) -> TilesError {
    TilesError::BadRequest(message.into())
}

/// A present, non-blank parameter value.
fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

impl TileQueryParams {
    /// Validate the request on its own. `subsets` are the `subset` values in
    /// request order.
    pub fn validate<S: AsRef<str>>(
        &self,
        subsets: &[S],
    ) -> Result<ValidatedTileParams, TilesError> {
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
            .map_err(TilesError::BadRequest)?;

        let time = self.time(subsets)?;

        // SCALING — the tile matrix sets the tile's area, as `bbox` does a
        // map's, so `scale-denominator` with `width` or `height` is a 400
        // (`/req/scaling/scale-denominator-definition` D,
        // `width-definition` F, `height-definition` F).
        let width = map_frame::parse_dimension("width", present(&self.width)).map_err(bad)?;
        let height = map_frame::parse_dimension("height", present(&self.height)).map_err(bad)?;
        let scaling = match map_frame::parse_scale_denominator(present(&self.scale_denominator))
            .map_err(bad)?
        {
            Some(_) if width.is_some() || height.is_some() => {
                return Err(bad(
                    "scale-denominator cannot be combined with width or height: the tile \
                     matrix sets the tile's area",
                ))
            }
            Some(scale) => TileScaling::Scale(scale),
            None => TileScaling::Size { width, height },
        };

        let parameter_name = present(&self.parameter_name).map(str::to_string);

        let z = match present(&self.elevation) {
            Some(raw) => Some(
                raw.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| bad(format!("elevation '{raw}' is not a finite number")))?,
            ),
            None => None,
        };

        Ok(ValidatedTileParams {
            time,
            format,
            quality,
            parameter_name,
            z,
            scaling,
        })
    }
}

impl TileQueryParams {
    /// The time a tile request selects: `datetime`
    /// (`/req/collections/rc-datetime-definition`) or `subset=datetime(…)`
    /// (`/req/datetime/axis`), not both; any other subset axis is a 400
    /// (`/req/collections/rc-subset-definition` B). Map and vector tiles
    /// alike (OGC API - Tiles DateTime).
    fn time<S: AsRef<str>>(&self, subsets: &[S]) -> Result<Option<RequestedTime>, TilesError> {
        let datetime = present(&self.datetime)
            .map(|v| TimeSelection::from_datetime(v).map_err(bad))
            .transpose()?;
        let ranges =
            subset::by_axis(subset::parse(subsets).map_err(bad)?, &[DATETIME_AXIS]).map_err(bad)?;
        let time_subset = ranges
            .get(DATETIME_AXIS.name)
            .map(|range| TimeSelection::from_subset(range).map_err(bad))
            .transpose()?;
        match (datetime, time_subset) {
            (Some(_), Some(_)) => Err(bad(
                "datetime and subset=datetime cannot be used together: both select the time",
            )),
            (Some(selection), None) => Ok(Some(RequestedTime {
                selection,
                from_subset: false,
            })),
            (None, Some(selection)) => Ok(Some(RequestedTime {
                selection,
                from_subset: true,
            })),
            (None, None) => Ok(None),
        }
    }

    /// Validate a vector tile request: `f`, `datetime` and `subset` only.
    /// A map tile parameter is a 400 saying vector tiles do not take it, any
    /// other a 400 naming the accepted ones; neither is ignored (#605).
    /// Returns the time selection the features are filtered by.
    pub fn validate_vector<S: AsRef<str>>(
        &self,
        raw_query: Option<&str>,
        subsets: &[S],
    ) -> Result<Option<RequestedTime>, TilesError> {
        if let Some(name) = subset::unknown_parameter(raw_query, VECTOR_TILE_PARAMETERS) {
            return Err(if MAP_TILE_PARAMETERS.contains(&name.as_str()) {
                bad(format!("'{name}' is not supported for vector tiles"))
            } else {
                bad(format!(
                    "Unknown query parameter '{name}'. Supported: {}",
                    VECTOR_TILE_PARAMETERS.join(", ")
                ))
            });
        }
        self.time(subsets)
    }
}

impl ValidatedTileParams {
    /// Width and height of the tile image over `frame`, the tile's area in
    /// its tile matrix set's CRS. `default` is the tile matrix's tileWidth
    /// and tileHeight, which the Scaling parameters override (Maps
    /// `/req/tilesets/tiles-parameters`).
    pub fn size(&self, frame: &Frame, default: (u32, u32)) -> Result<(u32, u32), TilesError> {
        match self.scaling {
            TileScaling::Size {
                width: None,
                height: None,
            } => Ok(default),
            TileScaling::Size { width, height } => map_frame::whole_pixels(
                map_frame::size_for_aspect(frame.aspect(), width, height),
                "the tile's aspect ratio",
            )
            .map_err(bad),
            TileScaling::Scale(scale) => {
                let (x, y) = frame.center();
                let (rx, ry) =
                    map_frame::resolution_at(frame.crs(), x, y, scale).ok_or_else(|| {
                        bad("scale-denominator cannot be applied at the tile's centre: the CRS has \
                         no local scale there")
                    })?;
                map_frame::whole_pixels(
                    map_frame::size_for_resolution(frame, rx, ry),
                    "scale-denominator over this tile",
                )
                .map_err(bad)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use api_common::map_frame::MapCrs;

    const NO_SUBSET: [&str; 0] = [];

    fn validate(params: &TileQueryParams) -> Result<ValidatedTileParams, TilesError> {
        params.validate(&NO_SUBSET)
    }

    fn message(result: Result<ValidatedTileParams, TilesError>) -> String {
        match result {
            Err(TilesError::BadRequest(message)) => message,
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[test]
    fn test_validate_default_format() {
        let validated = validate(&TileQueryParams::default()).unwrap();
        assert!(matches!(validated.format, ds_render::ImageFormat::Png));
        assert!(validated.time.is_none());
        assert_eq!(
            validated.scaling,
            TileScaling::Size {
                width: None,
                height: None
            }
        );
    }

    #[test]
    fn test_validate_jpeg_format() {
        let params = TileQueryParams {
            format: Some("image/jpeg".to_string()),
            ..Default::default()
        };
        let validated = validate(&params).unwrap();
        assert_eq!(validated.format, ds_render::ImageFormat::JPEG);
    }

    #[test]
    fn test_validate_invalid_format() {
        let params = TileQueryParams {
            format: Some("text/html".to_string()),
            ..Default::default()
        };
        assert!(validate(&params).is_err());
    }

    fn with_quality(format: Option<&str>, quality: &str) -> TileQueryParams {
        TileQueryParams {
            format: format.map(str::to_string),
            quality: Some(quality.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_validate_quality() {
        let validated = validate(&with_quality(Some("image/webp"), "60")).unwrap();
        assert_eq!(validated.quality, Some(60));
        assert_eq!(validated.format, ds_render::ImageFormat::WEBP);
        let validated = validate(&with_quality(Some("image/jpeg"), "100")).unwrap();
        assert_eq!(validated.quality, Some(100));
        for q in ["0", "101", "abc", "50.5"] {
            assert_eq!(
                message(validate(&with_quality(Some("image/webp"), q))),
                format!("quality '{q}' must be an integer from 1 to 100")
            );
        }
        // PNG, explicit or by default, has no quality.
        for f in [Some("image/png"), None] {
            assert_eq!(
                message(validate(&with_quality(f, "80"))),
                "quality applies only to image/jpeg and image/webp, not image/png"
            );
        }
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().unwrap()
    }

    fn time_of(
        datetime: Option<&str>,
        subsets: &[&str],
    ) -> Result<Option<RequestedTime>, TilesError> {
        TileQueryParams {
            datetime: datetime.map(str::to_string),
            ..Default::default()
        }
        .validate(subsets)
        .map(|v| v.time)
    }

    /// `/req/collections/rc-datetime-definition` B–D: an instant or an
    /// interval, either end open with `..` or empty.
    #[test]
    fn datetime_takes_instants_and_intervals() {
        assert_eq!(
            time_of(Some("2024-01-01T00:00:00Z"), &[]).unwrap(),
            Some(RequestedTime {
                selection: TimeSelection::Instant(at("2024-01-01T00:00:00Z")),
                from_subset: false,
            })
        );
        for (value, start, end) in [
            (
                "2024-01-01T00:00:00Z/2024-01-01T01:00:00Z",
                Some("2024-01-01T00:00:00Z"),
                Some("2024-01-01T01:00:00Z"),
            ),
            (
                "2024-01-01T00:00:00Z/..",
                Some("2024-01-01T00:00:00Z"),
                None,
            ),
            (
                "../2024-01-01T01:00:00Z",
                None,
                Some("2024-01-01T01:00:00Z"),
            ),
            ("/2024-01-01T01:00:00Z", None, Some("2024-01-01T01:00:00Z")),
        ] {
            assert_eq!(
                time_of(Some(value), &[]).unwrap(),
                Some(RequestedTime {
                    selection: TimeSelection::Range {
                        start: start.map(at),
                        end: end.map(at),
                    },
                    from_subset: false,
                }),
                "{value}"
            );
        }
        assert!(time_of(Some("not-a-time"), &[]).is_err());
        assert!(time_of(Some("2024-01-02T00:00:00Z/2024-01-01T00:00:00Z"), &[]).is_err());
    }

    /// `/req/datetime/axis`: the time axis is `datetime`; any other axis is a
    /// 400 naming it (`/req/collections/rc-subset-definition` B).
    #[test]
    fn subset_takes_only_the_datetime_axis() {
        assert_eq!(
            time_of(None, &[r#"datetime("2024-01-01T00:00:00Z")"#]).unwrap(),
            Some(RequestedTime {
                selection: TimeSelection::Instant(at("2024-01-01T00:00:00Z")),
                from_subset: true,
            })
        );
        assert_eq!(
            time_of(None, &[r#"datetime("2024-01-01T00:00:00Z":*)"#]).unwrap(),
            Some(RequestedTime {
                selection: TimeSelection::Range {
                    start: Some(at("2024-01-01T00:00:00Z")),
                    end: None,
                },
                from_subset: true,
            })
        );
        for axis in [
            "time(\"2024-01-01T00:00:00Z\")",
            "h(100)",
            "Lat(1:2)",
            "foo(1)",
        ] {
            let err = time_of(None, &[axis]).unwrap_err();
            let TilesError::BadRequest(msg) = err else {
                panic!("{axis}: expected BadRequest")
            };
            assert!(msg.ends_with("valid axes: datetime"), "{axis}: {msg}");
        }
        assert!(time_of(None, &["datetime(2024)"]).is_err());
        assert!(time_of(None, &["datetime(\"2024-01-01T00:00:00Z\""]).is_err());
        let both = time_of(
            Some("2024-01-01T00:00:00Z"),
            &[r#"datetime("2024-01-01T00:00:00Z")"#],
        );
        let TilesError::BadRequest(msg) = both.unwrap_err() else {
            panic!("expected BadRequest")
        };
        assert!(msg.contains("cannot be used together"), "{msg}");
    }

    fn scaled(width: Option<&str>, height: Option<&str>, scale: Option<&str>) -> TileQueryParams {
        TileQueryParams {
            width: width.map(str::to_string),
            height: height.map(str::to_string),
            scale_denominator: scale.map(str::to_string),
            ..Default::default()
        }
    }

    /// The Maps Scaling parameters on a tile (`/req/tilesets/tiles-parameters`):
    /// `width`/`height` set the image's pixels over the tile's own area, and
    /// `scale-denominator` derives them.
    #[test]
    fn scaling_sizes_the_tile_image() {
        // A WorldCRS84Quad tile, 45° square at the equator.
        let frame = Frame::from_plane(MapCrs::Crs84, [0.0, -45.0, 45.0, 0.0]);
        let size = |p: TileQueryParams| validate(&p).unwrap().size(&frame, (256, 256));
        assert_eq!(size(scaled(None, None, None)).unwrap(), (256, 256));
        assert_eq!(size(scaled(Some("512"), None, None)).unwrap(), (512, 512));
        assert_eq!(size(scaled(None, Some("100"), None)).unwrap(), (100, 100));
        assert_eq!(
            size(scaled(Some("300"), Some("200"), None)).unwrap(),
            (300, 200)
        );
        // 1:100 000 000 is 28 km a pixel: a 45° (≈5000 km) tall tile is
        // about 179 pixels. Its 45° of longitude are shorter on the ground by
        // cos(22.5°), at the tile's centre, so it is narrower than tall.
        let (width, height) = size(scaled(None, None, Some("1e8"))).unwrap();
        assert!((175..=183).contains(&height), "{height}");
        let ratio = f64::from(width) / f64::from(height);
        assert!(
            (ratio - 22.5f64.to_radians().cos()).abs() < 0.01,
            "{width}x{height}"
        );
        // Larger scales mean fewer pixels.
        let (w2, h2) = size(scaled(None, None, Some("2e8"))).unwrap();
        assert!(w2 < width && h2 < height);
        // Too fine a scale over the tile exceeds the caps.
        let err = size(scaled(None, None, Some("1000"))).unwrap_err();
        let TilesError::BadRequest(msg) = err else {
            panic!("expected BadRequest")
        };
        assert!(msg.contains("must not exceed 8000"), "{msg}");
    }

    #[test]
    fn scaling_values_and_combinations_are_validated() {
        for (params, needle) in [
            (
                scaled(Some("0"), None, None),
                "width '0' must be a positive integer",
            ),
            (
                scaled(None, Some("1.5"), None),
                "height '1.5' must be a positive integer",
            ),
            (scaled(Some("8001"), None, None), "must not exceed 8000"),
            (
                scaled(None, None, Some("-5")),
                "scale-denominator '-5' must be a positive number",
            ),
            (
                scaled(Some("256"), None, Some("1e6")),
                "scale-denominator cannot be combined with width or height",
            ),
            (
                scaled(None, Some("256"), Some("1e6")),
                "scale-denominator cannot be combined with width or height",
            ),
        ] {
            let msg = message(validate(&params));
            assert!(msg.contains(needle), "{msg}");
        }
    }

    #[test]
    fn unknown_parameters_are_named() {
        assert!(reject_unknown_parameters(None, MAP_TILE_PARAMETERS).is_ok());
        assert!(reject_unknown_parameters(
            Some("datetime=x&subset=datetime(*)&subset=y&f=image/png&width=1"),
            MAP_TILE_PARAMETERS
        )
        .is_ok());
        let Err(TilesError::BadRequest(msg)) =
            reject_unknown_parameters(Some("datetime=x&bbox=1,2,3,4"), MAP_TILE_PARAMETERS)
        else {
            panic!("expected BadRequest")
        };
        assert!(msg.starts_with("Unknown query parameter 'bbox'. Supported: f, datetime, subset"));
    }
}
