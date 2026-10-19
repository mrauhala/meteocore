//! HTML feature representations. Serialize domain values once via the GeoJSON
//! builder so properties, geometry and links stay consistent between formats.
use ds_core::html::{escape, Wanted};
use serde_json::{json, Value};

pub(crate) fn path_segment(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn with_format(href: &str, format: &str) -> String {
    let separator = if href.contains('?') { '&' } else { '?' };
    format!("{href}{separator}f={format}")
}

/// `href` without its `crs` parameter: the HTML view is CRS84 only, so its
/// alternate link from a GeoJSON response in another CRS drops it. Other
/// parameters keep their exact encoding.
fn without_crs(href: &str) -> String {
    let Some((path, query)) = href.split_once('?') else {
        return href.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| pair.split('=').next() != Some("crs"))
        .collect();
    if kept.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

/// Explicit formats keep pagination and alternate links usable even when a
/// browser follows them with a different Accept header from the first request.
pub(crate) fn representation_links(doc: &mut Value, wanted: Wanted) {
    let (format, media, alternate, alternate_media) = match wanted {
        Wanted::Json => ("json", "application/geo+json", "html", "text/html"),
        Wanted::Html => ("html", "text/html", "json", "application/geo+json"),
    };
    if let Some(links) = doc.get_mut("links").and_then(Value::as_array_mut) {
        let mut alternate_link = None;
        for link in links.iter_mut() {
            let href = link["href"].as_str().unwrap_or_default().to_owned();
            if link["rel"] == "self" {
                let alternate_href = match wanted {
                    Wanted::Json => without_crs(&href),
                    Wanted::Html => href.clone(),
                };
                alternate_link = Some(json!({
                    "href": with_format(&alternate_href, alternate),
                    "rel": "alternate", "type": alternate_media
                }));
            }
            if matches!(link["rel"].as_str(), Some("self" | "next" | "prev")) {
                link["href"] = json!(with_format(&href, format));
                link["type"] = json!(media);
            } else if wanted == Wanted::Html
                && matches!(link["rel"].as_str(), Some("collection" | "related"))
            {
                link["href"] = json!(with_format(&href, "html"));
                link["type"] = json!("text/html");
            }
        }
        links.extend(alternate_link);
    }
    if let Some(features) = doc.get_mut("features").and_then(Value::as_array_mut) {
        for feature in features {
            representation_links(feature, wanted);
        }
    }
}

#[derive(Default)]
pub(crate) struct FeatureControls {
    pub filterables: Vec<String>,
    pub sortables: Vec<String>,
    /// The engine's first columns (`FeatureEngine::summary_properties`).
    pub summary: Vec<String>,
    /// Whether `/items` takes `datetime` (`FeatureEngine::has_time_dimension`).
    pub temporal: bool,
}

fn href_for<'a>(doc: &'a Value, rel: &str) -> &'a str {
    doc["links"]
        .as_array()
        .and_then(|links| links.iter().find(|link| link["rel"] == rel))
        .and_then(|link| link["href"].as_str())
        .unwrap_or_default()
}

const LABEL_KEYS: &[&str] = &[
    "name",
    "label",
    "title",
    "display_name",
    "displayname",
    "nimi",
    "namn",
    "nom",
    "nombre",
    "naam",
    "bezeichnung",
];

fn is_label_key(key: &str) -> bool {
    LABEL_KEYS
        .iter()
        .any(|label| key.eq_ignore_ascii_case(label))
}

/// Never a default column (#1143): the id again, the owning collection,
/// and the point's coordinates, which the geometry and map already show.
const NOT_DEFAULT_COLUMNS: &[&str] = &["id", "collection", "latitude", "longitude"];

/// Property columns a listing shows before the reader chooses.
const DEFAULT_COLUMNS: usize = 4;

fn feature_id(feature: &Value) -> String {
    match &feature["id"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => "Feature".into(),
    }
}

fn feature_title(feature: &Value) -> String {
    let properties = feature["properties"].as_object();
    LABEL_KEYS
        .iter()
        .find_map(|key| {
            properties?.iter().find_map(|(name, value)| {
                (name.eq_ignore_ascii_case(key))
                    .then(|| value.as_str())
                    .flatten()
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
            })
        })
        .unwrap_or_else(|| feature_id(feature))
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A property's value; numbers follow the workbench rule for the property
/// `key` (an integral float without `.0`, a latitude or longitude to 5
/// decimals, #1140).
fn property_value(key: &str, value: Option<&Value>) -> String {
    use api_common::workbench as ui;
    match value {
        None => "<span class=\"muted\">Absent</span>".into(),
        Some(Value::Null) => "<code>null</code>".into(),
        Some(Value::String(s)) if s.is_empty() => "<span class=\"muted\">Empty string</span>".into(),
        Some(Value::String(s)) if s.starts_with("https://") || s.starts_with("http://") => ui::anchor(s,s,""),
        Some(Value::Array(values)) if values.iter().all(|v| !v.is_array() && !v.is_object()) => {
            if values.is_empty() {
                "<span class=\"muted\">Empty array</span>".into()
            } else {
                format!("<span class=\"chip-row\">{}</span>",values.iter().map(|v|format!("<span class=\"chip\">{}</span>",property_value(key, Some(v)))).collect::<String>())
            }
        },
        Some(v @ (Value::Array(_) | Value::Object(_))) => format!("<details class=\"property-value\"><summary>{} · {} {}</summary><pre>{}</pre></details>",value_type(v),v.as_array().map_or_else(||v.as_object().map_or(0,|o|o.len()),|a|a.len()),if v.is_array(){"values"}else{"keys"},escape(&serde_json::to_string_pretty(v).expect("properties serialize"))),
        Some(v) => ui::property_value_html(key, v),
    }
}

/// Whether property `key` only repeats its feature's id wherever it is set,
/// as a radar site's `nod` or a station's identifier does.
fn repeats_id(features: &[Value], key: &str) -> bool {
    let mut set = false;
    for feature in features {
        let text = match feature["properties"].get(key) {
            None | Some(Value::Null) => continue,
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(_) => return false,
        };
        if text != feature_id(feature) {
            return false;
        }
        set = true;
    }
    set
}

/// Every property column a listing offers, and the ones it shows first
/// (#1143): the engine's summary properties, else what it sorts on, then
/// the rest alphabetically, leaving out labels, the id, the collection and
/// coordinates. A default column has a value on this page.
fn property_columns(features: &[Value], controls: &FeatureControls) -> (Vec<String>, Vec<String>) {
    let mut columns: std::collections::BTreeSet<String> =
        controls.filterables.iter().cloned().collect();
    for f in features {
        if let Some(p) = f["properties"].as_object() {
            columns.extend(p.keys().cloned());
        }
    }
    let shown = |key: &str| {
        features.iter().any(|f| {
            f["properties"]
                .get(key)
                .is_some_and(|v| !v.is_null() && !v.is_array() && !v.is_object())
        })
    };
    let mut defaults: Vec<String> = controls
        .summary
        .iter()
        .filter(|key| shown(key))
        .take(DEFAULT_COLUMNS)
        .cloned()
        .collect();
    if defaults.is_empty() {
        for key in controls.sortables.iter().chain(&columns) {
            if defaults.len() == DEFAULT_COLUMNS {
                break;
            }
            let skipped = is_label_key(key)
                || NOT_DEFAULT_COLUMNS
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name))
                || repeats_id(features, key);
            if !skipped && shown(key) && !defaults.contains(key) {
                defaults.push(key.clone());
            }
        }
    }
    (columns.into_iter().collect(), defaults)
}

fn item_paging(doc: &Value, count: usize) -> String {
    use api_common::workbench as ui;
    let href = href_for(doc, "self");
    let pairs: Vec<_> =
        form_urlencoded::parse(href.split_once('?').map_or("", |(_, q)| q).as_bytes()).collect();
    let number = |key: &str, default| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(default)
    };
    let limit = number("limit", 100);
    let offset = number("offset", 0);
    let matched = doc["numberMatched"].as_u64();
    let range = if count == 0 {
        "No items on this page".into()
    } else {
        format!(
            "{}–{} shown",
            offset.saturating_add(1),
            offset.saturating_add(count)
        )
    };
    let mut sizes = vec![25, 50, 100, 1000, limit];
    sizes.sort_unstable();
    sizes.dedup();
    let options = sizes
        .iter()
        .map(|n| {
            format!(
                "<option value=\"{n}\" {}>{n}</option>",
                if *n == limit { "selected" } else { "" }
            )
        })
        .collect::<String>();
    let links: Vec<_> = doc["links"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            ds_core::html::LinkView::new(
                l["href"].as_str().unwrap_or_default(),
                l["rel"].as_str().unwrap_or_default(),
                None,
            )
        })
        .collect();
    format!("<div class=\"item-paging\"><span>{range} · {} matched</span><label class=\"per-page enhanced\">Per page<select data-page-size>{options}</select></label>{}</div>",matched.map_or("Unknown".into(),|n|n.to_string()),ui::pagination(&links))
}

/// A feature's `related` links as buttons (#1143): where it leads next,
/// such as a radar site's own collection or a station's EDR location.
fn related_links(feature: &Value) -> String {
    use api_common::workbench as ui;
    let buttons: String = feature["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|link| link["rel"] == "related")
        .filter_map(|link| {
            let href = link["href"].as_str()?;
            let title = link["title"].as_str().unwrap_or("Related resource");
            Some(ui::anchor(href, &format!("{title} →"), "btn"))
        })
        .collect();
    if buttons.is_empty() {
        return String::new();
    }
    format!("<section class=\"panel panel-body\"><span class=\"eyebrow\">NEXT STEPS</span><div class=\"action-row related-links\">{buttons}</div></section>")
}

fn coordinate(value: &Value) -> String {
    value
        .as_f64()
        .map(|n| {
            format!("{n:.5}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        })
        .unwrap_or_else(|| "Not available".into())
}

/// The parameter an HTML items request was rejected for, when a single one
/// is, and the message of its 400 (#1132).
#[derive(Clone, Copy)]
pub(crate) struct Invalid<'a> {
    pub parameter: Option<&'a str>,
    pub message: &'a str,
}

/// `datetime`'s bounds as `datetime-local` values in UTC, for the
/// builder's pickers: empty for an open or absent bound, or for a value
/// that does not parse.
fn picker_bounds(datetime: &str) -> (String, String) {
    let Ok(interval) = crate::params::parse_datetime(datetime) else {
        return Default::default();
    };
    let local = |t: Option<chrono::DateTime<chrono::Utc>>| {
        t.map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default()
    };
    (local(interval.start), local(interval.end))
}

/// The builder's order (#1143): a sortable property and a direction, which
/// the script joins into `sortby`. A `sortby` they cannot show, several
/// keys or a rejected one, stays selected as it was given. Returns the
/// property field, which carries a rejection, and the rest.
fn sort_fields(current: &str, sortables: &[String]) -> (String, String) {
    let (descending, key) = match current.strip_prefix('-') {
        Some(key) => (true, key),
        None => (false, current.trim_start_matches(['+', ' '])),
    };
    let single = sortables.iter().any(|name| name == key);
    let mut options = String::from("<option value=\"\">Default order</option>");
    if !current.is_empty() && !single {
        options.push_str(&format!(
            "<option value=\"{0}\" data-raw selected>As requested: {0}</option>",
            escape(current)
        ));
    }
    for name in sortables {
        options.push_str(&format!(
            "<option value=\"{0}\"{1}>{0}</option>",
            escape(name),
            if single && name == key {
                " selected"
            } else {
                ""
            }
        ));
    }
    let property = format!("<label class=\"field\"><span>Sort by</span><span class=\"hint\" id=\"sortby-hint\">Properties this collection sorts on</span><select data-sort-property aria-describedby=\"sortby-hint\">{options}</select></label>");
    let rest = format!(
        "<label class=\"field\"><span>Direction</span><span class=\"hint\">Ascending or descending</span><select data-sort-direction><option value=\"\"{}>Ascending</option><option value=\"-\"{}>Descending</option></select></label><input type=\"hidden\" data-param=\"sortby\" data-sortby value=\"{}\">",
        if descending { "" } else { " selected" },
        if descending { " selected" } else { "" },
        escape(current)
    );
    (property, rest)
}

/// The data request builder (#1143): property predicates, time, area,
/// order and paging, each a guided control, and the submit button last.
fn query_form(doc: &Value, controls: &FeatureControls, invalid: Option<Invalid<'_>>) -> String {
    use api_common::workbench as ui;
    // The field the request was rejected for carries the message.
    let mut marked = false;
    let mut mark = |field: String, name: &str| match invalid {
        Some(Invalid {
            parameter: Some(parameter),
            message,
        }) if parameter == name => {
            marked = true;
            ui::mark_invalid(&field, name, message)
        }
        _ => field,
    };
    let href = href_for(doc, "self");
    let (action, query) = href.split_once('?').unwrap_or((href, ""));
    let pairs: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
    let value = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref())
            .unwrap_or("")
    };
    let mut out = format!("<form class=\"query-form item-query items-controls panel enhanced\" action=\"{}\" method=\"get\"><input type=\"hidden\" name=\"f\" value=\"html\"><p class=\"search-hint\">Filter features within this selected collection. Use Collections to find a different dataset.</p>",escape(action));
    let mut applied = String::new();
    for name in &controls.filterables {
        for (_, value) in pairs.iter().filter(|(k, _)| k == name) {
            let mut input = ui::input(name, value, "equals · applied predicate", "text", true);
            if value.is_empty() {
                input = input.replace("data-param=", "data-keep-empty=\"true\" data-param=");
            }
            let input = mark(input, name);
            applied.push_str(&format!("<div data-predicate>{input}<button class=\"btn small\" type=\"button\" data-clear-predicate>Clear {}</button></div>",escape(name)));
        }
    }
    if !controls.filterables.is_empty() {
        out.push_str("<fieldset class=\"builder-group\"><legend>Property equals</legend><div class=\"equal-fields\"><label>Property<select data-new-property><option value=\"\">Choose a property</option>");
        for name in &controls.filterables {
            out.push_str(&format!(
                "<option value=\"{}\">{}</option>",
                escape(name),
                escape(name)
            ));
        }
        out.push_str("</select></label><label>Equals<input data-new-value data-param=\"\" placeholder=\"Exact value\"></label></div>");
        if !applied.is_empty() {
            out.push_str(&format!("<div class=\"fields spaced\">{applied}</div><p class=\"field-help\">Applied predicates are combined with AND. Numeric fields also accept comma-separated alternatives.</p>"));
        }
        out.push_str("</fieldset>");
    }
    let datetime = value("datetime");
    if controls.temporal || !datetime.is_empty() {
        let (start, end) = picker_bounds(datetime);
        out.push_str(&format!("<fieldset class=\"builder-group\"><legend>Time · UTC</legend><div class=\"fields\"><label class=\"field\"><span>From</span><span class=\"hint\">Start, UTC</span><input type=\"datetime-local\" step=\"1\" data-datetime-start value=\"{}\"></label><label class=\"field\"><span>To</span><span class=\"hint\">End, UTC; empty for an open end (..)</span><input type=\"datetime-local\" step=\"1\" data-datetime-end value=\"{}\"></label>{}</div></fieldset>",escape(&start),escape(&end),mark(ui::datetime_input(datetime, true), "datetime")));
    }
    let bbox = mark(
        ui::input(
            "bbox",
            value("bbox"),
            "CRS84 · west,south,east,north",
            "text",
            true,
        ),
        "bbox",
    );
    out.push_str(&format!("<fieldset class=\"builder-group\"><legend>Area</legend><div class=\"builder-area\">{bbox}<button class=\"btn small\" type=\"button\" data-use-map-view hidden>Use map view</button><span class=\"field-help\" data-map-view-status role=\"status\"></span></div></fieldset>"));
    if !controls.sortables.is_empty() || !value("sortby").is_empty() {
        let (property, rest) = sort_fields(value("sortby"), &controls.sortables);
        out.push_str(&format!(
            "<fieldset class=\"builder-group\"><legend>Order</legend><div class=\"fields\">{}{rest}</div></fieldset>",
            mark(property, "sortby")
        ));
    }
    let open = match invalid {
        Some(Invalid {
            parameter: Some("limit" | "offset"),
            ..
        }) => " open",
        _ => "",
    };
    out.push_str(&format!("<details class=\"filters\" data-disclosure=\"item-filters\"{open}><summary>Paging</summary><div class=\"fields\">"));
    for (key, default) in [("limit", "100"), ("offset", "0")] {
        let field = ui::input(
            key,
            if value(key).is_empty() {
                default
            } else {
                value(key)
            },
            "integer",
            "number",
            false,
        );
        out.push_str(&mark(field, key));
    }
    out.push_str("</div></details>");
    // A rejection with no field of its own (an unknown parameter, a
    // repeated one) is named above the fields.
    if let Some(Invalid { message, .. }) = invalid.filter(|_| !marked) {
        let summary = format!(
            "<p class=\"error-text field-error\" role=\"alert\">{}</p>",
            escape(message)
        );
        if let Some(at) = out.find("<p class=\"search-hint\">") {
            out.insert_str(at, &summary);
        }
    }
    out.push_str(&format!("<div class=\"filter-controls builder-actions\"><button class=\"btn primary\">Request features</button>{}</div><details class=\"draft-request\" data-draft-disclosure><summary>Preview data request</summary><small>Draft · submit to update features</small><code data-draft></code></details></form><noscript><p class=\"callout\">Paging, item links and JSON work without JavaScript. Enable JavaScript to edit item-query parameters.</p></noscript>",ui::anchor(&ui::with_format(action,"html"),"Clear item filters","quiet")));
    out
}

/// Where an items listing's generation time goes. Rendered with a blank
/// `timeStamp`, a listing carries this slot empty, as api-edr's items page
/// does.
pub(crate) const TIMESTAMP_SLOT: &str = "<time></time>";

/// Fill the [`TIMESTAMP_SLOT`] of an items page in place. The handler hashes
/// the page with the slot empty for the ETag (#499) and then fills it, so
/// the page is rendered once per request (#1135). The slot is the page's
/// first: it sits in the results head, above every row and the map data, and
/// every data string on the page is escaped, so none can forge it.
pub(crate) fn stamp_generation_time(html: &mut String, time_stamp: &str) {
    let at = html.find(TIMESTAMP_SLOT);
    debug_assert!(at.is_some(), "an items listing has a timestamp slot");
    if let Some(at) = at {
        let filled = format!("<time>{}</time>", escape(time_stamp));
        html.replace_range(at..at + TIMESTAMP_SLOT.len(), &filled);
    }
}

/// `base` is the external base URL (shared assets); `root` the absolute URL
/// of the API root serving the collection (#789).
pub(crate) fn features_html(
    doc: &Value,
    title: &str,
    collection_id: &str,
    base: &str,
    root: &str,
    controls: &FeatureControls,
) -> String {
    features_page(doc, title, collection_id, base, root, controls, None)
}

/// The items page of an HTML request rejected for `invalid` (#1132): the
/// data request form again, holding what the request gave, with the field
/// at fault marked and no results. `doc` is an empty feature collection
/// whose `self` link is the rejected request.
pub(crate) fn rejected_items_html(
    doc: &Value,
    title: &str,
    collection_id: &str,
    base: &str,
    root: &str,
    controls: &FeatureControls,
    invalid: Invalid<'_>,
) -> String {
    features_page(
        doc,
        title,
        collection_id,
        base,
        root,
        controls,
        Some(invalid),
    )
}

fn features_page(
    doc: &Value,
    title: &str,
    collection_id: &str,
    base: &str,
    root: &str,
    controls: &FeatureControls,
    invalid: Option<Invalid<'_>>,
) -> String {
    use api_common::workbench as ui;
    let collection_url = format!("{root}/collections/{}", path_segment(collection_id));
    let items_url = format!("{collection_url}/items");
    let is_list = doc["features"].is_array();
    let page_title = if is_list {
        title.to_owned()
    } else {
        feature_title(doc)
    };
    let json_url = ui::with_format(href_for(doc, "self"), "json");
    let mut body = if is_list {
        String::new()
    } else {
        format!(
            "<a class=\"back-link\" data-back-scope=\"{}\" href=\"{}\">← Back to items</a>",
            escape(&items_url),
            escape(&ui::with_format(&items_url, "html"))
        )
    };
    if is_list {
        body.push_str(&format!("<div class=\"detail-heading\"><h1>{}</h1><div class=\"mono\">{}</div></div><nav class=\"detail-tabs\" aria-label=\"Collection views\">{}<a class=\"active\" aria-current=\"page\" href=\"{}\">Request data</a>{}</nav>",escape(title),escape(collection_id),ui::anchor(&ui::with_format(&collection_url,"html"),"Overview",""),escape(&ui::with_format(&items_url,"html")),ui::anchor(&format!("{}#metadata",ui::with_format(&collection_url,"html")),"Metadata & links","")));
    } else {
        let id = feature_id(doc);
        body.push_str(&format!("<div class=\"page-title\"><div><span class=\"eyebrow\">FEATURE DETAIL</span><h1>{}</h1>{}<span class=\"chip\">{}</span></div></div>",escape(&page_title),if page_title != id {format!("<p class=\"mono\">{}</p>",escape(&id))} else {String::new()},escape(doc["geometry"]["type"].as_str().unwrap_or("No geometry"))));
    }
    let features = if let Some(features) = doc["features"].as_array() {
        body.push_str(&format!(
            "<div data-results-scope=\"{}\"></div>",
            escape(&items_url)
        ));
        let applied: String = form_urlencoded::parse(
            href_for(doc, "self")
                .split_once('?')
                .map_or("", |(_, q)| q)
                .as_bytes(),
        )
        .filter(|(key, _)| !matches!(key.as_ref(), "f" | "limit" | "offset"))
        .map(|(key, value)| {
            format!(
                "<span class=\"chip\"><code>{}</code>: {}</span>",
                escape(&key),
                escape(&value)
            )
        })
        .collect();
        // Open on a request that filters or sorts, so what was applied is
        // in view (#1143), and on a rejected one.
        let open = invalid.is_some() || !applied.is_empty();
        body.push_str(&format!("<details class=\"item-builder enhanced\" id=\"item-builder\" data-disclosure=\"item-builder\"{}><summary>Build a data request</summary>{}</details>",if open {" open"} else {""},query_form(doc, controls, invalid)));
        if !applied.is_empty() {
            body.push_str(&format!("<div class=\"chip-row applied-item-filters\" aria-label=\"Applied item filters\">{applied}</div>"));
        }
        // A rejected request has no response to date or page (#1132), so
        // its page has no timestamp slot either.
        if invalid.is_none() {
            let generated = match doc["timeStamp"].as_str() {
                Some(time) if !time.is_empty() => format!("<time>{}</time>", escape(time)),
                _ => TIMESTAMP_SLOT.to_owned(),
            };
            body.push_str(&format!(
                "<div class=\"results-head\"><p>Response generated: {generated}</p></div>"
            ));
        }
        features.as_slice()
    } else {
        std::slice::from_ref(doc)
    };
    let (columns, defaults) = property_columns(features, controls);
    // A listing's map draws each shape simplified, within a page budget
    // (#1135); an item's map draws its whole geometry.
    let preview = is_list.then(|| crate::map_preview::preview_geometries(features));
    let map_features: Vec<_> = features.iter().enumerate().filter(|(_, f)|!f["geometry"].is_null()).map(|(i, f)| {
        let facts: serde_json::Map<String,Value> = defaults.iter().filter_map(|key| f["properties"].get(key).map(|v|(key.clone(),v.clone()))).collect();
        let geometry = preview.as_ref().map_or(&f["geometry"], |p| &p[i]);
        json!({"type":"Feature","id":f["id"],"geometry":geometry,"properties":{"label":feature_title(f),"href":href_for(f,"self"),"facts":facts}})
    }).collect();
    let mut head = String::new();
    body.push_str(if is_list {
        "<div class=\"item-layout\"><section>"
    } else {
        "<div class=\"detail-layout spaced\"><section class=\"panel\">"
    });
    if is_list {
        if invalid.is_none() {
            body.push_str(&item_paging(doc, features.len()));
        }
        if !features.is_empty() {
            body.push_str("<details class=\"column-picker enhanced\"><summary>Choose property columns</summary><p>Showing properties from this response and advertised filter fields. Choose up to eight; this browser remembers your choice for this collection.</p><div class=\"column-options\">");
            for key in &columns {
                body.push_str(&format!(
                    "<label><input type=\"checkbox\" data-item-column value=\"{}\" {}>{}</label>",
                    escape(key),
                    if defaults.contains(key) {
                        "checked"
                    } else {
                        ""
                    },
                    escape(key)
                ));
            }
            body.push_str("</div><p data-column-status role=\"status\"></p></details>");
            body.push_str("<div class=\"table-wrap item-result-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"Feature results; scroll horizontally for more columns\"><table class=\"item-table\"><thead><tr><th scope=\"col\">Feature</th><th scope=\"col\">Geometry</th>");
            for key in &defaults {
                body.push_str(&format!(
                    "<th scope=\"col\" data-property-cell>{}</th>",
                    escape(key)
                ));
            }
            body.push_str("</tr></thead><tbody>");
            for feature in features {
                let id = feature_id(feature);
                let label = feature_title(feature);
                body.push_str(&format!(
                    "<tr><td><a class=\"table-link\" href=\"{}\">{}{}</a></td><td>{}</td>",
                    escape(href_for(feature, "self")),
                    escape(&label),
                    if id != label {
                        format!("<small>{}</small>", escape(&id))
                    } else {
                        String::new()
                    },
                    escape(
                        feature["geometry"]["type"]
                            .as_str()
                            .unwrap_or("No geometry")
                    )
                ));
                for key in &defaults {
                    body.push_str(&format!(
                        "<td data-property-cell>{}</td>",
                        property_value(key, feature["properties"].get(key))
                    ));
                }
                body.push_str("</tr>");
            }
            body.push_str("</tbody></table></div>");
            let properties: Vec<_> = features.iter().map(|f| &f["properties"]).collect();
            // The column choice is stored by collection id (#1143), the same
            // on every API root serving the collection.
            body.push_str(&format!(
                "<div id=\"item-properties\" data-collection=\"{}\" hidden>{}</div>",
                escape(collection_id),
                escape(&serde_json::to_string(&properties).expect("properties serialize"))
            ));
            body.push_str(&item_paging(doc, features.len()));
        } else if let Some(invalid) = invalid {
            let field = invalid
                .parameter
                .map(|p| format!("<p>Check <code>{}</code> and request again.</p>", escape(p)))
                .unwrap_or_default();
            body.push_str(&format!("<div class=\"empty\" role=\"alert\"><h2>This request could not run.</h2><p>{}</p>{field}{}</div>",escape(invalid.message),ui::anchor(&ui::with_format(&items_url,"html"),"Clear item filters","btn primary")));
        } else if doc["numberMatched"].as_u64().is_some_and(|n| n > 0) {
            let first = ui::query_edit(
                &ui::with_format(href_for(doc, "self"), "html"),
                "offset",
                None,
            );
            body.push_str(&format!("<div class=\"empty\"><h2>This page is outside the results.</h2><p>Matching features exist. Return to the first page to keep these filters.</p>{}</div>",ui::anchor(&first,"Go to first page","btn primary")));
        } else {
            body.push_str("<div class=\"empty\"><h2>No features match this query.</h2><p>Change or reset the filters.</p></div>");
        }
    } else {
        body.push_str(&format!("<div class=\"panel-head\"><h2>Properties <span class=\"muted\">· {}</span></h2><input class=\"property-filter enhanced\" id=\"property-search\" aria-label=\"Find a property\" placeholder=\"Find a property…\"></div><div class=\"table-scroll\"><table class=\"properties property-table typed-properties\"><thead><tr><th scope=\"col\">Property</th><th scope=\"col\">Type</th><th scope=\"col\">Value</th></tr></thead><tbody>",doc["properties"].as_object().map_or(0,|p|p.len())));
        if let Some(props) = doc["properties"].as_object() {
            for (key, value) in props {
                body.push_str(&format!("<tr data-property=\"{}\"><th scope=\"row\"><code>{}</code></th><td class=\"value-type\">{}</td><td>{}</td></tr>",escape(key),escape(key),value_type(value),property_value(key, Some(value))));
            }
        }
        body.push_str("</tbody></table></div>");
    }
    body.push_str(&format!("</section><aside class=\"aside-stack\"><section class=\"panel\"><div class=\"panel-head\"><h2>{}</h2><span class=\"chip\">CRS84</span></div>",if is_list {"Map · current page"} else {"Location"}));
    if !map_features.is_empty() {
        head = ui::map_head(base);
        body.push_str(&ui::map_html(
            base,
            &json!({"type":"FeatureCollection","features":map_features}),
            is_list,
            if is_list {
                "The features of this page are listed in the table."
            } else {
                "The geometry is described below."
            },
        ));
    } else if is_list && features.is_empty() {
        body.push_str("<div class=\"empty\"><p>No features on this page.</p></div>");
    } else {
        body.push_str("<div class=\"empty\"><p>No geometry available.</p></div>");
    }
    if !is_list {
        body.push_str("<div class=\"panel-body\"><dl class=\"definition\">");
        body.push_str(&format!(
            "<dt>Geometry</dt><dd>{}</dd>",
            ui::value_html(&doc["geometry"]["type"])
        ));
        if doc["geometry"]["type"] == "Point" {
            body.push_str(&format!(
                "<dt>Longitude</dt><dd>{}°</dd><dt>Latitude</dt><dd>{}°</dd>",
                coordinate(&doc["geometry"]["coordinates"][0]),
                coordinate(&doc["geometry"]["coordinates"][1])
            ));
        }
        body.push_str("</dl></div>");
    }
    for feature in features.iter().filter(|_| !is_list) {
        body.push_str(&format!(
            "<details class=\"geometry-details\"><summary>Geometry ({}) · {}</summary><pre>{}</pre></details>",
            escape(feature["geometry"]["type"].as_str().unwrap_or("None")),
            escape(feature["id"].as_str().unwrap_or_default()),
            escape(
                &serde_json::to_string_pretty(&feature["geometry"]).expect("geometry serializes")
            )
        ));
    }
    body.push_str("</section>");
    if !is_list {
        body.push_str(&format!("<section class=\"panel panel-body\"><span class=\"eyebrow\">PART OF THIS COLLECTION</span><h3>{}</h3>{}</section>",escape(title),ui::anchor(&ui::with_format(&collection_url,"html"),"Collection overview →","quiet")));
        body.push_str(&related_links(doc));
    }
    body.push_str("</aside></div>");
    // Tabs and history tell pages apart (#1131): an items page by its
    // range and filtering, a feature by its label.
    let document_title = if is_list {
        let query = href_for(doc, "self").split_once('?').map_or("", |(_, q)| q);
        format!("{} · {title}", ui::items_title(query, features.len()))
    } else {
        format!("{page_title} · {title}")
    };
    ui::Page {
        surface: ui::Surface {
            base,
            root,
            api: "features",
        },
        title: &document_title,
        json_url: &json_url,
    }
    .render_with_breadcrumbs(
        &body,
        &head,
        &[
            (&collection_url, title),
            (if is_list { "" } else { &json_url }, &page_title),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{feature_page_to_geojson, feature_to_geojson, preserved_query};
    use ds_core::feature::{Feature, FeaturePage, Geometry, PropertyValue};

    #[test]
    fn flat_property_arrays_are_visible_chips_and_nested_values_remain_expandable() {
        let html = property_value("", Some(&json!(["DBZH", "<script>&", 0, false, null, ""])));
        assert_eq!(html.matches("class=\"chip\"").count(), 6);
        assert!(html.contains("DBZH"));
        assert!(html.contains("&lt;script&gt;&amp;"));
        assert!(html.contains(">0</span>"));
        assert!(html.contains(">false</span>"));
        assert!(html.contains("<code>null</code>"));
        assert!(html.contains("Empty string"));
        assert!(!html.contains("<details"));
        assert!(property_value("", Some(&json!([]))).contains("Empty array"));
        for nested in [
            json!([[1, 2]]),
            json!([{"name":"nested"}]),
            json!({"a":[1,2]}),
        ] {
            assert!(property_value("", Some(&nested)).starts_with("<details"));
        }
    }

    /// #1140: an item table prints `135`, as the map popup does, not
    /// `135.0`; a latitude or longitude property shows 5 decimals and keeps
    /// its exact value in the `title`.
    #[test]
    fn property_numbers_follow_the_workbench_display_rule() {
        assert_eq!(property_value("direction", Some(&json!(135.0))), "135");
        assert_eq!(property_value("direction", Some(&json!(2.25))), "2.25");
        assert_eq!(
            property_value("latitude", Some(&json!(60.17093333333333))),
            "<span class=\"rounded\" title=\"60.17093333333333\">60.17093</span>"
        );
        assert!(
            property_value("lon", Some(&json!([24.93333333333333]))).contains(">24.93333</span>")
        );
    }

    /// An engine labelling its features `event · areaDesc`, as engine-cap
    /// does.
    struct Labelled;

    impl ds_core::feature_engine::FeatureEngine for Labelled {
        fn get_features(
            &self,
            _: &ds_core::feature::FeatureQuery,
        ) -> Result<FeaturePage, ds_core::error::DataServerError> {
            unreachable!("only labels")
        }

        fn get_feature(&self, _: &str) -> Result<Feature, ds_core::error::DataServerError> {
            unreachable!("only labels")
        }

        fn label(&self, feature: &Feature) -> Option<String> {
            let text = |key: &str| feature.properties.get(key)?.as_str();
            Some(format!("{} · {}", text("event")?, text("areaDesc")?))
        }
    }

    fn alert(properties: &[(&str, &str)]) -> Feature {
        Feature {
            id: "2.49.0.0.246.0.FI.260617/1 0".into(),
            geometry: Geometry::Null.into(),
            properties: properties
                .iter()
                .map(|(k, v)| ((*k).to_owned(), PropertyValue::String((*v).to_owned())))
                .collect::<std::collections::HashMap<_, _>>()
                .into(),
            related: Vec::new(),
        }
    }

    /// The renderer reads no weather field: labels are the generic keys,
    /// else the id. An engine's own label arrives as the `title` property
    /// (#1143) and names the row, the page and the map, the id beside it.
    #[test]
    fn labels_are_generic_and_never_interpret_weather_fields() {
        assert_eq!(
            feature_title(
                &json!({"id":"a","properties":{"name":"  ","TITLE":"Town","nimi":"Kunta"}})
            ),
            "Town"
        );
        assert_eq!(
            feature_title(&json!({"id":"a","properties":{"nimi":"Kunta"}})),
            "Kunta"
        );
        assert_eq!(
            feature_title(&json!({"id":42,"properties":{"event":"Wind","impact_over":"Town"}})),
            "42"
        );
        assert_eq!(coordinate(&json!(51.88055000000001)), "51.88055");

        let labelled = crate::response::Annotations {
            engine: Some(&Labelled),
            ..Default::default()
        };
        let wind = alert(&[("event", "Wind"), ("areaDesc", "Coast")]);
        let encode = |feature: &Feature, annotations| {
            let mut doc = feature_to_geojson(
                feature,
                "cap",
                "/features",
                &crate::crs::ResponseCrs::default(),
                annotations,
            );
            representation_links(&mut doc, Wanted::Html);
            doc
        };
        let doc = encode(&wind, &labelled);
        assert_eq!(doc["properties"]["title"], "Wind · Coast");
        assert_eq!(feature_title(&doc), "Wind · Coast");
        let page = features_html(
            &doc,
            "Alerts",
            "cap",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(
            page.contains(
                "<h1>Wind · Coast</h1><p class=\"mono\">2.49.0.0.246.0.FI.260617/1 0</p>"
            ),
            "{page}"
        );
        let list = json!({"features": [doc], "numberMatched": 1,
            "links": [{"rel": "self", "href": "/features/collections/cap/items?f=html"}]});
        let page = features_html(
            &list,
            "Alerts",
            "cap",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(
            page.contains(">Wind · Coast<small>2.49.0.0.246.0.FI.260617/1 0</small></a>"),
            "{page}"
        );
        // A title of the feature's own stays; no engine, no title.
        let titled = alert(&[("event", "Wind"), ("areaDesc", "Coast"), ("title", "Own")]);
        assert_eq!(encode(&titled, &labelled)["properties"]["title"], "Own");
        assert!(encode(&wind, &Default::default())["properties"]
            .get("title")
            .is_none());
    }

    /// #1131: an items page's tab title names its item range, whether it
    /// is filtered, and the collection; a feature's names its label.
    #[test]
    fn items_and_feature_pages_have_distinct_titles() {
        let feature = |id: &str| {
            json!({"id": id, "geometry": null, "properties": {"name": format!("Town {id}")},
            "links": [{"rel": "self", "href": format!("/features/collections/a/items/{id}?f=html")}]})
        };
        let page = |query: &str| {
            let doc = json!({"features": [feature("a"), feature("b")], "numberMatched": 300,
                "links": [{"rel": "self", "href": format!("/features/collections/a/items?{query}")}]});
            features_html(
                &doc,
                "Towns",
                "a",
                "",
                "/features",
                &FeatureControls::default(),
            )
        };
        assert!(page("limit=2&f=html").contains("<title>Items 1–2 · Towns · MeteoCore</title>"));
        assert!(page("limit=2&offset=100&f=html")
            .contains("<title>Items 101–102 · Towns · MeteoCore</title>"));
        assert!(page("name=x&limit=2&offset=2&f=html")
            .contains("<title>Items 3–4 · filtered · Towns · MeteoCore</title>"));
        // Ordering, columns and CRS filter nothing.
        assert!(
            page("sortby=-name&properties=name&crs=EPSG%3A4326&bbox-crs=CRS84&f=html")
                .contains("<title>Items 1–2 · Towns · MeteoCore</title>")
        );
        assert!(page("bbox=0,0,1,1&bbox-crs=CRS84&f=html")
            .contains("<title>Items 1–2 · filtered · Towns · MeteoCore</title>"));
        let detail = features_html(
            &feature("a"),
            "Towns",
            "a",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(
            detail.contains("<title>Town a · Towns · MeteoCore</title>"),
            "{detail}"
        );
    }

    #[test]
    fn generic_listing_keeps_mixed_properties_and_all_response_rows() {
        let doc = json!({"features":[
            {"id":"a","geometry":{"type":"Point","coordinates":[20,60]},"properties":{"label":"First","max_dbz":42,"nested":{"a":1},"null_value":null},"links":[{"rel":"self","href":"/items/a?f=html"}]},
            {"id":"b","geometry":null,"properties":{"nimi":"Second","last_report":"2026-09-18T09:00:00Z","evil</div>":"</script>"},"links":[{"rel":"self","href":"/items/b?f=html"}]}
        ],"numberMatched":1000,"numberReturned":2,"links":[{"rel":"self","href":"/features/collections/a/items?limit=2"},{"rel":"next","href":"/features/collections/a/items?limit=2&offset=2&f=html"}]});
        let html = features_html(
            &doc,
            "Collection",
            "a",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(html.contains("data-item-column value=\"last_report\""));
        assert!(html.contains("data-item-column value=\"nested\""));
        assert!(html.contains("evil&lt;/div&gt;"));
        assert!(!html.contains("evil</div>"));
        assert!(html.contains("item-result-scroll"));
        assert_eq!(html.matches("<select data-page-size>").count(), 2);
        assert!(!html.contains("<details class=\"geometry-details\""));
        assert!(!html.contains("Observed / sent"));
        assert!(!html.contains("Reflectivity"));
        assert!(!html.contains(" dBZ"));
        assert!(html.contains("Map · current page"));
        let detail = features_html(
            &doc["features"][0],
            "Collection",
            "a",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(detail.contains("Geometry (Point)"));
        assert!(detail.contains("<code>null</code>"));
        assert!(detail.contains("<td class=\"value-type\">object</td>"));
    }

    #[test]
    fn empty_offset_recovers_without_dropping_repeated_or_empty_predicates() {
        let doc = json!({"features":[],"numberMatched":308,"numberReturned":0,"links":[{"rel":"self","href":"/features/collections/a/items?limit=5&offset=1000&name=a%26b&name=&sortby=-name&f=html"}]});
        let html = features_html(
            &doc,
            "Collection",
            "a",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(html.contains("This page is outside the results."));
        assert!(!html.contains("No features match this query."));
        assert!(html.contains("href=\"/features/collections/a/items?limit=5&amp;name=a%26b&amp;name=&amp;sortby=-name&amp;f=html\">Go to first page"));
        let mut empty = doc.clone();
        empty["numberMatched"] = json!(0);
        assert!(features_html(
            &empty,
            "Collection",
            "a",
            "",
            "/features",
            &FeatureControls::default()
        )
        .contains("No features match this query."));
    }

    #[test]
    fn representation_links_do_not_add_foreign_null_members() {
        let mut doc = json!({"type":"Feature","id":"a","geometry":null,"properties":{},"links":[]});
        representation_links(&mut doc, Wanted::Json);
        assert!(doc.get("features").is_none());
        assert!(doc["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn query_builder_preserves_duplicate_and_empty_predicates_and_capabilities() {
        let doc = json!({"links":[{"rel":"self","href":"https://example.test/prefix/features/collections/a/items?name=a%26b&name=&offset=20&sortby=-score&f=html"}]});
        let controls = FeatureControls {
            filterables: vec!["name".into()],
            sortables: vec!["score".into()],
            temporal: true,
            ..Default::default()
        };
        let html = query_form(&doc, &controls, None);
        assert_eq!(html.matches("data-param=\"name\"").count(), 2);
        assert!(html.contains("value=\"a&amp;b\""));
        assert!(html.contains("data-keep-empty=\"true\""));
        assert!(html.contains("data-clear-predicate"));
        assert!(html.contains("data-param=\"sortby\" data-sortby value=\"-score\""));
        assert!(html.contains("data-param=\"datetime\""));
        let plain = query_form(
            &json!({"links":[{"rel":"self","href":"/features/collections/a/items"}]}),
            &FeatureControls::default(),
            None,
        );
        assert!(!plain.contains("data-param=\"sortby\""));
        assert!(!plain.contains("data-param=\"datetime\""));
        assert!(!plain.contains("data-param=\"name\""));
    }

    /// #1143: the builder guides each control. Sort is a property and a
    /// direction, time two pickers beside the `datetime` they fill, the box
    /// can take the map's view, and the submit button comes last.
    #[test]
    fn query_builder_guides_sort_time_and_area_and_submits_last() {
        let controls = FeatureControls {
            filterables: vec!["name".into()],
            sortables: vec!["score".into(), "name".into()],
            temporal: true,
            ..Default::default()
        };
        let form = |query: &str| {
            query_form(
                &json!({"links":[{"rel":"self","href":format!("/features/collections/a/items?{query}")}]}),
                &controls,
                None,
            )
        };
        let html = form("sortby=-score&datetime=2026-10-16T06:30:00Z/..&f=html");
        assert!(html.contains("<select data-sort-property aria-describedby=\"sortby-hint\"><option value=\"\">Default order</option><option value=\"score\" selected>score</option><option value=\"name\">name</option></select>"), "{html}");
        assert!(html.contains("<option value=\"-\" selected>Descending</option>"));
        assert!(html.contains(
            "<input type=\"datetime-local\" step=\"1\" data-datetime-start value=\"2026-10-16T06:30:00\">"
        ));
        assert!(html.contains("data-datetime-end value=\"\""));
        assert!(html.contains("empty for an open end (..)"));
        assert!(html.contains("e.g. 2026-10-17T00:00:00Z/.."));
        assert!(html.contains("data-use-map-view"));
        // Nothing comes after the submit button but the clear link and the
        // draft preview.
        let submit = html.find("Request features</button>").unwrap();
        for field in [
            "data-sort-property",
            "data-param=\"bbox\"",
            "name=\"limit\"",
            "data-new-property",
        ] {
            assert!(html.find(field).unwrap() < submit, "{field}");
        }
        // Several keys stay as they were given.
        let html = form("sortby=name,-score&f=html");
        assert!(html.contains(
            "<option value=\"name,-score\" data-raw selected>As requested: name,-score</option>"
        ));
        assert!(html.contains("<option value=\"name\">name</option>"));
        // An instant fills both pickers; text the pickers cannot show
        // leaves them empty.
        let html = form("datetime=2026-10-16T06:30:00Z&f=html");
        assert!(html.contains("data-datetime-end value=\"2026-10-16T06:30:00\""));
        let html = form("datetime=yesterday&f=html");
        assert!(html.contains("data-datetime-start value=\"\""));
        assert!(html.contains("value=\"yesterday\""));
    }

    /// The datetime control follows whether `/items` takes `datetime`
    /// (`has_time_dimension`), not whether the collection advertises a
    /// span (#1143).
    #[test]
    fn datetime_control_follows_the_time_dimension() {
        let doc = json!({"links":[{"rel":"self","href":"/features/collections/a/items?f=html"}]});
        let with = |temporal| {
            query_form(
                &doc,
                &FeatureControls {
                    temporal,
                    ..Default::default()
                },
                None,
            )
        };
        assert!(with(true).contains("data-datetime-start"));
        assert!(with(true).contains("data-param=\"datetime\""));
        assert!(!with(false).contains("data-datetime-start"));
        assert!(!with(false).contains("data-param=\"datetime\""));
    }

    /// A rejected request's form marks the field at fault and opens the
    /// filters holding it; a parameter with no field is named above them
    /// (#1132).
    #[test]
    fn rejected_requests_mark_the_field_at_fault() {
        let doc = json!({"links":[{"rel":"self","href":"/features/collections/a/items?sortby=nope&datetime=x&limit=x&f=html"}]});
        let controls = FeatureControls {
            sortables: vec!["score".into()],
            temporal: true,
            ..Default::default()
        };
        let invalid = |parameter, message| {
            Some(Invalid {
                parameter: Some(parameter),
                message,
            })
        };
        let html = query_form(
            &doc,
            &controls,
            invalid("sortby", "property 'nope' <is> not sortable"),
        );
        assert!(html.contains("<select data-sort-property aria-invalid=\"true\" aria-errormessage=\"sortby-error\" aria-describedby=\"sortby-error sortby-hint\">"), "{html}");
        assert!(html.contains(
            "class=\"error-text field-error\">property &#39;nope&#39; &lt;is&gt; not sortable</span></label>"
        ));
        assert!(
            html.contains("<option value=\"nope\" data-raw selected>As requested: nope</option>")
        );
        assert_eq!(html.matches("aria-invalid").count(), 1);
        assert!(!html.contains("data-disclosure=\"item-filters\" open"));
        // The datetime field shows the forms it takes.
        assert!(html
            .contains("e.g. 2026-10-17T00:00:00Z/.. or 2026-10-16T00:00:00Z/2026-10-17T00:00:00Z"));
        let html = query_form(&doc, &controls, invalid("datetime", "bad"));
        assert!(html.contains("aria-errormessage=\"datetime-error\""));
        let html = query_form(&doc, &controls, invalid("limit", "bad"));
        assert!(
            html.contains("data-disclosure=\"item-filters\" open"),
            "{html}"
        );
        let html = query_form(
            &doc,
            &controls,
            invalid("typo", "unsupported parameter 'typo'"),
        );
        assert!(!html.contains("aria-invalid"));
        assert!(html.contains("<p class=\"error-text field-error\" role=\"alert\">unsupported parameter &#39;typo&#39;</p><p class=\"search-hint\">"), "{html}");
    }

    #[test]
    fn untrusted_properties_ids_and_geometry_stay_inert_and_complete() {
        let attack = "</script><script>alert(1)</script>&\"'";
        let feature = Feature {
            id: format!("a/b ?#é{attack}"),
            geometry: Geometry::Polygon {
                exterior: vec![[0., 0.], [2., 0.], [2., 2.], [0., 0.]],
                holes: vec![vec![[0.5, 0.5], [1., 0.5], [1., 1.], [0.5, 0.5]]],
            }
            .into(),
            properties: [(
                attack.into(),
                PropertyValue::List(vec![
                    PropertyValue::String(attack.into()),
                    PropertyValue::Bool(true),
                    PropertyValue::Integer(42),
                    PropertyValue::Null,
                ]),
            )]
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>()
            .into(),
            related: Vec::new(),
        };
        let mut doc = feature_to_geojson(
            &feature,
            "test",
            "https://example.com/prefix/features",
            &crate::crs::ResponseCrs::default(),
            &Default::default(),
        );
        representation_links(&mut doc, Wanted::Html);
        let html = features_html(
            &doc,
            attack,
            "test",
            "https://example.com/prefix",
            "https://example.com/prefix/features",
            &FeatureControls::default(),
        );
        assert!(!html.contains(attack));
        assert!(html.contains(&escape(attack)));
        assert!(html.contains("a%2Fb%20%3F%23%C3%A9"));
        assert!(html.contains("Geometry (Polygon)"));
        assert!(html.contains("true"));
        assert!(html.contains("42"));
        assert!(html.contains("null"));
        assert!(html.contains("https://example.com/prefix/preview/vendor/maplibre-gl.js"));
        let embedded = html
            .split("<div id=\"map-data\" hidden>")
            .nth(1)
            .unwrap()
            .split("</div>")
            .next()
            .unwrap();
        assert!(!embedded.contains('<'));
        assert!(embedded.contains("0.5")); // holes survive into the map snapshot
    }

    #[test]
    fn null_geometry_is_readable_without_loading_a_map() {
        let mut doc = json!({"type": "Feature", "id": "empty", "geometry": null,
            "properties": {}, "links": [{"rel": "self", "href": "/items/empty"}]});
        representation_links(&mut doc, Wanted::Html);
        let html = features_html(
            &doc,
            "No geometry",
            "test",
            "",
            "/features",
            &FeatureControls::default(),
        );
        assert!(html.contains("No geometry available."));
        assert!(!html.contains("maplibre-gl.js"));
    }

    #[test]
    fn html_links_preserve_filters_sort_precision_and_format() {
        let filters = preserved_query(
            Some(&crate::params::parse_bbox("20,50,30,70").unwrap()),
            Some(&crate::params::parse_datetime("2026-01-01T00:00:00.500Z").unwrap()),
            &crate::params::parse_sortby("-score", &["score"]).unwrap(),
            &[
                ("name".into(), "a&b +é".into()),
                ("name".into(), "x".into()),
            ],
        );
        let page = FeaturePage {
            features: vec![],
            number_returned: 0,
            number_matched: 5,
            next_offset: Some(2),
        };
        let mut doc = feature_page_to_geojson(
            &page,
            "test",
            1,
            1,
            &filters,
            "",
            "",
            &crate::crs::ResponseCrs::default(),
            &Default::default(),
        );
        representation_links(&mut doc, Wanted::Html);
        for link in doc["links"].as_array().unwrap() {
            let href = link["href"].as_str().unwrap();
            let pairs: Vec<_> =
                form_urlencoded::parse(href.split_once('?').unwrap().1.as_bytes()).collect();
            assert!(pairs
                .iter()
                .any(|(k, v)| k == "datetime" && v == "2026-01-01T00:00:00.500Z"));
            assert!(pairs.iter().any(|(k, v)| k == "sortby" && v == "-score"));
            assert!(pairs.iter().any(|(k, v)| k == "name" && v == "a&b +é"));
            assert_eq!(pairs.iter().filter(|(k, _)| k == "name").count(), 2);
            let format = if link["rel"] == "alternate" {
                "json"
            } else {
                "html"
            };
            assert!(pairs.iter().any(|(k, v)| k == "f" && v == format));
        }
    }

    /// #1143: default columns are the engine's choice, else its sortables,
    /// else the rest alphabetically; never the id again, the collection or
    /// coordinates, and only properties this page has values for.
    #[test]
    fn default_columns_follow_engine_intent_then_sortables() {
        let features = [
            json!({"id": "fivih", "properties": {"nod": "fivih", "id": 7, "collection": "r-fivih",
                "latitude": 60.5, "longitude": 24.4, "name": "Vihti", "wmo": "02975",
                "volume_count": 12, "antenna_height_m": 181.0, "quantities": ["DBZH"], "empty": null}}),
            json!({"id": "fikor", "properties": {"nod": "fikor", "id": 8, "collection": "r-fikor",
                "latitude": 60.1, "longitude": 21.6, "name": "Korpo", "wmo": "02933",
                "volume_count": 10, "antenna_height_m": 61.0, "quantities": ["DBZH"], "empty": null}}),
        ];
        let defaults = |controls: FeatureControls| property_columns(&features, &controls).1;
        assert_eq!(
            defaults(FeatureControls::default()),
            ["antenna_height_m", "volume_count", "wmo"]
        );
        assert_eq!(
            defaults(FeatureControls {
                sortables: vec!["volume_count".into(), "name".into()],
                ..Default::default()
            }),
            ["volume_count", "antenna_height_m", "wmo"]
        );
        assert_eq!(
            defaults(FeatureControls {
                sortables: vec!["volume_count".into()],
                summary: vec![
                    "wmo".into(),
                    "absent".into(),
                    "empty".into(),
                    "collection".into()
                ],
                ..Default::default()
            }),
            ["wmo", "collection"]
        );
        // Every property stays choosable.
        let (columns, _) = property_columns(&features, &FeatureControls::default());
        assert!(columns.iter().any(|c| c == "nod"));
        assert!(columns.iter().any(|c| c == "latitude"));
    }

    /// The column choice is stored by collection id, and the page says it
    /// is kept in this browser (#1143).
    #[test]
    fn column_choice_is_kept_per_collection() {
        let doc = json!({"features": [{"id": "a", "geometry": null, "properties": {"x": 1}, "links": []}],
            "numberMatched": 1, "links": [{"rel": "self", "href": "/collections/radar/items?f=html"}]});
        let html = features_html(&doc, "Radar", "radar", "", "", &FeatureControls::default());
        assert!(html.contains("<div id=\"item-properties\" data-collection=\"radar\" hidden>"));
        assert!(html.contains("this browser remembers your choice for this collection"));
    }

    /// #1143: a request that filters or sorts opens the builder with what
    /// it applied; a plain one leaves it closed.
    #[test]
    fn builder_opens_when_the_request_filters() {
        let page = |query: &str| {
            let doc = json!({"features": [], "numberMatched": 0,
                "links": [{"rel": "self", "href": format!("/collections/a/items?{query}")}]});
            features_html(&doc, "A", "a", "", "", &FeatureControls::default())
        };
        let open = "id=\"item-builder\" data-disclosure=\"item-builder\" open>";
        assert!(page("limit=10&offset=20&f=html")
            .contains("id=\"item-builder\" data-disclosure=\"item-builder\">"));
        assert!(page("bbox=1,2,3,4&f=html").contains(open));
        assert!(page("sortby=-score&f=html").contains(open));
        assert!(page("name=x&f=html").contains(open));
    }

    /// #1143: past the end, `prev` leads to the last page with features,
    /// and the map says the page has none.
    #[test]
    fn a_page_past_the_end_leads_back_to_the_last_page() {
        let page = FeaturePage {
            features: vec![],
            number_returned: 0,
            number_matched: 23,
            next_offset: None,
        };
        let mut doc = feature_page_to_geojson(
            &page,
            "a",
            10,
            90,
            "&name=x",
            "",
            "/features",
            &crate::crs::ResponseCrs::default(),
            &Default::default(),
        );
        representation_links(&mut doc, Wanted::Html);
        let html = features_html(&doc, "A", "a", "", "/features", &FeatureControls::default());
        assert!(
            html.contains("href=\"/features/collections/a/items?offset=20&amp;limit=10&amp;name=x&amp;f=html\""),
            "{html}"
        );
        assert!(html.contains("No features on this page."));
        assert!(!html.contains("No geometry available."));
    }

    /// #1143: a feature's related links are buttons in the aside, in HTML
    /// leading to the HTML view.
    #[test]
    fn related_links_are_next_steps() {
        let pages = std::collections::HashMap::from([(
            "radar-fivih".to_owned(),
            "/collections/radar-fivih".to_owned(),
        )]);
        let site = Feature {
            id: "fivih".into(),
            geometry: Geometry::Point { x: 24.4, y: 60.5 }.into(),
            properties: std::collections::HashMap::new().into(),
            related: vec![
                ds_core::feature::Related {
                    target: ds_core::feature::RelatedTarget::Collection("radar-fivih".into()),
                    title: "Radar site collection".into(),
                },
                ds_core::feature::Related {
                    target: ds_core::feature::RelatedTarget::Collection("unknown".into()),
                    title: "Nowhere".into(),
                },
                ds_core::feature::Related::edr_location("fivih"),
            ],
        };
        let mut doc = feature_to_geojson(
            &site,
            "radar",
            "https://x.test/features",
            &crate::crs::ResponseCrs::default(),
            &crate::response::Annotations {
                base: "https://x.test",
                collection_pages: Some(&pages),
                edr: true,
                ..Default::default()
            },
        );
        representation_links(&mut doc, Wanted::Html);
        let html = features_html(
            &doc,
            "Radar",
            "radar",
            "",
            "https://x.test/features",
            &FeatureControls::default(),
        );
        assert!(html.contains("<span class=\"eyebrow\">NEXT STEPS</span><div class=\"action-row related-links\"><a class=\"btn\" href=\"https://x.test/collections/radar-fivih?f=html\">Radar site collection →</a><a class=\"btn\" href=\"https://x.test/edr/collections/radar/locations/fivih?f=html\">Station data (EDR location) →</a></div>"), "{html}");
        assert!(!html.contains("Nowhere"));
    }
}
