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
