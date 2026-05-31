use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::model::{CoverageResponse, Location, ParameterDescription};
use crate::vertical::VerticalDimension;

pub trait EdrEngine: Send + Sync {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError>;

    /// Execute a query for a named location.
    ///
    /// `z` selects vertical levels: `None` returns every level (a profile),
    /// `Some([v])` pins one level, `Some([v1, v2, …])` selects several.
    /// Engines with no vertical dimension ignore it (the API layer rejects
    /// a `z` against a collection that has no vertical extent).
    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError>;

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

    /// Returns the individual timesteps available for querying.
    /// Used in EDR collection metadata to advertise available times to clients.
    /// Default: None (only interval is shown). Override for engines with
    /// non-uniform time steps (e.g., GRIB forecasts with 3h/6h steps).
    fn get_available_times(&self) -> Option<Vec<DateTime<Utc>>> {
        None
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]>;

    /// Returns the collection's vertical axis, when it has one.
    /// Default: None (the collection has no vertical dimension).
    fn get_vertical_extent(&self) -> Option<VerticalDimension> {
        None
    }

    /// Returns the EDR query types this engine supports.
    /// Default: `["locations"]`. Override for engines that support position, area, etc.
    fn supported_query_types(&self) -> Vec<String> {
        vec!["locations".to_string()]
    }

    /// Execute an area query within the given bounding box / polygon.
    /// Default implementation returns an error.
    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let _ = (coords, datetime, parameters, z);
        Err(DataServerError::InvalidParameter(
            "Area query not supported by this engine".into(),
        ))
    }

    /// Execute a position query at the given coordinates.
    /// Default implementation returns an error.
    fn query_position(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let _ = (coords, datetime, parameters, z);
        Err(DataServerError::InvalidParameter(
            "Position query not supported by this engine".into(),
        ))
    }

    /// Execute a trajectory (vertical cross-section) query along a WKT
    /// `LINESTRING`. The result is a CoverageJSON `Section` domain (or a
    /// collection of them, one per timestep): a 2-D field over an
    /// along-path composite axis and a vertical `z` axis. `z`, when set,
    /// pins the height range — engines free to interpret as a discrete
    /// list, a `[min, max]` interval, or both.
    /// Default implementation returns an error.
    fn query_trajectory(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
    ) -> Result<CoverageResponse, DataServerError> {
        let _ = (coords, datetime, parameters, z);
        Err(DataServerError::InvalidParameter(
            "Trajectory query not supported by this engine".into(),
        ))
    }
}
