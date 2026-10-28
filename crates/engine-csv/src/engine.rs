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
use crate::map::StationMap;
use ds_core::station_map::StationMapSettings;
use ds_render::station_symbols::StationSymbols;

pub struct CsvEngine {
    pub(crate) store: CsvDataStore,
    /// Immutable station inventory in first-observation order. Observation
    /// history must not be scanned or turned into Features on each page request.
    stations: Vec<Feature>,
    /// The EDR location inventory in first-observation order, built once:
    /// `get_locations` hands every request this `Arc` (#1083), and an area
    /// query tests its positions instead of scanning the rows (#1262).
    pub(crate) locations: Arc<LocationSnapshot>,
    /// First and last observation time, computed once: the EDR and Features
    /// temporal extents are read per request (Critical Rule 10, #1249).
    time_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The bounding box of every row's coordinates, computed once for the
    /// same reason (#27).
    spatial_extent: Option<[f64; 4]>,
    /// Every distinct observation time, ascending, built once: a station
    /// grid's analysis times without a `cadence` (#1206).
    instants: Arc<[DateTime<Utc>]>,
    /// The station-symbol map layers (#1204), built once: the file is
    /// static until a reload rebuilds the engine.
    pub(crate) map: StationMap,
}

impl CsvEngine {
    /// The engine over a loaded file, its map layers on the default
    /// `[csv.map]` settings ([`Self::with_station_map`] sets others).
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
        let mut instants: Vec<DateTime<Utc>> = store.rows.iter().map(|row| row.time).collect();
        instants.sort_unstable();
        instants.dedup();
        let time_extent = instants.first().copied().zip(instants.last().copied());
        let spatial_extent = spatial_extent(&store);
        let locations = Arc::new(LocationSnapshot::new(inventory(&store)));
        let map = StationMap::new(
            &store,
            time_extent,
            spatial_extent,
            StationMapSettings::default(),
        );
        Self {
            store,
            stations,
            locations,
            time_extent,
            spatial_extent,
            instants: instants.into(),
            map,
        }
    }

    /// The station-symbol map layers on `settings` (`[csv.map]`, #1204).
    pub fn with_station_map(mut self, settings: StationMapSettings) -> Self {
        self.map = StationMap::new(&self.store, self.time_extent, self.spatial_extent, settings);
        self
    }

    /// Hand the map layers their style facts (#1204): each parameter
    /// layer's midpoint, which the overlap order measures from. The server
    /// calls this whenever it wires the collection to a map API.
    pub fn set_station_symbols(&self, symbols: StationSymbols) {
        self.map.layers.set_symbols(symbols);
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

    /// The parameters `parameter-name` selects, through the shared rule: an
    /// unknown name or an empty list is a 400 naming the valid ones, and
    /// matching ignores case (#666, #966). Resolved before any station is
    /// read, so an area without data cannot turn the 400 into a 404.
    fn selected_parameters(
        &self,
        parameters: Option<&[String]>,
    ) -> Result<Vec<String>, DataServerError> {
        let available: Vec<&str> = self
            .store
            .parameter_names
            .iter()
            .map(String::as_str)
            .collect();
        Ok(
            ds_core::edr_engine::select_parameters(parameters, &available)?
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Build a `PointSeries` coverage for one location, `None` when it has
    /// no row in `datetime`. Shared by `query_location` and `query_area`.
    fn location_series(
        &self,
        location_id: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        param_names: &[String],
    ) -> Result<Option<QueryResult>, DataServerError> {
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
            return Ok(None);
        }

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
        for name in param_names {
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
        for name in param_names {
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
                    precision: ValuePrecision::F64,
                },
            );
        }

        Ok(Some(QueryResult {
            domain,
            parameters: param_descs,
            ranges,
        }))
    }
}

impl EdrEngine for CsvEngine {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(Arc::clone(&self.locations))
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
        let param_names = self.selected_parameters(parameters)?;
        let series = self
            .location_series(location_id, datetime, &param_names)?
            .ok_or_else(|| {
                DataServerError::LocationNotFound(format!("{location_id} (no data in time range)"))
            })?;
        Ok(CoverageResponse::Single(series))
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

    /// The span the rows cover, from the load-time snapshot: read per
    /// request by collection metadata, query validation and a station
    /// grid's axis (#1249).
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.time_extent
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

    /// Every distinct row time of the file, for a station grid without a
    /// `cadence` (#1206). Not advertised: `get_available_times` stays `None`.
    fn observation_instants(&self) -> Option<Arc<[DateTime<Utc>]>> {
        Some(Arc::clone(&self.instants))
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
        let param_names = self.selected_parameters(parameters)?;

        // The stations inside the polygon, from the load-time inventory: in
        // first-observation order, each at its first row's position, as a
        // scan of the rows would find them, without reading a row (#1262).
        let matching_locations: Vec<&str> = self
            .locations
            .iter()
            .filter(|location| polygon.contains(location.longitude, location.latitude))
            .map(|location| location.id.as_str())
            .collect();

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

        // One PointSeries per matching location with data in the window; a
        // station without a row then is left out, not a 404 for all of them
        // (EDR 1.2 /req/edr/REQ_rc-within-response A), as in engine-bufr.
        let mut coverages = Vec::with_capacity(matching_locations.len());
        for loc_id in matching_locations {
            coverages.extend(self.location_series(loc_id, datetime, &param_names)?);
        }
        if coverages.is_empty() {
            return Err(DataServerError::LocationNotFound(
                "No location within the requested area has data in the requested time range".into(),
            ));
        }

        Ok(CoverageResponse::Collection(coverages))
    }

    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        self.spatial_extent
    }
}

impl FeatureEngine for CsvEngine {
    /// The observation times the stations' rows span; `datetime` filters
    /// stations to those with a row inside it (#682).
    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.time_extent
    }

    /// The bounding box of every row's coordinates, the load-time snapshot
    /// EDR and the map layers report too: read per request by collection
    /// metadata and search (#1262).
    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.spatial_extent
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

/// The bounding box of every row's coordinates; `None` for an empty file.
fn spatial_extent(store: &CsvDataStore) -> Option<[f64; 4]> {
    if store.rows.is_empty() {
        return None;
    }

    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;

    for row in &store.rows {
        min_lon = min_lon.min(row.longitude);
        min_lat = min_lat.min(row.latitude);
        max_lon = max_lon.max(row.longitude);
        max_lat = max_lat.max(row.latitude);
    }

    Some([min_lon, min_lat, max_lon, max_lat])
}

/// One location per station, at its first row's coordinates, in
/// first-observation order.
fn inventory(store: &CsvDataStore) -> Vec<Location> {
    let mut locations = Vec::new();
    let mut seen = HashMap::new();

    for row in &store.rows {
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

    locations
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
        // The station is the EDR location of its id (#1143).
        related: vec![ds_core::feature::Related::edr_location(&row.location)],
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

    /// The location inventory is built once (#1083): every call shares one
    /// snapshot, in the order `/items` lists the stations.
    #[test]
    fn locations_are_one_shared_snapshot_in_station_order() {
        let engine = CsvEngine::new(test_store());
        let locations = engine.get_locations().unwrap();
        assert!(Arc::ptr_eq(&locations, &engine.get_locations().unwrap()));
        let features = engine
            .get_features(&FeatureQuery {
                limit: 10000,
                ..Default::default()
            })
            .unwrap();
        let location_ids: Vec<&str> = locations.iter().map(|l| l.id.as_str()).collect();
        let feature_ids: Vec<&str> = features.features.iter().map(|f| f.id.as_str()).collect();
        assert!(location_ids.len() > 1);
        assert_eq!(location_ids, feature_ids);
        for location in locations.iter() {
            assert_eq!(locations.by_id(&location.id).unwrap().id, location.id);
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
        // The station is the EDR location of its id (#1143).
        assert_eq!(
            feature.related,
            [ds_core::feature::Related::edr_location(first_id)]
        );
    }

    #[test]
    fn get_feature_not_found() {
        let engine = CsvEngine::new(test_store());
        let result = engine.get_feature("NonExistent");
        assert!(result.is_err());
    }

    /// `parameter-name` is the shared rule (#966): case-insensitive, the
    /// canonical spelling back, and an unknown name or an empty list is a
    /// 400 naming the valid ones, never silently narrowed.
    #[test]
    fn parameter_name_is_known_names_only() {
        let engine = CsvEngine::new(test_store());
        let station = "Alajärvi Möksy";
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let CoverageResponse::Single(q) = engine
            .query_location(station, None, Some(&names(&["TEMPERATURE"])), None, None)
            .unwrap()
        else {
            panic!("a location query answers one coverage");
        };
        assert_eq!(q.ranges.keys().collect::<Vec<_>>(), vec!["temperature"]);
        for requested in [names(&["temperature", "nope"]), names(&[])] {
            assert!(matches!(
                engine.query_location(station, None, Some(&requested), None, None),
                Err(DataServerError::InvalidParameter(m)) if m.contains("wind_speed")
            ));
            assert!(matches!(
                engine.query_area("19,59,32,71", None, Some(&requested), None, None),
                Err(DataServerError::InvalidParameter(_))
            ));
        }
        // Resolved before any station is read: an area without data in the
        // window is a 404, but with an unknown name still the 400.
        let never = "2030-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let window = Some((never, never));
        assert!(matches!(
            engine.query_area("19,59,32,71", window, None, None, None),
            Err(DataServerError::LocationNotFound(_))
        ));
        assert!(matches!(
            engine.query_area("19,59,32,71", window, Some(&names(&["nope"])), None, None),
            Err(DataServerError::InvalidParameter(_))
        ));
    }

    fn station_history() -> CsvEngine {
        CsvEngine::new(
            CsvDataStore::load(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/station_history.csv"
            ))
            .unwrap(),
        )
    }

    /// A station grid over the file reads its distinct row times, in order
    /// (#1206); the collection's public metadata does not list them.
    #[test]
    fn observation_instants_are_the_distinct_row_times() {
        let engine = station_history();
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            engine.observation_instants().as_deref(),
            Some(&[at("2026-01-01T00:00:00Z"), at("2026-01-01T01:00:00Z")][..])
        );
        assert_eq!(engine.get_available_times(), None);
        let weather = CsvEngine::new(test_store());
        assert_eq!(weather.observation_instants().unwrap().len(), 31 * 24);
    }

    /// The temporal and spatial extents are the rows' minima and maxima,
    /// computed once at load and served from that snapshot, never by a scan
    /// per request (#1249, #27, #1262).
    #[test]
    fn extents_are_the_row_bounds_computed_once() {
        use ds_core::map_engine::MapEngine as _;
        let mut engine = CsvEngine::new(test_store());
        let rows = &engine.store.rows;
        let times = rows.iter().map(|row| row.time);
        let time_span = times.clone().min().zip(times.max());
        let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for row in rows {
            bbox[0] = bbox[0].min(row.longitude);
            bbox[1] = bbox[1].min(row.latitude);
            bbox[2] = bbox[2].max(row.longitude);
            bbox[3] = bbox[3].max(row.latitude);
        }
        assert!(time_span.is_some());
        let check = |engine: &CsvEngine| {
            assert_eq!(engine.get_temporal_extent(), time_span);
            assert_eq!(engine.temporal_extent(), time_span);
            assert_eq!(engine.get_spatial_extent(), Some(bbox));
            // Features serves the same box (#1262).
            assert_eq!(engine.spatial_extent(), Some(bbox));
            assert_eq!(engine.raster_info_shared().spatial_extent, Some(bbox));
        };
        check(&engine);
        // Emptying the rows after load changes nothing: a per-call scan
        // would now find no extent at all.
        engine.store.rows.clear();
        check(&engine);

        // A file of headers only has neither extent, as before.
        let empty = CsvEngine::new(CsvDataStore {
            rows: Vec::new(),
            location_index: HashMap::new(),
            time_index: HashMap::new(),
            parameter_names: vec!["temperature".into()],
            parameter_units: HashMap::new(),
        });
        assert_eq!(empty.get_temporal_extent(), None);
        assert_eq!(empty.temporal_extent(), None);
        assert_eq!(empty.get_spatial_extent(), None);
        assert_eq!(empty.spatial_extent(), None);
    }

    /// The x of every coverage, in answer order.
    fn station_xs(response: CoverageResponse) -> Vec<f64> {
        let CoverageResponse::Collection(coverages) = response else {
            panic!("an area query answers a collection");
        };
        coverages
            .iter()
            .map(|c| match c.domain {
                DomainDescription::PointSeries { x, .. } => x,
                ref other => panic!("expected a PointSeries: {other:?}"),
            })
            .collect()
    }

    /// EDR 1.2 `/req/edr/REQ_rc-within-response` A: a station inside the
    /// area without a row in the window is left out; the others still
    /// answer. Only an area without data at all is a 404.
    #[test]
    fn area_and_radius_skip_stations_without_data_in_the_window() {
        let engine = station_history();
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let area = |t: &str| {
            engine.query_area(
                "POLYGON((23 59,27 59,27 62,23 62,23 59))",
                Some((at(t), at(t))),
                None,
                None,
                None,
            )
        };
        let radius = |t: &str| {
            engine.query_radius(
                "POINT(25 60.5)",
                100_000.0,
                Some((at(t), at(t))),
                None,
                None,
                None,
            )
        };
        // Rows: zeta (x 24, then 30) and alpha (x 25) at 00:00 and 01:00,
        // beta (x 26) at 00:00 only.
        type Query<'a> = &'a dyn Fn(&str) -> Result<CoverageResponse, DataServerError>;
        for query in [&area as Query, &radius] {
            assert_eq!(
                station_xs(query("2026-01-01T00:00:00Z").unwrap()),
                [24.0, 25.0, 26.0]
            );
            let later = station_xs(query("2026-01-01T01:00:00Z").unwrap());
            assert_eq!(later.len(), 2, "{later:?}");
            assert!(!later.contains(&26.0), "{later:?}");
            assert!(matches!(
                query("2026-01-02T00:00:00Z"),
                Err(DataServerError::LocationNotFound(_))
            ));
        }
        // One location alone still 404s without data.
        assert!(matches!(
            engine.query_location("beta", Some((at("2026-01-01T01:00:00Z"), at("2026-01-01T01:00:00Z"))), None, None, None),
            Err(DataServerError::LocationNotFound(m)) if m.contains("no data in time range")
        ));
    }

    /// One coverage of an area answer: where it sits, its time axis and
    /// the values of the fixture's two parameters.
    #[derive(Debug, PartialEq)]
    struct Series {
        x: f64,
        y: f64,
        t: Vec<DateTime<Utc>>,
        temperature: Vec<Option<f64>>,
        humidity: Vec<Option<f64>>,
    }

    fn area_series(response: CoverageResponse) -> Vec<Series> {
        let CoverageResponse::Collection(coverages) = response else {
            panic!("an area query answers a collection");
        };
        coverages
            .into_iter()
            .map(|c| {
                let DomainDescription::PointSeries {
                    x,
                    y,
                    ref t,
                    z: None,
                } = c.domain
                else {
                    panic!("expected a PointSeries without z: {:?}", c.domain);
                };
                assert_eq!(c.ranges.len(), 2);
                Series {
                    x,
                    y,
                    t: t.clone(),
                    temperature: c.ranges["temperature"].values.clone(),
                    humidity: c.ranges["humidity"].values.clone(),
                }
            })
            .collect()
    }

    /// An area answers the stations whose first row lies inside it, in the
    /// order they first appear in the file, each with its whole series
    /// (#1262). `tests/fixtures/area_stations.csv` interleaves six
    /// stations: `kappa`, `alpha` and `leaver` start inside the polygon;
    /// `north` and `west` lie outside; `drifter` starts outside and moves
    /// inside, `leaver` starts inside and moves outside. The expected
    /// series are what the row scan before #1262 answered.
    #[test]
    fn area_answers_first_row_stations_in_first_observation_order() {
        let engine = CsvEngine::new(
            CsvDataStore::load(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/area_stations.csv"
            ))
            .unwrap(),
        );
        let at = |hour: u32| {
            format!("2026-01-01T{hour:02}:00:00Z")
                .parse::<DateTime<Utc>>()
                .unwrap()
        };
        let area = |coords: &str, datetime| engine.query_area(coords, datetime, None, None, None);
        let polygon = "POLYGON((23 59,27 59,27 62,23 62,23 59))";

        assert_eq!(
            area_series(area(polygon, None).unwrap()),
            [
                Series {
                    x: 24.5,
                    y: 60.5,
                    t: vec![at(0), at(1), at(2)],
                    temperature: vec![Some(1.0), Some(3.0), Some(5.0)],
                    humidity: vec![Some(80.0), Some(81.0), None],
                },
                Series {
                    x: 26.0,
                    y: 61.0,
                    t: vec![at(0), at(2)],
                    temperature: vec![Some(2.0), Some(4.0)],
                    humidity: vec![None, Some(85.0)],
                },
                Series {
                    x: 25.5,
                    y: 61.5,
                    t: vec![at(0), at(1)],
                    temperature: vec![Some(8.0), Some(12.0)],
                    humidity: vec![Some(60.0), Some(61.0)],
                },
            ]
        );
        // In a window the same stations, those without a row in it left out.
        assert_eq!(
            area_series(area(polygon, Some((at(2), at(2)))).unwrap()),
            [
                Series {
                    x: 24.5,
                    y: 60.5,
                    t: vec![at(2)],
                    temperature: vec![Some(5.0)],
                    humidity: vec![None],
                },
                Series {
                    x: 26.0,
                    y: 61.0,
                    t: vec![at(2)],
                    temperature: vec![Some(4.0)],
                    humidity: vec![Some(85.0)],
                },
            ]
        );
        // The later positions of `drifter` (25, 60) and `leaver` (30, 70)
        // match no station, as a polygon, a bbox or a radius.
        for coords in [
            "POLYGON((24.8 59.5,30.5 59.5,30.5 60.2,24.8 60.2,24.8 59.5))",
            "29,69,31,71",
        ] {
            assert!(
                matches!(
                    area(coords, None),
                    Err(DataServerError::LocationNotFound(_))
                ),
                "{coords}"
            );
        }
        assert!(matches!(
            engine.query_radius("POINT(25 60)", 20_000.0, None, None, None, None),
            Err(DataServerError::LocationNotFound(_))
        ));
    }

    /// The reported case: at 2026-01-29T19:00Z one of the five stations
    /// within 30 km of Porvoo has no row; the other four still answer.
    #[test]
    fn radius_answers_the_stations_with_data_at_an_instant() {
        let engine = CsvEngine::new(test_store());
        let radius = |t: &str| {
            let t = t.parse::<DateTime<Utc>>().unwrap();
            engine
                .query_radius("POINT(25.5 60.4)", 30_000.0, Some((t, t)), None, None, None)
                .map(station_xs)
        };
        assert_eq!(radius("2026-01-29T18:00:00Z").unwrap().len(), 5);
        assert_eq!(radius("2026-01-29T19:00:00Z").unwrap().len(), 4);
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
