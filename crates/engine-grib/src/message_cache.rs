//! Optional compressed-message retention behind the decoded-grid cache.
//!
//! Long time windows can exceed the grid cache. Keeping the smaller source
//! messages lets the next coordinate reuse their bytes, paying only decoding.

use std::sync::Arc;

use ds_cache::{ByteBoundedCache, CacheMetrics, MIB};
use ds_core::{deadline, error::DataServerError};
use ds_storage::{bytes::Bytes, object_store::path::Path, DataStore};

use crate::{cache::DecodedGrid, catalog::MessageEntry, reader};

#[derive(Clone, Hash, PartialEq, Eq)]
struct MessageKey {
    path: Arc<str>,
    offset: u64,
    // A changed indexed range must not reuse bytes from an earlier range.
    length: Option<u64>,
}

fn weight(key: &MessageKey, bytes: &Bytes) -> u64 {
    // Count the owned payload, key and Arc/cache-node overhead.
    (bytes.len() + key.path.len() + size_of::<MessageKey>() + size_of::<Bytes>() + 96) as u64
}

pub(crate) struct MessageCache {
    cache: ByteBoundedCache<MessageKey, Bytes>,
}

impl MessageCache {
    pub fn new(size_mb: u64) -> Option<Self> {
        (size_mb > 0).then(|| Self {
            cache: ByteBoundedCache::new(size_mb.saturating_mul(MIB), MIB / 2, weight),
        })
    }

    pub fn read(
        &self,
        store: &DataStore,
        path: &Path,
        entry: &MessageEntry,
    ) -> Result<DecodedGrid, DataServerError> {
        deadline::check()?;
        let key = MessageKey {
            path: Arc::from(path.as_ref()),
            offset: entry.offset,
            length: entry.length,
        };
        let mut decoded = None;
        let bytes = self.cache.get_or_insert_with(&key, || {
            let bytes = reader::fetch_message_bytes(store, path, entry)?;
            deadline::check()?;
            // Admit only successfully decoded fields. A truncated/corrupt or
            // unsupported payload must remain retryable, like a failed GET.
            let grid = reader::decode_message(&bytes, &entry.param)?;
            deadline::check()?;
            decoded = Some(grid);
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
        let grid = match decoded {
            Some(grid) => grid, // reuse the validating decode on a cold fill
            None => reader::decode_message(&bytes, &entry.param)?,
        };
        deadline::check()?;
        Ok(grid)
    }

    pub fn metrics(&self) -> CacheMetrics {
        self.cache.metrics()
    }
}

#[cfg(test)]
mod tests;
