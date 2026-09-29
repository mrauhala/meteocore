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
