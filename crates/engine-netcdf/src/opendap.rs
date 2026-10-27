//! Remote files read through OPeNDAP (DAP2, #1264): thredds.met.no serves
//! MEPS through its `dodsC` service only since 2026-10-22, and answers 403
//! on the fileServer the range reads of [`crate::remote`] use.
//!
//! - **Open:** the `.dds` and `.das` in one round (two requests), then one
//!   `.dods` of every scalar and 1-D coordinate variable: three requests,
//!   about 150 KB for a MEPS sfc file of 221 variables (its DDS 50 KB, DAS
//!   98 KB, coordinates 8 KB; measured 2026-10-23). The metadata becomes
//!   netcdf-reader's `NcVariable`s ([`dap::metadata`]), so ingest builds
//!   the same `Variable`s and `Layout` as from a NetCDF file. A 2-D
//!   coordinate, read only for a curvilinear grid, is one more `.dods`.
//!   The DDS states every size, so every whole read is checked against a
//!   hard cap before it is asked for ([`MAX_COORDINATE_BYTES`], and
//!   `CurvilinearNodes::check_size` for 2-D latitude/longitude) and
//!   reserves its reply and decode from the decode budget, as a field read
//!   does: a server declaring a 4-billion-value axis is refused unread.
//! - **Fields:** one `.dods` per field read, its constraint the exact
//!   hyperslab ([`dap::constraint`]), decoded from XDR in its stored type.
//!   The server decodes the file's chunks, so a MEPS field is its 4.06 MB
//!   of `Float32`, uncompressed: THREDDS ignores `Accept-Encoding` on
//!   `.dods` (measured). The reply is kept in the chunk cache
//!   ([`remote::dap_reply`]), single-flight, where a range read keeps its
//!   compressed chunk. An EDR query reads only the window of a field it
//!   samples (#1171, [`crate::field::read_uncached`]): a reply of its own,
//!   428 bytes for a point's 2 × 2 cells of a MEPS field. The windows of
//!   several variables at one step are one request (#1274,
//!   [`DapFile::read_fields`]): one projection each in the constraint, one
//!   reply listing them in dataset order.
//! - **Requests** go through the host's stores ([`Host`]): its request
//!   limit, pace and 429 back-off, its `storage` span, under the calling thread's
//!   [`Mode`], on the I/O runtime ([`io::blocking`]). THREDDS streams every
//!   reply chunked, without `Content-Length`: they are read as documents
//!   (`DataStore::get_documents_on`).
//! - **Replaced files:** THREDDS sends neither `Last-Modified` nor an ETag
//!   with a DAP reply (measured), so only the catalog shows a replaced
//!   file: the poll reads it again once its catalog entry changes. A
//!   server that sends `Last-Modified` with the `.dds` has every later
//!   reply compared with it, and a 404 is a file gone: either is handled
//!   as a replaced file is ([`remote::forget_replaced`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::{DocumentRequest, FetchedDocument, ObjectVersion};
use netcdf_reader::{NcSliceInfo, NcType, NcVariable};

use crate::dap::{self, Metadata};
use crate::decode_budget::Budget;
use crate::ingest::{FieldRead, Stored};
use crate::io::{self, Mode};
use crate::remote::{self, Changes, FileId, Host, ReadFailure};
use crate::source::Raw;

/// The most bytes a `.dds` or `.das` may be: a MEPS sfc file's DAS is
/// 98 KB.
const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;

/// Bytes a `.dods` reply may carry beyond its values: its DDS.
const REPLY_DDS_BYTES: u64 = 64 * 1024;

/// The most bytes, in XDR, a file's scalars and 1-D coordinates may take
/// together, or one whole read of a scalar or 1-D variable: 16 MiB, two
/// million `f64` values (MEPS's are 9.5 KB). Checked from the DDS before
/// the request: the DDS states the sizes the reply's cap would follow, so
/// without it a server declaring a 4-billion-value axis would be read.
pub(crate) const MAX_COORDINATE_BYTES: u64 = 16 * 1024 * 1024;

/// One remote file read through OPeNDAP, opened at one version.
pub(crate) struct DapFile {
    host: Arc<Host>,
    /// The dataset's key: its URL less the origin, `.dds` and the rest
    /// appended.
    key: String,
    id: Arc<FileId>,
    /// The ingest that opened it ([`crate::catalog::OpenFile::id`]).
    ingest: u64,
    changes: Arc<Changes>,
    metadata: Metadata,
    /// The scalars and 1-D coordinate variables, read at open, as stored.
    coordinates: HashMap<String, Vec<f64>>,
    /// What a whole read reserves its transient peak from:
    /// [`crate::decode_budget::BUDGET`].
    budget: Arc<Budget>,
    /// The `.dds` reply's `Last-Modified`, where the server sent one.
    served: Option<DateTime<Utc>>,
    replaced: AtomicBool,
    failed: AtomicBool,
}

/// Why [`DapFile::open`] failed.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// A request failed (network, deadline, a file gone): tried again
    /// next poll.
    Read(ReadFailure),
    /// The server's answer is not a dataset that can be read: skipped
    /// until the file changes.
    Format(String),
    /// The decode budget has no room for its coordinates now: tried again
    /// next poll, as a curvilinear grid's read under budget pressure is.
    Busy(String),
}

impl DapFile {
    /// Open `key` under `origin`, listed at `version`, for the ingest
    /// `ingest`: its metadata and coordinates, three requests in two
    /// rounds. Whole reads reserve from `budget`.
    pub(crate) fn open(
        host: Arc<Host>,
        origin: &str,
        key: &str,
        version: ObjectVersion,
        ingest: u64,
        changes: Arc<Changes>,
        budget: Arc<Budget>,
    ) -> Result<Self, OpenError> {
        let mut file = Self {
            host,
            key: key.to_string(),
            id: Arc::new(FileId {
                url: format!("{origin}/{key}"),
                version,
            }),
            ingest,
            changes,
            metadata: Metadata {
                variables: Vec::new(),
                projections: Vec::new(),
            },
            coordinates: HashMap::new(),
            budget,
            served: None,
            replaced: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        let documents = file
            .fetch(&[(".dds", None), (".das", None)], MAX_METADATA_BYTES)
            .map_err(OpenError::Read)?;
        let [dds, das] = &documents[..] else {
            unreachable!("one document per request");
        };
        if dds.last_modified != das.last_modified {
            file.found_replaced(io::mode());
            return Err(OpenError::Read(ReadFailure::Replaced));
        }
        file.served = dds.last_modified;
        let text = |document: &FetchedDocument, what: &str| {
            std::str::from_utf8(&document.bytes)
                .map(str::to_string)
                .map_err(|e| OpenError::Format(format!("its {what} is not UTF-8: {e}")))
        };
        file.metadata =
            dap::metadata(&text(dds, ".dds")?, &text(das, ".das")?).map_err(OpenError::Format)?;
        file.coordinates = file.read_coordinates()?;
        Ok(file)
    }

    /// Every numeric scalar and 1-D coordinate variable, in one `.dods`:
    /// at most [`MAX_COORDINATE_BYTES`] together, checked before the
    /// request, its transient peak reserved from the budget.
    fn read_coordinates(&self) -> Result<HashMap<String, Vec<f64>>, OpenError> {
        let wanted: Vec<usize> = (0..self.metadata.variables.len())
            .filter(|&i| {
                let var = &self.metadata.variables[i];
                !matches!(var.dtype, NcType::String | NcType::Char)
                    && (var.dimensions.is_empty() || var.is_coordinate_variable())
            })
            .collect();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let projections: Vec<&dap::Projection> = wanted
            .iter()
            .map(|&i| &self.metadata.projections[i])
            .collect();
        let (bytes, peak) = wanted
            .iter()
            .map(|&i| whole_read(&self.metadata.variables[i]))
            .fold((0u64, 0u64), |(bytes, peak), (b, p)| {
                (bytes.saturating_add(b), peak.saturating_add(p))
            });
        if bytes > MAX_COORDINATE_BYTES {
            return Err(OpenError::Format(format!(
                "its DDS declares {bytes} bytes of scalars and 1-D coordinates, more than the \
                 {} MiB a file's may take",
                MAX_COORDINATE_BYTES >> 20
            )));
        }
        let _permit = self.budget.reserve(peak).map_err(|_| {
            OpenError::Busy(format!(
                "the NetCDF read budget has no room for the {} KiB its coordinates take now",
                peak.div_ceil(1024)
            ))
        })?;
        let reply = self
            .fetch(
                &[(".dods", Some(dap::whole(&projections)))],
                bytes + REPLY_DDS_BYTES,
            )
            .map_err(OpenError::Read)?;
        let mut found: HashMap<String, Raw> = HashMap::new();
        for (name, raw) in dap::decode(&reply[0].bytes).map_err(OpenError::Format)? {
            found.entry(name).or_insert(raw);
        }
        let mut coordinates = HashMap::new();
        for (&i, projection) in wanted.iter().zip(projections) {
            let var = &self.metadata.variables[i];
            match found.remove(&projection.leaf) {
                Some(raw) if raw.len() as u64 == cells(var) => {
                    coordinates.insert(var.name.clone(), raw.into_f64());
                }
                _ => {
                    return Err(OpenError::Format(format!(
                        "the coordinates reply lacks '{}'",
                        var.name
                    )))
                }
            }
        }
        Ok(coordinates)
    }

    /// Every variable's metadata.
    pub(crate) fn variables(&self) -> &[NcVariable] {
        &self.metadata.variables
    }

    /// Whether a read of this file has failed.
    pub(crate) fn read_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Every value of `var`, as stored: a coordinate read at open, else one
    /// `.dods` of the whole variable. Only a scalar or 1-D variable of at
    /// most [`MAX_COORDINATE_BYTES`], or a 2-D one of at most
    /// `ds_core::resample::curvilinear::MAX_NODES` (the 2-D
    /// latitude/longitude of a curvilinear grid), checked before the
    /// request. A scalar or 1-D read reserves its transient peak from the
    /// budget here; a 2-D one's is reserved by its caller, as a range read's
    /// is (`ingest::file_grid`, `curvilinear_peak_bytes`, whose three times
    /// the stored size covers the reply and its decode). A full budget fails
    /// the read as a failed read does, so the file is opened again next
    /// poll, never skipped.
    pub(crate) fn read_all(&self, var: &NcVariable) -> Result<Vec<f64>, String> {
        if let Some(values) = self.coordinates.get(&var.name) {
            return Ok(values.clone());
        }
        let projection = self
            .metadata
            .projection(&var.name)
            .ok_or_else(|| format!("'{}' is not in the dataset", var.name))?;
        let (xdr, peak) = whole_read(var);
        let _permit = match var.dimensions[..] {
            [] | [_] if xdr > MAX_COORDINATE_BYTES => {
                return Err(format!(
                    "the DDS declares {xdr} bytes, more than the {} MiB a whole read of a \
                     scalar or 1-D variable may take",
                    MAX_COORDINATE_BYTES >> 20
                ))
            }
            [] | [_] => Some(self.budget.reserve(peak).map_err(|_| {
                self.failed.store(true, Ordering::Relaxed);
                format!(
                    "the NetCDF read budget has no room for the {} KiB this read takes now",
                    peak.div_ceil(1024)
                )
            })?),
            [ref y, ref x] => {
                ds_core::resample::CurvilinearNodes::check_size(x.size as usize, y.size as usize)?;
                None
            }
            _ => {
                return Err(format!(
                    "only scalars, 1-D variables and 2-D latitude/longitude are read whole, \
                     not its {} dimensions",
                    var.dimensions.len()
                ))
            }
        };
        let max = xdr + REPLY_DDS_BYTES;
        let reply = self
            .fetch(&[(".dods", Some(dap::whole(&[projection])))], max)
            .map_err(|e| e.to_string())?;
        let raw = dap::decode(&reply[0].bytes).and_then(|values| {
            dap::take(values, &projection.leaf).ok_or("not in the reply".into())
        })?;
        if raw.len() as u64 != cells(var) {
            return Err(format!("{} values read of {}", raw.len(), cells(var)));
        }
        Ok(raw.into_f64())
    }

    /// Variable `read`'s hyperslab `selection`, in its stored type: one of
    /// [`Self::read_fields`].
    #[cfg(test)]
    pub(crate) fn read_field(
        &self,
        read: &FieldRead,
        selection: &NcSliceInfo,
    ) -> Result<Raw, DataServerError> {
        let mut raws = self.read_fields(&[(read, selection)])?;
        Ok(raws.remove(0))
    }

    /// Each of `reads`, a variable's hyperslab, in its stored type, in
    /// `reads`' order: one `.dods` whose constraint projects each
    /// hyperslab, comma-separated in `reads`' order, from the chunk cache
    /// or fetched once for every concurrent reader. A whole field, each
    /// window of it (#1171) and each set of windows read together (#1274)
    /// are replies of their own, cached under their own constraints, each
    /// capped at the size of what it selects, plus one DDS.
    ///
    /// A DAP2 reply lists its variables in dataset order, whatever the
    /// constraint's (THREDDS, measured 2026-10-24): each read's values are
    /// found by its array's name ([`dap::Projection::leaf`]), the reads of
    /// one name in dataset order.
    pub(crate) fn read_fields(
        &self,
        reads: &[(&FieldRead, &NcSliceInfo)],
    ) -> Result<Vec<Raw>, DataServerError> {
        let mut found: Vec<(usize, &dap::Projection)> = Vec::with_capacity(reads.len());
        let mut constraints = Vec::with_capacity(reads.len());
        let mut max = REPLY_DDS_BYTES;
        for &(read, selection) in reads {
            let name = &*read.name;
            let position = self
                .metadata
                .variables
                .iter()
                .position(|v| v.name == name)
                .ok_or_else(|| {
                    DataServerError::Storage(format!("{}: no variable '{name}'", self.id.url))
                })?;
            let projection = &self.metadata.projections[position];
            constraints.push(dap::constraint(projection, selection));
            let dtype = &self.metadata.variables[position].dtype;
            max = max.saturating_add(dap::xdr_bytes(dtype, dap::selected(selection)));
            found.push((position, projection));
        }
        let constraint = constraints.join(",");
        let invalid = |e: String| DataServerError::Storage(format!("{}: {e}", self.id.url));
        let reply = remote::dap_reply(&self.id, &constraint, || {
            let mut reply = self.fetch(&[(".dods", Some(constraint.clone()))], max)?;
            let bytes = reply.remove(0).bytes;
            // Never cache a reply that is no data: an error a server sent
            // with a 200.
            dap::check(&bytes).map_err(|e| ReadFailure::Failed(invalid(e)))?;
            Ok(bytes)
        })
        .map_err(|failure| failure.to_error())?;
        let mut values: Vec<Option<(String, Raw)>> = dap::decode(&reply)
            .map_err(invalid)?
            .into_iter()
            .map(Some)
            .collect();
        let mut order: Vec<usize> = (0..reads.len()).collect();
        order.sort_by_key(|&i| found[i].0);
        let mut raws: Vec<Option<Raw>> = (0..reads.len()).map(|_| None).collect();
        for i in order {
            let leaf = &found[i].1.leaf;
            let raw = values
                .iter_mut()
                .find(|value| value.as_ref().is_some_and(|(name, _)| name == leaf))
                .and_then(Option::take)
                .map(|(_, raw)| raw)
                .ok_or_else(|| invalid(format!("'{leaf}' is not in the reply")))?;
            // DAP2's Byte is unsigned; a signed byte's bits are its own.
            raws[i] = Some(match (reads[i].0.stored, raw) {
                (Stored::Byte, Raw::U8(bytes)) => {
                    Raw::I8(bytes.into_iter().map(|b| b as i8).collect())
                }
                (_, raw) => raw,
            });
        }
        Ok(raws.into_iter().flatten().collect())
    }

    /// Whether the reply of `read`'s hyperslab `selection` is in the chunk
    /// cache now: no request, no recency bump.
    pub(crate) fn holds(&self, read: &FieldRead, selection: &NcSliceInfo) -> bool {
        self.metadata
            .projection(&read.name)
            .is_some_and(|projection| {
                remote::dap_cached(&self.id, &dap::constraint(projection, selection))
            })
    }

    /// One round of requests, `(suffix, query)` each, at most `max_bytes`
    /// each, through the host's stores under the calling thread's mode. A
    /// 404, or another `Last-Modified` than the `.dds` had, is the file
    /// replaced.
    fn fetch(
        &self,
        documents: &[(&str, Option<String>)],
        max_bytes: u64,
    ) -> Result<Vec<FetchedDocument>, ReadFailure> {
        if self.replaced.load(Ordering::Relaxed) {
            return Err(ReadFailure::Replaced);
        }
        let mode = io::mode();
        let store = match mode {
            Mode::Background => &self.host.background,
            Mode::Request | Mode::Detached => &self.host.requests,
        };
        let requests: Vec<DocumentRequest> = documents
            .iter()
            .map(|(suffix, query)| DocumentRequest {
                path: ObjectPath::from([self.key.as_str(), suffix].concat()),
                query: query.clone(),
            })
            .collect();
        self.host
            .gets
            .fetch_add(requests.len() as u64, Ordering::Relaxed);
        let fail = |e: DataServerError| {
            self.failed.store(true, Ordering::Relaxed);
            ReadFailure::Failed(e)
        };
        let answers = io::blocking(|| {
            store.get_documents_on(
                &requests,
                requests.len(),
                max_bytes,
                io::handle(),
                mode.budget(),
            )
        })
        .map_err(fail)?;
        let mut read = Vec::with_capacity(answers.len());
        for answer in answers {
            match answer.map_err(fail)? {
                Some(document)
                    if self.served.is_none()
                        || document.last_modified.is_none()
                        || document.last_modified == self.served =>
                {
                    read.push(document)
                }
                _ => {
                    self.found_replaced(mode);
                    return Err(ReadFailure::Replaced);
                }
            }
        }
        Ok(read)
    }

    /// Drop what was read from this version and report it, once.
    fn found_replaced(&self, mode: Mode) {
        self.failed.store(true, Ordering::Relaxed);
        if !self.replaced.swap(true, Ordering::Relaxed) {
            remote::forget_replaced(&self.id, self.ingest, &self.key, &self.changes, mode);
        }
    }
}

/// A whole read of `var`: its reply's XDR bytes, and its transient peak,
/// the reply buffered, its values decoded in their stored type and then
/// widened to `f64`.
fn whole_read(var: &NcVariable) -> (u64, u64) {
    let n = cells(var);
    let xdr = dap::xdr_bytes(&var.dtype, n);
    let stored = var.dtype.size().map_or(8, |size| size as u64);
    let decoded = n.saturating_mul(stored).saturating_add(n.saturating_mul(8));
    (
        xdr,
        xdr.saturating_add(REPLY_DDS_BYTES).saturating_add(decoded),
    )
}

/// Values of `var`: the product of its dimensions, 1 for a scalar.
fn cells(var: &NcVariable) -> u64 {
    var.dimensions
        .iter()
        .fold(1u64, |n, d| n.saturating_mul(d.size))
}

#[cfg(test)]
mod tests;
