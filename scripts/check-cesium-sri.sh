#!/usr/bin/env bash
# Verify the Subresource Integrity (SRI) hashes pinned in the 3D Tiles viewer
# match the live CesiumJS CDN. Re-run after bumping the CesiumJS version in
# crates/api-3dtiles/viewer/viewer.js, which inserts CesiumJS (a wrong hash
# makes browsers refuse to load CesiumJS → the viewer's fallback panel instead
# of a 3D view). Requires network + openssl.
#
#   scripts/check-cesium-sri.sh
set -euo pipefail

src="$(git rev-parse --show-toplevel)/crates/api-3dtiles/viewer/viewer.js"
ver="$(grep -oE 'cesiumjs/releases/[0-9.]+' "$src" | head -1 | grep -oE '[0-9.]+')"
base="https://cesium.com/downloads/cesiumjs/releases/${ver}/Build/Cesium"

# Pinned hashes, in file order (1 = Cesium.js, 2 = widgets.css). `sed -n Np`
# instead of `mapfile` so this runs on the bash 3.2 macOS ships.
pinned_js="$(grep -oE 'sha384-[A-Za-z0-9+/=]+' "$src" | sed -n '1p')"
pinned_css="$(grep -oE 'sha384-[A-Za-z0-9+/=]+' "$src" | sed -n '2p')"

check() {
  url="$1"; name="$2"; want="$3"
  got="sha384-$(curl -fsSL "$url" | openssl dgst -sha384 -binary | openssl base64 -A)"
  if [ "$want" = "$got" ]; then
    echo "OK    $name"
  else
    echo "FAIL  $name"
    echo "  pinned: $want"
    echo "  live:   $got"
    exit 1
  fi
}

echo "Checking CesiumJS $ver SRI hashes…"
check "${base}/Cesium.js" "Cesium.js" "$pinned_js"
check "${base}/Widgets/widgets.css" "widgets.css" "$pinned_css"
echo "All SRI hashes match."
