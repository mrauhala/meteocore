use std::borrow::Cow;
use std::collections::HashMap;

use ds_core::model::{
    CoverageResponse, DomainDescription, Location, NdArray, ParameterDescription, QueryResult,
    RangeValue, VerticalCoord,
};
use ds_core::units::qudt_unit;
use serde::ser::{SerializeMap, Serializer};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// The media type every CoverageJSON response carries (#920): the
/// registered CoverageJSON type, which OGC API - EDR 1.2 uses throughout.
/// The one home for it: the data-query `Content-Type`, the `/locations`
/// data links and the `/api` response content keys all read this constant.
pub const COVERAGE_JSON_MEDIA_TYPE: &str = "application/vnd.cov+json";

/// EDR 1.1's CoverageJSON media type. No longer sent; still accepted as an
/// `f` value ([`crate::params::parse_edr_format`]) so 1.1 clients keep working.
pub const LEGACY_COVERAGE_JSON_MEDIA_TYPE: &str = "application/prs.coverage+json";

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

/// Longest parameter `label` the OGC API - EDR Metocean Profile allows
/// (Requirement 7C).
pub const MAX_PARAMETER_LABEL_CHARS: usize = 50;

/// NERC Vocabulary Server namespace of the CF standard names: the
/// `observedProperty.id` form of Metocean Profile Requirement 7F.
pub const CF_STANDARD_NAME_BASE: &str = "https://vocab.nerc.ac.uk/standard_name/";

/// The parameter `label`: the engine's label, cut to at most
/// [`MAX_PARAMETER_LABEL_CHARS`] characters (ellipsis included). The full
/// text stays in the description and `observedProperty.label`.
fn parameter_label(desc: &ParameterDescription) -> Cow<'_, str> {
    let label = desc.label.trim();
    if label.chars().count() <= MAX_PARAMETER_LABEL_CHARS {
        return Cow::Borrowed(label);
    }
    let head: String = label.chars().take(MAX_PARAMETER_LABEL_CHARS - 1).collect();
    Cow::Owned(format!("{}…", head.trim_end()))
}

/// The parameter `description`: the engine's full label plus the unit it is
/// served in, so it says more than `label` (Metocean Requirement 7B).
fn parameter_description(desc: &ParameterDescription) -> String {
    let label = desc.label.trim();
    let unit = desc.unit.trim();
    if unit.is_empty() {
        return format!("{label} (unit not specified)");
    }
    let symbol = qudt_unit(unit).map_or(unit, |q| q.symbol);
    format!("{label}, in {symbol}")
}

/// `observedProperty.id` of a parameter whose engine knows its CF standard
/// name. A value that is not a bare CF name (a CF modifier such as
/// `… standard_error`, anything needing URL escaping) is not published.
fn cf_standard_name_uri(desc: &ParameterDescription) -> Option<String> {
    let name = desc.standard_name.as_deref()?.trim();
    let bare = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    bare.then(|| format!("{CF_STANDARD_NAME_BASE}{name}"))
}

fn i18n(text: &str) -> Value {
    let mut m = Map::with_capacity(1);
    m.insert("en".into(), Value::String(text.into()));
    Value::Object(m)
}

/// `observedProperty`: identified by the CF standard name URI when the
/// engine knows one, else by the engine's observed-property name and
/// described by `description` (Metocean Requirement 7F). EDR 1.2
/// `/req/edr/rc-parameters` F requires the `id` either way.
fn build_observed_property(desc: &ParameterDescription, description: Value) -> Value {
    let mut m = Map::with_capacity(3);
    let cf_id = cf_standard_name_uri(desc);
    let id = cf_id.as_deref().unwrap_or(&desc.observed_property);
    m.insert("id".into(), Value::String(id.into()));
    m.insert("label".into(), i18n(&desc.label));
    if cf_id.is_none() {
        m.insert("description".into(), description);
    }
    Value::Object(m)
}

/// One entry of a collection's `parameter_names` (an EDR 1.1 parameter
/// object: plain-string `label` and `description`). The coverage
/// `parameters` of a data query say the same things (#273), the same
/// `observedProperty.id` included.
pub fn collection_parameter_json(desc: &ParameterDescription) -> Value {
    let description = parameter_description(desc);
    let observed = build_observed_property(desc, Value::String(description.clone()));
    let mut param = Map::with_capacity(6);
    param.insert("type".into(), Value::String("Parameter".into()));
    param.insert(
        "label".into(),
        Value::String(parameter_label(desc).into_owned()),
    );
    param.insert("description".into(), Value::String(description));
    if let Some(unit) = api_common::unit_json(&desc.unit) {
        param.insert("unit".into(), unit);
    }
    param.insert("observedProperty".into(), observed);
    Value::Object(param)
}

/// A CoverageJSON parameter. Its short name is `observedProperty.label`:
/// CoverageJSON asks to leave out a parameter `label` identical to it. The
/// EDR GeoJSON `parameters` member lists the same objects (#929).
pub(crate) fn build_parameter(desc: &ParameterDescription) -> Value {
    let description = parameter_description(desc);
    let mut param = Map::with_capacity(4);
    param.insert("type".into(), Value::String("Parameter".into()));
    param.insert("description".into(), i18n(&description));
    if let Some(unit) = api_common::unit_json(&desc.unit) {
        param.insert("unit".into(), unit);
    }
    param.insert(
        "observedProperty".into(),
        build_observed_property(desc, i18n(&description)),
    );
    Value::Object(param)
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
        parameters.insert(name.clone(), build_parameter(desc));
    }
    parameters
}

/// A coverage of a CoverageJSON document; `parameters` is false inside a
/// CoverageCollection, which hoists them.
struct Coverage<'a> {
    result: &'a QueryResult,
    parameters: bool,
}

impl Serialize for Coverage<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut coverage = serializer.serialize_map(None)?;
        coverage.serialize_entry("type", "Coverage")?;
        coverage.serialize_entry("domain", &build_domain(&self.result.domain))?;
        if self.parameters {
            coverage.serialize_entry("parameters", &build_parameters(self.result))?;
        }
        coverage.serialize_entry("ranges", &Ranges(self.result))?;
        coverage.end()
    }
}

/// A coverage's `ranges`, by parameter name.
struct Ranges<'a>(&'a QueryResult);

impl Serialize for Ranges<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            sorted(&self.0.ranges)
                .into_iter()
                .map(|(name, ndarray)| (name, NdArrayJson(ndarray))),
        )
    }
}

struct NdArrayJson<'a>(&'a NdArray);

impl Serialize for NdArrayJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ndarray = self.0;
        let mut obj = serializer.serialize_map(None)?;
        obj.serialize_entry("type", "NdArray")?;
        // `float` whatever the precision: CoverageJSON's one type for both.
        obj.serialize_entry("dataType", "float")?;
        // A 0-d scalar range (a `Point` coverage's single value) omits
        // `axisNames`/`shape` per the CoverageJSON spec; any dimensioned
        // array keeps both.
        if !(ndarray.axis_names.is_empty() && ndarray.shape.is_empty()) {
            obj.serialize_entry("axisNames", &ndarray.axis_names)?;
            obj.serialize_entry("shape", &ndarray.shape)?;
        }
        obj.serialize_entry("values", &Values(ndarray))?;
        obj.end()
    }
}

/// A range's values, written one by one from the engine's result at the
/// precision they carry (`ds_core::model::RangeValue`, #1341), as WMS
/// GetFeatureInfo writes them. A non-finite value is `null`, never `0`,
/// which would read as a genuine zero (PR #275). EDR GeoJSON writes its
/// series through it too.
pub(crate) struct Values<'a>(pub &'a NdArray);

impl Serialize for Values<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let precision = self.0.precision;
        serializer.collect_seq(
            self.0
                .values
                .iter()
                .map(|&value| RangeValue::new(value, precision)),
        )
    }
}

/// A data query's CoverageJSON document, serialised straight into the
/// response body (#1041): the range values are written from the engine's
/// result, never copied into a `Value` tree first. Every object keeps the
/// member order the document has always had, so with serde_json's
/// `preserve_order`, which the server build enables, the body is the one
/// the `Value` tree serialised to. The domain and the parameters, small
/// beside the ranges, are still built as `Value`s.
pub struct CoverageJson<'a>(pub &'a CoverageResponse);

impl Serialize for CoverageJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let coverages = match self.0 {
            CoverageResponse::Single(result) => {
                return Coverage {
                    result,
                    parameters: true,
                }
                .serialize(serializer)
            }
            CoverageResponse::Collection(coverages) => coverages,
        };
        let mut collection = serializer.serialize_map(None)?;
        collection.serialize_entry("type", "CoverageCollection")?;
        if coverages.is_empty() {
            collection.serialize_entry("coverages", &[] as &[Value])?;
            return collection.end();
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
        if homogeneous {
            collection.serialize_entry("domainType", first_type)?;
        }
        collection.serialize_entry("parameters", &parameters)?;
        collection.serialize_entry("coverages", &Coverages(coverages))?;
        collection.end()
    }
}

/// A CoverageCollection's coverages, without their hoisted parameters.
struct Coverages<'a>(&'a [QueryResult]);

impl Serialize for Coverages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(|result| Coverage {
            result,
            parameters: false,
        }))
    }
}

/// A CoverageJSON document as a `Value`: what the HTML page lays out and
/// tests inspect. The response body ([`CoverageJson`]) parsed back, so the
/// two cannot drift: an `f32` range value is the number its shortest text
/// reads as, `10.531`, where `serde_json::to_value` would keep the `f32`
/// widened to `10.531000137329102` (#1341).
fn to_value(document: impl Serialize) -> Value {
    let body = serde_json::to_vec(&document).expect("a CoverageJSON document has only string keys");
    serde_json::from_slice(&body).expect("a serialised CoverageJSON document parses")
}

pub fn query_result_to_coverage_json(result: &QueryResult) -> Value {
    to_value(Coverage {
        result,
        parameters: true,
    })
}

/// CoverageJSON `domainType` string for a domain description.
fn domain_type_name(domain: &DomainDescription) -> &'static str {
    match domain {
        DomainDescription::Point { .. } => "Point",
        DomainDescription::PointSeries { .. } => "PointSeries",
        DomainDescription::Grid { .. } => "Grid",
        DomainDescription::VerticalProfile { .. } => "VerticalProfile",
        DomainDescription::Section { .. } => "Section",
        DomainDescription::Trajectory { .. } => "Trajectory",
    }
}

/// Serialise an EDR query result — a single `Coverage` or, for multiple
/// coverages, a `CoverageCollection` — as a `Value` (see [`to_value`]).
pub fn coverage_response_to_json(result: &CoverageResponse) -> Value {
    to_value(CoverageJson(result))
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
        DomainDescription::Trajectory { nodes, node_z, z } => {
            // The CoverageJSON 1.0 `Trajectory` domain: a composite axis of
            // `[t, x, y]` tuples, or `[t, x, y, z]` when every node has its
            // own level; a level the whole path shares is a single-valued
            // `z` axis beside it (the only other axis the schema allows).
            let mut referencing = vec![spatial_ref(), temporal_ref()];
            let (coordinates, tuples): (Value, Vec<Value>) = match node_z {
                Some(levels) => {
                    referencing.push(vertical_ref(levels));
                    (
                        json!(["t", "x", "y", "z"]),
                        nodes
                            .iter()
                            .zip(&levels.values)
                            .map(|((t, x, y), z)| json!([t.to_rfc3339(), x, y, z]))
                            .collect(),
                    )
                }
                None => (
                    json!(["t", "x", "y"]),
                    nodes
                        .iter()
                        .map(|(t, x, y)| json!([t.to_rfc3339(), x, y]))
                        .collect(),
                ),
            };
            let mut axes = Map::new();
            axes.insert(
                "composite".into(),
                json!({
                    "dataType": "tuple",
                    "coordinates": coordinates,
                    "values": tuples,
                }),
            );
            if let Some(zc) = z {
                axes.insert("z".into(), json!({ "values": zc.values }));
                referencing.push(vertical_ref(zc));
            }
            json!({
                "type": "Domain",
                "domainType": "Trajectory",
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

/// The links of a `/locations` list that is not the plain complete
/// inventory: one page of a list requested with `limit` (EDR 1.2, #922), or
/// a list filtered by `bbox` or `datetime` (#932).
pub struct LocationsPage<'a> {
    /// A page's `numberMatched`: the size of the (filtered) list it was cut
    /// from. `None` for an unpaged list, which carries no counts.
    pub number_matched: Option<usize>,
    /// `self`, then `next`/`prev` when they exist: `(href, rel, title)`.
    pub links: &'a [(String, &'static str, &'static str)],
}

/// The locations a `/locations` response lists, borrowed from the engine's
/// [`LocationSnapshot`](ds_core::model::LocationSnapshot) (#1083): a run of
/// the inventory (all of it, or a page), or the locations a `bbox` or
/// `datetime` filter kept (all of them, or a page). Never a copy.
#[derive(Clone, Copy)]
pub(crate) enum LocationRows<'a> {
    /// Consecutive locations of the inventory, in its order.
    Run(&'a [Location]),
    /// The locations a filter kept, in inventory order.
    Kept(&'a [&'a Location]),
}

impl<'a> LocationRows<'a> {
    pub(crate) fn len(self) -> usize {
        match self {
            Self::Run(run) => run.len(),
            Self::Kept(kept) => kept.len(),
        }
    }

    /// The rows in `range`: one page.
    pub(crate) fn page(self, range: std::ops::Range<usize>) -> Self {
        match self {
            Self::Run(run) => Self::Run(&run[range]),
            Self::Kept(kept) => Self::Kept(&kept[range]),
        }
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = &'a Location> {
        let (run, kept): (&[Location], &[&Location]) = match self {
            Self::Run(run) => (run, &[]),
            Self::Kept(kept) => (&[], kept),
        };
        run.iter().chain(kept.iter().copied())
    }
}

/// Serialize the complete EDR location inventory directly into response
/// bytes. Only one feature's links are allocated at a time; parameter and
/// temporal metadata are borrowed instead of cloned into a full JSON tree.
pub fn locations_to_json(
    locations: &[Location],
    ctx: &LocationsContext,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = Vec::new();
    locations_to_writer(LocationRows::Run(locations), ctx, None, &mut bytes)?;
    Ok(bytes)
}

/// Serialize into an admitted writer so size/deadline/memory failures stop
/// construction before an unbounded response buffer has been allocated.
/// `page` is `None` for the complete inventory, whose body stays exactly the
/// pre-paging one (no counts, one `self` link); otherwise its links replace
/// that `self` link, and a page adds `numberMatched` and `numberReturned`.
pub(crate) fn locations_to_writer(
    locations: LocationRows<'_>,
    ctx: &LocationsContext,
    page: Option<&LocationsPage>,
    writer: impl std::io::Write,
) -> Result<(), serde_json::Error> {
    let datetime = ctx
        .temporal_extent
        .as_ref()
        .map(|(start, end)| format!("{start}/{end}"))
        .unwrap_or_default();
    let href = format!(
        "{}/edr/collections/{}/locations",
        ctx.base_url, ctx.collection_id
    );
    let links: Vec<LocationLink> = match page {
        None => vec![LocationLink {
            href: &href,
            rel: "self",
            title: "Locations",
            kind: "application/geo+json",
        }],
        Some(page) => page
            .links
            .iter()
            .map(|(href, rel, title)| LocationLink {
                href,
                rel,
                title,
                kind: "application/geo+json",
            })
            .collect(),
    };
    serde_json::to_writer(
        writer,
        &LocationCollection {
            features: LocationFeatures {
                locations,
                ctx,
                datetime: &datetime,
            },
            links,
            number_matched: page.and_then(|p| p.number_matched),
            number_returned: page.and_then(|p| p.number_matched).map(|_| locations.len()),
            kind: "FeatureCollection",
        },
    )
}

#[derive(serde::Serialize)]
struct LocationCollection<'a> {
    features: LocationFeatures<'a>,
    links: Vec<LocationLink<'a>>,
    #[serde(rename = "numberMatched", skip_serializing_if = "Option::is_none")]
    number_matched: Option<usize>,
    #[serde(rename = "numberReturned", skip_serializing_if = "Option::is_none")]
    number_returned: Option<usize>,
    #[serde(rename = "type")]
    kind: &'static str,
}

struct LocationFeatures<'a> {
    locations: LocationRows<'a>,
    ctx: &'a LocationsContext<'a>,
    datetime: &'a str,
}

// Struct fields give the response a stable order independent of serde_json's
// preserve_order feature. The GeoJSON content is unchanged; existing ETags may
// change once when upgrading from the previous Value-based serializer.
#[derive(serde::Serialize)]
struct LocationFeature<'a> {
    geometry: LocationGeometry,
    id: &'a str,
    links: [LocationLink<'a>; 1],
    properties: LocationProperties<'a>,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(serde::Serialize)]
struct LocationGeometry {
    coordinates: [f64; 2],
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(serde::Serialize)]
struct LocationProperties<'a> {
    datetime: &'a str,
    edrqueryendpoint: &'a str,
    label: &'a str,
    #[serde(rename = "parameter-name")]
    parameter_names: &'a [String],
}

#[derive(serde::Serialize)]
struct LocationLink<'a> {
    href: &'a str,
    rel: &'a str,
    title: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

impl serde::Serialize for LocationFeatures<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.locations.len()))?;
        for loc in self.locations.iter() {
            // Ids are free text (CSV station names with spaces and
            // non-ASCII letters): encoded, the link is a valid RFC 3986 URI.
            let endpoint = format!(
                "{}/edr/collections/{}/locations/{}",
                self.ctx.base_url,
                self.ctx.collection_id,
                crate::geojson::encode_path_segment(&loc.id)
            );
            let title = format!("Data for {}", loc.label);
            sequence.serialize_element(&LocationFeature {
                geometry: LocationGeometry {
                    coordinates: [loc.longitude, loc.latitude],
                    kind: "Point",
                },
                id: &loc.id,
                links: [LocationLink {
                    href: &endpoint,
                    rel: "data",
                    title: &title,
                    kind: COVERAGE_JSON_MEDIA_TYPE,
                }],
                properties: LocationProperties {
                    datetime: self.datetime,
                    edrqueryendpoint: &endpoint,
                    label: &loc.label,
                    parameter_names: self.ctx.parameter_names,
                },
                kind: "Feature",
            })?;
        }
        sequence.end()
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use ds_core::model::ValuePrecision;

    /// The body is written member by member in the document's order, the
    /// order the `Value` tree it replaced serialised to, whatever
    /// serde_json's `preserve_order` (#1041); the `Value` form is the same
    /// document.
    #[test]
    fn the_body_streams_in_the_document_order() {
        let result = CoverageResponse::Single(QueryResult {
            domain: DomainDescription::VerticalProfile {
                x: 24.0,
                y: 60.0,
                t: None,
                z: VerticalCoord {
                    kind: ds_core::vertical::VerticalKind::Height,
                    values: vec![1.0, 2.0, 3.0],
                },
            },
            parameters: HashMap::new(),
            ranges: HashMap::from([(
                "a".to_owned(),
                NdArray {
                    shape: vec![3],
                    axis_names: vec!["z".into()],
                    values: vec![Some(1.5), Some(f64::NAN), None],
                    precision: ValuePrecision::F64,
                },
            )]),
        });
        let body = serde_json::to_string(&CoverageJson(&result)).unwrap();
        assert!(
            body.starts_with(r#"{"type":"Coverage","domain":{"#),
            "{body}"
        );
        assert!(
            body.ends_with(
                r#""parameters":{},"ranges":{"a":{"type":"NdArray","dataType":"float","axisNames":["z"],"shape":[3],"values":[1.5,null,null]}}}"#
            ),
            "{body}"
        );
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, coverage_response_to_json(&result));
        let empty = CoverageResponse::Collection(Vec::new());
        assert_eq!(
            serde_json::to_string(&CoverageJson(&empty)).unwrap(),
            r#"{"type":"CoverageCollection","coverages":[]}"#
        );
    }
}

/// Range values at the precision the source carries (#1341).
#[cfg(test)]
mod precision_tests {
    use super::*;
    use ds_core::model::ValuePrecision;

    fn range(values: Vec<Option<f64>>, precision: ValuePrecision) -> NdArray {
        NdArray {
            shape: vec![values.len()],
            axis_names: vec!["t".into()],
            values,
            precision,
        }
    }

    /// The `values` array as written, one text per value.
    fn texts(range: &NdArray) -> Vec<String> {
        let body = serde_json::to_string(&Values(range)).unwrap();
        let inner = &body[1..body.len() - 1];
        inner.split(',').map(str::to_owned).collect()
    }

    /// The significant digits of a number's text: `-10.5310e0` → `10531`.
    fn digits(text: &str) -> String {
        let mantissa = text
            .trim_start_matches('-')
            .split(['e', 'E'])
            .next()
            .unwrap();
        let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
        digits
            .trim_start_matches('0')
            .trim_end_matches('0')
            .to_owned()
    }

    /// Finite `f32`s across every exponent, both signs and the subnormals:
    /// every 65 521st bit pattern.
    fn f32_samples() -> Vec<f32> {
        (0..=u32::MAX)
            .step_by(65_521)
            .map(f32::from_bits)
            .filter(|v| v.is_finite())
            .collect()
    }

    #[test]
    fn an_f32_value_is_its_shortest_text_and_reads_back_bit_for_bit() {
        let samples = f32_samples();
        assert!(samples.len() > 60_000);
        let values = range(
            samples.iter().map(|&v| Some(f64::from(v))).collect(),
            ValuePrecision::F32,
        );
        let written = texts(&values);
        assert_eq!(written.len(), samples.len());
        for (text, &v) in written.iter().zip(&samples) {
            let back: f32 = text.parse().unwrap();
            assert_eq!(back.to_bits(), v.to_bits(), "{text} is not {v:e}");
            // As few digits as the shortest text that reads back as `v`,
            // Rust's `{:e}` (which may pick the other digit of a tie).
            assert_eq!(
                digits(text).len(),
                digits(&format!("{v:e}")).len(),
                "{text} vs {v:e}"
            );
        }
        // The issue's examples: the f64 widening's digits are gone.
        let issue = range(
            vec![
                Some(f64::from(10.531_f32)),
                Some(f64::from(1.490_413_9_f32)),
            ],
            ValuePrecision::F32,
        );
        assert_eq!(texts(&issue), ["10.531", "1.4904139"]);
    }

    /// A value computed from `f32` samples in `f64`, as a display conversion
    /// (K → °C) does, is written as the `f32` nearest it.
    #[test]
    fn a_value_computed_from_f32_samples_is_written_as_the_nearest_f32() {
        let kelvin = 283.681_f32;
        let celsius = f64::from(kelvin) - 273.15;
        assert_eq!(celsius.to_string(), "10.530999755859398");
        let written = texts(&range(vec![Some(celsius)], ValuePrecision::F32));
        assert_eq!(written, ["10.531"]);
        assert_eq!(written[0].parse::<f32>().unwrap(), celsius as f32);
    }

    /// An `f64` range is written exactly as before #1341: the shortest
    /// text that reads back as the same `f64`, `null` for a missing or
    /// non-finite value.
    #[test]
    fn an_f64_value_is_byte_identical_to_the_old_writer() {
        let values = vec![
            Some(10.530998535156414),
            Some(1.4904138764798391),
            Some(7.0954500000000005),
            Some(f64::from(0.1_f32)),
            Some(-0.0),
            Some(0.0),
            Some(12.0),
            Some(1e300),
            Some(5e-324),
            Some(f64::MAX),
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ];
        // The writer before #1341.
        let before: Vec<Option<f64>> = values.iter().map(|v| v.filter(|f| f.is_finite())).collect();
        let before = serde_json::to_string(&before).unwrap();
        let after = serde_json::to_string(&Values(&range(values, ValuePrecision::F64))).unwrap();
        assert_eq!(after, before);
        assert_eq!(
            after,
            "[10.530998535156414,1.4904138764798391,7.0954500000000005,\
             0.10000000149011612,-0.0,0.0,12.0,1e+300,5e-324,\
             1.7976931348623157e+308,null,null,null,null]"
        );
    }

    /// `f32` cannot hold every `f64`: a value beyond its range, or nonzero
    /// under its smallest step, keeps its `f64` form rather than turning
    /// into infinity (`null`) or zero. Missing and non-finite values are
    /// `null` at either precision.
    #[test]
    fn a_value_f32_cannot_hold_keeps_its_f64_form() {
        let values = vec![
            Some(1e300),
            Some(-1e300),
            Some(1e-300),
            Some(0.0),
            Some(-0.0),
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
        ];
        assert_eq!(
            texts(&range(values, ValuePrecision::F32)),
            ["1e+300", "-1e+300", "1e-300", "0.0", "-0.0", "null", "null", "null"]
        );
    }

    /// The `Value` the HTML page lays out is the body read back: an `f32`
    /// value is the number its text reads as, not the `f32` widened.
    #[test]
    fn the_value_tree_holds_the_numbers_the_body_reads_as() {
        let result = QueryResult {
            domain: DomainDescription::Point {
                x: 24.0,
                y: 60.0,
                z: None,
                t: None,
            },
            parameters: HashMap::new(),
            ranges: HashMap::from([(
                "t2m".to_owned(),
                NdArray {
                    shape: vec![],
                    axis_names: vec![],
                    values: vec![Some(f64::from(10.531_f32))],
                    precision: ValuePrecision::F32,
                },
            )]),
        };
        let value = query_result_to_coverage_json(&result);
        assert_eq!(value["ranges"]["t2m"]["values"][0].as_f64(), Some(10.531));
        let body = serde_json::to_string(&CoverageJson(&CoverageResponse::Single(result))).unwrap();
        assert!(body.contains(r#""values":[10.531]"#), "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), value);
    }

    /// Acceptance of #1341: the body of a 10° × 10° area of a 0.1° GRIB
    /// field shrinks by at least 40 %. The field is decoded as `grib`
    /// decodes simple packing, `(R + X × 2^E) × 10^-D` in `f32`, sampled at
    /// its nodes and converted K → °C in `f64`, as engine-grib serves it;
    /// the axes are the engine's `origin + i × step` coordinates.
    #[test]
    fn a_grib_area_body_shrinks_by_at_least_40_percent() {
        let n = 102;
        let (reference, exponent) = (24_000.0_f32, 2.0_f32.powi(-6));
        let decimal = 10.0_f32.powi(-2);
        let values: Vec<Option<f64>> = (0..n * n)
            .map(|i| {
                let (row, column) = ((i / n) as f32, (i % n) as f32);
                // 0 … 2^15 packed integers: a smooth field with some noise.
                let packed = (row * 40.0 + column * 25.0 + (i % 7) as f32 * 3.0).round();
                let kelvin = (reference + packed * exponent) * decimal;
                Some(f64::from(kelvin) - 273.15)
            })
            .collect();
        let axis = |origin: f64| -> Vec<f64> { (0..n).map(|i| origin + i as f64 * 0.1).collect() };
        let body = |precision| {
            let result = CoverageResponse::Single(QueryResult {
                domain: DomainDescription::Grid {
                    x: axis(19.9),
                    y: axis(54.95),
                    t: None,
                    z: None,
                },
                parameters: HashMap::new(),
                ranges: HashMap::from([(
                    "2t".to_owned(),
                    NdArray {
                        shape: vec![n, n],
                        axis_names: vec!["y".into(), "x".into()],
                        values: values.clone(),
                        precision,
                    },
                )]),
            });
            serde_json::to_vec(&CoverageJson(&result)).unwrap().len()
        };
        let (wide, narrow) = (body(ValuePrecision::F64), body(ValuePrecision::F32));
        let saved = 1.0 - narrow as f64 / wide as f64;
        eprintln!(
            "{} values: {wide} → {narrow} bytes, {:.1} % smaller",
            n * n,
            saved * 100.0
        );
        assert!(
            saved >= 0.40,
            "{wide} → {narrow} bytes: {:.1} %",
            saved * 100.0
        );
    }
}

#[cfg(test)]
mod location_tests {
    use super::*;

    #[test]
    fn direct_serialization_preserves_geojson_content_and_escaping() {
        let label = "Helsinki \"centre\"\n雪";
        let location = Location {
            id: "station-1".into(),
            label: label.into(),
            latitude: 60.0,
            longitude: 24.0,
        };
        let params = vec!["air_temperature".into(), "wind\"speed".into()];
        let ctx = LocationsContext {
            collection_id: "weather",
            parameter_names: &params,
            temporal_extent: Some(("2026-01-01T00:00:00Z".into(), "2026-01-02T00:00:00Z".into())),
            base_url: "https://example.org/prefix",
        };
        let endpoint = "https://example.org/prefix/edr/collections/weather/locations/station-1";
        let expected = json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature", "id": "station-1",
                "geometry": { "type": "Point", "coordinates": [24.0, 60.0] },
                "properties": { "label": label, "datetime": "2026-01-01T00:00:00Z/2026-01-02T00:00:00Z", "parameter-name": params, "edrqueryendpoint": endpoint },
                "links": [{ "href": endpoint, "rel": "data", "type": "application/vnd.cov+json", "title": format!("Data for {label}") }]
            }],
            "links": [{ "href": "https://example.org/prefix/edr/collections/weather/locations", "rel": "self", "type": "application/geo+json", "title": "Locations" }]
        });
        // Workspace dependencies can enable serde_json/preserve_order, which
        // changes Value serialization order without changing the JSON content.
        let actual: Value =
            serde_json::from_slice(&locations_to_json(&[location], &ctx).unwrap()).unwrap();
        assert_eq!(actual, expected);
        let empty: Value = serde_json::from_slice(&locations_to_json(&[], &ctx).unwrap()).unwrap();
        assert_eq!(empty["features"], json!([]));
        assert_eq!(empty["links"], expected["links"]);

        // A page carries its counts and navigation links instead.
        let links = [
            (format!("{endpoint}?limit=1"), "self", "This page"),
            (format!("{endpoint}?limit=1&offset=1"), "next", "Next page"),
        ];
        let mut bytes = Vec::new();
        let page = LocationsPage {
            number_matched: Some(7),
            links: &links,
        };
        locations_to_writer(
            LocationRows::Run(&[Location {
                id: "station-1".into(),
                label: "x".into(),
                latitude: 60.0,
                longitude: 24.0,
            }]),
            &ctx,
            Some(&page),
            &mut bytes,
        )
        .unwrap();
        let paged: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(paged["numberMatched"], 7);
        assert_eq!(paged["numberReturned"], 1);
        assert_eq!(paged["links"][1]["rel"], "next");
        assert_eq!(paged["links"][1]["type"], "application/geo+json");

        // An unpaged filtered list: its own `self` link, no counts.
        let self_href = format!("{endpoint}?bbox=0,0,1,1");
        let links = [(self_href.clone(), "self", "Locations")];
        let filtered = LocationsPage {
            number_matched: None,
            links: &links,
        };
        let mut bytes = Vec::new();
        locations_to_writer(LocationRows::Kept(&[]), &ctx, Some(&filtered), &mut bytes).unwrap();
        let filtered: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(filtered.get("numberMatched").is_none());
        assert!(filtered.get("numberReturned").is_none());
        assert_eq!(
            filtered["links"],
            json!([{ "href": self_href, "rel": "self", "title": "Locations", "type": "application/geo+json" }])
        );
    }

    /// A free-text id (a CSV station name) is percent-encoded in the data
    /// link and `edrqueryendpoint`, so both are valid RFC 3986 URIs; the
    /// feature id and label keep the text.
    #[test]
    fn location_links_percent_encode_the_id() {
        let params = vec!["t".into()];
        let ctx = LocationsContext {
            collection_id: "weather",
            parameter_names: &params,
            temporal_extent: None,
            base_url: "https://example.org",
        };
        let location = Location {
            id: "Alajärvi Möksy".into(),
            label: "Alajärvi Möksy".into(),
            latitude: 63.1,
            longitude: 24.3,
        };
        let doc: Value =
            serde_json::from_slice(&locations_to_json(&[location], &ctx).unwrap()).unwrap();
        let feature = &doc["features"][0];
        let endpoint =
            "https://example.org/edr/collections/weather/locations/Alaj%C3%A4rvi%20M%C3%B6ksy";
        assert_eq!(feature["properties"]["edrqueryendpoint"], endpoint);
        assert_eq!(feature["links"][0]["href"], endpoint);
        assert_eq!(feature["id"], "Alajärvi Möksy");
        assert_eq!(feature["properties"]["label"], "Alajärvi Möksy");
    }
}
