use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::model::{Location, ParameterDescription, QueryResult};

pub trait Engine: Send + Sync {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError>;

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError>;

    fn get_parameters(&self) -> Vec<String>;

    /// Returns parameter descriptions including units for collection-level metadata.
    /// Default implementation builds descriptions from `get_parameters()` with empty units.
    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.get_parameters()
            .into_iter()
            .map(|name| {
                let desc = ParameterDescription {
                    label: name.replace('_', " "),
                    unit: String::new(),
                    observed_property: name.clone(),
                };
                (name, desc)
            })
            .collect()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)>;

    fn get_spatial_extent(&self) -> Option<[f64; 4]>;

    /// Returns the EDR query types this engine supports.
    /// Default: `["locations"]`. Override for engines that support position, area, etc.
    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".to_string()]
    }

    /// Execute a position query at the given coordinates.
    /// Default implementation returns an error.
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        let _ = (coords, datetime, parameters);
        Err(DataServerError::InvalidParameter(
            "Position query not supported by this engine".into(),
        ))
    }
}
