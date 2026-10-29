//! Position queries read CCSDS-packed fields (#1300) and fields in complex
//! packing with spatial differencing (#1302) through a per-message index:
//! only the reference sample intervals, or the grid rows, holding their
//! points' bilinear neighbours are decoded, no grid is built, and nothing
//! enters the decoded-grid cache. Other fields, and every field when
//! `point_index_cache_mb` is 0, take the full decode as before.
//!
//! Under [`FieldFetch::Partial`] such a field also fetches only the bytes it
//! needs (#1301): one range GET of the indexed RSIs or rows, or a predicted
//! prefix of a message not yet indexed, read on by at most one
//! continuation. The 5.3 reads live in [`complex_reads`].

mod complex_reads;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use chrono::{DateTime, Utc};
use ds_cache::{ByteBoundedCache, CacheMetrics, MIB};
use ds_core::error::DataServerError;
use ds_storage::bytes::Bytes;
use ds_storage::object_store::path::Path;

use crate::cache::DecodedGrid;
use crate::catalog::{MessageEntry, ParameterKey};
use crate::ccsds::{self, CcsdsIndex, Extended, Header, Kin};
use crate::complex::{self, ComplexIndex};
use crate::message_cache::MessageKey;
use crate::metadata::MessageMetadata;
use crate::{reader, FieldFetch, FieldPlan, GribEngine, GridFill};

/// Why a point reader did not answer a field from its bytes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PointError {
    /// A message this reader does not handle: decode it in full.
    #[error("not read through a point index: {0}")]
    Unsupported(&'static str),
    /// The message, its payload or the index does not hold together, or
    /// libaec failed: the full decode decides what the message holds.
    #[error("invalid message for a point index: {0}")]
    Invalid(String),
    /// The query's deadline passed: fatal, as on every other path.
    #[error(transparent)]
    Fatal(DataServerError),
}

/// Data representation template 5.3, read by [`complex`].
const COMPLEX_TEMPLATE: u16 = 3;

/// One message's index: CCSDS RSI offsets (template 5.42), or 5.3 row
/// checkpoints and group descriptors.
#[derive(Debug, Clone)]
pub(crate) enum Index {
    Ccsds(Arc<CcsdsIndex>),
    Complex(Arc<ComplexIndex>),
}

impl Index {
    /// How far into the message the index reaches: RSIs or rows indexed.
    fn reach(&self) -> usize {
        match self {
            Self::Ccsds(index) => index.indexed_rsis(),
            Self::Complex(index) => index.indexed_rows(),
        }
    }

    fn size_bytes(&self) -> usize {
        match self {
            Self::Ccsds(index) => index.size_bytes(),
            Self::Complex(index) => index.size_bytes(),
        }
    }

    fn downgrade(&self) -> WeakIndex {
        match self {
            Self::Ccsds(index) => WeakIndex::Ccsds(Arc::downgrade(index)),
            Self::Complex(index) => WeakIndex::Complex(Arc::downgrade(index)),
        }
    }

    /// How long a prefix of another message, `message_len` bytes long on
    /// this one's grid, should be to sample `coords` there (#1301, #1302).
    fn predict_prefix(&self, coords: &[(f64, f64)], message_len: usize, kin: Kin) -> usize {
        match self {
            Self::Ccsds(index) => index.predict_prefix(coords, message_len, kin),
            Self::Complex(index) => index.predict_prefix(coords, message_len, kin),
        }
    }

    fn as_ccsds(&self) -> Option<&CcsdsIndex> {
        match self {
            Self::Ccsds(index) => Some(index),
            Self::Complex(_) => None,
        }
    }
}

/// An [`Index`] held weakly, as a sibling.
#[derive(Debug, Clone)]
enum WeakIndex {
    Ccsds(Weak<CcsdsIndex>),
    Complex(Weak<ComplexIndex>),
}

impl WeakIndex {
    fn upgrade(&self) -> Option<Index> {
        match self {
            Self::Ccsds(index) => index.upgrade().map(Index::Ccsds),
            Self::Complex(index) => index.upgrade().map(Index::Complex),
        }
    }

    fn alive(&self) -> bool {
        match self {
            Self::Ccsds(index) => index.strong_count() > 0,
            Self::Complex(index) => index.strong_count() > 0,
        }
    }
}

fn weight(key: &MessageKey, index: &Index) -> u64 {
    // The index, its key and Arc/cache-node overhead.
    (index.size_bytes() + key.size_bytes() + 2 * size_of::<usize>() + 64) as u64
}

/// A source's point indexes, keyed like its compressed-message cache, and
/// which of its parameters' messages they read.
pub(crate) struct PointIndex {
    cache: ByteBoundedCache<MessageKey, Index>,
    /// Per parameter and level: whether its last message read here could be
    /// indexed. One that could not (another packing, a scan order or option
    /// the readers do not take) takes the full decode directly until the
    /// catalog is next published ([`Self::forget_layouts`]), so one odd
    /// message cannot cost a parameter the point path for the engine's
    /// lifetime.
    readable: RwLock<HashMap<ParameterKey, bool>>,
    /// The latest index of each parameter's messages, per run, held weakly:
    /// what a prefix of a sibling message is predicted from (#1301).
    siblings: RwLock<HashMap<ParameterKey, Vec<RunIndex>>>,
}

/// One run's latest index of a parameter's messages.
type RunIndex = (DateTime<Utc>, WeakIndex);

impl PointIndex {
    /// `None` for 0: the point path off, every field decoded in full.
    ///
    /// One shard, as the grid and message caches (#1302): entries range from
    /// a CCSDS index of 1.2 KB at 0.25° (6.3 KB at 0.1°) to a GFS 0.25° 5.3
    /// index of about 110 KB, and no entry hint shards one budget for both.
    /// Sharded from a 4 KiB hint, 16 MiB was 64 slices of 256 KiB, two GFS
    /// indexes each, so a window's indexes evicted each other. Lookups are
    /// one per field, so one lock costs nothing.
    pub(crate) fn new(size_mb: u64) -> Option<Self> {
        (size_mb > 0).then(|| Self {
            cache: ByteBoundedCache::new_single_shard(size_mb.saturating_mul(MIB), 4096, weight),
            readable: RwLock::new(HashMap::new()),
            siblings: RwLock::new(HashMap::new()),
        })
    }

    pub(crate) fn metrics(&self) -> CacheMetrics {
        self.cache.metrics()
    }

    /// Whether `key`'s messages may be indexed: not yet seen, or the last
    /// one could be.
    pub(crate) fn may_read(&self, key: &ParameterKey) -> bool {
        self.readable
            .read()
            .unwrap()
            .get(key)
            .copied()
            .unwrap_or(true)
    }

    fn remember(&self, key: ParameterKey, readable: bool) {
        if self.readable.read().unwrap().get(&key) != Some(&readable) {
            self.readable.write().unwrap().insert(key, readable);
        }
    }

    /// Forget which parameters could be read, when the catalog publishes
    /// new runs or steps: each parameter's next message is tried again.
    /// Siblings whose indexes the cache has dropped go too.
    pub(crate) fn forget_layouts(&self) {
        self.readable.write().unwrap().clear();
        self.siblings.write().unwrap().retain(|_, runs| {
            runs.retain(|(_, index)| index.alive());
            !runs.is_empty()
        });
    }

    /// Keep `built` unless the cache already holds an index of the message
    /// reaching further; the one kept is returned.
    fn store_ccsds(&self, key: &MessageKey, built: CcsdsIndex) -> Arc<CcsdsIndex> {
        match self.cache.get_untracked(key) {
            Some(Index::Ccsds(cached)) if cached.indexed_rsis() >= built.indexed_rsis() => cached,
            _ => {
                let built = Arc::new(built);
                self.cache.insert(key.clone(), Index::Ccsds(built.clone()));
                built
            }
        }
    }

    /// [`Self::store_ccsds`] for a 5.3 index (#1302).
    fn store_complex(&self, key: &MessageKey, built: Arc<ComplexIndex>) -> Arc<ComplexIndex> {
        match self.cache.get_untracked(key) {
            Some(Index::Complex(cached)) if cached.indexed_rows() >= built.indexed_rows() => cached,
            _ => {
                self.cache
                    .insert(key.clone(), Index::Complex(built.clone()));
                built
            }
        }
    }

    /// Record `index` as `key`'s sibling in `run`.
    fn note_sibling(&self, key: ParameterKey, run: DateTime<Utc>, index: &Index) {
        let mut siblings = self.siblings.write().unwrap();
        let runs = siblings.entry(key).or_default();
        match runs.iter_mut().find(|(at, _)| *at == run) {
            Some((_, kept)) => {
                let shorter = kept
                    .upgrade()
                    .is_none_or(|kept| kept.reach() <= index.reach());
                if shorter {
                    *kept = index.downgrade();
                }
            }
            None => runs.push((run, index.downgrade())),
        }
    }

    /// The index a partial read predicts from, and its kin: `key`'s in
    /// `run`, else `key`'s in another run, else any parameter's, for its
    /// grid. `None` before any index exists.
    fn sibling(&self, key: &ParameterKey, run: DateTime<Utc>) -> Option<(Index, Kin)> {
        let siblings = self.siblings.read().unwrap();
        let alive = |runs: &Vec<RunIndex>| runs.iter().find_map(|(_, index)| index.upgrade());
        if let Some(runs) = siblings.get(key) {
            let same = runs
                .iter()
                .find(|(at, _)| *at == run)
                .and_then(|(_, index)| index.upgrade());
            if let Some(index) = same {
                return Some((index, Kin::SameRun));
            }
            if let Some(index) = alive(runs) {
                return Some((index, Kin::OtherRun));
            }
        }
        siblings
            .values()
            .find_map(alive)
            .map(|index| (index, Kin::OtherParameter))
    }
}

/// Position fields served from a CCSDS index and from a decoded grid, per
/// source, for `/metrics`.
#[derive(Default)]
pub(crate) struct PointFields {
    partial: AtomicU64,
    full: AtomicU64,
}

impl PointFields {
    /// `(partial, full)`.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (
            self.partial.load(Ordering::Relaxed),
            self.full.load(Ordering::Relaxed),
        )
    }
}

/// How the point path fetched message bytes (#1301).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchMode {
    /// The whole message.
    Whole,
    /// A predicted prefix of a message not yet indexed.
    Prefix,
    /// The indexed RSIs a query needs, or a read on from those an index
    /// knows.
    Range,
    /// The rest a short prefix or range needed.
    Continuation,
}

impl FetchMode {
    pub(crate) const ALL: [Self; 4] = [Self::Whole, Self::Prefix, Self::Range, Self::Continuation];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Prefix => "prefix",
            Self::Range => "range",
            Self::Continuation => "continuation",
        }
    }
}

/// GETs and bytes the point path fetched, by [`FetchMode`], per source.
#[derive(Default)]
pub(crate) struct PointFetches {
    gets: [AtomicU64; 4],
    bytes: [AtomicU64; 4],
}

impl PointFetches {
    fn record(&self, mode: FetchMode, bytes: usize) {
        self.gets[mode as usize].fetch_add(1, Ordering::Relaxed);
        self.bytes[mode as usize].fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// `(mode, GETs, bytes)` for each [`FetchMode`].
    pub(crate) fn counts(&self) -> [(&'static str, u64, u64); 4] {
        FetchMode::ALL.map(|mode| {
            (
                mode.label(),
                self.gets[mode as usize].load(Ordering::Relaxed),
                self.bytes[mode as usize].load(Ordering::Relaxed),
            )
        })
    }
}

/// One position field to read: its message, the run it belongs to, the
/// query's points and plan.
pub(crate) struct PointField<'a> {
    pub url: &'a str,
    pub entry: &'a MessageEntry,
    pub run: DateTime<Utc>,
    pub coords: &'a [(f64, f64)],
    pub plan: FieldPlan,
}

/// How one position field was read.
enum FieldRead {
    /// Samples from the point index, and what the message reports to the
    /// metadata caches.
    Points {
        values: Vec<Option<f64>>,
        metadata: MessageMetadata,
    },
    /// A decoded grid: resident, or a message the index cannot read.
    Grid(Arc<DecodedGrid>),
}

impl FieldRead {
    fn points(metadata: &MessageMetadata, values: Vec<Option<f64>>) -> Self {
        Self::Points {
            values,
            metadata: *metadata,
        }
    }
}

impl GribEngine {
    /// The raw samples of `field`'s points, before display conversion: from
    /// its point index when [`Self::point_index_for`] allows, else from the
    /// decoded grid [`Self::fetch_grid_by_entry`] reads under the plan's
    /// fill. Both interpolate alike ([`crate::cache::Bilinear`]), bit for
    /// bit.
    pub(crate) fn position_samples(
        &self,
        field: &PointField<'_>,
    ) -> Result<Vec<Option<f64>>, DataServerError> {
        let (url, entry, fill) = (field.url, field.entry, field.plan.fill);
        let read = match self.point_index_for(entry, fill) {
            Some(index) => self.read_points(index, field)?,
            None => FieldRead::Grid(self.fetch_grid_by_entry(url, entry, fill)?),
        };
        let fields = &self.source.point_fields;
        Ok(match read {
            FieldRead::Points { values, .. } => {
                fields.partial.fetch_add(1, Ordering::Relaxed);
                values
            }
            FieldRead::Grid(grid) => {
                fields.full.fetch_add(1, Ordering::Relaxed);
                field
                    .coords
                    .iter()
                    .map(|&(lon, lat)| grid.bilinear_value(lon, lat))
                    .collect()
            }
        })
    }

    /// Accept `bytes`, `field`'s whole message, as a position query's first
    /// read of it does before the message cache admits it, for the
    /// pre-warm (#1335): build its point index when [`Self::point_index_for`]
    /// allows, else decode it in full. `field.coords` is empty: nothing is
    /// sampled, and a later query samples from the index alone.
    pub(crate) fn index_message(
        &self,
        field: &PointField<'_>,
        bytes: &[u8],
    ) -> Result<(), DataServerError> {
        let entry = field.entry;
        match self.point_index_for(entry, field.plan.fill) {
            Some(index) => {
                let key = MessageKey::new(Path::from(field.url).as_ref(), entry);
                self.points_from(index, &key, field, bytes).map(drop)
            }
            None => reader::decode_message(bytes, &entry.param).map(drop),
        }
    }

    /// The point index, when this field may be read through it: the index is
    /// enabled, the parameter's messages have not shown a layout it cannot
    /// read, and the field is not one the decoded-grid cache would keep with
    /// no compressed message to fall back on. Such a grid serves a repeated
    /// query with no request at all, where a point read would fetch the
    /// message again.
    fn point_index_for(&self, entry: &MessageEntry, fill: GridFill) -> Option<&PointIndex> {
        let index = self.source.point_index.as_ref()?;
        let kept = fill == GridFill::Insert
            && self.source.grid_cache.is_some()
            && self.source.message_cache.is_none();
        (!kept && index.may_read(&entry.key())).then_some(index)
    }

    /// One field through the point index: a resident decoded grid as it is
    /// (looked up without joining a fill or inserting), else the message's
    /// bytes sampled through its index. [`FieldFetch::Whole`] takes the
    /// whole message from the compressed-message cache or storage, and
    /// builds the index on first read; [`FieldFetch::Partial`] fetches only
    /// the bytes the points need ([`Self::fetch_points`]), and the whole
    /// message, outside the cache, when that goes wrong. A message the
    /// index cannot read is decoded in full from bytes already read, never
    /// fetched twice. The grid cache counts a hit for a resident grid and a
    /// miss for a full decode, as the full path would, and nothing for a
    /// field read through the index.
    fn read_points(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
    ) -> Result<FieldRead, DataServerError> {
        let (url, entry, plan) = (field.url, field.entry, field.plan);
        // Only a grid found, or one decoded in full, counts in the grid
        // cache: a field read through the index loads none.
        if let Some(cache) = &self.source.grid_cache {
            if let Some(grid) = cache.get_untracked(url, entry.offset) {
                cache.record_hit();
                self.note_grid(url, entry, &grid);
                return Ok(FieldRead::Grid(grid));
            }
        }
        ds_core::deadline::check()?;
        let path = Path::from(url);
        let key = MessageKey::new(path.as_ref(), entry);
        let fetches = &self.source.point_fetches;
        let sample = |bytes: &[u8]| self.points_from(index, &key, field, bytes);
        let read = if plan.fetch == FieldFetch::Partial && entry.length.is_some() {
            match self.fetch_points(index, field, &path, &key)? {
                Some(read) => read,
                None => {
                    let bytes = reader::fetch_message_bytes(&self.source.store, &path, entry)?;
                    fetches.record(FetchMode::Whole, bytes.len());
                    ds_core::deadline::check()?;
                    sample(&bytes)?
                }
            }
        } else {
            let (bytes, on_fill) = match &self.source.message_cache {
                // A window `holds` approved stays for the query's next run
                // (#1340).
                Some(cache) => cache.get_with(
                    &self.source.store,
                    &path,
                    entry,
                    plan.fetch == FieldFetch::Whole,
                    |bytes| {
                        fetches.record(FetchMode::Whole, bytes.len());
                        sample(bytes)
                    },
                )?,
                None => {
                    let bytes = reader::fetch_message_bytes(&self.source.store, &path, entry)?;
                    fetches.record(FetchMode::Whole, bytes.len());
                    ds_core::deadline::check()?;
                    let read = sample(&bytes)?;
                    (bytes, Some(read))
                }
            };
            match on_fill {
                Some(read) => read,
                None => sample(&bytes)?,
            }
        };
        match &read {
            FieldRead::Points { metadata, .. } => {
                let mut changed =
                    self.populate_message_metadata(&entry.key(), metadata, entry.step_kind);
                changed |= self.populate_geometry(url, entry, metadata.geometry);
                if changed {
                    self.refresh_discovery();
                }
            }
            FieldRead::Grid(grid) => {
                if let (Some(cache), GridFill::Insert) = (&self.source.grid_cache, plan.fill) {
                    cache.insert(url, entry.offset, grid.clone());
                }
                self.note_grid(url, entry, grid);
            }
        }
        Ok(read)
    }

    /// `field` from only the bytes its points need (#1301): a message the
    /// compressed-message cache holds is read from there; one whose index
    /// covers the needed RSIs with one range GET of them; one indexed short
    /// of them with one GET from the RSIs it knows on; one not indexed with
    /// a predicted prefix, from a sibling's index. A prefix or read on that
    /// ends early gets one continuation. A 5.3 message reads alike through
    /// its own index ([`complex_reads`], #1302). `None` for the caller to
    /// fetch the whole message: no index of the source yet to predict from,
    /// a header beyond the prefix, a short read, an index that does not
    /// match, a continuation still short. Storage errors and the deadline
    /// fail the field, as on the full path.
    fn fetch_points(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
        path: &Path,
        key: &MessageKey,
    ) -> Result<Option<FieldRead>, DataServerError> {
        let (entry, run, coords) = (field.entry, field.run, field.coords);
        let Some(len) = entry.length.and_then(|len| usize::try_from(len).ok()) else {
            return Ok(None);
        };
        if let Some(bytes) = self
            .source
            .message_cache
            .as_ref()
            .and_then(|cache| cache.get_resident(key))
        {
            return self.points_from(index, key, field, &bytes).map(Some);
        }
        let unexpected = |error: PointError| match error {
            PointError::Fatal(error) => Err(error),
            error => {
                tracing::debug!(parameter = %entry.param, error = %error, "GRIB point field fetched whole");
                Ok(None)
            }
        };
        let any_sibling = index.sibling(&entry.key(), run);
        let sibling = any_sibling
            .as_ref()
            .and_then(|(index, kin)| Some((index.as_ccsds()?, *kin)));
        let cached = index.cache.get_untracked(key);
        let (header, mut prior, mut bytes, start) = match cached {
            Some(Index::Complex(complex)) => {
                return self.complex_warm(index, field, path, key, complex);
            }
            Some(Index::Ccsds(ccsds)) => match ccsds.needs(coords) {
                Some(needs) if ccsds.covers(&needs) => {
                    index.cache.record_hit();
                    let Some(range) = ccsds.rsi_bytes(&needs) else {
                        return Ok(None);
                    };
                    let start = range.start;
                    let Some(bytes) = self.fetch_part(path, entry, range, FetchMode::Range)? else {
                        return Ok(None);
                    };
                    return match ccsds.sample_bytes(&bytes, start, coords) {
                        Ok(values) => Ok(Some(FieldRead::points(ccsds.metadata(), values))),
                        Err(error) => unexpected(error),
                    };
                }
                None => {
                    index.cache.record_hit();
                    return match ccsds.sample_bytes(&[], 0, coords) {
                        Ok(values) => Ok(Some(FieldRead::points(ccsds.metadata(), values))),
                        Err(error) => unexpected(error),
                    };
                }
                Some(needs) => {
                    index.cache.record_miss();
                    let range = ccsds.read_on(&needs, sibling);
                    let start = range.start;
                    let Some(bytes) = self.fetch_part(path, entry, range, FetchMode::Range)? else {
                        return Ok(None);
                    };
                    (ccsds.header(), Some(ccsds), bytes, start)
                }
            },
            None => {
                // Nothing indexed yet to predict a prefix from: the first
                // whole read indexes the message, and seeds the predictions.
                let Some((sibling, kin)) = &any_sibling else {
                    return Ok(None);
                };
                let until = sibling.predict_prefix(coords, len, *kin);
                let Some(bytes) = self.fetch_part(path, entry, 0..until, FetchMode::Prefix)? else {
                    return Ok(None);
                };
                if ccsds::data_template(&bytes) == Some(COMPLEX_TEMPLATE) {
                    return self.complex_cold(index, field, path, key, bytes);
                }
                match Header::parse_prefix(&bytes, len) {
                    Ok(Some(header)) => {
                        index.cache.record_miss();
                        (header, None, bytes, 0)
                    }
                    Ok(None) => return Ok(None),
                    // Another layout: read the rest and decode it in full.
                    Err(PointError::Unsupported(_)) if until < len => {
                        let Some(rest) =
                            self.fetch_part(path, entry, until..len, FetchMode::Continuation)?
                        else {
                            return Ok(None);
                        };
                        let whole = [&bytes[..], &rest[..]].concat();
                        return self.points_from(index, key, field, &whole).map(Some);
                    }
                    Err(PointError::Unsupported(_)) => {
                        return self.points_from(index, key, field, &bytes).map(Some);
                    }
                    Err(error) => return unexpected(error),
                }
            }
        };
        let mut continued = false;
        loop {
            match header.extend(prior.as_deref(), &bytes, start, coords) {
                Ok(Extended::Done {
                    index: built,
                    values,
                }) => {
                    let kept = index.store_ccsds(key, built);
                    index.note_sibling(entry.key(), run, &Index::Ccsds(kept.clone()));
                    index.remember(entry.key(), true);
                    return Ok(Some(FieldRead::points(kept.metadata(), values)));
                }
                Ok(Extended::Short { index: partial }) => {
                    let kept = index.store_ccsds(key, partial);
                    index.note_sibling(entry.key(), run, &Index::Ccsds(kept.clone()));
                    let end = start + bytes.len();
                    let Some(needs) = kept.needs(coords).filter(|_| !continued) else {
                        return Ok(None);
                    };
                    // The prediction already fell short once: read on as far
                    // as either the sibling or an even packing of the rest
                    // says, and the rest of the message when neither passes
                    // what was read.
                    let until = Some(kept.read_on(&needs, sibling).end)
                        .max(Some(kept.read_on(&needs, None).end))
                        .filter(|&until| until > end)
                        .unwrap_or(len);
                    continued = true;
                    let Some(more) =
                        self.fetch_part(path, entry, end..until, FetchMode::Continuation)?
                    else {
                        return Ok(None);
                    };
                    bytes = Bytes::from([&bytes[..], &more[..]].concat());
                    prior = Some(kept);
                }
                Err(error) => return unexpected(error),
            }
        }
    }

    /// Bytes `range` of the message, one GET counted under `mode`; `None`
    /// for a read shorter than asked. Checks the deadline first.
    fn fetch_part(
        &self,
        path: &Path,
        entry: &MessageEntry,
        range: Range<usize>,
        mode: FetchMode,
    ) -> Result<Option<Bytes>, DataServerError> {
        ds_core::deadline::check()?;
        let Some(absolute) = usize::try_from(entry.offset)
            .ok()
            .and_then(|offset| {
                Some(offset.checked_add(range.start)?..offset.checked_add(range.end)?)
            })
            .filter(|absolute| !absolute.is_empty())
        else {
            return Ok(None);
        };
        let bytes = self.source.store.get_range(path, absolute).map_err(|e| {
            DataServerError::Storage(format!(
                "Failed to fetch GRIB message range {}/{}: {e}",
                entry.param,
                entry.level.map_or("sfc".to_string(), |l| l.to_string()),
            ))
        })?;
        self.source.point_fetches.record(mode, bytes.len());
        if bytes.len() != range.len() {
            tracing::debug!(
                parameter = %entry.param,
                expected = range.len(),
                got = bytes.len(),
                "Short GRIB point range read"
            );
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// `field`'s points from `bytes`, the whole message, through its index:
    /// the cached one when it reaches them, else one built (and the points
    /// sampled during that one decode) from these bytes, replacing a partial
    /// one. A 5.3 message goes to its own reader
    /// ([`Self::complex_points_from`]). Decodes the message in full when the
    /// index cannot read it; only the deadline and that full decode's own
    /// errors fail the field.
    fn points_from(
        &self,
        index: &PointIndex,
        key: &MessageKey,
        field: &PointField<'_>,
        bytes: &[u8],
    ) -> Result<FieldRead, DataServerError> {
        if ccsds::data_template(bytes) == Some(COMPLEX_TEMPLATE) {
            return self.complex_points_from(index, key, field, bytes);
        }
        let (entry, run, coords) = (field.entry, field.run, field.coords);
        let full = |error: PointError| self.decode_in_full(index, field, bytes, error);
        let cached = index.cache.get_untracked(key);
        if let Some(ccsds) = cached
            .as_ref()
            .and_then(Index::as_ccsds)
            .filter(|ccsds| ccsds.reaches(coords))
        {
            index.cache.record_hit();
            return match ccsds.sample(bytes, coords) {
                Ok(values) => Ok(FieldRead::points(ccsds.metadata(), values)),
                Err(error) => full(error),
            };
        }
        let header = match Header::parse(bytes) {
            Ok(header) => header,
            Err(error) => return full(error),
        };
        let mut sampled = None;
        let built = if cached.is_some() {
            // A partial index from an earlier range read: replace it.
            index.cache.record_miss();
            header.index(bytes, coords).map(|(ccsds, values)| {
                sampled = Some(values);
                index.store_ccsds(key, ccsds)
            })
        } else {
            index
                .cache
                .get_or_insert_with(key, || {
                    let (ccsds, values) = header.index(bytes, coords)?;
                    sampled = Some(values);
                    Ok::<_, PointError>(Index::Ccsds(Arc::new(ccsds)))
                })
                .and_then(|built| match built {
                    Index::Ccsds(ccsds) => Ok(ccsds),
                    Index::Complex(_) => Err(PointError::Invalid("a 5.3 index".into())),
                })
        };
        let ccsds = match built {
            Ok(ccsds) => ccsds,
            Err(error) => return full(error),
        };
        index.remember(entry.key(), true);
        index.note_sibling(entry.key(), run, &Index::Ccsds(ccsds.clone()));
        match sampled {
            Some(values) => Ok(FieldRead::points(ccsds.metadata(), values)),
            // Another reader built the index meanwhile.
            None => match ccsds.sample(bytes, coords) {
                Ok(values) => Ok(FieldRead::points(ccsds.metadata(), values)),
                Err(error) => full(error),
            },
        }
    }

    /// `field`'s message, `bytes`, decoded in full after a point reader
    /// gave `error`: fatal errors fail the field, and a layout the reader
    /// does not take marks the parameter unreadable until the catalog next
    /// publishes. Counts what the full path counts before it decodes: a
    /// grid-cache miss for a grid the cache keeps, a bypass for one read
    /// around it (#1308).
    fn decode_in_full(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
        bytes: &[u8],
        error: PointError,
    ) -> Result<FieldRead, DataServerError> {
        let entry = field.entry;
        if let PointError::Fatal(error) = error {
            return Err(error);
        }
        tracing::debug!(parameter = %entry.param, error = %error, "GRIB point field decoded in full");
        if matches!(error, PointError::Unsupported(_)) {
            index.remember(entry.key(), false);
        }
        if let Some(cache) = &self.source.grid_cache {
            match field.plan.fill {
                GridFill::Insert => cache.record_miss(field.url, entry.offset),
                GridFill::Bypass => cache.record_bypass(),
            }
        }
        reader::decode_message(bytes, &entry.param).map(|grid| FieldRead::Grid(Arc::new(grid)))
    }
}
