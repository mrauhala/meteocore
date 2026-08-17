#!/usr/bin/env bash
# Boot-smoke a built MeteoCore image: start it with a minimal config and
# require an HTTP answer from /health within 60 s.
#
# A plain `docker build` proves the binary links at BUILD time, but says
# nothing about the RUNTIME stage — a builder/runtime glibc mismatch, a
# missing shared library, or a broken CMD all surface only on exec. That
# exact class shipped to production on 2026-08-17 ("GLIBC_2.38 not
# found", fixed in #582): the image built green everywhere and died on
# start. This gate runs in every CI Docker job so it cannot recur.
#
# Usage: docker_boot_smoke.sh <image-ref>
set -euo pipefail

IMG="${1:?usage: docker_boot_smoke.sh <image-ref>}"
NAME="mc-boot-smoke-$$"
PORT="${SMOKE_PORT:-18000}"

# Minimal valid config: zero collections, no collections_dir. Mounted over
# the image's /data/config.toml so the gate is independent of whatever the
# repo config.toml references.
CFG_DIR="$(mktemp -d)"
printf '[server]\nhost = "0.0.0.0"\nport = 8000\n' > "$CFG_DIR/config.toml"
chmod 644 "$CFG_DIR/config.toml"

cleanup() {
  echo "--- container logs ---"
  docker logs "$NAME" 2>&1 || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$CFG_DIR"
}
trap cleanup EXIT

docker run -d --name "$NAME" -p "127.0.0.1:${PORT}:8000" \
  -v "$CFG_DIR/config.toml:/data/config.toml:ro" "$IMG" >/dev/null

for _ in $(seq 1 30); do
  # Any HTTP status counts: the gate asserts "binary execs and serves
  # HTTP", not collection health.
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${PORT}/health" || true)
  if [ -n "$code" ] && [ "$code" != "000" ]; then
    echo "boot smoke OK: /health answered HTTP $code"
    exit 0
  fi
  if [ "$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null)" != "true" ]; then
    echo "boot smoke FAILED: container exited before serving HTTP" >&2
    exit 1
  fi
  sleep 2
done

echo "boot smoke FAILED: no HTTP answer from /health within 60 s" >&2
exit 1
