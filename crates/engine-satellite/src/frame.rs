//! One scan of one product: a NetCDF-4 file held in memory, decoded block
//! by block on demand.
//!
//! Keeping the compressed file (a 2 km full disk is ~25 MB, versus 59 MB
//! decoded) and inflating only the blocks a render touches measured cheaper
//! than decoding whole grids at ingest (#819 phase 0). A decimated overview
//! is built once at ingest so a zoomed-out render never decodes the full
//! disk.
//!
//! A block is a rectangle of the grid decoded at once: `block_rows` by
//! `block_cols` pixels, row-major, the last row and column of blocks
//! clipped to the grid. A GOES-R block is a strip of chunk rows across the
//! full width; a GK2A AMI block is one of its 1375 × 1375 chunks.

use std::sync::Arc;

use ds_core::cf::{coordinate_scale, crs_from_grid_mapping, CfAttr};
use ds_core::geo::{Crs, GeoTransform};
use hdf5_reader::storage::{BytesStorage, DynStorage};
use netcdf_reader::{NcAttrValue, NcFile, NcOpenOptions, NcSliceInfo, NcSliceInfoElem, NcType};

use crate::ami;

/// Decimation of the overview grid built at ingest. A render whose source
/// window spans at least this many source pixels per output pixel samples
/// the overview instead of decoding strips.
pub(crate) const OVERVIEW_FACTOR: u32 = 4;

/// Largest mosaic grid, per axis in pixels: a 0.5 km AHI or ABI full disk
/// is ~22 000.
const MAX_MOSAIC_PIXELS: u64 = 65_536;

/// Most lattice cells in a mosaic: a Himawari full disk is 10 × 10.
const MAX_MOSAIC_CELLS: u64 = 4_096;

/// Block height when the variable is not chunked.
const DEFAULT_BLOCK_ROWS: u32 = 24;

pub(crate) struct Frame {
    files: Files,
    variable: String,
    /// Whether the variable is stored as `short` (read as `i16`); the
    /// values may still be unsigned (`_Unsigned = "true"`, as GOES-R CMI).
    stored_signed: bool,
    pub packing: Packing,
    /// Full-resolution grid.
    pub gt: GeoTransform,
    /// Rows per decoded block: the variable's chunk height, so a block read
    /// inflates each chunk once; a mosaic's tile height.
    pub block_rows: u32,
    /// Columns per decoded block: the full width, so a block is a strip; a
    /// mosaic's tile width; an AMI file's chunk width.
    pub block_cols: u32,
    /// AMI words → stored centi-kelvin, applied as blocks are read.
    counts: Option<ami::Counts>,
    pub overview: Overview,
    /// Bytes this frame holds (the files plus the overview), for the cache.
    pub weight: u64,
}

/// Where a scan's pixels are.
enum Files {
    /// One file holding the whole grid: blocks are slices of it.
    Single(NcFile),
    /// A mosaic (Himawari ISatSS): one file per block, row-major on the
    /// tile lattice, `None` where the scan has no tile (space off the
    /// disk's corners).
    Tiles(Vec<Option<NcFile>>),
}

/// How to read a product's files.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameOptions<'a> {
    /// How the files describe their grid and values.
    pub format: Format,
    /// The 2-D `(y, x)` field to serve.
    pub variable: &'a str,
    /// Packed validity for a file that declares no `valid_range`: ISatSS
    /// encodes space as ~0 K (packed −1076 with offset 69) with neither a
    /// `_FillValue` nor a range.
    pub valid_fallback: Option<(i32, i32)>,
}

/// How a product's files describe their grid and values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// CF: a grid mapping with 1-D x/y coordinates, packed integers
    /// (GOES-R, Himawari ISatSS).
    Cf,
    /// KMA GK2A AMI L1B: CGMS navigation attributes, counts with quality
    /// flags calibrated per scan ([`ami`]).
    AmiL1b,
}

/// One parsed file of a scan.
struct Part {
    nc: NcFile,
    stored_signed: bool,
    packing: Packing,
    gt: GeoTransform,
    chunk_rows: u32,
    chunk_cols: u32,
    counts: Option<ami::Counts>,
}

/// Every [`OVERVIEW_FACTOR`]-th pixel of the full grid, as stored integers.
pub(crate) struct Overview {
    pub gt: GeoTransform,
    pub raw: Vec<u16>,
}

/// How a stored integer becomes a physical value: CF packing
/// (`scale_factor`, `add_offset`) and masking (`_FillValue`,
/// `valid_range`, both in packed units). Values are carried as `u16`
/// bit patterns; `signed` says whether they are read as signed — a
/// `short` without `_Unsigned = "true"`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Packing {
    signed: bool,
    scale: f64,
    offset: f64,
    fill: Option<u16>,
    valid: Option<(i32, i32)>,
}

impl Packing {
    /// A stored integer that decodes to no value: the fill, else the first
    /// one past the valid range. What a mosaic's missing tile reads as.
    pub fn missing(&self) -> Option<u16> {
        if let Some(fill) = self.fill {
            return Some(fill);
        }
        let (lo, hi) = self.valid?;
        let (min, max) = if self.signed {
            (i16::MIN as i32, i16::MAX as i32)
        } else {
            (0, u16::MAX as i32)
        };
        let outside = if lo > min {
            lo - 1
        } else if hi < max {
            hi + 1
        } else {
            return None;
        };
        Some(if self.signed {
            outside as i16 as u16
        } else {
            outside as u16
        })
    }

    /// The physical value of `raw`, or `None` for fill and out-of-range
    /// values (space off the Earth disk, clear sky in a cloud product).
    pub fn decode(&self, raw: u16) -> Option<f64> {
        if Some(raw) == self.fill {
            return None;
        }
        let v = if self.signed {
            raw as i16 as i32
        } else {
            raw as i32
        };
        if let Some((lo, hi)) = self.valid {
            if v < lo || v > hi {
                return None;
            }
        }
        Some(v as f64 * self.scale + self.offset)
    }
}

impl Frame {
    /// Parse a scan, one file or a mosaic's tiles, and build its overview.
    pub fn open(files: Vec<Vec<u8>>, options: FrameOptions) -> Result<Frame, String> {
        let file_bytes: u64 = files.iter().map(|f| f.len() as u64).sum();
        let mut parts = files
            .into_iter()
            .map(|bytes| Part::open(bytes, options))
            .collect::<Result<Vec<_>, _>>()?;
        let Some(first) = parts.first() else {
            return Err("a scan with no files".to_string());
        };
        // A mosaic's tiles must agree with the first (checked in `mosaic`).
        let (packing, stored_signed) = (first.packing, first.stored_signed);
        let (files, gt, block_rows, block_cols, counts) = if parts.len() == 1 {
            let part = parts.pop().expect("one part");
            let (rows, cols) = (part.chunk_rows, part.chunk_cols);
            (Files::Single(part.nc), part.gt, rows, cols, part.counts)
        } else {
            let (files, gt, rows, cols) = mosaic(parts)?;
            (files, gt, rows, cols, None)
        };
        let file_bytes = file_bytes + counts.as_ref().map_or(0, ami::Counts::weight);
        let placeholder = GeoTransform {
            width: 0,
            height: 0,
            ..gt.clone()
        };
        let mut frame = Frame {
            files,
            variable: options.variable.to_string(),
            stored_signed,
            packing,
            gt,
            block_rows,
            block_cols,
            counts,
            overview: Overview {
                gt: placeholder,
                raw: Vec::new(),
            },
            weight: file_bytes,
        };
        frame.overview = frame.build_overview()?;
        frame.weight += frame.overview.raw.len() as u64 * 2;
        Ok(frame)
    }

    /// Blocks per row of blocks.
    fn blocks_across(&self) -> u32 {
        self.gt.width.div_ceil(self.block_cols)
    }

    /// Pixels in a full block (edge blocks are clipped smaller).
    pub fn block_pixels(&self) -> u64 {
        self.block_rows as u64 * self.block_cols as u64
    }

    pub fn block_count(&self) -> u32 {
        self.gt.height.div_ceil(self.block_rows) * self.blocks_across()
    }

    /// Pixel rows and columns block `index` covers, clipped to the grid.
    fn block_window(&self, index: u32) -> (std::ops::Range<u32>, std::ops::Range<u32>) {
        let (row, col) = (index / self.blocks_across(), index % self.blocks_across());
        let (r0, c0) = (row * self.block_rows, col * self.block_cols);
        (
            r0..(r0 + self.block_rows).min(self.gt.height),
            c0..(c0 + self.block_cols).min(self.gt.width),
        )
    }

    /// The block holding pixel `(row, col)`, and the pixel's offset in it.
    pub fn locate(&self, row: u32, col: u32) -> (u32, usize) {
        let index = (row / self.block_rows) * self.blocks_across() + col / self.block_cols;
        let stride = self.block_window(index).1.len();
        let offset = (row % self.block_rows) as usize * stride + (col % self.block_cols) as usize;
        (index, offset)
    }

    /// Blocks a read of pixel columns `c0..c1`, rows `r0..r1` decodes.
    pub fn blocks_in(&self, c0: u32, r0: u32, c1: u32, r1: u32) -> usize {
        if c1 <= c0 || r1 <= r0 {
            return 0;
        }
        let rows = (r1 - 1) / self.block_rows - r0 / self.block_rows + 1;
        let cols = (c1 - 1) / self.block_cols - c0 / self.block_cols + 1;
        rows as usize * cols as usize
    }

    /// Decode block `index` as stored integers, row-major.
    pub fn read_block(&self, index: u32) -> Result<Arc<[u16]>, String> {
        if index >= self.block_count() {
            return Err(format!("block {index} is past the grid"));
        }
        let (rows, cols) = self.block_window(index);
        let (nc, rows, cols) = match &self.files {
            Files::Single(nc) => (nc, rows, cols),
            Files::Tiles(tiles) => match &tiles[index as usize] {
                // A tile's own grid is the whole block.
                Some(nc) => (nc, 0..rows.len() as u32, 0..cols.len() as u32),
                None => {
                    let missing = self
                        .packing
                        .missing()
                        .ok_or("a mosaic without a fill value has a missing tile")?;
                    return Ok(vec![missing; rows.len() * cols.len()].into());
                }
            },
        };
        let slice = |range: std::ops::Range<u32>| NcSliceInfoElem::Slice {
            start: range.start as u64,
            end: range.end as u64,
            step: 1,
        };
        let selection = NcSliceInfo {
            selections: vec![slice(rows), slice(cols)],
        };
        let read = |e: netcdf_reader::Error| format!("block {index} of '{}': {e}", self.variable);
        let raw: Arc<[u16]> = if self.stored_signed {
            let values = nc
                .read_variable_slice::<i16>(&self.variable, &selection)
                .map_err(read)?;
            values.iter().map(|&v| v as u16).collect()
        } else {
            let values = nc
                .read_variable_slice::<u16>(&self.variable, &selection)
                .map_err(read)?;
            match &self.counts {
                Some(counts) => values.iter().map(|&word| counts.stored(word)).collect(),
                None => values.iter().copied().collect(),
            }
        };
        Ok(raw)
    }

    /// Every [`OVERVIEW_FACTOR`]-th pixel (the centre of each factor²
    /// cell), read one row of blocks at a time so the full grid is never
    /// held decoded at once.
    fn build_overview(&self) -> Result<Overview, String> {
        let factor = OVERVIEW_FACTOR;
        let (nx, ny) = (self.gt.width, self.gt.height);
        let (ox, oy) = (nx.div_ceil(factor), ny.div_ceil(factor));
        let mut raw = vec![0u16; ox as usize * oy as usize];
        let centre = |i: u32, n: u32| (i * factor + factor / 2).min(n - 1);
        let mut blocks: Vec<Option<Arc<[u16]>>> = vec![None; self.block_count() as usize];
        let mut block_row = None;
        for j in 0..oy {
            let row = centre(j, ny);
            // Blocks of rows already passed are never read again.
            if block_row != Some(row / self.block_rows) {
                block_row = Some(row / self.block_rows);
                blocks.iter_mut().for_each(|b| *b = None);
            }
            for i in 0..ox {
                let (index, offset) = self.locate(row, centre(i, nx));
                let block = match &mut blocks[index as usize] {
                    Some(block) => block,
                    slot => slot.insert(self.read_block(index)?),
                };
                raw[j as usize * ox as usize + i as usize] = block[offset];
            }
        }
        let mut overview = Overview {
            gt: GeoTransform {
                pixel_width: self.gt.pixel_width * factor as f64,
                pixel_height: self.gt.pixel_height * factor as f64,
                width: ox,
                height: oy,
                ..self.gt.clone()
            },
            raw,
        };
        if let Some(missing) = self.packing.missing() {
            let centre_x =
                |i: u32| self.gt.origin_x + (centre(i, nx) as f64 + 0.5) * self.gt.pixel_width;
            let centre_y =
                |j: u32| self.gt.origin_y - (centre(j, ny) as f64 + 0.5) * self.gt.pixel_height;
            mask_off_disk(&mut overview, &self.gt.crs, centre_x, centre_y, missing);
        }
        Ok(overview)
    }
}

impl Part {
    /// Parse one file's NetCDF-4 bytes: the field's packing, its grid from
    /// the CF grid mapping and 1-D coordinates, and its chunk height.
    fn open(bytes: Vec<u8>, options: FrameOptions) -> Result<Part, String> {
        if options.format == Format::AmiL1b {
            return Part::open_ami(bytes, options.variable);
        }
        let variable = options.variable;
        let storage: DynStorage = Arc::new(BytesStorage::new(bytes));
        let nc_options = NcOpenOptions {
            // Blocks are cached decoded, so the reader's own chunk cache
            // would only duplicate them.
            chunk_cache_bytes: 0,
            ..NcOpenOptions::default()
        };
        let nc = NcFile::from_storage_with_options(storage.clone(), nc_options)
            .map_err(|e| format!("not a readable NetCDF file: {e}"))?;
        let var = nc
            .variable(variable)
            .map_err(|e| format!("variable '{variable}': {e}"))?;
        let dims = var.dimensions();
        if dims.len() != 2 {
            return Err(format!(
                "variable '{variable}' has {} dimensions, expected (y, x)",
                dims.len()
            ));
        }
        let (y_name, x_name) = (dims[0].name.clone(), dims[1].name.clone());
        let (ny, nx) = (to_u32(dims[0].size)?, to_u32(dims[1].size)?);
        let unsigned_attr = text_attr(var.attribute("_Unsigned").map(|a| &a.value))
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let stored_signed = match var.dtype() {
            NcType::Short => true,
            NcType::UShort => false,
            other => {
                return Err(format!(
                    "variable '{variable}' is {other:?}; only packed short/ushort fields are served"
                ))
            }
        };
        let signed = stored_signed && !unsigned_attr;
        let number = |name: &str| var.attribute(name).and_then(|a| a.value.as_f64());
        // Attributes are stored in the variable's own type: a `short` fill
        // of -1 is the bit pattern 0xFFFF even when `_Unsigned` reads it as
        // 65535.
        let as_raw = |v: f64| {
            if stored_signed {
                v as i16 as u16
            } else {
                v as u16
            }
        };
        let packing = Packing {
            signed,
            scale: number("scale_factor").unwrap_or(1.0),
            offset: number("add_offset").unwrap_or(0.0),
            fill: number("_FillValue").map(as_raw),
            valid: var
                .attribute("valid_range")
                .and_then(|a| a.value.as_f64_vec())
                .filter(|v| v.len() == 2)
                .map(|v| {
                    let bound = |b: f64| {
                        let raw = as_raw(b);
                        if signed {
                            raw as i16 as i32
                        } else {
                            raw as i32
                        }
                    };
                    (bound(v[0]), bound(v[1]))
                })
                .or(options.valid_fallback),
        };

        let mapping = text_attr(var.attribute("grid_mapping").map(|a| &a.value))
            .ok_or_else(|| format!("variable '{variable}' has no grid_mapping"))?;
        let mapping_var = nc
            .variable(mapping.trim())
            .map_err(|e| format!("grid mapping '{mapping}': {e}"))?;
        let attr = |name: &str| mapping_var.attribute(name).and_then(|a| cf_attr(&a.value));
        let crs = crs_from_grid_mapping(|name| {
            attr(name).or_else(|| match name {
                // ISatSS names the earth figure without CF's `_axis`.
                "semi_major_axis" => attr("semi_major"),
                "semi_minor_axis" => attr("semi_minor"),
                _ => None,
            })
        })?;
        let (x0, dx, x_units) = coordinate_axis(&nc, &x_name)?;
        let (y0, dy, _) = coordinate_axis(&nc, &y_name)?;
        let scale = coordinate_scale(&x_units, &crs)
            .ok_or_else(|| format!("coordinate '{x_name}' has unusable units '{x_units}'"))?;
        let gt = GeoTransform::from_cell_centres(
            x0 * scale,
            dx * scale,
            y0 * scale,
            dy * scale,
            nx,
            ny,
            crs,
        )?;

        let chunk_rows = hdf5_reader::Hdf5File::from_storage(storage)
            .ok()
            .and_then(|h5| h5.dataset(variable).ok()?.chunks())
            .and_then(|chunks| chunks.first().copied())
            .filter(|&rows| rows > 0)
            .unwrap_or(DEFAULT_BLOCK_ROWS)
            .min(ny);
        Ok(Part {
            nc,
            stored_signed,
            packing,
            gt,
            chunk_rows,
            chunk_cols: nx,
            counts: None,
        })
    }

    /// Parse a GK2A AMI L1B file ([`ami`]): its grid from the CGMS
    /// navigation, its counts calibrated to centi-kelvin, and its chunk as
    /// the block (1375 × 1375 in a 2 km full disk: a full-width strip of
    /// them would decode 15 MB).
    fn open_ami(bytes: Vec<u8>, variable: &str) -> Result<Part, String> {
        let storage: DynStorage = Arc::new(BytesStorage::new(bytes));
        let nc_options = NcOpenOptions {
            chunk_cache_bytes: 0,
            ..NcOpenOptions::default()
        };
        let nc = NcFile::from_storage_with_options(storage.clone(), nc_options)
            .map_err(|e| format!("not a readable NetCDF file: {e}"))?;
        let dtype = nc
            .variable(variable)
            .map_err(|e| format!("variable '{variable}': {e}"))?
            .dtype();
        if *dtype != NcType::UShort {
            return Err(format!("AMI field '{variable}' is {dtype:?}, not ushort"));
        }
        let (gt, counts) = ami::open(&nc, variable)?;
        let chunks = hdf5_reader::Hdf5File::from_storage(storage)
            .ok()
            .and_then(|h5| h5.dataset(variable).ok()?.chunks());
        let chunk = |axis: usize, size: u32| {
            chunks
                .as_ref()
                .and_then(|c| c.get(axis).copied())
                .filter(|&n| n > 0)
                .unwrap_or(size)
                .min(size)
        };
        Ok(Part {
            nc,
            stored_signed: false,
            packing: Packing {
                signed: false,
                scale: ami::KELVIN_SCALE,
                offset: 0.0,
                fill: Some(ami::MISSING),
                valid: None,
            },
            chunk_rows: chunk(0, gt.height),
            chunk_cols: chunk(1, gt.width),
            gt,
            counts: Some(counts),
        })
    }
}

/// Place a scan's tiles on one lattice: equal tiles in one CRS, pixel size
/// and packing, whose offsets (from their coordinates) are whole tiles.
/// The grid is the lattice's bounding box; a lattice cell with no tile
/// reads as missing.
fn mosaic(parts: Vec<Part>) -> Result<(Files, GeoTransform, u32, u32), String> {
    if parts.iter().any(|p| p.counts.is_some()) {
        return Err("an AMI scan is one file, not tiles".to_string());
    }
    let first = &parts[0];
    let template = first.gt.clone();
    let (w, h) = (first.gt.width, first.gt.height);
    let (pw, ph) = (first.gt.pixel_width, first.gt.pixel_height);
    let same = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs());
    for part in &parts[1..] {
        if part.packing != first.packing || part.stored_signed != first.stored_signed {
            return Err("the tiles of a scan are packed differently".to_string());
        }
        if part.gt.crs != first.gt.crs
            || !same(part.gt.pixel_width, pw)
            || !same(part.gt.pixel_height, ph)
            || (part.gt.width, part.gt.height) != (w, h)
        {
            return Err("the tiles of a scan are not one grid of equal tiles".to_string());
        }
    }
    let x0 = parts
        .iter()
        .map(|p| p.gt.origin_x)
        .fold(f64::INFINITY, f64::min);
    let y0 = parts
        .iter()
        .map(|p| p.gt.origin_y)
        .fold(f64::NEG_INFINITY, f64::max);
    // A tile's lattice cell: its offset must be a whole number of pixels
    // (to 1e-3), that number a whole number of tiles, and the grid within
    // `MAX_MOSAIC_PIXELS` — so a tile with corrupt coordinates fails its
    // scan instead of sizing an enormous lattice.
    let cell = |offset: f64, size: f64, tile: u32| -> Result<u32, String> {
        let pixels = offset / size;
        let whole = pixels.round();
        if !((pixels - whole).abs() <= 1e-3 && (0.0..=MAX_MOSAIC_PIXELS as f64).contains(&whole)) {
            return Err(format!(
                "the tiles of a scan are not on one lattice within {MAX_MOSAIC_PIXELS} pixels"
            ));
        }
        let whole = whole as u64;
        if !whole.is_multiple_of(tile as u64) || whole + tile as u64 > MAX_MOSAIC_PIXELS {
            return Err(format!(
                "the tiles of a scan are not on one lattice within {MAX_MOSAIC_PIXELS} pixels"
            ));
        }
        Ok((whole / tile as u64) as u32)
    };
    let placed = parts
        .into_iter()
        .map(|part| {
            let col = cell(part.gt.origin_x - x0, pw, w)?;
            let row = cell(y0 - part.gt.origin_y, ph, h)?;
            Ok((row, col, part.nc))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let across = placed.iter().map(|(_, col, _)| col + 1).max().unwrap_or(1);
    let down = placed.iter().map(|(row, _, _)| row + 1).max().unwrap_or(1);
    if across as u64 * down as u64 > MAX_MOSAIC_CELLS {
        return Err(format!(
            "the tiles of a scan span a lattice of {across} × {down}; at most {MAX_MOSAIC_CELLS} cells"
        ));
    }
    let mut tiles: Vec<Option<NcFile>> = (0..across * down).map(|_| None).collect();
    for (row, col, nc) in placed {
        let slot = &mut tiles[(row * across + col) as usize];
        if slot.is_some() {
            return Err("two tiles of a scan cover the same place".to_string());
        }
        *slot = Some(nc);
    }
    let gt = GeoTransform {
        origin_x: x0,
        origin_y: y0,
        width: across * w,
        height: down * h,
        ..template
    };
    Ok((Files::Tiles(tiles), gt, h, w))
}

/// Blank the overview cells whose centre pixel the satellite cannot see,
/// given each column's and row's centre in scan-angle metres.
///
/// ISatSS leaves the stray-light halo just off the limb unmasked
/// (~110–170 K). A full-resolution read only ever lands on the disk, but an
/// overview cell straddling the limb keeps its centre pixel, which may lie
/// in space. The visible disk is convex and symmetric about the
/// sub-satellite meridian (x = 0) in scan-angle space, so each row's
/// visible cells are one interval around the column nearest x = 0: two
/// bisections per row rather than a projection per cell.
fn mask_off_disk(
    overview: &mut Overview,
    crs: &Crs,
    x: impl Fn(u32) -> f64,
    y: impl Fn(u32) -> f64,
    missing: u16,
) {
    if !matches!(crs, Crs::Geostationary { .. }) {
        return;
    }
    let (ox, oy) = (overview.gt.width, overview.gt.height);
    let Some(mid) = (0..ox).min_by(|&a, &b| x(a).abs().total_cmp(&x(b).abs())) else {
        return;
    };
    for j in 0..oy {
        let yj = y(j);
        let seen = |i: u32| crs.inverse(x(i), yj).is_some();
        let row = &mut overview.raw[(j * ox) as usize..((j + 1) * ox) as usize];
        if !seen(mid) {
            row.fill(missing);
            continue;
        }
        // First seen column: `lo` unseen, `hi` seen.
        let left = if seen(0) {
            0
        } else {
            let (mut lo, mut hi) = (0, mid);
            while hi - lo > 1 {
                let m = (lo + hi) / 2;
                if seen(m) {
                    hi = m
                } else {
                    lo = m
                }
            }
            hi
        };
        // Last seen column: `lo` seen, `hi` unseen.
        let right = if seen(ox - 1) {
            ox - 1
        } else {
            let (mut lo, mut hi) = (mid, ox - 1);
            while hi - lo > 1 {
                let m = (lo + hi) / 2;
                if seen(m) {
                    lo = m
                } else {
                    hi = m
                }
            }
            lo
        };
        row[..left as usize].fill(missing);
        row[right as usize + 1..].fill(missing);
    }
}

/// First value, step and units of the 1-D coordinate variable `name`, with
/// its CF packing applied (GOES-R stores x/y as packed `short`). The axis
/// must be regular.
fn coordinate_axis(nc: &NcFile, name: &str) -> Result<(f64, f64, String), String> {
    let var = nc
        .variable(name)
        .map_err(|e| format!("coordinate '{name}': {e}"))?;
    let number = |attr: &str| var.attribute(attr).and_then(|a| a.value.as_f64());
    let (scale, offset) = (
        number("scale_factor").unwrap_or(1.0),
        number("add_offset").unwrap_or(0.0),
    );
    let units = text_attr(var.attribute("units").map(|a| &a.value)).unwrap_or_default();
    let values: Vec<f64> = nc
        .read_variable_as_f64(name)
        .map_err(|e| format!("coordinate '{name}': {e}"))?
        .iter()
        .map(|v| v * scale + offset)
        .collect();
    if values.len() < 2 {
        return Err(format!("coordinate '{name}' has fewer than two values"));
    }
    let step = values[1] - values[0];
    let irregular = values
        .iter()
        .enumerate()
        .any(|(i, v)| (v - (values[0] + i as f64 * step)).abs() > step.abs() * 1e-3);
    if step == 0.0 || irregular {
        return Err(format!("coordinate '{name}' is not a regular axis"));
    }
    Ok((values[0], step, units))
}

fn text_attr(value: Option<&NcAttrValue>) -> Option<String> {
    value.and_then(NcAttrValue::as_string)
}

fn cf_attr(value: &NcAttrValue) -> Option<CfAttr> {
    match value {
        NcAttrValue::Chars(_) | NcAttrValue::Strings(_) => value.as_string().map(CfAttr::Text),
        _ => value.as_f64().map(CfAttr::Number),
    }
}

fn to_u32(size: u64) -> Result<u32, String> {
    u32::try_from(size).map_err(|_| format!("dimension of {size} is too large"))
}

#[cfg(test)]
mod tests {
    use super::{mosaic, Frame, FrameOptions, Packing, Part, OVERVIEW_FACTOR};

    /// GOES-R CMI: stored `short`, `_Unsigned = "true"`, fill `-1s`,
    /// `valid_range = 0s, 4095s`, 12-bit brightness temperatures.
    #[test]
    fn packing_masks_fill_and_range_and_scales() {
        let cmi = Packing {
            signed: false,
            scale: 0.06145332,
            offset: 89.62,
            fill: Some(0xFFFF),
            valid: Some((0, 4095)),
        };
        assert_eq!(cmi.decode(0xFFFF), None);
        assert_eq!(cmi.decode(4096), None);
        assert!((cmi.decode(2000).unwrap() - (2000.0 * 0.06145332 + 89.62)).abs() < 1e-9);
        // A signed field reads the same bits as negative values.
        let signed = Packing {
            signed: true,
            scale: 1.0,
            offset: 0.0,
            fill: None,
            valid: None,
        };
        assert_eq!(signed.decode(0xFFFE), Some(-2.0));
    }

    /// Blocks narrower than the grid, clipped at its right and bottom
    /// edges, read the same pixels and build the same overview as the
    /// full-width strips of the real C13 crop (320 × 240, chunks 24 rows).
    #[test]
    fn blocks_of_any_shape_read_the_same_pixels() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../testdata/goes19-abi/\
             OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc",
        );
        let bytes = std::fs::read(path).unwrap();
        let cmi = FrameOptions {
            format: super::Format::Cf,
            variable: "CMI",
            valid_fallback: None,
        };
        let strips = Frame::open(vec![bytes.clone()], cmi).unwrap();
        assert_eq!((strips.block_cols, strips.gt.width), (320, 320));
        let mut blocks = Frame::open(vec![bytes], cmi).unwrap();
        // 320 = 3 × 100 + 20 and 240 = 34 × 7 + 2: both edges clip.
        (blocks.block_rows, blocks.block_cols) = (7, 100);
        assert_eq!(blocks.block_count(), 35 * 4);
        assert_eq!(blocks.blocks_in(0, 0, 320, 240), 35 * 4);
        assert_eq!(blocks.blocks_in(99, 6, 101, 8), 4);
        assert_eq!(blocks.blocks_in(300, 238, 320, 240), 1);

        let mut decoded = std::collections::HashMap::new();
        let mut read = |frame: &Frame, row: u32, col: u32| {
            let (index, offset) = frame.locate(row, col);
            let block = decoded
                .entry((frame.block_cols, index))
                .or_insert_with(|| frame.read_block(index).unwrap());
            block[offset]
        };
        for row in (0..240).step_by(3).chain([239]) {
            for col in (0..320).step_by(7).chain([99, 100, 299, 300, 319]) {
                assert_eq!(
                    read(&strips, row, col),
                    read(&blocks, row, col),
                    "pixel ({row}, {col})"
                );
            }
        }
        assert_eq!(blocks.build_overview().unwrap().raw, strips.overview.raw);
    }

    /// Three real ISatSS tiles around a lattice corner on the limb, the
    /// fourth cell (space) having no tile: a 2 × 2 mosaic of 64-pixel
    /// blocks, whatever the input order, each block its tile, the missing
    /// one no data, and the overview blank exactly where a cell's centre is
    /// off the disk, masking the halo the files leave there.
    #[test]
    fn isatss_tiles_form_a_mosaic() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/himawari9-isatss");
        let read = |tile: &str| {
            let name = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.to_string_lossy().contains(&format!("-{tile}_")))
                .unwrap();
            std::fs::read(name).unwrap()
        };
        let options = FrameOptions {
            format: super::Format::Cf,
            variable: "Sectorized_CMI",
            valid_fallback: Some((0, i16::MAX as i32)),
        };
        let mosaic = Frame::open(vec![read("T008"), read("T001"), read("T007")], options).unwrap();
        let gt = &mosaic.gt;
        assert_eq!((gt.width, gt.height), (128, 128));
        assert_eq!((mosaic.block_rows, mosaic.block_cols), (64, 64));
        assert!(mosaic
            .read_block(0)
            .unwrap()
            .iter()
            .all(|&raw| mosaic.packing.decode(raw).is_none()));
        for (block, tile) in [(1, "T001"), (2, "T007"), (3, "T008")] {
            let alone = Frame::open(vec![read(tile)], options).unwrap();
            assert_eq!(alone.block_count(), 1);
            assert_eq!(
                mosaic.read_block(block).unwrap(),
                alone.read_block(0).unwrap(),
                "{tile}"
            );
            let (dx, dy) = ((block % 2) as f64 * 64.0, (block / 2) as f64 * 64.0);
            assert!((alone.gt.origin_x - (gt.origin_x + dx * gt.pixel_width)).abs() < 1e-3);
            assert!((alone.gt.origin_y - (gt.origin_y - dy * gt.pixel_height)).abs() < 1e-3);
        }

        let overview = &mosaic.overview;
        let (mut kept, mut masked) = (0, 0);
        for j in 0..overview.gt.height {
            for i in 0..overview.gt.width {
                let (col, row) = (i * OVERVIEW_FACTOR + 2, j * OVERVIEW_FACTOR + 2);
                let x = gt.origin_x + (col as f64 + 0.5) * gt.pixel_width;
                let y = gt.origin_y - (row as f64 + 0.5) * gt.pixel_height;
                let (index, offset) = mosaic.locate(row, col);
                let full = mosaic
                    .packing
                    .decode(mosaic.read_block(index).unwrap()[offset]);
                let value = mosaic
                    .packing
                    .decode(overview.raw[(j * overview.gt.width + i) as usize]);
                if gt.crs.inverse(x, y).is_some() {
                    assert_eq!(value, full, "overview ({i}, {j})");
                    kept += usize::from(value.is_some());
                } else {
                    assert_eq!(value, None, "overview ({i}, {j}) is off the disk");
                    masked += usize::from(full.is_some());
                }
            }
        }
        assert!(kept > 0 && masked > 0, "kept {kept}, masked {masked}");
    }

    /// A GK2A AMI file's block is its chunk (32 × 48 in the fixture, so
    /// the last column of blocks is clipped to 16), its words are served as
    /// calibrated centi-kelvin, and the overview holds the same values.
    #[test]
    fn ami_blocks_are_chunks_of_calibrated_counts() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/gk2a-ami/gk2a_ami_le1b_ir105_fd020ge_202609281200.nc");
        let options = FrameOptions {
            format: super::Format::AmiL1b,
            variable: "image_pixel_values",
            valid_fallback: None,
        };
        let frame = Frame::open(vec![std::fs::read(path).unwrap()], options).unwrap();
        assert_eq!((frame.gt.width, frame.gt.height), (160, 96));
        assert_eq!((frame.block_rows, frame.block_cols), (32, 48));
        assert_eq!(frame.block_count(), 12);
        assert_eq!(frame.read_block(11).unwrap().len(), 32 * 16);
        // Pixel (0, 0) is count 3286: 294.396639 K (independent Python
        // calibration of the file's coefficients).
        let (index, offset) = frame.locate(0, 0);
        let first = frame
            .packing
            .decode(frame.read_block(index).unwrap()[offset]);
        assert!(
            (first.unwrap() - 294.396_639).abs() <= 0.005 + 1e-9,
            "{first:?}"
        );
        let o = &frame.overview;
        for j in 0..o.gt.height {
            for i in 0..o.gt.width {
                let (row, col) = (
                    (j * OVERVIEW_FACTOR + 2).min(95),
                    (i * OVERVIEW_FACTOR + 2).min(159),
                );
                let (index, offset) = frame.locate(row, col);
                let full = frame.read_block(index).unwrap()[offset];
                assert_eq!(o.raw[(j * o.gt.width + i) as usize], full, "({i}, {j})");
                assert!(frame.packing.decode(full).is_some());
            }
        }
        // AMI words come one file per scan, never as a mosaic's tiles.
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/gk2a-ami/gk2a_ami_le1b_ir105_fd020ge_202609281200.nc"),
        )
        .unwrap();
        assert!(Frame::open(vec![bytes.clone(), bytes], options).is_err());
    }

    /// A tile with corrupt coordinates fails its scan: off the lattice, or
    /// so far away that the lattice would be enormous, never an allocation
    /// sized by the bad offset.
    #[test]
    fn mosaic_rejects_tiles_off_one_bounded_lattice() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/himawari9-isatss");
        let options = FrameOptions {
            format: super::Format::Cf,
            variable: "Sectorized_CMI",
            valid_fallback: Some((0, i16::MAX as i32)),
        };
        let parts = || -> Vec<Part> {
            let mut names: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "nc"))
                .collect();
            names.sort();
            names
                .into_iter()
                .map(|p| Part::open(std::fs::read(p).unwrap(), options).unwrap())
                .collect()
        };
        assert!(mosaic(parts()).is_ok());
        // (x, y) shifts of one tile, in pixels (tiles are 64 × 64).
        for (dx, dy) in [
            (10.0, 0.0),          // a fraction of a tile: off the lattice
            (6400.0, 6400.0),     // 100 × 100 tiles: over MAX_MOSAIC_CELLS
            (1100.0 * 64.0, 0.0), // 1100 tiles wide: over MAX_MOSAIC_PIXELS
            (1.0e12, 0.0),        // far past any grid
        ] {
            let mut bad = parts();
            let (pw, ph) = (bad[0].gt.pixel_width, bad[0].gt.pixel_height);
            bad[0].gt.origin_x += dx * pw;
            bad[0].gt.origin_y -= dy * ph;
            let err = mosaic(bad)
                .err()
                .unwrap_or_else(|| panic!("shift ({dx}, {dy}) accepted"));
            assert!(err.contains("lattice"), "shift ({dx}, {dy}): {err}");
        }
    }
}
