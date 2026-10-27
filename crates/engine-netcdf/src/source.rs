//! Where a catalogued file's metadata and values come from (#1264): a
//! NetCDF file through netcdf-reader, local or read in place by HTTP range,
//! or a remote file through OPeNDAP. Ingest and field reads go through
//! [`Reader`] alone, so both sources build the same `Variable`, `Layout`
//! and decoded fields.

use std::sync::Arc;

use ds_core::error::DataServerError;
use netcdf_reader::{DynStorage, NcFile, NcFormat, NcSliceInfo, NcSliceInfoElem, NcVariable};

use crate::ingest::{FieldRead, Packing, Span, Stored};
use crate::opendap::DapFile;
use crate::remote::{ReadFailure, RemoteFile};

// What a netcdf-reader file holds (#1307), fitted to the heap the
// committed NetCDF-4 fixtures leave allocated (5 to 19 variables, 15 to 86
// attributes, 2 to 8 served, 0 to 156 chunk-index entries, 3 to 8
// coordinates read; every fixture within 7 %): an open (superblock, root
// group), each variable and attribute `variables()` parses with its HDF5
// object header, and each dataset lookup the ingest leaves cached: a served
// variable's with its chunk index, a coordinate's. A real MEPS sfc file,
// 221 variables, comes out 10 % under the 820 KB measured per handle.
/// Bytes of an open NetCDF-4 file before its metadata is read.
const HANDLE_BYTES: u64 = 24_400;
/// Bytes per variable of the file's metadata.
const VARIABLE_BYTES: u64 = 1_960;
/// Bytes per attribute of the file's variables.
const ATTRIBUTE_BYTES: u64 = 200;
/// Bytes per served variable's dataset lookup.
const SERVED_BYTES: u64 = 3_700;
/// Bytes per entry of a served variable's cached chunk index.
const CHUNK_ENTRY_BYTES: u64 = 78;
/// Bytes per coordinate variable's dataset lookup.
const COORDINATE_BYTES: u64 = 3_500;

/// The heap a NetCDF variable's metadata owns: its name, dimensions and
/// attributes (#1307).
pub(crate) fn variable_bytes(var: &NcVariable) -> usize {
    use netcdf_reader::{NcAttrValue, NcAttribute, NcDimension};
    use std::mem::size_of;
    let value = |value: &NcAttrValue| match value {
        NcAttrValue::Bytes(v) => v.capacity(),
        NcAttrValue::UBytes(v) => v.capacity(),
        NcAttrValue::Chars(s) => s.capacity(),
        NcAttrValue::Shorts(v) => v.capacity() * 2,
        NcAttrValue::UShorts(v) => v.capacity() * 2,
        NcAttrValue::Ints(v) => v.capacity() * 4,
        NcAttrValue::UInts(v) => v.capacity() * 4,
        NcAttrValue::Floats(v) => v.capacity() * 4,
        NcAttrValue::Doubles(v) => v.capacity() * 8,
        NcAttrValue::Int64s(v) => v.capacity() * 8,
        NcAttrValue::UInt64s(v) => v.capacity() * 8,
        NcAttrValue::Strings(v) => {
            v.capacity() * size_of::<String>() + v.iter().map(String::capacity).sum::<usize>()
        }
    };
    var.name.capacity()
        + var.dimensions.capacity() * size_of::<NcDimension>()
        + var
            .dimensions
            .iter()
            .map(|d| d.name.capacity())
            .sum::<usize>()
        + var.attributes.capacity() * size_of::<NcAttribute>()
        + var
            .attributes
            .iter()
            .map(|a| a.name.capacity() + value(&a.value))
            .sum::<usize>()
}

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

    /// Variable `read` at step `time`, the stored levels of `span`, over
    /// `selection` (its whole field, [`FieldRead::selection`], or a window
    /// of it, [`FieldRead::window_selection`]), in its stored type: one of
    /// [`Self::read_fields`].
    #[cfg(test)]
    pub(crate) fn read_field(
        &self,
        read: &FieldRead,
        time: u32,
        span: Span,
        selection: &NcSliceInfo,
    ) -> Result<Raw, DataServerError> {
        let mut raws = self.read_fields(time, &[(read, span, selection)])?;
        Ok(raws.remove(0))
    }

    /// Each of `reads` at step `time`: a variable, the stored levels of its
    /// span, over its selection (its whole field, [`FieldRead::selection`],
    /// or a window of it, [`FieldRead::window_selection`]), in its stored
    /// type, in `reads`' order. Through OPeNDAP one request for all of
    /// them (#1274, [`DapFile::read_fields`]); from a NetCDF file one slice
    /// read each. A failed remote read keeps its meaning
    /// ([`ReadFailure::to_error`]): a deadline, or a replaced file's 503.
    pub(crate) fn read_fields(
        &self,
        time: u32,
        reads: &[(&FieldRead, Span, &NcSliceInfo)],
    ) -> Result<Vec<Raw>, DataServerError> {
        match self {
            Reader::Nc { nc, .. } => reads
                .iter()
                .map(|&(read, span, selection)| read_slice(nc, read, time, span, selection))
                .collect(),
            Reader::Dap(dap) => {
                let slabs: Vec<(&FieldRead, &NcSliceInfo)> = reads
                    .iter()
                    .map(|&(read, _, selection)| (read, selection))
                    .collect();
                dap.read_fields(&slabs)
            }
        }
    }

    /// Whether a window of a field costs less to read than the field, so an
    /// EDR query reads only the cells it samples (#1171): through OPeNDAP
    /// the server cuts the hyperslab, a 2 × 2 window of a MEPS field being
    /// 428 bytes against 4.06 MB. A NetCDF file is read by chunk, whatever
    /// the window (MEPS keeps a field in one chunk), so its queries read
    /// whole fields.
    pub(crate) fn reads_windows(&self) -> bool {
        matches!(self, Reader::Dap(_))
    }

    /// Whether the whole field of `read` at step `time`, level `level` was
    /// fetched already and is in the chunk cache: an OPeNDAP reply a
    /// render, a prewarm or another query left. A query then decodes it
    /// rather than fetch a window of it. Always `false` for a NetCDF file,
    /// which reads no windows.
    pub(crate) fn holds_field(&self, read: &FieldRead, time: u32, level: usize) -> bool {
        match self {
            Reader::Nc { .. } => false,
            Reader::Dap(dap) => dap.holds(read, &read.selection(time, level)),
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

    /// The estimated heap this open file's reader holds (#1307) once the
    /// ingest has read `coordinates` coordinate variables and warmed
    /// `served` variables, caching `chunk_entries` chunk-index entries
    /// ([`crate::ingest::Field::chunk_entries`]). A netcdf-reader file
    /// holds trees no API reports, so it is a model ([`HANDLE_BYTES`] and
    /// the rest), fitted to counting-allocator measurements of the
    /// committed fixtures. An OPeNDAP file's metadata is plain data,
    /// walked.
    pub(crate) fn heap_estimate(
        &self,
        served: usize,
        chunk_entries: u64,
        coordinates: usize,
    ) -> u64 {
        match self {
            Reader::Nc { nc, .. } => {
                let (variables, attributes) = nc.variables().map_or((0, 0), |variables| {
                    let attributes = variables.iter().map(|v| v.attributes.len()).sum();
                    (variables.len(), attributes)
                });
                HANDLE_BYTES
                    + VARIABLE_BYTES * variables as u64
                    + ATTRIBUTE_BYTES * attributes as u64
                    + SERVED_BYTES * served as u64
                    + CHUNK_ENTRY_BYTES * chunk_entries
                    + COORDINATE_BYTES * coordinates as u64
            }
            Reader::Dap(dap) => dap.heap_bytes() as u64,
        }
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

/// Variable `read` of `nc` at step `time`, the stored levels of `span`,
/// over `selection`, in its stored type: one typed slice read.
fn read_slice(
    nc: &NcFile,
    read: &FieldRead,
    time: u32,
    span: Span,
    selection: &NcSliceInfo,
) -> Result<Raw, DataServerError> {
    let name = &*read.name;
    let fail = |e: netcdf_reader::Error| match ReadFailure::find(&e) {
        Some(failure) => failure.to_error(),
        None => DataServerError::Storage(format!(
            "variable '{name}' step {time} stored levels {}-{}: {e}",
            span.first, span.last
        )),
    };
    macro_rules! typed {
        ($t:ty, $variant:ident) => {
            Raw::$variant(into_vec(
                nc.read_variable_slice::<$t>(name, selection)
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
    #[cfg(test)]
    pub(crate) fn decode(&self, packing: &Packing, flipped: bool, nx: usize) -> Vec<f32> {
        self.decode_plane(0..self.len(), packing, flipped, nx)
    }

    /// The CF-decoded field of the values in `plane` ([`Packing::decode`]:
    /// packing, and fill or NaN as NaN), its rows reversed when `flipped`
    /// (a field `nx` wide): a read's one field, or one level's of a read
    /// over several stored levels, each its own plane of rows.
    pub(crate) fn decode_plane(
        &self,
        plane: std::ops::Range<usize>,
        packing: &Packing,
        flipped: bool,
        nx: usize,
    ) -> Vec<f32> {
        each!(self, values => decode(&values[plane], packing, flipped, nx))
    }
}

/// `values` as `f64`, exactly.
fn widen<T: Into<f64>>(values: Vec<T>) -> Vec<f64> {
    values.into_iter().map(Into::into).collect()
}

/// [`Raw::decode_plane`] of values of one type.
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
