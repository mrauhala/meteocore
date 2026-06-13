//! Reverse-proxy-aware base-URL resolution (issue #12).
//!
//! When the server sits behind a reverse proxy, the address it bound to is not
//! the address clients use, so absolute self-links generated from the static
//! configured base URL are wrong (`http://0.0.0.0:8000/edr/...`). With
//! `[server] trust_proxy_headers = true` each request's self-links are instead
//! derived from the standard proxy forwarding headers.
//!
//! This module is **framework-free**: the HTTP layer passes header values in
//! through a closure, so ds-core never depends on `axum`/`http` (see the
//! "ds-core has no framework dependencies" rule). The axum handlers call
//! [`resolve_base_url`] with `|name| headers.get(name).and_then(|v|
//! v.to_str().ok())`.
//!
//! # Security
//!
//! Forwarding headers are client-controllable, so resolution is **off by
//! default** and only honoured when the operator opts in (the proxy is trusted
//! to set/overwrite these headers). Even then, host values are sanitised
//! (whitespace, slashes, `@`, non-ASCII and other authority-breaking characters
//! are rejected) and the scheme is restricted to `http`/`https`. A malformed or
//! suspicious header never produces a broken or attacker-chosen URL: resolution
//! simply falls through to the next source, ultimately the static fallback.

/// Resolve the effective base URL for a single request, without a trailing
/// slash.
///
/// Precedence when `trust` is `true`:
///   1. RFC 7239 `Forwarded` — `proto=` / `host=` of the **last** element (the
///      closest trusted proxy; see [`from_forwarded`] for why not the first).
///   2. `X-Forwarded-Proto` + `X-Forwarded-Host` (+ optional `X-Forwarded-Port`).
///   3. `fallback` — the startup-resolved base (`BASE_URL` env > `[server]
///      base_url` > `http://{host}:{port}`).
///
/// When `trust` is `false` (the default) `fallback` is returned unchanged, so
/// there is no behaviour change without the flag.
///
/// `header` returns the raw value of a header by its lowercase name (or `None`
/// when absent). For headers that may legitimately carry a comma-separated list
/// of proxy hops, only the **first** value is used.
pub fn resolve_base_url<'h>(
    fallback: &str,
    trust: bool,
    header: impl Fn(&str) -> Option<&'h str>,
) -> String {
    let fallback = fallback.trim_end_matches('/');
    if !trust {
        return fallback.to_string();
    }
    if let Some(url) = from_forwarded(header("forwarded"), fallback) {
        return url;
    }
    if let Some(url) = from_x_forwarded(
        header("x-forwarded-proto"),
        header("x-forwarded-host"),
        header("x-forwarded-port"),
        fallback,
    ) {
        return url;
    }
    fallback.to_string()
}

/// Parse an RFC 7239 `Forwarded` header, using the **last** forwarded element.
/// The `host` directive already includes any port, so it is used verbatim.
///
/// RFC 7239 §4 requires each proxy to *append* its entry to a comma-separated
/// list, so the **first** element is the oldest hop — which a client can
/// pre-inject (`Forwarded: host=evil.example.com;proto=https`) before the
/// trusted proxy appends its own entry. The last element is the one written by
/// the closest upstream proxy (the trusted one), so it is the only safe choice.
/// (`X-Forwarded-*` differs: proxies conventionally *overwrite* those, so the
/// trust model — "enable only when a trusted proxy sets/overwrites these" — makes
/// the single value authoritative there.)
fn from_forwarded(value: Option<&str>, fallback: &str) -> Option<String> {
    let last = value?.split(',').next_back().unwrap_or_default().trim();
    let mut host: Option<&str> = None;
    let mut proto: Option<&str> = None;
    for pair in last.split(';') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = unquote(v.trim());
        if host.is_none() && k.eq_ignore_ascii_case("host") {
            host = Some(v);
        } else if proto.is_none() && k.eq_ignore_ascii_case("proto") {
            proto = Some(v);
        }
    }
    let host = sanitize_host(host?)?;
    let scheme = proto
        .and_then(validate_scheme)
        .unwrap_or_else(|| scheme_of(fallback));
    Some(format!("{scheme}://{host}"))
}

/// Build a base URL from the `X-Forwarded-*` family. Returns `None` when there
/// is no usable host so the caller falls through to the next source.
fn from_x_forwarded(
    proto: Option<&str>,
    host: Option<&str>,
    port: Option<&str>,
    fallback: &str,
) -> Option<String> {
    let host = sanitize_host(first_value(host?))?;
    let scheme = proto
        .map(first_value)
        .and_then(validate_scheme)
        .unwrap_or_else(|| scheme_of(fallback));

    // `X-Forwarded-Host` may already carry the port. Otherwise append a valid,
    // non-default `X-Forwarded-Port`.
    let authority = if host_has_port(host) {
        host.to_string()
    } else if let Some(p) = port.map(first_value).and_then(validate_port) {
        if is_default_port(&scheme, p) {
            host.to_string()
        } else {
            format!("{host}:{p}")
        }
    } else {
        host.to_string()
    };
    Some(format!("{scheme}://{authority}"))
}

/// First value of a (possibly) comma-separated header list, trimmed.
fn first_value(s: &str) -> &str {
    s.split(',').next().unwrap_or(s).trim()
}

/// Strip a single pair of surrounding double quotes (RFC 7239 quoted-string).
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// Validate and lower-case a forwarded scheme. Only `http`/`https` are allowed.
fn validate_scheme(s: &str) -> Option<String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("http") {
        Some("http".to_string())
    } else if s.eq_ignore_ascii_case("https") {
        Some("https".to_string())
    } else {
        None
    }
}

/// Scheme of the static fallback URL, used when a forwarded host carries no
/// (valid) proto.
fn scheme_of(url: &str) -> String {
    if url.starts_with("https://") {
        "https".to_string()
    } else {
        "http".to_string()
    }
}

/// Reject host values that contain whitespace, slashes, or any character that
/// could break out of the authority component / inject another URL part
/// (`@`, `?`, `#`, `%`, control bytes, non-ASCII, …). Returns the trimmed host
/// when it is composed solely of safe host/IPv6-literal/port characters.
fn sanitize_host(host: &str) -> Option<&str> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let safe = host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']'));
    if !safe {
        return None;
    }
    // Beyond the byte allowlist, validate the authority *structure* so a
    // malformed header can never yield a broken URL (the module invariant).
    if host.starts_with('[') {
        // IPv6 literal: exactly one bracket pair, non-empty contents, and only
        // an optional `:port` after the closing bracket.
        if host.matches('[').count() != 1 || host.matches(']').count() != 1 {
            return None;
        }
        let close = host.find(']')?;
        if close == 1 {
            return None; // empty `[]`
        }
        match &host[close + 1..] {
            "" => {}
            rest => validate_port(rest.strip_prefix(':')?).map(|_| ())?,
        }
    } else {
        // Non-bracketed: no stray brackets, and a bare IPv6 (more than one
        // colon) MUST be bracketed — reject it rather than emit `https://::1`.
        if host.contains('[') || host.contains(']') || host.matches(':').count() > 1 {
            return None;
        }
        // A bare leading/trailing colon (`:8080`, `example.com:`) is malformed.
        if host.starts_with(':') || host.ends_with(':') {
            return None;
        }
    }
    Some(host)
}

/// Whether a sanitised host already carries an explicit port.
fn host_has_port(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: a port follows the closing bracket as `]:port`.
        matches!(rest.split_once(']'), Some((_, after)) if after.starts_with(':'))
    } else {
        // host:port — a single colon. Multiple colons in a non-bracketed host
        // would be a bare IPv6 address (invalid in a URL authority); treat it
        // as "no appendable port" and leave the (already sanitised) host as-is.
        host.matches(':').count() == 1
    }
}

/// Parse a port, rejecting `0` and out-of-range values.
fn validate_port(p: &str) -> Option<u16> {
    p.trim().parse::<u16>().ok().filter(|&n| n != 0)
}

fn is_default_port(scheme: &str, port: u16) -> bool {
    matches!((scheme, port), ("http", 80) | ("https", 443))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a closure mimicking the axum `HeaderMap` lookup from a static map.
    fn hdrs<'p>(
        pairs: &'p [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<&'static str> + 'p {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
        }
    }

    #[test]
    fn trust_disabled_returns_fallback_even_with_headers() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            false,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "https"),
            ]),
        );
        assert_eq!(url, "http://127.0.0.1:8000");
    }

    #[test]
    fn fallback_trailing_slash_trimmed() {
        let url = resolve_base_url("http://127.0.0.1:8000/", true, hdrs(&[]));
        assert_eq!(url, "http://127.0.0.1:8000");
    }

    #[test]
    fn no_headers_falls_back() {
        let url = resolve_base_url("http://127.0.0.1:8000", true, hdrs(&[]));
        assert_eq!(url, "http://127.0.0.1:8000");
    }

    #[test]
    fn x_forwarded_host_and_proto() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "https"),
            ]),
        );
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn x_forwarded_proto_missing_uses_fallback_scheme() {
        let url = resolve_base_url(
            "https://internal:8000",
            true,
            hdrs(&[("x-forwarded-host", "api.example.com")]),
        );
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn x_forwarded_port_appended_when_non_default() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "8443"),
            ]),
        );
        assert_eq!(url, "https://api.example.com:8443");
    }

    #[test]
    fn x_forwarded_default_port_not_appended() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "443"),
            ]),
        );
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn x_forwarded_host_with_explicit_port_ignores_forwarded_port() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com:9000"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "8443"),
            ]),
        );
        assert_eq!(url, "https://api.example.com:9000");
    }

    #[test]
    fn first_value_of_comma_list_used() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com, evil.example.org"),
                ("x-forwarded-proto", "https, http"),
            ]),
        );
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn forwarded_header_takes_precedence() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                (
                    "forwarded",
                    "for=192.0.2.60;proto=https;host=fwd.example.com",
                ),
                ("x-forwarded-host", "xfwd.example.com"),
            ]),
        );
        assert_eq!(url, "https://fwd.example.com");
    }

    #[test]
    fn forwarded_quoted_host_with_port() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[("forwarded", "host=\"api.example.com:8443\";proto=https")]),
        );
        assert_eq!(url, "https://api.example.com:8443");
    }

    #[test]
    fn forwarded_uses_last_element_not_client_injected_first() {
        // RFC 7239 §4: proxies *append*, so the first element is the oldest hop
        // (a client can pre-inject it); the trusted proxy's entry is last. The
        // client-supplied `evil.example.com` must be ignored in favour of the
        // trusted proxy's `trusted.example.com`.
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[(
                "forwarded",
                "host=evil.example.com;proto=https, host=trusted.example.com;proto=https",
            )]),
        );
        assert_eq!(url, "https://trusted.example.com");
    }

    #[test]
    fn malformed_host_rejected_falls_through() {
        // X-Forwarded-Host with a slash is rejected; nothing else usable -> fallback.
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "evil.com/path"),
                ("x-forwarded-proto", "https"),
            ]),
        );
        assert_eq!(url, "http://127.0.0.1:8000");
    }

    #[test]
    fn host_with_whitespace_or_at_rejected() {
        for bad in [
            "bad host.com",
            "user@evil.com",
            "ho st",
            "exa\tmple.com",
            "naïve.com",
        ] {
            let url = resolve_base_url(
                "http://127.0.0.1:8000",
                true,
                hdrs(&[("x-forwarded-host", bad), ("x-forwarded-proto", "https")]),
            );
            assert_eq!(
                url, "http://127.0.0.1:8000",
                "host {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn invalid_proto_falls_back_to_fallback_scheme() {
        let url = resolve_base_url(
            "https://internal:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "ftp"),
            ]),
        );
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn ipv6_literal_host() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "[2001:db8::1]:8443"),
                ("x-forwarded-proto", "https"),
            ]),
        );
        assert_eq!(url, "https://[2001:db8::1]:8443");
    }

    #[test]
    fn ipv6_literal_host_with_separate_port() {
        // Bracketed IPv6 with no embedded port + a separate X-Forwarded-Port:
        // the port is appended after the closing bracket.
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "[2001:db8::1]"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "8443"),
            ]),
        );
        assert_eq!(url, "https://[2001:db8::1]:8443");
    }

    #[test]
    fn bare_unbracketed_ipv6_rejected() {
        // A literal IPv6 in a URL authority MUST be bracketed; an unbracketed or
        // malformed one must not yield a broken URL — it falls back.
        for bad in ["2001:db8::1", "::1", "[2001:db8::1", "2001:db8::1]", "[]"] {
            let url = resolve_base_url(
                "http://127.0.0.1:8000",
                true,
                hdrs(&[("x-forwarded-host", bad), ("x-forwarded-proto", "https")]),
            );
            assert_eq!(
                url, "http://127.0.0.1:8000",
                "host {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn bare_colon_edges_rejected() {
        for bad in [":8080", "example.com:"] {
            let url = resolve_base_url(
                "http://127.0.0.1:8000",
                true,
                hdrs(&[("x-forwarded-host", bad), ("x-forwarded-proto", "https")]),
            );
            assert_eq!(
                url, "http://127.0.0.1:8000",
                "host {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn http_default_port_not_appended_and_fallback_scheme() {
        // Covers the http/80 default-port arm and scheme_of's http branch
        // (no proto + http fallback).
        let url = resolve_base_url(
            "http://internal:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-port", "80"),
            ]),
        );
        assert_eq!(url, "http://api.example.com");
    }

    #[test]
    fn invalid_port_ignored() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "api.example.com"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "not-a-port"),
            ]),
        );
        assert_eq!(url, "https://api.example.com");
    }

    #[test]
    fn forwarded_without_proto_uses_fallback_scheme() {
        let url = resolve_base_url(
            "https://internal:8000",
            true,
            hdrs(&[("forwarded", "host=api.example.com")]),
        );
        assert_eq!(url, "https://api.example.com");
    }
}
