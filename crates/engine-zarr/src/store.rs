//! A `zarrs` storage adapter over [`ds_storage::DataStore`].
//!
//! Bridges zarrs's synchronous `ReadableStorageTraits` / `ListableStorageTraits`
//! to the shared `ds-storage` object-store layer (local, S3, HTTP), so the Zarr
//! engine reaches every backend through the same code path as the other engines
//! (#125 Phase 2).
//!
//! Two invariants make this safe and effective:
//!
//! - **Single-threaded retrieval.** The engine drives every read with
//!   `CodecOptions::with_concurrent_target(1)` (see [`crate::catalog`]), so zarrs
//!   never dispatches a storage read onto a `rayon` worker. ds-storage's
//!   `block_in_place` bridge is valid on the calling request/poll thread but
//!   *panics* on a rayon pool thread — so the single-thread setting is
//!   load-bearing, not just a tuning knob (CLAUDE.md storage rules).
//! - **Whole-object reads + LRU cache.** Non-sharded Zarr chunks are read in
//!   full; the adapter caches the full object bytes (keyed by store key) and
//!   serves byte-range requests by slicing the cached buffer, so a time-series
//!   scan over one spatial neighbourhood re-reads each chunk at most once.
//!   (Sharded objects are read whole — a documented Phase-2 trade-off.)

use std::sync::Arc;

use bytes::Bytes;
use quick_cache::sync::Cache;

use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::DataStore;

use zarrs::storage::byte_range::ByteRangeIterator;
use zarrs::storage::{
    ListableStorageTraits, MaybeBytes, MaybeBytesIterator, ReadableStorageTraits, StorageError,
    StoreKey, StoreKeys, StoreKeysPrefixes, StorePrefix,
};

/// Weights cache entries by payload size (plus a small per-entry overhead).
#[derive(Clone)]
struct BytesWeighter;
impl quick_cache::Weighter<String, Bytes> for BytesWeighter {
    fn weight(&self, key: &String, val: &Bytes) -> u64 {
        val.len() as u64 + key.len() as u64 + 64
    }
}

/// A readable + listable zarrs store backed by `ds-storage`.
pub struct DsStore {
    store: DataStore,
    /// Object-path prefix prepended to every zarrs key — the store's location
    /// within the bucket. Empty for a locally-rooted store. No leading/trailing
    /// slashes.
    root: String,
    cache: Cache<String, Bytes, BytesWeighter>,
}

impl DsStore {
    /// Build an adapter over `store`, rooted at `root` (the store location
    /// within the backend; `""` for a locally-rooted store), with a chunk cache
    /// of `cache_mb` megabytes.
    pub fn new(store: DataStore, root: impl Into<String>, cache_mb: u64) -> Self {
        let max_bytes = cache_mb.saturating_mul(1024 * 1024).max(1024 * 1024);
        Self {
            store,
            root: root.into().trim_matches('/').to_string(),
            // 1024 is just a sizing hint; eviction is driven by `max_bytes`.
            cache: Cache::with_weighter(1024, max_bytes, BytesWeighter),
        }
    }

    /// Map a zarrs key to a backend object path, applying the root prefix.
    fn object_path(&self, key: &str) -> ObjectPath {
        if self.root.is_empty() {
            ObjectPath::from(key)
        } else {
            ObjectPath::from(format!("{}/{}", self.root, key))
        }
    }

    /// Strip the root prefix from a backend object path, returning the
    /// zarrs-relative key, or `None` if the path is outside the root.
    fn strip_root<'a>(&self, full: &'a str) -> Option<&'a str> {
        if self.root.is_empty() {
            return Some(full);
        }
        full.strip_prefix(self.root.as_str())
            .map(|s| s.trim_start_matches('/'))
    }

    /// Fetch a full object, caching it. `None` when the key is absent.
    fn get_full(&self, key: &StoreKey) -> Result<Option<Bytes>, StorageError> {
        let k = key.as_str();
        if let Some(b) = self.cache.get(k) {
            return Ok(Some(b));
        }
        match self.store.get_opt(&self.object_path(k)).map_err(io_err)? {
            Some(b) => {
                self.cache.insert(k.to_string(), b.clone());
                Ok(Some(b))
            }
            None => Ok(None),
        }
    }
}

/// Wrap any ds-storage error as a zarrs `StorageError` (which has no free-text
/// variant, so we route through an IO error).
fn io_err(e: ds_core::error::DataServerError) -> StorageError {
    StorageError::from(Arc::new(std::io::Error::other(e.to_string())))
}

impl ReadableStorageTraits for DsStore {
    fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        self.get_full(key)
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        let Some(full) = self.get_full(key)? else {
            return Ok(None);
        };
        let size = full.len() as u64;
        // Resolve each requested range against the whole object and slice it
        // (cheap refcounted `Bytes::slice`). Collected eagerly so the returned
        // iterator borrows nothing from `self`.
        let slices: Vec<Result<Bytes, StorageError>> = byte_ranges
            .map(|br| {
                let start = br.start(size);
                let end = br.end(size).min(size);
                if start > end {
                    Err(io_err(ds_core::error::DataServerError::Storage(format!(
                        "invalid byte range {start}..{end} for {size}-byte object"
                    ))))
                } else {
                    Ok(full.slice(start as usize..end as usize))
                }
            })
            .collect();
        Ok(Some(Box::new(slices.into_iter())))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        // The whole-object read populates the cache, so a following
        // `get_partial_many`/`get` for the same key is free.
        Ok(self.get_full(key)?.map(|b| b.len() as u64))
    }

    fn supports_get_partial(&self) -> bool {
        // Partials are synthesised by slicing a whole-object read, not by a
        // server-side range request, so report no efficient partial support.
        false
    }
}

impl ListableStorageTraits for DsStore {
    fn list(&self) -> Result<StoreKeys, StorageError> {
        let root = StorePrefix::new(String::new()).map_err(StorageError::from)?;
        self.list_prefix(&root)
    }

    fn list_prefix(&self, prefix: &StorePrefix) -> Result<StoreKeys, StorageError> {
        let metas = self
            .store
            .list(&self.object_path(prefix.as_str()))
            .map_err(io_err)?;
        let mut keys = Vec::with_capacity(metas.len());
        for m in metas {
            if let Some(rel) = self.strip_root(m.location.as_ref()) {
                if let Ok(k) = StoreKey::new(rel) {
                    keys.push(k);
                }
            }
        }
        Ok(keys)
    }

    fn list_dir(&self, prefix: &StorePrefix) -> Result<StoreKeysPrefixes, StorageError> {
        let (objects, prefixes) = self
            .store
            .list_dir(&self.object_path(prefix.as_str()))
            .map_err(io_err)?;
        let mut keys = Vec::with_capacity(objects.len());
        for m in objects {
            if let Some(rel) = self.strip_root(m.location.as_ref()) {
                if let Ok(k) = StoreKey::new(rel) {
                    keys.push(k);
                }
            }
        }
        let mut child_prefixes = Vec::with_capacity(prefixes.len());
        for p in prefixes {
            if let Some(rel) = self.strip_root(p.as_ref()) {
                let rel = if rel.ends_with('/') {
                    rel.to_string()
                } else {
                    format!("{rel}/")
                };
                if let Ok(sp) = StorePrefix::new(rel) {
                    child_prefixes.push(sp);
                }
            }
        }
        Ok(StoreKeysPrefixes::new(keys, child_prefixes))
    }

    fn size_prefix(&self, prefix: &StorePrefix) -> Result<u64, StorageError> {
        let metas = self
            .store
            .list(&self.object_path(prefix.as_str()))
            .map_err(io_err)?;
        Ok(metas.iter().map(|m| m.size as u64).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_root(root: &str) -> DsStore {
        let inner = Arc::new(ds_storage::object_store::memory::InMemory::new());
        DsStore::new(DataStore::new(inner), root, 16)
    }

    #[test]
    fn object_path_applies_root_prefix() {
        let s = store_with_root("zarr/era5.zarr");
        assert_eq!(
            s.object_path("t2m/c/0/0/0").as_ref(),
            "zarr/era5.zarr/t2m/c/0/0/0"
        );
        // A leading/trailing slash on the configured root is normalised away.
        let s2 = store_with_root("/zarr/era5.zarr/");
        assert_eq!(
            s2.object_path("lat/zarr.json").as_ref(),
            "zarr/era5.zarr/lat/zarr.json"
        );
    }

    #[test]
    fn strip_root_inverts_object_path() {
        let s = store_with_root("zarr/era5.zarr");
        assert_eq!(
            s.strip_root("zarr/era5.zarr/t2m/c/0/0/0"),
            Some("t2m/c/0/0/0")
        );
        assert_eq!(s.strip_root("outside/x"), None);
    }

    #[test]
    fn empty_root_passes_keys_through() {
        let s = store_with_root("");
        assert_eq!(s.object_path("t2m/zarr.json").as_ref(), "t2m/zarr.json");
        assert_eq!(s.strip_root("t2m/zarr.json"), Some("t2m/zarr.json"));
    }
}
