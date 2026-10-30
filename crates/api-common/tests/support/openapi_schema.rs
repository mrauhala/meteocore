//! Offline validation of `/api` documents against the bundled OpenAPI 3.0
//! meta-schema (`schemas/openapi-3.0.json`). Shared, as a `#[path]` module,
//! by every suite that checks an `/api` document: api-edr, api-features,
//! api-maps and the server's `common_discovery`.
//!
//! The meta-schema declares JSON Schema Draft 4, where jsonschema recognises
//! no `uri-reference` format, and it skips a format it does not recognise
//! unless told otherwise. Compiled plainly, none of the meta-schema's
//! `uri-reference` fields would be checked: every Reference Object `$ref`,
//! `termsOfService`, `Contact.url`, `License.url`, `ExternalDocumentation.url`
//! and the OAuth URLs. This helper registers an RFC 3986 `URI-reference`
//! check and fails to compile on any other format it does not recognise.
#![allow(dead_code)]

use std::sync::LazyLock;

use jsonschema::Validator;
use serde_json::Value;

static OPENAPI_3_0: LazyLock<Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(include_str!("../../../../schemas/openapi-3.0.json"))
        .expect("bundled OpenAPI 3.0 meta-schema parses");
    jsonschema::options()
        .with_format("uri-reference", is_uri_reference)
        .should_ignore_unknown_formats(false)
        .build(&schema)
        .expect("OpenAPI 3.0 meta-schema compiles")
});

/// RFC 3986 `URI-reference`: a URI or a relative reference, such as
/// `#/components/schemas/link`, `responses.json#/ok` or `../common.json`.
/// `jsonschema::uri::from_str` parses it with fluent-uri's `UriRef`, the
/// parser jsonschema's own Draft 6+ `uri-reference` check falls back on;
/// its only other step, resolving against an authority base, cannot fail.
pub fn is_uri_reference(text: &str) -> bool {
    jsonschema::uri::from_str(text).is_ok()
}

/// Every violation of the OpenAPI 3.0 meta-schema in `api`.
pub fn errors(api: &Value) -> Vec<String> {
    OPENAPI_3_0
        .iter_errors(api)
        .map(|error| format!("{error} at {}", error.instance_path()))
        .collect()
}

/// `api` is a valid OpenAPI 3.0 document.
pub fn assert_valid(api: &Value, context: &str) {
    let errors = errors(api);
    assert!(
        errors.is_empty(),
        "{context}: /api is not valid OpenAPI 3.0:\n{}",
        errors.join("\n")
    );
}
