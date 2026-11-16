# Security policy

## Reporting a vulnerability

Please do not report a vulnerability in a public issue, pull request or
discussion.

Report it privately through GitHub instead: open the repository's
[Security tab](https://github.com/mrauhala/meteocore/security) and choose
**Report a vulnerability**. Only the maintainers see the report.

A useful report says:

- the version or commit affected;
- the request, configuration or data that triggers it;
- what an attacker gains: data read, denial of service, code run, and so on.

## What happens next

- The report becomes a draft security advisory, private to the maintainers
  and the reporter.
- The fix is prepared in the advisory's temporary private fork, so it is not
  public before a release carries it.
- The advisory is published once a release with the fix is out, with a CVE
  where one applies. The reporter is credited unless they prefer not to be.

Handling is best effort; there is no fixed response time.

## Supported versions

Fixes go to `main` and ship in the next release. Only the latest release
is supported.

## Scope

In scope: the MeteoCore server, its crates in this repository, the container
image and the Helm chart in `charts/meteocore`.

How a deployment exposes the server is the operator's choice. For example:

- The admin endpoints answer 403 unless `ADMIN_TOKEN` is set. With it set,
  keep the token secret and the endpoints private.
- `/metrics` belongs behind a proxy or network policy that keeps it private.
- Whoever can write `collections_dir` controls what the server serves: with
  `watch_collections_dir = true`, a written file is a reload.

A deployment that publishes these is not a vulnerability in MeteoCore. A
default or a document that leads operators into it is: report that.
