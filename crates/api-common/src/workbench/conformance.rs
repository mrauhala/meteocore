//! The conformance page's body (#1140): the declared classes grouped by
//! standard and version, each class by name with its URI as an anchor, and
//! a class declared in both its `http` and `https` form on one row.

use ds_core::html::escape;

use super::plain_anchor;

/// The standard, version and class of an OGC conformance class URI
/// (`http(s)://www.opengis.net/spec/{standard}/{version}/conf/{class}`).
fn parse(uri: &str) -> Option<(&str, &str, &str)> {
    let rest = uri
        .strip_prefix("http://www.opengis.net/spec/")
        .or_else(|| uri.strip_prefix("https://www.opengis.net/spec/"))?;
    let (standard, rest) = rest.split_once('/')?;
    let (version, class) = rest.split_once("/conf/")?;
    (!standard.is_empty() && !version.is_empty() && !class.is_empty())
        .then_some((standard, version, class))
}

/// A standard's title, else its identifier.
fn standard_title(standard: &str) -> &str {
    match standard {
        "ogcapi-common-1" => "OGC API - Common - Part 1: Core",
        "ogcapi-common-2" => "OGC API - Common - Part 2: Geospatial Data",
        "ogcapi-edr-1" => "OGC API - Environmental Data Retrieval",
        "ogcapi-features-1" => "OGC API - Features - Part 1: Core",
        "ogcapi-features-2" => {
            "OGC API - Features - Part 2: Coordinate Reference Systems by Reference"
        }
        "ogcapi-features-3" => "OGC API - Features - Part 3: Filtering",
        "ogcapi-features-4" => "OGC API - Features - Part 4: Create, Replace, Update and Delete",
        "ogcapi-features-5" => "OGC API - Features - Part 5: Schemas",
        "ogcapi-maps-1" => "OGC API - Maps - Part 1: Core",
        "ogcapi-tiles-1" => "OGC API - Tiles - Part 1: Core",
        "tms" => "Two Dimensional Tile Matrix Set and Tile Set Metadata",
        "cql2" => "Common Query Language (CQL2)",
        other => other,
    }
}

/// A class's name in words: a known abbreviation spelled as written, any
/// other identifier with its hyphens as spaces.
fn class_name(class: &str) -> String {
    let known = match class {
        "oas30" => Some("OpenAPI 3.0"),
        "json" => Some("JSON"),
        "html" => Some("HTML"),
        "geojson" => Some("GeoJSON"),
        "edr-geojson" => Some("EDR GeoJSON"),
        "covjson" => Some("CoverageJSON"),
        "png" => Some("PNG"),
        "jpeg" => Some("JPEG"),
        "tiff" => Some("TIFF"),
        "mvt" => Some("Mapbox Vector Tiles"),
        "crs" => Some("CRS"),
        "datetime" => Some("Date and time"),
        "tilematrixset" => Some("Tile matrix set"),
        "json-tilematrixset" => Some("Tile matrix set JSON"),
        "cql2-text" => Some("CQL2 text"),
        "cql2-json" => Some("CQL2 JSON"),
        _ => None,
    };
    if let Some(name) = known {
        return name.into();
    }
    let words = class.replace(['-', '_', '/'], " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// One standard and version: its rows of (class, URIs).
type Group<'a> = ((&'a str, &'a str), Vec<(&'a str, Vec<&'a str>)>);

/// `classes` grouped by standard and version under headings, in the order
/// declared, each class one row with every form of its URI as an anchor.
/// A URI of no known form is listed under "Other classes".
pub(super) fn conformance_body(classes: &[&str]) -> String {
    let mut groups: Vec<Group> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for uri in classes {
        let Some((standard, version, class)) = parse(uri) else {
            if !other.contains(uri) {
                other.push(uri);
            }
            continue;
        };
        let rows = match groups
            .iter_mut()
            .find(|(key, _)| *key == (standard, version))
        {
            Some((_, rows)) => rows,
            None => {
                groups.push(((standard, version), Vec::new()));
                &mut groups.last_mut().expect("just pushed").1
            }
        };
        match rows.iter_mut().find(|(c, _)| *c == class) {
            Some((_, uris)) if uris.contains(uri) => {}
            Some((_, uris)) => uris.push(uri),
            None => rows.push((class, vec![uri])),
        }
    }
    let mut out = String::new();
    let mut section = |heading: String, term: String, rows: String| {
        out.push_str(&format!(
            "<section class=\"panel spaced conformance-group\"><div class=\"panel-head\"><h2>{heading}{term}</h2></div><div class=\"table-scroll\"><table class=\"data-table conformance-table\"><thead><tr><th scope=\"col\">Class</th><th scope=\"col\">URI</th></tr></thead><tbody>{rows}</tbody></table></div></section>"
        ));
    };
    for ((standard, version), rows) in &groups {
        let rows: String = rows
            .iter()
            .map(|(class, uris)| {
                format!(
                    "<tr><th scope=\"row\">{}<small class=\"term\"><code>{}</code></small></th><td>{}</td></tr>",
                    escape(&class_name(class)),
                    escape(class),
                    uris.iter()
                        .map(|uri| format!("<div>{}</div>", plain_anchor(uri, uri)))
                        .collect::<String>()
                )
            })
            .collect();
        section(
            format!("{} · {}", escape(standard_title(standard)), escape(version)),
            format!(
                "<small class=\"term\"><code>{}</code></small>",
                escape(standard)
            ),
            rows,
        );
    }
    if !other.is_empty() {
        let rows: String = other
            .iter()
            .map(|uri| {
                format!(
                    "<tr><th scope=\"row\"><code>{}</code></th><td>{}</td></tr>",
                    escape(uri),
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        plain_anchor(uri, uri)
                    } else {
                        escape(uri)
                    }
                )
            })
            .collect();
        section("Other classes".into(), String::new(), rows);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1140: classes are grouped by standard and version, named, linked,
    /// and a class declared as http and https is one row.
    #[test]
    fn classes_are_grouped_named_linked_and_folded() {
        let classes = [
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
            "http://www.opengis.net/spec/ogcapi-common-2/1.0/conf/collections",
            "https://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core",
            "https://www.opengis.net/spec/ogcapi-edr-1/1.2/conf/covjson",
            "http://www.opengis.net/spec/ogcapi-edr-1/1.1/conf/covjson",
            "http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/landing-page",
            "urn:example:<b>",
        ];
        let html = conformance_body(&classes);
        assert_eq!(html.matches("<section").count(), 5, "{html}");
        let common = html
            .split("OGC API - Common - Part 1: Core · 1.0")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap();
        // The http and https forms of core share a row, each an anchor.
        assert_eq!(
            common.matches("<tr><th scope=\"row\">").count(),
            2,
            "{common}"
        );
        assert!(common.contains(
            "<tr><th scope=\"row\">Core<small class=\"term\"><code>core</code></small></th><td><div><a href=\"http://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core\">"
        ), "{common}");
        assert!(
            common.contains("href=\"https://www.opengis.net/spec/ogcapi-common-1/1.0/conf/core\"")
        );
        assert!(common.contains(">Landing page<"));
        // Versions of one standard are separate groups.
        assert!(html.contains("OGC API - Environmental Data Retrieval · 1.2"));
        assert!(html.contains("OGC API - Environmental Data Retrieval · 1.1"));
        assert!(html.contains(">CoverageJSON<"));
        // Anything else is still listed, escaped.
        assert!(html.contains("<h2>Other classes</h2>"));
        assert!(html.contains("<code>urn:example:&lt;b&gt;</code>"));
        assert!(!html.contains("<b>"));
    }
}
