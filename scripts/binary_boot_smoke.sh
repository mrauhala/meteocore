#!/usr/bin/env bash
# Boot-smoke a built MeteoCore server binary: start it with a minimal
# config and require an HTTP answer from /health within 60 s.
#
# The release binaries counterpart of docker_boot_smoke.sh. Linking proves
# nothing about exec: a missing shared library, an invalid code signature
# (the kernel kills unsigned arm64 macOS binaries) or a macOS minimum newer
# than the host all surface only when the binary runs. Runs on Linux,
# macOS and Git Bash on Windows.
#
# Usage: binary_boot_smoke.sh <path-to-server-binary>
set -euo pipefail

BIN="${1:?usage: binary_boot_smoke.sh <path-to-server-binary>}"
PORT="${SMOKE_PORT:-18000}"

# Minimal valid config: zero collections, no collections_dir.
CFG_DIR="$(mktemp -d)"
printf '[server]\nhost = "127.0.0.1"\nport = %s\n' "$PORT" > "$CFG_DIR/config.toml"
LOG="$CFG_DIR/server.log"

"$BIN" --config "$CFG_DIR/config.toml" > "$LOG" 2>&1 &
PID=$!

cleanup() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  echo "--- server log ---"
  cat "$LOG" || true
  rm -rf "$CFG_DIR"
}
trap cleanup EXIT

for _ in $(seq 1 30); do
  # Any HTTP status counts: the gate asserts "binary execs and serves
  # HTTP", not collection health.
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${PORT}/health" || true)
  if [ -n "$code" ] && [ "$code" != "000" ]; then
    echo "boot smoke OK: /health answered HTTP $code"
    exit 0
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "boot smoke FAILED: server exited before serving HTTP" >&2
    exit 1
  fi
  sleep 2
done

echo "boot smoke FAILED: no HTTP answer from /health within 60 s" >&2
exit 1
