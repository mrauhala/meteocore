//! Structured HTML for the members of a collection document (#1140).
//!
//! The collection page keeps every member and link of its JSON, as OGC API
//! `/req/html/content` asks, but as tables, chips and anchors rather than
//! pretty-printed JSON: styles with their legends and links, the extent
//! with the catalog's formatting, parameters, data queries and CRS lists.
//! Labels are plain words, with the JSON member or link relation as the
//! secondary text. A member the page has no layout for is a collapsed
//! nested list, still in full.

use ds_core::html::escape;
use serde_json::{Map, Number, Value};

use super::{
    anchor, collection_time_html, data_query_target, media_badge, media_label, plain_anchor,
    property_value_html, safe_href, vertical_summary, vertical_values, with_format,
};
use crate::rel;

/// Missing metadata, in one wording on every page.
pub(super) const NOT_ADVERTISED: &str = "<span class=\"muted\">Not advertised</span>";

/// An i18n object's English (else first) text, or a plain string: the
/// `label` and `description` members of an EDR or Common parameter.
pub fn i18n_text(value: &Value) -> Option<String> {
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
pub fn unit_text(parameter: &Value) -> Option<String> {
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

/// Whether a member named `key` holds a latitude or a longitude.
fn is_coordinate_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "lat" | "lon" | "lng" | "latitude" | "longitude"
    ) || ["_lat", "_lon", "_lng", "latitude", "longitude"]
        .iter()
        .any(|suffix| key.ends_with(suffix))
}

/// A number's decimal text without trailing zeros (and `-0` as `0`).
fn trim_decimals(text: String) -> String {
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    };
    if text == "-0" {
        "0".into()
    } else {
        text
    }
}

/// A JSON number as the workbench shows it: an integral float without its
/// `.0` (`135`, not `135.0`), and the value of a latitude or longitude
/// member (`key`) to at most 5 decimals, about a metre. The second value
/// says whether the text is rounded.
pub(super) fn number_text(key: &str, n: &Number) -> (String, bool) {
    if n.is_i64() || n.is_u64() {
        return (n.to_string(), false);
    }
    let Some(f) = n.as_f64().filter(|f| f.is_finite()) else {
        return (n.to_string(), false);
    };
    if is_coordinate_key(key) {
        let text = trim_decimals(format!("{f:.5}"));
        let rounded = text.parse::<f64>().ok() != Some(f);
        return (text, rounded);
    }
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return (trim_decimals(format!("{f:.0}")), false);
    }
    (n.to_string(), false)
}

/// [`number_text`] as HTML: a rounded value keeps the exact one in its
/// `title`.
pub(super) fn number_html(key: &str, n: &Number) -> String {
    match number_text(key, n) {
        (text, true) => format!(
            "<span class=\"rounded\" title=\"{}\">{}</span>",
            escape(&n.to_string()),
            escape(&text)
        ),
        (text, false) => escape(&text),
    }
}

/// The short form of a link relation: a registered OGC or IANA relation URI
/// without its prefix.
fn relation_term(rel: &str) -> &str {
    rel.strip_prefix("http://www.opengis.net/def/rel/ogc/1.0/")
        .or_else(|| rel.strip_prefix("http://www.iana.org/assignments/relation/"))
        .unwrap_or(rel)
}

/// A plain label for a link relation, in its short or registered form.
pub(super) fn relation_label(rel: &str) -> Option<&'static str> {
    Some(match relation_term(rel) {
        "map" => "Map image",
        "tilesets-map" => "Map tiles",
        "tilesets-vector" => "Vector tiles",
        "tilesets-coverage" => "Coverage tiles",
        "tiles" => "Tiles",
        "styles" => "Styles",
        "legend" => "Legend",
        "items" => "Features",
        "queryables" => "Queryables",
        "sortables" => "Sortables",
        "schema" => "Schema",
        "data" | "geodata" => "Data",
        "conformance" => "Conformance",
        "license" => "License",
        "describedby" => "Description",
        "service-desc" => "API definition",
        "service-doc" => "API documentation",
        "self" => "This document",
        "alternate" => "Other representation",
        "collection" => "Collection",
        "up" => "Parent",
        "tiling-schemes" => "Tiling schemes",
        "tiling-scheme" => "Tiling scheme",
        "next" => "Next page",
        "prev" => "Previous page",
        _ => return None,
    })
}

/// A relation as secondary text: its short term, the full relation in the
/// `title`.
fn relation_code(rel: &str) -> String {
    format!(
        "<code class=\"term\" title=\"{}\">{}</code>",
        escape(rel),
        escape(relation_term(rel))
    )
}

/// The name a media type is known by (`JSON`, `PNG`), else the type itself.
fn media_name(media: &str) -> String {
    let essence = media.split(';').next().unwrap_or_default().trim();
    media_label(essence).map_or_else(|| essence.to_owned(), str::to_owned)
}

/// `href` without its `f` parameter: the resource every representation of
/// it shares.
fn resource_key(href: &str) -> String {
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let mut out = form_urlencoded::Serializer::new(String::new());
    let mut kept = false;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key != "f" {
            out.append_pair(&key, &value);
            kept = true;
        }
    }
    if kept {
        format!("{path}?{}", out.finish())
    } else {
        path.to_owned()
    }
}

/// Relations whose resource has an HTML view of its own.
fn has_html_view(rel: &str) -> bool {
    matches!(
        relation_term(rel),
        "data" | "child" | "collection" | "conformance" | "up" | "items"
    )
}

/// `links` as one card per resource: links to one href, or to
/// representations of one resource that differ only in `f`, share a card
/// (the JSON and HTML items, a map offered as PNG and JPEG, a tileset list
/// offered under several relations). A card is labelled in plain words
/// from its relation, with the relation term, the link titles and media
/// types as secondary text; every href stays an anchor.
pub(super) fn resource_cards(links: &[&Value]) -> String {
    fn text<'a>(link: &'a Value, key: &str) -> &'a str {
        link[key].as_str().unwrap_or_default()
    }
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for link in links {
        let key = resource_key(text(link, "href"));
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push(link),
            None => groups.push((key, vec![link])),
        }
    }
    let mut body = String::from("<div class=\"endpoint-list resource-links\">");
    for (_, group) in groups {
        let mut rels: Vec<&str> = Vec::new();
        let mut terms: Vec<&str> = Vec::new();
        for link in &group {
            let rel = text(link, "rel");
            if !terms.contains(&relation_term(rel)) {
                terms.push(relation_term(rel));
                rels.push(rel);
            }
        }
        let first = group[0];
        let label = rels
            .iter()
            .find_map(|rel| relation_label(rel))
            .map(str::to_owned)
            .or_else(|| {
                group
                    .iter()
                    .map(|l| text(l, "title"))
                    .find(|t| !t.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| text(first, "rel").to_owned());
        let html_link = group
            .iter()
            .find(|l| media_name(text(l, "type")) == "HTML")
            .copied();
        // The address shown is the resource's own: its first link that is
        // not the HTML view, if it has one.
        let resource = group
            .iter()
            .find(|l| media_name(text(l, "type")) != "HTML")
            .copied()
            .unwrap_or(first);
        let (target, page) = match html_link {
            Some(link) => (text(link, "href").to_owned(), true),
            None if rels.iter().any(|rel| has_html_view(rel)) => {
                (with_format(text(first, "href"), "html"), true)
            }
            None => (text(first, "href").to_owned(), false),
        };
        let badge = if page {
            String::new()
        } else {
            media_badge(text(first, "type"))
        };
        let mut secondary: Vec<String> = Vec::new();
        // The titles that say more than the label: not the label itself
        // (`Map` beside "Map image"), nor an HTML view's own title.
        let lower = label.to_lowercase();
        let mut titles: Vec<&str> = Vec::new();
        for link in &group {
            let title = text(link, "title");
            let said = title.to_lowercase();
            if title.is_empty()
                || lower.starts_with(&said)
                || said.starts_with(&lower)
                || (media_name(text(link, "type")) == "HTML" && group.len() > 1)
                || titles.contains(&title)
            {
                continue;
            }
            titles.push(title);
        }
        if !titles.is_empty() {
            secondary.push(escape(&titles.join(" · ")));
        }
        secondary.push(rels.iter().map(|rel| relation_code(rel)).collect());
        let mut types: Vec<String> = Vec::new();
        for media in group.iter().map(|l| text(l, "type")) {
            let name = media_name(media);
            if !media.is_empty() && !types.contains(&name) {
                types.push(name);
            }
        }
        if !types.is_empty() {
            let full: Vec<&str> = group
                .iter()
                .map(|l| text(l, "type"))
                .filter(|t| !t.is_empty())
                .collect();
            secondary.push(format!(
                "<span title=\"{}\">{}</span>",
                escape(&full.join(", ")),
                escape(&types.join(", "))
            ));
        }
        if rels.contains(&rel::MAP) || rels.contains(&"map") {
            secondary.push("Covers the whole extent unless the request sets a bbox".into());
        }
        // Every other href of the resource (the JSON beside an HTML view)
        // stays an anchor, named by its media type.
        let mut others = String::new();
        let mut listed = vec![target.as_str()];
        for link in &group {
            let href = text(link, "href");
            if listed.contains(&href) {
                continue;
            }
            listed.push(href);
            let media = text(link, "type");
            others.push_str(&anchor(
                href,
                &if media.is_empty() {
                    href.to_owned()
                } else {
                    format!("As {}", media_name(media))
                },
                "chip",
            ));
        }
        if !others.is_empty() {
            others = format!("<div class=\"chip-row\">{others}</div>");
        }
        body.push_str(&format!(
            "<div class=\"endpoint\"><div><a class=\"resource-title\" href=\"{}\"><strong>{}</strong>{badge}<code>{}</code></a><p>{}</p>{others}</div></div>",
            escape(safe_href(&target)),
            escape(&label),
            escape(text(resource, "href")),
            secondary.join(" · ")
        ));
    }
    body.push_str("</div>");
    body
}

/// A CRS URI's short name: `EPSG:4326`, `CRS84`; a CURIE or any other text
/// as it is.
pub(super) fn crs_name(crs: &str) -> String {
    let crs = crs.trim();
    let curie = crs
        .strip_prefix('[')
        .and_then(|c| c.strip_suffix(']'))
        .unwrap_or(crs);
    for prefix in [
        "http://www.opengis.net/def/crs/",
        "https://www.opengis.net/def/crs/",
    ] {
        if let Some(rest) = curie.strip_prefix(prefix) {
            if let [authority, _version, code] = rest.split('/').collect::<Vec<_>>()[..] {
                return if authority == "OGC" {
                    code.to_owned()
                } else {
                    format!("{authority}:{code}")
                };
            }
        }
    }
    curie.to_owned()
}

/// A CRS as a chip with its short name, the full identifier in its `title`.
pub(super) fn crs_chip(crs: &str) -> String {
    format!(
        "<span class=\"chip crs\" title=\"{}\">{}</span>",
        escape(crs),
        escape(&crs_name(crs))
    )
}

/// A list of CRS identifiers as chips.
fn crs_chips(list: &[Value]) -> String {
    format!(
        "<span class=\"chip-row\">{}</span>",
        list.iter()
            .map(|crs| match crs.as_str() {
                Some(crs) => crs_chip(crs),
                None => format!("<span class=\"chip\">{}</span>", structured_html("", crs)),
            })
            .collect::<String>()
    )
}

/// A plain name for a collection's `dataType` or `itemType`.
fn kind_label(kind: &str) -> &str {
    match kind {
        "map" => "Map layer",
        "vector" => "Vector data",
        "coverage" => "Coverage",
        "feature" => "Features",
        "record" => "Records",
        other => other,
    }
}

/// What a collection holds: its `dataType` (else its `itemType`) in plain
/// words, both terms as secondary text.
pub(super) fn kind_html(doc: &Value) -> String {
    let data_type = doc["dataType"].as_str();
    let item_type = doc["itemType"].as_str();
    let Some(kind) = data_type.or(item_type) else {
        return "Environmental data".into();
    };
    let terms: Vec<String> = [("dataType", data_type), ("itemType", item_type)]
        .into_iter()
        .filter_map(|(member, value)| value.map(|v| format!("{member} <code>{}</code>", escape(v))))
        .collect();
    format!(
        "{}<small class=\"term\">{}</small>",
        escape(kind_label(kind)),
        terms.join(" · ")
    )
}

/// A bbox's west, south, east and north: the xy values of a 4- or 6-value
/// CRS84 box.
pub(super) fn bbox_xy(bbox: &Value) -> Option<[f64; 4]> {
    let b = bbox.as_array()?;
    let xy = match b.len() {
        4 => [0, 1, 2, 3],
        6 => [0, 1, 3, 4],
        _ => return None,
    };
    let mut out = [0.0; 4];
    for (slot, i) in out.iter_mut().zip(xy) {
        *slot = b[i].as_f64().filter(|n| n.is_finite())?;
    }
    Some(out)
}

/// The catalog's bbox text: `Global`, or west, south → east, north to 2
/// decimals.
pub(super) fn bbox_label(v: [f64; 4]) -> String {
    if v == [-180., -90., 180., 90.] {
        "Global".into()
    } else {
        format!("{:.2}, {:.2} → {:.2}, {:.2}", v[0], v[1], v[2], v[3])
    }
}

/// The exact `west,south,east,north` of a bbox, as a `bbox` parameter.
fn bbox_exact(v: [f64; 4]) -> String {
    v.iter().map(f64::to_string).collect::<Vec<_>>().join(",")
}

/// An advertised temporal resolution in words (`Every 5 min`), else as
/// given.
pub(super) fn cadence_label(resolution: &str) -> String {
    ds_core::datetime::parse_iso8601_duration(resolution)
        .ok()
        .map(|step| {
            let secs = step.num_seconds();
            if secs % 86400 == 0 {
                format!("Every {} d", secs / 86400)
            } else if secs % 3600 == 0 {
                format!("Every {} h", secs / 3600)
            } else if secs % 60 == 0 {
                format!("Every {} min", secs / 60)
            } else {
                format!("Every {secs} s")
            }
        })
        .unwrap_or_else(|| resolution.to_owned())
}

/// The number of advertised time steps: the grid's cell count, else the
/// listed values or coordinates.
fn time_steps(temporal: &Value) -> Option<u64> {
    temporal["grid"]["cellsCount"]
        .as_u64()
        .or_else(|| temporal["values"].as_array().map(|v| v.len() as u64))
        .or_else(|| {
            temporal["grid"]["coordinates"]
                .as_array()
                .map(|v| v.len() as u64)
        })
        .filter(|n| *n > 0)
}

fn steps_text(n: u64) -> String {
    format!("{n} {}", if n == 1 { "step" } else { "steps" })
}

fn fact(label: &str, value: &str, wide: bool) -> String {
    format!(
        "<div class=\"fact{}\"><small>{}</small><strong>{value}</strong></div>",
        if wide { " wide" } else { "" },
        escape(label)
    )
}

/// The Overview's coverage facts: time with the catalog's formatting,
/// cadence and steps where advertised, and the bounds to 2 decimals with
/// the exact values in the `title` and a Copy bbox button.
pub(super) fn coverage_facts(doc: &Value) -> String {
    let extent = &doc["extent"];
    let temporal = &extent["temporal"];
    let mut out = String::from("<div class=\"coverage-facts\">");
    let time = if temporal["interval"][0].is_array() {
        collection_time_html(temporal)
    } else {
        NOT_ADVERTISED.into()
    };
    out.push_str(&fact("Time", &time, true));
    if let Some(resolution) = temporal["grid"]["resolution"].as_str() {
        out.push_str(&fact("Cadence", &escape(&cadence_label(resolution)), false));
    }
    if let Some(steps) = time_steps(temporal) {
        out.push_str(&fact("Steps", &escape(&steps_text(steps)), false));
    }
    match bbox_xy(&extent["spatial"]["bbox"][0]) {
        Some(v) => out.push_str(&format!(
            "<div class=\"fact wide\"><small>Bounds · CRS84 · west, south → east, north</small><div class=\"bbox-fact\"><strong class=\"mono\" title=\"{exact}\">{}</strong><button type=\"button\" class=\"btn small enhanced\" data-copy=\"{exact}\">Copy bbox</button></div></div>",
            escape(&bbox_label(v)),
            exact = escape(&bbox_exact(v))
        )),
        None => out.push_str(&fact("Bounds · CRS84", NOT_ADVERTISED, true)),
    }
    out.push_str(&super::vertical_coverage(&extent["vertical"]));
    out.push_str("</div>");
    out
}

/// A parameter's label: its `label`, else its observed property's.
fn parameter_label(parameter: &Value) -> Option<String> {
    i18n_text(&parameter["label"])
        .or_else(|| i18n_text(&parameter["observedProperty"]["label"]))
        .filter(|label| !label.trim().is_empty())
}

/// A collection's parameters as a table: the name a request gives
/// `parameter-name`, the label, the unit (or that the source gives none),
/// and each parameter's own levels and times where it advertises them.
/// With `all_fields` each row also holds the entry in full.
pub(super) fn parameter_table(parameters: &Map<String, Value>, all_fields: bool) -> String {
    let levels = parameters
        .values()
        .any(|p| p["extent"]["vertical"].is_object());
    let times = parameters
        .values()
        .any(|p| p["extent"]["temporal"].is_object());
    let mut out = String::from("<div class=\"table-scroll\"><table class=\"data-table parameter-table\"><thead><tr><th scope=\"col\">Name <small class=\"term\">for <code>parameter-name</code></small></th><th scope=\"col\">Label</th><th scope=\"col\">Unit</th>");
    if levels {
        out.push_str("<th scope=\"col\">Levels</th>");
    }
    if times {
        out.push_str("<th scope=\"col\">Time</th>");
    }
    if all_fields {
        out.push_str("<th scope=\"col\">All fields</th>");
    }
    out.push_str("</tr></thead><tbody>");
    for (name, parameter) in parameters {
        let label = parameter_label(parameter);
        let mut label_html = label.as_deref().map(escape).unwrap_or_default();
        // A description that says more than the label and unit (an RGB
        // composite's channels) is shown under it.
        if let Some(description) = i18n_text(&parameter["description"]) {
            if !description.trim().is_empty()
                && !label
                    .as_deref()
                    .is_some_and(|label| description.starts_with(label))
            {
                label_html.push_str(&format!(
                    "<small class=\"secondary\">{}</small>",
                    escape(&description)
                ));
            }
        }
        let unit = match unit_text(parameter) {
            Some(unit) if !unit.trim().is_empty() => {
                let long = i18n_text(&parameter["unit"]["label"]).filter(|l| *l != unit);
                match long {
                    Some(long) => {
                        format!("<span title=\"{}\">{}</span>", escape(&long), escape(&unit))
                    }
                    None => escape(&unit),
                }
            }
            _ => "<span class=\"muted\">unit not provided by source</span>".into(),
        };
        out.push_str(&format!(
            "<tr data-parameter=\"{}\"><th scope=\"row\"><code>{}</code></th><td>{label_html}</td><td data-label=\"Unit\">{unit}</td>",
            escape(name),
            escape(name)
        ));
        let extent = &parameter["extent"];
        if levels {
            let vertical = &extent["vertical"];
            let summary = vertical_summary(vertical).unwrap_or_default();
            let values = vertical_values(vertical);
            out.push_str(&format!(
                "<td data-label=\"Levels\">{summary}{}</td>",
                if values.is_empty() {
                    String::new()
                } else {
                    format!(
                        "<small class=\"secondary mono\">{}</small>",
                        escape(&values.join(", "))
                    )
                }
            ));
        }
        if times {
            let temporal = &extent["temporal"];
            let mut cell = if temporal["interval"][0].is_array() {
                collection_time_html(temporal)
            } else {
                "<span class=\"muted\">The collection's</span>".into()
            };
            if let Some(steps) = time_steps(temporal) {
                cell.push_str(&format!(
                    "<small class=\"secondary\">{}</small>",
                    escape(&steps_text(steps))
                ));
            }
            out.push_str(&format!("<td data-label=\"Time\">{cell}</td>"));
        }
        if all_fields {
            out.push_str(&format!(
                "<td><details><summary>Show</summary>{}</details></td>",
                structured_html(name, parameter)
            ));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
    out
}

/// The first link of `links` with relation `short` or its registered form.
fn find_link<'a>(links: &'a [Value], short: &str) -> Option<&'a Value> {
    links
        .iter()
        .find(|l| l["rel"].as_str().is_some_and(|r| relation_term(r) == short))
}

/// A link as an anchor labelled in plain words, its relation and title in
/// the anchor's `title`, its media type as a badge.
fn link_anchor(link: &Value) -> String {
    let href = link["href"].as_str().unwrap_or_default();
    let rel = link["rel"].as_str().unwrap_or_default();
    let title = link["title"].as_str().unwrap_or_default();
    let label = relation_label(rel)
        .map(str::to_owned)
        .or_else(|| (!title.is_empty()).then(|| title.to_owned()))
        .unwrap_or_else(|| rel.to_owned());
    let hover = [rel, title]
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    format!(
        "<a href=\"{}\" title=\"{}\">{}</a>{}",
        escape(safe_href(href)),
        escape(&hover),
        escape(&label),
        media_badge(link["type"].as_str().unwrap_or_default())
    )
}

/// The members of `object` other than `known`, if any.
fn rest(object: &Value, known: &[&str]) -> Option<Value> {
    let rest: Map<String, Value> = object
        .as_object()?
        .iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!rest.is_empty()).then_some(Value::Object(rest))
}

/// Other members of an entry, collapsed.
fn other_fields(object: &Value, known: &[&str]) -> String {
    rest(object, known)
        .map(|rest| {
            format!(
                "<details><summary>Other fields</summary>{}</details>",
                structured_html("", &rest)
            )
        })
        .unwrap_or_default()
}

/// A collection's styles as a table: each style's title and id, a legend
/// thumbnail linking to its legend, and every link as an anchor (map image,
/// map tiles, legend, …).
pub(super) fn styles_table(styles: &[Value]) -> String {
    let mut out = String::from("<div class=\"table-scroll\"><table class=\"data-table styles-table\"><thead><tr><th scope=\"col\">Style</th><th scope=\"col\">Legend</th><th scope=\"col\">Links</th></tr></thead><tbody>");
    for style in styles {
        let id = style["id"].as_str().unwrap_or_default();
        let title = style["title"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(id);
        let links = style["links"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let legend = find_link(links, "legend")
            .and_then(|l| l["href"].as_str())
            .filter(|href| safe_href(href) != "#")
            .map(|href| {
                format!(
                    "<a class=\"legend-thumb\" href=\"{}\"><img src=\"{}\" alt=\"Legend of the {} style\" loading=\"lazy\"></a>",
                    escape(href),
                    escape(&with_format(href, "png")),
                    escape(title)
                )
            })
            .unwrap_or_else(|| "<span class=\"muted\">No legend</span>".into());
        let anchors: String = links
            .iter()
            .map(|link| format!("<li>{}</li>", link_anchor(link)))
            .collect();
        out.push_str(&format!(
            "<tr data-style=\"{}\"><th scope=\"row\">{}<small class=\"secondary\"><code>{}</code></small>{}</th><td>{legend}</td><td data-label=\"Links\"><ul class=\"link-list\">{anchors}</ul></td></tr>",
            escape(id),
            escape(title),
            escape(id),
            other_fields(style, &["id", "title", "links"])
        ));
    }
    out.push_str("</tbody></table></div>");
    out
}

/// Plain names of the EDR query unit lists.
const QUERY_UNITS: [(&str, &str); 3] = [
    ("within_units", "Radius"),
    ("width_units", "Corridor width"),
    ("height_units", "Corridor height"),
];

/// Whether a data query of `query_type` takes a unit from one of the
/// [`QUERY_UNITS`] lists: a radius, a corridor's width and height, and a
/// cube's vertical axis. No other EDR query has a unit parameter.
fn takes_units(query_type: &str) -> bool {
    matches!(query_type, "radius" | "corridor" | "cube")
}

/// A collection's `data_queries` as a table: each query's title and
/// description with an anchor to it, its output formats (the default
/// marked), CRS and units. The Units column is there only when a query
/// lists units or takes them; a query without unit parameters shows a dash
/// in it, never "Not advertised".
pub(super) fn data_queries_table(queries: &Map<String, Value>) -> String {
    let query_type = |name: &str, query: &Value| -> String {
        query["link"]["variables"]["query_type"]
            .as_str()
            .unwrap_or(name)
            .to_owned()
    };
    let lists_units = |query: &Value| {
        QUERY_UNITS
            .iter()
            .any(|(member, _)| query["link"]["variables"][*member].is_array())
    };
    let units_column = queries
        .iter()
        .any(|(name, query)| lists_units(query) || takes_units(&query_type(name, query)));
    let mut out = String::from("<div class=\"table-scroll\"><table class=\"data-table query-table\"><thead><tr><th scope=\"col\">Query</th><th scope=\"col\">Output formats</th><th scope=\"col\">CRS</th>");
    if units_column {
        out.push_str("<th scope=\"col\">Units</th>");
    }
    out.push_str("</tr></thead><tbody>");
    for (name, query) in queries {
        let link = &query["link"];
        let variables = &link["variables"];
        let title = variables["title"]
            .as_str()
            .or(link["title"].as_str())
            .unwrap_or(name);
        let heading = match link["href"].as_str() {
            Some(href) => format!(
                "{}<small class=\"secondary\"><code>{}</code></small>",
                plain_anchor(&data_query_target(name, href), title),
                escape(href)
            ),
            None => escape(title),
        };
        let description = variables["description"]
            .as_str()
            .map(|d| format!("<small class=\"secondary\">{}</small>", escape(d)))
            .unwrap_or_default();
        let default = variables["default_output_format"].as_str();
        let formats: String = variables["output_formats"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|format| {
                let text = format.as_str().map(str::to_owned).unwrap_or_else(|| format.to_string());
                if Some(text.as_str()) == default {
                    format!(
                        "<span class=\"chip teal\" title=\"Default output format\">{} · default</span>",
                        escape(&text)
                    )
                } else {
                    format!("<span class=\"chip\">{}</span>", escape(&text))
                }
            })
            .collect();
        let formats = if formats.is_empty() {
            match default {
                Some(d) => format!("<span class=\"chip teal\">{} · default</span>", escape(d)),
                None => NOT_ADVERTISED.into(),
            }
        } else {
            format!("<span class=\"chip-row\">{formats}</span>")
        };
        let crs: String = variables["crs_details"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|detail| {
                format!(
                    "<span class=\"chip crs\" title=\"{}\">{}</span>",
                    escape(detail["wkt"].as_str().unwrap_or_default()),
                    escape(detail["crs"].as_str().unwrap_or_default())
                )
            })
            .collect();
        let crs = if crs.is_empty() {
            NOT_ADVERTISED.into()
        } else {
            format!("<span class=\"chip-row\">{crs}</span>")
        };
        let units: Vec<String> = QUERY_UNITS
            .iter()
            .filter_map(|(member, label)| {
                let list: Vec<String> = variables[*member]
                    .as_array()?
                    .iter()
                    .map(|u| {
                        u.as_str()
                            .map(escape)
                            .unwrap_or_else(|| escape(&u.to_string()))
                    })
                    .collect();
                Some(format!(
                    "<div>{label}: {}<small class=\"term\"><code>{member}</code></small></div>",
                    list.join(", ")
                ))
            })
            .collect();
        let units = if !units_column {
            String::new()
        } else if !units.is_empty() {
            format!("<td data-label=\"Units\">{}</td>", units.concat())
        } else if takes_units(&query_type(name, query)) {
            format!("<td data-label=\"Units\">{NOT_ADVERTISED}</td>")
        } else {
            "<td data-label=\"Units\"><span class=\"muted\" title=\"This query takes no unit\">—</span></td>".into()
        };
        let mut known: Vec<&str> = vec![
            "title",
            "description",
            "output_formats",
            "default_output_format",
            "crs_details",
        ];
        known.extend(QUERY_UNITS.iter().map(|(member, _)| *member));
        // The members shown nowhere else, under one disclosure: the link's
        // relation, a query type, any extension.
        let mut extra = rest(query, &["link"])
            .and_then(|rest| rest.as_object().cloned())
            .unwrap_or_default();
        for (member, value, shown) in [
            ("link", link, &["href", "variables"][..]),
            ("variables", variables, &known[..]),
        ] {
            if let Some(rest) = rest(value, shown) {
                extra.insert(member.into(), rest);
            }
        }
        let others = other_fields(&Value::Object(extra), &[]);
        out.push_str(&format!(
            "<tr data-query=\"{}\"><th scope=\"row\">{heading}{description}{others}</th><td data-label=\"Formats\">{formats}</td><td data-label=\"CRS\">{crs}</td>{units}</tr>",
            escape(name)
        ));
    }
    out.push_str("</tbody></table></div>");
    out
}

/// A collection's extent: its bounds, time and vertical axis with the
/// catalog's formatting and the exact values beside them, and every member
/// in full under a disclosure.
pub(super) fn extent_html(extent: &Value) -> String {
    let mut rows = String::new();
    let mut row = |label: &str, value: String| {
        rows.push_str(&format!("<dt>{}</dt><dd>{value}</dd>", escape(label)));
    };
    let spatial = &extent["spatial"];
    if spatial.is_object() {
        let boxes: Vec<String> = spatial["bbox"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|bbox| {
                let label = bbox_xy(bbox)
                    .map(|v| escape(&bbox_label(v)))
                    .unwrap_or_default();
                let exact = bbox
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| bbox.to_string());
                format!(
                    "<div>{label}<small class=\"secondary mono\">{}</small></div>",
                    escape(&exact)
                )
            })
            .collect();
        row(
            "Bounds",
            if boxes.is_empty() {
                NOT_ADVERTISED.into()
            } else {
                boxes.concat()
            },
        );
        if let Some(crs) = spatial["crs"].as_str() {
            row("Coordinate system", crs_chip(crs));
        }
        if let Some(axes) = spatial["grid"].as_array().filter(|a| !a.is_empty()) {
            let cells: Vec<String> = axes
                .iter()
                .map(|axis| {
                    axis["cellsCount"]
                        .as_u64()
                        .map_or_else(|| "?".into(), |n| n.to_string())
                })
                .collect();
            row("Grid cells", escape(&cells.join(" × ")));
        }
    }
    let temporal = &extent["temporal"];
    if temporal.is_object() {
        let time = if temporal["interval"][0].is_array() {
            collection_time_html(temporal)
        } else {
            NOT_ADVERTISED.into()
        };
        row("Time", time);
        if let Some(resolution) = temporal["grid"]["resolution"].as_str() {
            row("Cadence", escape(&cadence_label(resolution)));
        }
        if let Some(steps) = time_steps(temporal) {
            row("Steps", escape(&steps_text(steps)));
        }
        if let Some(trs) = temporal["trs"].as_str() {
            let name = trs.rsplit('/').next().unwrap_or(trs);
            row(
                "Calendar",
                format!(
                    "<span class=\"chip\" title=\"{}\">{}</span>",
                    escape(trs),
                    escape(name)
                ),
            );
        }
    }
    let vertical = &extent["vertical"];
    if let Some(summary) = vertical_summary(vertical) {
        let values = vertical_values(vertical);
        row(
            "Vertical",
            if values.is_empty() {
                summary
            } else {
                format!(
                    "{summary}<small class=\"secondary mono\">{}</small>",
                    escape(&values.join(", "))
                )
            },
        );
    }
    format!(
        "<dl class=\"definition extent-summary\">{rows}</dl><details class=\"all-fields\"><summary>All extent fields</summary>{}</details>",
        structured_html("extent", extent)
    )
}

/// Any JSON value as nested definition lists and lists: every member and
/// value kept, URLs as anchors, numbers by [`number_text`], and no `<pre>`.
pub(super) fn structured_html(key: &str, value: &Value) -> String {
    match value {
        Value::Object(map) if map.is_empty() => "<span class=\"muted\">Empty</span>".into(),
        Value::Object(map) => format!(
            "<dl class=\"definition structured\">{}</dl>",
            map.iter()
                .map(|(k, v)| format!(
                    "<dt><code>{}</code></dt><dd>{}</dd>",
                    escape(k),
                    structured_html(k, v)
                ))
                .collect::<String>()
        ),
        Value::Array(items) if items.is_empty() => "<span class=\"muted\">Empty list</span>".into(),
        Value::Array(items) if items.iter().all(|v| !v.is_array() && !v.is_object()) => format!(
            "<span class=\"chip-row\">{}</span>",
            items
                .iter()
                .map(|v| format!("<span class=\"chip\">{}</span>", scalar_html(key, v)))
                .collect::<String>()
        ),
        Value::Array(items) => format!(
            "<ol class=\"structured-list\">{}</ol>",
            items
                .iter()
                .map(|v| format!("<li>{}</li>", structured_html(key, v)))
                .collect::<String>()
        ),
        _ => scalar_html(key, value),
    }
}

/// A scalar as [`structured_html`] shows it: a URL as an anchor, and JSON
/// `null` as itself, since it is a value, not a missing one (an open end
/// of a time interval).
fn scalar_html(key: &str, value: &Value) -> String {
    match value {
        Value::String(s) if s.starts_with("https://") || s.starts_with("http://") => {
            plain_anchor(s, s)
        }
        Value::Null => "<code>null</code>".into(),
        _ => property_value_html(key, value),
    }
}

/// A plain label for a collection document member.
fn member_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "id" => "Identifier",
        "title" => "Title",
        "description" => "Description",
        "keywords" => "Keywords",
        "attribution" => "Attribution",
        "extent" => "Extent",
        "crs" => "Coordinate systems",
        "storageCrs" => "Storage coordinate system",
        "storageCrsCoordinateEpoch" => "Storage coordinate epoch",
        "dataType" => "Data type",
        "itemType" => "Item type",
        "numberItems" => "Number of items",
        "styles" => "Styles",
        "defaultStyle" => "Default style",
        "parameter_names" => "Parameters",
        "data_queries" => "Data queries",
        "output_formats" => "Output formats",
        "within_units" => "Radius units",
        "width_units" => "Corridor width units",
        "height_units" => "Corridor height units",
        "tileMatrixSetLinks" => "Tile matrix sets",
        _ => return None,
    })
}

fn member_heading(key: &str) -> String {
    match member_label(key) {
        Some(label) => format!(
            "{}<small class=\"term\"><code>{}</code></small>",
            escape(label),
            escape(key)
        ),
        None => format!("<code>{}</code>", escape(key)),
    }
}

/// The value of a member without a layout of its own: scalars and lists of
/// scalars inline, anything else as a collapsed nested list.
fn member_value(key: &str, value: &Value) -> String {
    match value {
        Value::Array(items) if items.is_empty() => "<span class=\"muted\">None</span>".into(),
        Value::Array(items) if items.iter().all(|v| !v.is_array() && !v.is_object()) => {
            structured_html(key, value)
        }
        Value::Array(items) => format!(
            "<details><summary>{} {}</summary>{}</details>",
            items.len(),
            if items.len() == 1 { "entry" } else { "entries" },
            structured_html(key, value)
        ),
        Value::Object(map) => format!(
            "<details><summary>{} {}</summary>{}</details>",
            map.len(),
            if map.len() == 1 { "member" } else { "members" },
            structured_html(key, value)
        ),
        _ => scalar_html(key, value),
    }
}

/// Every member of a collection document but its links: scalars and short
/// lists in a key/value table, then the extent, parameters, styles and
/// data queries each under its own heading as a table.
pub(super) fn metadata_html(doc: &Value) -> String {
    let Some(members) = doc.as_object() else {
        return String::new();
    };
    let mut rows = String::new();
    let mut sections = String::new();
    let mut section = |key: &str, content: String| {
        sections.push_str(&format!(
            "<div class=\"metadata-member\" data-property=\"{}\"><h3>{}</h3>{content}</div>",
            escape(key),
            member_heading(key)
        ));
    };
    for (key, value) in members {
        match (key.as_str(), value) {
            ("links", _) => {}
            ("extent", Value::Object(_)) => section(key, extent_html(value)),
            ("styles", Value::Array(styles)) if !styles.is_empty() => {
                section(key, styles_table(styles))
            }
            ("parameter_names", Value::Object(parameters)) if !parameters.is_empty() => {
                section(key, parameter_table(parameters, true))
            }
            ("data_queries", Value::Object(queries)) if !queries.is_empty() => {
                section(key, data_queries_table(queries))
            }
            ("crs", Value::Array(list)) if !list.is_empty() => rows.push_str(&format!(
                "<tr data-property=\"crs\"><th scope=\"row\">{}</th><td>{}</td></tr>",
                member_heading(key),
                crs_chips(list)
            )),
            ("storageCrs", Value::String(crs)) => rows.push_str(&format!(
                "<tr data-property=\"storageCrs\"><th scope=\"row\">{}</th><td>{}</td></tr>",
                member_heading(key),
                crs_chip(crs)
            )),
            _ => rows.push_str(&format!(
                "<tr data-property=\"{}\"><th scope=\"row\">{}</th><td>{}</td></tr>",
                escape(key),
                member_heading(key),
                member_value(key, value)
            )),
        }
    }
    format!(
        "<div class=\"table-scroll\"><table class=\"properties property-table\"><thead><tr><th scope=\"col\">Member</th><th scope=\"col\">Value</th></tr></thead><tbody>{rows}</tbody></table></div>{sections}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn number(value: Value) -> Number {
        match value {
            Value::Number(n) => n,
            _ => unreachable!(),
        }
    }

    /// #1140: an integral float shows without `.0`; a latitude or longitude
    /// member to at most 5 decimals, its exact value kept in the `title`.
    #[test]
    fn numbers_drop_integral_decimals_and_round_coordinates() {
        assert_eq!(number_text("", &number(json!(135.0))).0, "135");
        assert_eq!(number_text("", &number(json!(-0.0))).0, "0");
        assert_eq!(number_text("", &number(json!(42))).0, "42");
        assert_eq!(number_text("", &number(json!(2.5))).0, "2.5");
        assert_eq!(
            number_text("value", &number(json!(24.933333333333334))).0,
            "24.933333333333334"
        );
        for key in ["lat", "Longitude", "station_lat", "LON"] {
            assert_eq!(
                number_text(key, &number(json!(24.933333333333334))),
                ("24.93333".into(), true),
                "{key}"
            );
        }
        assert_eq!(
            number_text("lat", &number(json!(60.1))),
            ("60.1".into(), false)
        );
        assert_eq!(
            number_text("lon", &number(json!(25.0))),
            ("25".into(), false)
        );
        let html = number_html("latitude", &number(json!(60.123456789)));
        assert_eq!(
            html,
            "<span class=\"rounded\" title=\"60.123456789\">60.12346</span>"
        );
    }

    #[test]
    fn crs_identifiers_have_short_names() {
        assert_eq!(
            crs_name("http://www.opengis.net/def/crs/OGC/1.3/CRS84"),
            "CRS84"
        );
        assert_eq!(
            crs_name("http://www.opengis.net/def/crs/EPSG/0/3067"),
            "EPSG:3067"
        );
        assert_eq!(crs_name("[EPSG:4326]"), "EPSG:4326");
        assert_eq!(crs_name("CRS84"), "CRS84");
        let chip = crs_chip("http://www.opengis.net/def/crs/EPSG/0/4326");
        assert_eq!(
            chip,
            "<span class=\"chip crs\" title=\"http://www.opengis.net/def/crs/EPSG/0/4326\">EPSG:4326</span>"
        );
    }

    /// A query without unit parameters shows no "Not advertised" units; a
    /// table of such queries has no Units column at all.
    #[test]
    fn units_column_only_for_queries_that_take_units() {
        let query = |kind: &str, units: Option<Value>| {
            let mut variables = json!({"query_type": kind});
            if let Some(units) = units {
                variables["within_units"] = units;
            }
            json!({"link": {"href": format!("https://x/c/{kind}"), "rel": "data", "variables": variables}})
        };
        let table = |queries: Value| data_queries_table(queries.as_object().unwrap());
        let html = table(json!({"position": query("position", None), "area": query("area", None)}));
        assert!(!html.contains(">Units</th>"), "{html}");
        assert!(!html.contains("data-label=\"Units\""), "{html}");
        let html = table(json!({
            "position": query("position", None),
            "radius": query("radius", Some(json!(["km", "mi"])))}));
        assert!(html.contains(">Units</th>"), "{html}");
        let units = html
            .split("data-query=\"position\"")
            .nth(1)
            .and_then(|row| row.split("data-label=\"Units\">").nth(1))
            .and_then(|cell| cell.split("</td>").next())
            .unwrap();
        assert_eq!(
            units,
            "<span class=\"muted\" title=\"This query takes no unit\">—</span>"
        );
        assert!(html.contains("Radius: km, mi"), "{html}");
        // A radius query that lists no units is missing them.
        let html = table(json!({"radius": query("radius", None)}));
        assert!(html.contains(">Units</th>"), "{html}");
        assert!(
            html.contains(
                "<td data-label=\"Units\"><span class=\"muted\">Not advertised</span></td>"
            ),
            "{html}"
        );
    }

    /// A member with no layout of its own stays complete, as nested lists
    /// rather than pretty-printed JSON.
    #[test]
    fn structured_values_keep_every_member_without_pre() {
        let value = json!({"a": {"b": [1.0, "x<y"], "c": [{"d": null}]}, "url": "https://x/y?a=1&b=2", "lat": 60.123456789});
        let html = structured_html("", &value);
        assert!(!html.contains("<pre"), "{html}");
        for text in [
            "<code>a</code>",
            "<code>b</code>",
            "<code>d</code>",
            ">1<",
            "x&lt;y",
        ] {
            assert!(html.contains(text), "{text}: {html}");
        }
        assert!(html.contains("href=\"https://x/y?a=1&amp;b=2\""), "{html}");
        assert!(html.contains(">60.12346</span>"), "{html}");
        // JSON null is a value: an open-ended interval is not "missing".
        let extent =
            extent_html(&json!({"temporal": {"interval": [["2026-01-01T00:00:00Z", null]]}}));
        let all = extent.split("All extent fields").nth(1).unwrap();
        assert!(all.contains("<code>null</code>"), "{all}");
        assert!(!extent.contains("Not available"), "{extent}");
        let doc = metadata_html(&json!({"id": "a", "attribution": null}));
        assert!(doc.contains("<code>null</code>"), "{doc}");
    }
}
