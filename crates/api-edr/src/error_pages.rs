//! What an EDR error page says beyond the JSON body (#1132).
//!
//! `api_common::errors::html_errors` turns every JSON error of a request that
//! asked for HTML into a workbench page. Two EDR failures read better with
//! what only this API knows: an unknown `parameter-name` lists the
//! collection's parameters as links that re-run the query, and a data query
//! with no data says so in words that fit its query type, with the
//! collection's time range. [`error_detail`] attaches both as an
//! [`ErrorDetail`]; JSON responses are never touched.

use api_common::errors::{self, ErrorDetail};
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use ds_core::edr_engine::{select_parameters, EdrEngine};
use ds_core::error::DataServerError;
use ds_core::html::Wanted;

use crate::handlers::{request_base_url, AppState};

/// The error-page wrapper of the EDR router mounted at `mount`: its base
/// URL and catalog tell a page where to link back to.
pub(crate) fn wrap(router: axum::Router, state: AppState, mount: &'static str) -> axum::Router {
    let catalog = state.clone();
    let pages = errors::pages(
        move |headers| request_base_url(&state.load(), headers),
        move |id| catalog.load().engines.contains_key(id),
    );
    errors::html_errors(router, "edr", mount, pages)
}

/// Attach an [`ErrorDetail`] to a data query's 400 or 404 when the request
/// asked for HTML. Runs after routing, inside `html_errors`.
pub(crate) async fn error_detail(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if errors::wanted(request.uri().query(), request.headers()) != Wanted::Html {
        return next.run(request).await;
    }
    // The data query the route names, from `query_form`'s one list.
    let query_type = crate::query_form::routed_query(&request).map(|(query, _)| query);
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(request.uri())
        .map(|Query(pairs)| pairs)
        .unwrap_or_default();
    let (request, id, _) = crate::query_form::route_ids(request).await;
    let mut response = next.run(request).await;
    let (Some(query_type), Some(id)) = (query_type, id) else {
        return response;
    };
    if response.extensions().get::<ErrorDetail>().is_some() {
        return response;
    }
    let Some(engine) = state.load().engines.get(&id).cloned() else {
        return response;
    };
    let value = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let detail = match response.status() {
        StatusCode::BAD_REQUEST => {
            unknown_parameter(engine.as_ref(), value("parameter-name"), &response)
        }
        StatusCode::NOT_FOUND if is_no_data(&response) => {
            Some(no_data(engine.as_ref(), query_type, value("datetime")))
        }
        _ => None,
    };
    if let Some(detail) = detail {
        response.extensions_mut().insert(detail);
    }
    response
}

/// The detail of a 400 for a `parameter-name` naming a parameter the
/// collection does not advertise: each advertised one, as a link. Only a 400
/// that is about it: a request failing first on another parameter keeps
/// that parameter's page.
fn unknown_parameter(
    engine: &dyn EdrEngine,
    requested: Option<&str>,
    response: &Response,
) -> Option<ErrorDetail> {
    let requested = crate::params::parse_parameter_names(requested)?;
    let available = engine.get_parameters();
    let names: Vec<&str> = available.iter().map(String::as_str).collect();
    let rejection = match select_parameters(Some(requested.as_slice()), &names) {
        Err(DataServerError::InvalidParameter(message)) => message,
        _ => return None,
    };
    let reason = response.extensions().get::<ds_core::error::ErrorReason>()?;
    if !reason.0.contains(&rejection) {
        return None;
    }
    Some(ErrorDetail {
        parameter: Some("parameter-name".into()),
        title: Some("Unknown parameter".into()),
        choices: available,
        ..Default::default()
    })
}

/// Whether a 404 is the engines' "no data" (`LocationNotFound`), which a
/// data query answers for a window or place without data.
fn is_no_data(response: &Response) -> bool {
    let prefix = DataServerError::LocationNotFound(String::new()).to_string();
    response
        .extensions()
        .get::<ds_core::error::ErrorReason>()
        .is_some_and(|reason| {
            reason
                .0
                .strip_prefix("NotFound: ")
                .is_some_and(|description| description.starts_with(&prefix))
        })
}

/// The detail of a data query that found no data: wording for its query
/// type and the time range the collection advertises.
fn no_data(engine: &dyn EdrEngine, query_type: &str, datetime: Option<&str>) -> ErrorDetail {
    let (title, place) = match query_type {
        "position" => ("No data at this position", "at this position"),
        "trajectory" => ("No data along this path", "along this path"),
        _ => ("No data in this area", "inside this area"),
    };
    let when = match datetime.filter(|d| !d.trim().is_empty()) {
        Some(datetime) => format!(" at datetime {datetime}"),
        None => String::new(),
    };
    let hint = match engine.get_temporal_extent() {
        Some((start, end)) => format!(
            "The collection advertises data from {} to {}. Choose a time in that range, or another place.",
            start.format("%Y-%m-%dT%H:%M:%SZ"),
            end.format("%Y-%m-%dT%H:%M:%SZ")
        ),
        None => "Try another time, another area or fewer parameters.".to_owned(),
    };
    ErrorDetail {
        title: Some(title.into()),
        message: Some(format!(
            "The collection has no data {place}{when} for the requested parameters."
        )),
        hint: Some(hint),
        ..Default::default()
    }
}
