//! Optional compressed-message retention behind the decoded-grid cache.
//!
//! Long time windows can exceed the grid cache. Keeping the smaller source
//! messages lets the next coordinate reuse their bytes, paying only decoding.

use std::sync::Arc;

use ds_cache::{ByteBoundedCache, CacheMetrics, MIB};
use ds_core::{deadline, error::DataServerError};
use ds_storage::{bytes::Bytes, object_store::path::Path, DataStore};

use crate::{cache::DecodedGrid, catalog::MessageEntry, reader};

/// One indexed message: source-local path, offset and indexed length. The
/// CCSDS point index cache keys its entries the same way (#1300).
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct MessageKey {
    path: Arc<str>,
    offset: u64,
    // A changed indexed range must not reuse bytes from an earlier range.
    length: Option<u64>,
}

impl MessageKey {
    pub(crate) fn new(path: &str, entry: &MessageEntry) -> Self {
        Self {
            path: Arc::from(path),
            offset: entry.offset,
            length: entry.length,
        }
    }

    /// The key's own bytes and its path, for cache weights.
    pub(crate) fn size_bytes(&self) -> usize {
        self.path.len() + size_of::<Self>()
    }
}

fn weight(key: &MessageKey, bytes: &Bytes) -> u64 {
    // Count the owned payload, key and Arc/cache-node overhead.
    (bytes.len() + key.size_bytes() + size_of::<Bytes>() + 96) as u64
}

pub(crate) struct MessageCache {
    cache: ByteBoundedCache<MessageKey, Bytes>,
}

impl MessageCache {
    /// One shard: a position query plans whether its messages stay resident
    /// (#1301, [`Self::holds`]). Sharded from a 0.5 MiB entry hint, a
    /// 1024 MiB cache was four shards per CPU, 64 of 16 MiB on 12 CPUs, so
    /// a 41-step 0.1° window (55 MB) that fits the total could still
    /// overflow the shards its messages hash to. Lookups are one per field
    /// or map tile, so one lock costs nothing.
    pub fn new(size_mb: u64) -> Option<Self> {
        (size_mb > 0).then(|| Self {
            cache: ByteBoundedCache::new_single_shard(size_mb.saturating_mul(MIB), MIB / 2, weight),
        })
    }

    /// Whether messages of `bytes` together stay resident: within one
    /// shard's budget, all of it for this one-shard cache.
    pub(crate) fn holds(&self, bytes: u64) -> bool {
        bytes <= self.cache.shard_capacity_bytes()
    }

    /// The message's bytes when resident, counting a hit; nothing when
    /// absent, for a reader that then fetches only part of the message and
    /// never fills the cache (#1301).
    pub(crate) fn get_resident(&self, key: &MessageKey) -> Option<Bytes> {
        let bytes = self.cache.get_untracked(key)?;
        self.cache.record_hit();
        Some(bytes)
    }

    pub fn read(
        &self,
        store: &DataStore,
        path: &Path,
        entry: &MessageEntry,
    ) -> Result<DecodedGrid, DataServerError> {
        let (bytes, decoded) = self.get_with(store, path, entry, |bytes| {
            reader::decode_message(bytes, &entry.param)
        })?;
        let grid = match decoded {
            Some(grid) => grid, // reuse the validating decode on a cold fill
            None => reader::decode_message(&bytes, &entry.param)?,
        };
        deadline::check()?;
        Ok(grid)
    }

    /// The message's bytes, one fetch shared by concurrent readers. A miss
    /// fetches them and admits them only once `validate` accepts them, so a
    /// truncated, corrupt or unsupported payload stays retryable, like a
    /// failed GET. Returns what `validate` produced on that fill, `None` on
    /// a hit: [`Self::read`] decodes, a CCSDS point read (#1300) indexes.
    pub(crate) fn get_with<T>(
        &self,
        store: &DataStore,
        path: &Path,
        entry: &MessageEntry,
        validate: impl FnOnce(&[u8]) -> Result<T, DataServerError>,
    ) -> Result<(Bytes, Option<T>), DataServerError> {
        deadline::check()?;
        let key = MessageKey::new(path.as_ref(), entry);
        let mut validated = None;
        let bytes = self.cache.get_or_insert_with(&key, || {
            let bytes = reader::fetch_message_bytes(store, path, entry)?;
            deadline::check()?;
            validated = Some(validate(&bytes)?);
            deadline::check()?;
            // Object stores can return a slice backed by the entire file.
            // Retain exactly this message's bytes so the byte budget is real.
            // Oversized values are never admitted: avoid copying those buffers.
            Ok::<_, DataServerError>(if weight(&key, &bytes) <= self.cache.capacity_bytes() {
                Bytes::copy_from_slice(&bytes)
            } else {
                bytes
            })
        })?;
        deadline::check()?;
        Ok((bytes, validated))
    }

    pub fn metrics(&self) -> CacheMetrics {
        self.cache.metrics()
    }
}

#[cfg(test)]
mod tests;
