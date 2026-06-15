use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::feature::{Feature, FeaturePage, FeatureQuery};

pub trait FeatureEngine: Send + Sync {
    /// Get a page of features matching the query.
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError>;

    /// Get a single feature by ID.
    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError>;

    /// Total number of features in the collection. Used for collection metadata.
    fn feature_count(&self) -> usize {
        self.get_features(&FeatureQuery {
            limit: 0,
            ..Default::default()
        })
        .map(|p| p.number_matched)
        .unwrap_or(0)
    }

    /// Spatial extent as [west, south, east, north], if available.
    fn spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }

    /// Temporal extent `(start, end)` of the collection, if the features carry a
    /// time dimension (e.g. CAP alert validity windows). Surfaced as the
    /// `extent.temporal.interval` in the OGC API – Features collection metadata.
    /// `None` (the default) means the collection has no temporal extent — per
    /// OGC API – Common – Part 2, the element is then simply omitted.
    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        None
    }

    /// Opaque token that changes when the underlying feature data changes.
    ///
    /// Used as a data-version component in vector-tile ETags so that an
    /// `/admin/collections/reload` (or any in-process refresh) invalidates
    /// previously-issued tile ETags instead of serving `304 Not Modified`
    /// indefinitely. Engines that load once and never change can leave the
    /// default `0`; consumers should treat the value as opaque.
    fn data_version(&self) -> u64 {
        0
    }
}
