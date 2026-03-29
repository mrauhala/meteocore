use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use bytes::Bytes;
use ds_core::error::DataServerError;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::geo::{Crs, GeoTransform};

/// Bridge async to sync for standalone functions.
/// Uses `block_in_place` when inside a tokio runtime, or creates a temporary runtime otherwise.
fn block_on_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(future)
        }
    }
}

/// Security limits
const MAX_RASTER_DIMENSION: u32 = 100_000;
const MAX_DECODED_TILE_BYTES: usize = 64 * 1024 * 1024; // 64 MB

/// Maximum number of pixels in an area query result.
const MAX_AREA_PIXELS: usize = 1_000_000;

/// Size of the initial header read for COG range reads (512 KB).
/// Must be large enough to contain ALL IFDs (full-res + overviews) and their
/// tile offset/byte count arrays. 64 KB was insufficient for files with
/// multiple overview levels, causing overview tile offsets to be truncated.
pub(crate) const HEADER_READ_SIZE: usize = 512 * 1024;

/// TIFF compression methods we support for manual decompression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TiffCompression {
    None,
    Deflate,
    Lzw,
}

/// Sample data type from BitsPerSample + SampleFormat.
#[derive(Debug, Clone, Copy)]
pub enum SampleType {
    U8,
    U16,
    I16,
    F32,
    F64,
}

impl SampleType {
    fn bytes_per_sample(self) -> usize {
        match self {
            SampleType::U8 => 1,
            SampleType::U16 | SampleType::I16 => 2,
            SampleType::F32 => 4,
            SampleType::F64 => 8,
        }
    }
}

/// Tile layout info extracted from the IFD for range-read-based tile access.
#[derive(Debug, Clone)]
pub struct RemoteTileInfo {
    pub tile_offsets: Vec<u64>,
    pub tile_byte_counts: Vec<u64>,
    pub compression: TiffCompression,
    pub sample_type: SampleType,
    pub predictor: u16,
    pub tile_width: u32,
    pub tile_height: u32,
}

/// Data source for reading GeoTIFF data.
#[derive(Debug, Clone)]
pub enum DataSource {
    /// Local filesystem path.
    LocalFile(PathBuf),
    /// In-memory bytes (downloaded from S3/HTTP). Used as fallback.
    InMemory(Bytes),
    /// Remote file accessed via byte-range reads. Only IFD metadata is cached;
    /// tile data is fetched on demand.
    Remote {
        store: ds_storage::DataStore,
        path: ds_storage::object_store::path::Path,
        tile_info: RemoteTileInfo,
    },
    /// Remote file accessed via raw HTTP range reads (reqwest).
    /// Used for STAC assets where object_store URL-encodes path components.
    HttpDirect {
        url: String,
        http: Arc<reqwest::Client>,
        tile_info: RemoteTileInfo,
    },
}

impl DataSource {
    pub fn from_path(path: &Path) -> Self {
        DataSource::LocalFile(path.to_path_buf())
    }

    pub fn from_bytes(data: impl Into<Bytes>) -> Self {
        DataSource::InMemory(data.into())
    }

    /// Open a decoder for this data source (LocalFile and InMemory only).
    fn open_decoder(&self) -> Result<DecoderWrapper, DataServerError> {
        match self {
            DataSource::LocalFile(path) => {
                let file = File::open(path).map_err(|e| {
                    DataServerError::GeoTiff(format!("Cannot open {}: {e}", path.display()))
                })?;
                Ok(DecoderWrapper::File(
                    Decoder::new(BufReader::new(file)).map_err(|e| {
                        DataServerError::GeoTiff(format!("Invalid TIFF {}: {e}", path.display()))
                    })?,
                ))
            }
            DataSource::InMemory(bytes) => {
                let cursor = Cursor::new(bytes.to_vec());
                Ok(DecoderWrapper::Memory(Decoder::new(cursor).map_err(
                    |e| DataServerError::GeoTiff(format!("Invalid TIFF (in-memory): {e}")),
                )?))
            }
            DataSource::Remote { .. } | DataSource::HttpDirect { .. } => {
                Err(DataServerError::GeoTiff(
                    "Cannot open decoder for remote source; use range reads".into(),
                ))
            }
        }
    }

    fn display_name(&self) -> String {
        match self {
            DataSource::LocalFile(p) => p.display().to_string(),
            DataSource::InMemory(_) => "<in-memory>".to_string(),
            DataSource::Remote { path, .. } => format!("<remote:{}>", path),
            DataSource::HttpDirect { url, .. } => format!("<http:{}>", url),
        }
    }
}

/// Wraps Decoder over different reader types to avoid generics leaking everywhere.
enum DecoderWrapper {
    File(Decoder<BufReader<File>>),
    Memory(Decoder<Cursor<Vec<u8>>>),
}

impl DecoderWrapper {
    fn dimensions(&mut self) -> Result<(u32, u32), DataServerError> {
        match self {
            Self::File(d) => d
                .dimensions()
                .map_err(|e| DataServerError::GeoTiff(format!("{e}"))),
            Self::Memory(d) => d
                .dimensions()
                .map_err(|e| DataServerError::GeoTiff(format!("{e}"))),
        }
    }

    #[allow(dead_code)]
    fn colortype(&mut self) -> Result<tiff::ColorType, DataServerError> {
        match self {
            Self::File(d) => d
                .colortype()
                .map_err(|e| DataServerError::GeoTiff(format!("{e}"))),
            Self::Memory(d) => d
                .colortype()
                .map_err(|e| DataServerError::GeoTiff(format!("{e}"))),
        }
    }

    fn get_tag(&mut self, tag: Tag) -> Result<tiff::decoder::ifd::Value, tiff::TiffError> {
        match self {
            Self::File(d) => d.get_tag(tag),
            Self::Memory(d) => d.get_tag(tag),
        }
    }

    fn read_chunk(&mut self, idx: u32) -> Result<DecodingResult, tiff::TiffError> {
        match self {
            Self::File(d) => d.read_chunk(idx),
            Self::Memory(d) => d.read_chunk(idx),
        }
    }

    fn seek_to_image(&mut self, index: usize) -> Result<(), tiff::TiffError> {
        match self {
            Self::File(d) => d.seek_to_image(index),
            Self::Memory(d) => d.seek_to_image(index),
        }
    }

    fn more_images(&self) -> bool {
        match self {
            Self::File(d) => d.more_images(),
            Self::Memory(d) => d.more_images(),
        }
    }
}

/// Metadata for a single overview level in a COG.
#[derive(Debug, Clone)]
pub struct OverviewLevel {
    /// IFD index in the TIFF file (0 = full resolution, 1+ = overviews).
    pub ifd_index: usize,
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tiles_across: u32,
    pub tiles_down: u32,
    /// Tile info for remote byte-range reads (None for local sources).
    pub tile_info: Option<RemoteTileInfo>,
}

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
    pub samples_per_pixel: u32,
    pub geo_transform: GeoTransform,
    pub nodata: Option<f64>,
    pub scale: Option<f64>,
    pub offset: Option<f64>,
    /// COG overview levels, sorted by decreasing resolution (largest first).
    /// Empty if the file has no overviews.
    pub overviews: Vec<OverviewLevel>,
}

impl TiffMetadata {
    /// Parse metadata from a GeoTIFF file path.
    pub fn from_file(path: &Path) -> Result<Self, DataServerError> {
        Self::from_source(&DataSource::LocalFile(path.to_path_buf()))
    }

    /// Parse metadata from any data source.
    pub fn from_source(source: &DataSource) -> Result<Self, DataServerError> {
        let mut decoder = source.open_decoder()?;
        Self::from_decoder_wrapper(&mut decoder, source.display_name())
    }

    /// Read BitsPerSample from decoder, defaulting to 8 if missing.
    fn bits_per_sample_from_decoder(decoder: &mut DecoderWrapper) -> u32 {
        read_tag_u32(decoder, Tag::BitsPerSample).unwrap_or(8)
    }

    fn from_decoder_wrapper(
        decoder: &mut DecoderWrapper,
        source_name: String,
    ) -> Result<Self, DataServerError> {
        let (width, height) = decoder
            .dimensions()
            .map_err(|e| DataServerError::GeoTiff(format!("Cannot read dimensions: {e}")))?;

        if width > MAX_RASTER_DIMENSION || height > MAX_RASTER_DIMENSION {
            return Err(DataServerError::GeoTiff(format!(
                "Raster dimensions {}x{} exceed maximum {}",
                width, height, MAX_RASTER_DIMENSION
            )));
        }

        let tile_width = read_tag_u32(decoder, Tag::TileWidth)
            .ok_or_else(|| DataServerError::GeoTiff(format!(
                "{source_name}: Not a tiled TIFF (TileWidth missing). \
                 Convert to tiled COG with: gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif"
            )))?;
        let tile_height = read_tag_u32(decoder, Tag::TileLength)
            .ok_or_else(|| DataServerError::GeoTiff(format!(
                "{source_name}: Not a tiled TIFF (TileLength missing). \
                 Convert to tiled COG with: gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif"
            )))?;

        let tiles_across = width.div_ceil(tile_width);
        let tiles_down = height.div_ceil(tile_height);
        let samples_per_pixel = read_tag_u32(decoder, Tag::SamplesPerPixel).unwrap_or(1);

        // Security: check decoded tile size using actual sample size and band count.
        // samples_per_pixel could be large in multi-band files, so we use
        // the real value rather than assuming worst-case 8 bytes/sample.
        let bps = Self::bits_per_sample_from_decoder(decoder);
        let bytes_per_sample = (bps as usize).div_ceil(8);
        let decoded_tile_bytes = tile_width as usize
            * tile_height as usize
            * bytes_per_sample
            * samples_per_pixel as usize;
        if decoded_tile_bytes > MAX_DECODED_TILE_BYTES {
            return Err(DataServerError::GeoTiff(format!(
                "Decoded tile size {} bytes ({}x{} px, {} bands, {} bytes/sample) exceeds maximum {}",
                decoded_tile_bytes, tile_width, tile_height, samples_per_pixel, bytes_per_sample,
                MAX_DECODED_TILE_BYTES
            )));
        }

        let geo_transform = parse_geo_transform(decoder, width, height)?;
        let nodata = parse_nodata(decoder);
        let (scale, offset) = parse_scale_offset(decoder);

        // Discover COG overview levels by traversing the IFD chain
        let mut overviews = Vec::new();
        let mut ifd_index = 1;
        while decoder.more_images() {
            if decoder.seek_to_image(ifd_index).is_err() {
                break;
            }
            if let Ok((ov_width, ov_height)) = decoder.dimensions() {
                // Overview must be smaller than full resolution
                if ov_width < width && ov_height < height {
                    if let (Some(ov_tw), Some(ov_th)) = (
                        read_tag_u32(decoder, Tag::TileWidth),
                        read_tag_u32(decoder, Tag::TileLength),
                    ) {
                        let ov_tile_info = extract_tile_info_at_level(decoder, ov_tw, ov_th);
                        overviews.push(OverviewLevel {
                            ifd_index,
                            width: ov_width,
                            height: ov_height,
                            tile_width: ov_tw,
                            tile_height: ov_th,
                            tiles_across: ov_width.div_ceil(ov_tw),
                            tiles_down: ov_height.div_ceil(ov_th),
                            tile_info: ov_tile_info,
                        });
                    }
                }
            }
            ifd_index += 1;
        }
        // Sort overviews by decreasing resolution (largest first)
        overviews.sort_by(|a, b| {
            (b.width as u64 * b.height as u64).cmp(&(a.width as u64 * a.height as u64))
        });

        if !overviews.is_empty() {
            tracing::debug!(
                "{source_name}: found {} overview levels: {}",
                overviews.len(),
                overviews
                    .iter()
                    .map(|o| format!("{}x{}", o.width, o.height))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // Seek back to IFD 0 for subsequent reads
        let _ = decoder.seek_to_image(0);

        Ok(TiffMetadata {
            width,
            height,
            tile_width,
            tile_height,
            tiles_across,
            tiles_down,
            samples_per_pixel,
            geo_transform,
            nodata,
            scale,
            offset,
            overviews,
        })
    }

    /// Parse metadata from a partial header read (COG range read).
    ///
    /// Reads the first `HEADER_READ_SIZE` bytes from the remote file and
    /// parses the IFD. Also extracts tile offsets, byte counts, compression,
    /// and sample type for on-demand tile reading.
    ///
    /// Returns `None` if parsing fails (caller should fall back to full download).
    pub fn from_header_read(
        store: &ds_storage::DataStore,
        path: &ds_storage::object_store::path::Path,
        file_size: u64,
    ) -> Option<(Self, RemoteTileInfo)> {
        let read_size = HEADER_READ_SIZE.min(file_size as usize);
        let header_bytes = store.get_range(path, 0..read_size).ok()?;
        let cursor = Cursor::new(header_bytes.to_vec());
        let mut decoder = DecoderWrapper::Memory(Decoder::new(cursor).ok()?);

        let metadata =
            Self::from_decoder_wrapper(&mut decoder, format!("<remote:{}>", path)).ok()?;

        let tile_info = extract_tile_info(&mut decoder, &metadata)?;

        Some((metadata, tile_info))
    }

    /// Parse metadata from a partial HTTP header read (COG range read via reqwest).
    ///
    /// Similar to `from_header_read` but uses a raw reqwest HTTP client instead
    /// of object_store. Used for STAC assets where object_store URL-encodes path
    /// components, breaking servers like Ceph RGW.
    ///
    /// Returns `None` if parsing fails (caller should fall back to full download).
    pub fn from_http_header_read(
        http: &reqwest::Client,
        url: &str,
        file_size: u64,
    ) -> Option<(Self, RemoteTileInfo)> {
        let read_size = HEADER_READ_SIZE.min(file_size as usize);
        if read_size == 0 {
            return None;
        }
        let range_header = format!("bytes=0-{}", read_size - 1);
        let url_owned = url.to_string();
        let header_bytes = block_on_async(async {
            let resp = http
                .get(&url_owned)
                .header(reqwest::header::RANGE, &range_header)
                .send()
                .await
                .ok()?;
            if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                return None;
            }
            resp.bytes().await.ok()
        })?;
        let cursor = Cursor::new(header_bytes.to_vec());
        let mut decoder = DecoderWrapper::Memory(Decoder::new(cursor).ok()?);
        let metadata = Self::from_decoder_wrapper(&mut decoder, format!("<http:{}>", url)).ok()?;
        let tile_info = extract_tile_info(&mut decoder, &metadata)?;
        Some((metadata, tile_info))
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
    ///
    /// Uses bitwise comparison for integer-representable nodata values (exact match)
    /// and ULP-aware comparison for float nodata to handle f32→f64 promotion.
    fn is_nodata_raw(&self, raw: f64) -> bool {
        if let Some(nd) = self.nodata {
            // If nodata is an exact integer (common: 255, 0, 65535, -9999, etc.),
            // use exact comparison — integer-to-f64 conversion is lossless for
            // all integers up to 2^53.
            if nd == nd.trunc() && nd.is_finite() {
                return raw == nd;
            }
            // For non-integer nodata (e.g., -3.4028235e+38 from GDAL float32 max),
            // compare via f32 roundtrip to handle f32→f64 promotion mismatch.
            // The GDAL_NODATA tag is stored as ASCII text and parsed to f64,
            // but the actual pixel values may have been f32 promoted to f64.
            (raw as f32) == (nd as f32)
        } else {
            false
        }
    }

    /// Select the best overview level for the given output dimensions.
    ///
    /// Returns the overview whose resolution is closest to (but not less than)
    /// the output resolution. Returns `None` if full resolution should be used.
    /// Select the best overview level for the given bbox and output dimensions.
    ///
    /// Compares how many source pixels the bbox covers at each level against
    /// the output dimensions. Picks the smallest level where the source pixel
    /// coverage still exceeds the output (no upscaling).
    pub fn select_overview(
        &self,
        bbox_west: f64,
        bbox_south: f64,
        bbox_east: f64,
        bbox_north: f64,
        output_width: u32,
        output_height: u32,
    ) -> Option<&OverviewLevel> {
        if self.overviews.is_empty() {
            return None;
        }

        // Check how many full-res pixels the bbox covers
        let full_range = self
            .geo_transform
            .bbox_to_pixels(bbox_west, bbox_south, bbox_east, bbox_north);
        let (full_cols, full_rows) = match full_range {
            Some((c0, r0, c1, r1)) => ((c1 - c0), (r1 - r0)),
            None => return None, // bbox doesn't intersect
        };

        // If full resolution already fits the output, no need for overviews
        if full_cols <= output_width && full_rows <= output_height {
            return None;
        }

        // Find the smallest overview where the bbox still covers enough
        // source pixels to fill the output without upscaling.
        // Overviews are sorted by decreasing resolution (largest first).
        let mut best: Option<&OverviewLevel> = None;
        for ov in &self.overviews {
            let ov_gt = self.overview_geo_transform(ov);
            if let Some((c0, r0, c1, r1)) =
                ov_gt.bbox_to_pixels(bbox_west, bbox_south, bbox_east, bbox_north)
            {
                let ov_cols = c1 - c0;
                let ov_rows = r1 - r0;
                if ov_cols >= output_width && ov_rows >= output_height {
                    // This overview has enough pixels — it's a candidate
                    best = Some(ov);
                } else {
                    // Too small — stop, use previous candidate
                    break;
                }
            }
        }

        best
    }

    /// Build a GeoTransform for an overview level.
    ///
    /// The overview covers the same geographic extent as the full resolution,
    /// but with larger pixels.
    pub fn overview_geo_transform(&self, overview: &OverviewLevel) -> GeoTransform {
        let x_scale = self.width as f64 / overview.width as f64;
        let y_scale = self.height as f64 / overview.height as f64;
        GeoTransform {
            origin_x: self.geo_transform.origin_x,
            origin_y: self.geo_transform.origin_y,
            pixel_width: self.geo_transform.pixel_width * x_scale,
            pixel_height: self.geo_transform.pixel_height * y_scale,
            width: overview.width,
            height: overview.height,
            crs: self.geo_transform.crs.clone(),
        }
    }

    /// Apply config overrides for nodata, scale, and offset.
    /// Config values take precedence over file-embedded values.
    pub fn apply_overrides(
        &mut self,
        nodata: Option<f64>,
        scale: Option<f64>,
        offset: Option<f64>,
    ) {
        if let Some(nd) = nodata {
            self.nodata = Some(nd);
        }
        if let Some(s) = scale {
            self.scale = Some(s);
        }
        if let Some(o) = offset {
            self.offset = Some(o);
        }
    }
}

/// Decode a chunk into Vec<f64>, applying scale/offset.
/// Returns None for nodata/NaN values.
/// For multi-band files, `band_index` selects which band (0-based).
fn decode_chunk_f64(
    decoder: &mut DecoderWrapper,
    chunk_index: u32,
    metadata: &TiffMetadata,
    band_index: usize,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let result = decoder
        .read_chunk(chunk_index)
        .map_err(|e| DataServerError::GeoTiff(format!("Failed to read tile: {e}")))?;

    let spp = metadata.samples_per_pixel as usize;

    let values = match result {
        DecodingResult::F32(data) => data
            .iter()
            .skip(band_index)
            .step_by(spp)
            .map(|&v| {
                if v.is_nan() || metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            })
            .collect(),
        DecodingResult::F64(data) => data
            .iter()
            .skip(band_index)
            .step_by(spp)
            .map(|&v| {
                if v.is_nan() || metadata.is_nodata_raw(v) {
                    None
                } else {
                    Some(metadata.to_physical(v))
                }
            })
            .collect(),
        DecodingResult::U8(data) => data
            .iter()
            .skip(band_index)
            .step_by(spp)
            .map(|&v| {
                if metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            })
            .collect(),
        DecodingResult::U16(data) => data
            .iter()
            .skip(band_index)
            .step_by(spp)
            .map(|&v| {
                if metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            })
            .collect(),
        DecodingResult::I16(data) => data
            .iter()
            .skip(band_index)
            .step_by(spp)
            .map(|&v| {
                if metadata.is_nodata_raw(v as f64) {
                    None
                } else {
                    Some(metadata.to_physical(v as f64))
                }
            })
            .collect(),
        _ => return Err(DataServerError::GeoTiff("Unsupported data type".into())),
    };

    Ok(values)
}

/// Extract tile layout info from a decoder for remote range-read access.
fn extract_tile_info(
    decoder: &mut DecoderWrapper,
    metadata: &TiffMetadata,
) -> Option<RemoteTileInfo> {
    let tile_offsets = extract_u64_list(&decoder.get_tag(Tag::TileOffsets).ok()?)?;
    let tile_byte_counts = extract_u64_list(&decoder.get_tag(Tag::TileByteCounts).ok()?)?;

    if tile_offsets.is_empty() || tile_offsets.len() != tile_byte_counts.len() {
        return None;
    }

    let compression_code = read_tag_u32(decoder, Tag::Compression).unwrap_or(1);
    let compression = match compression_code {
        1 => TiffCompression::None,
        5 => TiffCompression::Lzw,
        8 | 32946 => TiffCompression::Deflate,
        _ => return None, // unsupported compression — caller falls back to full download
    };

    let bits_per_sample = read_tag_u32(decoder, Tag::BitsPerSample).unwrap_or(8) as u16;
    let sample_format = read_tag_u32(decoder, Tag::SampleFormat).unwrap_or(1);

    let sample_type = match (bits_per_sample, sample_format) {
        (8, 1) => SampleType::U8,
        (16, 1) => SampleType::U16,
        (16, 2) => SampleType::I16,
        (32, 3) => SampleType::F32,
        (64, 3) => SampleType::F64,
        _ => return None, // unsupported sample type
    };

    let predictor = read_tag_u32(decoder, Tag::Predictor).unwrap_or(1) as u16;
    if predictor > 2 {
        return None; // floating point prediction not supported
    }

    Some(RemoteTileInfo {
        tile_offsets,
        tile_byte_counts,
        compression,
        sample_type,
        predictor,
        tile_width: metadata.tile_width,
        tile_height: metadata.tile_height,
    })
}

/// Extract tile info from the currently-selected IFD (for overview levels).
fn extract_tile_info_at_level(
    decoder: &mut DecoderWrapper,
    tile_width: u32,
    tile_height: u32,
) -> Option<RemoteTileInfo> {
    let tile_offsets = extract_u64_list(&decoder.get_tag(Tag::TileOffsets).ok()?)?;
    let tile_byte_counts = extract_u64_list(&decoder.get_tag(Tag::TileByteCounts).ok()?)?;

    if tile_offsets.is_empty() || tile_offsets.len() != tile_byte_counts.len() {
        return None;
    }

    let compression_code = read_tag_u32(decoder, Tag::Compression).unwrap_or(1);
    let compression = match compression_code {
        1 => TiffCompression::None,
        5 => TiffCompression::Lzw,
        8 | 32946 => TiffCompression::Deflate,
        _ => return None,
    };

    let bits_per_sample = read_tag_u32(decoder, Tag::BitsPerSample).unwrap_or(8) as u16;
    let sample_format = read_tag_u32(decoder, Tag::SampleFormat).unwrap_or(1);

    let sample_type = match (bits_per_sample, sample_format) {
        (8, 1) => SampleType::U8,
        (16, 1) => SampleType::U16,
        (16, 2) => SampleType::I16,
        (32, 3) => SampleType::F32,
        (64, 3) => SampleType::F64,
        _ => return None,
    };

    let predictor = read_tag_u32(decoder, Tag::Predictor).unwrap_or(1) as u16;
    if predictor > 2 {
        return None;
    }

    Some(RemoteTileInfo {
        tile_offsets,
        tile_byte_counts,
        compression,
        sample_type,
        predictor,
        tile_width,
        tile_height,
    })
}

/// Extract a Vec<u64> from a TIFF IFD Value (for TileOffsets/TileByteCounts).
fn extract_u64_list(value: &tiff::decoder::ifd::Value) -> Option<Vec<u64>> {
    match value {
        tiff::decoder::ifd::Value::List(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    tiff::decoder::ifd::Value::Unsigned(v) => result.push(*v as u64),
                    tiff::decoder::ifd::Value::UnsignedBig(v) => result.push(*v),
                    tiff::decoder::ifd::Value::Short(v) => result.push(*v as u64),
                    _ => return None,
                }
            }
            Some(result)
        }
        tiff::decoder::ifd::Value::Unsigned(v) => Some(vec![*v as u64]),
        tiff::decoder::ifd::Value::UnsignedBig(v) => Some(vec![*v]),
        _ => None,
    }
}

/// Decompress raw tile bytes based on compression type.
/// Enforces MAX_DECODED_TILE_BYTES to prevent decompression bombs.
fn decompress_tile(
    compressed: &[u8],
    compression: TiffCompression,
) -> Result<Vec<u8>, DataServerError> {
    match compression {
        TiffCompression::None => Ok(compressed.to_vec()),
        TiffCompression::Deflate => {
            use std::io::Read;
            let decoder = flate2::read::ZlibDecoder::new(compressed);
            let mut decompressed = Vec::with_capacity(compressed.len().min(1024 * 1024));
            decoder
                .take(MAX_DECODED_TILE_BYTES as u64)
                .read_to_end(&mut decompressed)
                .map_err(|e| {
                    DataServerError::GeoTiff(format!("Deflate decompression failed: {e}"))
                })?;
            if decompressed.len() >= MAX_DECODED_TILE_BYTES {
                return Err(DataServerError::GeoTiff(format!(
                    "Decompressed tile exceeds maximum size ({} bytes)",
                    MAX_DECODED_TILE_BYTES
                )));
            }
            Ok(decompressed)
        }
        TiffCompression::Lzw => {
            // TIFF LZW uses an early code size increase compared to standard LZW.
            // with_tiff_size_switch enables this TIFF-specific behavior.
            let mut decoder =
                weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
            let decompressed = decoder
                .decode(compressed)
                .map_err(|e| DataServerError::GeoTiff(format!("LZW decompression failed: {e}")))?;
            if decompressed.len() > MAX_DECODED_TILE_BYTES {
                return Err(DataServerError::GeoTiff(format!(
                    "Decompressed tile exceeds maximum size ({} bytes)",
                    MAX_DECODED_TILE_BYTES
                )));
            }
            Ok(decompressed)
        }
    }
}

/// Apply horizontal differencing predictor (TIFF predictor=2).
/// Undoes the differencing: each sample = prev_sample + delta.
fn undo_horizontal_predictor(data: &mut [u8], tile_width: u32, bytes_per_sample: usize) {
    let row_bytes = tile_width as usize * bytes_per_sample;
    for row_start in (0..data.len()).step_by(row_bytes) {
        let row_end = (row_start + row_bytes).min(data.len());
        if row_end - row_start < 2 * bytes_per_sample {
            continue;
        }
        // For each byte position within the sample size, accumulate independently.
        // This matches TIFF spec: differencing is per byte position.
        for b in 0..bytes_per_sample {
            for i in (row_start + bytes_per_sample + b..row_end).step_by(bytes_per_sample) {
                data[i] = data[i].wrapping_add(data[i - bytes_per_sample]);
            }
        }
    }
}

/// Decode a raw (decompressed) tile into Vec<Option<f64>> values.
/// For multi-band files, `band_index` selects which band (0-based).
fn decode_raw_tile_f64(
    raw: &[u8],
    tile_info: &RemoteTileInfo,
    metadata: &TiffMetadata,
    band_index: usize,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let bps = tile_info.sample_type.bytes_per_sample();
    let spp = metadata.samples_per_pixel as usize;
    let sample_stride = bps * spp; // bytes per pixel (all bands)
    let band_byte_offset = band_index * bps;

    // Validate buffer is large enough for at least one pixel
    if sample_stride == 0 {
        return Err(DataServerError::GeoTiff(
            "Invalid tile: zero sample stride".into(),
        ));
    }
    let pixel_count = raw.len() / sample_stride;

    // Validate that every pixel access is within bounds (last pixel's last byte)
    if pixel_count > 0 {
        let last_offset = (pixel_count - 1) * sample_stride + band_byte_offset + bps;
        if last_offset > raw.len() {
            return Err(DataServerError::GeoTiff(format!(
                "Truncated tile data: need {} bytes for {} pixels but got {}",
                last_offset,
                pixel_count,
                raw.len()
            )));
        }
    }

    let mut values = Vec::with_capacity(pixel_count);

    match tile_info.sample_type {
        SampleType::U8 => {
            for i in 0..pixel_count {
                let v = raw[i * spp + band_index];
                let fv = v as f64;
                if metadata.is_nodata_raw(fv) {
                    values.push(None);
                } else {
                    values.push(Some(metadata.to_physical(fv)));
                }
            }
        }
        SampleType::U16 => {
            for i in 0..pixel_count {
                let off = i * sample_stride + band_byte_offset;
                let v = u16::from_le_bytes([raw[off], raw[off + 1]]);
                let fv = v as f64;
                if metadata.is_nodata_raw(fv) {
                    values.push(None);
                } else {
                    values.push(Some(metadata.to_physical(fv)));
                }
            }
        }
        SampleType::I16 => {
            for i in 0..pixel_count {
                let off = i * sample_stride + band_byte_offset;
                let v = i16::from_le_bytes([raw[off], raw[off + 1]]);
                let fv = v as f64;
                if metadata.is_nodata_raw(fv) {
                    values.push(None);
                } else {
                    values.push(Some(metadata.to_physical(fv)));
                }
            }
        }
        SampleType::F32 => {
            for i in 0..pixel_count {
                let off = i * sample_stride + band_byte_offset;
                let v = f32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
                let fv = v as f64;
                if v.is_nan() || metadata.is_nodata_raw(fv) {
                    values.push(None);
                } else {
                    values.push(Some(metadata.to_physical(fv)));
                }
            }
        }
        SampleType::F64 => {
            for i in 0..pixel_count {
                let off = i * sample_stride + band_byte_offset;
                let v = f64::from_le_bytes([
                    raw[off],
                    raw[off + 1],
                    raw[off + 2],
                    raw[off + 3],
                    raw[off + 4],
                    raw[off + 5],
                    raw[off + 6],
                    raw[off + 7],
                ]);
                if v.is_nan() || metadata.is_nodata_raw(v) {
                    values.push(None);
                } else {
                    values.push(Some(metadata.to_physical(v)));
                }
            }
        }
    }

    Ok(values)
}

/// Read and decode a tile from a remote source via byte-range read.
/// If a tile cache is provided, checks it first and stores fetched tiles in it.
/// `band_index` selects which band (0-based) for multi-band files.
#[allow(clippy::too_many_arguments)]
fn read_remote_chunk_f64(
    store: &ds_storage::DataStore,
    obj_path: &ds_storage::object_store::path::Path,
    tile_info: &RemoteTileInfo,
    metadata: &TiffMetadata,
    chunk_index: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
    ifd_index: u16,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let idx = chunk_index as usize;
    if idx >= tile_info.tile_offsets.len() {
        return Err(DataServerError::GeoTiff(format!(
            "Tile index {} out of range ({})",
            idx,
            tile_info.tile_offsets.len()
        )));
    }

    let offset = tile_info.tile_offsets[idx] as usize;
    let byte_count = tile_info.tile_byte_counts[idx] as usize;

    if byte_count == 0 {
        // Empty tile — return all nodata.
        // This is normal for COG tiles outside the data extent, but could also
        // indicate truncated tile offset arrays from an undersized header read.
        tracing::trace!(
            "Tile {} has byte_count=0 (offset={}), returning nodata",
            idx,
            offset
        );
        let pixel_count = (tile_info.tile_width * tile_info.tile_height) as usize;
        return Ok(vec![None; pixel_count]);
    }

    // Sanity check: offset 0 for a non-first tile is suspicious (likely truncated header)
    if offset == 0 && idx > 0 {
        tracing::warn!(
            "Tile {} has offset=0 (suspicious for non-first tile, possible truncated header)",
            idx
        );
    }

    // Validate range doesn't overflow
    let end = offset.checked_add(byte_count).ok_or_else(|| {
        DataServerError::GeoTiff(format!(
            "Tile {} byte range overflow: offset={} + count={}",
            idx, offset, byte_count
        ))
    })?;

    // Check cache for compressed bytes (keyed by file + chunk + IFD level)
    let compressed = if let Some(c) = cache {
        if let Some(cached) = c.get(file_path, chunk_index, ifd_index) {
            cached
        } else {
            let fetched = store
                .get_range(obj_path, offset..end)
                .map_err(|e| DataServerError::GeoTiff(format!("Failed to read tile range: {e}")))?;
            // Validate response length matches request
            if fetched.len() != byte_count {
                return Err(DataServerError::GeoTiff(format!(
                    "Tile {} truncated: requested {} bytes, got {}",
                    idx,
                    byte_count,
                    fetched.len()
                )));
            }
            c.insert(file_path, chunk_index, ifd_index, fetched.clone());
            fetched
        }
    } else {
        let fetched = store
            .get_range(obj_path, offset..end)
            .map_err(|e| DataServerError::GeoTiff(format!("Failed to read tile range: {e}")))?;
        if fetched.len() != byte_count {
            return Err(DataServerError::GeoTiff(format!(
                "Tile {} truncated: requested {} bytes, got {}",
                idx,
                byte_count,
                fetched.len()
            )));
        }
        fetched
    };

    let mut raw = decompress_tile(&compressed, tile_info.compression)?;

    if tile_info.predictor == 2 {
        undo_horizontal_predictor(
            &mut raw,
            tile_info.tile_width,
            tile_info.sample_type.bytes_per_sample(),
        );
    }

    decode_raw_tile_f64(&raw, tile_info, metadata, band_index)
}

/// Fetch a byte range from an HTTP URL using reqwest.
fn read_http_range(
    http: &reqwest::Client,
    url: &str,
    range: std::ops::Range<usize>,
) -> Result<Bytes, DataServerError> {
    let range_header = format!("bytes={}-{}", range.start, range.end.saturating_sub(1));
    let url_owned = url.to_string();
    block_on_async(async {
        let resp = http
            .get(&url_owned)
            .header(reqwest::header::RANGE, &range_header)
            .send()
            .await
            .map_err(|e| DataServerError::GeoTiff(format!("HTTP range read failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(DataServerError::GeoTiff(format!(
                "HTTP range read returned {}",
                resp.status()
            )));
        }
        resp.bytes()
            .await
            .map_err(|e| DataServerError::GeoTiff(format!("Failed to read body: {e}")))
    })
}

/// Read and decode a tile from an HTTP source via byte-range read (reqwest).
/// Mirrors `read_remote_chunk_f64` but uses `read_http_range` instead of object_store.
#[allow(clippy::too_many_arguments)]
fn read_http_chunk_f64(
    http: &reqwest::Client,
    url: &str,
    tile_info: &RemoteTileInfo,
    metadata: &TiffMetadata,
    chunk_index: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
    ifd_index: u16,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let idx = chunk_index as usize;
    if idx >= tile_info.tile_offsets.len() {
        return Err(DataServerError::GeoTiff(format!(
            "Tile index {} out of range ({})",
            idx,
            tile_info.tile_offsets.len()
        )));
    }

    let offset = tile_info.tile_offsets[idx] as usize;
    let byte_count = tile_info.tile_byte_counts[idx] as usize;

    if byte_count == 0 {
        tracing::trace!(
            "Tile {} has byte_count=0 (offset={}), returning nodata",
            idx,
            offset
        );
        let pixel_count = (tile_info.tile_width * tile_info.tile_height) as usize;
        return Ok(vec![None; pixel_count]);
    }

    if offset == 0 && idx > 0 {
        tracing::warn!(
            "Tile {} has offset=0 (suspicious for non-first tile, possible truncated header)",
            idx
        );
    }

    let end = offset.checked_add(byte_count).ok_or_else(|| {
        DataServerError::GeoTiff(format!(
            "Tile {} byte range overflow: offset={} + count={}",
            idx, offset, byte_count
        ))
    })?;

    let compressed = if let Some(c) = cache {
        if let Some(cached) = c.get(file_path, chunk_index, ifd_index) {
            cached
        } else {
            let fetched = read_http_range(http, url, offset..end)?;
            if fetched.len() != byte_count {
                return Err(DataServerError::GeoTiff(format!(
                    "Tile {} truncated: requested {} bytes, got {}",
                    idx,
                    byte_count,
                    fetched.len()
                )));
            }
            c.insert(file_path, chunk_index, ifd_index, fetched.clone());
            fetched
        }
    } else {
        let fetched = read_http_range(http, url, offset..end)?;
        if fetched.len() != byte_count {
            return Err(DataServerError::GeoTiff(format!(
                "Tile {} truncated: requested {} bytes, got {}",
                idx,
                byte_count,
                fetched.len()
            )));
        }
        fetched
    };

    let mut raw = decompress_tile(&compressed, tile_info.compression)?;

    if tile_info.predictor == 2 {
        undo_horizontal_predictor(
            &mut raw,
            tile_info.tile_width,
            tile_info.sample_type.bytes_per_sample(),
        );
    }

    decode_raw_tile_f64(&raw, tile_info, metadata, band_index)
}

/// Read a single pixel value from a GeoTIFF at a given pixel coordinate.
/// For remote sources, `cache` enables compressed tile caching and `file_path`
/// identifies the file in the cache.
/// `band_index` selects which band (0-based) for multi-band files.
pub fn read_pixel(
    source: &DataSource,
    metadata: &TiffMetadata,
    col: u32,
    row: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
) -> Result<Option<f64>, DataServerError> {
    let tile_col = col / metadata.tile_width;
    let tile_row = row / metadata.tile_height;
    let chunk_index = tile_row * metadata.tiles_across + tile_col;

    let local_col = col % metadata.tile_width;
    let local_row = row % metadata.tile_height;
    let local_idx = (local_row * metadata.tile_width + local_col) as usize;

    let values = match source {
        DataSource::Remote {
            store,
            path,
            tile_info,
        } => read_remote_chunk_f64(
            store,
            path,
            tile_info,
            metadata,
            chunk_index,
            cache,
            file_path,
            band_index,
            0, // full resolution IFD
        )?,
        DataSource::HttpDirect {
            url,
            http,
            tile_info,
        } => read_http_chunk_f64(
            http,
            url,
            tile_info,
            metadata,
            chunk_index,
            cache,
            file_path,
            band_index,
            0, // full resolution IFD
        )?,
        _ => {
            let mut decoder = source.open_decoder()?;
            decode_chunk_f64(&mut decoder, chunk_index, metadata, band_index)?
        }
    };

    if local_idx >= values.len() {
        return Ok(None);
    }
    Ok(values[local_idx])
}

/// Maximum number of concurrent tile fetches for remote sources.
const MAX_TILE_CONCURRENCY: usize = 5;

/// Shared rayon thread pool for parallel tile fetching.
/// Avoids the overhead of creating a new pool per request (~10-50us each).
static TILE_FETCH_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(MAX_TILE_CONCURRENCY)
        .thread_name(|i| format!("tile-fetch-{i}"))
        .build()
        .expect("failed to create tile fetch thread pool")
});

/// Read pixel values within a bounding box from a GeoTIFF data source.
/// Returns a row-major grid of values [row_start..row_end, col_start..col_end].
/// `band_index` selects which band (0-based) for multi-band files.
///
/// For remote sources, tiles are fetched in parallel (up to `MAX_TILE_CONCURRENCY`
/// Read a bbox from a specific overview level for map rendering.
///
/// Uses the overview's tile layout and geometry. Falls back to full resolution
/// if the overview doesn't have tile info for remote sources.
#[allow(clippy::too_many_arguments)]
pub fn read_bbox_overview(
    source: &DataSource,
    metadata: &TiffMetadata,
    overview: &OverviewLevel,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let nx = (col_end - col_start) as usize;
    let ny = (row_end - row_start) as usize;
    let total_pixels = nx * ny;

    // Build a temporary metadata for this overview level
    let ov_metadata = TiffMetadata {
        width: overview.width,
        height: overview.height,
        tile_width: overview.tile_width,
        tile_height: overview.tile_height,
        tiles_across: overview.tiles_across,
        tiles_down: overview.tiles_down,
        samples_per_pixel: metadata.samples_per_pixel,
        geo_transform: metadata.overview_geo_transform(overview),
        nodata: metadata.nodata,
        scale: metadata.scale,
        offset: metadata.offset,
        overviews: Vec::new(),
    };

    let tile_col_start = col_start / overview.tile_width;
    let tile_col_end = (col_end - 1) / overview.tile_width + 1;
    let tile_row_start = row_start / overview.tile_height;
    let tile_row_end = (row_end - 1) / overview.tile_height + 1;

    if let DataSource::Remote { store, path, .. } = source {
        let ov_tile_info = overview.tile_info.as_ref().ok_or_else(|| {
            DataServerError::GeoTiff(format!(
                "Overview IFD {} has no tile info for remote source (header too small?)",
                overview.ifd_index
            ))
        })?;
        return read_bbox_parallel(
            store,
            path,
            ov_tile_info,
            &ov_metadata,
            col_start,
            row_start,
            col_end,
            row_end,
            tile_col_start,
            tile_col_end,
            tile_row_start,
            tile_row_end,
            cache,
            file_path,
            band_index,
            nx,
            total_pixels,
            overview.ifd_index as u16,
        );
    }

    if let DataSource::HttpDirect {
        url,
        http,
        tile_info: _,
    } = source
    {
        let ov_tile_info = overview.tile_info.as_ref().ok_or_else(|| {
            DataServerError::GeoTiff(format!(
                "Overview IFD {} has no tile info for remote source (header too small?)",
                overview.ifd_index
            ))
        })?;
        return read_bbox_parallel_http(
            http,
            url,
            ov_tile_info,
            &ov_metadata,
            col_start,
            row_start,
            col_end,
            row_end,
            tile_col_start,
            tile_col_end,
            tile_row_start,
            tile_row_end,
            cache,
            file_path,
            band_index,
            nx,
            total_pixels,
            overview.ifd_index as u16,
        );
    }

    // For local files, seek to the overview IFD and read tiles
    let mut decoder = source.open_decoder()?;
    decoder.seek_to_image(overview.ifd_index).map_err(|e| {
        DataServerError::GeoTiff(format!(
            "Failed to seek to overview IFD {}: {e}",
            overview.ifd_index
        ))
    })?;

    let mut result = vec![None; total_pixels];

    for tile_row in tile_row_start..tile_row_end {
        for tile_col in tile_col_start..tile_col_end {
            let chunk_index = tile_row * overview.tiles_across + tile_col;
            let tile_data = decode_chunk_f64(&mut decoder, chunk_index, &ov_metadata, band_index)?;

            copy_tile_to_result(
                &tile_data,
                &mut result,
                tile_col,
                tile_row,
                &ov_metadata,
                col_start,
                row_start,
                col_end,
                row_end,
                nx,
            );
        }
    }

    Ok(result)
}

/// Maximum pixels for map rendering (higher than EDR area queries since output
/// is already bounded by MAX_MAP_PIXELS and data is resampled to output resolution).
const MAX_MAP_PIXELS: usize = 16_000_000;

/// Read a bbox for map rendering with a higher pixel limit.
/// Used by MapEngine::get_raster_tile where output size is already bounded.
#[allow(clippy::too_many_arguments)]
pub fn read_bbox_map(
    source: &DataSource,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let nx = (col_end - col_start) as usize;
    let ny = (row_end - row_start) as usize;
    let total_pixels = nx * ny;

    if total_pixels > MAX_MAP_PIXELS {
        return Err(DataServerError::InvalidParameter(format!(
            "Map render source area {} pixels exceeds maximum {}.",
            total_pixels, MAX_MAP_PIXELS
        )));
    }

    read_bbox_inner(
        source,
        metadata,
        col_start,
        row_start,
        col_end,
        row_end,
        cache,
        file_path,
        band_index,
        nx,
        total_pixels,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn read_bbox(
    source: &DataSource,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
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

    read_bbox_inner(
        source,
        metadata,
        col_start,
        row_start,
        col_end,
        row_end,
        cache,
        file_path,
        band_index,
        nx,
        total_pixels,
    )
}

#[allow(clippy::too_many_arguments)]
fn read_bbox_inner(
    source: &DataSource,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
    nx: usize,
    total_pixels: usize,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let tile_col_start = col_start / metadata.tile_width;
    let tile_col_end = (col_end - 1) / metadata.tile_width + 1;
    let tile_row_start = row_start / metadata.tile_height;
    let tile_row_end = (row_end - 1) / metadata.tile_height + 1;

    // For remote sources, fetch tiles in parallel
    if let DataSource::Remote {
        store,
        path,
        tile_info,
    } = source
    {
        return read_bbox_parallel(
            store,
            path,
            tile_info,
            metadata,
            col_start,
            row_start,
            col_end,
            row_end,
            tile_col_start,
            tile_col_end,
            tile_row_start,
            tile_row_end,
            cache,
            file_path,
            band_index,
            nx,
            total_pixels,
            0, // full resolution IFD
        );
    }

    if let DataSource::HttpDirect {
        url,
        http,
        tile_info,
    } = source
    {
        return read_bbox_parallel_http(
            http,
            url,
            tile_info,
            metadata,
            col_start,
            row_start,
            col_end,
            row_end,
            tile_col_start,
            tile_col_end,
            tile_row_start,
            tile_row_end,
            cache,
            file_path,
            band_index,
            nx,
            total_pixels,
            0, // full resolution IFD
        );
    }

    // Non-remote: sequential path (decoder is not thread-safe)
    let mut decoder = source.open_decoder()?;
    let mut result = vec![None; total_pixels];

    for tile_row in tile_row_start..tile_row_end {
        for tile_col in tile_col_start..tile_col_end {
            let chunk_index = tile_row * metadata.tiles_across + tile_col;
            let tile_data = decode_chunk_f64(&mut decoder, chunk_index, metadata, band_index)?;

            copy_tile_to_result(
                &tile_data,
                &mut result,
                tile_col,
                tile_row,
                metadata,
                col_start,
                row_start,
                col_end,
                row_end,
                nx,
            );
        }
    }

    Ok(result)
}

/// Parallel tile fetching for remote data sources.
/// Uses a rayon thread pool capped at `MAX_TILE_CONCURRENCY` threads.
#[allow(clippy::too_many_arguments)]
fn read_bbox_parallel(
    store: &ds_storage::DataStore,
    obj_path: &ds_storage::object_store::path::Path,
    tile_info: &RemoteTileInfo,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    tile_col_start: u32,
    tile_col_end: u32,
    tile_row_start: u32,
    tile_row_end: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
    nx: usize,
    total_pixels: usize,
    ifd_index: u16,
) -> Result<Vec<Option<f64>>, DataServerError> {
    use rayon::prelude::*;

    // Collect all tile coordinates we need to fetch
    let tile_coords: Vec<(u32, u32)> = (tile_row_start..tile_row_end)
        .flat_map(|tr| (tile_col_start..tile_col_end).map(move |tc| (tr, tc)))
        .collect();

    // Fetch all tiles in parallel using the shared thread pool
    let tile_pixel_count = (metadata.tile_width * metadata.tile_height) as usize;
    let tile_results: Vec<(u32, u32, Vec<Option<f64>>)> = TILE_FETCH_POOL.install(|| {
        tile_coords
            .par_iter()
            .map(|&(tile_row, tile_col)| {
                let chunk_index = tile_row * metadata.tiles_across + tile_col;
                let tile_data =
                    match read_remote_chunk_f64(
                        store,
                        obj_path,
                        tile_info,
                        metadata,
                        chunk_index,
                        cache,
                        file_path,
                        band_index,
                        ifd_index,
                    ) {
                        Ok(data) => data,
                        Err(first_err) => {
                            // Retry up to 2 times for transient S3 errors (truncation, throttling)
                            let mut last_err = first_err;
                            let mut succeeded = false;
                            let mut result_data = vec![None; tile_pixel_count];
                            for attempt in 1..=2 {
                                tracing::debug!(
                                "Tile ({}, {}), chunk {} failed (attempt {}), retrying: {last_err}",
                                tile_row, tile_col, chunk_index, attempt
                            );
                                // Brief backoff before retry
                                std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                                match read_remote_chunk_f64(
                                    store,
                                    obj_path,
                                    tile_info,
                                    metadata,
                                    chunk_index,
                                    cache,
                                    file_path,
                                    band_index,
                                    ifd_index,
                                ) {
                                    Ok(data) => {
                                        tracing::debug!(
                                            "Tile ({}, {}), chunk {} succeeded on retry {}",
                                            tile_row,
                                            tile_col,
                                            chunk_index,
                                            attempt
                                        );
                                        result_data = data;
                                        succeeded = true;
                                        break;
                                    }
                                    Err(e) => last_err = e,
                                }
                            }
                            if !succeeded {
                                tracing::warn!(
                                    "Tile ({}, {}), chunk {} failed after 2 retries: {last_err}",
                                    tile_row,
                                    tile_col,
                                    chunk_index
                                );
                            }
                            result_data
                        }
                    };
                (tile_row, tile_col, tile_data)
            })
            .collect()
    });

    // Assemble the result grid
    let mut result = vec![None; total_pixels];
    for (tile_row, tile_col, tile_data) in &tile_results {
        copy_tile_to_result(
            tile_data,
            &mut result,
            *tile_col,
            *tile_row,
            metadata,
            col_start,
            row_start,
            col_end,
            row_end,
            nx,
        );
    }

    Ok(result)
}

/// Parallel tile fetching for HTTP direct data sources (reqwest).
/// Mirrors `read_bbox_parallel` but uses `read_http_chunk_f64` instead of `read_remote_chunk_f64`.
#[allow(clippy::too_many_arguments)]
fn read_bbox_parallel_http(
    http: &Arc<reqwest::Client>,
    url: &str,
    tile_info: &RemoteTileInfo,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    tile_col_start: u32,
    tile_col_end: u32,
    tile_row_start: u32,
    tile_row_end: u32,
    cache: Option<&crate::cache::TileCache>,
    file_path: &Path,
    band_index: usize,
    nx: usize,
    total_pixels: usize,
    ifd_index: u16,
) -> Result<Vec<Option<f64>>, DataServerError> {
    use rayon::prelude::*;

    let tile_coords: Vec<(u32, u32)> = (tile_row_start..tile_row_end)
        .flat_map(|tr| (tile_col_start..tile_col_end).map(move |tc| (tr, tc)))
        .collect();

    let tile_pixel_count = (metadata.tile_width * metadata.tile_height) as usize;
    let http_clone = http.clone();
    let url_owned = url.to_string();

    let tile_results: Vec<(u32, u32, Vec<Option<f64>>)> = TILE_FETCH_POOL.install(|| {
        tile_coords
            .par_iter()
            .map(|&(tile_row, tile_col)| {
                let chunk_index = tile_row * metadata.tiles_across + tile_col;
                let tile_data = match read_http_chunk_f64(
                    &http_clone,
                    &url_owned,
                    tile_info,
                    metadata,
                    chunk_index,
                    cache,
                    file_path,
                    band_index,
                    ifd_index,
                ) {
                    Ok(data) => data,
                    Err(first_err) => {
                        let mut last_err = first_err;
                        let mut succeeded = false;
                        let mut result_data = vec![None; tile_pixel_count];
                        for attempt in 1..=2 {
                            tracing::debug!(
                                "Tile ({}, {}), chunk {} failed (attempt {}), retrying: {last_err}",
                                tile_row,
                                tile_col,
                                chunk_index,
                                attempt
                            );
                            std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                            match read_http_chunk_f64(
                                &http_clone,
                                &url_owned,
                                tile_info,
                                metadata,
                                chunk_index,
                                cache,
                                file_path,
                                band_index,
                                ifd_index,
                            ) {
                                Ok(data) => {
                                    tracing::debug!(
                                        "Tile ({}, {}), chunk {} succeeded on retry {}",
                                        tile_row,
                                        tile_col,
                                        chunk_index,
                                        attempt
                                    );
                                    result_data = data;
                                    succeeded = true;
                                    break;
                                }
                                Err(e) => last_err = e,
                            }
                        }
                        if !succeeded {
                            tracing::warn!(
                                "Tile ({}, {}), chunk {} failed after 2 retries: {last_err}",
                                tile_row,
                                tile_col,
                                chunk_index
                            );
                        }
                        result_data
                    }
                };
                (tile_row, tile_col, tile_data)
            })
            .collect()
    });

    let mut result = vec![None; total_pixels];
    for (tile_row, tile_col, tile_data) in &tile_results {
        copy_tile_to_result(
            tile_data,
            &mut result,
            *tile_col,
            *tile_row,
            metadata,
            col_start,
            row_start,
            col_end,
            row_end,
            nx,
        );
    }

    Ok(result)
}

/// Copy pixels from a decoded tile into the output result grid.
#[allow(clippy::too_many_arguments)]
fn copy_tile_to_result(
    tile_data: &[Option<f64>],
    result: &mut [Option<f64>],
    tile_col: u32,
    tile_row: u32,
    metadata: &TiffMetadata,
    col_start: u32,
    row_start: u32,
    col_end: u32,
    row_end: u32,
    nx: usize,
) {
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

// ============================================================================
// GeoTIFF metadata parsing
// ============================================================================

fn read_tag_u32(decoder: &mut DecoderWrapper, tag: Tag) -> Option<u32> {
    match decoder.get_tag(tag) {
        Ok(tiff::decoder::ifd::Value::Short(v)) => Some(v as u32),
        Ok(tiff::decoder::ifd::Value::Unsigned(v)) => Some(v),
        // Multi-value tags (e.g., BitsPerSample, SampleFormat for multi-band files):
        // return the first element.
        Ok(tiff::decoder::ifd::Value::List(items)) => match items.first() {
            Some(tiff::decoder::ifd::Value::Short(v)) => Some(*v as u32),
            Some(tiff::decoder::ifd::Value::Unsigned(v)) => Some(*v),
            _ => None,
        },
        _ => None,
    }
}

fn parse_geo_transform(
    decoder: &mut DecoderWrapper,
    width: u32,
    height: u32,
) -> Result<GeoTransform, DataServerError> {
    // Parse CRS first — needed for both paths
    let crs = parse_crs(decoder)?;
    tracing::debug!("Parsed CRS: {:?}", crs);

    // Try ModelTiepointTag (33922) + ModelPixelScaleTag (33550) first — the common case
    let tiepoint_result = decoder.get_tag(Tag::Unknown(33922));
    let pixelscale_result = decoder.get_tag(Tag::Unknown(33550));

    if let (Ok(tiepoint), Ok(pixelscale)) = (tiepoint_result, pixelscale_result) {
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

        return Ok(GeoTransform {
            origin_x,
            origin_y,
            pixel_width: ps[0],
            pixel_height: ps[1],
            width,
            height,
            crs,
        });
    }

    // Fallback: try ModelTransformationTag (34264) — a 4x4 affine matrix
    if let Ok(transform_tag) = decoder.get_tag(Tag::Unknown(34264)) {
        let matrix = extract_doubles(&transform_tag).ok_or_else(|| {
            DataServerError::GeoTiff("Cannot parse ModelTransformationTag".into())
        })?;

        return GeoTransform::from_transformation_matrix(&matrix, width, height, crs)
            .map_err(DataServerError::GeoTiff);
    }

    Err(DataServerError::GeoTiff(
        "Missing geolocation tags — need either ModelTiepointTag (33922) + \
         ModelPixelScaleTag (33550), or ModelTransformationTag (34264)"
            .into(),
    ))
}

/// Parse the CRS from GeoTIFF GeoKey Directory.
fn parse_crs(decoder: &mut DecoderWrapper) -> Result<Crs, DataServerError> {
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
                    keys.insert(
                        key_id,
                        GeoKeyValue::Doubles(
                            double_params[value_offset..value_offset + count].to_vec(),
                        ),
                    );
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
    let epsg = match keys.get(&3072) {
        Some(GeoKeyValue::Short(v)) => *v,
        _ => 0,
    };

    // If projection method is missing/user-defined, try to identify by EPSG code
    if proj_method == 0 && epsg != 0 && epsg != 32767 {
        // Use hardcoded parameters for well-known EPSG codes
        return match epsg {
            3067 => Ok(Crs::TransverseMercator {
                lat0: 0.0,
                lon0: 27.0_f64.to_radians(),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
            }),
            3035 => Ok(Crs::LambertAzimuthalEqualArea {
                lat0: 52.0_f64.to_radians(),
                lon0: 10.0_f64.to_radians(),
                false_e: 4_321_000.0,
                false_n: 3_210_000.0,
            }),
            _ => Err(DataServerError::GeoTiff(format!(
                "EPSG:{} is not supported and GeoKeys lack projection parameters. \
                 Convert with: gdalwarp -t_srs EPSG:4326 -of COG input.tif output.tif",
                epsg
            ))),
        };
    }

    match proj_method {
        // CT_TransverseMercator = 1
        1 => {
            let lat0 = get_double_key(&keys, 3081).unwrap_or(0.0).to_radians(); // NatOriginLat
            let lon0 = get_double_key(&keys, 3080).unwrap_or(0.0).to_radians(); // NatOriginLong
                                                                                // Try ProjNatOriginLong (3080), fall back to ProjCenterLong (3088)
            let lon0 = if lon0 == 0.0 {
                get_double_key(&keys, 3088).unwrap_or(0.0).to_radians()
            } else {
                lon0
            };
            let k0 = get_double_key(&keys, 3092).unwrap_or(1.0); // ScaleAtNatOrigin
            let false_e = get_double_key(&keys, 3082).unwrap_or(0.0); // FalseEasting
            let false_n = get_double_key(&keys, 3083).unwrap_or(0.0); // FalseNorthing

            Ok(Crs::TransverseMercator {
                lat0,
                lon0,
                k0,
                false_e,
                false_n,
            })
        }
        // CT_LambertConfConic_2SP = 8
        8 => {
            let lat1 = get_double_key(&keys, 3078).unwrap_or(0.0).to_radians(); // StdParallel1
            let lat2 = get_double_key(&keys, 3079).unwrap_or(0.0).to_radians(); // StdParallel2
            let lat0 = get_double_key(&keys, 3081).unwrap_or(0.0).to_radians(); // FalseOriginLat
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

            Ok(Crs::LambertConformalConic {
                lat1,
                lat2,
                lat0,
                lon0,
                false_e,
                false_n,
            })
        }
        // CT_LambertAzimEqualArea = 10
        10 => {
            // Try NatOriginLat (3081), then CenterLat (3089)
            let lat0 = get_double_key(&keys, 3081)
                .or_else(|| get_double_key(&keys, 3089))
                .unwrap_or(0.0)
                .to_radians();
            // Try NatOriginLong (3080), then CenterLong (3088)
            let lon0 = get_double_key(&keys, 3080)
                .or_else(|| get_double_key(&keys, 3088))
                .unwrap_or(0.0)
                .to_radians();
            let false_e = get_double_key(&keys, 3082)
                .or_else(|| get_double_key(&keys, 3086))
                .unwrap_or(0.0);
            let false_n = get_double_key(&keys, 3083)
                .or_else(|| get_double_key(&keys, 3087))
                .unwrap_or(0.0);

            Ok(Crs::LambertAzimuthalEqualArea {
                lat0,
                lon0,
                false_e,
                false_n,
            })
        }
        _ => Err(DataServerError::GeoTiff(format!(
            "Unsupported projection method {} (GeoKey 3075). \
                 Supported: Transverse Mercator (1), Lambert Conformal Conic 2SP (8), \
                 Lambert Azimuthal Equal Area (10). \
                 Convert with: gdalwarp -t_srs EPSG:4326 -of COG input.tif output.tif",
            proj_method
        ))),
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

fn parse_nodata(decoder: &mut DecoderWrapper) -> Option<f64> {
    match decoder.get_tag(Tag::Unknown(42113)) {
        Ok(tiff::decoder::ifd::Value::Ascii(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_scale_offset(decoder: &mut DecoderWrapper) -> (Option<f64>, Option<f64>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn find_test_radar_dir() -> PathBuf {
        let candidates = ["testdata/radar", "../../testdata/radar"];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.is_dir() {
                return p;
            }
        }
        panic!("Cannot find testdata/radar directory");
    }

    fn find_first_tif(dir: &Path) -> PathBuf {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if name.ends_with(".tif") {
                return entry.path();
            }
        }
        panic!("No .tif files found in {}", dir.display());
    }

    #[test]
    fn header_read_parses_same_metadata_as_full_file() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);

        // Full-file path: parse from local file
        let full_meta = TiffMetadata::from_file(&tif_path).unwrap();

        // Range-read path: use local store + get_range
        let (store, _prefix) = ds_storage::build_store(dir.to_str().unwrap()).unwrap();
        let filename = tif_path.file_name().unwrap().to_str().unwrap();
        let obj_path = ds_storage::object_store::path::Path::from(filename);
        let file_size = std::fs::metadata(&tif_path).unwrap().len();

        let (header_meta, tile_info) = TiffMetadata::from_header_read(&store, &obj_path, file_size)
            .expect("from_header_read should succeed on test COG");

        // Metadata should match
        assert_eq!(full_meta.width, header_meta.width);
        assert_eq!(full_meta.height, header_meta.height);
        assert_eq!(full_meta.tile_width, header_meta.tile_width);
        assert_eq!(full_meta.tile_height, header_meta.tile_height);
        assert_eq!(full_meta.tiles_across, header_meta.tiles_across);
        assert_eq!(full_meta.tiles_down, header_meta.tiles_down);
        assert_eq!(full_meta.nodata, header_meta.nodata);
        assert_eq!(full_meta.scale, header_meta.scale);
        assert_eq!(full_meta.offset, header_meta.offset);

        // Tile info should be populated
        let total_tiles = (header_meta.tiles_across * header_meta.tiles_down) as usize;
        assert_eq!(tile_info.tile_offsets.len(), total_tiles);
        assert_eq!(tile_info.tile_byte_counts.len(), total_tiles);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn range_read_pixel_matches_full_file_pixel() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);

        // Full-file read
        let full_meta = TiffMetadata::from_file(&tif_path).unwrap();
        let full_source = DataSource::from_path(&tif_path);

        // Range-read setup
        let (store, _prefix) = ds_storage::build_store(dir.to_str().unwrap()).unwrap();
        let filename = tif_path.file_name().unwrap().to_str().unwrap();
        let obj_path = ds_storage::object_store::path::Path::from(filename);
        let file_size = std::fs::metadata(&tif_path).unwrap().len();

        let (header_meta, tile_info) =
            TiffMetadata::from_header_read(&store, &obj_path, file_size).unwrap();

        let remote_source = DataSource::Remote {
            store: store.clone(),
            path: obj_path,
            tile_info,
        };

        // Compare several pixel values
        let test_coords = [
            (0, 0),
            (full_meta.width / 2, full_meta.height / 2),
            (full_meta.width - 1, full_meta.height - 1),
            (full_meta.tile_width, full_meta.tile_height), // second tile
        ];

        for (col, row) in test_coords {
            let full_val =
                read_pixel(&full_source, &full_meta, col, row, None, &tif_path, 0).unwrap();
            let range_val =
                read_pixel(&remote_source, &header_meta, col, row, None, &tif_path, 0).unwrap();
            assert_eq!(
                full_val, range_val,
                "Pixel mismatch at ({}, {}): full={:?} vs range={:?}",
                col, row, full_val, range_val
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tile_cache_avoids_refetch() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);

        let (store, _prefix) = ds_storage::build_store(dir.to_str().unwrap()).unwrap();
        let filename = tif_path.file_name().unwrap().to_str().unwrap();
        let obj_path = ds_storage::object_store::path::Path::from(filename);
        let file_size = std::fs::metadata(&tif_path).unwrap().len();

        let (meta, tile_info) =
            TiffMetadata::from_header_read(&store, &obj_path, file_size).unwrap();

        let remote_source = DataSource::Remote {
            store: store.clone(),
            path: obj_path,
            tile_info,
        };

        let cache = crate::cache::TileCache::new(64 * 1024 * 1024);
        let pseudo_path = PathBuf::from(filename);

        // First read: cache miss
        let val1 = read_pixel(&remote_source, &meta, 0, 0, Some(&cache), &pseudo_path, 0).unwrap();
        let (hits, misses) = cache.stats();
        assert_eq!(misses, 1);
        assert_eq!(hits, 0);

        // Second read: cache hit, same value
        let val2 = read_pixel(&remote_source, &meta, 0, 0, Some(&cache), &pseudo_path, 0).unwrap();
        assert_eq!(val1, val2);
        let (hits, misses) = cache.stats();
        assert_eq!(hits, 1);
        assert_eq!(misses, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_read_bbox_matches_sequential() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);

        // Sequential: read via local file (uses decode_chunk_f64 path)
        let full_meta = TiffMetadata::from_file(&tif_path).unwrap();
        let full_source = DataSource::from_path(&tif_path);

        // Pick a bbox that spans multiple tiles but stays under MAX_AREA_PIXELS
        let col_start = 0;
        let row_start = 0;
        let max_side = ((MAX_AREA_PIXELS as f64).sqrt() as u32).min(full_meta.tile_width * 2);
        let col_end = max_side.min(full_meta.width);
        let row_end = max_side.min(full_meta.height);

        let sequential_result = read_bbox(
            &full_source,
            &full_meta,
            col_start,
            row_start,
            col_end,
            row_end,
            None,
            &tif_path,
            0,
        )
        .unwrap();

        // Parallel: read via remote source (uses read_bbox_parallel path)
        let (store, _prefix) = ds_storage::build_store(dir.to_str().unwrap()).unwrap();
        let filename = tif_path.file_name().unwrap().to_str().unwrap();
        let obj_path = ds_storage::object_store::path::Path::from(filename);
        let file_size = std::fs::metadata(&tif_path).unwrap().len();

        let (header_meta, tile_info) =
            TiffMetadata::from_header_read(&store, &obj_path, file_size).unwrap();

        let remote_source = DataSource::Remote {
            store: store.clone(),
            path: obj_path,
            tile_info,
        };

        let cache = crate::cache::TileCache::new(64 * 1024 * 1024);
        let pseudo_path = PathBuf::from(filename);

        let parallel_result = read_bbox(
            &remote_source,
            &header_meta,
            col_start,
            row_start,
            col_end,
            row_end,
            Some(&cache),
            &pseudo_path,
            0,
        )
        .unwrap();

        assert_eq!(
            sequential_result.len(),
            parallel_result.len(),
            "Result lengths differ: sequential={} vs parallel={}",
            sequential_result.len(),
            parallel_result.len()
        );

        let mut mismatches = 0;
        for (i, (s, p)) in sequential_result
            .iter()
            .zip(parallel_result.iter())
            .enumerate()
        {
            if s != p {
                if mismatches < 5 {
                    eprintln!(
                        "Mismatch at index {}: sequential={:?}, parallel={:?}",
                        i, s, p
                    );
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches,
            0,
            "{} pixel mismatches out of {} total pixels",
            mismatches,
            sequential_result.len()
        );
    }

    // --- Nodata comparison tests ---

    fn meta_with_nodata(nodata: f64) -> TiffMetadata {
        TiffMetadata {
            width: 1,
            height: 1,
            tile_width: 1,
            tile_height: 1,
            tiles_across: 1,
            tiles_down: 1,
            samples_per_pixel: 1,
            geo_transform: crate::geo::GeoTransform {
                origin_x: 0.0,
                origin_y: 0.0,
                pixel_width: 1.0,
                pixel_height: 1.0,
                width: 1,
                height: 1,
                crs: crate::geo::Crs::Wgs84,
            },
            nodata: Some(nodata),
            scale: None,
            offset: None,
            overviews: Vec::new(),
        }
    }

    #[test]
    fn nodata_integer_exact_match() {
        let meta = meta_with_nodata(255.0);
        assert!(meta.is_nodata_raw(255.0));
        assert!(!meta.is_nodata_raw(254.0));
        assert!(!meta.is_nodata_raw(255.5));
    }

    #[test]
    fn nodata_negative_integer() {
        let meta = meta_with_nodata(-9999.0);
        assert!(meta.is_nodata_raw(-9999.0));
        assert!(!meta.is_nodata_raw(-9998.0));
    }

    #[test]
    fn nodata_f32_promotion() {
        // A nodata value that differs between f32 and f64 representation.
        // f32 can only represent ~7 decimal digits of precision.
        // When a nodata is specified as a precise f64 value but pixels are f32,
        // the pixel value (f32→f64 promoted) may differ from the tag value.
        let nodata_tag = 1.0000001_f64; // more precision than f32 can hold
        let pixel_f32 = nodata_tag as f32; // rounds to nearest f32
        let pixel_promoted = pixel_f32 as f64; // promoted back — differs from original

        // These should differ at f64 precision
        assert_ne!(
            nodata_tag, pixel_promoted,
            "f32 roundtrip should lose precision"
        );

        // But our comparison should still detect the match via f32 roundtrip
        let meta = meta_with_nodata(nodata_tag);
        assert!(
            meta.is_nodata_raw(pixel_promoted),
            "f32-promoted nodata should match tag value"
        );
    }

    #[test]
    fn nodata_zero() {
        let meta = meta_with_nodata(0.0);
        assert!(meta.is_nodata_raw(0.0));
        assert!(!meta.is_nodata_raw(1.0));
        // -0.0 == 0.0 in IEEE 754, so this should also match
        assert!(meta.is_nodata_raw(-0.0));
    }

    #[test]
    fn nodata_u16_max() {
        let meta = meta_with_nodata(65535.0);
        assert!(meta.is_nodata_raw(65535.0));
        assert!(!meta.is_nodata_raw(65534.0));
    }
}
