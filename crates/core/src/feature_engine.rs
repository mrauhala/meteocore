use crate::error::DataServerError;
use crate::feature::{Feature, FeaturePage, FeatureQuery};

pub trait FeatureEngine: Send + Sync {
    /// Get a page of features matching the query.
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError>;

    /// Get a single feature by ID.
    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError>;
}
