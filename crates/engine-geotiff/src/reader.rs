use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock};

use bytes::Bytes;
use ds_core::error::DataServerError;
use memmap2::Mmap;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use ds_core::geo::{Crs, GeoTransform};

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
const MAX_IFD_LEVELS: usize = 256;

/// Maximum number of pixels in an area query result.
const MAX_AREA_PIXELS: usize = 1_000_000;

/// Compute tile index with overflow protection.
fn safe_tile_index(
    tile_row: u32,
    tiles_across: u32,
    tile_col: u32,
) -> Result<u32, DataServerError> {
    let index = (tile_row as u64)
        .checked_mul(tiles_across as u64)
        .and_then(|v| v.checked_add(tile_col as u64))
        .ok_or_else(|| DataServerError::Engine("Tile index overflow".into()))?;
    if index > u32::MAX as u64 {
        return Err(DataServerError::Engine("Tile index overflow".into()));
    }
    Ok(index as u32)
}

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

/// Backing bytes for a [`Decoder`] — either heap-owned (downloaded payload) or
/// memory-mapped (local file). Both impl `AsRef<[u8]>` so a single
/// [`DecoderWrapper`] variant covers both.
#[derive(Clone)]
enum SharedBytes {
    Heap(Bytes),
    Mmap(Arc<Mmap>),
}

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Heap(b) => b,
            Self::Mmap(m) => m,
        }
    }
}

/// Lazily-populated mmap cache. Shared across clones of `DataSource::LocalFile`
/// so the file is mmaped once per catalog entry and reused on every render.
type MmapCache = Arc<OnceLock<Result<Arc<Mmap>, String>>>;

/// Data source for reading GeoTIFF data.
#[derive(Debug, Clone)]
pub enum DataSource {
    /// Local filesystem path. The mmap is built lazily on first decoder open
    /// (which in practice is the catalog scan) and cached for the lifetime of
    /// this entry — every subsequent render reuses the same mapping with no
    /// per-request `File::open` / `BufReader` / IFD re-parse from disk (#204).
    LocalFile {
        path: PathBuf,
        mmap_cache: MmapCache,
    },
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
        DataSource::LocalFile {
            path: path.to_path_buf(),
            mmap_cache: Arc::new(OnceLock::new()),
        }
    }

    pub fn from_bytes(data: impl Into<Bytes>) -> Self {
        DataSource::InMemory(data.into())
    }

    /// Map a local file lazily and cache the result (success or error) for the
    /// lifetime of this `DataSource`. A cached error means this `DataSource`
    /// won't retry — but the next catalog scan rebuilds a fresh `DataSource`
    /// when the file is new or its size has changed, so the recovery window
    /// for a transient failure (`EMFILE`, `ENOMEM`, NFS hiccup, …) is at most
    /// one `poll_interval_secs`. Files whose mmap fails on the initial scan
    /// are skipped by the scan loop with a warning rather than poisoning the
    /// catalog.
    ///
    /// Safety note on `Mmap::map`: read-only mapping of a file that, by our
    /// publishing convention, is replaced via atomic rename (creating a new
    /// inode) rather than overwritten in place. An in-place overwrite would
    /// produce torn reads — same hazard as the previous `File::open` path.
    fn load_mmap(
        path: &Path,
        cache: &OnceLock<Result<Arc<Mmap>, String>>,
    ) -> Result<Arc<Mmap>, DataServerError> {
        let cached = cache.get_or_init(|| {
            let file =
                File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
            // SAFETY: see the doc comment above — read-only mapping of a file
            // we treat as immutable for the lifetime of this `DataSource`.
            let mmap = unsafe { Mmap::map(&file) }
                .map_err(|e| format!("Cannot mmap {}: {e}", path.display()))?;
            Ok(Arc::new(mmap))
        });
        match cached {
            Ok(m) => Ok(Arc::clone(m)),
            Err(msg) => Err(DataServerError::Engine(msg.clone())),
        }
    }

    /// Open a decoder for this data source (LocalFile and InMemory only).
    fn open_decoder(&self) -> Result<DecoderWrapper, DataServerError> {
        match self {
            DataSource::LocalFile { path, mmap_cache } => {
                let mmap = Self::load_mmap(path, mmap_cache)?;
                let cursor = Cursor::new(SharedBytes::Mmap(mmap));
                Ok(DecoderWrapper(Decoder::new(cursor).map_err(|e| {
                    DataServerError::Engine(format!("Invalid TIFF {}: {e}", path.display()))
                })?))
            }
            DataSource::InMemory(bytes) => {
                let cursor = Cursor::new(SharedBytes::Heap(bytes.clone()));
                Ok(DecoderWrapper(Decoder::new(cursor).map_err(|e| {
                    DataServerError::Engine(format!("Invalid TIFF (in-memory): {e}"))
                })?))
            }
            DataSource::Remote { .. } | DataSource::HttpDirect { .. } => {
                Err(DataServerError::Engine(
                    "Cannot open decoder for remote source; use range reads".into(),
                ))
            }
        }
    }

    fn display_name(&self) -> String {
        match self {
            DataSource::LocalFile { path, .. } => path.display().to_string(),
            DataSource::InMemory(_) => "<in-memory>".to_string(),
            DataSource::Remote { path, .. } => format!("<remote:{}>", path),
            DataSource::HttpDirect { url, .. } => format!("<http:{}>", url),
        }
    }
}

/// Newtype over the unified `Decoder<Cursor<SharedBytes>>` so existing call
/// sites in this file don't need to thread the cursor type around.
struct DecoderWrapper(Decoder<Cursor<SharedBytes>>);

impl DecoderWrapper {
    fn dimensions(&mut self) -> Result<(u32, u32), DataServerError> {
        self.0
            .dimensions()
            .map_err(|e| DataServerError::Engine(format!("{e}")))
    }

    #[allow(dead_code)]
    fn colortype(&mut self) -> Result<tiff::ColorType, DataServerError> {
        self.0
            .colortype()
            .map_err(|e| DataServerError::Engine(format!("{e}")))
    }

    fn get_tag(&mut self, tag: Tag) -> Result<tiff::decoder::ifd::Value, tiff::TiffError> {
        self.0.get_tag(tag)
    }

    fn read_chunk(&mut self, idx: u32) -> Result<DecodingResult, tiff::TiffError> {
        self.0.read_chunk(idx)
    }

    fn seek_to_image(&mut self, index: usize) -> Result<(), tiff::TiffError> {
        self.0.seek_to_image(index)
    }

    fn more_images(&self) -> bool {
        self.0.more_images()
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
            .map_err(|e| DataServerError::Engine(format!("Cannot read dimensions: {e}")))?;

        if width > MAX_RASTER_DIMENSION || height > MAX_RASTER_DIMENSION {
            return Err(DataServerError::Engine(format!(
                "Raster dimensions {}x{} exceed maximum {}",
                width, height, MAX_RASTER_DIMENSION
            )));
        }

        let tile_width = read_tag_u32(decoder, Tag::TileWidth)
            .ok_or_else(|| DataServerError::Engine(format!(
                "{source_name}: Not a tiled TIFF (TileWidth missing). \
                 Convert to tiled COG with: gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif"
            )))?;
        let tile_height = read_tag_u32(decoder, Tag::TileLength)
            .ok_or_else(|| DataServerError::Engine(format!(
                "{source_name}: Not a tiled TIFF (TileLength missing). \
                 Convert to tiled COG with: gdal_translate -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif"
            )))?;

        if tile_width == 0 || tile_height == 0 {
            return Err(DataServerError::Engine(format!(
                "{source_name}: Invalid tile dimensions {}x{} (must be > 0)",
                tile_width, tile_height
            )));
        }

        let tiles_across = width.div_ceil(tile_width);
        let tiles_down = height.div_ceil(tile_height);
        let samples_per_pixel = read_tag_u32(decoder, Tag::SamplesPerPixel).unwrap_or(1);

        // Security: check decoded tile size using actual sample size and band count.
        // samples_per_pixel could be large in multi-band files, so we use
        // the real value rather than assuming worst-case 8 bytes/sample.
        let bps = Self::bits_per_sample_from_decoder(decoder);
        let bytes_per_sample = (bps as usize).div_ceil(8);
        let decoded_tile_bytes = (tile_width as usize)
            .checked_mul(tile_height as usize)
            .and_then(|v| v.checked_mul(bytes_per_sample))
            .and_then(|v| v.checked_mul(samples_per_pixel as usize))
            .ok_or_else(|| {
                DataServerError::Engine(format!(
                    "Decoded tile size overflow ({}x{} px, {} bands, {} bytes/sample)",
                    tile_width, tile_height, samples_per_pixel, bytes_per_sample
                ))
            })?;
        if decoded_tile_bytes > MAX_DECODED_TILE_BYTES {
            return Err(DataServerError::Engine(format!(
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
            if ifd_index > MAX_IFD_LEVELS {
                tracing::warn!(
                    "{source_name}: IFD chain exceeds {MAX_IFD_LEVELS} levels, stopping traversal"
                );
                break;
            }
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
        let cursor = Cursor::new(SharedBytes::Heap(header_bytes));
        let mut decoder = DecoderWrapper(Decoder::new(cursor).ok()?);

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
        let cursor = Cursor::new(SharedBytes::Heap(header_bytes));
        let mut decoder = DecoderWrapper(Decoder::new(cursor).ok()?);
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

    /// Select the overview level to read for the given bbox and output size.
    ///
    /// Compares how many source pixels the bbox covers at each level against the
    /// output dimensions and picks the coarsest level that still covers the
    /// output without upscaling (least data to decode). If no overview is large
    /// enough — the output exceeds the biggest overview but is still below full
    /// resolution — the biggest overview is used anyway provided the implied
    /// (nearest-neighbour) upscale stays within `MIN_OVERVIEW_FRACTION`; beyond
    /// that the output is at/near native resolution and `None` is returned so the
    /// caller reads full resolution. Also returns `None` when the bbox misses the
    /// raster or full resolution already fits the output.
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

        // Pass 1 — strict never-upscale. Pick the coarsest overview that still
        // has at least as many source pixels as the output (least data read, no
        // upscaling). Overviews are sorted largest-first and both axes shrink
        // monotonically across levels, so the qualifying levels are a prefix:
        // once an axis falls short, every coarser level falls short too, so we
        // can stop. This leaves every output an overview *can* satisfy selected
        // exactly as before — no change to mid-zoom selection.
        let mut best: Option<&OverviewLevel> = None;
        for ov in &self.overviews {
            let ov_gt = self.overview_geo_transform(ov);
            match ov_gt.bbox_to_pixels(bbox_west, bbox_south, bbox_east, bbox_north) {
                Some((c0, r0, c1, r1))
                    if (c1 - c0) >= output_width && (r1 - r0) >= output_height =>
                {
                    best = Some(ov);
                }
                // Stop at the first level that doesn't qualify. Overviews are
                // visited finest-first and both reasons to fall here are monotone
                // in that direction: a level with too few source pixels is only
                // followed by coarser (even smaller) levels, and a level whose
                // bbox maps to < 1 pixel is only followed by coarser (even more
                // sub-pixel) levels — so no later level can qualify either.
                _ => break,
            }
        }
        if best.is_some() {
            return best;
        }

        // Pass 2 — bounded upscale at the cliff. The output is larger than the
        // biggest overview but (per the early return above) smaller than full
        // resolution. A strict never-upscale rule falls straight through to the
        // full-resolution IFD here — and on the production FMI composite (base
        // 4963×7316, biggest overview 2481 px) a 2650-px-wide retina GetMap then
        // decoded the entire 36 MP base (measured ~770 ms) instead of the 9 MP
        // overview (~350 ms), for what is only a 1.07× upscale. Live, render
        // jumped from 348 ms at 2481 px to 653 ms at 2550 px.
        //
        // So accept the biggest overview as long as the implied upscale stays
        // within MIN_OVERVIEW_FRACTION (the resample is nearest-neighbour, so an
        // upscale is blocky pixel replication — bounded to keep it tolerable).
        // Beyond that bound the output is at/near native resolution, where full
        // resolution is genuinely the better source, so return None and let the
        // caller read it. This confines full-resolution decodes to ~native-res
        // requests instead of every retina viewport just above an overview.
        const MIN_OVERVIEW_FRACTION: f64 = 0.5; // accept up to a 2× nearest-neighbour upscale
        let largest = self.overviews.first()?;
        let lg = self.overview_geo_transform(largest);
        let (c0, r0, c1, r1) = lg.bbox_to_pixels(bbox_west, bbox_south, bbox_east, bbox_north)?;
        let min_cols = (output_width as f64 * MIN_OVERVIEW_FRACTION).ceil() as u32;
        let min_rows = (output_height as f64 * MIN_OVERVIEW_FRACTION).ceil() as u32;
        if (c1 - c0) >= min_cols && (r1 - r0) >= min_rows {
            Some(largest)
        } else {
            None
        }
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
        .map_err(|e| DataServerError::Engine(format!("Failed to read tile: {e}")))?;

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
        _ => return Err(DataServerError::Engine("Unsupported data type".into())),
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
                    DataServerError::Engine(format!("Deflate decompression failed: {e}"))
                })?;
            if decompressed.len() >= MAX_DECODED_TILE_BYTES {
                return Err(DataServerError::Engine(format!(
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
                .map_err(|e| DataServerError::Engine(format!("LZW decompression failed: {e}")))?;
            if decompressed.len() > MAX_DECODED_TILE_BYTES {
                return Err(DataServerError::Engine(format!(
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
        return Err(DataServerError::Engine(
            "Invalid tile: zero sample stride".into(),
        ));
    }
    let pixel_count = raw.len() / sample_stride;

    // Validate that every pixel access is within bounds (last pixel's last byte)
    if pixel_count > 0 {
        let last_offset = (pixel_count - 1) * sample_stride + band_byte_offset + bps;
        if last_offset > raw.len() {
            return Err(DataServerError::Engine(format!(
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
    // Runtime handle to drive the fetch on when called from a non-Tokio
    // (rayon) thread; `None` for callers already on a runtime worker. See #222.
    handle: Option<&tokio::runtime::Handle>,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let idx = chunk_index as usize;
    if idx >= tile_info.tile_offsets.len() {
        return Err(DataServerError::Engine(format!(
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
        DataServerError::Engine(format!(
            "Tile {} byte range overflow: offset={} + count={}",
            idx, offset, byte_count
        ))
    })?;

    // Fetch a byte range, reusing the caller's runtime handle when on a rayon
    // thread (avoids a per-call Runtime::new — see #222).
    let fetch_range = |range: std::ops::Range<usize>| match handle {
        Some(h) => store.get_range_on(obj_path, range, h),
        None => store.get_range(obj_path, range),
    };

    // Check cache for compressed bytes (keyed by file + chunk + IFD level)
    let compressed = if let Some(c) = cache {
        if let Some(cached) = c.get(file_path, chunk_index, ifd_index) {
            cached
        } else {
            let fetched = fetch_range(offset..end)
                .map_err(|e| DataServerError::Engine(format!("Failed to read tile range: {e}")))?;
            // Validate response length matches request
            if fetched.len() != byte_count {
                return Err(DataServerError::Engine(format!(
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
        let fetched = fetch_range(offset..end)
            .map_err(|e| DataServerError::Engine(format!("Failed to read tile range: {e}")))?;
        if fetched.len() != byte_count {
            return Err(DataServerError::Engine(format!(
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
    // Runtime handle to drive the request on when called from a non-Tokio
    // (rayon) thread; `None` for callers already on a runtime worker. See #222.
    handle: Option<&tokio::runtime::Handle>,
) -> Result<Bytes, DataServerError> {
    let range_header = format!("bytes={}-{}", range.start, range.end.saturating_sub(1));
    let url_owned = url.to_string();
    let fut = async {
        let resp = http
            .get(&url_owned)
            .header(reqwest::header::RANGE, &range_header)
            .send()
            .await
            .map_err(|e| DataServerError::Engine(format!("HTTP range read failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(DataServerError::Engine(format!(
                "HTTP range read returned {}",
                resp.status()
            )));
        }
        resp.bytes()
            .await
            .map_err(|e| DataServerError::Engine(format!("Failed to read body: {e}")))
    };
    match handle {
        Some(h) => h.block_on(fut),
        None => block_on_async(fut),
    }
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
    // Runtime handle for the fetch when on a non-Tokio (rayon) thread; `None`
    // for callers already on a runtime worker. See #222.
    handle: Option<&tokio::runtime::Handle>,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let idx = chunk_index as usize;
    if idx >= tile_info.tile_offsets.len() {
        return Err(DataServerError::Engine(format!(
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
        DataServerError::Engine(format!(
            "Tile {} byte range overflow: offset={} + count={}",
            idx, offset, byte_count
        ))
    })?;

    let compressed = if let Some(c) = cache {
        if let Some(cached) = c.get(file_path, chunk_index, ifd_index) {
            cached
        } else {
            let fetched = read_http_range(http, url, offset..end, handle)?;
            if fetched.len() != byte_count {
                return Err(DataServerError::Engine(format!(
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
        let fetched = read_http_range(http, url, offset..end, handle)?;
        if fetched.len() != byte_count {
            return Err(DataServerError::Engine(format!(
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
    let chunk_index = safe_tile_index(tile_row, metadata.tiles_across, tile_col)?;

    let local_col = col % metadata.tile_width;
    let local_row = row % metadata.tile_height;

    // The row stride of the decoded tile depends on the source: the local
    // `tiff`-crate decode clips the rightmost tile column of its padding (stride
    // = clipped width), while the remote/HTTP path decodes the full padded raw
    // tile (stride = tile_width). Indexing with the wrong stride reads a sheared
    // pixel in the last tile column (#458).
    let (values, tile_data_width) = match source {
        DataSource::Remote {
            store,
            path,
            tile_info,
        } => (
            read_remote_chunk_f64(
                store,
                path,
                tile_info,
                metadata,
                chunk_index,
                cache,
                file_path,
                band_index,
                0,    // full resolution IFD
                None, // single-pixel read: caller is already on a runtime worker
            )?,
            metadata.tile_width as usize,
        ),
        DataSource::HttpDirect {
            url,
            http,
            tile_info,
        } => (
            read_http_chunk_f64(
                http,
                url,
                tile_info,
                metadata,
                chunk_index,
                cache,
                file_path,
                band_index,
                0,    // full resolution IFD
                None, // single-pixel read: caller is already on a runtime worker
            )?,
            metadata.tile_width as usize,
        ),
        _ => {
            let mut decoder = source.open_decoder()?;
            (
                decode_chunk_f64(&mut decoder, chunk_index, metadata, band_index)?,
                local_tile_data_width(metadata, tile_col),
            )
        }
    };

    let local_idx = local_row as usize * tile_data_width + local_col as usize;
    if local_idx >= values.len() {
        return Ok(None);
    }
    Ok(values[local_idx])
}

/// Default ceiling on concurrent remote-tile fetches when
/// `MC_COG_TILE_CONCURRENCY` is unset.
///
/// A cold full-viewport WMS GetMap over a remote COG fetches all covering tiles
/// through [`TILE_FETCH_POOL`], so the pool's thread count is the number of
/// byte-range reads in flight at once. The work is **I/O-bound** — each worker
/// blocks on a network range read (driven on the captured runtime handle), then
/// spends a short CPU burst decoding the tile — so the right ceiling is set by
/// remote round-trip latency, not CPU cores. Measured against the live OPERA
/// pan-European COG on CloudFerro S3 (2026-06-14): a 1900×1100 EPSG:3857 cold
/// render fetches 70 tiles; at the old cap of 5 that serialized into ~14 waves
/// (~127 ms/wave ≈ 1.78 s in the tile loop, 96% of a 1.84 s render). 16 cuts
/// that to ~5 waves. The threads are mostly parked on the network, so a count
/// above the core count is fine; raise it further for high-latency stores.
const DEFAULT_TILE_CONCURRENCY: usize = 16;

/// Hard safety ceiling on the resolved tile-fetch concurrency.
///
/// `MC_COG_TILE_CONCURRENCY` feeds `rayon::ThreadPoolBuilder::num_threads`
/// inside the [`TILE_FETCH_POOL`] `LazyLock`. A fat-fingered value (e.g. a
/// `100000` typo) would make the OS refuse to spawn that many threads, the
/// `build().expect(...)` would panic *inside the static initialiser*, poison
/// the `LazyLock`, and then **every** later request that touches the pool would
/// panic too — an unrecoverable crash from one bad env var. Clamping to this
/// ceiling keeps the process alive; 1024 is far above any useful I/O fan-out
/// yet trivially creatable.
const MAX_TILE_CONCURRENCY: usize = 1024;

/// Pure parse of `MC_COG_TILE_CONCURRENCY`: `Some(n)` is an applied override
/// clamped into `[1, MAX_TILE_CONCURRENCY]`; `None` means fall back to
/// [`DEFAULT_TILE_CONCURRENCY`] (unset, non-numeric, or zero). Returning an
/// `Option` lets the pool initialiser report whether an override actually took
/// effect — the resolved value alone is ambiguous (an override of 16 and the
/// compiled default 16 are indistinguishable in a log line).
fn parse_tile_concurrency(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .map(|n| n.min(MAX_TILE_CONCURRENCY))
}

/// Shared rayon thread pool for parallel tile fetching.
/// Avoids the overhead of creating a new pool per request (~10-50us each).
/// Sized once on first use (the first remote-COG render); the resolved thread
/// count and its source are logged there so operators can confirm an
/// `MC_COG_TILE_CONCURRENCY` override took effect.
static TILE_FETCH_POOL: LazyLock<rayon::ThreadPool> =
    LazyLock::new(|| {
        match parse_tile_concurrency(std::env::var("MC_COG_TILE_CONCURRENCY").ok().as_deref()) {
            Some(n) => build_tile_pool(n, "MC_COG_TILE_CONCURRENCY"),
            None => build_tile_pool(DEFAULT_TILE_CONCURRENCY, "default"),
        }
    });

/// Build the tile-fetch pool, degrading gracefully instead of poisoning the
/// [`TILE_FETCH_POOL`] `LazyLock`.
///
/// `rayon::ThreadPoolBuilder::build` can fail to spawn the requested threads for
/// reasons unrelated to the value being absurd — OS thread limits (`ulimit -u`,
/// a cgroup `pids.max`, container runtime caps) can refuse even a modest count
/// like 32 in a constrained pod. A bare `.expect()` there would panic *inside
/// the static initialiser*, poison the lock, and crash every subsequent
/// remote-COG render with no recovery path. Instead we **halve and retry** until
/// a pool builds, recovering whatever concurrency the OS does allow rather than
/// dropping straight to fully serial. A 1-thread pool needs exactly one
/// spawnable thread; if even that fails the process can't serve any request, so
/// the final panic is a genuinely unreachable last resort.
///
/// `source` ("MC_COG_TILE_CONCURRENCY" or "default") is logged so an operator
/// can tell an applied override from the compiled default — the count alone is
/// ambiguous.
fn build_tile_pool(requested: usize, source: &str) -> rayon::ThreadPool {
    tracing::info!(
        threads = requested,
        source,
        "initializing remote-COG tile fetch pool"
    );
    let mut threads = requested.max(1);
    loop {
        match rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("tile-fetch-{i}"))
            .build()
        {
            Ok(pool) => {
                if threads != requested {
                    tracing::warn!(
                        requested,
                        threads,
                        "tile fetch pool built below requested size (OS thread limit?); \
                         remote-COG fetches will use reduced concurrency"
                    );
                }
                return pool;
            }
            Err(e) if threads > 1 => {
                tracing::debug!(threads, error = %e, "tile fetch pool build failed; halving and retrying");
                threads /= 2;
            }
            Err(e) => panic!("failed to build even a single-thread tile fetch pool: {e}"),
        }
    }
}

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
            DataServerError::Engine(format!(
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
            DataServerError::Engine(format!(
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
        DataServerError::Engine(format!(
            "Failed to seek to overview IFD {}: {e}",
            overview.ifd_index
        ))
    })?;

    let mut result = vec![None; total_pixels];

    for tile_row in tile_row_start..tile_row_end {
        for tile_col in tile_col_start..tile_col_end {
            let chunk_index = safe_tile_index(tile_row, overview.tiles_across, tile_col)?;
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
                local_tile_data_width(&ov_metadata, tile_col),
            );
        }
    }

    Ok(result)
}

/// Maximum source pixels for map rendering. Higher than EDR area queries since
/// the output is already bounded by MAX_MAP_DIMENSION (8000) and the data is
/// resampled to output resolution. Needs to be generous because projected CRS
/// data (e.g., TM35FIN radar covering all of Scandinavia) can have large source
/// extents even for moderate output sizes.
const MAX_MAP_PIXELS: usize = 64_000_000;

/// Public accessor for MAX_MAP_PIXELS (used by overview selection in lib.rs).
pub fn max_map_pixels() -> usize {
    MAX_MAP_PIXELS
}

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
            let chunk_index = safe_tile_index(tile_row, metadata.tiles_across, tile_col)?;
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
                local_tile_data_width(metadata, tile_col),
            );
        }
    }

    Ok(result)
}

/// Result of a parallel tile fetch: (row, col, pixel data).
/// Failed tile reads are logged at error level and replaced with all-nodata
/// to allow partial rendering — a map with gaps is better than a 500 error.
type TileFetchResult = (u32, u32, Vec<Option<f64>>);

/// Retry a remote tile read up to 2 times with brief backoff.
/// On final failure, logs at error level and returns all-nodata pixels.
/// Map rendering tolerates partial failures (gaps) — a 500 error is worse
/// than a tile with transparent holes that will be retried on next request.
fn read_remote_chunk_with_retry<F>(
    read_fn: F,
    tile_row: u32,
    tile_col: u32,
    chunk_index: u32,
    nodata_pixel_count: usize,
) -> Vec<Option<f64>>
where
    F: Fn() -> Result<Vec<Option<f64>>, DataServerError>,
{
    match read_fn() {
        Ok(data) => data,
        Err(first_err) => {
            let mut last_err = first_err;
            for attempt in 1..=2 {
                tracing::debug!(
                    "Tile ({}, {}), chunk {} failed (attempt {}), retrying: {last_err}",
                    tile_row,
                    tile_col,
                    chunk_index,
                    attempt
                );
                std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                match read_fn() {
                    Ok(data) => {
                        tracing::debug!(
                            "Tile ({}, {}), chunk {} succeeded on retry {}",
                            tile_row,
                            tile_col,
                            chunk_index,
                            attempt
                        );
                        return data;
                    }
                    Err(e) => last_err = e,
                }
            }
            tracing::error!(
                "Tile ({}, {}), chunk {} failed after 3 attempts: {last_err}. \
                 Rendering with transparent gap.",
                tile_row,
                tile_col,
                chunk_index
            );
            vec![None; nodata_pixel_count]
        }
    }
}

/// Parallel tile fetching for remote data sources.
/// Uses the shared [`TILE_FETCH_POOL`] (sized by `MC_COG_TILE_CONCURRENCY` /
/// [`DEFAULT_TILE_CONCURRENCY`]).
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

    // Capture the current runtime handle (this runs inside the request
    // runtime — whether on a spawn_blocking thread or an async task doesn't
    // matter, we only pass the handle down) so the rayon workers can drive
    // their fetches on the existing runtime instead of spawning a fresh
    // Runtime per tile (#222). `None` only if no runtime is current (e.g.
    // tests), in which case the storage layer falls back to a temporary one.
    let rt_handle = tokio::runtime::Handle::try_current().ok();

    // Fetch all tiles in parallel using the shared thread pool.
    // Failed tiles are logged at error level and replaced with nodata.
    let tile_pixel_count = (metadata.tile_width * metadata.tile_height) as usize;
    let tile_results: Vec<TileFetchResult> = TILE_FETCH_POOL.install(|| {
        tile_coords
            .par_iter()
            .map(|&(tile_row, tile_col)| {
                let chunk_index = match safe_tile_index(tile_row, metadata.tiles_across, tile_col) {
                    Ok(idx) => idx,
                    Err(e) => {
                        tracing::error!("Tile index overflow at ({tile_row}, {tile_col}): {e}");
                        return (tile_row, tile_col, vec![None; tile_pixel_count]);
                    }
                };
                let data = read_remote_chunk_with_retry(
                    || {
                        read_remote_chunk_f64(
                            store,
                            obj_path,
                            tile_info,
                            metadata,
                            chunk_index,
                            cache,
                            file_path,
                            band_index,
                            ifd_index,
                            rt_handle.as_ref(),
                        )
                    },
                    tile_row,
                    tile_col,
                    chunk_index,
                    tile_pixel_count,
                );
                (tile_row, tile_col, data)
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
            // Remote path decodes the full padded raw tile → stride = tile_width.
            metadata.tile_width as usize,
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

    let http_clone = http.clone();
    let url_owned = url.to_string();

    // Reuse the current runtime for the rayon workers' fetches (#222).
    let rt_handle = tokio::runtime::Handle::try_current().ok();

    let tile_pixel_count = (metadata.tile_width * metadata.tile_height) as usize;
    let tile_results: Vec<TileFetchResult> = TILE_FETCH_POOL.install(|| {
        tile_coords
            .par_iter()
            .map(|&(tile_row, tile_col)| {
                let chunk_index = match safe_tile_index(tile_row, metadata.tiles_across, tile_col) {
                    Ok(idx) => idx,
                    Err(e) => {
                        tracing::error!("Tile index overflow at ({tile_row}, {tile_col}): {e}");
                        return (tile_row, tile_col, vec![None; tile_pixel_count]);
                    }
                };
                let data = read_remote_chunk_with_retry(
                    || {
                        read_http_chunk_f64(
                            &http_clone,
                            &url_owned,
                            tile_info,
                            metadata,
                            chunk_index,
                            cache,
                            file_path,
                            band_index,
                            ifd_index,
                            rt_handle.as_ref(),
                        )
                    },
                    tile_row,
                    tile_col,
                    chunk_index,
                    tile_pixel_count,
                );
                (tile_row, tile_col, data)
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
            // HTTP path decodes the full padded raw tile → stride = tile_width.
            metadata.tile_width as usize,
        );
    }

    Ok(result)
}

/// Row stride of a tile decoded by the local `tiff`-crate path
/// (`decode_chunk_f64` → `read_chunk`), which returns edge tiles CLIPPED of their
/// padding. The rightmost tile column is `min(tile_width, width − col0)` wide; all
/// interior columns are exactly `tile_width`. (The remote/HTTP path decodes the
/// full padded raw tile and uses `tile_width` instead.)
fn local_tile_data_width(metadata: &TiffMetadata, tile_col: u32) -> usize {
    let col0 = tile_col * metadata.tile_width;
    metadata.width.saturating_sub(col0).min(metadata.tile_width) as usize
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
    // Row stride of `tile_data`. NOT always `tile_width`: the local `tiff`-crate
    // decode path (`read_chunk`) returns edge tiles CLIPPED of their padding, so
    // the rightmost tile column's rows are `min(tile_width, width - col0)` wide,
    // not `tile_width`. Indexing such a buffer with a `tile_width` stride shears
    // every row by the padding amount — invisible at full res (the data rarely
    // lands in the last tile column) but a "venetian-blind" stripe block once an
    // overview's edge tile carries real data (#458). The remote/HTTP path decodes
    // the FULL padded raw tile, so it passes `tile_width`.
    tile_data_width: usize,
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
            let tile_idx = local_row as usize * tile_data_width + local_col as usize;

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
            .ok_or_else(|| DataServerError::Engine("Cannot parse ModelTiepointTag".into()))?;
        let ps = extract_doubles(&pixelscale)
            .ok_or_else(|| DataServerError::Engine("Cannot parse ModelPixelScaleTag".into()))?;

        if tp.len() < 6 || ps.len() < 2 {
            return Err(DataServerError::Engine(
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
        let matrix = extract_doubles(&transform_tag)
            .ok_or_else(|| DataServerError::Engine("Cannot parse ModelTransformationTag".into()))?;

        return GeoTransform::from_transformation_matrix(&matrix, width, height, crs)
            .map_err(DataServerError::Engine);
    }

    Err(DataServerError::Engine(
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
            _ => Err(DataServerError::Engine(format!(
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
        _ => Err(DataServerError::Engine(format!(
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

    /// Regression test for #204: a `DataSource::LocalFile` must mmap the file
    /// exactly once and reuse the same `Arc<Mmap>` across every subsequent
    /// decoder open, so per-request rendering avoids `File::open`/`BufReader`
    /// and the on-disk IFD re-parse.
    #[test]
    fn local_file_mmaps_once_and_reuses_across_calls() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);

        let source = DataSource::from_path(&tif_path);
        // First open populates the cache.
        let _decoder1 = source.open_decoder().expect("first open");

        let cached_ptr = match &source {
            DataSource::LocalFile { mmap_cache, .. } => {
                let entry = mmap_cache
                    .get()
                    .expect("mmap cache populated after first open");
                let mmap = entry.as_ref().expect("mmap should succeed for the fixture");
                Arc::as_ptr(mmap)
            }
            _ => panic!("from_path should produce LocalFile"),
        };

        // Subsequent opens must NOT re-mmap — they must yield the same Arc.
        for _ in 0..5 {
            let _ = source.open_decoder().expect("subsequent open");
            let entry = match &source {
                DataSource::LocalFile { mmap_cache, .. } => mmap_cache.get().unwrap(),
                _ => unreachable!(),
            };
            let mmap = entry.as_ref().unwrap();
            assert_eq!(
                Arc::as_ptr(mmap),
                cached_ptr,
                "open_decoder must reuse the cached Arc<Mmap>, not allocate a fresh one"
            );
        }

        // Cloning the DataSource shares the cache too — a fresh clone must see
        // the same mmap, not start over.
        let source_clone = source.clone();
        let _ = source_clone.open_decoder().unwrap();
        let cloned_ptr = match &source_clone {
            DataSource::LocalFile { mmap_cache, .. } => {
                Arc::as_ptr(mmap_cache.get().unwrap().as_ref().unwrap())
            }
            _ => unreachable!(),
        };
        assert_eq!(cloned_ptr, cached_ptr, "Clone must share the mmap cache");
    }

    #[test]
    fn header_read_parses_same_metadata_as_full_file() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);

        // Full-file path: parse from local file
        let full_meta = TiffMetadata::from_source(&DataSource::from_path(&tif_path)).unwrap();

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
        let full_meta = TiffMetadata::from_source(&DataSource::from_path(&tif_path)).unwrap();
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
        let full_meta = TiffMetadata::from_source(&DataSource::from_path(&tif_path)).unwrap();
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

    // --- Overview selection (latency-cliff) tests ---

    /// Mirrors the production FMI radar composite COG: a 4963×7316 base with a
    /// power-of-two overview pyramid whose largest overview is 2481 px wide.
    /// CRS is WGS84 so bbox-to-pixel is an axis-aligned scale, letting these
    /// tests reason purely about the overview-selection arithmetic.
    fn fmi_like_meta() -> TiffMetadata {
        let ov = |ifd: usize, w: u32, h: u32| OverviewLevel {
            ifd_index: ifd,
            width: w,
            height: h,
            tile_width: 256,
            tile_height: 256,
            tiles_across: w.div_ceil(256),
            tiles_down: h.div_ceil(256),
            tile_info: None,
        };
        TiffMetadata {
            width: 4963,
            height: 7316,
            tile_width: 256,
            tile_height: 256,
            tiles_across: 4963u32.div_ceil(256),
            tiles_down: 7316u32.div_ceil(256),
            samples_per_pixel: 1,
            // A valid WGS84 extent (20–30°E, 55–70°N) so bbox→pixel is a clean
            // affine scale; the absolute geography is irrelevant to the test.
            geo_transform: ds_core::geo::GeoTransform {
                origin_x: 20.0,
                origin_y: 70.0,
                pixel_width: 10.0 / 4963.0,
                pixel_height: 15.0 / 7316.0,
                width: 4963,
                height: 7316,
                crs: ds_core::geo::Crs::Wgs84,
            },
            nodata: Some(255.0),
            scale: None,
            offset: None,
            overviews: vec![
                ov(1, 2481, 3658),
                ov(2, 1240, 1829),
                ov(3, 620, 914),
                ov(4, 310, 457),
                ov(5, 155, 228),
            ],
        }
    }

    // Full geographic extent of `fmi_like_meta` (west, south, east, north).
    const FULL: (f64, f64, f64, f64) = (20.0, 55.0, 30.0, 70.0);

    #[test]
    fn select_overview_just_above_cliff_uses_overview_not_full_res() {
        let m = fmi_like_meta();
        // 2650 px is just above the 2481-px largest overview. Under the old
        // strict never-upscale rule this returned None → full-res 36 MP decode.
        let ov = m
            .select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 2650, 2177)
            .expect("must use an overview, not fall back to full resolution");
        assert_eq!(ov.ifd_index, 1, "should pick the 2481-px overview");
    }

    #[test]
    fn select_overview_largest_retina_still_avoids_full_res() {
        let m = fmi_like_meta();
        // 3783 px (a 1.52× upscale off ov0) must still use the overview.
        let ov = m
            .select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 3783, 3108)
            .expect("3783-wide output should still use an overview");
        assert_eq!(ov.ifd_index, 1);
    }

    #[test]
    fn select_overview_full_res_when_output_matches_base() {
        let m = fmi_like_meta();
        // At native resolution no coarser level clears the upscale floor, so
        // full resolution is the correct choice.
        assert!(m
            .select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 4963, 7316)
            .is_none());
    }

    #[test]
    fn select_overview_mid_zoom_unchanged_never_upscales() {
        let m = fmi_like_meta();
        // 1300-wide output is satisfied by ov0 (2481 ≥ 1300) without upscaling,
        // so the bounded-upscale pass must NOT engage: selection is identical to
        // the original strict never-upscale behaviour. Guards against the fix
        // silently retuning every mid-zoom level to a coarser overview.
        let ov = m
            .select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 1300, 1068)
            .unwrap();
        assert_eq!(
            ov.ifd_index, 1,
            "mid-zoom must still pick ov0, not a coarser level"
        );
    }

    #[test]
    fn select_overview_bounded_upscale_caps_at_2x() {
        // Sparse pyramid: biggest overview is a 3× decimation (1654 px), leaving
        // a >2× gap to full resolution. An output needing more than a 2× upscale
        // off that overview must read full resolution, not upscale the overview 3×.
        let ov = |ifd: usize, w: u32, h: u32| OverviewLevel {
            ifd_index: ifd,
            width: w,
            height: h,
            tile_width: 256,
            tile_height: 256,
            tiles_across: w.div_ceil(256),
            tiles_down: h.div_ceil(256),
            tile_info: None,
        };
        let mut m = fmi_like_meta();
        m.overviews = vec![ov(1, 1654, 2439), ov(2, 620, 914)];
        // 3000 px ≤ 2× of 1654 (1.81× upscale) → use the overview.
        assert_eq!(
            m.select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 3000, 1000)
                .unwrap()
                .ifd_index,
            1
        );
        // 3500 px > 2× of 1654 → full resolution, not a 2.1× upscale.
        assert!(m
            .select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 3500, 1000)
            .is_none());
        // Row axis binds independently: width fits (1500 ≤ 1654) but height needs
        // a >2× upscale (min_rows = 2500 > 2439), so it must still fall to full
        // resolution. Guards the `(r1 - r0) >= min_rows` half of Pass 2.
        assert!(
            m.select_overview(FULL.0, FULL.1, FULL.2, FULL.3, 3000, 5000)
                .is_none(),
            "row axis exceeds 2× overview height → must read full resolution"
        );
    }

    /// Like `fmi_like_meta` but in the production CRS (EPSG:3067 / TM35FIN), so
    /// `bbox_to_pixels` exercises the 20-samples-per-edge curvature envelope
    /// rather than a clean axis-aligned scale.
    fn tm35fin_meta() -> TiffMetadata {
        let mut m = fmi_like_meta();
        m.geo_transform = ds_core::geo::GeoTransform {
            origin_x: 150_000.0,
            origin_y: 7_780_000.0,
            pixel_width: 115.0,
            pixel_height: 153.0,
            width: 4963,
            height: 7316,
            crs: ds_core::geo::Crs::TransverseMercator {
                lat0: 0.0,
                lon0: 27.0_f64.to_radians(),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
            },
        };
        m
    }

    #[test]
    fn select_overview_cliff_fix_holds_for_projected_crs() {
        let m = tm35fin_meta();
        // Generous Finland lon/lat bbox (covers the grid). Under TM the pixel
        // counts come from the curvature envelope, not a clean affine scale. A
        // 2650-px retina GetMap must still use the overview (ifd 1), not the
        // 36 MP full resolution — the same cliff fix, on the real projection.
        let ov = m
            .select_overview(18.0, 58.0, 33.0, 71.0, 2650, 2177)
            .expect("projected-CRS retina render must use an overview, not full-res");
        assert_eq!(ov.ifd_index, 1);
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
            geo_transform: ds_core::geo::GeoTransform {
                origin_x: 0.0,
                origin_y: 0.0,
                pixel_width: 1.0,
                pixel_height: 1.0,
                width: 1,
                height: 1,
                crs: ds_core::geo::Crs::Wgs84,
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

    #[test]
    fn tile_concurrency_parse() {
        // Unset / malformed / zero → no override (caller falls back to default).
        assert_eq!(parse_tile_concurrency(None), None);
        assert_eq!(parse_tile_concurrency(Some("")), None);
        assert_eq!(parse_tile_concurrency(Some("abc")), None);
        assert_eq!(parse_tile_concurrency(Some("0")), None);
        // Explicit value ≥ 1 is an applied override (whitespace tolerated).
        assert_eq!(parse_tile_concurrency(Some("1")), Some(1));
        assert_eq!(parse_tile_concurrency(Some(" 32 ")), Some(32));
        // The ceiling itself passes through unchanged, and anything above it is
        // clamped down. Boundaries derive from the constant so the test tracks
        // MAX_TILE_CONCURRENCY instead of silently desyncing from a literal.
        assert_eq!(
            parse_tile_concurrency(Some(&MAX_TILE_CONCURRENCY.to_string())),
            Some(MAX_TILE_CONCURRENCY)
        );
        assert_eq!(
            parse_tile_concurrency(Some(&(MAX_TILE_CONCURRENCY + 1).to_string())),
            Some(MAX_TILE_CONCURRENCY)
        );
        // …and a wildly oversized value clamps too, never reaching the builder.
        assert_eq!(
            parse_tile_concurrency(Some("100000")),
            Some(MAX_TILE_CONCURRENCY)
        );
    }

    fn tiny_meta(width: u32, height: u32, tile: u32) -> TiffMetadata {
        TiffMetadata {
            width,
            height,
            tile_width: tile,
            tile_height: tile,
            tiles_across: width.div_ceil(tile),
            tiles_down: height.div_ceil(tile),
            samples_per_pixel: 1,
            geo_transform: ds_core::geo::GeoTransform {
                origin_x: 0.0,
                origin_y: 0.0,
                pixel_width: 1.0,
                pixel_height: 1.0,
                width,
                height,
                crs: ds_core::geo::Crs::Wgs84,
            },
            nodata: Some(255.0),
            scale: None,
            offset: None,
            overviews: Vec::new(),
        }
    }

    // The local `tiff`-crate decode clips the rightmost tile column of its
    // padding, so its row stride is the clipped width, not `tile_width`.
    #[test]
    fn local_tile_data_width_clips_rightmost_column() {
        // width 6, tile 4 → 2 tile columns; rightmost covers cols 4..7 but the
        // image ends at 6, so its valid width is 2, not 4.
        let m = tiny_meta(6, 4, 4);
        assert_eq!(local_tile_data_width(&m, 0), 4); // interior column → full width
        assert_eq!(local_tile_data_width(&m, 1), 2); // edge column → clipped
    }

    // Regression for #458: copying a CLIPPED rightmost edge tile must use the
    // clipped row stride. With the old `tile_width` stride every row sheared by
    // the padding amount, which surfaced as a "venetian-blind" stripe block once
    // an overview's edge tile carried real data.
    #[test]
    fn copy_edge_tile_uses_clipped_stride_no_shear() {
        // Image 6×4, tiles 4×4. Rightmost tile (tile_col 1) is clipped to 2 wide.
        let m = tiny_meta(6, 4, 4);
        let tile_col = 1u32;
        // Clipped tile buffer: 2 (width) × 4 (height), row-major. Distinct values
        // so any shear would scramble the placement. (Mirrors what the `tiff`
        // crate's `read_chunk` returns for this edge tile.)
        let tile_data: Vec<Option<f64>> = (0..8).map(|v| Some(v as f64)).collect();
        let stride = local_tile_data_width(&m, tile_col);
        assert_eq!(stride, 2);

        // Read the whole rightmost column region: cols 4..6, rows 0..4.
        let (col_start, col_end, row_start, row_end) = (4u32, 6u32, 0u32, 4u32);
        let nx = (col_end - col_start) as usize;
        let mut result = vec![None; nx * (row_end - row_start) as usize];
        copy_tile_to_result(
            &tile_data,
            &mut result,
            tile_col,
            0,
            &m,
            col_start,
            row_start,
            col_end,
            row_end,
            nx,
            stride,
        );
        // The window IS the clipped tile, so result must equal tile_data verbatim.
        assert_eq!(result, tile_data);

        // Contrast: the OLD buggy stride (tile_width) shears — e.g. output
        // (row 1, col 4) would read tile_data[1*4 + 0] = 4 instead of the correct
        // tile_data[1*2 + 0] = 2. Prove the correct stride did NOT do that.
        // (row 1, col 0 of the window = result index `nx`.)
        let out_row1_col4 = result[nx];
        assert_eq!(out_row1_col4, Some(2.0)); // correct; the buggy stride gave 4.0
    }

    // Pins the load-bearing ASSUMPTION behind the #458 fix: the local
    // `tiff`-crate decode path (`decode_chunk_f64` → `read_chunk`) really does
    // return the rightmost tile column CLIPPED to its valid width, NOT padded to
    // `tile_width`. The unit tests above pin the indexing math against a
    // synthetic clipped buffer; this one decodes a REAL committed fixture so a
    // future `tiff`-crate bump that changed to padding edge tiles would fail here
    // (otherwise `local_tile_data_width` would silently over-clip and reintroduce
    // the shear). Uses `testdata/radar` (3249×1750, tile 512 → rightmost column
    // clipped to 177 wide).
    #[test]
    fn local_tiff_decode_returns_clipped_rightmost_tile() {
        let dir = find_test_radar_dir();
        let tif_path = find_first_tif(&dir);
        let source = DataSource::from_path(&tif_path);
        let metadata = TiffMetadata::from_source(&source).expect("parse fixture metadata");

        // The test is only meaningful if the fixture actually has a clipped edge
        // column (width not a multiple of tile_width).
        let last_col = metadata.tiles_across - 1;
        let clipped_w = local_tile_data_width(&metadata, last_col);
        assert!(
            clipped_w < metadata.tile_width as usize,
            "fixture {} has no clipped edge tile (width {} is a multiple of tile {}) — \
             pick a fixture whose width is not a tile multiple",
            tif_path.display(),
            metadata.width,
            metadata.tile_width
        );

        // Decode the TOP rightmost tile (row 0 → full tile_height, only the width
        // is clipped). The decoded buffer must have the CLIPPED row stride: a
        // padded/sheared buffer would be tile_width × tile_height instead.
        let chunk_index = safe_tile_index(0, metadata.tiles_across, last_col).unwrap();
        let mut decoder = source.open_decoder().expect("open decoder");
        let tile = decode_chunk_f64(&mut decoder, chunk_index, &metadata, 0).expect("decode tile");
        assert_eq!(
            tile.len(),
            clipped_w * metadata.tile_height as usize,
            "rightmost edge tile decoded with stride {} (len {}), expected clipped {}×{}; \
             if the tiff crate now PADS edge tiles to tile_width, the #458 fix's local \
             stride must switch back to tile_width",
            tile.len() / metadata.tile_height as usize,
            tile.len(),
            clipped_w,
            metadata.tile_height
        );
    }
}
