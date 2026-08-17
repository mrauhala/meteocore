use std::collections::HashMap;

use ds_core::model::{CoverageResponse, DomainDescription, Location, QueryResult, VerticalCoord};
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

/// CoverageJSON `referencing` entry for a vertical (`z`) coordinate. The
/// vertical CRS is described by its coordinate-system axis (name,
/// direction, unit) rather than an identifier — vertical coordinate
/// kinds like radar elevation angle have no standard CRS URI.
fn vertical_ref(z: &VerticalCoord) -> Value {
    json!({
        "coordinates": ["z"],
        "system": {
            "type": "VerticalCRS",
            "cs": {
                "csAxes": [{
                    "name": { "en": z.kind.default_label() },
                    "direction": z.kind.direction(),
                    "unit": { "symbol": z.kind.default_unit() }
                }]
            }
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
            // `Number::from_f64` returns `None` for NaN / ±inf (JSON has
            // no representation), so a non-finite measurement must encode
            // as `null` — not `0`, which would be indistinguishable from a
            // genuine zero reading. Flagged by claude-review on PR #275.
            Some(f) => Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            None => Value::Null,
        })
        .collect();

    let mut obj = Map::with_capacity(5);
    obj.insert("type".into(), Value::String("NdArray".into()));
    obj.insert("dataType".into(), Value::String("float".into()));
    // A 0-d scalar range (a `Point` coverage's single value) omits
    // `axisNames`/`shape` per the CoverageJSON spec; any dimensioned
    // array keeps both.
    if !(ndarray.axis_names.is_empty() && ndarray.shape.is_empty()) {
        obj.insert("axisNames".into(), json!(ndarray.axis_names));
        obj.insert("shape".into(), json!(ndarray.shape));
    }
    obj.insert("values".into(), Value::Array(values));
    Value::Object(obj)
}

/// Iterate a per-query `HashMap` in sorted key order. This workspace's
/// serde_json is built with `preserve_order` (pulled in by zarrs), so
/// insertion order IS the wire order — and a fresh `HashMap`'s iteration
/// order differs per instance. Without sorting, byte-identical queries would
/// serialize in different key orders and the content-derived ETag would
/// never revalidate (#499).
fn sorted<V>(map: &HashMap<String, V>) -> Vec<(&String, &V)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by_key(|(name, _)| *name);
    entries
}

fn build_parameters(result: &QueryResult) -> Map<String, Value> {
    let mut parameters = Map::with_capacity(result.parameters.len());
    for (name, desc) in sorted(&result.parameters) {
        parameters.insert(
            name.clone(),
            build_parameter(&desc.label, &desc.unit, &desc.observed_property),
        );
    }
    parameters
}

fn build_ranges(result: &QueryResult) -> Map<String, Value> {
    let mut ranges = Map::with_capacity(result.ranges.len());
    for (name, ndarray) in sorted(&result.ranges) {
        ranges.insert(name.clone(), build_ndarray(ndarray));
    }
    ranges
}

pub fn query_result_to_coverage_json(result: &QueryResult) -> Value {
    let mut coverage = Map::with_capacity(4);
    coverage.insert("type".into(), Value::String("Coverage".into()));
    coverage.insert("domain".into(), build_domain(&result.domain));
    coverage.insert("parameters".into(), Value::Object(build_parameters(result)));
    coverage.insert("ranges".into(), Value::Object(build_ranges(result)));
    Value::Object(coverage)
}

/// CoverageJSON `domainType` string for a domain description.
fn domain_type_name(domain: &DomainDescription) -> &'static str {
    match domain {
        DomainDescription::Point { .. } => "Point",
        DomainDescription::PointSeries { .. } => "PointSeries",
        DomainDescription::Grid { .. } => "Grid",
        DomainDescription::VerticalProfile { .. } => "VerticalProfile",
        DomainDescription::Section { .. } => "Section",
    }
}

/// Serialise an EDR query result — a single `Coverage` or, for multiple
/// coverages, a `CoverageCollection`.
pub fn coverage_response_to_json(result: &CoverageResponse) -> Value {
    match result {
        CoverageResponse::Single(qr) => query_result_to_coverage_json(qr),
        CoverageResponse::Collection(coverages) => {
            if coverages.is_empty() {
                return json!({
                    "type": "CoverageCollection",
                    "coverages": []
                });
            }

            // Hoist parameters to collection level — the union across
            // every coverage, so a (hypothetical) mixed-parameter
            // collection still advertises them all rather than only the
            // first coverage's. Each coverage's domain keeps its own
            // `referencing`, so a collection may mix domain shapes safely.
            let mut parameters = Map::new();
            for qr in coverages {
                parameters.append(&mut build_parameters(qr));
            }

            // A collection-level `domainType` is only emitted when every
            // coverage agrees (it is an optional hint). A heterogeneous
            // collection omits it rather than emitting a type that
            // mismatches some coverages — each coverage's domain still
            // carries its own `domainType`.
            let first_type = domain_type_name(&coverages[0].domain);
            let homogeneous = coverages
                .iter()
                .all(|c| domain_type_name(&c.domain) == first_type);

            let coverage_items: Vec<Value> = coverages
                .iter()
                .map(|qr| {
                    let mut cov = Map::with_capacity(3);
                    cov.insert("type".into(), Value::String("Coverage".into()));
                    cov.insert("domain".into(), build_domain(&qr.domain));
                    cov.insert("ranges".into(), Value::Object(build_ranges(qr)));
                    Value::Object(cov)
                })
                .collect();

            let mut collection = Map::with_capacity(4);
            collection.insert("type".into(), Value::String("CoverageCollection".into()));
            if homogeneous {
                collection.insert("domainType".into(), Value::String(first_type.into()));
            }
            collection.insert("parameters".into(), Value::Object(parameters));
            collection.insert("coverages".into(), Value::Array(coverage_items));
            Value::Object(collection)
        }
    }
}

fn build_domain(desc: &DomainDescription) -> Value {
    match desc {
        DomainDescription::Point { x, y, t, z } => {
            let mut axes = Map::new();
            axes.insert("x".into(), json!({ "values": [x] }));
            axes.insert("y".into(), json!({ "values": [y] }));
            let mut referencing = vec![spatial_ref()];
            if let Some(time) = t {
                axes.insert("t".into(), json!({ "values": [time.to_rfc3339()] }));
                referencing.push(temporal_ref());
            }
            if let Some(zc) = z {
                axes.insert("z".into(), json!({ "values": zc.values }));
                referencing.push(vertical_ref(zc));
            }
            json!({
                "type": "Domain",
                "domainType": "Point",
                "axes": axes,
                "referencing": referencing
            })
        }
        DomainDescription::PointSeries { x, y, t, z } => {
            let times: Vec<String> = t.iter().map(|t| t.to_rfc3339()).collect();
            let mut axes = Map::new();
            axes.insert("x".into(), json!({ "values": [x] }));
            axes.insert("y".into(), json!({ "values": [y] }));
            axes.insert("t".into(), json!({ "values": times }));
            let mut referencing = vec![spatial_ref(), temporal_ref()];
            if let Some(zc) = z {
                axes.insert("z".into(), json!({ "values": zc.values }));
                referencing.push(vertical_ref(zc));
            }
            json!({
                "type": "Domain",
                "domainType": "PointSeries",
                "axes": axes,
                "referencing": referencing
            })
        }
        DomainDescription::Grid { x, y, t, z } => {
            let mut axes = Map::new();
            axes.insert("x".into(), json!({ "values": x }));
            axes.insert("y".into(), json!({ "values": y }));

            let mut referencing = vec![spatial_ref()];

            if let Some(times) = t {
                let time_strings: Vec<String> = times.iter().map(|t| t.to_rfc3339()).collect();
                axes.insert("t".into(), json!({ "values": time_strings }));
                referencing.push(temporal_ref());
            }
            if let Some(zc) = z {
                axes.insert("z".into(), json!({ "values": zc.values }));
                referencing.push(vertical_ref(zc));
            }

            json!({
                "type": "Domain",
                "domainType": "Grid",
                "axes": axes,
                "referencing": referencing
            })
        }
        DomainDescription::VerticalProfile { x, y, t, z } => {
            let mut axes = Map::new();
            axes.insert("x".into(), json!({ "values": [x] }));
            axes.insert("y".into(), json!({ "values": [y] }));
            axes.insert("z".into(), json!({ "values": z.values }));
            let mut referencing = vec![spatial_ref(), vertical_ref(z)];
            if let Some(time) = t {
                axes.insert("t".into(), json!({ "values": [time.to_rfc3339()] }));
                referencing.push(temporal_ref());
            }
            json!({
                "type": "Domain",
                "domainType": "VerticalProfile",
                "axes": axes,
                "referencing": referencing
            })
        }
        DomainDescription::Section {
            nodes,
            z,
            coverage_floor,
        } => {
            // Each composite-axis entry is a 3-tuple `[t, x, y]`, exactly
            // matching the CoverageJSON 1.0 `Section` schema (the only
            // tuple shape it accepts).
            let tuples: Vec<Value> = nodes
                .iter()
                .map(|(t, lon, lat)| json!([t.to_rfc3339(), lon, lat]))
                .collect();
            let mut axes = Map::new();
            axes.insert(
                "composite".into(),
                json!({
                    "dataType": "tuple",
                    "coordinates": ["t", "x", "y"],
                    "values": tuples,
                }),
            );
            axes.insert("z".into(), json!({ "values": z.values }));
            let referencing = vec![spatial_ref(), temporal_ref(), vertical_ref(z)];
            let mut domain = Map::new();
            domain.insert("type".into(), Value::String("Domain".into()));
            domain.insert("domainType".into(), Value::String("Section".into()));
            domain.insert("axes".into(), Value::Object(axes));
            domain.insert("referencing".into(), json!(referencing));
            // Lowest-beam coverage floor (#514) as a CoverageJSON foreign
            // member — one value per composite-axis node, in the z axis's
            // unit (metres above antenna). A foreign member (not an axis
            // or a parameter) because the schema forbids extra axes,
            // and a derived-range encoding would surface in the parameter
            // list where naive clients plot it as data. Raw values: they
            // may dip below 0 near the radar or exceed the z-axis top.
            if let Some(floor) = coverage_floor {
                domain.insert("meteocore:beamCoverage".into(), json!({ "floor": floor }));
            }
            Value::Object(domain)
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
