use chrono::{DateTime, Utc};
use std::collections::HashMap;

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use std::sync::Arc;

use ds_core::feature::{
    parse_area_coords, Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue,
};
use ds_core::feature_engine::FeatureEngine;
use ds_core::model::*;

use crate::loader::CsvDataStore;

pub struct CsvEngine {
    store: CsvDataStore,
    /// Immutable station inventory in first-observation order. Observation
    /// history must not be scanned or turned into Features on each page request.
    stations: Vec<Feature>,
    /// First and last observation time, computed once: the Features
    /// temporal extent is read per request (Critical Rule 10).
    time_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl CsvEngine {
    pub fn new(store: CsvDataStore) -> Self {
        let mut first_rows: Vec<_> = store
            .location_index
            .values()
            .filter_map(|indices| indices.first().copied())
            .collect();
        first_rows.sort_unstable();
        let stations = first_rows
            .into_iter()
            .map(|index| station_feature(&store.rows[index]))
            .collect();
        let time_extent = store
            .rows
            .iter()
            .map(|row| row.time)
            .min()
            .zip(store.rows.iter().map(|row| row.time).max());
        Self {
            store,
            stations,
            time_extent,
        }
    }

    /// Whether `location_id` has an observation row inside `interval`
    /// (open bounds are unbounded): the one `datetime` rule of Features
    /// `/items` and EDR `/locations` (#682, #932).
    fn has_rows_in(
        &self,
        location_id: &str,
        interval: &ds_core::feature::DatetimeInterval,
    ) -> bool {
        let Some(times) = self.store.time_index.get(location_id) else {
            return false;
        };
        // `BTreeMap::range` panics on an inverted range; the API layer
        // rejects one, but an empty interval matches nothing either way.
        if let (Some(start), Some(end)) = (interval.start, interval.end) {
            if start > end {
                return false;
            }
        }
        let start = interval
            .start
            .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included);
        let end = interval
            .end
            .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included);
        times.range((start, end)).next().is_some()
    }

    /// Build a `PointSeries` coverage for one location. Shared by
    /// `query_location` and `query_area`.
    fn location_series(
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
            z: None,
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
                    standard_name: None,
                },
            );
        }

        // Build time→row_index map for O(1) lookups
        let time_to_row: HashMap<DateTime<Utc>, usize> = row_indices
            .iter()
            .map(|&i| (self.store.rows[i].time, i))
            .collect();

        // Build ranges — values ordered by time
        let mut ranges = HashMap::new();
        for name in &param_names {
            let mut values: Vec<Option<f64>> = Vec::with_capacity(times.len());
            for t in &times {
                let val = time_to_row
                    .get(t)
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
}

impl EdrEngine for CsvEngine {
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

    /// `/locations?datetime=` (#932): stations with an observation row in
    /// one of the intervals, the Features `datetime` rule (#682).
    fn location_time_filter<'a>(
        &'a self,
        intervals: &'a [ds_core::feature::DatetimeInterval],
    ) -> Option<ds_core::edr_engine::LocationFilter<'a>> {
        Some(Box::new(move |location| {
            intervals
                .iter()
                .any(|interval| self.has_rows_in(&location.id, interval))
        }))
    }

    fn query_location(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Ok(CoverageResponse::Single(self.location_series(
            location_id,
            datetime,
            parameters,
        )?))
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
                        standard_name: None,
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

    fn supported_query_types(&self) -> Vec<String> {
        vec![
            "locations".to_string(),
            "area".to_string(),
            "radius".to_string(),
        ]
    }

    /// Every result is one `PointSeries` per CSV location, at the
    /// coordinates `get_locations` lists: EDR GeoJSON can name each (#929).
    fn serves_station_series(&self) -> bool {
        true
    }

    fn query_area(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        _z: Option<&[f64]>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        const MAX_LOCATIONS: usize = 500;

        let polygon = parse_area_coords(coords)?;

        // Find unique locations within the polygon
        let mut seen = HashMap::new();
        let mut matching_locations = Vec::new();

        for row in &self.store.rows {
            if seen.contains_key(&row.location) {
                continue;
            }
            seen.insert(&row.location, true);
            if polygon.contains(row.longitude, row.latitude) {
                matching_locations.push(row.location.clone());
            }
        }

        if matching_locations.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No locations found within the requested area".into(),
            ));
        }

        if matching_locations.len() > MAX_LOCATIONS {
            return Err(DataServerError::InvalidParameter(format!(
                "Area query matched {} locations, maximum is {}. Use a smaller area.",
                matching_locations.len(),
                MAX_LOCATIONS
            )));
        }

        // Build a PointSeries QueryResult for each matching location
        let mut coverages = Vec::with_capacity(matching_locations.len());
        for loc_id in &matching_locations {
            coverages.push(self.location_series(loc_id, datetime, parameters)?);
        }

        Ok(CoverageResponse::Collection(coverages))
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
    /// The observation times the stations' rows span; `datetime` filters
    /// stations to those with a row inside it (#682).
    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.time_extent
    }

    fn filterables(&self) -> ds_core::feature::FilterableProperties {
        static NAMES: std::sync::LazyLock<ds_core::feature::FilterableProperties> =
            std::sync::LazyLock::new(|| {
                Arc::new(
                    ["name", "latitude", "longitude"]
                        .into_iter()
                        .map(String::from)
                        .collect(),
                )
            });
        NAMES.clone()
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let (page, number_matched) = if query.bbox.is_none()
            && query.property_filters.is_empty()
            && query.datetime.is_none()
        {
            let total = self.stations.len();
            let start = query.offset.min(total);
            let end = start.saturating_add(query.limit).min(total);
            (self.stations[start..end].to_vec(), total)
        } else {
            let mut page = Vec::new();
            let mut matched = 0;
            for station in &self.stations {
                if let Some(bbox) = &query.bbox {
                    let Geometry::Point { x, y } = *station.geometry else {
                        unreachable!("CSV station geometry is always Point");
                    };
                    if !bbox.contains(x, y) {
                        continue;
                    }
                }
                if !ds_core::feature::matches_property_filters(station, &query.property_filters) {
                    continue;
                }
                // `datetime`: stations with at least one observation row in
                // the interval (#682), as the ODIM PVOL network does with
                // volumes.
                if let Some(interval) = &query.datetime {
                    if !self.has_rows_in(&station.id, interval) {
                        continue;
                    }
                }
                if matched >= query.offset && page.len() < query.limit {
                    page.push(station.clone());
                }
                // Continue counting after filling the page, without cloning
                // more features or scanning any station's observation rows.
                matched += 1;
            }
            (page, matched)
        };
        let number_returned = page.len();
        let end = query.offset.min(number_matched) + number_returned;
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

        Ok(station_feature(&self.store.rows[indices[0]]))
    }
}

fn station_feature(row: &crate::loader::CsvRow) -> Feature {
    Feature {
        id: row.location.clone(),
        geometry: Arc::new(Geometry::Point {
            x: row.longitude,
            y: row.latitude,
        }),
        properties: Arc::new(HashMap::from([
            ("name".into(), PropertyValue::String(row.location.clone())),
            ("latitude".into(), PropertyValue::Float(row.latitude)),
            ("longitude".into(), PropertyValue::Float(row.longitude)),
        ])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::feature::Bbox;

    fn test_store() -> CsvDataStore {
        CsvDataStore::load("../../testdata/weather.csv").unwrap()
    }

    /// `datetime` keeps the stations with an observation row inside the
    /// interval, open bounds included (#682), and the Features temporal
    /// extent spans the rows.
    #[test]
    fn datetime_filters_stations_by_their_observations() {
        use ds_core::feature::DatetimeInterval;
        use ds_core::feature_engine::FeatureEngine as _;
        let engine = CsvEngine::new(
            CsvDataStore::load(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/station_history.csv"
            ))
            .unwrap(),
        );
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let ids = |start: Option<&str>, end: Option<&str>| {
            let page = engine
                .get_features(&FeatureQuery {
                    datetime: Some(DatetimeInterval {
                        start: start.map(at),
                        end: end.map(at),
                    }),
                    limit: 10,
                    ..Default::default()
                })
                .unwrap();
            let ids: Vec<String> = page.features.iter().map(|f| f.id.clone()).collect();
            assert_eq!(page.number_matched, ids.len());
            ids
        };
        // Rows: zeta and alpha at 00:00 and 01:00, beta at 00:00 only.
        assert_eq!(
            ids(Some("2026-01-01T00:30:00Z"), Some("2026-01-01T01:30:00Z")),
            ["zeta", "alpha"]
        );
        assert_eq!(ids(Some("2026-01-01T00:30:00Z"), None), ["zeta", "alpha"]);
        assert_eq!(
            ids(None, Some("2026-01-01T00:00:00Z")),
            ["zeta", "alpha", "beta"]
        );
        assert!(ids(Some("2026-01-02T00:00:00Z"), None).is_empty());
        // An inverted interval matches nothing instead of panicking in
        // `BTreeMap::range`.
        assert!(ids(Some("2026-01-01T01:00:00Z"), Some("2026-01-01T00:00:00Z")).is_empty());
        assert_eq!(
            engine.temporal_extent(),
            Some((at("2026-01-01T00:00:00Z"), at("2026-01-01T01:00:00Z")))
        );
    }

    #[test]
    fn station_pages_preserve_first_observation_order_and_filter_counts() {
        let engine = CsvEngine::new(
            CsvDataStore::load(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/station_history.csv"
            ))
            .unwrap(),
        );
        let all = engine.get_features(&FeatureQuery::default()).unwrap();
        assert_eq!(
            all.features
                .iter()
                .map(|f| f.id.as_str())
                .collect::<Vec<_>>(),
            ["zeta", "alpha", "beta"]
        );
        let mut query = FeatureQuery {
            bbox: Some(Bbox::new(23.0, 59.0, 27.0, 62.0).unwrap()),
            property_filters: vec![("latitude".into(), "60,62".into())],
            limit: 1,
            ..Default::default()
        };
        let first = engine.get_features(&query).unwrap();
        assert_eq!(first.number_matched, 2);
        assert_eq!(first.features[0].id, "zeta");
        assert_eq!(first.next_offset, Some(1));
        query.offset = 1;
        let last = engine.get_features(&query).unwrap();
        assert_eq!(last.number_matched, 2);
        assert_eq!(last.features[0].id, "beta");
        assert_eq!(last.next_offset, None);

        // Later observations with changed coordinates do not move a station's
        // inventory geometry. Listing and by-id use the same representative.
        assert!(matches!(
            *engine.get_feature("zeta").unwrap().geometry,
            Geometry::Point { x: 24.0, y: 60.0 }
        ));
        for filtered in [false, true] {
            let mut query = if filtered {
                query.clone()
            } else {
                FeatureQuery::default()
            };
            query.offset = usize::MAX;
            query.limit = usize::MAX;
            let beyond = engine.get_features(&query).unwrap();
            assert!(beyond.features.is_empty());
            assert_eq!(beyond.number_matched, if filtered { 2 } else { 3 });
            assert_eq!(beyond.next_offset, None);
            query.offset = 0;
            query.limit = 0;
            let zero = engine.get_features(&query).unwrap();
            assert!(zero.features.is_empty());
            assert_eq!(zero.number_matched, beyond.number_matched);
        }
    }

    #[test]
    fn property_filters_select_stations_before_paging() {
        let engine = CsvEngine::new(test_store());
        let all = engine.get_features(&FeatureQuery::default()).unwrap();
        let target = all.features.last().unwrap();
        let name = target.properties["name"].as_str().unwrap();
        let mut query = FeatureQuery {
            property_filters: vec![("name".into(), name.into())],
            limit: 1,
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 1);
        assert_eq!(page.features[0].id, target.id);
        query.offset = 1;
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 1);
        assert!(page.features.is_empty());
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
            assert!(matches!(*f.geometry, Geometry::Point { .. }));
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
            match *f.geometry {
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
        let all = engine
            .get_features(&FeatureQuery {
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        let first_id = &all.features[0].id;

        let feature = engine.get_feature(first_id).unwrap();
        assert_eq!(&feature.id, first_id);
        assert!(feature.properties.contains_key("name"));
        assert!(matches!(*feature.geometry, Geometry::Point { .. }));
    }

    #[test]
    fn get_feature_not_found() {
        let engine = CsvEngine::new(test_store());
        let result = engine.get_feature("NonExistent");
        assert!(result.is_err());
    }

    /// The `serves_station_series` contract EDR GeoJSON relies on (#929):
    /// every coverage is a `PointSeries` without `z`, at the coordinates of
    /// one `get_locations` entry.
    #[test]
    fn station_series_sit_at_listed_locations() {
        let engine = CsvEngine::new(test_store());
        assert!(engine.serves_station_series());
        let locations = engine.get_locations().unwrap();
        let CoverageResponse::Collection(coverages) = engine
            .query_area(
                "POLYGON((19 59,32 59,32 71,19 71,19 59))",
                None,
                None,
                None,
                None,
            )
            .unwrap()
        else {
            panic!("an area query answers a collection");
        };
        assert_eq!(coverages.len(), locations.len());
        for coverage in &coverages {
            let DomainDescription::PointSeries { x, y, z: None, .. } = coverage.domain else {
                panic!("expected a PointSeries without z: {:?}", coverage.domain);
            };
            let at_location = locations
                .iter()
                .filter(|l| l.longitude == x && l.latitude == y)
                .count();
            assert_eq!(at_location, 1, "({x}, {y})");
        }
    }
}
