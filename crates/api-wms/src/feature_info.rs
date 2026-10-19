//! WMS 1.3.0 GetFeatureInfo (§7.4, #1064): the value of a map layer at a
//! pixel of the map the client shows.
//!
//! The value is the collection's EDR position query at the centre of pixel
//! `(I, J)`: the same engine call, at the same point, time, run and level,
//! answers EDR `position` and GetFeatureInfo, so the two report the same
//! value. It is the source value there, sampled as the engine's position
//! query samples it (bilinear for GRIB, QueryData and Zarr), not the colour
//! of the pixel, which an EPSG:3857/3067/3035 GetMap resamples from
//! meta-tiles by nearest neighbour.
//!
//! The map part resolves through [`resolve_layers`], GetMap's own
//! resolution, so each query layer's reported time and run are the ones
//! GetMap draws it at and keys its caches on, and the level through
//! [`MapEngine::resolve_elevation`], the one GetMap draws. A request with
//! several `QUERY_LAYERS` answers for each, in `QUERY_LAYERS` order.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;
use serde::Serialize;

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::map_engine::MapEngine;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};

use crate::error::WmsError;
use crate::handlers::{cache_control_value, resolve_layers, Paint, ResolvedLayer, WmsState};
use crate::params::{InfoFormat, WmsQuery};

/// Whether an EDR engine answers position queries, the capability that
/// makes its collection's WMS layers queryable. The server reads it once per
/// collection when it builds the WMS state and registers the engines that
/// answer in [`WmsState::point_engines`]; it is never read per request,
/// since `supported_query_types` builds a list.
pub fn answers_position(engine: &dyn EdrEngine) -> bool {
    engine
        .supported_query_types()
        .iter()
        .any(|query| query == "position")
}

/// Whether the layer of `collection_id` drawing `parameter` is queryable:
/// its collection has a position engine, and the layer shows the
/// parameter's own values ([`MapEngine::layer_shows_values`]). RGB composite
/// layers are never queryable; their callers do not ask. O(1): a map lookup
/// and a snapshot read, for every layer of every GetCapabilities.
pub(crate) fn layer_is_queryable(
    point_engines: &HashMap<String, Arc<dyn EdrEngine>>,
    collection_id: &str,
    engine: &dyn MapEngine,
    parameter: Option<&str>,
) -> bool {
    point_engines.contains_key(collection_id)
        && parameter.is_none_or(|parameter| engine.layer_shows_values(parameter))
}

/// One queried layer's answer.
struct FeatureInfo {
    layer: String,
    parameter: Option<String>,
    /// `None` is nodata: no value at the point, or the point outside the
    /// data or outside the CRS.
    value: Option<f64>,
    unit: Option<String>,
    time: Option<DateTime<Utc>>,
    reference_time: Option<DateTime<Utc>>,
    elevation: Option<f64>,
    /// `(lon, lat)` of the pixel's centre; `None` outside the CRS's domain.
    position: Option<(f64, f64)>,
}

/// The GetFeatureInfo handler.
pub(crate) async fn get_feature_info(
    state: &WmsState,
    query: &WmsQuery,
) -> Result<Response, WmsError> {
    let params = query.validate_get_feature_info()?;
    let map = &params.map;
    let layers = resolve_layers(state, map)?;

    // QUERY_LAYERS names layers of the map: Table E.1's LayerNotDefined is
    // also "a GetFeatureInfo request for a layer not shown on the map". A
    // layer LAYERS lists twice is queried as its first listing draws it.
    let mut queried = Vec::with_capacity(params.query_layers.len());
    for name in &params.query_layers {
        let resolved = layers
            .iter()
            .find(|layer| layer.layer == *name)
            .ok_or_else(|| {
                WmsError::LayerNotDefined(format!(
                    "QUERY_LAYERS layer '{name}' is not one of LAYERS"
                ))
            })?;
        queried.push(resolved);
    }
    // Every query layer is queryable, or the request is refused before any
    // engine is asked.
    let queries = queried
        .into_iter()
        .map(|resolved| {
            let (edr, parameter) = point_engine(state, resolved)?;
            Ok(Query::new(resolved, edr, parameter))
        })
        .collect::<Result<Vec<_>, WmsError>>()?;

    // The centre of pixel (I, J), through the output CRS mapping every
    // engine renders the map with: EPSG:4326's lat/lon BBOX is already
    // normalized to lon/lat, EPSG:3857 maps through `ds_core::web_mercator`
    // and EPSG:3067/3035 through their projection's inverse, from the
    // projected BBOX `output_crs` carries, never from `map.bbox`.
    let fx = (f64::from(params.i) + 0.5) / f64::from(map.width);
    let fy = (f64::from(params.j) + 0.5) / f64::from(map.height);
    // A pixel centre off the Earth has no position and so no value (WMS
    // 1.3.0 §7.3.3.6), and no engine is asked. A map GetMap draws as the
    // empty image without a read has no pixel on it: its `domain_rows` is
    // `None`, as for a projected BBOX wholly outside its projection, whose
    // `map.bbox` is NaN (#1089). Otherwise the centre itself is tested:
    // past a pole of a geographic BBOX, or outside a projection's valid
    // area, where its inverse has no answer: past the LAEA disk, or past a
    // pole or the eastings where Transverse Mercator's series stops being
    // exact (#1113, #1130).
    let position = if map.output_crs.domain_rows(map.bbox, map.height).is_none() {
        None
    } else {
        map.output_crs
            .position_on_earth(map.bbox, fx, fy)
            .map(|(lon, lat)| (ds_core::geo::wrap_lon(lon), lat))
    };

    let samples = match position {
        None => queries.iter().map(|_| Sample::default()).collect(),
        Some((lon, lat)) => sample_layers(state, &queries, lon, lat).await?,
    };

    let features: Vec<FeatureInfo> = queries
        .iter()
        .zip(samples)
        .map(|(query, sample)| FeatureInfo {
            layer: query.layer.clone(),
            unit: sample.unit.or_else(|| query.unit.clone()),
            parameter: query.parameter.clone(),
            value: sample.value,
            time: query.time.or(sample.time),
            reference_time: query.reference_time,
            // The layer's ELEVATION dimension only: a level the source names
            // on a layer without one (a 10 m wind's height) is no WMS
            // dimension.
            elevation: query.elevation,
            position,
        })
        .collect();
    // As GetMap: `immutable` only for an explicit TIME that resolved, over
    // content the engine never revises in place, here for every queried
    // layer. A null value always revalidates: some engines answer a read
    // that failed transiently with a null sample, and GetMap never pins a
    // failed render either.
    let pinned = queries
        .iter()
        .zip(&features)
        .all(|(query, info)| query.explicit_time && query.time.is_some() && info.value.is_some());
    let content_version = queries
        .iter()
        .map(|query| query.content_version)
        .max()
        .unwrap_or_default();
    let cache_control = cache_control_value(pinned, content_version);

    let mut response = match params.info_format {
        InfoFormat::Text => plain_text(&features).into_response(),
        InfoFormat::Json => json(&features).into_response(),
        InfoFormat::Html => html(&features).into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(params.info_format.content_type()),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// One query layer's position query: its engine, and the parameter, time,
/// run and level GetMap draws the layer at.
#[derive(Clone)]
struct Query {
    /// The `QUERY_LAYERS` name.
    layer: String,
    edr: Arc<dyn EdrEngine>,
    parameter: Option<String>,
    /// The parameter's unit from the layer's metadata, for an answer whose
    /// coverage names none.
    unit: Option<String>,
    explicit_time: bool,
    time: Option<DateTime<Utc>>,
    reference_time: Option<DateTime<Utc>>,
    elevation: Option<f64>,
    content_version: u64,
}

impl Query {
    fn new(resolved: &ResolvedLayer, edr: Arc<dyn EdrEngine>, parameter: Option<String>) -> Self {
        let (time, reference_time) = (resolved.time, resolved.reference_time);
        // The level GetMap draws for this layer's ELEVATION, or without one,
        // as the engine selects it (`MapEngine::resolve_elevation`), from
        // the same parameter, time and run GetMap passes it. It need not be
        // ELEVATION or the first advertised level: GRIB draws the run's own
        // first level, a radar volume the nearest sweep carrying the
        // quantity. Only a layer with an ELEVATION dimension has one.
        let elevation = resolved.info.vertical.as_ref().and_then(|_| {
            resolved.engine.resolve_elevation(
                resolved.parameter.as_deref(),
                resolved.elevation,
                time,
                reference_time,
            )
        });
        Self {
            layer: resolved.layer.clone(),
            unit: resolved
                .info
                .parameter_unit(parameter.as_deref())
                .filter(|unit| !unit.is_empty())
                .map(str::to_string),
            edr,
            parameter,
            explicit_time: resolved.explicit_time,
            time,
            reference_time,
            elevation,
            content_version: resolved.content_version,
        }
    }

    /// The position query at `coords`, at the layer's time, level and run.
    fn run(&self, coords: &str) -> Result<CoverageResponse, DataServerError> {
        let parameters = self.parameter.clone().map(|parameter| vec![parameter]);
        let levels = self.elevation.map(|z| vec![z]);
        self.edr.query_position(
            coords,
            self.time.map(|t| (t, t)),
            parameters.as_deref(),
            levels.as_deref(),
            self.reference_time,
        )
    }
}

/// Whether a position query's error is an empty answer: no data at the
/// point or the time, or no longer any: a run evicted, or a collection
/// reloaded away, since the map resolved.
fn is_no_data(error: &DataServerError) -> bool {
    matches!(
        error,
        DataServerError::LocationNotFound(_)
            | DataServerError::FeatureNotFound(_)
            | DataServerError::ReferenceTimeNotFound(_)
            | DataServerError::CollectionNotFound(_)
    )
}

/// Each query layer's value at `(lon, lat)`, in `queries` order. One
/// admission for them all: the render slots and queue of a GetMap render,
/// under GetFeatureInfo's own deadline (`MC_FEATURE_INFO_TIMEOUT_MS`,
/// #1103), which covers the slot wait and every query. The queries run one
/// after another on the shared query runtime, as EDR's do: engines
/// `block_in_place` and spawn their own tasks there, never on the runtime
/// serving requests (root CLAUDE.md Critical Rule 7). The first query that
/// fails, other than with no data, fails the request, and the ones after it
/// are not run.
async fn sample_layers(
    state: &WmsState,
    queries: &[Query],
    lon: f64,
    lat: f64,
) -> Result<Vec<Sample>, WmsError> {
    let job = ds_executor::RenderJob::acquire_feature_info(state.render_semaphore.clone())
        .await
        .map_err(WmsError::from)?;
    let coords = format!("POINT({lon} {lat})");
    let runs = queries.to_vec();
    let answers = job
        .run_query(move || {
            let mut answers = Vec::with_capacity(runs.len());
            for query in &runs {
                // The deadline covers every query: one that expired while an
                // earlier layer was read is not started.
                let answer = ds_core::deadline::check().and_then(|()| query.run(&coords));
                let failed = answer.as_ref().is_err_and(|e| !is_no_data(e));
                answers.push(answer);
                if failed {
                    break;
                }
            }
            answers
        })
        .await
        .map_err(WmsError::from)?;

    let mut samples = Vec::with_capacity(queries.len());
    for (query, answer) in queries.iter().zip(answers) {
        samples.push(match answer {
            Ok(response) => sample(
                &response,
                query.parameter.as_deref(),
                query.time,
                query.elevation,
            )
            .unwrap_or_default(),
            Err(e) if is_no_data(&e) => Sample::default(),
            Err(e @ (DataServerError::ResourceExhausted | DataServerError::DeadlineExceeded)) => {
                return Err(WmsError::ServiceUnavailable(e.to_string()));
            }
            Err(
                e @ (DataServerError::InvalidParameter(_)
                | DataServerError::InvalidDatetime(_)
                | DataServerError::InvalidBbox(_)
                | DataServerError::QueryTooLarge(_)),
            ) => {
                tracing::warn!(
                    "WMS GetFeatureInfo on layer '{}' refused by the engine: {e}",
                    query.layer
                );
                return Err(WmsError::invalid_parameter(&format!(
                    "Layer '{}' cannot be queried with these dimension values",
                    query.layer
                )));
            }
            Err(e) => {
                tracing::warn!("WMS GetFeatureInfo error for layer '{}': {e}", query.layer);
                return Err(WmsError::Internal(e.to_string()));
            }
        });
    }
    Ok(samples)
}

/// The position engine and parameter that answer GetFeatureInfo on a map
/// layer, or `LayerNotQueryable`.
fn point_engine(
    state: &WmsState,
    resolved: &ResolvedLayer,
) -> Result<(Arc<dyn EdrEngine>, Option<String>), WmsError> {
    let not_queryable =
        || WmsError::LayerNotQueryable(format!("Layer '{}' is not queryable", resolved.layer));
    // An RGB composite has a value per band, and no position query answers
    // its name.
    if matches!(resolved.paint, Paint::Composite(_)) {
        return Err(not_queryable());
    }
    // The parameter drawn: the layer's or style's, else the collection's
    // default, which a single-parameter layer draws.
    let parameter = resolved.parameter.clone().or_else(|| {
        Some(resolved.info.parameter.clone()).filter(|parameter| !parameter.is_empty())
    });
    if !layer_is_queryable(
        &state.point_engines,
        &resolved.collection_id,
        resolved.engine.as_ref(),
        parameter.as_deref(),
    ) {
        return Err(not_queryable());
    }
    let engine = state
        .point_engines
        .get(&resolved.collection_id)
        .ok_or_else(not_queryable)?
        .clone();
    Ok((engine, parameter))
}

/// What a position query answered for the queried parameter, time and level.
#[derive(Default)]
struct Sample {
    value: Option<f64>,
    unit: Option<String>,
    /// The time of the value, read from the coverage when the map resolved
    /// none.
    time: Option<DateTime<Utc>>,
}

/// The value of `parameter` at `time` and level `z` in a position query's
/// coverages: the first coverage carrying the parameter whose domain has
/// that time and level. `time` / `z` `None` take the coverage's latest time
/// and first level. `None` when no coverage has them.
fn sample(
    response: &CoverageResponse,
    parameter: Option<&str>,
    time: Option<DateTime<Utc>>,
    z: Option<f64>,
) -> Option<Sample> {
    let coverages = match response {
        CoverageResponse::Single(coverage) => std::slice::from_ref(coverage),
        CoverageResponse::Collection(coverages) => coverages.as_slice(),
    };
    coverages
        .iter()
        .find_map(|coverage| sample_coverage(coverage, parameter, time, z))
}

fn sample_coverage(
    coverage: &QueryResult,
    parameter: Option<&str>,
    time: Option<DateTime<Utc>>,
    z: Option<f64>,
) -> Option<Sample> {
    let (name, range) = match parameter {
        Some(parameter) => coverage.ranges.get_key_value(parameter).or_else(|| {
            coverage
                .ranges
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(parameter))
        })?,
        None if coverage.ranges.len() == 1 => coverage.ranges.iter().next()?,
        None => return None,
    };
    let no_levels = || -> &[f64] { &[] };
    let (times, levels): (&[DateTime<Utc>], &[f64]) = match &coverage.domain {
        DomainDescription::Point { t, z, .. } => (
            t.as_slice(),
            z.as_ref().map_or_else(no_levels, |z| &z.values),
        ),
        DomainDescription::PointSeries { t, z, .. } => {
            (t, z.as_ref().map_or_else(no_levels, |z| &z.values))
        }
        DomainDescription::Grid { t, z, .. } => (
            t.as_deref().unwrap_or_default(),
            z.as_ref().map_or_else(no_levels, |z| &z.values),
        ),
        DomainDescription::VerticalProfile { t, z, .. } => (t.as_slice(), &z.values),
        DomainDescription::Section { .. } | DomainDescription::Trajectory { .. } => return None,
    };
    let t_index = match time {
        _ if times.is_empty() => None,
        Some(time) => Some(times.iter().position(|&t| t == time)?),
        None => Some(times.len() - 1),
    };
    let z_index = match z {
        _ if levels.is_empty() => None,
        Some(z) => Some(levels.iter().position(|&level| same_level(level, z))?),
        None => Some(0),
    };
    if range.shape.len() != range.axis_names.len() {
        return None;
    }
    // A domain axis of several values must index the range.
    let has_axis = |axis: &str| range.axis_names.iter().any(|name| name == axis);
    if (times.len() > 1 && !has_axis("t")) || (levels.len() > 1 && !has_axis("z")) {
        return None;
    }
    let mut flat = 0usize;
    for (axis, &size) in range.axis_names.iter().zip(&range.shape) {
        let index = match axis.as_str() {
            "t" => t_index,
            "z" => z_index,
            // A position's x and y have one value each.
            _ => Some(0),
        }
        .filter(|&index| index < size)?;
        flat = flat * size + index;
    }
    let value = range
        .values
        .get(flat)
        .copied()
        .flatten()
        .filter(|value| value.is_finite());
    let unit = coverage
        .parameters
        .get(name)
        .map(|description| description.unit.clone())
        .filter(|unit| !unit.is_empty());
    Some(Sample {
        value,
        unit,
        time: t_index.map(|i| times[i]),
    })
}

/// Whether a coverage's level is the requested one: equal up to the
/// rounding a level's decimal text can carry.
fn same_level(level: f64, z: f64) -> bool {
    (level - z).abs() <= 1e-9 * level.abs().max(z.abs()).max(1.0)
}

/// A time as the response writes it: RFC 3339 in UTC, with `Z`.
fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// The fields of one answer, in the order the text and HTML formats list
/// them, as `(label, text)`. Nodata is `nodata`; a field without a value is
/// left out, except the value itself.
fn fields(info: &FeatureInfo) -> Vec<(&'static str, String)> {
    let mut fields = Vec::with_capacity(8);
    if let Some(parameter) = &info.parameter {
        fields.push(("parameter", parameter.clone()));
    }
    fields.push((
        "value",
        info.value
            .map_or_else(|| "nodata".to_string(), |value| value.to_string()),
    ));
    if let Some(unit) = &info.unit {
        fields.push(("unit", unit.clone()));
    }
    if let Some(time) = info.time {
        fields.push(("time", timestamp(time)));
    }
    if let Some(reference_time) = info.reference_time {
        fields.push(("reference_time", timestamp(reference_time)));
    }
    if let Some(elevation) = info.elevation {
        fields.push(("elevation", elevation.to_string()));
    }
    if let Some((lon, lat)) = info.position {
        fields.push(("lon", lon.to_string()));
        fields.push(("lat", lat.to_string()));
    }
    fields
}

/// `text/plain`: `layer: <name>`, then one `label: text` line per field,
/// a blank line between layers.
fn plain_text(features: &[FeatureInfo]) -> String {
    let mut out = String::new();
    for (i, info) in features.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "layer: {}", info.layer);
        for (label, text) in fields(info) {
            let _ = writeln!(out, "{label}: {text}");
        }
    }
    out
}

/// `application/json`: a GeoJSON FeatureCollection, one Point feature per
/// queried layer at the sampled position (`geometry` null outside the
/// CRS), its properties the layer, parameter, value (null for nodata),
/// unit and time, and the reference time and elevation when present.
fn json(features: &[FeatureInfo]) -> axum::Json<impl Serialize + '_> {
    #[derive(Serialize)]
    struct Collection<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        features: Vec<Feature<'a>>,
    }
    #[derive(Serialize)]
    struct Feature<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        geometry: Option<Point>,
        properties: Properties<'a>,
    }
    #[derive(Serialize)]
    struct Point {
        #[serde(rename = "type")]
        kind: &'static str,
        coordinates: [f64; 2],
    }
    #[derive(Serialize)]
    struct Properties<'a> {
        layer: &'a str,
        parameter: Option<&'a str>,
        value: Option<f64>,
        unit: Option<&'a str>,
        time: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference_time: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        elevation: Option<f64>,
    }
    let collection = Collection {
        kind: "FeatureCollection",
        features: features
            .iter()
            .map(|info| Feature {
                kind: "Feature",
                geometry: info.position.map(|(lon, lat)| Point {
                    kind: "Point",
                    coordinates: [lon, lat],
                }),
                properties: Properties {
                    layer: &info.layer,
                    parameter: info.parameter.as_deref(),
                    value: info.value,
                    unit: info.unit.as_deref(),
                    time: info.time.map(timestamp),
                    reference_time: info.reference_time.map(timestamp),
                    elevation: info.elevation,
                },
            })
            .collect(),
    };
    axum::Json(collection)
}

/// `text/html`: a document with one table per queried layer, captioned
/// with the layer's name, a row per field. Written with quick-xml, which
/// escapes every label and value (root CLAUDE.md Critical Rule 3).
fn html(features: &[FeatureInfo]) -> Vec<u8> {
    let mut writer = Writer::new(Vec::new());
    let _ = writer.write_event(Event::DocType(BytesText::from_escaped("html")));
    let _ = writer.write_event(Event::Start(BytesStart::new("html")));
    let _ = writer.write_event(Event::Start(BytesStart::new("head")));
    let mut meta = BytesStart::new("meta");
    meta.push_attribute(("charset", "utf-8"));
    let _ = writer.write_event(Event::Empty(meta));
    text_element(&mut writer, "title", "GetFeatureInfo");
    let _ = writer.write_event(Event::End(BytesEnd::new("head")));
    let _ = writer.write_event(Event::Start(BytesStart::new("body")));
    for info in features {
        let _ = writer.write_event(Event::Start(BytesStart::new("table")));
        text_element(&mut writer, "caption", &info.layer);
        for (label, text) in fields(info) {
            let _ = writer.write_event(Event::Start(BytesStart::new("tr")));
            text_element(&mut writer, "th", label);
            text_element(&mut writer, "td", &text);
            let _ = writer.write_event(Event::End(BytesEnd::new("tr")));
        }
        let _ = writer.write_event(Event::End(BytesEnd::new("table")));
    }
    let _ = writer.write_event(Event::End(BytesEnd::new("body")));
    let _ = writer.write_event(Event::End(BytesEnd::new("html")));
    writer.into_inner()
}

/// `<tag>text</tag>`, the text escaped.
fn text_element(writer: &mut Writer<Vec<u8>>, tag: &str, text: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new(tag)));
    let _ = writer.write_event(Event::Text(BytesText::new(text)));
    let _ = writer.write_event(Event::End(BytesEnd::new(tag)));
}
