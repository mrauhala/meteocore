use ds_core::model::{AreaQueryResult, DomainDescription, Location, QueryResult};
use serde_json::{json, Map, Number, Value};

/// Pre-built reference system objects (shared across all responses).
fn spatial_ref() -> Value {
    json!({
        "coordinates": ["x", "y"],
        "system": {
            "type": "GeographicCRS",
            "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"
        }
    })
}

fn temporal_ref() -> Value {
    json!({
        "coordinates": ["t"],
        "system": {
            "type": "TemporalRS",
            "calendar": "Gregorian"
        }
    })
}

fn build_parameter(label: &str, unit: &str, observed_property: &str) -> Value {
    let mut param = Map::with_capacity(4);
    param.insert("type".into(), Value::String("Parameter".into()));
    param.insert(
        "description".into(),
        Value::Object({
            let mut m = Map::with_capacity(1);
            m.insert("en".into(), Value::String(label.into()));
            m
        }),
    );
    param.insert(
        "unit".into(),
        Value::Object({
            let mut m = Map::with_capacity(2);
            m.insert(
                "label".into(),
                Value::Object({
                    let mut lm = Map::with_capacity(1);
                    lm.insert("en".into(), Value::String(unit.into()));
                    lm
                }),
            );
            m.insert(
                "symbol".into(),
                Value::Object({
                    let mut sm = Map::with_capacity(2);
                    sm.insert("value".into(), Value::String(unit.into()));
                    sm.insert(
                        "type".into(),
                        Value::String("http://www.opengis.net/def/uom/UCUM/".into()),
                    );
                    sm
                }),
            );
            m
        }),
    );
    param.insert(
        "observedProperty".into(),
        Value::Object({
            let mut m = Map::with_capacity(2);
            m.insert("id".into(), Value::String(observed_property.into()));
            m.insert(
                "label".into(),
                Value::Object({
                    let mut lm = Map::with_capacity(1);
                    lm.insert("en".into(), Value::String(label.into()));
                    lm
                }),
            );
            m
        }),
    );
    Value::Object(param)
}

fn build_ndarray(ndarray: &ds_core::model::NdArray) -> Value {
    let values: Vec<Value> = ndarray
        .values
        .iter()
        .map(|v| match v {
            Some(f) => Value::Number(Number::from_f64(*f).unwrap_or(Number::from(0))),
            None => Value::Null,
        })
        .collect();

    let mut obj = Map::with_capacity(5);
    obj.insert("type".into(), Value::String("NdArray".into()));
    obj.insert("dataType".into(), Value::String("float".into()));
    obj.insert("axisNames".into(), json!(ndarray.axis_names));
    obj.insert("shape".into(), json!(ndarray.shape));
    obj.insert("values".into(), Value::Array(values));
    Value::Object(obj)
}

pub fn query_result_to_coverage_json(result: &QueryResult) -> Value {
    let mut parameters = Map::with_capacity(result.parameters.len());
    for (name, desc) in &result.parameters {
        parameters.insert(
            name.clone(),
            build_parameter(&desc.label, &desc.unit, &desc.observed_property),
        );
    }

    let mut ranges = Map::with_capacity(result.ranges.len());
    for (name, ndarray) in &result.ranges {
        ranges.insert(name.clone(), build_ndarray(ndarray));
    }

    let domain = build_domain(&result.domain);

    let mut coverage = Map::with_capacity(4);
    coverage.insert("type".into(), Value::String("Coverage".into()));
    coverage.insert("domain".into(), domain);
    coverage.insert("parameters".into(), Value::Object(parameters));
    coverage.insert("ranges".into(), Value::Object(ranges));
    Value::Object(coverage)
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
            let mut parameters = Map::with_capacity(first.parameters.len());
            for (name, desc) in &first.parameters {
                parameters.insert(
                    name.clone(),
                    build_parameter(&desc.label, &desc.unit, &desc.observed_property),
                );
            }

            let coverage_items: Vec<Value> = coverages
                .iter()
                .map(|qr| {
                    let domain = build_domain(&qr.domain);

                    let mut ranges = Map::with_capacity(qr.ranges.len());
                    for (name, ndarray) in &qr.ranges {
                        ranges.insert(name.clone(), build_ndarray(ndarray));
                    }

                    let mut cov = Map::with_capacity(3);
                    cov.insert("type".into(), Value::String("Coverage".into()));
                    cov.insert("domain".into(), domain);
                    cov.insert("ranges".into(), Value::Object(ranges));
                    Value::Object(cov)
                })
                .collect();

            json!({
                "type": "CoverageCollection",
                "domainType": "PointSeries",
                "parameters": parameters,
                "referencing": [spatial_ref(), temporal_ref()],
                "coverages": coverage_items
            })
        }
    }
}

fn build_domain(desc: &DomainDescription) -> Value {
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
                "referencing": [spatial_ref(), temporal_ref()]
            })
        }
        DomainDescription::Grid { x, y, t } => {
            let mut axes = Map::new();
            axes.insert("x".into(), json!({ "values": x }));
            axes.insert("y".into(), json!({ "values": y }));

            let mut referencing = vec![spatial_ref()];

            if let Some(times) = t {
                let time_strings: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
                axes.insert("t".into(), json!({ "values": time_strings }));
                referencing.push(temporal_ref());
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
