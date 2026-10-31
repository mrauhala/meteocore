# api-mcp

Model Context Protocol (MCP) tools over MeteoCore's tracked storm cells,
served at `/mcp` behind a bearer token.

## What it solves

A language model asked "what is the storm situation?" needs the ranked cells,
their history and the caveats that come with them. Without this crate each
such client would drive the Features API itself, pick the frame and ordering,
and write its own guard against presenting a heuristic score as a warning.
This crate exposes a small, typed tool set for that one question and writes
its answers and errors for a model to read.

## How it works

- `router()` wraps rmcp's Streamable HTTP service in an axum middleware that
  checks the bearer token and rate limit. The server nests the result at
  `/mcp` only when `[mcp] enabled = true`.
- Tool state (`McpState`) is the Features registry: the `FeatureEngine` and
  config of every collection whose `apis` includes `"features"`. It lives in
  an `ArcSwap`, and the server rebuilds it after each collection reload.
- The cell tools accept only `engine_type = "nowcast"` collections, and
  `get_collection_info` calls engine methods only for those. Nowcast serves
  cells from an in-memory snapshot, so a tool call does no storage or
  database I/O on a request worker.
- Ranking, sorting and frame selection happen in the engine through
  `FeatureQuery`. This crate validates arguments, applies the
  `min_significance` floor, shapes the JSON and adds notes where an answer
  could be misread.
- `get_cell_track` walks the engine's retained frame instants
  (`FeatureEngine::available_times`) newest first and looks the cell up by id
  in each one (`get_feature_at`). Frames without cells count as steps.
- The `initialize` result carries server instructions: significance is a
  ranking heuristic, not an official warning; report only values present in
  the response; null means unknown, not zero; cells describe observed frames
  and never forecast positions.

## Tools

| Tool | Arguments | Returns |
|---|---|---|
| `list_collections` | none | every visible collection with `id`, `title`, `engine_type`, `serves_storm_cells`, plus `storm_cell_collections` |
| `get_collection_info` | `collection` | `id`, `title`, `description`, `engine_type`, `serves_storm_cells`; for nowcast also `tracked_cells` (cells in the newest frame) and, when known, `retained_frames` and `sortable_properties` |
| `get_storm_cells` | `collection`, `limit`, `at`, `sort_by`, `order`, `min_significance` | cells at one analysis frame, most significant first |
| `get_cell_track` | `collection`, `cell_id`, `samples` | one cell's properties at each retained frame, newest first |

`get_storm_cells` arguments:

| Argument | Default | Rule |
|---|---|---|
| `limit` | 10 | 1–50 |
| `at` | latest frame | RFC 3339 instant; serves the newest frame at or before it |
| `sort_by` | `significance`, descending | one of the collection's sortable properties |
| `order` | `desc` | `asc` or `desc`; only with `sort_by` |
| `min_significance` | none | 0–1; filters the whole frame before `limit` |

`get_cell_track` takes `samples`: the number of retained frames to walk,
default 24, range 1–48.

Every cell is an object with `id`, `lon`, `lat` and each property the engine
emits, sorted by name. A null property stays null.

Key response fields:

- `get_storm_cells`: `observed` (the frame served), `retained_frames`
  (`from`, `to`), `returned`, `total_tracked` (cells in the frame served),
  `no_frame_for_requested_time` (`at` is before the oldest retained frame),
  `requested_time_after_newest_frame`, `matching_min_significance` and
  `below_min_significance` (null without a floor), `cells`, and `note`, which
  always ends with the not-an-official-warning disclaimer.
- `get_cell_track`: `history`, `frames_walked`, `stopped_because`
  (`reached_earliest_retained_frame` or `samples_reached`),
  `retained_frames`, `note`. With no frames retained, `history` is empty and
  `retained_frames` is null.

Errors:

- Bad arguments return a JSON-RPC `invalid_params` error, not an `isError`
  tool result. The message says what was wrong: in the cell tools an unknown
  or non-nowcast collection lists the storm-cell collections, and an unknown
  `sort_by` lists the sortable properties. Out-of-range values are rejected,
  never clamped. Unknown argument names (`count` for `limit`) are rejected.
- An engine failure answers by its class, as in every API
  (`ds_core::error::ErrorClass`): a request the engine turns down or a
  resource it does not have is `invalid_params` with the engine's message; a
  busy engine or the deadline an internal error "Server busy, try again
  later"; a data source that keeps refusing reads one naming no upstream;
  anything else a generic `Query failed`. The detail of the last two goes to
  the server log only.

## Endpoint and authentication

Clients speak MCP over Streamable HTTP (the rmcp library's server
transport). A client that opens with `initialize` gets a session: it then
sends `notifications/initialized`, `tools/list` and `tools/call`, each with
the `mcp-session-id` header that `initialize` returned. Replies may be
SSE-framed (`data: {...}`). Every request needs
`Authorization: Bearer <token>`.

| Status | When |
|---|---|
| 404 | MCP is off (not enabled at boot, or disabled by a reload) |
| 401 | token missing or wrong (same body for both) |
| 429 | over the per-minute budget |
| 403 | the `Host` header is not in the allowlist |

- Token contents are compared in constant time (only the length check can
  short-circuit). The configured and presented
  tokens are both trimmed, so a trailing newline from a secret file is
  harmless.
- Authenticated requests and failed attempts have separate budgets of
  `rate_limit_per_min` each, so guessing cannot exhaust the real client's
  allowance.
- The `Host` allowlist protects against DNS rebinding. It holds `localhost`,
  `127.0.0.1`, `::1`, the host of the server's base URL (`BASE_URL` env var,
  else `[server] base_url`, else `host:port`) with and without its port, and
  any `allowed_hosts` entries.

## Configuration

`[mcp]` goes in the top-level `config.toml`. It is a load error in a
`collections_dir` file.

```toml
[mcp]
enabled = true              # default false
token_env = "MCP_TOKEN"     # default "MCP_TOKEN"; env var holding the token
rate_limit_per_min = 60     # default 60, per budget; 0 disables both
# allowed_hosts = ["alt.example.com"]  # default []; added to the derived list
```

- `enabled = true` with an empty `token_env`, or with that variable unset or
  blank, fails config validation. There is no unauthenticated mode.
- A collection appears in the tools only if its `apis` includes
  `"features"`. The cell tools also need `engine_type = "nowcast"`;
  [`collections.d/radar-fi-nowcast.toml`](../../collections.d/radar-fi-nowcast.toml)
  is such a collection.
- A reload refreshes the visible collections and honours a change to
  `enabled` (off answers 404). The token is read once at boot. Enabling MCP
  on a server that started without it, rotating the token, and changing
  `token_env`, `rate_limit_per_min` or `allowed_hosts` need a restart.

## Limits

- Only nowcast storm cells. Other Features collections show up in
  `list_collections` and `get_collection_info` as config metadata only. There
  is no point, area or raster access.
- One shared token, so the rate limit is global, not per client. The window
  is a fixed 60 seconds: a burst across a window boundary can briefly reach
  twice the limit.
- The `Host` allowlist is built once at boot from the startup base URL.
  Per-request proxy headers (`trust_proxy_headers`) do not extend it.
- A `min_significance` floor checks at most the first 1000 cells of a frame;
  past that, the note says the counts are partial.

## Testing

```bash
cargo test -p api-mcp
```

Unit tests in `src/auth.rs` cover token comparison, trimming, the disable
switch and the two rate budgets. `tests/mcp_tests.rs` sends HTTP requests
through the full router in process, with in-memory stub `FeatureEngine`s (no
data fixtures) and a non-loopback `Host`. It covers the auth boundary, the
Host allowlist, the handshake, the advertised tool list,
`get_collection_info`, both cell tools, argument validation and error
hiding.
The missing-token load error is tested in ds-core
(`cargo test -p ds-core mcp_enabled_without_a_token`).

Contributor rules and design reasons are in [CLAUDE.md](CLAUDE.md).
