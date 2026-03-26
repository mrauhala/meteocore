use std::collections::HashMap;
use std::io::BufReader;

use ds_core::error::DataServerError;
use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;

use crate::spatial::SpatialIndex;

/// Maximum number of coordinates per geometry to prevent geometry bombs.
const MAX_COORDS_PER_GEOMETRY: usize = 100_000;

/// Maximum number of features per file.
const MAX_FEATURES: usize = 1_000_000;

/// Maximum file size in bytes (500 MB).
const MAX_FILE_SIZE: u64 = 500 * 1024 * 1024;

struct StoredFeature {
    id: String,
    geometry: Geometry,
    properties: HashMap<String, PropertyValue>,
    /// None for null-geometry features.
    bbox: Option<[f64; 4]>,
}

pub struct GeoJsonEngine {
    features: Vec<StoredFeature>,
    id_index: HashMap<String, usize>,
    spatial_index: SpatialIndex,
    spatial_extent: Option<[f64; 4]>,
}

impl GeoJsonEngine {
    /// Load a GeoJSON FeatureCollection from a file.
    /// Coordinates must be in WGS84 (EPSG:4326).
    pub fn load(path: &str) -> Result<Self, DataServerError> {
        // Check file size
        let metadata = std::fs::metadata(path)
            .map_err(|e| DataServerError::GeoJson(format!("Failed to read file metadata: {e}")))?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err(DataServerError::GeoJson(format!(
                "File exceeds maximum size of {} MB",
                MAX_FILE_SIZE / (1024 * 1024)
            )));
        }

        let file = std::fs::File::open(path)
            .map_err(|e| DataServerError::GeoJson(format!("Failed to open file: {e}")))?;
        let reader = BufReader::new(file);

        let geojson: serde_json::Value = serde_json::from_reader(reader)
            .map_err(|e| DataServerError::GeoJson(format!("Invalid JSON: {e}")))?;

        let fc_type = geojson.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if fc_type != "FeatureCollection" {
            return Err(DataServerError::GeoJson(
                "Expected a GeoJSON FeatureCollection".into(),
            ));
        }

        let raw_features = geojson
            .get("features")
            .and_then(|v| v.as_array())
            .ok_or_else(|| DataServerError::GeoJson("Missing 'features' array".into()))?;

        if raw_features.len() > MAX_FEATURES {
            return Err(DataServerError::GeoJson(format!(
                "File has {} features, exceeding limit of {}",
                raw_features.len(),
                MAX_FEATURES
            )));
        }

        let mut features = Vec::with_capacity(raw_features.len());
        let mut id_index = HashMap::new();

        for (idx, raw) in raw_features.iter().enumerate() {
            let id = extract_feature_id(raw, idx);
            let geometry = parse_geometry(raw.get("geometry"))?;
            let properties = parse_properties(raw.get("properties"));
            let bbox = geometry.bbox();

            // Validate coordinates are in WGS84 range
            if let Some(ref b) = bbox {
                validate_wgs84_bbox(b)?;
            }

            if let std::collections::hash_map::Entry::Vacant(e) = id_index.entry(id.clone()) {
                e.insert(features.len());
            }
            // Duplicate IDs: first one wins in the index, but all are stored

            features.push(StoredFeature {
                id,
                geometry,
                properties,
                bbox,
            });
        }

        // Build spatial index from features that have geometry.
        // Collect (original_index, bbox) pairs so the spatial index maps back correctly.
        let indexed_bboxes: Vec<(usize, [f64; 4])> = features
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.bbox.map(|b| (i, b)))
            .collect();
        let spatial_index = SpatialIndex::build_indexed(&indexed_bboxes);

        // Compute overall extent from features with geometry
        let spatial_extent = {
            let mut extent = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            let mut has_geometry = false;
            for f in &features {
                if let Some(b) = f.bbox {
                    has_geometry = true;
                    extent[0] = extent[0].min(b[0]);
                    extent[1] = extent[1].min(b[1]);
                    extent[2] = extent[2].max(b[2]);
                    extent[3] = extent[3].max(b[3]);
                }
            }
            if has_geometry {
                Some(extent)
            } else {
                None
            }
        };

        Ok(GeoJsonEngine {
            features,
            id_index,
            spatial_index,
            spatial_extent,
        })
    }

    /// Get the overall spatial extent [west, south, east, north].
    pub fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.spatial_extent
    }

    /// Get the number of loaded features.
    pub fn feature_count(&self) -> usize {
        self.features.len()
    }

    fn to_feature(&self, stored: &StoredFeature) -> Feature {
        Feature {
            id: stored.id.clone(),
            geometry: stored.geometry.clone(),
            properties: stored.properties.clone(),
        }
    }
}

impl FeatureEngine for GeoJsonEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let indices: Vec<usize> = match &query.bbox {
            Some(bbox) => {
                let mut hits = self.spatial_index.query(bbox);
                hits.sort_unstable();
                hits.dedup();
                hits
            }
            None => (0..self.features.len()).collect(),
        };

        let number_matched = indices.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);

        let features: Vec<Feature> = indices[offset..end]
            .iter()
            .map(|&i| self.to_feature(&self.features[i]))
            .collect();

        let number_returned = features.len();
        let next_offset = if end < number_matched {
            Some(end)
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
        let &idx = self
            .id_index
            .get(feature_id)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))?;
        Ok(self.to_feature(&self.features[idx]))
    }

    fn feature_count(&self) -> usize {
        self.features.len()
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.spatial_extent
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn extract_feature_id(feature: &serde_json::Value, fallback_idx: usize) -> String {
    // Try top-level "id" field first
    if let Some(id) = feature.get("id") {
        match id {
            serde_json::Value::String(s) => return s.clone(),
            serde_json::Value::Number(n) => return n.to_string(),
            _ => {}
        }
    }
    // Fall back to array index
    fallback_idx.to_string()
}

fn parse_geometry(geom: Option<&serde_json::Value>) -> Result<Geometry, DataServerError> {
    let geom = geom.ok_or_else(|| DataServerError::GeoJson("Feature missing geometry".into()))?;

    if geom.is_null() {
        return Ok(Geometry::Null);
    }

    let geom_type = geom.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match geom_type {
        "Point" => {
            let coords = geom
                .get("coordinates")
                .and_then(|v| v.as_array())
                .ok_or_else(|| DataServerError::GeoJson("Point missing coordinates".into()))?;
            let x = coords
                .first()
                .and_then(|v| v.as_f64())
                .ok_or_else(|| DataServerError::GeoJson("Invalid Point x coordinate".into()))?;
            let y = coords
                .get(1)
                .and_then(|v| v.as_f64())
                .ok_or_else(|| DataServerError::GeoJson("Invalid Point y coordinate".into()))?;
            validate_coord(x, y)?;
            Ok(Geometry::Point { x, y })
        }
        "Polygon" => {
            let rings = geom
                .get("coordinates")
                .and_then(|v| v.as_array())
                .ok_or_else(|| DataServerError::GeoJson("Polygon missing coordinates".into()))?;
            let (exterior, holes) = parse_polygon_rings(rings)?;
            Ok(Geometry::Polygon { exterior, holes })
        }
        "MultiPolygon" => {
            let polygons_raw = geom
                .get("coordinates")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    DataServerError::GeoJson("MultiPolygon missing coordinates".into())
                })?;

            let mut polygons = Vec::with_capacity(polygons_raw.len());
            let mut total_coords = 0;

            for poly_raw in polygons_raw {
                let rings = poly_raw.as_array().ok_or_else(|| {
                    DataServerError::GeoJson("Invalid MultiPolygon ring array".into())
                })?;
                let (ext, holes) = parse_polygon_rings(rings)?;
                total_coords += ext.len();
                for h in &holes {
                    total_coords += h.len();
                }
                if total_coords > MAX_COORDS_PER_GEOMETRY {
                    return Err(DataServerError::GeoJson(format!(
                        "Geometry exceeds maximum coordinate count of {}",
                        MAX_COORDS_PER_GEOMETRY
                    )));
                }
                polygons.push((ext, holes));
            }

            Ok(Geometry::MultiPolygon { polygons })
        }
        other => Err(DataServerError::GeoJson(format!(
            "Unsupported geometry type: {other}"
        ))),
    }
}

#[allow(clippy::type_complexity)]
fn parse_polygon_rings(
    rings: &[serde_json::Value],
) -> Result<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>), DataServerError> {
    if rings.is_empty() {
        return Err(DataServerError::GeoJson(
            "Polygon must have at least one ring".into(),
        ));
    }

    let exterior = parse_ring(&rings[0])?;
    let mut holes = Vec::new();
    for ring in &rings[1..] {
        holes.push(parse_ring(ring)?);
    }

    Ok((exterior, holes))
}

fn parse_ring(ring: &serde_json::Value) -> Result<Vec<[f64; 2]>, DataServerError> {
    let coords_raw = ring
        .as_array()
        .ok_or_else(|| DataServerError::GeoJson("Ring is not an array".into()))?;

    let mut coords = Vec::with_capacity(coords_raw.len());
    for c in coords_raw {
        let pair = c
            .as_array()
            .ok_or_else(|| DataServerError::GeoJson("Coordinate is not an array".into()))?;
        let x = pair
            .first()
            .and_then(|v| v.as_f64())
            .ok_or_else(|| DataServerError::GeoJson("Invalid x coordinate".into()))?;
        let y = pair
            .get(1)
            .and_then(|v| v.as_f64())
            .ok_or_else(|| DataServerError::GeoJson("Invalid y coordinate".into()))?;
        validate_coord(x, y)?;
        coords.push([x, y]);
    }

    Ok(coords)
}

fn validate_coord(x: f64, y: f64) -> Result<(), DataServerError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(DataServerError::GeoJson(
            "Coordinates must be finite numbers".into(),
        ));
    }
    if !(-180.0..=180.0).contains(&x) || !(-90.0..=90.0).contains(&y) {
        return Err(DataServerError::GeoJson(format!(
            "Coordinates ({x}, {y}) outside WGS84 range. \
             If your data uses a projected CRS, convert to WGS84 first."
        )));
    }
    Ok(())
}

fn validate_wgs84_bbox(bbox: &[f64; 4]) -> Result<(), DataServerError> {
    for &v in bbox {
        if !v.is_finite() {
            return Err(DataServerError::GeoJson(
                "Geometry has non-finite coordinates".into(),
            ));
        }
    }
    Ok(())
}

fn parse_properties(props: Option<&serde_json::Value>) -> HashMap<String, PropertyValue> {
    let mut result = HashMap::new();
    let obj = match props.and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return result,
    };

    for (key, value) in obj {
        let pv = match value {
            serde_json::Value::String(s) => PropertyValue::String(s.clone()),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    PropertyValue::Integer(i)
                } else if let Some(f) = n.as_f64() {
                    PropertyValue::Float(f)
                } else {
                    PropertyValue::Null
                }
            }
            serde_json::Value::Bool(b) => PropertyValue::Bool(*b),
            serde_json::Value::Null => PropertyValue::Null,
            // Nested objects/arrays: serialize to string representation
            other => PropertyValue::String(other.to_string()),
        };
        result.insert(key.clone(), pv);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_geojson() -> &'static str {
        r#"{
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "id": "city.1",
                    "geometry": {
                        "type": "Point",
                        "coordinates": [24.9384, 60.1699]
                    },
                    "properties": {
                        "name": "Helsinki",
                        "population": 658457
                    }
                },
                {
                    "type": "Feature",
                    "id": "area.1",
                    "geometry": {
                        "type": "Polygon",
                        "coordinates": [[
                            [24.0, 60.0],
                            [25.0, 60.0],
                            [25.0, 61.0],
                            [24.0, 61.0],
                            [24.0, 60.0]
                        ]]
                    },
                    "properties": {
                        "name": "Test Area",
                        "area_km2": 12345.6
                    }
                },
                {
                    "type": "Feature",
                    "id": "multi.1",
                    "geometry": {
                        "type": "MultiPolygon",
                        "coordinates": [
                            [[
                                [20.0, 59.0],
                                [21.0, 59.0],
                                [21.0, 60.0],
                                [20.0, 60.0],
                                [20.0, 59.0]
                            ]],
                            [[
                                [22.0, 59.0],
                                [23.0, 59.0],
                                [23.0, 60.0],
                                [22.0, 60.0],
                                [22.0, 59.0]
                            ]]
                        ]
                    },
                    "properties": {
                        "name": "Island Group",
                        "active": true
                    }
                }
            ]
        }"#
    }

    fn load_from_string(json: &str) -> GeoJsonEngine {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("test_geojson_{n}.json"));
        std::fs::write(&tmp, json).unwrap();
        GeoJsonEngine::load(tmp.to_str().unwrap()).unwrap()
    }

    #[test]
    fn loads_mixed_geometry_types() {
        let engine = load_from_string(sample_geojson());
        assert_eq!(engine.feature_count(), 3);
    }

    #[test]
    fn feature_ids_extracted() {
        let engine = load_from_string(sample_geojson());
        assert!(engine.get_feature("city.1").is_ok());
        assert!(engine.get_feature("area.1").is_ok());
        assert!(engine.get_feature("multi.1").is_ok());
        assert!(engine.get_feature("nonexistent").is_err());
    }

    #[test]
    fn properties_parsed_correctly() {
        let engine = load_from_string(sample_geojson());
        let f = engine.get_feature("city.1").unwrap();
        assert_eq!(
            f.properties.get("name"),
            Some(&PropertyValue::String("Helsinki".into()))
        );
        assert_eq!(
            f.properties.get("population"),
            Some(&PropertyValue::Integer(658457))
        );
    }

    #[test]
    fn polygon_geometry_preserved() {
        let engine = load_from_string(sample_geojson());
        let f = engine.get_feature("area.1").unwrap();
        match &f.geometry {
            Geometry::Polygon { exterior, holes } => {
                assert_eq!(exterior.len(), 5);
                assert!(holes.is_empty());
                assert_eq!(exterior[0], [24.0, 60.0]);
            }
            _ => panic!("Expected Polygon geometry"),
        }
    }

    #[test]
    fn multipolygon_geometry_preserved() {
        let engine = load_from_string(sample_geojson());
        let f = engine.get_feature("multi.1").unwrap();
        match &f.geometry {
            Geometry::MultiPolygon { polygons } => {
                assert_eq!(polygons.len(), 2);
                assert_eq!(polygons[0].0.len(), 5); // first polygon exterior
                assert_eq!(polygons[1].0.len(), 5); // second polygon exterior
            }
            _ => panic!("Expected MultiPolygon geometry"),
        }
    }

    #[test]
    fn bbox_query_filters_correctly() {
        let engine = load_from_string(sample_geojson());

        // Bbox that covers only the Helsinki point and the Test Area polygon
        let bbox = ds_core::feature::Bbox::new(24.0, 60.0, 25.5, 61.5).unwrap();
        let query = FeatureQuery {
            bbox: Some(bbox),
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        // Should match city.1 (point at 24.9,60.1) and area.1 (bbox 24-25, 60-61)
        assert_eq!(page.number_matched, 2);
    }

    #[test]
    fn bbox_query_no_match() {
        let engine = load_from_string(sample_geojson());
        let bbox = ds_core::feature::Bbox::new(0.0, 0.0, 1.0, 1.0).unwrap();
        let query = FeatureQuery {
            bbox: Some(bbox),
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 0);
    }

    #[test]
    fn pagination_works() {
        let engine = load_from_string(sample_geojson());
        let query = FeatureQuery {
            limit: 2,
            offset: 0,
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_matched, 3);
        assert_eq!(page.number_returned, 2);
        assert_eq!(page.next_offset, Some(2));

        let query = FeatureQuery {
            limit: 2,
            offset: 2,
            ..Default::default()
        };
        let page = engine.get_features(&query).unwrap();
        assert_eq!(page.number_returned, 1);
        assert!(page.next_offset.is_none());
    }

    #[test]
    fn spatial_extent_computed() {
        let engine = load_from_string(sample_geojson());
        let extent = engine.spatial_extent().unwrap();
        // MultiPolygon starts at lon=20, Point goes to lon~25
        assert!(extent[0] <= 20.0); // west
        assert!(extent[1] <= 59.0); // south
        assert!(extent[2] >= 24.9); // east
        assert!(extent[3] >= 61.0); // north
    }

    #[test]
    fn rejects_non_wgs84_coordinates() {
        let json = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "Point",
                    "coordinates": [385785.7, 6672356.2]
                },
                "properties": {}
            }]
        }"#;
        let tmp = std::env::temp_dir().join("test_geojson_proj.json");
        std::fs::write(&tmp, json).unwrap();
        let result = GeoJsonEngine::load(tmp.to_str().unwrap());
        assert!(result.is_err());
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("Expected error"),
        };
        assert!(err.contains("WGS84"));
    }

    #[test]
    fn rejects_non_feature_collection() {
        let json = r#"{"type": "Feature", "geometry": null, "properties": {}}"#;
        let tmp = std::env::temp_dir().join("test_geojson_single.json");
        std::fs::write(&tmp, json).unwrap();
        let result = GeoJsonEngine::load(tmp.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn fallback_id_from_index() {
        let json = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [0.0, 0.0]},
                "properties": {"name": "test"}
            }]
        }"#;
        let engine = load_from_string(json);
        // No "id" field → falls back to index "0"
        assert!(engine.get_feature("0").is_ok());
    }
}
