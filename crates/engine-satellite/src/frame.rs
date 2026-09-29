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
//! clipped to the grid. A single file's block is one chunk: a GOES-R chunk
//! (so a block) is a strip of rows across the full width, a GMGSI chunk a
//! quarter of its width.

use std::sync::Arc;

use ds_core::cf::{coordinate_scale, crs_from_grid_mapping, CfAttr};
use ds_core::geo::{crs84_extent, Crs, GeoTransform};
use hdf5_reader::storage::{BytesStorage, DynStorage};
use netcdf_reader::{NcAttrValue, NcFile, NcOpenOptions, NcSliceInfo, NcSliceInfoElem, NcType};

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

/// The stored integer a `float` field's missing value is carried as.
const FLOAT_MISSING: u16 = u16::MAX;

pub(crate) struct Frame {
    files: Files,
    variable: String,
    stored: Stored,
    /// Leading dimensions before `(y, x)`, each of size 1 (GMGSI's `time`),
    /// read at index 0.
    leading: usize,
    pub packing: Packing,
    /// Full-resolution grid.
    pub gt: GeoTransform,
    /// Columns per 360° of longitude when the grid wraps around the globe
    /// (GMGSI): its columns are then read modulo this ([`Frame::pixel`]).
    pub col_period: Option<f64>,
    /// Rows per decoded block: the variable's chunk height, so a block read
    /// inflates each chunk once; a mosaic's tile height.
    pub block_rows: u32,
    /// Columns per decoded block: the variable's chunk width (a GOES-R
    /// chunk spans the full width, so its block is a strip); a mosaic's
    /// tile width.
    pub block_cols: u32,
    pub overview: Overview,
    /// Bytes this frame holds (the files plus the overview), for the cache.
    pub weight: u64,
}

/// How a field's values are stored, and so read.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stored {
    /// `short`, read as `i16`; the values may still be unsigned
    /// (`_Unsigned = "true"`, as GOES-R CMI).
    Short,
    /// `ushort`, read as `u16`.
    UShort,
    /// `float` holding whole-number counts, as GMGSI's 0–255 display
    /// counts: each is carried as its `u16`, its `_FillValue`, NaN and any
    /// value outside `valid_range` as [`FLOAT_MISSING`]. A value that is not
    /// a whole count in `0..FLOAT_MISSING` fails the block, and so the scan
    /// (its overview reads every block at ingest).
    FloatCounts {
        fill: Option<f32>,
        valid: Option<(f32, f32)>,
    },
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
    /// The 2-D `(y, x)` field to serve.
    pub variable: &'a str,
    /// Packed validity for a file that declares no `valid_range`: ISatSS
    /// encodes space as ~0 K (packed −1076 with offset 69) with neither a
    /// `_FillValue` nor a range.
    pub valid_fallback: Option<(i32, i32)>,
}

/// One parsed file of a scan.
struct Part {
    nc: NcFile,
    stored: Stored,
    leading: usize,
    packing: Packing,
    gt: GeoTransform,
    col_period: Option<f64>,
    chunk_rows: u32,
    chunk_cols: u32,
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
        let (packing, stored, leading) = (first.packing, first.stored, first.leading);
        let (files, gt, col_period, block_rows, block_cols) = if parts.len() == 1 {
            let part = parts.pop().expect("one part");
            let (rows, cols) = (part.chunk_rows, part.chunk_cols);
            (Files::Single(part.nc), part.gt, part.col_period, rows, cols)
        } else {
            let (files, gt, rows, cols) = mosaic(parts)?;
            (files, gt, None, rows, cols)
        };
        let placeholder = GeoTransform {
            width: 0,
            height: 0,
            ..gt.clone()
        };
        let mut frame = Frame {
            files,
            variable: options.variable.to_string(),
            stored,
            leading,
            packing,
            gt,
            col_period,
            block_rows,
            block_cols,
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
        let mut selections = vec![NcSliceInfoElem::Index(0); self.leading];
        selections.extend([slice(rows), slice(cols)]);
        let selection = NcSliceInfo { selections };
        let read = |e: netcdf_reader::Error| format!("block {index} of '{}': {e}", self.variable);
        let raw: Arc<[u16]> = match self.stored {
            Stored::Short => {
                let values = nc
                    .read_variable_slice::<i16>(&self.variable, &selection)
                    .map_err(read)?;
                values.iter().map(|&v| v as u16).collect()
            }
            Stored::UShort => {
                let values = nc
                    .read_variable_slice::<u16>(&self.variable, &selection)
                    .map_err(read)?;
                values.iter().copied().collect()
            }
            Stored::FloatCounts { fill, valid } => {
                let values = nc
                    .read_variable_slice::<f32>(&self.variable, &selection)
                    .map_err(read)?;
                values
                    .iter()
                    .map(|&v| float_count(v, fill, valid))
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("block {index} of '{}': {e}", self.variable))?
            }
        };
        Ok(raw)
    }

    /// Columns per 360° on the full grid or the overview, for a global
    /// grid: what a `ProjectionGrid` over that level unwraps its columns by.
    pub fn period(&self, overview: bool) -> Option<f64> {
        let factor = if overview {
            OVERVIEW_FACTOR as f64
        } else {
            1.0
        };
        self.col_period.map(|p| p / factor)
    }

    /// The pixel `(col, row)` of the full grid (or the overview) holding
    /// the fractional source position `(c, r)`, such as a
    /// `ProjectionGrid` samples; `None` off the grid, or where the mapping
    /// is undefined (a geostationary point the satellite cannot see).
    ///
    /// A global grid's columns wrap modulo its period. Where its columns
    /// fall short of a whole turn (GMGSI's by 0.38 px), a position in the
    /// sliver between the last column and the first reads the nearer of
    /// the two: nearest-neighbour across the seam.
    ///
    /// Both levels resolve the position on the full grid, and the overview
    /// then reads the cell holding that pixel. An overview cell spans
    /// [`OVERVIEW_FACTOR`] pixels, so its last column or row can reach past
    /// the grid's edge: GMGSI's 4999 columns make 1250 cells, 5000 pixels,
    /// more than its 4999.378-pixel turn. Resolved at the overview's own
    /// scale, the seam's sliver would fall inside that last cell, and the
    /// half of it nearer the first column would read the last one.
    ///
    /// Everywhere else the overview's own extent bounds it: a position in
    /// the outer slice of its last row, or of its last column on an axis
    /// that does not wrap, reads that cell (whose sample the overview
    /// clamps to the grid's last pixel), and one past the extent reads
    /// nothing.
    pub fn pixel(&self, overview: bool, c: f64, r: f64) -> Option<(u32, u32)> {
        if !overview {
            return self.full_pixel(c, r);
        }
        let (o, factor) = (&self.overview.gt, OVERVIEW_FACTOR as f64);
        let inside = |v: f64, n: u32| v.is_finite() && v >= 0.0 && v < n as f64;
        // The grid's last pixel, for a position past it in the last cell.
        let last = |n: u32| (n - 1) as f64;
        if !inside(r, o.height) {
            return None;
        }
        let r = (r * factor).min(last(self.gt.height));
        let c = if self.col_period.is_some() {
            c * factor
        } else if inside(c, o.width) {
            (c * factor).min(last(self.gt.width))
        } else {
            return None;
        };
        let (col, row) = self.full_pixel(c, r)?;
        Some((col / OVERVIEW_FACTOR, row / OVERVIEW_FACTOR))
    }

    /// [`Self::pixel`] on the full grid.
    fn full_pixel(&self, c: f64, r: f64) -> Option<(u32, u32)> {
        let gt = &self.gt;
        if !(r.is_finite() && r >= 0.0 && r < gt.height as f64) {
            return None;
        }
        let width = gt.width as f64;
        let c = match self.col_period {
            Some(period) => {
                let c = c.rem_euclid(period);
                if c >= width {
                    if c - width < (period - width) / 2.0 {
                        width - 1.0
                    } else {
                        0.0
                    }
                } else {
                    c
                }
            }
            None => c,
        };
        (c.is_finite() && c >= 0.0 && c < width).then_some((c as u32, r as u32))
    }

    /// The full-grid pixel windows `[c0, r0, c1, r1]` (exclusive ends) a
    /// CRS84 bbox covers: none when it misses the grid, one on a regional
    /// grid, and on a global grid two when it crosses the grid's seam (the
    /// bbox's longitudes read modulo 360°; one wider than a turn covers
    /// every column).
    pub fn windows(&self, bbox: [f64; 4]) -> Vec<[u32; 4]> {
        let [west, south, east, north] = bbox;
        let Some(period) = self.col_period else {
            return self
                .gt
                .bbox_to_pixels(west, south, east, north)
                .map(|(c0, r0, c1, r1)| vec![[c0, r0, c1, r1]])
                .unwrap_or_default();
        };
        let gt = &self.gt;
        // A global grid's CRS is separable (x from longitude alone, y from
        // latitude alone): rows from the latitudes, columns from the
        // longitudes.
        let row = |lat: f64| (gt.origin_y - gt.crs.forward(0.0, lat).1) / gt.pixel_height;
        let col = |lon: f64| (gt.crs.forward(lon, 0.0).0 - gt.origin_x) / gt.pixel_width;
        let (r0, r1) = (
            row(north).floor().max(0.0),
            row(south).ceil().min(gt.height as f64),
        );
        let span = if east >= west {
            east - west
        } else {
            east + 360.0 - west
        };
        let (width, start) = (gt.width as f64, col(west));
        let (a, b) = (
            start.rem_euclid(period),
            start.rem_euclid(period) + col(west + span) - start,
        );
        if !(r0 < r1 && a.is_finite() && b.is_finite()) {
            return Vec::new();
        }
        let (r0, r1) = (r0 as u32, r1 as u32);
        if b - a >= period {
            return vec![[0, r0, gt.width, r1]];
        }
        // From `a` to the end of the turn (a start past the last column,
        // in the seam's sliver, reads the last column) …
        let c0 = a.floor().min(width - 1.0);
        let c1 = b.min(width).ceil().max(c0 + 1.0);
        let mut windows = vec![[c0 as u32, r0, c1 as u32, r1]];
        // … and on from the first column into the next turn.
        if b > period {
            windows.push([0, r0, (b - period).ceil().clamp(1.0, width) as u32, r1]);
        }
        windows
    }

    /// The CRS84 extent of the grid: every longitude for a global grid,
    /// with west > east when a regional grid crosses the antimeridian.
    pub fn extent(&self) -> Option<[f64; 4]> {
        if self.col_period.is_none() {
            return crs84_extent(self.gt.bbox());
        }
        let gt = &self.gt;
        let lat = |y: f64| gt.crs.inverse(gt.origin_x, y).map(|(_, lat)| lat);
        let north = lat(gt.origin_y)?;
        let south = lat(gt.origin_y - gt.height as f64 * gt.pixel_height)?;
        crs84_extent([-180.0, south, 180.0, north])
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
    /// Parse one file's NetCDF-4 bytes: the field's packing, its grid (from
    /// a CF grid mapping and 1-D coordinates, or 2-D lat/lon arrays) and
    /// its chunk shape.
    fn open(bytes: Vec<u8>, options: FrameOptions) -> Result<Part, String> {
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
        // (y, x), after leading dimensions of size 1 (GMGSI's `time`).
        let leading = dims.len().saturating_sub(2);
        if dims.len() < 2 || dims[..leading].iter().any(|d| d.size != 1) {
            return Err(format!(
                "variable '{variable}' has dimensions {:?}, expected (y, x) after any of size 1",
                dims.iter().map(|d| (&d.name, d.size)).collect::<Vec<_>>()
            ));
        }
        let (y_dim, x_dim) = (&dims[leading], &dims[leading + 1]);
        let (y_name, x_name) = (y_dim.name.clone(), x_dim.name.clone());
        let (ny, nx) = (to_u32(y_dim.size)?, to_u32(x_dim.size)?);
        let unsigned_attr = text_attr(var.attribute("_Unsigned").map(|a| &a.value))
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let number = |name: &str| var.attribute(name).and_then(|a| a.value.as_f64());
        let valid_range = var
            .attribute("valid_range")
            .and_then(|a| a.value.as_f64_vec())
            .filter(|v| v.len() == 2);
        let stored = match var.dtype() {
            NcType::Short => Stored::Short,
            NcType::UShort => Stored::UShort,
            NcType::Float => Stored::FloatCounts {
                fill: number("_FillValue").map(|v| v as f32),
                valid: valid_range.as_ref().map(|v| (v[0] as f32, v[1] as f32)),
            },
            other => {
                return Err(format!(
                    "variable '{variable}' is {other:?}; only packed short/ushort fields and \
                     float counts are served"
                ))
            }
        };
        let stored_signed = stored == Stored::Short;
        let signed = stored_signed && !unsigned_attr;
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
        let packing = match stored {
            // Carried as whole counts; the missing ones as FLOAT_MISSING.
            Stored::FloatCounts { .. } => Packing {
                signed: false,
                scale: number("scale_factor").unwrap_or(1.0),
                offset: number("add_offset").unwrap_or(0.0),
                fill: Some(FLOAT_MISSING),
                valid: None,
            },
            Stored::Short | Stored::UShort => Packing {
                signed,
                scale: number("scale_factor").unwrap_or(1.0),
                offset: number("add_offset").unwrap_or(0.0),
                fill: number("_FillValue").map(as_raw),
                valid: valid_range
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
            },
        };

        // A CF grid mapping with 1-D x/y coordinates (GOES-R, ISatSS), or
        // 2-D lat/lon arrays forming a global Mercator grid (GMGSI).
        let (gt, col_period) = match text_attr(var.attribute("grid_mapping").map(|a| &a.value)) {
            Some(mapping) => (cf_grid(&nc, &mapping, &x_name, &y_name, nx, ny)?, None),
            None => {
                let grid = crate::mercator::from_lat_lon(&nc, var, &y_name, &x_name)
                    .map_err(|e| format!("variable '{variable}' has no grid_mapping, and {e}"))?;
                (grid.gt, Some(grid.col_period))
            }
        };

        // The chunk's extent over (y, x): a block inflates each chunk once.
        let chunks = hdf5_reader::Hdf5File::from_storage(storage)
            .ok()
            .and_then(|h5| h5.dataset(variable).ok()?.chunks());
        let chunk = |axis: usize| {
            chunks
                .as_ref()
                .and_then(|c| c.get(leading + axis).copied())
                .filter(|&n| n > 0)
        };
        let chunk_rows = chunk(0).unwrap_or(DEFAULT_BLOCK_ROWS).min(ny);
        let chunk_cols = chunk(1).unwrap_or(nx).min(nx);
        Ok(Part {
            nc,
            stored,
            leading,
            packing,
            gt,
            col_period,
            chunk_rows,
            chunk_cols,
        })
    }
}

/// The grid of a field with a CF grid mapping and regular 1-D x/y
/// coordinates.
fn cf_grid(
    nc: &NcFile,
    mapping: &str,
    x_name: &str,
    y_name: &str,
    nx: u32,
    ny: u32,
) -> Result<GeoTransform, String> {
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
    let (x0, dx, x_units) = coordinate_axis(nc, x_name)?;
    let (y0, dy, _) = coordinate_axis(nc, y_name)?;
    let scale = coordinate_scale(&x_units, &crs)
        .ok_or_else(|| format!("coordinate '{x_name}' has unusable units '{x_units}'"))?;
    GeoTransform::from_cell_centres(x0 * scale, dx * scale, y0 * scale, dy * scale, nx, ny, crs)
}

/// A `float` field's value `v` as its whole count: missing (its fill, NaN,
/// outside its valid range) as [`FLOAT_MISSING`].
fn float_count(v: f32, fill: Option<f32>, valid: Option<(f32, f32)>) -> Result<u16, String> {
    if v.is_nan() || Some(v) == fill || valid.is_some_and(|(lo, hi)| v < lo || v > hi) {
        return Ok(FLOAT_MISSING);
    }
    if v.fract() != 0.0 || !(0.0..FLOAT_MISSING as f32).contains(&v) {
        return Err(format!(
            "value {v} is not a whole count in 0..{FLOAT_MISSING}; float fields are served \
             only as display counts"
        ));
    }
    Ok(v as u16)
}

/// Place a scan's tiles on one lattice: equal tiles in one CRS, pixel size
/// and packing, whose offsets (from their coordinates) are whole tiles.
/// The grid is the lattice's bounding box; a lattice cell with no tile
/// reads as missing.
fn mosaic(parts: Vec<Part>) -> Result<(Files, GeoTransform, u32, u32), String> {
    if parts.iter().any(|p| p.col_period.is_some()) {
        return Err("a global grid is a whole scan, not a tile of one".to_string());
    }
    let first = &parts[0];
    let template = first.gt.clone();
    let (w, h) = (first.gt.width, first.gt.height);
    let (pw, ph) = (first.gt.pixel_width, first.gt.pixel_height);
    let same = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs());
    for part in &parts[1..] {
        if part.packing != first.packing
            || part.stored != first.stored
            || part.leading != first.leading
        {
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
    use super::{float_count, mosaic, Frame, FrameOptions, Packing, Part, OVERVIEW_FACTOR};

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

    /// A tile with corrupt coordinates fails its scan: off the lattice, or
    /// so far away that the lattice would be enormous, never an allocation
    /// sized by the bad offset.
    #[test]
    fn mosaic_rejects_tiles_off_one_bounded_lattice() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/himawari9-isatss");
        let options = FrameOptions {
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

    /// The decimated GMGSI fixture (`testdata/gmgsi`): 60 × 102 pixels of
    /// a global spherical-Mercator mosaic, float counts.
    fn gmgsi() -> Frame {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../testdata/gmgsi/\
             GLOBCOMPLIR_v3r0_blend_s202609281200000_e202609281209599_c202609281234509.nc",
        );
        let options = FrameOptions {
            variable: "data",
            valid_fallback: None,
        };
        Frame::open(vec![std::fs::read(path).unwrap()], options).unwrap()
    }

    /// The grid recognised from the fixture's lat/lon arrays puts each
    /// pixel's centre where PROJ puts the latitude and longitude the file
    /// gives it: `cs2cs -d 3 EPSG:4326 EPSG:3857` (PROJ 9.8.1) of the
    /// `lat`/`lon` values at pixel (row, col). Columns wrap modulo the
    /// period, the grid starting at 178.2°E.
    #[test]
    fn gmgsi_grid_matches_cs2cs() {
        let frame = gmgsi();
        let gt = &frame.gt;
        assert_eq!(gt.crs, ds_core::geo::Crs::WebMercator);
        assert_eq!((gt.width, gt.height), (102, 60));
        let period = frame.col_period.unwrap();
        assert!(
            (period - 4999.378 / 49.0).abs() < 1e-3,
            "the period is 4999.378 / 49 columns"
        );
        for (row, col, x, y) in [
            (0, 0, 20_037_466.041, 12_015_991.631),
            (0, 101, 19_633_635.664, 12_015_991.631),
            (59, 0, 20_037_466.041, -11_631_215.643),
            (59, 101, 19_633_635.664, -11_631_215.643),
            (30, 51, -5_565.975, -8_014.849),
            (10, 30, -8_254_030.774, 8_007_985.343),
            (45, 80, 11_385_170.865, -6_020_015.024),
        ] {
            let c = ((x - gt.origin_x) / gt.pixel_width).rem_euclid(period);
            let r = (gt.origin_y - y) / gt.pixel_height;
            // 1e-4 px is ~40 m on this 393 km grid.
            assert!(
                (c - (col as f64 + 0.5)).abs() < 1e-4 && (r - (row as f64 + 0.5)).abs() < 1e-4,
                "pixel ({row}, {col})"
            );
        }
    }

    /// Float counts are carried whole; the fill is no value; the chunk
    /// (1 × 16 × 27) is the block, clipped at the edges.
    #[test]
    fn gmgsi_blocks_are_chunks_of_whole_counts() {
        let frame = gmgsi();
        assert_eq!((frame.block_rows, frame.block_cols), (16, 27));
        assert_eq!(frame.block_count(), 16);
        let last = frame.read_block(15).unwrap();
        assert_eq!(last.len(), 12 * 21);
        let (index, offset) = frame.locate(0, 0);
        let first = frame
            .packing
            .decode(frame.read_block(index).unwrap()[offset]);
        assert_eq!(first, Some(160.0));
        assert!(frame.overview.raw.iter().all(|&raw| {
            let v = frame.packing.decode(raw).unwrap();
            (13.0..=255.0).contains(&v)
        }));

        assert_eq!(float_count(160.0, Some(-9999.0), None), Ok(160));
        assert_eq!(float_count(-9999.0, Some(-9999.0), None), Ok(u16::MAX));
        assert_eq!(float_count(f32::NAN, None, None), Ok(u16::MAX));
        assert_eq!(float_count(300.0, None, Some((0.0, 255.0))), Ok(u16::MAX));
        assert!(float_count(271.5, None, None).is_err());
        assert!(float_count(-3.0, None, None).is_err());
        assert!(frame.packing.decode(u16::MAX).is_none());
    }

    /// Columns wrap modulo the period on the grid and the overview; the
    /// sliver of the turn past the last column reads the nearer edge
    /// column; rows do not wrap.
    #[test]
    fn gmgsi_columns_wrap_and_the_seam_sliver_reads_the_nearer_column() {
        let frame = gmgsi();
        let period = frame.col_period.unwrap();
        assert_eq!(frame.pixel(false, 5.5, 3.2), Some((5, 3)));
        assert_eq!(frame.pixel(false, 5.5 + period, 3.2), Some((5, 3)));
        assert_eq!(frame.pixel(false, -0.5, 0.0), Some((101, 0)));
        // The sliver [102, period) splits at its middle.
        let sliver = period - 102.0;
        assert!(sliver > 0.0 && sliver < 0.05, "a sliver under 0.05 px");
        assert_eq!(
            frame.pixel(false, 102.0 + sliver * 0.4, 0.0),
            Some((101, 0))
        );
        assert_eq!(frame.pixel(false, 102.0 + sliver * 0.6, 0.0), Some((0, 0)));
        assert_eq!(frame.pixel(false, 5.0, -0.1), None);
        assert_eq!(frame.pixel(false, 5.0, 60.0), None);
        assert_eq!(frame.pixel(false, f64::NAN, 1.0), None);
        // The overview: 26 × 15 cells, a period of a quarter.
        let o = &frame.overview.gt;
        assert_eq!((o.width, o.height), (26, 15));
        assert_eq!(frame.pixel(true, -0.5, 0.0), Some((25, 0)));
        assert_eq!(frame.pixel(true, 25.5 + period / 4.0, 14.9), Some((25, 14)));
    }

    /// A grid whose sides are not a multiple of the overview factor: the
    /// outer slice of the overview's last column and last row lies past the
    /// grid's edge but inside the cell, and reads that cell; past the
    /// overview's extent is nothing (#906 review). The C13 crop cut to
    /// 318 × 237 pixels: 80 × 60 cells spanning 320 × 240.
    #[test]
    fn overview_edge_cells_read_their_outer_slice() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../testdata/goes19-abi/\
             OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc",
        );
        let cmi = FrameOptions {
            variable: "CMI",
            valid_fallback: None,
        };
        let mut frame = Frame::open(vec![std::fs::read(path).unwrap()], cmi).unwrap();
        (frame.gt.width, frame.gt.height) = (318, 237);
        frame.overview = frame.build_overview().unwrap();
        let o = &frame.overview.gt;
        assert_eq!((o.width, o.height), (80, 60));
        // The last column's and last row's outer slices: 79.7 × 4 = 318.8
        // and 59.5 × 4 = 238 lie past the grid, inside the last cells.
        assert_eq!(frame.pixel(true, 79.7, 10.2), Some((79, 10)));
        assert_eq!(frame.pixel(true, 10.2, 59.5), Some((10, 59)));
        assert_eq!(frame.pixel(true, 79.99, 59.99), Some((79, 59)));
        // Their inner parts, and the rest of the overview, as before.
        assert_eq!(frame.pixel(true, 79.2, 10.2), Some((79, 10)));
        assert_eq!(frame.pixel(true, 10.2, 59.1), Some((10, 59)));
        // Past the overview's extent, or before it: nothing.
        for (c, r) in [(80.0, 10.0), (10.0, 60.0), (-0.1, 10.0), (10.0, -0.1)] {
            assert_eq!(frame.pixel(true, c, r), None, "overview ({c}, {r})");
        }
        // The full grid keeps its own edge.
        assert_eq!(frame.pixel(false, 317.9, 236.9), Some((317, 236)));
        assert_eq!(frame.pixel(false, 318.0, 10.0), None);
        assert_eq!(frame.pixel(false, 10.0, 237.0), None);

        // A global grid's rows likewise, its columns still wrapping at the
        // seam: GMGSI's fixture cut to 58 rows makes 15 cells of 60.
        let mut global = gmgsi();
        global.gt.height = 58;
        global.overview = global.build_overview().unwrap();
        assert_eq!(global.overview.gt.height, 15);
        assert_eq!(global.pixel(true, 5.2, 14.8), Some((5, 14)));
        assert_eq!(global.pixel(true, 5.2, 15.0), None);
        let period = global.col_period.unwrap();
        assert_eq!(global.pixel(true, 5.2 + period / 4.0, 14.8), Some((5, 14)));
    }

    /// The overview resolves a position on the full grid, then reads the
    /// cell holding that pixel. Its 26 cells span 104 pixels, past the
    /// 102.028-pixel turn, so at its own scale the seam's sliver falls
    /// inside the last cell (#906 review): the sliver's eastern half, where
    /// the full grid reads the first column, read the last column's cell.
    #[test]
    fn gmgsi_overview_resolves_the_seam_on_the_full_grid() {
        let frame = gmgsi();
        let period = frame.col_period.unwrap();
        let factor = OVERVIEW_FACTOR as f64;
        assert_eq!(frame.overview.gt.width, 26);
        assert!(
            26.0 * factor > period,
            "the last cell reaches past the turn"
        );
        let sliver = |f: f64| 102.0 + f * (period - 102.0);
        // East of the sliver's middle: the first column, and its cell.
        assert_eq!(frame.pixel(false, sliver(0.9), 7.0), Some((0, 7)));
        assert_eq!(
            frame.pixel(true, sliver(0.9) / factor, 7.0 / factor),
            Some((0, 1))
        );
        // West of it: the last column, and its cell.
        assert_eq!(frame.pixel(false, sliver(0.1), 7.0), Some((101, 7)));
        assert_eq!(
            frame.pixel(true, sliver(0.1) / factor, 7.0 / factor),
            Some((25, 1))
        );
        // Across the seam and into the next turn, every overview lookup
        // is the cell of the pixel the full grid reads.
        for i in 0..=4000 {
            let c = 95.0 + i as f64 * 0.003;
            for r in [0.5, 30.2, 59.9] {
                let full = frame
                    .pixel(false, c, r)
                    .map(|(col, row)| (col / OVERVIEW_FACTOR, row / OVERVIEW_FACTOR));
                assert_eq!(frame.pixel(true, c / factor, r / factor), full, "step {i}");
            }
        }
    }

    /// A bbox's pixel windows on the global grid: one inside it, two across
    /// its seam (170°E → 170°W, given either way), the whole width for a
    /// turn or more, none off its rows.
    #[test]
    fn gmgsi_windows_split_at_the_seam() {
        let frame = gmgsi();
        let spans = |bbox: [f64; 4]| {
            frame
                .windows(bbox)
                .iter()
                .map(|w| (w[0], w[2]))
                .collect::<Vec<_>>()
        };
        // 20°E–40°E: 5.7 columns of 3.53°, from column 57.
        let inside = spans([20.0, 0.0, 40.0, 10.0]);
        assert_eq!(inside.len(), 1);
        let (c0, c1) = inside[0];
        assert!(
            c0 >= 56 && c1 <= 64 && c1 - c0 >= 6,
            "20°E–40°E is about columns 57 to 63"
        );
        // West > east, and east past 180°.
        for (k, seam) in [[170.0, 10.0, -170.0, 20.0], [170.0, 10.0, 190.0, 20.0]]
            .into_iter()
            .enumerate()
        {
            let windows = spans(seam);
            assert_eq!(windows.len(), 2, "seam box {k}");
            assert_eq!(windows[0].1, 102, "seam box {k}");
            assert_eq!(windows[1].0, 0, "seam box {k}");
            let cols: u32 = windows.iter().map(|(a, b)| b - a).sum();
            assert!((6..=9).contains(&cols), "seam box {k}");
        }
        assert_eq!(spans([-180.0, 0.0, 180.0, 10.0]), [(0, 102)]);
        assert_eq!(spans([-540.0, 0.0, 540.0, 10.0]), [(0, 102)]);
        let rows = frame.windows([0.0, -10.0, 10.0, 10.0]);
        assert!(
            rows[0][1] < 30 && rows[0][3] > 30,
            "10°S–10°N holds the equator's row"
        );
        assert!(frame.windows([0.0, 80.0, 10.0, 85.0]).is_empty());
        assert_eq!(frame.extent().map(|e| [e[0], e[2]]), Some([-180.0, 180.0]));
    }
}
