//! Parameter discovery from bounded GRIB2 header reads, without unpacking values.

use std::time::{Duration, Instant};

use ds_core::error::DataServerError;
use ds_storage::{bytes::Bytes, object_store::path::Path, DataStore};

use crate::{cache::DecodedGrid, catalog::MessageEntry, runtime::run_fetches};

const PROBE_CONCURRENCY: usize = 8;
const READ_AHEAD: usize = 4096;
const MAX_SECTION_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod tests;

/// Node bounds and the cells between them. A cyclic longitude axis has a
/// closing cell; a duplicated seam node does not add another cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GridGeometry {
    pub bbox: [f64; 4],
    pub cells: [u32; 2],
}

impl GridGeometry {
    pub(crate) fn new(
        ni: usize,
        nj: usize,
        west: f64,
        north: f64,
        dx: f64,
        dy: f64,
    ) -> Option<Self> {
        if ni == 0 || nj == 0 || dx <= 0.0 || dy >= 0.0 {
            return None;
        }
        let south = north + (nj - 1) as f64 * dy;
        let east = west + (ni - 1) as f64 * dx;
        if ![west, east, north, south, dx, dy]
            .iter()
            .all(|v| v.is_finite())
            || south < -90.000001
            || north > 90.000001
        {
            return None;
        }
        let (west, east, nx) = match crate::cache::longitude_wrap_columns(ni, dx) {
            Some(columns) => (-180.0, 180.0, columns.round() as u32),
            None => (
                ds_core::geo::wrap_lon(west),
                ds_core::geo::wrap_lon(east),
                u32::try_from(ni - 1).ok()?,
            ),
        };
        Some(Self {
            bbox: [west, south.max(-90.0), east, north.min(90.0)],
            cells: [nx, u32::try_from(nj - 1).ok()?],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MessageMetadata {
    pub triple: (u8, u8, u8),
    pub centre: u16,
    pub first_surface_type: u8,
    pub first_surface_value: Option<f64>,
    pub geometry: Option<GridGeometry>,
}

impl From<&DecodedGrid> for MessageMetadata {
    fn from(grid: &DecodedGrid) -> Self {
        Self {
            triple: grid.triple,
            centre: grid.centre,
            first_surface_type: grid.first_surface_type,
            first_surface_value: grid.first_surface_value,
            geometry: GridGeometry::new(
                grid.ni,
                grid.nj,
                grid.lon_first,
                grid.lat_first,
                grid.lon_inc,
                grid.lat_inc,
            ),
        }
    }
}

impl MessageMetadata {
    /// Shared with full decoding: keep WMO and fixed-surface interpretation in
    /// one place, including local centre codes and missing surface values.
    pub(crate) fn from_product(
        discipline: u8,
        centre: u16,
        product: &grib::ProdDefinition,
    ) -> Result<Self, DataServerError> {
        // grib 0.15's fixed_surfaces() slices without checking the payload
        // length. Guard its template-specific offsets before calling it.
        // These are bounds, not a second parameter/surface decoder.
        let surface_offset = match product.prod_tmpl_num() {
            0..=15 | 51 | 60..=61 | 86..=87 | 91 | 1100..=1101 => Some(13),
            40..=43 => Some(15),
            44 => Some(24),
            45..=47 | 85 => Some(26),
            48..=49 => Some(37),
            55..=56 | 59 | 62..=63 => Some(19),
            70..=73 => Some(18),
            76..=79 => Some(16),
            80..=81 => Some(38),
            82..=84 => Some(27),
            88 => Some(5),
            _ => None,
        };
        if surface_offset.is_some_and(|offset| product.iter().len() < 4 + offset + 12) {
            return Err(invalid("truncated product definition"));
        }
        let category = product
            .parameter_category()
            .ok_or_else(|| invalid("missing parameter category"))?;
        let number = product
            .parameter_number()
            .ok_or_else(|| invalid("missing parameter number"))?;
        let (first_surface_type, first_surface_value) = match product.fixed_surfaces() {
            Some((surface, _)) => {
                let value = surface.value();
                (surface.surface_type, (!value.is_nan()).then_some(value))
            }
            None => (255, None),
        };
        Ok(Self {
            triple: (discipline, category, number),
            centre,
            first_surface_type,
            first_surface_value,
            geometry: None,
        })
    }
}

fn invalid(reason: &str) -> DataServerError {
    DataServerError::Engine(format!("Invalid GRIB2 metadata: {reason}"))
}

/// Fetch independently on the dedicated poll runtime (or shared CLI fallback).
/// Await every worker, including failures, and return results in input order.
pub(crate) fn read_batch(
    store: &DataStore,
    entries: &[(&str, &MessageEntry)],
) -> Vec<Result<MessageMetadata, DataServerError>> {
    let deadline = ds_core::deadline::current();
    run_fetches(async {
        let mut results = Vec::with_capacity(entries.len());
        for chunk in entries.chunks(PROBE_CONCURRENCY) {
            let jobs: Vec<_> = chunk
                .iter()
                .map(|(url, entry)| {
                    let store = store.clone();
                    let path = Path::from(*url);
                    let entry = (*entry).clone();
                    tokio::spawn(async move {
                        tokio::task::block_in_place(|| {
                            let _deadline = ds_core::deadline::enter(deadline);
                            read_metadata(&store, &path, &entry)
                        })
                    })
                })
                .collect();
            for job in jobs {
                results.push(job.await.unwrap_or_else(|e| {
                    Err(DataServerError::Engine(format!(
                        "GRIB metadata worker failed: {e}"
                    )))
                }));
            }
        }
        results
    })
}

/// Read Sections 0, 1, the regular-grid header in 3, and 4. Skip local-use
/// and unsupported grid bodies; stop before packing/bitmap/value decoding.
/// A normal header fits in the first 4 KiB range. Tail records use the GRIB
/// indicator's own length, so discovering their metadata needs no HEAD.
pub(crate) fn read_metadata(
    store: &DataStore,
    path: &Path,
    entry: &MessageEntry,
) -> Result<MessageMetadata, DataServerError> {
    // Bound the entire probe, including uncommon headers needing extra ranges.
    let end =
        ds_core::deadline::current().unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
    let _deadline = ds_core::deadline::enter(Some(end));
    let offset = usize::try_from(entry.offset).map_err(|_| invalid("offset overflow"))?;
    let limit = entry
        .length
        .map(usize::try_from)
        .transpose()
        .map_err(|_| invalid("length overflow"))?
        .unwrap_or(usize::MAX - offset);
    let mut reader = HeaderReader {
        store,
        path,
        offset,
        limit,
        start: 0,
        bytes: Bytes::new(),
    };
    let indicator = reader.read(0, 16)?;
    if &indicator[..4] != b"GRIB" || indicator[7] != 2 {
        return Err(invalid("expected GRIB edition 2 at index offset"));
    }
    let discipline = indicator[6];
    let length = usize::try_from(u64::from_be_bytes(indicator[8..16].try_into().unwrap()))
        .map_err(|_| invalid("message length overflow"))?;
    if length < 20 || length > limit || offset.checked_add(length).is_none() {
        return Err(invalid("message length exceeds index bounds"));
    }
    reader.limit = length;
    let mut position: usize = 16;
    let mut previous = 0;
    let mut centre = None;
    let mut geometry = None;
    loop {
        let header = reader.read(position, 5)?;
        let section_length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let number = header[4];
        if section_length < 5
            || position
                .checked_add(section_length)
                .is_none_or(|end| end > length - 4)
        {
            return Err(invalid("section length exceeds message bounds"));
        }
        if !matches!((previous, number), (0, 1) | (1, 2 | 3) | (2, 3) | (3, 4)) {
            return Err(invalid("unexpected header section order"));
        }
        match number {
            1 | 4 => {
                if section_length > MAX_SECTION_BYTES {
                    return Err(invalid("metadata section exceeds 64 KiB probe limit"));
                }
                let payload = reader
                    .read(position + 5, section_length - 5)?
                    .to_vec()
                    .into_boxed_slice();
                if number == 1 {
                    let identification = grib::Identification::from_payload(payload)
                        .map_err(|e| invalid(&e.to_string()))?;
                    centre = Some(identification.centre_id());
                } else {
                    let product = grib::ProdDefinition::from_payload(payload)
                        .map_err(|e| invalid(&e.to_string()))?;
                    let mut metadata = MessageMetadata::from_product(
                        discipline,
                        centre.ok_or_else(|| invalid("missing identification"))?,
                        &product,
                    )?;
                    metadata.geometry = geometry;
                    return Ok(metadata);
                }
            }
            3 => {
                // Template 3.0 has a fixed 72-byte header. Do not download an
                // optional point list or an unsupported template's large body.
                if section_length >= 14 {
                    let header = reader.read(position, 14)?;
                    if header[12..14] == [0, 0] && section_length >= 72 {
                        let payload = reader.read(position + 5, 67)?.to_vec().into_boxed_slice();
                        geometry = grib::GridDefinition::from_payload(payload)
                            .ok()
                            .and_then(|grid| crate::reader::grid_geometry(&grid).ok());
                    }
                }
            }
            2 => {}
            _ => unreachable!(),
        }
        previous = number;
        position += section_length;
    }
}

struct HeaderReader<'a> {
    store: &'a DataStore,
    path: &'a Path,
    offset: usize,
    limit: usize,
    start: usize,
    bytes: Bytes,
}

impl HeaderReader<'_> {
    fn read(&mut self, start: usize, length: usize) -> Result<&[u8], DataServerError> {
        let end = start
            .checked_add(length)
            .filter(|end| *end <= self.limit)
            .ok_or_else(|| invalid("header range exceeds message bounds"))?;
        if start < self.start || end > self.start + self.bytes.len() {
            let read_end = start.saturating_add(length.max(READ_AHEAD)).min(self.limit);
            let absolute_start = self
                .offset
                .checked_add(start)
                .ok_or_else(|| invalid("range offset overflow"))?;
            let absolute_end = self
                .offset
                .checked_add(read_end)
                .ok_or_else(|| invalid("range end overflow"))?;
            self.bytes = self
                .store
                .get_range(self.path, absolute_start..absolute_end)?;
            self.start = start;
        }
        self.bytes
            .get(start - self.start..end - self.start)
            .ok_or_else(|| invalid("truncated header range"))
    }
}
