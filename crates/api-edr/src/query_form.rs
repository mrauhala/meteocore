//! The GET form of an EDR data query (#1138).
//!
//! Every position, area, radius, trajectory and cube link on an HTML page
//! (catalog chips, collection and run pages, a data page's breadcrumb)
//! opens the query's HTML view. Without its coordinates (`coords`, a cube's
//! `bbox`) the query cannot run, so [`bare_query_form`] answers such a
//! request, when it asked for HTML, with a form for the query instead of the
//! 400 a client gets: coordinates prefilled with an example inside the
//! collection's extent, a time select from its advertised times, `z` from
//! its levels, a checkbox per parameter, the formats the query offers, and
//! the request's URL with a Copy button. A data page shows the same form
//! filled from its request ([`QueryForm::html`]). JSON requests, and every
//! request that names its coordinates, never reach it.
//!
//! The form works without JavaScript: it submits natively, every field a
//! valid value ("All times" is the advertised interval, never an empty
//! `datetime`; an empty `z` or `parameter-name` is every level or
//! parameter, as `params::parse_z` and `params::parse_parameter_names`
//! read them), and the parameters are a comma-separated text field. With
//! JavaScript the checkboxes replace that field, empty fields are left out,
//! and the URL preview follows every change.

use std::fmt::Write as _;

use api_common::errors;
use axum::extract::{FromRequestParts, MatchedPath, Query, RawPathParams, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use ds_core::edr_engine::{EdrEngine, TrajectoryShape};
use ds_core::html::{display_coordinate, escape, Wanted};
use ds_core::instances::RunInfo;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use crate::handlers::{data_query_variables, execute_query, request_base_url, with_vary, AppState};
use crate::params::{query_formats, EdrFormat, WITHIN_UNITS};

/// The data queries a coordinate parameter places, each with that
/// parameter: `coords`, or a cube's `bbox`. The one list the query forms,
/// the error pages (#1132) and the breadcrumbs read: a new data query
/// route is added here, and its route ends in this name.
pub(crate) const DATA_QUERIES: [(&str, &str); 5] = [
    ("position", "coords"),
    ("area", "coords"),
    ("radius", "coords"),
    ("trajectory", "coords"),
    ("cube", "bbox"),
];

/// The parameter that places a data query, from [`DATA_QUERIES`]. `None`
/// for a query type without one (a location query names its place in the
/// path).
pub(crate) fn location_parameter(query_type: &str) -> Option<&'static str> {
    DATA_QUERIES
        .iter()
        .find(|(query, _)| *query == query_type)
        .map(|(_, parameter)| *parameter)
}

/// The data query a request was routed to, with its placing parameter:
/// the matched route ends in the query's own segment
/// (`/collections/{id}/position`, a run's), never in a path parameter, so a
/// location or an item whose id is `area` is not one. Runs after routing.
pub(crate) fn routed_query(request: &Request) -> Option<(&'static str, &'static str)> {
    let route = request.extensions().get::<MatchedPath>()?.as_str();
    let last = route.rsplit('/').next()?;
    DATA_QUERIES
        .iter()
        .find(|(query, _)| *query == last)
        .copied()
}

/// A routed request's collection `id` and run `instanceId`, as its route
/// captured them, and the request back.
pub(crate) async fn route_ids(request: Request) -> (Request, Option<String>, Option<String>) {
    let (mut parts, body) = request.into_parts();
    let params = RawPathParams::from_request_parts(&mut parts, &())
        .await
        .ok();
    let param = |key: &str| {
        params.as_ref().and_then(|params| {
            params
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_owned())
        })
    };
    let (id, instance) = (param("id"), param("instanceId"));
    (Request::from_parts(parts, body), id, instance)
}

/// What a query URL leaves unencoded besides the unreserved characters: a
/// WKT's parentheses and commas, a datetime's colons and slashes. The
/// form's script encodes the same way, so its preview and this crate's
/// agree byte for byte.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'(')
    .remove(b')')
    .remove(b',')
    .remove(b':')
    .remove(b'/');

/// `action` with `pairs` as its query, encoded like the form's script.
pub(crate) fn query_url(action: &str, pairs: &[(String, String)]) -> String {
    let mut url = action.to_owned();
    for (i, (name, value)) in pairs.iter().enumerate() {
        url.push(if i == 0 { '?' } else { '&' });
        let _ = write!(
            url,
            "{}={}",
            utf8_percent_encode(name, QUERY_VALUE),
            utf8_percent_encode(value, QUERY_VALUE)
        );
    }
    url
}

/// The most times the select lists, newest first: the time-axis length
/// past which the server stops listing an axis (`ds_core::time_axis`).
const MAX_LISTED_TIMES: usize = ds_core::time_axis::MAX_LISTED_VALUES;

/// A data query's form: what its fields offer, read from the engine once.
pub(crate) struct QueryForm {
    query_type: String,
    /// The query end point: `…/collections/{id}/position`, a run's, or a
    /// location's.
    action: String,
    description: String,
    formats: Vec<EdrFormat>,
    /// Parameter id, label and unit, in the engine's order.
    parameters: Vec<(String, String, String)>,
    /// The advertised times, ascending.
    times: Vec<DateTime<Utc>>,
    extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    levels: Vec<f64>,
    level_label: &'static str,
    level_unit: &'static str,
    /// The example's centre (longitude, latitude) and half-size in degrees.
    centre: (f64, f64),
    half: f64,
    trajectory: TrajectoryShape,
    /// Whether a position query defaults to a forecast run's whole series,
    /// a run's length of steps. Elsewhere one time, or [`Self::recent`].
    series: bool,
    /// Whether a position query defaults to the last day of a station
    /// series ([`Self::recent_window`]): an observation table can hold
    /// years of one station, more than one response may carry.
    recent: bool,
}

impl QueryForm {
    /// The form of `query_type` at `action`, on `run`'s valid times when it
    /// is a run's. Reads the engine, so it is built on the query executor.
    pub(crate) fn new(
        engine: &dyn EdrEngine,
        query_type: &str,
        action: String,
        run: Option<&RunInfo>,
    ) -> Self {
        let station = engine.serves_station_series();
        let trajectory = engine.trajectory_shape();
        let descriptions = engine.get_parameter_descriptions();
        let parameters = engine
            .get_parameters()
            .into_iter()
            .map(|id| {
                let (label, unit) = descriptions
                    .get(&id)
                    .map(|d| (d.label.clone(), d.unit.clone()))
                    .unwrap_or_default();
                (id, label, unit)
            })
            .collect();
        let (times, extent) = match run {
            Some(run) => (run.valid_times.clone(), run.temporal_extent()),
            None => (
                engine.get_available_times().unwrap_or_default(),
                engine.get_temporal_extent(),
            ),
        };
        let vertical = engine.get_vertical_extent();
        let (centre, half) = example_area(
            engine
                .get_spatial_extent()
                .and_then(ds_core::geo::crs84_extent),
        );
        let description = data_query_variables(query_type, station, trajectory)
            .and_then(|v| v["description"].as_str().map(str::to_owned))
            .unwrap_or_default();
        Self {
            query_type: query_type.to_owned(),
            action,
            description,
            formats: query_formats(query_type, station, trajectory).to_vec(),
            parameters,
            times,
            extent,
            levels: vertical
                .as_ref()
                .map(|v| v.levels.clone())
                .unwrap_or_default(),
            level_label: vertical.as_ref().map_or("Level", |v| v.label()),
            level_unit: vertical.as_ref().map_or("", |v| v.unit()),
            centre: clamp_centre(centre, half),
            half,
            trajectory,
            series: matches!(query_type, "position" | "locations")
                && (run.is_some() || engine.has_instances()),
            recent: matches!(query_type, "position" | "locations") && station && run.is_none(),
        }
    }

    /// Only `offered` of the query's formats: a location list offers no
    /// PNG, which plots one location.
    pub(crate) fn offering(mut self, offered: &[EdrFormat]) -> Self {
        self.formats.retain(|format| offered.contains(format));
        self
    }

    /// The example placed at the station nearest the extent's centre, on a
    /// station collection, which answers at its stations. Where the form
    /// defaults to the last day and the engine can tell
    /// (`location_time_filter`, in-memory), the nearest station with data
    /// in it: the one nearest the centre may have stopped reporting. Reads
    /// the location inventory: only the bare form, whose coordinates are
    /// the example, asks for it.
    pub(crate) fn near_station(mut self, engine: &dyn EdrEngine) -> Self {
        if !engine.serves_station_series() || location_parameter(&self.query_type).is_none() {
            return self;
        }
        let Ok(inventory) = engine.get_locations() else {
            return self;
        };
        let (x, y) = self.centre;
        let distance =
            |l: &ds_core::model::Location| (l.longitude - x).powi(2) + (l.latitude - y).powi(2);
        let nearest = |candidates: &mut dyn Iterator<Item = &ds_core::model::Location>| {
            candidates
                .min_by(|a, b| distance(a).total_cmp(&distance(b)))
                .map(|l| (l.longitude, l.latitude))
        };
        let window: Vec<ds_core::feature::DatetimeInterval> = self
            .recent
            .then(|| self.recent_window())
            .flatten()
            .map(|(start, end)| ds_core::feature::DatetimeInterval {
                start: Some(start),
                end: Some(end),
            })
            .into_iter()
            .collect();
        let reporting = if window.is_empty() {
            None
        } else {
            engine.location_time_filter(&window)
        };
        let place = reporting
            .as_ref()
            .and_then(|has_data| nearest(&mut inventory.iter().filter(|l| has_data(l))))
            .or_else(|| nearest(&mut inventory.iter()));
        if let Some(place) = place {
            self.centre = clamp_centre(place, self.half);
        }
        self
    }

    /// The query type: `position`, `area`, ….
    pub(crate) fn query_type(&self) -> &str {
        &self.query_type
    }

    /// The query's one-sentence description, as `data_queries` gives it.
    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    /// The example coordinates: a point at the centre, a small square or a
    /// short west–east path around it.
    fn example(&self) -> String {
        let (x, y) = self.centre;
        let h = self.half;
        let c = display_coordinate;
        let (w, s, e, n) = (c(x - h), c(y - h), c(x + h), c(y + h));
        match self.query_type.as_str() {
            "area" => format!("POLYGON(({w} {s},{e} {s},{e} {n},{w} {n},{w} {s}))"),
            "trajectory" => format!("LINESTRING({w} {},{e} {})", c(y), c(y)),
            "cube" => format!("{w},{s},{e},{n}"),
            _ => format!("POINT({} {})", c(x), c(y)),
        }
    }

    /// `start/end` over the advertised extent: "All times", a valid
    /// `datetime` (an empty one is a 400).
    fn all_times(&self) -> Option<String> {
        self.extent
            .map(|(start, end)| format!("{}/{}", time_value(start), time_value(end)))
    }

    /// The last day of the advertised extent, or all of a shorter one.
    fn recent_window(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.extent
            .map(|(start, end)| ((end - chrono::Duration::days(1)).max(start), end))
    }

    /// [`Self::recent_window`] as a `datetime`, where [`Self::recent`].
    fn recent_times(&self) -> Option<String> {
        self.recent_window()
            .filter(|_| self.recent)
            .map(|(start, end)| format!("{}/{}", time_value(start), time_value(end)))
    }

    /// The default time: the last day where [`Self::recent`], the whole
    /// series where [`Self::series`], else the advertised time nearest now
    /// (the latest observation, a forecast's current step), else the
    /// extent's end.
    fn default_datetime(&self) -> Option<String> {
        if self.recent {
            return self.recent_times();
        }
        if self.series {
            return self.all_times();
        }
        let now = Utc::now();
        self.times
            .iter()
            .copied()
            .min_by_key(|t| (*t - now).num_seconds().unsigned_abs())
            .or(self.extent.map(|(_, end)| end))
            .map(time_value)
    }

    /// The default level: every level for a position (a profile), a cube
    /// and a radar cross-section; the first level for a map-like area,
    /// radius or path, which would otherwise multiply its cells by the
    /// levels.
    fn default_z(&self) -> String {
        let all = matches!(self.query_type.as_str(), "position" | "locations" | "cube")
            || (self.query_type == "trajectory"
                && self.trajectory == TrajectoryShape::CrossSection);
        match self.levels.first() {
            Some(level) if !all => level.to_string(),
            _ => String::new(),
        }
    }

    /// The form's field values: the request's, and where `defaults` the
    /// example's for what it leaves out (the bare form); else what leaving
    /// it out means (a data page's request).
    fn values(&self, pairs: &[(String, String)], defaults: bool) -> Values {
        let given = |name: &str| {
            pairs
                .iter()
                .find(|(k, v)| k == name && !v.trim().is_empty())
                .map(|(_, v)| v.trim().to_owned())
        };
        let location = location_parameter(&self.query_type)
            .map(|name| (name, given(name).unwrap_or_else(|| self.example())));
        let radius = self.query_type == "radius";
        let datetime = given("datetime").or_else(|| {
            if defaults {
                self.default_datetime()
            } else {
                self.all_times()
            }
        });
        let z = given("z").unwrap_or_else(|| {
            if defaults {
                self.default_z()
            } else {
                String::new()
            }
        });
        // The request's names as the collection spells them: the query
        // matches names ignoring ASCII case (`select_parameters`), so must
        // the checkboxes, or a re-run would drop the filter.
        let parameters = match given("parameter-name") {
            Some(names) => names
                .split(',')
                .map(|name| {
                    let name = name.trim();
                    self.parameters
                        .iter()
                        .find(|(id, _, _)| id.eq_ignore_ascii_case(name))
                        .map_or_else(|| name.to_owned(), |(id, _, _)| id.clone())
                })
                .collect(),
            None if defaults => self
                .parameters
                .iter()
                .take(1)
                .map(|p| p.0.clone())
                .collect(),
            None => Vec::new(),
        };
        let format = given("f")
            .and_then(|f| {
                let f = f.replace(' ', "+");
                self.formats.iter().copied().find(|format| {
                    format.name().eq_ignore_ascii_case(&f)
                        || format.media_type().eq_ignore_ascii_case(&f)
                })
            })
            .unwrap_or(EdrFormat::Html);
        const OWN: [&str; 8] = [
            "coords",
            "bbox",
            "within",
            "within-units",
            "datetime",
            "z",
            "parameter-name",
            "f",
        ];
        let extra = pairs
            .iter()
            .filter(|(k, v)| !k.is_empty() && !OWN.contains(&k.as_str()) && !v.trim().is_empty())
            .cloned()
            .collect();
        Values {
            location,
            within: radius.then(|| given("within").unwrap_or_else(|| "50".into())),
            within_units: radius.then(|| given("within-units").unwrap_or_else(|| "km".into())),
            datetime,
            z,
            parameters,
            extra,
            format,
        }
    }

    /// The URL the form's values make: the request it runs.
    fn url(&self, values: &Values) -> String {
        query_url(&self.action, &values.pairs())
    }

    /// The URL of the example query in `format`: the bare form page's JSON
    /// switch.
    pub(crate) fn example_url(&self, pairs: &[(String, String)], format: EdrFormat) -> String {
        let mut values = self.values(pairs, true);
        values.format = format;
        self.url(&values)
    }

    /// The form, filled from `pairs` (with the example's defaults for what
    /// they leave out where `defaults`), as trusted markup: every value is
    /// escaped. `heading` titles its panel.
    pub(crate) fn html(&self, pairs: &[(String, String)], defaults: bool, heading: &str) -> String {
        let values = self.values(pairs, defaults);
        let url = self.url(&values);
        let mut out = format!(
            "<form class=\"panel spaced edr-query\" data-edr-query method=\"get\" action=\"{}\" aria-label=\"{}\"><div class=\"panel-head\"><h2>{}</h2><span class=\"chip\">GET</span></div><div class=\"panel-body\"><div class=\"edr-query-fields\">",
            escape(&self.action),
            escape(&format!("{} query", capitalised(&self.query_type))),
            escape(heading)
        );
        if let Some((name, value)) = &values.location {
            let (label, hint) = self.location_text();
            let _ = write!(
                out,
                "<label class=\"field edr-wide\"><span>{label} <code>{name}</code></span><span class=\"hint\">{hint}</span><input name=\"{name}\" value=\"{}\" required spellcheck=\"false\" autocomplete=\"off\"></label>",
                escape(value)
            );
        }
        if let (Some(within), Some(units)) = (&values.within, &values.within_units) {
            let _ = write!(
                out,
                "<label class=\"field\"><span>Distance <code>within</code></span><span class=\"hint\">At most 1000 km</span><input name=\"within\" type=\"number\" min=\"0\" step=\"any\" value=\"{}\" required></label><label class=\"field\"><span>Unit <code>within-units</code></span><span class=\"hint\">Kilometres, metres or miles</span><select name=\"within-units\">{}</select></label>",
                escape(within),
                options(WITHIN_UNITS.iter().map(|u| (u.to_string(), u.to_string())), units)
            );
        }
        self.datetime_fields(&mut out, values.datetime.as_deref());
        if !self.levels.is_empty() || !values.z.is_empty() {
            let unit = self.level_unit;
            let mut choices = vec![(String::new(), "All levels".to_owned())];
            choices.extend(self.levels.iter().map(|level| {
                let value = level.to_string();
                let label = format!("{value} {unit}").trim_end().to_owned();
                (value, label)
            }));
            let _ = write!(
                out,
                "<label class=\"field\"><span>{} <code>z</code></span><span class=\"hint\">{} advertised levels</span><select name=\"z\">{}</select></label>",
                escape(self.level_label),
                self.levels.len(),
                options(choices.into_iter(), &values.z)
            );
        }
        out.push_str("</div>");
        self.parameter_fields(&mut out, &values.parameters);
        for (name, value) in &values.extra {
            let _ = write!(
                out,
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                escape(name),
                escape(value)
            );
        }
        // The page first: what a browser user wants back.
        let formats = self
            .formats
            .iter()
            .filter(|f| **f == EdrFormat::Html)
            .chain(self.formats.iter().filter(|f| **f != EdrFormat::Html))
            .map(|f| (f.name().to_owned(), format_label(*f).to_owned()));
        let _ = write!(
            out,
            "<div class=\"edr-query-run\"><label class=\"field\"><span>Format <code>f</code></span><select name=\"f\">{}</select></label><button class=\"btn primary\" type=\"submit\">Run query</button></div><div class=\"edr-query-url\"><span class=\"method\">GET</span><code data-query-url>{}</code><button type=\"button\" class=\"btn small enhanced\" data-copy=\"{}\" data-query-copy>Copy URL</button></div></div></form>",
            options(formats, values.format.name()),
            escape(&url),
            escape(&url)
        );
        out
    }

    /// The coordinates field's label and hint.
    fn location_text(&self) -> (&'static str, &'static str) {
        match self.query_type.as_str() {
            "area" => (
                "Polygon",
                "WKT POLYGON((lon lat, …)) in CRS84 degrees, the first point repeated last",
            ),
            "radius" => ("Centre", "WKT POINT(longitude latitude) in CRS84 degrees"),
            "trajectory" if self.trajectory == TrajectoryShape::CrossSection => (
                "Path",
                "WKT LINESTRING(lon lat, lon lat, …) in CRS84 degrees",
            ),
            "trajectory" => (
                "Path",
                "WKT LINESTRING(lon lat, …) in CRS84 degrees; LINESTRING Z, M or ZM carries each vertex's level or time",
            ),
            "cube" => ("Bounding box", "west,south,east,north in CRS84 degrees"),
            _ => (
                "Point",
                "WKT POINT(longitude latitude) in CRS84 degrees; MULTIPOINT((lon lat),(lon lat)) for several",
            ),
        }
    }

    /// The time select (an advertised axis), else a text field (an extent
    /// only), and a free field that overrides either.
    fn datetime_fields(&self, out: &mut String, value: Option<&str>) {
        if value.is_none() && self.extent.is_none() && self.times.is_empty() {
            return;
        }
        let value = value.unwrap_or_default();
        let listed = self.times.len().min(MAX_LISTED_TIMES);
        if self.times.is_empty() {
            let hint = match self.extent {
                Some((start, end)) => format!(
                    "An instant, start/end or a comma list, from {} to {}",
                    time_label(start),
                    time_label(end)
                ),
                None => "An instant, start/end or a comma list".to_owned(),
            };
            let _ = write!(
                out,
                "<label class=\"field edr-wide\"><span>Time <code>datetime</code></span><span class=\"hint\">{}</span><input name=\"datetime\" value=\"{}\" spellcheck=\"false\" autocomplete=\"off\"></label>",
                escape(&hint),
                escape(value)
            );
            return;
        }
        let span = match (self.times.first(), self.times.last()) {
            (Some(&first), Some(&last)) if first != last => {
                format!(", {} to {}", time_label(first), time_label(last))
            }
            _ => String::new(),
        };
        let hint = if listed < self.times.len() {
            format!(
                "The {listed} newest of {} advertised times{span}",
                self.times.len()
            )
        } else {
            format!("{} advertised times{span}", self.times.len())
        };
        let mut choices = Vec::with_capacity(listed + 2);
        // The span is in the hint; the option stays short enough to read.
        let all = self.all_times();
        if let Some(all) = &all {
            choices.push((all.clone(), format!("All {} times", self.times.len())));
        }
        if let Some(recent) = self
            .recent_times()
            .filter(|recent| Some(recent) != all.as_ref())
        {
            choices.push((recent, "Last 24 hours".to_owned()));
        }
        choices.extend(
            self.times
                .iter()
                .rev()
                .take(listed)
                .enumerate()
                .map(|(i, &t)| {
                    let mut label = time_label(t);
                    if i == 0 {
                        label.push_str(" · latest");
                    }
                    (time_value(t), label)
                }),
        );
        let _ = write!(
            out,
            "<label class=\"field\"><span>Time <code>datetime</code></span><span class=\"hint\">{}</span><select name=\"datetime\">{}</select></label><label class=\"field enhanced\"><span>Other time</span><span class=\"hint\">An instant, start/end or a comma list; replaces the selection</span><input data-custom=\"datetime\" spellcheck=\"false\" autocomplete=\"off\" placeholder=\"{}\"></label>",
            escape(&hint),
            options(choices.into_iter(), value),
            escape(
                &self
                    .times
                    .last()
                    .map(|t| format!("e.g. {}", time_value(*t)))
                    .unwrap_or_default()
            )
        );
    }

    /// A checkbox per parameter (with JavaScript), and the comma-separated
    /// text field a page without it submits.
    fn parameter_fields(&self, out: &mut String, checked: &[String]) {
        if self.parameters.is_empty() {
            return;
        }
        let count = self.parameters.len();
        let _ = write!(
            out,
            "<fieldset class=\"edr-parameters enhanced\" data-parameters><legend>Parameters <code>parameter-name</code></legend><p class=\"hint\">{count} {}. Leave all unchecked for every one.</p><div class=\"edr-parameter-list\">",
            if count == 1 { "parameter" } else { "parameters" }
        );
        for (id, label, unit) in &self.parameters {
            // The label, then the id when it differs and the unit.
            let label = if label.trim().is_empty() { id } else { label };
            let detail: Vec<&str> = [(label != id).then_some(id.as_str()), Some(unit.as_str())]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .collect();
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!("<small>{}</small>", escape(&detail.join(" · ")))
            };
            let _ = write!(
                out,
                "<label><input type=\"checkbox\" data-parameter value=\"{}\"{}> <span>{}{detail}</span></label>",
                escape(id),
                if checked.contains(id) { " checked" } else { "" },
                escape(label)
            );
        }
        let _ = write!(
            out,
            "</div></fieldset><noscript><label class=\"field\"><span>Parameters <code>parameter-name</code></span><span class=\"hint\">Comma-separated, of {}; leave empty for every one</span><input name=\"parameter-name\" value=\"{}\"></label></noscript>",
            escape(
                &self
                    .parameters
                    .iter()
                    .map(|p| p.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            escape(&checked.join(","))
        );
    }
}

/// A form's resolved field values.
struct Values {
    /// The placing parameter and its value: `coords` or `bbox`.
    location: Option<(&'static str, String)>,
    within: Option<String>,
    within_units: Option<String>,
    datetime: Option<String>,
    /// Empty: every level.
    z: String,
    /// Empty: every parameter.
    parameters: Vec<String>,
    /// The request's other parameters (`crs`, `limit`, `width`, …), kept.
    extra: Vec<(String, String)>,
    format: EdrFormat,
}

impl Values {
    /// The query's parameters in the form's order, empty ones left out:
    /// what the form's script submits.
    fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        if let Some((name, value)) = &self.location {
            pairs.push(((*name).to_owned(), value.clone()));
        }
        if let Some(within) = &self.within {
            pairs.push(("within".into(), within.clone()));
        }
        if let Some(units) = &self.within_units {
            pairs.push(("within-units".into(), units.clone()));
        }
        if let Some(datetime) = self.datetime.as_ref().filter(|d| !d.is_empty()) {
            pairs.push(("datetime".into(), datetime.clone()));
        }
        if !self.z.is_empty() {
            pairs.push(("z".into(), self.z.clone()));
        }
        if !self.parameters.is_empty() {
            pairs.push(("parameter-name".into(), self.parameters.join(",")));
        }
        pairs.extend(self.extra.iter().cloned());
        pairs.push(("f".into(), self.format.name().to_owned()));
        pairs
    }
}

/// `<option>`s for `(value, label)` choices, `selected` selected; a value
/// that is none of them (a request's own) is listed first, selected.
fn options(choices: impl Iterator<Item = (String, String)>, selected: &str) -> String {
    let mut out = String::new();
    let mut found = false;
    for (value, label) in choices {
        let chosen = value == selected;
        found |= chosen;
        let _ = write!(
            out,
            "<option value=\"{}\"{}>{}</option>",
            escape(&value),
            if chosen { " selected" } else { "" },
            escape(&label)
        );
    }
    // Nothing to select (no value): the browser selects the first.
    if found || selected.is_empty() {
        out
    } else {
        format!(
            "<option value=\"{}\" selected>{} · this request</option>{out}",
            escape(selected),
            escape(selected)
        )
    }
}

/// A format's name in the select.
fn format_label(format: EdrFormat) -> &'static str {
    match format {
        EdrFormat::Html => "HTML page",
        EdrFormat::CoverageJson => "CoverageJSON",
        EdrFormat::GeoJson => "GeoJSON",
        EdrFormat::Png => "PNG plot",
        EdrFormat::Csv => "CSV",
    }
}

/// A time as a `datetime` value: RFC 3339 UTC with `Z`, fractions kept.
fn time_value(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// A time as a select shows it: `2026-10-17 12:00 UTC`.
pub(crate) fn time_label(t: DateTime<Utc>) -> String {
    if t.second() == 0 && t.nanosecond() == 0 {
        t.format("%Y-%m-%d %H:%M UTC").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    }
}

/// A raw query string's pairs, decoded (`+` as a space).
pub(crate) fn query_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    };
    raw.unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

/// What a data page answers, for its heading and tab title (#1138): the
/// query and where, from its coordinates: `Position at 60.16667° N,
/// 24.93333° E`, `Area 59° N to 61° N, 24° E to 26° E`, `Path from … to …`.
/// Coordinates it cannot read leave the query's name alone.
pub(crate) fn page_title(query_type: &str, pairs: &[(String, String)]) -> String {
    let value = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    };
    let fallback = || format!("{} query", capitalised(query_type));
    match query_type {
        "position" => match wkt_points(value("coords"))[..] {
            [point] => format!("Position at {}", point_text(point)),
            [] => fallback(),
            ref points => format!("Position at {} points", points.len()),
        },
        "radius" => match wkt_points(value("coords"))[..] {
            [point] => format!(
                "Within {} {} of {}",
                value("within").trim(),
                value("within-units").trim(),
                point_text(point)
            ),
            _ => fallback(),
        },
        "area" => match bounds(&wkt_points(value("coords"))) {
            Some(bounds) => format!("Area {}", bounds_text(bounds)),
            None => fallback(),
        },
        "trajectory" => match wkt_points(value("coords"))[..] {
            [first, .., last] => {
                format!("Path from {} to {}", point_text(first), point_text(last))
            }
            _ => fallback(),
        },
        "cube" => {
            let numbers: Vec<f64> = value("bbox")
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            match numbers[..] {
                [w, s, e, n] | [w, s, _, e, n, _] => {
                    format!("Cube {}", bounds_text((w, s, e, n)))
                }
                _ => fallback(),
            }
        }
        _ => fallback(),
    }
}

/// The (longitude, latitude) of each vertex of a WKT geometry, whatever its
/// dimension (`Z`, `M` and `ZM` vertices carry 3 or 4 numbers). Empty for
/// text that is not one.
fn wkt_points(wkt: &str) -> Vec<(f64, f64)> {
    let upper = wkt.to_ascii_uppercase();
    let head = upper.split('(').next().unwrap_or_default();
    let dimensions = match head.split_whitespace().nth(1) {
        Some("ZM") => 4,
        Some("Z" | "M") => 3,
        _ => 2,
    };
    let Some(body) = upper.find('(').map(|i| &upper[i..]) else {
        return Vec::new();
    };
    let numbers: Option<Vec<f64>> = body
        .split(|c: char| c == '(' || c == ')' || c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| t.parse().ok())
        .collect();
    match numbers {
        Some(numbers) if numbers.len() % dimensions == 0 => {
            numbers.chunks(dimensions).map(|c| (c[0], c[1])).collect()
        }
        _ => Vec::new(),
    }
}

/// The west, south, east and north of `points`.
fn bounds(points: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    let (&(x, y), rest) = points.split_first()?;
    Some(rest.iter().fold((x, y, x, y), |(w, s, e, n), &(x, y)| {
        (w.min(x), s.min(y), e.max(x), n.max(y))
    }))
}

/// `60.16667° N, 24.93333° E`.
pub(crate) fn point_text((lon, lat): (f64, f64)) -> String {
    format!("{}, {}", latitude_text(lat), longitude_text(lon))
}

/// `59° N to 61° N, 24° E to 26° E`.
fn bounds_text((w, s, e, n): (f64, f64, f64, f64)) -> String {
    format!(
        "{} to {}, {} to {}",
        latitude_text(s),
        latitude_text(n),
        longitude_text(w),
        longitude_text(e)
    )
}

fn latitude_text(lat: f64) -> String {
    let hemisphere = if lat < 0.0 { "S" } else { "N" };
    format!("{}° {hemisphere}", display_coordinate(lat.abs()))
}

fn longitude_text(lon: f64) -> String {
    let hemisphere = if lon < 0.0 { "W" } else { "E" };
    format!("{}° {hemisphere}", display_coordinate(lon.abs()))
}

/// `position` → `Position`.
pub(crate) fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The example's centre and half-size from a CRS84 extent: the extent's
/// middle (across the antimeridian when `west > east`), a twentieth of its
/// narrower side between 0.05° and 1°. No extent: 0°, 0°.
fn example_area(extent: Option<[f64; 4]>) -> ((f64, f64), f64) {
    let Some([w, s, e, n]) = extent else {
        return ((0.0, 0.0), 0.5);
    };
    let width = if w <= e { e - w } else { e + 360.0 - w };
    let mut lon = w + width / 2.0;
    if lon > 180.0 {
        lon -= 360.0;
    }
    let half = (width.min(n - s) / 20.0).clamp(0.05, 1.0);
    ((lon, (s + n) / 2.0), half)
}

/// The centre moved inside the CRS84 domain by `half`, so the example's
/// square and path stay valid coordinates.
fn clamp_centre((lon, lat): (f64, f64), half: f64) -> (f64, f64) {
    (
        lon.clamp(-180.0 + half, 180.0 - half),
        lat.clamp(-90.0 + half, 90.0 - half),
    )
}

/// The page's styles: the form's grid and its URL line, in the workbench's
/// tokens. Added to the head of every EDR data and form page.
pub(crate) const STYLE: &str = "<style>.edr-query-fields{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:4px 16px}.edr-query-fields .edr-wide{grid-column:1/-1}.edr-query input:not([type=checkbox]),.edr-query select{width:100%}.edr-parameters{border:1px solid var(--line);border-radius:8px;padding:12px 16px;margin:0 0 16px}.edr-parameters legend{font-weight:600;padding:0 6px}.edr-parameters .hint{color:var(--muted);font-size:14px;margin:0 0 8px}.edr-parameter-list{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:6px 16px;max-height:320px;overflow:auto}.edr-parameter-list label{display:flex;gap:10px;align-items:flex-start;font-weight:400;margin:0}.edr-parameter-list input{flex:0 0 auto;width:18px;height:18px;min-height:0;padding:0;margin:3px 0 0}.edr-parameter-list small{display:block;color:var(--muted);overflow-wrap:anywhere}.edr-query-run{display:flex;gap:16px;align-items:flex-end;flex-wrap:wrap}.edr-query-run .field{flex:0 1 260px;margin:0}.edr-query-url{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-top:16px}.edr-query-url code{flex:1 1 300px;overflow-wrap:anywhere}.edr-facts{display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr));gap:12px 20px;margin:0}.edr-facts dt{font-size:14px;color:var(--muted)}.edr-facts dd{margin:2px 0 0;font-weight:600;overflow-wrap:anywhere}.edr-chart{margin:24px 0 0}.edr-chart img{display:block;width:100%;max-width:960px;height:auto;margin:0 auto;background:#fff;border-radius:6px}[data-locations] tbody th{min-width:14ch}[data-locations] tbody th a{overflow-wrap:normal}.edr-chart figcaption{color:var(--muted);font-size:14px;margin-top:8px}.edr-more>summary{cursor:pointer;font-weight:600;padding:14px 0}.edr-filter{display:flex;gap:12px;align-items:center;flex-wrap:wrap;margin:0 0 12px}.edr-filter input{flex:1 1 240px}</style>";

/// The form's script: the parameter checkboxes as one `parameter-name`, a
/// free time over the select, empty fields left out, the URL preview and
/// its Copy button kept current, and the submit navigating to that URL.
pub(crate) const SCRIPT: &str = "<script>(()=>{const enc=v=>encodeURIComponent(v).replace(/[!'*]/g,c=>'%'+c.charCodeAt(0).toString(16).toUpperCase()).replace(/%2C/gi,',').replace(/%3A/gi,':').replace(/%2F/gi,'/');document.querySelectorAll('form[data-edr-query]').forEach(form=>{const preview=form.querySelector('[data-query-url]'),copy=form.querySelector('[data-query-copy]');const build=()=>{const pairs=[];for(const el of form.elements){if(el.matches('fieldset[data-parameters]')){const names=[...el.querySelectorAll('input[data-parameter]:checked')].map(c=>c.value);if(names.length)pairs.push(['parameter-name',names.join(',')]);continue}if(!el.name)continue;let value=el.value;const custom=form.querySelector('[data-custom=\"'+el.name+'\"]');if(custom&&custom.value.trim())value=custom.value;value=value.trim();if(value!=='')pairs.push([el.name,value])}const url=new URL(form.action,location.href);return url.origin+url.pathname+(pairs.length?'?'+pairs.map(([k,v])=>enc(k)+'='+enc(v)).join('&'):'')};const update=()=>{const href=build();if(preview)preview.textContent=href;if(copy)copy.dataset.copy=href};form.addEventListener('input',update);form.addEventListener('change',update);form.addEventListener('submit',event=>{if(!form.reportValidity())return;event.preventDefault();location.href=build()});update()})})()</script>";

/// Answer a data query that asked for HTML without its coordinates with
/// its form (#1138), status 200. Runs after routing, so the route's `id`
/// (and `instanceId`) are known; anything it cannot place (an unknown
/// collection or run, a query the collection does not offer) goes on to the
/// handler, whose error page says so.
pub(crate) async fn bare_query_form(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some((query_type, parameter)) = routed_query(&request) else {
        return next.run(request).await;
    };
    let query_type = query_type.to_owned();
    if errors::wanted(request.uri().query(), request.headers()) != Wanted::Html {
        return next.run(request).await;
    }
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(request.uri())
        .map(|Query(pairs)| pairs)
        .unwrap_or_default();
    if pairs
        .iter()
        .any(|(k, v)| k == parameter && !v.trim().is_empty())
    {
        return next.run(request).await;
    }
    let (request, id, instance) = route_ids(request).await;
    let snapshot = state.load_full();
    let Some((engine, config)) = id.as_ref().and_then(|id| {
        Some((
            snapshot.engines.get(id)?.clone(),
            snapshot.collections.get(id)?.clone(),
        ))
    }) else {
        return next.run(request).await;
    };
    if !engine.supported_query_types().contains(&query_type) {
        return next.run(request).await;
    }
    let reference_time = match instance.as_deref() {
        None => None,
        Some(iid) => match ds_core::instances::parse_instance_id(iid) {
            Some(rt) if engine.has_instances() => Some(rt),
            _ => return next.run(request).await,
        },
    };
    let base = request_base_url(&snapshot, request.headers());
    let built = execute_query(false, move |_budget| {
        let run = match reference_time {
            Some(rt) => match engine.find_instance(rt) {
                Some(run) => Some(run),
                None => return Ok(None),
            },
            None => None,
        };
        let mut action = format!("{base}/edr/collections/{}", config.id);
        if let Some(run) = &run {
            let _ = write!(action, "/instances/{}", run.instance_id());
        }
        let _ = write!(action, "/{query_type}");
        let form = QueryForm::new(engine.as_ref(), &query_type, action, run.as_ref())
            .near_station(engine.as_ref());
        Ok(Some(crate::html::form_page(
            &form,
            &pairs,
            &base,
            &config.id,
            &config.title,
            run.as_ref().map(RunInfo::instance_id).as_deref(),
        )))
    })
    .await;
    match built {
        Ok(Some(page)) => with_vary(crate::html::response(page)),
        // An unknown run: the handler's 404 page names it.
        Ok(None) => next.run(request).await,
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_titles_name_the_place() {
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(
            page_title(
                "position",
                &pairs(&[("coords", "POINT(24.933333333333334 60.1)")])
            ),
            "Position at 60.1° N, 24.93333° E"
        );
        assert_eq!(
            page_title("position", &pairs(&[("coords", "MULTIPOINT((1 2),(3 4))")])),
            "Position at 2 points"
        );
        assert_eq!(
            page_title(
                "radius",
                &pairs(&[
                    ("coords", "POINT(-3.5 -10)"),
                    ("within", "50"),
                    ("within-units", "km")
                ])
            ),
            "Within 50 km of 10° S, 3.5° W"
        );
        assert_eq!(
            page_title(
                "area",
                &pairs(&[("coords", "POLYGON((24 59,26 59,26 61,24 61,24 59))")])
            ),
            "Area 59° N to 61° N, 24° E to 26° E"
        );
        assert_eq!(
            page_title(
                "trajectory",
                &pairs(&[("coords", "LINESTRING Z(24 60 1000,25 61 850)")])
            ),
            "Path from 60° N, 24° E to 61° N, 25° E"
        );
        assert_eq!(
            page_title("cube", &pairs(&[("bbox", "24,59,26,61")])),
            "Cube 59° N to 61° N, 24° E to 26° E"
        );
        assert_eq!(
            page_title("area", &pairs(&[("coords", "garbage")])),
            "Area query"
        );
    }

    #[test]
    fn query_urls_keep_wkt_and_times_readable() {
        let url = query_url(
            "https://x/edr/collections/c/position",
            &[
                ("coords".into(), "POINT(24.9 60.2)".into()),
                ("datetime".into(), "2026-01-01T00:00:00+02:00/..".into()),
                ("parameter-name".into(), "a b,c&d".into()),
            ],
        );
        assert_eq!(
            url,
            "https://x/edr/collections/c/position?coords=POINT(24.9%2060.2)&datetime=2026-01-01T00:00:00%2B02:00/..&parameter-name=a%20b,c%26d"
        );
    }

    #[test]
    fn the_example_stays_inside_the_domain() {
        // Across the antimeridian: the middle of 170°E … 170°W is 180°.
        let ((lon, lat), half) = example_area(Some([170.0, -10.0, -170.0, 10.0]));
        assert!((lon - 180.0).abs() < 1e-9 && lat == 0.0, "{lon} {lat}");
        let (lon, _) = clamp_centre((lon, lat), half);
        assert!(lon + half <= 180.0);
        // A small extent keeps a small square; a large one at most 1°.
        assert_eq!(example_area(Some([24.0, 60.0, 25.0, 61.0])).1, 0.05);
        assert_eq!(example_area(Some([-180.0, -90.0, 180.0, 90.0])).1, 1.0);
        assert_eq!(example_area(None), ((0.0, 0.0), 0.5));
    }

    #[test]
    fn a_request_value_outside_the_choices_is_kept_selected() {
        let html = options(
            [("a".to_owned(), "A".to_owned())].into_iter(),
            "2026-01-01T00:00:00Z/..",
        );
        assert!(html.starts_with(
            "<option value=\"2026-01-01T00:00:00Z/..\" selected>2026-01-01T00:00:00Z/.. · this request</option>"
        ));
        let html = options([("a".to_owned(), "A".to_owned())].into_iter(), "a");
        assert_eq!(html, "<option value=\"a\" selected>A</option>");
    }
}
