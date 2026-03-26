use ds_core::model::{AreaQueryResult, DomainDescription, Location, QueryResult};
use serde_json::{json, Value};

pub fn query_result_to_coverage_json(result: &QueryResult) -> Value {
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

    let domain = build_domain(&result.domain);

    json!({
        "type": "Coverage",
        "domain": domain,
        "parameters": parameters,
        "ranges": ranges
    })
}

pub fn area_query_result_to_json(result: &AreaQueryResult) -> Value {
    match result {
        AreaQueryResult::Single(qr) => query_result_to_coverage_json(qr),
        AreaQueryResult::Collection(coverages) => {
            if coverages.is_empty() {
                return json!({
                    "type": "CoverageCollection",
                    "coverages": []
                });
            }

            // Hoist shared parameters and referencing to collection level
            let first = &coverages[0];
            let mut parameters = serde_json::Map::new();
            for (name, desc) in &first.parameters {
                parameters.insert(
                    name.clone(),
                    json!({
                        "type": "Parameter",
                        "description": { "en": desc.label },
                        "unit": {
                            "label": { "en": desc.unit },
                            "symbol": {
                                "value": desc.unit,
                                "type": "http://www.opengis.net/def/uom/UCUM/"
                            }
                        },
                        "observedProperty": {
                            "id": desc.observed_property,
                            "label": { "en": desc.label }
                        }
                    }),
                );
            }

            let spatial_ref = json!({
                "coordinates": ["x", "y"],
                "system": {
                    "type": "GeographicCRS",
                    "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
                }
            });
            let temporal_ref = json!({
                "coordinates": ["t"],
                "system": {
                    "type": "TemporalRS",
                    "calendar": "Gregorian"
                }
            });

            let coverage_items: Vec<Value> = coverages
                .iter()
                .map(|qr| {
                    let domain = build_domain(&qr.domain);

                    let mut ranges = serde_json::Map::new();
                    for (name, ndarray) in &qr.ranges {
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
                        "domain": domain,
                        "ranges": ranges
                    })
                })
                .collect();

            json!({
                "type": "CoverageCollection",
                "domainType": "PointSeries",
                "parameters": parameters,
                "referencing": [spatial_ref, temporal_ref],
                "coverages": coverage_items
            })
        }
    }
}

fn build_domain(desc: &DomainDescription) -> Value {
    let spatial_ref = json!({
        "coordinates": ["x", "y"],
        "system": {
            "type": "GeographicCRS",
            "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        }
    });
    let temporal_ref = json!({
        "coordinates": ["t"],
        "system": {
            "type": "TemporalRS",
            "calendar": "Gregorian"
        }
    });

    match desc {
        DomainDescription::PointSeries { x, y, t } => {
            let times: Vec<String> = t.iter().map(|t| t.to_rfc3339()).collect();
            json!({
                "type": "Domain",
                "domainType": "PointSeries",
                "axes": {
                    "x": { "values": [x] },
                    "y": { "values": [y] },
                    "t": { "values": times }
                },
                "referencing": [spatial_ref, temporal_ref]
            })
        }
        DomainDescription::Grid { x, y, t } => {
            let mut axes = serde_json::Map::new();
            axes.insert("x".to_string(), json!({ "values": x }));
            axes.insert("y".to_string(), json!({ "values": y }));

            let mut referencing = vec![spatial_ref];

            if let Some(times) = t {
                let time_strings: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
                axes.insert("t".to_string(), json!({ "values": time_strings }));
                referencing.push(temporal_ref);
            }

            json!({
                "type": "Domain",
                "domainType": "Grid",
                "axes": axes,
                "referencing": referencing
            })
        }
    }
}

/// Metadata needed for building EDR location features.
pub struct LocationsContext<'a> {
    pub collection_id: &'a str,
    pub parameter_names: &'a [String],
    pub temporal_extent: Option<(String, String)>,
    pub base_url: &'a str,
}

pub fn locations_to_geojson(locations: &[Location], ctx: &LocationsContext) -> Value {
    let datetime = ctx
        .temporal_extent
        .as_ref()
        .map(|(start, end)| format!("{start}/{end}"))
        .unwrap_or_default();
    let base = ctx.base_url;

    let features: Vec<Value> = locations
        .iter()
        .map(|loc| {
            let edr_endpoint = format!(
                "{base}/edr/collections/{}/locations/{}",
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
                "href": format!("{base}/edr/collections/{}/locations", ctx.collection_id),
                "rel": "self",
                "type": "application/geo+json",
                "title": "Locations"
            }
        ]
    })
}
