//! Opening one file and reading what the catalog needs from its metadata:
//! its grid, valid times and model run, and how to read each served
//! variable's 2-D field.
//!
//! Everything is read through a [`Reader`]: a NetCDF file, or a remote one
//! through OPeNDAP, whose DDS and DAS become the same `NcVariable`s
//! (#1264). From a NetCDF file, metadata comes only through
//! `NcFile::variables()`. `variable(name)` builds a second, separate
//! metadata tree and is never called. Values come only through slice reads
//! (`read_variable_slice*`): `read_variable*` keeps every decoded dataset
//! of up to 32 MiB in a `OnceLock` that is never evicted. The one exception
//! is a scalar `forecast_reference_time`, eight bytes. See the crate's
//! CLAUDE.md.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::cf::{self, AxisRole, CfAttr, PressureUnit, RegularAxis};
use ds_core::geo::{Crs, GeoTransform};
use ds_core::resample::{CellWindow, CurvilinearNodes};

use crate::decode_budget::BUDGET;
use crate::source::Reader;
use netcdf_reader::{
    DynStorage, FileStorage, NcAttrValue, NcFile, NcOpenOptions, NcSliceInfo, NcSliceInfoElem,
    NcType, NcVariable,
};

/// netCDF's default fill for `float` and `double`, which unwritten values
/// hold when a variable declares no `_FillValue`.
const DEFAULT_FILL: f64 = 9.969_209_968_386_869e36;

/// Open `path` for positional reads. The reader's own chunk cache is off:
/// decoded fields are cached engine-wide ([`crate::field`]), which also
/// shares one fill between concurrent readers, as the reader's cache does
/// not.
pub(crate) fn open(path: &Path) -> Result<Reader, String> {
    let storage: DynStorage =
        Arc::new(FileStorage::open(path).map_err(|e| format!("cannot open: {e}"))?);
    Ok(Reader::Nc {
        nc: open_storage(storage.clone())?,
        storage,
        remote: None,
    })
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
    /// The pressure axis, at the level read ([`FieldRead::levels`]).
    Level,
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
    /// The stored index along the pressure axis of each served level,
    /// bottom first ([`Variable::pressure`]'s order); empty for a field
    /// with one level.
    pub levels: Vec<u64>,
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
        (ny as u64)
            .saturating_mul(nx as u64)
            .saturating_mul(4)
            .max(self.chunk_bytes)
    }

    /// The hyperslab of time step `time` at served level `level` (an index
    /// into [`Self::levels`]; 0 for a field with one level).
    pub(crate) fn selection(&self, time: u32, level: usize) -> NcSliceInfo {
        let selections = self
            .dims
            .iter()
            .map(|dim| match *dim {
                Dim::Time => NcSliceInfoElem::Index(u64::from(time)),
                Dim::Index(i) => NcSliceInfoElem::Index(i),
                Dim::Level => NcSliceInfoElem::Index(self.levels.get(level).copied().unwrap_or(0)),
                Dim::Whole(n) => NcSliceInfoElem::Slice {
                    start: 0,
                    end: n,
                    step: 1,
                },
            })
            .collect();
        NcSliceInfo { selections }
    }

    /// [`Self::selection`] of `window`'s cells alone (#1171): its rows and
    /// columns, rows counted north first as the sampler counts them, so a
    /// field stored south-up (`flipped`) reads the stored rows they are,
    /// reversed when decoded. `None` when the field's last two axes are not
    /// its grid's or the window passes their end.
    pub(crate) fn window_selection(
        &self,
        time: u32,
        level: usize,
        window: CellWindow,
        flipped: bool,
    ) -> Option<NcSliceInfo> {
        let (ny, nx) = self.shape();
        let (ny, nx) = (ny as u64, nx as u64);
        let (rows, columns) = (window.rows, window.columns);
        if u64::from(rows.1) >= ny || u64::from(columns.1) >= nx {
            return None;
        }
        let rows = if flipped {
            (ny - 1 - u64::from(rows.1), ny - 1 - u64::from(rows.0))
        } else {
            (u64::from(rows.0), u64::from(rows.1))
        };
        let slice = |(start, last): (u64, u64)| NcSliceInfoElem::Slice {
            start,
            end: last + 1,
            step: 1,
        };
        let mut selection = self.selection(time, level);
        let n = selection.selections.len();
        selection.selections[n - 2] = slice(rows);
        selection.selections[n - 1] = slice((u64::from(columns.0), u64::from(columns.1)));
        Some(selection)
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
    /// `units` as the file spells it, empty when absent: the stored
    /// values' unit. The layer is served in its display unit where
    /// `ds_core::units::cf_display_conversion` has a rule for it and the
    /// standard name (`Layout::display`, #1209).
    pub units: String,
    /// CF `standard_name`, when the file states one: EDR's observed
    /// property, what identifies wind components (`x_wind`), and whether a
    /// value in unit `1` is a fraction served in % (`relative_humidity`).
    pub standard_name: Option<String>,
    /// The identity of a field with one level ([`Level`]): every size-1
    /// axis that is not time, run, lead or ensemble.
    pub levels: Vec<Level>,
    /// The pressure levels of a field on a pressure axis (#1067 PR 8), in
    /// hPa, bottom first: the largest pressure first, as engine-grib orders
    /// its pressure views, so a map without `ELEVATION` draws the lowest.
    /// `None` for a field with one level.
    pub pressure: Option<Vec<f64>>,
}

impl Variable {
    /// Whether `other` holds this variable's quantity at its levels: the
    /// same units and level units in any spelling
    /// (`ds_core::units::same_unit`: `kg/m^2` is `kg m-2`), the same level
    /// values, and the same pressure levels (in hPa, whatever unit and
    /// order each file stores them in). The first file's unit decides the
    /// display conversion, and its spelling is served where none applies:
    /// one canonical unit has one rule, so every file converts alike.
    pub(crate) fn same_quantity(&self, other: &Variable) -> bool {
        use ds_core::units::same_unit;
        same_unit(&self.units, &other.units)
            && self.pressure == other.pressure
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
pub(crate) enum FileGrid {
    /// 1-D latitude/longitude or projected x/y axes: a regular grid, its
    /// transform north-up. `flipped`: the stored rows run south to north
    /// and are reversed when decoded.
    Regular {
        transform: GeoTransform,
        flipped: bool,
    },
    /// 2-D latitude and longitude auxiliaries and no CRS (#1174): each
    /// node's position, read once at ingest. Rows are drawn as stored.
    Curvilinear(Arc<CurvilinearNodes>),
}

impl FileGrid {
    /// A regular grid's transform; `None` on a curvilinear grid.
    pub(crate) fn transform(&self) -> Option<&GeoTransform> {
        match self {
            FileGrid::Regular { transform, .. } => Some(transform),
            FileGrid::Curvilinear(_) => None,
        }
    }

    /// Whether the stored rows are reversed when decoded: a regular grid
    /// stored south to north. Never a curvilinear one.
    pub(crate) fn flipped(&self) -> bool {
        matches!(self, FileGrid::Regular { flipped: true, .. })
    }

    /// `(columns, rows)`.
    pub(crate) fn size(&self) -> (u32, u32) {
        match self {
            FileGrid::Regular { transform, .. } => (transform.width, transform.height),
            FileGrid::Curvilinear(nodes) => {
                let (nx, ny) = nodes.size();
                (nx as u32, ny as u32)
            }
        }
    }

    /// Whether `other` is the same grid: a regular one to a thousandth of
    /// a cell, a curvilinear one node by node to a thousandth of each
    /// node's spacing ([`CurvilinearNodes::same`]).
    pub(crate) fn same(&self, other: &FileGrid) -> bool {
        match (self, other) {
            (
                FileGrid::Regular {
                    transform: a,
                    flipped,
                },
                FileGrid::Regular {
                    transform: b,
                    flipped: other_flipped,
                },
            ) => {
                let close = |x: f64, y: f64, cell: f64| (x - y).abs() <= cell.abs() * 1e-3;
                flipped == other_flipped
                    && a.crs == b.crs
                    && (a.width, a.height) == (b.width, b.height)
                    && close(a.origin_x, b.origin_x, a.pixel_width)
                    && close(a.origin_y, b.origin_y, a.pixel_height)
                    && close(a.pixel_width, b.pixel_width, a.pixel_width)
                    && close(a.pixel_height, b.pixel_height, a.pixel_height)
            }
            (FileGrid::Curvilinear(a), FileGrid::Curvilinear(b)) => Arc::ptr_eq(a, b) || a.same(b),
            _ => false,
        }
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

/// Why [`inspect`] serves nothing from a file.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unservable {
    /// The file cannot be served as it is: remembered, and not opened
    /// again until it changes.
    #[error("{0}")]
    File(String),
    /// Not now: the decode budget (`MC_NETCDF_READ_MEMORY_MB`) had no room
    /// to read a curvilinear grid's 2-D latitude/longitude (#1174). The
    /// poll forgets the file and opens it again next poll.
    #[error("{0}")]
    Busy(String),
}

impl From<String> for Unservable {
    fn from(reason: String) -> Self {
        Unservable::File(reason)
    }
}

/// Which variables [`inspect`] looks at.
pub(crate) enum Wanted<'a> {
    /// These, in this order; the file fails without any one of them.
    Listed(&'a [String]),
    /// Every variable on one grid, in file order.
    All,
}

/// Read the grid, times, run and fields of the file `reader` reads. An
/// `Err` is why the file cannot be served at all ([`Unservable::File`]),
/// or cannot be read now ([`Unservable::Busy`]).
///
/// A NetCDF-4 file's storage is read once more through hdf5-reader for the
/// chunk shapes, which the NetCDF metadata does not carry
/// ([`Reader::hdf5`]).
pub(crate) fn inspect(
    reader: &Reader,
    wanted: &Wanted,
    member: u32,
) -> Result<Inspected, Unservable> {
    let variables = reader.variables()?;
    let h5 = reader.hdf5();
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
            .filter(|v| placement(variables, v).is_some())
            .collect(),
    };

    let mut grid: Option<(GridKey, FileGrid)> = None;
    let mut time_dim: Option<String> = None;
    let mut fields = Vec::new();
    let mut skipped = Vec::new();
    for var in candidates {
        let outcome = match field(reader, h5.as_ref(), variables, var, member) {
            Err(reason) => Err(reason),
            Ok((field, key)) => match &grid {
                Some((found, _)) if *found != key => {
                    let (y, x, _) = &key;
                    Err(format!("is on another grid ({y}, {x})"))
                }
                Some(_) => Ok(field),
                None => {
                    let (y, x, placement) = &key;
                    match file_grid(reader, variables, var, y, x, placement) {
                        Ok(found) => {
                            grid = Some((key, found));
                            Ok(field)
                        }
                        Err(Unservable::File(reason)) => Err(reason),
                        // Not this variable's fault: the whole file waits.
                        Err(busy) => return Err(busy),
                    }
                }
            },
        };
        let outcome = outcome.and_then(|field| {
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
                Wanted::Listed(_) => return Err(format!("variable '{}' {reason}", var.name).into()),
                Wanted::All => skipped.push((var.name.clone(), reason)),
            },
        }
    }
    let fields = one_vertical_axis(fields, wanted, &mut skipped)?;
    let (Some((_, grid)), Some(time_dim)) = (grid, time_dim) else {
        return Err(
            "has no variable on a 1-D latitude/longitude or projected x/y grid, or on 2-D \
             latitude/longitude, with a time axis"
                .to_string()
                .into(),
        );
    };
    let time_var = coordinate(&time_dim)
        .ok_or_else(|| format!("time axis '{time_dim}' has no coordinate variable"))?;
    let times = times(reader, time_var)?;
    let run = reference_time(reader, variables)?;
    Ok(Inspected {
        run,
        times,
        grid,
        fields,
        skipped,
    })
}

/// The fields a collection serves on its one vertical axis (#1067 PR 8): a
/// collection's variables all sit on the same pressure levels, or none has
/// any, since `RasterInfo::vertical` is one axis that `ELEVATION` and EDR
/// `z` select on for every layer. MEPS keeps its pressure-level fields in
/// their own file family (`pl`), served as their own collection.
///
/// With an allowlist the first listed variable decides, and a variable on
/// another axis fails the file, naming it. Without one, the fields are
/// grouped by their levels (one level, or each distinct set of pressure
/// levels) and the largest group wins: single-level fields on a tie, as
/// before pressure levels were served, else the first group found. The
/// others are skipped with why.
fn one_vertical_axis(
    fields: Vec<Field>,
    wanted: &Wanted,
    skipped: &mut Vec<(String, String)>,
) -> Result<Vec<Field>, String> {
    let axis: Option<Vec<f64>> = match wanted {
        Wanted::Listed(_) => fields.first().and_then(|f| f.variable.pressure.clone()),
        Wanted::All => {
            let mut groups: Vec<(Option<&Vec<f64>>, usize)> = Vec::new();
            for field in &fields {
                let levels = field.variable.pressure.as_ref();
                match groups.iter_mut().find(|(group, _)| *group == levels) {
                    Some((_, count)) => *count += 1,
                    None => groups.push((levels, 1)),
                }
            }
            let single = groups
                .iter()
                .find(|(levels, _)| levels.is_none())
                .map_or(0, |&(_, count)| count);
            let mut best: (Option<&Vec<f64>>, usize) = (None, single);
            for &(levels, count) in &groups {
                if levels.is_some() && count > best.1 {
                    best = (levels, count);
                }
            }
            best.0.cloned()
        }
    };
    let mut kept = Vec::with_capacity(fields.len());
    for field in fields {
        let reason = match (&axis, &field.variable.pressure) {
            (None, None) => None,
            (Some(axis), Some(levels)) if axis == levels => None,
            (Some(axis), Some(levels)) => Some(format!(
                "is on the pressure levels {} hPa, the collection's other variables on {} hPa",
                list_levels(levels),
                list_levels(axis)
            )),
            (Some(_), None) => Some(
                "has one level, while the collection's other variables are on pressure \
                 levels: a collection serves one vertical axis"
                    .to_string(),
            ),
            (None, Some(levels)) => Some(format!(
                "is on {} pressure levels, while the collection's other variables have one \
                 level: a collection serves one vertical axis",
                levels.len()
            )),
        };
        match (reason, wanted) {
            (None, _) => kept.push(field),
            (Some(reason), Wanted::Listed(_)) => {
                return Err(format!("variable '{}' {reason}", field.variable.name))
            }
            (Some(reason), Wanted::All) => skipped.push((field.variable.name.to_string(), reason)),
        }
    }
    Ok(kept)
}

/// Levels as a short list: `1000, 850, 500`.
pub(crate) fn list_levels(levels: &[f64]) -> String {
    levels
        .iter()
        .map(f64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A field before its grid is resolved: the field, its time dimension and
/// its grid key (y dimension, x dimension, grid mapping).
struct Candidate {
    field: Field,
    time_dim: String,
}

/// A field's grid: its y and x dimensions and how they are placed.
type GridKey = (String, String, Placement);

/// How a variable's `(y, x)` dimensions are placed on the Earth.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Placement {
    /// 1-D coordinate variables: latitude/longitude, or projected x/y
    /// placed by the variable's `grid_mapping`, named here.
    Axes(Option<String>),
    /// 2-D latitude and longitude auxiliaries (#1174), named by the
    /// variable's CF `coordinates`: a curvilinear grid.
    Auxiliary { lat: String, lon: String },
}

/// How `var`'s last two dimensions, `(y, x)`, are placed, `None` when
/// nothing places them.
///
/// Its axes when their roles form a grid ([`spatial_pair`]: latitude and
/// longitude, or projected y and x), as before 2-D coordinates were read.
/// Otherwise its 2-D latitude and longitude auxiliaries
/// ([`auxiliaries`]): a curvilinear grid. Where it has both, the axes win
/// only as their coordinate variables' own attributes state them, never
/// by a dimension's name (a bare index axis `x` reads as a longitude by
/// name): latitude/longitude, or projected x/y with a `grid_mapping`. So
/// MEPS, whose fields carry 2-D `latitude`/`longitude` beside their
/// Lambert axes, stays a projected grid and its 2-D arrays stay unread.
fn placement(variables: &[NcVariable], var: &NcVariable) -> Option<Placement> {
    let n = var.dimensions.len();
    if n < 2 {
        return None;
    }
    let (y, x) = (&var.dimensions[n - 2].name, &var.dimensions[n - 1].name);
    let mapping = text_attr(var, "grid_mapping").map(|m| m.trim().to_string());
    let Some((lat, lon)) = auxiliaries(variables, var, y, x) else {
        return spatial_pair(role(variables, y), role(variables, x))
            .map(|()| Placement::Axes(mapping));
    };
    let stated = |dim: &str| coordinate(variables, dim).map(stated_role);
    match (stated(y), stated(x)) {
        (Some(AxisRole::Lat), Some(AxisRole::Lon)) => Some(Placement::Axes(mapping)),
        (Some(AxisRole::ProjectionY), Some(AxisRole::ProjectionX)) if mapping.is_some() => {
            Some(Placement::Axes(mapping))
        }
        _ => Some(Placement::Auxiliary { lat, lon }),
    }
}

/// The 2-D latitude and longitude auxiliaries of `var` on its `(y, x)`:
/// the variables its CF `coordinates` attribute names whose dimensions are
/// exactly `(y, x)`, a latitude and a longitude by their `standard_name`
/// or `units` ([`stated_role`]), never by their names. `None` unless there
/// is exactly one of each.
fn auxiliaries(
    variables: &[NcVariable],
    var: &NcVariable,
    y: &str,
    x: &str,
) -> Option<(String, String)> {
    let names = text_attr(var, "coordinates")?;
    let (mut lat, mut lon) = (Vec::new(), Vec::new());
    for name in names.split_whitespace() {
        let Some(aux) = variables.iter().find(|v| v.name == name) else {
            continue;
        };
        if !matches!(&aux.dimensions[..], [a, b] if a.name == y && b.name == x) {
            continue;
        }
        match stated_role(aux) {
            AxisRole::Lat => lat.push(name),
            AxisRole::Lon => lon.push(name),
            _ => {}
        }
    }
    match (&lat[..], &lon[..]) {
        ([lat], [lon]) => Some((lat.to_string(), lon.to_string())),
        _ => None,
    }
}

/// The role `var`'s own `standard_name` and `units` state, without the
/// name fallback [`cf::classify_axis`] applies to a dimension.
fn stated_role(var: &NcVariable) -> AxisRole {
    cf::classify_axis(
        "",
        text_attr(var, "standard_name").as_deref(),
        text_attr(var, "units").as_deref(),
    )
}

/// Classify `var`'s dimensions and read how its field is stored.
fn field(
    reader: &Reader,
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
    let placement = placement(variables, var);
    let curvilinear = matches!(placement, Some(Placement::Auxiliary { .. }));
    let shape = classify(&roles, member, curvilinear, |dim| {
        pressure_unit(variables, dim).is_some()
    })?;
    let (pressure, level_index) = match shape.vertical {
        Some(i) => pressure_levels(reader, variables, &roles[i].0, roles[i].1)?,
        None => (None, Vec::new()),
    };
    let mut levels = Vec::new();
    for &i in &shape.levels {
        let name = &roles[i].0;
        if let Some(coord) = coordinate(variables, name) {
            let value = read_coordinate(reader, coord)?;
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
    // Saturating: an OPeNDAP DDS states any size it likes, and a field
    // past the decode budget is refused before it is read (`crate::field`).
    let field_bytes = ny.saturating_mul(nx).saturating_mul(stored.size());
    let chunk_bytes = chunk_bytes(h5, var, stored).unwrap_or(field_bytes);
    let name: Arc<str> = var.name.as_str().into();
    let key = (
        roles[roles.len() - 2].0.clone(),
        roles[roles.len() - 1].0.clone(),
        placement.unwrap_or(Placement::Axes(None)),
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
            pressure,
        },
        read: FieldRead {
            name,
            dims: shape.dims,
            stored,
            packing,
            chunk_bytes,
            field_bytes,
            levels: level_index,
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
    /// Index of the pressure axis ([`Dim::Level`]), when the field has one.
    pub vertical: Option<usize>,
    pub ny: u64,
    pub nx: u64,
}

/// Classify a variable's `(name, size, role)` dimensions: `(y, x)` last
/// (latitude/longitude or projected axes, or any two placed by 2-D
/// latitude/longitude auxiliaries when `curvilinear`, #1174),
/// one time axis, any ensemble axis read at `member`, at most one axis of
/// several pressure levels (a dimension `pressure` says is one: its
/// coordinate's units are a pressure and it is no parametric coordinate,
/// [`pressure_unit`], #1067 PR 8), and every other axis of size 1. Another
/// axis longer than 1 (height above ground, hybrid model levels) is a
/// reason not to serve it: a field is never silently pinned to one of its
/// levels.
pub(crate) fn classify(
    dims: &[(String, u64, AxisRole)],
    member: u32,
    curvilinear: bool,
    pressure: impl Fn(&str) -> bool,
) -> Result<Shape, String> {
    let n = dims.len();
    if n < 2 {
        return Err("has fewer than two dimensions".to_string());
    }
    let (y, x) = (&dims[n - 2], &dims[n - 1]);
    if !curvilinear && spatial_pair(y.2, x.2).is_none() {
        return Err(format!(
            "is not on a 1-D latitude/longitude or projected x/y grid, nor on 2-D latitude/\
             longitude (its last dimensions are '{}', '{}')",
            y.0, x.0
        ));
    }
    let mut out = Vec::with_capacity(n);
    let mut time_dim = None;
    let mut levels = Vec::new();
    let mut vertical = None;
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
            AxisRole::Other if vertical.is_none() && pressure(name) => {
                vertical = Some(i);
                out.push(Dim::Level);
            }
            _ => {
                return Err(format!(
                    "has {size} values on its '{name}' axis; only fields with one level, or \
                     with levels on one pressure axis, are served"
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
        vertical,
        ny: y.1,
        nx: x.1,
    })
}

/// The pressure unit of dimension `dim`'s coordinate variable, when its
/// units are a pressure (CF 1.11 §4.3) and it is not a parametric
/// coordinate: what makes an axis a pressure axis. A coordinate with
/// `formula_terms`, or with a `standard_name` other than `air_pressure`,
/// is not one whatever its units: CF 4.3.3 knows a parametric (hybrid)
/// axis by those, and CESM/CAM's hybrid `lev` has `units = "hPa"`. A
/// coordinate without a `standard_name` is known by its units alone.
fn pressure_unit(variables: &[NcVariable], dim: &str) -> Option<PressureUnit> {
    let coord = coordinate(variables, dim)?;
    if coord.attribute("formula_terms").is_some() {
        return None;
    }
    if let Some(name) = text_attr(coord, "standard_name") {
        let name = name.trim();
        if !name.is_empty() && name != "air_pressure" {
            return None;
        }
    }
    PressureUnit::parse(&text_attr(coord, "units")?)
}

/// The levels of pressure axis `dim` (`size` long), in hPa, bottom first
/// (the largest pressure first), and the stored index of each. MEPS stores
/// them in hPa from 50 up; a file in Pa (ERA5) gives the same hPa values.
/// Levels must be finite, positive and distinct.
fn pressure_levels(
    reader: &Reader,
    variables: &[NcVariable],
    dim: &str,
    size: u64,
) -> Result<(Option<Vec<f64>>, Vec<u64>), String> {
    let (Some(coord), Some(unit)) = (coordinate(variables, dim), pressure_unit(variables, dim))
    else {
        return Err(format!("pressure axis '{dim}' has no coordinate variable"));
    };
    let hpa: Vec<f64> = read_coordinate(reader, coord)?
        .into_iter()
        .map(|value| unit.to_hpa(value))
        .collect();
    if hpa.len() as u64 != size {
        return Err(format!(
            "pressure axis '{dim}' has {} values for {size} levels",
            hpa.len()
        ));
    }
    if hpa.iter().any(|p| !p.is_finite() || *p <= 0.0) {
        return Err(format!(
            "pressure axis '{dim}' has a level that is not a positive pressure"
        ));
    }
    let mut order: Vec<usize> = (0..hpa.len()).collect();
    order.sort_by(|&a, &b| hpa[b].total_cmp(&hpa[a]));
    let levels: Vec<f64> = order.iter().map(|&i| hpa[i]).collect();
    if levels.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(format!("pressure axis '{dim}' repeats a level"));
    }
    Ok((Some(levels), order.into_iter().map(|i| i as u64).collect()))
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
/// else the CRS of its `grid_mapping`; or the curvilinear grid of its 2-D
/// latitude/longitude auxiliaries, both read whole, once per file
/// (#1174): they are the grid's identity, which every later file's must
/// match ([`FileGrid::same`]).
///
/// A curvilinear grid's size is checked from the dimensions before
/// anything is read ([`CurvilinearNodes::check_size`]), so a grid over
/// `MAX_NODES` is refused unread, and the read reserves its transient
/// peak from the decode budget, as a field read does
/// ([`curvilinear_peak_bytes`]): when it does not fit, the file is
/// [`Unservable::Busy`] and opened again next poll.
fn file_grid(
    reader: &Reader,
    variables: &[NcVariable],
    var: &NcVariable,
    y: &str,
    x: &str,
    placement: &Placement,
) -> Result<FileGrid, Unservable> {
    let mapping = match placement {
        Placement::Axes(mapping) => mapping.as_deref(),
        Placement::Auxiliary { lat, lon } => {
            let named = |e: String| format!("2-D latitude '{lat}' and longitude '{lon}': {e}");
            let size = |dim: &str| {
                var.dimensions
                    .iter()
                    .find(|d| d.name == dim)
                    .map_or(0, |d| d.size as usize)
            };
            let (nx, ny) = (size(x), size(y));
            let nodes = CurvilinearNodes::check_size(nx, ny).map_err(named)?;
            let aux = |name: &str| {
                variables
                    .iter()
                    .find(|v| v.name == name)
                    .ok_or_else(|| format!("coordinate '{name}' does not exist"))
            };
            let (lat_var, lon_var) = (aux(lat)?, aux(lon)?);
            let peak = curvilinear_peak_bytes(nodes, lat_var, lon_var);
            let _permit = BUDGET.reserve(peak).map_err(|_| {
                Unservable::Busy(named(format!(
                    "the NetCDF read budget has no room for the {} MiB their read takes now",
                    peak.div_ceil(1 << 20)
                )))
            })?;
            let lat_values = read_whole(reader, lat_var)?;
            let lon_values = read_whole(reader, lon_var)?;
            let grid = CurvilinearNodes::new(nx, ny, &lon_values, &lat_values).map_err(named)?;
            return Ok(FileGrid::Curvilinear(Arc::new(grid)));
        }
    };
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
        let values = read_coordinate(reader, coord)?;
        RegularAxis::from_values(dim, &values, &text_attr(coord, "units").unwrap_or_default())
    };
    let grid =
        cf::grid(crs, &axis(x)?, &axis(y)?).map_err(|e| format!("variable '{}': {e}", var.name))?;
    Ok(FileGrid::Regular {
        transform: grid.transform,
        flipped: grid.flipped,
    })
}

/// The transient peak of reading a curvilinear grid of `nodes` nodes from
/// `lat` and `lon`, which [`file_grid`] reserves: both arrays as `f64`
/// (16 B per node, the first held while the second is read), the unit
/// vectors built from them (12 B), and one array's decode in its stored
/// type, three times over as a field read counts it (the inflated chunk,
/// its unshuffled copy, and the compressed chunk it bounds). 52 B per node
/// for `double` arrays: 2.6 MB per 50 000 nodes, 870 MB at `MAX_NODES`.
pub(crate) fn curvilinear_peak_bytes(nodes: usize, lat: &NcVariable, lon: &NcVariable) -> u64 {
    let stored = |v: &NcVariable| Stored::of(v.dtype()).map_or(8, Stored::size);
    let per_node = 16 + 12 + 3 * stored(lat).max(stored(lon));
    (nodes as u64).saturating_mul(per_node)
}

/// A whole 2-D auxiliary coordinate (#1174), CF packing applied in place,
/// with `_FillValue`, `missing_value` (compared in the stored type) and
/// NaN as NaN: a node without a position. A slice read, so nothing stays
/// cached in the reader.
fn read_whole(reader: &Reader, var: &NcVariable) -> Result<Vec<f64>, String> {
    let stored = Stored::of(var.dtype())
        .ok_or_else(|| format!("coordinate '{}' is not numeric", var.name))?;
    let mut values = reader.read_all(var)?;
    let number = |name: &str| var.attribute(name).and_then(|a| a.value.as_f64());
    let fill: Vec<f64> = ["_FillValue", "missing_value"]
        .iter()
        .filter_map(|name| var.attribute(name)?.value.as_f64_vec())
        .flatten()
        .map(|v| stored.normalize(v))
        .collect();
    let (scale, offset) = (
        number("scale_factor").unwrap_or(1.0),
        number("add_offset").unwrap_or(0.0),
    );
    for v in &mut values {
        *v = if v.is_nan() || fill.contains(v) {
            f64::NAN
        } else {
            *v * scale + offset
        };
    }
    Ok(values)
}

/// A whole 1-D (or scalar) coordinate variable, CF packing applied: a slice
/// read, so nothing stays cached in the reader.
fn read_coordinate(reader: &Reader, var: &NcVariable) -> Result<Vec<f64>, String> {
    let number = |name: &str| var.attribute(name).and_then(|a| a.value.as_f64());
    let (scale, offset) = (
        number("scale_factor").unwrap_or(1.0),
        number("add_offset").unwrap_or(0.0),
    );
    let values: Vec<f64> = reader
        .read_all(var)?
        .iter()
        .map(|v| v * scale + offset)
        .collect();
    if values.is_empty() {
        return Err(format!("coordinate '{}' is empty", var.name));
    }
    Ok(values)
}

/// The valid times of the time coordinate `var`.
fn times(reader: &Reader, var: &NcVariable) -> Result<Vec<DateTime<Utc>>, String> {
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
    let raw = read_coordinate(reader, var)?;
    cf::decode_times(&raw, &units).map_err(|e| format!("time coordinate '{}': {e}", var.name))
}

/// The model run of the file: its `forecast_reference_time` variable,
/// scalar or 1-D with one value, or `None` without one.
fn reference_time(
    reader: &Reader,
    variables: &[NcVariable],
) -> Result<Option<DateTime<Utc>>, String> {
    let Some(var) = variables
        .iter()
        .find(|v| text_attr(v, "standard_name").as_deref() == Some("forecast_reference_time"))
    else {
        return Ok(None);
    };
    let units = text_attr(var, "units").ok_or_else(|| format!("'{}' has no units", var.name))?;
    let raw = read_coordinate(reader, var)?;
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
