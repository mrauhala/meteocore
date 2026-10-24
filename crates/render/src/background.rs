//! How an encoded map paints its empty pixels: WMS `TRANSPARENT`/`BGCOLOR`
//! (#163) and the OGC API - Maps Background class (`transparent`,
//! `bgcolor`, `void-color`, `void-transparent`, #894).
//!
//! A render's RGBA pixels carry no background: no data is RGBA 0. The
//! background is applied once, just before encoding, so the rendered and
//! meta-tile pixel caches stay background-free.

use std::ops::Range;

use ds_core::map_engine::OutputCrs;

use crate::encode::flatten_onto;
use crate::ImageFormat;

/// How one part of a map's pixels is painted at encode time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Fill {
    /// The pixels as rendered: alpha kept, no data RGBA 0 (WMS
    /// `TRANSPARENT=TRUE`, and Maps `transparent` without a `bgcolor`).
    #[default]
    Keep,
    /// Alpha kept, and every fully transparent pixel given this colour as
    /// its RGB, at alpha 0 (Maps `transparent=true` with a `bgcolor`).
    Clear([u8; 3]),
    /// Composited onto this colour ([`flatten_onto`]), every pixel opaque
    /// (WMS `TRANSPARENT=FALSE`, Maps `transparent=false`, and JPEG, which
    /// has no alpha).
    Opaque([u8; 3]),
}

impl Fill {
    /// Paint `rgba` with this fill.
    pub fn paint(self, rgba: &mut [u8]) {
        match self {
            Fill::Keep => {}
            Fill::Clear(rgb) => {
                for pixel in rgba.as_chunks_mut::<4>().0 {
                    if pixel[3] == 0 {
                        *pixel = [rgb[0], rgb[1], rgb[2], 0];
                    }
                }
            }
            Fill::Opaque(rgb) => flatten_onto(rgba, rgb),
        }
    }

    /// The fill in a cache key's ETag: a tag and the colour.
    pub(crate) fn etag_bytes(self) -> [u8; 4] {
        match self {
            Fill::Keep => [0; 4],
            Fill::Clear([r, g, b]) => [1, r, g, b],
            Fill::Opaque([r, g, b]) => [2, r, g, b],
        }
    }
}

impl From<Option<[u8; 3]>> for Fill {
    /// WMS's background: `Some` opaque colour, or `None` to keep the alpha.
    fn from(background: Option<[u8; 3]>) -> Self {
        background.map_or(Fill::Keep, Fill::Opaque)
    }
}

/// A map's background: the fill of the pixels on the Earth, and that of
/// the pixels outside the output CRS's valid area (OGC API - Maps "void"),
/// which an engine leaves empty. Part of the rendered-image cache key,
/// since it changes the encoded bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Background {
    pub fill: Fill,
    pub void: Fill,
}

impl Background {
    /// One fill for every pixel.
    pub const fn uniform(fill: Fill) -> Self {
        Self { fill, void: fill }
    }

    /// Whether the void pixels are painted like the rest, so painting needs
    /// no map of where the Earth is.
    pub fn is_uniform(&self) -> bool {
        self.fill == self.void
    }

    /// The map of where the Earth is that painting a `width`×`height` image
    /// over `wgs84_bbox` in `output_crs` needs: per row, the columns on the
    /// Earth ([`OutputCrs::earth_columns`]). `None` when every pixel is, or
    /// when this background paints them all alike, which never computes it.
    /// Call it on the render's blocking worker.
    pub fn earth_columns(
        &self,
        output_crs: &OutputCrs,
        wgs84_bbox: [f64; 4],
        width: u32,
        height: u32,
    ) -> Option<Vec<Range<u32>>> {
        if self.is_uniform() {
            return None;
        }
        output_crs.earth_columns(wgs84_bbox, width, height)
    }

    /// Paint `rgba`, an image `width` pixels wide: [`Self::fill`] on the
    /// pixels on the Earth, [`Self::void`] on the rest. `earth` is, per row,
    /// the columns on the Earth ([`OutputCrs::earth_columns`]); `None`, or
    /// a row it lacks, is all of them.
    pub fn paint(self, rgba: &mut [u8], width: u32, earth: Option<&[Range<u32>]>) {
        let earth = match earth {
            Some(rows) if !self.is_uniform() => rows,
            _ => return self.fill.paint(rgba),
        };
        let row_len = width as usize * 4;
        if row_len == 0 {
            return;
        }
        for (row, pixels) in rgba.chunks_exact_mut(row_len).enumerate() {
            let Some(columns) = earth.get(row) else {
                self.fill.paint(pixels);
                continue;
            };
            let start = (columns.start.min(width) as usize) * 4;
            let end = (columns.end.min(width) as usize).max(start / 4) * 4;
            let (west, rest) = pixels.split_at_mut(start);
            let (inside, east) = rest.split_at_mut(end - start);
            self.void.paint(west);
            self.fill.paint(inside);
            self.void.paint(east);
        }
    }
}

impl From<Option<[u8; 3]>> for Background {
    /// WMS's background ([`Fill::from`]) on every pixel.
    fn from(background: Option<[u8; 3]>) -> Self {
        Self::uniform(Fill::from(background))
    }
}

/// `bgcolor`'s default, `0xFFFFFF` (`/req/background/bgcolor-definition`).
pub const DEFAULT_BGCOLOR: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// The OGC API - Maps Background parameters of a map or map tile request,
/// as sent. Blank values count as absent.
#[derive(Debug, Default, Clone, Copy)]
pub struct BackgroundQuery<'a> {
    pub transparent: Option<&'a str>,
    pub bgcolor: Option<&'a str>,
    pub void_color: Option<&'a str>,
    pub void_transparent: Option<&'a str>,
}

/// The [`Background`] of an OGC API - Maps request in `format` (OGC 20-058
/// Requirements Class "Background", #894):
///
/// - `bgcolor` is `0xRRGGBB` (also `#RRGGBB`) or a W3C web colour name,
///   any case ([`web_color`]); default white.
/// - `transparent` is `true` or `false`, any case. Without it the map is
///   transparent unless a `bgcolor` is given (`/req/background/
///   transparent-definition` C, D).
/// - Transparent, the alpha is kept, and with a `bgcolor` the no-data
///   pixels take its colour at opacity 0 (`transparent-definition` E).
///   Opaque, the map is composited onto the `bgcolor`.
/// - `void-color` and `void-transparent` do the same for the pixels outside
///   the CRS's valid area, defaulting to `bgcolor` and `transparent`
///   (`/req/background/void-color-definition` C,
///   `void-transparent-definition` C).
/// - JPEG has no alpha: every pixel is opaque, on the `bgcolor` (default
///   white; the style names no background, `bgcolor-definition` C).
///
/// An invalid value is an error naming the valid forms.
pub fn parse_background(
    query: BackgroundQuery<'_>,
    format: ImageFormat,
) -> Result<Background, String> {
    let bgcolor = parse_color("bgcolor", query.bgcolor)?;
    let transparent = parse_boolean("transparent", query.transparent)?.unwrap_or(bgcolor.is_none());
    let void_color = parse_color("void-color", query.void_color)?.or(bgcolor);
    let void_transparent =
        parse_boolean("void-transparent", query.void_transparent)?.unwrap_or(transparent);
    let alpha = !matches!(format, ImageFormat::Jpeg { .. });
    let fill = |transparent: bool, color: Option<[u8; 3]>| match color {
        _ if !(transparent && alpha) => Fill::Opaque(color.unwrap_or(DEFAULT_BGCOLOR)),
        Some(color) => Fill::Clear(color),
        None => Fill::Keep,
    };
    Ok(Background {
        fill: fill(transparent, bgcolor),
        void: fill(void_transparent, void_color),
    })
}

/// A present, non-blank parameter value.
fn present(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

/// A boolean parameter: `true` or `false`, any case; `None` when absent.
fn parse_boolean(param: &str, raw: Option<&str>) -> Result<Option<bool>, String> {
    let Some(raw) = present(raw) else {
        return Ok(None);
    };
    if raw.eq_ignore_ascii_case("true") {
        Ok(Some(true))
    } else if raw.eq_ignore_ascii_case("false") {
        Ok(Some(false))
    } else {
        Err(format!("{param} '{raw}' must be true or false"))
    }
}

/// A colour parameter: `0xRRGGBB` or `#RRGGBB` (hex digits of either case),
/// or a W3C web colour name; `None` when absent.
fn parse_color(param: &str, raw: Option<&str>) -> Result<Option<[u8; 3]>, String> {
    let Some(raw) = present(raw) else {
        return Ok(None);
    };
    let hex = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .or_else(|| raw.strip_prefix('#'));
    let parsed = match hex {
        Some(hex) if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            u32::from_str_radix(hex, 16).ok().map(|rgb| {
                let [_, r, g, b] = rgb.to_be_bytes();
                [r, g, b]
            })
        }
        Some(_) => None,
        None => web_color(raw),
    };
    parsed.map(Some).ok_or_else(|| {
        format!(
            "{param} '{raw}' must be a hexadecimal colour 0xRRGGBB (or #RRGGBB) or a W3C web \
             colour name, such as 0x336699, white or SteelBlue"
        )
    })
}

/// The colour a W3C web colour name names, any case: the named colours of
/// CSS Color Module Level 4 §6.1, which OGC 20-058 Annex D reproduces.
pub fn web_color(name: &str) -> Option<[u8; 3]> {
    WEB_COLORS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|&(_, rgb)| rgb)
}

/// The W3C named colours (CSS Color Module Level 4 §6.1), sorted.
const WEB_COLORS: [(&str, [u8; 3]); 148] = [
    ("aliceblue", [0xf0, 0xf8, 0xff]),
    ("antiquewhite", [0xfa, 0xeb, 0xd7]),
    ("aqua", [0x00, 0xff, 0xff]),
    ("aquamarine", [0x7f, 0xff, 0xd4]),
    ("azure", [0xf0, 0xff, 0xff]),
    ("beige", [0xf5, 0xf5, 0xdc]),
    ("bisque", [0xff, 0xe4, 0xc4]),
    ("black", [0x00, 0x00, 0x00]),
    ("blanchedalmond", [0xff, 0xeb, 0xcd]),
    ("blue", [0x00, 0x00, 0xff]),
    ("blueviolet", [0x8a, 0x2b, 0xe2]),
    ("brown", [0xa5, 0x2a, 0x2a]),
    ("burlywood", [0xde, 0xb8, 0x87]),
    ("cadetblue", [0x5f, 0x9e, 0xa0]),
    ("chartreuse", [0x7f, 0xff, 0x00]),
    ("chocolate", [0xd2, 0x69, 0x1e]),
    ("coral", [0xff, 0x7f, 0x50]),
    ("cornflowerblue", [0x64, 0x95, 0xed]),
    ("cornsilk", [0xff, 0xf8, 0xdc]),
    ("crimson", [0xdc, 0x14, 0x3c]),
    ("cyan", [0x00, 0xff, 0xff]),
    ("darkblue", [0x00, 0x00, 0x8b]),
    ("darkcyan", [0x00, 0x8b, 0x8b]),
    ("darkgoldenrod", [0xb8, 0x86, 0x0b]),
    ("darkgray", [0xa9, 0xa9, 0xa9]),
    ("darkgreen", [0x00, 0x64, 0x00]),
    ("darkgrey", [0xa9, 0xa9, 0xa9]),
    ("darkkhaki", [0xbd, 0xb7, 0x6b]),
    ("darkmagenta", [0x8b, 0x00, 0x8b]),
    ("darkolivegreen", [0x55, 0x6b, 0x2f]),
    ("darkorange", [0xff, 0x8c, 0x00]),
    ("darkorchid", [0x99, 0x32, 0xcc]),
    ("darkred", [0x8b, 0x00, 0x00]),
    ("darksalmon", [0xe9, 0x96, 0x7a]),
    ("darkseagreen", [0x8f, 0xbc, 0x8f]),
    ("darkslateblue", [0x48, 0x3d, 0x8b]),
    ("darkslategray", [0x2f, 0x4f, 0x4f]),
    ("darkslategrey", [0x2f, 0x4f, 0x4f]),
    ("darkturquoise", [0x00, 0xce, 0xd1]),
    ("darkviolet", [0x94, 0x00, 0xd3]),
    ("deeppink", [0xff, 0x14, 0x93]),
    ("deepskyblue", [0x00, 0xbf, 0xff]),
    ("dimgray", [0x69, 0x69, 0x69]),
    ("dimgrey", [0x69, 0x69, 0x69]),
    ("dodgerblue", [0x1e, 0x90, 0xff]),
    ("firebrick", [0xb2, 0x22, 0x22]),
    ("floralwhite", [0xff, 0xfa, 0xf0]),
    ("forestgreen", [0x22, 0x8b, 0x22]),
    ("fuchsia", [0xff, 0x00, 0xff]),
    ("gainsboro", [0xdc, 0xdc, 0xdc]),
    ("ghostwhite", [0xf8, 0xf8, 0xff]),
    ("gold", [0xff, 0xd7, 0x00]),
    ("goldenrod", [0xda, 0xa5, 0x20]),
    ("gray", [0x80, 0x80, 0x80]),
    ("green", [0x00, 0x80, 0x00]),
    ("greenyellow", [0xad, 0xff, 0x2f]),
    ("grey", [0x80, 0x80, 0x80]),
    ("honeydew", [0xf0, 0xff, 0xf0]),
    ("hotpink", [0xff, 0x69, 0xb4]),
    ("indianred", [0xcd, 0x5c, 0x5c]),
    ("indigo", [0x4b, 0x00, 0x82]),
    ("ivory", [0xff, 0xff, 0xf0]),
    ("khaki", [0xf0, 0xe6, 0x8c]),
    ("lavender", [0xe6, 0xe6, 0xfa]),
    ("lavenderblush", [0xff, 0xf0, 0xf5]),
    ("lawngreen", [0x7c, 0xfc, 0x00]),
    ("lemonchiffon", [0xff, 0xfa, 0xcd]),
    ("lightblue", [0xad, 0xd8, 0xe6]),
    ("lightcoral", [0xf0, 0x80, 0x80]),
    ("lightcyan", [0xe0, 0xff, 0xff]),
    ("lightgoldenrodyellow", [0xfa, 0xfa, 0xd2]),
    ("lightgray", [0xd3, 0xd3, 0xd3]),
    ("lightgreen", [0x90, 0xee, 0x90]),
    ("lightgrey", [0xd3, 0xd3, 0xd3]),
    ("lightpink", [0xff, 0xb6, 0xc1]),
    ("lightsalmon", [0xff, 0xa0, 0x7a]),
    ("lightseagreen", [0x20, 0xb2, 0xaa]),
    ("lightskyblue", [0x87, 0xce, 0xfa]),
    ("lightslategray", [0x77, 0x88, 0x99]),
    ("lightslategrey", [0x77, 0x88, 0x99]),
    ("lightsteelblue", [0xb0, 0xc4, 0xde]),
    ("lightyellow", [0xff, 0xff, 0xe0]),
    ("lime", [0x00, 0xff, 0x00]),
    ("limegreen", [0x32, 0xcd, 0x32]),
    ("linen", [0xfa, 0xf0, 0xe6]),
    ("magenta", [0xff, 0x00, 0xff]),
    ("maroon", [0x80, 0x00, 0x00]),
    ("mediumaquamarine", [0x66, 0xcd, 0xaa]),
    ("mediumblue", [0x00, 0x00, 0xcd]),
    ("mediumorchid", [0xba, 0x55, 0xd3]),
    ("mediumpurple", [0x93, 0x70, 0xdb]),
    ("mediumseagreen", [0x3c, 0xb3, 0x71]),
    ("mediumslateblue", [0x7b, 0x68, 0xee]),
    ("mediumspringgreen", [0x00, 0xfa, 0x9a]),
    ("mediumturquoise", [0x48, 0xd1, 0xcc]),
    ("mediumvioletred", [0xc7, 0x15, 0x85]),
    ("midnightblue", [0x19, 0x19, 0x70]),
    ("mintcream", [0xf5, 0xff, 0xfa]),
    ("mistyrose", [0xff, 0xe4, 0xe1]),
    ("moccasin", [0xff, 0xe4, 0xb5]),
    ("navajowhite", [0xff, 0xde, 0xad]),
    ("navy", [0x00, 0x00, 0x80]),
    ("oldlace", [0xfd, 0xf5, 0xe6]),
    ("olive", [0x80, 0x80, 0x00]),
    ("olivedrab", [0x6b, 0x8e, 0x23]),
    ("orange", [0xff, 0xa5, 0x00]),
    ("orangered", [0xff, 0x45, 0x00]),
    ("orchid", [0xda, 0x70, 0xd6]),
    ("palegoldenrod", [0xee, 0xe8, 0xaa]),
    ("palegreen", [0x98, 0xfb, 0x98]),
    ("paleturquoise", [0xaf, 0xee, 0xee]),
    ("palevioletred", [0xdb, 0x70, 0x93]),
    ("papayawhip", [0xff, 0xef, 0xd5]),
    ("peachpuff", [0xff, 0xda, 0xb9]),
    ("peru", [0xcd, 0x85, 0x3f]),
    ("pink", [0xff, 0xc0, 0xcb]),
    ("plum", [0xdd, 0xa0, 0xdd]),
    ("powderblue", [0xb0, 0xe0, 0xe6]),
    ("purple", [0x80, 0x00, 0x80]),
    ("rebeccapurple", [0x66, 0x33, 0x99]),
    ("red", [0xff, 0x00, 0x00]),
    ("rosybrown", [0xbc, 0x8f, 0x8f]),
    ("royalblue", [0x41, 0x69, 0xe1]),
    ("saddlebrown", [0x8b, 0x45, 0x13]),
    ("salmon", [0xfa, 0x80, 0x72]),
    ("sandybrown", [0xf4, 0xa4, 0x60]),
    ("seagreen", [0x2e, 0x8b, 0x57]),
    ("seashell", [0xff, 0xf5, 0xee]),
    ("sienna", [0xa0, 0x52, 0x2d]),
    ("silver", [0xc0, 0xc0, 0xc0]),
    ("skyblue", [0x87, 0xce, 0xeb]),
    ("slateblue", [0x6a, 0x5a, 0xcd]),
    ("slategray", [0x70, 0x80, 0x90]),
    ("slategrey", [0x70, 0x80, 0x90]),
    ("snow", [0xff, 0xfa, 0xfa]),
    ("springgreen", [0x00, 0xff, 0x7f]),
    ("steelblue", [0x46, 0x82, 0xb4]),
    ("tan", [0xd2, 0xb4, 0x8c]),
    ("teal", [0x00, 0x80, 0x80]),
    ("thistle", [0xd8, 0xbf, 0xd8]),
    ("tomato", [0xff, 0x63, 0x47]),
    ("turquoise", [0x40, 0xe0, 0xd0]),
    ("violet", [0xee, 0x82, 0xee]),
    ("wheat", [0xf5, 0xde, 0xb3]),
    ("white", [0xff, 0xff, 0xff]),
    ("whitesmoke", [0xf5, 0xf5, 0xf5]),
    ("yellow", [0xff, 0xff, 0x00]),
    ("yellowgreen", [0x9a, 0xcd, 0x32]),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn query<'a>(pairs: &[(&str, &'a str)]) -> BackgroundQuery<'a> {
        let get = |name: &str| pairs.iter().find(|(n, _)| *n == name).map(|&(_, v)| v);
        BackgroundQuery {
            transparent: get("transparent"),
            bgcolor: get("bgcolor"),
            void_color: get("void-color"),
            void_transparent: get("void-transparent"),
        }
    }

    fn parse(pairs: &[(&str, &str)], format: ImageFormat) -> Result<Background, String> {
        parse_background(query(pairs), format)
    }

    const RED: [u8; 3] = [0xff, 0, 0];
    const BLUE: [u8; 3] = [0, 0, 0xff];

    /// `/req/background/transparent-definition` C–E and `bgcolor-definition`
    /// B, over every combination the abstract test A.7 names, plus the
    /// void defaults of `void-color-definition` C and
    /// `void-transparent-definition` C.
    #[test]
    fn transparent_defaults_to_true_without_a_bgcolor_and_false_with_one() {
        let png = ImageFormat::Png;
        let uniform = |fill| Ok(Background::uniform(fill));
        for (pairs, expected) in [
            (vec![], uniform(Fill::Keep)),
            (vec![("transparent", "true")], uniform(Fill::Keep)),
            (
                vec![("transparent", "false")],
                uniform(Fill::Opaque(DEFAULT_BGCOLOR)),
            ),
            (vec![("bgcolor", "0xFF0000")], uniform(Fill::Opaque(RED))),
            (vec![("bgcolor", "red")], uniform(Fill::Opaque(RED))),
            (
                vec![("bgcolor", "0xff0000"), ("transparent", "true")],
                uniform(Fill::Clear(RED)),
            ),
            (
                vec![("bgcolor", "Red"), ("transparent", "FALSE")],
                uniform(Fill::Opaque(RED)),
            ),
        ] {
            assert_eq!(parse(&pairs, png), expected, "{pairs:?}");
        }
    }

    #[test]
    fn void_parameters_default_to_the_background_and_override_it() {
        let png = ImageFormat::Png;
        for (pairs, fill, void) in [
            (vec![("void-color", "blue")], Fill::Keep, Fill::Clear(BLUE)),
            (
                vec![("void-transparent", "false")],
                Fill::Keep,
                Fill::Opaque(DEFAULT_BGCOLOR),
            ),
            (
                vec![("bgcolor", "red"), ("void-transparent", "true")],
                Fill::Opaque(RED),
                Fill::Clear(RED),
            ),
            (
                vec![("transparent", "false"), ("void-color", "#0000FF")],
                Fill::Opaque(DEFAULT_BGCOLOR),
                Fill::Opaque(BLUE),
            ),
            (
                vec![
                    ("bgcolor", "red"),
                    ("void-color", "0x0000ff"),
                    ("void-transparent", "true"),
                ],
                Fill::Opaque(RED),
                Fill::Clear(BLUE),
            ),
        ] {
            assert_eq!(
                parse(&pairs, png),
                Ok(Background { fill, void }),
                "{pairs:?}"
            );
        }
    }

    /// JPEG cannot encode transparency: every fill is opaque, on the
    /// `bgcolor`, white by default.
    #[test]
    fn jpeg_is_always_opaque() {
        let jpeg = ImageFormat::JPEG;
        for (pairs, expected) in [
            (vec![], Background::uniform(Fill::Opaque(DEFAULT_BGCOLOR))),
            (
                vec![("transparent", "true")],
                Background::uniform(Fill::Opaque(DEFAULT_BGCOLOR)),
            ),
            (
                vec![("transparent", "true"), ("bgcolor", "red")],
                Background::uniform(Fill::Opaque(RED)),
            ),
            (
                vec![("void-color", "blue")],
                Background {
                    fill: Fill::Opaque(DEFAULT_BGCOLOR),
                    void: Fill::Opaque(BLUE),
                },
            ),
        ] {
            assert_eq!(parse(&pairs, jpeg), Ok(expected), "{pairs:?}");
        }
        // WebP keeps its alpha like PNG.
        assert_eq!(
            parse(&[], ImageFormat::WEBP),
            Ok(Background::uniform(Fill::Keep))
        );
    }

    #[test]
    fn invalid_values_name_the_valid_forms() {
        let png = ImageFormat::Png;
        for (name, value) in [("transparent", "yes"), ("void-transparent", "1")] {
            assert_eq!(
                parse(&[(name, value)], png),
                Err(format!("{name} '{value}' must be true or false"))
            );
        }
        for (name, value) in [
            ("bgcolor", "0xFFF"),
            ("bgcolor", "0x80FF0000"),
            ("bgcolor", "0xGG0000"),
            ("bgcolor", "FF0000"),
            ("bgcolor", "notacolour"),
            ("void-color", "#12345"),
        ] {
            let message = parse(&[(name, value)], png).unwrap_err();
            assert_eq!(
                message,
                format!(
                    "{name} '{value}' must be a hexadecimal colour 0xRRGGBB (or #RRGGBB) or a \
                     W3C web colour name, such as 0x336699, white or SteelBlue"
                )
            );
        }
        // Blank is absent, as for every map parameter.
        assert_eq!(
            parse(&[("transparent", " "), ("bgcolor", "")], png),
            Ok(Background::default())
        );
    }

    #[test]
    fn web_colors_are_the_w3c_names_in_any_case() {
        assert!(WEB_COLORS.windows(2).all(|w| w[0].0 < w[1].0), "sorted");
        assert!(WEB_COLORS
            .iter()
            .all(|(name, _)| name.bytes().all(|b| b.is_ascii_lowercase())));
        for (name, rgb) in [
            ("SteelBlue", [0x46, 0x82, 0xb4]),
            ("STEELBLUE", [0x46, 0x82, 0xb4]),
            ("white", [0xff; 3]),
            ("darkGrey", [0xa9; 3]),
            ("RebeccaPurple", [0x66, 0x33, 0x99]),
            ("violet", [0xee, 0x82, 0xee]),
        ] {
            assert_eq!(web_color(name), Some(rgb), "{name}");
        }
        assert_eq!(web_color("transparent"), None);
        assert_eq!(web_color("fuschia"), None);
    }

    /// The fill and the void fill split each row at its run of columns on
    /// the Earth; a uniform background paints every pixel alike.
    #[test]
    fn paint_splits_rows_at_the_earth() {
        let empty = [0u8; 4];
        let drawn = [10, 20, 30, 255];
        let half = [10, 20, 30, 128];
        // Two rows of four: row 0 has columns 1..3 on the Earth, row 1 none.
        let mut rgba = [empty, drawn, half, empty, empty, empty, empty, empty].concat();
        let background = Background {
            fill: Fill::Opaque(RED),
            void: Fill::Clear(BLUE),
        };
        background.paint(&mut rgba, 4, Some(&[1..3, 0..0]));
        let pixels: Vec<[u8; 4]> = rgba.chunks(4).map(|p| p.try_into().unwrap()).collect();
        let clear_blue = [0, 0, 0xff, 0];
        // `half` over red: (10·128 + 255·127) / 255, 20·128 / 255, 30·128 / 255.
        let blended = [132, 10, 15, 255];
        assert_eq!(
            pixels,
            vec![
                clear_blue, drawn, blended, clear_blue, clear_blue, clear_blue, clear_blue,
                clear_blue
            ]
        );
        // Uniform, or with no Earth map: the fill everywhere.
        let mut rgba = [empty, half].concat();
        let off_earth = 0..0;
        Background::uniform(Fill::Clear(RED)).paint(
            &mut rgba,
            2,
            Some(std::slice::from_ref(&off_earth)),
        );
        assert_eq!(rgba, [[0xff, 0, 0, 0], half].concat());
        let mut rgba = [empty, half].concat();
        background.paint(&mut rgba, 2, None);
        assert_eq!(&rgba[..4], &[0xff, 0, 0, 255]);
        assert_eq!(rgba[7], 255);
        // Keep leaves the pixels as rendered.
        let mut rgba = [empty, half].concat();
        let west = 0..1;
        Background::default().paint(&mut rgba, 2, Some(std::slice::from_ref(&west)));
        assert_eq!(rgba, [empty, half].concat());
    }
}
