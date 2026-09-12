//! Subscriber → dedup → payload resolution, wired together.

use std::sync::Arc;

use ds_core::config::Wis2Config;
use ds_poll::Shutdown;
use futures::stream::StreamExt;
use tokio::sync::mpsc;

use crate::fetch::CONCURRENCY;
use crate::payload::Payload;
use crate::status::Status;
use crate::subscriber::{Subscriber, SubscriberOptions, CHANNEL_CAPACITY};
use crate::{DownloadPolicy, Fetcher, Notification, Wis2Error};

/// A notification whose data object has been obtained (or that announces a
/// deletion, which carries no object).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub notification: Notification,
    /// `None` for a `rel=deletion` notification.
    pub payload: Option<Payload>,
}

/// Handle to a running pipeline.
pub struct Pipeline {
    pub receiver: mpsc::Receiver<Resolved>,
    pub status: Arc<Status>,
    pub fetcher: Arc<Fetcher>,
}

/// Start the subscriber and the resolver stage on the **current runtime**
/// (call it from an engine's `poll_loop`, which runs on `poll_runtime()`).
/// Tasks stop when `shutdown` fires or the receiver is dropped.
///
/// Notifications are resolved with up to [`CONCURRENCY`] downloads in flight;
/// inline payloads (the common SYNOP case) never touch the network. Failures
/// are counted on `Status` and logged at debug; the stream continues.
pub fn spawn_pipeline(
    cfg: &Wis2Config,
    label: &str,
    shutdown: Arc<Shutdown>,
) -> Result<Pipeline, Wis2Error> {
    let opts = SubscriberOptions::from_config(cfg, label)?;
    let status = Arc::new(Status::new());
    let fetcher = Arc::new(Fetcher::new(
        DownloadPolicy::new(cfg.download_allowlist.clone()),
        cfg.max_download_bytes,
        status.clone(),
    )?);
    if !fetcher.policy().is_strict() && cfg.topics.iter().any(|t| t.starts_with("origin/")) {
        tracing::warn!(
            "[{label}] wis2: subscribing to origin/ topics without a download_allowlist — \
             canonical links point at producer servers; consider restricting downloads"
        );
    }

    let notifications =
        Subscriber::spawn(opts, label.to_string(), shutdown.clone(), status.clone());
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);

    let label = label.to_string();
    let f = fetcher.clone();
    tokio::spawn(async move {
        let stream = tokio_stream_from(notifications);
        let mut resolved = stream
            .map(|n: Notification| {
                let f = f.clone();
                async move {
                    if n.is_deletion() {
                        return Some(Resolved {
                            notification: n,
                            payload: None,
                        });
                    }
                    match f.resolve(&n).await {
                        Ok(payload) => Some(Resolved {
                            notification: n,
                            payload: Some(payload),
                        }),
                        Err(e) => {
                            tracing::debug!("wis2: {} dropped: {e}", n.data_id);
                            None
                        }
                    }
                }
            })
            .buffer_unordered(CONCURRENCY);
        loop {
            let item = tokio::select! {
                biased;
                _ = shutdown.wait() => break,
                item = resolved.next() => item,
            };
            match item {
                Some(Some(r)) => {
                    if tx.send(r).await.is_err() {
                        break;
                    }
                }
                Some(None) => {}
                None => break,
            }
        }
        tracing::info!("[{label}] wis2: resolver stopped");
    });

    Ok(Pipeline {
        receiver: rx,
        status,
        fetcher,
    })
}

fn tokio_stream_from<T>(mut rx: mpsc::Receiver<T>) -> impl futures::Stream<Item = T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
