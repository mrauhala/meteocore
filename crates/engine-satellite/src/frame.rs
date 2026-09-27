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
//! full width.

use std::sync::Arc;

use ds_core::cf::{coordinate_scale, crs_from_grid_mapping, CfAttr};
use ds_core::geo::GeoTransform;
use hdf5_reader::storage::{BytesStorage, DynStorage};
use netcdf_reader::{NcAttrValue, NcFile, NcOpenOptions, NcSliceInfo, NcSliceInfoElem, NcType};

/// Decimation of the overview grid built at ingest. A render whose source
/// window spans at least this many source pixels per output pixel samples
/// the overview instead of decoding strips.
pub(crate) const OVERVIEW_FACTOR: u32 = 4;

/// Block height when the variable is not chunked.
const DEFAULT_BLOCK_ROWS: u32 = 24;

pub(crate) struct Frame {
    nc: NcFile,
    variable: String,
    /// Whether the variable is stored as `short` (read as `i16`); the
    /// values may still be unsigned (`_Unsigned = "true"`, as GOES-R CMI).
    stored_signed: bool,
    pub packing: Packing,
    /// Full-resolution grid.
    pub gt: GeoTransform,
    /// Rows per decoded block: the variable's chunk height, so a block read
    /// inflates each chunk once.
    pub block_rows: u32,
    /// Columns per decoded block: the full width, so a block is a strip.
    pub block_cols: u32,
    pub overview: Overview,
    /// Bytes this frame holds (the file plus the overview), for the cache.
    pub weight: u64,
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
    /// Parse a scan's NetCDF-4 bytes and build its overview. `variable` is
    /// the 2-D `(y, x)` field to serve.
    pub fn open(bytes: Vec<u8>, variable: &str) -> Result<Frame, String> {
        let file_len = bytes.len() as u64;
        let storage: DynStorage = Arc::new(BytesStorage::new(bytes));
        let options = NcOpenOptions {
            // Strips are cached decoded, so the reader's own chunk cache
            // would only duplicate them.
            chunk_cache_bytes: 0,
            ..NcOpenOptions::default()
        };
        let nc = NcFile::from_storage_with_options(storage.clone(), options)
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
                }),
        };

        let mapping = text_attr(var.attribute("grid_mapping").map(|a| &a.value))
            .ok_or_else(|| format!("variable '{variable}' has no grid_mapping"))?;
        let mapping_var = nc
            .variable(mapping.trim())
            .map_err(|e| format!("grid mapping '{mapping}': {e}"))?;
        let crs = crs_from_grid_mapping(|name| {
            mapping_var.attribute(name).and_then(|a| cf_attr(&a.value))
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

        let block_rows = hdf5_reader::Hdf5File::from_storage(storage)
            .ok()
            .and_then(|h5| h5.dataset(variable).ok()?.chunks())
            .and_then(|chunks| chunks.first().copied())
            .filter(|&rows| rows > 0)
            .unwrap_or(DEFAULT_BLOCK_ROWS)
            .min(ny);

        let placeholder = GeoTransform {
            width: 0,
            height: 0,
            ..gt.clone()
        };
        let mut frame = Frame {
            nc,
            variable: variable.to_string(),
            stored_signed,
            packing,
            gt,
            block_rows,
            block_cols: nx,
            overview: Overview {
                gt: placeholder,
                raw: Vec::new(),
            },
            weight: file_len,
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
            let values = self
                .nc
                .read_variable_slice::<i16>(&self.variable, &selection)
                .map_err(read)?;
            values.iter().map(|&v| v as u16).collect()
        } else {
            let values = self
                .nc
                .read_variable_slice::<u16>(&self.variable, &selection)
                .map_err(read)?;
            values.iter().copied().collect()
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
        Ok(Overview {
            gt: GeoTransform {
                pixel_width: self.gt.pixel_width * factor as f64,
                pixel_height: self.gt.pixel_height * factor as f64,
                width: ox,
                height: oy,
                ..self.gt.clone()
            },
            raw,
        })
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
    use super::{Frame, Packing};

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
        let strips = Frame::open(bytes.clone(), "CMI").unwrap();
        assert_eq!((strips.block_cols, strips.gt.width), (320, 320));
        let mut blocks = Frame::open(bytes, "CMI").unwrap();
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
}
