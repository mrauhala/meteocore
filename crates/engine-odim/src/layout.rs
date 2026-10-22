//! Where in a polar volume file each moment's decode reads (#1047), so that
//! a cold pixel miss fetches a few byte ranges instead of the whole file.
//!
//! A 20–40 MB volume carries a hundred or more moments, and a map frame needs
//! one of them. When a scan has a new volume's bytes in hand, [`probe`] opens
//! the file once per moment over a storage that records each range the
//! reader asks for: the superblock, the groups and object headers on the
//! moment's path, its chunk index and its chunks. The probe reads every
//! chunk but decompresses none (its filters stop at the raw bytes,
//! [`probe_filters`]), so it costs a metadata walk, not a decode. Each
//! moment keeps its spans (`PolarMoment::spans`), merged when at most
//! [`PLAN_GAP`] apart.
//!
//! On a miss, [`ReadPlan::new`] takes the union of the spans of the moments
//! wanted. A layout it cannot vouch for has no plan, and the caller reads the
//! whole file: a moment the probe could not follow (contiguous or compact
//! storage, a read past the end), more ranges than one round of parallel
//! GETs ([`MAX_PLAN_RANGES`]), or a batch needing most of the file
//! ([`MAX_PLAN_FRACTION`]). The request decodes through [`SpanStorage`],
//! which serves only the fetched ranges. The reader is deterministic, so it
//! asks for the ranges it asked the probe for; a read outside them all the
//! same is a [miss](SpanStorage::missed), which the caller answers by
//! reading the whole file, never as bad data.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ds_storage::bytes::Bytes;
use hdf5_reader::error::{Error as Hdf5Error, Result as Hdf5Result};
use hdf5_reader::filters::FilterRegistry;
use hdf5_reader::{Hdf5File, OpenOptions, Storage, StorageBuffer};

use crate::pvol::{PolarMoment, PolarVolume};

/// Spans at most this far apart are fetched as one range. Reading the gap
/// costs a few milliseconds; a range more costs a GET, but GETs run in
/// parallel. On the FMI, SMHI and DMI volumes a base-sweep moment is then
/// four to seven ranges: its metadata at the start of the file, its chunks,
/// and a few small group indexes further on. At object_store's own 1 MiB
/// merge distance the same moment reads 7–21× the bytes on FMI volumes.
pub(crate) const PLAN_GAP: u64 = 64 * 1024;

/// Most ranges one plan fetches: the GETs `DataStore::get_ranges_opt` runs
/// at once, so a plan costs one round trip. More means moments scattered
/// over the file, and the whole file is read instead.
pub(crate) const MAX_PLAN_RANGES: usize = 10;

/// A plan reading more than this share of the file reads the whole file
/// instead (an all-quantity EDR profile, a small volume): one GET of the
/// file then costs about what the ranges would.
pub(crate) const MAX_PLAN_FRACTION: f64 = 0.5;

/// One byte span of a volume file a moment's decode reads. 32-bit: a file
/// of 4 GiB or more gets no spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    start: u32,
    len: u32,
}

impl Span {
    fn range(self) -> Range<u64> {
        let start = u64::from(self.start);
        start..start + u64::from(self.len)
    }
}

/// Record on each moment of `volume` the spans of `bytes` its decode reads
/// (#1047), one fresh open per moment, so that each moment's spans hold all
/// it reads, the metadata other moments share included. A moment the probe
/// cannot follow keeps none, and a miss on it reads the whole file. Returns
/// how many moments have spans.
pub(crate) fn probe(bytes: &Bytes, volume: &mut PolarVolume) -> usize {
    if u32::try_from(bytes.len()).is_err() {
        return 0;
    }
    let recorder = Arc::new(Recorder {
        bytes: bytes.clone(),
        reads: Mutex::default(),
        failed: AtomicBool::new(false),
    });
    let mut probed = 0;
    for moment in volume.sweeps.iter_mut().flat_map(|s| s.moments.iter_mut()) {
        moment.spans = probe_moment(&recorder, &moment.dataset_path);
        probed += usize::from(moment.spans.is_some());
    }
    probed
}

/// Byte ranges of one volume file to fetch: sorted, disjoint and more than
/// [`PLAN_GAP`] apart.
#[derive(Debug, Clone)]
pub(crate) struct ReadPlan {
    file_len: u64,
    ranges: Vec<Range<u64>>,
}

impl ReadPlan {
    /// The ranges to fetch for decoding `moments` of `volume`, or `None` to
    /// read the whole file: some moment has no spans, the ranges are more
    /// than [`MAX_PLAN_RANGES`], or they add up to more than
    /// [`MAX_PLAN_FRACTION`] of the file.
    pub(crate) fn new<'a>(
        volume: &PolarVolume,
        moments: impl IntoIterator<Item = &'a PolarMoment>,
    ) -> Option<Self> {
        let mut wanted = Vec::new();
        for moment in moments {
            wanted.extend(moment.spans.as_deref()?.iter().map(|s| s.range()));
        }
        let plan = ReadPlan {
            file_len: volume.file_len,
            ranges: merge(wanted, PLAN_GAP),
        };
        let fits = !plan.ranges.is_empty()
            && plan.ranges.len() <= MAX_PLAN_RANGES
            && plan.bytes() as f64 <= plan.file_len as f64 * MAX_PLAN_FRACTION;
        fits.then_some(plan)
    }

    /// The file's length when it was probed: a local read checks the file
    /// still has it.
    pub(crate) fn file_len(&self) -> u64 {
        self.file_len
    }

    pub(crate) fn ranges(&self) -> &[Range<u64>] {
        &self.ranges
    }

    /// Bytes the plan fetches.
    pub(crate) fn bytes(&self) -> u64 {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }
}

/// An HDF5 [`Storage`] over the ranges a [`ReadPlan`] fetched. A read
/// outside them fails and is remembered ([`Self::missed`]).
pub(crate) struct SpanStorage {
    len: u64,
    /// `(start, bytes)` per fetched range, sorted by start.
    spans: Vec<(u64, Bytes)>,
    missed: AtomicBool,
}

impl SpanStorage {
    /// `fetched` holds one buffer per range of `plan`, in order. `None` when
    /// a buffer is not its range's length: the file is shorter than when it
    /// was probed.
    pub(crate) fn new(plan: &ReadPlan, fetched: Vec<Bytes>) -> Option<Self> {
        if fetched.len() != plan.ranges.len() {
            return None;
        }
        let spans = plan
            .ranges
            .iter()
            .zip(fetched)
            .map(|(range, bytes)| {
                (bytes.len() as u64 == range.end - range.start).then_some((range.start, bytes))
            })
            .collect::<Option<_>>()?;
        Some(SpanStorage {
            len: plan.file_len,
            spans,
            missed: AtomicBool::new(false),
        })
    }

    /// Whether the reader asked for bytes outside the fetched ranges. A
    /// decode that failed then says nothing about the data.
    pub(crate) fn missed(&self) -> bool {
        self.missed.load(Ordering::Relaxed)
    }
}

impl Storage for SpanStorage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_range(&self, offset: u64, len: usize) -> Hdf5Result<StorageBuffer> {
        let end = offset.saturating_add(len as u64);
        let after = self.spans.partition_point(|(start, _)| *start <= offset);
        match after.checked_sub(1).map(|i| &self.spans[i]) {
            Some((start, bytes)) if end <= start + bytes.len() as u64 => {
                let from = (offset - start) as usize;
                Ok(StorageBuffer::from_vec(bytes[from..from + len].to_vec()))
            }
            _ => {
                self.missed.store(true, Ordering::Relaxed);
                Err(Hdf5Error::Other(format!(
                    "bytes {offset}..{end} were not fetched"
                )))
            }
        }
    }
}

/// The probe's storage: the volume's bytes, recording each range read.
struct Recorder {
    bytes: Bytes,
    reads: Mutex<Vec<Range<u64>>>,
    /// A read past the end: the reader followed a bad address, and the
    /// moment gets no spans.
    failed: AtomicBool,
}

impl Storage for Recorder {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_range(&self, offset: u64, len: usize) -> Hdf5Result<StorageBuffer> {
        let end = offset.saturating_add(len as u64);
        if end > self.len() {
            self.failed.store(true, Ordering::Relaxed);
            return Err(Hdf5Error::UnexpectedEof {
                offset,
                needed: len as u64,
                available: self.len().saturating_sub(offset),
            });
        }
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(offset..end);
        Ok(StorageBuffer::from_vec(
            self.bytes[offset as usize..end as usize].to_vec(),
        ))
    }
}

/// The spans one moment's decode reads: what opening the file, resolving
/// the dataset and loading each chunk reads, the reads
/// `Dataset::read_array` makes on a chunked dataset. `None` for a moment
/// that is not chunked (contiguous or compact storage) or that the reader
/// cannot follow.
fn probe_moment(recorder: &Arc<Recorder>, path: &str) -> Option<Box<[Span]>> {
    merge(recorded_reads(recorder, path)?, PLAN_GAP)
        .into_iter()
        .map(|r| {
            Some(Span {
                start: u32::try_from(r.start).ok()?,
                len: u32::try_from(r.end - r.start).ok()?,
            })
        })
        .collect()
}

/// The ranges [`probe_moment`] records for `path`, as read.
fn recorded_reads(recorder: &Arc<Recorder>, path: &str) -> Option<Vec<Range<u64>>> {
    recorder
        .reads
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    recorder.failed.store(false, Ordering::Relaxed);
    let options = OpenOptions {
        filter_registry: Some(probe_filters()),
        // Nothing is decoded, so nothing is worth keeping.
        chunk_cache_bytes: 0,
        ..OpenOptions::default()
    };
    let file = Hdf5File::from_storage_with_options(recorder.clone(), options).ok()?;
    let dataset = file.dataset(path).ok()?;
    dataset.chunks()?;
    // Each step reads one chunk's raw bytes and stops at its first filter:
    // decompressing is the decode's work, not the probe's.
    for chunk in dataset.iter_chunks().ok()? {
        drop(chunk);
    }
    if recorder.failed.load(Ordering::Relaxed) {
        return None;
    }
    let reads = std::mem::take(
        &mut *recorder
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    Some(reads)
}

/// Filters that fail at once: the probe needs where a chunk lies, which
/// reading its raw bytes records, not its values.
fn probe_filters() -> FilterRegistry {
    let mut registry = FilterRegistry::new();
    for id in PROBE_FILTER_IDS {
        registry.register(
            id,
            Box::new(|_, _, _| Err(Hdf5Error::FilterError("not decoded by the probe".into()))),
        );
    }
    registry
}

/// Every filter `hdf5-reader` decodes: deflate, shuffle, fletcher32, nbit,
/// scaleoffset and LZ4. It decodes no other, so any other fails after the
/// raw read all the same.
const PROBE_FILTER_IDS: [u16; 6] = [
    hdf5_reader::filters::FILTER_DEFLATE,
    hdf5_reader::filters::FILTER_SHUFFLE,
    hdf5_reader::filters::FILTER_FLETCHER32,
    hdf5_reader::filters::FILTER_NBIT,
    hdf5_reader::filters::FILTER_SCALEOFFSET,
    hdf5_reader::filters::FILTER_LZ4,
];

/// `ranges` sorted, with those at most `gap` apart merged.
fn merge(mut ranges: Vec<Range<u64>>, gap: u64) -> Vec<Range<u64>> {
    ranges.sort_unstable_by_key(|r| r.start);
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end.saturating_add(gap) => {
                last.end = last.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    merged
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pvol::{RadarSite, Sweep};
    use crate::reader::{RawPixels, ReadError};

    /// The committed DMI composite: one 2-D dataset.
    pub(crate) const COMP: &[u8] = include_bytes!("../../../testdata/odim-dmi-fixture.h5");
    pub(crate) const COMP_DATA: &str = "/dataset1/data1/data";

    /// `bytes` followed by zeros up to `len`. HDF5 never reads past its own
    /// end, so the file reads the same, but a moment's spans are then a small
    /// part of it, as in a 20–40 MB volume.
    pub(crate) fn padded(bytes: &[u8], len: usize) -> Bytes {
        let mut out = bytes.to_vec();
        out.resize(len.max(bytes.len()), 0);
        Bytes::from(out)
    }

    /// A one-sweep volume whose one moment is the 2-D dataset `path` of
    /// `bytes`, as long as `bytes`, with no spans recorded yet.
    pub(crate) fn one_moment_volume(bytes: &Bytes, path: &str) -> PolarVolume {
        let file = Hdf5File::from_bytes(bytes).unwrap();
        let shape = file.dataset(path).unwrap().shape().to_vec();
        PolarVolume {
            site: RadarSite {
                lon: 12.0,
                lat: 56.0,
                height: 0.0,
                nod: Some("test".into()),
                plc: None,
                wmo: None,
            },
            time: chrono::Utc::now(),
            object: "PVOL".into(),
            sweeps: vec![Sweep {
                elangle: 0.5,
                nbins: shape[1] as usize,
                nrays: shape[0] as usize,
                rscale: 1000.0,
                rstart: 0.0,
                a1gate: 0,
                moments: vec![PolarMoment {
                    quantity: "DBZH".into(),
                    gain: 1.0,
                    offset: 0.0,
                    nodata: 255.0,
                    undetect: 0.0,
                    dataset_path: path.into(),
                    spans: None,
                }],
            }],
            file_len: bytes.len() as u64,
        }
    }

    /// Cut `moment`'s last span to half its length, so that its decode
    /// reads past what a plan fetches.
    pub(crate) fn truncate_last_span(moment: &mut PolarMoment) {
        let spans = moment.spans.as_mut().expect("spans recorded");
        let last = spans.last_mut().expect("a span");
        last.len /= 2;
    }

    /// Bit-identical arrays of one type.
    pub(crate) fn same_pixels(a: &RawPixels, b: &RawPixels) -> bool {
        match (a, b) {
            (RawPixels::U8(a), RawPixels::U8(b)) => a == b,
            (RawPixels::U16(a), RawPixels::U16(b)) => a == b,
            (RawPixels::I16(a), RawPixels::I16(b)) => a == b,
            (RawPixels::F32(a), RawPixels::F32(b)) => {
                a.dim() == b.dim() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
            }
            _ => false,
        }
    }

    /// The ranges the decode of `path` reads, merged only where they touch:
    /// a plan with no byte to spare, so a decode reading anything the probe
    /// did not record misses.
    pub(crate) fn exact_plan(bytes: &Bytes, path: &str) -> Option<ReadPlan> {
        let recorder = Arc::new(Recorder {
            bytes: bytes.clone(),
            reads: Mutex::default(),
            failed: AtomicBool::new(false),
        });
        Some(ReadPlan {
            file_len: bytes.len() as u64,
            ranges: merge(recorded_reads(&recorder, path)?, 0),
        })
    }

    /// Decode `path` from only the ranges of `plan`, cut from `bytes`.
    pub(crate) fn decode_planned(
        bytes: &Bytes,
        plan: &ReadPlan,
        path: &str,
        nrays: usize,
        nbins: usize,
    ) -> (Result<RawPixels, ReadError>, bool) {
        let parts = plan
            .ranges()
            .iter()
            .map(|r| bytes.slice(r.start as usize..r.end as usize))
            .collect();
        let storage = Arc::new(SpanStorage::new(plan, parts).unwrap());
        let mut out = None;
        let opened = crate::pvol::visit_moments_pixels_in(
            storage.clone(),
            [(path, nrays, nbins)],
            |_, r| out = Some(r),
        );
        let decoded = opened.and_then(|()| out.expect("visited"));
        (decoded, storage.missed())
    }

    /// Decode `requests` in one open from only the ranges of `plan`, cut
    /// from `bytes`, as a batch read decodes them.
    pub(crate) fn decode_planned_batch(
        bytes: &Bytes,
        plan: &ReadPlan,
        requests: &[(&str, usize, usize)],
    ) -> (Vec<(String, RawPixels)>, bool) {
        let parts = plan
            .ranges()
            .iter()
            .map(|r| bytes.slice(r.start as usize..r.end as usize))
            .collect();
        let storage = Arc::new(SpanStorage::new(plan, parts).unwrap());
        let decoded =
            crate::pvol::read_moments_pixels_in(storage.clone(), requests.iter().copied()).unwrap();
        (decoded, storage.missed())
    }

    /// #1047: the probe records every byte a chunked moment's decode reads,
    /// so the decode through exactly those ranges reads nothing else and
    /// yields the whole-file decode bit for bit; with the production merge
    /// distance the plan is the file's HDF5 content, not its length.
    #[test]
    fn a_chunked_moment_decodes_from_its_recorded_spans_alone() {
        let bytes = padded(COMP, 4 << 20);
        let mut volume = one_moment_volume(&bytes, COMP_DATA);
        let (nrays, nbins) = (volume.sweeps[0].nrays, volume.sweeps[0].nbins);
        let whole = crate::pvol::read_moment_pixels(&bytes, COMP_DATA, nrays, nbins).unwrap();

        let exact = exact_plan(&bytes, COMP_DATA).expect("a chunked dataset");
        let (decoded, missed) = decode_planned(&bytes, &exact, COMP_DATA, nrays, nbins);
        assert!(!missed, "the decode read only recorded bytes");
        assert!(same_pixels(&decoded.unwrap(), &whole));

        assert_eq!(probe(&bytes, &mut volume), 1);
        let plan = ReadPlan::new(&volume, &volume.sweeps[0].moments).expect("a plan");
        assert!(plan.ranges().len() <= MAX_PLAN_RANGES);
        assert!(plan.bytes() <= COMP.len() as u64, "{plan:?}");
        let (decoded, missed) = decode_planned(&bytes, &plan, COMP_DATA, nrays, nbins);
        assert!(!missed);
        assert!(same_pixels(&decoded.unwrap(), &whole));
    }

    /// A decode reading a byte outside the fetched ranges fails and says so
    /// (`missed`), so the caller reads the whole file instead of calling the
    /// data bad.
    #[test]
    fn a_read_outside_the_fetched_ranges_is_a_miss() {
        let bytes = padded(COMP, 4 << 20);
        let volume = one_moment_volume(&bytes, COMP_DATA);
        let (nrays, nbins) = (volume.sweeps[0].nrays, volume.sweeps[0].nbins);
        let mut plan = exact_plan(&bytes, COMP_DATA).unwrap();
        assert!(plan.ranges.len() > 1, "{plan:?}");
        plan.ranges.pop();
        let (decoded, missed) = decode_planned(&bytes, &plan, COMP_DATA, nrays, nbins);
        assert!(decoded.is_err());
        assert!(missed);

        let storage = SpanStorage::new(&plan, vec![Bytes::from_static(b"x"); plan.ranges.len()]);
        assert!(storage.is_none(), "a short range means another file");
    }

    /// Contiguous storage (the h5py default, as in the committed cold-batch
    /// volume) is not followed: no spans, no plan, the whole file is read.
    #[test]
    fn a_contiguous_moment_has_no_spans() {
        let bytes = Bytes::from_static(include_bytes!("../../../testdata/pvol-cold-batch.h5"));
        let mut volume = crate::pvol::read_polar_volume(&bytes).unwrap();
        assert_eq!(probe(&bytes, &mut volume), 0);
        let moment = &volume.sweeps[0].moments[0];
        assert!(moment.spans.is_none());
        assert!(ReadPlan::new(&volume, [moment]).is_none());
    }

    /// The plan reads the whole file when the ranges would be most of it,
    /// too many for one round of GETs, or a moment has no spans; spans
    /// within `PLAN_GAP` share a range.
    #[test]
    fn a_plan_falls_back_to_the_whole_file() {
        let bytes = padded(COMP, 4 << 20);
        let mut volume = one_moment_volume(&bytes, COMP_DATA);
        let span = |start: u64, len: u64| Span {
            start: start as u32,
            len: len as u32,
        };
        let with_spans = |volume: &mut PolarVolume, spans: Vec<Span>| {
            volume.sweeps[0].moments[0].spans = Some(spans.into_boxed_slice());
        };
        let plan = |volume: &PolarVolume| ReadPlan::new(volume, &volume.sweeps[0].moments);

        assert!(plan(&volume).is_none(), "no spans recorded");

        with_spans(&mut volume, vec![span(0, 100), span(100 + PLAN_GAP, 50)]);
        let near = plan(&volume).unwrap();
        assert_eq!(near.ranges().len(), 1);
        assert_eq!(near.ranges()[0], 0..150 + PLAN_GAP);

        let far: Vec<_> = (0..=MAX_PLAN_RANGES as u64)
            .map(|i| span(i * 2 * PLAN_GAP, 10))
            .collect();
        with_spans(&mut volume, far.clone());
        assert!(
            plan(&volume).is_none(),
            "more ranges than one round of GETs"
        );
        with_spans(&mut volume, far[..MAX_PLAN_RANGES].to_vec());
        assert_eq!(plan(&volume).unwrap().ranges().len(), MAX_PLAN_RANGES);

        let half = bytes.len() as u64 / 2;
        with_spans(&mut volume, vec![span(0, half)]);
        assert!(plan(&volume).is_some());
        with_spans(&mut volume, vec![span(0, half + 1)]);
        assert!(plan(&volume).is_none(), "most of the file");
    }
}
