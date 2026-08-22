use ds_core::feature::{
    Bbox, DatetimeInterval, Feature, FeaturePage, Geometry, PropertyValue, SortDirection, SortKey,
};
use serde_json::{json, Value};

fn property_value_to_json(v: &PropertyValue) -> Value {
    match v {
        PropertyValue::String(s) => Value::String(s.clone()),
        PropertyValue::Float(f) => json!(f),
        PropertyValue::Integer(i) => json!(i),
        PropertyValue::Bool(b) => json!(b),
        PropertyValue::Null => Value::Null,
        PropertyValue::List(items) => {
            Value::Array(items.iter().map(property_value_to_json).collect())
        }
    }
}

fn coords_to_json(ring: &[[f64; 2]]) -> Value {
    Value::Array(ring.iter().map(|c| json!([c[0], c[1]])).collect())
}

fn geometry_to_json(g: &Geometry) -> Value {
    match g {
        Geometry::Point { x, y } => json!({
            "type": "Point",
            "coordinates": [x, y]
        }),
        Geometry::Polygon { exterior, holes } => {
            let mut rings = vec![coords_to_json(exterior)];
            for hole in holes {
                rings.push(coords_to_json(hole));
            }
            json!({
                "type": "Polygon",
                "coordinates": rings
            })
        }
        Geometry::MultiPolygon { polygons } => {
            let polys: Vec<Value> = polygons
                .iter()
                .map(|(ext, holes)| {
                    let mut rings = vec![coords_to_json(ext)];
                    for hole in holes {
                        rings.push(coords_to_json(hole));
                    }
                    Value::Array(rings)
                })
                .collect();
            json!({
                "type": "MultiPolygon",
                "coordinates": polys
            })
        }
        Geometry::Null => Value::Null,
    }
}

pub fn feature_to_geojson(feature: &Feature, collection_id: &str, base_url: &str) -> Value {
    // Sorted iteration: serde_json's workspace-enabled `preserve_order` makes
    // insertion order the wire order, and engines build each feature's
    // property HashMap fresh per request — unsorted, byte-identical requests
    // would serialize differently and the content-derived ETag would never
    // revalidate (#499).
    let mut entries: Vec<_> = feature.properties.iter().collect();
    entries.sort_by_key(|(k, _)| *k);
    let properties: serde_json::Map<String, Value> = entries
        .into_iter()
        .map(|(k, v)| (k.clone(), property_value_to_json(v)))
        .collect();

    json!({
        "type": "Feature",
        "id": feature.id,
        "geometry": geometry_to_json(&feature.geometry),
        "properties": properties,
        "links": [
            {
                "href": format!("{base_url}/features/collections/{}/items/{}", collection_id, feature.id),
                "rel": "self",
                "type": "application/geo+json"
            },
            {
                "href": format!("{base_url}/features/collections/{}", collection_id),
                "rel": "collection",
                "type": "application/json"
            }
        ]
    })
}

/// Rebuild the filter/sort part of the query string for pagination links.
///
/// Built from the PARSED values rather than echoed from the raw input, which
/// makes the links canonical and sidesteps re-encoding: a client that sent
/// `sortby=+score` gave us `" score"` after form decoding, and echoing that
/// back would emit a literal space into a URL.
///
/// Returns either an empty string or a fragment starting with `&`.
pub fn preserved_query(
    bbox: Option<&Bbox>,
    datetime: Option<&DatetimeInterval>,
    sortby: &[SortKey],
) -> String {
    let mut q = String::new();
    if let Some(b) = bbox {
        q.push_str(&format!(
            "&bbox={},{},{},{}",
            b.west, b.south, b.east, b.north
        ));
    }
    if let Some(d) = datetime {
        // AutoSi, not Secs: truncating `.500Z` would make the next link apply
        // a DIFFERENT time window than page 1 and return a different row set
        // — the pagination-drops-your-query bug this function exists to fix,
        // reintroduced at sub-second scale. Collections with sub-second
        // timestamps (the PostGIS events shape) hit this.
        let fmt = |t: chrono::DateTime<chrono::Utc>| {
            t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        };
        let value = match (d.start, d.end) {
            (Some(s), Some(e)) if s == e => fmt(s),
            (s, e) => format!(
                "{}/{}",
                s.map(fmt).unwrap_or_else(|| "..".into()),
                e.map(fmt).unwrap_or_else(|| "..".into())
            ),
        };
        q.push_str(&format!("&datetime={value}"));
    }
    if !sortby.is_empty() {
        let terms: Vec<String> = sortby
            .iter()
            .map(|k| match k.direction {
                // Ascending is emitted bare, never as `+`: an unencoded `+`
                // decodes back to a space on the next request.
                SortDirection::Ascending => k.property.clone(),
                SortDirection::Descending => format!("-{}", k.property),
            })
            .collect();
        q.push_str(&format!("&sortby={}", terms.join(",")));
    }
    q
}

#[allow(clippy::too_many_arguments)] // pagination links need every query axis
pub fn feature_page_to_geojson(
    page: &FeaturePage,
    collection_id: &str,
    limit: usize,
    offset: usize,
    // Filter/sort fragment from `preserved_query`, carried onto every
    // pagination link. Without it, following `rel="next"` — the pattern OGC
    // recommends — silently drops the caller's filters and ordering.
    filters: &str,
    timestamp: &str,
    base_url: &str,
) -> Value {
    let features: Vec<Value> = page
        .features
        .iter()
        .map(|f| feature_to_geojson(f, collection_id, base_url))
        .collect();

    let mut links = vec![json!({
        "href": format!("{base_url}/features/collections/{}/items?offset={}&limit={}{}", collection_id, offset, limit, filters),
        "rel": "self",
        "type": "application/geo+json"
    })];

    if let Some(next) = page.next_offset {
        links.push(json!({
            "href": format!("{base_url}/features/collections/{}/items?offset={}&limit={}{}", collection_id, next, limit, filters),
            "rel": "next",
            "type": "application/geo+json"
        }));
    }

    if offset > 0 {
        let prev_offset = offset.saturating_sub(limit);
        links.push(json!({
            "href": format!("{base_url}/features/collections/{}/items?offset={}&limit={}{}", collection_id, prev_offset, limit, filters),
            "rel": "prev",
            "type": "application/geo+json"
        }));
    }

    json!({
        "type": "FeatureCollection",
        "timeStamp": timestamp,
        "numberMatched": page.number_matched,
        "numberReturned": page.number_returned,
        "features": features,
        "links": links
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_feature() -> Feature {
        let mut properties = HashMap::new();
        properties.insert("name".into(), PropertyValue::String("Helsinki".into()));
        properties.insert("temp".into(), PropertyValue::Float(-2.5));
        properties.insert("active".into(), PropertyValue::Bool(true));
        properties.insert("missing".into(), PropertyValue::Null);
        properties.insert(
            "quantities".into(),
            PropertyValue::List(vec![
                PropertyValue::String("DBZH".into()),
                PropertyValue::String("VRADH".into()),
            ]),
        );

        Feature {
            id: "Helsinki".into(),
            geometry: std::sync::Arc::new(Geometry::Point {
                x: 24.9384,
                y: 60.1699,
            }),
            properties: std::sync::Arc::new(properties),
        }
    }

    #[test]
    fn feature_geojson_structure() {
        let f = sample_feature();
        let json = feature_to_geojson(&f, "weather", "");

        assert_eq!(json["type"], "Feature");
        assert_eq!(json["id"], "Helsinki");
        assert_eq!(json["geometry"]["type"], "Point");
        assert_eq!(json["geometry"]["coordinates"][0], 24.9384);
        assert_eq!(json["geometry"]["coordinates"][1], 60.1699);
        assert_eq!(json["properties"]["name"], "Helsinki");
        assert_eq!(json["properties"]["temp"], -2.5);
        assert_eq!(json["properties"]["active"], true);
        assert!(json["properties"]["missing"].is_null());
        // List → JSON array
        assert_eq!(json["properties"]["quantities"], json!(["DBZH", "VRADH"]));
    }

    #[test]
    fn feature_page_geojson_structure() {
        let page = FeaturePage {
            features: vec![sample_feature()],
            number_matched: 3,
            number_returned: 1,
            next_offset: Some(1),
        };
        let json = feature_page_to_geojson(&page, "weather", 1, 0, "", "2024-01-01T00:00:00Z", "");

        assert_eq!(json["type"], "FeatureCollection");
        assert_eq!(json["numberMatched"], 3);
        assert_eq!(json["numberReturned"], 1);
        assert_eq!(json["features"].as_array().unwrap().len(), 1);

        // Has self and next links
        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(links.iter().any(|l| l["rel"] == "next"));
    }

    #[test]
    fn feature_page_no_next_link_on_last_page() {
        let page = FeaturePage {
            features: vec![sample_feature()],
            number_matched: 1,
            number_returned: 1,
            next_offset: None,
        };
        let json = feature_page_to_geojson(&page, "weather", 10, 0, "", "2024-01-01T00:00:00Z", "");

        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "self"));
        assert!(!links.iter().any(|l| l["rel"] == "next"));
    }

    #[test]
    fn feature_page_prev_link_when_offset() {
        let page = FeaturePage {
            features: vec![sample_feature()],
            number_matched: 3,
            number_returned: 1,
            next_offset: Some(2),
        };
        let json = feature_page_to_geojson(&page, "weather", 1, 1, "", "2024-01-01T00:00:00Z", "");

        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "prev"));
    }

    #[test]
    fn empty_feature_page() {
        let page = FeaturePage {
            features: vec![],
            number_matched: 0,
            number_returned: 0,
            next_offset: None,
        };
        let json = feature_page_to_geojson(&page, "weather", 10, 0, "", "2024-01-01T00:00:00Z", "");

        assert_eq!(json["type"], "FeatureCollection");
        assert_eq!(json["numberMatched"], 0);
        assert!(json["features"].as_array().unwrap().is_empty());
    }
}
