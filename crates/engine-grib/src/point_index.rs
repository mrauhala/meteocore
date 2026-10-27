//! Position queries read CCSDS-packed fields through a per-message index
//! (#1300): only the reference sample intervals holding their points'
//! bilinear neighbours are decoded, no grid is built, and nothing enters
//! the decoded-grid cache. Other fields, and every field when
//! `point_index_cache_mb` is 0, take the full decode as before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use ds_cache::{ByteBoundedCache, CacheMetrics, MIB};
use ds_core::error::DataServerError;
use ds_storage::object_store::path::Path;

use crate::cache::DecodedGrid;
use crate::catalog::{MessageEntry, ParameterKey};
use crate::ccsds::{CcsdsError, CcsdsIndex, Header};
use crate::message_cache::MessageKey;
use crate::metadata::MessageMetadata;
use crate::{reader, GribEngine, GridFill};

fn weight(key: &MessageKey, index: &Arc<CcsdsIndex>) -> u64 {
    // The index, its key and Arc/cache-node overhead.
    (index.size_bytes() + key.size_bytes() + 2 * size_of::<usize>() + 64) as u64
}

/// A source's CCSDS indexes, keyed like its compressed-message cache, and
/// which of its parameters' messages that index reads.
pub(crate) struct PointIndex {
    cache: ByteBoundedCache<MessageKey, Arc<CcsdsIndex>>,
    /// Per parameter and level: whether its last message read here could be
    /// indexed. One that could not (GFS complex packing, a field with a
    /// bitmap) takes the full decode directly until the catalog is next
    /// published ([`Self::forget_layouts`]), so one odd message cannot cost a
    /// parameter the point path for the engine's lifetime.
    readable: RwLock<HashMap<ParameterKey, bool>>,
}

impl PointIndex {
    /// `None` for 0: the point path off, every field decoded in full.
    pub(crate) fn new(size_mb: u64) -> Option<Self> {
        (size_mb > 0).then(|| Self {
            // A 0.25° global field's index is about 1 KB, a 0.1° one 6.3 KB.
            cache: ByteBoundedCache::new(size_mb.saturating_mul(MIB), 4096, weight),
            readable: RwLock::new(HashMap::new()),
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
    pub(crate) fn forget_layouts(&self) {
        self.readable.write().unwrap().clear();
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

/// How one position field was read.
enum FieldRead {
    /// Samples from the CCSDS index, and what the message reports to the
    /// metadata caches.
    Points {
        values: Vec<Option<f64>>,
        metadata: MessageMetadata,
    },
    /// A decoded grid: resident, or a message the index cannot read.
    Grid(Arc<DecodedGrid>),
}

impl GribEngine {
    /// The raw samples of `coords` in one position field, before display
    /// conversion: from its CCSDS index when [`Self::point_index_for`]
    /// allows, else from the decoded grid [`Self::fetch_grid_by_entry`]
    /// reads under `fill`. Both interpolate alike ([`crate::cache::Bilinear`]),
    /// bit for bit.
    pub(crate) fn position_samples(
        &self,
        url: &str,
        entry: &MessageEntry,
        fill: GridFill,
        coords: &[(f64, f64)],
    ) -> Result<Vec<Option<f64>>, DataServerError> {
        let read = match self.point_index_for(entry, fill) {
            Some(index) => self.read_points(index, url, entry, fill, coords)?,
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
                coords
                    .iter()
                    .map(|&(lon, lat)| grid.bilinear_value(lon, lat))
                    .collect()
            }
        })
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
    /// bytes from the compressed-message cache or storage, sampled through
    /// its index, built on first read. A message the index cannot read is
    /// decoded in full from the same bytes, never fetched twice. The grid
    /// cache counts a hit for a resident grid and a miss for a full decode,
    /// as the full path would, and nothing for a field read through the
    /// index.
    fn read_points(
        &self,
        index: &PointIndex,
        url: &str,
        entry: &MessageEntry,
        fill: GridFill,
        coords: &[(f64, f64)],
    ) -> Result<FieldRead, DataServerError> {
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
        let sample = |bytes: &[u8]| self.points_from(index, &key, entry, bytes, coords);
        let (bytes, on_fill) = match &self.source.message_cache {
            Some(cache) => cache.get_with(&self.source.store, &path, entry, sample)?,
            None => {
                let bytes = reader::fetch_message_bytes(&self.source.store, &path, entry)?;
                ds_core::deadline::check()?;
                let read = sample(&bytes)?;
                (bytes, Some(read))
            }
        };
        let read = match on_fill {
            Some(read) => read,
            None => sample(&bytes)?,
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
                if let (Some(cache), GridFill::Insert) = (&self.source.grid_cache, fill) {
                    cache.insert(url, entry.offset, grid.clone());
                }
                self.note_grid(url, entry, grid);
            }
        }
        Ok(read)
    }

    /// `coords` from `bytes` through the message's index, built (and the
    /// points sampled during that one decode) on first read. Decodes the
    /// message in full when the index cannot read it; only the deadline and
    /// that full decode's own errors fail the field.
    fn points_from(
        &self,
        index: &PointIndex,
        key: &MessageKey,
        entry: &MessageEntry,
        bytes: &[u8],
        coords: &[(f64, f64)],
    ) -> Result<FieldRead, DataServerError> {
        let full = |error: CcsdsError| match error {
            CcsdsError::Fatal(error) => Err(error),
            error => {
                tracing::debug!(parameter = %entry.param, error = %error, "GRIB point field decoded in full");
                if matches!(error, CcsdsError::Unsupported(_)) {
                    index.remember(entry.key(), false);
                }
                // The miss the full path counts before it decodes.
                if let Some(cache) = &self.source.grid_cache {
                    cache.record_miss();
                }
                reader::decode_message(bytes, &entry.param)
                    .map(|grid| FieldRead::Grid(Arc::new(grid)))
            }
        };
        let points = |ccsds: &CcsdsIndex, values| FieldRead::Points {
            values,
            metadata: *ccsds.metadata(),
        };
        if let Some(ccsds) = index.cache.get_untracked(key) {
            index.cache.record_hit();
            return match ccsds.sample(bytes, coords) {
                Ok(values) => Ok(points(&ccsds, values)),
                Err(error) => full(error),
            };
        }
        let header = match Header::parse(bytes) {
            Ok(header) => header,
            Err(error) => return full(error),
        };
        let mut sampled = None;
        let built = index.cache.get_or_insert_with(key, || {
            let (ccsds, values) = header.index(bytes, coords)?;
            sampled = Some(values);
            Ok::<_, CcsdsError>(Arc::new(ccsds))
        });
        let ccsds = match built {
            Ok(ccsds) => ccsds,
            Err(error) => return full(error),
        };
        index.remember(entry.key(), true);
        match sampled {
            Some(values) => Ok(points(&ccsds, values)),
            // Another reader built the index meanwhile.
            None => match ccsds.sample(bytes, coords) {
                Ok(values) => Ok(points(&ccsds, values)),
                Err(error) => full(error),
            },
        }
    }
}
