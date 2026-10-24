//! WMS 1.3.0 GetFeatureInfo (§7.4, #1064, #1100): what a map layer shows at
//! a pixel of the map the client shows.
//!
//! A layer answers in one of three ways, each from the engine call that
//! answers the same question elsewhere, at the frame GetMap draws:
//!
//! - **A parameter's value** is the collection's EDR position query at the
//!   centre of pixel `(I, J)`: the same engine call, at the same point,
//!   time, run and level, answers EDR `position` and GetFeatureInfo, so the
//!   two report the same value. It is the source value there, sampled as
//!   the engine's position query samples it (bilinear for GRIB, QueryData
//!   and Zarr), not the colour of the pixel, which an EPSG:3857/3067/3035
//!   GetMap resamples from meta-tiles by nearest neighbour.
//! - **An RGB composite** answers each band's value, from one position
//!   query of its bands at the scan the composite is drawn from (#1100).
//! - **A layer that draws features** (CAP alert areas, the ODIM `CELLS`
//!   overlay, PostGIS lightning strikes) answers the features the map draws
//!   at the pixel, picked by the engine with its render's own selection
//!   ([`MapEngine::pick_features`], #1100): at most `FEATURE_COUNT` of
//!   them, all counted.
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
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};

use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{Geometry, PropertyValue};
use ds_core::feature_pick::{FeaturePick, PickMap, PickedFeature, PickedFeatures};
use ds_core::map_engine::MapEngine;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};

use crate::error::WmsError;
use crate::handlers::{cache_control_value, resolve_layers, Paint, ResolvedLayer, WmsState};
use crate::params::{InfoFormat, WmsQuery};

/// The most features GetFeatureInfo reports for one layer, whatever
/// `FEATURE_COUNT` asks: a larger count reports this many, and the answer
/// still states how many there are.
pub const MAX_FEATURE_COUNT: u32 = 50;

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
/// it draws features its engine picks ([`MapEngine::layer_picks_features`]),
/// or its collection has a position engine and the layer shows the
/// parameter's own values ([`MapEngine::layer_shows_values`]). RGB
/// composite layers are [`composite_is_queryable`]. O(1): a map lookup and
/// snapshot reads, for every layer of every GetCapabilities.
pub(crate) fn layer_is_queryable(
    point_engines: &HashMap<String, Arc<dyn EdrEngine>>,
    collection_id: &str,
    engine: &dyn MapEngine,
    parameter: Option<&str>,
) -> bool {
    parameter.is_some_and(|parameter| engine.layer_picks_features(parameter))
        || (point_engines.contains_key(collection_id)
            && parameter.is_none_or(|parameter| engine.layer_shows_values(parameter)))
}

/// Whether an RGB composite layer of `collection_id` over `bands` is
/// queryable: its collection has a position engine, which reads each band,
/// and every band shows its own values. O(bands), from snapshots.
pub(crate) fn composite_is_queryable(
    point_engines: &HashMap<String, Arc<dyn EdrEngine>>,
    collection_id: &str,
    engine: &dyn MapEngine,
    bands: &[String],
) -> bool {
    point_engines.contains_key(collection_id)
        && bands.iter().all(|band| engine.layer_shows_values(band))
}

/// One value an answer reports: a layer's value, or one band of an RGB
/// composite.
struct Value {
    parameter: Option<String>,
    /// `None` is nodata: no value at the point, or the point outside the
    /// data or outside the CRS.
    value: Option<f64>,
    unit: Option<String>,
}

/// What one queried layer shows at the pixel.
enum Content {
    /// One value per parameter read: a layer's one, a composite's bands.
    Values(Vec<Value>),
    /// The features the layer draws there.
    Features {
        parameter: Option<String>,
        picked: PickedFeatures,
    },
}

/// One queried layer's answer.
struct FeatureInfo {
    layer: String,
    time: Option<DateTime<Utc>>,
    reference_time: Option<DateTime<Utc>>,
    elevation: Option<f64>,
    /// `(lon, lat)` of the pixel's centre; `None` outside the CRS's domain.
    position: Option<(f64, f64)>,
    content: Content,
}

impl FeatureInfo {
    /// Whether the answer holds what the layer shows there: every value,
    /// or at least one feature. An empty answer always revalidates.
    fn answered(&self) -> bool {
        match &self.content {
            Content::Values(values) => values.iter().all(|value| value.value.is_some()),
            Content::Features { picked, .. } => picked.number_matched > 0,
        }
    }
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
        .map(|resolved| Query::new(state, resolved))
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

    let replies = match position {
        None => queries.iter().map(|_| Reply::Nothing).collect(),
        Some(point) => {
            // The pixel on the map's grid, for a layer that draws features:
            // the engine maps its ground area as the map's nodes are, grown
            // by its symbols where they are wider than a pixel.
            let pixel = Pixel {
                output_crs: map.output_crs.clone(),
                bbox: map.bbox,
                size: (map.width, map.height),
                pixel: (params.i, params.j),
                point,
                limit: params.feature_count.min(MAX_FEATURE_COUNT) as usize,
            };
            read_layers(state, &queries, pixel).await?
        }
    };

    let features: Vec<FeatureInfo> = queries
        .iter()
        .zip(replies)
        .map(|(query, reply)| query.answer(reply, position))
        .collect();
    // As GetMap: `immutable` only for an explicit TIME that resolved, over
    // content the engine never revises in place, here for every queried
    // layer. An empty answer always revalidates: some engines answer a read
    // that failed transiently with a null sample, and GetMap never pins a
    // failed render either.
    let pinned = queries
        .iter()
        .zip(&features)
        .all(|(query, info)| query.explicit_time && query.time.is_some() && info.answered());
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

/// How a query layer is read.
#[derive(Clone)]
enum Read {
    /// The position query of the layer's parameter.
    Value(Arc<dyn EdrEngine>),
    /// One position query of an RGB composite's bands.
    Bands(Arc<dyn EdrEngine>, Vec<String>),
    /// The features the map engine draws at the pixel.
    Features(Arc<dyn MapEngine>),
}

/// The picked pixel and the map's grid, for the layers that draw features.
#[derive(Clone)]
struct Pixel {
    output_crs: ds_core::map_engine::OutputCrs,
    /// The map's WGS84 `bbox`, as `get_raster_tile` is passed it.
    bbox: [f64; 4],
    size: (u32, u32),
    pixel: (u32, u32),
    point: (f64, f64),
    limit: usize,
}

/// One query layer's read: how, and the parameter, time, run and level
/// GetMap draws the layer at.
#[derive(Clone)]
struct Query {
    /// The `QUERY_LAYERS` name.
    layer: String,
    read: Read,
    /// The parameter drawn; for a composite, its name.
    parameter: Option<String>,
    /// Each value's unit from the layer's metadata, for an answer whose
    /// coverage names none: the parameter's, or each band's.
    units: Vec<Option<String>>,
    explicit_time: bool,
    time: Option<DateTime<Utc>>,
    reference_time: Option<DateTime<Utc>>,
    /// The level GetMap draws, for a layer with an `ELEVATION` dimension.
    elevation: Option<f64>,
    /// The `ELEVATION` the layer takes, which a feature pick is given.
    requested_elevation: Option<f64>,
    content_version: u64,
}

impl Query {
    /// How `resolved` is read, or `LayerNotQueryable`.
    fn new(state: &WmsState, resolved: &ResolvedLayer) -> Result<Self, WmsError> {
        let not_queryable =
            || WmsError::LayerNotQueryable(format!("Layer '{}' is not queryable", resolved.layer));
        let unit = |parameter: Option<&str>| {
            resolved
                .info
                .parameter_unit(parameter)
                .filter(|unit| !unit.is_empty())
                .map(str::to_string)
        };
        let point_engine = || {
            state
                .point_engines
                .get(&resolved.collection_id)
                .cloned()
                .ok_or_else(not_queryable)
        };
        // The parameter drawn: the layer's or style's, else the
        // collection's default, which a single-parameter layer draws.
        let parameter = resolved.parameter.clone().or_else(|| {
            Some(resolved.info.parameter.clone()).filter(|parameter| !parameter.is_empty())
        });
        let (read, units) = match &resolved.paint {
            // An RGB composite: each band's value.
            Paint::Composite(spec) => {
                if !composite_is_queryable(
                    &state.point_engines,
                    &resolved.collection_id,
                    resolved.engine.as_ref(),
                    &spec.parameters,
                ) {
                    return Err(not_queryable());
                }
                let units = spec
                    .parameters
                    .iter()
                    .map(|band| unit(Some(band)))
                    .collect();
                (Read::Bands(point_engine()?, spec.parameters.clone()), units)
            }
            Paint::Colormap(_)
                if parameter
                    .as_deref()
                    .is_some_and(|parameter| resolved.engine.layer_picks_features(parameter)) =>
            {
                (Read::Features(resolved.engine.clone()), Vec::new())
            }
            Paint::Colormap(_) => {
                if !layer_is_queryable(
                    &state.point_engines,
                    &resolved.collection_id,
                    resolved.engine.as_ref(),
                    parameter.as_deref(),
                ) {
                    return Err(not_queryable());
                }
                (
                    Read::Value(point_engine()?),
                    vec![unit(parameter.as_deref())],
                )
            }
        };

        let (time, reference_time) = (resolved.time, resolved.reference_time);
        // The level GetMap draws for this layer's ELEVATION, or without one,
        // as the engine selects it (`MapEngine::resolve_elevation`), from
        // the same parameter, time and run GetMap passes it. It need not be
        // ELEVATION or the first advertised level: GRIB draws the run's own
        // first level, a radar volume the nearest sweep carrying the
        // quantity. Only a layer with an ELEVATION dimension has one, and
        // only a value has a level: features are picked as drawn.
        let elevation = match read {
            Read::Features(_) => None,
            _ => resolved.info.vertical.as_ref().and_then(|_| {
                resolved.engine.resolve_elevation(
                    resolved.parameter.as_deref(),
                    resolved.elevation,
                    time,
                    reference_time,
                )
            }),
        };
        Ok(Self {
            layer: resolved.layer.clone(),
            read,
            parameter,
            units,
            explicit_time: resolved.explicit_time,
            time,
            reference_time,
            elevation,
            requested_elevation: resolved.elevation,
            content_version: resolved.content_version,
        })
    }

    /// The layer's read at `pixel`, at the layer's time, level and run.
    fn run(&self, pixel: &Pixel) -> Result<Reply, DataServerError> {
        let (lon, lat) = pixel.point;
        let coords = format!("POINT({lon} {lat})");
        let datetime = self.time.map(|t| (t, t));
        match &self.read {
            Read::Value(edr) => {
                let parameters = self.parameter.clone().map(|parameter| vec![parameter]);
                let levels = self.elevation.map(|z| vec![z]);
                edr.query_position(
                    &coords,
                    datetime,
                    parameters.as_deref(),
                    levels.as_deref(),
                    self.reference_time,
                )
                .map(Reply::Coverage)
            }
            // No scan every band has: GetMap draws the empty image, and
            // there is nothing to read.
            Read::Bands(_, _) if self.time.is_none() => Ok(Reply::Nothing),
            Read::Bands(edr, bands) => {
                let levels = self.elevation.map(|z| vec![z]);
                edr.query_position(
                    &coords,
                    datetime,
                    Some(bands),
                    levels.as_deref(),
                    self.reference_time,
                )
                .map(Reply::Coverage)
            }
            Read::Features(engine) => engine
                .pick_features(&FeaturePick {
                    parameter: self.parameter.as_deref(),
                    map: PickMap {
                        output_crs: &pixel.output_crs,
                        wgs84_bbox: pixel.bbox,
                        width: pixel.size.0,
                        height: pixel.size.1,
                    },
                    pixel: pixel.pixel,
                    point: pixel.point,
                    time: self.time,
                    reference_time: self.reference_time,
                    z: self.requested_elevation,
                    limit: pixel.limit.max(1),
                })
                .map(Reply::Picked),
        }
    }

    /// The layer's answer from what its read replied.
    fn answer(&self, reply: Reply, position: Option<(f64, f64)>) -> FeatureInfo {
        let coverage = match &reply {
            Reply::Coverage(response) => Some(response),
            Reply::Picked(_) | Reply::Nothing => None,
        };
        let mut time = self.time;
        let content = match &self.read {
            Read::Value(_) => {
                let sample = coverage
                    .and_then(|response| {
                        sample(
                            response,
                            self.parameter.as_deref(),
                            self.time,
                            self.elevation,
                        )
                    })
                    .unwrap_or_default();
                time = time.or(sample.time);
                Content::Values(vec![Value {
                    parameter: self.parameter.clone(),
                    value: sample.value,
                    unit: sample
                        .unit
                        .or_else(|| self.units.first().cloned().flatten()),
                }])
            }
            Read::Bands(_, bands) => Content::Values(
                bands
                    .iter()
                    .zip(&self.units)
                    .map(|(band, unit)| {
                        let sample = coverage
                            .and_then(|response| {
                                sample(response, Some(band), self.time, self.elevation)
                            })
                            .unwrap_or_default();
                        Value {
                            parameter: Some(band.clone()),
                            value: sample.value,
                            unit: sample.unit.or_else(|| unit.clone()),
                        }
                    })
                    .collect(),
            ),
            Read::Features(_) => Content::Features {
                parameter: self.parameter.clone(),
                picked: match reply {
                    Reply::Picked(picked) => picked,
                    Reply::Coverage(_) | Reply::Nothing => PickedFeatures::default(),
                },
            },
        };
        FeatureInfo {
            layer: self.layer.clone(),
            time,
            reference_time: self.reference_time,
            // The layer's ELEVATION dimension only: a level the source names
            // on a layer without one (a 10 m wind's height) is no WMS
            // dimension.
            elevation: self.elevation,
            position,
            content,
        }
    }
}

/// What a query layer's read replied.
enum Reply {
    /// A position query's coverage.
    Coverage(CoverageResponse),
    /// The features picked at the pixel.
    Picked(PickedFeatures),
    /// Nothing read: no data there, or nothing to read.
    Nothing,
}

/// Whether a read's error is an empty answer: no data at the point or the
/// time, or no longer any: a run evicted, or a collection reloaded away,
/// since the map resolved.
fn is_no_data(error: &DataServerError) -> bool {
    matches!(
        error,
        DataServerError::LocationNotFound(_)
            | DataServerError::FeatureNotFound(_)
            | DataServerError::ReferenceTimeNotFound(_)
            | DataServerError::CollectionNotFound(_)
    )
}

/// Each query layer's read at `pixel`, in `queries` order. One admission
/// for them all: the render slots and queue of a GetMap render, under
/// GetFeatureInfo's own deadline (`MC_FEATURE_INFO_TIMEOUT_MS`, #1103),
/// which covers the slot wait and every read. The reads run one after
/// another on the shared query runtime, as EDR's queries do: engines
/// `block_in_place` and spawn their own tasks there, never on the runtime
/// serving requests (root CLAUDE.md Critical Rule 7). The first read that
/// fails, other than with no data, fails the request, and the ones after it
/// are not run.
async fn read_layers(
    state: &WmsState,
    queries: &[Query],
    pixel: Pixel,
) -> Result<Vec<Reply>, WmsError> {
    let job = ds_executor::RenderJob::acquire_feature_info(state.render_semaphore.clone())
        .await
        .map_err(WmsError::from)?;
    let runs = queries.to_vec();
    let answers = job
        .run_query(move || {
            let mut answers = Vec::with_capacity(runs.len());
            for query in &runs {
                // The deadline covers every read: one that expired while an
                // earlier layer was read is not started.
                let answer = ds_core::deadline::check().and_then(|()| query.run(&pixel));
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

    let mut replies = Vec::with_capacity(queries.len());
    for (query, answer) in queries.iter().zip(answers) {
        replies.push(match answer {
            Ok(reply) => reply,
            Err(e) if is_no_data(&e) => Reply::Nothing,
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
    Ok(replies)
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

/// A feature property's value as the text formats write it: a list's items
/// and a record's fields joined with commas, a null `null`.
fn property_text(value: &PropertyValue) -> String {
    match value {
        PropertyValue::String(text) => text.clone(),
        PropertyValue::Float(value) => value.to_string(),
        PropertyValue::Integer(value) => value.to_string(),
        PropertyValue::Bool(value) => value.to_string(),
        PropertyValue::Null => "null".to_string(),
        PropertyValue::List(items) => items
            .iter()
            .map(property_text)
            .collect::<Vec<_>>()
            .join(", "),
        PropertyValue::Object(fields) => fields
            .iter()
            .map(|(name, value)| format!("{name}: {}", property_text(value)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The fields of one answer, in the order the text and HTML formats list
/// them, as `(label, text)`: one block per value read, and one for a
/// layer's features, which lists their count and then the features.
/// Nodata is `nodata`; a field without a value is left out, except the
/// value itself.
fn blocks(info: &FeatureInfo) -> Vec<Block<'_>> {
    let dimensions = |fields: &mut Vec<(String, String)>| {
        if let Some(time) = info.time {
            fields.push(("time".into(), timestamp(time)));
        }
        if let Some(reference_time) = info.reference_time {
            fields.push(("reference_time".into(), timestamp(reference_time)));
        }
        if let Some(elevation) = info.elevation {
            fields.push(("elevation".into(), elevation.to_string()));
        }
        if let Some((lon, lat)) = info.position {
            fields.push(("lon".into(), lon.to_string()));
            fields.push(("lat".into(), lat.to_string()));
        }
    };
    match &info.content {
        Content::Values(values) => values
            .iter()
            .map(|value| {
                let mut fields = Vec::with_capacity(9);
                if let Some(parameter) = &value.parameter {
                    fields.push(("parameter".into(), parameter.clone()));
                }
                fields.push((
                    "value".into(),
                    value
                        .value
                        .map_or_else(|| "nodata".to_string(), |value| value.to_string()),
                ));
                if let Some(unit) = &value.unit {
                    fields.push(("unit".into(), unit.clone()));
                }
                dimensions(&mut fields);
                Block {
                    fields,
                    features: Vec::new(),
                }
            })
            .collect(),
        Content::Features { parameter, picked } => {
            let mut fields = Vec::with_capacity(9);
            if let Some(parameter) = parameter {
                fields.push(("parameter".into(), parameter.clone()));
            }
            dimensions(&mut fields);
            fields.push(("matched".into(), picked.number_matched.to_string()));
            fields.push(("returned".into(), picked.features.len().to_string()));
            vec![Block {
                fields,
                features: picked.features.iter().collect(),
            }]
        }
    }
}

/// One block the text and HTML formats write: a value's or a layer's
/// fields, and the features picked.
struct Block<'a> {
    fields: Vec<(String, String)>,
    features: Vec<&'a PickedFeature>,
}

/// `text/plain`: per block, `layer: <name>`, then one `label: text` line
/// per field, then each picked feature as `id: <id>` and one line per
/// property; a blank line between blocks. Line breaks inside a property are
/// written as spaces, so every line is one field.
fn plain_text(features: &[FeatureInfo]) -> String {
    let one_line = |text: &str| text.replace(['\r', '\n'], " ");
    let mut out = String::new();
    for info in features {
        for block in blocks(info) {
            if !out.is_empty() {
                out.push('\n');
            }
            let _ = writeln!(out, "layer: {}", info.layer);
            for (label, text) in &block.fields {
                let _ = writeln!(out, "{label}: {}", one_line(text));
            }
            for feature in &block.features {
                let _ = writeln!(out, "id: {}", one_line(&feature.id));
                for (name, value) in &feature.properties {
                    let _ = writeln!(
                        out,
                        "{}: {}",
                        one_line(name),
                        one_line(&property_text(value))
                    );
                }
            }
        }
    }
    out
}

/// `application/json`: a GeoJSON FeatureCollection. A value is a Point
/// feature at the sampled position (`geometry` null outside the CRS), its
/// properties the layer, parameter, value (null for nodata), unit and time,
/// and the reference time and elevation when present: one per queried
/// layer, one per band of an RGB composite. A picked feature is a feature
/// with its id, its own geometry and the layer and its properties. Each
/// layer that draws features is also listed under the foreign member
/// `layers`, with its time and how many features it has there
/// (`numberMatched`) and the answer holds (`numberReturned`).
fn json(features: &[FeatureInfo]) -> axum::Json<impl Serialize + '_> {
    #[derive(Serialize)]
    struct Collection<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        features: Vec<Item<'a>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        layers: Vec<Layer<'a>>,
    }
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Item<'a> {
        Value(ValueFeature<'a>),
        Picked(PickedJson<'a>),
    }
    #[derive(Serialize)]
    struct ValueFeature<'a> {
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
    #[derive(Serialize)]
    struct Layer<'a> {
        layer: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameter: Option<&'a str>,
        time: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference_time: Option<String>,
        #[serde(rename = "numberMatched")]
        number_matched: usize,
        #[serde(rename = "numberReturned")]
        number_returned: usize,
    }

    let mut items = Vec::new();
    let mut layers = Vec::new();
    for info in features {
        match &info.content {
            Content::Values(values) => {
                items.extend(values.iter().map(|value| {
                    Item::Value(ValueFeature {
                        kind: "Feature",
                        geometry: info.position.map(|(lon, lat)| Point {
                            kind: "Point",
                            coordinates: [lon, lat],
                        }),
                        properties: Properties {
                            layer: &info.layer,
                            parameter: value.parameter.as_deref(),
                            value: value.value,
                            unit: value.unit.as_deref(),
                            time: info.time.map(timestamp),
                            reference_time: info.reference_time.map(timestamp),
                            elevation: info.elevation,
                        },
                    })
                }));
            }
            Content::Features { parameter, picked } => {
                items.extend(picked.features.iter().map(|feature| {
                    Item::Picked(PickedJson {
                        layer: &info.layer,
                        feature,
                    })
                }));
                layers.push(Layer {
                    layer: &info.layer,
                    parameter: parameter.as_deref(),
                    time: info.time.map(timestamp),
                    reference_time: info.reference_time.map(timestamp),
                    number_matched: picked.number_matched,
                    number_returned: picked.features.len(),
                });
            }
        }
    }
    axum::Json(Collection {
        kind: "FeatureCollection",
        features: items,
        layers,
    })
}

/// A picked feature as GeoJSON: its id, geometry, and as properties the
/// queried layer and the feature's own, in the engine's order.
struct PickedJson<'a> {
    layer: &'a str,
    feature: &'a PickedFeature,
}

impl Serialize for PickedJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Properties<'a>(&'a str, &'a [(String, PropertyValue)]);
        impl Serialize for Properties<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("layer", self.0)?;
                // The layer's name keeps its key.
                for (name, value) in self.1.iter().filter(|(name, _)| name != "layer") {
                    map.serialize_entry(name, &JsonValue(value))?;
                }
                map.end()
            }
        }
        let mut map = serializer.serialize_map(Some(4))?;
        map.serialize_entry("type", "Feature")?;
        map.serialize_entry("id", &self.feature.id)?;
        map.serialize_entry("geometry", &JsonGeometry(&self.feature.geometry))?;
        map.serialize_entry(
            "properties",
            &Properties(self.layer, &self.feature.properties),
        )?;
        map.end()
    }
}

/// A property value as JSON: lists as arrays, records as objects, a
/// non-finite number as null.
struct JsonValue<'a>(&'a PropertyValue);

impl Serialize for JsonValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            PropertyValue::String(text) => serializer.serialize_str(text),
            PropertyValue::Float(value) if value.is_finite() => serializer.serialize_f64(*value),
            PropertyValue::Float(_) | PropertyValue::Null => serializer.serialize_none(),
            PropertyValue::Integer(value) => serializer.serialize_i64(*value),
            PropertyValue::Bool(value) => serializer.serialize_bool(*value),
            PropertyValue::List(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&JsonValue(item))?;
                }
                seq.end()
            }
            PropertyValue::Object(fields) => {
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (name, value) in fields {
                    map.serialize_entry(name, &JsonValue(value))?;
                }
                map.end()
            }
        }
    }
}

/// A geometry as GeoJSON, `null` for none.
struct JsonGeometry<'a>(&'a Geometry);

impl Serialize for JsonGeometry<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        type Rings<'a> = (&'a [[f64; 2]], &'a [Vec<[f64; 2]>]);
        /// A polygon's rings: the exterior, then the holes.
        struct Polygon<'a>(Rings<'a>);
        impl Serialize for Polygon<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let (exterior, holes) = self.0;
                let mut seq = serializer.serialize_seq(Some(1 + holes.len()))?;
                seq.serialize_element(exterior)?;
                for hole in holes {
                    seq.serialize_element(hole)?;
                }
                seq.end()
            }
        }
        if matches!(self.0, Geometry::Null) {
            return serializer.serialize_none();
        }
        let mut map = serializer.serialize_map(Some(2))?;
        match self.0 {
            Geometry::Null => {}
            Geometry::Point { x, y } => {
                map.serialize_entry("type", "Point")?;
                map.serialize_entry("coordinates", &[x, y])?;
            }
            Geometry::Polygon { exterior, holes } => {
                map.serialize_entry("type", "Polygon")?;
                map.serialize_entry("coordinates", &Polygon((exterior, holes)))?;
            }
            Geometry::MultiPolygon { polygons } => {
                let polygons: Vec<Polygon<'_>> = polygons
                    .iter()
                    .map(|(exterior, holes)| Polygon((exterior, holes)))
                    .collect();
                map.serialize_entry("type", "MultiPolygon")?;
                map.serialize_entry("coordinates", &polygons)?;
            }
        }
        map.end()
    }
}

/// `text/html`: a document with one table per block, captioned with the
/// layer's name, a row per field, and after a layer's table one table per
/// picked feature, captioned with its id, a row per property. Written with
/// quick-xml, which escapes every label and value (root CLAUDE.md Critical
/// Rule 3).
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
        for block in blocks(info) {
            table(
                &mut writer,
                &info.layer,
                block
                    .fields
                    .iter()
                    .map(|(label, text)| (label.as_str(), text.clone())),
            );
            for feature in &block.features {
                table(
                    &mut writer,
                    &feature.id,
                    feature
                        .properties
                        .iter()
                        .map(|(name, value)| (name.as_str(), property_text(value))),
                );
            }
        }
    }
    let _ = writer.write_event(Event::End(BytesEnd::new("body")));
    let _ = writer.write_event(Event::End(BytesEnd::new("html")));
    writer.into_inner()
}

/// `<table>` captioned `caption`, a `<tr><th>label</th><td>text</td></tr>`
/// per row, every text escaped.
fn table<'a>(
    writer: &mut Writer<Vec<u8>>,
    caption: &str,
    rows: impl Iterator<Item = (&'a str, String)>,
) {
    let _ = writer.write_event(Event::Start(BytesStart::new("table")));
    text_element(writer, "caption", caption);
    for (label, text) in rows {
        let _ = writer.write_event(Event::Start(BytesStart::new("tr")));
        text_element(writer, "th", label);
        text_element(writer, "td", &text);
        let _ = writer.write_event(Event::End(BytesEnd::new("tr")));
    }
    let _ = writer.write_event(Event::End(BytesEnd::new("table")));
}

/// `<tag>text</tag>`, the text escaped.
fn text_element(writer: &mut Writer<Vec<u8>>, tag: &str, text: &str) {
    let _ = writer.write_event(Event::Start(BytesStart::new(tag)));
    let _ = writer.write_event(Event::Text(BytesText::new(text)));
    let _ = writer.write_event(Event::End(BytesEnd::new(tag)));
}
