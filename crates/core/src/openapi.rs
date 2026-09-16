/// Documentation pages load only embedded, version-pinned scripts. Swagger UI
/// needs inline styles for its controls, but neither inline scripts nor eval.
pub const SWAGGER_UI_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Framework-free asset lookup shared by every API's documentation route.
/// Unknown names never access the filesystem.
pub fn swagger_ui_asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    match name {
        "swagger-ui-5.33.0.js" => Some((
            "text/javascript; charset=utf-8",
            include_bytes!("../assets/swagger-ui-5.33.0/swagger-ui-bundle.js"),
        )),
        "swagger-ui-5.33.0.css" => Some((
            "text/css; charset=utf-8",
            include_bytes!("../assets/swagger-ui-5.33.0/swagger-ui.css"),
        )),
        "init.js" => Some((
            "text/javascript; charset=utf-8",
            include_bytes!("../assets/swagger-init.js"),
        )),
        "layout.css" => Some((
            "text/css; charset=utf-8",
            include_bytes!("../assets/swagger-layout.css"),
        )),
        "LICENSE" => Some((
            "text/plain; charset=utf-8",
            include_bytes!("../assets/swagger-ui-5.33.0/LICENSE"),
        )),
        "NOTICE" => Some((
            "text/plain; charset=utf-8",
            include_bytes!("../assets/swagger-ui-5.33.0/NOTICE"),
        )),
        "swagger-ui-bundle.js.LICENSE.txt" => Some((
            "text/plain; charset=utf-8",
            include_bytes!("../assets/swagger-ui-5.33.0/swagger-ui-bundle.js.LICENSE.txt"),
        )),
        _ => None,
    }
}

/// Generate Swagger UI using same-origin assets relative to `/api/docs`.
/// Spec URLs go through an escaped data attribute, never JavaScript source.
pub fn swagger_ui_html(title: &str, spec_url: &str) -> String {
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{title}</title>
  <link rel="stylesheet" href="docs/swagger-ui-5.33.0.css">
  <link rel="stylesheet" href="docs/layout.css">
</head>
<body>
  <div id="swagger-ui" data-spec-url="{spec_url}"></div>
  <script src="docs/swagger-ui-5.33.0.js" defer></script>
  <script src="docs/init.js" defer></script>
</body>
</html>"##,
        title = crate::html::escape(title),
        spec_url = crate::html::escape(spec_url),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_use_only_local_assets_and_escape_dynamic_values() {
        let html = swagger_ui_html("</title><script>alert(1)</script>", "\" onerror=\"alert(1)");
        assert!(!html.contains("<script>"));
        assert!(!html.contains("https://"));
        assert!(html.contains("&lt;/title&gt;"));
        assert!(html.contains("data-spec-url=\"&quot; onerror=&quot;alert(1)\""));
        for name in [
            "swagger-ui-5.33.0.js",
            "swagger-ui-5.33.0.css",
            "init.js",
            "layout.css",
        ] {
            assert!(html.contains(&format!("docs/{name}")));
            assert!(!swagger_ui_asset(name).unwrap().1.is_empty());
        }
        assert!(swagger_ui_asset("../Cargo.toml").is_none());
        assert!(swagger_ui_asset("unknown.js").is_none());
    }
}
