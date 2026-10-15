#!/usr/bin/env bash
# Write THIRD-PARTY-NOTICES.txt for the server binary (#1081).
#
# The licences of everything compiled into the server, generated from
# Cargo.lock at build time rather than committed, so a dependency bump cannot
# leave them stale. The Dockerfile copies the result into the image
# (/usr/share/doc/meteocore/) and .github/workflows/build-binaries.yml into
# every release archive.
#
#   1. Rust crates: cargo-about, configured by about.toml and rendered with
#      third-party/notices.hbs.
#   2. C libraries that crates compile from bundled sources. cargo-about only
#      sees the licence a crate declares for its Rust code, so the upstream
#      library's own licence file is listed in bundled_licences below.
#   3. NOTICE files of the crates (Apache-2.0 section 4d).
#   4. Components vendored in this repository: ds-bufr (tinybufr), Swagger UI,
#      MapLibre GL JS, the Natural Earth land outlines and the ColorBrewer
#      colour scheme of the `diverging` palette.
#
# Needs cargo-about 0.9.2 (`cargo install --locked cargo-about --version
# 0.9.2 --features cli`) and network access for `cargo fetch`.
#
# Usage: scripts/third_party_notices.sh OUTPUT
set -euo pipefail

out="${1:?usage: third_party_notices.sh OUTPUT}"
case "$out" in
  /*) ;;
  *) out="$PWD/$out" ;;
esac
cd "$(dirname "$0")/.."

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# bundled_licences CRATE prints the licence files of the C libraries CRATE
# compiles from sources it bundles, one glob per line, relative to the
# crate's source directory; a path starting with `@` is relative to this
# repository instead. It prints nothing for a crate whose native code is
# only its own (covered by section 1), and fails for a crate it does not
# know. Every crate that builds with `cc` or `cmake` must be listed, so a new
# one stops the build until someone has looked at what it compiles.
bundled_licences() {
  case "$1" in
    # AWS-LC and the BoringSSL, OpenSSL and third_party code it contains.
    aws-lc-sys) echo 'LICENSE' ;;
    # The crate ships none of C-Blosc's licence files.
    blosc-src) echo '@third-party/c-blosc.txt' ;;
    libaec-sys) echo 'libaec-*/LICENSE.txt' ;;
    libwebp-sys) printf '%s\n' 'vendor/COPYING' 'vendor/PATENTS' ;;
    # blosc-src enables the `static` feature: the bundled zlib, not zlib-ng.
    libz-sys) echo 'src/zlib/LICENSE' ;;
    lz4-sys) echo 'liblz4/lib/LICENSE' ;;
    snappy_src) echo 'snappy/COPYING' ;;
    tikv-jemalloc-sys) echo 'jemalloc/COPYING' ;;
    zstd-sys) echo 'zstd/LICENSE' ;;
    # Native code of their own, under the crate's licence: ring's BoringSSL
    # licences are among the crate's licence files, which section 1 lists.
    link-cplusplus | ring) ;;
    *) return 1 ;;
  esac
}

rule() {
  printf '%s\n' '--------------------------------------------------------------------------------'
}

section() {
  printf '\n%s\n%s\n%s\n' \
    '================================================================================' \
    "$1" \
    '================================================================================'
}

entry() {
  printf '\n'
  rule
  printf '%s\n' "$1"
  rule
  printf '\n'
}

# Print a file, ending it with a newline when it has none.
include() {
  cat "$1"
  if [ -n "$(tail -c 1 "$1")" ]; then
    printf '\n'
  fi
}

# A crate's source directory as a path this shell can use (Git Bash on the
# Windows release runner gets `C:\...` from cargo metadata).
crate_dir() {
  local dir="${1%Cargo.toml}"
  dir="${dir%[/\\]}"
  if command -v cygpath > /dev/null 2>&1; then
    dir="$(cygpath -u "$dir")"
  fi
  printf '%s\n' "$dir"
}

# cargo-about reads each crate's licence files from the local registry, for
# every release target, so fetch them all first. Generation then runs with
# --frozen: offline, against the committed Cargo.lock, and without fetching
# licence texts from the crates' git repositories.
cargo fetch --locked

# The features the Docker image and the release archives are built with.
if ! cargo about --color never generate --frozen --fail \
  --manifest-path crates/server/Cargo.toml --features icechunk \
  --output-file "$work/about.txt" third-party/notices.hbs 2> "$work/about.log"; then
  cat "$work/about.log" >&2
  echo "third_party_notices.sh: cargo about generate failed" >&2
  exit 1
fi
# cargo-about only warns about a crate entry in about.toml that is no longer
# in the dependency graph. Fail instead, so the entry, and any
# acknowledgement made for it, is removed with the dependency.
if grep -Eiq 'warn|error' "$work/about.log"; then
  cat "$work/about.log" >&2
  echo "third_party_notices.sh: cargo-about reported a problem; fix about.toml" >&2
  exit 1
fi

# LF line endings throughout, whatever the checkout did to the template.
tr -d '\r' < "$work/about.txt" > "$work/about.lf"
grep '^@@crate ' "$work/about.lf" > "$work/crates" || true
grep '^@@native ' "$work/about.lf" | awk '!seen[$0]++' > "$work/native" || true
if [ ! -s "$work/crates" ]; then
  echo "third_party_notices.sh: no crates in the cargo-about output" >&2
  exit 1
fi

# Every crate that builds native code must be known to bundled_licences.
while read -r _ name; do
  if ! bundled_licences "$name" > /dev/null; then
    echo "third_party_notices.sh: crate '$name' builds native code with cc or cmake." >&2
    echo "  Find the licences of the C/C++ sources it compiles and list them in" >&2
    echo "  bundled_licences (scripts/third_party_notices.sh)." >&2
    exit 1
  fi
done < "$work/native"

# third-party/c-blosc.txt is copied from one C-Blosc release. Fail when
# blosc-src bundles another, so the copy is refreshed along with it.
while read -r _ name _ manifest; do
  [ "$name" = blosc-src ] || continue
  grep -qx '@@native blosc-src' "$work/native" || continue
  header="$(crate_dir "$manifest")/c-blosc/blosc/blosc.h"
  bundled="$(sed -n 's/^#define BLOSC_VERSION_STRING *"\([^"]*\)".*/\1/p' "$header" 2> /dev/null || true)"
  kept="$(tr -d '\r' < third-party/c-blosc.txt | sed -n '1s/^C-Blosc \([^ ,]*\).*/\1/p')"
  if [ -z "$bundled" ] || [ "$bundled" != "$kept" ]; then
    echo "third_party_notices.sh: blosc-src bundles C-Blosc '${bundled:-unknown}'," >&2
    echo "  but third-party/c-blosc.txt is for '${kept:-unknown}'. Copy LICENSE.txt and" >&2
    echo "  the LICENSES files from that C-Blosc release into it." >&2
    exit 1
  fi
done < "$work/crates"

{
  # Section 1 without the @@ lines and without licence blocks that list no
  # crate. A block runs from `@@licence` to the next one; its crates are the
  # indented lines between its first two rules.
  awk '
    function flush() { if (block != "" && crates > 0) printf "%s", block; block = ""; crates = 0 }
    /^@@licence$/ { flush(); inblock = 1; rules = 0; next }
    /^@@/ { next }
    inblock {
      block = block $0 "\n"
      if ($0 ~ /^-+$/) rules++
      else if (rules == 1 && $0 ~ /^  /) crates++
      next
    }
    { print }
    END { flush() }
  ' "$work/about.lf"

  section "2. C libraries compiled from sources bundled in those crates"
  printf '\n%s\n' "Each crate below compiles a C library from sources it bundles. Section 1"
  printf '%s\n' "lists the crate's own licence; these are the library's."
  while read -r _ name version manifest; do
    grep -qx "@@native $name" "$work/native" || continue
    dir="$(crate_dir "$manifest")"
    while read -r pattern; do
      [ -n "$pattern" ] || continue
      if [ "${pattern#@}" != "$pattern" ]; then
        files=("${pattern#@}")
      else
        shopt -s nullglob
        # shellcheck disable=SC2206 # the pattern is a glob on purpose
        files=("$dir"/$pattern)
        shopt -u nullglob
      fi
      if [ "${#files[@]}" -eq 0 ] || [ ! -f "${files[0]}" ]; then
        echo "third_party_notices.sh: $name $version has no '$pattern' under $dir;" >&2
        echo "  update bundled_licences (scripts/third_party_notices.sh)." >&2
        exit 1
      fi
      for f in "${files[@]}"; do
        if [ "${pattern#@}" != "$pattern" ]; then
          entry "$name $version: $f, kept in the MeteoCore repository"
        else
          entry "$name $version: ${f#"$dir"/}"
        fi
        include "$f"
      done
    done < <(bundled_licences "$name")
  done < "$work/crates"

  section "3. NOTICE files of those crates"
  while read -r _ name version manifest; do
    dir="$(crate_dir "$manifest")"
    for f in "$dir"/NOTICE "$dir"/NOTICE.*; do
      [ -f "$f" ] || continue
      entry "$name $version: ${f##*/}"
      include "$f"
    done
  done < "$work/crates"

  section "4. Components vendored in the MeteoCore repository"
  entry "ds-bufr: a BUFR decoder derived from tinybufr 0.1.3 (MIT OR Apache-2.0)"
  include crates/ds-bufr/LICENSE-MIT.txt
  printf '\n'
  include crates/ds-bufr/LICENSE-APACHE.txt
  for dir in crates/core/assets/swagger-ui-*; do
    entry "Swagger UI ${dir##*-} (Apache-2.0), used by the API documentation pages"
    include "$dir/NOTICE"
    printf '\n'
    include "$dir/LICENSE"
    entry "Third-party code bundled in Swagger UI's swagger-ui-bundle.js"
    include "$dir/swagger-ui-bundle.js.LICENSE.txt"
  done
  entry "MapLibre GL JS (BSD-3-Clause), served at /preview/vendor/"
  include crates/server/preview/vendor/VERSION
  printf '\n'
  include crates/server/preview/vendor/LICENSE-maplibre-gl.txt
  entry "Natural Earth country outlines (public domain), served at /preview/vendor/"
  include crates/server/preview/vendor/LICENSE-workbench-land.txt
  entry "ColorBrewer RdBu colour scheme (Apache-2.0), the built-in diverging palette"
  include third-party/colorbrewer.txt
} > "$work/notices.txt"

mv "$work/notices.txt" "$out"
echo "Wrote $out ($(wc -l < "$out" | tr -d ' ') lines)" >&2
