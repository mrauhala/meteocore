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
}
