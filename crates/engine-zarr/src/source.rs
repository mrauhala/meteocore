//! A source opens read sessions; versioned catalogs retain a pinned snapshot.
use std::sync::{Arc, Mutex};

use ds_core::{config::ZarrConfig, error::DataServerError};

use crate::store::{DsStore, EngineStore, Generation};

pub(crate) struct PlainSource {
    store: DsStore,
    published: Mutex<Option<Arc<Generation>>>,
}

pub(crate) enum Source {
    Plain(PlainSource),
    #[cfg(feature = "icechunk")]
    Icechunk(Box<crate::icechunk::Source>),
}

impl Source {
    pub(crate) fn open(id: &str, config: &ZarrConfig) -> Result<Self, DataServerError> {
        if config.icechunk.is_some() {
            #[cfg(feature = "icechunk")]
            return crate::icechunk::Source::open(id, config)
                .map(|source| Self::Icechunk(Box::new(source)));
            #[cfg(not(feature = "icechunk"))]
            return Err(DataServerError::Config(format!(
                "Collection '{id}': [zarr.icechunk] is configured but this server was \
                 built without the 'icechunk' feature"
            )));
        }
        crate::build_store(id, config).map(|store| {
            Self::Plain(PlainSource {
                store,
                published: Mutex::new(None),
            })
        })
    }

    /// The plain store's object cache (`[zarr] cache_mb`); `None` for
    /// Icechunk, whose payload cache is the library's own.
    pub(crate) fn object_cache_metrics(&self) -> Option<ds_cache::CacheMetrics> {
        match self {
            Self::Plain(source) => Some(source.store.cache_metrics()),
            #[cfg(feature = "icechunk")]
            Self::Icechunk(_) => None,
        }
    }

    /// An Icechunk repository's payload cache (`[zarr] cache_mb`, #1353);
    /// `None` for a plain store.
    pub(crate) fn payload_cache_metrics(&self) -> Option<ds_cache::CacheMetrics> {
        match self {
            Self::Plain(_) => None,
            #[cfg(feature = "icechunk")]
            Self::Icechunk(source) => Some(source.meter().payload_metrics()),
        }
    }

    /// An Icechunk repository's storage requests and the bytes their
    /// bodies yielded, cumulative; `None` for a plain store.
    pub(crate) fn storage_counts(&self) -> Option<(u64, u64)> {
        match self {
            Self::Plain(_) => None,
            #[cfg(feature = "icechunk")]
            Self::Icechunk(source) => Some(source.meter().counts()),
        }
    }

    /// An Icechunk repository's inner-chunk ranges fetched ahead and used,
    /// unused, left serial and failed (#1353); `None` for a plain store.
    pub(crate) fn prefetched(&self) -> Option<[u64; 4]> {
        match self {
            Self::Plain(_) => None,
            #[cfg(feature = "icechunk")]
            Self::Icechunk(source) => Some(source.meter().prefetched()),
        }
    }

    /// An Icechunk repository's bytes fetched ahead and not yet taken by a
    /// decode (#1353); `None` for a plain store.
    pub(crate) fn staged_bytes(&self) -> Option<u64> {
        match self {
            Self::Plain(_) => None,
            #[cfg(feature = "icechunk")]
            Self::Icechunk(source) => Some(source.meter().staged_bytes()),
        }
    }

    /// `None` means the source is still at the published revision. A changed
    /// session is returned separately, so a failed catalog build never mutates
    /// the session used by requests or marks the new revision as published.
    pub(crate) fn snapshot(
        &self,
        published: Option<&str>,
    ) -> Result<Option<Arc<EngineStore>>, DataServerError> {
        match self {
            Self::Plain(source) => {
                let _ = published;
                Ok(Some(Arc::new(EngineStore::plain(source.store.fresh()))))
            }
            #[cfg(feature = "icechunk")]
            Self::Icechunk(source) => source.snapshot(published),
        }
    }

    /// Register the initial plain catalog, or retire its predecessor after a
    /// successful rebuild has been swapped into the engine. Existing cached
    /// bytes and sampled windows remain valid; uncached old reads fail rather
    /// than fetching a newer object's bytes.
    pub(crate) fn publish(&self, store: &EngineStore) {
        match self {
            Self::Plain(source) => {
                if let Some(generation) = &store.generation {
                    let previous = source.published.lock().unwrap().replace(generation.clone());
                    if let Some(previous) = previous {
                        if !Arc::ptr_eq(&previous, generation) {
                            previous.retire();
                        }
                    }
                }
            }
            #[cfg(feature = "icechunk")]
            Self::Icechunk(_) => {}
        }
    }
}
