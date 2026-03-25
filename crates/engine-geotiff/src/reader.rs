use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ds_core::error::DataServerError;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::geo::GeoTransform;

/// Security limits
const MAX_RASTER_DIMENSION: u32 = 100_000;
const MAX_DECODED_TILE_BYTES: usize = 64 * 1024 * 1024; // 64 MB
#[allow(dead_code)]
const MAX_IFD_CHAIN_DEPTH: usize = 1_000; // used by count_overviews

/// Parsed metadata from a GeoTIFF file's IFD headers.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TiffMetadata {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tiles_across: u32,
    pub tiles_down: u32,
    pub geo_transform: GeoTransform,
    pub nodata: Option<f64>,
}

impl TiffMetadata {
    /// Parse metadata from a GeoTIFF file.
    pub fn from_file(path: &Path) -> Result<Self, DataServerError> {
        let file = File::open(path)
            .map_err(|e| DataServerError::GeoTiff(format!("Cannot open {}: {e}", path.display())))?;
        let mut decoder = Decoder::new(BufReader::new(file))
            .map_err(|e| DataServerError::GeoTiff(format!("Invalid TIFF {}: {e}", path.display())))?;

        let (width, height) = decoder.dimensions()
            .map_err(|e| DataServerError::GeoTiff(format!("Cannot read dimensions: {e}")))?;

        // Security: reject oversized rasters
        if width > MAX_RASTER_DIMENSION || height > MAX_RASTER_DIMENSION {
            return Err(DataServerError::GeoTiff(format!(
                "Raster dimensions {}x{} exceed maximum {}",
                width, height, MAX_RASTER_DIMENSION
            )));
        }

        let tile_width = read_tag_u32(&mut decoder, Tag::TileWidth)
            .ok_or_else(|| DataServerError::GeoTiff("Not a tiled TIFF (TileWidth missing)".into()))?;
        let tile_height = read_tag_u32(&mut decoder, Tag::TileLength)
            .ok_or_else(|| DataServerError::GeoTiff("Not a tiled TIFF (TileLength missing)".into()))?;

        // Security: check decoded tile size
        let decoded_tile_bytes = tile_width as usize * tile_height as usize * 4; // Float32 = 4 bytes
        if decoded_tile_bytes > MAX_DECODED_TILE_BYTES {
            return Err(DataServerError::GeoTiff(format!(
                "Decoded tile size {} bytes exceeds maximum {}",
                decoded_tile_bytes, MAX_DECODED_TILE_BYTES
            )));
        }

        let tiles_across = (width + tile_width - 1) / tile_width;
        let tiles_down = (height + tile_height - 1) / tile_height;

        let geo_transform = parse_geo_transform(&mut decoder, width, height)?;
        let nodata = parse_nodata(&mut decoder);

        Ok(TiffMetadata {
            width,
            height,
            tile_width,
            tile_height,
            tiles_across,
            tiles_down,
            geo_transform,
            nodata,
        })
    }
}

/// Read a single Float32 pixel value from a GeoTIFF at a given pixel coordinate.
/// Opens the file, seeks to the correct tile, decompresses it, and extracts the value.
pub fn read_pixel(path: &Path, metadata: &TiffMetadata, col: u32, row: u32) -> Result<Option<f64>, DataServerError> {
    let tile_col = col / metadata.tile_width;
    let tile_row = row / metadata.tile_height;
    let chunk_index = tile_row * metadata.tiles_across + tile_col;

    let local_col = col % metadata.tile_width;
    let local_row = row % metadata.tile_height;
    let local_idx = (local_row * metadata.tile_width + local_col) as usize;

    let file = File::open(path)
        .map_err(|e| DataServerError::GeoTiff(format!("Cannot open {}: {e}", path.display())))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| DataServerError::GeoTiff(format!("Invalid TIFF: {e}")))?;

    match decoder.read_chunk(chunk_index) {
        Ok(DecodingResult::F32(data)) => {
            if local_idx >= data.len() {
                return Ok(None);
            }
            let value = data[local_idx];
            if value.is_nan() {
                return Ok(None);
            }
            if let Some(nodata) = metadata.nodata {
                if (value as f64 - nodata).abs() < 1e-10 {
                    return Ok(None);
                }
            }
            Ok(Some(value as f64))
        }
        Ok(DecodingResult::F64(data)) => {
            if local_idx >= data.len() {
                return Ok(None);
            }
            let value = data[local_idx];
            if value.is_nan() {
                return Ok(None);
            }
            if let Some(nodata) = metadata.nodata {
                if (value - nodata).abs() < 1e-10 {
                    return Ok(None);
                }
            }
            Ok(Some(value))
        }
        Ok(_) => Err(DataServerError::GeoTiff("Unsupported data type (expected Float32 or Float64)".into())),
        Err(e) => Err(DataServerError::GeoTiff(format!("Failed to read tile: {e}"))),
    }
}

/// Maximum number of pixels in an area query result.
const MAX_AREA_PIXELS: usize = 1_000_000;

/// Read pixel values within a bounding box from a GeoTIFF file.
/// Returns a row-major grid of values [row_start..row_end, col_start..col_end].
pub fn read_bbox(
    path: &Path,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let nx = (col_end - col_start) as usize;
    let ny = (row_end - row_start) as usize;
    let total_pixels = nx * ny;

    if total_pixels > MAX_AREA_PIXELS {
        return Err(DataServerError::InvalidParameter(format!(
            "Area query would return {} pixels, maximum is {}. Use a smaller bbox.",
            total_pixels, MAX_AREA_PIXELS
        )));
    }

    let file = File::open(path)
        .map_err(|e| DataServerError::GeoTiff(format!("Cannot open {}: {e}", path.display())))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| DataServerError::GeoTiff(format!("Invalid TIFF: {e}")))?;

    let mut result = vec![None; total_pixels];

    // Determine which tiles we need to read
    let tile_col_start = col_start / metadata.tile_width;
    let tile_col_end = (col_end - 1) / metadata.tile_width + 1;
    let tile_row_start = row_start / metadata.tile_height;
    let tile_row_end = (row_end - 1) / metadata.tile_height + 1;

    for tile_row in tile_row_start..tile_row_end {
        for tile_col in tile_col_start..tile_col_end {
            let chunk_index = tile_row * metadata.tiles_across + tile_col;

            let tile_data = match decoder.read_chunk(chunk_index) {
                Ok(DecodingResult::F32(data)) => data.iter().map(|v| *v as f64).collect::<Vec<_>>(),
                Ok(DecodingResult::F64(data)) => data.to_vec(),
                Ok(_) => return Err(DataServerError::GeoTiff(
                    "Unsupported data type (expected Float32 or Float64)".into(),
                )),
                Err(e) => return Err(DataServerError::GeoTiff(format!("Failed to read tile: {e}"))),
            };

            // Extract pixels from this tile that fall within our bbox
            let tile_pixel_col_start = tile_col * metadata.tile_width;
            let tile_pixel_row_start = tile_row * metadata.tile_height;

            let overlap_col_start = col_start.max(tile_pixel_col_start);
            let overlap_col_end = col_end.min(tile_pixel_col_start + metadata.tile_width);
            let overlap_row_start = row_start.max(tile_pixel_row_start);
            let overlap_row_end = row_end.min(tile_pixel_row_start + metadata.tile_height);

            for row in overlap_row_start..overlap_row_end {
                for col in overlap_col_start..overlap_col_end {
                    let local_col = col - tile_pixel_col_start;
                    let local_row = row - tile_pixel_row_start;
                    let tile_idx = (local_row * metadata.tile_width + local_col) as usize;

                    if tile_idx >= tile_data.len() {
                        continue;
                    }

                    let value = tile_data[tile_idx];
                    let out_col = (col - col_start) as usize;
                    let out_row = (row - row_start) as usize;
                    let out_idx = out_row * nx + out_col;

                    if value.is_nan() {
                        continue; // leave as None
                    }
                    if let Some(nodata) = metadata.nodata {
                        if (value - nodata).abs() < 1e-10 {
                            continue;
                        }
                    }
                    result[out_idx] = Some(value);
                }
            }
        }
    }

    Ok(result)
}

/// Validate that the GeoTIFF has a reasonable number of overview IFDs (security check).
#[allow(dead_code)]
pub fn count_overviews(path: &Path) -> Result<usize, DataServerError> {
    let file = File::open(path)
        .map_err(|e| DataServerError::GeoTiff(format!("Cannot open {}: {e}", path.display())))?;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| DataServerError::GeoTiff(format!("Invalid TIFF: {e}")))?;

    let mut count = 0;
    while decoder.next_image().is_ok() {
        count += 1;
        if count >= MAX_IFD_CHAIN_DEPTH {
            return Err(DataServerError::GeoTiff(format!(
                "IFD chain exceeds maximum depth {MAX_IFD_CHAIN_DEPTH}"
            )));
        }
    }
    Ok(count)
}

fn read_tag_u32(decoder: &mut Decoder<BufReader<File>>, tag: Tag) -> Option<u32> {
    match decoder.get_tag(tag) {
        Ok(tiff::decoder::ifd::Value::Short(v)) => Some(v as u32),
        Ok(tiff::decoder::ifd::Value::Unsigned(v)) => Some(v),
        _ => None,
    }
}

fn parse_geo_transform(
    decoder: &mut Decoder<BufReader<File>>,
    width: u32,
    height: u32,
) -> Result<GeoTransform, DataServerError> {
    let tiepoint = decoder.get_tag(Tag::Unknown(33922))
        .map_err(|_| DataServerError::GeoTiff("Missing ModelTiepointTag — not a GeoTIFF?".into()))?;
    let pixelscale = decoder.get_tag(Tag::Unknown(33550))
        .map_err(|_| DataServerError::GeoTiff("Missing ModelPixelScaleTag — not a GeoTIFF?".into()))?;

    let tp = extract_doubles(&tiepoint)
        .ok_or_else(|| DataServerError::GeoTiff("Cannot parse ModelTiepointTag".into()))?;
    let ps = extract_doubles(&pixelscale)
        .ok_or_else(|| DataServerError::GeoTiff("Cannot parse ModelPixelScaleTag".into()))?;

    if tp.len() < 6 || ps.len() < 2 {
        return Err(DataServerError::GeoTiff(
            "ModelTiepointTag or ModelPixelScaleTag has too few values".into(),
        ));
    }

    let origin_x = tp[3] - tp[0] * ps[0];
    let origin_y = tp[4] + tp[1] * ps[1];

    // Validate that coordinates look like WGS84 (lon: -180..360, lat: -90..90)
    // We allow up to 360 for origin_x to handle some edge cases
    let bbox_east = origin_x + width as f64 * ps[0];
    let bbox_south = origin_y - height as f64 * ps[1];
    if origin_x < -180.0 || bbox_east > 360.0 || bbox_south < -90.0 || origin_y > 90.0 {
        // Likely a projected CRS
        return Err(DataServerError::GeoTiff(format!(
            "Coordinates suggest a projected CRS (origin: {}, {}). \
             Only WGS84 (EPSG:4326) is supported. Convert with: \
             gdalwarp -t_srs EPSG:4326 -of COG input.tif output.tif",
            origin_x, origin_y
        )));
    }

    Ok(GeoTransform {
        origin_x,
        origin_y,
        pixel_width: ps[0],
        pixel_height: ps[1],
        width,
        height,
    })
}

fn parse_nodata(decoder: &mut Decoder<BufReader<File>>) -> Option<f64> {
    match decoder.get_tag(Tag::Unknown(42113)) {
        Ok(tiff::decoder::ifd::Value::Ascii(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn extract_doubles(value: &tiff::decoder::ifd::Value) -> Option<Vec<f64>> {
    match value {
        tiff::decoder::ifd::Value::List(items) => {
            let mut result = Vec::new();
            for item in items {
                match item {
                    tiff::decoder::ifd::Value::Double(v) => result.push(*v),
                    tiff::decoder::ifd::Value::Float(v) => result.push(*v as f64),
                    tiff::decoder::ifd::Value::Unsigned(v) => result.push(*v as f64),
                    tiff::decoder::ifd::Value::Short(v) => result.push(*v as f64),
                    _ => return None,
                }
            }
            Some(result)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Integration tests require actual GeoTIFF files.
    // These are tested via the spike-geotiff project and engine-level tests.
}
