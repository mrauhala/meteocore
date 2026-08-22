//! Bearer auth and rate limiting for `/mcp`.
//!
//! This is the first authenticated public route in MeteoCore, so the rules are
//! written down rather than inferred:
//!
//! - **The token is required.** There is no "unset means open" mode. A config
//!   with `[mcp] enabled = true` and no resolvable token fails at load, so the
//!   endpoint cannot be published unauthenticated by omission — the failure
//!   mode a default-empty token would create.
//! - **The comparison is constant-time.** `/admin` uses `==` on its token
//!   (a known wart); a byte-by-byte compare leaks position-of-first-mismatch
//!   through timing, which is a practical attack against a token an MCP
//!   client will retry cheaply.
//! - **Rate limiting is global, not per-client.** One shared token means
//!   there is no client identity to key on, so the limit protects the server
//!   rather than fairly apportioning between callers. Said plainly here so
//!   nobody mistakes it for the latter.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Fixed-window rate limiter over the whole endpoint.
pub struct RateLimiter {
    per_minute: u64,
    window_start: std::sync::Mutex<Instant>,
    count: AtomicU64,
}

impl RateLimiter {
    pub fn new(per_minute: u64) -> Self {
        Self {
            per_minute,
            window_start: std::sync::Mutex::new(Instant::now()),
            count: AtomicU64::new(0),
        }
    }

    /// Whether this request fits in the current window.
    ///
    /// Fixed window, not sliding: a burst straddling a boundary can reach 2×
    /// the limit briefly. That is a deliberate simplification — the limit here
    /// is a blast-radius cap on an authenticated endpoint, not a fairness
    /// mechanism, and a sliding window would need per-request bookkeeping for
    /// no benefit at this scale.
    pub fn allow(&self) -> bool {
        if self.per_minute == 0 {
            return true;
        }
        {
            let mut start = self.window_start.lock().unwrap_or_else(|e| e.into_inner());
            if start.elapsed() >= Duration::from_secs(60) {
                *start = Instant::now();
                self.count.store(0, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed) < self.per_minute
    }
}

pub struct McpAuth {
    token: String,
    /// Separate budget for FAILED auth attempts.
    ///
    /// Two buckets rather than one check before auth: a single shared bucket
    /// consulted first would let unauthenticated traffic exhaust it and lock
    /// out the legitimate client — trading a brute-force hole for a denial of
    /// service. Separate budgets throttle guessing without letting a stranger
    /// spend the real caller's allowance.
    failure_limiter: RateLimiter,
    /// Flipped by reload. The route itself is nested at boot, so without this
    /// a config change disabling MCP would leave `/mcp` live and reachable
    /// with the original token until the process restarted — an operator
    /// turning it off would reasonably expect it to be off.
    enabled: std::sync::atomic::AtomicBool,
    pub limiter: RateLimiter,
}

impl McpAuth {
    pub fn new(token: String, rate_limit_per_min: u64) -> Self {
        Self {
            // Trimmed to match the presented token, which is also trimmed.
            // An env var set from a file or a here-doc commonly carries a
            // trailing newline; without this the two never compare equal and
            // every request 401s with no indication why.
            token: token.trim().to_string(),
            enabled: std::sync::atomic::AtomicBool::new(true),
            failure_limiter: RateLimiter::new(rate_limit_per_min),
            limiter: RateLimiter::new(rate_limit_per_min),
        }
    }

    /// Serve requests at all. Set from config on every reload.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Constant-time token comparison.
    ///
    /// Length is compared first and leaks, which is acceptable — token length
    /// is not the secret. The contents are compared with a fixed-cost fold so
    /// the time taken does not depend on how many leading bytes matched.
    fn token_matches(&self, presented: &str) -> bool {
        // Defence in depth. ServerConfig::validate() already refuses to boot
        // an enabled endpoint without a token, but the boot path re-reads the
        // env var independently, and `unwrap_or_default()` on a var that
        // vanished in between would leave an empty token here — which an
        // empty `Bearer ` would then match.
        if self.token.is_empty() {
            return false;
        }
        let expected = self.token.as_bytes();
        let got = presented.as_bytes();
        if expected.len() != got.len() {
            return false;
        }
        expected
            .iter()
            .zip(got)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        // No detail about which part failed: a distinguishable "bad token"
        // vs "missing token" is a small oracle, and a client that can't tell
        // still knows to check its credential.
        Json(json!({ "error": msg })),
    )
        .into_response()
}

fn too_many_requests() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "error": "Rate limit exceeded; retry in under a minute" })),
    )
        .into_response()
}

/// Bearer check plus rate limit, applied to the whole `/mcp` subtree.
pub async fn guard(
    State(auth): State<Arc<McpAuth>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Checked before auth: a disabled endpoint should look absent, not
    // "wrong credential".
    if !auth.is_enabled() {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "Not found" }))).into_response();
    }

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);

    let authenticated = presented.is_some_and(|p| auth.token_matches(p));
    if !authenticated {
        // Throttle guessing on its own budget. Without this the limiter only
        // ever saw requests that had ALREADY authenticated, leaving the one
        // kind of traffic it most needs to bound completely unmetered.
        if !auth.failure_limiter.allow() {
            return too_many_requests();
        }
        return unauthorized("Authorization: Bearer <token> required");
    }
    if !auth.limiter.allow() {
        return too_many_requests();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_accepts_only_the_exact_token() {
        let auth = McpAuth::new("s3cret-token".into(), 0);
        assert!(auth.token_matches("s3cret-token"));
        assert!(!auth.token_matches("s3cret-toke"), "prefix must not pass");
        assert!(
            !auth.token_matches("s3cret-tokenn"),
            "extension must not pass"
        );
        assert!(!auth.token_matches("S3CRET-TOKEN"), "case matters");
        assert!(!auth.token_matches(""));
    }

    #[test]
    fn an_empty_configured_token_authenticates_nothing() {
        let auth = McpAuth::new(String::new(), 0);
        assert!(!auth.token_matches(""));
        assert!(!auth.token_matches(" "));
        assert!(!auth.token_matches("anything"));
    }

    #[test]
    fn a_token_with_surrounding_whitespace_still_matches() {
        // `MCP_TOKEN=$(cat secret)` and docker secrets both leave a newline.
        let auth = McpAuth::new("s3cret-token\n".into(), 0);
        assert!(auth.token_matches("s3cret-token"));
    }

    #[test]
    fn disabling_takes_effect_without_a_restart() {
        let auth = McpAuth::new("t".into(), 0);
        assert!(auth.is_enabled());
        auth.set_enabled(false);
        assert!(!auth.is_enabled());
    }

    #[test]
    fn rate_limiter_admits_up_to_the_limit_then_refuses() {
        let limiter = RateLimiter::new(3);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow(), "the 4th request in the window is refused");
    }

    #[test]
    fn failed_auth_has_its_own_budget() {
        // Guessing must be throttled, but a stranger must not be able to
        // exhaust the legitimate caller's allowance by failing on purpose.
        let auth = McpAuth::new("right".into(), 2);
        assert!(auth.failure_limiter.allow());
        assert!(auth.failure_limiter.allow());
        assert!(!auth.failure_limiter.allow(), "guessing is throttled");
        // The authenticated budget is untouched by those failures.
        assert!(auth.limiter.allow());
        assert!(auth.limiter.allow());
    }

    #[test]
    fn zero_means_unlimited() {
        // An omitted `rate_limit_per_min` defaults to 60 in config; validate()
        // never inspects it. 0 is an explicit opt-out for a deployment where
        // the port is not publicly reachable.
        let limiter = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(limiter.allow());
        }
    }
}
