//! Reverse-proxy-aware base-URL resolution (issues #12, #1403).
//!
//! When the server sits behind a reverse proxy, the address it bound to is not
//! the address clients use, so absolute self-links generated from the static
//! configured base URL are wrong (`http://0.0.0.0:8000/edr/...`). With
//! `[server] trust_proxy_headers = true` each request's self-links are instead
//! derived from the standard proxy forwarding headers, including the path
//! prefix a gateway stripped before proxying (`X-Forwarded-Prefix`).
//!
//! This module is **framework-free**: the HTTP layer passes header values in
//! through a closure, so ds-core never depends on `axum`/`http` (see the
//! "ds-core has no framework dependencies" rule). The closure returns
//! **every** line of a header, since which one counts differs by header
//! (#1403); the axum handlers call [`resolve_base_url`] through
//! `api_common::request_base_url`, which passes `headers.get_all(name)`.
//!
//! # Security
//!
//! Forwarding headers are client-controllable, so resolution is **off by
//! default** and only honoured when the operator opts in (the proxy is trusted
//! to set/overwrite these headers). Even then, host values are sanitised
//! (whitespace, slashes, `@`, non-ASCII and other authority-breaking characters
//! are rejected), the scheme is restricted to `http`/`https`, and a prefix is
//! one value of unreserved-character path segments (see [`sanitize_prefix`]). A
//! malformed or suspicious header never produces a broken or attacker-chosen
//! URL: resolution simply falls through to the next source, ultimately the
//! static fallback, and an invalid prefix is dropped.

/// Longest `X-Forwarded-Prefix` honoured, in bytes, before its trailing `/`
/// is trimmed. A gateway's door prefix is a segment or two; every link of a
/// response repeats it, so a longer value is dropped rather than echoed.
pub const MAX_PREFIX_LEN: usize = 128;

/// Resolve the effective base URL for a single request, without a trailing
/// slash.
///
/// The base is an origin plus an optional path. Precedence for the origin
/// when `trust` is `true`:
///   1. RFC 7239 `Forwarded` — `proto=` / `host=` of the **last** element (the
///      closest trusted proxy; see [`from_forwarded`] for why not the first).
///   2. `X-Forwarded-Proto` + `X-Forwarded-Host` (+ optional `X-Forwarded-Port`).
///   3. `fallback` — the startup-resolved base (`BASE_URL` env > `[server]
///      base_url` > `http://{host}:{port}`).
///
/// The path is a valid `X-Forwarded-Prefix` (#1403): the prefix a gateway
/// stripped before proxying, so `/open/edr/collections` at the gateway that
/// reached this server as `/edr/collections` links back under `/open`. It
/// applies in every branch, as the **whole** path of the base:
///   - after a forwarded origin (1, 2), which carries no path, it is appended;
///   - with the static fallback (3) it **replaces** the fallback's own path,
///     so `base_url = "https://gw.example.org/open"` plus `X-Forwarded-Prefix:
///     /open` is `https://gw.example.org/open`, never `/open/open`. The header
///     is the per-request statement of the public path, as a forwarded host is
///     of the public host; Spring and ASP.NET Core treat it the same way.
///     Replaced, not nested: with `base_url = "https://host/meteocore"`
///     behind an outer proxy that strips `/meteocore` and forwards no host,
///     an inner door sending `/open` yields `https://host/open`, not
///     `https://host/meteocore/open`. Such a chain must forward the full
///     public prefix (or a host, so the fallback's path is not involved).
///
/// Without a valid prefix the base keeps the path it had: none after a
/// forwarded origin (as before #1403), the fallback's own otherwise. An
/// invalid prefix is ignored, never an error; see [`sanitize_prefix`] for the
/// rules.
///
/// When `trust` is `false` (the default) `fallback` is returned unchanged and
/// every forwarding header, `X-Forwarded-Prefix` included, is ignored, so
/// there is no behaviour change without the flag.
///
/// `header` returns every line of a header by its lowercase name, in the
/// order received (empty when absent; a line that is not text as `""`).
/// Which value counts differs by header:
///   - `Forwarded`: the **last** element across all lines, the closest
///     proxy's, since proxies append (RFC 7239 §4) and may append a line of
///     their own after the client's;
///   - `X-Forwarded-Proto/Host/Port`: the **first** value of the first line,
///     on the trust that the proxy overwrites them;
///   - `X-Forwarded-Prefix`: the one value of exactly one line, or none (see
///     [`sanitize_prefix`]).
pub fn resolve_base_url<'h>(
    fallback: &str,
    trust: bool,
    header: impl Fn(&str) -> Vec<&'h str>,
) -> String {
    let fallback = fallback.trim_end_matches('/');
    if !trust {
        return fallback.to_string();
    }
    let first = |name: &str| header(name).first().copied();
    let prefix = sanitize_prefix(&header("x-forwarded-prefix"));
    let origin = from_forwarded(header("forwarded").last().copied(), fallback).or_else(|| {
        from_x_forwarded(
            first("x-forwarded-proto"),
            first("x-forwarded-host"),
            first("x-forwarded-port"),
            fallback,
        )
    });
    match (origin, prefix) {
        (Some(origin), Some(prefix)) => format!("{origin}{prefix}"),
        (Some(origin), None) => origin,
        (None, Some(prefix)) => format!("{}{prefix}", origin_of(fallback)),
        (None, None) => fallback.to_string(),
    }
}

/// Validate the `X-Forwarded-Prefix` lines of a request, returning the prefix
/// to append (leading `/`, no trailing `/`) or `None` when there is none to
/// apply.
///
/// - Exactly one line with exactly one value, trimmed. Two lines or a comma
///   list are ignored: proxies that strip a prefix differ in whether they
///   overwrite the header or add to it (Traefik's StripPrefix and Spring
///   Cloud Gateway add), so the first value can be the client's own and the
///   last is the client's whenever a proxy passes it through, and nested
///   strips have no agreed order to join in.
/// - At most [`MAX_PREFIX_LEN`] bytes.
/// - It starts with `/`; one trailing `/` is trimmed, and `/` alone is no
///   prefix.
/// - Every segment is non-empty, not `.` or `..`, and RFC 3986 unreserved
///   characters only (`A-Z a-z 0-9 - . _ ~`): no `%`-escapes, no query or
///   fragment, nothing that needs escaping in a URL, an HTML attribute or XML.
///
/// Anything else is `None`: the header is ignored, never an error.
fn sanitize_prefix<'h>(lines: &[&'h str]) -> Option<&'h str> {
    let [line] = lines else {
        return None;
    };
    let prefix = line.trim();
    if prefix.contains(',') || prefix.len() > MAX_PREFIX_LEN || !prefix.starts_with('/') {
        return None;
    }
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    if prefix.is_empty() {
        return None; // `/` alone
    }
    let valid = prefix[1..].split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    });
    valid.then_some(prefix)
}

/// The path of a base URL [`resolve_base_url`] returned: empty, or a leading
/// `/` and no trailing one (`/open` of `https://gw.example.org/open`). A page
/// served from this origin uses it for root-relative URLs that keep a
/// stripped prefix without naming the host.
pub fn path_of(base_url: &str) -> &str {
    &base_url[origin_of(base_url).len()..]
}

/// The `scheme://authority` part of a base URL, dropping any path: where a
/// forwarded prefix replaces the static fallback's path.
fn origin_of(url: &str) -> &str {
    let authority = url.find("://").map_or(0, |i| i + 3);
    match url[authority..].find('/') {
        Some(path) => &url[..authority + path],
        None => url,
    }
}

/// Parse an RFC 7239 `Forwarded` header's last line, using its **last**
/// forwarded element: the last element across all lines, as a header split
/// over lines is one comma list (RFC 9110 §5.3). The `host` directive already
/// includes any port, so it is used verbatim.
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
    /// A repeated name is a header sent on several lines, in order.
    fn hdrs<'p>(
        pairs: &'p [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Vec<&'static str> + 'p {
        move |name: &str| {
            pairs
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
                .collect()
        }
    }

    /// [`sanitize_prefix`] of a header sent on one line.
    fn one(value: &str) -> Option<&str> {
        sanitize_prefix(&[value])
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

    // ── X-Forwarded-Prefix (#1403) ─────────────────────────────────────

    #[test]
    fn prefix_appended_after_x_forwarded_origin() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "gw.example.org"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-prefix", "/open"),
            ]),
        );
        assert_eq!(url, "https://gw.example.org/open");
    }

    #[test]
    fn prefix_appended_after_x_forwarded_origin_with_port() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "gw.example.org"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "8443"),
                ("x-forwarded-prefix", "/local"),
            ]),
        );
        assert_eq!(url, "https://gw.example.org:8443/local");
    }

    #[test]
    fn prefix_appended_after_forwarded_origin() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                (
                    "forwarded",
                    "for=192.0.2.60;proto=https;host=gw.example.org",
                ),
                ("x-forwarded-prefix", "/open"),
            ]),
        );
        assert_eq!(url, "https://gw.example.org/open");
    }

    #[test]
    fn prefix_applies_to_fallback_origin() {
        // No forwarded host: the prefix still applies, after the fallback's
        // origin.
        let url = resolve_base_url(
            "https://gw.example.org",
            true,
            hdrs(&[("x-forwarded-prefix", "/open")]),
        );
        assert_eq!(url, "https://gw.example.org/open");
        // Also when the forwarded host is rejected and the origin falls back.
        let url = resolve_base_url(
            "http://127.0.0.1:8000/",
            true,
            hdrs(&[
                ("x-forwarded-host", "evil.com/path"),
                ("x-forwarded-prefix", "/open"),
            ]),
        );
        assert_eq!(url, "http://127.0.0.1:8000/open");
    }

    #[test]
    fn prefix_replaces_the_fallback_path_never_doubles_it() {
        // The prefix is the whole public path: a fallback that already names
        // it is not doubled, and a different door's prefix wins.
        let fallback = "https://gw.example.org/open";
        for (prefix, expected) in [
            ("/open", "https://gw.example.org/open"),
            ("/local", "https://gw.example.org/local"),
            ("/a/b", "https://gw.example.org/a/b"),
        ] {
            let pairs = [("x-forwarded-prefix", prefix)];
            let url = resolve_base_url(fallback, true, hdrs(&pairs));
            assert_eq!(url, expected, "prefix {prefix:?}");
        }
    }

    #[test]
    fn fallback_path_kept_without_a_valid_prefix() {
        let fallback = "https://gw.example.org/open/";
        assert_eq!(
            resolve_base_url(fallback, true, hdrs(&[])),
            "https://gw.example.org/open"
        );
        for ignored in ["/../evil", "/", "open"] {
            let pairs = [("x-forwarded-prefix", ignored)];
            assert_eq!(
                resolve_base_url(fallback, true, hdrs(&pairs)),
                "https://gw.example.org/open",
                "prefix {ignored:?}"
            );
        }
    }

    #[test]
    fn forwarded_origin_without_prefix_has_no_path() {
        // As before #1403: a forwarded origin does not inherit the fallback's
        // path.
        let url = resolve_base_url(
            "https://internal:8000/mc",
            true,
            hdrs(&[("x-forwarded-host", "gw.example.org")]),
        );
        assert_eq!(url, "https://gw.example.org");
    }

    #[test]
    fn prefix_ignored_when_trust_disabled() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            false,
            hdrs(&[
                ("x-forwarded-host", "gw.example.org"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-prefix", "/open"),
            ]),
        );
        assert_eq!(url, "http://127.0.0.1:8000");
        let url = resolve_base_url(
            "https://gw.example.org/static",
            false,
            hdrs(&[("x-forwarded-prefix", "/open")]),
        );
        assert_eq!(url, "https://gw.example.org/static");
    }

    #[test]
    fn invalid_prefix_ignored_keeps_the_forwarded_origin() {
        for bad in [
            "/../evil",
            "/a b",
            "//evil.example.com",
            "https://evil.example.com",
        ] {
            let pairs = [
                ("x-forwarded-host", "gw.example.org"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-prefix", bad),
            ];
            assert_eq!(
                resolve_base_url("http://127.0.0.1:8000", true, hdrs(&pairs)),
                "https://gw.example.org",
                "prefix {bad:?} should be ignored"
            );
        }
    }

    /// A proxy that adds its prefix to a client's (Traefik's StripPrefix,
    /// Spring Cloud Gateway) makes a list whose first value is the client's;
    /// one that passes the client's through leaves its own value anywhere.
    /// Neither end is safe, so a list is no prefix at all.
    #[test]
    fn prefix_comma_list_is_ignored() {
        for list in [
            "/local, /open",
            " /open , /local",
            "/open,",
            ",/open",
            "/../evil, /open",
        ] {
            let url = resolve_base_url(
                "http://127.0.0.1:8000",
                true,
                hdrs(&[
                    ("x-forwarded-host", "gw.example.org"),
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-prefix", list),
                ]),
            );
            assert_eq!(url, "https://gw.example.org", "{list:?}");
        }
    }

    /// The same, sent as two lines: a client's line and the proxy's.
    #[test]
    fn prefix_on_two_lines_is_ignored() {
        for lines in [["/local", "/open"], ["/open", "/open"], ["/open", ""]] {
            let url = resolve_base_url(
                "https://gw.example.org/static",
                true,
                hdrs(&[
                    ("x-forwarded-host", "gw.example.org"),
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-prefix", lines[0]),
                    ("x-forwarded-prefix", lines[1]),
                ]),
            );
            assert_eq!(url, "https://gw.example.org", "{lines:?}");
        }
        // With no forwarded origin the fallback keeps its own path.
        let url = resolve_base_url(
            "https://gw.example.org/static",
            true,
            hdrs(&[("x-forwarded-prefix", "/a"), ("x-forwarded-prefix", "/b")]),
        );
        assert_eq!(url, "https://gw.example.org/static");
    }

    #[test]
    fn single_prefix_value_is_trimmed_and_used() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "gw.example.org"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-prefix", "  /open/ "),
            ]),
        );
        assert_eq!(url, "https://gw.example.org/open");
        assert_eq!(sanitize_prefix(&[]), None);
    }

    /// A proxy may append its `Forwarded` entry as a line of its own after
    /// the client's: the last element across all lines is the proxy's.
    #[test]
    fn forwarded_uses_the_last_element_across_lines() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                (
                    "forwarded",
                    "host=evil.example.com;proto=http, host=evil2.example.com",
                ),
                ("forwarded", "host=gw.example.org;proto=https"),
            ]),
        );
        assert_eq!(url, "https://gw.example.org");
        // An empty last line has no host: on to `X-Forwarded-*`, never back
        // to the client's line.
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("forwarded", "host=evil.example.com;proto=https"),
                ("forwarded", ""),
                ("x-forwarded-host", "gw.example.org"),
            ]),
        );
        assert_eq!(url, "http://gw.example.org");
    }

    /// `X-Forwarded-Host/Proto/Port` keep the first value of the first line:
    /// the proxy is trusted to overwrite them.
    #[test]
    fn x_forwarded_uses_the_first_line() {
        let url = resolve_base_url(
            "http://127.0.0.1:8000",
            true,
            hdrs(&[
                ("x-forwarded-host", "gw.example.org"),
                ("x-forwarded-host", "other.example.org"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-proto", "http"),
            ]),
        );
        assert_eq!(url, "https://gw.example.org");
    }

    #[test]
    fn prefix_must_start_with_slash() {
        for bad in ["open", "open/", "", " ", "./open", "~/open"] {
            assert_eq!(one(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn prefix_trailing_slash_trimmed_and_root_is_none() {
        assert_eq!(one("/open/"), Some("/open"));
        assert_eq!(one("/a/b/"), Some("/a/b"));
        assert_eq!(one("/"), None);
        // Only one trailing slash is trimmed; the rest is an empty segment.
        assert_eq!(one("/open//"), None);
        assert_eq!(one("//"), None);
    }

    #[test]
    fn prefix_segments_are_unreserved_characters() {
        assert_eq!(one("/Az09-._~/x.y_z~1"), Some("/Az09-._~/x.y_z~1"));
        assert_eq!(one("/a..b/.c/d."), Some("/a..b/.c/d."));
        for bad in [
            "/a b", "/a%2Fb", "/%2e%2e", "/a?b", "/a#b", "/a;b", "/a:b", "/a@b", "/a+b", "/a=b",
            "/a\\b", "/a\"b", "/a<b>", "/a'b", "/a&b", "/naïve", "/a\tb",
        ] {
            assert_eq!(one(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn prefix_rejects_empty_dot_and_dot_dot_segments() {
        for bad in [
            "//evil.example.com",
            "/a//b",
            "/.",
            "/./a",
            "/a/.",
            "/..",
            "/../evil",
            "/a/../b",
            "/a/..",
            "/../",
        ] {
            assert_eq!(one(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn prefix_length_cap() {
        let at_cap = format!("/{}", "a".repeat(MAX_PREFIX_LEN - 1));
        assert_eq!(one(&at_cap), Some(at_cap.as_str()));
        let over = format!("/{}", "a".repeat(MAX_PREFIX_LEN));
        assert_eq!(one(&over), None);
        // The cap counts the value as sent, before its trailing slash goes.
        assert_eq!(one(&format!("{at_cap}/")), None);
    }

    #[test]
    fn path_of_keeps_only_the_path() {
        assert_eq!(path_of("https://gw.example.org/open"), "/open");
        assert_eq!(path_of("https://gw.example.org/a/b"), "/a/b");
        assert_eq!(path_of("http://127.0.0.1:8000"), "");
        assert_eq!(path_of(""), "");
    }

    #[test]
    fn origin_of_drops_the_path() {
        assert_eq!(
            origin_of("https://gw.example.org/a/b"),
            "https://gw.example.org"
        );
        assert_eq!(origin_of("http://127.0.0.1:8000"), "http://127.0.0.1:8000");
        assert_eq!(
            origin_of("https://[2001:db8::1]:8443/x"),
            "https://[2001:db8::1]:8443"
        );
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
