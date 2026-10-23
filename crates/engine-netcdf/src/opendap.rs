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
//! - **Fields:** one `.dods` per field read, its constraint the exact
//!   hyperslab ([`dap::constraint`]), decoded from XDR in its stored type.
//!   The server decodes the file's chunks, so a MEPS field is its 4.06 MB
//!   of `Float32`, uncompressed: THREDDS ignores `Accept-Encoding` on
//!   `.dods` (measured). The reply is kept in the chunk cache
//!   ([`remote::dap_reply`]), single-flight, where a range read keeps its
//!   compressed chunk.
//! - **Requests** go through the host's stores ([`Host`]): its request
//!   limit and 429 back-off, its `storage` span, under the calling thread's
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
use crate::ingest::{FieldRead, Stored};
use crate::io::{self, Mode};
use crate::remote::{self, Changes, FileId, Host, ReadFailure};
use crate::source::Raw;

/// The most bytes a `.dds` or `.das` may be: a MEPS sfc file's DAS is
/// 98 KB.
const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;

/// Bytes a `.dods` reply may carry beyond its values: its DDS.
const REPLY_DDS_BYTES: u64 = 64 * 1024;

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
}

impl DapFile {
    /// Open `key` under `origin`, listed at `version`, for the ingest
    /// `ingest`: its metadata and coordinates, three requests in two
    /// rounds.
    pub(crate) fn open(
        host: Arc<Host>,
        origin: &str,
        key: &str,
        version: ObjectVersion,
        ingest: u64,
        changes: Arc<Changes>,
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

    /// Every numeric scalar and 1-D coordinate variable, in one `.dods`.
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
        let projections: Vec<&str> = wanted
            .iter()
            .map(|&i| self.metadata.projections[i].as_str())
            .collect();
        let bytes: u64 = wanted
            .iter()
            .map(|&i| {
                let var = &self.metadata.variables[i];
                dap::xdr_bytes(&var.dtype, cells(var))
            })
            .sum();
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
            match found.remove(leaf(projection)) {
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
    /// `.dods` of the whole variable.
    pub(crate) fn read_all(&self, var: &NcVariable) -> Result<Vec<f64>, String> {
        if let Some(values) = self.coordinates.get(&var.name) {
            return Ok(values.clone());
        }
        let projection = self
            .metadata
            .projection(&var.name)
            .ok_or_else(|| format!("'{}' is not in the dataset", var.name))?;
        let max = dap::xdr_bytes(&var.dtype, cells(var)) + REPLY_DDS_BYTES;
        let reply = self
            .fetch(&[(".dods", Some(dap::whole(&[projection])))], max)
            .map_err(|e| e.to_string())?;
        let raw = dap::decode(&reply[0].bytes).and_then(|values| {
            dap::take(values, leaf(projection)).ok_or("not in the reply".into())
        })?;
        if raw.len() as u64 != cells(var) {
            return Err(format!("{} values read of {}", raw.len(), cells(var)));
        }
        Ok(raw.into_f64())
    }

    /// Variable `read`'s hyperslab `selection`, in its stored type: one
    /// `.dods`, from the chunk cache or fetched once for every concurrent
    /// reader.
    pub(crate) fn read_field(
        &self,
        read: &FieldRead,
        selection: &NcSliceInfo,
    ) -> Result<Raw, DataServerError> {
        let name = &*read.name;
        let projection = self.metadata.projection(name).ok_or_else(|| {
            DataServerError::Storage(format!("{}: no variable '{name}'", self.id.url))
        })?;
        let constraint = dap::constraint(projection, selection);
        let (ny, nx) = read.shape();
        let dtype = &self
            .metadata
            .variables
            .iter()
            .find(|v| v.name == name)
            .map_or(NcType::Double, |v| v.dtype.clone());
        let max = dap::xdr_bytes(dtype, (ny * nx) as u64) + REPLY_DDS_BYTES;
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
        let raw = dap::decode(&reply)
            .and_then(|values| {
                dap::take(values, leaf(projection)).ok_or_else(|| "not in the reply".into())
            })
            .map_err(invalid)?;
        // DAP2's Byte is unsigned; a signed byte's bits are its own.
        Ok(match (read.stored, raw) {
            (Stored::Byte, Raw::U8(bytes)) => Raw::I8(bytes.into_iter().map(|b| b as i8).collect()),
            (_, raw) => raw,
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

/// The name a projection's values carry in a reply: a Grid's array's.
fn leaf(projection: &str) -> &str {
    projection.rsplit('.').next().unwrap_or(projection)
}

/// Values of `var`: the product of its dimensions, 1 for a scalar.
fn cells(var: &NcVariable) -> u64 {
    var.dimensions
        .iter()
        .fold(1u64, |n, d| n.saturating_mul(d.size))
}

#[cfg(test)]
mod tests;
