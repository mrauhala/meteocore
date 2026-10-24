# Storage transport policy

Generic HTTP/HTTPS sources do not follow redirects, including same-origin
redirects. Configure the final object/feed URL directly. A 3xx response is a
storage error, not a missing object. GET, HEAD, range reads, batched reads and documents
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
fill. `start_get_opt` is a `FetchBudget::Request` read too: it runs ahead on
the runtime (#999), and several threads can wait for it behind an `Arc`
(#1127). `start_get_ranges_opt` starts `get_ranges_opt` the same way, one
buffer per range (#1261). Each `PendingGet::wait` is limited by its own
thread's request deadline, so one request's deadline never cuts off a read
another waits on; the read is aborted when its last holder drops it.

`get_range_many` takes the budget too: one byte range of each of many
objects on one storage-bridge call, at most `concurrency` in flight, one
slot per read in input order (#1042). The batch also ends at an `until`
instant: a read that has not finished by then is `None`, whether it was
in flight or never started, so a poll keeps what arrived and leaves the
rest to the next one. GeoTIFF's remote scan reads new files' COG headers
through it, newest first.

# Catalog discovery

`discovery` holds what every engine that polls one file per timestep needs:
the `time_window`, strftime prefix expansion, the `FilenameMatcher` and the
catalog scan. Engines call `scan_local` for a directory and `scan_remote` for
an object store's date-expanded prefixes. Both keep a file when its basename
is not excluded, matches and has a timestamp inside the inclusive window.
Exclusion comes first, so an excluded partial upload never displaces the
finished file of the same timestamp or takes a `max_files` slot. A hidden
basename is always excluded (#1009). A LIST is recursive, so `scan_remote`
also skips an object in a hidden folder below its prefix, such as rsync's
`.~tmp~/`; `scan_local` never descends (#1029). They return the
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
`list_prefixes` and applies its own rules, the hidden-folder one through
`ds_core::temp_files::in_hidden_folder` or `is_temporary_below`.

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

# THREDDS catalog discovery

A THREDDS Data Server lists each directory in a `catalog.xml`: every file
(`dataset`) with its `urlPath`, a `dataSize` in units (`71.56 Gbytes`) and
a `date type="modified"`, every subdirectory as a `catalogRef`, and the
services the files are served by. `discovery::thredds::parse` reads one with
quick-xml events: entity references in text arrive as `GeneralRef` events
of their own since quick-xml 0.38 and are kept, attribute values are
unescaped, and a document whose root is not `catalog` (an HTML error page)
is an error.

`CatalogProber` is a `TemplateProber` whose checks the catalogs answer
(`TemplateProber::poll_with`) instead of a HEAD each: the template still
names the files, and the catalog of each key's directory says whether the
file is there and in which version. Settling, publication and rechecks are
the template prober's, over the catalog's size and date. A poll reads the
catalogs on the way to the directories it checks: from the root
`catalog_url` it follows only `catalogRef`s leading toward a directory of a
slot in the window, never one to another origin or outside the root's
directory, at most `MAX_CATALOGS_PER_POLL` (64) catalogs, four at a time
and one storage-bridge call per level of the walk. A directory's catalog URL, once found, is kept
while a slot in the window lies under it and read directly. A catalog that
cannot be read fails its keys, which keep their state, and is walked to
again from its parent. The catalog tree must mirror the file tree under the
template's base, as THREDDS `datasetScan` serves it.

A catalog's size has four significant digits, so it cannot be the identity
a conditional range read checks. Each listed file is HEADed once for its
exact `ObjectVersion`, and again when its catalog entry changes or
`recheck(key)` is called; `files()` carries the HEADs' versions, and a
poll's `added` and `withdrawn` are what joined and left it. The catalogs and
the HEADs take the stores they are given: an engine passes its host's
background share.

THREDDS streams its catalogs chunked, without `Content-Length`
(thredds.met.no answers `Transfer-Encoding: chunked`), and object_store's
GET fails a response without one. Catalogs are therefore read with
`DataStore::get_documents`: a GET marked as a document in its
`GetOptions::extensions` passes through every layer of the store as any GET
(the `storage` span naming the collection, the store's request share, the
host's adaptive limit and request gauges, each held until the body is
read) and is sent, at the bottom of every HTTP store this crate builds, by
the store's own transport (`document.rs`): the host's throttled `reqwest`
client, so its `Retry-After` holds the request and a 429 cuts the host's
limit, with no redirect followed and nothing decompressed. The body is read
whole with a size cap checked against a declared length first and then
while it streams, never after; a 429, a 5xx or a failed connection is
retried twice, within the request deadline or the fetch budget. Any other
store answers a plain GET, capped the same way.

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

`get_ranges_opt` (and `_on`) reads several byte ranges of one object under
the request budget: one GET per range, up to ten at once, through
object_store's `coalesce_ranges`. Only ranges that overlap or touch are
merged, so the caller decides which near ranges are worth one GET;
object_store's own `get_ranges` would merge any within 1 MiB. A missing
object is `Ok(None)`, as in `get_opt`, and a range past the end may come
back short, so the caller checks lengths. The PVOL engine reads the byte
spans of a moment's decode with it (#1047).

# Request limits

`DataStore::with_request_limit(n)` wraps a store in object_store's
`LimitStore`: at most `n` operations in flight, a GET holding its permit
until its body is read. Limits stack, so a background share of 2 within a
host's 8 is `host.with_request_limit(2)`. Waiting for a permit counts
against the operation's budget and the request deadline.
`build_http_store_limited(url, n)` gives every store built for the same
scheme, host and port one shared limit, so collections reading one server
stay under it together; the first limit named for a host is kept for the
life of the process. That limit adapts to a host that throttles (next
section). `host_requests()` reports, per host and port, the limit as it is
now, the requests holding one of its permits and those waiting for one
(two counting stores around the limited store), for the server's
`storage_host_request_limit` and `storage_host_requests_{in_flight,queued}`
gauges.

# Host back-off

A concurrency limit is not a rate limit, and object_store retries a 429 Too
Many Requests one request at a time, after a 0.1 s backoff, while the other
requests to the host go on. So every HTTP store, limited or not, sends its
requests through one throttle per scheme, host and port (`throttle.rs`,
#1235). It sees the response to every attempt, object_store's retries
included, and on a 429:

- **`Retry-After`**, in seconds or as an HTTP-date, pauses new requests to
  the host, from every store and collection on it, until then, at most 60 s
  (`MAX_RETRY_AFTER`). The wait comes before the request is sent and is
  part of the operation, so its fetch budget and the request deadline bound
  it: one longer than the deadline fails the operation at the deadline.
  A 429 without `Retry-After` pauses nothing.
- **The adaptive limit** of a `build_http_store_limited` host halves, never
  below 1 and at most once per second (`CUT_WINDOW`), so the 429s of one
  burst cut it once. Each 30 s without a 429 (`QUIET_INTERVAL`) adds one
  back, up to the configured limit. A cut takes the free permits at once
  and the held ones as they are released: requests in flight finish, new
  ones see the lower limit. A share stacked with `with_request_limit` stays
  within it.

`storage_host_request_limit{host}` shows the limit as it is now, so a cut
shows on the dashboard's host panel. A cut, or a `Retry-After` pause, logs
one WARN per host per minute naming the host, the limit, the configured
limit and the wait; the limit's return to the configured one logs an INFO.
S3 is out of scope: it throttles with 503 SlowDown, which object_store
retries.

# The storage span

object_store logs each retry of a throttled or failed request at INFO,
target `object_store::client::retry`, with only the status, the backoff and
the attempt number. So every operation on a remote store, S3 or HTTP, runs
inside `info_span!("storage", collection, store, path)` (#1233):

- `collection`: the collection the store reads for. An engine names it
  once, where it builds its store, with `DataStore::for_collection(id)`;
  the field is absent until then.
- `store`: the source. An HTTP server is `host[:port]` (`thredds.met.no`),
  an S3 bucket its endpoint's `host[:port]` and the bucket
  (`s3.us-east-1.amazonaws.com/noaa-goes19`). Never userinfo, a path or a
  query string, which on a signed URL carries the signature.
- `path`: the object key, or the list prefix.

The span covers the whole operation: the wait for a request-limit permit
and for a host's `Retry-After`, the request, object_store's retries, and the GET body or LIST stream that
object_store retries body reads in. With `LOG_FORMAT=json` the server writes
the current span on every event, so a retry line carries
`"span":{"name":"storage","collection":"meps-sfc","store":"thredds.met.no","path":"…"}`;
the human-readable format prints it as a `storage{…}` prefix. The span is at
INFO under target `ds_storage::traced`, so a `RUST_LOG` that keeps
object_store's INFO lines but drops that target logs them without it.

Every `build_*` function that makes a remote store labels it
(`DataStore::remote` labels a store built elsewhere); local and in-memory
stores have no span, since they never retry. `DataStore` keeps its store
without the span and rebuilds the span when it is limited or names its
collection, so an operation is in exactly one `storage` span, and stores of
one host limit that name different collections still share that limit
(`DataStore::same_store`). `inner()` returns the store with the span, for
callers that hand it to another library.
