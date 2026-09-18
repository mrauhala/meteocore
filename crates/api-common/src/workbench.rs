//! Shared, server-rendered HTML representations of API resources.
//!
//! JSON remains the resource contract. Adapters supply the same metadata used
//! for JSON; this module supplies only presentation and progressive enhancement.
use ds_core::collection_search::{CollectionParameter, SearchParams, SearchQueryParams};
use ds_core::config::LicenseConfig;
use ds_core::html::{escape, LinkView};
use serde_json::{json, Value};

const CSS: &str = include_str!("workbench/style.css");
const SCRIPT: &str = include_str!("workbench/app.js");
const THEME: &str = include_str!("workbench/theme.js");
const APIS: &[(&str, &str)] = &[
    ("edr", "EDR"),
    ("features", "Features"),
    ("maps", "Maps"),
    ("tiles", "Tiles"),
];

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
    pub base: &'a str,
    pub api: &'a str,
    pub title: &'a str,
    pub json_url: &'a str,
}
impl Page<'_> {
    /// `body` and `head` must be trusted markup built from escaped values.
    pub fn render(&self, body: &str, head: &str) -> String {
        let Self {
            base,
            api,
            title,
            json_url,
        } = self;
        let html_url = with_format(json_url, "html");
        let current_path = json_url.split('?').next().unwrap_or(json_url);
        let api_title = APIS
            .iter()
            .find(|(id, _)| id == api)
            .map_or("MeteoCore", |(_, name)| *name);
        let api_nav = APIS
            .iter()
            .map(|(id, name)| {
                anchor(
                    &format!("{base}/{id}/?f=html"),
                    name,
                    if id == api { "active" } else { "" },
                )
            })
            .collect::<String>();
        let workspace_api = if api.is_empty() { "features" } else { api };
        let options = APIS
            .iter()
            .map(|(id, name)| {
                format!(
                    "<option value=\"{}\" {}>{name}</option>",
                    escape(&format!("{base}/{id}/?f=html")),
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
                let path = format!("{base}/{workspace_api}{suffix}");
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
        if !api.is_empty() {
            crumbs.push_str(&format!(
                "<span>/</span>{}",
                anchor(&format!("{base}/{api}/?f=html"), api_title, "")
            ));
            let prefix = format!("{base}/{api}/");
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
                    crumbs.push_str(&format!(
                        "<span>/</span>{}",
                        anchor(&with_format(&path, "html"), &decoded, "")
                    ));
                }
            }
        }
        let json_type = if *api == "features"
            && current_path
                .strip_prefix(&format!("{base}/features/collections/"))
                .and_then(|rest| rest.split('/').nth(1))
                == Some("items")
        {
            "application/geo+json"
        } else {
            "application/json"
        };
        let curl = format!("curl --get '{}'", json_url.replace('\'', "'\"'\"'"));
        format!(
            r##"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title} · MeteoCore</title><link rel="alternate" type="{json_type}" href="{json_url}"><style>{CSS}</style><script>{THEME}</script>{head}</head>
<body><a class="skip" href="#main">Skip to content</a><div class="shell"><aside class="sidebar" aria-label="API navigation"><a class="brand" href="{base}/?f=html"><span class="brand-mark"><svg viewBox="0 0 32 32" fill="none" aria-hidden="true"><circle cx="16" cy="16" r="11"/><circle cx="16" cy="16" r="6"/><path d="M16 16 27 5M16 3v3M3 16h3M16 26v3M26 16h3"/></svg></span><span>MeteoCore<small>API WORKBENCH</small></span></a><div class="api-switch enhanced"><label for="api-select">API workspace</label><select id="api-select">{options}</select></div><noscript><nav aria-label="APIs">{api_nav}</nav></noscript><nav id="primary-nav" aria-label="Workspace">{nav}</nav><div class="sidebar-note"><span class="eyebrow">OPEN STANDARDS</span><p>One data platform.<br>Different ways to explore.</p><a href="{base}/?f=html">Explore all APIs ↗</a></div><div class="sidebar-footer">MeteoCore<small>OGC API · Developer workspace</small></div></aside>
<div class="workspace"><header class="topbar"><nav id="breadcrumbs" aria-label="Breadcrumb">{crumbs}</nav><div class="topbar-actions"><button class="quiet enhanced" id="help-button">Help</button><label class="theme-label enhanced" for="theme">Theme<select id="theme"><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label><nav class="representation-switch" aria-label="Representation"><a aria-current="page" href="{html_url}">HTML</a><a rel="alternate" id="json-link" href="{json_url}">JSON</a></nav></div></header>
<section id="request-context" aria-label="Current API request"><div class="request-identity"><span class="method">GET</span><code id="request-url">{json_url}</code></div><div class="request-tools"><span class="request-note">Applied request · JSON representation</span><button class="btn small enhanced" data-copy="{json_url}">Copy URL</button><button class="btn small enhanced" data-copy="{curl}">Copy cURL</button><a class="btn small" href="{json_url}">Open JSON ↗</a></div></section>
<main id="main" tabindex="-1">{body}</main><footer class="page-footer"><span>MeteoCore · Open weather data</span><span>HTML representation · Times in UTC</span></footer></div></div><dialog id="help-dialog"><div class="dialog-head"><h2>Find a collection, then request data</h2><button class="icon-button" id="close-help" aria-label="Close help">×</button></div><div class="panel-body"><p><strong>1. Find a collection.</strong> Collection search matches titles, descriptions and keywords. Commas separate alternatives; words form a phrase.</p><p>Advanced search adds required (+) and excluded (-) terms. Area and time narrow coverage; unknown extents remain eligible.</p><p><strong>2. Request data.</strong> Open a collection to see its data operations. Feature filters select items within that collection; discovery filters are not copied into the data request. The JSON switch always opens the current resource with its applied filters and paging.</p></div></dialog><div id="toast" role="status" aria-live="polite"></div><script>{SCRIPT}</script></body></html>"##,
            title = escape(title),
            json_url = escape(safe_href(json_url)),
            html_url = escape(safe_href(&html_url)),
            base = escape(base),
            curl = escape(&curl)
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
    if let Some(links) = doc["links"].as_array() {
        for link in links {
            let href = link["href"].as_str().unwrap_or_default();
            let rel = link["rel"].as_str().unwrap_or_default();
            if matches!(rel, "self" | "alternate") {
                continue;
            }
            let label = link["title"].as_str().unwrap_or(rel);
            let human = matches!(rel, "data" | "child" | "collection" | "conformance" | "up")
                || rel == "items" && href.contains("/features/");
            let target = if human {
                with_format(href, "html")
            } else {
                href.to_owned()
            };
            body.push_str(&format!(
                "<div class=\"endpoint\"><div>{}<code>{}</code><p>{} · {}</p></div></div>",
                anchor(&target, label, "resource-title"),
                escape(href),
                escape(rel),
                escape(link["type"].as_str().unwrap_or("Linked resource"))
            ));
        }
    }
    body.push_str("</div>");
    body
}

pub fn landing_html(
    base: &str,
    api: &str,
    title: &str,
    description: &str,
    links: &[LinkView],
) -> String {
    let doc = json!({"links":links.iter().map(|l|json!({"href":l.href,"rel":l.rel,"title":l.title})).collect::<Vec<_>>()});
    landing_document(base, api, title, description, &doc)
}

pub fn landing_document(
    base: &str,
    api: &str,
    title: &str,
    description: &str,
    doc: &Value,
) -> String {
    let url = format!(
        "{base}/{}?f=json",
        if api.is_empty() {
            String::new()
        } else {
            format!("{api}/")
        }
    );
    let discovery = if api.is_empty() { "features" } else { api };
    let mut body = page_heading(title, description);
    body.push_str("<section class=\"panel\"><div class=\"panel-head\"><h2>Resources</h2></div><div class=\"resource-table\">");
    for link in doc["links"].as_array().into_iter().flatten() {
        let rel = link["rel"].as_str().unwrap_or_default();
        let href = link["href"].as_str().unwrap_or_default();
        let primary = if api.is_empty() {
            rel == "child"
                && APIS
                    .iter()
                    .any(|(id, _)| href.trim_end_matches('/').ends_with(&format!("/{id}")))
        } else {
            matches!(rel, "data" | "conformance" | "service-desc")
        };
        if !primary {
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
        _ => "Choose an API and a collection first. Then request features, environmental values, map images or tiles using that collection's supported operations.",
    };
    body.push_str(&format!(r#"<div class="developer-start"><section class="panel panel-body"><span class="eyebrow">COLLECTION DISCOVERY</span><h2>1. Find a collection</h2><form method="get" action="{base}/{discovery}/collections"><input type="hidden" name="f" value="html"><label for="q">Search collections <span class="parameter-type">q · optional</span></label><div class="search-row"><input id="q" name="q" placeholder="radar"><button class="btn primary">Find collections {arrow}</button></div><p class="field-help">Search dataset titles, descriptions and keywords. Area and time filters in the catalog narrow the collection coverage.</p></form></section><section class="panel panel-body"><span class="eyebrow">DATA ACCESS</span><h2>2. Request data</h2><p class="section-note">{data_guidance}</p><p class="field-help">Select a collection to see the available data requests. Discovery filters are not carried over as data filters.</p></section></div>"#,base=escape(base),arrow=icon("arrow")));
    body.push_str("<section class=\"section-space\"><div class=\"section-header\"><h2>API definitions &amp; resource links</h2></div>");
    body.push_str(&document_links(doc));
    body.push_str("</section>");
    Page {
        base,
        api,
        title,
        json_url: &url,
    }
    .render(&body, "")
}

pub fn conformance_html(base: &str, api: &str, classes: &[&str], nav: &[LinkView]) -> String {
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
        base,
        api,
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

pub fn map_html(base: &str, features: &Value, quicklook: bool) -> String {
    format!("<div class=\"map-panel\"><div id=\"feature-map\" data-land=\"{base}/preview/vendor/workbench-land.json\" data-quicklook=\"{quicklook}\" role=\"region\" aria-label=\"Feature geometry map\"></div><p id=\"map-status\">Map requires JavaScript and WebGL; coordinates remain available below.</p><div id=\"map-data\" hidden>{}</div></div><script src=\"{base}/preview/vendor/maplibre-gl.js\"></script><script>{}</script>",escape(&features.to_string()),include_str!("workbench/map.js"),base=escape(base))
}

pub fn collection_html(
    base: &str,
    api: &str,
    doc: &Value,
    license: Option<&LicenseConfig>,
) -> String {
    let title = doc["title"]
        .as_str()
        .or(doc["id"].as_str())
        .unwrap_or("Collection");
    let id = doc["id"].as_str().unwrap_or_default();
    let links = doc["links"].as_array();
    let href = links
        .and_then(|ls| ls.iter().find(|l| l["rel"] == "self"))
        .and_then(|l| l["href"].as_str())
        .unwrap_or_default();
    let url = with_format(href, "json");
    let catalog = format!("{base}/{api}/collections");
    let items = links
        .and_then(|ls| ls.iter().find(|l| l["rel"] == "items"))
        .and_then(|l| l["href"].as_str())
        .filter(|_| api == "features");
    let kind = doc["itemType"]
        .as_str()
        .or(doc["dataType"].as_str())
        .unwrap_or("Environmental data");
    let mut body=format!("<a class=\"back-link\" data-back-scope=\"{}\" href=\"{}\">← Back to collections</a><div class=\"detail-heading\"><div class=\"chip-row\"><span class=\"chip teal\">{}</span><span class=\"chip\">{}</span></div><h1>{}</h1><div class=\"mono\">{}</div><p>{}</p></div><nav class=\"detail-tabs\" aria-label=\"Collection views\"><a data-collection-tab=\"overview\" class=\"active\" href=\"#overview\">Overview</a>",escape(&catalog),escape(&with_format(&catalog,"html")),escape(api),escape(kind),escape(title),escape(id),escape(doc["description"].as_str().unwrap_or_default()));
    if let Some(items) = items {
        body.push_str(&anchor(&with_format(items, "html"), "Request data", ""));
    }
    body.push_str("<a data-collection-tab=\"metadata\" href=\"#metadata\">Metadata &amp; links</a></nav><section id=\"overview\" class=\"collection-view\" data-collection-view><div class=\"detail-layout\"><div><section class=\"panel\"><div class=\"panel-head\"><h2>Spatial &amp; temporal coverage</h2><span class=\"chip\">CRS84</span></div>");
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
        body.push_str(&map_html(base,&json!({"type":"FeatureCollection","features":[{"type":"Feature","geometry":geometry,"properties":{"label":"Advertised collection extent"}}]}),false));
    } else {
        body.push_str("<div class=\"empty-state\"><p>Spatial extent not specified.</p></div>");
    }
    body.push_str(&format!("<div class=\"coverage-facts\"><div class=\"fact\"><small>Start · UTC</small><strong>{}</strong></div><div class=\"fact\"><small>End · UTC</small><strong>{}</strong></div><div class=\"fact wide\"><small>Advertised bounds · west, south, east, north</small><strong class=\"mono\">{}</strong></div></div></section><section class=\"section-space\"><h2>Request data from this collection</h2>",value_html(&interval[0]),value_html(&interval[1]),value_html(bbox)));
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
                    format!("{base}/edr/api/docs")
                };
                body.push_str(&format!("<div class=\"endpoint\"><div><strong>{} query</strong><code>{}</code><p>{}</p></div>{}</div>",escape(name),escape(href),value_html(&query["link"]["variables"]["output_formats"]),anchor(&target,if name=="instances"{"Browse runs →"}else{"API docs ↗"},"btn small")));
            }
        }
        body.push_str("</div>");
    }
    if matches!(api, "maps" | "tiles") {
        body.push_str(&document_links(doc));
        if api == "maps" {
            body.push_str(&anchor(
                &format!("{base}/preview"),
                "Open map preview ↗",
                "btn primary",
            ));
        }
    }
    body.push_str("</section></div><aside class=\"aside-stack\"><section class=\"panel\"><div class=\"panel-head\"><h2>Collection details</h2></div><div class=\"panel-body\"><dl class=\"definition\">");
    body.push_str(&format!(
        "<dt>Resource</dt><dd>{}</dd><dt>Coordinate system</dt><dd>{}</dd>",
        escape(kind),
        escape(doc["storageCrs"].as_str().unwrap_or("CRS84"))
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
    body.push_str("</div></div></div></section><section class=\"panel\"><div class=\"panel-head\"><h2>Resource links</h2></div><div class=\"panel-body\">");
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
        base,
        api,
        title,
        json_url: &url,
    }
    .render(&body, &head)
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

pub fn collections_html(
    url: &str,
    query: &SearchQueryParams,
    search: &SearchParams,
    matched: usize,
    docs: &[CollectionView<'_>],
    nav: &[LinkView],
) -> String {
    let api_root = url.strip_suffix("/collections").unwrap_or(url);
    let (base, api) = api_root.rsplit_once('/').unwrap_or(("", api_root));
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
            CollectionParameter::Q=>primary=format!("<div class=\"search-row\"><div class=\"search-main\"><label for=\"collection-search\">q <span class=\"parameter-type\">string · optional</span></label><div class=\"search-input-wrap\">{}<input id=\"collection-search\" name=\"q\" value=\"{}\" placeholder=\"radar\"></div></div><button class=\"btn primary\">Find collections {}</button></div><p class=\"search-hint\">Titles, descriptions and keywords.<span>Try {} or {}</span></p>",icon("search"),escape(current),icon("arrow"),anchor(&query_edit(&json_url,"q",Some("radar")),"radar","quiet"),anchor(&query_edit(&json_url,"q",Some("observations")),"observations","quiet")),
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
                let field=input(name,current,help,"text",true);
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
    let form=format!("<form class=\"search-panel query-form\" method=\"get\" action=\"{}\"><input type=\"hidden\" name=\"f\" value=\"html\"><div class=\"builder-heading\"><h2>Collection search</h2><span class=\"mono\">GET</span></div>{primary}<details class=\"filters\" open><summary>Advanced search, area and time</summary><div class=\"filters-grid\">{advanced}</div><div class=\"paging-fields\">{paging}</div><div class=\"filter-controls\"><span class=\"field-help\">Collections with unknown extents remain eligible.</span>{}</div></details><div class=\"active-filters\">{chips}</div><div class=\"draft-request enhanced\"><small>Collection search request · submit to update matches</small><code data-draft></code></div><noscript><p>Enable JavaScript to edit advanced parameters.</p></noscript></form>",escape(url),anchor(&with_format(url,"html"),"Clear filters","quiet"));
    let mut body = page_heading(
        "Collections",
        "Find datasets by their metadata and coverage. Select a collection to build a data request.",
    );
    body=body.replace("OGC API · HTML REPRESENTATION",&format!("{} · DATA CATALOG",api.to_uppercase())).replacen("</div></div>",&format!("</div><div class=\"action-row\"><button class=\"btn enhanced\" data-copy=\"{}\">Copy API URL ↗</button></div></div>",escape(&json_url)),1);
    let first = if docs.is_empty() {
        0
    } else {
        search.offset + 1
    };
    let last = search.offset + docs.len();
    body.push_str(&format!("<div data-results-scope=\"{}\"></div><div class=\"query-workspace\"><aside class=\"query-builder\">{form}</aside><section class=\"query-results\" aria-label=\"Matching collections\"><div class=\"results-bar\"><div><strong>{matched} matching collections</strong><small>{first}–{last} shown · Collection ID order</small></div><div class=\"view-switch enhanced\" aria-label=\"Result presentation\"><button data-view=\"list\" class=\"selected\" aria-pressed=\"true\">{}List</button><button data-view=\"cards\" aria-pressed=\"false\">{}Cards</button></div></div><div class=\"collection-list\">",escape(url),icon("list"),icon("grid")));
    if docs.is_empty() {
        body.push_str(&format!("<div class=\"empty-state\"><span class=\"empty-icon\">{}</span><h2>No collections match these filters.</h2><p>Try a broader search, expand the area or remove a time filter.</p>{}</div>",icon("search"),anchor(&with_format(url,"html"),"Clear filters","btn primary")));
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
        let kind = match api {
            "features" => "Features",
            "maps" => "Map layer",
            "tiles" => "Tileset",
            _ => "Environmental data",
        };
        let interval = &doc["extent"]["temporal"]["interval"][0];
        let times = match (interval[0].as_str(), interval[1].as_str()) {
            (Some(a), Some(b)) => format!(
                "{} – {}",
                a.chars().take(10).collect::<String>(),
                b.chars().take(10).collect::<String>()
            ),
            _ => "Time extent unspecified".into(),
        };
        let count = doc
            .get("numberItems")
            .map(|n| format!("{} items", value_html(n)))
            .unwrap_or_else(|| {
                if interval.is_array() {
                    "Time-aware"
                } else {
                    "Spatial data"
                }
                .into()
            });
        let symbol = if doc["keywords"]
            .as_array()
            .is_some_and(|ks| ks.iter().any(|k| k == "radar"))
        {
            "radar"
        } else {
            "layers"
        };
        body.push_str(&format!("<article class=\"collection-row\"><div class=\"collection-icon\">{}</div><div><h3>{}</h3><span class=\"mono\">{}</span><p>{}</p><div class=\"chip-row\">{keywords}</div>{}</div><div class=\"collection-side\"><span class=\"chip teal\">{kind}</span><span>{count}</span><small>{}</small></div><a class=\"row-arrow\" href=\"{}\" aria-label=\"Open {}\">{}</a></article>",icon(symbol),anchor(&href,doc["title"].as_str().unwrap_or(id),""),escape(id),escape(doc["description"].as_str().unwrap_or_default()),license_html(view.license),escape(&times),escape(&href),escape(doc["title"].as_str().unwrap_or(id)),icon("arrow")));
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
    body.push_str(&format!(
        "</select></label><span class=\"page-indicator\">Page {} of {}</span>",
        search.offset / search.limit + 1,
        matched.div_ceil(search.limit).max(1)
    ));
    body.push_str(&pagination(nav));
    body.push_str("</div></section></div>");
    Page {
        base,
        api,
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
    base: &str,
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
        base,
        api: "edr",
        title,
        json_url: url,
    }
    .render(&body, "")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn shell_and_metadata_escape_content_and_retain_proxy_prefix() {
        let attack = "</script><script>alert(1)</script>";
        let doc = json!({"id":"a","title":attack,"description":attack,"custom":{"nested":attack},"links":[{"rel":"self","href":"https://example.test/prefix/maps/collections/a"},{"rel":"license","href":"javascript:alert(1)","title":attack}]});
        let html = collection_html("https://example.test/prefix", "maps", &doc, None);
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
