//! Shared, server-rendered HTML representations of API resources.
//!
//! JSON remains the resource contract. Adapters supply the same metadata used
//! for JSON; this module supplies only presentation and progressive enhancement.
use ds_core::collection_search::{
    sortby_value, CollectionParameter, CollectionSortKey, CollectionSortProperty, SearchParams,
    SearchQueryParams,
};
use ds_core::config::LicenseConfig;
use ds_core::html::{escape, LinkView, FAVICON};
use serde_json::{json, Value};

use crate::{mounts, rel};

pub mod assets;
mod conformance;
mod metadata;
pub use metadata::{i18n_text, unit_text};

/// The stylesheet, for the tests that read its rules; pages link it as a
/// static asset (`assets`).
#[cfg(test)]
const CSS: &str = include_str!("workbench/style.css");
/// Sets the theme before the first paint, so it stays inline.
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
/// Per-API services the shared root supersedes (#789): still routed, but
/// offered by the switcher only on their own pages.
const SUPERSEDED: &[&str] = &["features", "maps", "tiles"];

/// Collections on an HTML catalog page whose request gives no `limit`
/// (#1141): a screenful of compact rows. JSON keeps listing every one.
pub const HTML_PAGE_SIZE: usize = 24;
/// The order of an HTML catalog page without `sortby` (#1141): by title,
/// the name people look for. JSON keeps id order.
pub const HTML_DEFAULT_SORT: CollectionSortKey =
    CollectionSortKey::ascending(CollectionSortProperty::Title);
/// The orders the catalog's sort controls offer, as `(sortby, label)`.
const SORT_CHOICES: [(&str, &str); 4] = [
    ("title", "Title A–Z"),
    ("-title", "Title Z–A"),
    ("id", "ID A–Z"),
    ("-id", "ID Z–A"),
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

/// First link carrying the registered map relation (Maps Req 46). No
/// surface emits the short `map` relation since #806.
fn map_link(links: &[Value]) -> Option<&str> {
    links
        .iter()
        .find(|l| l["rel"] == rel::MAP)
        .and_then(|l| l["href"].as_str())
}

/// First link carrying the registered `tilesets-map` relation (Tiles Req 13).
fn map_tilesets_link(links: &[Value]) -> Option<&str> {
    links
        .iter()
        .find(|l| l["rel"] == rel::TILESETS_MAP)
        .and_then(|l| l["href"].as_str())
}

/// First link carrying the registered `tilesets-vector` relation.
fn vector_tilesets_link(links: &[Value]) -> Option<&str> {
    links
        .iter()
        .find(|l| l["rel"] == rel::TILESETS_VECTOR)
        .and_then(|l| l["href"].as_str())
}

/// The OGC APIs a collection description advertises access through, as
/// `(label, chip class, description)`. Derived from its links, not from the
/// API serving the page, so a shared-root collection lists every mechanism.
fn api_chips(doc: &Value) -> Vec<(&'static str, &'static str, &'static str)> {
    let links = doc["links"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let has = |rel: &str| links.iter().any(|l| l["rel"] == rel);
    let mut chips = Vec::new();
    if map_link(links).is_some() {
        chips.push(("Maps", "purple", "OGC API - Maps: rendered map images"));
    }
    if links.iter().any(|l| {
        l["rel"]
            .as_str()
            .is_some_and(|r| r.starts_with(TILESETS_REL_PREFIX))
    }) {
        chips.push(("Tiles", "amber", "OGC API - Tiles: map or vector tiles"));
    }
    if has("items") {
        chips.push(("Features", "teal", "OGC API - Features: feature items"));
    }
    if doc["data_queries"].is_object() {
        chips.push((
            "EDR",
            "teal",
            "OGC API - EDR: position, area and other data queries",
        ));
    }
    chips
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

/// `href` with `key` set to `value` alone, in place of the first value it
/// had; every other pair, `f` and paging included, is kept in order.
pub fn query_replace(href: &str, key: &str, value: &str) -> String {
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let mut out = form_urlencoded::Serializer::new(String::new());
    let mut placed = false;
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        if k != key {
            out.append_pair(&k, &v);
        } else if !placed {
            out.append_pair(key, value);
            placed = true;
        }
    }
    if !placed {
        out.append_pair(key, value);
    }
    format!("{path}?{}", out.finish())
}

/// The workspace a server path belongs to, as `(kind, mount)`: the per-API
/// service it is under, else the shared OGC API root.
pub fn workspace_of(path: &str) -> (&'static str, &'static str) {
    APIS.iter()
        .find(|(_, _, mount)| {
            !mount.is_empty()
                && path
                    .strip_prefix(mount)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        .map_or((crate::shared::WORKSPACE, ""), |(kind, _, mount)| {
            (*kind, *mount)
        })
}

/// The name a media type is known by (`HTML`, `JSON`, `PNG`), from its
/// essence, the type without parameters; `None` for any other type. The
/// one table behind the media badges and the resource cards (#1140).
fn media_label(essence: &str) -> Option<&'static str> {
    Some(match essence.trim().to_ascii_lowercase().as_str() {
        "text/html" => "HTML",
        "application/json" => "JSON",
        "application/geo+json" => "GeoJSON",
        "application/vnd.cov+json" | "application/prs.coverage+json" => "CoverageJSON",
        "application/vnd.oai.openapi+json" => "OpenAPI JSON",
        "application/vnd.mapbox-vector-tile" => "MVT",
        "image/png" => "PNG",
        "image/jpeg" => "JPEG",
        "image/webp" => "WebP",
        "text/csv" => "CSV",
        "application/xml" | "text/xml" => "XML",
        _ => return None,
    })
}

/// A small badge naming the media type of a link that does not open an
/// HTML page (#1132), so a JSON document or an image is never a surprise.
/// Empty for `text/html` and for a link whose type is not known.
pub fn media_badge(media: &str) -> String {
    let media = media.trim();
    let essence = media.split(';').next().unwrap_or_default().trim();
    if essence.is_empty() || essence.eq_ignore_ascii_case("text/html") {
        return String::new();
    }
    let label = media_label(essence).unwrap_or(essence);
    format!(
        "<span class=\"chip media-type\" title=\"Opens {}\">{}</span>",
        escape(media),
        escape(label)
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

/// [`anchor`] without a `class` attribute.
fn plain_anchor(href: &str, label: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a>",
        escape(safe_href(href)),
        escape(label)
    )
}

/// An inline SVG icon: a 24-unit `viewBox` stroked in `currentColor`, no
/// icon font. The map's controls (#1220) draw theirs at 20 px with a 1.75
/// stroke (`map.css`); `layers` is also the fallback.
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
        "play" => "M8 5.5v13l10-6.5Z",
        "pause" => "M8 5h2.5v14H8Z M13.5 5H16v14h-2.5Z",
        "step-back" => "m15 6-6 6 6 6",
        "step-forward" => "m9 6 6 6-6 6",
        "to-start" => "M6 5v14 M18 6l-8.5 6 8.5 6Z",
        "to-end" => "M18 5v14 M6 6l8.5 6L6 18Z",
        "loop" => "m17 2 3 3-3 3 M4 11V9a4 4 0 0 1 4-4h12 M7 22l-3-3 3-3 M20 13v2a4 4 0 0 1-4 4H4",
        "bounce" => "m7 8-4 4 4 4 M17 8l4 4-4 4 M3 12h18",
        "once" => "M3 12h13 m-4-4 4 4-4 4 M20 5v14",
        "expand" => "M15 3h6v6 M9 21H3v-6 M21 3l-7 7 M3 21l7-7",
        "collapse" => "M4 14h6v6 M20 10h-6V4 M14 10l7-7 M3 21l7-7",
        "help" => "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18 M9.2 9a2.9 2.9 0 0 1 5.6 1c0 2-2.8 2.6-2.8 2.6 M12 17h.01",
        "basemap" => "M9 4 3 6.5v13.5l6-2.5 6 2.5 6-2.5V4l-6 2.5Z M9 4v13.5 M15 6.5V20",
        "chevron" => "m6 9 6 6 6-6",
        "close" => "M6 6l12 12 M18 6 6 18",
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
        let document_title = format!("{} · MeteoCore", self.title);
        self.render_shell(body, head, labels, &document_title)
    }

    /// A landing page: its title already names the server, so the tab
    /// shows it as it is (#1131).
    fn render_landing(&self, body: &str) -> String {
        self.render_shell(body, "", &[], self.title)
    }

    fn render_shell(
        &self,
        body: &str,
        head: &str,
        labels: &[(&str, &str)],
        document_title: &str,
    ) -> String {
        let Self {
            surface: Surface { base, root, api },
            title: _,
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
        let listed = || {
            APIS.iter()
                .filter(|(id, _, _)| id == api || !SUPERSEDED.contains(id))
        };
        let api_nav = listed()
            .map(|(id, name, mount)| {
                anchor(
                    &api_home(id, mount),
                    name,
                    if id == api { "active" } else { "" },
                )
            })
            .collect::<String>();
        let (workspace_api, workspace_root) = (*api, *root);
        let options = listed()
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
                ("/conformance", "Conformance"),
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
                    let decoded = decode_path(segment);
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
        let (json_label, json_type) = representation(json_url, items);
        let curl = format!("curl --get '{}'", json_url.replace('\'', "'\"'\"'"));
        let catalog = current_path.trim_end_matches('/') == format!("{root}/collections");
        let request_content = format!(
            r#"<div class="request-identity"><span class="method">GET</span><code id="request-url">{json_url}</code></div><div class="request-tools"><span class="request-note">Applied request · {json_label} representation</span><button class="btn small enhanced" data-copy="{json_url}">Copy URL</button><button class="btn small enhanced" data-copy="{curl}">Copy cURL</button><a class="btn small" href="{json_url}">Open {json_label} ↗</a></div>"#,
            json_url = escape(safe_href(json_url)),
            curl = escape(&curl)
        );
        // The request folds into its summary on catalog and items pages,
        // and on every page below 800 px: there the script right after it
        // closes it before the page paints (without JavaScript it stays
        // open, as on a wide screen, where its summary is hidden: the
        // `aria-label` names the bar then).
        let request_context = format!(
            "<details id=\"request-context\" class=\"catalog-request\" aria-label=\"Current API request\"{}><summary>Current API request <code>GET {}</code></summary><div class=\"request-content\">{request_content}</div></details>{}",
            if catalog || items { "" } else { " open" },
            escape(&request_summary(json_url.strip_prefix(base).unwrap_or(json_url))),
            if catalog || items { "" } else { REQUEST_FOLD }
        );
        format!(
            r##"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{document_title}</title><link rel="icon" href="{FAVICON}"><link rel="alternate" type="{json_type}" href="{json_url}"><link rel="stylesheet" href="{stylesheet}"><script>{THEME}</script>{head}</head>
<body class="{page_class}"><a class="skip" href="#main">Skip to content</a><div class="shell"><aside class="sidebar" aria-label="API navigation"><a class="brand" href="{base}/?f=html"><span class="brand-mark"><svg viewBox="0 0 32 32" fill="none" aria-hidden="true"><circle cx="16" cy="16" r="11"/><circle cx="16" cy="16" r="6"/><path d="M16 16 27 5M16 3v3M3 16h3M16 26v3M26 16h3"/></svg></span><span>MeteoCore<small>API WORKBENCH</small></span></a><div class="api-switch enhanced"><label for="api-select">API workspace</label><select id="api-select">{options}</select></div><noscript><nav aria-label="APIs">{api_nav}</nav></noscript><nav id="primary-nav" aria-label="Workspace">{nav}</nav><div class="sidebar-note"><span class="eyebrow">OPEN STANDARDS</span><p>One data platform.<br>Different ways to explore.</p></div><div class="sidebar-footer">MeteoCore<small>OGC API · Developer workspace</small></div></aside>
<div class="workspace"><header class="topbar"><nav id="breadcrumbs" aria-label="Breadcrumb">{crumbs}</nav><div class="topbar-actions"><button class="quiet enhanced" id="help-button">Help</button><label class="theme-label enhanced" for="theme">Theme<select id="theme"><option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option></select></label><nav class="representation-switch" aria-label="Representation"><a aria-current="page" href="{html_url}">HTML</a><a rel="alternate" id="json-link" href="{json_url}">JSON</a></nav></div></header>
{request_context}
<main id="main" tabindex="-1">{body}</main><footer class="page-footer"><span>MeteoCore · Open weather data</span><span>HTML representation · Times in UTC</span></footer></div></div><dialog id="help-dialog"><div class="dialog-head"><h2>Find a collection, then request data</h2><button class="icon-button" id="close-help" aria-label="Close help">×</button></div><div class="panel-body"><p><strong>1. Find a collection.</strong> Collection search matches titles, descriptions and keywords. Commas separate alternatives; words form a phrase.</p><p>Advanced search adds required (+) and excluded (-) terms. Area and time narrow coverage; unknown extents remain eligible.</p><p><strong>2. Request data.</strong> Open a collection to see its data operations. Feature filters select items within that collection; discovery filters are not copied into the data request. The JSON switch always opens the current resource with its applied filters and paging.</p></div></dialog><div id="toast" role="status" aria-live="polite"></div><script src="{script}"></script></body></html>"##,
            document_title = escape(document_title),
            json_url = escape(safe_href(json_url)),
            html_url = escape(safe_href(&html_url)),
            base = escape(base),
            stylesheet = escape(&assets::href(base, "style.css")),
            script = escape(&assets::href(base, "app.js")),
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

/// Keeps a resource page's request bar open on a wide screen and folded
/// below 800 px, also when the window is resized across that width.
const REQUEST_FOLD: &str = "<script>(()=>{const d=document.getElementById('request-context'),m=matchMedia('(max-width: 800px)'),s=()=>{d.open=!m.matches};s();m.addEventListener('change',s)})()</script>";

/// The name and media type of the representation the page's JSON switch
/// opens, from the `f` of its URL: CoverageJSON for an EDR data query,
/// GeoJSON for feature items, else JSON.
fn representation(json_url: &str, items: bool) -> (&'static str, &'static str) {
    let query = json_url
        .split('#')
        .next()
        .and_then(|href| href.split_once('?'))
        .map_or("", |(_, query)| query);
    let format = form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "f")
        .map(|(_, value)| value.to_ascii_lowercase());
    match format.as_deref() {
        Some("coveragejson") => ("CoverageJSON", "application/vnd.cov+json"),
        Some("geojson") => ("GeoJSON", "application/geo+json"),
        Some("csv") => ("CSV", "text/csv"),
        Some("png") => ("PNG", "image/png"),
        _ if items => ("GeoJSON", "application/geo+json"),
        _ => ("JSON", "application/json"),
    }
}

/// The request bar's summary: the path and the applied query, decoded,
/// without `f` (the representation is the bar's own choice).
fn request_summary(href: &str) -> String {
    let href = href.split('#').next().unwrap_or_default();
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let path = decode_path(path);
    let pairs: Vec<String> = form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key != "f")
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    if pairs.is_empty() {
        path
    } else {
        format!("{path}?{}", pairs.join("&"))
    }
}

/// A URL path, or one segment of it, with its `%XX` escapes decoded, for
/// display only (a CAP feature id is mostly escapes). As in a path, `+` and
/// `&` stay themselves; a malformed escape stays as it is.
fn decode_path(path: &str) -> String {
    let protected = path.replace('+', "%2B").replace('&', "%26");
    form_urlencoded::parse(format!("x={protected}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

pub fn page_heading(title: &str, description: &str) -> String {
    format!("<div class=\"page-title\"><div><span class=\"eyebrow\">OGC API · HTML REPRESENTATION</span><h1>{}</h1><p>{}</p></div></div>",escape(title),escape(description))
}

/// Render arbitrary metadata without losing types or flattening nested values.
pub fn value_html(value: &Value) -> String {
    property_value_html("", value)
}

/// [`value_html`] of the member or property `key`: an integral float
/// without its `.0`, and a latitude or longitude to 5 decimals with its
/// exact value in the `title` (#1140).
pub fn property_value_html(key: &str, value: &Value) -> String {
    match value {
        Value::Null => "<span class=\"muted\">Not available</span>".into(),
        Value::String(v) => escape(v),
        Value::Number(n) => metadata::number_html(key, n),
        Value::Array(values) if values.iter().all(|v| !v.is_object() && !v.is_array()) => {
            format!(
                "<span class=\"chip-row\">{}</span>",
                values
                    .iter()
                    .map(|v| format!(
                        "<span class=\"chip\">{}</span>",
                        property_value_html(key, v)
                    ))
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
                property_value_html(key, value)
            ));
        }
    }
    out.push_str("</tbody></table></div>");
    out
}

/// Every link of `links` as a table row with its relation, title and media
/// type, and an `<a>` to the link's own href (OGC API - EDR
/// `/req/html/content` A: all of a response's information in the body).
/// Links that differ only in relation (the short and the registered URI
/// form of one) share a row that lists every relation; nothing is skipped,
/// and no href is rewritten to its HTML view: a `rel=data` query end point
/// stays itself. The anchor also carries the title, its text when a phone
/// stacks each row as a card (#1131).
pub fn link_table(links: &Value) -> String {
    fn text<'a>(link: &'a Value, key: &str) -> &'a str {
        link[key].as_str().unwrap_or_default()
    }
    /// A row's (href, title, type).
    type Target<'a> = (&'a str, &'a str, &'a str);
    // Each target with the relations that list it.
    let mut rows: Vec<(Target, Vec<&str>)> = Vec::new();
    for link in links.as_array().into_iter().flatten() {
        let target = (text(link, "href"), text(link, "title"), text(link, "type"));
        let rel = text(link, "rel");
        match rows.iter_mut().find(|(t, _)| *t == target) {
            Some((_, rels)) if rels.contains(&rel) => {}
            Some((_, rels)) => rels.push(rel),
            None => rows.push((target, vec![rel])),
        }
    }
    let mut out = String::from("<div class=\"table-scroll\"><table class=\"properties link-table\"><colgroup><col class=\"link-rel\"><col class=\"link-title\"><col class=\"link-type\"><col class=\"link-href\"></colgroup><thead><tr><th scope=\"col\">Relation</th><th scope=\"col\">Title</th><th scope=\"col\">Type</th><th scope=\"col\">Link</th></tr></thead><tbody>");
    for ((href, title, media_type), rels) in rows {
        let rels: String = rels
            .iter()
            .map(|rel| format!("<code>{}</code>", escape(rel)))
            .collect();
        let label = if title.is_empty() {
            String::new()
        } else {
            format!("<span class=\"link-label\">{}</span>", escape(title))
        };
        out.push_str(&format!(
            "<tr><td>{rels}</td><td>{}</td><td>{}</td><td><a class=\"table-link\" href=\"{}\">{label}{}</a>{}</td></tr>",
            escape(title),
            escape(media_type),
            escape(safe_href(href)),
            escape(href),
            media_badge(media_type)
        ));
    }
    out.push_str("</tbody></table></div>");
    out
}

/// A document's members other than `links` as a property table, and its
/// links as a [`link_table`], folded under a disclosure: the full content of
/// one entry of a list page (a collection, an instance).
fn entry_details(doc: &Value) -> String {
    let mut members = doc.clone();
    if let Some(m) = members.as_object_mut() {
        m.remove("links");
    }
    format!(
        "<details class=\"entry-details\"><summary>All metadata &amp; links</summary>{}{}</details>",
        property_table(&members),
        link_table(&doc["links"])
    )
}

/// A document's links other than `self` and `alternate` as resource cards,
/// one per resource (#1140): links to one href, or to representations of
/// one resource that differ only in `f`, share a card, labelled in plain
/// words from the relation with the relation term as secondary text. A
/// relation with an HTML view (items, data, conformance, …) opens it; any
/// other link opens its own media type, named by a badge.
pub fn document_links(doc: &Value) -> String {
    let links: Vec<&Value> = doc["links"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| !matches!(l["rel"].as_str(), Some("self" | "alternate")))
        .collect();
    metadata::resource_cards(&links)
}

pub fn landing_html(
    surface: Surface<'_>,
    title: &str,
    description: &str,
    links: &[LinkView],
) -> String {
    let doc = json!({"links":links.iter().map(|l|json!({"href":l.href,"rel":l.rel,"type":l.media_type,"title":l.title})).collect::<Vec<_>>()});
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
    let links = doc["links"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    // The API definition opens as its reference page: the raw OpenAPI
    // document is up to a megabyte of JSON. It stays a secondary link.
    let service_doc = links
        .iter()
        .find(|l| l["rel"] == "service-doc")
        .and_then(|l| l["href"].as_str());
    let mut definition = None;
    for link in links {
        let rel = link["rel"].as_str().unwrap_or_default();
        let href = link["href"].as_str().unwrap_or_default();
        if !matches!(rel, "data" | "conformance" | "service-desc") {
            continue;
        }
        let (target, shown, label) = match (rel, service_doc) {
            ("service-desc", Some(reference)) => {
                definition = Some(href);
                (reference.to_owned(), reference, "API reference")
            }
            ("service-desc", None) => (href.to_owned(), href, "API definition"),
            _ => (
                with_format(href, "html"),
                href,
                link["title"].as_str().unwrap_or(rel),
            ),
        };
        let badge = if rel == "service-desc" && service_doc.is_none() {
            media_badge("application/vnd.oai.openapi+json")
        } else {
            String::new()
        };
        body.push_str(&format!("<a class=\"resource-row\" href=\"{}\"><span class=\"method\">GET</span><code>{}</code><span>{}{badge}</span>{}</a>",escape(safe_href(&target)),escape(shown.strip_prefix(base).unwrap_or(shown)),escape(label),icon("arrow")));
    }
    body.push_str("</div>");
    if let Some(definition) = definition {
        body.push_str(&format!(
            "<p class=\"panel-body field-help resource-alt\">Raw OpenAPI definition: {}{}</p>",
            anchor(
                definition,
                definition.strip_prefix(base).unwrap_or(definition),
                ""
            ),
            media_badge("application/vnd.oai.openapi+json")
        ));
    }
    body.push_str("</section>");
    let data_guidance = match api {
        "features" => "Open a collection, then use Request data to filter its features by the supported properties, area and time. Inspect the results or copy the GeoJSON request.",
        "edr" => "Open a collection to see its supported data queries and parameters. Choose a location, position or area query, then use the linked API reference to supply the required inputs.",
        "maps" => "Open a collection to see its map resources. Use the map preview to inspect the data, or the API reference to request an image for an area, time and style.",
        "tiles" => "Open a collection and follow its tileset links. Select a tile matrix set and tile coordinates to request map or vector tiles.",
        crate::shared::WORKSPACE => "Open a collection to see every way to access it: map images, map tiles and vector tiles, as the collection offers them. Related services are linked below.",
        _ => "Choose an API and a collection first. Then request features, environmental values, map images or tiles using that collection's supported operations.",
    };
    body.push_str(&format!(r#"<div class="developer-start"><section class="panel panel-body"><span class="eyebrow">COLLECTION DISCOVERY</span><h2>1. Find a collection</h2><form method="get" action="{discovery}/collections"><input type="hidden" name="f" value="html"><label for="q">Search collections <span class="parameter-type">q · optional</span></label><div class="search-row"><input id="q" name="q" placeholder="e.g. radar or GFS"><button class="btn primary">Find collections {arrow}</button></div><p class="field-help">Search dataset titles, descriptions and keywords. Area and time filters in the catalog narrow the collection coverage.</p></form></section><section class="panel panel-body"><span class="eyebrow">DATA ACCESS</span><h2>2. Request data</h2><p class="section-note">{data_guidance}</p><p class="field-help">Select a collection to see the available data requests. Discovery filters are not carried over as data filters.</p></section></div>"#,discovery=escape(discovery),arrow=icon("arrow")));
    body.push_str("<section class=\"section-space\"><div class=\"section-header\"><h2>API definitions &amp; resource links</h2></div>");
    body.push_str(&link_table(&doc["links"]));
    body.push_str("</section>");
    Page {
        surface,
        title,
        json_url: &url,
    }
    .render_landing(&body)
}

pub fn conformance_html(surface: Surface<'_>, classes: &[&str], nav: &[LinkView]) -> String {
    let url = nav
        .iter()
        .find(|l| l.rel == "alternate")
        .map(|l| l.href.as_str())
        .unwrap_or("");
    let mut body=page_heading("Conformance classes","Classes declared by this API. Draft work is advertised only when its requirements are implemented.");
    // Grouped by standard and version, each class named and linked (#1140).
    body.push_str(&conformance::conformance_body(classes));
    Page {
        surface,
        title: "Conformance classes",
        json_url: url,
    }
    .render(&body, "")
}

/// One failed request as its HTML page shows it (#1132).
pub struct ErrorView<'a> {
    /// The response status, kept by the page.
    pub status: u16,
    /// The heading: the status's reason phrase, or what the handler names.
    pub title: &'a str,
    /// The client-visible message (generic for a server error).
    pub message: &'a str,
    /// The query parameter at fault and the value the request gave it.
    pub parameter: Option<(&'a str, Option<&'a str>)>,
    /// A plain-language explanation beside the message.
    pub hint: Option<&'a str>,
    /// Accepted values of `parameter` as `(label, href)`: the request again
    /// with that value.
    pub choices: &'a [(String, String)],
    /// The collection the failed resource belongs to, as `(id, href)`.
    pub collection: Option<(&'a str, &'a str)>,
    /// The request as JSON, which the shell's JSON switch opens.
    pub json_url: &'a str,
}

/// The workbench page of a failed request: its status, message, the
/// parameter at fault, and links back to the collection, the catalog and the
/// API reference of `surface`.
pub fn error_html(surface: Surface<'_>, view: &ErrorView<'_>) -> String {
    let root = surface.root;
    let mut body = format!(
        "<div class=\"page-title\"><div><span class=\"eyebrow\">HTTP {} · REQUEST NOT COMPLETED</span><h1>{}</h1><p>{}</p></div></div><section class=\"panel panel-body error-page\" role=\"alert\">",
        view.status,
        escape(view.title),
        escape(view.message)
    );
    if let Some((name, value)) = view.parameter {
        body.push_str(&format!(
            "<p class=\"error-text\">Parameter <code>{}</code>{}</p>",
            escape(name),
            value
                .map(|value| format!(": <code>{}</code>", escape(value)))
                .unwrap_or_default()
        ));
    }
    if let Some(hint) = view.hint {
        body.push_str(&format!("<p class=\"callout spaced\">{}</p>", escape(hint)));
    }
    if !view.choices.is_empty() {
        body.push_str("<h2 class=\"spaced\">Valid values</h2><p class=\"field-help\">Each link runs this request again with that value.</p><div class=\"chip-row\">");
        for (label, href) in view.choices {
            body.push_str(&anchor(href, label, "chip"));
        }
        body.push_str("</div>");
    }
    body.push_str("<div class=\"action-row spaced\">");
    if let Some((id, href)) = view.collection {
        body.push_str(&anchor(href, &format!("← Back to {id}"), "btn primary"));
    }
    body.push_str(&anchor(
        &with_format(&format!("{root}/collections"), "html"),
        "Collections",
        if view.collection.is_some() {
            "btn"
        } else {
            "btn primary"
        },
    ));
    body.push_str(&anchor(&format!("{root}/api/docs"), "API reference", "btn"));
    body.push_str("</div></section>");
    Page {
        surface,
        title: &format!("{} {}", view.status, view.title),
        json_url: view.json_url,
    }
    .render(&body, "")
}

/// Where a collection page lists what its map would show.
const MAP_EXTENT_FALLBACK: &str = "The advertised extent is listed below.";
/// The same for a collection that advertises no spatial extent: only its
/// map requests are listed.
const MAP_REQUEST_FALLBACK: &str =
    "Map requests are listed below, under Request data from this collection.";

/// What a page with a map links in its head: MapLibre's stylesheet and the
/// map's own (`map.css`, a static asset).
pub fn map_head(base: &str) -> String {
    format!(
        "<link rel=\"stylesheet\" href=\"{}/preview/vendor/maplibre-gl.css\"><link rel=\"stylesheet\" href=\"{}\">",
        escape(base),
        escape(&assets::href(base, "map.css"))
    )
}

/// The most times an irregular axis lists in `mapRequest`, which carries
/// them a second time after the collection document.
const MAX_LISTED_FRAMES: u64 = 10_000;
/// The most frames of a regular axis the time bar takes: numbers in the
/// script, never markup.
const MAX_GRID_FRAMES: u64 = 100_000;

/// A temporal extent's axis as the map's time bar reads it (#1220):
/// `{"start", "step", "count"}` for a regular grid, `step` in whole
/// seconds, else `{"times": [...]}`. Times are RFC 3339 in UTC with `Z`,
/// the form `datetime` takes and the map writes to the URL fragment. Only
/// an advertised grid is read, never a cadence inferred from the bounds; an
/// interval of one instant is that instant. `None` without an axis.
fn map_axis(temporal: &Value) -> Option<Value> {
    let parse = |t: &Value| {
        chrono::DateTime::parse_from_rfc3339(t.as_str()?)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let grid = &temporal["grid"];
    let Some(count) = grid["cellsCount"].as_u64().filter(|count| *count >= 1) else {
        let bounds = temporal["interval"][0].as_array()?;
        let (start, end) = (parse(bounds.first()?)?, parse(bounds.get(1)?)?);
        return (start == end).then(|| json!({"times": [crate::subset::rfc3339(start)]}));
    };
    if let Some(coordinates) = grid["coordinates"].as_array() {
        if coordinates.len() as u64 != count || count > MAX_LISTED_FRAMES {
            return None;
        }
        let times = coordinates
            .iter()
            .map(|t| parse(t).map(crate::subset::rfc3339))
            .collect::<Option<Vec<_>>>()?;
        return Some(json!({"times": times}));
    }
    if count > MAX_GRID_FRAMES {
        return None;
    }
    let step = ds_core::datetime::parse_iso8601_duration(grid["resolution"].as_str()?).ok()?;
    let seconds = step.num_seconds();
    if seconds <= 0 || step != chrono::Duration::seconds(seconds) {
        return None;
    }
    let start = parse(&grid["firstCoordinate"]).or_else(|| parse(&temporal["interval"][0][0]))?;
    let end = parse(&temporal["interval"][0][1])?;
    let last = start.checked_add_signed(step.checked_mul(i32::try_from(count - 1).ok()?)?)?;
    if last > end {
        return None;
    }
    Some(json!({"start": crate::subset::rfc3339(start), "step": seconds, "count": count}))
}

/// The first and last time of a [`map_axis`], and its step in seconds when
/// it is a regular grid.
fn axis_ends(
    axis: &Value,
) -> Option<(
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    Option<u64>,
)> {
    let parse = |t: &Value| {
        chrono::DateTime::parse_from_rfc3339(t.as_str()?)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    if let Some(times) = axis["times"].as_array() {
        return Some((parse(times.first()?)?, parse(times.last()?)?, None));
    }
    let start = parse(&axis["start"])?;
    let step = axis["step"].as_u64()?;
    let count = axis["count"].as_u64()?.checked_sub(1)?;
    let last = start.checked_add_signed(chrono::Duration::seconds(
        i64::try_from(step.checked_mul(count)?).ok()?,
    ))?;
    Some((start, last, Some(step)))
}

/// How many times a [`map_axis`] holds.
fn axis_len(axis: &Value) -> u64 {
    axis["times"].as_array().map_or_else(
        || axis["count"].as_u64().unwrap_or(0),
        |times| times.len() as u64,
    )
}

/// The parameters a map draws, from the collection's `parameter_names` (the
/// valid `parameter-name` values, #279): name, label and unit, in the
/// document's order. Reads the Maps/Tiles form (`observedProperty.label`,
/// `unit.symbol.value`) and EDR's (`label`, `unit.symbol` or `unit.label`).
/// A label that ends in its own name, as QueryData's
/// "2 Metre Temperature (2t)" does, drops it: the controls add the unit in
/// parentheses, and "(2t) (°C)" read as two units. A parameter on its own
/// time axis (`extent.temporal`, #819) carries it as `axis`, the
/// [`map_axis`] form, which the time bar binds to when it is chosen.
fn map_parameters(parameters: &Value) -> Vec<Value> {
    fn text(value: &Value) -> Option<String> {
        value
            .as_str()
            .or_else(|| value["en"].as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    }
    parameters
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, parameter)| {
            let label = text(&parameter["label"])
                .or_else(|| text(&parameter["observedProperty"]["label"]))
                .map(|label| {
                    label
                        .strip_suffix(&format!("({name})"))
                        .map(str::trim_end)
                        .filter(|rest| !rest.is_empty())
                        .map_or(label.clone(), str::to_owned)
                })
                .unwrap_or_else(|| name.clone());
            let unit = text(&parameter["unit"]["symbol"]["value"])
                .or_else(|| text(&parameter["unit"]["symbol"]))
                .or_else(|| text(&parameter["unit"]["label"]))
                .unwrap_or_default();
            let mut entry = json!({"name": name, "label": label, "unit": unit});
            if let Some(axis) = map_axis(&parameter["extent"]["temporal"]) {
                entry["axis"] = axis;
            }
            entry
        })
        .collect()
}

/// An icon button of the map's floating panels: 44 px, its name for
/// assistive technology in `aria-label`, its keys in the title.
fn map_button(id: &str, label: &str, title: &str, inner: &str, attrs: &str) -> String {
    format!("<button type=\"button\" class=\"map-icon-button\" id=\"{id}\" aria-label=\"{label}\" title=\"{title}\"{attrs}>{inner}</button>")
}

/// The map's time bar (#1220), the script's alone: hidden until the script
/// binds a time axis to it, docked across the bottom of the map. The track
/// is laid out by time; its range input is the playhead, and the marks over
/// it are drawn by the script. Repeat is a radio group outside the form.
fn map_time_bar(help: &str) -> String {
    let mut bar = String::from("<div id=\"map-timebar\" class=\"map-timebar map-card enhanced\" role=\"group\" aria-label=\"Time\" hidden><div class=\"map-timebar-row\"><div class=\"map-transport\">");
    bar.push_str(&map_button(
        "map-first",
        "First frame of the loop",
        "First frame of the loop; with Shift, of all",
        &icon("to-start"),
        "",
    ));
    bar.push_str(&map_button(
        "map-prev",
        "Previous frame",
        "Previous frame (,); with Shift, 10 frames",
        &icon("step-back"),
        "",
    ));
    bar.push_str(&map_button(
        "map-play",
        "Play",
        "Play or pause (Space or K)",
        &format!(
            "<span class=\"when-paused\">{}</span><span class=\"when-playing\">{}</span>",
            icon("play"),
            icon("pause")
        ),
        "",
    ));
    bar.push_str(&map_button(
        "map-next",
        "Next frame",
        "Next frame (.); with Shift, 10 frames",
        &icon("step-forward"),
        "",
    ));
    bar.push_str(&map_button(
        "map-last",
        "Last frame of the loop",
        "Last frame of the loop; with Shift, of all",
        &icon("to-end"),
        "",
    ));
    bar.push_str("</div><div class=\"map-time-text\"><p class=\"map-time-label\"><span id=\"map-time-drawn\">–</span> <span id=\"map-time-relative\"></span> <span id=\"map-time-seek\" hidden></span></p><p id=\"map-window-text\" class=\"map-window-text\"></p></div><div class=\"map-time-options\">");
    bar.push_str("<label class=\"map-speed\"><span class=\"visually-hidden\">Speed</span><select id=\"map-speed\"><option value=\"1\">1 fps</option><option value=\"2\">2 fps</option><option value=\"4\">4 fps</option><option value=\"8\">8 fps</option></select></label>");
    bar.push_str("<fieldset id=\"map-repeat\" class=\"map-repeat\"><legend class=\"visually-hidden\">Repeat</legend><div class=\"map-repeat-row\">");
    for (value, label, checked) in [
        ("loop", "Loop", " checked"),
        ("bounce", "Bounce", ""),
        ("once", "Once", ""),
    ] {
        bar.push_str(&format!(
            "<label title=\"{label}\"><input type=\"radio\" name=\"map-repeat\" value=\"{value}\"{checked}><span>{}<span class=\"visually-hidden\">{label}</span></span></label>",
            icon(value)
        ));
    }
    bar.push_str("</div></fieldset>");
    bar.push_str("<details id=\"map-time-menu\" class=\"map-time-menu\"><summary aria-label=\"More time options\" title=\"More time options\">…</summary><div class=\"map-time-menu-body map-card\"><label>Jump to · UTC<input id=\"map-jump\" type=\"datetime-local\" step=\"1\"></label><button type=\"button\" class=\"btn small\" id=\"map-window-start\">Start the loop here</button><button type=\"button\" class=\"btn small\" id=\"map-window-end\">End the loop here</button><button type=\"button\" class=\"btn small\" id=\"map-window-reset\">Default loop</button></div></details>");
    bar.push_str(&map_button(
        "map-keys",
        "Help and keyboard shortcuts",
        "Help and keyboard shortcuts (?)",
        &icon("help"),
        " aria-expanded=\"false\" aria-controls=\"map-keys-help\"",
    ));
    bar.push_str("</div></div>");
    bar.push_str(&format!(
        "<div id=\"map-keys-help\" class=\"map-keys-help map-card\" hidden>{help}"
    ));
    bar.push_str(concat!(
        "<p>With focus on the map or its time bar:</p><dl>",

        "<dt>Space or K</dt><dd>Play or pause</dd>",
        "<dt>, and .</dt><dd>Previous or next frame; with Shift, 10 frames</dd>",
        "<dt>← and → on the time slider</dt><dd>Previous or next frame; with Shift, 10 frames</dd>",
        "<dt>Page Up and Page Down</dt><dd>One day earlier or later</dd>",
        "<dt>Home and End</dt><dd>First or last frame</dd>",
        "<dt>[ and ]</dt><dd>Start or end the loop at this frame</dd>",
        "<dt>&lt; and &gt;</dt><dd>Slower or faster</dd>",
        "<dt>N</dt><dd>The frame nearest now</dd>",
        "<dt>?</dt><dd>This list</dd>",
        "<dt>Esc</dt><dd>Close a panel, or leave the full window</dd>",
        "</dl></div>",
        "<div id=\"map-track\" class=\"map-track\">",
        "<div id=\"map-track-marks\" class=\"map-track-marks\" aria-hidden=\"true\"></div>",
        "<div id=\"map-track-buffer\" class=\"map-track-buffer\" aria-hidden=\"true\"></div>",
        "<div id=\"map-window\" class=\"map-window\" aria-hidden=\"true\"><span id=\"map-window-lo\" class=\"map-window-handle\"></span><span id=\"map-window-hi\" class=\"map-window-handle\"></span></div>",
        "<span id=\"map-playhead-mark\" class=\"map-playhead-mark\" aria-hidden=\"true\"></span>",
        "<input id=\"map-playhead\" type=\"range\" min=\"0\" max=\"0\" value=\"0\" step=\"any\" aria-label=\"Time · UTC\">",
        "</div></div>"
    ));
    bar
}

/// The most levels the map offers as a ladder on its right edge (#1220);
/// more stay in the layer card's select alone.
const MAX_LADDER_LEVELS: usize = 12;

/// The map's level ladder: one button per level, the highest in the
/// atmosphere on top. Pressure and model levels count down as they rise;
/// heights, angles and isentropes count up; any other axis keeps its order.
fn map_ladder(vertical: &Value, levels: &[String]) -> String {
    use ds_core::vertical::VerticalKind;
    if levels.is_empty() || levels.len() > MAX_LADDER_LEVELS {
        return String::new();
    }
    let (label, unit) = vertical_axis(vertical);
    let vrs = vertical["vrs"].as_str();
    let rising = |kind: VerticalKind| vrs == Some(kind.vrs());
    let mut ordered: Vec<&String> = levels.iter().collect();
    let value = |level: &&String| level.parse::<f64>().unwrap_or(0.0);
    if rising(VerticalKind::Pressure) || rising(VerticalKind::ModelLevel) {
        ordered.sort_by(|a, b| value(a).total_cmp(&value(b)));
    } else if [
        VerticalKind::Height,
        VerticalKind::ElevationAngle,
        VerticalKind::HeightAboveAntenna,
        VerticalKind::Isentropic,
    ]
    .into_iter()
    .any(rising)
    {
        ordered.sort_by(|a, b| value(b).total_cmp(&value(a)));
    }
    let name = if unit.is_empty() {
        label.to_owned()
    } else {
        format!("{label} · {unit}")
    };
    let mut ladder = format!(
        "<div id=\"map-ladder\" class=\"map-ladder map-card enhanced\" role=\"group\" aria-label=\"{}\"><span class=\"map-ladder-unit\" aria-hidden=\"true\">{}</span>",
        escape(&name),
        escape(if unit.is_empty() { label } else { unit })
    );
    for level in ordered {
        ladder.push_str(&format!(
            "<button type=\"button\" data-level=\"{0}\" aria-pressed=\"false\">{0}</button>",
            escape(level)
        ));
    }
    ladder.push_str("</div>");
    ladder
}

/// The top-right tools: the full window and the basemap with the data's
/// opacity, a popover. The basemap is the bundled Natural Earth only.
fn map_tools() -> String {
    let mut tools = String::from("<div id=\"map-tools\" class=\"map-tools enhanced\">");
    tools.push_str(&map_button(
        "map-expand",
        "Full window",
        "Fill the window with the map; Esc leaves",
        &format!(
            "<span class=\"when-normal\">{}</span><span class=\"when-expanded\">{}</span>",
            icon("expand"),
            icon("collapse")
        ),
        " aria-pressed=\"false\"",
    ));
    tools.push_str(&map_button(
        "map-basemap-button",
        "Basemap and data opacity",
        "Basemap and data opacity",
        &icon("basemap"),
        " aria-expanded=\"false\" aria-controls=\"map-basemap\"",
    ));
    tools.push_str("<div id=\"map-basemap\" class=\"map-basemap map-card\" role=\"group\" aria-label=\"Basemap and data opacity\" hidden><fieldset class=\"map-choice\"><legend class=\"visually-hidden\">Basemap</legend><div class=\"map-choice-row\"><span class=\"map-choice-label\" aria-hidden=\"true\">Basemap</span><div class=\"map-segmented\">");
    for (value, label, title, checked) in [
        ("land", "Land", "Land and borders", " checked"),
        ("borders", "Borders", "Borders only", ""),
        ("none", "None", "No basemap", ""),
    ] {
        tools.push_str(&format!(
            "<label title=\"{title}\"><input type=\"radio\" name=\"map-basemap\" value=\"{value}\"{checked}><span>{label}</span></label>"
        ));
    }
    tools.push_str("</div></div></fieldset><label class=\"map-check\"><input type=\"checkbox\" id=\"map-labels\" checked><span>Country labels</span></label><div class=\"map-opacity\"><label for=\"map-opacity\">Data opacity</label><output id=\"map-opacity-value\" for=\"map-opacity\">85 %</output><input type=\"range\" id=\"map-opacity\" min=\"0\" max=\"100\" step=\"5\" value=\"85\"></div></div></div>");
    tools
}

/// The map panel: the map, its status line, and for a collection with a
/// `mapRequest` the controls, the legend, the time bar and the frame's
/// request. `fallback` finishes the sentence shown when the browser cannot
/// draw the map: where the same facts are on this page.
///
/// A collection's map (#1220) opens the Overview at the content width.
/// Its panels float over it: the layer card top left, which folds into a
/// pill (a bottom sheet on a phone); the full-window and basemap tools top
/// right; a level ladder on the right edge for 12 levels or fewer; the
/// legend bottom left; the time bar docked across the bottom; a status chip
/// top centre, whose text the visually hidden status line also carries.
/// Below the map, "This frame as an API request" holds the request drawn.
///
/// The controls are a real form: a `GET` to the default style's Maps
/// endpoint whose only named fields are Maps parameters (`parameter-name`,
/// `datetime`, `elevation`), so without JavaScript it opens the image the
/// choices describe. The draw-mode switch sits outside the form and the
/// style select has no name: neither is a Maps parameter. With JavaScript
/// every control applies on `change`; there is no submit.
///
/// The script and stylesheets are static assets (`assets`), not inlined.
pub fn map_html(base: &str, features: &Value, quicklook: bool, fallback: &str) -> String {
    let fallback = escape(fallback);
    let scripts = format!(
        "<script src=\"{}/preview/vendor/maplibre-gl.js\" defer></script><script src=\"{}\" defer></script>",
        escape(base),
        escape(&assets::href(base, "map.js"))
    );
    let land = format!("{}/preview/vendor/workbench-land.json", escape(base));
    let data = escape(&features.to_string());
    let Some(request) = features.get("mapRequest") else {
        // A locator: the page's own geometry, with a quick look on an
        // items page.
        return format!("<div class=\"map-panel\"><div id=\"feature-map\" data-land=\"{land}\" data-quicklook=\"{quicklook}\" data-fallback=\"{fallback}\" role=\"region\" aria-label=\"Feature geometry map\"></div><p id=\"map-status\" role=\"status\">The map needs JavaScript and WebGL. {fallback}</p><div id=\"map-data\" hidden>{data}</div></div>{scripts}");
    };
    // A time axis, the collection's or a parameter's own: the time bar.
    let has_axis = |value: &Value| value.get("axis").is_some_and(|axis| !axis.is_null());
    let has_time_bar = has_axis(request)
        || request["parameter_names"]
            .as_array()
            .is_some_and(|parameters| parameters.iter().any(has_axis));
    let modes: Vec<&str> = request["modes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    // Map tiles where advertised, else Maps (#1220): pans keep the
    // tiles, and the server's tile cache is shared between viewers.
    let default_mode = ["tiles", "maps", "wms", "vector"]
        .into_iter()
        .find(|mode| modes.contains(mode))
        .unwrap_or("maps");
    // Vector tiles alone have nothing to choose: the layer card stays
    // hidden, also before the script runs. They have no legend either.
    let vector_only = modes == ["vector"];
    let legend = if modes.iter().any(|mode| *mode != "vector") {
        format!("<div id=\"map-legend\" class=\"map-legend map-card enhanced\"><div class=\"map-legend-head\"><strong id=\"map-legend-title\">Legend</strong><button type=\"button\" id=\"map-legend-toggle\" class=\"map-icon-button map-fold\" aria-expanded=\"true\" aria-controls=\"map-legend-more\" aria-label=\"Legend scale and links\" title=\"Show or hide the scale and links\">{}</button></div><div id=\"map-legend-scale\" hidden></div><p id=\"map-legend-status\">Loading legend…</p><div id=\"map-legend-more\" class=\"map-legend-links\"><a id=\"map-legend-image\" hidden>Legend image ↗</a><a id=\"map-legend-link\" hidden>Legend JSON ↗</a></div></div>", icon("chevron"))
    } else {
        String::new()
    };
    let mut card = format!(
        "<div id=\"map-controls\" class=\"map-card map-layer open\"{}><button type=\"button\" id=\"map-layer-pill\" class=\"map-pill enhanced\" aria-expanded=\"true\" aria-controls=\"map-layer-body\">{}<span id=\"map-layer-text\">Map layer</span><span class=\"map-fold\">{}</span></button><div id=\"map-layer-body\" class=\"map-layer-body\"><div class=\"map-sheet-head enhanced\"><strong>Map layer</strong>{}</div>",
        if vector_only { " hidden" } else { "" },
        icon("layers"),
        icon("chevron"),
        map_button("map-sheet-close", "Close", "Close", &icon("close"), "")
    );
    let styles = request["styles"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    fn maps_href(style: &Value) -> Option<&str> {
        style["href"]
            .as_str()
            .map(safe_href)
            .filter(|href| *href != "#")
    }
    // Without a Maps endpoint there is nothing to GET: the form is the
    // script's alone.
    let action = styles.first().and_then(maps_href);
    match action {
        Some(href) => card.push_str(&format!(
            "<form id=\"map-form\" class=\"map-controls\" method=\"get\" action=\"{}\">",
            escape(href)
        )),
        None => card.push_str("<form id=\"map-form\" class=\"map-controls enhanced\">"),
    }
    // A multi-parameter collection: choose what to draw. "Default" sends
    // no `parameter-name`; the script names what the server drew.
    let parameters = request["parameter_names"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if parameters.len() > 1 {
        card.push_str(
            "<label>Parameter<select id=\"map-parameter\" name=\"parameter-name\"><option value=\"\">Default</option>",
        );
        let label = |p: &Value| p["label"].as_str().unwrap_or_default().to_owned();
        for parameter in parameters {
            let name = parameter["name"].as_str().unwrap_or_default();
            let unit = parameter["unit"].as_str().unwrap_or_default();
            let mut text = label(parameter);
            if !unit.is_empty() {
                text.push_str(&format!(" ({unit})"));
            }
            // Two parameters with one label stay apart by name.
            if parameters
                .iter()
                .filter(|p| label(p) == label(parameter))
                .count()
                > 1
            {
                text.push_str(&format!(" · {name}"));
            }
            card.push_str(&format!(
                "<option value=\"{}\" data-label=\"{}\" data-unit=\"{}\">{}</option>",
                escape(name),
                escape(&label(parameter)),
                escape(unit),
                escape(&text)
            ));
        }
        card.push_str("</select></label>");
    }
    // The style picks the endpoint, not a query parameter: no name, and
    // shown only with the script. One style is no choice at all.
    card.push_str(&format!(
        "<label class=\"enhanced\"{}>Style<select id=\"map-style\" data-mode=\"{}\">",
        if styles.len() < 2 { " hidden" } else { "" },
        escape(default_mode)
    ));
    for style in styles {
        card.push_str(&format!(
            "<option value=\"{}\" data-id=\"{}\" data-tiles=\"{}\" data-legend=\"{}\">{}</option>",
            escape(maps_href(style).unwrap_or_default()),
            escape(style["id"].as_str().unwrap_or_default()),
            escape(style["tiles"].as_str().map(safe_href).unwrap_or_default()),
            escape(style["legend"].as_str().map(safe_href).unwrap_or_default()),
            escape(style["title"].as_str().unwrap_or("Default"))
        ));
    }
    card.push_str("</select></label>");
    let axis = request.get("axis").filter(|axis| !axis.is_null());
    // No temporal extent (a static source, or one with no data yet):
    // there is no time to choose, so no time row. Otherwise a date and
    // time without a zone, which `datetime` reads as UTC, bounded by the
    // axis and stepping by a regular one's step. The script hides it for
    // the time bar, or applies it on `change` when there is no axis to
    // bind.
    if axis.is_some() || request["temporal"] == true {
        let bounds = match axis.and_then(axis_ends) {
            Some((first, last, step)) => {
                let local = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%S");
                format!(
                    " min=\"{}\" max=\"{}\" step=\"{}\"",
                    local(first),
                    local(last),
                    step.unwrap_or(1)
                )
            }
            None => " step=\"1\"".to_owned(),
        };
        card.push_str(&format!("<div class=\"map-time-field\"><label for=\"map-time\">Time · UTC</label><input id=\"map-time\" name=\"datetime\" type=\"datetime-local\"{bounds} aria-describedby=\"map-help\"></div>"));
    }
    let vertical = &request["vertical"];
    let levels = vertical_values(vertical);
    if !levels.is_empty() {
        let (label, unit) = vertical_axis(vertical);
        card.push_str(&format!(
            "<label>{}<select id=\"map-level\" name=\"elevation\"><option value=\"\">Default</option>",
            escape(label)
        ));
        for level in &levels {
            card.push_str(&format!(
                "<option value=\"{}\">{} {}</option>",
                escape(level),
                escape(level),
                escape(unit)
            ));
        }
        card.push_str("</select></label>");
    }
    // The help: in the time bar's help popover where there is one, else
    // under the choices.
    let mut help = String::from("<p id=\"map-help\" class=\"map-help\">Every choice applies at once; Default leaves it to the server.");
    if let Some(axis) = axis {
        help.push_str(&format!(
            " The time bar plays the {} advertised times.",
            axis_len(axis)
        ));
    }
    help.push_str("</p>");
    let time_bar = if has_time_bar {
        map_time_bar(&help)
    } else {
        card.push_str(&help.replacen("class=\"map-help\"", "class=\"map-help enhanced\"", 1));
        String::new()
    };
    // Without the script: one Maps image of the collection's extent per
    // style, each its own endpoint.
    if action.is_some() {
        card.push_str("<noscript><p>Without JavaScript, these choices open one Maps image of the whole collection.</p>");
        for (i, style) in styles.iter().enumerate() {
            let Some(href) = maps_href(style) else {
                continue;
            };
            if i == 0 {
                card.push_str("<button class=\"btn primary\">Open map image</button>");
            } else {
                card.push_str(&format!(
                    "<button class=\"btn\" formaction=\"{}\">Open map image · {}</button>",
                    escape(href),
                    escape(style["title"].as_str().unwrap_or("Style"))
                ));
            }
        }
        card.push_str("</noscript>");
    }
    card.push_str("</form>");
    // More than one mechanism draws this collection: let the viewer
    // choose. The switch is the script's, so it is no form field.
    if modes.len() > 1 {
        card.push_str("<fieldset class=\"map-mode map-choice enhanced\"><legend class=\"visually-hidden\">Draw with</legend><div class=\"map-choice-row\"><span class=\"map-choice-label\" aria-hidden=\"true\">Draw with</span><div class=\"map-segmented\">");
        for (mode, label, title) in [
            ("tiles", "Tiles", "Map tiles (OGC API - Tiles)"),
            ("maps", "Image", "One image per view (OGC API - Maps)"),
            ("wms", "WMS", "WMS GetMap · one image per view"),
            ("vector", "Vector", "Vector tiles"),
        ]
        .into_iter()
        .filter(|(mode, _, _)| modes.contains(mode))
        {
            card.push_str(&format!(
                "<label title=\"{title}\"><input type=\"radio\" name=\"map-mode\" value=\"{mode}\"{}><span>{label}</span></label>",
                if mode == default_mode { " checked" } else { "" }
            ));
        }
        card.push_str("</div></div></fieldset>");
    }
    card.push_str("</div></div>");
    let ladder = map_ladder(vertical, &levels);
    let tools = map_tools();
    let request_disclosure = "<details id=\"map-request\" class=\"map-request enhanced\"><summary>This frame as an API request</summary><div class=\"map-request-body\"><div class=\"request-identity\"><span class=\"method\">GET</span><code id=\"map-image-request\"></code></div><div class=\"map-request-actions\"><button type=\"button\" class=\"btn small\" id=\"map-request-copy\" data-copy=\"\" disabled>Copy</button><a id=\"map-image-link\" class=\"btn small\" aria-disabled=\"true\">Open ↗</a></div></div></details>";
    format!("<div class=\"map-panel map-raster\"><div id=\"map-stage\" class=\"map-stage\"><div class=\"map-view\">{card}<div id=\"feature-map\" data-land=\"{land}\" data-quicklook=\"{quicklook}\" data-fallback=\"{fallback}\" role=\"region\" aria-label=\"Collection map data\"></div><p id=\"map-chip\" class=\"map-chip enhanced\" aria-hidden=\"true\" hidden></p>{tools}{ladder}{legend}</div>{time_bar}</div><p id=\"map-status\" role=\"status\">The map needs JavaScript and WebGL. {fallback}</p>{request_disclosure}<div id=\"map-data\" hidden>{data}</div></div>{scripts}")
}

/// What a collection's Overview says of its feature items beyond the
/// metadata document (#1143), from the Features API: what one item is, and
/// the properties its items page filters and sorts by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemsOverview {
    pub description: Option<String>,
    pub filterables: Vec<String>,
    pub sortables: Vec<String>,
}

pub fn collection_html(
    surface: Surface<'_>,
    doc: &Value,
    license: Option<&LicenseConfig>,
) -> String {
    collection_page(surface, doc, license, None, None)
}

/// A chip row of item properties on the Overview, each opening the items
/// page with its data request builder open at that property (#1143): the
/// fragment `#{kind}={name}` names it, and no query parameter, which
/// `/items` would reject.
fn item_field_chips(items: &str, label: &str, kind: &str, names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let page = with_format(items, "html");
    let chips: String = names
        .iter()
        .map(|name| {
            anchor(
                &format!("{page}#{kind}={}", path_segment(name)),
                name,
                "chip",
            )
        })
        .collect();
    format!("<div class=\"row-line item-fields\"><span class=\"row-label\">{label}</span><span class=\"chip-row\">{chips}</span></div>")
}

/// [`collection_html`] with what the Features API says of the collection's
/// items, when it serves them, and the WMS endpoint when WMS serves the
/// collection as a layer named by its id: the map then offers WMS GetMap
/// as a draw mode (#1220).
pub fn collection_page(
    surface: Surface<'_>,
    doc: &Value,
    license: Option<&LicenseConfig>,
    overview: Option<&ItemsOverview>,
    wms: Option<&str>,
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
    // The preview draws with Maps (one image per view), map tiles and/or
    // vector tiles (Web Mercator tilesets), whichever the collection
    // advertises. Each style carries its own map link and map-tileset list
    // when it has them; vector tiles are unstyled, one layer per collection.
    let safe = |href: &&str| safe_href(href) != "#";
    let map_href = links.and_then(|ls| map_link(ls)).filter(safe);
    let tiles_href = links.and_then(|ls| map_tilesets_link(ls)).filter(safe);
    let vector_href = links.and_then(|ls| vector_tilesets_link(ls)).filter(safe);
    let map_request = (map_href.is_some() || tiles_href.is_some() || vector_href.is_some()).then(|| {
        let legend = |style: &Value| style["links"].as_array()
            .and_then(|ls|ls.iter().find(|l|l["rel"] == rel::LEGEND))
            .and_then(|l|l["href"].as_str()).filter(safe)
            .map(str::to_owned);
        let default_style = doc["styles"].as_array()
            .and_then(|styles|styles.iter().find(|s|s["id"] == "default"));
        let default_legend = default_style.and_then(legend);
        // Each style with its id, which the map's URL fragment names.
        let mut styles = vec![json!({"id":"default","title":"Default","href":map_href,"tiles":tiles_href,"legend":default_legend})];
        for style in doc["styles"].as_array().into_iter().flatten() {
            // The collection map and tileset endpoints already render this style.
            if style["id"] == "default" { continue; }
            let style_links = style["links"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let href = map_link(style_links).filter(safe);
            let tiles = map_tilesets_link(style_links).filter(safe);
            if href.is_some() || tiles.is_some() {
                styles.push(json!({"id":style["id"],"title":style["title"].as_str().or(style["id"].as_str()).unwrap_or("Style"),"href":href,"tiles":tiles,"legend":legend(style)}));
            }
        }
        // WMS draws a raster collection, as the layer named by its id.
        let wms = wms.filter(|_| map_href.is_some() || tiles_href.is_some()).map(safe_href).filter(|href| *href != "#");
        // In the order the switch offers them, the default first (#1220).
        let modes: Vec<&str> = [tiles_href.map(|_| "tiles"), map_href.map(|_| "maps"), wms.map(|_| "wms"), vector_href.map(|_| "vector")].into_iter().flatten().collect();
        // A time axis to choose from: an interval with at least one bound.
        let temporal = doc["extent"]["temporal"]["interval"][0].as_array().is_some_and(|bounds| bounds.iter().any(Value::is_string));
        // The map's attribution names the collection's licence (#1220).
        let license = license.map(|license| json!({"title":license.title,"href":license.resolved_url()}));
        json!({"styles":styles,"modes":modes,"vector":vector_href.map(|href| json!({"href":href,"layer":id})),"wms":wms.map(|href| json!({"href":href,"layer":id})),"parameter_names":map_parameters(&doc["parameter_names"]),"axis":map_axis(&doc["extent"]["temporal"]),"temporal":temporal,"vertical":doc["extent"]["vertical"],"license":license})
    });
    // One chip per OGC API the collection is available through. Its kind
    // (`dataType`/`itemType`) is under "Collection details" as Data type; a
    // `feature` chip beside a `Features` one only read as a duplicate.
    let chips = api_chips(doc);
    let api_chips = if chips.is_empty() {
        format!("<span class=\"chip teal\">{}</span>", escape(api))
    } else {
        chips
            .iter()
            .map(|(label, class, description)| {
                format!(
                    "<span class=\"chip api-chip {class}\" title=\"{description}\">{label}</span>"
                )
            })
            .collect()
    };
    // What one item is, where the description alone could mislead (#1143).
    let items_note = overview
        .and_then(|o| o.description.as_deref())
        .map(|note| format!("<p class=\"items-note\">{}</p>", escape(note)))
        .unwrap_or_default();
    let mut body=format!("<a class=\"back-link\" data-back-scope=\"{}\" href=\"{}\">← Back to collections</a><div class=\"detail-heading\"><div class=\"chip-row\" aria-label=\"Available through\">{api_chips}</div><h1>{}</h1><div class=\"mono\">{}</div><p>{}</p>{items_note}</div><nav class=\"detail-tabs\" aria-label=\"Collection views\"><a data-collection-tab=\"overview\" class=\"active\" href=\"#overview\">Overview</a>",escape(&catalog),escape(&with_format(&catalog,"html")),escape(title),escape(id),escape(doc["description"].as_str().unwrap_or_default()));
    if let Some(items) = items {
        body.push_str(&anchor(&with_format(items, "html"), "Request data", ""));
    }
    body.push_str("<a data-collection-tab=\"metadata\" href=\"#metadata\">Metadata &amp; links</a></nav><section id=\"overview\" class=\"collection-view\" data-collection-view>");
    let bbox = &doc["extent"]["spatial"]["bbox"][0];
    // The advertised extent as a polygon, which the map frames and outlines.
    let extent = bbox
        .as_array()
        .filter(|b| matches!(b.len(), 4 | 6) && b.iter().all(Value::is_number))
        .map(|b| {
            let east = if b.len() == 6 { 3 } else { 2 };
            let north = east + 1;
            let (w, s, e, n) = (
                b[0].as_f64().unwrap(),
                b[1].as_f64().unwrap(),
                b[east].as_f64().unwrap(),
                b[north].as_f64().unwrap(),
            );
            // A box crossing the antimeridian (west > east) is one region:
            // one polygon in unwrapped longitudes, east + 360, so the map
            // frames and draws it whole instead of as two halves at the
            // world's edges.
            let e = if w > e { e + 360. } else { e };
            json!({"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[w, s], [e, s], [e, n], [w, n], [w, s]]]},"properties":{"label":"Advertised collection extent"}})
        });
    let mut head = String::new();
    // A collection with a map opens on it (#1220): the full content width,
    // its controls on it, the metadata below.
    if let Some(request) = &map_request {
        head = map_head(base);
        let features: Vec<&Value> = extent.iter().collect();
        body.push_str("<div class=\"map-section\"><h2 class=\"visually-hidden\">Map</h2>");
        body.push_str(&map_html(
            base,
            &json!({"type":"FeatureCollection","features":features,"mapRequest":request}),
            false,
            if extent.is_some() {
                MAP_EXTENT_FALLBACK
            } else {
                MAP_REQUEST_FALLBACK
            },
        ));
        body.push_str("</div>");
    }
    body.push_str("<div class=\"detail-layout\"><div><section class=\"panel\"><div class=\"panel-head\"><h2>Data coverage</h2><span class=\"chip\">CRS84</span></div>");
    if map_request.is_none() {
        // A locator of the advertised extent.
        if let Some(extent) = &extent {
            head = map_head(base);
            body.push_str(&map_html(
                base,
                &json!({"type":"FeatureCollection","features":[extent]}),
                false,
                MAP_EXTENT_FALLBACK,
            ));
        } else {
            body.push_str("<div class=\"empty-state\"><p>Spatial extent not advertised.</p></div>");
        }
    }
    // Time, cadence, steps and bounds with the catalog's formatting (#1140).
    body.push_str(&metadata::coverage_facts(doc));
    body.push_str("</section>");
    // The parameters a request names in `parameter-name`, with their units.
    if let Some(parameters) = doc["parameter_names"].as_object().filter(|p| !p.is_empty()) {
        body.push_str(&format!(
            "<section class=\"panel spaced\" id=\"parameters\"><div class=\"panel-head\"><h2>Parameters</h2><span class=\"chip\">{}</span></div>{}</section>",
            parameters.len(),
            metadata::parameter_table(parameters, false)
        ));
    }
    body.push_str("<section class=\"section-space\"><h2>Request data from this collection</h2>");
    if let Some(items) = items {
        body.push_str(&format!("<div class=\"callout spaced\">Filter features within this collection by its supported properties, area and time. Inspect the results or copy the GeoJSON request. These filters select data, not collections.</div><div class=\"action-row spaced\">{}{}</div>",anchor(&with_format(items,"html"),"Build a data request →","btn primary"),anchor(&url,"View metadata JSON","btn")));
        if let Some(overview) = overview {
            body.push_str(&item_field_chips(
                items,
                "Filter by",
                "filter",
                &overview.filterables,
            ));
            body.push_str(&item_field_chips(
                items,
                "Sort by",
                "sort",
                &overview.sortables,
            ));
        }
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
                body.push_str(&format!("<div class=\"endpoint\"><div><a class=\"resource-title\" href=\"{}\"><strong>{} query</strong><code>{}</code></a><p>{}</p></div>{}</div>",escape(safe_href(&data_query_target(name, href))),escape(name),escape(href),value_html(&query["link"]["variables"]["output_formats"]),anchor(&target,if name=="instances"{"Browse runs →"}else{"API docs ↗"},"btn small")));
            }
        }
        body.push_str("</div>");
    }
    if let Some(map_href) = map_href.filter(|_| map_request.is_some()) {
        body.push_str("<p class=\"spaced\">With JavaScript, “This frame as an API request” under the map holds the exact request of the frame drawn: its style, time, vertical level, bounds and output size. The JSON switch opens this collection’s metadata.</p>");
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
    // Plain labels; the JSON terms and CRS URIs are secondary text (#1140).
    body.push_str(&format!(
        "<dt>Data type</dt><dd>{}</dd><dt>Storage coordinate system</dt><dd>{}</dd>",
        metadata::kind_html(doc),
        doc["storageCrs"]
            .as_str()
            .map_or_else(|| metadata::NOT_ADVERTISED.to_owned(), metadata::crs_chip)
    ));
    if let Some(list) = doc["crs"].as_array().filter(|l| !l.is_empty()) {
        let chips: String = list
            .iter()
            .filter_map(Value::as_str)
            .map(metadata::crs_chip)
            .collect();
        body.push_str(&format!(
            "<dt>Coordinate systems</dt><dd><span class=\"chip-row\">{chips}</span></dd>"
        ));
    }
    if let Some(n) = doc.get("numberItems") {
        body.push_str(&format!("<dt>Items</dt><dd>{}</dd>", value_html(n)));
    }
    body.push_str("<dt>Keywords</dt><dd><div class=\"tag-list\">");
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
        body.push_str(metadata::NOT_ADVERTISED);
    }
    body.push_str("</div></dd></dl>");
    body.push_str(&license_html(license));
    body.push_str("</div></section>");
    // Each href once on the Overview (#1140): the data query end points are
    // listed under "Request data" from `data_queries` (each opening its
    // HTML view, a query's form, #1138), the tilesets beside the
    // map, and a configured license in the details. The metadata tab's
    // link table lists every link at its own href.
    let query_hrefs: Vec<&str> = doc["data_queries"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, query)| query["link"]["href"].as_str())
        .collect();
    let license_href = license.and_then(|license| license.resolved_url());
    let resource_links: Vec<&Value> = links
        .into_iter()
        .flatten()
        .filter(|l| {
            let href = l["href"].as_str().unwrap_or_default();
            !matches!(l["rel"].as_str(), Some("self" | "alternate"))
                && !(l["rel"] == "data" && query_hrefs.contains(&href))
                && !tilesets.iter().any(|t| t["href"] == href)
                && !(l["rel"] == "license" && license_href.as_deref() == Some(href))
        })
        .collect();
    // An empty list gets no panel.
    if !resource_links.is_empty() {
        body.push_str("<section class=\"panel\"><div class=\"panel-head\"><h2>Resource links</h2></div><div class=\"panel-body\">");
        body.push_str(&metadata::resource_cards(&resource_links));
        body.push_str("</div></section>");
    }
    body.push_str("</aside></div></section><section id=\"metadata\" class=\"collection-view metadata-section\" data-collection-view><section class=\"panel\"><div class=\"panel-head\"><h2>Coverage &amp; metadata</h2>");
    body.push_str(&anchor(&url, "View full JSON { }", "quiet"));
    body.push_str("</div>");
    // Every member but the links, as tables and anchors, never as JSON text.
    body.push_str(&metadata::metadata_html(doc));
    body.push_str(
        "</section><section class=\"panel spaced\"><div class=\"panel-head\"><h2>Links</h2></div>",
    );
    body.push_str(&link_table(&doc["links"]));
    body.push_str("</section></section>");
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

/// Where a `data_queries` link points in HTML (every link an `<a>`, OGC
/// API - EDR `/req/html/content`): the end point's HTML view. Instances,
/// locations and items are lists; a parameterised query (position, area,
/// radius, trajectory, cube) asked for as HTML without its coordinates
/// answers with its query form (#1138), so no link from a page ends in a
/// 400. The one place catalog chips, collection and run pages choose it;
/// the query type no longer changes the answer, every one has a view.
fn data_query_target(_name: &str, href: &str) -> String {
    with_format(href, "html")
}

/// A document's `data_queries` links as anchors, one per query type.
pub fn data_query_links(doc: &Value) -> String {
    doc["data_queries"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, query)| {
            let href = query["link"]["href"].as_str()?;
            Some(anchor(&data_query_target(name, href), name, "chip"))
        })
        .collect()
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

/// An empty catalog fact slot: a dash, read out as "Not advertised".
const EMPTY_SLOT: &str =
    "<span aria-hidden=\"true\">—</span><span class=\"visually-hidden\">Not advertised</span>";

/// The catalog's Cadence slot: the advertised time step and the number of
/// time steps, either or both; `None` when the collection advertises neither.
fn cadence(temporal: &Value) -> Option<String> {
    let step = temporal["grid"]["resolution"]
        .as_str()
        .map(metadata::cadence_label);
    let count = temporal["grid"]["cellsCount"]
        .as_u64()
        .or_else(|| {
            temporal["values"]
                .as_array()
                .filter(|v| v.len() > 1)
                .map(|v| v.len() as u64)
        })
        .map(|n| format!("{n} time step{}", if n == 1 { "" } else { "s" }));
    let text = match (step, count) {
        (Some(step), Some(count)) => format!("{step} · {count}"),
        (step, count) => step.or(count)?,
    };
    Some(escape(&text))
}

/// The catalog's Area slot: the first CRS84 bbox, its exact numbers in the
/// tooltip; `None` without a usable one.
fn area(bbox: &Value) -> Option<String> {
    let v = metadata::bbox_xy(bbox)?;
    Some(format!(
        "<span title=\"{}\">{}</span>",
        escape(&bbox.to_string()),
        escape(&metadata::bbox_label(v))
    ))
}

/// What a catalog row calls an entry of `parameter_names`: its `label`,
/// else its observed property's, each plain text or a language map (English
/// first); else the name itself.
fn catalog_parameter_label<'a>(name: &'a str, parameter: &'a Value) -> &'a str {
    let text = |value: &'a Value| {
        value
            .as_str()
            .or_else(|| value["en"].as_str())
            .or_else(|| value.as_object()?.values().find_map(Value::as_str))
            .filter(|text| !text.trim().is_empty())
    };
    text(&parameter["label"])
        .or_else(|| text(&parameter["observedProperty"]["label"]))
        .unwrap_or(name)
}

/// `(label, name)` entries as a fact: up to three labels inline; more show
/// the first three and fold the full list, each label with its name, under
/// a disclosure (#1141), instead of a tooltip only a mouse can read.
fn label_list(entries: &[(&str, &str)]) -> String {
    const SHOWN: usize = 3;
    let preview = entries
        .iter()
        .take(SHOWN)
        .map(|(label, _)| escape(label))
        .collect::<Vec<_>>()
        .join(", ");
    if entries.len() <= SHOWN {
        return preview;
    }
    let items: String = entries
        .iter()
        .map(|(label, name)| {
            if name.is_empty() || label == name {
                format!("<li>{}</li>", escape(label))
            } else {
                format!("<li>{} <code>{}</code></li>", escape(label), escape(name))
            }
        })
        .collect();
    format!(
        "<details class=\"slot-list\"><summary>{preview} <span class=\"slot-more\">+{} more</span></summary><ul>{items}</ul></details>",
        entries.len() - SHOWN
    )
}

/// A catalog row's facts, in fixed slots so they line up from row to row
/// (#1141): Time, Cadence, Area, Vertical, and Parameters (or Styles), each
/// a dash when the collection does not advertise it; Items follows for a
/// feature collection. The data queries come after, as their own line of
/// action chips.
fn collection_facts(doc: &Value) -> String {
    let mut facts = String::from("<dl class=\"collection-facts\">");
    let mut slot = |label: &str, value: Option<String>| {
        facts.push_str(&format!(
            "<div><dt>{}</dt><dd>{}</dd></div>",
            escape(label),
            value.as_deref().unwrap_or(EMPTY_SLOT)
        ))
    };
    let temporal = &doc["extent"]["temporal"];
    slot(
        "Time · UTC",
        temporal["interval"][0]
            .is_array()
            .then(|| collection_time_html(temporal)),
    );
    slot("Cadence", cadence(temporal));
    slot("Area · CRS84", area(&doc["extent"]["spatial"]["bbox"][0]));
    slot("Vertical", vertical_summary(&doc["extent"]["vertical"]));
    let parameters: Vec<(&str, &str)> = doc["parameter_names"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, parameter)| (catalog_parameter_label(name, parameter), name.as_str()))
        .collect();
    let styles: Vec<(&str, &str)> = doc["styles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|style| {
            let id = style["id"].as_str();
            Some((style["title"].as_str().or(id)?, id.unwrap_or_default()))
        })
        .collect();
    if !parameters.is_empty() {
        slot("Parameters", Some(label_list(&parameters)));
    } else if !styles.is_empty() {
        slot("Styles", Some(label_list(&styles)));
    } else {
        slot("Parameters", None);
    }
    if let Some(n) = doc.get("numberItems") {
        slot("Items", Some(value_html(n)));
    }
    facts.push_str("</dl>");
    let queries = data_query_links(doc);
    if !queries.is_empty() {
        facts.push_str(&format!(
            "<div class=\"row-line query-chips\"><span class=\"row-label\">Data queries</span><span class=\"chip-row\">{queries}</span></div>"
        ));
    }
    facts
}

/// Examples of the `datetime` forms a catalog or item search accepts, shown
/// beside the field (#1132). Not a placeholder: one would read as a value
/// already applied.
pub const DATETIME_EXAMPLES: [&str; 2] = [
    "2026-10-17T00:00:00Z/..",
    "2026-10-16T00:00:00Z/2026-10-17T00:00:00Z",
];

/// The help text of a `datetime` field: what it takes, with examples.
pub fn datetime_help() -> String {
    format!(
        "UTC instant or interval, e.g. {} or {}",
        DATETIME_EXAMPLES[0], DATETIME_EXAMPLES[1]
    )
}

/// A `datetime` text field (see [`input`]) with its examples as help.
pub fn datetime_input(value: &str, enhanced: bool) -> String {
    input("datetime", value, &datetime_help(), "text", enhanced)
}

/// `field` (markup holding one `<input>`) marked as the field a request was
/// rejected for: `aria-invalid`, and `message` beside it (#1132).
pub fn mark_invalid(field: &str, name: &str, message: &str) -> String {
    let id = format!("{}-error", path_segment(name));
    let attributes = format!("aria-invalid=\"true\" aria-errormessage=\"{id}\"");
    let field = if field.contains("aria-describedby=\"") {
        field.replacen(
            "aria-describedby=\"",
            &format!("{attributes} aria-describedby=\"{id} "),
            1,
        )
    } else {
        field.replacen(
            "<input ",
            &format!("<input {attributes} aria-describedby=\"{id}\" "),
            1,
        )
    };
    // Inside the field's label, so it stays with the field in any layout.
    let note = format!(
        "<span id=\"{id}\" class=\"error-text field-error\">{}</span>",
        escape(message)
    );
    match field.rfind("</label>") {
        Some(at) => format!("{}{note}{}", &field[..at], &field[at..]),
        None => format!("{field}{note}"),
    }
}

/// The `<option>`s of a catalog sort control, `current` selected: the
/// offered orders, and `current` itself first when it is none of them (a
/// multi-key `sortby`).
fn sort_options(current: &str) -> String {
    let mut options = String::new();
    if !SORT_CHOICES.iter().any(|(value, _)| *value == current) {
        options.push_str(&format!(
            "<option value=\"{}\" selected>{}</option>",
            escape(current),
            escape(current)
        ));
    }
    for (value, label) in SORT_CHOICES {
        options.push_str(&format!(
            "<option value=\"{value}\"{}>{label}</option>",
            if value == current { " selected" } else { "" }
        ));
    }
    options
}

/// How a results bar names an order: `Title A–Z`, keys joined by "then".
fn sort_label(keys: &[CollectionSortKey]) -> String {
    keys.iter()
        .map(|key| {
            let value = key.to_string();
            SORT_CHOICES
                .iter()
                .find(|(choice, _)| *choice == value)
                .map_or(value.clone(), |(_, label)| (*label).to_owned())
        })
        .collect::<Vec<_>>()
        .join(", then ")
}

/// The same search with each multi-word alternative of `q` split into
/// separate words (`radar composite` → `radar,composite`): what a search
/// that matched nothing as a phrase offers instead (#1141). `None` when no
/// alternative has more than one word.
fn comma_alternatives(q: &str) -> Option<String> {
    let alternatives: Vec<&str> = q.split(',').map(str::trim).collect();
    if !alternatives
        .iter()
        .any(|alternative| alternative.split_whitespace().nth(1).is_some())
    {
        return None;
    }
    Some(
        alternatives
            .iter()
            .flat_map(|alternative| alternative.split_whitespace())
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// The catalog search form over the request's decoded `pairs`; `invalid`
/// is the parameter a rejected request failed on and the message, marked
/// on its field. `page_url` is the request as HTML, which the applied-filter
/// chips edit; `sortby` is the order the page uses.
fn catalog_form(
    url: &str,
    page_url: &str,
    pairs: &[(String, String)],
    limit: &str,
    offset: &str,
    sortby: &str,
    invalid: Option<(Option<&str>, &str)>,
) -> String {
    let value = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    };
    let rejected = |name: &str| match invalid {
        Some((Some(parameter), message)) if parameter == name => Some(message),
        _ => None,
    };
    let mark = |field: String, name: &str| match rejected(name) {
        Some(message) => mark_invalid(&field, name, message),
        None => field,
    };
    let mut primary = String::new();
    let mut advanced = String::new();
    let mut paging = String::new();
    for parameter in CollectionParameter::ALL {
        let name = parameter.name();
        let current = value(name);
        match parameter {
            CollectionParameter::Format=>{},
            CollectionParameter::Q=>primary=format!("<div class=\"search-row\"><div class=\"search-main\"><label for=\"collection-search\">Search text <code>q</code></label><div class=\"search-input-wrap\">{}{}</div></div><button class=\"btn primary\">Find collections {}</button></div><p id=\"collection-search-help\" class=\"search-hint\">Searches titles, descriptions and keywords for whole words, not ids. Words match as an exact phrase; separate alternatives with commas.</p>",icon("search"),mark(format!("<input id=\"collection-search\" name=\"q\" value=\"{}\" placeholder=\"e.g. radar or GFS\" aria-describedby=\"collection-search-help\">",escape(current)),name),icon("arrow")),
            CollectionParameter::Limit|CollectionParameter::Offset=>paging.push_str(&mark(input(name,if name=="limit"{limit}else{offset},"integer","number",false),name)),
            // A rejected order is shown as typed, to be corrected in place.
            CollectionParameter::Sortby if rejected(name).is_some()=>paging.push_str(&mark(input(name,current,"title or id; - descends","text",false).replace("<span>sortby</span>","<span>Order <code>sortby</code></span>"),name)),
            CollectionParameter::Sortby=>paging.push_str(&format!("<label class=\"field\"><span>Order <code>sortby</code></span><span class=\"hint\">Title or ID</span><select name=\"sortby\">{}</select></label>",sort_options(sortby))),
            CollectionParameter::Bbox=>{
                let bounds:Vec<_>=current.split(',').collect();
                if rejected(name).is_none() && (current.is_empty()||bounds.len()==4) {
                    advanced.push_str(&format!("<div class=\"enhanced\"><label>bbox <span class=\"parameter-type\">number[4] · CRS84</span></label><input type=\"hidden\" data-param=\"bbox\" value=\"{}\"><div class=\"bbox-fields\">",escape(current)));
                    for (i,(label,hint)) in [("West","e.g. 18"),("South","e.g. 58"),("East","e.g. 32"),("North","e.g. 71")].iter().enumerate(){
                        advanced.push_str(&format!("<label>{label}<input data-bbox=\"{i}\" type=\"number\" step=\"any\" value=\"{}\" placeholder=\"{hint}\"></label>",escape(bounds.get(i).copied().unwrap_or_default())));
                    }
                    advanced.push_str("</div><p class=\"field-help\">CRS84 · leave blank for any area</p></div>");
                } else if bounds.len()==6 && rejected(name).is_none() {advanced.push_str(&input(name,current,"Six-dimensional bounds · CRS84","text",true));}
                // A rejected box is shown as typed, to be corrected in place.
                else {advanced.push_str(&mark(input(name,current,"CRS84 · west,south,east,north","text",true),name));}
            },
            CollectionParameter::BboxCrs if current.is_empty()=>{},
            _=>{
                let help=match parameter {CollectionParameter::Query=>"string · optional",_=>"CRS84 coordinate reference system"};
                let mut field=if matches!(parameter,CollectionParameter::Datetime) {datetime_input(current,true)} else {input(name,current,help,"text",true)};
                if matches!(parameter,CollectionParameter::Query) {field=field.replace("<span>query</span>","<span>Advanced expression <code>query</code></span>");}
                if matches!(parameter,CollectionParameter::Datetime) {field=field.replace("<span>datetime</span>","<span>Time coverage <code>datetime</code></span>");}
                field=mark(field,name);
                if matches!(parameter,CollectionParameter::Query){advanced.insert_str(0,&format!("<div>{field}<p class=\"field-help\">+ requires a term · − excludes it · commas mean OR</p></div>"));}
                else {advanced.push_str(&field);}
            }
        }
        if !matches!(
            parameter,
            CollectionParameter::Q
                | CollectionParameter::Sortby
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
        .filter(|(k, v)| ["q", "query", "bbox", "datetime"].contains(&k.as_str()) && !v.is_empty())
        .map(|(k, v)| {
            anchor(
                &query_edit(page_url, k, None),
                &format!("{k}: {v} ×"),
                "filter-chip",
            )
        })
        .collect::<String>();
    // A rejected field below the search box opens the filters it is in;
    // a parameter with no field of its own is named above them.
    let open = match invalid {
        Some((Some(parameter), _)) if parameter != "q" => " open",
        _ => "",
    };
    let summary = match invalid {
        Some((parameter, message))
            if parameter.is_none_or(|p| {
                !CollectionParameter::ALL.iter().any(|c| c.name() == p) || p == "f"
            }) =>
        {
            format!(
                "<p class=\"error-text field-error\" role=\"alert\">{}</p>",
                escape(message)
            )
        }
        _ => String::new(),
    };
    format!("<form aria-label=\"Collection search\" class=\"search-panel query-form\" method=\"get\" action=\"{}\"><input type=\"hidden\" name=\"f\" value=\"html\">{primary}{summary}<details class=\"filters\" data-disclosure=\"collection-search\"{open}><summary>Advanced search, area and time</summary><div class=\"filters-grid\">{advanced}</div><div class=\"paging-fields\">{paging}</div><div class=\"filter-controls\"><span class=\"field-help\">Collections with unknown extents remain eligible.</span>{}</div></details><div class=\"active-filters\">{chips}</div><details class=\"draft-request enhanced\" data-draft-disclosure><summary>Preview search request</summary><small>Draft · submit to update matches</small><code data-draft></code></details><noscript><p>Enable JavaScript to edit advanced parameters.</p></noscript></form>",escape(url),anchor(&with_format(url,"html"),"Clear filters","quiet"))
}

/// The catalog page of an HTML search whose parameters were rejected
/// (#1132), answered with status 400: the form again, holding the values the
/// request gave, the field at fault marked with the message, and no
/// results. `pairs` are the request's decoded query pairs; `parameter` is
/// the one at fault, when a single one is.
pub fn rejected_collections_html(
    surface: Surface<'_>,
    pairs: &[(String, String)],
    parameter: Option<&str>,
    message: &str,
) -> String {
    let url = &format!("{}/collections", surface.root);
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs.iter().filter(|(key, _)| key != "f") {
        query.append_pair(key, value);
    }
    let page_url = format!("{url}?{}", query.finish());
    let json_url = with_format(&page_url, "json");
    let value = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let limit = value("limit").unwrap_or_default().to_owned();
    let limit = if limit.is_empty() {
        HTML_PAGE_SIZE.to_string()
    } else {
        limit
    };
    let offset = value("offset").filter(|v| !v.is_empty()).unwrap_or("0");
    let sortby = value("sortby")
        .map(|v| v.trim().trim_start_matches('+').trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| HTML_DEFAULT_SORT.to_string());
    let form = catalog_form(
        url,
        &page_url,
        pairs,
        &limit,
        offset,
        &sortby,
        Some((parameter, message)),
    );
    let mut body = page_heading(
        "Collections",
        "Find a dataset, then open it to request data.",
    )
    .replace(
        "OGC API · HTML REPRESENTATION",
        &format!("{} · DATA CATALOG", surface.api.to_uppercase()),
    );
    let field = parameter
        .map(|p| format!("<p>Check <code>{}</code> and search again.</p>", escape(p)))
        .unwrap_or_default();
    body.push_str(&format!("<div data-results-scope=\"{}\"></div><div class=\"query-workspace\"><aside class=\"query-builder\">{form}</aside><section class=\"query-results\" aria-label=\"Matching collections\"><div class=\"empty-state\" role=\"alert\"><h2>This search could not run.</h2><p>{}</p>{field}{}</div></section></div>",escape(url),escape(message),anchor(&with_format(url,"html"),"Clear filters","btn primary")));
    Page {
        surface,
        title: "Collections",
        json_url: &json_url,
    }
    .render(&body, "")
}

/// The HTML catalog page: the search form, one compact row per collection
/// on the page with every member and link of its description (OGC API -
/// EDR `/req/html/content`), paging, and the page's links. `search.sortby`
/// is the order the page lists (title when empty), and the page's JSON
/// switch spells it out with the page size (#1141).
pub fn collections_html(
    surface: Surface<'_>,
    query: &SearchQueryParams,
    search: &SearchParams,
    matched: usize,
    docs: &[CollectionView<'_>],
    nav: &[LinkView],
    links: &Value,
) -> String {
    let api = surface.api;
    let url = &format!("{}/collections", surface.root);
    let sort = if search.sortby.is_empty() {
        vec![HTML_DEFAULT_SORT]
    } else {
        search.sortby.clone()
    };
    let json_url = format!(
        "{url}{}",
        query
            .explicit_page(search.limit, &sort)
            .query_string_with_format(search.limit, search.offset, "json")
    );
    let query_string = query.query_string_with_format(search.limit, search.offset, "html");
    let page_url = format!("{url}{query_string}");
    let pairs: Vec<(String, String)> =
        form_urlencoded::parse(query_string.trim_start_matches('?').as_bytes())
            .into_owned()
            .collect();
    let sort_value = sortby_value(&sort);
    let form = catalog_form(
        url,
        &page_url,
        &pairs,
        &search.limit.to_string(),
        &search.offset.to_string(),
        &sort_value,
        None,
    );
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
    body.push_str(&format!("<div data-results-scope=\"{}\"></div><div class=\"query-workspace\"><aside class=\"query-builder\">{form}</aside><section class=\"query-results\" aria-labelledby=\"results-count\"><div class=\"results-bar\"><div><h2 id=\"results-count\">{matched} matching {collection_noun}</h2><small>{range} · {}</small></div><div class=\"results-tools\"><label class=\"sort-select enhanced\">Sort<select data-sort>{}</select></label><div class=\"view-switch enhanced\" aria-label=\"Result presentation\"><button data-view=\"list\" class=\"selected\" aria-pressed=\"true\">{}List</button><button data-view=\"cards\" aria-pressed=\"false\">{}Cards</button></div></div></div><div class=\"collection-list\">",escape(url),escape(&sort_label(&sort)),sort_options(&sort_value),icon("list"),icon("grid")));
    if docs.is_empty() {
        let clear = |class| anchor(&with_format(url, "html"), "Clear filters", class);
        if matched > 0 {
            let first_page = format!(
                "{url}{}",
                query.query_string_with_format(search.limit, 0, "html")
            );
            let match_verb = if matched == 1 { "matches" } else { "match" };
            body.push_str(&format!("<div class=\"empty-state\"><h2>This page is outside the results.</h2><p>{matched} {collection_noun} {match_verb}. Return to the first page to keep these filters.</p>{}</div>",anchor(&first_page,"Go to first page","btn primary")));
        } else if let Some(words) = query.q.as_deref().and_then(comma_alternatives) {
            // Words match as one phrase: offer the same search for any of
            // them, the other filters kept (#1141).
            let any = query_edit(&page_url, "q", Some(&words));
            body.push_str(&format!("<div class=\"empty-state\"><h2>No collections match these filters.</h2><p>The words of a search match together, as one exact phrase. Search for any of them instead, or clear the filters.</p><div class=\"empty-actions\">{}{}</div></div>",anchor(&any,&format!("Search any of: {}",words.replace(',', ", ")),"btn primary"),clear("btn")));
        } else {
            // Advice for the filters the request gave: words only when it
            // searched for some (#1141).
            let given = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.trim().is_empty());
            let advice = if given(&query.q) || given(&query.query) {
                Some("The search matches whole words in titles, descriptions and keywords, not ids. Remove a word or a filter, or clear them all.")
            } else if given(&query.bbox) || given(&query.datetime) {
                Some("Widen the area or the time, or clear the filters.")
            } else {
                None
            };
            match advice {
                Some(advice) => body.push_str(&format!("<div class=\"empty-state\"><h2>No collections match these filters.</h2><p>{advice}</p><div class=\"empty-actions\">{}</div></div>",clear("btn primary"))),
                None => body.push_str("<div class=\"empty-state\"><h2>No collections are published here.</h2></div>"),
            }
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
            .map(|k| anchor(&query_edit(&page_url, "q", Some(k)), k, "chip"))
            .collect::<String>();
        let keywords = if keywords.is_empty() {
            keywords
        } else {
            format!("<div class=\"row-line\"><span class=\"row-label\">Keywords</span><span class=\"chip-row\">{keywords}</span></div>")
        };
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
        let description = doc["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .map(|d| format!("<p>{}</p>", escape(d)))
            .unwrap_or_default();
        // The title is the row's only link to the collection page: the
        // arrow repeats it for pointers and is skipped by the keyboard.
        body.push_str(&format!("<article class=\"collection-row\"><div class=\"collection-icon\">{}</div><div class=\"collection-main\"><h3>{}</h3><span class=\"mono\">{}</span>{description}{}{keywords}<div class=\"row-foot\">{}{}</div></div><a class=\"row-arrow\" href=\"{}\" tabindex=\"-1\" aria-hidden=\"true\">{}</a></article>",icon(symbol),anchor(&href,title,""),escape(id),collection_facts(doc),license_html(view.license),entry_details(doc),escape(&href),icon("arrow")));
    }
    body.push_str("</div><div class=\"pagination\">");
    body.push_str("<label class=\"per-page enhanced\">Per page<select data-page-size>");
    let mut sizes = vec![12, HTML_PAGE_SIZE, 48, 96, search.limit];
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
    body.push_str(
        "</div><section class=\"panel spaced\"><div class=\"panel-head\"><h2>Links</h2></div>",
    );
    body.push_str(&link_table(links));
    body.push_str("</section></section></div>");
    Page {
        surface,
        title: "Collections",
        json_url: &json_url,
    }
    .render(&body, "")
}

/// An items page's title, from the page's query string and its item
/// count: the item range and whether a filter narrows it
/// (`Items 101–110 · filtered`), so pages differ in tabs and history
/// (#1131). Paging, ordering, column, CRS and representation controls
/// filter nothing. Features and EDR `items` share it.
pub fn items_title(query: &str, count: usize) -> String {
    let mut offset = 0usize;
    let mut filtered = false;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "offset" => offset = value.parse().unwrap_or(0),
            "f" | "limit" | "sortby" | "properties" | "crs" | "bbox-crs" | "filter-lang"
            | "filter-crs" => {}
            _ => filtered = true,
        }
    }
    let range = if count == 0 {
        "Items".to_owned()
    } else {
        format!(
            "Items {}–{}",
            offset.saturating_add(1),
            offset.saturating_add(count)
        )
    };
    if filtered {
        format!("{range} · filtered")
    } else {
        range
    }
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

/// A run card's valid-time span and step count, from the run's temporal
/// extent: its `values`, else an advertised grid's `cellsCount`. A fact the
/// run does not advertise is left out; with neither, so is the list.
fn run_facts(temporal: &Value) -> String {
    let mut facts = String::new();
    if temporal["interval"][0].is_array() {
        // The span names its time zone itself.
        facts.push_str(&format!(
            "<div><dt>Valid time</dt><dd>{}</dd></div>",
            collection_time_html(temporal)
        ));
    }
    let steps = temporal["values"]
        .as_array()
        .filter(|values| !values.is_empty())
        .map(|values| values.len() as u64)
        .or_else(|| temporal["grid"]["cellsCount"].as_u64())
        .filter(|steps| *steps > 0);
    if let Some(steps) = steps {
        facts.push_str(&format!(
            "<div><dt>Time steps</dt><dd>{steps} {}</dd></div>",
            if steps == 1 { "step" } else { "steps" }
        ));
    }
    if facts.is_empty() {
        facts
    } else {
        format!("<dl class=\"collection-facts run-facts\">{facts}</dl>")
    }
}

/// Model-run navigation uses the same shell without claiming collection search.
/// `doc` is the instances list document, the JSON representation's value:
/// each of its `instances` is a card with the instance's title, id,
/// description and data query anchors, and all of its metadata and links
/// under the card; the document's own links close the page (OGC API - EDR
/// `/req/html/content` A).
pub fn instances_html(surface: Surface<'_>, title: &str, doc: &Value, nav: &[LinkView]) -> String {
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
    let instances = doc["instances"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for instance in instances {
        let id = instance["id"].as_str().unwrap_or_default();
        let title = instance["title"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(id);
        // The JSON `title` is the collection's on every run (EDR 1.2
        // `/req/instances/src-md-success` C), so the card heading takes the
        // run's own label from its self link's `title`.
        let heading = instance["links"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["rel"] == "self")
            .and_then(|l| l["href"].as_str().map(|href| (href, l["title"].as_str())))
            .map(|(href, label)| {
                let label = label.filter(|t| !t.trim().is_empty()).unwrap_or(title);
                anchor(&with_format(href, "html"), label, "")
            })
            .unwrap_or_else(|| escape(title));
        // One full-width column: a run card is not a catalog row with an
        // icon, a side column and an arrow (#1131).
        body.push_str(&format!(
            "<article class=\"collection-row instance-row\"><div class=\"collection-main\"><h2>{heading}</h2><code>{}</code><p>{}</p>{}<div class=\"chip-row\">{}</div>{}</div></article>",
            escape(id),
            escape(instance["description"].as_str().unwrap_or_default()),
            run_facts(&instance["extent"]["temporal"]),
            data_query_links(instance),
            entry_details(instance)
        ));
    }
    if instances.is_empty() {
        body.push_str("<p class=\"panel\">No model runs available.</p>");
    }
    body.push_str("</div>");
    // A long run axis pages (EDR, #1006): the pager follows `prev`/`next`.
    if nav.iter().any(|l| l.rel == "prev" || l.rel == "next") {
        body.push_str(&pagination(nav));
    }
    body.push_str("<section class=\"panel spaced\"><div class=\"panel-head\"><h2>Links</h2></div>");
    body.push_str(&link_table(&doc["links"]));
    body.push_str("</section>");
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

    /// A rejected catalog search is the form again (#1132): the values the
    /// request gave, the field at fault marked with the message, its
    /// filters open; `datetime` shows the forms it takes.
    #[test]
    fn rejected_catalog_search_marks_the_field_at_fault() {
        let pairs: Vec<(String, String)> = [
            ("q", "radar"),
            ("datetime", "yesterday <b>"),
            ("limit", "5"),
            ("f", "html"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec();
        let html = rejected_collections_html(
            MAPS_PROXY,
            &pairs,
            Some("datetime"),
            "Invalid datetime: <x>",
        );
        let field = html
            .split("data-param=\"datetime\"")
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .unwrap();
        assert!(html.contains("<input aria-invalid=\"true\" aria-errormessage=\"datetime-error\" aria-describedby=\"datetime-error\" data-param=\"datetime\""), "{html}");
        assert!(field.contains("value=\"yesterday &lt;b&gt;\""), "{field}");
        assert!(html.contains("<span id=\"datetime-error\" class=\"error-text field-error\">Invalid datetime: &lt;x&gt;</span></label>"));
        assert!(html.contains(
            "UTC instant or interval, e.g. 2026-10-17T00:00:00Z/.. or 2026-10-16T00:00:00Z/2026-10-17T00:00:00Z"
        ));
        assert!(html.contains("data-disclosure=\"collection-search\" open"));
        assert!(html.contains("value=\"radar\""));
        assert!(html.contains("This search could not run."));
        assert!(html.contains("<p>Check <code>datetime</code> and search again.</p>"));
        // The JSON switch is the same search, as JSON.
        assert!(html.contains(
            "https://example.test/proxy/maps/collections?q=radar&amp;datetime=yesterday+%3Cb%3E&amp;limit=5&amp;f=json"
        ));
        // A parameter without a field of its own is named above them.
        let html = rejected_collections_html(
            MAPS_PROXY,
            &[("typo".into(), "1".into())],
            Some("typo"),
            "Unknown collection query parameter 'typo'",
        );
        assert!(html.contains(
            "<p class=\"error-text field-error\" role=\"alert\">Unknown collection query parameter &#39;typo&#39;</p>"
        ));
        assert!(!html.contains("aria-errormessage"));
    }

    /// Links that open something other than an HTML page say what they
    /// open (#1132); HTML views and human relations do not.
    #[test]
    fn non_html_links_carry_a_media_type_badge() {
        assert_eq!(media_badge("text/html"), "");
        assert_eq!(media_badge(""), "");
        assert_eq!(media_badge("text/html; charset=utf-8"), "");
        assert!(media_badge("application/json").contains(">JSON</span>"));
        assert!(media_badge("application/vnd.oai.openapi+json;version=3.0")
            .contains(">OpenAPI JSON</span>"));
        assert!(media_badge("image/<svg>").contains(">image/&lt;svg&gt;</span>"));
        let links = json!([
            {"href": "https://x/collections/a/styles", "rel": "styles", "type": "application/json", "title": "Styles"},
            {"href": "https://x/collections/a/items", "rel": "items", "type": "application/geo+json", "title": "Items"},
            {"href": "https://x/collections/a?f=html", "rel": "alternate", "type": "text/html", "title": "HTML"},
        ]);
        let table = link_table(&links);
        assert_eq!(
            table.matches("class=\"chip media-type\"").count(),
            2,
            "{table}"
        );
        assert!(table.contains(">GeoJSON</span>"));
        let resources = document_links(&json!({"links": links}));
        // Styles open JSON; items open their HTML page.
        assert_eq!(
            resources.matches("class=\"chip media-type\"").count(),
            1,
            "{resources}"
        );
        assert!(resources.contains("<strong>Styles</strong><span class=\"chip media-type\""));
    }

    /// The landing page's API definition row opens the API reference; the
    /// raw OpenAPI document stays a secondary link (#1132).
    #[test]
    fn landing_resources_open_the_api_reference() {
        let doc = json!({"links": [
            {"href": "https://example.test/proxy/maps/api", "rel": "service-desc", "type": "application/vnd.oai.openapi+json;version=3.0", "title": "API definition"},
            {"href": "https://example.test/proxy/maps/api/docs", "rel": "service-doc", "type": "text/html", "title": "API documentation"},
            {"href": "https://example.test/proxy/maps/collections", "rel": "data", "type": "application/json", "title": "Collections"},
        ]});
        let html = landing_document(MAPS_PROXY, "Maps", "", &doc);
        assert!(html.contains(
            "<a class=\"resource-row\" href=\"https://example.test/proxy/maps/api/docs\"><span class=\"method\">GET</span><code>/maps/api/docs</code><span>API reference</span>"
        ), "{html}");
        assert!(html.contains("Raw OpenAPI definition: <a"));
        assert!(html.contains("href=\"https://example.test/proxy/maps/api\""));
        assert!(!html.contains("<span>API definition"));
        // Without a reference page the row keeps the definition, badged.
        let doc = json!({"links": [doc["links"][0].clone()]});
        let html = landing_document(MAPS_PROXY, "Maps", "", &doc);
        assert!(
            html.contains("<span>API definition<span class=\"chip media-type\""),
            "{html}"
        );
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
        let html = collections_html(MAPS_PROXY, &query, &search, 3, &[], &[], &json!([]));
        assert!(html.contains("3 matching collections"));
        assert!(html.contains("This page is outside the results."));
        assert!(html.contains("No page at offset 1000"));
        assert!(!html.contains("0–1000"));
        assert!(!html.contains("Page 84 of 1"));
        assert!(!html.contains("No collections match these filters."));
        let first = format!("{url}{}", query.query_string_with_format(12, 0, "html"));
        assert!(html.contains(&format!("href=\"{}\">Go to first page", escape(&first))));
        let empty = collections_html(MAPS_PROXY, &query, &search, 0, &[], &[], &json!([]));
        assert!(empty.contains("No collections match these filters."));
        assert!(empty.contains("0 results"));
        assert!(!empty.contains("Go to first page"));
    }

    /// The empty state's advice names the filters the request gave: words
    /// only for a text search, the area or time for the others (#1141).
    #[test]
    fn catalog_empty_state_advice_follows_the_given_filters() {
        let empty = |pairs: Vec<(&str, &str)>| {
            let query = SearchQueryParams::from_pairs(
                pairs
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
            )
            .unwrap();
            let html = collections_html(
                MAPS_PROXY,
                &query,
                &query.parse().unwrap(),
                0,
                &[],
                &[],
                &json!([]),
            );
            html.split("<div class=\"empty-state\">")
                .nth(1)
                .unwrap()
                .split("</div>")
                .next()
                .unwrap()
                .to_owned()
        };
        let words = "The search matches whole words";
        for text in [
            vec![("q", "radar")],
            vec![("query", "+radar"), ("bbox", "1,2,3,4")],
        ] {
            let state = empty(text);
            assert!(state.contains(words), "{state}");
            assert!(state.contains("No collections match these filters."));
        }
        for area_or_time in [
            vec![("bbox", "1,2,3,4")],
            vec![("datetime", "2026-10-17T00:00:00Z")],
            vec![("q", " "), ("bbox", "1,2,3,4")],
        ] {
            let state = empty(area_or_time);
            assert!(!state.contains(words), "{state}");
            assert!(state.contains("Widen the area or the time, or clear the filters."));
        }
        let none = empty(vec![("limit", "5")]);
        assert_eq!(none, "<h2>No collections are published here.</h2>");
    }

    /// A catalog row (#1141): the facts in fixed slots, a dash where the
    /// collection advertises nothing; parameter labels, with the full list
    /// under a disclosure rather than a tooltip; data queries as action
    /// chips of their own; a labelled keyword row; the title as the row's
    /// only keyboard link to the collection page; the count as the results'
    /// heading.
    #[test]
    fn catalog_rows_have_fixed_fact_slots_and_one_collection_link() {
        let observed = |label: &str| json!({"observedProperty": {"label": {"en": label}}});
        // Keys in name order, as `parameter_names` lists them.
        let rich = json!({
            "id": "rich", "title": "Rich", "description": "Every fact", "keywords": ["radar"],
            "extent": {
                "temporal": {"interval": [["2026-10-17T00:00:00Z", "2026-10-17T01:00:00Z"]],
                             "grid": {"resolution": "PT5M", "cellsCount": 13}},
                "spatial": {"bbox": [[20.0, 60.0, 30.0, 70.0]]}
            },
            "parameter_names": {"p": {"label": "Pressure"}, "t": observed("Temperature"),
                                "u": observed("Wind u"), "v": observed("Wind v"), "x": {}},
            "data_queries": {"position": {"link": {"href": "https://example.test/proxy/edr/collections/rich/position"}}},
            "links": []
        });
        let bare = json!({"id": "bare", "title": " ", "links": []});
        let query = SearchQueryParams::default();
        let search = query.parse().unwrap();
        let views = [
            CollectionView {
                metadata: &rich,
                license: None,
            },
            CollectionView {
                metadata: &bare,
                license: None,
            },
        ];
        let html = collections_html(MAPS_PROXY, &query, &search, 2, &views, &[], &json!([]));
        assert!(html.contains("<h2 id=\"results-count\">2 matching collections</h2>"));
        assert!(html.contains("<small>1–2 shown · Title A–Z</small>"));
        assert!(
            html.contains("<select data-sort><option value=\"title\" selected>Title A–Z</option>")
        );
        // The JSON switch names the same page: its order spelled out.
        let json_link = html.split("id=\"json-link\" href=\"").nth(1).unwrap();
        assert!(json_link.starts_with(
            "https://example.test/proxy/maps/collections?limit=1000&amp;f=json&amp;sortby=title\""
        ));
        let rows: Vec<&str> = html
            .split("<article class=\"collection-row\">")
            .skip(1)
            .collect();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let slots: Vec<&str> = row
                .split("<dt>")
                .skip(1)
                .map(|dt| dt.split("</dt>").next().unwrap())
                .collect();
            assert_eq!(
                slots,
                [
                    "Time · UTC",
                    "Cadence",
                    "Area · CRS84",
                    "Vertical",
                    "Parameters"
                ]
            );
            // The arrow repeats the title's link for pointers, out of the
            // tab order.
            assert_eq!(
                row.matches("tabindex=\"-1\" aria-hidden=\"true\"").count(),
                1
            );
            assert!(!row.contains("aria-label=\"Open"));
        }
        let (rich, bare) = (rows[0], rows[1]);
        assert!(rich.contains("<dd>Every 5 min · 13 time steps</dd>"));
        assert!(rich.contains("20.00, 60.00 → 30.00, 70.00"));
        assert!(rich.contains(&format!("<dt>Vertical</dt><dd>{EMPTY_SLOT}</dd>")));
        let parameters = rich.split("<dt>Parameters</dt><dd>").nth(1).unwrap();
        let parameters = parameters.split("</dd>").next().unwrap();
        assert!(parameters.starts_with("<details class=\"slot-list\"><summary>Pressure, Temperature, Wind u <span class=\"slot-more\">+2 more</span></summary>"), "{parameters}");
        assert!(parameters.contains("<li>Wind v <code>v</code></li>"));
        // A parameter without a label is listed by its name.
        assert!(parameters.contains("<li>x</li>"));
        assert!(!parameters.contains("title="));
        // The query chip opens the query's HTML view, its form (#1138).
        assert!(rich.contains("<div class=\"row-line query-chips\"><span class=\"row-label\">Data queries</span><span class=\"chip-row\"><a class=\"chip\" href=\"https://example.test/proxy/edr/collections/rich/position?f=html\">position</a>"));
        assert!(rich.contains("<span class=\"row-label\">Keywords</span>"));
        // Nothing advertised: five dashes, and the id stands in for a title.
        assert_eq!(bare.matches(EMPTY_SLOT).count(), 5);
        assert!(bare.contains("<h3><a class=\"\" href=\"https://example.test/proxy/maps/collections/bare?f=html\">bare</a></h3>"));
        assert!(!bare.contains("row-label"));
        // Up to three labels need no disclosure.
        let few = json!({"parameter_names": {"a": {"label": "Alpha"}, "b": {}}});
        assert!(collection_facts(&few).contains("<dt>Parameters</dt><dd>Alpha, b</dd>"));
        // Styles fill the slot when there are no parameters.
        let styled = json!({"styles": [{"id": "default", "title": "Default"}, {"id": "dbz"}]});
        assert!(collection_facts(&styled).contains("<dt>Styles</dt><dd>Default, dbz</dd>"));
    }

    /// The search says what `q` does (#1141): the fields it reads, words as
    /// one phrase, commas between alternatives. A multi-word search that
    /// found nothing offers the same search for any of its words, the other
    /// filters kept; placeholders read as examples.
    #[test]
    fn catalog_search_hints_say_what_q_does() {
        let query = SearchQueryParams::from_pairs(vec![
            ("q".into(), "radar composite, gfs".into()),
            ("bbox".into(), "1,2,3,4".into()),
        ])
        .unwrap();
        let search = query.parse().unwrap();
        let html = collections_html(MAPS_PROXY, &query, &search, 0, &[], &[], &json!([]));
        assert!(html.contains("Searches titles, descriptions and keywords for whole words, not ids. Words match as an exact phrase; separate alternatives with commas."));
        assert!(html.contains("<a class=\"btn primary\" href=\"https://example.test/proxy/maps/collections?bbox=1%2C2%2C3%2C4&amp;q=radar%2Ccomposite%2Cgfs&amp;f=html\">Search any of: radar, composite, gfs</a>"), "{html}");
        assert!(!html.contains("Try a broader search"));
        for (i, hint) in ["e.g. 18", "e.g. 58", "e.g. 32", "e.g. 71"]
            .iter()
            .enumerate()
        {
            assert!(html.contains(&format!("data-bbox=\"{i}\" type=\"number\" step=\"any\" value=\"{}\" placeholder=\"{hint}\"", i + 1)), "{hint}");
        }
        // One word per alternative: nothing to split, so no offer.
        let single = SearchQueryParams::from_pairs(vec![("q".into(), "radar,gfs".into())]).unwrap();
        let html = collections_html(
            MAPS_PROXY,
            &single,
            &single.parse().unwrap(),
            0,
            &[],
            &[],
            &json!([]),
        );
        assert!(!html.contains("Search any of"));
        assert!(html.contains("not ids"));
        assert_eq!(
            comma_alternatives(" radar  composite ,gfs"),
            Some("radar,composite,gfs".into())
        );
        assert_eq!(comma_alternatives("radar, gfs"), None);
        let landing = landing_document(MAPS_PROXY, "Maps", "", &json!({"links": []}));
        assert!(landing.contains("placeholder=\"e.g. radar or GFS\""));
    }

    /// The order controls (#1141): the select offers title and id, either
    /// way, and keeps an order it does not offer; a rejected order is shown
    /// as typed, marked; a rejected page keeps the HTML page size.
    #[test]
    fn catalog_sort_controls_name_the_order() {
        let keys = [
            CollectionSortKey {
                property: CollectionSortProperty::Title,
                descending: true,
            },
            CollectionSortKey::ascending(CollectionSortProperty::Id),
        ];
        assert_eq!(sort_label(&keys), "Title Z–A, then ID A–Z");
        assert!(sort_options("-title,id").starts_with(
            "<option value=\"-title,id\" selected>-title,id</option><option value=\"title\">"
        ));
        assert!(sort_options("-id").contains("<option value=\"-id\" selected>ID Z–A</option>"));
        let html = rejected_collections_html(
            MAPS_PROXY,
            &[("sortby".into(), "updated".into())],
            Some("sortby"),
            "Unknown sortby property 'updated'; sortable: title, id",
        );
        assert!(html.contains("<span>Order <code>sortby</code></span>"));
        assert!(html.contains("aria-errormessage=\"sortby-error\""));
        assert!(html.contains("value=\"updated\""));
        assert!(html.contains(&format!(
            "name=\"limit\" type=\"number\" value=\"{HTML_PAGE_SIZE}\""
        )));
        // Without a fault in it, the order is a select holding the request's.
        let html = rejected_collections_html(
            MAPS_PROXY,
            &[("sortby".into(), "+id".into()), ("typo".into(), "1".into())],
            Some("typo"),
            "Unknown collection query parameter 'typo'",
        );
        assert!(html.contains("<select name=\"sortby\">"));
        assert!(html.contains("<option value=\"id\" selected>ID A–Z</option>"));
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

    /// #1220: the map's time axis is compact, `{start, step, count}` for a
    /// regular grid and the list for any other, read from the advertised
    /// grid only. Nothing is inferred from the bounds alone.
    #[test]
    fn map_axis_encodes_regular_grids_compactly_and_lists_irregular_ones() {
        let mut temporal = json!({"interval":[["2026-09-18T00:00:00Z","2026-09-18T06:00:00Z"]]});
        assert_eq!(map_axis(&temporal), None);
        temporal["grid"] = json!({"cellsCount":3,"resolution":"PT3H"});
        assert_eq!(
            map_axis(&temporal),
            Some(json!({"start":"2026-09-18T00:00:00Z","step":10800,"count":3}))
        );
        // The grid's first coordinate wins over the interval start, and an
        // offset is written in UTC with `Z`.
        temporal["grid"] = json!({"cellsCount":2,"resolution":"PT3H","firstCoordinate":"2026-09-18T05:00:00+02:00"});
        assert_eq!(
            map_axis(&temporal),
            Some(json!({"start":"2026-09-18T03:00:00Z","step":10800,"count":2}))
        );
        // 288 five-minute steps or far more stay three numbers.
        temporal["interval"] = json!([["2026-09-18T00:00:00Z", "2026-10-18T00:00:00Z"]]);
        temporal["grid"] = json!({"cellsCount":8641,"resolution":"PT5M"});
        assert_eq!(map_axis(&temporal).unwrap()["count"], 8641);
        assert!(map_axis(&temporal).unwrap().to_string().len() < 64);
        temporal["grid"] = json!({"cellsCount":100_001,"resolution":"PT1S"});
        assert_eq!(map_axis(&temporal), None);
        temporal["interval"] = json!([["2026-09-18T00:00:00Z", "2026-09-18T06:00:00Z"]]);
        // A grid reaching past the interval end is no grid of it.
        temporal["grid"] = json!({"cellsCount":3,"resolution":"P1D"});
        assert_eq!(map_axis(&temporal), None);
        temporal["grid"] = json!({"cellsCount":3,"resolution":"PT0S"});
        assert_eq!(map_axis(&temporal), None);
        // An irregular grid: its coordinates, in UTC with `Z`.
        temporal["grid"] = json!({"cellsCount":3,"coordinates":["2026-09-18T00:00:00Z","2026-09-18T03:00:00+02:00","2026-09-18T06:00:30Z"]});
        assert_eq!(
            map_axis(&temporal),
            Some(
                json!({"times":["2026-09-18T00:00:00Z","2026-09-18T01:00:00Z","2026-09-18T06:00:30Z"]})
            )
        );
        temporal["grid"] = json!({"cellsCount":2,"coordinates":["invalid","2026-09-18T01:00:00Z"]});
        assert_eq!(map_axis(&temporal), None);
        temporal["grid"] = json!({"cellsCount":3,"coordinates":["2026-09-18T00:00:00Z"]});
        assert_eq!(map_axis(&temporal), None);
        // An interval of one instant is that instant; an open one is none.
        let one = json!({"interval":[["2026-09-18T00:00:00Z","2026-09-18T00:00:00Z"]]});
        assert_eq!(
            map_axis(&one),
            Some(json!({"times":["2026-09-18T00:00:00Z"]}))
        );
        assert_eq!(
            map_axis(&json!({"interval":[["2026-09-18T00:00:00Z",null]]})),
            None
        );
        // The form's bounds come from either form.
        let regular = json!({"start":"2026-09-18T00:00:00Z","step":10800,"count":3});
        let (first, last, step) = axis_ends(&regular).unwrap();
        assert_eq!(
            (
                crate::subset::rfc3339(first),
                crate::subset::rfc3339(last),
                step
            ),
            (
                "2026-09-18T00:00:00Z".into(),
                "2026-09-18T06:00:00Z".into(),
                Some(10800)
            )
        );
        assert_eq!(axis_len(&regular), 3);
        let listed = json!({"times":["2026-09-18T00:00:00Z","2026-09-18T06:00:30Z"]});
        assert_eq!(axis_ends(&listed).unwrap().2, None);
        assert_eq!(axis_len(&listed), 2);
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
                {"rel":rel::MAP,"href":"https://example.test/maps/collections/levels/map"}]});
            let facts = collection_facts(&doc);
            let slot = facts.split("<dt>Vertical</dt><dd>").nth(1).unwrap();
            let slot = slot.split("</dd>").next().unwrap();
            assert!(
                slot.contains("100–1000 hPa") || slot.contains("Model level: 1–137"),
                "{facts}"
            );
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
            json!({"id":"single","links":[{"rel":rel::MAP,"href":"/maps/collections/single/map"}]});
        assert!(collection_facts(&doc).contains(&format!("<dt>Vertical</dt><dd>{EMPTY_SLOT}</dd>")));
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
            media_type: None,
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
        assert!(html.contains(
            "Storage coordinate system</dt><dd><span class=\"muted\">Not advertised</span></dd>"
        ));
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
            {"rel":rel::MAP,"href":format!("{base}/maps/collections/a/map")}
        ],"styles":[{"title":"Rain & snow","links":[{"rel":rel::MAP,"href":format!("{base}/maps/collections/a/styles/rain/map")}]},{"title":"Unsafe","links":[{"rel":rel::MAP,"href":"javascript:alert(1)"}]}]});
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
            .retain(|l| l["rel"] != rel::MAP);
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
        // Other superseded per-API services are not offered (#789).
        assert!(html.contains("value=\"https://example.test/edr/?f=html\""));
        assert!(!html.contains("value=\"https://example.test/maps/?f=html\""));
        // Map request parameters are documented by the API serving the map.
        assert!(html.contains("href=\"https://example.test/maps/api/docs\">Map request parameters"));
        // The conformance page is named for what it lists (#1140).
        let conformance = html
            .split("href=\"https://example.test/relocated/features/conformance?f=html\">")
            .nth(1)
            .and_then(|rest| rest.split("</a>").next())
            .unwrap_or_else(|| panic!("no conformance nav link: {html}"));
        assert!(conformance.ends_with("Conformance"), "{conformance}");
        assert!(!html.contains("Standards</a>"), "{html}");
    }

    #[test]
    fn shared_root_switcher_omits_superseded_per_api_services() {
        let html = landing_html(
            Surface {
                base: "https://example.test",
                root: "https://example.test",
                api: crate::shared::WORKSPACE,
            },
            "MeteoCore",
            "",
            &[],
        );
        assert!(html.contains("value=\"https://example.test/?f=html\" selected"));
        assert!(html.contains("value=\"https://example.test/edr/?f=html\""));
        for mount in [mounts::FEATURES, mounts::MAPS, mounts::TILES] {
            assert!(
                !html.contains(&format!("https://example.test{mount}/")),
                "{mount}"
            );
        }
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

    /// The heading lists every OGC API the collection advertises, from its
    /// links rather than from the API serving the page.
    #[test]
    fn heading_chips_name_every_advertised_api() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let chips = |doc: &Value| {
            let html = collection_html(shared, doc, None);
            let row = html
                .split("aria-label=\"Available through\">")
                .nth(1)
                .unwrap()
                .split("</div>")
                .next()
                .unwrap()
                .to_owned();
            ["Maps", "Tiles", "Features", "EDR", crate::shared::WORKSPACE]
                .into_iter()
                .filter(|label| row.contains(&format!(">{label}</span>")))
                .collect::<Vec<_>>()
        };
        let all = json!({"id":"a","links":[
            {"rel":"self","href":"https://x/collections/a"},
            {"rel":rel::MAP,"href":"https://x/collections/a/map"},
            {"rel":rel::TILESETS_VECTOR,"href":"https://x/collections/a/tiles"},
            {"rel":"items","href":"https://x/collections/a/items","type":"application/geo+json"}]});
        assert_eq!(chips(&all), ["Maps", "Tiles", "Features"]);
        let features = json!({"id":"b","itemType":"feature","links":[
            {"rel":"items","href":"https://x/collections/b/items","type":"application/geo+json"}]});
        assert_eq!(chips(&features), ["Features"]);
        // The kind is a detail row, not a second "feature" chip.
        let html = collection_html(shared, &features, None);
        let row = html
            .split("aria-label=\"Available through\">")
            .nth(1)
            .unwrap();
        assert!(!row.split("</div>").next().unwrap().contains(">feature<"));
        assert!(html.contains(
            "<dt>Data type</dt><dd>Features<small class=\"term\">itemType <code>feature</code></small></dd>"
        ));
        let edr = json!({"id":"c","links":[],"data_queries":{"position":{}}});
        assert_eq!(chips(&edr), ["EDR"]);
        // Nothing advertised: the serving API still labels the page.
        assert_eq!(
            chips(&json!({"id":"d","links":[]})),
            [crate::shared::WORKSPACE]
        );
    }

    /// With both Maps and Tiles (and vector tiles) the preview offers a choice
    /// of what draws it, map tiles first and by default (#1220); each style
    /// carries its own map-tileset list.
    #[test]
    fn map_preview_offers_maps_map_tiles_and_vector_tiles() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = json!({"id":"cap","extent":{"spatial":{"bbox":[[20,60,30,70]]}},
            "links":[
                {"rel":"self","href":"https://x/collections/cap"},
                {"rel":rel::MAP,"href":"https://x/collections/cap/map"},
                {"rel":rel::TILESETS_MAP,"href":"https://x/collections/cap/map/tiles"},
                {"rel":rel::TILESETS_VECTOR,"href":"https://x/collections/cap/tiles"}],
            "styles":[{"id":"severity","title":"Severity","links":[
                {"rel":rel::MAP,"href":"https://x/collections/cap/styles/severity/map"},
                {"rel":rel::TILESETS_MAP,"href":"https://x/collections/cap/styles/severity/map/tiles"}]}]});
        let html = collection_html(shared, &doc, None);
        let switch = html
            .split("<fieldset class=\"map-mode map-choice enhanced\">")
            .nth(1)
            .unwrap()
            .split("</fieldset>")
            .next()
            .unwrap();
        let order: Vec<usize> = ["tiles", "maps", "vector"]
            .iter()
            .map(|mode| {
                switch
                    .find(&format!("name=\"map-mode\" value=\"{mode}\""))
                    .unwrap_or_else(|| panic!("{mode}"))
            })
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{switch}");
        assert!(switch.contains("value=\"tiles\" checked"));
        assert_eq!(switch.matches(" checked").count(), 1);
        // The visible label and the buttons are one grid row inside a div:
        // Safari never lays a fieldset's legend out as a grid item, so a
        // legend in the label column squeezed the buttons into it. The
        // legend stays for assistive technology only.
        assert!(switch.starts_with(
            "<legend class=\"visually-hidden\">Draw with</legend><div class=\"map-choice-row\"><span class=\"map-choice-label\" aria-hidden=\"true\">Draw with</span><div class=\"map-segmented\">"
        ));
        let css = include_str!("workbench/map.css");
        assert!(!css.contains("map-choice legend"));
        assert!(css.contains(".map-choice{display:block;"));
        // Style ids name a style in the fragment.
        assert!(html.contains("data-id=\"default\""));
        assert!(html.contains("data-id=\"severity\""));
        assert!(html.contains("data-tiles=\"https://x/collections/cap/map/tiles\""));
        assert!(html.contains("data-tiles=\"https://x/collections/cap/styles/severity/map/tiles\""));
        let data = html.split("id=\"map-data\" hidden>").nth(1).unwrap();
        let data: Value =
            serde_json::from_str(&data.split("</div>").next().unwrap().replace("&quot;", "\""))
                .unwrap();
        assert_eq!(
            data["mapRequest"]["modes"],
            json!(["tiles", "maps", "vector"])
        );
        assert_eq!(
            data["mapRequest"]["vector"],
            json!({"href":"https://x/collections/cap/tiles","layer":"cap"})
        );

        // One mechanism: no switch. Map tiles alone still get a preview.
        let mut maps_only = doc.clone();
        maps_only["links"].as_array_mut().unwrap().truncate(2);
        let html = collection_html(shared, &maps_only, None);
        assert!(html.contains("id=\"map-controls\""));
        assert!(html.contains("data-mode=\"maps\""));
        assert!(!html.contains("<fieldset class=\"map-mode"));
        let mut tiles_only = doc.clone();
        tiles_only["links"].as_array_mut().unwrap().remove(1);
        tiles_only["links"].as_array_mut().unwrap().truncate(2);
        tiles_only["styles"] = json!([]);
        let html = collection_html(shared, &tiles_only, None);
        assert!(html.contains("id=\"map-controls\""));
        assert!(html.contains("data-mode=\"tiles\""));
        assert!(!html.contains("<fieldset class=\"map-mode"));
        // No Maps endpoint: nothing to GET, so the form is the script's.
        assert!(
            map_controls(&html).contains("<form id=\"map-form\" class=\"map-controls enhanced\">")
        );
        assert!(!map_controls(&html).contains("<noscript>"));
    }

    /// The JSON embedded in a collection page's map panel.
    fn map_data(html: &str) -> Value {
        let data = html
            .split("id=\"map-data\" hidden>")
            .nth(1)
            .unwrap()
            .split("</div>")
            .next()
            .unwrap()
            .replace("&quot;", "\"")
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">");
        serde_json::from_str(&data).unwrap()
    }

    /// The layer card: the pill, the form and the draw-mode switch after
    /// it; the map itself follows the card.
    fn map_controls(html: &str) -> &str {
        let start = html.find("<div id=\"map-controls\"").unwrap();
        let end = start + html[start..].find("<div id=\"feature-map\"").unwrap();
        &html[start..end]
    }

    /// #1136: a multi-parameter collection offers a Parameter select. Its
    /// first choice, Default, sends no `parameter-name`; the others are the
    /// collection's `parameter_names`, labelled with their unit, read from
    /// both the Maps/Tiles and the EDR form.
    #[test]
    fn map_request_carries_parameter_names_and_offers_a_parameter_select() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = json!({"id":"nwp","extent":{"spatial":{"bbox":[[20,60,30,70]]}},
            "links":[{"rel":"self","href":"https://x/collections/nwp"},
                {"rel":rel::MAP,"href":"https://x/collections/nwp/map"}],
            "parameter_names":{
                "t":{"type":"Parameter","observedProperty":{"label":{"en":"Temperature"}},
                    "unit":{"label":{"en":"K"},"symbol":{"value":"K","type":"http://www.opengis.net/def/uom/UCUM/"}}},
                "r":{"type":"Parameter","label":"Relative humidity","unit":{"symbol":"%"}},
                "gh":{"type":"Parameter","observedProperty":{"label":{"en":"Temperature"}}},
                "<x>":{"type":"Parameter"}}});
        let html = collection_html(shared, &doc, None);
        // In the document's order, which depends on serde_json's features.
        let mut parameters = map_data(&html)["mapRequest"]["parameter_names"]
            .as_array()
            .unwrap()
            .clone();
        parameters.sort_by_key(|p| p["name"].as_str().unwrap().to_owned());
        assert_eq!(
            Value::Array(parameters),
            json!([
                {"name":"<x>","label":"<x>","unit":""},
                {"name":"gh","label":"Temperature","unit":""},
                {"name":"r","label":"Relative humidity","unit":"%"},
                {"name":"t","label":"Temperature","unit":"K"}
            ])
        );
        let controls = map_controls(&html);
        assert!(controls.contains(
            "<label>Parameter<select id=\"map-parameter\" name=\"parameter-name\"><option value=\"\">Default</option>"
        ));
        assert!(controls.contains(
            "<option value=\"r\" data-label=\"Relative humidity\" data-unit=\"%\">Relative humidity (%)</option>"
        ));
        // One label for two parameters: the name tells them apart.
        assert!(controls.contains(">Temperature (K) · t</option>"));
        assert!(controls.contains(">Temperature · gh</option>"));
        assert!(controls.contains("<option value=\"&lt;x&gt;\""));
        assert!(!controls.contains("<x>"));
        // The defaults are short and alike.
        assert!(!html.contains("Collection default"));

        // One parameter: nothing to choose.
        let mut single = doc.clone();
        single["parameter_names"] = json!({"t": doc["parameter_names"]["t"]});
        let html = collection_html(shared, &single, None);
        assert_eq!(
            map_data(&html)["mapRequest"]["parameter_names"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(!map_controls(&html).contains("id=\"map-parameter\""));
    }

    /// #1136: the time row exists only for a collection with a time axis.
    /// A source with no data yet, or a static one, advertises no temporal
    /// extent and gets no blank time input.
    #[test]
    fn map_time_row_follows_the_temporal_extent() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let mut doc = json!({"id":"radar","links":[{"rel":"self","href":"https://x/collections/radar"},
            {"rel":rel::MAP,"href":"https://x/collections/radar/map"}]});
        let html = collection_html(shared, &doc, None);
        assert_eq!(map_data(&html)["mapRequest"]["temporal"], false);
        assert!(!map_controls(&html).contains("id=\"map-time\""));
        assert!(!map_controls(&html).contains("map-time-field"));

        // An interval without a grid: a date/time input.
        doc["extent"] = json!({"temporal":{"interval":[["2026-10-17T00:00:00Z",null]]}});
        let html = collection_html(shared, &doc, None);
        assert_eq!(map_data(&html)["mapRequest"]["temporal"], true);
        assert!(map_controls(&html)
            .contains("<input id=\"map-time\" name=\"datetime\" type=\"datetime-local\""));

        // No axis: no time bar either.
        assert!(!html.contains("id=\"map-timebar\""));

        // A grid (#1220): one date and time bounded by the axis, never an
        // option per step, and the time bar under the map for the script.
        doc["extent"]["temporal"] = json!({"interval":[["2026-10-17T17:00:00Z","2026-10-17T17:10:30Z"]],
            "grid":{"cellsCount":2,"coordinates":["2026-10-17T17:00:00Z","2026-10-17T17:10:30Z"]}});
        let html = collection_html(shared, &doc, None);
        let controls = map_controls(&html);
        assert!(controls.contains("<div class=\"map-time-field\"><label for=\"map-time\">Time · UTC</label><input id=\"map-time\" name=\"datetime\" type=\"datetime-local\" min=\"2026-10-17T17:00:00\" max=\"2026-10-17T17:10:30\" step=\"1\" aria-describedby=\"map-help\"></div>"), "{controls}");
        assert!(!controls.contains("<option value=\"2026-10-17T17:00:00Z\""));
        assert!(!controls.contains("map-time-prev"));
        assert_eq!(
            map_data(&html)["mapRequest"]["axis"],
            json!({"times":["2026-10-17T17:00:00Z","2026-10-17T17:10:30Z"]})
        );
        let panel = html
            .split("<div class=\"map-panel map-raster\">")
            .nth(1)
            .unwrap();
        assert!(panel.contains("<div id=\"map-timebar\" class=\"map-timebar map-card enhanced\" role=\"group\" aria-label=\"Time\" hidden>"));
        assert!(panel.contains("<input id=\"map-playhead\" type=\"range\""));
        // A regular grid steps the input by its step.
        doc["extent"]["temporal"] = json!({"interval":[["2026-10-17T00:00:00Z","2026-10-17T23:55:00Z"]],
            "grid":{"cellsCount":288,"resolution":"PT5M"}});
        let html = collection_html(shared, &doc, None);
        assert!(map_controls(&html)
            .contains(" min=\"2026-10-17T00:00:00\" max=\"2026-10-17T23:55:00\" step=\"300\""));
        assert!(html.contains("The time bar plays the 288 advertised times."));
        assert_eq!(
            map_controls(&html).matches("<option value=\"2026").count(),
            0
        );
    }

    /// #1136: the panel says where the page lists the same facts when the
    /// browser cannot draw the map; the legend sits under the map, inside
    /// the panel; a vector-only collection's controls are hidden before any
    /// script runs.
    #[test]
    fn map_panel_fallback_legend_and_vector_only_controls() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = json!({"id":"radar","extent":{"spatial":{"bbox":[[20,60,30,70]]}},
            "links":[{"rel":"self","href":"https://x/collections/radar"},
                {"rel":rel::MAP,"href":"https://x/collections/radar/map"}]});
        let html = collection_html(shared, &doc, None);
        assert!(html.contains(&format!("data-fallback=\"{MAP_EXTENT_FALLBACK}\"")));
        assert!(html.contains(&format!(
            "<p id=\"map-status\" role=\"status\">The map needs JavaScript and WebGL. {MAP_EXTENT_FALLBACK}</p>"
        )));
        assert!(!html.contains("coordinates remain available"));
        // Without a spatial extent the page lists no extent, only requests:
        // the sentence names the section that does, and that section exists.
        let mut bare = doc.clone();
        bare.as_object_mut().unwrap().remove("extent");
        let bare_html = collection_html(shared, &bare, None);
        assert!(bare_html.contains(&format!("data-fallback=\"{MAP_REQUEST_FALLBACK}\"")));
        assert!(!bare_html.contains(MAP_EXTENT_FALLBACK));
        assert!(bare_html.contains("<h2>Request data from this collection</h2>"));
        let panel = html
            .split("<div class=\"map-panel map-raster\">")
            .nth(1)
            .unwrap();
        let panel = panel.split("<script").next().unwrap();
        assert!(panel.contains("<div id=\"map-legend\" class=\"map-legend map-card enhanced\">"));
        assert_eq!(html.matches("id=\"map-legend\"").count(), 1);
        assert!(!html.contains("<h2>Map legend</h2>"));
        let card = map_controls(&html);
        assert!(card.starts_with(
            "<div id=\"map-controls\" class=\"map-card map-layer open\"><button type=\"button\" id=\"map-layer-pill\" class=\"map-pill enhanced\" aria-expanded=\"true\" aria-controls=\"map-layer-body\">"
        ));
        assert!(card.contains(
            "<form id=\"map-form\" class=\"map-controls\" method=\"get\" action=\"https://x/collections/radar/map\">"
        ));

        let vector = json!({"id":"regions","extent":{"spatial":{"bbox":[[20,60,30,70]]}},
            "links":[{"rel":"self","href":"https://x/collections/regions"},
                {"rel":rel::TILESETS_VECTOR,"href":"https://x/collections/regions/tiles"}]});
        let html = collection_html(shared, &vector, None);
        assert!(map_controls(&html)
            .starts_with("<div id=\"map-controls\" class=\"map-card map-layer open\" hidden>"));
        // Vector tiles have no legend.
        assert!(!html.contains("id=\"map-legend\""));

        let items = map_html(
            "",
            &json!({"type":"FeatureCollection","features":[]}),
            true,
            "Listed in the table.",
        );
        assert!(items.contains("The map needs JavaScript and WebGL. Listed in the table."));
        assert!(!items.contains("id=\"map-controls\""));
        assert!(!items.contains("id=\"map-legend\""));
    }

    /// #1220: without JavaScript the controls are a real GET to the default
    /// style's Maps endpoint whose named fields are Maps parameters only: the
    /// draw-mode switch sits before the form, the style select has no name
    /// (it picks the endpoint), and each style gets its own submit button.
    /// There is no Update button, and a single style is no choice.
    #[test]
    fn map_form_is_a_maps_get_with_maps_parameter_names_only() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = json!({"id":"nwp","extent":{"spatial":{"bbox":[[20,60,30,70]]},
                "temporal":{"interval":[["2026-10-17T17:00:00Z","2026-10-17T18:00:00Z"]],
                    "grid":{"cellsCount":2,"resolution":"PT1H"}},
                "vertical":{"interval":[[850,1000]],"values":[1000,850],"unit":"hPa"}},
            "links":[{"rel":"self","href":"https://x/collections/nwp"},
                {"rel":rel::MAP,"href":"https://x/collections/nwp/map"},
                {"rel":rel::TILESETS_MAP,"href":"https://x/collections/nwp/map/tiles"}],
            "parameter_names":{
                "2t":{"type":"Parameter","observedProperty":{"label":{"en":"2 Metre Temperature (2t)"}},
                    "unit":{"symbol":{"value":"°C"}}},
                "r":{"type":"Parameter","label":"Relative humidity","unit":{"symbol":"%"}}},
            "styles":[{"id":"warm","title":"Warm","links":[
                {"rel":rel::MAP,"href":"https://x/collections/nwp/styles/warm/map"}]}]});
        let html = collection_html(shared, &doc, None);
        assert!(!html.contains("Update map"));
        let controls = map_controls(&html);
        let (form, switch) = controls.split_once("</form>").unwrap();
        let form = form.split_once("<form").unwrap().1;
        assert!(
            form.starts_with(" id=\"map-form\" class=\"map-controls\" method=\"get\" action=\"https://x/collections/nwp/map\">"),
            "{form}"
        );
        // Every named field of the form, and nothing else, is a Maps
        // parameter (api-maps submits the form to its own router).
        let names: std::collections::BTreeSet<&str> = form
            .split(" name=\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(names, ["datetime", "elevation", "parameter-name"].into());
        assert!(switch.contains("<input type=\"radio\" name=\"map-mode\" value=\"tiles\" checked>"));
        assert!(form.contains(
            "<label class=\"enhanced\">Style<select id=\"map-style\" data-mode=\"tiles\"><option value=\"https://x/collections/nwp/map\" data-id=\"default\""
        ));
        assert!(form.contains("<select id=\"map-level\" name=\"elevation\">"));
        // One submit per style with a Maps endpoint, without JavaScript only.
        let noscript = form.split("<noscript>").nth(1).unwrap();
        assert!(noscript.contains("<button class=\"btn primary\">Open map image</button>"));
        assert!(noscript.contains(
            "<button class=\"btn\" formaction=\"https://x/collections/nwp/styles/warm/map\">Open map image · Warm</button>"
        ));
        // The help is the script's, in the time bar's help; it names no
        // button.
        assert!(html.contains(
            "<div id=\"map-keys-help\" class=\"map-keys-help map-card\" hidden><p id=\"map-help\" class=\"map-help\">Every choice applies at once; Default leaves it to the server. The time bar plays the 2 advertised times.</p>"
        ));
        // A label ending in its own name drops it before the unit.
        assert!(
            form.contains(">2 Metre Temperature (°C)</option>"),
            "{form}"
        );
        assert_eq!(
            map_data(&html)["mapRequest"]["parameter_names"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == "2t")
                .unwrap()["label"],
            "2 Metre Temperature"
        );

        // One style: nothing to choose, so no style select is shown.
        let mut single = doc.clone();
        single.as_object_mut().unwrap().remove("styles");
        let html = collection_html(shared, &single, None);
        assert!(map_controls(&html)
            .contains("<label class=\"enhanced\" hidden>Style<select id=\"map-style\""));
        assert_eq!(map_controls(&html).matches(" formaction=").count(), 0);
    }

    /// #1220: `mapRequest` carries each parameter's own axis, and offers
    /// WMS GetMap as a draw mode for a raster collection the WMS serves:
    /// after Maps, outside the no-JS form, as the layer named by its id.
    #[test]
    fn map_request_carries_parameter_axes_and_the_wms_draw_mode() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = json!({"id":"sat","extent":{"spatial":{"bbox":[[20,60,30,70]]},
                "temporal":{"interval":[["2026-10-17T00:00:00Z","2026-10-17T00:20:00Z"]],
                    "grid":{"cellsCount":3,"resolution":"PT10M"}}},
            "links":[{"rel":"self","href":"https://x/collections/sat"},
                {"rel":rel::MAP,"href":"https://x/collections/sat/map"},
                {"rel":rel::TILESETS_MAP,"href":"https://x/collections/sat/map/tiles"}],
            "parameter_names":{
                "ir":{"type":"Parameter","label":"Infrared",
                    "extent":{"temporal":{"interval":[["2026-10-17T00:05:00Z","2026-10-17T00:15:00Z"]],
                        "grid":{"cellsCount":2,"resolution":"PT10M","firstCoordinate":"2026-10-17T00:05:00Z"}}}},
                "vis":{"type":"Parameter","label":"Visible"}}});
        let html = collection_page(shared, &doc, None, None, Some("https://x/wms"));
        let request = &map_data(&html)["mapRequest"];
        assert_eq!(request["modes"], json!(["tiles", "maps", "wms"]));
        assert_eq!(
            request["wms"],
            json!({"href":"https://x/wms","layer":"sat"})
        );
        assert_eq!(
            request["axis"],
            json!({"start":"2026-10-17T00:00:00Z","step":600,"count":3})
        );
        assert!(request.get("times").is_none());
        let axes: Vec<(&str, &Value)> = request["parameter_names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["name"].as_str().unwrap(), &p["axis"]))
            .collect();
        assert!(axes.contains(&(
            "ir",
            &json!({"start":"2026-10-17T00:05:00Z","step":600,"count":2})
        )));
        assert!(axes.contains(&("vis", &Value::Null)));
        let controls = map_controls(&html);
        let (form, switch) = controls.split_once("</form>").unwrap();
        assert!(switch.contains("<label title=\"WMS GetMap · one image per view\"><input type=\"radio\" name=\"map-mode\" value=\"wms\"><span>WMS</span></label>"));
        assert!(!form.contains("wms"));
        // Map tiles stay the default.
        assert!(switch.contains("value=\"tiles\" checked"));

        // Without WMS, or for vector tiles alone, there is no WMS mode.
        let html = collection_html(shared, &doc, None);
        assert!(map_data(&html)["mapRequest"]["wms"].is_null());
        assert_eq!(
            map_data(&html)["mapRequest"]["modes"],
            json!(["tiles", "maps"])
        );
        let vector = json!({"id":"regions","links":[{"rel":"self","href":"https://x/collections/regions"},
            {"rel":rel::TILESETS_VECTOR,"href":"https://x/collections/regions/tiles"}]});
        let html = collection_page(shared, &vector, None, None, Some("https://x/wms"));
        assert_eq!(map_data(&html)["mapRequest"]["modes"], json!(["vector"]));
    }

    /// #1220: the workbench's stylesheets and scripts are static assets at
    /// versioned URLs below the base URL, the same on every API mount, so a
    /// browser caches one copy. A page links them and inlines neither: its
    /// only inline scripts set the theme and fold the request bar before
    /// the first paint.
    #[test]
    fn pages_link_the_versioned_assets_and_inline_none() {
        let base = "https://example.test/prefix";
        let doc = json!({"id":"a","extent":{"spatial":{"bbox":[[20,60,30,70]]}},"links":[
            {"rel":"self","href":format!("{base}/collections/a")},
            {"rel":rel::MAP,"href":format!("{base}/collections/a/map")}]});
        let shared = collection_html(
            Surface {
                base,
                root: base,
                api: crate::shared::WORKSPACE,
            },
            &doc,
            None,
        );
        let pages: Vec<String> = ["edr", "features", "maps", "tiles"]
            .into_iter()
            .map(|api| collection(base, api, &doc, None))
            .chain([shared])
            .collect();
        for html in &pages {
            for name in ["style.css", "map.css"] {
                assert!(html.contains(&format!(
                    "<link rel=\"stylesheet\" href=\"{}\">",
                    assets::href(base, name)
                )));
            }
            assert!(html.contains(&format!(
                "<script src=\"{}\" defer></script>",
                assets::href(base, "map.js")
            )));
            assert!(html.contains(&format!(
                "<script src=\"{}\"></script>",
                assets::href(base, "app.js")
            )));
            assert!(!html.contains("<style>"));
            assert!(!html.contains("function installRasterPreview"));
            assert!(!html.contains("Progressive enhancement only"));
            assert!(!html.contains(".js .map-stage{"));
            assert_eq!(html.matches("<script>").count(), 2, "{html}");
        }
        assert!(assets::href(base, "map.js").starts_with("https://example.test/prefix/workbench/"));
    }

    /// #1220: a collection's map opens its Overview at the content width,
    /// before the coverage facts. Its panels are the script's: the layer
    /// card's pill, the status chip, the full-window and basemap tools with
    /// the data's opacity at 85 %, icons drawn inline. The attribution names
    /// the collection's licence, and the frame's request sits under the map
    /// with Copy and Open. A collection without a map keeps the extent
    /// locator in the coverage panel.
    #[test]
    fn the_overview_opens_on_the_map_and_names_the_licence() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = json!({"id":"nwp","extent":{"spatial":{"bbox":[[20,60,30,70]]},
                "temporal":{"interval":[["2026-10-17T00:00:00Z","2026-10-17T01:00:00Z"]],
                    "grid":{"cellsCount":2,"resolution":"PT1H"}}},
            "links":[{"rel":"self","href":"https://x/collections/nwp"},
                {"rel":rel::MAP,"href":"https://x/collections/nwp/map"}]});
        let license = LicenseConfig {
            title: "CC-BY-4.0".into(),
            url: None,
            media_type: None,
        };
        let html = collection_page(shared, &doc, Some(&license), None, None);
        let overview = html.split("<section id=\"overview\"").nth(1).unwrap();
        let map = overview.find("<div class=\"map-section\">").unwrap();
        assert!(map < overview.find("<h2>Data coverage</h2>").unwrap());
        assert!(map < overview.find("<div class=\"detail-layout\">").unwrap());
        assert_eq!(
            map_data(&html)["mapRequest"]["license"],
            json!({"title":"CC-BY-4.0","href":"https://spdx.org/licenses/CC-BY-4.0.html"})
        );
        assert!(html.contains("<details id=\"map-request\" class=\"map-request enhanced\"><summary>This frame as an API request</summary>"));
        assert!(html.contains(
            "<button type=\"button\" class=\"btn small\" id=\"map-request-copy\" data-copy=\"\" disabled>Copy</button><a id=\"map-image-link\" class=\"btn small\" aria-disabled=\"true\">Open ↗</a>"
        ));
        assert!(html.contains(
            "<p id=\"map-chip\" class=\"map-chip enhanced\" aria-hidden=\"true\" hidden></p>"
        ));
        assert!(html.contains("id=\"map-expand\" aria-label=\"Full window\""));
        assert!(html.contains("<input type=\"radio\" name=\"map-basemap\" value=\"land\" checked>"));
        assert!(html.contains("<input type=\"checkbox\" id=\"map-labels\" checked>"));
        assert!(html.contains(
            "<input type=\"range\" id=\"map-opacity\" min=\"0\" max=\"100\" step=\"5\" value=\"85\">"
        ));
        // The basemap and the repeat mode are no Maps parameters: neither
        // is in the form.
        let form = html.split("<form id=\"map-form\"").nth(1).unwrap();
        let form = form.split("</form>").next().unwrap();
        assert!(!form.contains("map-basemap") && !form.contains("map-repeat"));
        // Icons are inline SVG in the page's own markup, no icon font.
        assert!(html.contains(&format!(
            "<span class=\"when-paused\">{}</span><span class=\"when-playing\">{}</span>",
            icon("play"),
            icon("pause")
        )));
        for name in [
            "to-start",
            "step-back",
            "step-forward",
            "to-end",
            "loop",
            "bounce",
            "expand",
            "collapse",
            "help",
            "layers",
        ] {
            assert!(html.contains(&icon(name)), "{name}");
        }

        let mut no_map = doc.clone();
        no_map["links"]
            .as_array_mut()
            .unwrap()
            .retain(|l| l["rel"] != rel::MAP);
        let html = collection_page(shared, &no_map, None, None, None);
        assert!(!html.contains("map-section"));
        let coverage = html.split("<h2>Data coverage</h2>").nth(1).unwrap();
        assert!(coverage
            .split("</section>")
            .next()
            .unwrap()
            .contains("<div class=\"map-panel\"><div id=\"feature-map\""));
        assert!(map_data(&html).get("mapRequest").is_none());
    }

    /// #1220: 12 levels or fewer also get a ladder on the map's right edge,
    /// the highest in the atmosphere on top: pressure counts down as it
    /// rises, height and elevation angle count up. More levels stay in the
    /// layer card's select alone.
    #[test]
    fn the_level_ladder_holds_up_to_twelve_levels_highest_on_top() {
        use ds_core::vertical::VerticalKind;
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let ladder = |vertical: Value| -> Option<(String, Vec<String>)> {
            let doc = json!({"id":"nwp","extent":{"spatial":{"bbox":[[20,60,30,70]]},"vertical":vertical},
                "links":[{"rel":"self","href":"https://x/collections/nwp"},
                    {"rel":rel::MAP,"href":"https://x/collections/nwp/map"}]});
            let html = collection_html(shared, &doc, None);
            let rest = html.split("<div id=\"map-ladder\"").nth(1)?;
            let rest = rest.split("</div>").next().unwrap();
            let label = rest
                .split("aria-label=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            let levels = rest
                .split("data-level=\"")
                .skip(1)
                .map(|value| value.split('"').next().unwrap().to_owned())
                .collect();
            Some((label.to_owned(), levels))
        };
        let pressure = ladder(
            json!({"interval":[[300,1000]],"values":[1000,850,500,300],"unit":"hPa",
            "vrs":VerticalKind::Pressure.vrs()}),
        )
        .unwrap();
        assert_eq!(pressure.1, ["300", "500", "850", "1000"]);
        assert_eq!(
            pressure.0,
            format!("{} · hPa", VerticalKind::Pressure.default_label())
        );
        let angles = ladder(
            json!({"interval":[[0.5,5]],"values":[0.5,1.5,5],"unit":"deg",
            "vrs":VerticalKind::ElevationAngle.vrs()}),
        )
        .unwrap();
        assert_eq!(angles.1, ["5", "1.5", "0.5"]);
        // An axis of unknown kind keeps the order it is given in.
        let unknown = ladder(json!({"interval":[[1,3]],"values":[2,1,3]})).unwrap();
        assert_eq!(unknown.1, ["2", "1", "3"]);
        let twelve: Vec<u32> = (1..=12).map(|level| level * 100).collect();
        assert_eq!(
            ladder(json!({"interval":[[100,1200]],"values":twelve,"unit":"hPa","vrs":VerticalKind::Pressure.vrs()}))
                .unwrap()
                .1
                .len(),
            12
        );
        let thirteen: Vec<u32> = (1..=13).map(|level| level * 75).collect();
        assert!(ladder(json!({"interval":[[75,975]],"values":thirteen,"unit":"hPa","vrs":VerticalKind::Pressure.vrs()})).is_none());
        // No levels: no ladder.
        assert!(ladder(json!({"interval":[[0,0]]})).is_none());
    }

    /// The map panel of `tests/fixtures/map-panel.html`, which the map
    /// script's node tests (`tests/map.test.cjs`) parse and run against: map
    /// tiles, Maps and WMS, two parameters (one on its own time axis), two
    /// styles, a time grid and levels.
    fn map_panel_fixture() -> String {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let legend = |style: &str| json!({"rel":rel::LEGEND,"href":format!("https://x/collections/nwp/styles/{style}/legend")});
        let doc = json!({"id":"nwp","title":"NWP",
            "extent":{"spatial":{"bbox":[[20,55,30,65]]},
                "temporal":{"interval":[["2024-01-01T00:00:00Z","2024-01-01T02:00:00Z"]],
                    "grid":{"cellsCount":3,"resolution":"PT1H"}},
                "vertical":{"interval":[[850,1000]],"values":[1000,850],"unit":"hPa"}},
            "links":[{"rel":"self","href":"https://x/collections/nwp"},
                {"rel":rel::MAP,"href":"https://x/collections/nwp/map"},
                {"rel":rel::TILESETS_MAP,"href":"https://x/collections/nwp/map/tiles"}],
            // In name order, which is also document order: the options read
            // the same with and without serde_json's `preserve_order`.
            "parameter_names":{
                "r":{"type":"Parameter","label":"Relative humidity","unit":{"symbol":"%"},
                    "extent":{"temporal":{"interval":[["2024-01-01T00:30:00Z","2024-01-01T01:30:00Z"]],
                        "grid":{"cellsCount":2,"coordinates":["2024-01-01T00:30:00Z","2024-01-01T01:30:00Z"]}}}},
                "t":{"type":"Parameter","observedProperty":{"label":{"en":"Temperature (t)"}},
                    "unit":{"symbol":{"value":"K"}}}},
            "styles":[{"id":"default","title":"Default","links":[legend("default")]},
                {"id":"warm","title":"Warm","links":[
                    {"rel":rel::MAP,"href":"https://x/collections/nwp/styles/warm/map"},
                    {"rel":rel::TILESETS_MAP,"href":"https://x/collections/nwp/styles/warm/map/tiles"},
                    legend("warm")]}]});
        let license = LicenseConfig {
            title: "CC-BY-4.0".into(),
            url: None,
            media_type: None,
        };
        let html = collection_page(shared, &doc, Some(&license), None, Some("https://x/wms"));
        let start = html.find("<div class=\"map-panel map-raster\">").unwrap();
        let end = start + html[start..].find("<script").unwrap();
        format!("{}\n", &html[start..end])
    }

    /// The fixture the node tests read is the markup this crate renders.
    /// After a deliberate markup change, regenerate it with
    /// `UPDATE_MAP_FIXTURE=1 cargo test -p api-common map_panel_fixture`.
    /// The embedded `map-data` JSON compares as JSON: its key order follows
    /// serde_json's features, which differ between build selections.
    #[test]
    fn map_panel_fixture_matches_the_rendered_markup() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/map-panel.html");
        let rendered = map_panel_fixture();
        if std::env::var_os("UPDATE_MAP_FIXTURE").is_some() {
            std::fs::write(path, &rendered).unwrap();
        }
        let fixture = std::fs::read_to_string(path).unwrap_or_default();
        let parts = |html: &str| {
            let (markup, rest) = html
                .split_once("<div id=\"map-data\" hidden>")
                .unwrap_or((html, ""));
            let (data, tail) = rest.split_once("</div>").unwrap_or(("", ""));
            (
                markup.to_owned(),
                map_data(&format!("<div id=\"map-data\" hidden>{data}</div>")),
                tail.to_owned(),
            )
        };
        let stale = format!(
            "{path} is stale: regenerate it with UPDATE_MAP_FIXTURE=1 cargo test -p api-common map_panel_fixture"
        );
        assert!(fixture.contains("id=\"map-data\""), "{stale}");
        assert!(parts(&fixture) == parts(&rendered), "{stale}");
    }

    /// #1136: an extent crossing the antimeridian (west > east) is one
    /// region in unwrapped longitudes, not two halves at the world's edges.
    #[test]
    fn antimeridian_extent_is_one_unwrapped_polygon() {
        let shared = Surface {
            base: "https://x",
            root: "https://x",
            api: crate::shared::WORKSPACE,
        };
        let doc = |bbox: Value| {
            json!({"id":"disk","extent":{"spatial":{"bbox":[bbox]}},
            "links":[{"rel":"self","href":"https://x/collections/disk"},
                {"rel":rel::MAP,"href":"https://x/collections/disk/map"}]})
        };
        let geometry = |bbox: Value| {
            map_data(&collection_html(shared, &doc(bbox), None))["features"][0]["geometry"].clone()
        };
        assert_eq!(
            geometry(json!([60.5, -81.0, -138.5, 81.0])),
            json!({"type":"Polygon","coordinates":[[[60.5,-81.0],[221.5,-81.0],[221.5,81.0],[60.5,81.0],[60.5,-81.0]]]})
        );
        assert_eq!(
            geometry(json!([20.0, 60.0, 30.0, 70.0])),
            json!({"type":"Polygon","coordinates":[[[20.0,60.0],[30.0,60.0],[30.0,70.0],[20.0,70.0],[20.0,60.0]]]})
        );
    }

    #[test]
    /// OGC API - EDR `/req/html/content` A: every `data_queries` link is an
    /// `<a>` on the collection (and instance) page and in the catalog row,
    /// to its HTML view: a list, or a query's form (#1138).
    fn data_query_links_are_anchors() {
        let edr = Surface {
            base: "https://x",
            root: "https://x/edr",
            api: "edr",
        };
        let link = |href: &str| json!({"link": {"href": href, "rel": "data"}});
        let doc = json!({"id": "c", "links": [{"rel": "self", "href": "https://x/edr/collections/c"}],
            "data_queries": {
                "position": link("https://x/edr/collections/c/position"),
                "locations": link("https://x/edr/collections/c/locations"),
                "instances": link("https://x/edr/collections/c/instances"),
                "evil": link("javascript:alert(1)")}});
        let hrefs = [
            "https://x/edr/collections/c/position?f=html\"",
            "https://x/edr/collections/c/locations?f=html\"",
            "https://x/edr/collections/c/instances?f=html\"",
        ];
        for html in [collection_html(edr, &doc, None), collection_facts(&doc)] {
            for href in hrefs {
                assert!(html.contains(&format!("href=\"{href}")), "{href}: {html}");
            }
            assert!(!html.contains("href=\"javascript:"));
        }
    }

    #[test]
    fn resource_links_list_each_target_once() {
        let tiles = "https://x/tiles/collections/a/tiles";
        let doc = json!({"links":[
            {"rel":"conformance","href":"https://x/maps/conformance","type":"application/json","title":"Conformance"},
            {"rel":rel::CONFORMANCE,"href":"https://x/maps/conformance","type":"application/json","title":"Conformance"},
            {"rel":rel::MAP,"href":"https://x/maps/collections/a/map","type":"image/png"},
            {"rel":rel::MAP,"href":"https://x/maps/collections/a/map","type":"image/jpeg"},
            {"rel":"tiles","href":tiles,"type":"application/json","title":"Tilesets"},
            {"rel":rel::TILESETS_MAP,"href":tiles,"type":"application/json","title":"Map tilesets"},
            {"rel":rel::TILESETS_VECTOR,"href":tiles,"type":"application/json","title":"Vector tilesets"}
        ]});
        let html = document_links(&doc);
        assert_eq!(html.matches("maps/conformance?f=html").count(), 1);
        assert!(!html.contains(rel::CONFORMANCE));
        // One card per resource (#1140): its representations are listed
        // on it, not as cards of their own.
        assert_eq!(html.matches("class=\"endpoint\"").count(), 3, "{html}");
        assert_eq!(html.matches("<strong>Map image</strong>").count(), 1);
        assert!(
            html.contains("image/png, image/jpeg\">PNG, JPEG</span>"),
            "{html}"
        );
        // A resource offered under several relations keeps every relation
        // and the titles that say more, under the first relation's label.
        assert_eq!(html.matches("<strong>Tiles</strong>").count(), 1);
        assert!(
            html.contains("<p>Map tilesets · Vector tilesets · <code"),
            "{html}"
        );
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

    /// `/req/html/content` A: a list page holds each entry in full — every
    /// keyword, every member, each link's rel, type and title — and the
    /// page's own links (#984).
    #[test]
    fn list_pages_hold_every_entry_in_full() {
        let attack = "<b>x</b>";
        let doc = json!({"id":"a","title":"A","keywords":["k1","k2","k3","k4","k5-last"],
            "crs":["OGC:CRS84"],"output_formats":["CoverageJSON"],
            "data_queries":{"position":{"link":{"href":"https://x/edr/collections/a/position","rel":"data",
                "variables":{"title":"Position query","within_units":["km"]}}}},
            "links":[{"rel":"self","href":"https://x/edr/collections/a","type":"application/json","title":attack},
                {"rel":"data","href":"https://x/edr/collections/a/position","type":"application/vnd.cov+json","title":"Position query"}]});
        let query = SearchQueryParams::from_pairs(Vec::new()).unwrap();
        let search = query.parse().unwrap();
        let page_links = json!([{"rel":"self","href":"https://x/edr/collections?f=html","type":"text/html","title":"This page"}]);
        let html = collections_html(
            MAPS_PROXY,
            &query,
            &search,
            1,
            &[CollectionView {
                metadata: &doc,
                license: None,
            }],
            &[],
            &page_links,
        );
        for text in [
            "k5-last",
            "OGC:CRS84",
            "Position query",
            "within_units",
            "This page",
            "text/html",
            "application/vnd.cov+json",
        ] {
            assert!(html.contains(text), "{text}");
        }
        assert!(html.contains(&escape(attack)) && !html.contains(attack));
        // The data link is listed at its own href; the query chip opens the
        // query's HTML view, its form (#1138).
        assert!(html.contains("href=\"https://x/edr/collections/a/position\">"));
        assert!(html.contains("href=\"https://x/edr/collections/a/position?f=html\">"));

        let instance = json!({"id":"2026-01-01T00:00:00Z","title":"A — run 2026-01-01T00:00:00Z",
            "description":"One run","extent":{"vertical":{"vrs":"VRS-WKT"}},
            "links":[{"rel":"self","href":"https://x/edr/collections/a/instances/2026-01-01T00:00:00Z","type":"application/json","title":"This instance"}]});
        let list = json!({"instances":[instance],"links":[{"rel":"self","href":"https://x/edr/collections/a/instances","type":"application/json","title":"A — instances"}]});
        let html = instances_html(MAPS_PROXY, "A — instances", &list, &[]);
        for text in [
            "A — run 2026-01-01T00:00:00Z",
            "One run",
            "VRS-WKT",
            "This instance",
            "A — instances",
            "application/json",
        ] {
            assert!(html.contains(text), "{text}");
        }
        assert!(html.contains(
            "href=\"https://x/edr/collections/a/instances/2026-01-01T00:00:00Z?f=html\""
        ));
    }

    /// Every run's JSON `title` is the collection's (EDR 1.2
    /// `/req/instances/src-md-success` C), so each instance card is headed
    /// by its self link's own title, which names the run.
    #[test]
    fn instance_cards_are_headed_by_their_run() {
        let run = |id: &str| {
            json!({"id": id, "title": "A", "description": "A",
                "links": [{"rel": "self", "href": format!("https://x/edr/collections/a/instances/{id}"),
                    "type": "application/json", "title": format!("A — run {id}")}]})
        };
        let list = json!({"instances": [run("2026-01-01T00:00:00Z"), run("2026-01-01T06:00:00Z")], "links": []});
        let html = instances_html(MAPS_PROXY, "A — instances", &list, &[]);
        for id in ["2026-01-01T00:00:00Z", "2026-01-01T06:00:00Z"] {
            assert!(
                html.contains(&format!("?f=html\">A — run {id}</a></h2>")),
                "{id}"
            );
        }
    }

    /// The collection page's link table lists every link, `self`,
    /// `alternate` and `rel=data` included, at its own href; the aside
    /// leaves out the data links the page lists from `data_queries`.
    #[test]
    fn collection_page_lists_every_link_member() {
        let doc = json!({"id":"c","data_queries":{"position":{"link":{"href":"https://x/edr/collections/c/position"}}},
            "links":[{"rel":"self","href":"https://x/edr/collections/c","type":"application/json","title":"This collection"},
                {"rel":"alternate","href":"https://x/edr/collections/c?f=html","type":"text/html","title":"This collection as HTML"},
                {"rel":"data","href":"https://x/edr/collections/c/position","type":"application/vnd.cov+json","title":"Position query"}]});
        let edr = Surface {
            base: "https://x",
            root: "https://x/edr",
            api: "edr",
        };
        let html = collection_html(edr, &doc, None);
        for text in [
            "This collection as HTML",
            "text/html",
            "<code>self</code>",
            "<code>alternate</code>",
            "<td>Position query</td>",
            "application/vnd.cov+json",
        ] {
            assert!(html.contains(text), "{text}");
        }
        // The link table keeps the data link's own href; the query row opens
        // the query's form (#1138).
        assert!(html.contains("href=\"https://x/edr/collections/c/position\">"));
        assert!(html.contains("href=\"https://x/edr/collections/c/position?f=html\">"));
    }

    /// The `href` of every `<a>` element of `html`, unescaped.
    fn anchor_hrefs(html: &str) -> Vec<String> {
        html.split("<a ")
            .skip(1)
            .filter_map(|tag| {
                let tag = tag.split('>').next()?;
                let href = tag.split("href=\"").nth(1)?.split('"').next()?;
                Some(href.replace("&amp;", "&"))
            })
            .collect()
    }

    /// Every `href` member anywhere in `value`.
    fn json_hrefs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    match (key.as_str(), member) {
                        ("href", Value::String(href)) => out.push(href.clone()),
                        _ => json_hrefs(member, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| json_hrefs(item, out)),
            _ => {}
        }
    }

    /// A shared-root collection with styles, parameters, items and an
    /// EDR-style query, as the #1140 tests read it.
    fn rich_collection() -> Value {
        let style = |id: &str, title: &str| {
            json!({"id": id, "title": title, "links": [
                {"href": format!("https://x/collections/r/styles/{id}/map"), "rel": rel::MAP, "type": "image/png"},
                {"href": format!("https://x/collections/r/styles/{id}/legend"), "rel": rel::LEGEND, "type": "application/json"},
                {"href": format!("https://x/collections/r/styles/{id}/map/tiles"), "rel": rel::TILESETS_MAP, "type": "application/json", "title": "Map tilesets in this style"}]})
        };
        json!({"id": "r", "title": "Radar", "description": "Reflectivity",
            "links": [
                {"href": "https://x/collections/r", "rel": "self", "type": "application/json", "title": "This collection"},
                {"href": "https://x/collections/r?f=html", "rel": "alternate", "type": "text/html", "title": "This collection as HTML"},
                {"href": "https://x/collections/r/map", "rel": rel::MAP, "type": "image/png", "title": "Map"},
                {"href": "https://x/collections/r/styles", "rel": rel::STYLES, "type": "application/json", "title": "Styles"},
                {"href": "https://x/collections/r/map/tiles", "rel": rel::TILESETS_MAP, "type": "application/json", "title": "Map tilesets"},
                {"href": "https://x/collections/r/items", "rel": "items", "type": "application/geo+json", "title": "Items"},
                {"href": "https://x/collections/r/items?f=html", "rel": "items", "type": "text/html", "title": "Items as HTML"},
                {"href": "https://x/licenses/cc-by", "rel": "license", "type": "text/html", "title": "CC-BY-4.0"}],
            "keywords": ["radar"],
            "dataType": "map", "itemType": "feature", "numberItems": 3,
            "crs": ["http://www.opengis.net/def/crs/OGC/1.3/CRS84", "http://www.opengis.net/def/crs/EPSG/0/3067"],
            "storageCrs": "http://www.opengis.net/def/crs/EPSG/0/3067",
            "styles": [style("default", "Default"), style("grayscale", "Grayscale")],
            "parameter_names": {
                "dbzh": {"type": "Parameter", "label": "Reflectivity", "description": "Reflectivity, in dBZ",
                    "unit": {"label": {"en": "decibel Z"}, "symbol": {"value": "dBZ", "type": "https://qudt.org/vocab/unit/DeciB_Z"}},
                    "observedProperty": {"id": "https://example.test/reflectivity", "label": {"en": "Reflectivity"}}},
                "qind": {"type": "Parameter", "observedProperty": {"label": {"en": "Quality index"}}},
                "rgb": {"type": "Parameter", "description": "RGB composite: red dbzh, green qind",
                    "observedProperty": {"label": {"en": "Composite"}},
                    "extent": {"temporal": {"interval": [["2026-03-24T23:20:00Z", "2026-03-24T23:30:00Z"]], "grid": {"cellsCount": 3}}}}},
            "data_queries": {"position": {"link": {"href": "https://x/edr/collections/r/position", "rel": "data",
                "variables": {"title": "Position query", "description": "Data at a point", "query_type": "position",
                    "output_formats": ["CoverageJSON", "HTML"], "default_output_format": "CoverageJSON",
                    "crs_details": [{"crs": "CRS84", "wkt": "GEOGCRS[\"WGS 84\"]"}], "within_units": ["km"]}}}},
            "extent": {"spatial": {"bbox": [[0.4190042054438888, 54.570768841987515, 37.99495803626129, 74.81020226886669]],
                    "crs": "http://www.opengis.net/def/crs/OGC/1.3/CRS84"},
                "temporal": {"interval": [["2026-03-24T23:15:00+00:00", "2026-03-24T23:35:00+00:00"]],
                    "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian",
                    "grid": {"cellsCount": 5, "resolution": "PT5M", "firstCoordinate": "2026-03-24T23:15:00+00:00"}}},
            "custom": {"nested": [{"value": 135.0}]}})
    }

    const SHARED_X: Surface<'static> = Surface {
        base: "https://x",
        root: "https://x",
        api: crate::shared::WORKSPACE,
    };

    /// #1143: the Overview says what an item is and lists the properties
    /// the items page filters and sorts by, each chip opening the builder
    /// at its property through the fragment, never a query parameter.
    #[test]
    fn overview_lists_item_filters_and_sorts() {
        let doc = json!({"id": "b", "title": "Radar sites", "description": "Polar volumes", "links": [
            {"rel": "items", "href": "https://x/collections/b/items", "type": "application/geo+json"}]});
        let overview = ItemsOverview {
            description: Some("Radar site inventory: one point per radar.".into()),
            filterables: vec!["name".into(), "a b/c".into()],
            sortables: vec!["volume_count".into()],
        };
        let html = collection_page(SHARED_X, &doc, None, Some(&overview), None);
        assert!(html.contains("<p>Polar volumes</p><p class=\"items-note\">Radar site inventory: one point per radar.</p>"));
        assert!(html.contains("<span class=\"row-label\">Filter by</span><span class=\"chip-row\"><a class=\"chip\" href=\"https://x/collections/b/items?f=html#filter=name\">name</a><a class=\"chip\" href=\"https://x/collections/b/items?f=html#filter=a%20b%2Fc\">a b/c</a></span>"), "{html}");
        assert!(html.contains("<a class=\"chip\" href=\"https://x/collections/b/items?f=html#sort=volume_count\">volume_count</a>"));
        // Nothing to say, no rows; no items link, no chips.
        let html = collection_page(SHARED_X, &doc, None, Some(&ItemsOverview::default()), None);
        // The stylesheet is inline: look for the elements.
        assert!(
            !html.contains("class=\"row-line item-fields\"")
                && !html.contains("class=\"items-note\"")
        );
        let no_items = json!({"id": "b", "links": []});
        assert!(
            !collection_page(SHARED_X, &no_items, None, Some(&overview), None)
                .contains("class=\"row-line item-fields\"")
        );
        assert_eq!(
            collection_html(SHARED_X, &doc, None),
            collection_page(SHARED_X, &doc, None, None, None)
        );
    }

    /// #1140, Common Part 1 `/req/html/content`: every `href` of the
    /// collection JSON, the styles' links and the data queries' included,
    /// is the `href` of an `<a>` on the page; the metadata tab shows no
    /// member as JSON text.
    #[test]
    fn every_href_of_the_collection_json_is_an_anchor() {
        let doc = rich_collection();
        let html = collection_html(SHARED_X, &doc, None);
        let anchors = anchor_hrefs(&html);
        let mut hrefs = Vec::new();
        json_hrefs(&doc, &mut hrefs);
        assert!(hrefs.len() >= 15, "{hrefs:?}");
        for href in hrefs {
            assert!(anchors.contains(&href), "{href} is not an anchor");
        }
        let metadata = html.split("id=\"metadata\"").nth(1).unwrap();
        assert!(!metadata.contains("<pre"), "{metadata}");
        assert!(!html.contains("<pre"));
        // Each style is a row with its legend and its map, tile and legend
        // links as anchors labelled in plain words.
        let styles = metadata.split("data-property=\"styles\"").nth(1).unwrap();
        assert!(styles.contains(
            "<img src=\"https://x/collections/r/styles/grayscale/legend?f=png\" alt=\"Legend of the Grayscale style\" loading=\"lazy\">"
        ), "{styles}");
        for label in ["Map image", "Map tiles", "Legend"] {
            assert_eq!(
                styles.matches(&format!(">{label}</a>")).count(),
                2,
                "{label}"
            );
        }
        // Data queries: formats with the default marked, CRS and units.
        assert!(metadata.contains("CoverageJSON · default"));
        assert!(metadata.contains("title=\"GEOGCRS[&quot;WGS 84&quot;]\">CRS84</span>"));
        assert!(metadata.contains(
            "<div>Radius: km<small class=\"term\"><code>within_units</code></small></div>"
        ));
        // CRS lists as short names, the URI in the title.
        assert!(metadata.contains(
            "<span class=\"chip crs\" title=\"http://www.opengis.net/def/crs/EPSG/0/3067\">EPSG:3067</span>"
        ));
        // The extent with the catalog's formatting, every member kept.
        assert!(metadata.contains("<dt>Cadence</dt><dd>Every 5 min</dd>"));
        assert!(metadata.contains("<dt>Steps</dt><dd>5 steps</dd>"));
        assert!(metadata.contains("0.42, 54.57 → 37.99, 74.81"));
        assert!(metadata.contains(">2026-03-24T23:15:00+00:00</span>"));
        // An unknown member is a collapsed nested list; its float shows
        // without `.0`.
        assert!(metadata.contains("<details><summary>1 member</summary>"));
        assert!(metadata.contains("<dt><code>value</code></dt><dd>135</dd>"));
        // Members are labelled in plain words, the JSON name secondary.
        assert!(metadata.contains(
            "<th scope=\"row\">Storage coordinate system<small class=\"term\"><code>storageCrs</code></small></th>"
        ));
    }

    /// #1140: the Overview lists the parameters a request names in
    /// `parameter-name`, with label and unit, says when the source gives
    /// no unit, and shows a parameter's own time axis.
    #[test]
    fn overview_lists_parameters_with_label_and_unit() {
        let html = collection_html(SHARED_X, &rich_collection(), None);
        let overview = html.split("id=\"metadata\"").next().unwrap();
        let table = overview
            .split("id=\"parameters\"")
            .nth(1)
            .unwrap()
            .split("</section>")
            .next()
            .unwrap();
        assert!(table.contains("<h2>Parameters</h2><span class=\"chip\">3</span>"));
        assert!(table.contains(
            "<tr data-parameter=\"dbzh\"><th scope=\"row\"><code>dbzh</code></th><td>Reflectivity</td><td data-label=\"Unit\"><span title=\"decibel Z\">dBZ</span></td>"
        ), "{table}");
        assert!(table.contains(
            "<td>Quality index</td><td data-label=\"Unit\"><span class=\"muted\">unit not provided by source</span></td>"
        ));
        // A description that says more than the label is shown under it.
        assert!(table
            .contains("<small class=\"secondary\">RGB composite: red dbzh, green qind</small>"));
        assert!(!table.contains("Reflectivity, in dBZ"));
        // The composite's own time axis; the others follow the collection.
        assert!(table.contains("<th scope=\"col\">Time</th>"));
        assert!(table.contains("<span>23:20–23:30 UTC</span>"), "{table}");
        assert!(table.contains("<small class=\"secondary\">3 steps</small>"));
        // A collection without parameters has no table.
        let mut none = rich_collection();
        none.as_object_mut().unwrap().remove("parameter_names");
        assert!(!collection_html(SHARED_X, &none, None).contains("id=\"parameters\""));
    }

    /// #1140: the coverage facts use the catalog's formatting: the time
    /// span, cadence and steps, the bounds to 2 decimals with a Copy bbox
    /// button holding the exact values, and one wording when missing.
    #[test]
    fn coverage_facts_use_the_catalog_formatting() {
        let doc = rich_collection();
        let facts = metadata::coverage_facts(&doc);
        assert!(
            facts.contains("<span>2026-03-24</span><span>23:15–23:35 UTC</span>"),
            "{facts}"
        );
        assert!(facts.contains("<small>Cadence</small><strong>Every 5 min</strong>"));
        assert!(facts.contains("<small>Steps</small><strong>5 steps</strong>"));
        assert!(facts.contains("title=\"0.4190042054438888,54.570768841987515,37.99495803626129,74.81020226886669\">0.42, 54.57 → 37.99, 74.81</strong>"), "{facts}");
        assert!(facts.contains("data-copy=\"0.4190042054438888,54.570768841987515,37.99495803626129,74.81020226886669\">Copy bbox</button>"));
        // The catalog row shows the same bounds text.
        assert!(collection_facts(&doc).contains(">0.42, 54.57 → 37.99, 74.81</span>"));
        let empty = metadata::coverage_facts(&json!({"id": "e"}));
        assert_eq!(empty.matches("Not advertised").count(), 2, "{empty}");
        assert!(!empty.contains("Cadence") && !empty.contains("Steps"));
        let html = collection_html(SHARED_X, &json!({"id": "e", "links": []}), None);
        assert!(!html.contains("Not available"), "{html}");
    }

    /// #1140: the Overview's resource cards are labelled in plain words, one
    /// per resource; the items JSON and HTML share a card; the map card
    /// says it covers the whole extent; nothing listed elsewhere on the
    /// Overview repeats, and an empty list gets no panel. The details give
    /// the kind from `dataType` and keywords as a term.
    #[test]
    fn overview_resource_cards_use_plain_labels_once_per_resource() {
        let license = LicenseConfig {
            title: "CC-BY-4.0".into(),
            url: Some("https://x/licenses/cc-by".into()),
            media_type: None,
        };
        let html = collection_html(SHARED_X, &rich_collection(), Some(&license));
        let overview = html.split("id=\"metadata\"").next().unwrap();
        let aside = overview
            .split("<aside class=\"aside-stack\">")
            .nth(1)
            .unwrap();
        let cards = aside.split("<h2>Resource links</h2>").nth(1).unwrap();
        assert_eq!(cards.matches("class=\"endpoint\"").count(), 3, "{cards}");
        for label in ["Map image", "Styles", "Features"] {
            assert!(
                cards.contains(&format!("<strong>{label}</strong>")),
                "{label}"
            );
        }
        // The items card opens the HTML view and keeps the GeoJSON link.
        assert!(cards.contains(
            "<a class=\"resource-title\" href=\"https://x/collections/r/items?f=html\"><strong>Features</strong><code>https://x/collections/r/items</code></a>"
        ), "{cards}");
        assert!(cards
            .contains("<a class=\"chip\" href=\"https://x/collections/r/items\">As GeoJSON</a>"));
        assert!(cards.contains("<code class=\"term\" title=\"items\">items</code>"));
        assert!(cards.contains(&format!(
            "<code class=\"term\" title=\"{}\">map</code>",
            rel::MAP
        )));
        assert!(cards.contains("Covers the whole extent unless the request sets a bbox"));
        // A title that only repeats the label is not shown again.
        assert!(!cards.contains("Map · "));
        assert!(!overview.contains("requires bbox"));
        // Tilesets are listed beside the map, the license in the details.
        assert!(!cards.contains("map/tiles\""));
        assert!(!cards.contains("licenses/cc-by"));
        assert_eq!(
            overview
                .matches("href=\"https://x/collections/r/map/tiles\"")
                .count(),
            1
        );
        assert!(aside.contains(
            "<dt>Data type</dt><dd>Map layer<small class=\"term\">dataType <code>map</code> · itemType <code>feature</code></small></dd>"
        ), "{aside}");
        assert!(aside.contains("<dt>Keywords</dt><dd><div class=\"tag-list\">"));
        assert!(!aside.contains("<label>Keywords</label>"));
        assert!(aside.contains("<dt>Storage coordinate system</dt><dd><span class=\"chip crs\" title=\"http://www.opengis.net/def/crs/EPSG/0/3067\">EPSG:3067</span></dd>"));
        // Nothing left to list: no empty panel.
        let bare =
            json!({"id": "b", "links": [{"href": "https://x/collections/b", "rel": "self"}]});
        assert!(!collection_html(SHARED_X, &bare, None).contains("Resource links"));
    }

    /// The stylesheet's rules as (selector list, declarations, inside an
    /// at-rule), comments removed.
    fn css_rules() -> Vec<(String, String, bool)> {
        fn walk(css: &str, nested: bool, out: &mut Vec<(String, String, bool)>) {
            let mut rest = css;
            while let Some(open) = rest.find('{') {
                let selector = rest[..open].trim().to_owned();
                let mut depth = 1;
                let mut end = rest.len();
                for (i, c) in rest[open + 1..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = open + 1 + i;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let body = &rest[open + 1..end];
                if selector.starts_with('@') {
                    walk(body, true, out);
                } else {
                    out.push((selector, body.to_owned(), nested));
                }
                rest = rest.get(end + 1..).unwrap_or_default();
            }
        }
        let mut css = String::new();
        let mut rest = CSS;
        while let Some(start) = rest.find("/*") {
            css.push_str(&rest[..start]);
            rest = rest[start..]
                .split_once("*/")
                .map_or("", |(_, after)| after);
        }
        css.push_str(rest);
        let mut out = Vec::new();
        walk(&css, false, &mut out);
        out
    }

    /// A selector list's selectors (commas inside `:is(…)` kept), each
    /// with its subject: the last compound selector.
    fn subjects(list: &str) -> Vec<(&str, &str)> {
        let mut out = Vec::new();
        let (mut depth, mut start) = (0, 0);
        for (i, c) in list.char_indices().chain([(list.len(), ',')]) {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    let selector = list[start..i].trim();
                    let subject = selector
                        .rsplit(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~'))
                        .next()
                        .unwrap_or_default();
                    out.push((selector, subject));
                    start = i + 1;
                }
                _ => {}
            }
        }
        out
    }

    fn declares(body: &str, property: &str) -> Option<String> {
        body.split(';').find_map(|d| {
            let (name, value) = d.split_once(':')?;
            (name.trim() == property).then(|| value.trim().to_owned())
        })
    }

    fn is_th(subject: &str) -> bool {
        subject == "th" || ["th[", "th:", "th."].iter().any(|p| subject.starts_with(p))
    }

    /// #1131: a width on header cells is for the row headers of key/value
    /// tables only. A column header with one (`.properties th{width:42%}`
    /// once) squeezed every link table's Link column to 55 px. Every
    /// header cell is top-aligned, as data cells are.
    #[test]
    fn only_row_headers_have_a_width_and_every_header_is_top_aligned() {
        let rules = css_rules();
        for (list, body, _) in &rules {
            if declares(body, "width").is_none() {
                continue;
            }
            for (selector, subject) in subjects(list) {
                if is_th(subject) {
                    assert!(subject.contains("[scope=row]"), "{selector}{{{body}}}");
                }
            }
        }
        assert!(rules.iter().any(|(list, body, nested)| list == "th"
            && !nested
            && declares(body, "vertical-align").as_deref() == Some("top")));
        for (list, body, _) in &rules {
            if let Some(align) = declares(body, "vertical-align") {
                for (selector, subject) in subjects(list) {
                    if is_th(subject) {
                        assert_eq!(align, "top", "{selector}");
                    }
                }
            }
        }
        // The link table sizes its own columns, the link at least 24ch.
        assert!(rules
            .iter()
            .any(|(list, body, nested)| list == ".link-table td:last-child"
                && !nested
                && declares(body, "min-width").as_deref() == Some("24ch")));
    }

    /// #1131: the landing's link table carries each link's type, lists a
    /// link offered under two relation forms once, and sizes its columns
    /// with a `<colgroup>`; the anchor carries the title for the phone
    /// card.
    #[test]
    fn landing_link_table_merges_relation_forms_and_keeps_types() {
        let conformance = "https://x/conformance";
        let links = [
            LinkView::new("https://x/", "self", Some("This document"))
                .with_type("application/json"),
            LinkView::new(conformance, "conformance", Some("Conformance classes"))
                .with_type("application/json"),
            LinkView::new(conformance, rel::CONFORMANCE, Some("Conformance classes"))
                .with_type("application/json"),
            // Another title or type is another row.
            LinkView::new(conformance, "conformance", Some("Conformance as HTML"))
                .with_type("text/html"),
        ];
        let html = landing_html(
            Surface {
                base: "https://x",
                root: "https://x",
                api: crate::shared::WORKSPACE,
            },
            "MeteoCore",
            "",
            &links,
        );
        let table = html
            .split("class=\"properties link-table\">")
            .nth(1)
            .unwrap();
        assert!(table.starts_with("<colgroup>"), "{table}");
        assert_eq!(
            table.matches(&format!("href=\"{conformance}\"")).count(),
            2,
            "{table}"
        );
        assert!(table.contains(&format!(
            "<td><code>conformance</code><code>{}</code></td><td>Conformance classes</td><td>application/json</td>",
            rel::CONFORMANCE
        )));
        assert!(table.contains("<td>text/html</td>"));
        assert!(table.contains(&format!(
            "<a class=\"table-link\" href=\"{conformance}\"><span class=\"link-label\">Conformance classes</span>{conformance}</a>"
        )));
        // The landing's title names the server once.
        assert!(html.contains("<title>MeteoCore</title>"), "{html}");
    }

    /// #1131: a run card is one full-width column (not the catalog grid's
    /// icon column) and shows the run's valid-time span and step count.
    #[test]
    fn run_cards_are_full_width_with_span_and_step_count() {
        let run = json!({"id": "2026-01-01T00:00:00Z", "title": "A", "description": "A",
            "extent": {"temporal": {"interval": [["2026-01-01T00:00:00Z", "2026-01-02T12:00:00Z"]],
                "values": ["2026-01-01T00:00:00Z", "2026-01-01T06:00:00Z", "2026-01-02T12:00:00Z"]}},
            "links": []});
        let list = json!({"instances": [run], "links": []});
        let html = instances_html(MAPS_PROXY, "A — instances", &list, &[]);
        assert!(html.contains(
            "<article class=\"collection-row instance-row\"><div class=\"collection-main\">"
        ));
        assert!(!html.contains("collection-icon\">"));
        assert!(
            html.contains(
                "<span>2026-01-01 00:00</span><span>→</span><span>2026-01-02 12:00</span>"
            ),
            "{html}"
        );
        assert!(html.contains("<dt>Time steps</dt><dd>3 steps</dd>"));
        // The span names its time zone once.
        assert!(html.contains("<dt>Valid time</dt>"), "{html}");
        // The card's one column overrides the catalog row's grid at every width.
        assert!(css_rules().iter().any(|(list, body, nested)| list
            == ".collection-row.instance-row"
            && !nested
            && declares(body, "grid-template-columns").as_deref() == Some("minmax(0,1fr)")));
        // A fact the run does not advertise is left out, not shown empty.
        assert_eq!(run_facts(&json!({"values": []})), "");
        let span_only = run_facts(&json!({"interval": [["2026-01-01T00:00:00Z", null]],
            "values": []}));
        assert!(span_only.contains("<dt>Valid time</dt>"), "{span_only}");
        assert!(!span_only.contains("Time steps"), "{span_only}");
        assert_eq!(
            run_facts(&json!({"grid": {"cellsCount": 1}})),
            "<dl class=\"collection-facts run-facts\"><div><dt>Time steps</dt><dd>1 step</dd></div></dl>"
        );
    }

    /// #1131: the shell declares an icon, names the page once, summarises
    /// the request with its query, names the representation in use, folds
    /// the request bar below 800 px and links no page to itself.
    #[test]
    fn shell_summarises_the_request_and_names_its_representation() {
        let edr = Surface {
            base: "https://x",
            root: "https://x/edr",
            api: "edr",
        };
        let page = Page {
            surface: edr,
            title: "Position query · Forecast",
            json_url:
                "https://x/edr/collections/c/position?coords=POINT%2824%2060%29&f=CoverageJSON",
        }
        .render("", "");
        assert!(page.contains("<title>Position query · Forecast · MeteoCore</title>"));
        assert!(page.contains("<link rel=\"icon\" href=\"data:image/svg+xml,"));
        assert!(page.contains("Applied request · CoverageJSON representation"));
        assert!(page.contains("type=\"application/vnd.cov+json\""));
        assert!(page.contains(
            "<summary>Current API request <code>GET /edr/collections/c/position?coords=POINT(24 60)</code></summary>"
        ));
        // The bar keeps its name where its summary is hidden.
        assert!(page.contains(
            "<details id=\"request-context\" class=\"catalog-request\" aria-label=\"Current API request\" open>"
        ));
        assert!(page.contains(REQUEST_FOLD));
        assert!(!page.contains("Explore all APIs"));
        // Catalog and items pages fold the request at every width.
        let items = Page {
            surface: edr,
            title: "Items 1–10 · Forecast",
            json_url: "https://x/edr/collections/c/items?limit=10&f=json",
        }
        .render("", "");
        assert!(items.contains(
            "<details id=\"request-context\" class=\"catalog-request\" aria-label=\"Current API request\"><summary>"
        ));
        assert!(!items.contains(REQUEST_FOLD));
        assert!(items.contains("Applied request · GeoJSON representation"));
        assert!(items.contains("<code>GET /edr/collections/c/items?limit=10</code>"));
        // A feature id shows decoded; `+` and `&` stay themselves, as in a
        // path, and a bad escape stays as it is.
        assert_eq!(
            request_summary("/c/items/cap%3A22%3Aa%40b%2Fc+d&e=1%zz%2?f=html#x"),
            "/c/items/cap:22:a@b/c+d&e=1%zz%2"
        );
        // So do the breadcrumbs.
        let feature = Page {
            surface: edr,
            title: "A",
            json_url: "https://x/edr/collections/c/items/a+b&c%3Ad?f=json",
        }
        .render("", "");
        assert!(feature.contains(">a+b&amp;c:d</a></nav>"), "{feature}");
        // Only a landing page's title goes without the server's name, not
        // every title that mentions it.
        let named = Page {
            surface: edr,
            title: "MeteoCore demo radar",
            json_url: "https://x/edr/collections/c?f=json",
        }
        .render("", "");
        assert!(named.contains("<title>MeteoCore demo radar · MeteoCore</title>"));
    }
}
