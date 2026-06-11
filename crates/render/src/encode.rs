use ds_core::error::DataServerError;

/// Maximum number of distinct RGBA colours that can fit in an 8-bit indexed PNG.
const PNG8_MAX_COLORS: usize = 256;

/// Slot count for the open-addressing palette probe table: a power of two at
/// 2× the palette cap, so the load factor never exceeds 50% and linear-probe
/// chains stay short even with all 256 entries occupied.
const PALETTE_TABLE_SLOTS: usize = 512;

// `palette_slot` uses `trailing_zeros()` as log2 — only valid for a
// power-of-two slot count (a non-power-of-two would silently cluster every
// key into a few slots: correct output, catastrophic probing).
const _: () = assert!(PALETTE_TABLE_SLOTS.is_power_of_two());
// The probe loop in `encode_png_indexed` terminates only because the table
// always has a free slot to land on; growing the palette cap without growing
// the table would otherwise turn the 257th-distinct-colour probe into an
// infinite loop.
const _: () = assert!(PALETTE_TABLE_SLOTS >= 2 * PNG8_MAX_COLORS);

/// Hash a packed RGBA pixel to a probe-table slot. FxHash-style multiplicative
/// hash (golden-ratio constant), taking the high bits — the per-pixel cost is
/// one multiply instead of a SipHash round (#376).
#[inline(always)]
fn palette_slot(key: u32) -> usize {
    (key.wrapping_mul(0x9E37_79B9) >> (u32::BITS - PALETTE_TABLE_SLOTS.trailing_zeros())) as usize
}

/// Encode an RGBA buffer to PNG bytes.
///
/// Auto-selects the encoding based on the buffer's colour count:
///
/// * **≤256 distinct RGBA values** → 8-bit indexed-palette PNG ("PNG8"). For
///   colormap-rendered layers (radar, classification, single-parameter
///   rasters) this produces a visually identical image at roughly 3–4×
///   smaller bytes than the 32-bit RGBA path. Matches what FMI's GeoServer
///   emits for its styled raster layers, with no client opt-in required.
/// * **>256 distinct values** → 32-bit RGBA PNG. Continuous gradients and
///   multi-band false-colour layers land here.
///
/// Content-type is `image/png` either way — there is no API knob to choose
/// between them, since the per-pixel scan is bounded (≤ one probe-table
/// lookup per output pixel, early-exit at 257) and clients can't tell the two
/// encodings apart without decoding. The scan is deterministic (palette
/// ordered by first pixel-occurrence) so two encodes of the same buffer
/// produce byte-identical output and the content-derived ETag stays stable
/// across requests + redeploys (#145 invariant).
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DataServerError> {
    // Cast to usize *before* multiplying so the product can't silently wrap
    // u32. Unreachable via the API layer (dimensions are capped well below
    // u32::MAX there), but `encode_png` has no internal cap — a non-API
    // caller (fuzz target, test, future engine code) could otherwise pass a
    // wrapped `expected_len` and slip an inconsistent buffer past this check.
    let expected_len = (width as usize) * (height as usize) * 4;
    if rgba.len() != expected_len {
        return Err(DataServerError::Render(format!(
            "RGBA buffer length {} does not match {}x{}x4 = {}",
            rgba.len(),
            width,
            height,
            expected_len,
        )));
    }
    // Length is validated; the two helpers can skip re-checking it.
    match encode_png_indexed(rgba, width, height)? {
        Some(bytes) => Ok(bytes),
        None => encode_png_rgba(rgba, width, height),
    }
}

/// Encode an RGBA buffer as a 32-bit RGBA PNG.
///
/// Internal helper for the >256-colour fallback path of [`encode_png`].
/// `rgba` length is assumed pre-validated by the caller.
fn encode_png_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DataServerError> {
    let mut buf = Vec::with_capacity(rgba.len() / 2); // rough estimate
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);

        let mut writer = encoder
            .write_header()
            .map_err(|e| DataServerError::Render(format!("PNG header error: {e}")))?;

        writer
            .write_image_data(rgba)
            .map_err(|e| DataServerError::Render(format!("PNG write error: {e}")))?;
    }

    Ok(buf)
}

/// Try to encode an RGBA buffer as an 8-bit indexed-palette PNG.
///
/// Returns `Ok(Some(bytes))` when the buffer fits in ≤256 distinct RGBA
/// colours, `Ok(None)` when it doesn't (caller's signal to fall back to
/// the 32-bit RGBA path), or `Err(_)` on a true encoder failure.
///
/// The scan is single-pass and deterministic: the palette is ordered by
/// first pixel-occurrence, so two encodes of the same buffer produce
/// byte-identical output. `rgba` length is assumed pre-validated by the
/// caller.
fn encode_png_indexed(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<Option<Vec<u8>>, DataServerError> {
    let mut palette: Vec<[u8; 4]> = Vec::with_capacity(PNG8_MAX_COLORS);
    let mut indices: Vec<u8> = Vec::with_capacity((width as usize) * (height as usize));

    // Open-addressing probe table replacing the former per-pixel SipHash
    // HashMap lookup (#376): `table_idx[slot]` holds palette-index + 1
    // (0 = empty slot), `table_key[slot]` the packed RGBA it maps. Colormapped
    // rasters are extremely run-heavy, so a previous-pixel memo short-circuits
    // most probes entirely.
    let mut table_idx = [0u16; PALETTE_TABLE_SLOTS];
    let mut table_key = [0u32; PALETTE_TABLE_SLOTS];
    let mut last: Option<(u32, u8)> = None;

    for pixel in rgba.chunks_exact(4) {
        let key = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
        if let Some((last_key, last_idx)) = last {
            if last_key == key {
                indices.push(last_idx);
                continue;
            }
        }
        let mut slot = palette_slot(key);
        let idx = loop {
            let stored = table_idx[slot];
            if stored == 0 {
                // Empty slot — `key` is a new colour.
                if palette.len() >= PNG8_MAX_COLORS {
                    // Palette would overflow — signal the caller to fall
                    // through to the RGBA path. No partial state escapes this
                    // function.
                    return Ok(None);
                }
                let idx = palette.len() as u8;
                table_idx[slot] = idx as u16 + 1;
                table_key[slot] = key;
                palette.push(key.to_le_bytes());
                break idx;
            }
            if table_key[slot] == key {
                break (stored - 1) as u8;
            }
            slot = (slot + 1) % PALETTE_TABLE_SLOTS;
        };
        indices.push(idx);
        last = Some((key, idx));
    }

    // Flatten the palette into the PLTE chunk (RGB triples) and, when any
    // entry has α < 255, the tRNS chunk (per-entry alpha bytes). PNG specifies
    // tRNS as a prefix: entries beyond its length default to opaque, so we
    // emit it only up to the last non-opaque entry to keep it short.
    let mut plte = Vec::with_capacity(palette.len() * 3);
    for &[r, g, b, _] in &palette {
        plte.extend_from_slice(&[r, g, b]);
    }
    let last_non_opaque = palette
        .iter()
        .rposition(|&[_, _, _, a]| a != 255)
        .map(|i| i + 1);
    let trns: Option<Vec<u8>> = last_non_opaque.map(|len| {
        let mut v = Vec::with_capacity(len);
        for &[_, _, _, a] in &palette[..len] {
            v.push(a);
        }
        v
    });

    // Indexed PNG compresses dramatically better than RGBA at the same zlib
    // level — keep `Fast` so encoding stays well under the render budget
    // while still shrinking the body 3–4× vs the RGBA path.
    let mut buf = Vec::with_capacity((width as usize) * (height as usize) / 2);
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_palette(plte);
        // Move `trns` into the encoder rather than cloning — it's a local that
        // drops right after this block, and this fires on every indexed encode
        // with a transparent class (≈ every radar tile).
        if let Some(t) = trns {
            encoder.set_trns(t);
        }

        let mut writer = encoder
            .write_header()
            .map_err(|e| DataServerError::Render(format!("PNG8 header error: {e}")))?;
        writer
            .write_image_data(&indices)
            .map_err(|e| DataServerError::Render(format!("PNG8 write error: {e}")))?;
    }

    Ok(Some(buf))
}

/// Encode an RGBA buffer to JPEG bytes.
///
/// Drops the alpha channel (JPEG doesn't support transparency).
/// Quality 85 gives a good size/quality tradeoff.
pub fn encode_jpeg(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DataServerError> {
    let expected_len = (width * height * 4) as usize;
    if rgba.len() != expected_len {
        return Err(DataServerError::Render(format!(
            "RGBA buffer length {} does not match {}x{}x4 = {}",
            rgba.len(),
            width,
            height,
            expected_len,
        )));
    }

    // Convert RGBA to RGB (drop alpha)
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for pixel in rgba.chunks_exact(4) {
        // Premultiply alpha onto white background for non-opaque pixels
        let a = pixel[3] as f32 / 255.0;
        let r = (pixel[0] as f32 * a + 255.0 * (1.0 - a)) as u8;
        let g = (pixel[1] as f32 * a + 255.0 * (1.0 - a)) as u8;
        let b = (pixel[2] as f32 * a + 255.0 * (1.0 - a)) as u8;
        rgb.extend_from_slice(&[r, g, b]);
    }

    let mut buf = Vec::with_capacity((width * height * 3) as usize);
    let encoder = jpeg_encoder::Encoder::new(&mut buf, 85);
    encoder
        .encode(
            &rgb,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|e| DataServerError::Render(format!("JPEG encode error: {e}")))?;
    Ok(buf)
}

/// Encode an RGBA buffer to WebP bytes.
///
/// Uses lossless encoding. Our raster output is colormapped radar/NWP tiles:
/// hard class boundaries drawn from a small palette. Lossy WebP introduces
/// ringing around those edges and can shift pixels off the palette, which
/// effectively corrupts the encoded data values. Lossless preserves every
/// pixel exactly while still compressing the limited palette well.
/// WebP supports alpha channel natively, so no transparency compositing needed.
///
/// We set libwebp's `exact` flag so the RGB channels are preserved even under
/// fully transparent (alpha == 0) pixels. By default libwebp's lossless mode
/// rewrites the RGB of transparent regions to improve compression, which would
/// make the output not byte-exact. For nodata pixels the RGB is irrelevant to
/// the viewer, but keeping the encode truly exact avoids surprises and keeps
/// the round-trip guarantee unconditional.
pub fn encode_webp(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DataServerError> {
    let expected_len = (width * height * 4) as usize;
    if rgba.len() != expected_len {
        return Err(DataServerError::Render(format!(
            "RGBA buffer length {} does not match {}x{}x4 = {}",
            rgba.len(),
            width,
            height,
            expected_len,
        )));
    }

    let encoder = webp::Encoder::from_rgba(rgba, width, height);

    // Mirror `Encoder::encode_lossless()` but enable `exact` to keep transparent
    // pixels' RGB intact. `WebPConfig::new()` only fails if libwebp's version
    // doesn't match the header — treat that as a render error rather than panic.
    //
    // `WebPConfig::new()` uses the default preset, which leaves `method = 4`
    // (a lossy-oriented effort level). `encode_lossless()` applies the lossless
    // preset which sets `method = 0` (fastest lossless encoder); we replicate
    // that here so we don't pay extra encode latency under the render semaphore.
    let mut config = webp::WebPConfig::new()
        .map_err(|()| DataServerError::Render("WebP config init failed".to_string()))?;
    config.lossless = 1;
    config.method = 0;
    config.alpha_compression = 0;
    config.quality = 75.0;
    config.exact = 1;

    let memory = encoder
        .encode_advanced(&config)
        .map_err(|e| DataServerError::Render(format!("WebP encode error: {e:?}")))?;
    Ok(memory.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_png_valid() {
        let rgba = vec![255u8; 4 * 4 * 4]; // 4x4 white
        let result = encode_png(&rgba, 4, 4);
        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn test_encode_png_wrong_size() {
        let rgba = vec![0u8; 10]; // wrong size
        let result = encode_png(&rgba, 4, 4);
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_jpeg_valid() {
        let rgba = vec![255u8; 4 * 4 * 4]; // 4x4 white
        let result = encode_jpeg(&rgba, 4, 4);
        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert!(bytes[0] == 0xFF && bytes[1] == 0xD8); // JPEG SOI marker
    }

    #[test]
    fn test_encode_webp_valid() {
        let rgba = vec![255u8; 4 * 4 * 4]; // 4x4 white
        let result = encode_webp(&rgba, 4, 4);
        assert!(result.is_ok());
        let bytes = result.unwrap();
        // WebP files start with RIFF header
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
    }

    #[test]
    fn test_encode_webp_lossless_roundtrip() {
        // 4x4 RGBA with sharp edges, distinct per-channel values, and varied
        // alpha (incl. a non-opaque pixel) so the decoder takes the RGBA path.
        // Lossy WebP would shift these values; lossless must reproduce them
        // byte-for-byte.
        let width = 4u32;
        let height = 4u32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                if x == 0 && y == 0 {
                    // Fully transparent pixel with NON-ZERO RGB: proves the
                    // RGB channels survive even where alpha == 0. A lossy (or
                    // alpha-discarding) codec would not reproduce (200,100,50,0).
                    rgba.extend_from_slice(&[200, 100, 50, 0]);
                    continue;
                }
                let r = (x * 60) as u8;
                let g = (y * 60) as u8;
                let b = ((x + y) * 30) as u8;
                // Hard edge: half opaque.
                let a = if (x + y) % 2 == 0 { 128 } else { 255 };
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }

        let bytes = encode_webp(&rgba, width, height).expect("encode should succeed");

        let decoded = webp::Decoder::new(&bytes)
            .decode()
            .expect("encoded WebP should decode");
        assert_eq!(decoded.width(), width);
        assert_eq!(decoded.height(), height);
        assert!(decoded.is_alpha(), "decoded image should retain alpha");

        // WebPImage derefs to its raw RGBA bytes.
        assert_eq!(
            &*decoded,
            rgba.as_slice(),
            "lossless WebP must reproduce the input RGBA exactly"
        );
    }

    #[test]
    fn test_encode_jpeg_transparent_to_white() {
        // Fully transparent pixel → white background in JPEG
        let rgba = vec![0, 0, 0, 0, 255, 0, 0, 255]; // 2x1: transparent, red
        let result = encode_jpeg(&rgba, 2, 1);
        assert!(result.is_ok());
    }

    // --- Auto-PNG8 (indexed-palette) --------------------------------------

    /// Build a `width × height` RGBA tile by mapping each pixel through `f`.
    fn rgba_from<F: Fn(u32, u32) -> [u8; 4]>(width: u32, height: u32, f: F) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                out.extend_from_slice(&f(x, y));
            }
        }
        out
    }

    /// Decode a PNG back to straight RGBA so the per-pixel-equivalence
    /// roundtrip assertions stay framework-agnostic. Returns the colour
    /// type too so a test can assert which branch (indexed vs RGBA) ran.
    fn decode_png_to_rgba(bytes: &[u8]) -> (u32, u32, png::ColorType, Vec<u8>) {
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder.read_info().unwrap();
        let info = reader.info().clone();
        let mut raw = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut raw).unwrap();
        let w = frame.width;
        let h = frame.height;
        let used = &raw[..frame.buffer_size()];
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        match frame.color_type {
            png::ColorType::Indexed => {
                // An indexed PNG without a PLTE is malformed; `.expect` keeps
                // the failure legible instead of an opaque OOB index panic on
                // the next line (the encoder always writes one, so this only
                // fires on a truncated/foreign fixture).
                let plte = info
                    .palette
                    .as_deref()
                    .expect("indexed PNG must carry a PLTE chunk");
                let trns = info.trns.as_deref().unwrap_or(&[]);
                for &idx in used {
                    let p = idx as usize;
                    let r = plte[p * 3];
                    let g = plte[p * 3 + 1];
                    let b = plte[p * 3 + 2];
                    let a = if p < trns.len() { trns[p] } else { 255 };
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
            }
            png::ColorType::Rgba => rgba.extend_from_slice(used),
            other => panic!("unexpected decoded colour type: {other:?}"),
        }
        (w, h, frame.color_type, rgba)
    }

    /// Whether a PNG stream carries a `tRNS` chunk, determined by decoding
    /// the header rather than scanning raw bytes — a raw `b"tRNS"` search
    /// can match inside the DEFLATE-compressed IDAT payload and fire
    /// spuriously on larger images.
    fn png_has_trns(bytes: &[u8]) -> bool {
        let decoder = png::Decoder::new(bytes);
        let reader = decoder.read_info().unwrap();
        reader.info().trns.is_some()
    }

    #[test]
    #[ignore = "ad-hoc timing, run manually with --release"]
    fn bench_palette_scan() {
        let palette: [[u8; 4]; 16] = [
            [0, 0, 0, 0],
            [10, 20, 30, 255],
            [200, 0, 0, 255],
            [0, 200, 0, 255],
            [0, 0, 200, 255],
            [255, 255, 0, 255],
            [0, 255, 255, 255],
            [255, 0, 255, 255],
            [128, 128, 128, 200],
            [64, 64, 64, 150],
            [255, 128, 0, 255],
            [128, 0, 255, 255],
            [0, 128, 255, 255],
            [128, 255, 0, 255],
            [255, 0, 128, 255],
            [0, 255, 128, 255],
        ];
        // Run-heavy variant (realistic colormapped raster)
        let runs = rgba_from(1024, 1024, |x, y| {
            palette[((x / 32 + y / 32) % 16) as usize]
        });
        // Noisy variant (worst case for the memo)
        let noisy = rgba_from(1024, 1024, |x, y| {
            let mut s: u32 = x
                .wrapping_mul(1_103_515_245)
                .wrapping_add(y.wrapping_mul(12_345));
            s ^= s >> 16;
            palette[(s as usize) & 0xF]
        });
        for (name, buf) in [("runs", &runs), ("noisy", &noisy)] {
            // Warmup
            for _ in 0..3 {
                let _ = encode_png_indexed(buf, 1024, 1024).unwrap();
            }
            let n = 20;
            let t0 = std::time::Instant::now();
            for _ in 0..n {
                let _ = encode_png_indexed(buf, 1024, 1024).unwrap();
            }
            println!("{name}: {:?}/encode", t0.elapsed() / n);
        }
    }

    #[test]
    fn encode_png_auto_palettes_when_under_256_colors() {
        // 16-colour pattern over 256×256: `encode_png` must auto-select the
        // indexed-palette path and round-trip every pixel byte-for-byte.
        // Pixel-equivalence is the headline correctness guarantee for a
        // colormap-output layer.
        let palette: [[u8; 4]; 16] = [
            [0, 0, 0, 0],
            [10, 20, 30, 255],
            [200, 0, 0, 255],
            [0, 200, 0, 255],
            [0, 0, 200, 255],
            [255, 255, 0, 255],
            [0, 255, 255, 255],
            [255, 0, 255, 255],
            [128, 128, 128, 200],
            [64, 64, 64, 150],
            [255, 128, 0, 255],
            [128, 0, 255, 255],
            [0, 128, 255, 255],
            [128, 255, 0, 255],
            [255, 0, 128, 255],
            [0, 255, 128, 255],
        ];
        let rgba = rgba_from(256, 256, |x, y| palette[((x + y) % 16) as usize]);
        let bytes = encode_png(&rgba, 256, 256).unwrap();
        let (w, h, ct, decoded) = decode_png_to_rgba(&bytes);
        assert_eq!((w, h), (256, 256));
        assert_eq!(
            ct,
            png::ColorType::Indexed,
            "≤256-colour input must auto-emit an indexed PNG"
        );
        assert_eq!(
            decoded, rgba,
            "auto-PNG8 path must round-trip every pixel exactly"
        );
    }

    #[test]
    fn encode_png_auto_palette_is_at_least_three_times_smaller() {
        // Issue #252 sets the bar at "≥3× smaller for the same image" — pit
        // the auto-PNG8 path against the explicit RGBA fallback on a frame
        // with entropy-rich pixel-to-pixel transitions (not just big
        // contiguous bands, which compress trivially under both encoders).
        // Use a 16-entry palette in a noisy pseudo-random pattern so RGBA
        // gets its realistic ~3 byte/px DEFLATE share and the indexed path
        // lands near 1 byte/px.
        let palette: [[u8; 4]; 16] = [
            [0, 0, 0, 0],
            [0, 128, 255, 255],
            [0, 200, 0, 255],
            [255, 255, 0, 255],
            [255, 128, 0, 255],
            [255, 0, 0, 255],
            [180, 0, 180, 255],
            [255, 255, 255, 255],
            [0, 50, 200, 255],
            [0, 230, 0, 255],
            [200, 200, 0, 255],
            [255, 100, 0, 255],
            [200, 0, 0, 255],
            [150, 0, 150, 255],
            [80, 80, 80, 255],
            [40, 40, 40, 200],
        ];
        // Cheap, deterministic LCG so each pixel picks an unrelated palette
        // entry — defeats run-length compression on the RGBA side while still
        // keeping the buffer in-palette for the indexed path.
        let rgba = rgba_from(1024, 1024, |x, y| {
            let mut s: u32 = x
                .wrapping_mul(1_103_515_245)
                .wrapping_add(y.wrapping_mul(12_345));
            s ^= s >> 16;
            palette[(s as usize) & 0xF]
        });
        // Public auto path: hits the indexed branch.
        let auto = encode_png(&rgba, 1024, 1024).unwrap();
        // Forced RGBA path for the size comparison — accessible because
        // tests live inside the same module.
        let rgba_only = encode_png_rgba(&rgba, 1024, 1024).unwrap();
        assert!(
            (rgba_only.len() as f64 / auto.len() as f64) >= 3.0,
            "auto-PNG8 {} bytes vs forced-RGBA {} bytes — ratio {:.2}× must clear 3×",
            auto.len(),
            rgba_only.len(),
            rgba_only.len() as f64 / auto.len() as f64,
        );
    }

    #[test]
    fn encode_png_is_deterministic() {
        // ETag stability hinges on byte-identical output for byte-identical
        // input. Two encodes of the same buffer must produce the same bytes —
        // no encoder-side timestamps or randomisation.
        //
        // The pattern is constrained to 16 distinct colours so this test
        // exercises the **indexed-palette path** specifically — that path's
        // determinism is *our* invariant (palette ordered by first
        // pixel-occurrence). The RGBA fallback's determinism is the `png`
        // crate's responsibility and is incidentally covered elsewhere.
        let rgba = rgba_from(64, 64, |x, y| {
            [((x % 4) * 64) as u8, ((y % 4) * 64) as u8, 0, 255]
        });
        let a = encode_png(&rgba, 64, 64).unwrap();
        let b = encode_png(&rgba, 64, 64).unwrap();
        assert_eq!(a, b, "PNG encoding must be deterministic for a given input");
        // Guard against a silent regression: if this test ever falls back to
        // RGBA (e.g. someone widens the colour range above 256), it would
        // stop testing the invariant it claims to.
        let decoder = png::Decoder::new(&a[..]);
        let reader = decoder.read_info().unwrap();
        assert_eq!(
            reader.info().color_type,
            png::ColorType::Indexed,
            "this test must exercise the indexed-palette path, not the RGBA fallback"
        );
    }

    #[test]
    fn encode_png_stays_indexed_at_exactly_256_colors() {
        // Exactly 256 distinct colours — the palette cap and the probe
        // table's maximum load. Must still take the indexed path and
        // round-trip every pixel exactly (exercises full probe chains and
        // the stored-index encoding at the boundary, #376).
        let rgba = rgba_from(256, 16, |x, _| {
            [x as u8, (x as u8).wrapping_mul(37), 99, 255]
        });
        let bytes = encode_png(&rgba, 256, 16).unwrap();
        let (w, h, ct, decoded) = decode_png_to_rgba(&bytes);
        assert_eq!((w, h), (256, 16));
        assert_eq!(
            ct,
            png::ColorType::Indexed,
            "exactly 256 colours must still take the indexed path"
        );
        assert_eq!(decoded, rgba, "256-colour boundary must round-trip exactly");
    }

    #[test]
    fn encode_png_falls_back_to_rgba_above_256_colors() {
        // 257 distinct opaque colours along a row → palette overflow forces
        // the RGBA fallback. Decoded output must declare RGBA colour type so
        // we know which branch ran.
        let mut rgba = Vec::with_capacity(257 * 4);
        for i in 0..257u32 {
            rgba.extend_from_slice(&[(i & 0xFF) as u8, (i >> 8) as u8, 0, 255]);
        }
        let bytes = encode_png(&rgba, 257, 1).unwrap();
        let decoder = png::Decoder::new(&bytes[..]);
        let reader = decoder.read_info().unwrap();
        assert_eq!(
            reader.info().color_type,
            png::ColorType::Rgba,
            "above 256 colours must fall back to RGBA"
        );
    }

    #[test]
    fn encode_png_writes_trns_only_when_palette_has_alpha() {
        // All-opaque palette → no `tRNS` chunk (saves bytes; matches GeoServer).
        let rgba = rgba_from(4, 4, |x, y| [x as u8 * 10, y as u8 * 10, 128, 255]);
        let bytes = encode_png(&rgba, 4, 4).unwrap();
        assert!(
            !png_has_trns(&bytes),
            "fully-opaque palette must not emit a tRNS chunk"
        );

        // Add a transparent entry → `tRNS` chunk must appear.
        let rgba_alpha = rgba_from(4, 4, |x, y| {
            if x == 0 && y == 0 {
                [0, 0, 0, 0]
            } else {
                [x as u8 * 10, y as u8 * 10, 128, 255]
            }
        });
        let bytes = encode_png(&rgba_alpha, 4, 4).unwrap();
        assert!(
            png_has_trns(&bytes),
            "palette with any non-opaque entry must emit tRNS"
        );
    }
}
