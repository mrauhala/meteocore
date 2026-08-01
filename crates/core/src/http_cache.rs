//! HTTP response-caching primitives shared by the API crates (#499).
//!
//! ds-core is framework-free, so this module holds only the pure pieces —
//! ETag computation, `If-None-Match` matching, and the `Cache-Control`
//! policy strings. The thin axum glue (a response middleware that buffers
//! the body, stamps these headers, and answers conditional requests with
//! 304) lives per API crate in `src/caching.rs` (api-edr, api-features);
//! api-3dtiles calls these helpers directly from its content path.

use chrono::{DateTime, Duration, Utc};

/// `Cache-Control` for "latest" / open-ended data queries and metadata
/// endpoints: they change on every data arrival, so a short window that
/// coalesces refresh loops (matching the WMS and 3D Tiles "latest" policy).
pub const CACHE_CONTROL_SHORT: &str = "public, max-age=60";

/// `Cache-Control` for a *settled* data query (see [`data_cache_control`]):
/// the answer is about the past and can be held for a day.
///
/// Deliberately NOT `immutable`: unlike a 3D Tiles volume pinned to an
/// exactly-advertised timestep, the API layer cannot know whether the
/// engine's window can still change — late-arriving observations back-fill,
/// and rolling retention prunes. After expiry the client revalidates with
/// `If-None-Match`, so a stale window self-heals within a day.
pub const CACHE_CONTROL_SETTLED: &str = "public, max-age=86400";

/// How far in the past a query window's end must be before the response is
/// considered settled. Sized for late-arriving observations: real-time obs
/// typically land within minutes; an hour covers the common stragglers.
/// (Later QC revisions are accepted staleness — capped by the one-day
/// `max-age` in [`CACHE_CONTROL_SETTLED`].)
fn settled_horizon() -> Duration {
    Duration::hours(1)
}

/// Select the `Cache-Control` policy for a data-query response from its
/// `datetime` selection. `start`/`end` are the requested interval bounds with
/// open (`..`) bounds mapped to `None` by the caller; an instant is
/// `start == end`.
///
/// *Settled* — a **closed** interval whose end is at least
/// [`settled_horizon`] before `now` — gets the long policy: the response
/// describes the past and byte-identical refetches dominate. Everything else
/// (no datetime, open bounds, windows touching now or the future) gets the
/// short policy, because "latest" answers change as data arrives. An open
/// *start* is not settled even with a past end: the response then includes
/// the oldest retained data, which rolling retention keeps changing.
pub fn data_cache_control(
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> &'static str {
    let settled =
        start.is_some() && end.is_some_and(|e| now.signed_duration_since(e) >= settled_horizon());
    if settled {
        CACHE_CONTROL_SETTLED
    } else {
        CACHE_CONTROL_SHORT
    }
}

/// Strong content-derived ETag — quoted hex with no `W/` prefix (the bytes
/// are exact, so byte-equal responses are equivalent per RFC 7232 §2.1).
/// FNV-1a 64-bit — stable across Rust versions and instances (unlike
/// `DefaultHasher`), so a toolchain upgrade or a mixed-version fleet doesn't
/// silently invalidate ETags.
pub fn etag_of(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

/// Whether an `If-None-Match` header value matches `etag` (as produced by
/// [`etag_of`]). RFC 7232 §3.2: the value may be `*` or a comma-separated
/// list of entity tags.
pub fn if_none_match_matches(header_value: &str, etag: &str) -> bool {
    header_value == "*" || header_value.split(',').any(|t| t.trim() == etag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn etag_is_stable_and_quoted() {
        let a = etag_of(b"hello");
        assert_eq!(a, etag_of(b"hello"));
        assert!(a.starts_with('"') && a.ends_with('"'));
        // Pinned value: FNV-1a 64 of "hello". A change here means every
        // deployed client's cache silently invalidates — deliberate only.
        assert_eq!(a, "\"a430d84680aabd0b\"");
        assert_ne!(a, etag_of(b"hello2"));
    }

    #[test]
    fn if_none_match_forms() {
        let e = etag_of(b"x");
        assert!(if_none_match_matches(&e, &e));
        assert!(if_none_match_matches("*", &e));
        assert!(if_none_match_matches(&format!("\"other\", {e}"), &e));
        assert!(!if_none_match_matches("\"other\"", &e));
        // The quotes are part of the tag: a bare hex token must not match.
        assert!(!if_none_match_matches(e.trim_matches('"'), &e));
    }

    #[test]
    fn closed_past_interval_is_settled() {
        let now = t("2026-08-01T12:00:00Z");
        assert_eq!(
            data_cache_control(
                Some(t("2026-08-01T06:00:00Z")),
                Some(t("2026-08-01T08:00:00Z")),
                now
            ),
            CACHE_CONTROL_SETTLED
        );
        // An instant (start == end) counts too.
        assert_eq!(
            data_cache_control(
                Some(t("2026-08-01T08:00:00Z")),
                Some(t("2026-08-01T08:00:00Z")),
                now
            ),
            CACHE_CONTROL_SETTLED
        );
    }

    #[test]
    fn recent_open_or_absent_windows_are_short() {
        let now = t("2026-08-01T12:00:00Z");
        // No datetime at all ("latest").
        assert_eq!(data_cache_control(None, None, now), CACHE_CONTROL_SHORT);
        // End inside the settled horizon.
        assert_eq!(
            data_cache_control(
                Some(t("2026-08-01T10:00:00Z")),
                Some(t("2026-08-01T11:30:00Z")),
                now
            ),
            CACHE_CONTROL_SHORT
        );
        // Exactly on the horizon boundary is settled (>=).
        assert_eq!(
            data_cache_control(
                Some(t("2026-08-01T10:00:00Z")),
                Some(t("2026-08-01T11:00:00Z")),
                now
            ),
            CACHE_CONTROL_SETTLED
        );
        // Open start (`../end`): retention keeps changing the answer.
        assert_eq!(
            data_cache_control(None, Some(t("2026-08-01T08:00:00Z")), now),
            CACHE_CONTROL_SHORT
        );
        // Open end (`start/..`) and future windows.
        assert_eq!(
            data_cache_control(Some(t("2026-08-01T06:00:00Z")), None, now),
            CACHE_CONTROL_SHORT
        );
        assert_eq!(
            data_cache_control(
                Some(t("2026-08-02T00:00:00Z")),
                Some(t("2026-08-02T06:00:00Z")),
                now
            ),
            CACHE_CONTROL_SHORT
        );
    }
}
