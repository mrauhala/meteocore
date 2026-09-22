//! A source opens read sessions; versioned catalogs retain a pinned snapshot.
use std::sync::Arc;

use ds_core::{config::ZarrConfig, error::DataServerError};

use crate::store::EngineStore;

pub(crate) enum Source {
    Plain(Arc<EngineStore>),
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
        crate::build_store(id, config).map(|store| Self::Plain(Arc::new(store)))
    }

    /// `None` means the source is still at the published revision. A changed
    /// session is returned separately, so a failed catalog build never mutates
    /// the session used by requests or marks the new revision as published.
    pub(crate) fn snapshot(
        &self,
        published: Option<&str>,
    ) -> Result<Option<Arc<EngineStore>>, DataServerError> {
        match self {
            Self::Plain(store) => {
                let _ = published;
                Ok(Some(store.clone()))
            }
            #[cfg(feature = "icechunk")]
            Self::Icechunk(source) => source.snapshot(published),
        }
    }
}
