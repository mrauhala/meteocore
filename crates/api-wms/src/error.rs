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
    /// CRS is not supported.
    CrsNotDefined(String),
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
    /// Internal server error.
    Internal(String),
    /// Server too busy (render semaphore exhausted).
    ServiceUnavailable(String),
}

impl WmsError {
    pub fn missing_parameter(name: &str) -> Self {
        WmsError::MissingParameterValue(format!("Missing required parameter: {name}"))
    }

    pub fn invalid_parameter(msg: &str) -> Self {
        WmsError::InvalidParameterValue(msg.to_string())
    }

    pub fn invalid_crs(crs: &str) -> Self {
        WmsError::CrsNotDefined(format!("CRS '{crs}' is not supported"))
    }

    pub fn invalid_format(format: &str) -> Self {
        WmsError::InvalidFormat(format!(
            "Format '{format}' is not supported. Use image/png."
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
            WmsError::CrsNotDefined(_) => "CRSNotDefined",
            WmsError::InvalidDimensionValue(_) => "InvalidDimensionValue",
            WmsError::MissingParameterValue(_) => "MissingParameterValue",
            WmsError::InvalidFormat(_) => "InvalidFormat",
            WmsError::InvalidParameterValue(_) => "InvalidParameterValue",
            WmsError::OperationNotSupported(_) => "OperationNotSupported",
            WmsError::Internal(_) => "Internal",
            WmsError::ServiceUnavailable(_) => "Internal",
        }
    }

    fn message(&self) -> &str {
        match self {
            WmsError::LayerNotDefined(m)
            | WmsError::StyleNotDefined(m)
            | WmsError::CrsNotDefined(m)
            | WmsError::InvalidDimensionValue(m)
            | WmsError::MissingParameterValue(m)
            | WmsError::InvalidFormat(m)
            | WmsError::InvalidParameterValue(m)
            | WmsError::OperationNotSupported(m)
            | WmsError::Internal(m)
            | WmsError::ServiceUnavailable(m) => m,
        }
    }

    fn status_code(&self) -> StatusCode {
        match self {
            WmsError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            WmsError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_REQUEST,
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
        let reason =
            ds_core::error::ErrorReason(format!("{}: {}", self.error_code(), self.message()));
        let xml = self.to_xml();
        let mut response = (
            status,
            [
                (header::CONTENT_TYPE, "application/vnd.ogc.se_xml"),
                (
                    header::HeaderName::from_static("x-content-type-options"),
                    "nosniff",
                ),
            ],
            xml,
        )
            .into_response();
        response.extensions_mut().insert(reason);
        response
    }
}
