pub mod colormap;
mod encode;

pub use colormap::{
    parse_hex_color, BuiltinColormap, ColorMap, ColorStop, LinearColorMap, LutColorMap,
};
pub use encode::encode_png;

use ds_core::error::DataServerError;
use ds_core::map_engine::RasterTile;

/// Render a raster tile to PNG bytes using the given colormap.
pub fn render_tile_png(
    tile: &RasterTile,
    colormap: &dyn ColorMap,
) -> Result<Vec<u8>, DataServerError> {
    let rgba = colorize(tile, colormap);
    encode::encode_png(&rgba, tile.width, tile.height)
}

/// Colorize a raster tile into an RGBA buffer using a colormap.
fn colorize(tile: &RasterTile, colormap: &dyn ColorMap) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((tile.width * tile.height * 4) as usize);
    for value in &tile.values {
        let color = colormap.color(*value);
        rgba.extend_from_slice(&color);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::map_engine::RasterTile;

    #[test]
    fn test_render_tile_png_produces_valid_png() {
        let tile = RasterTile {
            width: 4,
            height: 4,
            values: (0..16).map(|i| Some(i as f64 / 15.0)).collect(),
        };
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 1.0);
        let png_bytes = render_tile_png(&tile, &cmap).unwrap();
        assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn test_nodata_produces_transparent_pixel() {
        let tile = RasterTile {
            width: 1,
            height: 1,
            values: vec![None],
        };
        let cmap = LutColorMap::from_builtin(BuiltinColormap::Grayscale, 0.0, 1.0);
        let rgba = colorize(&tile, &cmap);
        assert_eq!(rgba, vec![0, 0, 0, 0]); // fully transparent
    }
}
