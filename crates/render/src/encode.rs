use ds_core::error::DataServerError;

/// Encode an RGBA buffer to PNG bytes.
///
/// Uses compression level 1 (fast) — optimized for real-time serving.
/// Typical 256x256 radar tile: ~50-80KB, encoding time <3ms.
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DataServerError> {
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

    let mut buf = Vec::with_capacity(expected_len / 2); // rough estimate
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
    let mut config = webp::WebPConfig::new()
        .map_err(|()| DataServerError::Render("WebP config init failed".to_string()))?;
    config.lossless = 1;
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
}
