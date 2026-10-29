//! What a browser sees when a request fails (#1132).
//!
//! JSON stays the error contract: every handler answers a failed request
//! with its `{"code", "description"}` body. A request that asked for HTML —
//! `f=html`, or no `f` and an `Accept` that prefers `text/html` — gets that
//! error as a workbench page instead, with the same status, the message, the
//! parameter at fault and links back. [`html_errors`] makes that switch for a
//! whole router, so no handler renders an error page itself. The
//! representation is read from the raw query before any parameter is
//! parsed, so a request whose other parameters fail still gets the
//! representation it asked for.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::Router;
use ds_core::html::Wanted;
use serde_json::Value;

use crate::workbench::{self, Surface};

/// Error bodies are a code and a sentence; anything larger is not one.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// The representation an error goes out in, read before any other parameter
/// is parsed: the first `f` of the raw query, else `Accept` when it
/// prefers `text/html` ([`prefers_html`]), as a browser's does. An `f` that
/// names no metadata format (`png`, `CoverageJSON`, a typo) is JSON, the
/// representation of the 400 it gets where it is not valid.
pub fn wanted(query: Option<&str>, headers: &HeaderMap) -> Wanted {
    // An empty `f` is no `f`, as `ds_core::html::negotiate` reads it.
    let f = query
        .and_then(|query| {
            form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "f")
                .map(|(_, value)| value.trim().to_owned())
        })
        .filter(|f| !f.is_empty());
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    match f {
        Some(f) => ds_core::html::negotiate(Some(&f), accept).unwrap_or(Wanted::Json),
        None if accept.is_some_and(prefers_html) => Wanted::Html,
        None => Wanted::Json,
    }
}

/// Whether `accept` ranks `text/html` first: a non-zero q-value above every
/// other type it lists, or equal and listed before them. `*/*`, `text/*` and
/// the XML types browsers list beside HTML do not compete, so a browser's
/// header prefers HTML while `application/vnd.cov+json, text/html;q=0.1`,
/// which gets CoverageJSON from a data query, gets its errors as JSON too.
fn prefers_html(accept: &str) -> bool {
    let mut html: Option<(f32, usize)> = None;
    let mut other: Option<(f32, usize)> = None;
    for (index, entry) in accept.split(',').enumerate() {
        let mut parts = entry.split(';').map(str::trim);
        let media = parts.next().unwrap_or_default().to_ascii_lowercase();
        let q = parts
            .find_map(|p| p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")))
            .map_or(1.0, |v| v.trim().parse::<f32>().unwrap_or(0.0));
        let slot = match media.as_str() {
            "text/html" => &mut html,
            "" | "*/*" | "text/*" | "application/xhtml+xml" | "application/xml" => continue,
            _ => &mut other,
        };
        if slot.is_none_or(|(best, _)| q > best) {
            *slot = Some((q, index));
        }
    }
    match (html, other) {
        (Some((q, _)), _) if q <= 0.0 => false,
        (Some(_), None) => true,
        (Some((q, at)), Some((other_q, other_at))) => {
            q > other_q || (q == other_q && at < other_at)
        }
        (None, _) => false,
    }
}

/// What an HTML error page can say beyond the JSON body's description.
/// The handler that knows it attaches it to its error response as an
/// extension ([`ErrorDetail::attach`]); the JSON body is unchanged. A server
/// error's page ignores all of it (Critical Rule 11).
#[derive(Clone, Debug, Default)]
pub struct ErrorDetail {
    /// The query parameter at fault. The page names it and the value the
    /// request gave it.
    pub parameter: Option<String>,
    /// The page heading, in place of the status's reason phrase.
    pub title: Option<String>,
    /// The message the page shows in place of the JSON body's description,
    /// when that is worded for another case (an area with no data answers
    /// "Location not found").
    pub message: Option<String>,
    /// A plain-language explanation shown with the server's message.
    pub hint: Option<String>,
    /// Values `parameter` accepts. Each links to this request with that
    /// value in place of the rejected one.
    pub choices: Vec<String>,
}

impl ErrorDetail {
    /// `response` carrying this detail for its HTML error page.
    pub fn attach(self, response: impl IntoResponse) -> Response {
        let mut response = response.into_response();
        response.extensions_mut().insert(self);
        response
    }
}

/// What an API tells its error pages about a request.
pub trait ErrorPages: Send + Sync + 'static {
    /// External base URL of the request.
    fn base_url(&self, headers: &HeaderMap) -> String;
    /// Whether the API serves collection `id`: a page about a resource
    /// below it then links back to it.
    fn has_collection(&self, id: &str) -> bool;
}

/// [`ErrorPages`] from an API's base-URL resolver and catalog lookup.
pub fn pages(
    base_url: impl Fn(&HeaderMap) -> String + Send + Sync + 'static,
    has_collection: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> Arc<dyn ErrorPages> {
    struct Fns<B, C>(B, C);
    impl<B, C> ErrorPages for Fns<B, C>
    where
        B: Fn(&HeaderMap) -> String + Send + Sync + 'static,
        C: Fn(&str) -> bool + Send + Sync + 'static,
    {
        fn base_url(&self, headers: &HeaderMap) -> String {
            (self.0)(headers)
        }
        fn has_collection(&self, id: &str) -> bool {
            (self.1)(id)
        }
    }
    Arc::new(Fns(base_url, has_collection))
}

/// `router` with every JSON error answered as an HTML page when the request
/// asked for HTML. `api` is the workbench kind and `mount` the path the
/// router is mounted at below the external base URL. A JSON error that
/// passes through carries `Vary: accept` as the page does, since the same
/// URL can answer either (#1123); other responses pass through untouched.
pub fn html_errors(
    router: Router,
    api: &'static str,
    mount: &'static str,
    pages: Arc<dyn ErrorPages>,
) -> Router {
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let pages = pages.clone();
            async move { html_error(request, next, api, mount, pages.as_ref()).await }
        },
    ))
}

async fn html_error(
    request: Request,
    next: Next,
    api: &'static str,
    mount: &'static str,
    pages: &dyn ErrorPages,
) -> Response {
    if wanted(request.uri().query(), request.headers()) != Wanted::Html {
        let mut response = next.run(request).await;
        if is_json_error(&response) {
            vary_accept(response.headers_mut());
        }
        return response;
    }
    // The path below the router's mount: links are `root` (base URL +
    // mount) and this path, as every handler builds them, so a proxy
    // prefix or a nesting above the mount is never counted twice.
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let response = next.run(request).await;
    if !is_json_error(&response) {
        return response;
    }
    let status = response.status();
    let (mut parts, body) = response.into_parts();
    let body: Value = axum::body::to_bytes(body, MAX_ERROR_BODY)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let detail = parts
        .extensions
        .get::<ErrorDetail>()
        .cloned()
        .unwrap_or_default();
    let base = pages.base_url(&headers);
    let root = format!("{base}{mount}");
    let page = ErrorPage {
        status,
        code: body["code"].as_str(),
        description: body["description"].as_str(),
        detail: &detail,
        uri: &uri,
    };
    let html = page.render(
        Surface {
            base: &base,
            root: &root,
            api,
        },
        pages,
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    vary_accept(&mut parts.headers);
    Response::from_parts(parts, Body::from(html))
}

/// A 4xx or 5xx with a JSON body: the error [`html_errors`] turns into a
/// page for a request that asked for HTML.
fn is_json_error(response: &Response) -> bool {
    let status = response.status();
    (status.is_client_error() || status.is_server_error())
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|media| media.starts_with("application/json"))
}

/// Add `accept` to `headers`' `Vary`, merged into the value already there
/// (one header, never a second `accept`). A `Vary: *` already covers it.
fn vary_accept(headers: &mut HeaderMap) {
    let existing: Vec<String> = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .collect();
    if existing
        .iter()
        .any(|token| token == "*" || token.eq_ignore_ascii_case("accept"))
    {
        return;
    }
    let merged = existing
        .into_iter()
        .chain(std::iter::once("accept".to_owned()))
        .collect::<Vec<_>>()
        .join(", ");
    if let Ok(value) = HeaderValue::from_str(&merged) {
        headers.insert(header::VARY, value);
    }
}

/// One failed request, as its HTML page describes it.
struct ErrorPage<'a> {
    status: StatusCode,
    /// The JSON body's code.
    code: Option<&'a str>,
    /// The JSON body's description: the client-visible message.
    description: Option<&'a str>,
    detail: &'a ErrorDetail,
    /// The request, its path below the router's mount.
    uri: &'a Uri,
}

impl ErrorPage<'_> {
    fn render(&self, surface: Surface<'_>, pages: &dyn ErrorPages) -> String {
        let reason = self.status.canonical_reason().unwrap_or("Error");
        // Never an internal detail (root CLAUDE.md, Critical Rule 11): a
        // server error says only what the status does, whatever the body
        // and whatever detail a handler attached.
        let server_error = self.status.is_server_error();
        let none = ErrorDetail::default();
        let detail = if server_error { &none } else { self.detail };
        let message = if server_error {
            if self.status != StatusCode::SERVICE_UNAVAILABLE {
                "The server could not complete this request."
            } else if self.code == Some(crate::SOURCE_UNAVAILABLE) {
                "A data source is not answering for now. Try again in a few seconds."
            } else {
                "The server is busy. Try again in a moment."
            }
        } else {
            detail
                .message
                .as_deref()
                .or(self.description)
                .unwrap_or(reason)
        };
        let query = self.uri.query().unwrap_or_default();
        let parameter = detail.parameter.as_deref().map(|name| {
            let value = form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned());
            (name, value)
        });
        let href = format!("{}{}", surface.root, self.uri.path());
        let current = if query.is_empty() {
            href.clone()
        } else {
            format!("{href}?{query}")
        };
        let choices: Vec<(String, String)> = match &parameter {
            Some((name, _)) => detail
                .choices
                .iter()
                .map(|value| {
                    (
                        value.clone(),
                        workbench::query_replace(&current, name, value),
                    )
                })
                .collect(),
            None => Vec::new(),
        };
        let collection = self
            .collection_below()
            .filter(|id| pages.has_collection(id))
            .map(|id| {
                (
                    id.clone(),
                    format!(
                        "{}/collections/{}?f=html",
                        surface.root,
                        workbench::path_segment(&id)
                    ),
                )
            });
        workbench::error_html(
            surface,
            &workbench::ErrorView {
                status: self.status.as_u16(),
                title: detail.title.as_deref().unwrap_or(reason),
                message,
                parameter: parameter
                    .as_ref()
                    .map(|(name, value)| (*name, value.as_deref())),
                hint: detail.hint.as_deref(),
                choices: &choices,
                collection: collection
                    .as_ref()
                    .map(|(id, href)| (id.as_str(), href.as_str())),
                json_url: &workbench::with_format(&current, "json"),
            },
        )
    }

    /// The collection a resource below `/collections/{id}/…` belongs to.
    /// The collection's own page is not "below" it: a failure there links
    /// to the catalog only.
    fn collection_below(&self) -> Option<String> {
        let mut segments = self.uri.path().strip_prefix("/collections/")?.split('/');
        let id = segments.next().filter(|id| !id.is_empty())?;
        segments.next().filter(|rest| !rest.is_empty())?;
        form_urlencoded::parse(format!("id={}", id.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, id)| id.into_owned())
    }
}

/// The query parameter a struct-shaped query's rejection names, from the
/// deserializer's wording (``duplicate field `width` ``, ``unknown field
/// `x` ``): the field an HTML error page then marks. `None` when it names
/// none.
pub fn rejected_field(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once("field `")?;
    let (name, _) = rest.split_once('`')?;
    (!name.is_empty()).then_some(name)
}

/// The 404 for a path no route serves. A request that asked for HTML gets a
/// page with links into the workspace its path belongs to; any other gets
/// the empty 404 it always has. `base` is the external base URL.
pub fn not_found(base: &str, uri: &Uri, headers: &HeaderMap) -> Response {
    if wanted(uri.query(), headers) != Wanted::Html {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (api, mount) = workbench::workspace_of(uri.path());
    let root = format!("{base}{mount}");
    let href = format!("{base}{}", uri.path());
    let html = workbench::error_html(
        Surface {
            base,
            root: &root,
            api,
        },
        &workbench::ErrorView {
            status: StatusCode::NOT_FOUND.as_u16(),
            title: "Page not found",
            message: "No resource is served at this address. It may have moved, or the link may be mistyped.",
            parameter: None,
            hint: None,
            choices: &[],
            collection: None,
            json_url: &workbench::with_format(&href, "json"),
        },
    );
    let mut response = (StatusCode::NOT_FOUND, Html(html)).into_response();
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("accept"));
    response
}

/// `f` of a resource served only as JSON: none, `json` or
/// `application/json`. Anything else, `html` included, is the error message
/// of the 400 it gets, naming the supported format the way the legend's
/// does. `what` names the resource ("tile matrix sets").
pub fn json_only_format(f: Option<&str>, what: &str) -> Result<(), String> {
    match f.map(str::trim).filter(|f| !f.is_empty()) {
        None => Ok(()),
        Some(f) if f.eq_ignore_ascii_case("json") || f.eq_ignore_ascii_case("application/json") => {
            Ok(())
        }
        Some(other) => Err(format!(
            "Format '{other}' is not supported for {what}. Supported: json"
        )),
    }
}

/// The only query parameter of a route whose handler reads no query.
const FORMAT_ONLY: &[&str] = &["f"];

/// The `f` of a request to a route whose only query parameter is `f`
/// (#1123). A parameter of another name is the 400 naming `f`, worded as
/// on every route that rejects unknown parameters (root CLAUDE.md "Adding a
/// new API endpoint" rule 3), and its HTML page marks it; `f` given twice
/// is the 400 [`crate::JsonQuery`] answers.
fn format_only(uri: &Uri) -> Result<Option<String>, Box<Response>> {
    if let Some(name) = crate::subset::unknown_parameter(uri.query(), FORMAT_ONLY) {
        let message = crate::subset::unknown_parameter_message(&name, FORMAT_ONLY);
        let detail = ErrorDetail {
            parameter: Some(name),
            ..Default::default()
        };
        return Err(Box::new(detail.attach(crate::bad_request(&message))));
    }
    axum::extract::Query::<ds_core::html::FormatParams>::try_from_uri(uri)
        .map(|axum::extract::Query(params)| params.f)
        .map_err(|rejection| Box::new(crate::query_rejection(rejection)))
}

/// A GET route for a resource served only as JSON (#1132). It takes `f`
/// and no other parameter, which [`format_only`] rejects (#1123). A request
/// whose `f` names another format, `html` included, is the 400
/// [`json_only_format`] words, naming the one format the resource has, as
/// the legend's is; any other request reaches `handler` unchanged. `what`
/// names the resource ("tile matrix sets").
pub fn json_only_get<H, T, S>(handler: H, what: &'static str) -> axum::routing::MethodRouter<S>
where
    H: axum::handler::Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    format_only_route(handler, Some(what))
}

/// A GET route whose handler reads no query and serves one representation
/// whatever `f` says: the OpenAPI document at `/api`, which Common lists
/// with an `f`. Any other parameter, or `f` given twice, is the 400
/// [`format_only`] words (#1123).
pub fn format_only_get<H, T, S>(handler: H) -> axum::routing::MethodRouter<S>
where
    H: axum::handler::Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    format_only_route(handler, None)
}

/// `handler` behind [`format_only`], and behind [`json_only_format`] for
/// the resource `json_only` names.
fn format_only_route<H, T, S>(
    handler: H,
    json_only: Option<&'static str>,
) -> axum::routing::MethodRouter<S>
where
    H: axum::handler::Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    axum::routing::get(handler).layer(axum::middleware::from_fn(
        move |request: Request, next: Next| async move {
            let f = match format_only(request.uri()) {
                Ok(f) => f,
                Err(response) => return *response,
            };
            match json_only.map(|what| json_only_format(f.as_deref(), what)) {
                Some(Err(message)) => crate::bad_request(&message),
                _ => next.run(request).await,
            }
        },
    ))
}

/// Declare on the OpenAPI path item `item` that its GET is a
/// [`json_only_get`] route (#1132): the optional `f` it takes, `json` or
/// `application/json`, and the 400 any other value, any other parameter or
/// a repeated `f` gets (#1123), a page for a request that asked for HTML.
pub fn declare_json_only(item: &mut Value) {
    let get = &mut item["get"];
    let format = serde_json::json!({
        "name": "f",
        "in": "query",
        "required": false,
        "schema": {"type": "string", "enum": ["json", "application/json"]},
        "description": "Response format. The resource is served only as JSON; any other value, `html` included, is a 400 naming the supported format."
    });
    match get["parameters"].as_array_mut() {
        Some(parameters) => parameters.push(format),
        None => get["parameters"] = Value::Array(vec![format]),
    }
    get["responses"]["400"] = crate::bad_request_response(
        "Unsupported format, as the resource is served only as JSON; a query parameter other than `f`; or `f` given twice. A request that asked for HTML gets the error as a page.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Json;
    use ds_core::error::ErrorReason;
    use serde_json::json;
    use tower::ServiceExt;

    struct Pages;
    impl ErrorPages for Pages {
        fn base_url(&self, _: &HeaderMap) -> String {
            "https://x/proxy".into()
        }
        fn has_collection(&self, id: &str) -> bool {
            id == "radar"
        }
    }

    fn app() -> Router {
        let router = Router::new()
            .route(
                "/collections/{id}/position",
                get(|| async {
                    ErrorDetail {
                        parameter: Some("parameter-name".into()),
                        choices: vec!["DBZH".into(), "a&b".into()],
                        ..Default::default()
                    }
                    .attach(crate::JsonError(
                        StatusCode::BAD_REQUEST,
                        Json(json!({"code": "BadRequest", "description": "Unknown parameter 'x<y>'"})),
                    ))
                }),
            )
            .route(
                "/collections/{id}/boom",
                get(|| async {
                    crate::JsonError(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"code": "ServerError", "description": "secret: disk /var/x"})),
                    )
                }),
            )
            .route(
                "/collections/{id}/throttled",
                get(|| async { crate::source_unavailable() }),
            )
            .route("/ok", get(|| async { Json(json!({"ok": true})) }));
        // Nested as the server nests the EDR router.
        Router::new().nest("/edr", html_errors(router, "edr", "/edr", Arc::new(Pages)))
    }

    async fn fetch(app: &Router, uri: &str, accept: Option<&str>) -> (Response, String) {
        let mut request = Request::builder().uri(uri);
        if let Some(accept) = accept {
            request = request.header(header::ACCEPT, accept);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (
            Response::from_parts(parts, Body::empty()),
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    /// A JSON error from a route that can also answer it as a page varies on
    /// `Accept`, so a shared cache never serves one client the other's
    /// representation (#1123). `accept` joins a `Vary` already there, once;
    /// successes and non-JSON errors are left as they are.
    #[tokio::test]
    async fn json_errors_vary_on_accept() {
        let error = |vary: Option<&'static str>| {
            move || async move {
                let mut response = crate::bad_request("Unknown parameter 'x'");
                if let Some(vary) = vary {
                    response
                        .headers_mut()
                        .insert(header::VARY, HeaderValue::from_static(vary));
                }
                response
            }
        };
        let app = Router::new().nest(
            "/edr",
            html_errors(
                Router::new()
                    .route("/plain", get(error(None)))
                    .route("/origin", get(error(Some("Origin"))))
                    .route("/accept", get(error(Some("Accept-Encoding, Accept"))))
                    .route("/star", get(error(Some("*"))))
                    .route("/ok", get(|| async { Json(json!({"ok": true})) }))
                    .route(
                        "/text",
                        get(|| async { (StatusCode::BAD_REQUEST, "plain") }),
                    ),
                "edr",
                "/edr",
                Arc::new(Pages),
            ),
        );
        let vary = |response: &Response| -> Vec<String> {
            response
                .headers()
                .get_all(header::VARY)
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect()
        };
        for (uri, expected) in [
            ("/edr/plain", vec!["accept"]),
            ("/edr/plain?f=json", vec!["accept"]),
            ("/edr/origin", vec!["Origin, accept"]),
            ("/edr/accept", vec!["Accept-Encoding, Accept"]),
            ("/edr/star", vec!["*"]),
            ("/edr/ok", vec![]),
            ("/edr/text", vec![]),
        ] {
            let (response, _) = fetch(&app, uri, None).await;
            assert_eq!(vary(&response), expected, "{uri}");
        }
        // The page for an HTML request varies once too.
        let (response, _) = fetch(&app, "/edr/accept?f=html", None).await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(vary(&response), ["Accept-Encoding, Accept"]);
        let (response, _) = fetch(&app, "/edr/plain", Some("text/html")).await;
        assert_eq!(vary(&response), ["accept"]);
    }

    /// The representation is the request's first `f`, else `Accept`; an `f`
    /// naming another format is JSON.
    #[test]
    fn wanted_reads_the_raw_f_before_accept() {
        let html = |accept: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT, accept.parse().unwrap());
            headers
        };
        let none = HeaderMap::new();
        assert_eq!(wanted(Some("f=html&datetime=x"), &none), Wanted::Html);
        assert_eq!(wanted(Some("f=text%2Fhtml"), &none), Wanted::Html);
        assert_eq!(wanted(Some("f=json&f=html"), &none), Wanted::Json);
        assert_eq!(wanted(Some("f=png"), &html("text/html")), Wanted::Json);
        assert_eq!(wanted(None, &html("text/html,*/*;q=0.8")), Wanted::Html);
        assert_eq!(wanted(None, &html("*/*")), Wanted::Json);
        // Without `f`, only an Accept that ranks HTML first: a browser's,
        // not a client naming a data type above it (#1132).
        let browser =
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
        assert_eq!(wanted(None, &html(browser)), Wanted::Html);
        assert_eq!(
            wanted(None, &html("text/html, application/json")),
            Wanted::Html
        );
        assert_eq!(
            wanted(None, &html("application/json;q=0.5, text/html")),
            Wanted::Html
        );
        for accept in [
            "application/vnd.cov+json, text/html;q=0.1",
            "application/json, text/html",
            "text/html;q=0, application/json",
            "text/html;q=0",
            "image/png,*/*;q=0.8",
        ] {
            assert_eq!(wanted(None, &html(accept)), Wanted::Json, "{accept}");
        }
        // `f` still wins over any Accept.
        assert_eq!(
            wanted(Some("f=html"), &html("application/json")),
            Wanted::Html
        );
        assert_eq!(wanted(Some("bbox=1,2,3"), &none), Wanted::Json);
        // An empty `f` is no `f`: the same Accept rule as a missing one.
        let both = html("application/json, text/html");
        assert_eq!(wanted(Some("f="), &both), Wanted::Json);
        assert_eq!(wanted(Some("f=%20"), &both), Wanted::Json);
        assert_eq!(wanted(Some("f="), &html("text/html")), Wanted::Html);
    }

    /// An HTML request's JSON error is a page with the same status, the
    /// message, the parameter and its value, the accepted values as links
    /// re-running the request, and a way back; a JSON request's is unchanged.
    #[tokio::test]
    async fn json_errors_become_pages_for_html_requests_only() {
        let app = app();
        let uri =
            "/edr/collections/radar/position?coords=POINT(1%202)&parameter-name=x%3Cy%3E&f=html";
        let (response, html) = fetch(&app, uri, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(response.headers()[header::VARY]
            .to_str()
            .unwrap()
            .contains("accept"));
        // The request log still reads the reason.
        assert_eq!(
            response.extensions().get::<ErrorReason>().unwrap().0,
            "BadRequest: Unknown parameter 'x<y>'"
        );
        assert!(
            html.contains("Unknown parameter &#39;x&lt;y&gt;&#39;"),
            "{html}"
        );
        assert!(!html.contains("x<y>"));
        assert!(html.contains("<code>parameter-name</code>"));
        // Each valid value re-runs the request with it, keeping the rest.
        assert!(html.contains(
            "href=\"https://x/proxy/edr/collections/radar/position?coords=POINT%281+2%29&amp;parameter-name=DBZH&amp;f=html\""
        ), "{html}");
        assert!(html.contains("parameter-name=a%26b&amp;f=html\""), "{html}");
        // Back to the collection it belongs to, and to the catalog.
        assert!(html.contains("href=\"https://x/proxy/edr/collections/radar?f=html\""));
        assert!(html.contains("href=\"https://x/proxy/edr/collections?f=html\""));

        // JSON: the body is the handler's, byte for byte.
        let json = uri.replace("&f=html", "");
        for accept in [None, Some("application/json"), Some("*/*")] {
            let (response, body) = fetch(&app, &json, accept).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            assert_eq!(
                body,
                r#"{"code":"BadRequest","description":"Unknown parameter 'x<y>'"}"#
            );
        }
        // A browser's Accept asks for the page too.
        let (response, html) = fetch(&app, &json, Some("text/html,*/*;q=0.8")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(html.starts_with("<!DOCTYPE html>"));
        // A success is never touched.
        let (response, body) = fetch(&app, "/edr/ok?f=html", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body, r#"{"ok":true}"#);
    }

    /// A server error's page says only what the status does, never the
    /// body's text (Critical Rule 11); a collection the API does not serve
    /// gets no link back.
    #[tokio::test]
    async fn server_error_pages_hold_no_internal_detail() {
        let (response, html) = fetch(&app(), "/edr/collections/other/boom?f=html", None).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!html.contains("secret"));
        assert!(!html.contains("/var/x"));
        assert!(html.contains("The server could not complete this request."));
        assert!(!html.contains("Back to other"));
    }

    /// A data source that keeps refusing reads (#1355) is a 503 page that
    /// says so, keeping the JSON answer's `Retry-After`.
    #[tokio::test]
    async fn an_unavailable_source_is_a_page_of_its_own() {
        let uri = "/edr/collections/radar/throttled?f=html";
        let (response, html) = fetch(&app(), uri, None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "10");
        assert!(
            html.contains("A data source is not answering for now."),
            "{html}"
        );
        assert!(!html.contains("The server is busy"), "{html}");
    }

    #[test]
    fn rejected_fields_are_read_from_the_deserializer_message() {
        assert_eq!(
            rejected_field("Failed to deserialize query string: duplicate field `width`"),
            Some("width")
        );
        assert_eq!(
            rejected_field("unknown field `x`, expected one of `f`, `bbox`"),
            Some("x")
        );
        assert_eq!(rejected_field("invalid digit found in string"), None);
        assert_eq!(rejected_field("field ``"), None);
    }

    /// A handler's message replaces the body's on a 4xx page; a 5xx page
    /// still says only what its status does, ignoring the whole detail.
    #[tokio::test]
    async fn a_detail_message_replaces_a_4xx_description_only() {
        let failing = |status: StatusCode| {
            get(move || async move {
                ErrorDetail {
                    title: Some("No data in this area".into()),
                    message: Some("Nothing <here>.".into()),
                    hint: Some("Try 2026.".into()),
                    ..Default::default()
                }
                .attach(crate::JsonError(
                    status,
                    Json(json!({"code": "X", "description": "Location not found: no data"})),
                ))
            })
        };
        let app = Router::new().nest(
            "/edr",
            html_errors(
                Router::new()
                    .route("/collections/{id}/area", failing(StatusCode::NOT_FOUND))
                    .route("/collections/{id}/boom", failing(StatusCode::BAD_GATEWAY)),
                "edr",
                "/edr",
                Arc::new(Pages),
            ),
        );
        let (response, html) = fetch(&app, "/edr/collections/radar/area?f=html", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            html.contains("<h1>No data in this area</h1><p>Nothing &lt;here&gt;.</p>"),
            "{html}"
        );
        assert!(html.contains("Try 2026."));
        assert!(!html.contains("Location not found"));
        let (_, html) = fetch(&app, "/edr/collections/radar/boom?f=html", None).await;
        assert!(html.contains("The server could not complete this request."));
        assert!(!html.contains("Nothing"));
        assert!(!html.contains("No data in this area"), "{html}");
        assert!(!html.contains("Try 2026."), "{html}");
        assert!(html.contains("<h1>Bad Gateway</h1>"), "{html}");
    }

    /// A JSON-only route answers an `f` naming another format with the 400
    /// naming JSON, a page for an HTML request; JSON requests reach it.
    #[tokio::test]
    async fn json_only_routes_reject_other_formats() {
        let app = html_errors(
            Router::new().route(
                "/tileMatrixSets",
                json_only_get(
                    || async { Json(json!({"tileMatrixSets": []})) },
                    "tile matrix sets",
                ),
            ),
            "tiles",
            "",
            Arc::new(Pages),
        );
        for uri in [
            "/tileMatrixSets",
            "/tileMatrixSets?f=json",
            "/tileMatrixSets?f=application%2Fjson",
        ] {
            let (response, body) = fetch(&app, uri, Some("text/html")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(body, r#"{"tileMatrixSets":[]}"#);
        }
        let (response, html) = fetch(&app, "/tileMatrixSets?f=html", None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            html.contains(
                "Format &#39;html&#39; is not supported for tile matrix sets. Supported: json"
            ),
            "{html}"
        );
        let (response, body) = fetch(&app, "/tileMatrixSets?f=png", Some("text/html")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            r#"{"code":"BadRequest","description":"Format 'png' is not supported for tile matrix sets. Supported: json"}"#
        );
    }

    /// A route whose handler reads no query takes `f` and nothing else: an
    /// unknown parameter is the JSON 400 naming `f`, not a 200 that ignored
    /// it, and a repeated `f` the 400 `JsonQuery` answers (#1123). An HTML
    /// request gets the page marking the parameter.
    #[tokio::test]
    async fn format_only_routes_reject_other_and_repeated_parameters() {
        let handler = || async { Json(json!({"ok": true})) };
        let app = html_errors(
            Router::new()
                .route(
                    "/tileMatrixSets",
                    json_only_get(handler, "tile matrix sets"),
                )
                .route("/api", format_only_get(handler)),
            "tiles",
            "",
            Arc::new(Pages),
        );
        for uri in ["/tileMatrixSets?f=json", "/api", "/api?f=html", "/api?"] {
            let (response, body) = fetch(&app, uri, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(body, r#"{"ok":true}"#, "{uri}");
        }
        for path in ["/tileMatrixSets", "/api"] {
            for (query, description) in [
                (
                    "f=json&foo=1",
                    "Unknown query parameter 'foo'. Supported: f",
                ),
                (
                    "datetime=2026",
                    "Unknown query parameter 'datetime'. Supported: f",
                ),
                (
                    "f=json&f=json",
                    "Failed to deserialize query string: duplicate field `f`",
                ),
            ] {
                let uri = format!("{path}?{query}");
                let (response, body) = fetch(&app, &uri, None).await;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(
                    body,
                    json!({"code": "BadRequest", "description": description}),
                    "{uri}"
                );
            }
            let uri = format!("{path}?foo=%3Cb%3E");
            let (response, html) = fetch(&app, &uri, Some("text/html")).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
            assert!(
                html.contains("Unknown query parameter &#39;foo&#39;"),
                "{html}"
            );
            assert!(
                html.contains("Parameter <code>foo</code>: <code>&lt;b&gt;</code>"),
                "{html}"
            );
        }
    }

    /// The OpenAPI of a JSON-only route declares its `f` and its 400,
    /// keeping the parameters it had.
    #[test]
    fn json_only_routes_declare_f_and_the_400() {
        let mut item = json!({"get": {"responses": {"200": {"description": "ok"}}}});
        declare_json_only(&mut item);
        let get = &item["get"];
        assert_eq!(get["parameters"][0]["name"], "f");
        assert_eq!(
            get["parameters"][0]["schema"]["enum"],
            json!(["json", "application/json"])
        );
        assert!(get["responses"]["400"]["content"]["text/html"].is_object());
        assert!(get["responses"]["400"]["content"]["application/json"].is_object());
        let mut item = json!({"get": {"parameters": [{"name": "tileMatrixSetId"}]}});
        declare_json_only(&mut item);
        let names: Vec<_> = item["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].clone())
            .collect();
        assert_eq!(names, [json!("tileMatrixSetId"), json!("f")]);
    }

    #[test]
    fn json_only_resources_name_the_format_they_serve() {
        for f in [
            None,
            Some(""),
            Some("json"),
            Some("JSON"),
            Some("application/json"),
        ] {
            assert_eq!(json_only_format(f, "tile matrix sets"), Ok(()), "{f:?}");
        }
        assert_eq!(
            json_only_format(Some("html"), "tile matrix sets").unwrap_err(),
            "Format 'html' is not supported for tile matrix sets. Supported: json"
        );
    }

    /// An unknown path: an HTML 404 page for a browser, in the workspace
    /// its path is under; the empty 404 for anything else.
    #[tokio::test]
    async fn unknown_paths_are_an_html_404_for_browsers_only() {
        let browser = {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT, "text/html".parse().unwrap());
            headers
        };
        let response = not_found("https://x", &"/edr/nope".parse().unwrap(), &browser);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Page not found"));
        assert!(html.contains("href=\"https://x/edr/collections?f=html\""));
        let response = not_found("https://x", &"/nope".parse().unwrap(), &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }
}
