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
//! `MAX_POSITION_VALUES`, …) bound it too, at about twice the JSON's bytes.
//! It is written into one buffer reserved up front from the value count
//! ([`DataPage::render_into`]), so a large page never holds a second copy
//! of itself or regrows by doubling. The `/locations` list and `items`
//! render their GeoJSON as a feature table, the list through the same
//! budgeted writer as its GeoJSON.
//!
//! Every value is escaped, and every href goes through the workbench's
//! `anchor` (`safe_href`).

use std::fmt::Write as _;
use std::io;

use api_common::workbench::{self as ui, Page, Surface};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use ds_core::html::escape;
use ds_core::model::{CoverageResponse, Location};
use serde_json::Value;

use crate::geojson::encode_path_segment;
use crate::response::{coverage_response_to_json, LocationsContext};

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
pub(crate) struct DataPage<'a> {
    /// The external base URL: workbench assets and navigation.
    pub base: &'a str,
    pub collection_id: &'a str,
    pub collection_title: &'a str,
    /// The page heading, e.g. `Position query`.
    pub title: String,
    /// The request's raw query string, listed decoded (without `f`).
    pub raw_query: Option<&'a str>,
    /// The default representation's URL: the page's JSON switch.
    pub json_url: String,
    /// This representation's links: `self`, an `alternate` per other
    /// format, and the others the JSON representation carries.
    pub links: Vec<Link>,
}

impl DataPage<'_> {
    /// The workbench page's markup before and after its body.
    fn shell(&self) -> (String, String) {
        // The shell's text is escaped, so no response value can produce this.
        const SLOT: &str = "<!--edr-body-->";
        let root = format!("{}{}", self.base, api_common::mounts::EDR);
        let collection_url = format!("{root}/collections/{}", self.collection_id);
        let page = Page {
            surface: Surface {
                base: self.base,
                root: &root,
                api: "edr",
            },
            title: &self.title,
            json_url: &self.json_url,
        }
        .render_with_breadcrumbs(SLOT, "", &[(&collection_url, self.collection_title)]);
        match page.split_once(SLOT) {
            Some((head, tail)) => (head.to_owned(), tail.to_owned()),
            None => (page, String::new()),
        }
    }

    /// The page, its body (trusted markup of escaped values) written by
    /// `body` straight into the one buffer, reserved for `estimate` body
    /// bytes: never a second copy of the body.
    fn render_into(&self, estimate: usize, body: impl FnOnce(&mut String)) -> String {
        let (head, tail) = self.shell();
        let mut out = String::with_capacity(head.len() + estimate + tail.len());
        out.push_str(&head);
        body(&mut out);
        out.push_str(&tail);
        out
    }
}

/// An HTML page as a response.
pub(crate) fn response(body: impl Into<axum::body::Body>) -> Response {
    ([(header::CONTENT_TYPE, CONTENT_TYPE)], body.into()).into_response()
}

/// The HTML page of a data query's result: its CoverageJSON, all of it.
pub(crate) fn coverage_page(result: &CoverageResponse, page: &DataPage) -> String {
    coverage_html(&coverage_response_to_json(result), page)
}

fn coverage_html(doc: &Value, page: &DataPage) -> String {
    let coverages: Vec<&Value> = match doc["coverages"].as_array() {
        Some(coverages) => coverages.iter().collect(),
        None => vec![doc],
    };
    page.render_into(body_estimate(doc, &coverages, page), |body| {
        coverage_body(body, doc, coverages, page)
    })
}

/// The bytes a coverage page's body takes, from above: a value cell is at
/// most `<td>`, an f64's longest JSON form and `</td>` ([`CELL`]); a row
/// table adds a cell per axis coordinate and the row tags per value; the
/// domain table lists every axis value. The request, links, parameters and
/// any other member are measured ([`panels_estimate`], [`json_html_len`]);
/// 16 KiB covers the fixed markup.
fn body_estimate(doc: &Value, coverages: &[&Value], page: &DataPage) -> usize {
    const CELL: usize = 33;
    const ROW: usize = 9;
    const AXIS_VALUE: usize = 40;
    let mut bytes: usize = 16 * 1024 + panels_estimate(page);
    bytes += members_len(doc, &["coverages", "parameters", "domain", "ranges"]);
    // A parameter's row shows its label, unit and observed property, each
    // part of the parameter, then all of it.
    for (key, parameter) in doc["parameters"].as_object().into_iter().flatten() {
        bytes = bytes.saturating_add(256 + escaped_len(key) + 4 * json_html_len(parameter));
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
            let values = range["values"].as_array().map_or(0, Vec::len);
            let names: Vec<&str> = range["axisNames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let per_value = if names.len() >= 2 && names[names.len() - 2..] == ["y", "x"] {
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

fn coverage_body(body: &mut String, doc: &Value, coverages: Vec<&Value>, page: &DataPage) {
    let kind = doc["type"].as_str().unwrap_or("Coverage");
    body.push_str(&ui::page_heading(
        &page.title,
        &format!(
            "{} · CoverageJSON {kind} · {} {}",
            page.collection_title,
            coverages.len(),
            if coverages.len() == 1 {
                "coverage"
            } else {
                "coverages"
            }
        ),
    ));
    body.push_str(&request_panel(page.raw_query));
    body.push_str(&links_panel(&page.links));
    // The document's own members; a single Coverage's domain and ranges are
    // its coverage section, its parameters the parameter table.
    body.push_str(&members_panel(
        "Document",
        doc,
        &["coverages", "parameters", "domain", "ranges"],
    ));
    body.push_str(&parameters_panel(&doc["parameters"]));
    let total = coverages.len();
    for (i, coverage) in coverages.into_iter().enumerate() {
        let heading = if total == 1 {
            "Coverage".to_owned()
        } else {
            format!("Coverage {} of {total}", i + 1)
        };
        coverage_section(body, &heading, coverage, &doc["parameters"]);
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

/// Every link, each an `<a>` (`/req/html/content` A).
fn links_panel(links: &[Link]) -> String {
    let mut out = String::from("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Links</h2></div><div class=\"table-scroll\"><table class=\"properties\"><thead><tr><th scope=\"col\">Relation</th><th scope=\"col\">Title</th><th scope=\"col\">Type</th><th scope=\"col\">Link</th></tr></thead><tbody>");
    for link in links {
        let _ = write!(
            out,
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&link.rel),
            escape(&link.title),
            escape(&link.kind),
            ui::anchor(&link.href, &link.href, "table-link")
        );
    }
    out.push_str("</tbody></table></div></section>");
    out
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
    let mut out = String::from("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Parameters</h2></div><div class=\"table-scroll\"><table class=\"properties\"><thead><tr><th scope=\"col\">Parameter</th><th scope=\"col\">Label</th><th scope=\"col\">Unit</th><th scope=\"col\">Observed property</th><th scope=\"col\">All fields</th></tr></thead><tbody>");
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

/// One coverage: its domain (axes, referencing, any other member) and
/// every range value.
fn coverage_section(out: &mut String, heading: &str, coverage: &Value, parameters: &Value) {
    let domain = &coverage["domain"];
    let _ = write!(
        out,
        "<section class=\"panel spaced\"><div class=\"panel-head\"><h2>{}</h2><span class=\"chip\">{}</span></div><div class=\"panel-body\"><h3>Domain axes</h3><div class=\"table-scroll\"><table class=\"properties\"><thead><tr><th scope=\"col\">Axis</th><th scope=\"col\">Count</th><th scope=\"col\">Values</th></tr></thead><tbody>",
        escape(heading),
        escape(domain["domainType"].as_str().unwrap_or("Domain"))
    );
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
        let listed = (0..count)
            .map(|i| match axis_value(axis, i) {
                Value::Array(tuple) => format!("({})", scalars(&tuple)),
                v => scalar_text(&v),
            })
            .collect::<Vec<_>>()
            .join(", ");
        values.push_str(&escape(&listed));
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
    out.push_str(
        "<h3>Values</h3><p class=\"muted\">An empty cell is a null value: no data there.</p>",
    );
    ranges_html(out, domain, &coverage["ranges"], parameters);
    out.push_str("</div></section>");
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
        for (name, range) in members {
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
        let _ = write!(
            x_header,
            "<th scope=\"col\">{}</th>",
            escape(&scalar_text(&axis_value(x_axis, xi)))
        );
    }
    for (name, range) in members {
        let values = range["values"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        for lead in 0..lead_count {
            let index = unravel(lead, lead_shape);
            let mut caption = escape(name);
            if let Some(unit) = unit_text(&parameters[*name]) {
                let _ = write!(caption, " ({})", escape(&unit));
            }
            for (axis, i) in lead_axes.iter().zip(&index) {
                let _ = write!(
                    caption,
                    " · {} = {}",
                    escape(axis),
                    escape(&scalar_text(&axis_value(&domain["axes"][*axis], *i)))
                );
            }
            let _ = write!(
                out,
                "<div class=\"table-scroll\"><table class=\"properties\"><caption>{caption}</caption><thead><tr><th scope=\"col\">y \\ x</th>{x_header}</tr></thead><tbody>"
            );
            let offset = lead * ny * nx;
            for yi in 0..ny {
                let _ = write!(
                    out,
                    "<tr><th scope=\"row\">{}</th>",
                    escape(&scalar_text(&axis_value(y_axis, yi)))
                );
                for value in &values[offset + yi * nx..offset + (yi + 1) * nx] {
                    push_cell(out, value);
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
    out.push_str("<div class=\"table-scroll\"><table class=\"properties\"><thead><tr>");
    // A composite axis's width: its coordinates; any other axis: one column.
    let mut widths = Vec::with_capacity(axes.len());
    for axis in axes {
        match domain["axes"][*axis]["coordinates"].as_array() {
            Some(coordinates) => {
                for c in coordinates {
                    let _ = write!(out, "<th scope=\"col\">{}</th>", escape(&scalar_text(c)));
                }
                widths.push(Some(coordinates.len()));
            }
            None => {
                let _ = write!(out, "<th scope=\"col\">{}</th>", escape(axis));
                widths.push(None);
            }
        }
    }
    for (name, _) in members {
        let _ = write!(out, "<th scope=\"col\">{}", escape(name));
        if let Some(unit) = unit_text(&parameters[*name]) {
            let _ = write!(out, " ({})", escape(&unit));
        }
        out.push_str("</th>");
    }
    out.push_str("</tr></thead><tbody>");
    let count: usize = shape.iter().product();
    for flat in 0..count {
        out.push_str("<tr>");
        for ((axis, i), width) in axes.iter().zip(unravel(flat, shape)).zip(&widths) {
            let value = axis_value(&domain["axes"][*axis], i);
            match width {
                Some(width) => {
                    for c in 0..*width {
                        push_cell(out, value.get(c).unwrap_or(&Value::Null));
                    }
                }
                None => push_cell(out, &value),
            }
        }
        for (_, range) in members {
            push_cell(out, &range["values"][flat]);
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

/// A table cell: a scalar as text, null as an empty cell.
fn push_cell(out: &mut String, value: &Value) {
    out.push_str("<td>");
    match value {
        Value::Null => {}
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        other => out.push_str(&escape(&scalar_text(other))),
    }
    out.push_str("</td>");
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

/// An i18n object's English (else first) text, or a plain string.
fn i18n_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map
            .get("en")
            .or_else(|| map.values().next())
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// A parameter's unit: its symbol (a string, or `{value, type}`), else its
/// label.
fn unit_text(parameter: &Value) -> Option<String> {
    let unit = &parameter["unit"];
    match &unit["symbol"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(symbol) => symbol
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => i18n_text(&unit["label"]),
    }
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
/// the request is decoded then escaped (at most 6 bytes per raw byte),
/// each link's href shown twice.
fn panels_estimate(page: &DataPage) -> usize {
    let raw = page.raw_query.unwrap_or("");
    let heading = 1024 + 6 * (page.title.len() + page.collection_title.len());
    let request = 512 + 6 * raw.len() + 40 * (raw.matches('&').count() + 1);
    let links = 512
        + page
            .links
            .iter()
            .map(|l| {
                160 + 2 * escaped_len(&l.href)
                    + escaped_len(&l.rel)
                    + escaped_len(&l.title)
                    + escaped_len(&l.kind)
            })
            .sum::<usize>();
    heading + request + links
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
            let href = l["href"].as_str().unwrap_or_default();
            let label = l["title"].as_str().or(l["rel"].as_str()).unwrap_or(href);
            34 + escaped_len(href) + escaped_len(label) + 4
        })
        .sum();
    48 + id_cell
        + escaped_len(geometry["type"].as_str().unwrap_or("null"))
        + coordinates.0
        + cells
        + 14
        + links
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
/// Every location's `datetime` and `parameter-name` are the collection's,
/// so they are listed once above the table; each row carries the
/// location's id, label, point and data links.
pub(crate) fn write_locations(
    locations: &[Location],
    ctx: &LocationsContext,
    page: &DataPage,
    counts: Option<(usize, usize)>,
    mut w: impl io::Write,
) -> io::Result<()> {
    let (head, tail) = page.shell();
    w.write_all(head.as_bytes())?;
    let mut body = ui::page_heading(
        &page.title,
        &format!(
            "{} · {} {}",
            page.collection_title,
            locations.len(),
            if locations.len() == 1 {
                "location"
            } else {
                "locations"
            }
        ),
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
        "<dt>Every location's <code>datetime</code></dt><dd>{}</dd><dt>Every location's <code>parameter-name</code></dt><dd>{}</dd></dl></div></section>",
        escape(&datetime),
        escape(&ctx.parameter_names.join(", "))
    );
    body.push_str("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Locations</h2></div><div class=\"table-scroll\"><table class=\"properties\"><thead><tr><th scope=\"col\">id</th><th scope=\"col\">label</th><th scope=\"col\">Longitude</th><th scope=\"col\">Latitude</th><th scope=\"col\">Data · edrqueryendpoint</th><th scope=\"col\">View</th></tr></thead><tbody>");
    w.write_all(body.as_bytes())?;
    for loc in locations {
        let endpoint = format!(
            "{}/edr/collections/{}/locations/{}",
            ctx.base_url,
            ctx.collection_id,
            encode_path_segment(&loc.id)
        );
        let row = format!(
            "<tr><th scope=\"row\"><code>{}</code></th><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&loc.id),
            escape(&loc.label),
            loc.longitude,
            loc.latitude,
            ui::anchor(&endpoint, &endpoint, "table-link"),
            ui::anchor(&ui::with_format(&endpoint, "html"), "HTML", "table-link")
        );
        w.write_all(row.as_bytes())?;
    }
    w.write_all(b"</tbody></table></div></section>")?;
    w.write_all(tail.as_bytes())
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
    body.push_str("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Features</h2></div><div class=\"table-scroll\"><table class=\"properties\"><thead><tr><th scope=\"col\">id</th><th scope=\"col\">geometry</th>");
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
            "<tr><th scope=\"row\">{id_cell}</th><td>{} <code>{}</code></td>",
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
                let href = l["href"].as_str().unwrap_or_default();
                let label = l["title"].as_str().or(l["rel"].as_str()).unwrap_or(href);
                ui::anchor(href, label, "table-link")
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
        let html = coverage_html(&doc, &page());
        assert_eq!(html.matches("<caption>").count(), 2, "{html}");
        for v in [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12] {
            assert!(html.contains(&format!("<td>{v}</td>")), "{v}");
        }
        assert!(html.contains("<td></td>"));
        assert!(html.contains("t = 2026-01-01T01:00:00Z"));
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
        let html = coverage_html(&doc, &page());
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
            let html = coverage_html(&doc, &page);
            let reserved = head.len() + body_estimate(&doc, &[&doc], &page) + tail.len();
            assert!(html.len() <= reserved, "{} > {reserved}", html.len());
            assert_eq!(html.capacity(), reserved, "the buffer regrew");
            // And not wildly above it: within twice the page.
            assert!(reserved < 2 * html.len(), "{reserved} vs {}", html.len());
        }
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
