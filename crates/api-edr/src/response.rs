use ds_core::model::{Location, QueryResult};
use serde_json::{json, Value};

pub fn query_result_to_coverage_json(result: &QueryResult) -> Value {
    let times: Vec<String> = result
        .domain
        .axes_t
        .iter()
        .map(|t| t.to_rfc3339())
        .collect();

    let mut parameters = serde_json::Map::new();
    for (name, desc) in &result.parameters {
        parameters.insert(
            name.clone(),
            json!({
                "type": "Parameter",
                "description": {
                    "en": desc.label
                },
                "unit": {
                    "label": {
                        "en": desc.unit
                    },
                    "symbol": {
                        "value": desc.unit,
                        "type": "http://www.opengis.net/def/uom/UCUM/"
                    }
                },
                "observedProperty": {
                    "id": desc.observed_property,
                    "label": {
                        "en": desc.label
                    }
                }
            }),
        );
    }

    let mut ranges = serde_json::Map::new();
    for (name, ndarray) in &result.ranges {
        let values: Vec<Value> = ndarray
            .values
            .iter()
            .map(|v| match v {
                Some(f) => json!(f),
                None => Value::Null,
            })
            .collect();
        ranges.insert(
            name.clone(),
            json!({
                "type": "NdArray",
                "dataType": "float",
                "axisNames": ndarray.axis_names,
                "shape": ndarray.shape,
                "values": values
            }),
        );
    }

    json!({
        "type": "Coverage",
        "domain": {
            "type": "Domain",
            "domainType": result.domain.domain_type,
            "axes": {
                "x": { "values": [result.domain.axes_x] },
                "y": { "values": [result.domain.axes_y] },
                "t": { "values": times }
            },
            "referencing": [
                {
                    "coordinates": ["x", "y"],
                    "system": {
                        "type": "GeographicCRS",
                        "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
                    }
                },
                {
                    "coordinates": ["t"],
                    "system": {
                        "type": "TemporalRS",
                        "calendar": "Gregorian"
                    }
                }
            ]
        },
        "parameters": parameters,
        "ranges": ranges
    })
}

pub fn locations_to_geojson(locations: &[Location]) -> Value {
    let features: Vec<Value> = locations
        .iter()
        .map(|loc| {
            json!({
                "type": "Feature",
                "id": loc.id,
                "geometry": {
                    "type": "Point",
                    "coordinates": [loc.longitude, loc.latitude]
                },
                "properties": {
                    "name": loc.label
                }
            })
        })
        .collect();

    json!({
        "type": "FeatureCollection",
        "features": features
    })
}
