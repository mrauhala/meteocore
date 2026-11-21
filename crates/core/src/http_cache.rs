//! HTTP response-caching primitives shared by the API crates (#499).
//!
//! ds-core is framework-free, so this module holds only the pure pieces —
//! ETag computation, `If-None-Match` matching, and the `Cache-Control`
//! policy strings. The thin axum glue (a response middleware that buffers
//! the body, stamps these headers, and answers conditional requests with
//! 304) is `api_common::caching`, shared by EDR, Features and the shared
//! OGC API root; the other APIs call these helpers from their own paths.
//!
//! The one comparison of `If-None-Match` is [`if_none_match_matches`], and
//! the one ETag of a body the server's compression layer may encode is
//! [`weak_etag_of`] (#1500).

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
/// `max-age` in [`CACHE_CONTROL_SETTLED`].) Also how far before its newest
/// strike a PostGIS lightning map frame must end before it is settled, and
/// may be `immutable` (#1421).
pub fn settled_horizon() -> Duration {
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

/// Strong content-derived ETag — quoted hex with no `W/` prefix: a
/// validator of these exact bytes (RFC 9110 §8.8.1). FNV-1a 64-bit — stable
/// across Rust versions and instances (unlike `DefaultHasher`), so a
/// toolchain upgrade or a mixed-version fleet doesn't silently invalidate
/// ETags.
///
/// Only for a representation nothing re-encodes on its way out. The
/// server's `CompressionLayer` sits outside every router and gzips any
/// body but an image, so a handler's tag names the gzipped and the plain
/// bytes alike: those take [`weak_etag_of`].
pub fn etag_of(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

/// Weak content-derived ETag, `W/"<16 hex>"`: [`etag_of`]'s hash of the
/// bytes, marked weak (RFC 9110 §8.8.1) because it names every content
/// coding of them. The server's `CompressionLayer` gzips the body after the
/// handler or middleware tagged it, so the plain and the gzipped responses
/// carry this one tag: a strong tag would claim two different byte
/// sequences are one (#1500). The hash is the strong tag's, so a client
/// holding the strong tag of the same bytes still revalidates
/// ([`if_none_match_matches`] compares weakly).
pub fn weak_etag_of(bytes: &[u8]) -> String {
    format!("W/{}", etag_of(bytes))
}

/// Whether an `If-None-Match` header value matches `etag`, by the weak
/// comparison RFC 9110 §13.1.2 requires for it: `W/"x"` and `"x"` match
/// each other and themselves. The value may be `*` or a comma-separated
/// list of entity tags (§13.1.2); a `*` member of a list is taken as `*`.
/// The quotes are part of a tag, so a bare token never matches, and no tag
/// matches an `etag` that is not an entity tag.
pub fn if_none_match_matches(header_value: &str, etag: &str) -> bool {
    let ours = opaque_tag(etag);
    header_value
        .split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || (ours.is_some() && opaque_tag(tag) == ours))
}

/// The quoted opaque part of an entity tag, `W/` dropped: what the weak
/// comparison compares. `None` for anything but a quoted tag.
fn opaque_tag(tag: &str) -> Option<&str> {
    let tag = tag.trim();
    let tag = tag.strip_prefix("W/").unwrap_or(tag);
    (tag.len() >= 2 && tag.starts_with('"') && tag.ends_with('"')).then_some(tag)
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
        assert!(if_none_match_matches(" * ", &e));
        assert!(if_none_match_matches(&format!("\"other\", {e}"), &e));
        assert!(if_none_match_matches(&format!("\"a\",{e},\"z\""), &e));
        assert!(!if_none_match_matches("\"other\"", &e));
        assert!(!if_none_match_matches("\"aaa\", \"bbb\"", "\"ccc\""));
        // The quotes are part of the tag: a bare hex token must not match.
        assert!(!if_none_match_matches(e.trim_matches('"'), &e));
        assert!(!if_none_match_matches("", &e));
        // No tag matches what is not an entity tag; `*` matches anything.
        assert!(!if_none_match_matches("abc", "abc"));
        assert!(if_none_match_matches("*", "abc"));
    }

    /// RFC 9110 §13.1.2: `If-None-Match` compares weakly, so a tag an
    /// intermediary weakened, or the strong tag of the same bytes a client
    /// held from before #1500, still revalidates — both ways round.
    #[test]
    fn if_none_match_compares_weakly_both_ways() {
        let strong = etag_of(b"x");
        let weak = weak_etag_of(b"x");
        assert_eq!(weak, format!("W/{strong}"));
        assert!(if_none_match_matches(&weak, &strong));
        assert!(if_none_match_matches(&strong, &weak));
        assert!(if_none_match_matches(&weak, &weak));
        assert!(if_none_match_matches(&format!("\"aaa\", {weak}"), &strong));
        assert!(if_none_match_matches(&format!("\"aaa\", {strong}"), &weak));
        assert!(!if_none_match_matches(&weak_etag_of(b"y"), &strong));
        assert!(!if_none_match_matches(&weak_etag_of(b"y"), &weak));
        // `W/` without a quoted tag is no tag.
        assert!(!if_none_match_matches("W/", &weak));
        assert!(!if_none_match_matches(strong.trim_matches('"'), &weak));
    }

    #[test]
    fn weak_etag_is_the_strong_hash_marked_weak() {
        // Pinned: FNV-1a 64 of "hello", as `etag_is_stable_and_quoted`.
        assert_eq!(weak_etag_of(b"hello"), "W/\"a430d84680aabd0b\"");
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
