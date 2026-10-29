use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

/// WMS error codes per WMS 1.3.0 spec.
#[derive(Debug)]
pub enum WmsError {
    /// Layer does not exist.
    LayerNotDefined(String),
    /// Style does not exist.
    StyleNotDefined(String),
    /// CRS is not offered for the requested layer: WMS 1.3.0 Table E.1's
    /// `InvalidCRS`.
    InvalidCrs(String),
    /// Invalid dimension value (e.g., TIME).
    InvalidDimensionValue(String),
    /// Missing required parameter.
    MissingParameterValue(String),
    /// Invalid format requested.
    InvalidFormat(String),
    /// General invalid parameter.
    InvalidParameterValue(String),
    /// Operation not supported.
    OperationNotSupported(String),
    /// A GetFeatureInfo `QUERY_LAYERS` layer is not queryable: WMS 1.3.0
    /// Table E.1's `LayerNotQueryable`.
    LayerNotQueryable(String),
    /// A GetFeatureInfo `I` or `J` that is no pixel of the map: Table E.1's
    /// `InvalidPoint`.
    InvalidPoint(String),
    /// Internal server error.
    Internal(String),
    /// Server too busy (render semaphore exhausted).
    ServiceUnavailable(String),
    /// A data source that kept refusing the reads
    /// (`DataServerError::Unavailable`, #1355): the busy answer's 503, with
    /// `Retry-After: 10` and a message of its own that names no upstream.
    SourceUnavailable,
}

impl WmsError {
    pub fn missing_parameter(name: &str) -> Self {
        WmsError::MissingParameterValue(format!("Missing required parameter: {name}"))
    }

    pub fn invalid_parameter(msg: &str) -> Self {
        WmsError::InvalidParameterValue(msg.to_string())
    }

    pub fn invalid_crs(crs: &str) -> Self {
        WmsError::InvalidCrs(format!("CRS '{crs}' is not supported"))
    }

    pub fn invalid_format(format: &str) -> Self {
        WmsError::InvalidFormat(format!(
            "Format '{format}' is not supported. Use {}.",
            crate::params::SUPPORTED_FORMATS.join(", ")
        ))
    }

    /// An `INFO_FORMAT` GetFeatureInfo does not answer in: `InvalidFormat`,
    /// naming the formats it does.
    pub fn invalid_info_format(format: &str) -> Self {
        WmsError::InvalidFormat(format!(
            "INFO_FORMAT '{format}' is not supported. Use {}.",
            crate::params::INFO_FORMATS.join(", ")
        ))
    }

    pub fn layer_not_found(layer: &str) -> Self {
        WmsError::LayerNotDefined(format!("Layer '{layer}' does not exist"))
    }

    pub fn operation_not_supported(op: &str) -> Self {
        WmsError::OperationNotSupported(format!("Operation '{op}' is not supported"))
    }

    fn error_code(&self) -> &str {
        match self {
            WmsError::LayerNotDefined(_) => "LayerNotDefined",
            WmsError::StyleNotDefined(_) => "StyleNotDefined",
            WmsError::InvalidCrs(_) => "InvalidCRS",
            WmsError::InvalidDimensionValue(_) => "InvalidDimensionValue",
            WmsError::MissingParameterValue(_) => "MissingParameterValue",
            WmsError::InvalidFormat(_) => "InvalidFormat",
            WmsError::InvalidParameterValue(_) => "InvalidParameterValue",
            WmsError::OperationNotSupported(_) => "OperationNotSupported",
            WmsError::LayerNotQueryable(_) => "LayerNotQueryable",
            WmsError::InvalidPoint(_) => "InvalidPoint",
            WmsError::Internal(_) => "Internal",
            WmsError::ServiceUnavailable(_) | WmsError::SourceUnavailable => "Internal",
        }
    }

    /// Client-visible message — written into the WMS XML response body.
    ///
    /// 5xx variants must NOT echo the inner string here, because the
    /// `ServiceExceptionReport` is shipped to the requesting client. Per
    /// CLAUDE.md ("Do not leak internal error details to clients — use
    /// generic messages for 500 errors") and the `MapsError` precedent,
    /// `Internal` and `ServiceUnavailable` get a fixed redacted message;
    /// the original detail is captured at the handler via `tracing::warn!`
    /// before the error is mapped, so operators don't lose it.
    fn message(&self) -> &str {
        match self {
            WmsError::LayerNotDefined(m)
            | WmsError::StyleNotDefined(m)
            | WmsError::InvalidCrs(m)
            | WmsError::InvalidDimensionValue(m)
            | WmsError::MissingParameterValue(m)
            | WmsError::InvalidFormat(m)
            | WmsError::InvalidParameterValue(m)
            | WmsError::OperationNotSupported(m)
            | WmsError::LayerNotQueryable(m)
            | WmsError::InvalidPoint(m) => m,
            WmsError::Internal(_) => "Internal server error",
            WmsError::ServiceUnavailable(_) => "Server busy, try again later",
            WmsError::SourceUnavailable => ds_core::error::DataServerError::UNAVAILABLE_DESCRIPTION,
        }
    }

    fn status_code(&self) -> StatusCode {
        match self {
            WmsError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WmsError::ServiceUnavailable(_) | WmsError::SourceUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::BAD_REQUEST,
        }
    }

    /// The seconds a 503 asks the client to wait (`Retry-After`): the busy
    /// answer's 1, a data source's 10 (#1355).
    fn retry_after(&self) -> Option<u64> {
        match self {
            WmsError::ServiceUnavailable(_) => Some(1),
            WmsError::SourceUnavailable => {
                Some(ds_core::error::DataServerError::UNAVAILABLE_RETRY_AFTER_SECS)
            }
            _ => None,
        }
    }

    /// Serialize to WMS 1.3.0 ServiceExceptionReport XML.
    fn to_xml(&self) -> Vec<u8> {
        let mut writer = Writer::new(Vec::new());

        let _ = writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));

        let mut root = BytesStart::new("ServiceExceptionReport");
        root.push_attribute(("version", "1.3.0"));
        root.push_attribute(("xmlns", "http://www.opengis.net/ogc"));
        let _ = writer.write_event(Event::Start(root));

        let mut exception = BytesStart::new("ServiceException");
        exception.push_attribute(("code", self.error_code()));
        let _ = writer.write_event(Event::Start(exception));
        let _ = writer.write_event(Event::Text(BytesText::new(self.message())));
        let _ = writer.write_event(Event::End(BytesEnd::new("ServiceException")));

        let _ = writer.write_event(Event::End(BytesEnd::new("ServiceExceptionReport")));

        writer.into_inner()
    }
}

impl IntoResponse for WmsError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        let retry_after = self.retry_after();
        let reason =
            ds_core::error::ErrorReason(format!("{}: {}", self.error_code(), self.message()));
        let xml = self.to_xml();
        // WMS 1.3.0's MIME type for the `XML` exception format. WMS 1.1.1's
        // `application/vnd.ogc.se_xml` is not a 1.3.0 type: the OGC test
        // suite fails it (#1065).
        let mut response = (
            status,
            [
                (header::CONTENT_TYPE, "text/xml"),
                (
                    header::HeaderName::from_static("x-content-type-options"),
                    "nosniff",
                ),
            ],
            xml,
        )
            .into_response();
        if let Some(seconds) = retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, axum::http::HeaderValue::from(seconds));
        }
        response.extensions_mut().insert(reason);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::error::ErrorReason;

    /// Locks in the contract that the request-logging middleware depends on:
    /// every WmsError → response must carry an `ErrorReason` extension. A
    /// future refactor that drops the `extensions_mut().insert(...)` call
    /// would silently re-empty the `error` field in production Loki logs;
    /// catching it here means it can't.
    #[test]
    fn into_response_attaches_error_reason_extension() {
        let err = WmsError::layer_not_found("some-bogus-layer");
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reason = response
            .extensions()
            .get::<ErrorReason>()
            .expect("ErrorReason must be attached so request_logging_middleware can pick it up");
        assert!(
            reason.0.starts_with("LayerNotDefined: "),
            "reason should be `code: message` for grep-friendly logs, got {:?}",
            reason.0
        );
        assert!(
            reason.0.contains("some-bogus-layer"),
            "reason should carry the offending value, got {:?}",
            reason.0
        );
    }

    #[test]
    fn into_response_redacts_internal_message_in_body_and_reason() {
        // The XML body and ErrorReason both carry a fixed generic message
        // for 5xx — the inner panic detail must not leak to clients via
        // the response body, and ErrorReason mirrors the body so log lines
        // and user-visible errors stay consistent. Operators get the
        // original detail via `tracing::warn!` at the handler before the
        // error is mapped to WmsError::Internal.
        let err = WmsError::Internal("render task panicked: secret detail".to_string());
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let reason = response.extensions().get::<ErrorReason>().expect(
            "5xx must also attach ErrorReason — operators triage these from Loki, not response bodies",
        );
        assert_eq!(reason.0, "Internal: Internal server error");
        assert!(
            !reason.0.contains("secret detail"),
            "inner panic detail must not leak via ErrorReason, got {:?}",
            reason.0
        );
    }

    /// A data source that keeps refusing reads (#1355) is the busy
    /// answer's 503 ServiceException with a longer `Retry-After` and a
    /// message of its own.
    #[test]
    fn source_unavailable_is_a_503_with_a_longer_retry_after() {
        let xml = String::from_utf8(WmsError::SourceUnavailable.to_xml()).unwrap();
        assert!(
            xml.contains("Data source temporarily unavailable, try again later"),
            "{xml}"
        );
        let response = WmsError::SourceUnavailable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "10");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/xml");
        let busy = WmsError::ServiceUnavailable("shed".into()).into_response();
        assert_eq!(busy.headers()[header::RETRY_AFTER], "1");
    }

    #[test]
    fn xml_body_does_not_leak_internal_detail() {
        let err = WmsError::Internal("connection refused at 10.0.0.5:5432".to_string());
        let xml = err.to_xml();
        let xml_str = std::str::from_utf8(&xml).expect("valid utf-8");
        assert!(
            xml_str.contains("Internal server error"),
            "body must carry the redacted message, got {xml_str}"
        );
        assert!(
            !xml_str.contains("10.0.0.5"),
            "body must not echo internal connection detail, got {xml_str}"
        );
    }

    /// The codes WMS 1.3.0 Table E.1 defines go out spelled as it spells
    /// them: clients and the OGC test suite match them exactly. An
    /// unsupported CRS was `CRSNotDefined`, which ets-wms13's
    /// `getmap:invalid-crs` fails (#1065).
    #[test]
    fn table_e1_codes_are_spelled_as_the_spec_spells_them() {
        for (err, code) in [
            (WmsError::invalid_crs("UndefinedCRS"), "InvalidCRS"),
            (WmsError::invalid_format("image/gif"), "InvalidFormat"),
            (WmsError::layer_not_found("NonExistant"), "LayerNotDefined"),
            (WmsError::StyleNotDefined("x".into()), "StyleNotDefined"),
            (
                WmsError::InvalidDimensionValue("x".into()),
                "InvalidDimensionValue",
            ),
            (
                WmsError::operation_not_supported("DescribeLayer"),
                "OperationNotSupported",
            ),
            (WmsError::LayerNotQueryable("x".into()), "LayerNotQueryable"),
            (WmsError::InvalidPoint("x".into()), "InvalidPoint"),
            (WmsError::invalid_info_format("text/xml"), "InvalidFormat"),
        ] {
            let xml = String::from_utf8(err.to_xml()).expect("valid utf-8");
            assert_eq!(service_exception_code(&xml).as_deref(), Some(code), "{xml}");
        }
    }

    /// The `code` attribute of the report's `<ServiceException>`, read with
    /// quick-xml rather than matched as text (check_geo_safety.sh).
    fn service_exception_code(xml: &str) -> Option<String> {
        use quick_xml::events::Event;
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event().ok()? {
                Event::Start(e) | Event::Empty(e)
                    if e.local_name().as_ref() == "ServiceException" =>
                {
                    let attr = e.try_get_attribute("code").ok()??;
                    return Some(attr.value.into_owned());
                }
                Event::Eof => return None,
                _ => {}
            }
        }
    }

    /// Lock the `InvalidFormat` message to `params::SUPPORTED_FORMATS` so a
    /// future format addition can't leave the error hint stale. The
    /// drift-guard test in params.rs only asserts the slice round-trips
    /// through `parse_image_format`; the rejection message lives here.
    #[test]
    fn invalid_format_message_lists_every_supported_format() {
        let WmsError::InvalidFormat(msg) = WmsError::invalid_format("image/gif") else {
            panic!("invalid_format() must produce InvalidFormat");
        };
        for fmt in crate::params::SUPPORTED_FORMATS {
            assert!(
                msg.contains(fmt),
                "InvalidFormat message {msg:?} should mention {fmt:?}"
            );
        }
    }
}
