//! Smoke-test tap: subscribe to WIS2 topics and print what arrives.
//!
//! ```text
//! cargo run -p ds-wis2 --example wis2_tap -- \
//!     --topic 'cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop' \
//!     --seconds 120 --resolve
//! ```
//!
//! With `--resolve` every accepted notification goes through the full
//! pipeline (inline decode or download + integrity), exactly as an engine
//! would see it; without it only the subscriber/dedup stage runs.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use ds_core::config::Wis2Config;
use ds_poll::Shutdown;
use ds_wis2::status::DropReason;
use ds_wis2::subscriber::{Subscriber, SubscriberOptions};
use ds_wis2::{spawn_pipeline, PayloadSource, Status};

#[derive(Parser)]
struct Args {
    /// Broker URL (mqtts://host:port).
    #[arg(long, default_value = ds_core::config::DEFAULT_WIS2_BROKER)]
    broker: String,
    /// Topic filter(s); repeatable.
    #[arg(long, required = true)]
    topic: Vec<String>,
    /// How long to listen.
    #[arg(long, default_value_t = 60)]
    seconds: u64,
    /// Resolve payloads (inline decode / download + integrity).
    #[arg(long)]
    resolve: bool,
    /// Download allowlist prefixes (strict mode).
    #[arg(long)]
    allow: Vec<String>,
    /// Print every accepted notification (default: first 20).
    #[arg(long)]
    verbose: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ds_wis2=debug".into()),
        )
        .init();
    let args = Args::parse();
    let cfg = Wis2Config {
        broker: args.broker.clone(),
        topics: args.topic.clone(),
        download_allowlist: args.allow.clone(),
        ..Wis2Config::default()
    };
    ds_core::config::validate_wis2("tap", "wis2", &cfg).expect("config");
    let shutdown = Arc::new(Shutdown::new());
    let deadline = tokio::time::sleep(Duration::from_secs(args.seconds));
    tokio::pin!(deadline);
    let mut shown = 0usize;
    let limit = if args.verbose { usize::MAX } else { 20 };

    let status: Arc<Status> = if args.resolve {
        let mut p = spawn_pipeline(&cfg, "tap", shutdown.clone()).expect("pipeline");
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                r = p.receiver.recv() => {
                    let Some(r) = r else { break };
                    if shown < limit {
                        shown += 1;
                        let n = &r.notification;
                        match &r.payload {
                            Some(pl) => println!(
                                "{} | {} | {} bytes {} verified={:?} | {}",
                                n.topic, n.data_id, pl.bytes.len(),
                                match &pl.source { PayloadSource::Inline => "inline".to_string(), PayloadSource::Downloaded(u) => format!("from {u}") },
                                pl.verified,
                                n.wigos_station_identifier.as_deref().unwrap_or("-")
                            ),
                            None => println!("{} | {} | DELETION", n.topic, n.data_id),
                        }
                    }
                }
            }
        }
        p.status
    } else {
        let opts = SubscriberOptions::from_config(&cfg, "tap").expect("options");
        let status = Arc::new(Status::new());
        let mut rx = Subscriber::spawn(opts, "tap".into(), shutdown.clone(), status.clone());
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                n = rx.recv() => {
                    let Some(n) = n else { break };
                    if shown < limit {
                        shown += 1;
                        println!(
                            "{} | {} | inline={} | {}",
                            n.topic, n.data_id, n.content.is_some(),
                            n.canonical_link().map(|l| l.href.as_str()).unwrap_or("-")
                        );
                    }
                }
            }
        }
        status
    };
    shutdown.shutdown();
    let s = status.snapshot();
    println!("--- {}s ---", args.seconds);
    println!(
        "connected={} subscribed={} received={} reconnects={}",
        s.connected, s.subscribed, s.messages_received_total, s.reconnects_total
    );
    for r in DropReason::ALL {
        if s.dropped(r) > 0 {
            println!("dropped[{}]={}", r.label(), s.dropped(r));
        }
    }
    println!(
        "downloads={} failures={} integrity_unverified={} last_lag_secs={:?}",
        s.downloads_total, s.download_failures_total, s.integrity_unverified_total, s.last_lag_secs
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
}
