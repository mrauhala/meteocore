use chrono::{DateTime, Utc};
use std::collections::HashMap;

use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::model::*;

use crate::loader::CsvDataStore;

pub struct CsvEngine {
    store: CsvDataStore,
}

impl CsvEngine {
    pub fn new(store: CsvDataStore) -> Self {
        Self { store }
    }
}

impl Engine for CsvEngine {
    fn get_locations(&self) -> Result<Vec<Location>, DataServerError> {
        let mut locations = Vec::new();
        let mut seen = HashMap::new();

        for row in &self.store.rows {
            if seen.contains_key(&row.location) {
                continue;
            }
            seen.insert(&row.location, true);
            locations.push(Location {
                id: row.location.clone(),
                label: row.location.clone(),
                latitude: row.latitude,
                longitude: row.longitude,
            });
        }

        Ok(locations)
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
    ) -> Result<QueryResult, DataServerError> {
        let time_map = self
            .store
            .time_index
            .get(location_id)
            .ok_or_else(|| DataServerError::LocationNotFound(location_id.to_string()))?;

        // Collect matching row indices by time range
        let row_indices: Vec<usize> = match datetime {
            Some((start, end)) => time_map
                .range(start..=end)
                .flat_map(|(_, indices)| indices.iter().copied())
                .collect(),
            None => self
                .store
                .location_index
                .get(location_id)
                .cloned()
                .unwrap_or_default(),
        };

        if row_indices.is_empty() {
            return Err(DataServerError::LocationNotFound(format!(
                "{location_id} (no data in time range)"
            )));
        }

        // Determine which parameters to include
        let param_names: Vec<String> = match parameters {
            Some(requested) => requested
                .iter()
                .filter(|p| self.store.parameter_names.contains(p))
                .cloned()
                .collect(),
            None => self.store.parameter_names.clone(),
        };

        // Build time axis (sorted)
        let first_row = &self.store.rows[row_indices[0]];
        let mut times: Vec<DateTime<Utc>> = row_indices
            .iter()
            .map(|&i| self.store.rows[i].time)
            .collect();
        times.sort();
        times.dedup();

        // Build domain
        let domain = DomainDescription {
            domain_type: "PointSeries".to_string(),
            axes_x: first_row.longitude,
            axes_y: first_row.latitude,
            axes_t: times.clone(),
        };

        // Build parameter descriptions
        let mut param_descs = HashMap::new();
        for name in &param_names {
            let unit = self
                .store
                .parameter_units
                .get(name)
                .cloned()
                .unwrap_or_default();
            param_descs.insert(
                name.clone(),
                ParameterDescription {
                    label: name.replace('_', " "),
                    unit: unit.clone(),
                    observed_property: name.clone(),
                },
            );
        }

        // Build ranges — values ordered by time
        let mut ranges = HashMap::new();
        for name in &param_names {
            let mut values: Vec<Option<f64>> = Vec::with_capacity(times.len());
            for t in &times {
                // Find the row for this time
                let val = row_indices
                    .iter()
                    .find(|&&i| self.store.rows[i].time == *t)
                    .and_then(|&i| self.store.rows[i].values.get(name).copied())
                    .unwrap_or(None);
                values.push(val);
            }
            ranges.insert(
                name.clone(),
                NdArray {
                    shape: vec![times.len()],
                    axis_names: vec!["t".to_string()],
                    values,
                },
            );
        }

        Ok(QueryResult {
            domain,
            parameters: param_descs,
            ranges,
        })
    }

    fn get_parameters(&self) -> Vec<String> {
        self.store.parameter_names.clone()
    }

    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let mut min = DateTime::<Utc>::MAX_UTC;
        let mut max = DateTime::<Utc>::MIN_UTC;

        for row in &self.store.rows {
            if row.time < min {
                min = row.time;
            }
            if row.time > max {
                max = row.time;
            }
        }

        if min <= max {
            Some((min, max))
        } else {
            None
        }
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        if self.store.rows.is_empty() {
            return None;
        }

        let mut min_lon = f64::MAX;
        let mut min_lat = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut max_lat = f64::MIN;

        for row in &self.store.rows {
            min_lon = min_lon.min(row.longitude);
            min_lat = min_lat.min(row.latitude);
            max_lon = max_lon.max(row.longitude);
            max_lat = max_lat.max(row.latitude);
        }

        Some([min_lon, min_lat, max_lon, max_lat])
    }
}
