# ds-wis2 — WMO WIS2 consumer client

Shared by every engine that ingests through WIS2 notifications
(`[collections.cap.wis2]`, `[collections.bufr.wis2]`). Config struct
`Wis2Config` + `validate_wis2` live in `ds_core::config` (framework-free
serde data); everything with a socket lives here.

## What WIS2 looks like from the consumer side (verified live 2026-09-12)

- Global Brokers (`mqtts://globalbroker.meteo.fr:8883`, `everyone`/
  `everyone`, MQTT v5, QoS 1) relay a GeoJSON **notification** per data
  object. Consumers subscribe to `cache/a/wis2/{centre-id}/data/core/…`;
  `origin/…` carries the producer's own copy (and all `recommended` data,
  which the caches never mirror).
- **Every `data_id` arrives once per Global Cache** — six copies in
  practice, each with `links[rel=canonical]` rewritten to that cache's host.
  `Dedup` (keyed by `data_id`, latest `pubtime` wins; message `id` for exact
  redelivery) is not optional. Expect `wis2_messages_dropped_total{reason=
  "duplicate"}` ≈ 5× the accepted count.
- Small objects (SYNOP BUFR, small CAP) ride **inline** in
  `properties.content` (base64/utf-8/gzip, ≤ 4096 encoded bytes) — no HTTP
  at all. Link-only notifications (us-noaa ship, il-ims, MeteoAlarm CAP) are
  downloaded from `rel=canonical` (fallback `rel=update`); `rel=deletion`
  means "forget `data_id`".
- `properties.integrity` = sha256/384/512 (verified) or sha3-* (accepted,
  counted in `integrity_unverified_total` — no sha3 dependency on purpose).
- Producer-specific properties (MeteoAlarm `alertId`/`indexInfo`/
  `indexArea`, `gts`, …) are kept verbatim in `Notification::extra`; this
  crate never interprets them.

## Rules

- **Runtime:** `spawn_pipeline` / `Subscriber::spawn` spawn onto the
  *current* runtime. Call them from an engine's `poll_loop()` (which runs on
  `poll_runtime()`), never from an engine constructor — constructors run on
  the request-serving runtime at boot and on reload.
- **Sessions:** `clean_start = false` + `session_expiry_interval =
  session_expiry_secs` (default 900, cap 86400). A short reconnect resumes
  the QoS-1 backlog; a retired replica's session dies on its own. Client id
  = `{prefix}-{collection}-{8 random hex}` — never a stable id, two replicas
  with the same id evict each other forever. `$share/` filters are rejected
  in config: every replica keeps its own in-memory store and needs the
  whole stream.
- **Subscribe from the event-loop task with `try_subscribe_many` (sync).**
  Awaiting `subscribe()` from the task that drives `EventLoop::poll()`
  deadlocks when the request channel is full.
- **TLS** is built explicitly (`webpki-roots` + the `ring` provider): both
  `ring` (reqwest) and `aws-lc-rs` (object_store) are in the dependency
  graph, so `rustls::ClientConfig::builder()` without a named provider
  panics at connect time; and rumqttc's default config `expect()`s on the
  OS certificate store.
- **Packet size:** rumqttc defaults to 10 KiB and would silently reject
  every notification carrying inline content; we set 1 MiB.
- **Download policy** (`policy.rs`): https only, DNS-name host (no IP
  literals / localhost / `.local` / `.internal`), resolved addresses must be
  public, redirects off, size cap streamed. `download_allowlist` switches to
  strict prefix mode — recommended for `origin/` subscriptions (canonical
  links point at arbitrary producer servers). There is deliberately **no
  built-in cache-host allowlist**: the Global Cache hostnames vary per cache
  and change without notice. DNS-rebinding TOCTOU is accepted and
  documented in the module.
- **Backpressure, not drops:** the subscriber→resolver and resolver→engine
  channels are bounded (1024); a slow consumer stops the MQTT read loop and
  the broker queues (2000/client) rather than us losing messages.
- Errors carry URLs and broker details — log them; the `From<Wis2Error>
  for DataServerError` impl maps to the generic `Engine` variant so nothing
  reaches an HTTP client.

## Smoke test

```bash
cargo run -p ds-wis2 --example wis2_tap -- \
  --topic 'cache/a/wis2/+/data/core/weather/surface-based-observations/synop' \
  --seconds 90 --resolve
# expect: connected=true subscribed=true, dropped[duplicate] ≈ 1.5× accepted,
# inline payloads with verified=Some(true), a handful of downloads, 0 policy drops
```

Fixtures in `tests/fixtures/` are real captured messages (`{"topic",
"message"}` wrapper): SMHI SYNOP inline (sha512 verifiable), NOAA ship
link-only, Roshydromet `rel=update`, MeteoAlarm CAP with `rel=geometry`,
IMD CAP inline. Re-capture with a paho-mqtt tap if the spec moves.
