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

/// Metadata needed for building EDR location features.
pub struct LocationsContext<'a> {
    pub collection_id: &'a str,
    pub parameter_names: &'a [String],
    pub temporal_extent: Option<(String, String)>,
}

pub fn locations_to_geojson(locations: &[Location], ctx: &LocationsContext) -> Value {
    let datetime = ctx
        .temporal_extent
        .as_ref()
        .map(|(start, end)| format!("{start}/{end}"))
        .unwrap_or_default();

    let features: Vec<Value> = locations
        .iter()
        .map(|loc| {
            let edr_endpoint = format!(
                "/edr/collections/{}/locations/{}",
                ctx.collection_id, loc.id
            );
            json!({
                "type": "Feature",
                "id": loc.id,
                "geometry": {
                    "type": "Point",
                    "coordinates": [loc.longitude, loc.latitude]
                },
                "properties": {
                    "label": loc.label,
                    "datetime": datetime,
                    "parameter-name": ctx.parameter_names,
                    "edrqueryendpoint": edr_endpoint
                },
                "links": [
                    {
                        "href": edr_endpoint,
                        "rel": "data",
                        "type": "application/prs.coverage+json",
                        "title": format!("Data for {}", loc.label)
                    }
                ]
            })
        })
        .collect();

    json!({
        "type": "FeatureCollection",
        "features": features,
        "links": [
            {
                "href": format!("/edr/collections/{}/locations", ctx.collection_id),
                "rel": "self",
                "type": "application/geo+json",
                "title": "Locations"
            }
        ]
    })
}
