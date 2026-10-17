# Storage transport policy

Generic HTTP/HTTPS sources do not follow redirects, including same-origin
redirects. Configure the final object/feed URL directly. A 3xx response is a
storage error, not a missing object. GET, HEAD, range reads and batched reads
share this policy. Explicit `http://` sources are allowed; HTTPS sources remain
HTTPS-only. Certificate validation and the process's proxy environment apply.

The HTTP connector disables transparent content decompression so object byte
ranges retain their source offsets. Connections time out after five seconds;
requests and the storage bridge have 30-second limits. CAP feeds explicitly use this HTTP path even for S3-hosted documents, and
query-bearing URLs retain their query parameters. Generic `build_store` still
recognizes query-free AWS/CloudFerro hosts as S3 prefix-discovery sources; that
separate S3 connector retains its existing redirect behavior. Call
`build_http_store` when a URL is an exact HTTP document rather than a storage
prefix. This policy does not claim to prevent DNS
rebinding or validate arbitrary operator-configured source hosts.

# Fetch budgets

object_store gives each attempt of a request its own 30-second timeout (its
`ClientOptions` default for S3, the client above for HTTP). When a body read
stalls, that timeout fails the attempt and object_store retries after a short
backoff, asking only for the bytes still missing. The bridge's overall limit
decides whether that retry gets to run, so a whole-object fetch names its
`FetchBudget` (#1011):

- `FetchBudget::Request`, 30 seconds or the request deadline: every method
  without a budget argument, and every read a request waits on. A stalled
  attempt fails the call; a retry would not fit a render deadline anyway.
- `FetchBudget::Background`, 120 seconds: whole-object downloads on the poll
  runtime, which nobody waits on (satellite scan ingest, the PVOL catalog
  scan, GeoTIFF full downloads). Room for the stalled attempt, its retry and
  one more. A request deadline in scope still applies.

`get_with_budget`, `get_range_with_budget`, `get_range_on_with_budget`,
`get_range_if_unmodified` and `get_many_with_budget` take the budget; `get`,
`get_range`, `get_range_on`, `get_range_or_refusal` and `get_many` are the request budget. Don't pass the background budget where a
request can wait on the result, directly or through a single-flight cache
fill.

# Catalog discovery

`discovery` holds what every engine that polls one file per timestep needs:
the `time_window`, strftime prefix expansion, the `FilenameMatcher` and the
catalog scan. Engines call `scan_local` for a directory and `scan_remote` for
an object store's date-expanded prefixes. Both keep a file when its basename
is not excluded, matches and has a timestamp inside the inclusive window.
Exclusion comes first, so an excluded partial upload never displaces the
finished file of the same timestamp or takes a `max_files` slot. They return the
kept files oldest first, one per timestamp, capped to the newest `max_files`.
Of files sharing a timestamp the greatest key or path wins, whatever the
listing order, and each dropped file is logged at WARN.

A remote scan lists its prefixes concurrently, at most `MAX_CONCURRENT_LISTS`
(8) at a time, through `DataStore::list_many` on one storage-bridge call. It
never runs one blocking `list` per prefix in a loop. A 24-hour window over an
hourly prefix template, up to 26 prefixes, therefore takes four rounds of
LISTs rather than 26 sequential ones. Each LIST keeps the 30-second budget a single `list` has. A prefix that
fails to list is reported per prefix and does not stop the others; each engine
decides whether that fails its poll. Call the scan where `list` may be called:
from the background poll runtime, never from a request handler.

An engine that recognises its files without a matcher, like the PVOL network
scan, which keeps every site's volume at each timestamp, lists through
`list_prefixes` and applies its own rules.

A matcher may read a forecast lead as well as a timestamp: a template's
`{lead}`, two zero-padded digits, or a pattern's `lead` capture. A template's
`{run}` is the timestamp's hour, read at minute 0. The scan then keeps one
file per (timestamp, lead), and `max_files` counts timestamps, each keeping
all its leads. MET Norway's `meps_sfc_{lead}_%Y%m%dT{run}Z.nc` reads lead 5
of the 06 UTC run from `meps_sfc_05_20261017T06Z.nc`.

# Template discovery

Some HTTP servers cannot be listed: an Apache autoindex answers no WebDAV
`PROPFIND`, and thredds.met.no's fileServer answers it with 400. For these,
`TemplateProber` renders the keys files would have and HEADs them, at most 16
at a time on one storage-bridge call (`head_many`). A key is a literal base,
a strftime prefix template and a strftime filename template, rendered for a
slot time: every cadence step back from now (`Schedule::Cadence`, the DWD
radar composites), or every model run at its `run_hours`
(`Schedule::Runs`, with the run hour as `{run}`). A slot holds one file per
configured lead, `{lead}` in the filename. MEPS per-lead files are base
`thredds/fileServer`, prefix `meps25epsarchive/%Y/%m/%d/{run}/member_00`
and filename `meps_sfc_{lead}_%Y%m%dT{run}Z.nc`, run hours every 3 hours and
leads 0 to 66.

The prober keeps what it found between polls:

- The first lead is the sentinel. A slot whose sentinel has not been seen
  costs one HEAD per poll; its other leads are HEADed once it exists.
- A file seen goes from unsettled to settled: on sight (`Settling::OnSight`),
  or once two more polls see the same size and `Last-Modified`, or at once
  when its `Last-Modified` is `min_age` old (`Settling::Stable`). An
  unsettled file is re-HEADed on every poll.
- A slot is published all at once, when every lead has settled or
  `run_grace` after its sentinel appeared. A partial slot is published with
  the leads that have settled, with a WARN listing the missing ones; they
  are looked for again with the recheck below.
- Unless the source is `immutable`, every 30 minutes a published slot's
  sentinel is re-HEADed. A sentinel that changed or vanished has every file
  of its slot re-HEADed on the next poll. A file whose version changed is
  withdrawn and settles again as its new version.
- `recheck(key)` has a settled file re-HEADed on the next poll, ahead of
  discovery: for a reader whose conditional range read answered
  `RangeRead::Changed` (below), so a replaced lead is found at once, not at
  its run's next sentinel recheck.
- One poll makes at most `max_heads_per_poll` HEADs: unsettled files first,
  then files to recheck, then newest slot first the unpublished slots'
  sentinels and leads, then the due rechecks. The rest waits for the next
  poll. A slot is never published partially while some of its leads have
  not been HEADed yet.

Each poll's `ProbeReport` lists the files that joined and left `files()`,
the published files, so an engine can open new files and drop what it holds
for withdrawn ones. Call `poll` where `head` may be called: from the
background poll runtime. engine-geotiff's local scan settles new files with
the same `PendingFile` rule, by size alone.

# Range reads of files read in place

`get_range_if_unmodified` reads a byte range only while the key still names
the `ObjectVersion` (size, `Last-Modified`, ETag) the caller catalogued. The
GET carries `If-Unmodified-Since`, and `If-Match` for a strong ETag, and the
response's own size, `Last-Modified` and ETag are compared with the version
as well, since a server may ignore the preconditions. A 412, a 404 or a
response naming another version is `RangeRead::Changed`, not an error: the
caller drops what it holds for the file and re-ingests it. A rewrite within
the same second at the same size, on a server without ETags, is beyond any
HTTP check. `get_range_if_unmodified_on` drives the read on a supplied
runtime handle.

`get_range_or_refusal` (and `_on`) reads a byte range without a version and
answers a refusal no retry can change as `RangeAnswer::Refused`, not an
error (#1074): `RangeRefusal::NotFound` for a 404, `Forbidden` for a 401 or
403, and `Truncated` for a 416 or a response whose total size ends before
the range. object_store types only some of these; a 416, and a 403 from its
HTTP store, carry the status only in the text of a private error, which
`error::range_refusal` reads and `range_tests` pins against a real server.
Every other failure stays an `Err`. engine-geotiff's tile reads use it to
tell a gone object from a failed read.

# Request limits

`DataStore::with_request_limit(n)` wraps a store in object_store's
`LimitStore`: at most `n` operations in flight, a GET holding its permit
until its body is read. Limits stack, so a background share of 2 within a
host's 8 is `host.with_request_limit(2)`. Waiting for a permit counts
against the operation's budget and the request deadline.
`build_http_store_limited(url, n)` gives every store built for the same
scheme, host and port one shared limit, so collections reading one server
stay under it together; the first limit named for a host is kept for the
life of the process. `host_requests()` reports, per host and port, the
requests holding one of its permits and those waiting for one (two counting
stores around its `LimitStore`), for the server's
`storage_host_requests_{in_flight,queued}` and `storage_host_request_limit`
gauges.
