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

fn links_html(doc: &Value) -> String {
    let mut body = String::from("<nav aria-label=\"Resource links\"><ul>\n");
    if let Some(links) = doc["links"].as_array() {
        for link in links {
            let rel = link["rel"].as_str().unwrap_or_default();
            let label = match rel {
                "self" => "This resource",
                "next" => "Next page",
                "prev" => "Previous page",
                "collection" => "Collection",
                "alternate" => "GeoJSON",
                _ => rel,
            };
            body.push_str(&format!(
                "<li><a rel=\"{}\" href=\"{}\">{}</a></li>\n",
                escape(rel),
                escape(link["href"].as_str().unwrap_or_default()),
                escape(label),
            ));
        }
    }
    body.push_str("</ul></nav>\n");
    body
}

fn feature_html(feature: &Value) -> String {
    let id = feature["id"].as_str().unwrap_or_default();
    let href = feature["links"][0]["href"].as_str().unwrap_or_default();
    let mut body = format!(
        "<article><h2>Feature <a href=\"{}\">{}</a></h2>\n\
         <table><caption>Properties</caption><thead><tr><th scope=\"col\">Property</th>\
         <th scope=\"col\">Value</th></tr></thead><tbody>\n",
        escape(href),
        escape(id),
    );
    if let Some(properties) = feature["properties"].as_object() {
        for (name, value) in properties {
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            body.push_str(&format!(
                "<tr><th scope=\"row\">{}</th><td>{}</td></tr>\n",
                escape(name),
                escape(&text),
            ));
        }
    }
    body.push_str("</tbody></table>\n");
    if feature["geometry"].is_null() {
        body.push_str("<p>No geometry available.</p>\n");
    } else {
        body.push_str(&format!(
            "<details><summary>Geometry ({})</summary><pre>{}</pre></details>\n",
            escape(feature["geometry"]["type"].as_str().unwrap_or_default()),
            escape(
                &serde_json::to_string_pretty(&feature["geometry"]).expect("geometry serializes")
            ),
        ));
    }
    body.push_str(&links_html(feature));
    body.push_str("</article>\n");
    body
}

pub(crate) fn features_html(doc: &Value, title: &str, collection_id: &str, base: &str) -> String {
    let mut body = format!(
        "<h1>{}</h1>\n<p><a rel=\"collection\" href=\"{}/features/collections/{}?f=html\">Collection</a> \
         · <a href=\"{}/features/collections/{}/items?f=html\">Browse features</a></p>\n",
        escape(title), escape(base), escape(&path_segment(collection_id)),
        escape(base), escape(&path_segment(collection_id)),
    );
    let features = if let Some(features) = doc["features"].as_array() {
        body.push_str(&format!(
            "<p>Features returned: {} · Matched: {}</p>\n<p>Response time: <time>{}</time></p>\n",
            doc["numberReturned"],
            doc["numberMatched"],
            escape(doc["timeStamp"].as_str().unwrap_or_default()),
        ));
        body.push_str(&links_html(doc));
        if features.is_empty() {
            body.push_str("<p>No features match this query.</p>\n");
        }
        features.as_slice()
    } else {
        std::slice::from_ref(doc)
    };
    // Embed only geometry + IDs: tables already contain all properties. Escaped
    // JSON in an inert HTML element avoids script-closing injection entirely.
    let map_features: Vec<_> = features.iter().filter(|f| !f["geometry"].is_null())
        .map(|f| json!({"type": "Feature", "id": f["id"], "geometry": f["geometry"], "properties": {}}))
        .collect();
    let mut head = String::from("<style>table{border-collapse:collapse;width:100%;margin-bottom:1rem}th,td{text-align:left;vertical-align:top;border-bottom:1px solid #ddd;padding:.4rem;overflow-wrap:anywhere}pre{white-space:pre-wrap;overflow-wrap:anywhere}article{margin:2rem 0}</style>\n");
    if !map_features.is_empty() {
        head.push_str(&format!(
            "<link rel=\"stylesheet\" href=\"{}/preview/vendor/maplibre-gl.css\">\n",
            escape(base)
        ));
        body.push_str(&format!(
            "<div id=\"feature-map\" role=\"region\" aria-label=\"Feature geometry map\" style=\"height:20rem\"></div>\n\
             <p id=\"map-status\">Map requires JavaScript and WebGL; geometry is also listed below.</p>\n\
             <div id=\"map-data\" hidden>{}</div>\n\
             <script src=\"{}/preview/vendor/maplibre-gl.js\"></script>\n<script>{}</script>\n",
            escape(&json!({"type": "FeatureCollection", "features": map_features}).to_string()),
            escape(base), include_str!("feature-map.js"),
        ));
    }
    for feature in features {
        body.push_str(&feature_html(feature));
    }
    ds_core::html::page_with_head(title, &head, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{feature_page_to_geojson, feature_to_geojson, preserved_query};
    use ds_core::feature::{Feature, FeaturePage, Geometry, PropertyValue};

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
        let html = features_html(&doc, attack, "test", "https://example.com/prefix");
        assert!(!html.contains(attack));
        assert!(html.contains(&escape(attack)));
        assert!(html.contains("a%2Fb%20%3F%23%C3%A9"));
        assert!(html.contains("Geometry (Polygon)"));
        assert!(html.contains("true,42,null"));
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
        let html = features_html(&doc, "No geometry", "test", "");
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
