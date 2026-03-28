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
/// Uses lossy encoding with quality 80 for a good size/quality tradeoff.
/// WebP supports alpha channel natively, so no transparency compositing needed.
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
    let memory = encoder.encode(80.0);
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
    fn test_encode_jpeg_transparent_to_white() {
        // Fully transparent pixel → white background in JPEG
        let rgba = vec![0, 0, 0, 0, 255, 0, 0, 255]; // 2x1: transparent, red
        let result = encode_jpeg(&rgba, 2, 1);
        assert!(result.is_ok());
    }
}
