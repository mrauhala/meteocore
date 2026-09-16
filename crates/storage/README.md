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
