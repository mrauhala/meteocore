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
        let mut nav = String::new();
        if !api.is_empty() {
            for (suffix, label) in [
                ("/", "Overview"),
                ("/collections", "Collections"),
                ("/conformance", "Conformance"),
                ("/api/docs", "API reference"),
            ] {
                let path = format!("{base}/{api}{suffix}");
                nav.push_str(&anchor(
                    &if suffix.ends_with("docs") {
                        path.clone()
                    } else {
                        with_format(&path, "html")
                    },
                    label,
                    if current_path.trim_end_matches('/') == path.trim_end_matches('/')
                        || suffix == "/collections" && current_path.starts_with(&format!("{path}/"))
                    {
                        "active"
                    } else {
                        ""
                    },
                ));
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
<body><a class="skip" href="#main">Skip to content</a><div class="shell"><aside class="sidebar" aria-label="API navigation"><a class="brand" href="{base}/?f=html">MeteoCore<small>API workbench</small></a><div><p class="nav-label">API workspace</p><nav class="api-nav" aria-label="APIs">{api_nav}</nav></div><nav class="resource-nav" aria-label="Resources">{nav}</nav><p class="sidebar-note">Open standards.<br>One resource, multiple representations.</p></aside>
<div class="workspace"><header class="topbar"><nav class="breadcrumbs" aria-label="Breadcrumb">{crumbs}</nav><div class="top-actions"><label class="theme-control enhanced" for="theme">Theme<select id="theme"><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label><nav class="representations" aria-label="Representation"><a aria-current="page" href="{html_url}">HTML</a><a rel="alternate" id="json-link" href="{json_url}">JSON</a></nav></div></header>
<section class="request" aria-label="Current API request"><div class="request-url"><span class="method">GET</span><code id="request-url">{json_url}</code></div><div class="request-actions"><span>JSON URL for the current resource and applied parameters</span><button class="btn enhanced" data-copy="{json_url}">Copy URL</button><button class="btn enhanced" data-copy="{curl}">Copy cURL</button></div></section>
<main id="main" tabindex="-1">{body}</main><footer>MeteoCore · OGC API <span>HTML representation · Times in UTC</span></footer></div></div><div class="toast" id="toast" role="status" aria-live="polite"></div><script>{SCRIPT}</script></body></html>"##,
            title = escape(title),
            json_url = escape(safe_href(json_url)),
            html_url = escape(safe_href(&html_url)),
            base = escape(base),
            curl = escape(&curl)
        )
    }
}

pub fn page_heading(title: &str, description: &str) -> String {
    format!("<div class=\"page-heading\"><p class=\"eyebrow\">OGC API · HTML representation</p><h1>{}</h1><p>{}</p></div>",escape(title),escape(description))
}

/// Render arbitrary metadata without losing types or flattening nested values.
pub fn value_html(value: &Value) -> String {
    match value {
        Value::Null => "<span class=\"muted\">Not available</span>".into(),
        Value::String(v) => escape(v),
        Value::Array(values) if values.iter().all(|v| !v.is_object() && !v.is_array()) => {
            format!(
                "<span class=\"tags\">{}</span>",
                values
                    .iter()
                    .map(|v| format!("<span class=\"badge\">{}</span>", value_html(v)))
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
    let mut out=String::from("<div class=\"table-scroll\"><table class=\"properties\"><thead><tr><th scope=\"col\">Property</th><th scope=\"col\">Value</th></tr></thead><tbody>");
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
    let mut body = String::from("<div class=\"resource-list\">");
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
            body.push_str(&format!("<div class=\"resource-row\"><span class=\"method\">GET</span><div>{}<code>{}</code><span class=\"muted\">{} · {}</span></div></div>",anchor(&target,label,"resource-title"),escape(href),escape(rel),escape(link["type"].as_str().unwrap_or("Linked resource"))));
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
    let mut body = page_heading(title, description);
    body.push_str("<section class=\"panel\"><h2>Resources</h2>");
    body.push_str(&document_links(doc));
    body.push_str("</section>");
    let discovery = if api.is_empty() { "features" } else { api };
    body.push_str(&format!("<section class=\"panel spaced\"><h2>Build a collection query</h2><form method=\"get\" action=\"{}/{}{}\"><input type=\"hidden\" name=\"f\" value=\"html\"><label for=\"q\">q <span class=\"hint\">Text search across titles, descriptions and keywords</span></label><div class=\"search-line\"><input id=\"q\" name=\"q\" placeholder=\"radar\"><button class=\"btn primary\">Build query</button></div></form></section>",escape(base),discovery,"/collections"));
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
    let href = doc["links"]
        .as_array()
        .and_then(|ls| ls.iter().find(|l| l["rel"] == "self"))
        .and_then(|l| l["href"].as_str())
        .unwrap_or_default();
    let url = with_format(href, "json");
    let catalog = format!("{base}/{api}/collections");
    let mut body = format!(
        "<a class=\"back-link\" data-back-scope=\"{}\" href=\"{}\">← Back to collections</a>",
        escape(&catalog),
        escape(&with_format(&catalog, "html"))
    );
    body.push_str(&page_heading(
        title,
        doc["description"].as_str().unwrap_or_default(),
    ));
    body.push_str(&format!(
        "<p class=\"identifier\"><code>{}</code></p>",
        escape(doc["id"].as_str().unwrap_or_default())
    ));
    body.push_str(&license_html(license));
    if let Some(keywords) = doc.get("keywords") {
        body.push_str(&value_html(keywords));
    }
    body.push_str("<div class=\"detail-layout spaced\"><section class=\"panel\"><h2>Data access &amp; resource links</h2>");
    body.push_str(&document_links(doc));
    if let Some(queries) = doc["data_queries"].as_object() {
        body.push_str("<h3>EDR data queries</h3>");
        for (name, query) in queries {
            if let Some(href) = query["link"]["href"].as_str() {
                let href = if name == "instances" {
                    with_format(href, "html")
                } else {
                    href.to_owned()
                };
                body.push_str(&format!("<p>{}</p>", anchor(&href, name, "btn")));
            }
        }
    }
    body.push_str("</section><section class=\"panel\"><h2>Coverage &amp; metadata</h2>");
    let mut metadata = doc.clone();
    if let Some(m) = metadata.as_object_mut() {
        for key in ["links", "title", "description", "keywords"] {
            m.remove(key);
        }
    }
    body.push_str(&property_table(&metadata));
    body.push_str("</section></div>");
    Page {
        base,
        api,
        title,
        json_url: &url,
    }
    .render(&body, "")
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
    let pairs = query.query_string_with_format(search.limit, search.offset, "html");
    let pairs: Vec<_> = form_urlencoded::parse(pairs.trim_start_matches('?').as_bytes()).collect();
    let mut form=format!("<form class=\"query-form panel\" method=\"get\" action=\"{}\"><h2>Query parameters</h2><input type=\"hidden\" name=\"f\" value=\"html\">",escape(url));
    let mut parameters = CollectionParameter::ALL.to_vec();
    // Presentation order only; the authoritative inventory still supplies every field.
    parameters.sort_by_key(|parameter| match parameter {
        CollectionParameter::Q => 0,
        CollectionParameter::Query => 1,
        CollectionParameter::Bbox => 2,
        CollectionParameter::BboxCrs => 3,
        CollectionParameter::Datetime => 4,
        CollectionParameter::Limit => 5,
        CollectionParameter::Offset => 6,
        CollectionParameter::Format => 7,
    });
    for parameter in parameters {
        let name = parameter.name();
        if matches!(parameter, CollectionParameter::Format) {
            continue;
        }
        let value = pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_ref())
            .unwrap_or("");
        if matches!(parameter, CollectionParameter::BboxCrs) && value.is_empty() {
            continue;
        }
        let (help, kind, enhanced) = match parameter {
            CollectionParameter::Q => (
                "string · comma-separated alternatives; words form a phrase",
                "text",
                false,
            ),
            CollectionParameter::Query => ("string · e.g. radar +Finland -volume", "text", true),
            CollectionParameter::Bbox => (
                "CRS84 · west,south,east,north; 6D bounds also accepted",
                "text",
                true,
            ),
            CollectionParameter::BboxCrs => ("CRS84 coordinate reference system", "text", true),
            CollectionParameter::Datetime => (
                "UTC instant or start/end interval; .. for an open end",
                "text",
                true,
            ),
            CollectionParameter::Limit => ("integer · maximum 1000", "number", false),
            CollectionParameter::Offset => {
                ("integer · matching collections to skip", "number", false)
            }
            CollectionParameter::Format => unreachable!(),
        };
        let value = match parameter {
            CollectionParameter::Limit => search.limit.to_string(),
            CollectionParameter::Offset => search.offset.to_string(),
            _ => value.to_owned(),
        };
        form.push_str(&input(name, &value, help, kind, enhanced));
        // Without JS retain existing advanced filters in ordinary GET forms.
        if enhanced && !value.is_empty() {
            form.push_str(&format!(
                "<noscript><input type=\"hidden\" name=\"{}\" value=\"{}\"></noscript>",
                escape(name),
                escape(&value)
            ));
        }
    }
    form.push_str(&format!("<button class=\"btn primary\">Apply query</button>{}<noscript><p>Text search, paging and links work without JavaScript. Enable JavaScript to edit advanced parameters.</p></noscript><div class=\"draft enhanced\"><p class=\"hint\">Request preview · apply to update results</p><code data-draft></code></div></form>",anchor(&with_format(url,"html"),"Reset query","btn")));
    let mut body = page_heading(
        "Collections",
        "Build a discovery query and inspect the matching resources.",
    );
    body.push_str(&format!(
        "<div data-results-scope=\"{}\"></div>",
        escape(url)
    ));
    body.push_str(&format!("<div class=\"query-layout\">{form}<section class=\"results\" aria-label=\"Query results\"><div class=\"results-head\"><div><h2>{matched} matching collections</h2><p>{} returned · ID order</p></div><div class=\"view-switch enhanced\"><button data-view=\"list\" aria-pressed=\"true\">List</button><button data-view=\"cards\" aria-pressed=\"false\">Cards</button></div></div><div class=\"collection-list\">",docs.len()));
    if docs.is_empty() {
        body.push_str("<div class=\"panel empty\"><h2>No matching collections</h2><p>Change or reset your query to broaden the results.</p></div>");
    }
    for view in docs {
        let doc = view.metadata;
        let id = doc["id"].as_str().unwrap_or_default();
        let href = format!("{url}/{}?f=html", path_segment(id));
        let item_count = doc
            .get("numberItems")
            .map(|n| format!("<p class=\"hint\">{} items</p>", value_html(n)))
            .unwrap_or_default();
        let keywords = doc["keywords"]
            .as_array()
            .map(|ks| {
                ks.iter()
                    .filter_map(Value::as_str)
                    .map(|k| format!("<span class=\"badge\">{}</span>", escape(k)))
                    .collect::<String>()
            })
            .unwrap_or_default();
        body.push_str(&format!("<article class=\"collection-card\"><h3>{}</h3><code>{}</code><p>{}</p><div class=\"tags\">{keywords}</div>{}{item_count}</article>",anchor(&href,doc["title"].as_str().unwrap_or(id),""),escape(id),escape(doc["description"].as_str().unwrap_or_default()),license_html(view.license)));
    }
    body.push_str("</div>");
    body.push_str(&pagination(nav));
    body.push_str("</section></div>");
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
            "<article class=\"collection-card\"><h2>{}</h2><code>{}</code><p>{}</p></article>",
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
