#!/usr/bin/env bash
# Add one packaged chart to a classic Helm repository index (#1365).
#
# usage: scripts/helm_repo_index.sh <chart.tgz> <download-base-url> <index.yaml>
#
# The index lists the chart at <download-base-url>/<file name>: the GitHub
# release it is attached to, so the repository (the gh-pages branch) holds
# only index.yaml. <index.yaml> is updated in place and created when missing.
# Entries for other versions are kept; an entry for the same version is
# replaced, so re-publishing a release is idempotent.
#
# The release workflow runs this on the gh-pages checkout, and the chart
# workflow runs it on a scratch index so the merge is tested before a release.
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <chart.tgz> <download-base-url> <index.yaml>" >&2
  exit 2
fi
chart=$1
url=${2%/}
index=$3

if [ ! -f "$chart" ]; then
  echo "error: no chart package at $chart" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -- "$chart" "$work/"
args=(repo index "$work" --url "$url")
if [ -s "$index" ]; then
  # helm keeps the new entry when both indexes hold the same version.
  args+=(--merge "$index")
fi
helm "${args[@]}"
mv -- "$work/index.yaml" "$index"
