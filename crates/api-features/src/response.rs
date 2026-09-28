use ds_core::feature::{
    Bbox, DatetimeInterval, Feature, FeaturePage, Geometry, PropertyValue, SortDirection, SortKey,
};
use serde_json::{json, Value};

use crate::crs::ResponseCrs;

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
        PropertyValue::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), property_value_to_json(v)))
                .collect(),
        ),
    }
}

fn coords_to_json(ring: &[[f64; 2]], crs: &ResponseCrs) -> Option<Value> {
    ring.iter()
        .map(|c| crs.position(c[0], c[1]).map(|[a, b]| json!([a, b])))
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

fn polygon_to_json(
    exterior: &[[f64; 2]],
    holes: &[Vec<[f64; 2]>],
    crs: &ResponseCrs,
) -> Option<Value> {
    std::iter::once(exterior)
        .chain(holes.iter().map(Vec::as_slice))
        .map(|ring| coords_to_json(ring, crs))
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

/// GeoJSON geometry with every vertex mapped into `crs` (Part 2). A geometry
/// with a vertex that has no finite coordinates in `crs` is `null`: JSON has
/// no infinity, and a partial geometry would be a different shape.
fn geometry_to_json(g: &Geometry, crs: &ResponseCrs) -> Value {
    let (kind, coordinates) = match g {
        Geometry::Point { x, y } => ("Point", crs.position(*x, *y).map(|[a, b]| json!([a, b]))),
        Geometry::Polygon { exterior, holes } => ("Polygon", polygon_to_json(exterior, holes, crs)),
        Geometry::MultiPolygon { polygons } => (
            "MultiPolygon",
            polygons
                .iter()
                .map(|(exterior, holes)| polygon_to_json(exterior, holes, crs))
                .collect::<Option<Vec<_>>>()
                .map(Value::Array),
        ),
        Geometry::Null => return Value::Null,
    };
    match coordinates {
        Some(coordinates) => json!({"type": kind, "coordinates": coordinates}),
        None => Value::Null,
    }
}

/// `path` with the response CRS's link query, if any.
fn with_crs(path: String, crs: &ResponseCrs) -> String {
    match crs.link_query() {
        q if q.is_empty() => path,
        q => format!("{path}?{q}"),
    }
}

/// `root` is the absolute URL of the API root serving the collection: the
/// per-API `/features` service or the shared OGC API root (#789). Geometry is
/// in `crs`, and the `self` link names it when the request did.
pub fn feature_to_geojson(
    feature: &Feature,
    collection_id: &str,
    root: &str,
    crs: &ResponseCrs,
) -> Value {
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
        "geometry": geometry_to_json(&feature.geometry, crs),
        "properties": properties,
        "links": [
            {
                "href": with_crs(format!("{root}/collections/{}/items/{}", collection_id, crate::html::path_segment(&feature.id)), crs),
                "rel": "self",
                "type": "application/geo+json"
            },
            {
                "href": format!("{root}/collections/{}", collection_id),
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
    property_filters: &[(String, String)],
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
    if !property_filters.is_empty() {
        q.push('&');
        q.push_str(
            &form_urlencoded::Serializer::new(String::new())
                .extend_pairs(property_filters.iter().map(|(k, v)| (k, v)))
                .finish(),
        );
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
    root: &str,
    crs: &ResponseCrs,
) -> Value {
    let features: Vec<Value> = page
        .features
        .iter()
        .map(|f| feature_to_geojson(f, collection_id, root, crs))
        .collect();
    // The CRS rides along with the filters: a `next` page in another CRS
    // would mix coordinate systems in one result set.
    let filters = match crs.link_query() {
        q if q.is_empty() => filters.to_owned(),
        q => format!("{filters}&{q}"),
    };

    let mut links = vec![json!({
        "href": format!("{root}/collections/{}/items?offset={}&limit={}{}", collection_id, offset, limit, filters),
        "rel": "self",
        "type": "application/geo+json"
    })];

    if let Some(next) = page.next_offset {
        links.push(json!({
            "href": format!("{root}/collections/{}/items?offset={}&limit={}{}", collection_id, next, limit, filters),
            "rel": "next",
            "type": "application/geo+json"
        }));
    }

    if offset > 0 {
        let prev_offset = offset.saturating_sub(limit);
        links.push(json!({
            "href": format!("{root}/collections/{}/items?offset={}&limit={}{}", collection_id, prev_offset, limit, filters),
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
        properties.insert(
            "contributions".into(),
            PropertyValue::List(vec![PropertyValue::Object(vec![
                ("term".into(), PropertyValue::String("clutter".into())),
                ("value".into(), PropertyValue::Float(-0.4)),
            ])]),
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
        let json = feature_to_geojson(&f, "weather", "", &ResponseCrs::default());

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
        // Record → JSON object, fields in their built order.
        assert_eq!(
            json["properties"]["contributions"],
            json!([{"term": "clutter", "value": -0.4}])
        );
        assert_eq!(
            serde_json::to_string(&json["properties"]["contributions"]).unwrap(),
            r#"[{"term":"clutter","value":-0.4}]"#
        );
    }

    #[test]
    fn feature_page_geojson_structure() {
        let page = FeaturePage {
            features: vec![sample_feature()],
            number_matched: 3,
            number_returned: 1,
            next_offset: Some(1),
        };
        let json = feature_page_to_geojson(
            &page,
            "weather",
            1,
            0,
            "",
            "2024-01-01T00:00:00Z",
            "",
            &ResponseCrs::default(),
        );

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
        let json = feature_page_to_geojson(
            &page,
            "weather",
            10,
            0,
            "",
            "2024-01-01T00:00:00Z",
            "",
            &ResponseCrs::default(),
        );

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
        let json = feature_page_to_geojson(
            &page,
            "weather",
            1,
            1,
            "",
            "2024-01-01T00:00:00Z",
            "",
            &ResponseCrs::default(),
        );

        let links = json["links"].as_array().unwrap();
        assert!(links.iter().any(|l| l["rel"] == "prev"));
    }

    #[test]
    fn geometry_is_reprojected_per_vertex_in_axis_order() {
        use crate::crs::FeatureCrs;
        let polygon = Geometry::MultiPolygon {
            polygons: vec![(
                vec![[25.0, 60.0], [26.0, 60.0], [26.0, 61.0], [25.0, 60.0]],
                vec![vec![[25.2, 60.2], [25.4, 60.2], [25.4, 60.4], [25.2, 60.2]]],
            )],
        };
        let lat_lon = geometry_to_json(&polygon, &ResponseCrs::new(Some(FeatureCrs::Epsg4326)));
        assert_eq!(lat_lon["type"], "MultiPolygon");
        assert_eq!(lat_lon["coordinates"][0][0][1], json!([60.0, 26.0]));
        assert_eq!(lat_lon["coordinates"][0][1][2], json!([60.4, 25.4]));
        // cs2cs EPSG:4326 EPSG:3067 of (60°N, 26°E) and (61°N, 26°E).
        let tm = geometry_to_json(&polygon, &ResponseCrs::new(Some(FeatureCrs::Epsg3067)));
        for (vertex, expected) in [
            (1, [444223.7332, 6651832.7353]),
            (2, [445915.6190, 6763200.1641]),
        ] {
            let got = &tm["coordinates"][0][0][vertex];
            assert!(
                (got[0].as_f64().unwrap() - expected[0]).abs() < 0.001,
                "{got}"
            );
            assert!(
                (got[1].as_f64().unwrap() - expected[1]).abs() < 0.001,
                "{got}"
            );
        }
        assert_eq!(
            geometry_to_json(&polygon, &ResponseCrs::default()),
            geometry_to_json(&polygon, &ResponseCrs::new(Some(FeatureCrs::Crs84)))
        );
    }

    #[test]
    fn a_geometry_the_crs_cannot_represent_is_null() {
        use crate::crs::FeatureCrs;
        let mercator = ResponseCrs::new(Some(FeatureCrs::Epsg3857));
        let pole = Geometry::Point { x: 0.0, y: -90.0 };
        assert!(geometry_to_json(&pole, &mercator).is_null());
        let ring = Geometry::Polygon {
            exterior: vec![[0.0, -80.0], [10.0, -90.0], [20.0, -80.0], [0.0, -80.0]],
            holes: vec![],
        };
        assert!(geometry_to_json(&ring, &mercator).is_null());
        // CRS84 passes every stored coordinate through.
        assert_eq!(
            geometry_to_json(&pole, &ResponseCrs::default())["coordinates"],
            json!([0.0, -90.0])
        );
    }

    #[test]
    fn links_carry_a_requested_crs() {
        use crate::crs::FeatureCrs;
        let crs = ResponseCrs::new(Some(FeatureCrs::Epsg4326));
        let page = FeaturePage {
            features: vec![sample_feature()],
            number_matched: 3,
            number_returned: 1,
            next_offset: Some(1),
        };
        let json = feature_page_to_geojson(&page, "weather", 1, 0, "&bbox=1,2,3,4", "", "", &crs);
        let encoded = "crs=http%3A%2F%2Fwww.opengis.net%2Fdef%2Fcrs%2FEPSG%2F0%2F4326";
        for link in json["links"].as_array().unwrap() {
            let href = link["href"].as_str().unwrap();
            assert!(
                href.ends_with(&format!("&bbox=1,2,3,4&{encoded}")),
                "{href}"
            );
        }
        assert_eq!(
            json["features"][0]["links"][0]["href"],
            format!("/collections/weather/items/Helsinki?{encoded}")
        );
        assert_eq!(
            json["features"][0]["geometry"]["coordinates"],
            json!([60.1699, 24.9384])
        );
    }

    #[test]
    fn empty_feature_page() {
        let page = FeaturePage {
            features: vec![],
            number_matched: 0,
            number_returned: 0,
            next_offset: None,
        };
        let json = feature_page_to_geojson(
            &page,
            "weather",
            10,
            0,
            "",
            "2024-01-01T00:00:00Z",
            "",
            &ResponseCrs::default(),
        );

        assert_eq!(json["type"], "FeatureCollection");
        assert_eq!(json["numberMatched"], 0);
        assert!(json["features"].as_array().unwrap().is_empty());
    }
}
