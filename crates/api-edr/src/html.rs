//! HTML representations of EDR data responses (#971).
//!
//! EDR 1.2 `/req/html/definition` A: every 200 of every operation supports
//! `text/html`. `/req/html/content` A: that page holds all the information
//! of the response's schema, and every link as an `<a>`. A data query's page
//! is therefore its CoverageJSON document (`coverage_response_to_json`, the
//! value the JSON representation serialises, so the two cannot drift) laid
//! out as tables: every parameter, each coverage's axes, referencing and
//! foreign members, and every range value. A grid range is one y × x matrix
//! per parameter and leading (t, z) index, one cell per value; any other
//! range is one row per domain position. The page has no cap of its own:
//! the values caps that bound the CoverageJSON (`MAX_AREA_VALUES`,
//! `MAX_POSITION_VALUES`, …) bound it too, at two to three times the JSON's
//! bytes (a rounded number's cell keeps its exact form in `title`, #1138).
//! The data comes first: a summary, the query's plot where it has one, the
//! values; then the request as its form, the links, parameters and document
//! members folded below.
//! It is written into one buffer reserved up front from the value count
//! ([`DataPage::render_into`]), so a large page never holds a second copy
//! of itself or regrows by doubling, and a data page's buffer is admitted
//! by the EDR response budget at that size before it renders
//! ([`DataPage::render_admitted`], #1041). The `/locations` list and
//! `items` render their GeoJSON as a feature table, the list through the
//! same budgeted writer as its GeoJSON.
//!
//! Every value is escaped, and every href goes through the workbench's
//! `anchor` (`safe_href`).

use std::fmt::Write as _;
use std::io;

use api_common::workbench::{self as ui, i18n_text, unit_text, Page, Surface};
use axum::body::Bytes;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use ds_core::html::{display_coordinate, display_value, escape};
use ds_core::model::CoverageResponse;
use serde_json::Value;

use crate::geojson::{encode_path_segment, FeatureSummary};
use crate::response::{
    coverage_response_to_json, LocationRows, LocationsContext, COVERAGE_JSON_MEDIA_TYPE,
};
use crate::response_budget::Failure;

/// The `Content-Type` of every HTML data page.
pub(crate) const CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// One link of an HTML page, rendered as an `<a>` with its relation, media
/// type and title.
pub(crate) struct Link {
    pub href: String,
    pub rel: String,
    pub kind: String,
    pub title: String,
}

/// What an HTML data page shows besides the response: where it sits, the
/// request, and the representation's links.
#[derive(Default)]
pub(crate) struct DataPage<'a> {
    /// The external base URL: workbench assets and navigation.
    pub base: &'a str,
    pub collection_id: &'a str,
    pub collection_title: &'a str,
    /// What the page answers, e.g. `Position at 60.2° N, 24.9° E`: the
    /// heading and the tab title, each followed by the collection's title
    /// (#1138).
    pub title: String,
    /// The request's raw query string, listed decoded (without `f`).
    pub raw_query: Option<&'a str>,
    /// The default representation's URL: the page's JSON switch.
    pub json_url: String,
    /// This representation's links: `self`, an `alternate` per other
    /// format, and the others the JSON representation carries.
    pub links: Vec<Link>,
    /// The request as an editable form (#1138): trusted markup from
    /// `QueryForm::html`.
    pub form: Option<String>,
    /// The same query as a PNG plot, shown above the values (#1138).
    pub chart: Option<Chart>,
    /// What the summary names first, as `(label, value)`: the place, the
    /// model run.
    pub facts: Vec<(String, String)>,
}

/// A data page's plot: the query's `f=PNG` URL and the image's alt text.
pub(crate) struct Chart {
    pub src: String,
    pub alt: String,
}

impl DataPage<'_> {
    /// The workbench page's markup before and after its body, with the EDR
    /// pages' styles in its head.
    fn shell(&self) -> (String, String) {
        self.shell_with(crate::query_form::STYLE)
    }

    /// [`Self::shell`] with `head` (trusted markup) in the page's head.
    fn shell_with(&self, head: &str) -> (String, String) {
        // The shell's text is escaped, so no response value can produce this.
        const SLOT: &str = "<!--edr-body-->";
        let root = format!("{}{}", self.base, api_common::mounts::EDR);
        let collection_url = format!("{root}/collections/{}", self.collection_id);
        let mut labels = vec![(collection_url.clone(), self.collection_title.to_owned())];
        labels.extend(crumb_labels(&collection_url, &self.json_url));
        let labels: Vec<(&str, &str)> = labels
            .iter()
            .map(|(href, label)| (href.as_str(), label.as_str()))
            .collect();
        let page = Page {
            surface: Surface {
                base: self.base,
                root: &root,
                api: "edr",
            },
            // The tab names the query and the collection (#1131).
            title: &self.heading(),
            json_url: &self.json_url,
        }
        .render_with_breadcrumbs(SLOT, head, &labels);
        match page.split_once(SLOT) {
            Some((head, tail)) => (head.to_owned(), tail.to_owned()),
            None => (page, String::new()),
        }
    }

    /// The page's heading and tab title: what it answers and the
    /// collection's title.
    fn heading(&self) -> String {
        format!("{} · {}", self.title, self.collection_title)
    }

    /// The page, its body (trusted markup of escaped values) written by
    /// `body` straight into the one buffer, reserved for `estimate` body
    /// bytes: never a second copy of the body.
    fn render_into(&self, estimate: usize, body: impl FnOnce(&mut String)) -> String {
        let (head, tail) = self.shell();
        let capacity = head.len() + estimate + tail.len();
        fill(&head, &tail, capacity, body)
    }

    /// [`Self::render_into`] admitted by the EDR response budget (#1041):
    /// the whole buffer is reserved before it is allocated, so a page over
    /// the budget fails before it renders, and stays charged until the
    /// body is delivered or dropped.
    fn render_admitted(
        &self,
        estimate: usize,
        body: impl FnOnce(&mut String),
    ) -> Result<Bytes, Failure> {
        let (head, tail) = self.shell();
        let capacity = head
            .len()
            .saturating_add(estimate)
            .saturating_add(tail.len());
        let reservation = crate::response_budget::reserve(capacity)?;
        reservation.into_bytes(fill(&head, &tail, capacity, body).into_bytes())
    }
}

/// A page in one buffer of `capacity` bytes: `head`, what `body` writes,
/// `tail`.
fn fill(head: &str, tail: &str, capacity: usize, body: impl FnOnce(&mut String)) -> String {
    let mut out = String::with_capacity(capacity);
    out.push_str(head);
    body(&mut out);
    out.push_str(tail);
    out
}

/// The breadcrumb labels below a collection, from a page's URL: a query
/// end point as `Position query`, a model run as `Run …` (#1138). The query
/// crumb opens the query's form.
fn crumb_labels(collection_url: &str, url: &str) -> Vec<(String, String)> {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let Some(rest) = path.strip_prefix(collection_url) else {
        return Vec::new();
    };
    let mut labels = Vec::new();
    let mut href = collection_url.to_owned();
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    for (i, segment) in segments.iter().enumerate() {
        href.push('/');
        href.push_str(segment);
        // A query's own segment follows the collection or a run, never a
        // location: a location whose id is `area` is a location.
        let query_place = i == 0 || (i == 2 && segments[0] == "instances");
        let label = match (i, *segment) {
            (_, query) if query_place && crate::query_form::location_parameter(query).is_some() => {
                format!("{} query", crate::query_form::capitalised(query))
            }
            (0, "locations") => "Locations".to_owned(),
            (1, run) if segments[0] == "instances" => format!(
                "Run {}",
                percent_encoding::percent_decode_str(run).decode_utf8_lossy()
            ),
            _ => continue,
        };
        labels.push((href.clone(), label));
    }
    labels
}

/// An HTML page as a response.
pub(crate) fn response(body: impl Into<axum::body::Body>) -> Response {
    ([(header::CONTENT_TYPE, CONTENT_TYPE)], body.into()).into_response()
}

/// The location identity of a station-series result (#988): what its EDR
/// GeoJSON form says besides the series, one row per coverage in order, and
/// `numberMatched` (`None` when uncounted; `numberReturned` is the rows).
pub(crate) struct Locations {
    pub rows: Vec<FeatureSummary>,
    pub number_matched: Option<usize>,
}

/// The HTML page of a data query's result: its CoverageJSON, all of it,
/// and on a station collection which location each coverage is. Admitted
/// by the EDR response budget at its estimated size before it renders.
pub(crate) fn coverage_page(
    result: &CoverageResponse,
    page: &DataPage,
    locations: Option<&Locations>,
) -> Result<Bytes, Failure> {
    let doc = coverage_response_to_json(result);
    let (coverages, estimate) = coverage_layout(&doc, page, locations);
    page.render_admitted(estimate, |body| {
        coverage_body(body, &doc, coverages, page, locations)
    })
}

/// A coverage page's coverages, and the bytes its body takes from above.
fn coverage_layout<'d>(
    doc: &'d Value,
    page: &DataPage,
    locations: Option<&Locations>,
) -> (Vec<&'d Value>, usize) {
    let coverages: Vec<&Value> = match doc["coverages"].as_array() {
        Some(coverages) => coverages.iter().collect(),
        None => vec![doc],
    };
    let estimate = body_estimate(doc, &coverages, page) + locations.map_or(0, locations_len);
    (coverages, estimate)
}

/// [`coverage_page`]'s page of a document, unadmitted, as a `String`.
#[cfg(test)]
fn coverage_html(doc: &Value, page: &DataPage, locations: Option<&Locations>) -> String {
    let (coverages, estimate) = coverage_layout(doc, page, locations);
    page.render_into(estimate, |body| {
        coverage_body(body, doc, coverages, page, locations)
    })
}

/// The location table: per coverage its feature id, label, `datetime` and
/// `edrqueryendpoint` as an `<a>`, with `numberMatched` and
/// `numberReturned` — every identity member of the GeoJSON form
/// (`/req/html/content` A).
fn locations_panel(locations: &Locations) -> String {
    let mut out = String::from("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Locations</h2></div><div class=\"panel-body\"><dl class=\"definition\">");
    if let Some(matched) = locations.number_matched {
        let _ = write!(out, "<dt><code>numberMatched</code></dt><dd>{matched}</dd>");
    }
    let _ = write!(
        out,
        "<dt><code>numberReturned</code></dt><dd>{}</dd></dl></div><div class=\"table-scroll\"><table class=\"data-table\"><thead><tr><th scope=\"col\">Coverage</th><th scope=\"col\">Location id</th><th scope=\"col\">Label</th><th scope=\"col\">Datetime</th><th scope=\"col\">EDR query endpoint</th></tr></thead><tbody>",
        locations.rows.len()
    );
    for (i, row) in locations.rows.iter().enumerate() {
        let _ = write!(
            out,
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            i + 1,
            escape(row.id.as_deref().unwrap_or_default()),
            escape(&row.label),
            escape(&row.datetime),
            ui::anchor(&row.edrqueryendpoint, &row.edrqueryendpoint, "table-link")
        );
    }
    out.push_str("</tbody></table></div></section>");
    out
}

/// The bytes the location table and the coverage headings naming each
/// location take, from above.
fn locations_len(locations: &Locations) -> usize {
    512 + locations
        .rows
        .iter()
        .map(|row| {
            let id = escaped_len(row.id.as_deref().unwrap_or_default());
            let label = escaped_len(&row.label);
            // The row, its anchor (href and text) and the coverage heading.
            128 + id
                + label
                + escaped_len(&row.datetime)
                + 34
                + 2 * escaped_len(&row.edrqueryendpoint)
                + 16
                + label
        })
        .sum::<usize>()
}

/// The bytes a coverage page's body takes, from above: a value cell is at
/// most `<td title="`, an f64's longest JSON form, `">`, its longest display
/// form and `</td>` ([`CELL`]); a row table adds a cell per axis coordinate
/// and the row tags per value; a grid table a header cell per column and
/// row ([`HEADER`]); the domain table lists every axis value. The request,
/// links, form, chart, facts, parameters and any other member are measured
/// ([`panels_estimate`], [`json_html_len`]); 16 KiB covers the page's fixed
/// markup, [`SECTION`] each coverage section's and [`TABLE`] each range
/// table's.
fn body_estimate(doc: &Value, coverages: &[&Value], page: &DataPage) -> usize {
    const CELL: usize = 68;
    /// A grid table's `<th scope="…">` with its title, as [`CELL`].
    const HEADER: usize = 80;
    const ROW: usize = 9;
    const AXIS_VALUE: usize = 40;
    /// A coverage section's own markup: its heading, the axis summaries,
    /// the domain table's head and rows, the value notes.
    const SECTION: usize = 2048;
    /// A range table's head, caption and column headers.
    const TABLE: usize = 512;
    let mut bytes: usize = 16 * 1024 + panels_estimate(page);
    bytes = bytes.saturating_add(coverages.len().saturating_mul(SECTION));
    bytes += members_len(doc, &["coverages", "parameters", "domain", "ranges"]);
    // A parameter's row shows its label, unit and observed property, each
    // part of the parameter, then all of it; the summary names it again.
    for (key, parameter) in doc["parameters"].as_object().into_iter().flatten() {
        bytes = bytes.saturating_add(256 + escaped_len(key) + 5 * json_html_len(parameter));
    }
    for coverage in coverages {
        bytes = bytes.saturating_add(members_len(
            &coverage["domain"],
            &["type", "domainType", "axes"],
        ));
        bytes = bytes.saturating_add(members_len(
            coverage,
            &["type", "domain", "ranges", "parameters"],
        ));
        let axes = &coverage["domain"]["axes"];
        for axis in axes.as_object().into_iter().flatten().map(|(_, a)| a) {
            let width = axis["coordinates"].as_array().map_or(1, Vec::len);
            bytes = bytes.saturating_add(axis_len(axis).saturating_mul(AXIS_VALUE * width));
        }
        for range in coverage["ranges"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(_, r)| r)
        {
            bytes = bytes.saturating_add(TABLE);
            let values = range["values"].as_array().map_or(0, Vec::len);
            let names: Vec<&str> = range["axisNames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let per_value = if names.len() >= 2 && names[names.len() - 2..] == ["y", "x"] {
                // Each y × x table: a header cell per column and per row.
                let shape: Vec<usize> = range["shape"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|n| n.as_u64().and_then(|n| usize::try_from(n).ok()))
                    .collect();
                if let [.., ny, nx] = shape[..] {
                    let tables = values / ny.saturating_mul(nx).max(1);
                    let headers = ny.saturating_add(nx).saturating_mul(HEADER);
                    bytes = bytes.saturating_add(tables.saturating_mul(TABLE + headers));
                }
                CELL
            } else {
                let axis_cells: usize = names
                    .iter()
                    .map(|a| axes[*a]["coordinates"].as_array().map_or(1, Vec::len))
                    .sum();
                CELL * (1 + axis_cells) + ROW
            };
            bytes = bytes.saturating_add(values.saturating_mul(per_value));
        }
    }
    bytes
}

/// A data page: what it answers, then the data (summary, the location
/// table, the plot, every coverage's values), then the request as a form,
/// and the links, parameters and document members folded below (#1138).
fn coverage_body(
    body: &mut String,
    doc: &Value,
    coverages: Vec<&Value>,
    page: &DataPage,
    locations: Option<&Locations>,
) {
    let kind = doc["type"].as_str().unwrap_or("Coverage");
    body.push_str(&ui::page_heading(
        &page.heading(),
        &format!(
            "CoverageJSON {kind} · {} {}",
            coverages.len(),
            if coverages.len() == 1 {
                "coverage"
            } else {
                "coverages"
            }
        ),
    ));
    summary_panel(body, doc, &coverages, page);
    if let Some(locations) = locations {
        body.push_str(&locations_panel(locations));
    }
    if let Some(chart) = &page.chart {
        chart_panel(body, chart);
    }
    let total = coverages.len();
    // What tells the coverages apart: their point, their one time, or both.
    let point = |c: &Value| -> Option<(f64, f64)> {
        let axes = &c["domain"]["axes"];
        match (
            axes["x"]["values"].as_array(),
            axes["y"]["values"].as_array(),
        ) {
            (Some(x), Some(y)) if x.len() == 1 && y.len() == 1 => {
                Some((x[0].as_f64()?, y[0].as_f64()?))
            }
            _ => None,
        }
    };
    let instant = |c: &Value| -> Option<String> {
        match c["domain"]["axes"]["t"]["values"]
            .as_array()
            .map(Vec::as_slice)
        {
            Some([Value::String(time)]) => Some(time.clone()),
            _ => None,
        }
    };
    let varies = |key: &dyn Fn(&Value) -> Option<String>| {
        total > 1 && {
            let first = key(coverages[0]);
            first.is_some() && coverages.iter().any(|c| key(c) != first)
        }
    };
    let by_place = varies(&|c| point(c).map(|(x, y)| format!("{x} {y}")));
    let by_time = varies(&instant);
    for (i, coverage) in coverages.into_iter().enumerate() {
        let mut heading = if total == 1 {
            "Values".to_owned()
        } else {
            format!("Coverage {} of {total}", i + 1)
        };
        // Name the coverage's location, as its GeoJSON feature's label does.
        if let Some(row) = locations.and_then(|l| l.rows.get(i)) {
            heading.push_str(" · ");
            heading.push_str(&row.label);
        }
        // Among several, a coverage says where (a station of an area) and
        // when (a profile per step) it is, when that differs between them.
        if let Some(place) = point(coverage).filter(|_| by_place && locations.is_none()) {
            heading.push_str(" · ");
            heading.push_str(&crate::query_form::point_text(place));
        }
        if let Some(time) = instant(coverage).filter(|_| by_time) {
            heading.push_str(" · ");
            heading.push_str(&time_text(&time));
        }
        coverage_section(body, &heading, coverage, &doc["parameters"]);
    }
    if let Some(form) = &page.form {
        body.push_str(form);
        body.push_str(crate::query_form::SCRIPT);
    }
    body.push_str("<details class=\"edr-more spaced\"><summary>Request, links, parameters and document</summary>");
    body.push_str(&request_panel(page.raw_query));
    body.push_str(&links_panel(&page.links));
    body.push_str(&parameters_panel(&doc["parameters"]));
    // The document's own members; a single Coverage's domain and ranges are
    // its coverage section, its parameters the parameter table.
    body.push_str(&members_panel(
        "Document",
        doc,
        &["coverages", "parameters", "domain", "ranges"],
    ));
    body.push_str("</details>");
}

/// What the page holds, at a glance: the facts the handler names (the
/// place, the run), the time span of the values, and each parameter with
/// its unit.
fn summary_panel(body: &mut String, doc: &Value, coverages: &[&Value], page: &DataPage) {
    let mut facts = String::new();
    for (label, value) in &page.facts {
        let _ = write!(
            facts,
            "<div><dt>{}</dt><dd>{}</dd></div>",
            escape(label),
            escape(value)
        );
    }
    // The time span: every coverage's `t` values, in RFC 3339's order,
    // each once (the stations of an area share theirs).
    let mut times: Vec<&str> = coverages
        .iter()
        .flat_map(|c| {
            c["domain"]["axes"]["t"]["values"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(Value::as_str)
        .collect();
    times.sort_unstable();
    times.dedup();
    if let (Some(first), Some(last)) = (times.first(), times.last()) {
        let span = if first == last {
            time_text(first)
        } else {
            format!("{} → {}", time_text(first), time_text(last))
        };
        let _ = write!(
            facts,
            "<div><dt>Time · {} {}</dt><dd>{}</dd></div>",
            times.len(),
            if times.len() == 1 { "step" } else { "steps" },
            escape(&span)
        );
    }
    let parameters: Vec<String> = doc["parameters"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, parameter)| parameter_heading(name, parameter))
        .collect();
    if !parameters.is_empty() {
        let _ = write!(
            facts,
            "<div><dt>Parameters</dt><dd>{}</dd></div>",
            escape(&parameters.join(", "))
        );
    }
    if !facts.is_empty() {
        let _ = write!(
            body,
            "<section class=\"panel spaced\"><div class=\"panel-body\"><dl class=\"edr-facts\">{facts}</dl></div></section>"
        );
    }
}

/// An RFC 3339 instant as `2026-10-17 12:00 UTC`; anything else as it is.
fn time_text(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|t| crate::query_form::time_label(t.with_timezone(&chrono::Utc)))
        .unwrap_or_else(|_| value.to_owned())
}

/// A parameter as a column or caption names it: its label and unit, and
/// its id when the label differs (`Air temperature (K) · t2m`); a unit the
/// source does not give is said so.
fn parameter_heading(name: &str, parameter: &Value) -> String {
    let label = i18n_text(&parameter["label"])
        .or_else(|| i18n_text(&parameter["observedProperty"]["label"]))
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| name.to_owned());
    let unit = unit_text(parameter).unwrap_or_else(|| "unit not provided".to_owned());
    if label == name {
        format!("{label} ({unit})")
    } else {
        format!("{label} ({unit}) · {name}")
    }
}

/// The query's PNG plot, with a link to the image on its own.
fn chart_panel(body: &mut String, chart: &Chart) {
    let _ = write!(
        body,
        "<figure class=\"panel spaced edr-chart\"><div class=\"panel-head\"><h2>Chart</h2>{}</div><div class=\"panel-body\"><img src=\"{}\" alt=\"{}\" loading=\"lazy\"><figcaption>The same query as a PNG plot, f=PNG.</figcaption></div></figure>",
        ui::anchor(&chart.src, "Open image ↗", "btn small"),
        escape(image_src(&chart.src)),
        escape(&chart.alt)
    );
}

/// `src` when it is an http(s) or root-relative URL, else nothing to load:
/// the workbench anchor's rule (`safe_href`), for an `<img>`.
fn image_src(src: &str) -> &str {
    if src.starts_with("https://")
        || src.starts_with("http://")
        || (src.starts_with('/') && !src.starts_with("//"))
    {
        src
    } else {
        ""
    }
}

/// The request's query parameters, decoded, `f` left out.
fn request_panel(raw_query: Option<&str>) -> String {
    let mut rows = String::new();
    for pair in raw_query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(&s.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        let name = decode(name);
        if name != "f" {
            let _ = write!(
                rows,
                "<dt><code>{}</code></dt><dd>{}</dd>",
                escape(&name),
                escape(&decode(value))
            );
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    format!("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Request</h2></div><div class=\"panel-body\"><dl class=\"definition\">{rows}</dl></div></section>")
}

/// Every link, each an `<a>` (`/req/html/content` A), in the workbench's
/// link table: the layout every other page's links have.
fn links_panel(links: &[Link]) -> String {
    let links: Vec<Value> = links
        .iter()
        .map(
            |l| serde_json::json!({"href": l.href, "rel": l.rel, "type": l.kind, "title": l.title}),
        )
        .collect();
    format!(
        "<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Links</h2></div>{}</section>",
        ui::link_table(&Value::Array(links))
    )
}

/// The members of `doc` other than `skip`, as a definition list.
fn members_panel(heading: &str, doc: &Value, skip: &[&str]) -> String {
    let Some(members) = doc.as_object() else {
        return String::new();
    };
    let mut rows = String::new();
    for (key, value) in members.iter().filter(|(k, _)| !skip.contains(&k.as_str())) {
        let _ = write!(
            rows,
            "<dt><code>{}</code></dt><dd>{}</dd>",
            escape(key),
            json_html(value)
        );
    }
    if rows.is_empty() {
        return String::new();
    }
    format!("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>{}</h2></div><div class=\"panel-body\"><dl class=\"definition\">{rows}</dl></div></section>", escape(heading))
}

/// The CoverageJSON parameters: the usual fields as columns, every field
/// under "All fields".
fn parameters_panel(parameters: &Value) -> String {
    let Some(parameters) = parameters.as_object() else {
        return String::new();
    };
    let mut out = String::from("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Parameters</h2></div><div class=\"table-scroll\"><table class=\"data-table\"><thead><tr><th scope=\"col\">Parameter</th><th scope=\"col\">Label</th><th scope=\"col\">Unit</th><th scope=\"col\">Observed property</th><th scope=\"col\">All fields</th></tr></thead><tbody>");
    for (key, parameter) in parameters {
        let observed = &parameter["observedProperty"];
        let _ = write!(
            out,
            "<tr><th scope=\"row\"><code>{}</code></th><td>{}</td><td>{}</td><td>{} {}</td><td><details><summary>Show</summary>{}</details></td></tr>",
            escape(key),
            escape(&i18n_text(&parameter["label"]).or_else(|| i18n_text(&observed["label"])).unwrap_or_default()),
            escape(&unit_text(parameter).unwrap_or_default()),
            escape(&i18n_text(&observed["label"]).unwrap_or_default()),
            observed["id"].as_str().map(|id| json_html(&Value::String(id.into()))).unwrap_or_default(),
            json_html(parameter)
        );
    }
    out.push_str("</tbody></table></div></section>");
    out
}

/// One coverage: every range value first, then, folded, its domain (axes,
/// referencing, any other member) and each range's own members (#1138).
fn coverage_section(out: &mut String, heading: &str, coverage: &Value, parameters: &Value) {
    let domain = &coverage["domain"];
    let _ = write!(
        out,
        "<section class=\"panel spaced\"><div class=\"panel-head\"><h2>{}</h2><span class=\"chip\">{}</span></div><div class=\"panel-body\"><p class=\"muted\">An empty cell is a null value: no data there. A rounded value shows its exact value on hover.</p>",
        escape(heading),
        escape(domain["domainType"].as_str().unwrap_or("Domain"))
    );
    ranges_html(out, domain, &coverage["ranges"], parameters);
    out.push_str("<details class=\"edr-more\"><summary>Domain axes, referencing and range members</summary><div class=\"table-scroll\"><table class=\"data-table\"><thead><tr><th scope=\"col\">Axis</th><th scope=\"col\">Count</th><th scope=\"col\">Values</th></tr></thead><tbody>");
    for (name, axis) in domain["axes"].as_object().into_iter().flatten() {
        let mut values = String::new();
        if let Some(coordinates) = axis["coordinates"].as_array() {
            let _ = write!(
                values,
                "<p>Tuples of ({}){}</p>",
                escape(&scalars(coordinates)),
                axis["dataType"]
                    .as_str()
                    .map(|t| format!(" · {}", escape(t)))
                    .unwrap_or_default()
            );
        }
        let count = axis_len(axis);
        // Every value exactly, as the JSON gives it.
        let listed = (0..count)
            .map(|i| match axis_value(axis, i) {
                Value::Array(tuple) => format!("({})", scalars(&tuple)),
                v => scalar_text(&v),
            })
            .collect::<Vec<_>>()
            .join(", ");
        // A long axis (a series' times): its count and ends, the list folded.
        if count > SHORT_AXIS {
            let names = axis_names(name, axis);
            let end = |i: usize| display_axis_value(&axis_value(axis, i), &names);
            let _ = write!(
                values,
                "{count} values: {} … {}<details><summary>All {count} values</summary>{}</details>",
                escape(&end(0)),
                escape(&end(count - 1)),
                escape(&listed)
            );
        } else {
            values.push_str(&escape(&listed));
        }
        let _ = write!(
            out,
            "<tr><th scope=\"row\"><code>{}</code></th><td>{count}</td><td>{values}</td></tr>",
            escape(name)
        );
    }
    out.push_str("</tbody></table></div>");
    for (key, value) in domain.as_object().into_iter().flatten() {
        if matches!(key.as_str(), "type" | "domainType" | "axes") {
            continue;
        }
        let _ = write!(
            out,
            "<h3><code>{}</code></h3>{}",
            escape(key),
            json_html(value)
        );
    }
    for (key, value) in coverage.as_object().into_iter().flatten() {
        if matches!(key.as_str(), "type" | "domain" | "ranges" | "parameters") {
            continue;
        }
        let _ = write!(
            out,
            "<h3><code>{}</code></h3>{}",
            escape(key),
            json_html(value)
        );
    }
    for (name, range) in coverage["ranges"].as_object().into_iter().flatten() {
        // The range's own members (type, dataType, axisNames, shape).
        let meta: Vec<String> = range
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| k.as_str() != "values")
            .map(|(k, v)| format!("{}: {}", escape(k), json_html(v)))
            .collect();
        let _ = write!(
            out,
            "<p><code>{}</code> · {}</p>",
            escape(name),
            meta.join(" · ")
        );
    }
    out.push_str("</details></div></section>");
}

/// The most values an axis lists in full in its row; a longer one shows
/// its count and ends, every value folded below.
const SHORT_AXIS: usize = 8;

/// The coordinate names of an axis's values: a composite axis's
/// `coordinates`, else the axis's own name.
fn axis_names<'v>(name: &'v str, axis: &'v Value) -> Vec<&'v str> {
    match axis["coordinates"].as_array() {
        Some(coordinates) => coordinates.iter().filter_map(Value::as_str).collect(),
        None => vec![name],
    }
}

/// An axis value for display: a number rounded as its coordinate is (a
/// longitude or latitude to 5 decimals, anything else as a value), a
/// tuple as `(…)`, a time as it is.
fn display_axis_value(value: &Value, names: &[&str]) -> String {
    match value {
        Value::Array(tuple) => format!(
            "({})",
            tuple
                .iter()
                .enumerate()
                .map(|(i, v)| display_scalar(v, names.get(i).copied().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        v => display_scalar(v, names.first().copied().unwrap_or_default()),
    }
}

/// A scalar on coordinate `name` for display, see [`display_axis_value`].
fn display_scalar(value: &Value, name: &str) -> String {
    match value.as_f64() {
        Some(v) if value.is_f64() && is_horizontal(name) => display_coordinate(v),
        Some(v) if value.is_f64() => display_value(v),
        _ => scalar_text(value),
    }
}

/// Whether a coordinate name is a horizontal one, shown to 5 decimals.
fn is_horizontal(name: &str) -> bool {
    matches!(name, "x" | "y")
}

/// The ranges, grouped by layout (same axes and shape): a grid's as y × x
/// matrices, the others as one row per domain position with a column per
/// parameter.
fn ranges_html(out: &mut String, domain: &Value, ranges: &Value, parameters: &Value) {
    type Group<'v> = (Vec<&'v str>, Vec<usize>, Vec<(&'v str, &'v Value)>);
    let mut groups: Vec<Group> = Vec::new();
    for (name, range) in ranges.as_object().into_iter().flatten() {
        let axes: Vec<&str> = range["axisNames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let shape: Vec<usize> = range["shape"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_u64().and_then(|n| usize::try_from(n).ok()))
            .collect();
        match groups.iter_mut().find(|g| g.0 == axes && g.1 == shape) {
            Some(group) => group.2.push((name, range)),
            None => groups.push((axes, shape, vec![(name, range)])),
        }
    }
    for (axes, shape, members) in &groups {
        let count: usize = shape.iter().product();
        let consistent = axes.len() == shape.len()
            && members
                .iter()
                .all(|(_, r)| r["values"].as_array().map_or(0, Vec::len) == count);
        if !consistent {
            // Not a layout this page can tabulate: the values as listed.
            for (name, range) in members {
                let _ = write!(
                    out,
                    "<p><code>{}</code></p><p>{}</p>",
                    escape(name),
                    json_html(&range["values"])
                );
            }
        } else if axes.len() >= 2 && axes[axes.len() - 2..] == ["y", "x"] {
            grid_tables(out, domain, axes, shape, members, parameters);
        } else {
            row_table(out, domain, axes, shape, members, parameters);
        }
    }
}

/// One y × x matrix per parameter and leading index (t, z), one cell per
/// value: the compact layout that keeps a large grid's page near its JSON.
fn grid_tables(
    out: &mut String,
    domain: &Value,
    axes: &[&str],
    shape: &[usize],
    members: &[(&str, &Value)],
    parameters: &Value,
) {
    let n = axes.len();
    let (ny, nx) = (shape[n - 2], shape[n - 1]);
    let (lead_axes, lead_shape) = (&axes[..n - 2], &shape[..n - 2]);
    let lead_count: usize = lead_shape.iter().product();
    let (x_axis, y_axis) = (&domain["axes"]["x"], &domain["axes"]["y"]);
    let mut x_header = String::new();
    for xi in 0..nx {
        push_number_cell(
            &mut x_header,
            "th scope=\"col\"",
            &axis_value(x_axis, xi),
            true,
        );
    }
    for (name, range) in members {
        let values = range["values"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        for lead in 0..lead_count {
            let index = unravel(lead, lead_shape);
            let mut caption = escape(&parameter_heading(name, &parameters[*name]));
            for (axis, i) in lead_axes.iter().zip(&index) {
                let value = axis_value(&domain["axes"][*axis], *i);
                let shown = match value.as_str() {
                    Some(time) if *axis == "t" => time_text(time),
                    _ => display_scalar(&value, axis),
                };
                let _ = write!(caption, " · {} = {}", escape(axis), escape(&shown));
            }
            let _ = write!(
                out,
                "<div class=\"table-scroll\"><table class=\"data-table\"><caption>{caption}</caption><thead><tr><th scope=\"col\">y \\ x</th>{x_header}</tr></thead><tbody>"
            );
            let offset = lead * ny * nx;
            for yi in 0..ny {
                out.push_str("<tr>");
                push_number_cell(out, "th scope=\"row\"", &axis_value(y_axis, yi), true);
                for value in &values[offset + yi * nx..offset + (yi + 1) * nx] {
                    push_cell(out, value, false);
                }
                out.push_str("</tr>");
            }
            out.push_str("</tbody></table></div>");
        }
    }
}

/// One row per domain position (row-major over the range's axes, a
/// composite axis spread over its coordinates), one column per parameter.
fn row_table(
    out: &mut String,
    domain: &Value,
    axes: &[&str],
    shape: &[usize],
    members: &[(&str, &Value)],
    parameters: &Value,
) {
    out.push_str("<div class=\"table-scroll\"><table class=\"data-table\"><thead><tr>");
    // A composite axis's columns: its coordinates; any other axis: one,
    // each with whether it is a horizontal coordinate.
    let mut columns: Vec<Vec<bool>> = Vec::with_capacity(axes.len());
    for axis in axes {
        let names = axis_names(axis, &domain["axes"][*axis]);
        for name in &names {
            let _ = write!(out, "<th scope=\"col\">{}</th>", escape(name));
        }
        columns.push(names.iter().map(|n| is_horizontal(n)).collect());
    }
    for (name, _) in members {
        let _ = write!(
            out,
            "<th scope=\"col\">{}</th>",
            escape(&parameter_heading(name, &parameters[*name]))
        );
    }
    out.push_str("</tr></thead><tbody>");
    let count: usize = shape.iter().product();
    for flat in 0..count {
        out.push_str("<tr>");
        for ((axis, i), horizontal) in axes.iter().zip(unravel(flat, shape)).zip(&columns) {
            let value = axis_value(&domain["axes"][*axis], i);
            if domain["axes"][*axis]["coordinates"].is_array() {
                for (c, &horizontal) in horizontal.iter().enumerate() {
                    push_cell(out, value.get(c).unwrap_or(&Value::Null), horizontal);
                }
            } else {
                push_cell(out, &value, horizontal.first().copied().unwrap_or(false));
            }
        }
        for (_, range) in members {
            push_cell(out, &range["values"][flat], false);
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
}

/// The multi-index of a row-major flat index.
fn unravel(mut flat: usize, shape: &[usize]) -> Vec<usize> {
    let mut index = vec![0; shape.len()];
    for (slot, &len) in index.iter_mut().zip(shape).rev() {
        if len > 0 {
            *slot = flat % len;
            flat /= len;
        }
    }
    index
}

/// The number of values on a CoverageJSON axis (`values`, or a regular
/// `start`/`stop`/`num` axis).
fn axis_len(axis: &Value) -> usize {
    axis["values"].as_array().map(Vec::len).unwrap_or_else(|| {
        axis["num"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0)
    })
}

/// Value `i` of a CoverageJSON axis.
fn axis_value(axis: &Value, i: usize) -> Value {
    if let Some(values) = axis["values"].as_array() {
        return values.get(i).cloned().unwrap_or(Value::Null);
    }
    match (
        axis["start"].as_f64(),
        axis["stop"].as_f64(),
        axis["num"].as_u64(),
    ) {
        (Some(start), Some(stop), Some(num)) if num > 1 => {
            Value::from(start + (stop - start) * i as f64 / (num - 1) as f64)
        }
        (Some(start), _, _) => Value::from(start),
        _ => Value::Null,
    }
}

/// A table cell: a number rounded for display ([`display_value`], or
/// [`display_coordinate`] for a `horizontal` coordinate) with its exact
/// JSON form as the cell's `title` when rounding changed it, any other
/// scalar as text, null as an empty cell (#1138).
fn push_cell(out: &mut String, value: &Value, horizontal: bool) {
    match value {
        Value::Null => out.push_str("<td></td>"),
        Value::Number(_) => push_number_cell(out, "td", value, horizontal),
        other => {
            out.push_str("<td>");
            out.push_str(&escape(&scalar_text(other)));
            out.push_str("</td>");
        }
    }
}

/// A cell `<{tag}>` (`td`, or a `th` with its scope) holding `value`
/// rounded for display, its exact form in `title` when they differ; a
/// non-number as text.
fn push_number_cell(out: &mut String, tag: &str, value: &Value, horizontal: bool) {
    let name = tag.split(' ').next().unwrap_or(tag);
    match (value, value.as_f64()) {
        (Value::Number(n), Some(v)) if n.is_f64() => {
            let shown = if horizontal {
                display_coordinate(v)
            } else {
                display_value(v)
            };
            if shown.parse::<f64>().ok() == Some(v) {
                let _ = write!(out, "<{tag}>{shown}</{name}>");
            } else {
                let _ = write!(out, "<{tag} title=\"{n}\">{shown}</{name}>");
            }
        }
        (Value::Number(n), _) => {
            let _ = write!(out, "<{tag}>{n}</{name}>");
        }
        (other, _) => {
            let _ = write!(out, "<{tag}>{}</{name}>", escape(&scalar_text(other)));
        }
    }
}

/// A scalar as plain (unescaped) text; anything else as its JSON.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn scalars(values: &[Value]) -> String {
    values
        .iter()
        .map(scalar_text)
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Any JSON value as HTML, nothing dropped: objects as definition lists,
/// arrays of scalars inline, URLs as anchors.
fn json_html(value: &Value) -> String {
    match value {
        Value::Null => "<span class=\"muted\">null</span>".into(),
        Value::String(s) if is_url(s) => ui::anchor(s, s, ""),
        Value::String(s) => escape(s),
        Value::Array(items) if items.is_empty() => "<span class=\"muted\">[]</span>".into(),
        Value::Array(items) if items.iter().all(|i| !i.is_array() && !i.is_object()) => {
            items.iter().map(json_html).collect::<Vec<_>>().join(", ")
        }
        Value::Array(items) => format!(
            "<ol>{}</ol>",
            items
                .iter()
                .map(|i| format!("<li>{}</li>", json_html(i)))
                .collect::<String>()
        ),
        Value::Object(map) => format!(
            "<dl class=\"definition\">{}</dl>",
            map.iter()
                .map(|(k, v)| format!(
                    "<dt><code>{}</code></dt><dd>{}</dd>",
                    escape(k),
                    json_html(v)
                ))
                .collect::<String>()
        ),
        other => escape(&other.to_string()),
    }
}

/// Counts the bytes [`escape`] would make of what is written to it.
struct EscapedLen(usize);

impl std::fmt::Write for EscapedLen {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0 += escaped_len(s);
        Ok(())
    }
}

/// `escape(s).len()`, without building it.
fn escaped_len(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '&' | '\'' => 5,
            '<' | '>' => 4,
            '"' => 6,
            c => c.len_utf8(),
        })
        .sum()
}

/// `json_html(value).len()`, without building it (pinned by
/// `json_html_len_is_exact`).
fn json_html_len(value: &Value) -> usize {
    match value {
        Value::Null => 31,
        Value::String(s) if is_url(s) => 24 + 2 * escaped_len(s),
        Value::String(s) => escaped_len(s),
        Value::Array(items) if items.is_empty() => 29,
        Value::Array(items) if items.iter().all(|i| !i.is_array() && !i.is_object()) => {
            items.iter().map(json_html_len).sum::<usize>() + 2 * (items.len() - 1)
        }
        Value::Array(items) => 9 + items.iter().map(|i| 9 + json_html_len(i)).sum::<usize>(),
        Value::Object(map) => {
            28 + map
                .iter()
                .map(|(k, v)| 31 + escaped_len(k) + json_html_len(v))
                .sum::<usize>()
        }
        other => {
            let mut len = EscapedLen(0);
            let _ = write!(len, "{other}");
            len.0
        }
    }
}

/// The bytes of [`members_panel`]'s rows for `doc` without `skip`, plus
/// its frame.
fn members_len(doc: &Value, skip: &[&str]) -> usize {
    256 + doc
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .map(|(k, v)| 31 + escaped_len(k) + json_html_len(v))
        .sum::<usize>()
}

/// The bytes of a page's heading, request and links panels, from above:
/// the request is decoded then escaped (at most 6 bytes per raw byte); the
/// links panel exactly.
fn panels_estimate(page: &DataPage) -> usize {
    let raw = page.raw_query.unwrap_or("");
    // The heading names the page and the collection twice.
    let heading = 1024 + 12 * (page.title.len() + page.collection_title.len());
    let request = 512 + 6 * raw.len() + 40 * (raw.matches('&').count() + 1);
    // A handful of links: measured by writing them.
    let links = links_panel(&page.links).len();
    // The form and its script are written as they are; the plot's URL
    // twice and its alt text, and the facts, escaped.
    let form = page
        .form
        .as_ref()
        .map_or(0, |form| form.len() + crate::query_form::SCRIPT.len());
    let chart = page
        .chart
        .as_ref()
        .map_or(0, |c| 512 + 3 * escaped_len(&c.src) + escaped_len(&c.alt));
    let facts: usize = page
        .facts
        .iter()
        .map(|(label, value)| 32 + escaped_len(label) + escaped_len(value))
        .sum();
    heading + request + links + form + chart + facts
}

/// The bytes of one `items` table row ([`features_body`]), from above:
/// its id link, geometry type and coordinates, a cell per column and its
/// links, each measured.
fn feature_row_len(feature: &Value, columns: &[&str]) -> usize {
    let id = scalar_text(&feature["id"]);
    let id_cell = match self_href(feature) {
        Some(href) => 34 + escaped_len(&ui::with_format(href, "html")) + escaped_len(&id),
        None => escaped_len(&id),
    };
    let geometry = &feature["geometry"];
    let mut coordinates = EscapedLen(0);
    let _ = write!(coordinates, "{}", geometry["coordinates"]);
    let cells: usize = columns
        .iter()
        .map(|c| 9 + feature["properties"].get(*c).map_or(0, json_html_len))
        .sum();
    let links: usize = feature["links"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            let text = |key: &str| l[key].as_str().unwrap_or_default();
            feature_link_len(text("rel"), text("title"), text("type"), text("href")) + 4
        })
        .sum();
    63 + id_cell
        + escaped_len(geometry["type"].as_str().unwrap_or("null"))
        + coordinates.0
        + cells
        + 14
        + links
}

/// One link of a feature row: its relation, an `<a>` titled like the link
/// (else by its href), and its media type — every member of the GeoJSON's
/// link object (`/req/html/content` A).
fn feature_link(rel: &str, title: &str, kind: &str, href: &str) -> String {
    let label = if title.is_empty() { href } else { title };
    format!(
        "<code>{}</code> {} <code>{}</code>",
        escape(rel),
        ui::anchor(href, label, "table-link"),
        escape(kind)
    )
}

/// The bytes [`feature_link`] writes, at most: the anchor's href is
/// measured unrewritten (`safe_href` only ever shortens it to `#`).
fn feature_link_len(rel: &str, title: &str, kind: &str, href: &str) -> usize {
    let label = if title.is_empty() { href } else { title };
    32 + escaped_len(rel) + 34 + escaped_len(href) + escaped_len(label) + escaped_len(kind)
}

/// A feature's `self` link.
fn self_href(feature: &Value) -> Option<&str> {
    feature["links"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|l| l["rel"] == "self")
        .and_then(|l| l["href"].as_str())
}

/// The `/locations` list as HTML, written into `w` (the GeoJSON's budgeted
/// writer, so the page meets the same byte limit and memory admission).
/// The table comes first (#1138): per location its label, opening the
/// location's data page at `view_datetime`, the window
/// `handlers::location_view_datetime` picks, its id, point,
/// `edrqueryendpoint` (opening the same window) and data link. A filter narrows the page's
/// rows, the pager walks `prev`/`next`, and a map plots the page's points.
/// Every location's `datetime` and `parameter-name` are the collection's, so
/// they are listed once in the folded document panel with the request and
/// the links.
pub(crate) fn write_locations(
    locations: LocationRows<'_>,
    ctx: &LocationsContext,
    page: &DataPage,
    counts: Option<(usize, usize)>,
    view_datetime: Option<&str>,
    mut w: impl io::Write,
) -> io::Result<()> {
    let mut head = crate::query_form::STYLE.to_owned();
    head.push_str(&ui::map_head(page.base));
    let (head, tail) = page.shell_with(&head);
    w.write_all(head.as_bytes())?;
    let shown = locations.len();
    let description = match counts {
        Some((matched, _)) if shown > 0 => {
            let first = page_offset(page.raw_query) + 1;
            format!(
                "{first}–{} of {matched} {}",
                first + shown - 1,
                if matched == 1 {
                    "location"
                } else {
                    "locations"
                }
            )
        }
        Some((matched, _)) => format!(
            "No locations on this page · {matched} {}",
            if matched == 1 {
                "location"
            } else {
                "locations"
            }
        ),
        None => format!(
            "{shown} {}",
            if shown == 1 { "location" } else { "locations" }
        ),
    };
    let mut body = ui::page_heading(&page.heading(), &description);
    let nav: Vec<ds_core::html::LinkView> = page
        .links
        .iter()
        .filter(|l| matches!(l.rel.as_str(), "prev" | "next"))
        .map(|l| ds_core::html::LinkView::new(l.href.clone(), l.rel.clone(), Some(&l.title)))
        .collect();
    let _ = write!(
        body,
        "<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Locations</h2></div><div class=\"panel-body\"><div class=\"edr-filter enhanced\"><label for=\"location-filter\">Filter this page</label><input id=\"location-filter\" type=\"search\" placeholder=\"Name or id\" autocomplete=\"off\"><span class=\"muted\" data-filter-status></span></div></div><div class=\"table-scroll\"><table class=\"data-table\" data-locations><thead><tr><th scope=\"col\">label</th><th scope=\"col\">id</th><th scope=\"col\">Latitude</th><th scope=\"col\">Longitude</th><th scope=\"col\">geometry</th><th scope=\"col\">edrqueryendpoint</th><th scope=\"col\">links</th></tr></thead><tbody>"
    );
    w.write_all(body.as_bytes())?;
    let mut points = Vec::with_capacity(shown);
    for loc in locations.iter() {
        let endpoint = format!(
            "{}/edr/collections/{}/locations/{}",
            ctx.base_url,
            ctx.collection_id,
            encode_path_segment(&loc.id)
        );
        let mut view = Vec::with_capacity(2);
        if let Some(datetime) = view_datetime {
            view.push(("datetime".to_owned(), datetime.to_owned()));
        }
        // The end point as the GeoJSON names it, opening the same window
        // as the label: a browser following it gets the page too.
        let endpoint_view = crate::query_form::query_url(&endpoint, &view);
        view.push(("f".to_owned(), "html".to_owned()));
        let view = crate::query_form::query_url(&endpoint, &view);
        // The feature's one link, as the GeoJSON carries it
        // (`LocationLink`): relation, title and type next to its anchor.
        let link = feature_link(
            "data",
            &format!("Data for {}", loc.label),
            COVERAGE_JSON_MEDIA_TYPE,
            &endpoint,
        );
        let label = if loc.label.trim().is_empty() {
            &loc.id
        } else {
            &loc.label
        };
        let mut row = format!(
            "<tr data-location=\"{}\"><th scope=\"row\">{}</th><td><code>{}</code></td>",
            escape(&format!("{} {}", loc.id, loc.label).to_lowercase()),
            ui::anchor(&view, label, "table-link"),
            escape(&loc.id),
        );
        push_number_cell(&mut row, "td", &Value::from(loc.latitude), true);
        push_number_cell(&mut row, "td", &Value::from(loc.longitude), true);
        let _ = write!(
            row,
            "<td>Point</td><td>{}</td><td>{link}</td></tr>",
            ui::anchor(&endpoint_view, &endpoint, "table-link"),
        );
        w.write_all(row.as_bytes())?;
        if loc.longitude.is_finite() && loc.latitude.is_finite() {
            points.push(serde_json::json!({
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [loc.longitude, loc.latitude]},
                "properties": {"label": label, "href": view, "facts": {"Id": loc.id}}
            }));
        }
    }
    let mut body = String::from("</tbody></table></div></section>");
    if !nav.is_empty() {
        body.push_str(&ui::pagination(&nav));
    }
    if !points.is_empty() {
        let _ = write!(
            body,
            "<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Map · this page</h2><span class=\"chip\">CRS84</span></div>{}</section>",
            ui::map_html(
                page.base,
                &serde_json::json!({"type": "FeatureCollection", "features": points}),
                true,
                "The locations of this page are listed in the table."
            )
        );
    }
    body.push_str(
        "<details class=\"edr-more spaced\"><summary>Request, links and document</summary>",
    );
    body.push_str(&request_panel(page.raw_query));
    body.push_str(&links_panel(&page.links));
    let datetime = ctx
        .temporal_extent
        .as_ref()
        .map(|(start, end)| format!("{start}/{end}"))
        .unwrap_or_default();
    let _ = write!(
        body,
        "<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Document</h2></div><div class=\"panel-body\"><dl class=\"definition\"><dt><code>type</code></dt><dd>FeatureCollection</dd>"
    );
    if let Some((matched, returned)) = counts {
        let _ = write!(
            body,
            "<dt><code>numberMatched</code></dt><dd>{matched}</dd><dt><code>numberReturned</code></dt><dd>{returned}</dd>"
        );
    }
    let _ = write!(
        body,
        "<dt>Every location's <code>type</code></dt><dd>Feature</dd><dt>Every location's <code>datetime</code></dt><dd>{}</dd><dt>Every location's <code>parameter-name</code></dt><dd>{}</dd></dl></div></section></details>{LOCATION_FILTER}",
        escape(&datetime),
        escape(&ctx.parameter_names.join(", "))
    );
    w.write_all(body.as_bytes())?;
    w.write_all(tail.as_bytes())
}

/// The `offset` of a `/locations` request, 0 when absent: where a page's
/// rows start in the list.
fn page_offset(raw_query: Option<&str>) -> usize {
    raw_query
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("offset="))
        .and_then(|offset| offset.trim().parse().ok())
        .unwrap_or(0)
}

/// The `/locations` page's filter: hides the rows whose id and label do not
/// hold the typed text, and says how many are shown.
const LOCATION_FILTER: &str = "<script>(()=>{const input=document.getElementById('location-filter'),status=document.querySelector('[data-filter-status]');if(!input)return;const rows=[...document.querySelectorAll('[data-locations] tbody tr')];input.addEventListener('input',()=>{const term=input.value.trim().toLowerCase();let shown=0;rows.forEach(row=>{const hit=!term||row.dataset.location.includes(term);row.hidden=!hit;if(hit)shown++});status.textContent=term?shown+' of '+rows.length+' shown':''})})()</script>";

/// The bare form of a data query (#1138): what a query link opens when it
/// gives no coordinates. Its heading names the query and the collection,
/// its JSON switch opens the example query as CoverageJSON.
pub(crate) fn form_page(
    form: &crate::query_form::QueryForm,
    pairs: &[(String, String)],
    base: &str,
    collection_id: &str,
    collection_title: &str,
    run: Option<&str>,
) -> String {
    let query = crate::query_form::capitalised(form.query_type());
    let page = DataPage {
        base,
        collection_id,
        collection_title,
        title: format!("{query} query"),
        json_url: form.example_url(pairs, crate::params::EdrFormat::CoverageJson),
        ..Default::default()
    };
    let mut intro = String::from("Choose where, when and what, then run the query. The coordinates are an example inside the collection's extent.");
    if let Some(run) = run {
        let _ = write!(intro, " The query reads model run {run}.");
    }
    let description = form.description();
    let markup = form.html(pairs, true, "Request");
    let estimate = 4096 + markup.len() + 6 * (description.len() + intro.len());
    page.render_into(estimate, |body| {
        body.push_str(&ui::page_heading(&page.heading(), description));
        let _ = write!(
            body,
            "<div class=\"callout spaced\">{}</div>",
            escape(&intro)
        );
        body.push_str(&markup);
        body.push_str(crate::query_form::SCRIPT);
    })
}

/// The links of an `items` page from its GeoJSON links: `self`, `next` and
/// `prev` to their HTML pages, an `alternate` to the GeoJSON, the others
/// as they are.
pub(crate) fn feature_page_links(links: &Value) -> Vec<Link> {
    let mut out = Vec::new();
    for link in links.as_array().into_iter().flatten() {
        let text = |key: &str| link[key].as_str().unwrap_or_default().to_owned();
        let (href, rel) = (text("href"), text("rel"));
        if matches!(rel.as_str(), "self" | "next" | "prev") {
            out.push(Link {
                href: ui::with_format(&href, "html"),
                rel: rel.clone(),
                kind: "text/html".into(),
                title: text("title"),
            });
            if rel == "self" {
                out.push(Link {
                    href: ui::with_format(&href, "GeoJSON"),
                    rel: "alternate".into(),
                    kind: "application/geo+json".into(),
                    title: "This document as GeoJSON".into(),
                });
            }
        } else {
            out.push(Link {
                href,
                rel,
                kind: text("type"),
                title: text("title"),
            });
        }
    }
    out
}

/// Where an `items` page's generation time goes: the handler hashes the
/// page with this slot empty, then fills it.
pub(crate) const TIMESTAMP_SLOT: &str = "<time data-generated></time>";

/// An `items` response (a FeatureCollection page or one Feature) as HTML:
/// its members, links, and one table row per feature with its id,
/// geometry, every property and its links. `time_stamp` renders the
/// `timeStamp` member as [`TIMESTAMP_SLOT`].
pub(crate) fn features_page(doc: &Value, page: &DataPage, time_stamp: bool) -> String {
    let (features, columns) = features_layout(doc);
    let estimate = features_estimate(doc, &features, &columns, page);
    page.render_into(estimate, |body| {
        features_body(body, doc, &features, &columns, page, time_stamp)
    })
}

/// An `items` response's features (a page's, or the one Feature) and its
/// property columns, in first-seen order.
fn features_layout(doc: &Value) -> (Vec<&Value>, Vec<&str>) {
    let features: Vec<&Value> = match doc["features"].as_array() {
        Some(features) => features.iter().collect(),
        None => vec![doc],
    };
    let mut columns: Vec<&str> = Vec::new();
    for feature in &features {
        for key in feature["properties"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            if !columns.contains(&key.as_str()) {
                columns.push(key);
            }
        }
    }
    (features, columns)
}

/// The bytes an `items` page's body takes, from above: measured per
/// feature ([`feature_row_len`]), never guessed, so a polygon's coordinates
/// or a wide property set cannot overrun the reservation.
fn features_estimate(doc: &Value, features: &[&Value], columns: &[&str], page: &DataPage) -> usize {
    // The members panel `features_body` writes, and the `timeStamp` slot.
    let members = if doc["features"].is_array() {
        members_len(doc, &["features", "links", "timeStamp"]) + 64
    } else {
        members_len(doc, &["type", "id", "geometry", "properties", "links"])
    };
    16 * 1024
        + panels_estimate(page)
        + members
        + columns.iter().map(|c| 32 + escaped_len(c)).sum::<usize>()
        + features
            .iter()
            .map(|f| feature_row_len(f, columns))
            .sum::<usize>()
}

fn features_body(
    body: &mut String,
    doc: &Value,
    features: &[&Value],
    columns: &[&str],
    page: &DataPage,
    time_stamp: bool,
) {
    body.push_str(&ui::page_heading(
        &page.title,
        &format!(
            "{} · {} {}",
            page.collection_title,
            features.len(),
            if features.len() == 1 {
                "feature"
            } else {
                "features"
            }
        ),
    ));
    body.push_str(&request_panel(page.raw_query));
    body.push_str(&links_panel(&page.links));
    if doc["features"].is_array() {
        let mut members = members_panel("Document", doc, &["features", "links", "timeStamp"]);
        if time_stamp {
            let slot = format!("<dt><code>timeStamp</code></dt><dd>{TIMESTAMP_SLOT}</dd></dl>");
            members = members.replacen("</dl>", &slot, 1);
        }
        body.push_str(&members);
    }
    body.push_str("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Features</h2></div><div class=\"table-scroll\"><table class=\"data-table\"><thead><tr><th scope=\"col\">id</th><th scope=\"col\">geometry</th>");
    for column in columns {
        let _ = write!(body, "<th scope=\"col\">{}</th>", escape(column));
    }
    body.push_str("<th scope=\"col\">links</th></tr></thead><tbody>");
    for feature in features {
        let id = scalar_text(&feature["id"]);
        let id_cell = match self_href(feature) {
            Some(href) => ui::anchor(&ui::with_format(href, "html"), &id, "table-link"),
            None => escape(&id),
        };
        let geometry = &feature["geometry"];
        let _ = write!(
            body,
            "<tr><th scope=\"row\">{id_cell}</th><td>{} <code class=\"coords\">{}</code></td>",
            escape(geometry["type"].as_str().unwrap_or("null")),
            escape(&geometry["coordinates"].to_string())
        );
        for column in columns {
            let _ = write!(
                body,
                "<td>{}</td>",
                feature["properties"]
                    .get(*column)
                    .map(json_html)
                    .unwrap_or_default()
            );
        }
        let links: Vec<String> = feature["links"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| {
                let text = |key: &str| l[key].as_str().unwrap_or_default();
                feature_link(text("rel"), text("title"), text("type"), text("href"))
            })
            .collect();
        let _ = write!(body, "<td>{}</td></tr>", links.join("<br>"));
    }
    body.push_str("</tbody></table></div></section>");
    // Members of a single Feature beyond the table's (e.g. foreign ones).
    if !doc["features"].is_array() {
        body.push_str(&members_panel(
            "Other members",
            doc,
            &["type", "id", "geometry", "properties", "links"],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// #1138: a query crumb follows the collection or a run; a location
    /// whose id names a query is a location.
    #[test]
    fn crumbs_name_queries_by_their_place() {
        let collection = "https://x/edr/collections/c";
        let labels = |url: &str| -> Vec<String> {
            crumb_labels(collection, url)
                .into_iter()
                .map(|(_, label)| label)
                .collect()
        };
        assert_eq!(labels(&format!("{collection}/area?f=html")), ["Area query"]);
        assert_eq!(
            labels(&format!("{collection}/instances/2026-06-07T00:00:00Z/cube")),
            ["Run 2026-06-07T00:00:00Z", "Cube query"]
        );
        assert_eq!(
            labels(&format!("{collection}/locations/area?f=html")),
            ["Locations"]
        );
    }

    fn page() -> DataPage<'static> {
        DataPage {
            base: "https://example.org",
            collection_id: "c",
            collection_title: "<Coll>",
            title: "Area query".into(),
            raw_query: Some("coords=POLYGON((1%202))&f=html&note=%3Cb%3E"),
            json_url: "https://example.org/edr/collections/c/area?f=CoverageJSON".into(),
            links: vec![Link {
                href: "https://example.org/edr/collections/c/area?f=HTML".into(),
                rel: "self".into(),
                kind: "text/html".into(),
                title: "This document".into(),
            }],
            ..Default::default()
        }
    }

    /// A [t, y, x] grid renders one y × x matrix per timestep, every value
    /// in a cell, nulls empty.
    #[test]
    fn grid_ranges_are_matrices_with_every_value() {
        let doc = json!({
            "type": "Coverage",
            "domain": {"type": "Domain", "domainType": "Grid",
                "axes": {"x": {"values": [10.0, 11.0, 12.0]}, "y": {"values": [60.0, 61.0]},
                         "t": {"values": ["2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z"]}},
                "referencing": [{"coordinates": ["x", "y"], "system": {"type": "GeographicCRS",
                    "id": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"}}]},
            "parameters": {"t2m": {"type": "Parameter", "unit": {"symbol": {"value": "K", "type": "http://qudt.org/vocab/unit/"}},
                "observedProperty": {"id": "https://vocab.nerc.ac.uk/standard_name/air_temperature/", "label": {"en": "Air temperature"}}}},
            "ranges": {"t2m": {"type": "NdArray", "dataType": "float", "axisNames": ["t", "y", "x"],
                "shape": [2, 2, 3], "values": [1, 2, 3, 4, 5, 6, 7, 8, null, 10, 11, 12]}}
        });
        let html = coverage_html(&doc, &page(), None);
        assert_eq!(html.matches("<caption>").count(), 2, "{html}");
        for v in [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12] {
            assert!(html.contains(&format!("<td>{v}</td>")), "{v}");
        }
        assert!(html.contains("<td></td>"));
        assert!(html.contains("t = 2026-01-01 01:00 UTC"), "{html}");
        // URIs are anchors; the collection title and request are escaped.
        assert!(html.contains(
            "<a class=\"\" href=\"https://vocab.nerc.ac.uk/standard_name/air_temperature/\">"
        ));
        assert!(html.contains("&lt;Coll&gt;") && !html.contains("<Coll>"));
        assert!(html.contains("&lt;b&gt;") && !html.contains("<b>"));
        assert!(html.contains("POLYGON((1 2))"));
    }

    /// A Section's composite axis spreads over its coordinates, one row
    /// per (node, level), and the foreign member is listed.
    #[test]
    fn composite_ranges_are_rows_per_domain_position() {
        let doc = json!({
            "type": "Coverage",
            "domain": {"type": "Domain", "domainType": "Section",
                "axes": {"composite": {"dataType": "tuple", "coordinates": ["t", "x", "y"],
                    "values": [["2026-01-01T00:00:00Z", 24.5, 60.25], ["2026-01-01T00:00:00Z", 25.5, 60.75]]},
                    "z": {"values": [500, 1000]}},
                "referencing": [],
                "meteocore:beamCoverage": {"floor": [120.5, 340.25]}},
            "parameters": {},
            "ranges": {"DBZH": {"type": "NdArray", "dataType": "float", "axisNames": ["composite", "z"],
                "shape": [2, 2], "values": [1.5, null, 3.5, 4.5]}}
        });
        let html = coverage_html(&doc, &page(), None);
        assert!(
            html.contains(
                "<td>2026-01-01T00:00:00Z</td><td>25.5</td><td>60.75</td><td>1000</td><td>4.5</td>"
            ),
            "{html}"
        );
        assert!(html.contains("meteocore:beamCoverage") && html.contains("340.25"));
    }

    /// The body fits the buffer reserved for it: a grid of the longest
    /// f64 forms and a series with RFC 3339 rows never regrow the page.
    #[test]
    fn the_reservation_covers_the_body() {
        let long = -1.234_567_890_123_456_7e-300;
        let (nt, ny, nx) = (3, 40, 50);
        let grid = json!({
            "type": "Coverage",
            "domain": {"type": "Domain", "domainType": "Grid", "axes": {
                "x": {"values": vec![long; nx]}, "y": {"values": vec![long; ny]},
                "t": {"values": vec!["2026-01-01T00:00:00+00:00"; nt]}}, "referencing": []},
            "parameters": {},
            "ranges": {"a": {"type": "NdArray", "dataType": "float", "axisNames": ["t", "y", "x"],
                "shape": [nt, ny, nx], "values": vec![long; nt * ny * nx]}}
        });
        let series = json!({
            "type": "Coverage",
            "domain": {"type": "Domain", "domainType": "PointSeries", "axes": {
                "x": {"values": [1.0]}, "y": {"values": [2.0]},
                "t": {"values": vec!["2026-01-01T00:00:00+00:00"; 5000]}}, "referencing": []},
            "parameters": {},
            "ranges": {"a": {"type": "NdArray", "dataType": "float", "axisNames": ["t"],
                "shape": [5000], "values": vec![long; 5000]}}
        });
        let page = page();
        let (head, tail) = page.shell();
        for doc in [grid, series] {
            let html = coverage_html(&doc, &page, None);
            let reserved = head.len() + body_estimate(&doc, &[&doc], &page) + tail.len();
            assert!(html.len() <= reserved, "{} > {reserved}", html.len());
            assert_eq!(html.capacity(), reserved, "the buffer regrew");
            // And not wildly above it: within twice the page.
            assert!(reserved < 2 * html.len(), "{reserved} vs {}", html.len());
        }
    }

    /// The location table and the headings naming each coverage's station
    /// (#988) fit the reservation too, and every label is escaped.
    #[test]
    fn the_locations_reservation_covers_the_page() {
        let series = json!({
            "type": "Coverage",
            "domain": {"type": "Domain", "domainType": "PointSeries", "axes": {
                "x": {"values": [1.0]}, "y": {"values": [2.0]},
                "t": {"values": ["2026-01-01T00:00:00+00:00"]}}, "referencing": []},
            "parameters": {},
            "ranges": {"a": {"type": "NdArray", "dataType": "float", "axisNames": ["t"],
                "shape": [1], "values": [1.5]}}
        });
        let n = 200;
        let doc = json!({"type": "CoverageCollection", "parameters": {},
            "coverages": vec![series; n]});
        let label = "<&\"Ä station\"&>".repeat(20);
        let locations = Locations {
            rows: (0..n)
                .map(|i| FeatureSummary {
                    id: Some(format!("<id {i}>")),
                    label: label.clone(),
                    edrqueryendpoint: format!(
                        "https://example.org/edr/collections/c/locations/%3Cid%20{i}%3E&x=\"{label}\""
                    ),
                    datetime: "2026-01-01T00:00:00+00:00/2026-01-01T02:00:00+00:00".into(),
                })
                .collect(),
            number_matched: Some(n),
        };
        let page = page();
        let coverages: Vec<&Value> = doc["coverages"].as_array().unwrap().iter().collect();
        let (head, tail) = page.shell();
        // Many small coverages: each section's own markup is reserved.
        let bare = coverage_html(&doc, &page, None);
        let bare_reserved = head.len() + body_estimate(&doc, &coverages, &page) + tail.len();
        assert_eq!(bare.capacity(), bare_reserved, "the bare page regrew");
        let html = coverage_html(&doc, &page, Some(&locations));
        let reserved = head.len()
            + body_estimate(&doc, &coverages, &page)
            + locations_len(&locations)
            + tail.len();
        assert!(html.len() <= reserved, "{} > {reserved}", html.len());
        assert_eq!(html.capacity(), reserved, "the buffer regrew");
        assert!(html.contains(&format!("Coverage {n} of {n} · {}</h2>", escape(&label))));
        assert!(html.contains("<code>&lt;id 7&gt;</code>"));
        assert!(!html.contains("<id 7>") && !html.contains(&label));
    }

    /// An `items` page fits its reservation too, whatever its features
    /// carry: a 5000-vertex polygon, 40 property columns of escaped text
    /// and URLs, and nested values.
    #[test]
    fn the_features_reservation_covers_the_page() {
        let ring: Vec<Value> = (0..5000)
            .map(|i| json!([-179.123_456_789_012 + i as f64 * 1e-3, -89.987_654_321_098]))
            .collect();
        let wide = |i: usize| {
            let mut properties = serde_json::Map::new();
            for c in 0..40 {
                properties.insert(
                    format!("p<{c}>"),
                    json!(format!("\"{i}\" & <{c}>").repeat(8)),
                );
            }
            properties.insert(
                "url".into(),
                json!(format!("https://example.org/a?b=1&c={i}")),
            );
            properties.insert("nested".into(), json!({"a": [1, 2, {"b": null}], "c": []}));
            Value::Object(properties)
        };
        let feature = |i: usize, geometry: Value| {
            json!({"type": "Feature", "id": format!("f{i}"), "geometry": geometry, "properties": wide(i),
                "links": [{"href": format!("https://example.org/edr/collections/c/items/f{i}"), "rel": "self", "title": "This \"item\""},
                          {"href": "https://example.org/edr/collections/c", "rel": "collection"}]})
        };
        let polygon = json!({"type": "Polygon", "coordinates": [ring]});
        let point = json!({"type": "Point", "coordinates": [24.5, 60.25]});
        let mut features = vec![feature(0, polygon.clone())];
        features.extend((1..200).map(|i| feature(i, point.clone())));
        let list = json!({"type": "FeatureCollection", "features": features, "numberMatched": 200,
            "numberReturned": 200, "links": [{"href": "https://example.org/edr/collections/c/items?limit=200", "rel": "self"}]});
        let single = feature(0, polygon);
        let page = page();
        let (head, tail) = page.shell();
        for (doc, time_stamp) in [(&list, true), (&single, false)] {
            let html = features_page(doc, &page, time_stamp);
            let (features, columns) = features_layout(doc);
            let reserved =
                head.len() + features_estimate(doc, &features, &columns, &page) + tail.len();
            assert!(html.len() <= reserved, "{} > {reserved}", html.len());
            assert!(reserved < 2 * html.len(), "{reserved} vs {}", html.len());
            // Built in the one reservation: the buffer never regrew.
            assert_eq!(html.capacity(), reserved);
        }
    }

    /// Among several coverages, each heading names what tells it apart
    /// (#1138): the stations of an area by their point, the profiles of a
    /// series by their time; what they share is left out. The summary
    /// counts each time once.
    #[test]
    fn coverages_are_named_by_what_tells_them_apart() {
        let at = |x: f64, y: f64, t: &str| {
            json!({"type": "Coverage",
                "domain": {"type": "Domain", "domainType": "PointSeries", "axes": {
                    "x": {"values": [x]}, "y": {"values": [y]}, "t": {"values": [t]}},
                    "referencing": []},
                "ranges": {"a": {"type": "NdArray", "dataType": "float", "axisNames": ["t"],
                    "shape": [1], "values": [1.5]}}})
        };
        let t = "2026-01-01T00:00:00+00:00";
        let doc = json!({"type": "CoverageCollection", "parameters": {},
            "coverages": [at(24.0, 60.0, t), at(25.5, -1.25, t)]});
        let html = coverage_html(&doc, &page(), None);
        assert!(
            html.contains("<h2>Coverage 1 of 2 · 60° N, 24° E</h2>"),
            "{html}"
        );
        assert!(html.contains("<h2>Coverage 2 of 2 · 1.25° S, 25.5° E</h2>"));
        assert!(html.contains("<dt>Time · 1 step</dt>"));
        let doc = json!({"type": "CoverageCollection", "parameters": {},
            "coverages": [at(24.0, 60.0, t), at(24.0, 60.0, "2026-01-01T06:00:00+00:00")]});
        let html = coverage_html(&doc, &page(), None);
        assert!(
            html.contains("<h2>Coverage 2 of 2 · 2026-01-01 06:00 UTC</h2>"),
            "{html}"
        );
        assert!(html.contains("<dt>Time · 2 steps</dt>"));
    }

    /// `json_html_len` measures exactly what `json_html` writes.
    #[test]
    fn json_html_len_is_exact() {
        let value = json!({"a<b": ["x & y", 1.5e-300, -7, true, null],
            "u": "https://example.org/?a=1&b=\"2\"", "o": [{"k": []}, [1, 2]],
            "e": [], "n": null, "s": "it's <ok>"});
        assert_eq!(json_html_len(&value), json_html(&value).len());
        for v in value.as_object().unwrap().values() {
            assert_eq!(json_html_len(v), json_html(v).len(), "{v}");
        }
    }

    #[test]
    fn unravel_is_row_major() {
        assert_eq!(unravel(5, &[2, 3]), [1, 2]);
        assert_eq!(unravel(0, &[]), Vec::<usize>::new());
    }
}
