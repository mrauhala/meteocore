# api-mcp crate — Claude Instructions

Model Context Protocol tools over MeteoCore's tracked storm cells, so a model
can ask about the weather situation directly instead of every client
re-implementing the ranking. Read the root `CLAUDE.md` first — Critical Rules
6/7 and 11 are the ones that shape this crate.

## The auth boundary is the load-bearing part

This is the **first authenticated public route** in MeteoCore. A broken tool
returns a confusing answer; a broken guard publishes every Features collection
to anyone who finds the URL. Rules, all tested:

- **There is no unauthenticated mode.** `[mcp] enabled = true` with no
  resolvable token is a `ServerConfig::validate` error, so the server refuses
  to boot rather than serving `/mcp` open. A default-empty token would make a
  typo publish the endpoint.
- **The token comes from an env var** (`token_env`, default `MCP_TOKEN`),
  never inline — same reasoning as `[postgis] dsn_env`.
- **Comparison is constant-time.** `/admin` uses `==` (a known wart); a
  byte-by-byte compare leaks position-of-first-mismatch, which is practical
  against a token an MCP client retries cheaply.
- **Auth failures are indistinguishable.** Missing and wrong tokens return the
  same body — a distinguishable message is a small oracle.
- **`router()` returns a Router, not the bare transport**, because the guard
  has to wrap it. Mounting `StreamableHttpService` directly would publish an
  unauthenticated endpoint; that's why the signature is shaped this way.
- **Disable takes effect on reload; first-time enable does not.** The route
  is nested once at boot, so `McpAuth` carries an `enabled` flag that
  `do_reload` flips; a disabled endpoint answers 404 (absent, not "wrong
  credential"). A server that booted *without* MCP cannot gain it by reload —
  `do_reload` logs a warning saying so rather than reporting success and
  changing nothing. Token and rate limit are fixed at boot too.
- **Engine errors never reach the client** (Critical Rule 11).
  `DataServerError`'s Display carries `Storage`/`Io` detail — filesystem
  paths, backend messages — so `query_failed()` logs it and returns a fixed
  string, exactly as api-features does at the equivalent site. *Parameter*
  errors are different and deliberately verbose: they describe the caller's
  own input and are what lets a model self-correct.
- **The configured token is trimmed**, matching the presented one. An env var
  set from a file or a here-doc carries a trailing newline, and without this
  the two never compare equal and every request 401s with no clue why.
- **Failed auth has its own budget.** Two buckets, not one check before auth:
  a single shared bucket consulted first would let unauthenticated traffic
  exhaust it and lock out the legitimate client — trading a brute-force hole
  for a denial of service. As originally written the limiter only ever saw
  requests that had *already* authenticated, leaving guessing unmetered.
- **Rate limiting is global, not per-client.** One shared token means there is
  no client identity to key on, so it caps blast radius rather than
  apportioning fairly. Fixed window, so a burst across a boundary can briefly
  reach 2× — deliberate.

## Why only `engine_type = "nowcast"`

Every data tool resolves the collection and rejects anything that isn't a
nowcast collection. That is two rules at once:

1. **Semantic** — only nowcast serves tracked cells.
2. **Runtime safety** — nowcast's `FeatureEngine` reads an in-memory `ArcSwap`
   snapshot, but a postgis `FeatureEngine` is a sync bridge over a database.
   Calling that from an MCP handler would park a request-serving worker
   (Critical Rule 7). **Widening the tool set means solving that first**, not
   adding a match arm.

`get_collection_info` is included in that rule: it returns config metadata for
any collection but only calls engine methods (`feature_count`,
`temporal_extent`, `sortables`) for cells collections. `feature_count()` on a
postgis engine issues a COUNT against the database.

## Tools

`list_collections`, `get_collection_info`, `get_storm_cells`,
`get_cell_track`. Deliberately narrow — broad access (`query_position`,
`query_area`) needs per-engine bounds and cost controls first.

- `get_storm_cells` is one bounded query: server-side sorting (#605) means
  top-K doesn't mean fetching everything and sorting in the handler.
- `get_cell_track` walks snapshots backward by asking for "newest frame at or
  before t", then stepping to just before that frame's instant. **No cadence
  is assumed** — the engine's retention decides the steps, so a source that
  changes interval still walks correctly. Capped at `MAX_TRACK_SAMPLES`,
  because each step materializes that frame's whole cell set. A frame with
  **zero** cells carries no `observed` to step from, so the walk probes a
  minute further back rather than concluding the history ends there — bounded
  separately by `MAX_TRACK_PROBES`.

## Writing for a model, not a person

- **Error messages name the collections that would work.** A wrong guess then
  self-corrects on the next call instead of becoming an apology to the user.
- **The server instructions carry the hallucination guards** — significance is
  a ranking heuristic and not a warning, report only values present in the
  response, null means unknown not zero, cells never forecast. A test asserts
  the "not an official warning" phrasing survives, because losing it is a
  silent correctness regression, not a cosmetic one.
- **`get_storm_cells` repeats the disclaimer in its own response**, so a model
  summarizing one call in isolation still sees it.
- **Null is passed through as null.** Flattening it to `false`/`0` would let a
  model state something untrue about a frame where a join was skipped.

## Gotchas

- **The transport validates the `Host` header against an allowlist**, not
  merely requires one — rmcp defaults to `localhost`/`127.0.0.1`/`::1`, so a
  deployment behind ANY reverse proxy answers 403 to every request until its
  public host is added. `allowed_hosts()` derives it from `base_url`;
  `[mcp] allowed_hosts` overrides for a proxy presenting another name.
  **This shipped broken**, because the smoke test ran against `127.0.0.1` —
  which the default allows. The integration tests now speak to a public
  hostname for exactly that reason; do not "simplify" them back to
  localhost.
- Responses are SSE-framed (`data: {...}`) whenever the client accepts
  `text/event-stream`, which MCP clients must. Tests unwrap it.
- MCP requires an `initialize` handshake plus the `initialized` notification
  before any other method; a test helper that builds a fresh app per call
  loses the session.
- Argument errors surface as JSON-RPC `invalid_params` rather than
  `isError: true` tool content. Both reach the client; the result channel
  would be marginally better for model self-correction. Revisit if models are
  observed failing to retry.
- MCP state is **derived from the Features registry**, not loaded separately —
  a second source would only be a way for the two to disagree after a reload.
  `do_reload` re-derives it right after swapping Features state.
