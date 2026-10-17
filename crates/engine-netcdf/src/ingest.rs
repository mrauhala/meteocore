//! Opening one file and reading what the catalog needs from its metadata:
//! its grid, valid times and model run, and how to read each served
//! variable's 2-D field.
//!
//! Metadata comes only through `NcFile::variables()`. `variable(name)`
//! builds a second, separate metadata tree and is never called. Values come
//! only through slice reads (`read_variable_slice*`): `read_variable*` keeps
//! every decoded dataset of up to 32 MiB in a `OnceLock` that is never
//! evicted. The one exception is a scalar `forecast_reference_time`, eight
//! bytes. See the crate's CLAUDE.md.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::cf::{self, AxisRole, CfAttr, RegularAxis};
use ds_core::geo::{Crs, GeoTransform};
use netcdf_reader::{
    DynStorage, FileStorage, NcAttrValue, NcFile, NcFormat, NcOpenOptions, NcSliceInfo,
    NcSliceInfoElem, NcType, NcVariable,
};

/// netCDF's default fill for `float` and `double`, which unwritten values
/// hold when a variable declares no `_FillValue`.
const DEFAULT_FILL: f64 = 9.969_209_968_386_869e36;

/// Open `path` for positional reads. The reader's own chunk cache is off:
/// decoded fields are cached engine-wide ([`crate::field`]), which also
/// shares one fill between concurrent readers, as the reader's cache does
/// not.
pub(crate) fn open(path: &Path) -> Result<(NcFile, DynStorage), String> {
    let storage: DynStorage =
        Arc::new(FileStorage::open(path).map_err(|e| format!("cannot open: {e}"))?);
    Ok((open_storage(storage.clone())?, storage))
}

/// [`open`] over any storage.
pub(crate) fn open_storage(storage: DynStorage) -> Result<NcFile, String> {
    NcFile::from_storage_with_options(
        storage,
        NcOpenOptions {
            chunk_cache_bytes: 0,
            ..NcOpenOptions::default()
        },
    )
    .map_err(|e| format!("not a readable NetCDF file: {e}"))
}

/// The stored type of a field, read through the matching typed slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stored {
    Byte,
    UByte,
    Short,
    UShort,
    Int,
    UInt,
    Float,
    Double,
}

impl Stored {
    fn of(dtype: &NcType) -> Option<Self> {
        Some(match dtype {
            NcType::Byte => Self::Byte,
            NcType::UByte => Self::UByte,
            NcType::Short => Self::Short,
            NcType::UShort => Self::UShort,
            NcType::Int => Self::Int,
            NcType::UInt => Self::UInt,
            NcType::Float => Self::Float,
            NcType::Double => Self::Double,
            _ => return None,
        })
    }

    pub(crate) fn size(self) -> u64 {
        match self {
            Self::Byte | Self::UByte => 1,
            Self::Short | Self::UShort => 2,
            Self::Int | Self::UInt | Self::Float => 4,
            Self::Double => 8,
        }
    }

    /// `value` as this type holds it: a fill declared in another type is
    /// compared with the stored values exactly.
    fn normalize(self, value: f64) -> f64 {
        match self {
            Self::Byte => value as i8 as f64,
            Self::UByte => value as u8 as f64,
            Self::Short => value as i16 as f64,
            Self::UShort => value as u16 as f64,
            Self::Int => value as i32 as f64,
            Self::UInt => value as u32 as f64,
            Self::Float => value as f32 as f64,
            Self::Double => value,
        }
    }
}

/// CF packing and masking: a stored value equal to one of `fill`
/// (`_FillValue`, `missing_value`; both in stored units) or NaN is nodata,
/// any other is `stored * scale + offset`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Packing {
    pub scale: f64,
    pub offset: f64,
    pub fill: Vec<f64>,
}

impl Packing {
    /// The physical value of `raw`, NaN for nodata.
    #[inline]
    pub(crate) fn decode(&self, raw: f64) -> f32 {
        if raw.is_nan() || self.fill.contains(&raw) {
            f32::NAN
        } else {
            (raw * self.scale + self.offset) as f32
        }
    }
}

/// What a field read selects along one of the variable's dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dim {
    /// The time axis, at the step read.
    Time,
    /// A fixed index: a level of one, or the configured ensemble member.
    Index(u64),
    /// The whole y or x axis, of this many cells.
    Whole(u64),
}

/// How to read one variable's 2-D field from one file.
#[derive(Debug, Clone)]
pub(crate) struct FieldRead {
    pub name: Arc<str>,
    pub dims: Vec<Dim>,
    pub stored: Stored,
    pub packing: Packing,
    /// Decoded bytes of one chunk the read inflates: the field's own size
    /// for a contiguous variable or a field-sized chunk.
    pub chunk_bytes: u64,
    /// Bytes of the field in its stored type.
    pub field_bytes: u64,
}

impl FieldRead {
    /// The field's `(rows, columns)`: its last two dimensions.
    pub(crate) fn shape(&self) -> (usize, usize) {
        match self.dims[..] {
            [.., Dim::Whole(ny), Dim::Whole(nx)] => (ny as usize, nx as usize),
            _ => (0, 0),
        }
    }

    /// Bytes one read of the field decodes, from the metadata alone: its
    /// `f32` values, or the whole chunk when a chunk spans several fields.
    /// 4.06 MB for a MEPS field. The EDR query-shape cap counts it
    /// ([`crate::edr`]).
    pub(crate) fn decoded_bytes(&self) -> u64 {
        let (ny, nx) = self.shape();
        ((ny * nx) as u64 * 4).max(self.chunk_bytes)
    }

    /// The hyperslab of time step `time`.
    pub(crate) fn selection(&self, time: u32) -> NcSliceInfo {
        let selections = self
            .dims
            .iter()
            .map(|dim| match *dim {
                Dim::Time => NcSliceInfoElem::Index(u64::from(time)),
                Dim::Index(i) => NcSliceInfoElem::Index(i),
                Dim::Whole(n) => NcSliceInfoElem::Slice {
                    start: 0,
                    end: n,
                    step: 1,
                },
            })
            .collect();
        NcSliceInfo { selections }
    }
}

/// A level a single-level field sits at, identified by its coordinate's
/// value and units, never by its dimension's name: MEPS puts 10 m wind on
/// `height6`, `height7` or `height2` depending on the file family.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Level {
    pub value: f64,
    pub units: String,
}

/// A served variable, as the layer advertises it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Variable {
    pub name: Arc<str>,
    /// `long_name`, else the name.
    pub title: String,
    /// `units` as the file spells it, empty when absent.
    pub units: String,
    /// CF `standard_name`, when the file states one: EDR's observed
    /// property, and what identifies wind components (`x_wind`).
    pub standard_name: Option<String>,
    pub levels: Vec<Level>,
}

impl Variable {
    /// Whether `other` holds this variable's quantity at its levels: the
    /// same units and level units in any spelling
    /// (`ds_core::units::same_unit`: `kg/m^2` is `kg m-2`), the same level
    /// values. The served spelling stays the first file's.
    pub(crate) fn same_quantity(&self, other: &Variable) -> bool {
        use ds_core::units::same_unit;
        same_unit(&self.units, &other.units)
            && self.levels.len() == other.levels.len()
            && self
                .levels
                .iter()
                .zip(&other.levels)
                .all(|(a, b)| a.value == b.value && same_unit(&a.units, &b.units))
    }
}

/// A file's grid: the CF grid of its fields' `(y, x)` axes.
#[derive(Debug, Clone)]
pub(crate) struct FileGrid {
    pub transform: GeoTransform,
    /// The stored rows run south to north and are reversed when decoded.
    pub flipped: bool,
}

impl FileGrid {
    /// Whether `other` is the same grid, to a thousandth of a cell.
    pub(crate) fn same(&self, other: &FileGrid) -> bool {
        let (a, b) = (&self.transform, &other.transform);
        let close = |x: f64, y: f64, cell: f64| (x - y).abs() <= cell.abs() * 1e-3;
        self.flipped == other.flipped
            && a.crs == b.crs
            && (a.width, a.height) == (b.width, b.height)
            && close(a.origin_x, b.origin_x, a.pixel_width)
            && close(a.origin_y, b.origin_y, a.pixel_height)
            && close(a.pixel_width, b.pixel_width, a.pixel_width)
            && close(a.pixel_height, b.pixel_height, a.pixel_height)
    }
}

/// One served variable of one file.
#[derive(Debug, Clone)]
pub(crate) struct Field {
    pub variable: Variable,
    pub read: FieldRead,
}

/// What a file holds, as far as the catalog is concerned.
pub(crate) struct Inspected {
    pub run: Option<DateTime<Utc>>,
    /// Valid time of each index along the fields' time axis.
    pub times: Vec<DateTime<Utc>>,
    pub grid: FileGrid,
    pub fields: Vec<Field>,
    /// Candidate variables not served, with why: logged when the file
    /// sets the collection's layout.
    pub skipped: Vec<(String, String)>,
}

/// Which variables [`inspect`] looks at.
pub(crate) enum Wanted<'a> {
    /// These, in this order; the file fails without any one of them.
    Listed(&'a [String]),
    /// Every variable on one grid, in file order.
    All,
}

/// Read `nc`'s grid, times, run and fields. An `Err` is why the file
/// cannot be served at all.
///
/// `storage` is the file's storage, read once more through hdf5-reader for
/// the chunk shapes, which the NetCDF metadata does not carry.
pub(crate) fn inspect(
    nc: &NcFile,
    storage: &DynStorage,
    wanted: &Wanted,
    member: u32,
) -> Result<Inspected, String> {
    let variables = nc.variables().map_err(|e| format!("metadata: {e}"))?;
    let h5 = matches!(nc.format(), NcFormat::Nc4 | NcFormat::Nc4Classic)
        .then(|| hdf5_reader::Hdf5File::from_storage(storage.clone()).ok())
        .flatten();
    let role = |dim: &str| role(variables, dim);
    let coordinate = |dim: &str| coordinate(variables, dim);

    let candidates: Vec<&NcVariable> = match wanted {
        Wanted::Listed(names) => names
            .iter()
            .map(|name| {
                variables
                    .iter()
                    .find(|v| &v.name == name)
                    .ok_or_else(|| format!("has no variable '{name}'"))
            })
            .collect::<Result<_, _>>()?,
        Wanted::All => variables
            .iter()
            .filter(|v| v.dimensions.len() >= 2 && !v.is_coordinate_variable())
            .filter(|v| {
                let n = v.dimensions.len();
                spatial_pair(
                    role(&v.dimensions[n - 2].name),
                    role(&v.dimensions[n - 1].name),
                )
                .is_some()
            })
            .collect(),
    };

    let mut grid: Option<(String, String, Option<String>, FileGrid)> = None;
    let mut time_dim: Option<String> = None;
    let mut fields = Vec::new();
    let mut skipped = Vec::new();
    for var in candidates {
        let outcome = field(nc, h5.as_ref(), variables, var, member).and_then(|(field, key)| {
            let (y, x, mapping) = &key;
            match &grid {
                Some((gy, gx, gm, _)) if (gy, gx, gm) != (y, x, mapping) => {
                    return Err(format!("is on another grid ({y}, {x})"));
                }
                Some(_) => {}
                None => {
                    let found = file_grid(nc, variables, var, y, x, mapping.as_deref())?;
                    grid = Some((y.clone(), x.clone(), mapping.clone(), found));
                }
            }
            let time = field.time_dim.clone();
            match &time_dim {
                Some(t) if *t != time => Err(format!("is on another time axis ('{time}')")),
                Some(_) => Ok(field.field),
                None => {
                    time_dim = Some(time);
                    Ok(field.field)
                }
            }
        });
        match outcome {
            Ok(field) => fields.push(field),
            Err(reason) => match wanted {
                Wanted::Listed(_) => return Err(format!("variable '{}' {reason}", var.name)),
                Wanted::All => skipped.push((var.name.clone(), reason)),
            },
        }
    }
    let (Some((.., grid)), Some(time_dim)) = (grid, time_dim) else {
        return Err(
            "has no variable on a 1-D latitude/longitude or projected x/y grid with a \
                    time axis"
                .to_string(),
        );
    };
    let time_var = coordinate(&time_dim)
        .ok_or_else(|| format!("time axis '{time_dim}' has no coordinate variable"))?;
    let times = times(nc, time_var)?;
    let run = reference_time(nc, variables)?;
    Ok(Inspected {
        run,
        times,
        grid,
        fields,
        skipped,
    })
}

/// A field before its grid is resolved: the field, its time dimension and
/// its grid key (y dimension, x dimension, grid mapping).
struct Candidate {
    field: Field,
    time_dim: String,
}

type GridKey = (String, String, Option<String>);

/// Classify `var`'s dimensions and read how its field is stored.
fn field(
    nc: &NcFile,
    h5: Option<&hdf5_reader::Hdf5File>,
    variables: &[NcVariable],
    var: &NcVariable,
    member: u32,
) -> Result<(Candidate, GridKey), String> {
    let stored = Stored::of(var.dtype())
        .ok_or_else(|| format!("is {:?}; only numeric fields are served", var.dtype()))?;
    let roles: Vec<(String, u64, AxisRole)> = var
        .dimensions
        .iter()
        .map(|d| (d.name.clone(), d.size, role(variables, &d.name)))
        .collect();
    let shape = classify(&roles, member)?;
    let mut levels = Vec::new();
    for &i in &shape.levels {
        let name = &roles[i].0;
        if let Some(coord) = coordinate(variables, name) {
            let value = read_coordinate(nc, coord)?;
            levels.push(Level {
                value: value[0],
                units: text_attr(coord, "units").unwrap_or_default(),
            });
        }
    }
    let number = |name: &str| var.attribute(name).and_then(|a| a.value.as_f64());
    let mut fill: Vec<f64> = ["_FillValue", "missing_value"]
        .iter()
        .filter_map(|name| var.attribute(name)?.value.as_f64_vec())
        .flatten()
        .map(|v| stored.normalize(v))
        .collect();
    if var.attribute("_FillValue").is_none() && matches!(stored, Stored::Float | Stored::Double) {
        fill.push(stored.normalize(DEFAULT_FILL));
    }
    let packing = Packing {
        scale: number("scale_factor").unwrap_or(1.0),
        offset: number("add_offset").unwrap_or(0.0),
        fill,
    };
    let (ny, nx) = (shape.ny, shape.nx);
    let field_bytes = ny * nx * stored.size();
    let chunk_bytes = chunk_bytes(h5, var, stored).unwrap_or(field_bytes);
    let name: Arc<str> = var.name.as_str().into();
    let key = (
        roles[roles.len() - 2].0.clone(),
        roles[roles.len() - 1].0.clone(),
        text_attr(var, "grid_mapping").map(|m| m.trim().to_string()),
    );
    let field = Field {
        variable: Variable {
            name: name.clone(),
            title: text_attr(var, "long_name")
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| var.name.clone()),
            units: text_attr(var, "units")
                .unwrap_or_default()
                .trim()
                .to_string(),
            standard_name: text_attr(var, "standard_name")
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty()),
            levels,
        },
        read: FieldRead {
            name,
            dims: shape.dims,
            stored,
            packing,
            chunk_bytes,
            field_bytes,
        },
    };
    Ok((
        Candidate {
            field,
            time_dim: shape.time_dim,
        },
        key,
    ))
}

/// A variable's dimensions, classified ([`classify`]).
#[derive(Debug, PartialEq)]
pub(crate) struct Shape {
    pub dims: Vec<Dim>,
    pub time_dim: String,
    /// Indices of the single-level dimensions that are not time,
    /// reference, lead or ensemble: the field's level identity.
    pub levels: Vec<usize>,
    pub ny: u64,
    pub nx: u64,
}

/// Classify a variable's `(name, size, role)` dimensions: `(y, x)` last,
/// one time axis, any ensemble axis read at `member`, and every other axis
/// of size 1. Another axis longer than 1 is a reason not to serve it: a
/// field is never silently pinned to one of its levels.
pub(crate) fn classify(dims: &[(String, u64, AxisRole)], member: u32) -> Result<Shape, String> {
    let n = dims.len();
    if n < 2 {
        return Err("has fewer than two dimensions".to_string());
    }
    let (y, x) = (&dims[n - 2], &dims[n - 1]);
    if spatial_pair(y.2, x.2).is_none() {
        return Err(format!(
            "is not on a 1-D latitude/longitude or projected x/y grid (its last dimensions are \
             '{}', '{}')",
            y.0, x.0
        ));
    }
    let mut out = Vec::with_capacity(n);
    let mut time_dim = None;
    let mut levels = Vec::new();
    for (i, (name, size, role)) in dims[..n - 2].iter().enumerate() {
        match role {
            AxisRole::Time if time_dim.is_none() => {
                time_dim = Some(name.clone());
                out.push(Dim::Time);
            }
            AxisRole::Ensemble => {
                if u64::from(member) >= *size {
                    return Err(format!(
                        "has {size} ensemble members on '{name}'; member {member} is not one"
                    ));
                }
                out.push(Dim::Index(u64::from(member)));
            }
            _ if *size == 1 => {
                if matches!(role, AxisRole::Other) {
                    levels.push(i);
                }
                out.push(Dim::Index(0));
            }
            _ => {
                return Err(format!(
                    "has {size} values on its '{name}' axis; only fields with one level are \
                     served"
                ))
            }
        }
    }
    let time_dim = time_dim.ok_or("has no time axis")?;
    out.push(Dim::Whole(y.1));
    out.push(Dim::Whole(x.1));
    Ok(Shape {
        dims: out,
        time_dim,
        levels,
        ny: y.1,
        nx: x.1,
    })
}

/// Whether `(y, x)` roles form a supported horizontal grid: latitude and
/// longitude, or projected y and x.
fn spatial_pair(y: AxisRole, x: AxisRole) -> Option<()> {
    matches!(
        (y, x),
        (AxisRole::Lat, AxisRole::Lon) | (AxisRole::ProjectionY, AxisRole::ProjectionX)
    )
    .then_some(())
}

/// The grid of `var`'s `(y, x)` axes: CRS84 for latitude/longitude axes,
/// else the CRS of its `grid_mapping`.
fn file_grid(
    nc: &NcFile,
    variables: &[NcVariable],
    var: &NcVariable,
    y: &str,
    x: &str,
    mapping: Option<&str>,
) -> Result<FileGrid, String> {
    let crs = match role(variables, x) {
        AxisRole::Lon => Crs::Wgs84,
        _ => {
            let mapping = mapping
                .ok_or_else(|| format!("has projected axes '{y}', '{x}' but no grid_mapping"))?;
            let mapping_var = variables
                .iter()
                .find(|v| v.name == mapping)
                .ok_or_else(|| format!("names grid_mapping '{mapping}', which does not exist"))?;
            let attr = |name: &str| mapping_var.attribute(name).and_then(|a| cf_attr(&a.value));
            cf::crs_from_grid_mapping(attr).map_err(|e| format!("grid mapping '{mapping}': {e}"))?
        }
    };
    let axis = |dim: &str| -> Result<RegularAxis, String> {
        let coord = coordinate(variables, dim)
            .ok_or_else(|| format!("axis '{dim}' has no coordinate variable"))?;
        let values = read_coordinate(nc, coord)?;
        RegularAxis::from_values(dim, &values, &text_attr(coord, "units").unwrap_or_default())
    };
    let grid =
        cf::grid(crs, &axis(x)?, &axis(y)?).map_err(|e| format!("variable '{}': {e}", var.name))?;
    Ok(FileGrid {
        transform: grid.transform,
        flipped: grid.flipped,
    })
}

/// A whole 1-D (or scalar) coordinate variable, CF packing applied: a slice
/// read, so nothing stays cached in the reader.
fn read_coordinate(nc: &NcFile, var: &NcVariable) -> Result<Vec<f64>, String> {
    let read = |e: netcdf_reader::Error| format!("coordinate '{}': {e}", var.name);
    let values = if var.dimensions.is_empty() {
        nc.read_variable_as_f64(&var.name).map_err(read)?
    } else {
        let selections = var
            .dimensions
            .iter()
            .map(|d| NcSliceInfoElem::Slice {
                start: 0,
                end: d.size,
                step: 1,
            })
            .collect();
        nc.read_variable_slice_as_f64(&var.name, &NcSliceInfo { selections })
            .map_err(read)?
    };
    let number = |name: &str| var.attribute(name).and_then(|a| a.value.as_f64());
    let (scale, offset) = (
        number("scale_factor").unwrap_or(1.0),
        number("add_offset").unwrap_or(0.0),
    );
    let values: Vec<f64> = values.iter().map(|v| v * scale + offset).collect();
    if values.is_empty() {
        return Err(format!("coordinate '{}' is empty", var.name));
    }
    Ok(values)
}

/// The valid times of the time coordinate `var`.
fn times(nc: &NcFile, var: &NcVariable) -> Result<Vec<DateTime<Utc>>, String> {
    let units = text_attr(var, "units")
        .ok_or_else(|| format!("time coordinate '{}' has no units", var.name))?;
    let calendar = text_attr(var, "calendar");
    if !cf::is_standard_calendar(calendar.as_deref()) {
        return Err(format!(
            "time coordinate '{}' uses the calendar '{}', which is not served",
            var.name,
            calendar.unwrap_or_default()
        ));
    }
    let raw = read_coordinate(nc, var)?;
    cf::decode_times(&raw, &units).map_err(|e| format!("time coordinate '{}': {e}", var.name))
}

/// The model run of the file: its `forecast_reference_time` variable,
/// scalar or 1-D with one value, or `None` without one.
fn reference_time(nc: &NcFile, variables: &[NcVariable]) -> Result<Option<DateTime<Utc>>, String> {
    let Some(var) = variables
        .iter()
        .find(|v| text_attr(v, "standard_name").as_deref() == Some("forecast_reference_time"))
    else {
        return Ok(None);
    };
    let units = text_attr(var, "units").ok_or_else(|| format!("'{}' has no units", var.name))?;
    let raw = read_coordinate(nc, var)?;
    if raw.iter().any(|&v| v != raw[0]) {
        return Err(format!(
            "'{}' holds several model runs; a file must hold one",
            var.name
        ));
    }
    let run = cf::decode_times(&raw[..1], &units).map_err(|e| format!("'{}': {e}", var.name))?;
    Ok(run.first().copied())
}

/// Decoded bytes of one of `var`'s chunks, when it is chunked: from the
/// HDF5 layout. `None` for a contiguous variable or a classic file.
fn chunk_bytes(
    h5: Option<&hdf5_reader::Hdf5File>,
    var: &NcVariable,
    stored: Stored,
) -> Option<u64> {
    let chunks = h5?.dataset(&var.name).ok()?.chunks()?;
    chunks
        .iter()
        .try_fold(stored.size(), |bytes, &n| bytes.checked_mul(u64::from(n)))
}

/// The coordinate variable of dimension `dim`: 1-D, named after it.
fn coordinate<'a>(variables: &'a [NcVariable], dim: &str) -> Option<&'a NcVariable> {
    variables
        .iter()
        .find(|v| v.name == dim && v.dimensions.len() == 1 && v.dimensions[0].name == dim)
}

/// The role of dimension `dim`, from its coordinate variable's
/// `standard_name` and `units`, else its name.
fn role(variables: &[NcVariable], dim: &str) -> AxisRole {
    let coord = coordinate(variables, dim);
    let text = |name: &str| coord.and_then(|c| text_attr(c, name));
    cf::classify_axis(
        dim,
        text("standard_name").as_deref(),
        text("units").as_deref(),
    )
}

fn text_attr(var: &NcVariable, name: &str) -> Option<String> {
    var.attribute(name).and_then(|a| a.value.as_string())
}

/// A grid-mapping attribute for [`cf::crs_from_grid_mapping`]: a numeric
/// attribute of several elements stays whole, so a parameter that takes one
/// number rejects it rather than reading its first.
fn cf_attr(value: &NcAttrValue) -> Option<CfAttr> {
    match value {
        NcAttrValue::Chars(_) | NcAttrValue::Strings(_) => value.as_string().map(CfAttr::Text),
        _ => {
            let mut values = value.as_f64_vec()?;
            match values.len() {
                0 => None,
                1 => values.pop().map(CfAttr::Number),
                _ => Some(CfAttr::Numbers(values)),
            }
        }
    }
}

#[cfg(test)]
mod tests;
