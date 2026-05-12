#!/usr/bin/env bash
# vendor-maplibre.sh — pin and ship MapLibre GL JS for the built-in preview UI.
#
# Run once per version bump. Downloads the official `dist.zip` release asset,
# verifies the sha256, and extracts only the runtime files (minified JS, CSS,
# license) into `crates/server/preview/vendor/`. Those files are checked into
# git so `cargo build` is fully offline — no network round-trip at build time
# and no surprise at deploy time.
#
# Bumping the pin:
#   1. Edit `VERSION` below.
#   2. Run this script once unverified (`SHA256` empty) to print the actual
#      sha256.
#   3. Paste the printed value into `SHA256` and commit both the script bump
#      and the refreshed `crates/server/preview/vendor/` contents in one PR.
#
# License: MapLibre GL JS is BSD-3-Clause. The vendored `LICENSE.txt` lives
# next to the JS bundle to satisfy the redistribution-with-attribution clause.

set -euo pipefail

VERSION="${MAPLIBRE_VERSION:-5.24.0}"
SHA256_EXPECTED="edcc812e8334825d91cd97b54c3953e361cfbf6c8f37f0b01312181a855522e6"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR_DIR="$REPO_ROOT/crates/server/preview/vendor"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

URL="https://github.com/maplibre/maplibre-gl-js/releases/download/v${VERSION}/dist.zip"

echo "Fetching MapLibre GL JS v${VERSION}..."
curl --fail --silent --show-error --location --output "$TMP/dist.zip" "$URL"

# Portable sha256: prefer GNU coreutils (`sha256sum`, ubiquitous on Linux);
# fall back to `shasum -a 256` (macOS / BSD via Perl). One of the two is
# guaranteed to exist on any developer or CI environment we target.
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "$TMP/dist.zip" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL=$(shasum -a 256 "$TMP/dist.zip" | awk '{print $1}')
else
    echo >&2 "neither sha256sum nor shasum is on PATH; install GNU coreutils or perl-Digest-SHA"
    exit 1
fi
if [ -n "$SHA256_EXPECTED" ]; then
    if [ "$ACTUAL" != "$SHA256_EXPECTED" ]; then
        echo >&2 "sha256 mismatch for dist.zip"
        echo >&2 "  expected: $SHA256_EXPECTED"
        echo >&2 "  actual:   $ACTUAL"
        exit 1
    fi
    echo "sha256 verified: $ACTUAL"
else
    echo "sha256 (unverified, paste into script):"
    echo "$ACTUAL"
fi

mkdir -p "$VENDOR_DIR"
# Extract only the runtime files; ignore source maps, the CSP build (covered
# by a future toggle), the .d.ts (we're not consuming types from Rust), and
# the package.json stub.
unzip -j -o "$TMP/dist.zip" \
    "dist/maplibre-gl.js" \
    "dist/maplibre-gl.css" \
    "dist/LICENSE.txt" \
    -d "$VENDOR_DIR"

# Rename the license so it's obvious which library it covers when the preview
# directory grows over time.
mv "$VENDOR_DIR/LICENSE.txt" "$VENDOR_DIR/LICENSE-maplibre-gl.txt"

# Record the pinned version next to the bundle so an operator inspecting the
# vendor directory at runtime can identify what's there without consulting
# this script.
cat > "$VENDOR_DIR/VERSION" <<EOF
maplibre-gl-js v${VERSION}
sha256: ${ACTUAL}
source: ${URL}
license: BSD-3-Clause (see LICENSE-maplibre-gl.txt)
EOF

echo
echo "Vendored to $VENDOR_DIR:"
ls -lh "$VENDOR_DIR"
