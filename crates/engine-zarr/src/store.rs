//! A `zarrs` storage adapter over [`ds_storage::DataStore`].
//!
//! Bridges zarrs's synchronous `ReadableStorageTraits` / `ListableStorageTraits`
//! to the shared `ds-storage` object-store layer (local, S3, HTTP), so the Zarr
//! engine reaches plain Zarr stores through the same code path as the other
//! engines (#125 Phase 2). Icechunk uses its own storage adapter and runtime.
//!
//! Two invariants make this safe and effective:
//!
//! - **Single-threaded plain-Zarr retrieval.** The engine drives each read with
//!   `CodecOptions::with_concurrent_target(1)` (see [`crate::catalog`]), so zarrs
//!   never dispatches a storage read onto a `rayon` worker. Those workers lose
//!   the calling thread's deadline and runtime context; ds-storage may create
//!   a runtime per call. Plain reads stay on the engine execution thread;
//!   Icechunk's separately admitted fan-out uses its own runtime bridge.
//! - **Whole-object reads + LRU cache.** Non-sharded Zarr chunks are read in
//!   full; the adapter caches object bytes by catalog generation and store key
//!   and serves byte ranges by slicing retained buffers. Repeated reads within
//!   one catalog reuse bytes while they remain in the cache.
//!   (Sharded objects are read whole — a documented Phase-2 trade-off.)

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use ds_cache::ByteBoundedCache;

use ds_storage::object_store::path::Path as ObjectPath;
use ds_storage::DataStore;

use zarrs::storage::byte_range::ByteRangeIterator;
use zarrs::storage::{
    ListableStorageTraits, MaybeBytes, MaybeBytesIterator, ReadableStorageTraits, StorageError,
    StoreKey, StoreKeys, StoreKeysPrefixes, StorePrefix,
};

#[derive(Hash, PartialEq, Eq)]
struct Key {
    generation: u64,
    path: String,
}

fn weigh_bytes(key: &Key, val: &Option<Bytes>) -> u64 {
    val.as_ref().map_or(0, |bytes| bytes.len() as u64) + key.path.len() as u64 + 80
}

/// Shared across catalog generations: refresh must not multiply cache budgets
/// or rebuild object-store clients. Missing keys are cached too, so an old
/// generation can keep returning fill values after a new chunk appears.
struct Shared {
    store: DataStore,
    /// Object-path prefix prepended to every zarrs key — the store's location
    /// within the bucket. Empty for a locally-rooted store. No leading/trailing
    /// slashes.
    root: String,
    cache: ByteBoundedCache<Key, Option<Bytes>>,
}

pub(crate) struct Generation {
    pub(crate) version: u64,
    retired: AtomicBool,
}

impl Generation {
    fn new() -> Self {
        // Nonzero and unique across collection rebuilds in this process; seed
        // from wall time so browser validators also change across restarts.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128 - 1) as u64;
        let previous = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
                previous.checked_add(1).map(|next| next.max(now))
            })
            .expect("Zarr generation counter exhausted");
        Self {
            version: (previous + 1).max(now),
            retired: AtomicBool::new(false),
        }
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }

    fn check_active(&self) -> Result<(), StorageError> {
        if self.retired.load(Ordering::Acquire) {
            // The next request can use the current catalog. Preserve this
            // typed signal through zarrs so HTTP handlers return a retryable 503.
            Err(io_err(ds_core::error::DataServerError::ResourceExhausted))
        } else {
            Ok(())
        }
    }
}

/// A readable + listable store with a generation scoped to one catalog.
pub struct DsStore {
    shared: Arc<Shared>,
    pub(crate) generation: Arc<Generation>,
}

impl DsStore {
    /// Build an adapter over `store`, rooted at `root` (the store location
    /// within the backend; `""` for a locally-rooted store), with a chunk cache
    /// of `cache_mb` megabytes shared by all catalog generations. Zero disables
    /// retention.
    pub fn new(store: DataStore, root: impl Into<String>, cache_mb: u64) -> Self {
        let max_bytes = cache_mb.saturating_mul(ds_cache::MIB);
        Self {
            shared: Arc::new(Shared {
                store,
                root: root.into().trim_matches('/').to_string(),
                cache: ByteBoundedCache::new(max_bytes, ds_cache::MIB, weigh_bytes),
            }),
            generation: Arc::new(Generation::new()),
        }
    }

    pub(crate) fn fresh(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            generation: Arc::new(Generation::new()),
        }
    }

    /// Map a zarrs key to a backend object path, applying the root prefix.
    fn object_path(&self, key: &str) -> ObjectPath {
        if self.shared.root.is_empty() {
            ObjectPath::from(key)
        } else {
            ObjectPath::from(format!("{}/{}", self.shared.root, key))
        }
    }

    /// Strip the root prefix from a backend object path, returning the
    /// zarrs-relative key, or `None` if the path is outside the root.
    fn strip_root<'a>(&self, full: &'a str) -> Option<&'a str> {
        if self.shared.root.is_empty() {
            return Some(full);
        }
        full.strip_prefix(self.shared.root.as_str())?
            .strip_prefix('/')
    }

    /// Fetch a full object, caching it. `None` when the key is absent.
    fn get_full(&self, key: &StoreKey) -> Result<Option<Bytes>, StorageError> {
        ds_core::deadline::check().map_err(io_err)?;
        let k = Key {
            generation: self.generation.version,
            path: key.as_str().to_owned(),
        };
        if let Some(bytes) = self.shared.cache.get(&k) {
            return Ok(bytes);
        }
        // Never refill a retired catalog from the current mutable backend.
        // Check again after I/O to cover a fetch racing retirement.
        self.generation.check_active()?;
        let bytes = self
            .shared
            .store
            .get_opt(&self.object_path(key.as_str()))
            .map_err(io_err)?;
        ds_core::deadline::check().map_err(io_err)?;
        self.generation.check_active()?;
        self.shared.cache.insert(k, bytes.clone());
        Ok(bytes)
    }
}

/// Preserve backend error types inside zarrs's cloneable IO error wrapper.
pub(crate) fn io_err(e: ds_core::error::DataServerError) -> StorageError {
    StorageError::from(Arc::new(std::io::Error::other(e)))
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

    // NOTE: recursive (no delimiter) — its contract is "every key under the
    // prefix". The engine's child/array discovery goes through `list_dir`
    // (one-level), so this is not on the open/read hot path; avoid calling it at
    // the store root of a large remote store, where it would enumerate every
    // chunk key.
    fn list_prefix(&self, prefix: &StorePrefix) -> Result<StoreKeys, StorageError> {
        self.generation.check_active()?;
        let metas = self
            .shared
            .store
            .list(&self.object_path(prefix.as_str()))
            .map_err(io_err)?;
        self.generation.check_active()?;
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
        self.generation.check_active()?;
        let (objects, prefixes) = self
            .shared
            .store
            .list_dir(&self.object_path(prefix.as_str()))
            .map_err(io_err)?;
        self.generation.check_active()?;
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
        self.generation.check_active()?;
        let metas = self
            .shared
            .store
            .list(&self.object_path(prefix.as_str()))
            .map_err(io_err)?;
        self.generation.check_active()?;
        Ok(metas.iter().map(|m| m.size).sum())
    }
}

/// A backend-agnostic readable + listable store. The catalog and read paths
/// work over this single concrete type, so `Catalog`/`ZarrEngine` stay
/// non-generic, while the actual backend — plain [`DsStore`], or the Icechunk
/// deadline-aware adapter under the `icechunk` feature — lives behind two upcast
/// trait-object handles to the *same* store.
///
/// We hold separate `Readable` and `Listable` handles (not one
/// `dyn ReadableListableStorageTraits`) because zarrs's `child_arrays` requires
/// the storage type to satisfy `ReadableStorageTraits` **and**
/// `ListableStorageTraits` as separate bounds, which a `dyn`-of-the-combined-
/// supertrait does not provide.
pub struct EngineStore {
    readable: Arc<dyn ReadableStorageTraits>,
    listable: Arc<dyn ListableStorageTraits>,
    pub(crate) revision: Option<String>,
    pub(crate) generation: Option<Arc<Generation>>,
}

impl EngineStore {
    /// Wrap any concrete readable+listable backend store.
    pub fn new<S>(store: S) -> Self
    where
        S: ReadableStorageTraits + ListableStorageTraits + 'static,
    {
        let arc = Arc::new(store);
        Self {
            readable: arc.clone(),
            listable: arc,
            revision: None,
            generation: None,
        }
    }

    pub(crate) fn plain(store: DsStore) -> Self {
        let generation = store.generation.clone();
        Self {
            generation: Some(generation),
            ..Self::new(store)
        }
    }

    #[cfg(feature = "icechunk")]
    pub(crate) fn with_revision(mut self, revision: String) -> Self {
        self.revision = Some(revision);
        self
    }
}

impl ReadableStorageTraits for EngineStore {
    fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        self.readable.get(key)
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        self.readable.get_partial_many(key, byte_ranges)
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        self.readable.size_key(key)
    }

    fn supports_get_partial(&self) -> bool {
        self.readable.supports_get_partial()
    }
}

impl ListableStorageTraits for EngineStore {
    fn list(&self) -> Result<StoreKeys, StorageError> {
        self.listable.list()
    }

    fn list_prefix(&self, prefix: &StorePrefix) -> Result<StoreKeys, StorageError> {
        self.listable.list_prefix(prefix)
    }

    fn list_dir(&self, prefix: &StorePrefix) -> Result<StoreKeysPrefixes, StorageError> {
        self.listable.list_dir(prefix)
    }

    fn size_prefix(&self, prefix: &StorePrefix) -> Result<u64, StorageError> {
        self.listable.size_prefix(prefix)
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
        assert_eq!(s.strip_root("zarr/era5.zarr-sibling/t2m/zarr.json"), None);
    }

    #[test]
    fn empty_root_passes_keys_through() {
        let s = store_with_root("");
        assert_eq!(s.object_path("t2m/zarr.json").as_ref(), "t2m/zarr.json");
        assert_eq!(s.strip_root("t2m/zarr.json"), Some("t2m/zarr.json"));
    }
}

#[cfg(test)]
mod refresh_tests;
