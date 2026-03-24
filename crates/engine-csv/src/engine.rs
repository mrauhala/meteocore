use chrono::{DateTime, Utc};
use std::collections::HashMap;

use ds_core::engine::Engine;
use ds_core::error::DataServerError;
use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;
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
        let domain = DomainDescription::PointSeries {
            x: first_row.longitude,
            y: first_row.latitude,
            t: times.clone(),
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

    fn get_parameter_descriptions(&self) -> HashMap<String, ParameterDescription> {
        self.store
            .parameter_names
            .iter()
            .map(|name| {
                let unit = self
                    .store
                    .parameter_units
                    .get(name)
                    .cloned()
                    .unwrap_or_default();
                (
                    name.clone(),
                    ParameterDescription {
                        label: name.replace('_', " "),
                        unit,
                        observed_property: name.clone(),
                    },
                )
            })
            .collect()
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

impl FeatureEngine for CsvEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        // Build unique locations as features
        let mut seen = HashMap::new();
        let mut all_features = Vec::new();

        for row in &self.store.rows {
            if seen.contains_key(&row.location) {
                continue;
            }
            seen.insert(&row.location, true);

            // Apply bbox filter
            if let Some(bbox) = &query.bbox {
                if !bbox.contains(row.longitude, row.latitude) {
                    continue;
                }
            }

            let mut properties = HashMap::new();
            properties.insert(
                "name".to_string(),
                PropertyValue::String(row.location.clone()),
            );
            properties.insert(
                "latitude".to_string(),
                PropertyValue::Float(row.latitude),
            );
            properties.insert(
                "longitude".to_string(),
                PropertyValue::Float(row.longitude),
            );

            all_features.push(Feature {
                id: row.location.clone(),
                geometry: Geometry::Point {
                    x: row.longitude,
                    y: row.latitude,
                },
                properties,
            });
        }

        let number_matched = all_features.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);
        let page = all_features[offset..end].to_vec();
        let number_returned = page.len();
        let next_offset = if end < number_matched {
            Some(end)
        } else {
            None
        };

        Ok(FeaturePage {
            features: page,
            number_matched,
            number_returned,
            next_offset,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        // Find the first row for this location
        let indices = self
            .store
            .location_index
            .get(feature_id)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))?;

        let row = &self.store.rows[indices[0]];

        let mut properties = HashMap::new();
        properties.insert(
            "name".to_string(),
            PropertyValue::String(row.location.clone()),
        );
        properties.insert(
            "latitude".to_string(),
            PropertyValue::Float(row.latitude),
        );
        properties.insert(
            "longitude".to_string(),
            PropertyValue::Float(row.longitude),
        );

        Ok(Feature {
            id: row.location.clone(),
            geometry: Geometry::Point {
                x: row.longitude,
                y: row.latitude,
            },
            properties,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::feature::Bbox;

    fn test_store() -> CsvDataStore {
        CsvDataStore::load("../../testdata/weather.csv").unwrap()
    }

    #[test]
    fn get_features_returns_all_locations() {
        let engine = CsvEngine::new(test_store());
        let all = engine
            .get_features(&FeatureQuery {
                limit: 10000,
                ..Default::default()
            })
            .unwrap();
        assert!(all.number_matched > 0);
        assert_eq!(all.number_matched, all.number_returned);
        assert!(all.next_offset.is_none());
        // Each feature should have a point geometry and properties
        for f in &all.features {
            assert!(matches!(f.geometry, Geometry::Point { .. }));
            assert!(f.properties.contains_key("name"));
        }
    }

    #[test]
    fn get_features_pagination() {
        let engine = CsvEngine::new(test_store());
        let all = engine.get_features(&FeatureQuery::default()).unwrap();
        let total = all.number_matched;
        assert!(total >= 3, "need at least 3 locations for pagination test");

        let query = FeatureQuery {
            limit: 2,
            offset: 0,
            ..Default::default()
        };
        let page1 = engine.get_features(&query).unwrap();
        assert_eq!(page1.number_matched, total);
        assert_eq!(page1.number_returned, 2);
        assert_eq!(page1.next_offset, Some(2));

        // Last page
        let query = FeatureQuery {
            limit: total,
            offset: total - 1,
            ..Default::default()
        };
        let last = engine.get_features(&query).unwrap();
        assert_eq!(last.number_returned, 1);
        assert!(last.next_offset.is_none());
    }

    #[test]
    fn get_features_bbox_filter() {
        let engine = CsvEngine::new(test_store());
        // Bbox covering Helsinki area (lon ~24.9-25.0, lat ~60.1-60.2)
        let bbox = Bbox::new(24.8, 60.1, 25.1, 60.25).unwrap();
        let query = FeatureQuery {
            bbox: Some(bbox),
            ..Default::default()
        };
        let result = engine.get_features(&query).unwrap();
        assert!(result.number_matched > 0, "expected Helsinki-area stations");
        // All returned features should be within the bbox
        for f in &result.features {
            match f.geometry {
                Geometry::Point { x, y } => {
                    assert!(bbox.contains(x, y), "feature {} outside bbox", f.id);
                }
                _ => panic!("Expected Point geometry"),
            }
        }
    }

    #[test]
    fn get_features_bbox_no_match() {
        let engine = CsvEngine::new(test_store());
        // Bbox far from Finland
        let bbox = Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap();
        let query = FeatureQuery {
            bbox: Some(bbox),
            ..Default::default()
        };
        let result = engine.get_features(&query).unwrap();
        assert_eq!(result.number_matched, 0);
        assert!(result.features.is_empty());
    }

    #[test]
    fn get_feature_by_id() {
        let engine = CsvEngine::new(test_store());
        // Get any feature from the listing and fetch it by ID
        let all = engine.get_features(&FeatureQuery { limit: 1, ..Default::default() }).unwrap();
        let first_id = &all.features[0].id;

        let feature = engine.get_feature(first_id).unwrap();
        assert_eq!(&feature.id, first_id);
        assert!(feature.properties.contains_key("name"));
        assert!(matches!(feature.geometry, Geometry::Point { .. }));
    }

    #[test]
    fn get_feature_not_found() {
        let engine = CsvEngine::new(test_store());
        let result = engine.get_feature("NonExistent");
        assert!(result.is_err());
    }
}
