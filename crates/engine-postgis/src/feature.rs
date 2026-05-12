//! `FeatureEngine` impl for `PostgisEngine`. Feature = station.
//!
//! Feature metadata (id, geometry, properties) lives in the
//! [`MetadataCache`]; feature lookups therefore never touch the pool.
//! Properties are already coerced from `pg_type` at refresh time —
//! arrays, `json`/`jsonb`, enums, timestamps, and any other types that
//! don't map cleanly to [`PropertyValue`] are rejected at startup so
//! operators see the mismatch early.
//!
//! Paging semantics mirror `CsvEngine::get_features`: `bbox` + `limit` +
//! `offset`; `number_matched` counts the full filtered set (pre-paging)
//! so consumers can compute total pages.
//!
//! This file doesn't import `FeatureQuery.datetime` — `feature =
//! station` is inherently time-agnostic in v1 (temporal features are an
//! explicit non-goal per the plan doc). The field is silently ignored.

use std::sync::Arc;

use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, Feature, FeaturePage, FeatureQuery, Geometry};
use ds_core::feature_engine::FeatureEngine;

use crate::engine::PostgisEngine;
use crate::metadata::FeatureStation;

impl FeatureEngine for PostgisEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let meta = self.cache().load();
        let filtered: Vec<&FeatureStation> = meta
            .feature_stations
            .iter()
            .filter(|s| bbox_contains(query.bbox.as_ref(), s.lon, s.lat))
            .collect();

        let number_matched = filtered.len();
        let offset = query.offset.min(number_matched);
        let limit = if query.limit == 0 {
            number_matched.saturating_sub(offset)
        } else {
            query.limit
        };
        let slice = &filtered[offset..(offset + limit).min(number_matched)];

        let features: Vec<Feature> = slice.iter().map(|s| station_to_feature(s)).collect();

        let number_returned = features.len();
        let next_offset = if offset + number_returned < number_matched {
            Some(offset + number_returned)
        } else {
            None
        };

        Ok(FeaturePage {
            features,
            number_matched,
            number_returned,
            next_offset,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        let meta = self.cache().load();
        let i = *meta
            .station_idx
            .get(feature_id)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))?;
        Ok(station_to_feature(&meta.feature_stations[i]))
    }

    fn feature_count(&self) -> usize {
        self.cache().load().feature_stations.len()
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.cache().load().spatial_extent
    }

    fn data_version(&self) -> u64 {
        // The metadata cache bumps `version` on every successful refresh, so
        // station-set changes (and PR #110's planned background refresh) will
        // automatically invalidate vector-tile ETags.
        self.cache().load().version
    }
}

fn station_to_feature(s: &FeatureStation) -> Feature {
    Feature {
        id: s.id.clone(),
        geometry: Arc::new(Geometry::Point { x: s.lon, y: s.lat }),
        properties: s.properties.clone(),
    }
}

fn bbox_contains(bbox: Option<&Bbox>, lon: f64, lat: f64) -> bool {
    match bbox {
        None => true,
        Some(b) => b.contains(lon, lat),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::feature::PropertyValue;
    use std::collections::HashMap;

    fn mk_station(id: &str, lon: f64, lat: f64, territory: &str) -> FeatureStation {
        let mut props = HashMap::new();
        props.insert(
            "territory".to_string(),
            PropertyValue::String(territory.into()),
        );
        FeatureStation {
            id: id.into(),
            label: format!("Station {id}"),
            lat,
            lon,
            properties: Arc::new(props),
        }
    }

    #[test]
    fn station_to_feature_wraps_geometry() {
        let s = mk_station("s1", 24.9, 60.2, "Finland");
        let f = station_to_feature(&s);
        assert_eq!(f.id, "s1");
        match f.geometry.as_ref() {
            Geometry::Point { x, y } => {
                assert_eq!(*x, 24.9);
                assert_eq!(*y, 60.2);
            }
            _ => panic!("expected Point"),
        }
        assert_eq!(
            f.properties.get("territory"),
            Some(&PropertyValue::String("Finland".into()))
        );
    }

    #[test]
    fn bbox_contains_filters_correctly() {
        let bbox = Bbox::new(0.0, 0.0, 10.0, 10.0).unwrap();
        assert!(bbox_contains(Some(&bbox), 5.0, 5.0));
        assert!(!bbox_contains(Some(&bbox), 15.0, 5.0));
        // None bbox accepts everything.
        assert!(bbox_contains(None, -90.0, 90.0));
    }
}
