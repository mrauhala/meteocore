//! Shared, server-rendered HTML representations of API resources.
//!
//! JSON remains the resource contract. Adapters supply the same metadata used
//! for JSON; this module supplies only presentation and progressive enhancement.
use ds_core::collection_search::{CollectionParameter, SearchParams, SearchQueryParams};
use ds_core::config::LicenseConfig;
use ds_core::html::{escape, LinkView};
use serde_json::{json, Value};

use crate::{mounts, rel};

const CSS: &str = include_str!("workbench/style.css");
const SCRIPT: &str = include_str!("workbench/app.js");
const THEME: &str = include_str!("workbench/theme.js");
/// API workspaces offered by the switcher: (kind, label, mount path). The
/// shared OGC API root (#789) is mounted at the server root.
const APIS: &[(&str, &str, &str)] = &[
    (crate::shared::WORKSPACE, "OGC API", ""),
    ("edr", "EDR", mounts::EDR),
    ("features", "Features", mounts::FEATURES),
    ("maps", "Maps", mounts::MAPS),
    ("tiles", "Tiles", mounts::TILES),
];

/// Where a page's API lives. `base` is the server's external base URL (for
/// server-wide assets and the API switcher); `root` is the absolute root of the
/// API the page belongs to (base URL + mount, `base` itself for the shared
/// OGC API root); `api` is the API kind shown in navigation.
#[derive(Clone, Copy, Debug)]
pub struct Surface<'a> {
    pub base: &'a str,
    pub root: &'a str,
    pub api: &'a str,
}

/// Registered relations naming a list of tilesets (Tiles Req 13).
const TILESETS_REL_PREFIX: &str = "http://www.opengis.net/def/rel/ogc/1.0/tilesets-";

/// First link carrying the short or registered map relation (Maps Req 46).
fn map_link(links: &[Value]) -> Option<&str> {
    links
        .iter()
        .find(|l| l["rel"] == "map" || l["rel"] == rel::MAP)
        .and_then(|l| l["href"].as_str())
}

pub fn path_segment(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// Replace only f, retaining duplicate property predicates and encoded operators.
pub fn with_format(href: &str, format: &str) -> String {
    let (href, fragment) = href.split_once('#').map_or((href, ""), |(a, b)| (a, b));
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let mut out = form_urlencoded::Serializer::new(String::new());
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key != "f" {
            out.append_pair(&key, &value);
        }
    }
    out.append_pair("f", format);
    format!(
        "{path}?{}{}",
        out.finish(),
        if fragment.is_empty() {
            String::new()
        } else {
            format!("#{fragment}")
        }
    )
}

fn safe_href(href: &str) -> &str {
    if href.starts_with("https://")
        || href.starts_with("http://")
        || (href.starts_with('/') && !href.starts_with("//"))
        || href.starts_with('#')
    {
        href
    } else {
        "#"
    }
}

pub fn anchor(href: &str, label: &str, class: &str) -> String {
    format!(
        "<a class=\"{}\" href=\"{}\">{}</a>",
        escape(class),
        escape(safe_href(href)),
        escape(label)
    )
}

pub fn icon(name: &str) -> String {
    let path = match name {
        "home" => "m3 10 9-7 9 7v11h-6v-7H9v7H3Z",
        "code" => "m8 5-6 7 6 7 m8-14 6 7-6 7 M14 3l-4 18",
        "book" => "M12 5C8 2 4 3 2 4v16c3-1 7-1 10 1 3-2 7-2 10-1V4c-2-1-6-2-10 1Z M12 5v16",
        "search" => "M21 21l-5-5 M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0",
        "arrow" => "M5 12h14 m-5-5 5 5-5 5",
        "radar" => "M12 2a10 10 0 1 0 10 10 M12 6a6 6 0 1 0 6 6 M12 12 22 2",
        "list" => "M8 5h13 M8 12h13 M8 19h13 M3 5h1 M3 12h1 M3 19h1",
        "grid" => "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z",
        _ => "m12 3 10 5-10 5L2 8Z M2 12l10 5 10-5 M2 16l10 5 10-5",
    };
    format!(
        "<svg class=\"icon\" viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"{path}\"/></svg>"
    )
}

pub struct Page<'a> {
    pub surface: Surface<'a>,
    pub title: &'a str,
    pub json_url: &'a str,
}
impl Page<'_> {
    /// `body` and `head` must be trusted markup built from escaped values.
    pub fn render(&self, body: &str, head: &str) -> String {
        self.render_with_breadcrumbs(body, head, &[])
    }

    /// Override labels using resource paths; URLs retain their original IDs.
    pub fn render_with_breadcrumbs(
        &self,
        body: &str,
        head: &str,
        labels: &[(&str, &str)],
    ) -> String {
        let Self {
            surface: Surface { base, root, api },
            title,
            json_url,
        } = self;
        let html_url = with_format(json_url, "html");
        let current_path = json_url.split('?').next().unwrap_or(json_url);
        let api_title = APIS
            .iter()
            .find(|(id, _, _)| id == api)
            .map_or("MeteoCore", |(_, name, _)| *name);
        // The current API opens at this page's root, wherever it is mounted.
        let api_home = |id: &str, mount: &str| {
            if id == *api {
                format!("{root}/?f=html")
            } else {
                format!("{base}{mount}/?f=html")
            }
        };
        let api_nav = APIS
            .iter()
            .map(|(id, name, mount)| {
                anchor(
                    &api_home(id, mount),
                    name,
                    if id == api { "active" } else { "" },
                )
            })
            .collect::<String>();
        let (workspace_api, workspace_root) = (*api, *root);
        let options = APIS
            .iter()
            .map(|(id, name, mount)| {
                format!(
                    "<option value=\"{}\" {}>{name}</option>",
                    escape(&api_home(id, mount)),
                    if *id == workspace_api { "selected" } else { "" }
                )
            })
            .collect::<String>();
        let mut nav = String::from("<div class=\"nav-label\">EXPLORE</div>");
        {
            for (suffix, label) in [
                ("/", "Overview"),
                ("/collections", "Collections"),
                ("/api/docs", "API reference"),
                ("/conformance", "Standards"),
            ] {
                let path = format!("{workspace_root}{suffix}");
                if suffix == "/api/docs" {
                    nav.push_str(
                        "<div class=\"nav-divider\"></div><div class=\"nav-label\">RESOURCES</div>",
                    );
                }
                nav.push_str(
                    &anchor(
                        &if suffix.ends_with("docs") {
                            path.clone()
                        } else {
                            with_format(&path, "html")
                        },
                        label,
                        if current_path.trim_end_matches('/') == path.trim_end_matches('/')
                            || suffix == "/collections"
                                && current_path.starts_with(&format!("{path}/"))
                        {
                            "nav-link active"
                        } else {
                            "nav-link"
                        },
                    )
                    .replacen(
                        &format!(">{label}</a>"),
                        &format!(
                            ">{}{label}</a>",
                            icon(match suffix {
                                "/" => "home",
                                "/api/docs" => "code",
                                "/conformance" => "book",
                                _ => "layers",
                            })
                        ),
                        1,
                    ),
                );
            }
        }
        let mut crumbs = anchor(&format!("{base}/?f=html"), "MeteoCore", "");
        {
            // An API mounted at the server root is already the first crumb.
            if root != base {
                crumbs.push_str(&format!(
                    "<span>/</span>{}",
                    anchor(&format!("{root}/?f=html"), api_title, "")
                ));
            }
            let prefix = format!("{root}/");
            let mut path = prefix.trim_end_matches('/').to_string();
            if let Some(rest) = current_path.strip_prefix(&prefix) {
                for segment in rest.split('/').filter(|s| !s.is_empty()) {
                    path.push('/');
                    path.push_str(segment);
                    let decoded = form_urlencoded::parse(
                        format!("x={}", segment.replace('+', "%2B")).as_bytes(),
                    )
                    .next()
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                    let label = labels
                        .iter()
                        .find(|(href, label)| {
                            href.split('?').next().unwrap_or(href).trim_end_matches('/') == path
                                && !label.trim().is_empty()
                        })
                        .map(|(_, label)| *label)
                        .unwrap_or(match segment {
                            "collections" => "Collections",
                            "items" => "Items",
                            "instances" => "Model runs",
                            "conformance" => "Conformance",
                            _ => &decoded,
                        });
                    crumbs.push_str(&format!(
                        "<span>/</span>{}",
                        anchor(&with_format(&path, "html"), label, "")
                    ));
                }
            }
        }
        // Feature items (`…/collections/{id}/items[/{featureId}]`) are GeoJSON.
        let items = current_path
            .strip_prefix(&format!("{root}/collections/"))
            .is_some_and(|tail| tail.split('/').nth(1) == Some("items"));
        let json_type = if items {
            "application/geo+json"
        } else {
            "application/json"
        };
        let curl = format!("curl --get '{}'", json_url.replace('\'', "'\"'\"'"));
        let catalog = current_path.trim_end_matches('/') == format!("{root}/collections");
        let request_content = format!(
            r#"<div class="request-identity"><span class="method">GET</span><code id="request-url">{json_url}</code></div><div class="request-tools"><span class="request-note">Applied request · JSON representation</span><button class="btn small enhanced" data-copy="{json_url}">Copy URL</button><button class="btn small enhanced" data-copy="{curl}">Copy cURL</button><a class="btn small" href="{json_url}">Open JSON ↗</a></div>"#,
            json_url = escape(safe_href(json_url)),
            curl = escape(&curl)
        );
        let request_context = if catalog || items {
            format!("<details id=\"request-context\" class=\"catalog-request\"><summary>Current API request <code>GET {}</code></summary><div class=\"request-content\">{request_content}</div></details>",escape(current_path.strip_prefix(base).unwrap_or(current_path)))
        } else {
            format!("<section id=\"request-context\" aria-label=\"Current API request\">{request_content}</section>")
        };
        format!(
            r##"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title} · MeteoCore</title><link rel="alternate" type="{json_type}" href="{json_url}"><style>{CSS}</style><script>{THEME}</script>{head}</head>
<body class="{page_class}"><a class="skip" href="#main">Skip to content</a><div class="shell"><aside class="sidebar" aria-label="API navigation"><a class="brand" href="{base}/?f=html"><span class="brand-mark"><svg viewBox="0 0 32 32" fill="none" aria-hidden="true"><circle cx="16" cy="16" r="11"/><circle cx="16" cy="16" r="6"/><path d="M16 16 27 5M16 3v3M3 16h3M16 26v3M26 16h3"/></svg></span><span>MeteoCore<small>API WORKBENCH</small></span></a><div class="api-switch enhanced"><label for="api-select">API workspace</label><select id="api-select">{options}</select></div><noscript><nav aria-label="APIs">{api_nav}</nav></noscript><nav id="primary-nav" aria-label="Workspace">{nav}</nav><div class="sidebar-note"><span class="eyebrow">OPEN STANDARDS</span><p>One data platform.<br>Different ways to explore.</p><a href="{base}/?f=html">Explore all APIs ↗</a></div><div class="sidebar-footer">MeteoCore<small>OGC API · Developer workspace</small></div></aside>
<div class="workspace"><header class="topbar"><nav id="breadcrumbs" aria-label="Breadcrumb">{crumbs}</nav><div class="topbar-actions"><button class="quiet enhanced" id="help-button">Help</button><label class="theme-label enhanced" for="theme">Theme<select id="theme"><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label><nav class="representation-switch" aria-label="Representation"><a aria-current="page" href="{html_url}">HTML</a><a rel="alternate" id="json-link" href="{json_url}">JSON</a></nav></div></header>
{request_context}
<main id="main" tabindex="-1">{body}</main><footer class="page-footer"><span>MeteoCore · Open weather data</span><span>HTML representation · Times in UTC</span></footer></div></div><dialog id="help-dialog"><div class="dialog-head"><h2>Find a collection, then request data</h2><button class="icon-button" id="close-help" aria-label="Close help">×</button></div><div class="panel-body"><p><strong>1. Find a collection.</strong> Collection search matches titles, descriptions and keywords. Commas separate alternatives; words form a phrase.</p><p>Advanced search adds required (+) and excluded (-) terms. Area and time narrow coverage; unknown extents remain eligible.</p><p><strong>2. Request data.</strong> Open a collection to see its data operations. Feature filters select items within that collection; discovery filters are not copied into the data request. The JSON switch always opens the current resource with its applied filters and paging.</p></div></dialog><div id="toast" role="status" aria-live="polite"></div><script>{SCRIPT}</script></body></html>"##,
            title = escape(title),
            json_url = escape(safe_href(json_url)),
            html_url = escape(safe_href(&html_url)),
            base = escape(base),
            page_class = if items {
                "items-page"
            } else if catalog {
                "catalog-page"
            } else {
                "resource-page"
            }
        )
    }
}

pub fn page_heading(title: &str, description: &str) -> String {
    format!("<div class=\"page-title\"><div><span class=\"eyebrow\">OGC API · HTML REPRESENTATION</span><h1>{}</h1><p>{}</p></div></div>",escape(title),escape(description))
}

/// Render arbitrary metadata without losing types or flattening nested values.
pub fn value_html(value: &Value) -> String {
    match value {
        Value::Null => "<span class=\"muted\">Not available</span>".into(),
        Value::String(v) => escape(v),
        Value::Array(values) if values.iter().all(|v| !v.is_object() && !v.is_array()) => {
            format!(
                "<span class=\"chip-row\">{}</span>",
                values
                    .iter()
                    .map(|v| format!("<span class=\"chip\">{}</span>", value_html(v)))
                    .collect::<String>()
            )
        }
        Value::Array(_) | Value::Object(_) => format!(
            "<pre>{}</pre>",
            escape(&serde_json::to_string_pretty(value).expect("metadata serializes"))
        ),
        _ => escape(&value.to_string()),
    }
}

pub fn property_table(properties: &Value) -> String {
    let mut out=String::from("<div class=\"table-scroll\"><table class=\"properties property-table\"><thead><tr><th scope=\"col\">Property</th><th scope=\"col\">Value</th></tr></thead><tbody>");
    if let Some(props) = properties.as_object() {
        for (key, value) in props {
            out.push_str(&format!(
                "<tr data-property=\"{}\"><th scope=\"row\"><code>{}</code></th><td>{}</td></tr>",
                escape(key),
                escape(key),
                value_html(value)
            ));
        }
    }
    out.push_str("</tbody></table></div>");
    out
}

pub fn document_links(doc: &Value) -> String {
    let mut body = String::from("<div class=\"endpoint-list resource-links\">");
    // Relations are often advertised twice (short and registered URI form)
    // with the same href, type and title; list such a pair once, under the
    // first relation. Differently titled links to one resource (a tileset
    // list offered as map and vector tilesets) each stay visible.
    let mut listed: Vec<(&str, &str, &str)> = Vec::new();
    if let Some(links) = doc["links"].as_array() {
        for link in links {
            let href = link["href"].as_str().unwrap_or_default();
            let rel = link["rel"].as_str().unwrap_or_default();
            if matches!(rel, "self" | "alternate") {
                continue;
            }
            let media = link["type"].as_str().unwrap_or_default();
            let title = link["title"].as_str().unwrap_or_default();
            if listed.contains(&(href, media, title)) {
                continue;
            }
            listed.push((href, media, title));
            let label = link["title"].as_str().unwrap_or(rel);
            let human = matches!(
                rel,
                "data" | "child" | "collection" | "conformance" | "up" | "items"
            );
            let target = if human {
                with_format(href, "html")
            } else {
                href.to_owned()
            };
            body.push_str(&format!(
                "<div class=\"endpoint\"><div><a class=\"resource-title\" href=\"{}\"><strong>{}</strong><code>{}</code></a><p>{} · {}{}</p></div></div>",
                escape(safe_href(&target)),
                escape(label),
                escape(href),
                escape(rel),
                escape(link["type"].as_str().unwrap_or("Linked resource")),
                if rel == "map" { " · requires bbox; use the map controls to build a request" } else { "" }
            ));
        }
    }
    body.push_str("</div>");
    body
}

pub fn landing_html(
    surface: Surface<'_>,
    title: &str,
    description: &str,
    links: &[LinkView],
) -> String {
    let doc = json!({"links":links.iter().map(|l|json!({"href":l.href,"rel":l.rel,"title":l.title})).collect::<Vec<_>>()});
    landing_document(surface, title, description, &doc)
}

pub fn landing_document(
    surface: Surface<'_>,
    title: &str,
    description: &str,
    doc: &Value,
) -> String {
    let Surface { base, root, api } = surface;
    let url = format!("{root}/?f=json");
    let discovery = root;
    let mut body = page_heading(title, description);
    body.push_str("<section class=\"panel\"><div class=\"panel-head\"><h2>Resources</h2></div><div class=\"resource-table\">");
    for link in doc["links"].as_array().into_iter().flatten() {
        let rel = link["rel"].as_str().unwrap_or_default();
        let href = link["href"].as_str().unwrap_or_default();
        if !matches!(rel, "data" | "conformance" | "service-desc") {
            continue;
        }
        let target = if rel == "service-desc" {
            href.to_owned()
        } else {
            with_format(href, "html")
        };
        let label = link["title"].as_str().unwrap_or(rel);
        body.push_str(&format!("<a class=\"resource-row\" href=\"{}\"><span class=\"method\">GET</span><code>{}</code><span>{}</span>{}</a>",escape(safe_href(&target)),escape(href.strip_prefix(base).unwrap_or(href)),escape(label),icon("arrow")));
    }
    body.push_str("</div></section>");
    let data_guidance = match api {
        "features" => "Open a collection, then use Request data to filter its features by the supported properties, area and time. Inspect the results or copy the GeoJSON request.",
        "edr" => "Open a collection to see its supported data queries and parameters. Choose a location, position or area query, then use the linked API reference to supply the required inputs.",
        "maps" => "Open a collection to see its map resources. Use the map preview to inspect the data, or the API reference to request an image for an area, time and style.",
        "tiles" => "Open a collection and follow its tileset links. Select a tile matrix set and tile coordinates to request map or vector tiles.",
        crate::shared::WORKSPACE => "Open a collection to see every way to access it: map images, map tiles and vector tiles, as the collection offers them. The per-API services remain available below.",
        _ => "Choose an API and a collection first. Then request features, environmental values, map images or tiles using that collection's supported operations.",
    };
    body.push_str(&format!(r#"<div class="developer-start"><section class="panel panel-body"><span class="eyebrow">COLLECTION DISCOVERY</span><h2>1. Find a collection</h2><form method="get" action="{discovery}/collections"><input type="hidden" name="f" value="html"><label for="q">Search collections <span class="parameter-type">q · optional</span></label><div class="search-row"><input id="q" name="q" placeholder="radar"><button class="btn primary">Find collections {arrow}</button></div><p class="field-help">Search dataset titles, descriptions and keywords. Area and time filters in the catalog narrow the collection coverage.</p></form></section><section class="panel panel-body"><span class="eyebrow">DATA ACCESS</span><h2>2. Request data</h2><p class="section-note">{data_guidance}</p><p class="field-help">Select a collection to see the available data requests. Discovery filters are not carried over as data filters.</p></section></div>"#,discovery=escape(discovery),arrow=icon("arrow")));
    body.push_str("<section class=\"section-space\"><div class=\"section-header\"><h2>API definitions &amp; resource links</h2></div>");
    body.push_str(&document_links(doc));
    body.push_str("</section>");
    Page {
        surface,
        title,
        json_url: &url,
    }
    .render(&body, "")
}

pub fn conformance_html(surface: Surface<'_>, classes: &[&str], nav: &[LinkView]) -> String {
    let url = nav
        .iter()
        .find(|l| l.rel == "alternate")
        .map(|l| l.href.as_str())
        .unwrap_or("");
    let mut body=page_heading("Conformance classes","Classes declared by this API. Draft work is advertised only when its requirements are implemented.");
    body.push_str("<ul class=\"panel conformance\">");
    for class in classes {
        body.push_str(&format!("<li><code>{}</code></li>", escape(class)));
    }
    body.push_str("</ul>");
    Page {
        surface,
        title: "Conformance classes",
        json_url: url,
    }
    .render(&body, "")
}

pub fn map_head(base: &str) -> String {
    format!(
        "<link rel=\"stylesheet\" href=\"{}/preview/vendor/maplibre-gl.css\">",
        escape(base)
    )
}

/// Expand only advertised temporal grids, never infer a cadence from bounds.
/// Large axes fall back to a native UTC date/time input instead of huge HTML.
fn map_times(temporal: &Value) -> Vec<String> {
    fn expand(temporal: &Value) -> Option<Vec<String>> {
        let grid = &temporal["grid"];
        let count = grid["cellsCount"].as_u64()?;
        if !(1..=10_000).contains(&count) {
            return None;
        }
        if let Some(coordinates) = grid["coordinates"].as_array() {
            if coordinates.len() as u64 != count {
                return None;
            }
            return coordinates
                .iter()
                .map(|t| {
                    chrono::DateTime::parse_from_rfc3339(t.as_str()?)
                        .ok()
                        .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
                })
                .collect();
        }
        let step = ds_core::datetime::parse_iso8601_duration(grid["resolution"].as_str()?).ok()?;
        let start =
            chrono::DateTime::parse_from_rfc3339(temporal["interval"][0][0].as_str()?).ok()?;
        let end =
            chrono::DateTime::parse_from_rfc3339(temporal["interval"][0][1].as_str()?).ok()?;
        let last = start.checked_add_signed(step.checked_mul((count - 1) as i32)?)?;
        if last > end {
            return None;
        }
        (0..count)
            .map(|i| {
                start
                    .checked_add_signed(step.checked_mul(i as i32)?)
                    .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339())
            })
            .collect()
    }
    expand(temporal).unwrap_or_default()
}

pub fn map_html(base: &str, features: &Value, quicklook: bool) -> String {
    let raster = features.get("mapRequest");
    let mut controls = String::new();
    if let Some(request) = raster {
        controls.push_str("<form id=\"map-controls\" class=\"map-controls enhanced\"><label>Style<select id=\"map-style\">");
        for style in request["styles"].as_array().into_iter().flatten() {
            controls.push_str(&format!(
                "<option value=\"{}\" data-legend=\"{}\">{}</option>",
                escape(safe_href(style["href"].as_str().unwrap_or_default())),
                escape(style["legend"].as_str().map(safe_href).unwrap_or_default()),
                escape(style["title"].as_str().unwrap_or("Default"))
            ));
        }
        controls.push_str("</select></label><div class=\"map-time-field\"><label for=\"map-time\">Time · UTC</label><div class=\"map-time-picker\">");
        let times = request["times"]
            .as_array()
            .filter(|times| !times.is_empty());
        if let Some(times) = times {
            controls.push_str("<button type=\"button\" class=\"btn\" id=\"map-time-prev\" aria-label=\"Previous available time\">←</button><select id=\"map-time\" aria-describedby=\"map-help\"><option value=\"\">Collection default</option>");
            for time in times.iter().filter_map(Value::as_str) {
                let label = chrono::DateTime::parse_from_rfc3339(time)
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|_| time.to_owned());
                controls.push_str(&format!(
                    "<option value=\"{}\">{}</option>",
                    escape(time),
                    escape(&label)
                ));
            }
            controls.push_str("</select><button type=\"button\" class=\"btn\" id=\"map-time-next\" aria-label=\"Next available time\">→</button>");
        } else {
            controls.push_str("<input id=\"map-time\" type=\"datetime-local\" step=\"1\" aria-describedby=\"map-help\">");
        }
        controls.push_str("</div></div>");
        let vertical = &request["vertical"];
        let levels = vertical_values(vertical);
        if !levels.is_empty() {
            let (label, unit) = vertical_axis(vertical);
            controls.push_str(&format!(
                "<label>{}<select id=\"map-level\"><option value=\"\">Collection default</option>",
                escape(label)
            ));
            for level in levels {
                controls.push_str(&format!(
                    "<option value=\"{}\">{} {}</option>",
                    escape(&level),
                    escape(&level),
                    escape(unit)
                ));
            }
            controls.push_str("</select></label>");
        }
        controls.push_str("<button class=\"btn primary\">Update map</button><p id=\"map-help\">Pan or zoom to request the visible area. ");
        if let Some(times) = times {
            controls.push_str(&format!(
                "{} advertised times. Arrow buttons load the adjacent time. ",
                times.len()
            ));
        }
        controls.push_str("An empty/default time uses the collection default.</p></form>");
    }
    let label = if raster.is_some() {
        "Collection map data"
    } else {
        "Feature geometry map"
    };
    let request_link = if raster.is_some() {
        "<div class=\"map-request panel-body enhanced\"><a id=\"map-image-link\" class=\"btn small\" aria-disabled=\"true\">Open rendered image ↗</a><code id=\"map-image-request\"></code></div>"
    } else {
        ""
    };
    format!("{controls}<div class=\"map-panel\"><div id=\"feature-map\" data-land=\"{base}/preview/vendor/workbench-land.json\" data-quicklook=\"{quicklook}\" role=\"region\" aria-label=\"{label}\"></div><p id=\"map-status\" role=\"status\">Map requires JavaScript and WebGL; coordinates remain available below.</p>{request_link}<div id=\"map-data\" hidden>{}</div></div><script src=\"{base}/preview/vendor/maplibre-gl.js\"></script><script>{}</script>",escape(&features.to_string()),include_str!("workbench/map.js"),base=escape(base))
}

pub fn collection_html(
    surface: Surface<'_>,
    doc: &Value,
    license: Option<&LicenseConfig>,
) -> String {
    let Surface { base, root, api } = surface;
    let title = doc["title"]
        .as_str()
        .filter(|title| !title.trim().is_empty())
        .or(doc["id"].as_str())
        .unwrap_or("Collection");
    let id = doc["id"].as_str().unwrap_or_default();
    let links = doc["links"].as_array();
    let href = links
        .and_then(|ls| ls.iter().find(|l| l["rel"] == "self"))
        .and_then(|l| l["href"].as_str())
        .unwrap_or_default();
    let url = with_format(href, "json");
    let catalog = format!("{root}/collections");
    // Data access follows the advertised links, not the API the page is
    // served by, so a collection offering several mechanisms shows each.
    let items = links
        .and_then(|ls| ls.iter().find(|l| l["rel"] == "items"))
        .and_then(|l| l["href"].as_str());
    let map_request = {
        links.and_then(|ls| map_link(ls))
            .filter(|href| safe_href(href) != "#")
            .map(|href| {
                let legend = |style: &Value| style["links"].as_array()
                    .and_then(|ls|ls.iter().find(|l|l["rel"] == "legend"))
                    .and_then(|l|l["href"].as_str()).filter(|href|safe_href(href) != "#")
                    .map(str::to_owned);
                let default_legend = doc["styles"].as_array()
                    .and_then(|styles|styles.iter().find(|s|s["id"] == "default")).and_then(legend);
                let mut styles = vec![json!({"title":"Collection default","href":href,"legend":default_legend})];
                for style in doc["styles"].as_array().into_iter().flatten() {
                    // The collection map endpoint already renders this style.
                    if style["id"] == "default" { continue; }
                    if let Some(href) = style["links"].as_array().and_then(|ls|map_link(ls)).filter(|href|safe_href(href)!="#") {
                        styles.push(json!({"title":style["title"].as_str().or(style["id"].as_str()).unwrap_or("Style"),"href":href,"legend":legend(style)}));
                    }
                }
                json!({"styles":styles,"times":map_times(&doc["extent"]["temporal"]),"vertical":doc["extent"]["vertical"]})
            })
    };
    let kind = doc["itemType"]
        .as_str()
        .or(doc["dataType"].as_str())
        .unwrap_or("Environmental data");
    let mut body=format!("<a class=\"back-link\" data-back-scope=\"{}\" href=\"{}\">← Back to collections</a><div class=\"detail-heading\"><div class=\"chip-row\"><span class=\"chip teal\">{}</span><span class=\"chip\">{}</span></div><h1>{}</h1><div class=\"mono\">{}</div><p>{}</p></div><nav class=\"detail-tabs\" aria-label=\"Collection views\"><a data-collection-tab=\"overview\" class=\"active\" href=\"#overview\">Overview</a>",escape(&catalog),escape(&with_format(&catalog,"html")),escape(api),escape(kind),escape(title),escape(id),escape(doc["description"].as_str().unwrap_or_default()));
    if let Some(items) = items {
        body.push_str(&anchor(&with_format(items, "html"), "Request data", ""));
    }
    body.push_str("<a data-collection-tab=\"metadata\" href=\"#metadata\">Metadata &amp; links</a></nav><section id=\"overview\" class=\"collection-view\" data-collection-view><div class=\"detail-layout\"><div><section class=\"panel\"><div class=\"panel-head\"><h2>Data coverage</h2><span class=\"chip\">CRS84</span></div>");
    if map_request.is_some() {
        body = body.replace("<h2>Data coverage</h2>", "<h2>Map data &amp; coverage</h2>");
    }
    let bbox = &doc["extent"]["spatial"]["bbox"][0];
    let interval = &doc["extent"]["temporal"]["interval"][0];
    let mut head = String::new();
    if let Some(b) = bbox
        .as_array()
        .filter(|b| matches!(b.len(), 4 | 6) && b.iter().all(Value::is_number))
    {
        let east = if b.len() == 6 { 3 } else { 2 };
        let north = east + 1;
        let (w, s, e, n) = (
            b[0].as_f64().unwrap(),
            b[1].as_f64().unwrap(),
            b[east].as_f64().unwrap(),
            b[north].as_f64().unwrap(),
        );
        let ring = |a: f64, z: f64| json!([[a, s], [z, s], [z, n], [a, n], [a, s]]);
        let geometry = if w <= e {
            json!({"type":"Polygon","coordinates":[ring(w,e)]})
        } else {
            json!({"type":"MultiPolygon","coordinates":[[ring(w,180.)],[ring(-180.,e)]]})
        };
        head = map_head(base);
        let mut map_data = json!({"type":"FeatureCollection","features":[{"type":"Feature","geometry":geometry,"properties":{"label":"Advertised collection extent"}}]});
        if let Some(request) = &map_request {
            map_data["mapRequest"] = request.clone();
        }
        body.push_str(&map_html(base, &map_data, false));
    } else if let Some(request) = &map_request {
        head = map_head(base);
        body.push_str(&map_html(
            base,
            &json!({"type":"FeatureCollection","features":[],"mapRequest":request}),
            false,
        ));
    } else {
        body.push_str("<div class=\"empty-state\"><p>Spatial extent not specified.</p></div>");
    }
    body.push_str(&format!("<div class=\"coverage-facts\"><div class=\"fact\"><small>Start · UTC</small><strong>{}</strong></div><div class=\"fact\"><small>End · UTC</small><strong>{}</strong></div><div class=\"fact wide\"><small>Advertised bounds · CRS84 axis order</small><strong class=\"mono\">{}</strong></div>{}</div></section><section class=\"section-space\"><h2>Request data from this collection</h2>",value_html(&interval[0]),value_html(&interval[1]),value_html(bbox),vertical_coverage(&doc["extent"]["vertical"])));
    if let Some(items) = items {
        body.push_str(&format!("<div class=\"callout spaced\">Filter features within this collection by its supported properties, area and time. Inspect the results or copy the GeoJSON request. These filters select data, not collections.</div><div class=\"action-row spaced\">{}{}</div>",anchor(&with_format(items,"html"),"Build a data request →","btn primary"),anchor(&url,"View metadata JSON","btn")));
    }
    if let Some(queries) = doc["data_queries"].as_object() {
        body.push_str("<div class=\"endpoint-list spaced\">");
        for (name, query) in queries {
            if let Some(href) = query["link"]["href"].as_str() {
                let target = if name == "instances" {
                    with_format(href, "html")
                } else {
                    format!("{root}/api/docs")
                };
                body.push_str(&format!("<div class=\"endpoint\"><div><strong>{} query</strong><code>{}</code><p>{}</p></div>{}</div>",escape(name),escape(href),value_html(&query["link"]["variables"]["output_formats"]),anchor(&target,if name=="instances"{"Browse runs →"}else{"API docs ↗"},"btn small")));
            }
        }
        body.push_str("</div>");
    }
    if let Some(map_href) = links
        .and_then(|ls| map_link(ls))
        .filter(|_| map_request.is_some())
    {
        body.push_str("<p class=\"spaced\">Use the map controls above to build an image request for the visible area. The image URL includes the selected style, time, vertical level, bounds and output size. The JSON switch opens this collection’s metadata.</p>");
        // Document the map request at the API serving the map link, which
        // need not be the API rendering this page.
        let map_api = map_href.split("/collections/").next().unwrap_or(root);
        body.push_str(&anchor(
            &format!("{map_api}/api/docs"),
            "Map request parameters ↗",
            "btn",
        ));
    }
    // Tilesets are listed whenever advertised, next to any map preview.
    let tilesets: Vec<Value> = links
        .into_iter()
        .flatten()
        .filter(|l| {
            l["rel"]
                .as_str()
                .is_some_and(|r| r == "tiles" || r.starts_with(TILESETS_REL_PREFIX))
        })
        .cloned()
        .collect();
    if !tilesets.is_empty() {
        body.push_str(&document_links(&json!({ "links": tilesets })));
    }
    body.push_str("</section></div><aside class=\"aside-stack\"><section class=\"panel\"><div class=\"panel-head\"><h2>Collection details</h2></div><div class=\"panel-body\"><dl class=\"definition\">");
    body.push_str(&format!(
        "<dt>Resource</dt><dd>{}</dd><dt>Storage coordinate system</dt><dd>{}</dd>",
        escape(kind),
        escape(doc["storageCrs"].as_str().unwrap_or("Not advertised"))
    ));
    if let Some(n) = doc.get("numberItems") {
        body.push_str(&format!("<dt>Items</dt><dd>{}</dd>", value_html(n)));
    }
    body.push_str("</dl>");
    body.push_str(&license_html(license));
    body.push_str("<div class=\"spaced\"><label>Keywords</label><div class=\"tag-list\">");
    for k in doc["keywords"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        body.push_str(&anchor(&query_edit(&catalog, "q", Some(k)), k, ""));
    }
    if !doc["keywords"]
        .as_array()
        .is_some_and(|ks| ks.iter().any(Value::is_string))
    {
        body.push_str("<span class=\"muted\">Not advertised</span>");
    }
    body.push_str("</div></div></div></section>");
    if map_request.is_some() {
        body.push_str("<section class=\"panel enhanced\"><div class=\"panel-head\"><h2>Map legend</h2></div><div class=\"map-legend\"><span id=\"map-legend-status\">Legend will appear after the map loads.</span><a id=\"map-legend-link\" hidden><img id=\"map-legend-image\" alt=\"Selected map style legend\" hidden></a></div></section>");
    }
    body.push_str("<section class=\"panel\"><div class=\"panel-head\"><h2>Resource links</h2></div><div class=\"panel-body\">");
    body.push_str(&document_links(doc));
    body.push_str("</div></section></aside></div></section><section id=\"metadata\" class=\"collection-view metadata-section\" data-collection-view><section class=\"panel\"><div class=\"panel-head\"><h2>Coverage &amp; metadata</h2>");
    body.push_str(&anchor(&url, "View full JSON { }", "quiet"));
    body.push_str("</div>");
    let mut metadata = doc.clone();
    if let Some(m) = metadata.as_object_mut() {
        m.remove("links");
    }
    body.push_str(&property_table(&metadata));
    body.push_str("</section><section class=\"panel spaced\"><div class=\"panel-head\"><h2>Resource links</h2></div><div class=\"panel-body\">");
    body.push_str(&document_links(doc));
    body.push_str("</div></section></section>");
    Page {
        surface,
        title,
        json_url: &url,
    }
    .render_with_breadcrumbs(&body, &head, &{
        let mut labels = links
            .into_iter()
            .flatten()
            .filter(|l| l["rel"] == "collection")
            .filter_map(|l| Some((l["href"].as_str()?, l["title"].as_str()?)))
            .collect::<Vec<_>>();
        labels.push((href, title));
        labels
    })
}

/// A labelled native input; optional enhanced fields acquire names only while
/// nonempty, avoiding accidental empty exact-match filters or malformed query.
pub fn input(name: &str, value: &str, help: &str, kind: &str, enhanced: bool) -> String {
    let binding = if enhanced {
        format!("data-param=\"{}\"", escape(name))
    } else {
        format!("name=\"{}\"", escape(name))
    };
    format!("<label class=\"field {}\"><span>{}</span><span class=\"hint\">{}</span><input {binding} type=\"{}\" value=\"{}\" {}></label>",if enhanced{"enhanced"}else{""},escape(name),escape(help),escape(kind),escape(value),if kind=="number" {if name=="limit"{"min=\"1\" step=\"1\""}else{"min=\"0\" step=\"1\""}}else{""})
}

pub struct CollectionView<'a> {
    pub metadata: &'a Value,
    pub license: Option<&'a LicenseConfig>,
}

// A free-text license may have no JSON link, but must remain visible in HTML.
fn license_html(license: Option<&LicenseConfig>) -> String {
    license
        .map(|license| {
            let text = license
                .resolved_url()
                .map(|url| anchor(&url, &license.title, ""))
                .unwrap_or_else(|| escape(&license.title));
            format!("<p class=\"license\">License: {text}</p>")
        })
        .unwrap_or_default()
}

pub fn query_edit(href: &str, key: &str, value: Option<&str>) -> String {
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let mut out = form_urlencoded::Serializer::new(String::new());
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        if k != key && k != "offset" && k != "f" {
            out.append_pair(&k, &v);
        }
    }
    if let Some(value) = value {
        out.append_pair(key, value);
    }
    out.append_pair("f", "html");
    format!("{path}?{}", out.finish())
}

/// Compact UTC coverage, retaining exact bounds in the accessible tooltip.
fn collection_time_html(temporal: &Value) -> String {
    let interval = &temporal["interval"][0];
    if !interval.is_array() {
        return "Not advertised".into();
    }
    let parse = |v: &Value| {
        v.as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let start = parse(&interval[0]);
    let end = parse(&interval[1]);
    let clock = |t: &chrono::DateTime<chrono::Utc>| {
        if t.timestamp_subsec_nanos() != 0 {
            t.format("%H:%M:%S%.f").to_string()
        } else if t.timestamp() % 60 != 0 {
            t.format("%H:%M:%S").to_string()
        } else {
            t.format("%H:%M").to_string()
        }
    };
    let endpoint = |t: Option<chrono::DateTime<chrono::Utc>>, raw: &Value, open: &str| {
        t.map(|t| format!("{} {}", t.format("%Y-%m-%d"), clock(&t)))
            .unwrap_or_else(|| raw.as_str().unwrap_or(open).to_owned())
    };
    let parts = match (start, end) {
        (Some(a), Some(b)) if a == b => {
            vec![endpoint(Some(a), &interval[0], "Open start"), "UTC".into()]
        }
        (Some(a), Some(b)) if a.date_naive() == b.date_naive() => vec![
            a.format("%Y-%m-%d").to_string(),
            format!("{}–{} UTC", clock(&a), clock(&b)),
        ],
        _ => vec![
            endpoint(start, &interval[0], "Open start"),
            "→".into(),
            endpoint(end, &interval[1], "Open end"),
            "UTC".into(),
        ],
    };
    format!(
        "<span class=\"collection-time\" title=\"{}\">{}</span>",
        escape(&interval.to_string()),
        parts
            .iter()
            .map(|p| format!("<span>{}</span>", escape(p)))
            .collect::<String>()
    )
}

// EDR uses string coordinates and a VRS; Common uses numbers and a unit.
// Recognize our canonical VRS definitions without guessing units for unknown CRS.
fn vertical_axis(vertical: &Value) -> (&str, &str) {
    use ds_core::vertical::VerticalKind;
    for kind in [
        VerticalKind::Pressure,
        VerticalKind::ModelLevel,
        VerticalKind::Height,
        VerticalKind::ElevationAngle,
        VerticalKind::HeightAboveAntenna,
        VerticalKind::Isentropic,
    ] {
        if vertical["vrs"].as_str() == Some(kind.vrs()) {
            return (
                kind.default_label(),
                vertical["unit"].as_str().unwrap_or(kind.default_unit()),
            );
        }
    }
    (
        "Vertical levels",
        vertical["unit"].as_str().unwrap_or_default(),
    )
}

fn vertical_coordinate(value: &Value) -> Option<String> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_f64().map(|n| n.to_string()))?;
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|_| text)
}

fn vertical_values(vertical: &Value) -> Vec<String> {
    vertical["values"]
        .as_array()
        .or_else(|| vertical["grid"]["coordinates"].as_array())
        .into_iter()
        .flatten()
        .filter_map(vertical_coordinate)
        .collect()
}

fn vertical_summary(vertical: &Value) -> Option<String> {
    if !vertical.is_object() {
        return None;
    }
    let (label, unit) = vertical_axis(vertical);
    let range = &vertical["interval"][0];
    let bounds = match (
        vertical_coordinate(&range[0]),
        vertical_coordinate(&range[1]),
    ) {
        (Some(lo), Some(hi)) if lo == hi => lo,
        (Some(lo), Some(hi)) => format!("{lo}–{hi}"),
        _ => String::new(),
    };
    let levels = vertical_values(vertical);
    let count = if levels.is_empty() {
        String::new()
    } else {
        format!(" · {} levels", levels.len())
    };
    Some(escape(&format!("{label}: {bounds} {unit}{count}")))
}

fn vertical_coverage(vertical: &Value) -> String {
    let Some(summary) = vertical_summary(vertical) else {
        return String::new();
    };
    let mut html = format!(
        "<div class=\"fact wide\"><small>Vertical dimension</small><strong>{summary}</strong>"
    );
    let levels = vertical_values(vertical);
    if !levels.is_empty() {
        html.push_str(&format!(
            "<details><summary>Available levels · z</summary><p class=\"mono\">{}</p></details>",
            escape(&levels.join(", "))
        ));
    }
    if let Some(vrs) = vertical.get("vrs") {
        html.push_str(&format!(
            "<details><summary>Vertical reference system</summary>{}</details>",
            value_html(vrs)
        ));
    }
    html.push_str("</div>");
    html
}

fn collection_facts(doc: &Value) -> String {
    let mut facts = String::from("<dl class=\"collection-facts\">");
    let mut fact = |label: &str, value: String| {
        facts.push_str(&format!(
            "<div><dt>{}</dt><dd>{value}</dd></div>",
            escape(label)
        ))
    };
    if let Some(summary) = vertical_summary(&doc["extent"]["vertical"]) {
        fact("Vertical dimension", summary);
    }
    let temporal = &doc["extent"]["temporal"];
    fact("Time · UTC", collection_time_html(temporal));
    if let Some(resolution) = temporal["grid"]["resolution"].as_str() {
        let label = ds_core::datetime::parse_iso8601_duration(resolution)
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
            .unwrap_or_else(|| resolution.to_owned());
        fact("Time resolution", escape(&label));
    } else if let Some(count) = temporal["grid"]["cellsCount"].as_u64().or_else(|| {
        temporal["values"]
            .as_array()
            .filter(|v| v.len() > 1)
            .map(|v| v.len() as u64)
    }) {
        fact("Time steps", format!("{count} advertised"));
    }
    let bbox = &doc["extent"]["spatial"]["bbox"][0];
    if let Some(b) = bbox.as_array().filter(|b| matches!(b.len(), 4 | 6)) {
        let xy = if b.len() == 6 {
            [0, 1, 3, 4]
        } else {
            [0, 1, 2, 3]
        };
        let values: Option<Vec<f64>> = xy
            .iter()
            .map(|&i| b[i].as_f64().filter(|n| n.is_finite()))
            .collect();
        if let Some(v) = values {
            let label = if v == [-180., -90., 180., 90.] {
                "Global".into()
            } else {
                format!("{:.2}, {:.2} → {:.2}, {:.2}", v[0], v[1], v[2], v[3])
            };
            fact(
                "Bounds · CRS84",
                format!(
                    "<span title=\"{}\">{}</span>",
                    escape(&bbox.to_string()),
                    escape(&label)
                ),
            );
        }
    }
    if let Some(parameters) = doc["parameter_names"].as_object().filter(|p| !p.is_empty()) {
        let names: Vec<_> = parameters.keys().map(String::as_str).collect();
        let mut label = names.iter().take(4).copied().collect::<Vec<_>>().join(", ");
        if names.len() > 4 {
            label.push_str(&format!(" +{}", names.len() - 4));
        }
        fact(
            "Parameters",
            format!(
                "<span title=\"{}\">{}</span>",
                escape(&names.join(", ")),
                escape(&label)
            ),
        );
    } else if let Some(styles) = doc["styles"].as_array().filter(|s| !s.is_empty()) {
        let names: Vec<_> = styles
            .iter()
            .filter_map(|s| s["title"].as_str().or(s["id"].as_str()))
            .collect();
        let mut label = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        if names.len() > 3 {
            label.push_str(&format!(" +{}", names.len() - 3));
        }
        fact(
            "Styles",
            format!(
                "<span title=\"{}\">{}</span>",
                escape(&names.join(", ")),
                escape(&label)
            ),
        );
    }
    if let Some(n) = doc.get("numberItems") {
        fact("Items", value_html(n));
    }
    facts.push_str("</dl>");
    facts
}

pub fn collections_html(
    surface: Surface<'_>,
    query: &SearchQueryParams,
    search: &SearchParams,
    matched: usize,
    docs: &[CollectionView<'_>],
    nav: &[LinkView],
) -> String {
    let api = surface.api;
    let url = &format!("{}/collections", surface.root);
    let json_url = format!(
        "{url}{}",
        query.query_string_with_format(search.limit, search.offset, "json")
    );
    let query_string = query.query_string_with_format(search.limit, search.offset, "html");
    let pairs: Vec<_> =
        form_urlencoded::parse(query_string.trim_start_matches('?').as_bytes()).collect();
    let value = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_ref())
            .unwrap_or("")
    };
    let mut primary = String::new();
    let mut advanced = String::new();
    let mut paging = String::new();
    for parameter in CollectionParameter::ALL {
        let name = parameter.name();
        let current = value(name);
        match parameter {
            CollectionParameter::Format=>{},
            CollectionParameter::Q=>primary=format!("<div class=\"search-row\"><div class=\"search-main\"><label for=\"collection-search\">Search text <code>q</code></label><div class=\"search-input-wrap\">{}<input id=\"collection-search\" name=\"q\" value=\"{}\" placeholder=\"e.g. radar or GFS\" aria-describedby=\"collection-search-help\"></div></div><button class=\"btn primary\">Find collections {}</button></div><p id=\"collection-search-help\" class=\"search-hint\">Matches titles, descriptions and keywords. Commas separate alternatives.</p>",icon("search"),escape(current),icon("arrow")),
            CollectionParameter::Limit|CollectionParameter::Offset=>paging.push_str(&input(name,&if name=="limit"{search.limit}else{search.offset}.to_string(),"integer","number",false)),
            CollectionParameter::Bbox=>{
                let bounds:Vec<_>=current.split(',').collect();
                if current.is_empty()||bounds.len()==4 {
                    advanced.push_str(&format!("<div class=\"enhanced\"><label>bbox <span class=\"parameter-type\">number[4] · CRS84</span></label><input type=\"hidden\" data-param=\"bbox\" value=\"{}\"><div class=\"bbox-fields\">",escape(current)));
                    for (i,(label,hint)) in [("West","18"),("South","58"),("East","32"),("North","71")].iter().enumerate(){
                        advanced.push_str(&format!("<label>{label}<input data-bbox=\"{i}\" type=\"number\" step=\"any\" value=\"{}\" placeholder=\"{hint}\"></label>",escape(bounds.get(i).copied().unwrap_or_default())));
                    }
                    advanced.push_str("</div><p class=\"field-help\">CRS84 · leave blank for any area</p></div>");
                } else {advanced.push_str(&input(name,current,"Six-dimensional bounds · CRS84","text",true));}
            },
            CollectionParameter::BboxCrs if current.is_empty()=>{},
            _=>{
                let help=match parameter {CollectionParameter::Query=>"string · optional",CollectionParameter::Datetime=>"instant / interval · UTC",_=>"CRS84 coordinate reference system"};
                let mut field=input(name,current,help,"text",true);
                if matches!(parameter,CollectionParameter::Query) {field=field.replace("<span>query</span>","<span>Advanced expression <code>query</code></span>");}
                if matches!(parameter,CollectionParameter::Datetime) {field=field.replace("<span>datetime</span>","<span>Time coverage <code>datetime</code></span>");}
                if matches!(parameter,CollectionParameter::Query){advanced.insert_str(0,&format!("<div>{field}<p class=\"field-help\">+ requires a term · − excludes it · commas mean OR</p></div>"));}
                else {advanced.push_str(&field);}
            }
        }
        if !matches!(
            parameter,
            CollectionParameter::Q
                | CollectionParameter::Limit
                | CollectionParameter::Offset
                | CollectionParameter::Format
        ) && !current.is_empty()
        {
            advanced.push_str(&format!(
                "<noscript><input type=\"hidden\" name=\"{name}\" value=\"{}\"></noscript>",
                escape(current)
            ));
        }
    }
    let chips = pairs
        .iter()
        .filter(|(k, v)| ["q", "query", "bbox", "datetime"].contains(&k.as_ref()) && !v.is_empty())
        .map(|(k, v)| {
            anchor(
                &query_edit(&json_url, k, None),
                &format!("{k}: {v} ×"),
                "filter-chip",
            )
        })
        .collect::<String>();
    let form=format!("<form aria-label=\"Collection search\" class=\"search-panel query-form\" method=\"get\" action=\"{}\"><input type=\"hidden\" name=\"f\" value=\"html\">{primary}<details class=\"filters\" data-disclosure=\"collection-search\"><summary>Advanced search, area and time</summary><div class=\"filters-grid\">{advanced}</div><div class=\"paging-fields\">{paging}</div><div class=\"filter-controls\"><span class=\"field-help\">Collections with unknown extents remain eligible.</span>{}</div></details><div class=\"active-filters\">{chips}</div><details class=\"draft-request enhanced\" data-draft-disclosure><summary>Preview search request</summary><small>Draft · submit to update matches</small><code data-draft></code></details><noscript><p>Enable JavaScript to edit advanced parameters.</p></noscript></form>",escape(url),anchor(&with_format(url,"html"),"Clear filters","quiet"));
    let mut body = page_heading(
        "Collections",
        "Find a dataset, then open it to request data.",
    );
    body = body.replace(
        "OGC API · HTML REPRESENTATION",
        &format!("{} · DATA CATALOG", api.to_uppercase()),
    );
    let first = if docs.is_empty() {
        0
    } else {
        search.offset + 1
    };
    let last = if docs.is_empty() {
        0
    } else {
        search.offset + docs.len()
    };
    let range = if docs.is_empty() {
        "No collections on this page".to_owned()
    } else {
        format!("{first}–{last} shown")
    };
    let collection_noun = if matched == 1 {
        "collection"
    } else {
        "collections"
    };
    body.push_str(&format!("<div data-results-scope=\"{}\"></div><div class=\"query-workspace\"><aside class=\"query-builder\">{form}</aside><section class=\"query-results\" aria-label=\"Matching collections\"><div class=\"results-bar\"><div><strong>{matched} matching {collection_noun}</strong><small>{range} · Collection ID order</small></div><div class=\"view-switch enhanced\" aria-label=\"Result presentation\"><button data-view=\"list\" class=\"selected\" aria-pressed=\"true\">{}List</button><button data-view=\"cards\" aria-pressed=\"false\">{}Cards</button></div></div><div class=\"collection-list\">",escape(url),icon("list"),icon("grid")));
    if docs.is_empty() {
        if matched > 0 {
            let first_page = format!(
                "{url}{}",
                query.query_string_with_format(search.limit, 0, "html")
            );
            let match_verb = if matched == 1 { "matches" } else { "match" };
            body.push_str(&format!("<div class=\"empty-state\"><h2>This page is outside the results.</h2><p>{matched} {collection_noun} {match_verb}. Return to the first page to keep these filters.</p>{}</div>",anchor(&first_page,"Go to first page","btn primary")));
        } else {
            body.push_str(&format!("<div class=\"empty-state\"><h2>No collections match these filters.</h2><p>Try a broader search, expand the area or remove a time filter.</p>{}</div>",anchor(&with_format(url,"html"),"Clear filters","btn primary")));
        }
    }
    for view in docs {
        let doc = view.metadata;
        let id = doc["id"].as_str().unwrap_or_default();
        let href = format!("{url}/{}?f=html", path_segment(id));
        let keywords = doc["keywords"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(4)
            .map(|k| anchor(&query_edit(&json_url, "q", Some(k)), k, "chip"))
            .collect::<String>();
        let symbol = if doc["keywords"]
            .as_array()
            .is_some_and(|ks| ks.iter().any(|k| k == "radar"))
        {
            "radar"
        } else {
            "layers"
        };
        let title = doc["title"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(id);
        body.push_str(&format!("<article class=\"collection-row\"><div class=\"collection-icon\">{}</div><div class=\"collection-main\"><h3>{}</h3><span class=\"mono\">{}</span><p>{}</p>{}<div class=\"chip-row\">{keywords}</div>{}</div><a class=\"row-arrow\" href=\"{}\" aria-label=\"Open {}\">{}</a></article>",icon(symbol),anchor(&href,title,""),escape(id),escape(doc["description"].as_str().unwrap_or_default()),collection_facts(doc),license_html(view.license),escape(&href),escape(title),icon("arrow")));
    }
    body.push_str("</div><div class=\"pagination\">");
    body.push_str("<label class=\"per-page enhanced\">Per page<select data-page-size>");
    let mut sizes = vec![6, 12, 24, search.limit];
    sizes.sort_unstable();
    sizes.dedup();
    for n in sizes {
        body.push_str(&format!(
            "<option value=\"{n}\" {}>{n}</option>",
            if n == search.limit { "selected" } else { "" }
        ));
    }
    let page = if matched == 0 {
        "0 results".into()
    } else if docs.is_empty() {
        format!("No page at offset {}", search.offset)
    } else if !search.offset.is_multiple_of(search.limit) {
        format!("{first}–{last} of {matched}")
    } else {
        format!(
            "Page {} of {}",
            search.offset / search.limit + 1,
            matched.div_ceil(search.limit)
        )
    };
    body.push_str(&format!(
        "</select></label><span class=\"page-indicator\">{page}</span>"
    ));
    body.push_str(&pagination(nav));
    body.push_str("</div></section></div>");
    Page {
        surface,
        title: "Collections",
        json_url: &json_url,
    }
    .render(&body, "")
}

pub fn pagination(nav: &[LinkView]) -> String {
    let mut body = String::from("<nav class=\"pagination\" aria-label=\"Pagination\">");
    for rel in ["prev", "next"] {
        if let Some(link) = nav.iter().find(|l| l.rel == rel) {
            body.push_str(&format!(
                "<a class=\"btn\" rel=\"{rel}\" href=\"{}\">{}</a>",
                escape(safe_href(&link.href)),
                if rel == "prev" {
                    "← Previous page"
                } else {
                    "Next page →"
                }
            ));
        } else {
            body.push_str(&format!(
                "<button class=\"btn small\" disabled>{}</button>",
                if rel == "prev" {
                    "← Previous page"
                } else {
                    "Next page →"
                }
            ));
        }
    }
    body.push_str("</nav>");
    body
}

/// Model-run navigation uses the same shell without claiming collection search.
pub fn instances_html(
    surface: Surface<'_>,
    title: &str,
    cards: &[ds_core::html::CollectionCard],
    nav: &[LinkView],
) -> String {
    let url = nav
        .iter()
        .find(|l| l.rel == "alternate")
        .map(|l| l.href.as_str())
        .unwrap_or_default();
    let mut body = page_heading(title, "Available forecast model runs.");
    for link in nav.iter().filter(|l| l.rel == "collection") {
        body.push_str(&anchor(
            &with_format(&link.href, "html"),
            "← Collection metadata",
            "back-link",
        ));
    }
    body.push_str("<div class=\"collection-list\">");
    for card in cards {
        body.push_str(&format!(
            "<article class=\"collection-row\"><div class=\"collection-main\"><h2>{}</h2><code>{}</code><p>{}</p></div></article>",
            anchor(&with_format(&card.self_href, "html"), &card.title, ""),
            escape(&card.id),
            escape(&card.description)
        ));
    }
    if cards.is_empty() {
        body.push_str("<p class=\"panel\">No model runs available.</p>");
    }
    body.push_str("</div>");
    Page {
        surface,
        title,
        json_url: url,
    }
    .render_with_breadcrumbs(
        &body,
        "",
        &nav.iter()
            .filter(|l| l.rel == "collection")
            .filter_map(|l| Some((l.href.as_str(), l.title.as_deref()?)))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS_PROXY: Surface<'static> = Surface {
        base: "https://example.test/proxy",
        root: "https://example.test/proxy/maps",
        api: "maps",
    };

    /// Render a collection page for an API at its per-API mount below `base`.
    fn collection(base: &str, api: &str, doc: &Value, license: Option<&LicenseConfig>) -> String {
        let root = format!("{base}/{api}");
        collection_html(
            Surface {
                base,
                root: &root,
                api,
            },
            doc,
            license,
        )
    }

    #[test]
    fn catalog_empty_page_keeps_filters_and_does_not_claim_zero_matches() {
        let query = SearchQueryParams::from_pairs(vec![
            ("q".into(), "radar & snow".into()),
            ("query".into(), "+rain -test".into()),
            ("limit".into(), "12".into()),
            ("offset".into(), "1000".into()),
        ])
        .unwrap();
        let search = query.parse().unwrap();
        let url = "https://example.test/proxy/maps/collections";
        let html = collections_html(MAPS_PROXY, &query, &search, 3, &[], &[]);
        assert!(html.contains("3 matching collections"));
        assert!(html.contains("This page is outside the results."));
        assert!(html.contains("No page at offset 1000"));
        assert!(!html.contains("0–1000"));
        assert!(!html.contains("Page 84 of 1"));
        assert!(!html.contains("No collections match these filters."));
        let first = format!("{url}{}", query.query_string_with_format(12, 0, "html"));
        assert!(html.contains(&format!("href=\"{}\">Go to first page", escape(&first))));
        let empty = collections_html(MAPS_PROXY, &query, &search, 0, &[], &[]);
        assert!(empty.contains("No collections match these filters."));
        assert!(empty.contains("0 results"));
        assert!(!empty.contains("Go to first page"));
    }

    #[test]
    fn catalog_time_summaries_keep_utc_precision_and_only_advertised_resolution() {
        let temporal =
            json!({"interval":[["2026-03-25T01:15:05+02:00","2026-03-25T01:35:05+02:00"]]});
        let time = collection_time_html(&temporal);
        assert!(time.contains("2026-03-24"));
        assert!(time.contains("23:15:05–23:35:05 UTC"));
        let mut doc = json!({"extent":{"temporal":temporal},"parameter_names":{"rain & snow":{}}});
        let facts = collection_facts(&doc);
        assert!(!facts.contains("Time resolution"));
        assert!(!facts.contains("Time steps"));
        assert!(facts.contains("rain &amp; snow"));
        doc["extent"]["temporal"]["grid"] = json!({"resolution":"PT5M","cellsCount":5});
        assert!(collection_facts(&doc).contains("Every 5 min"));
        let open = json!({"interval":[[null,"2026-03-25T00:00:00Z"]]});
        assert!(collection_time_html(&open).contains("Open start"));
        assert_eq!(collection_time_html(&Value::Null), "Not advertised");
    }

    #[test]
    fn time_choices_respect_regular_and_irregular_grids_without_inventing_samples() {
        let mut temporal = json!({"interval":[["2026-09-18T00:00:00Z","2026-09-18T06:00:00Z"]]});
        assert!(map_times(&temporal).is_empty());
        temporal["grid"] = json!({"cellsCount":3,"resolution":"PT3H"});
        let times = map_times(&temporal);
        assert_eq!(times.len(), 3);
        assert_eq!(times[1], "2026-09-18T03:00:00+00:00");
        temporal["grid"] = json!({"cellsCount":3,"coordinates":["2026-09-18T00:00:00Z","2026-09-18T01:00:00Z","2026-09-18T06:00:00Z"]});
        assert_eq!(map_times(&temporal)[1], "2026-09-18T01:00:00+00:00");
        temporal["grid"] = json!({"cellsCount":10001,"resolution":"PT1S"});
        assert!(map_times(&temporal).is_empty());
        temporal["grid"] = json!({"cellsCount":3,"resolution":"P1D"});
        assert!(map_times(&temporal).is_empty());
        temporal["grid"] = json!({"cellsCount":2,"coordinates":["invalid","2026-09-18T01:00:00Z"]});
        assert!(map_times(&temporal).is_empty());
    }

    #[test]
    fn vertical_dimensions_are_visible_in_catalog_overview_and_map_controls() {
        use ds_core::vertical::VerticalKind;
        for vertical in [
            json!({"interval":[[100,1000]],"values":[1000,850,100],"unit":"hPa"}),
            json!({"interval":[["100","1000"]],"values":["1000","850","100"],"vrs":VerticalKind::Pressure.vrs()}),
            json!({"interval":[["1","137"]],"values":["137","1"],"vrs":VerticalKind::ModelLevel.vrs()}),
        ] {
            let doc = json!({"id":"levels","extent":{"vertical":vertical},"links":[
                {"rel":"self","href":"https://example.test/maps/collections/levels"},
                {"rel":"map","href":"https://example.test/maps/collections/levels/map"}]});
            assert!(collection_facts(&doc).contains("Vertical dimension"));
            for api in ["edr", "maps", "tiles"] {
                let html = collection("https://example.test", api, &doc, None);
                let overview = html.split("id=\"metadata\"").next().unwrap();
                assert!(overview.contains("Vertical dimension"));
                assert!(overview.contains("Available levels · z"));
                if api == "maps" {
                    assert!(overview.contains("id=\"map-level\""));
                }
                if vertical["vrs"] == VerticalKind::ModelLevel.vrs() {
                    assert!(overview.contains("Model level: 1–137"));
                } else {
                    assert!(overview.contains("100–1000 hPa"));
                    assert!(overview.contains("1000, 850, 100"));
                }
            }
        }
        let doc =
            json!({"id":"single","links":[{"rel":"map","href":"/maps/collections/single/map"}]});
        assert!(!collection_facts(&doc).contains("Vertical dimension"));
        // The script contains the selector name, so assert the actual element.
        assert!(!collection("", "maps", &doc, None).contains("id=\"map-level\""));
        let malicious = json!({"interval":[[0,1]],"values":[0,"<script>","NaN",1],"unit":"<img onerror=alert(1)>"});
        assert!(!vertical_coverage(&malicious).contains("<img"));
        assert!(vertical_coverage(&malicious).contains("&lt;img"));
        assert_eq!(vertical_values(&malicious), ["0", "1"]);
    }

    #[test]
    fn rich_collection_metadata_and_escaped_breadcrumb_titles_remain_available() {
        let base = "https://example.test/proxy";
        let href = format!("{base}/maps/collections/a%2Bb");
        let doc = json!({"id":"a+b","title":"Forecast & <wind>","keywords":["wind","global"],
            "extent":{"vertical":{"interval":[[100,1000]],"values":[100,500,1000],"unit":"hPa"}},
            "custom":{"nested":{"value":false}},
            "links":[{"rel":"self","href":href},{"rel":"license","title":"License","href":"https://example.test/license"}]});
        let license = LicenseConfig {
            title: "Use with attribution".into(),
            url: None,
        };
        let html = collection(base, "maps", &doc, Some(&license));
        let crumbs = html
            .split("id=\"breadcrumbs\"")
            .nth(1)
            .unwrap()
            .split("</nav>")
            .next()
            .unwrap();
        assert!(crumbs.contains("Forecast &amp; &lt;wind&gt;"));
        assert!(crumbs.contains("/a%2Bb?f=html"));
        assert!(html.contains("Use with attribution"));
        assert!(html.contains("https://example.test/license"));
        for value in [
            "vertical", "1000", "hPa", "nested", "false", "wind", "global",
        ] {
            assert!(html.contains(value), "missing {value}");
        }
        assert!(html.contains("Storage coordinate system</dt><dd>Not advertised</dd>"));
    }

    #[test]
    fn format_switch_retains_duplicate_predicates_operators_and_fragment() {
        let href="https://example.test/base/features/collections/a/items?name=a%26b+%2B%C3%A9&name=&query=%2Bradar+-volume&offset=20&f=html#results";
        let switched = with_format(href, "json");
        assert!(switched.ends_with("#results"));
        let pairs: Vec<_> = form_urlencoded::parse(
            switched
                .split_once('?')
                .unwrap()
                .1
                .split('#')
                .next()
                .unwrap()
                .as_bytes(),
        )
        .collect();
        assert_eq!(
            pairs
                .iter()
                .filter(|(k, _)| k == "name")
                .map(|(_, v)| v.as_ref())
                .collect::<Vec<_>>(),
            ["a&b +é", ""]
        );
        assert!(pairs
            .iter()
            .any(|(k, v)| k == "query" && v == "+radar -volume"));
        assert!(pairs.iter().any(|(k, v)| k == "offset" && v == "20"));
        assert_eq!(
            pairs
                .iter()
                .filter(|(k, _)| k == "f")
                .map(|(_, v)| v.as_ref())
                .collect::<Vec<_>>(),
            ["json"]
        );
    }

    #[test]
    fn maps_preview_uses_advertised_endpoints_and_retains_metadata_representation() {
        let base = "https://example.test/prefix";
        let doc = json!({"id":"a","extent":{"spatial":{"bbox":[[20,60,0,30,70,100]]}},"links":[
            {"rel":"self","href":format!("{base}/maps/collections/a")},
            {"rel":"map","href":format!("{base}/maps/collections/a/map")}
        ],"styles":[{"title":"Rain & snow","links":[{"rel":"map","href":format!("{base}/maps/collections/a/styles/rain/map")}]},{"title":"Unsafe","links":[{"rel":"map","href":"javascript:alert(1)"}]}]});
        let html = collection(base, "maps", &doc, None);
        let data = html
            .split("id=\"map-data\" hidden>")
            .nth(1)
            .unwrap()
            .split("</div>")
            .next()
            .unwrap()
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        let data: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(data["mapRequest"]["styles"].as_array().unwrap().len(), 2);
        assert_eq!(
            data["mapRequest"]["styles"][1]["href"],
            format!("{base}/maps/collections/a/styles/rain/map")
        );
        assert_eq!(
            data["features"][0]["geometry"]["coordinates"][0][2],
            json!([30., 70.])
        );
        assert!(html.contains(&format!(
            "id=\"json-link\" href=\"{base}/maps/collections/a?f=json\""
        )));
        // The preview follows the advertised map link, whichever API serves the page.
        assert!(collection(base, "features", &doc, None).contains("id=\"map-controls\""));
        let mut no_map = doc.clone();
        no_map["links"]
            .as_array_mut()
            .unwrap()
            .retain(|l| l["rel"] != "map");
        assert!(!collection(base, "maps", &no_map, None).contains("id=\"map-controls\""));
    }

    #[test]
    fn navigation_and_map_docs_follow_the_serving_and_linked_apis() {
        let base = "https://example.test";
        let doc = json!({"id":"a","links":[
            {"rel":"self","href":"https://example.test/features/collections/a"},
            {"rel":rel::MAP,"href":"https://example.test/maps/collections/a/map"}]});
        // A page served at a relocated mount keeps its own API root active.
        let html = collection_html(
            Surface {
                base,
                root: "https://example.test/relocated/features",
                api: "features",
            },
            &doc,
            None,
        );
        assert!(html.contains("value=\"https://example.test/relocated/features/?f=html\" selected"));
        assert!(html.contains("value=\"https://example.test/maps/?f=html\""));
        // Map request parameters are documented by the API serving the map.
        assert!(html.contains("href=\"https://example.test/maps/api/docs\">Map request parameters"));
    }

    #[test]
    fn request_section_lists_tilesets_next_to_the_map_preview() {
        let doc = json!({"id":"a","links":[
            {"rel":"self","href":"https://x/collections/a"},
            {"rel":rel::MAP,"href":"https://x/collections/a/map"},
            {"rel":rel::TILESETS_MAP,"href":"https://x/collections/a/map/tiles","type":"application/json","title":"Map tilesets"},
            {"rel":rel::TILESETS_VECTOR,"href":"https://x/collections/a/tiles","type":"application/json","title":"Vector tilesets"}]});
        let surface = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let html = collection_html(surface, &doc, None);
        let request = html
            .split("<h2>Request data from this collection</h2>")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap();
        assert!(request.contains("Map request parameters"));
        assert!(request.contains("https://x/collections/a/map/tiles"));
        assert!(request.contains("https://x/collections/a/tiles"));
    }

    #[test]
    fn resource_links_list_each_target_once() {
        let tiles = "https://x/tiles/collections/a/tiles";
        let doc = json!({"links":[
            {"rel":"conformance","href":"https://x/maps/conformance","type":"application/json","title":"Conformance"},
            {"rel":rel::CONFORMANCE,"href":"https://x/maps/conformance","type":"application/json","title":"Conformance"},
            {"rel":"map","href":"https://x/maps/collections/a/map","type":"image/png"},
            {"rel":"map","href":"https://x/maps/collections/a/map","type":"image/jpeg"},
            {"rel":"tiles","href":tiles,"type":"application/json","title":"Tilesets"},
            {"rel":rel::TILESETS_MAP,"href":tiles,"type":"application/json","title":"Map tilesets"},
            {"rel":rel::TILESETS_VECTOR,"href":tiles,"type":"application/json","title":"Vector tilesets"}
        ]});
        let html = document_links(&doc);
        assert_eq!(html.matches("maps/conformance?f=html").count(), 1);
        assert!(!html.contains(rel::CONFORMANCE));
        // Distinct representations of one resource remain separate entries.
        assert_eq!(html.matches("image/").count(), 2);
        // Differently titled offers of one resource keep their relations.
        for title in ["Tilesets", "Map tilesets", "Vector tilesets"] {
            assert!(
                html.contains(&format!("<strong>{title}</strong>")),
                "{title}"
            );
        }
        assert!(html.contains(rel::TILESETS_VECTOR));
    }

    #[test]
    fn shell_and_metadata_escape_content_and_retain_proxy_prefix() {
        let attack = "</script><script>alert(1)</script>";
        let doc = json!({"id":"a","title":attack,"description":attack,"custom":{"nested":attack},"links":[{"rel":"self","href":"https://example.test/prefix/maps/collections/a"},{"rel":"license","href":"javascript:alert(1)","title":attack}]});
        let html = collection("https://example.test/prefix", "maps", &doc, None);
        assert!(!html.contains(attack));
        assert!(html.contains(&escape(attack)));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("https://example.test/prefix/maps/collections/a?f=json"));
        assert!(html.contains("https://example.test/prefix/edr/?f=html"));
        assert!(html.contains("nested"));
        assert!(value_html(&json!([false, 0, null])).contains(">false</span>"));
        assert!(value_html(&json!([false, 0, null])).contains(">0</span>"));
    }
}
