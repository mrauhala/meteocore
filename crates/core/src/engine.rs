use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::model::{Location, QueryResult};

pub trait Engine: Send + Sync {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError>;

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError>;

    fn get_parameters(&self) -> Vec<String>;

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)>;

    fn get_spatial_extent(&self) -> Option<[f64; 4]>;
}
