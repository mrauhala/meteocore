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
                alternate_link = Some(json!({
                    "href": with_format(&href, alternate),
                    "rel": "alternate", "type": alternate_media
                }));
            }
            if matches!(link["rel"].as_str(), Some("self" | "next" | "prev")) {
                link["href"] = json!(with_format(&href, format));
                link["type"] = json!(media);
            } else if wanted == Wanted::Html && link["rel"] == "collection" {
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

fn property_value(value: Option<&Value>) -> String {
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
                format!("<span class=\"chip-row\">{}</span>",values.iter().map(|v|format!("<span class=\"chip\">{}</span>",property_value(Some(v)))).collect::<String>())
            }
        },
        Some(v @ (Value::Array(_) | Value::Object(_))) => format!("<details class=\"property-value\"><summary>{} · {} {}</summary><pre>{}</pre></details>",value_type(v),v.as_array().map_or_else(||v.as_object().map_or(0,|o|o.len()),|a|a.len()),if v.is_array(){"values"}else{"keys"},escape(&serde_json::to_string_pretty(v).expect("properties serialize"))),
        Some(v) => ui::value_html(v),
    }
}

fn property_columns(features: &[Value], controls: &FeatureControls) -> (Vec<String>, Vec<String>) {
    let mut columns: std::collections::BTreeSet<String> =
        controls.filterables.iter().cloned().collect();
    for f in features {
        if let Some(p) = f["properties"].as_object() {
            columns.extend(p.keys().cloned());
        }
    }
    let defaults = columns
        .iter()
        .filter(|key| {
            !LABEL_KEYS
                .iter()
                .any(|label| key.eq_ignore_ascii_case(label))
        })
        .filter(|key| {
            features.iter().any(|f| {
                f["properties"]
                    .get(*key)
                    .is_some_and(|v| !v.is_null() && !v.is_array() && !v.is_object())
            })
        })
        .take(4)
        .cloned()
        .collect();
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

fn query_form(doc: &Value, controls: &FeatureControls) -> String {
    use api_common::workbench as ui;
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
    let mut out=format!("<form class=\"query-form item-query items-controls panel enhanced\" action=\"{}\" method=\"get\"><input type=\"hidden\" name=\"f\" value=\"html\"><div class=\"item-primary-fields\">",escape(action));
    if !controls.filterables.is_empty() {
        out.push_str("<div class=\"equal-fields\"><label>Property<select data-new-property><option value=\"\">Choose a property</option>");
        for name in &controls.filterables {
            out.push_str(&format!(
                "<option value=\"{}\">{}</option>",
                escape(name),
                escape(name)
            ));
        }
        out.push_str("</select></label><label>Equals<input data-new-value data-param=\"\" placeholder=\"Exact value\"></label></div>");
    }
    if controls.temporal || !value("datetime").is_empty() {
        out.push_str(&ui::input(
            "datetime",
            value("datetime"),
            "UTC instant or interval",
            "text",
            true,
        ));
    }
    out.push_str("<button class=\"btn primary\">Request features</button></div><p class=\"search-hint\">Filter features within this selected collection. Use Collections to find a different dataset.</p>");
    let mut applied = String::new();
    for name in &controls.filterables {
        for (_, value) in pairs.iter().filter(|(k, _)| k == name) {
            let mut input = ui::input(name, value, "equals · applied predicate", "text", true);
            if value.is_empty() {
                input = input.replace("data-param=", "data-keep-empty=\"true\" data-param=");
            }
            applied.push_str(&format!("<div data-predicate>{input}<button class=\"btn small\" type=\"button\" data-clear-predicate>Clear {}</button></div>",escape(name)));
        }
    }
    if !applied.is_empty() {
        out.push_str(&format!("<div class=\"fields spaced\">{applied}</div><p class=\"field-help\">Applied predicates are combined with AND. Numeric fields also accept comma-separated alternatives.</p>"));
    }
    out.push_str("<details class=\"filters\" data-disclosure=\"item-filters\"><summary>Area, ordering &amp; paging</summary><div class=\"fields\">");
    out.push_str(&ui::input(
        "bbox",
        value("bbox"),
        "CRS84 · west,south,east,north",
        "text",
        true,
    ));
    for (key, default) in [("limit", "100"), ("offset", "0")] {
        out.push_str(&ui::input(
            key,
            if value(key).is_empty() {
                default
            } else {
                value(key)
            },
            "integer",
            "number",
            false,
        ));
    }
    if !controls.sortables.is_empty() {
        out.push_str(&ui::input(
            "sortby",
            value("sortby"),
            &format!(
                "Comma-separated; - for descending. Available: {}",
                controls.sortables.join(", ")
            ),
            "text",
            true,
        ));
    }
    out.push_str(&format!("</div></details><div class=\"filter-controls\">{}</div><details class=\"draft-request\" data-draft-disclosure><summary>Preview data request</summary><small>Draft · submit to update features</small><code data-draft></code></details></form><noscript><p class=\"callout\">Paging, item links and JSON work without JavaScript. Enable JavaScript to edit item-query parameters.</p></noscript>",ui::anchor(&ui::with_format(action,"html"),"Clear item filters","quiet")));
    out
}

pub(crate) fn features_html(
    doc: &Value,
    title: &str,
    collection_id: &str,
    base: &str,
    controls: &FeatureControls,
) -> String {
    use api_common::workbench as ui;
    let collection_url = format!(
        "{base}/features/collections/{}",
        path_segment(collection_id)
    );
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
        body.push_str(&format!("<details class=\"item-builder enhanced\" data-disclosure=\"item-builder\"><summary>Build a data request</summary>{}</details>",query_form(doc, controls)));
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
        if !applied.is_empty() {
            body.push_str(&format!("<div class=\"chip-row applied-item-filters\" aria-label=\"Applied item filters\">{applied}</div>"));
        }
        body.push_str(&format!(
            "<div class=\"results-head\"><p>Response generated: <time>{}</time></p></div>",
            escape(doc["timeStamp"].as_str().unwrap_or_default())
        ));
        features.as_slice()
    } else {
        std::slice::from_ref(doc)
    };
    let (columns, defaults) = property_columns(features, controls);
    let map_features: Vec<_> = features.iter().filter(|f|!f["geometry"].is_null()).map(|f| {
        let facts: serde_json::Map<String,Value> = defaults.iter().filter_map(|key| f["properties"].get(key).map(|v|(key.clone(),v.clone()))).collect();
        json!({"type":"Feature","id":f["id"],"geometry":f["geometry"],"properties":{"label":feature_title(f),"href":href_for(f,"self"),"facts":facts}})
    }).collect();
    let mut head = String::new();
    body.push_str(if is_list {
        "<div class=\"item-layout\"><section>"
    } else {
        "<div class=\"detail-layout spaced\"><section class=\"panel\">"
    });
    if is_list {
        body.push_str(&item_paging(doc, features.len()));
        if !features.is_empty() {
            body.push_str("<details class=\"column-picker enhanced\"><summary>Choose property columns</summary><p>Showing properties from this response and advertised filter fields. Choose up to eight; your choice is remembered for this collection.</p><div class=\"column-options\">");
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
                        property_value(feature["properties"].get(key))
                    ));
                }
                body.push_str("</tr>");
            }
            body.push_str("</tbody></table></div>");
            let properties: Vec<_> = features.iter().map(|f| &f["properties"]).collect();
            body.push_str(&format!(
                "<div id=\"item-properties\" hidden>{}</div>",
                escape(&serde_json::to_string(&properties).expect("properties serialize"))
            ));
            body.push_str(&item_paging(doc, features.len()));
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
                body.push_str(&format!("<tr data-property=\"{}\"><th scope=\"row\"><code>{}</code></th><td class=\"value-type\">{}</td><td>{}</td></tr>",escape(key),escape(key),value_type(value),property_value(Some(value))));
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
        ));
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
    }
    body.push_str("</aside></div>");
    ui::Page {
        base,
        api: "features",
        title: &page_title,
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
        let html = property_value(Some(&json!(["DBZH", "<script>&", 0, false, null, ""])));
        assert_eq!(html.matches("class=\"chip\"").count(), 6);
        assert!(html.contains("DBZH"));
        assert!(html.contains("&lt;script&gt;&amp;"));
        assert!(html.contains(">0</span>"));
        assert!(html.contains(">false</span>"));
        assert!(html.contains("<code>null</code>"));
        assert!(html.contains("Empty string"));
        assert!(!html.contains("<details"));
        assert!(property_value(Some(&json!([]))).contains("Empty array"));
        for nested in [
            json!([[1, 2]]),
            json!([{"name":"nested"}]),
            json!({"a":[1,2]}),
        ] {
            assert!(property_value(Some(&nested)).starts_with("<details"));
        }
    }

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
    }

    #[test]
    fn generic_listing_keeps_mixed_properties_and_all_response_rows() {
        let doc = json!({"features":[
            {"id":"a","geometry":{"type":"Point","coordinates":[20,60]},"properties":{"label":"First","max_dbz":42,"nested":{"a":1},"null_value":null},"links":[{"rel":"self","href":"/items/a?f=html"}]},
            {"id":"b","geometry":null,"properties":{"nimi":"Second","last_report":"2026-09-18T09:00:00Z","evil</div>":"</script>"},"links":[{"rel":"self","href":"/items/b?f=html"}]}
        ],"numberMatched":1000,"numberReturned":2,"links":[{"rel":"self","href":"/features/collections/a/items?limit=2"},{"rel":"next","href":"/features/collections/a/items?limit=2&offset=2&f=html"}]});
        let html = features_html(&doc, "Collection", "a", "", &FeatureControls::default());
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
            &FeatureControls::default(),
        );
        assert!(detail.contains("Geometry (Point)"));
        assert!(detail.contains("<code>null</code>"));
        assert!(detail.contains("<td class=\"value-type\">object</td>"));
    }

    #[test]
    fn empty_offset_recovers_without_dropping_repeated_or_empty_predicates() {
        let doc = json!({"features":[],"numberMatched":308,"numberReturned":0,"links":[{"rel":"self","href":"/features/collections/a/items?limit=5&offset=1000&name=a%26b&name=&sortby=-name&f=html"}]});
        let html = features_html(&doc, "Collection", "a", "", &FeatureControls::default());
        assert!(html.contains("This page is outside the results."));
        assert!(!html.contains("No features match this query."));
        assert!(html.contains("href=\"/features/collections/a/items?limit=5&amp;name=a%26b&amp;name=&amp;sortby=-name&amp;f=html\">Go to first page"));
        let mut empty = doc.clone();
        empty["numberMatched"] = json!(0);
        assert!(
            features_html(&empty, "Collection", "a", "", &FeatureControls::default())
                .contains("No features match this query.")
        );
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
        };
        let html = query_form(&doc, &controls);
        assert_eq!(html.matches("data-param=\"name\"").count(), 2);
        assert!(html.contains("value=\"a&amp;b\""));
        assert!(html.contains("data-keep-empty=\"true\""));
        assert!(html.contains("data-clear-predicate"));
        assert!(html.contains("data-param=\"sortby\""));
        assert!(html.contains("value=\"-score\""));
        assert!(html.contains("data-param=\"datetime\""));
        let plain = query_form(
            &json!({"links":[{"rel":"self","href":"/features/collections/a/items"}]}),
            &FeatureControls::default(),
        );
        assert!(!plain.contains("data-param=\"sortby\""));
        assert!(!plain.contains("data-param=\"datetime\""));
        assert!(!plain.contains("data-param=\"name\""));
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
        };
        let mut doc = feature_to_geojson(&feature, "test", "https://example.com/prefix");
        representation_links(&mut doc, Wanted::Html);
        let html = features_html(
            &doc,
            attack,
            "test",
            "https://example.com/prefix",
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
        let html = features_html(&doc, "No geometry", "test", "", &FeatureControls::default());
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
        let mut doc = feature_page_to_geojson(&page, "test", 1, 1, &filters, "", "");
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
}
