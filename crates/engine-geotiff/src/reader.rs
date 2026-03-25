use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ds_core::error::DataServerError;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::geo::{Crs, GeoTransform};

/// Security limits
const MAX_RASTER_DIMENSION: u32 = 100_000;
const MAX_DECODED_TILE_BYTES: usize = 64 * 1024 * 1024; // 64 MB

/// Maximum number of pixels in an area query result.
const MAX_AREA_PIXELS: usize = 1_000_000;

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
    pub scale: Option<f64>,
    pub offset: Option<f64>,
}

impl TiffMetadata {
    /// Parse metadata from a GeoTIFF file.
    pub fn from_file(path: &Path) -> Result<Self, DataServerError> {
        let file = File::open(path)
            .map_err(|e| DataServerError::GeoTiff(format!("Cannot open {}: {e}", path.display())))?;
        let mut decoder = Decoder::new(BufReader::new(file))
            .map_err(|e| DataServerError::GeoTiff(format!("Invalid TIFF {}: {e}", path.display())))?;

        Self::from_decoder(&mut decoder, path.display().to_string())
    }

    fn from_decoder<R: std::io::Read + std::io::Seek>(
        decoder: &mut Decoder<R>,
        source_name: String,
    ) -> Result<Self, DataServerError> {
        let (width, height) = decoder.dimensions()
            .map_err(|e| DataServerError::GeoTiff(format!("Cannot read dimensions: {e}")))?;

        if width > MAX_RASTER_DIMENSION || height > MAX_RASTER_DIMENSION {
            return Err(DataServerError::GeoTiff(format!(
                "Raster dimensions {}x{} exceed maximum {}",
                width, height, MAX_RASTER_DIMENSION
            )));
        }

        let tile_width = read_tag_u32(decoder, Tag::TileWidth)
            .ok_or_else(|| DataServerError::GeoTiff(format!("{source_name}: Not a tiled TIFF (TileWidth missing)")))?;
        let tile_height = read_tag_u32(decoder, Tag::TileLength)
            .ok_or_else(|| DataServerError::GeoTiff(format!("{source_name}: Not a tiled TIFF (TileLength missing)")))?;

        // Security: check decoded tile size (assuming worst case 8 bytes/pixel)
        let decoded_tile_bytes = tile_width as usize * tile_height as usize * 8;
        if decoded_tile_bytes > MAX_DECODED_TILE_BYTES {
            return Err(DataServerError::GeoTiff(format!(
                "Decoded tile size {} bytes exceeds maximum {}",
                decoded_tile_bytes, MAX_DECODED_TILE_BYTES
            )));
        }

        let tiles_across = (width + tile_width - 1) / tile_width;
        let tiles_down = (height + tile_height - 1) / tile_height;

        let geo_transform = parse_geo_transform(decoder, width, height)?;
        let nodata = parse_nodata(decoder);
        let (scale, offset) = parse_scale_offset(decoder);

        Ok(TiffMetadata {
            width,
            height,
            tile_width,
            tile_height,
            tiles_across,
            tiles_down,
            geo_transform,
            nodata,
            scale,
            offset,
        })
    }

    /// Apply scale/offset to convert raw value to physical value.
    fn to_physical(&self, raw: f64) -> f64 {
        match (self.scale, self.offset) {
            (Some(s), Some(o)) => raw * s + o,
            (Some(s), None) => raw * s,
            (None, Some(o)) => raw + o,
            (None, None) => raw,
        }
    }

    /// Check if a raw value is nodata (before scale/offset).
    fn is_nodata_raw(&self, raw: f64) -> bool {
        if let Some(nd) = self.nodata {
            (raw - nd).abs() < 1e-10
        } else {
            false
        }
    }
}

/// Decode a chunk into Vec<f64>, applying scale/offset.
/// Returns None for nodata/NaN values.
fn decode_chunk_f64<R: std::io::Read + std::io::Seek>(
    decoder: &mut Decoder<R>,
    chunk_index: u32,
    metadata: &TiffMetadata,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let result = decoder.read_chunk(chunk_index)
        .map_err(|e| DataServerError::GeoTiff(format!("Failed to read tile: {e}")))?;

    let values = match result {
        DecodingResult::F32(data) => {
            data.iter().map(|&v| {
                if v.is_nan() || metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            }).collect()
        }
        DecodingResult::F64(data) => {
            data.iter().map(|&v| {
                if v.is_nan() || metadata.is_nodata_raw(v) {
                    None
                } else {
                    Some(metadata.to_physical(v))
                }
            }).collect()
        }
        DecodingResult::U8(data) => {
            data.iter().map(|&v| {
                if metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            }).collect()
        }
        DecodingResult::U16(data) => {
            data.iter().map(|&v| {
                if metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            }).collect()
        }
        DecodingResult::I16(data) => {
            data.iter().map(|&v| {
                if metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            }).collect()
        }
        _ => return Err(DataServerError::GeoTiff("Unsupported data type".into())),
    };

    Ok(values)
}

/// Read a single pixel value from a GeoTIFF at a given pixel coordinate.
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

    let values = decode_chunk_f64(&mut decoder, chunk_index, metadata)?;

    if local_idx >= values.len() {
        return Ok(None);
    }
    Ok(values[local_idx])
}

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

    let tile_col_start = col_start / metadata.tile_width;
    let tile_col_end = (col_end - 1) / metadata.tile_width + 1;
    let tile_row_start = row_start / metadata.tile_height;
    let tile_row_end = (row_end - 1) / metadata.tile_height + 1;

    for tile_row in tile_row_start..tile_row_end {
        for tile_col in tile_col_start..tile_col_end {
            let chunk_index = tile_row * metadata.tiles_across + tile_col;
            let tile_data = decode_chunk_f64(&mut decoder, chunk_index, metadata)?;

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

                    let out_col = (col - col_start) as usize;
                    let out_row = (row - row_start) as usize;
                    let out_idx = out_row * nx + out_col;
                    result[out_idx] = tile_data[tile_idx];
                }
            }
        }
    }

    Ok(result)
}

// ============================================================================
// GeoTIFF metadata parsing
// ============================================================================

fn read_tag_u32<R: std::io::Read + std::io::Seek>(decoder: &mut Decoder<R>, tag: Tag) -> Option<u32> {
    match decoder.get_tag(tag) {
        Ok(tiff::decoder::ifd::Value::Short(v)) => Some(v as u32),
        Ok(tiff::decoder::ifd::Value::Unsigned(v)) => Some(v),
        _ => None,
    }
}

fn parse_geo_transform<R: std::io::Read + std::io::Seek>(
    decoder: &mut Decoder<R>,
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

    // Parse CRS from GeoKeys
    let crs = parse_crs(decoder)?;

    Ok(GeoTransform {
        origin_x,
        origin_y,
        pixel_width: ps[0],
        pixel_height: ps[1],
        width,
        height,
        crs,
    })
}

/// Parse the CRS from GeoTIFF GeoKey Directory.
fn parse_crs<R: std::io::Read + std::io::Seek>(decoder: &mut Decoder<R>) -> Result<Crs, DataServerError> {
    let geokeys = match decoder.get_tag(Tag::Unknown(34735)) {
        Ok(v) => extract_shorts(&v).unwrap_or_default(),
        Err(_) => return Ok(Crs::Wgs84), // No GeoKeys → assume WGS84
    };

    let double_params = match decoder.get_tag(Tag::Unknown(34736)) {
        Ok(v) => extract_doubles(&v).unwrap_or_default(),
        Err(_) => vec![],
    };

    if geokeys.len() < 4 {
        return Ok(Crs::Wgs84);
    }

    // GeoKey directory: [version, revision, minor_revision, num_keys, key1_id, key1_tiff_tag, key1_count, key1_value, ...]
    let num_keys = geokeys[3] as usize;
    let mut keys = std::collections::HashMap::new();

    for i in 0..num_keys {
        let base = 4 + i * 4;
        if base + 3 >= geokeys.len() {
            break;
        }
        let key_id = geokeys[base];
        let tiff_tag = geokeys[base + 1];
        let count = geokeys[base + 2] as usize;
        let value_offset = geokeys[base + 3] as usize;

        if tiff_tag == 0 {
            // Value is stored directly in the value_offset field
            keys.insert(key_id, GeoKeyValue::Short(value_offset as u16));
        } else if tiff_tag == 34736 {
            // Value is in GeoDoubleParamsTag
            if value_offset < double_params.len() {
                if count == 1 {
                    keys.insert(key_id, GeoKeyValue::Double(double_params[value_offset]));
                } else {
                    keys.insert(key_id, GeoKeyValue::Doubles(
                        double_params[value_offset..value_offset + count].to_vec()
                    ));
                }
            }
        }
    }

    // Key 1024: GTModelTypeGeoKey (1=Projected, 2=Geographic)
    let model_type = match keys.get(&1024) {
        Some(GeoKeyValue::Short(v)) => *v,
        _ => 2, // default geographic
    };

    if model_type == 2 {
        // Geographic CRS — check if it's WGS84-like
        return Ok(Crs::Wgs84);
    }

    // Key 3075: ProjCoordTransGeoKey (projection method)
    let proj_method = match keys.get(&3075) {
        Some(GeoKeyValue::Short(v)) => *v,
        _ => 0,
    };

    // Key 3072: ProjectedCSTypeGeoKey (EPSG code for the projected CRS)
    let _epsg = match keys.get(&3072) {
        Some(GeoKeyValue::Short(v)) => *v,
        _ => 0,
    };

    match proj_method {
        // CT_TransverseMercator = 1
        1 => {
            let lat0 = get_double_key(&keys, 3081).unwrap_or(0.0).to_radians();  // NatOriginLat
            let lon0 = get_double_key(&keys, 3080).unwrap_or(0.0).to_radians();  // NatOriginLong
            // Try ProjNatOriginLong (3080), fall back to ProjCenterLong (3088)
            let lon0 = if lon0 == 0.0 {
                get_double_key(&keys, 3088).unwrap_or(0.0).to_radians()
            } else {
                lon0
            };
            let k0 = get_double_key(&keys, 3092).unwrap_or(1.0);                 // ScaleAtNatOrigin
            let false_e = get_double_key(&keys, 3082).unwrap_or(0.0);             // FalseEasting
            let false_n = get_double_key(&keys, 3083).unwrap_or(0.0);             // FalseNorthing

            Ok(Crs::TransverseMercator { lat0, lon0, k0, false_e, false_n })
        }
        // CT_LambertConfConic_2SP = 8
        8 => {
            let lat1 = get_double_key(&keys, 3078).unwrap_or(0.0).to_radians();  // StdParallel1
            let lat2 = get_double_key(&keys, 3079).unwrap_or(0.0).to_radians();  // StdParallel2
            let lat0 = get_double_key(&keys, 3081).unwrap_or(0.0).to_radians();  // FalseOriginLat
            // Try NatOriginLong (3080), fall back to FalseOriginLong (3084)
            let lon0_nat = get_double_key(&keys, 3080);
            let lon0_false = get_double_key(&keys, 3084);
            let lon0 = lon0_nat.or(lon0_false).unwrap_or(0.0).to_radians();
            let false_e = get_double_key(&keys, 3082)
                .or_else(|| get_double_key(&keys, 3086))
                .unwrap_or(0.0);
            let false_n = get_double_key(&keys, 3083)
                .or_else(|| get_double_key(&keys, 3087))
                .unwrap_or(0.0);

            Ok(Crs::LambertConformalConic { lat1, lat2, lat0, lon0, false_e, false_n })
        }
        // CT_LambertAzimEqualArea = 10
        10 => {
            let lat0 = get_double_key(&keys, 3081).unwrap_or(0.0).to_radians();  // CenterLat
            let lon0 = get_double_key(&keys, 3080)
                .or_else(|| get_double_key(&keys, 3088))
                .unwrap_or(0.0).to_radians();
            let false_e = get_double_key(&keys, 3082)
                .or_else(|| get_double_key(&keys, 3086))
                .unwrap_or(0.0);
            let false_n = get_double_key(&keys, 3083)
                .or_else(|| get_double_key(&keys, 3087))
                .unwrap_or(0.0);

            Ok(Crs::LambertAzimuthalEqualArea { lat0, lon0, false_e, false_n })
        }
        _ => {
            Err(DataServerError::GeoTiff(format!(
                "Unsupported projection method {} (GeoKey 3075). \
                 Supported: Transverse Mercator (1), Lambert Conformal Conic 2SP (8), \
                 Lambert Azimuthal Equal Area (10). \
                 Convert with: gdalwarp -t_srs EPSG:4326 -of COG input.tif output.tif",
                proj_method
            )))
        }
    }
}

#[derive(Debug)]
enum GeoKeyValue {
    Short(u16),
    Double(f64),
    #[allow(dead_code)]
    Doubles(Vec<f64>),
}

fn get_double_key(keys: &std::collections::HashMap<u16, GeoKeyValue>, key_id: u16) -> Option<f64> {
    match keys.get(&key_id) {
        Some(GeoKeyValue::Double(v)) => Some(*v),
        Some(GeoKeyValue::Short(v)) => Some(*v as f64),
        _ => None,
    }
}

fn parse_nodata<R: std::io::Read + std::io::Seek>(decoder: &mut Decoder<R>) -> Option<f64> {
    match decoder.get_tag(Tag::Unknown(42113)) {
        Ok(tiff::decoder::ifd::Value::Ascii(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_scale_offset<R: std::io::Read + std::io::Seek>(decoder: &mut Decoder<R>) -> (Option<f64>, Option<f64>) {
    // Try GDAL metadata XML tag (42112)
    if let Ok(tiff::decoder::ifd::Value::Ascii(xml)) = decoder.get_tag(Tag::Unknown(42112)) {
        let scale = extract_xml_item(&xml, "SCALE");
        let offset = extract_xml_item(&xml, "OFFSET");
        if scale.is_some() || offset.is_some() {
            return (scale, offset);
        }
    }

    // Try TIFF SMinSampleValue/SMaxSampleValue as fallback — these aren't scale/offset
    // but some producers put scale in tag 34264 etc. For now, skip.

    (None, None)
}

/// Extract a named item from GDAL metadata XML like:
/// `<GDALMetadata><Item name="SCALE">0.5</Item><Item name="OFFSET">-32</Item></GDALMetadata>`
fn extract_xml_item(xml: &str, name: &str) -> Option<f64> {
    let pattern = format!("name=\"{}\"", name);
    if let Some(pos) = xml.find(&pattern) {
        if let Some(gt) = xml[pos..].find('>') {
            let after = &xml[pos + gt + 1..];
            if let Some(lt) = after.find('<') {
                return after[..lt].trim().parse().ok();
            }
        }
    }
    None
}

fn extract_shorts(value: &tiff::decoder::ifd::Value) -> Option<Vec<u16>> {
    match value {
        tiff::decoder::ifd::Value::List(items) => {
            let mut result = Vec::new();
            for item in items {
                match item {
                    tiff::decoder::ifd::Value::Short(v) => result.push(*v),
                    tiff::decoder::ifd::Value::Unsigned(v) => result.push(*v as u16),
                    _ => return None,
                }
            }
            Some(result)
        }
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
