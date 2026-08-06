//! Parsers for external palette file formats — GMT color palette tables
//! (`.cpt`) and GDAL color-relief / `.clr` text files.
//!
//! Both convert into the crate's [`Palette`] model, so a user-supplied file
//! is indistinguishable from a built-in palette once loaded into a
//! [`crate::PaletteRegistry`]. Hand-rolled (no serde, no regex) to keep
//! `ds-render` framework-free.
//!
//! What is deliberately NOT supported, since neither affects the rendered
//! result we can represent: GMT per-color transparency (`r/g/b@50`), CMYK
//! color models, and GDAL percentage entries (which need the raster's value
//! range, unavailable at parse time — they produce an explicit error).

use crate::colormap::{parse_hex_color, ColorStop};
use crate::palette::{Interpolation, Palette};

// ---------------------------------------------------------------------------
// GMT color palette tables (.cpt)
// ---------------------------------------------------------------------------

/// Color model a `.cpt` file's triplets are expressed in, taken from its
/// `# COLOR_MODEL = ...` header comment (default RGB).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorModel {
    Rgb,
    Hsv,
}

/// One `z0 color0 z1 color1` row of a `.cpt`.
struct Segment {
    z0: f64,
    c0: [u8; 4],
    z1: f64,
    c1: [u8; 4],
}

/// Parse a GMT color palette table into a [`Palette`] named `name`.
///
/// Segment lines are `z0 color0 z1 color1`, where each color is an `R G B`
/// triplet (whitespace- or slash-separated), a `#RRGGBB` / `#RRGGBBAA` hex
/// literal, or a single 0–255 gray value. Under `# COLOR_MODEL = HSV` (or
/// `+HSV`) triplets are read as hue/saturation/value and converted.
///
/// Boundaries shared by adjacent segments collapse into one stop; a genuine
/// discontinuity (same `z`, different color) is kept as two stops at that
/// value, which [`crate::LinearColorMap`] renders as a hard edge. A file
/// whose every segment is a single flat color (`color0 == color1`) is a
/// class table and yields [`Interpolation::Step`].
///
/// `B` (background) and `F` (foreground) lines are ignored; `N` sets
/// [`Palette::nodata_color`]. Trailing `;label` annotations and GMT's
/// `L`/`U`/`B` annotation flags are tolerated.
pub fn parse_cpt(name: &str, text: &str) -> Result<Palette, String> {
    // Pre-pass: the color model governs how every segment is read, so pick
    // it up before parsing any of them regardless of where it appears.
    let model = detect_color_model(text);

    let mut segments: Vec<Segment> = Vec::new();
    let mut nodata_color: Option<[u8; 4]> = None;

    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = raw.trim();
        // A '#' only opens a comment at the start of a line — mid-line it
        // introduces a hex color.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // GMT allows a trailing ';Label' annotation on any row.
        let line = trimmed.split(';').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut tokens: Vec<&str> = line.split_whitespace().collect();

        // B/F/N rows: background, foreground, nodata.
        if let Some(&first) = tokens.first() {
            if first.len() == 1 {
                match first.to_ascii_uppercase().as_str() {
                    "B" | "F" => continue,
                    "N" => {
                        nodata_color = Some(parse_cpt_color(&tokens[1..], model, lineno, trimmed)?);
                        continue;
                    }
                    _ => {}
                }
            }
        }

        // An annotation flag (L/U/B) may trail the second color; drop it
        // before the token count decides the layout. Guarded on length so a
        // 4-token row's color can never be mistaken for a flag.
        if tokens.len() >= 5
            && tokens
                .last()
                .is_some_and(|t| matches!(t.to_ascii_uppercase().as_str(), "L" | "U" | "B"))
        {
            tokens.pop();
        }

        // Colors are 1 or 3 tokens each, so the row is 4, 6 or 8 tokens. At
        // 6 the split is ambiguous by count alone — the shape of the first
        // color token resolves it.
        let (c0_len, c1_len) = match tokens.len() {
            4 => (1usize, 1usize),
            8 => (3, 3),
            6 => {
                if is_compound_color(tokens[1]) {
                    (1, 3)
                } else {
                    (3, 1)
                }
            }
            n => {
                return Err(cpt_err(
                    lineno,
                    trimmed,
                    &format!("expected 'z0 color0 z1 color1' (4, 6 or 8 tokens), found {n}"),
                ))
            }
        };

        let z0 = parse_cpt_z(tokens[0], lineno, trimmed)?;
        let c0 = parse_cpt_color(&tokens[1..1 + c0_len], model, lineno, trimmed)?;
        let z1 = parse_cpt_z(tokens[1 + c0_len], lineno, trimmed)?;
        let c1 = parse_cpt_color(
            &tokens[2 + c0_len..2 + c0_len + c1_len],
            model,
            lineno,
            trimmed,
        )?;
        segments.push(Segment { z0, c0, z1, c1 });
    }

    if segments.is_empty() {
        return Err("no color segments found in .cpt".to_string());
    }

    // Sort by lower bound first so boundary de-duplication only ever fires
    // between genuinely adjacent segments, and a descending-order file
    // produces the same stop sequence as its ascending twin.
    segments.sort_by(|a, b| a.z0.total_cmp(&b.z0));

    // A table whose every segment is one flat color is a class table, not a
    // ramp: one stop per lower bound, closed by the final upper bound.
    let discrete = segments.iter().all(|s| s.c0 == s.c1);
    let stops = if discrete {
        let mut stops: Vec<ColorStop> = segments
            .iter()
            .map(|s| ColorStop {
                value: s.z0,
                color: s.c0,
            })
            .collect();
        let last = &segments[segments.len() - 1];
        stops.push(ColorStop {
            value: last.z1,
            color: last.c1,
        });
        stops
    } else {
        let mut stops: Vec<ColorStop> = Vec::with_capacity(segments.len() * 2);
        for seg in &segments {
            let lower = ColorStop {
                value: seg.z0,
                color: seg.c0,
            };
            // Skip the lower stop only when the previous segment already
            // ended exactly there with the same color; an equal value with a
            // different color is a discontinuity and both stops are kept.
            if stops.last() != Some(&lower) {
                stops.push(lower);
            }
            stops.push(ColorStop {
                value: seg.z1,
                color: seg.c1,
            });
        }
        stops
    };

    let mut palette = Palette::new(
        name,
        stops,
        if discrete {
            Interpolation::Step
        } else {
            Interpolation::Linear
        },
    );
    palette.nodata_color = nodata_color;
    Ok(palette)
}

/// Scan header comments for `COLOR_MODEL = HSV` / `+HSV`. Anything else
/// (including an absent directive) is RGB.
fn detect_color_model(text: &str) -> ColorModel {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(comment) = trimmed.strip_prefix('#') else {
            continue;
        };
        let upper = comment.to_ascii_uppercase();
        if !upper.contains("COLOR_MODEL") {
            continue;
        }
        let Some((_, value)) = upper.split_once('=') else {
            continue;
        };
        // GMT 5 writes '+HSV' to request hue-wise interpolation; the '+' does
        // not change how the components are read.
        if value.trim().trim_start_matches('+') == "HSV" {
            return ColorModel::Hsv;
        }
    }
    ColorModel::Rgb
}

/// Whether a token carries a whole color on its own (hex, or a `/`- or
/// `-`-separated triplet) rather than being one component of three. A
/// leading `-` is a negative number, not a separator.
fn is_compound_color(token: &str) -> bool {
    token.starts_with('#')
        || token.contains('/')
        || (token.contains('-') && !token.starts_with('-'))
}

fn cpt_err(lineno: usize, line: &str, msg: &str) -> String {
    format!("line {lineno}: {msg}: '{line}'")
}

fn parse_cpt_z(token: &str, lineno: usize, line: &str) -> Result<f64, String> {
    token
        .parse::<f64>()
        .map_err(|_| cpt_err(lineno, line, &format!("invalid z value '{token}'")))
}

/// Parse a `.cpt` color from either 1 token (hex, packed triplet, or gray)
/// or 3 tokens (one component each), in the file's color model.
fn parse_cpt_color(
    tokens: &[&str],
    model: ColorModel,
    lineno: usize,
    line: &str,
) -> Result<[u8; 4], String> {
    match tokens {
        [single] => {
            if single.starts_with('#') {
                return parse_hex_color(single)
                    .map_err(|e| cpt_err(lineno, line, &format!("invalid color '{single}': {e}")));
            }
            let sep = if single.contains('/') {
                Some('/')
            } else if is_compound_color(single) {
                Some('-')
            } else {
                None
            };
            if let Some(sep) = sep {
                let parts: Vec<&str> = single.split(sep).collect();
                if parts.len() != 3 {
                    return Err(cpt_err(
                        lineno,
                        line,
                        &format!("expected 3 color components in '{single}'"),
                    ));
                }
                return triplet(&parts, model, lineno, line);
            }
            // A lone number is a gray level, in either color model.
            let g = component(single, 0.0, 255.0, lineno, line)?;
            let g = g.round() as u8;
            Ok([g, g, g, 255])
        }
        [_, _, _] => triplet(tokens, model, lineno, line),
        other => Err(cpt_err(
            lineno,
            line,
            &format!("expected 1 or 3 color tokens, found {}", other.len()),
        )),
    }
}

/// Convert three component tokens to RGBA under the given color model.
fn triplet(
    parts: &[&str],
    model: ColorModel,
    lineno: usize,
    line: &str,
) -> Result<[u8; 4], String> {
    match model {
        ColorModel::Rgb => {
            let r = component(parts[0], 0.0, 255.0, lineno, line)?;
            let g = component(parts[1], 0.0, 255.0, lineno, line)?;
            let b = component(parts[2], 0.0, 255.0, lineno, line)?;
            Ok([r.round() as u8, g.round() as u8, b.round() as u8, 255])
        }
        ColorModel::Hsv => {
            let h = component(parts[0], 0.0, 360.0, lineno, line)?;
            let s = component(parts[1], 0.0, 1.0, lineno, line)?;
            let v = component(parts[2], 0.0, 1.0, lineno, line)?;
            let [r, g, b] = hsv_to_rgb(h, s, v);
            Ok([r, g, b, 255])
        }
    }
}

/// Parse one color component and range-check it.
fn component(token: &str, min: f64, max: f64, lineno: usize, line: &str) -> Result<f64, String> {
    let v: f64 = token
        .parse()
        .map_err(|_| cpt_err(lineno, line, &format!("invalid color component '{token}'")))?;
    if !(min..=max).contains(&v) {
        return Err(cpt_err(
            lineno,
            line,
            &format!("color component '{token}' out of range {min}..{max}"),
        ));
    }
    Ok(v)
}

/// HSV → RGB (hue in degrees, saturation and value in 0..1).
fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    // 360° wraps back to 0 so the sector index below stays in 0..=5.
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to_u8 = |ch: f64| ((ch + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to_u8(r), to_u8(g), to_u8(b)]
}

// ---------------------------------------------------------------------------
// GDAL color-relief / .clr text files
// ---------------------------------------------------------------------------

/// Parse a GDAL color-relief text file (as accepted by `gdaldem
/// color-relief`, also the `.clr` form) into a [`Palette`] named `name`.
///
/// Each line is `value R G B [A]` (alpha defaults to opaque), where the
/// fields may be separated by whitespace, commas or colons. The color may
/// instead be one of GDAL's named colors (white, black, red, green, blue,
/// yellow, magenta, cyan, gray/grey). An `nv` line sets
/// [`Palette::nodata_color`]. `#` comments and blank lines are ignored.
///
/// Percentage entries (`50%`) are rejected: resolving them needs the
/// raster's value range, which a palette file has no access to.
pub fn parse_gdal_txt(name: &str, text: &str) -> Result<Palette, String> {
    let mut stops: Vec<ColorStop> = Vec::new();
    let mut nodata_color: Option<[u8; 4]> = None;

    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = trimmed
            .split(|c: char| c.is_whitespace() || c == ',' || c == ':')
            .filter(|t| !t.is_empty())
            .collect();
        // A line of nothing but separators carries no fields at all.
        let Some(&head) = tokens.first() else {
            return Err(cpt_err(lineno, trimmed, "expected 'value R G B [A]'"));
        };

        if head.ends_with('%') {
            return Err(cpt_err(
                lineno,
                trimmed,
                &format!("percentage entry '{head}' is not supported; only absolute values are"),
            ));
        }

        let color = parse_gdal_color(&tokens[1..], lineno, trimmed)?;
        if head.eq_ignore_ascii_case("nv") {
            nodata_color = Some(color);
            continue;
        }
        let value = tokens[0]
            .parse::<f64>()
            .map_err(|_| cpt_err(lineno, trimmed, &format!("invalid value '{head}'")))?;
        stops.push(ColorStop { value, color });
    }

    if stops.is_empty() {
        return Err("no color entries found in GDAL color file".to_string());
    }

    // GDAL color-relief interpolates between entries by default.
    let mut palette = Palette::new(name, stops, Interpolation::Linear);
    palette.nodata_color = nodata_color;
    Ok(palette)
}

/// GDAL's named color set, lower-cased.
fn named_color(name: &str) -> Option<[u8; 4]> {
    let rgb = match name.to_ascii_lowercase().as_str() {
        "white" => [255, 255, 255],
        "black" => [0, 0, 0],
        "red" => [255, 0, 0],
        "green" => [0, 255, 0],
        "blue" => [0, 0, 255],
        "yellow" => [255, 255, 0],
        "magenta" => [255, 0, 255],
        "cyan" => [0, 255, 255],
        "gray" | "grey" => [128, 128, 128],
        _ => return None,
    };
    Some([rgb[0], rgb[1], rgb[2], 255])
}

/// Parse the color field of a GDAL line: `R G B [A]`, or a named color with
/// an optional alpha.
fn parse_gdal_color(tokens: &[&str], lineno: usize, line: &str) -> Result<[u8; 4], String> {
    let numeric = |t: &str| t.parse::<f64>().is_ok();
    match tokens {
        [name] | [name, _] if !numeric(name) => {
            let mut color = named_color(name)
                .ok_or_else(|| cpt_err(lineno, line, &format!("unknown color name '{name}'")))?;
            if let [_, alpha] = tokens {
                color[3] = component(alpha, 0.0, 255.0, lineno, line)?.round() as u8;
            }
            Ok(color)
        }
        [r, g, b] | [r, g, b, _] => {
            let a = match tokens {
                [_, _, _, alpha] => component(alpha, 0.0, 255.0, lineno, line)?.round() as u8,
                _ => 255,
            };
            Ok([
                component(r, 0.0, 255.0, lineno, line)?.round() as u8,
                component(g, 0.0, 255.0, lineno, line)?.round() as u8,
                component(b, 0.0, 255.0, lineno, line)?.round() as u8,
                a,
            ])
        }
        other => Err(cpt_err(
            lineno,
            line,
            &format!(
                "expected 'value R G B [A]' or 'value <color-name>', found {} color token(s)",
                other.len()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(value, color)` view of a palette's stops, for compact assertions.
    fn stops_of(p: &Palette) -> Vec<(f64, [u8; 4])> {
        p.stops.iter().map(|s| (s.value, s.color)).collect()
    }

    // -- .cpt ---------------------------------------------------------------

    #[test]
    fn cpt_continuous_ramp_dedupes_shared_boundary() {
        let text = "\
# a two-segment ramp
0   0 0 255   10   0 255 0
10  0 255 0   20   255 0 0
";
        let p = parse_cpt("ramp", text).unwrap();
        assert_eq!(p.name, "ramp");
        assert_eq!(p.title, None);
        assert_eq!(p.interpolation, Interpolation::Linear);
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 255, 255]),
                (10.0, [0, 255, 0, 255]),
                (20.0, [255, 0, 0, 255]),
            ],
            "the shared 10 boundary must collapse to one stop"
        );
        assert_eq!(p.default_range(), Some((0.0, 20.0)));
        assert_eq!(p.nodata_color, None);
    }

    #[test]
    fn cpt_discontinuity_keeps_both_stops() {
        // Segment 1 ramps up to cyan at 10, segment 2 restarts at red — a
        // hard edge, kept as two stops at the same value so the zero-width
        // bracket renders as a jump rather than a blend.
        let text = "\
0   0 0 255    10  0 255 255
10  255 0 0    20  255 255 0
";
        let p = parse_cpt("edge", text).unwrap();
        assert_eq!(p.interpolation, Interpolation::Linear);
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 255, 255]),
                (10.0, [0, 255, 255, 255]),
                (10.0, [255, 0, 0, 255]),
                (20.0, [255, 255, 0, 255]),
            ],
            "both sides of the discontinuity must be present, lower side first"
        );
    }

    #[test]
    fn cpt_discrete_classes_become_step_interpolation() {
        let text = "\
0   128 128 128   10  128 128 128
10  0 255 0       20  0 255 0
20  255 0 0       30  255 0 0
";
        let p = parse_cpt("classes", text).unwrap();
        assert_eq!(p.interpolation, Interpolation::Step);
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [128, 128, 128, 255]),
                (10.0, [0, 255, 0, 255]),
                (20.0, [255, 0, 0, 255]),
                (30.0, [255, 0, 0, 255]),
            ],
            "one stop per class lower bound, closed by the last upper bound"
        );
        // Step semantics: a value inside a class takes that class's color.
        assert_eq!(p.sample(5.0), [128, 128, 128, 255]);
        assert_eq!(p.sample(15.0), [0, 255, 0, 255]);
        assert_eq!(p.sample(25.0), [255, 0, 0, 255]);
    }

    #[test]
    fn cpt_hsv_color_model_converts_to_rgb() {
        let text = "\
# COLOR_MODEL = HSV
0   0 1 1     10   120 1 1
10  120 1 1   20   240 1 1
";
        let p = parse_cpt("hsv", text).unwrap();
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [255, 0, 0, 255]),  // h=0   → red
                (10.0, [0, 255, 0, 255]), // h=120 → green
                (20.0, [0, 0, 255, 255]), // h=240 → blue
            ]
        );
    }

    #[test]
    fn cpt_hsv_accepts_plus_prefix_and_packed_triplets() {
        // GMT 5 writes '+HSV'; the packed 'h-s-v' single-token form is also
        // valid there.
        let text = "\
# COLOR_MODEL = +HSV
0  60-1-1  10  180-0.5-1
";
        let p = parse_cpt("hsv2", text).unwrap();
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [255, 255, 0, 255]),    // h=60  s=1   → yellow
                (10.0, [128, 255, 255, 255]), // h=180 s=0.5 → pale cyan
            ]
        );
    }

    #[test]
    fn cpt_slash_separated_and_hex_and_gray_colors() {
        let slash = parse_cpt("slash", "0 255/0/0 10 0/0/255\n").unwrap();
        assert_eq!(
            stops_of(&slash),
            vec![(0.0, [255, 0, 0, 255]), (10.0, [0, 0, 255, 255])]
        );

        let hex = parse_cpt("hex", "0 #ff0000 10 #0000ff80\n").unwrap();
        assert_eq!(
            stops_of(&hex),
            vec![(0.0, [255, 0, 0, 255]), (10.0, [0, 0, 255, 128])]
        );

        let gray = parse_cpt("gray", "0 0 10 255\n").unwrap();
        assert_eq!(
            stops_of(&gray),
            vec![(0.0, [0, 0, 0, 255]), (10.0, [255, 255, 255, 255])]
        );

        // Mixed widths on one row (6 tokens) resolve by the shape of the
        // first color token, in both orders.
        let mixed = parse_cpt("mixed", "0 255/0/0 10 0 0 255\n").unwrap();
        assert_eq!(
            stops_of(&mixed),
            vec![(0.0, [255, 0, 0, 255]), (10.0, [0, 0, 255, 255])]
        );
        let mixed = parse_cpt("mixed", "0 255 0 0 10 0/0/255\n").unwrap();
        assert_eq!(
            stops_of(&mixed),
            vec![(0.0, [255, 0, 0, 255]), (10.0, [0, 0, 255, 255])]
        );
    }

    #[test]
    fn cpt_nodata_line_sets_nodata_color_and_bf_are_ignored() {
        let text = "\
0 0 0 255 10 255 0 0
B 0 0 0
F 255 255 255
N 128 128 128
";
        let p = parse_cpt("nd", text).unwrap();
        assert_eq!(p.nodata_color, Some([128, 128, 128, 255]));
        assert_eq!(p.stops.len(), 2, "B/F rows must not become stops");

        // Hex form of N is accepted too.
        let p = parse_cpt("nd", "0 0 0 255 10 255 0 0\nN #7f7f7f\n").unwrap();
        assert_eq!(p.nodata_color, Some([127, 127, 127, 255]));
    }

    #[test]
    fn cpt_tolerates_labels_and_annotation_flags() {
        let text = "\
0   0 0 255   10  0 255 0  ; low
10  0 255 0   20  255 0 0  L
";
        let p = parse_cpt("annot", text).unwrap();
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 255, 255]),
                (10.0, [0, 255, 0, 255]),
                (20.0, [255, 0, 0, 255]),
            ]
        );
    }

    #[test]
    fn cpt_descending_file_yields_ascending_stops() {
        let text = "\
20  255 0 0   30  255 255 255
10  0 255 0   20  255 0 0
0   0 0 255   10  0 255 0
";
        let p = parse_cpt("desc", text).unwrap();
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 255, 255]),
                (10.0, [0, 255, 0, 255]),
                (20.0, [255, 0, 0, 255]),
                (30.0, [255, 255, 255, 255]),
            ],
            "stops sort ascending and boundaries still de-duplicate"
        );
        assert!(p.stops.windows(2).all(|w| w[0].value <= w[1].value));
    }

    #[test]
    fn cpt_malformed_lines_report_line_number_and_content() {
        // Wrong token count.
        let err = parse_cpt("bad", "0 0 0 255 10 255 0 0\n0 255 10\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "got: {err}");
        assert!(err.contains("0 255 10"), "got: {err}");

        // Unparsable z.
        let err = parse_cpt("bad", "# header\nxx 0 0 255 10 255 0 0\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "got: {err}");
        assert!(err.contains("invalid z value 'xx'"), "got: {err}");

        // Unparsable color component.
        let err = parse_cpt("bad", "0 0 0 blue 10 255 0 0\n").unwrap_err();
        assert!(err.starts_with("line 1:"), "got: {err}");
        assert!(err.contains("'blue'"), "got: {err}");

        // Out-of-range component.
        let err = parse_cpt("bad", "0 0 0 300 10 255 0 0\n").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");

        // Out-of-range HSV hue.
        let err = parse_cpt("bad", "# COLOR_MODEL = HSV\n0 400 1 1 10 0 1 1\n").unwrap_err();
        assert!(
            err.starts_with("line 2:") && err.contains("out of range"),
            "got: {err}"
        );

        // Nothing usable at all.
        let err = parse_cpt("empty", "# just a comment\n\n").unwrap_err();
        assert!(err.contains("no color segments"), "got: {err}");
    }

    #[test]
    fn hsv_to_rgb_pins_primaries_and_neutrals() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [255, 0, 0]);
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), [0, 255, 0]);
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), [0, 0, 255]);
        assert_eq!(hsv_to_rgb(360.0, 1.0, 1.0), [255, 0, 0], "360 wraps to 0");
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), [255, 255, 255], "s=0 → white");
        assert_eq!(hsv_to_rgb(200.0, 1.0, 0.0), [0, 0, 0], "v=0 → black");
        assert_eq!(hsv_to_rgb(0.0, 0.0, 0.5), [128, 128, 128]);
    }

    // -- GDAL color-relief --------------------------------------------------

    #[test]
    fn gdal_basic_ramp_with_optional_alpha() {
        let text = "\
# elevation ramp
0    0 0 255
500  0 255 0 128
1000 255 0 0
";
        let p = parse_gdal_txt("relief", text).unwrap();
        assert_eq!(p.name, "relief");
        assert_eq!(p.interpolation, Interpolation::Linear);
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 255, 255]),
                (500.0, [0, 255, 0, 128]),
                (1000.0, [255, 0, 0, 255]),
            ]
        );
        assert_eq!(p.nodata_color, None);
        assert_eq!(p.default_range(), Some((0.0, 1000.0)));
    }

    #[test]
    fn gdal_nv_line_sets_nodata_color() {
        let text = "\
nv 0 0 0 0
0  0 0 255
10 255 0 0
";
        let p = parse_gdal_txt("nd", text).unwrap();
        assert_eq!(p.nodata_color, Some([0, 0, 0, 0]));
        assert_eq!(p.stops.len(), 2, "the nv row must not become a stop");

        // Named color and a 3-component form both work for nv.
        let p = parse_gdal_txt("nd", "nv black\n0 white\n").unwrap();
        assert_eq!(p.nodata_color, Some([0, 0, 0, 255]));
    }

    #[test]
    fn gdal_named_colors() {
        let text = "\
0    black
100  RED
200  green
300  blue
400  yellow
500  magenta
600  cyan
700  gray
800  grey
900  white
";
        let p = parse_gdal_txt("named", text).unwrap();
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 0, 255]),
                (100.0, [255, 0, 0, 255]),
                (200.0, [0, 255, 0, 255]),
                (300.0, [0, 0, 255, 255]),
                (400.0, [255, 255, 0, 255]),
                (500.0, [255, 0, 255, 255]),
                (600.0, [0, 255, 255, 255]),
                (700.0, [128, 128, 128, 255]),
                (800.0, [128, 128, 128, 255]),
                (900.0, [255, 255, 255, 255]),
            ],
            "names are case-insensitive"
        );
    }

    #[test]
    fn gdal_unknown_named_color_is_an_error() {
        let err = parse_gdal_txt("bad", "0 white\n100 chartreuse\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "got: {err}");
        assert!(err.contains("chartreuse"), "got: {err}");
    }

    #[test]
    fn gdal_percentage_entries_are_rejected_with_a_clear_message() {
        let err = parse_gdal_txt("pct", "0 0 0 255\n50% 255 0 0\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "got: {err}");
        assert!(
            err.contains("absolute values"),
            "the error must point at the absolute-value requirement: {err}"
        );
        assert!(err.contains("50%"), "got: {err}");
    }

    #[test]
    fn gdal_accepts_comma_and_colon_separators() {
        let p = parse_gdal_txt("sep", "0,0,0,255\n10:255:0:0\n").unwrap();
        assert_eq!(
            stops_of(&p),
            vec![(0.0, [0, 0, 255, 255]), (10.0, [255, 0, 0, 255])]
        );
    }

    #[test]
    fn gdal_descending_file_yields_ascending_stops() {
        let text = "\
1000 255 0 0
500  0 255 0
0    0 0 255
";
        let p = parse_gdal_txt("desc", text).unwrap();
        assert_eq!(
            stops_of(&p),
            vec![
                (0.0, [0, 0, 255, 255]),
                (500.0, [0, 255, 0, 255]),
                (1000.0, [255, 0, 0, 255]),
            ]
        );
        assert!(p.stops.windows(2).all(|w| w[0].value <= w[1].value));
    }

    #[test]
    fn gdal_malformed_lines_report_line_number_and_content() {
        // Too few color tokens.
        let err = parse_gdal_txt("bad", "0 0 0 255\n10 255 0\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "got: {err}");
        assert!(err.contains("10 255 0"), "got: {err}");

        // Unparsable value.
        let err = parse_gdal_txt("bad", "abc 255 0 0\n").unwrap_err();
        assert!(err.contains("invalid value 'abc'"), "got: {err}");

        // Out-of-range component.
        let err = parse_gdal_txt("bad", "0 0 0 300\n").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");

        // Nothing usable at all.
        let err = parse_gdal_txt("empty", "# only comments\n\n   \n").unwrap_err();
        assert!(err.contains("no color entries"), "got: {err}");
    }
}
