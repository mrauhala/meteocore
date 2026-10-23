//! Where a catalogued file's metadata and values come from (#1264): a
//! NetCDF file through netcdf-reader, local or read in place by HTTP range,
//! or a remote file through OPeNDAP. Ingest and field reads go through
//! [`Reader`] alone, so both sources build the same `Variable`, `Layout`
//! and decoded fields.

use std::sync::Arc;

use ds_core::error::DataServerError;
use netcdf_reader::{DynStorage, NcFile, NcFormat, NcSliceInfo, NcSliceInfoElem, NcVariable};

use crate::ingest::{FieldRead, Packing, Stored};
use crate::opendap::DapFile;
use crate::remote::{ReadFailure, RemoteFile};

/// One open file's reader.
pub(crate) enum Reader {
    /// netcdf-reader over a local file, or a remote one read by HTTP range
    /// (`remote`, its storage).
    Nc {
        nc: NcFile,
        storage: DynStorage,
        remote: Option<Arc<RemoteFile>>,
    },
    /// A remote file read through OPeNDAP (DAP2).
    Dap(DapFile),
}

impl Reader {
    /// Every variable's metadata.
    pub(crate) fn variables(&self) -> Result<&[NcVariable], String> {
        match self {
            Reader::Nc { nc, .. } => nc.variables().map_err(|e| format!("metadata: {e}")),
            Reader::Dap(dap) => Ok(dap.variables()),
        }
    }

    /// The file read once more through hdf5-reader, for the chunk shapes the
    /// NetCDF metadata does not carry: a NetCDF-4 file only. A file read
    /// through OPeNDAP is decoded by its server, chunks and all.
    pub(crate) fn hdf5(&self) -> Option<hdf5_reader::Hdf5File> {
        match self {
            Reader::Nc { nc, storage, .. } => {
                matches!(nc.format(), NcFormat::Nc4 | NcFormat::Nc4Classic)
                    .then(|| hdf5_reader::Hdf5File::from_storage(storage.clone()).ok())
                    .flatten()
            }
            Reader::Dap(_) => None,
        }
    }

    /// Every value of `var`, scalar or not, as stored (no packing applied),
    /// row-major. A slice read, so nothing stays cached in netcdf-reader;
    /// a scalar is the one `read_variable_as_f64` (a few bytes).
    pub(crate) fn read_all(&self, var: &NcVariable) -> Result<Vec<f64>, String> {
        let read = |e: &dyn std::fmt::Display| format!("coordinate '{}': {e}", var.name);
        match self {
            Reader::Nc { nc, .. } => {
                let array = if var.dimensions.is_empty() {
                    nc.read_variable_as_f64(&var.name)
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
                }
                .map_err(|e| read(&e))?;
                Ok(into_vec(array))
            }
            Reader::Dap(dap) => dap.read_all(var).map_err(|e| read(&e)),
        }
    }

    /// Variable `read` at step `time` and served level `level`, in its
    /// stored type. A failed remote read keeps its meaning
    /// ([`ReadFailure::to_error`]): a deadline, or a replaced file's 503.
    pub(crate) fn read_field(
        &self,
        read: &FieldRead,
        time: u32,
        level: usize,
    ) -> Result<Raw, DataServerError> {
        let selection = read.selection(time, level);
        match self {
            Reader::Nc { nc, .. } => {
                let name = &*read.name;
                let fail = |e: netcdf_reader::Error| match ReadFailure::find(&e) {
                    Some(failure) => failure.to_error(),
                    None => DataServerError::Storage(format!(
                        "variable '{name}' step {time} level {level}: {e}"
                    )),
                };
                macro_rules! typed {
                    ($t:ty, $variant:ident) => {
                        Raw::$variant(into_vec(
                            nc.read_variable_slice::<$t>(name, &selection)
                                .map_err(fail)?,
                        ))
                    };
                }
                Ok(match read.stored {
                    Stored::Byte => typed!(i8, I8),
                    Stored::UByte => typed!(u8, U8),
                    Stored::Short => typed!(i16, I16),
                    Stored::UShort => typed!(u16, U16),
                    Stored::Int => typed!(i32, I32),
                    Stored::UInt => typed!(u32, U32),
                    Stored::Float => typed!(f32, F32),
                    Stored::Double => typed!(f64, F64),
                })
            }
            Reader::Dap(dap) => dap.read_field(read, &selection),
        }
    }

    /// Warm what a request's read uses: netcdf-reader's dataset lookup, and
    /// the chunk index walked once without reading a chunk. Nothing for a
    /// file read through OPeNDAP.
    pub(crate) fn warm(&self, name: &str) -> Result<(), String> {
        match self {
            Reader::Nc { nc, .. } => nc
                .iter_variable_chunks(name)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Reader::Dap(_) => Ok(()),
        }
    }

    /// Whether the file is remote: its cold fields are filled detached
    /// ([`crate::field`]).
    pub(crate) fn is_remote(&self) -> bool {
        matches!(
            self,
            Reader::Nc {
                remote: Some(_),
                ..
            } | Reader::Dap(_)
        )
    }

    /// Whether a remote read of the file has failed (network, deadline,
    /// replaced): an open that failed then is tried again, not skipped.
    pub(crate) fn read_failed(&self) -> bool {
        match self {
            Reader::Nc { remote, .. } => remote.as_ref().is_some_and(|r| r.read_failed()),
            Reader::Dap(dap) => dap.read_failed(),
        }
    }
}

/// A read's values in their stored type, row-major.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Raw {
    I8(Vec<i8>),
    U8(Vec<u8>),
    I16(Vec<i16>),
    U16(Vec<u16>),
    I32(Vec<i32>),
    U32(Vec<u32>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

macro_rules! each {
    ($raw:expr, $values:ident => $body:expr) => {
        match $raw {
            Raw::I8($values) => $body,
            Raw::U8($values) => $body,
            Raw::I16($values) => $body,
            Raw::U16($values) => $body,
            Raw::I32($values) => $body,
            Raw::U32($values) => $body,
            Raw::F32($values) => $body,
            Raw::F64($values) => $body,
        }
    };
}

impl Raw {
    pub(crate) fn len(&self) -> usize {
        each!(self, values => values.len())
    }

    /// Every value as `f64`, exactly.
    pub(crate) fn into_f64(self) -> Vec<f64> {
        match self {
            Raw::F64(values) => values,
            Raw::I8(values) => widen(values),
            Raw::U8(values) => widen(values),
            Raw::I16(values) => widen(values),
            Raw::U16(values) => widen(values),
            Raw::I32(values) => widen(values),
            Raw::U32(values) => widen(values),
            Raw::F32(values) => widen(values),
        }
    }

    /// The CF-decoded field ([`Packing::decode`]: packing, and fill or NaN
    /// as NaN), its rows reversed when `flipped` (a field `nx` wide).
    pub(crate) fn decode(&self, packing: &Packing, flipped: bool, nx: usize) -> Vec<f32> {
        each!(self, values => decode(values, packing, flipped, nx))
    }
}

/// `values` as `f64`, exactly.
fn widen<T: Into<f64>>(values: Vec<T>) -> Vec<f64> {
    values.into_iter().map(Into::into).collect()
}

/// [`Raw::decode`] of values of one type.
fn decode<T: Copy + Into<f64>>(
    values: &[T],
    packing: &Packing,
    flipped: bool,
    nx: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(values.len());
    if flipped && nx > 0 {
        for row in values.chunks(nx).rev() {
            out.extend(row.iter().map(|&v| packing.decode(v.into())));
        }
    } else {
        out.extend(values.iter().map(|&v| packing.decode(v.into())));
    }
    out
}

/// An array's values in row-major order, moved out of a freshly read
/// (standard-layout) array without a copy.
fn into_vec<T: Clone>(array: ndarray::ArrayD<T>) -> Vec<T> {
    if !array.is_standard_layout() {
        return array.iter().cloned().collect();
    }
    let len = array.len();
    let (mut values, first) = array.into_raw_vec_and_offset();
    values.drain(..first.unwrap_or(0));
    values.truncate(len);
    values
}
