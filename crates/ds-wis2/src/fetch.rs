//! Payload retrieval: inline content first, HTTPS download otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use tokio::sync::Semaphore;
use url::Url;

use crate::payload::{decode_inline, verify_integrity, Payload, PayloadSource, Verification};
use crate::status::{DropReason, Status};
use crate::{DownloadPolicy, Notification, Wis2Error};

/// Per-request timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Concurrent downloads per fetcher (a firehose subscription with link-only
/// notifications must not open hundreds of sockets).
pub const CONCURRENCY: usize = 8;
/// How long a downloaded body is remembered by URL. MeteoAlarm publishes one
/// notification per alert × info × area, all pointing at the same CAP XML;
/// the cache turns those into one download.
const URL_CACHE_TTL: Duration = Duration::from_secs(60);
const URL_CACHE_MAX: usize = 256;

pub struct Fetcher {
    client: reqwest::Client,
    policy: DownloadPolicy,
    max_bytes: u64,
    limiter: Arc<Semaphore>,
    cache: Mutex<HashMap<String, (Instant, Bytes)>>,
    status: Arc<Status>,
}

impl Fetcher {
    pub fn new(
        policy: DownloadPolicy,
        max_bytes: u64,
        status: Arc<Status>,
    ) -> Result<Self, Wis2Error> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("MeteoCore/", env!("CARGO_PKG_VERSION"), " (wis2)"))
            .build()
            .map_err(|e| Wis2Error::Config(format!("http client: {e}")))?;
        Ok(Fetcher {
            client,
            policy,
            max_bytes,
            limiter: Arc::new(Semaphore::new(CONCURRENCY)),
            cache: Mutex::new(HashMap::new()),
            status,
        })
    }

    pub fn policy(&self) -> &DownloadPolicy {
        &self.policy
    }

    /// Produce the data object a notification describes: decode inline
    /// content when present, otherwise download the canonical link. Applies
    /// the size cap and, when the notification carries an `integrity` block,
    /// verifies the checksum. Records drop reasons on `Status` on failure.
    pub async fn resolve(&self, n: &Notification) -> Result<Payload, Wis2Error> {
        let result = self.resolve_inner(n).await;
        if let Err(e) = &result {
            self.status.record_dropped(match e {
                Wis2Error::Policy(_) => DropReason::Policy,
                Wis2Error::Integrity(_) => DropReason::Integrity,
                Wis2Error::TooLarge(_) => DropReason::Size,
                Wis2Error::Decode(_) => DropReason::Decode,
                _ => DropReason::Download,
            });
        }
        result
    }

    async fn resolve_inner(&self, n: &Notification) -> Result<Payload, Wis2Error> {
        let (bytes, source, media_type) = if let Some(content) = &n.content {
            (
                decode_inline(content, self.max_bytes)?,
                PayloadSource::Inline,
                None,
            )
        } else {
            let link = n.canonical_link().ok_or_else(|| {
                Wis2Error::Download("no inline content and no canonical/update link".into())
            })?;
            if let Some(len) = link.length {
                if len > self.max_bytes {
                    return Err(Wis2Error::TooLarge(format!(
                        "link declares {len} bytes (cap {})",
                        self.max_bytes
                    )));
                }
            }
            let bytes = self.download(&link.href).await?;
            (
                bytes,
                PayloadSource::Downloaded(link.href.clone()),
                link.media_type.clone(),
            )
        };
        let verified = match &n.integrity {
            Some(i) => match verify_integrity(i, &bytes)? {
                Verification::Verified => Some(true),
                Verification::Unsupported => {
                    self.status.record_integrity_unverified();
                    None
                }
            },
            None => None,
        };
        Ok(Payload {
            bytes,
            source,
            media_type,
            verified,
        })
    }

    /// Download an arbitrary URL under the policy (used for the data link and
    /// for auxiliary links such as MeteoAlarm's `rel=geometry`). Cached by
    /// URL for [`URL_CACHE_TTL`].
    pub async fn download(&self, href: &str) -> Result<Bytes, Wis2Error> {
        if let Some(b) = self.cached(href) {
            return Ok(b);
        }
        let url = self.policy.check_static(href)?;
        // Blocking getaddrinfo — bounded by the OS resolver timeout; the
        // pipeline runs on the background runtime so a stall here parks no
        // request worker.
        let url2 = url.clone();
        tokio::task::spawn_blocking(move || DownloadPolicy::check_resolved(&url2))
            .await
            .map_err(|e| Wis2Error::Download(format!("dns task: {e}")))??;
        let _permit = self
            .limiter
            .acquire()
            .await
            .map_err(|_| Wis2Error::Download("fetcher closed".into()))?;
        let result = self.get_bounded(&url).await;
        self.status.record_download(result.is_ok());
        let bytes = result?;
        self.remember(href, bytes.clone());
        Ok(bytes)
    }

    async fn get_bounded(&self, url: &Url) -> Result<Bytes, Wis2Error> {
        let resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| Wis2Error::Download(format!("'{url}': {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Wis2Error::Download(format!("'{url}': HTTP {status}")));
        }
        if let Some(len) = resp.content_length() {
            if len > self.max_bytes {
                return Err(Wis2Error::TooLarge(format!(
                    "'{url}': Content-Length {len} exceeds cap {}",
                    self.max_bytes
                )));
            }
        }
        let mut buf = BytesMut::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Wis2Error::Download(format!("'{url}': body: {e}")))?;
            if (buf.len() + chunk.len()) as u64 > self.max_bytes {
                return Err(Wis2Error::TooLarge(format!(
                    "'{url}': body exceeds cap {}",
                    self.max_bytes
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    fn cached(&self, href: &str) -> Option<Bytes> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(href)
            .filter(|(t, _)| t.elapsed() < URL_CACHE_TTL)
            .map(|(_, b)| b.clone())
    }

    fn remember(&self, href: &str, bytes: Bytes) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= URL_CACHE_MAX {
            cache.retain(|_, (t, _)| t.elapsed() < URL_CACHE_TTL);
            if cache.len() >= URL_CACHE_MAX {
                cache.clear();
            }
        }
        cache.insert(href.to_string(), (Instant::now(), bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::parse_notification;
    use crate::test_util::fixture;

    fn fetcher() -> Fetcher {
        Fetcher::new(
            DownloadPolicy::new(vec![]),
            1 << 20,
            Arc::new(Status::new()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn inline_content_needs_no_network_and_verifies_sha512() {
        // Captured se-smhi notification (2026-09-12); the digest is the
        // producer's sha512 of the inline BUFR.
        let (topic, raw) = fixture("wnm-se-smhi-synop-inline.json");
        let n = parse_notification(&topic, &raw).unwrap();
        let f = fetcher();
        let p = f.resolve(&n).await.unwrap();
        assert_eq!(p.source, PayloadSource::Inline);
        assert_eq!(p.verified, Some(true));
        assert_eq!(&p.bytes[..4], b"BUFR");
        assert_eq!(p.bytes.len(), 257);
        assert_eq!(f.status.snapshot().downloads_total, 0);
    }

    #[tokio::test]
    async fn corrupted_inline_content_fails_integrity() {
        let (topic, raw) = fixture("wnm-se-smhi-synop-inline.json");
        let mut n = parse_notification(&topic, &raw).unwrap();
        let c = n.content.as_mut().unwrap();
        // Flip one character inside the base64 body.
        let mut v: Vec<u8> = c.value.clone().into_bytes();
        v[40] = if v[40] == b'A' { b'B' } else { b'A' };
        c.value = String::from_utf8(v).unwrap();
        let f = fetcher();
        assert!(matches!(f.resolve(&n).await, Err(Wis2Error::Integrity(_))));
        assert_eq!(f.status.snapshot().dropped(DropReason::Integrity), 1);
    }

    #[tokio::test]
    async fn link_only_notification_is_policy_checked_before_any_request() {
        let raw = r#"{"id":"a","type":"Feature","version":"v04","properties":{"data_id":"d","pubtime":"2026-01-01T00:00:00Z"},
          "links":[{"rel":"canonical","href":"https://169.254.169.254/latest/meta-data","type":"application/bufr"}]}"#;
        let n = parse_notification("cache/a/wis2/x/y", raw.as_bytes()).unwrap();
        let f = fetcher();
        assert!(matches!(f.resolve(&n).await, Err(Wis2Error::Policy(_))));
        assert_eq!(f.status.snapshot().dropped(DropReason::Policy), 1);
        assert_eq!(f.status.snapshot().downloads_total, 0);
    }

    #[tokio::test]
    async fn declared_length_over_cap_is_rejected_without_a_request() {
        let raw = r#"{"id":"a","type":"Feature","version":"v04","properties":{"data_id":"d","pubtime":"2026-01-01T00:00:00Z"},
          "links":[{"rel":"canonical","href":"https://gc.example.org/x.bufr","length":99999999}]}"#;
        let n = parse_notification("cache/a/wis2/x/y", raw.as_bytes()).unwrap();
        let f = fetcher();
        assert!(matches!(f.resolve(&n).await, Err(Wis2Error::TooLarge(_))));
        assert_eq!(f.status.snapshot().downloads_total, 0);
    }
}
