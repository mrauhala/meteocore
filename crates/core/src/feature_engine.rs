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
}
