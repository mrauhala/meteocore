use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
use ds_core::geo::{Crs, GeoTransform};
use memmap2::Mmap;
use thiserror::Error;

/// Missing value sentinel in querydata files.
pub const MISSING_VALUE: f32 = 32700.0;

/// Magic bytes at the start of a querydata file: `@$°£Q`
const MAGIC: &[u8] = &[0x40, 0x24, 0xb0, 0xa3, 0x51];
/// Second magic marker: `@$°£`
const MAGIC2: &[u8] = &[0x40, 0x24, 0xb0, 0xa3];
/// Endian marker for little-endian: "INFO" as bytes
const ENDIAN_LE: &[u8] = b"INFO";

#[derive(Error, Debug)]
pub enum QueryDataError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Not a querydata file (bad magic)")]
    BadMagic,
    #[error("Big-endian querydata files are not supported")]
    BigEndian,
    #[error("Unsupported querydata version: {0}")]
    UnsupportedVersion(String),
    #[error("Parse error at offset {offset}: {message}")]
    Parse { offset: usize, message: String },
    #[error("Unsupported area type: classId={0}")]
    UnsupportedArea(u32),
    #[error("Data size mismatch: expected {expected} bytes, got {actual}")]
    DataSizeMismatch { expected: usize, actual: usize },
}

/// A parsed parameter descriptor.
#[derive(Debug, Clone)]
pub struct ParamInfo {
    /// Parameter ID (FMI param number).
    pub id: u32,
    /// Parameter name.
    pub name: String,
    /// Minimum valid value (32700 = not set).
    pub min_value: f32,
    /// Maximum valid value (32700 = not set).
    pub max_value: f32,
    /// Producer name.
    pub producer: String,
}

/// Grid area definition.
#[derive(Debug, Clone)]
pub struct GridArea {
    /// Bottom-left corner (lon, lat) in degrees.
    pub bottom_left: (f64, f64),
    /// Top-right corner (lon, lat) in degrees.
    pub top_right: (f64, f64),
    /// CRS for coordinate transforms.
    pub crs: Crs,
}

/// Grid dimensions and layout.
#[derive(Debug, Clone)]
pub struct GridInfo {
    /// Number of grid columns (x direction).
    pub nx: u32,
    /// Number of grid rows (y direction).
    pub ny: u32,
    /// Grid area (corner coordinates + CRS).
    pub area: GridArea,
}

impl GridInfo {
    /// Build a GeoTransform for pixel ↔ world coordinate mapping.
    ///
    /// QueryData grids have bottom-left origin (row 0 = south edge), but
    /// GeoTransform expects top-left origin (row 0 = north edge). This
    /// method constructs the transform with origin_y at the top (north)
    /// edge so that `pixel_to_world(0, 0)` returns the northwest corner.
    ///
    /// Use `grid_lonlat()` on QueryData for direct grid-index-to-lonlat
    /// mapping that accounts for the bottom-left origin convention.
    pub fn geo_transform(&self) -> GeoTransform {
        let (lon0, _lat0) = self.area.bottom_left;
        let (lon1, lat1) = self.area.top_right;

        let pixel_width = (lon1 - lon0) / self.nx as f64;
        let pixel_height = (lat1 - _lat0) / self.ny as f64;

        GeoTransform {
            origin_x: lon0,
            origin_y: lat1, // top edge (north)
            pixel_width,
            pixel_height,
            width: self.nx,
            height: self.ny,
            crs: self.area.crs.clone(),
        }
    }
}

/// Parsed querydata file providing access to metadata and float data.
pub struct QueryData {
    /// Parameters in this file.
    pub params: Vec<ParamInfo>,
    /// Grid definition.
    pub grid: GridInfo,
    /// Vertical levels (level values).
    pub levels: Vec<f32>,
    /// Time steps (UTC).
    pub times: Vec<DateTime<Utc>>,
    /// Origin (analysis) time (UTC).
    pub origin_time: DateTime<Utc>,
    /// Memory-mapped file data.
    mmap: Mmap,
    /// Offset in bytes to the start of binary float data.
    data_offset: usize,
    /// Whether float bytes need endian swapping (always false for now).
    swap_endian: bool,
}

impl QueryData {
    /// Open and parse a querydata file from disk.
    pub fn open(path: &Path) -> Result<Self, QueryDataError> {
        let file = std::fs::File::open(path)?;
        // SAFETY: we don't modify the file while mapped, and we handle SIGBUS
        // by treating all data access through bounds-checked methods.
        let mmap = unsafe { Mmap::map(&file)? };
        Self::from_mmap(mmap)
    }

    /// Parse a querydata file from a memory-mapped buffer.
    fn from_mmap(mmap: Mmap) -> Result<Self, QueryDataError> {
        let data = &mmap[..];
        if data.len() < 14 {
            return Err(QueryDataError::BadMagic);
        }

        // Validate magic
        if &data[0..5] != MAGIC {
            return Err(QueryDataError::BadMagic);
        }
        // Check endianness
        if &data[5..9] != ENDIAN_LE {
            return Err(QueryDataError::BigEndian);
        }
        if &data[9..13] != MAGIC2 {
            return Err(QueryDataError::BadMagic);
        }

        let mut reader = TextReader::new(data, 14);

        // "VER <version>"
        let ver_line = reader.read_line()?;
        if !ver_line.starts_with("VER ") {
            return Err(QueryDataError::UnsupportedVersion(ver_line));
        }
        let version: f64 = ver_line[4..]
            .trim()
            .parse()
            .map_err(|_| QueryDataError::UnsupportedVersion(ver_line.clone()))?;
        if version < 6.0 {
            return Err(QueryDataError::UnsupportedVersion(ver_line));
        }

        // "<classId> NFmiQueryInfo"
        let _class_line = reader.read_line()?;
        // "0 0 0 0 " (reserved)
        let _reserved = reader.read_line()?;

        // NFmiStringList: itsHeaderText
        read_string_list(&mut reader)?;
        // NFmiStringList: itsPostProc
        read_string_list(&mut reader)?;

        // NFmiParamDescriptor
        let params = read_param_descriptor(&mut reader)?;

        // NFmiHPlaceDescriptor
        let grid = read_hplace_descriptor(&mut reader)?;

        // NFmiVPlaceDescriptor
        let levels = read_vplace_descriptor(&mut reader)?;

        // NFmiTimeDescriptor (also parses through raw data header)
        let (times, origin_time, is_binary, pool_size) = read_time_descriptor(&mut reader)?;

        if !is_binary {
            return Err(QueryDataError::Parse {
                offset: reader.pos,
                message: "Text-mode data not supported (expected binary flag=1)".into(),
            });
        }

        let data_offset = reader.pos;

        // Validate data size
        let expected =
            params.len() * (grid.nx as usize * grid.ny as usize) * levels.len() * times.len() * 4;
        if pool_size != expected {
            return Err(QueryDataError::DataSizeMismatch {
                expected,
                actual: pool_size,
            });
        }

        // Verify we have enough bytes
        if data_offset + pool_size > mmap.len() {
            return Err(QueryDataError::DataSizeMismatch {
                expected: data_offset + pool_size,
                actual: mmap.len(),
            });
        }

        Ok(QueryData {
            params,
            grid,
            levels,
            times,
            origin_time,
            mmap,
            data_offset,
            swap_endian: false,
        })
    }

    /// Total number of grid points (nx * ny).
    pub fn grid_size(&self) -> usize {
        self.grid.nx as usize * self.grid.ny as usize
    }

    /// Get a single float value by index.
    ///
    /// Data layout: `param -> location -> level -> time`
    /// Index = param * (locations * levels * times) + location * (levels * times) + level * times + time
    ///
    /// Returns `None` if the value equals the missing sentinel (32700.0).
    pub fn value(
        &self,
        param_idx: usize,
        grid_idx: usize,
        level_idx: usize,
        time_idx: usize,
    ) -> Option<f64> {
        let locations = self.grid_size();
        let levels = self.levels.len();
        let times = self.times.len();

        let idx = param_idx * (locations * levels * times)
            + grid_idx * (levels * times)
            + level_idx * times
            + time_idx;

        let byte_offset = self.data_offset + idx * 4;
        if byte_offset + 4 > self.mmap.len() {
            return None;
        }

        let bytes = &self.mmap[byte_offset..byte_offset + 4];
        let val = if self.swap_endian {
            f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        };

        if (val - MISSING_VALUE).abs() < 0.5 {
            None
        } else {
            Some(val as f64)
        }
    }

    /// Get the grid (col, row) for a given linear grid index.
    /// Grid is row-major: index = row * nx + col.
    pub fn grid_col_row(&self, grid_idx: usize) -> (u32, u32) {
        let col = (grid_idx % self.grid.nx as usize) as u32;
        let row = (grid_idx / self.grid.nx as usize) as u32;
        (col, row)
    }

    /// Get the (lon, lat) for a grid index.
    ///
    /// QueryData grid indices are row-major with row 0 at the bottom (south).
    /// This method flips the row to match GeoTransform's top-left convention.
    pub fn grid_lonlat(&self, grid_idx: usize) -> (f64, f64) {
        let (col, row) = self.grid_col_row(grid_idx);
        // Flip row: querydata row 0 = south, GeoTransform row 0 = north
        let flipped_row = self.grid.ny - 1 - row;
        self.grid.geo_transform().pixel_to_world(col, flipped_row)
    }

    /// Get all values for a specific parameter and time step across the full grid.
    /// Returns a Vec of Option<f64> with length nx * ny.
    pub fn grid_values(
        &self,
        param_idx: usize,
        level_idx: usize,
        time_idx: usize,
    ) -> Vec<Option<f64>> {
        let locations = self.grid_size();
        (0..locations)
            .map(|loc| self.value(param_idx, loc, level_idx, time_idx))
            .collect()
    }

    /// Find a parameter index by ID.
    pub fn param_index(&self, param_id: u32) -> Option<usize> {
        self.params.iter().position(|p| p.id == param_id)
    }

    /// Find a parameter index by name (case-insensitive).
    ///
    /// Matches against (in order of priority):
    /// 1. Full name: `"2 Metre Temperature (2t)"`
    /// 2. Short name in parentheses: `"2t"`
    /// 3. Numeric parameter ID: `"4"`
    pub fn param_index_by_name(&self, name: &str) -> Option<usize> {
        let lower = name.to_lowercase();

        // Try exact full name match
        if let Some(idx) = self
            .params
            .iter()
            .position(|p| p.name.to_lowercase() == lower)
        {
            return Some(idx);
        }

        // Try short name: match content inside parentheses at the end, e.g., "(2t)"
        if let Some(idx) = self.params.iter().position(|p| {
            p.name
                .rfind('(')
                .and_then(|start| p.name[start + 1..].strip_suffix(')'))
                .is_some_and(|short| short.to_lowercase() == lower)
        }) {
            return Some(idx);
        }

        // Try numeric ID
        if let Ok(id) = name.parse::<u32>() {
            return self.param_index(id);
        }

        None
    }
}

// ============================================================================
// Text-based header parser
// ============================================================================

/// Reader for the text-based header portion of a querydata file.
struct TextReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> TextReader<'a> {
    fn new(data: &'a [u8], start: usize) -> Self {
        Self { data, pos: start }
    }

    /// Read the next line (up to \n). Returns the line content without the newline.
    fn read_line(&mut self) -> Result<String, QueryDataError> {
        let start = self.pos;
        let remaining = &self.data[self.pos..];
        let nl_pos = remaining
            .iter()
            .position(|&b| b == b'\n')
            .ok_or(QueryDataError::Parse {
                offset: start,
                message: "Unexpected end of header (no newline found)".into(),
            })?;
        let line_bytes = &remaining[..nl_pos];
        self.pos += nl_pos + 1;
        String::from_utf8(line_bytes.to_vec()).map_err(|_| QueryDataError::Parse {
            offset: start,
            message: "Invalid UTF-8 in header".into(),
        })
    }

    /// Read an NFmiString: `<length> <raw_bytes>` where length is followed by
    /// a space and then exactly `length` bytes (which may include newlines).
    fn read_nfmi_string(&mut self) -> Result<String, QueryDataError> {
        let start = self.pos;
        // Read until we find a space — that gives us the length
        let remaining = &self.data[self.pos..];
        let space_pos = remaining
            .iter()
            .position(|&b| b == b' ')
            .ok_or(QueryDataError::Parse {
                offset: start,
                message: "Expected space after string length".into(),
            })?;
        let len_str =
            std::str::from_utf8(&remaining[..space_pos]).map_err(|_| QueryDataError::Parse {
                offset: start,
                message: "Invalid string length".into(),
            })?;
        let len: usize = len_str.parse().map_err(|_| QueryDataError::Parse {
            offset: start,
            message: format!("Invalid string length: '{len_str}'"),
        })?;
        self.pos += space_pos + 1; // skip past the space

        if self.pos + len > self.data.len() {
            return Err(QueryDataError::Parse {
                offset: self.pos,
                message: format!("String length {len} exceeds remaining data"),
            });
        }

        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;

        // The string content followed by \n
        String::from_utf8(bytes.to_vec()).map_err(|_| QueryDataError::Parse {
            offset: start,
            message: "Invalid UTF-8 in string".into(),
        })
    }
}

/// Read an NFmiStringList (headerText or postProc).
fn read_string_list(r: &mut TextReader) -> Result<Vec<String>, QueryDataError> {
    let count_line = r.read_line()?;
    let count: usize = count_line
        .trim()
        .parse()
        .map_err(|_| QueryDataError::Parse {
            offset: r.pos,
            message: format!("Invalid string list count: '{count_line}'"),
        })?;
    let mut strings = Vec::with_capacity(count);
    for _ in 0..count {
        // Each item: "<classId> <NFmiString>"
        // classId is on the same logical unit but for simplicity we just read the line
        let _class_and_string = r.read_line()?;
        strings.push(_class_and_string);
    }
    Ok(strings)
}

/// Read NFmiParamDescriptor → returns Vec<ParamInfo>.
fn read_param_descriptor(r: &mut TextReader) -> Result<Vec<ParamInfo>, QueryDataError> {
    // "<classId> NFmiParamDescriptor"
    let _class_line = r.read_line()?;
    // "<interpolate> 0 0 0 "
    let _flags = r.read_line()?;

    // NFmiParamBag: <size>\n then that many NFmiDataIdent entries
    let size_line = r.read_line()?;
    let size: usize = size_line
        .trim()
        .parse()
        .map_err(|_| QueryDataError::Parse {
            offset: r.pos,
            message: format!("Invalid param count: '{size_line}'"),
        })?;

    let mut params = Vec::with_capacity(size);
    for _ in 0..size {
        let param = read_data_ident(r)?;
        params.push(param);
    }

    // Activity flags: "1 1 1 ... \n"
    let _activity = r.read_line()?;

    Ok(params)
}

/// Read a single NFmiDataIdent (parameter entry).
fn read_data_ident(r: &mut TextReader) -> Result<ParamInfo, QueryDataError> {
    // NFmiParam (extends NFmiIndividual):
    // <ident>\n
    let id_line = r.read_line()?;
    let id: u32 = id_line.trim().parse().map_err(|_| QueryDataError::Parse {
        offset: r.pos,
        message: format!("Invalid param id: '{id_line}'"),
    })?;

    // NFmiString: <length> <name>
    let name = r.read_nfmi_string()?;

    // Skip trailing newline after string if present
    if r.pos < r.data.len() && r.data[r.pos] == b'\n' {
        r.pos += 1;
    }

    // <minValue> <maxValue> <interpolationMethod> <scale> <base> <precision> <formatString>
    // This is on one line: "32700 32700 1 32700 32700 4 %.1f"
    let values_line = r.read_line()?;
    let parts: Vec<&str> = values_line.split_whitespace().collect();
    let min_value: f32 = parts
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(MISSING_VALUE);
    let max_value: f32 = parts
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(MISSING_VALUE);

    // NFmiProducer (extends NFmiIndividual):
    // <ident>\n
    let _producer_id = r.read_line()?;
    // NFmiString: <length> <name>
    let producer = r.read_nfmi_string()?;
    if r.pos < r.data.len() && r.data[r.pos] == b'\n' {
        r.pos += 1;
    }

    // Type flags: "<type> <isGroup> <isActive> <containsIndividualParams> <isDataParam> <hasDataParams> 0 0 "
    let flags_line = r.read_line()?;
    let flag_parts: Vec<&str> = flags_line.split_whitespace().collect();
    let has_data_params = flag_parts
        .get(5)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    // Sub-parameters count
    if has_data_params != 0 {
        // NFmiParamBag of sub-params — skip them.
        // Note: NFmiParamBag does NOT have activity flags; those belong to
        // NFmiParamDescriptor which wraps the top-level bag only.
        let sub_count_line = r.read_line()?;
        let sub_count: usize = sub_count_line.trim().parse().unwrap_or(0);
        for _ in 0..sub_count {
            let _sub = read_data_ident(r)?;
        }
    }

    // Number of secondary producers
    let secondary_line = r.read_line()?;
    let secondary_count: usize = secondary_line.trim().parse().unwrap_or(0);
    for _ in 0..secondary_count {
        // Skip each secondary producer (NFmiIndividual: id + string)
        let _id = r.read_line()?;
        let _name = r.read_nfmi_string()?;
        if r.pos < r.data.len() && r.data[r.pos] == b'\n' {
            r.pos += 1;
        }
    }

    Ok(ParamInfo {
        id,
        name,
        min_value,
        max_value,
        producer,
    })
}

/// Read NFmiHPlaceDescriptor → returns GridInfo.
fn read_hplace_descriptor(r: &mut TextReader) -> Result<GridInfo, QueryDataError> {
    // "<selectedType> <maxNumberOfSources> 0 0"
    let _header = r.read_line()?;

    // Location bag section: "<classId> NFmiLocationBag" or "0 ..."
    let loc_class_line = r.read_line()?;
    let loc_class_id: u32 = loc_class_line
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if loc_class_id != 0 {
        // Skip location bag (station data — not supported for now)
        return Err(QueryDataError::Parse {
            offset: r.pos,
            message: "Station-based querydata not supported (only gridded data)".into(),
        });
    }

    // Area section: "<classId> NFmiArea" or "0 ..."
    let area_class_line = r.read_line()?;
    let _area_class_id: u32 = area_class_line
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // Standalone area is typically 0 for grid data (area is inside the grid)

    // Grid section: "41 NFmiGrid" or "0 ..."
    let grid_class_line = r.read_line()?;
    let grid_class_id: u32 = grid_class_line
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if grid_class_id != 41 {
        return Err(QueryDataError::Parse {
            offset: r.pos,
            message: format!("Expected NFmiGrid (classId=41), got classId={grid_class_id}"),
        });
    }

    // NFmiGrid contains: area + grid base + data pool
    let area = read_grid_area(r)?;

    // GridBase: interpolationMethod, startingCorner, xNumber yNumber
    let _interp = r.read_line()?;
    let _starting_corner = r.read_line()?;
    let dims_line = r.read_line()?;
    let dims: Vec<u32> = dims_line
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if dims.len() < 2 {
        return Err(QueryDataError::Parse {
            offset: r.pos,
            message: format!("Invalid grid dimensions: '{dims_line}'"),
        });
    }
    let nx = dims[0];
    let ny = dims[1];

    // DataPool (grid's own data — usually empty)
    let _data_type = r.read_line()?; // "6"
    let binary_flag_line = r.read_line()?; // "0" or "1"
    let pool_size_line = r.read_line()?;
    let pool_size: usize = pool_size_line.trim().parse().unwrap_or(0);
    if pool_size > 0 {
        // Skip embedded data
        let binary = binary_flag_line.trim() == "1";
        if binary {
            r.pos += pool_size;
        } else {
            // Text floats — skip lines
            for _ in 0..pool_size / 4 {
                let _ = r.read_line()?;
            }
        }
    }

    Ok(GridInfo { nx, ny, area })
}

/// Read the area definition inside an NFmiGrid.
fn read_grid_area(r: &mut TextReader) -> Result<GridArea, QueryDataError> {
    // "<classId> <className>"
    let class_line = r.read_line()?;
    let class_id: u32 = class_line
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    match class_id {
        10 => read_latlon_area(r),
        11 => read_rotated_latlon_area(r),
        13 => read_stereographic_area(r),
        84 => read_lcc_area(r),
        _ => Err(QueryDataError::UnsupportedArea(class_id)),
    }
}

/// Read NFmiLatLonArea (classId=10).
fn read_latlon_area(r: &mut TextReader) -> Result<GridArea, QueryDataError> {
    // NFmiArea base: NFmiRect (XY rect) — 2 points
    let _xy_p1 = r.read_line()?; // place point
    let _xy_p2 = r.read_line()?; // size point

    // Bottom-left (lon, lat)
    let bl_line = r.read_line()?;
    let bl = parse_point(&bl_line)?;

    // Top-right (lon, lat)
    let tr_line = r.read_line()?;
    let tr = parse_point(&tr_line)?;

    // 4 dummy doubles (old removed variables): "0 0\n0 0\n"
    let _dummy1 = r.read_line()?;
    let _dummy2 = r.read_line()?;

    // Scale factors: "xScale yScale"
    let _scale = r.read_line()?;

    Ok(GridArea {
        bottom_left: bl,
        top_right: tr,
        crs: Crs::Wgs84,
    })
}

/// Read NFmiRotatedLatLonArea (classId=11).
fn read_rotated_latlon_area(r: &mut TextReader) -> Result<GridArea, QueryDataError> {
    // First, the full NFmiLatLonArea serialization
    let base = read_latlon_area(r)?;

    // Then south pole position: "lon lat"
    let sp_line = r.read_line()?;
    let sp = parse_point(&sp_line)?;

    Ok(GridArea {
        bottom_left: base.bottom_left,
        top_right: base.top_right,
        crs: Crs::RotatedLatLon {
            south_pole_lat: sp.1.to_radians(),
            south_pole_lon: sp.0.to_radians(),
        },
    })
}

/// Read NFmiStereographicArea (classId=13, extends NFmiAzimuthalArea).
fn read_stereographic_area(r: &mut TextReader) -> Result<GridArea, QueryDataError> {
    // NFmiArea base: NFmiRect (XY rect)
    let _xy_p1 = r.read_line()?;
    let _xy_p2 = r.read_line()?;

    // Bottom-left (lon, lat)
    let bl_line = r.read_line()?;
    let bl = parse_point(&bl_line)?;

    // Top-right (lon, lat)
    let tr_line = r.read_line()?;
    let tr = parse_point(&tr_line)?;

    // Central longitude
    let clon_line = r.read_line()?;
    let central_lon: f64 = clon_line
        .trim()
        .parse()
        .map_err(|_| QueryDataError::Parse {
            offset: r.pos,
            message: format!("Invalid central longitude: '{clon_line}'"),
        })?;

    // Central latitude
    let clat_line = r.read_line()?;
    let central_lat: f64 = clat_line
        .trim()
        .parse()
        .map_err(|_| QueryDataError::Parse {
            offset: r.pos,
            message: format!("Invalid central latitude: '{clat_line}'"),
        })?;

    // True latitude
    let _true_lat_line = r.read_line()?;

    // "radialRange 0 0"
    let _radial = r.read_line()?;

    // World rect (4 doubles as 2 points)
    let _wr_p1 = r.read_line()?;
    let _wr_p2 = r.read_line()?;

    Ok(GridArea {
        bottom_left: bl,
        top_right: tr,
        crs: Crs::Stereographic {
            lat0: central_lat.to_radians(),
            lon0: central_lon.to_radians(),
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        },
    })
}

/// Read NFmiLambertConformalConicArea (classId=84, extends NFmiArea directly).
///
/// Format (9 lines + possible empty lines between sections):
///   XY rect place, XY rect size, BL lon/lat, TR lon/lat,
///   [empty], central lon+lat, true lat1+lat2, radius,
///   world rect place, world rect size, [empty]
fn read_lcc_area(r: &mut TextReader) -> Result<GridArea, QueryDataError> {
    // NFmiArea base: NFmiRect (XY rect)
    let _xy_p1 = r.read_line()?;
    let _xy_p2 = r.read_line()?;

    // Bottom-left (lon, lat)
    let bl_line = r.read_line()?;
    let bl = parse_point(&bl_line)?;

    // Top-right (lon, lat)
    let tr_line = r.read_line()?;
    let tr = parse_point(&tr_line)?;

    // Skip empty lines between sections
    let mut central_line = r.read_line()?;
    while central_line.trim().is_empty() {
        central_line = r.read_line()?;
    }

    // "centralLon centralLat" on one line
    let central = parse_point(&central_line)?;
    let central_lon = central.0;
    let central_lat = central.1;

    // "trueLat1 trueLat2" on one line
    let true_lats_line = r.read_line()?;
    let true_lats = parse_point(&true_lats_line)?;
    let true_lat1 = true_lats.0;
    let true_lat2 = true_lats.1;

    // radius (Earth radius, e.g. 6371220)
    let _radius = r.read_line()?;

    // World rect (4 doubles as 2 points, precision 15)
    let _wr_p1 = r.read_line()?;
    let _wr_p2 = r.read_line()?;

    // Skip trailing empty line if present
    if r.pos < r.data.len() && r.data[r.pos] == b'\n' {
        r.pos += 1;
    }

    Ok(GridArea {
        bottom_left: bl,
        top_right: tr,
        crs: Crs::LambertConformalConic {
            lat1: true_lat1.to_radians(),
            lat2: true_lat2.to_radians(),
            lat0: central_lat.to_radians(),
            lon0: central_lon.to_radians(),
            false_e: 0.0,
            false_n: 0.0,
        },
    })
}

/// Read NFmiVPlaceDescriptor → returns Vec<f32> of level values.
fn read_vplace_descriptor(r: &mut TextReader) -> Result<Vec<f32>, QueryDataError> {
    // NFmiLevelBag: <size>\n
    let size_line = r.read_line()?;
    let size: usize = size_line
        .trim()
        .parse()
        .map_err(|_| QueryDataError::Parse {
            offset: r.pos,
            message: format!("Invalid level count: '{size_line}'"),
        })?;

    let mut levels = Vec::with_capacity(size);
    for _ in 0..size {
        // NFmiLevel: <ident>\n <NFmiString>\n <levelValue>\n
        let _level_type = r.read_line()?;
        let _level_name = r.read_nfmi_string()?;
        if r.pos < r.data.len() && r.data[r.pos] == b'\n' {
            r.pos += 1;
        }
        let val_line = r.read_line()?;
        let val: f32 = val_line.trim().parse().unwrap_or(0.0);
        levels.push(val);
    }

    // Step
    let _step = r.read_line()?;

    Ok(levels)
}

/// Read NFmiTimeDescriptor → returns (times, origin_time).
/// Parsed time descriptor result: (times, origin_time, is_binary, pool_size_bytes).
type TimeDescriptorResult = (Vec<DateTime<Utc>>, DateTime<Utc>, bool, usize);

/// Parses through the raw data header as well since the time descriptor
/// footer blends into it.
fn read_time_descriptor(r: &mut TextReader) -> Result<TimeDescriptorResult, QueryDataError> {
    // "<timeListIdent> NFmiTimeDescriptor"
    let header_line = r.read_line()?;
    let time_list_ident: u32 = header_line
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let times = if time_list_ident == 1 {
        // NFmiTimeList: <count>\n then <year month day hour min sec>\n per item
        let count_line = r.read_line()?;
        let count: usize = count_line
            .trim()
            .parse()
            .map_err(|_| QueryDataError::Parse {
                offset: r.pos,
                message: format!("Invalid time count: '{count_line}'"),
            })?;
        let mut times = Vec::with_capacity(count);
        for _ in 0..count {
            let time_line = r.read_line()?;
            times.push(parse_time(&time_line)?);
        }
        times
    } else {
        // NFmiTimeBag: first_time, last_time, resolution
        let first_line = r.read_line()?;
        let first = parse_time(&first_line)?;
        let last_line = r.read_line()?;
        let last = parse_time(&last_line)?;
        let res_line = r.read_line()?;
        let res_minutes: i64 = res_line.trim().parse().unwrap_or(60);

        let mut times = vec![first];
        let mut t = first;
        while t < last {
            t += chrono::Duration::minutes(res_minutes);
            if t <= last {
                times.push(t);
            }
        }
        times
    };

    // Origin time bag (always NFmiTimeBag): first_time, last_time
    let origin_first_line = r.read_line()?;
    let origin_time = parse_time(&origin_first_line)?;
    let _origin_last = r.read_line()?;

    // Origin time bag resolution + descriptor flags + activity flags.
    // The exact structure varies by version but always ends before the raw
    // data section which starts with "6\n1\n<poolsize>\n". Consume lines
    // until we hit the raw data marker.
    loop {
        let line = r.read_line()?;
        let trimmed = line.trim();
        if trimmed == "6" {
            // Peek: next line should be "0" or "1" (binary flag)
            let next = r.read_line()?;
            let flag = next.trim();
            if flag == "0" || flag == "1" {
                // This is the raw data header — put the binary flag back
                // by rewinding past it. We'll re-read it in the caller.
                // Actually, easier to just return early with the flag info.
                let pool_size_str = r.read_line()?;
                let pool_size: usize =
                    pool_size_str
                        .trim()
                        .parse()
                        .map_err(|_| QueryDataError::Parse {
                            offset: r.pos,
                            message: format!("Invalid pool size: '{pool_size_str}'"),
                        })?;
                return Ok((times, origin_time, flag == "1", pool_size));
            }
            // Not the raw data marker — continue scanning
        }
    }
}

/// Parse "x y" as (f64, f64).
fn parse_point(line: &str) -> Result<(f64, f64), QueryDataError> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(QueryDataError::Parse {
            offset: 0,
            message: format!("Expected 2 values in point, got: '{line}'"),
        });
    }
    let x: f64 = parts[0].parse().map_err(|_| QueryDataError::Parse {
        offset: 0,
        message: format!("Invalid x coordinate: '{}'", parts[0]),
    })?;
    let y: f64 = parts[1].parse().map_err(|_| QueryDataError::Parse {
        offset: 0,
        message: format!("Invalid y coordinate: '{}'", parts[1]),
    })?;
    Ok((x, y))
}

/// Parse "year month day hour min sec" as DateTime<Utc>.
fn parse_time(line: &str) -> Result<DateTime<Utc>, QueryDataError> {
    let parts: Vec<i32> = line
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if parts.len() < 6 {
        return Err(QueryDataError::Parse {
            offset: 0,
            message: format!("Expected 6 time components, got: '{line}'"),
        });
    }
    let dt = NaiveDateTime::new(
        chrono::NaiveDate::from_ymd_opt(parts[0], parts[1] as u32, parts[2] as u32).ok_or_else(
            || QueryDataError::Parse {
                offset: 0,
                message: format!("Invalid date: {}-{}-{}", parts[0], parts[1], parts[2]),
            },
        )?,
        chrono::NaiveTime::from_hms_opt(parts[3] as u32, parts[4] as u32, parts[5] as u32)
            .ok_or_else(|| QueryDataError::Parse {
                offset: 0,
                message: format!("Invalid time: {}:{}:{}", parts[3], parts[4], parts[5]),
            })?,
    );
    Ok(DateTime::from_naive_utc_and_offset(dt, Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_file() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/ecmwf-kenya/202604042019_202604040600_ecmwf_kenya_surface.sqd")
    }

    #[test]
    fn parse_ecmwf_kenya() {
        let path = test_file();
        assert!(
            path.exists(),
            "ecmwf-kenya fixture missing: {}",
            path.display()
        );

        let qd = QueryData::open(&path).unwrap();

        // Verify parameters. The committed fixture is a qdcrop subset of the
        // original (msl/2t/precip kept, in that order) — see testdata README.
        assert_eq!(qd.params.len(), 3, "Expected 3 parameters");
        assert_eq!(qd.params[0].name, "Mean Sea Level Pressure (msl)");
        assert_eq!(qd.params[1].name, "2 Metre Temperature (2t)");

        // Verify grid (cropped Kenya box, decimated 2×)
        assert_eq!(qd.grid.nx, 16);
        assert_eq!(qd.grid.ny, 21);
        assert!(matches!(qd.grid.area.crs, Crs::Wgs84));
        assert!((qd.grid.area.bottom_left.0 - 34.0).abs() < 0.01);
        assert!((qd.grid.area.bottom_left.1 - 4.75).abs() < 0.01);
        assert!((qd.grid.area.top_right.0 - 41.5).abs() < 0.01);
        assert!((qd.grid.area.top_right.1 - (-5.25)).abs() < 0.01);

        // Verify levels
        assert_eq!(qd.levels.len(), 1, "Expected 1 level (surface)");

        // Verify times (cropped to +0/+3/+6/+9h)
        assert_eq!(qd.times.len(), 4, "Expected 4 time steps");
        assert_eq!(
            qd.times[0].format("%Y-%m-%dT%H:%M").to_string(),
            "2026-04-04T06:00"
        );

        // Verify origin time
        assert_eq!(
            qd.origin_time.format("%Y-%m-%dT%H:%M").to_string(),
            "2026-04-04T06:00"
        );

        // Verify data access — first grid point, first param, first time
        // Should be a valid MSLP value (typically 900-1100 hPa)
        let val = qd.value(0, 0, 0, 0);
        if let Some(v) = val {
            assert!(v > 800.0 && v < 1200.0, "MSLP value {v} out of range");
        }

        // Verify temperature (param 1) at some grid point — ECMWF Kenya uses Celsius
        let temp = qd.value(1, qd.grid_size() / 2, 0, 0);
        if let Some(t) = temp {
            assert!(
                t > -100.0 && t < 100.0,
                "Temperature {t} out of range (Celsius)"
            );
        }
    }

    #[test]
    fn grid_lonlat_corners() {
        let path = test_file();
        assert!(path.exists(), "ecmwf-kenya fixture missing");

        let qd = QueryData::open(&path).unwrap();

        // First grid corner (index 0) is near (34, 4.75) — note this fixture's
        // index 0 is the *north*west corner (lat 4.75), not a "bottom-left".
        let (lon, lat) = qd.grid_lonlat(0);
        assert!((lon - 34.0).abs() < 0.5, "corner0 lon={lon}");
        assert!((lat - 4.75).abs() < 0.5, "corner0 lat={lat}");

        // Opposite corner (last index) is near (41.5, -5.25).
        let last_idx = qd.grid_size() - 1;
        let (lon, lat) = qd.grid_lonlat(last_idx);
        assert!((lon - 41.5).abs() < 0.5, "cornerN lon={lon}");
        assert!((lat - (-5.25)).abs() < 0.5, "cornerN lat={lat}");
    }

    #[test]
    fn param_lookup() {
        let path = test_file();
        assert!(path.exists(), "ecmwf-kenya fixture missing");

        let qd = QueryData::open(&path).unwrap();
        // By numeric ID
        assert!(qd.param_index(1).is_some()); // MSLP
        assert!(qd.param_index(99999).is_none());

        // By full name
        assert!(qd.param_index_by_name("2 Metre Temperature (2t)").is_some());

        // By short name (content inside parentheses)
        assert_eq!(
            qd.param_index_by_name("2t"),
            qd.param_index_by_name("2 Metre Temperature (2t)")
        );
        assert_eq!(
            qd.param_index_by_name("msl"),
            qd.param_index_by_name("Mean Sea Level Pressure (msl)")
        );

        // By numeric ID as string
        assert_eq!(qd.param_index_by_name("1"), qd.param_index(1));

        // No match
        assert!(qd.param_index_by_name("nonexistent").is_none());
    }

    #[test]
    fn parse_meps_lcc() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/meps");
        let files: Vec<_> = std::fs::read_dir(&path)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "sqd"))
            .collect();
        assert!(!files.is_empty(), "meps fixture missing in {path:?}");

        let qd = QueryData::open(&files[0].path()).unwrap();

        assert!(matches!(
            qd.grid.area.crs,
            Crs::LambertConformalConic { .. }
        ));
        assert!(qd.grid.nx > 0);
        assert!(qd.grid.ny > 0);
        // Cropped fixture shape (see testdata/QUERYDATA_FIXTURES.md): 2 params,
        // 3 timesteps — assert exactly so a regression in the LCC parse path
        // can't pass with 0 params / 1 time.
        assert_eq!(qd.params.len(), 2);
        assert_eq!(qd.times.len(), 3);
    }
}
