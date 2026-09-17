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
    if let Some(links) = doc["links"].as_array_mut() {
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
    if let Some(features) = doc["features"].as_array_mut() {
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

fn feature_title(feature: &Value) -> String {
    let p = &feature["properties"];
    ["name", "event", "impact_over"]
        .iter()
        .find_map(|key| p[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned))
        .unwrap_or_else(|| feature["id"].as_str().unwrap_or("Feature").to_owned())
}

fn feature_flags(feature: &Value) -> String {
    let p = &feature["properties"];
    let mut out = String::new();
    if let Some(severity) = p["severity"].as_str() {
        out.push_str(&format!(
            "<span class=\"badge\">{}</span>",
            escape(severity)
        ));
    }
    if p["likely_clutter"] == true {
        out.push_str("<span class=\"badge warning\">Likely clutter</span>");
    }
    // Browser enhancement labels expired alerts; server output and ETags stay deterministic.
    if let Some(expires) = p["expires"].as_str() {
        out.push_str(&format!(
            "<span class=\"badge\" data-expiry=\"{}\">Expires {}</span>",
            escape(expires),
            escape(expires)
        ));
    }
    out
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
    let mut out = format!("<form class=\"query-form item-query panel enhanced\" action=\"{}\" method=\"get\"><h2>Query parameters</h2><input type=\"hidden\" name=\"f\" value=\"html\"><div class=\"fields\">",escape(action));
    out.push_str(&ui::input(
        "bbox",
        value("bbox"),
        "CRS84 · west,south,east,north",
        "text",
        true,
    ));
    if controls.temporal || !value("datetime").is_empty() {
        out.push_str(&ui::input(
            "datetime",
            value("datetime"),
            "UTC instant or interval",
            "text",
            true,
        ));
    }
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
                "Comma-separated; prefix - for descending. Available: {}",
                controls.sortables.join(", ")
            ),
            "text",
            true,
        ));
    }
    out.push_str("</div>");
    if !controls.filterables.is_empty() {
        let active = controls
            .filterables
            .iter()
            .any(|key| pairs.iter().any(|(k, _)| k == key));
        out.push_str(&format!("<details {}><summary>Property equality filters · {}</summary><p class=\"hint\">Exact values; all predicates are combined with AND. Numeric fields also accept comma-separated alternatives.</p><div class=\"fields\">",if active{"open"}else{""},controls.filterables.len()));
        for name in &controls.filterables {
            let values: Vec<_> = pairs
                .iter()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| v.as_ref())
                .collect();
            if values.is_empty() {
                out.push_str(&ui::input(name, "", "equals · optional", "text", true));
            } else {
                for value in values {
                    let mut input =
                        ui::input(name, value, "equals · applied predicate", "text", true);
                    if value.is_empty() {
                        input =
                            input.replace("data-param=", "data-keep-empty=\"true\" data-param=");
                    }
                    out.push_str(&format!("<div data-predicate>{input}<button class=\"btn\" type=\"button\" data-clear-predicate>Clear {}</button></div>",escape(name)));
                }
            }
        }
        out.push_str("</div></details>");
    }
    out.push_str(&format!("<button class=\"btn primary\">Apply query</button>{}<div class=\"draft\"><p class=\"hint\">Request preview · apply to update results</p><code data-draft></code></div></form><noscript><p class=\"notice\">Paging, item links and JSON work without JavaScript. Enable JavaScript to edit item-query parameters.</p></noscript>",ui::anchor(&ui::with_format(action,"html"),"Reset query","btn")));
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
    let mut body = format!(
        "<a class=\"back-link\" data-back-scope=\"{}\" href=\"{}\">← {}</a>",
        escape(if is_list { &collection_url } else { &items_url }),
        escape(&ui::with_format(
            if is_list { &collection_url } else { &items_url },
            "html"
        )),
        if is_list {
            "Collection metadata"
        } else {
            "Back to items"
        }
    );
    body.push_str(&ui::page_heading(
        &page_title,
        if is_list {
            "Inspect the features returned by this request. The map shows this page only."
        } else {
            "Feature properties and geometry from the same resource as GeoJSON."
        },
    ));
    let features = if let Some(features) = doc["features"].as_array() {
        body.push_str(&format!(
            "<div data-results-scope=\"{}\"></div>",
            escape(&items_url)
        ));
        body.push_str(&query_form(doc, controls));
        body.push_str(&format!("<div class=\"results-head\"><h2>Features returned: {} · Matched: {}</h2><p>Response time: <time>{}</time></p></div>",ui::value_html(&doc["numberReturned"]),ui::value_html(&doc["numberMatched"]),escape(doc["timeStamp"].as_str().unwrap_or_default())));
        features.as_slice()
    } else {
        std::slice::from_ref(doc)
    };
    let map_features: Vec<_> = features.iter().filter(|f|!f["geometry"].is_null()).map(|f|json!({"type":"Feature","id":f["id"],"geometry":f["geometry"],"properties":{"label":feature_title(f),"href":href_for(f,"self")}})).collect();
    let mut head = String::new();
    body.push_str("<div class=\"item-layout\"><section class=\"panel\">");
    if is_list {
        body.push_str("<div class=\"table-scroll\"><table class=\"item-table\"><thead><tr><th scope=\"col\">Feature / ID</th><th scope=\"col\">Geometry</th><th scope=\"col\">Context</th></tr></thead><tbody>");
        for feature in features {
            body.push_str(&format!("<tr><td><a class=\"table-link\" href=\"{}\">{}<small>{}</small></a></td><td>{}</td><td><div class=\"tags\">{}</div></td></tr>",escape(href_for(feature,"self")),escape(&feature_title(feature)),escape(feature["id"].as_str().unwrap_or_default()),escape(feature["geometry"]["type"].as_str().unwrap_or("No geometry")),feature_flags(feature)));
        }
        body.push_str("</tbody></table></div>");
        if features.is_empty() {
            body.push_str("<div class=\"empty\"><h2>No features match this query.</h2><p>Change or reset the filters.</p></div>");
        }
        let nav: Vec<_> = doc["links"]
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
        body.push_str(&ui::pagination(&nav));
    } else {
        body.push_str(&format!("<div class=\"feature-summary\"><code>{}</code><div class=\"tags\">{}</div></div><label class=\"property-search enhanced\" for=\"property-search\">Find a property<input id=\"property-search\" placeholder=\"Property name\"></label>",escape(doc["id"].as_str().unwrap_or_default()),feature_flags(doc)));
        body.push_str(&ui::property_table(&doc["properties"]));
    }
    body.push_str("</section><aside class=\"panel\"><h2>Geometry</h2>");
    if !map_features.is_empty() {
        head.push_str(&format!(
            "<link rel=\"stylesheet\" href=\"{}/preview/vendor/maplibre-gl.css\">",
            escape(base)
        ));
        body.push_str(&format!("<div class=\"map-panel\"><div id=\"feature-map\" role=\"region\" aria-label=\"Feature geometry map\"></div><p id=\"map-status\">Map requires JavaScript and WebGL; geometry is also listed below.</p><div id=\"map-data\" hidden>{}</div></div><script src=\"{}/preview/vendor/maplibre-gl.js\"></script><script>{}</script>",escape(&json!({"type":"FeatureCollection","features":map_features}).to_string()),escape(base),include_str!("feature-map.js")));
    } else {
        body.push_str("<div class=\"empty\"><p>No geometry available.</p></div>");
    }
    for feature in features {
        body.push_str(&format!(
            "<details><summary>Geometry ({}) · {}</summary><pre>{}</pre></details>",
            escape(feature["geometry"]["type"].as_str().unwrap_or("None")),
            escape(feature["id"].as_str().unwrap_or_default()),
            escape(
                &serde_json::to_string_pretty(&feature["geometry"]).expect("geometry serializes")
            )
        ));
    }
    body.push_str("</aside></div>");
    ui::Page {
        base,
        api: "features",
        title: &page_title,
        json_url: &json_url,
    }
    .render(&body, &head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{feature_page_to_geojson, feature_to_geojson, preserved_query};
    use ds_core::feature::{Feature, FeaturePage, Geometry, PropertyValue};

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
        assert!(html.contains(">true</span>"));
        assert!(html.contains(">42</span>"));
        assert!(html.contains("Not available"));
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
