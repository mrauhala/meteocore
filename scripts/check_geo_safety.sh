#!/bin/sh
# check_geo_safety.sh — guard rail for two workspace-wide invariants.
#
# 1. EPSG:3857 ↔ WGS84 math lives ONLY in `ds_core::web_mercator`.
#    Four hand-rolled copies once drifted and displaced rendered data
#    ~10° at low zoom (#452); the consolidation was #454, and two more
#    leftover copies were found later (#482). This script fails CI when
#    it finds the forward transform (ln∘tan / asinh∘tan), the inverse
#    (atan∘sinh / atan∘exp), or the Web Mercator magic constants
#    (20037508…, 85.0511…) anywhere outside crates/core.
#
# 2. api-wms builds all XML through quick-xml::Writer, never format!()
#    or string concatenation (XML injection risk).
#
# Wire into CI next to check_sql_safety.sh:
#   - name: Geo/XML safety guard
#     run: bash scripts/check_geo_safety.sh
#
# A definitely-legitimate line can opt out with a trailing `// nogeocheck`
# comment — use sparingly and say why.
#
# Exits 0 when clean, 1 when suspicious patterns are found, 2 when the
# source tree is missing. POSIX sh; BSD-compatible grep (runs on macOS).

set -eu

if [ ! -d "crates" ]; then
    echo "check_geo_safety: crates/ not found (run from the repo root)" >&2
    exit 2
fi

found=0

# All engine/api/render source, excluding crates/core (where web_mercator
# and geo legitimately implement the math).
GEO_DIRS=$(find crates -maxdepth 1 -mindepth 1 -type d ! -name core)

# Drop comment-only lines and explicit opt-outs from grep -rn output
# (file:line:content — filter on the content portion).
strip_noise() {
    grep -vE ':[0-9]+:[[:space:]]*//' | grep -v '// nogeocheck'
}

# --- 1a. Forward Web Mercator: ln(tan φ + sec φ) or asinh(tan φ) ----------
# shellcheck disable=SC2086
if grep -rnE '(tan\(.*\.ln\(\)|\.ln\(\).*tan\(|tan\(\)[[:space:]]*\.asinh\(|asinh\(.*tan)' \
    $GEO_DIRS --include='*.rs' | strip_noise
then
    echo >&2
    echo "check_geo_safety: hand-rolled forward Web Mercator transform." >&2
    echo "  Use ds_core::web_mercator::lat_to_y / lon_to_x instead (#452)." >&2
    found=1
fi

# --- 1b. Inverse Web Mercator: atan(sinh y) or atan(exp y) ----------------
# shellcheck disable=SC2086
if grep -rnE 'atan\(.*sinh\(|sinh\(.*atan\(|atan\(.*exp\(|exp\(.*atan\(' \
    $GEO_DIRS --include='*.rs' | strip_noise
then
    echo >&2
    echo "check_geo_safety: hand-rolled inverse Web Mercator transform." >&2
    echo "  Use ds_core::web_mercator::y_to_lat / x_to_lon instead (#452)." >&2
    found=1
fi

# --- 1c. Web Mercator magic constants -------------------------------------
# 20037508… (π·R, the world half-width) and 85.0511… (the tile-grid pole
# cutoff). Both have named homes: web_mercator::{EARTH_RADIUS, LAT_LIMIT_DEG}.
# shellcheck disable=SC2086
if grep -rnE '20037508|85\.0511' $GEO_DIRS --include='*.rs' | strip_noise
then
    echo >&2
    echo "check_geo_safety: Web Mercator magic constant outside crates/core." >&2
    echo "  Use ds_core::web_mercator::{EARTH_RADIUS, LAT_LIMIT_DEG} (#454)." >&2
    found=1
fi

# --- 2. XML assembled with format!() in api-wms ----------------------------
# All XML output goes through quick-xml::Writer for escaping. A format!()
# whose template opens an XML tag is the injection vector.
if grep -rnE 'format!\([[:space:]]*"[^"]*<[A-Za-z/]' \
    crates/api-wms/src --include='*.rs' | strip_noise
then
    echo >&2
    echo "check_geo_safety: format!() assembling XML in api-wms." >&2
    echo "  All XML must go through quick-xml::Writer (injection risk)." >&2
    found=1
fi

if [ "$found" -ne 0 ]; then
    exit 1
fi

echo "check_geo_safety: clean"
exit 0
