//! Minimal embedded 5×7 bitmap font for drawing text into RGBA buffers.
//!
//! `ds-render` must stay framework-free (only `ds-core` + `png`, see CLAUDE.md),
//! so we can't pull a font-rasterization crate just to label a legend. This
//! module is a tiny self-contained alternative: a hand-authored 5×7 glyph table
//! covering printable ASCII (digits, punctuation, A–Z, a–z) plus the degree
//! sign, and two helpers — [`draw_text`] and [`text_width`] — that blit those
//! glyphs into a flat RGBA pixel buffer. It is used by the WMS
//! `GetLegendGraphic` legend ([`crate::render_legend`]) to draw tick-value
//! labels and a title, and is general enough for any other small in-image text.
//!
//! Glyph encoding: each glyph is 5 px wide × 7 px tall, stored as `[u8; 7]` —
//! one byte per row, top row first. Only the low 5 bits of each byte are used,
//! bit 4 (`0b10000`) is the **leftmost** pixel and bit 0 the rightmost. Writing
//! the rows as `0bXXXXX` binary literals makes each glyph a readable little
//! picture in source, so the table is reviewable by eye.

/// Glyph cell width in pixels (before scaling).
pub const GLYPH_W: u32 = 5;
/// Glyph cell height in pixels (before scaling).
pub const GLYPH_H: u32 = 7;
/// Horizontal gap between adjacent glyphs in pixels (before scaling).
pub const GLYPH_GAP: u32 = 1;
/// Horizontal advance per glyph (cell + gap), before scaling.
pub const GLYPH_ADVANCE: u32 = GLYPH_W + GLYPH_GAP;

/// Return the 5×7 bitmap for `c` (7 rows, low 5 bits each, MSB = leftmost).
///
/// Unknown characters render as a blank cell (all-zero rows) so unsupported
/// glyphs leave a space rather than garbage — the caller still advances the
/// cursor, keeping alignment predictable.
pub fn glyph(c: char) -> [u8; 7] {
    match c {
        ' ' => [0; 7],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        ',' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b00100, 0b01000,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '+' => [
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '[' => [
            0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110,
        ],
        ']' => [
            0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110,
        ],
        ':' => [
            0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000,
        ],
        '%' => [
            0b11000, 0b11001, 0b00010, 0b00100, 0b01000, 0b10011, 0b00011,
        ],
        // Degree sign (U+00B0) — common in units like °C.
        '\u{00B0}' => [
            0b01100, 0b10010, 0b10010, 0b01100, 0b00000, 0b00000, 0b00000,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        'a' => [
            0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111,
        ],
        'b' => [
            0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110,
        ],
        'c' => [
            0b00000, 0b00000, 0b01110, 0b10001, 0b10000, 0b10001, 0b01110,
        ],
        'd' => [
            0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111,
        ],
        'e' => [
            0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110,
        ],
        'f' => [
            0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000,
        ],
        'g' => [
            0b00000, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110,
        ],
        'h' => [
            0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
        ],
        'i' => [
            0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'j' => [
            0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'k' => [
            0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010,
        ],
        'l' => [
            0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'm' => [
            0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001,
        ],
        'n' => [
            0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
        ],
        'o' => [
            0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'p' => [
            0b00000, 0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000,
        ],
        'q' => [
            0b00000, 0b01101, 0b10011, 0b10001, 0b01111, 0b00001, 0b00001,
        ],
        'r' => [
            0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000,
        ],
        's' => [
            0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110,
        ],
        't' => [
            0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110,
        ],
        'u' => [
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101,
        ],
        'v' => [
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'w' => [
            0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010,
        ],
        'x' => [
            0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001,
        ],
        'y' => [
            0b00000, 0b10001, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110,
        ],
        'z' => [
            0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        _ => [0; 7],
    }
}

/// Pixel width of `text` rendered at integer `scale` (≥1), including inter-glyph
/// gaps but excluding the trailing gap after the last glyph. Empty text is 0.
pub fn text_width(text: &str, scale: u32) -> u32 {
    let n = text.chars().count() as u32;
    if n == 0 {
        return 0;
    }
    let scale = scale.max(1);
    // n cells + (n-1) gaps.
    (n * GLYPH_W + (n - 1) * GLYPH_GAP) * scale
}

/// Blit `text` into the `width`×`height` RGBA buffer with its top-left corner at
/// `(x, y)`, in `color` (RGBA), at integer `scale` (≥1, each glyph pixel becomes
/// a `scale`×`scale` block).
///
/// Set pixels are written opaquely as `color` (no alpha blending) — legend text
/// is solid on a solid background, so a straight overwrite is both correct and
/// deterministic. Drawing is clipped to the buffer bounds, and the origin may be
/// negative (off-canvas glyphs are simply clipped), so callers can vertically
/// centre a label without bounds-checking themselves.
// buffer + dims + origin + text + colour + scale are all genuine, independent
// inputs to a low-level blit; bundling them into a struct would only obscure it.
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    text: &str,
    color: [u8; 4],
    scale: u32,
) {
    let scale = scale.max(1) as i32;
    let mut cursor_x = x;
    for ch in text.chars() {
        let bitmap = glyph(ch);
        for (row, bits) in bitmap.iter().enumerate() {
            for col in 0..GLYPH_W as i32 {
                // bit 4 = leftmost pixel.
                if (bits >> (GLYPH_W as i32 - 1 - col)) & 1 == 0 {
                    continue;
                }
                let px0 = cursor_x + col * scale;
                let py0 = y + row as i32 * scale;
                for dy in 0..scale {
                    let py = py0 + dy;
                    if py < 0 || py >= height as i32 {
                        continue;
                    }
                    for dx in 0..scale {
                        let px = px0 + dx;
                        if px < 0 || px >= width as i32 {
                            continue;
                        }
                        let idx = ((py as u32 * width + px as u32) * 4) as usize;
                        rgba[idx] = color[0];
                        rgba[idx + 1] = color[1];
                        rgba[idx + 2] = color[2];
                        rgba[idx + 3] = color[3];
                    }
                }
            }
        }
        cursor_x += GLYPH_ADVANCE as i32 * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_width_accounts_for_gaps() {
        // One glyph: just the cell, no trailing gap.
        assert_eq!(text_width("0", 1), GLYPH_W);
        // Three glyphs at scale 1: 3 cells + 2 gaps.
        assert_eq!(text_width("123", 1), 3 * GLYPH_W + 2 * GLYPH_GAP);
        // Scale multiplies the whole advance.
        assert_eq!(text_width("123", 2), (3 * GLYPH_W + 2 * GLYPH_GAP) * 2);
        assert_eq!(text_width("", 1), 0);
    }

    #[test]
    fn unknown_glyph_is_blank_not_garbage() {
        // A char we don't have a bitmap for renders as an empty cell.
        assert_eq!(glyph('\u{2603}'), [0u8; 7]); // ☃
        assert_eq!(glyph(' '), [0u8; 7]);
        // But characters we do have are non-empty.
        assert_ne!(glyph('A'), [0u8; 7]);
        assert_ne!(glyph('0'), [0u8; 7]);
        assert_ne!(glyph('\u{00B0}'), [0u8; 7]); // degree sign
    }

    #[test]
    fn draw_text_sets_pixels_within_bounds_only() {
        let (w, h) = (40u32, 9u32);
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        draw_text(&mut rgba, w, h, 1, 1, "Hi", [0, 0, 0, 255], 1);
        // At least one pixel was darkened.
        let any_black = rgba.chunks_exact(4).any(|p| p == [0, 0, 0, 255]);
        assert!(any_black, "expected some glyph pixels to be drawn");
        // The buffer length is unchanged (no out-of-bounds writes panicked).
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn draw_text_clips_negative_and_overflow_origin() {
        let (w, h) = (10u32, 10u32);
        let mut rgba = vec![255u8; (w * h * 4) as usize];
        // Far off-canvas in every direction — must not panic, must not write.
        draw_text(&mut rgba, w, h, -100, -100, "ABC", [0, 0, 0, 255], 2);
        draw_text(&mut rgba, w, h, 1000, 1000, "ABC", [0, 0, 0, 255], 2);
        assert!(
            rgba.iter().all(|&b| b == 255),
            "no pixels should be touched"
        );
    }
}
