#!/bin/zsh
# lint.sh <worktree-dir> <crate> [crate ...]
#
# `cargo fmt`, then `cargo clippy --all-targets -- -D warnings` for the named
# crates (all targets, so tests compile). No tests run: CI runs the suite, and
# on macOS every freshly linked test binary is scanned, which makes local runs
# slow. Run NEW tests that pin reference values yourself before pushing.
#
# Every worktree shares the main checkout's target dir, so cargo runs
# serialize on its lock: run lints one after another, not in parallel.
#
# Prints "LINT OK <name>" or "LINT FAILED <name>" plus the first diagnostics.
set -u
wt=${1:A}; shift
main=$(cd "$(git -C "$wt" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$main/target}
name=${wt:t}
cd "$wt" || exit 1
cargo fmt || { echo "FMT FAILED $name"; exit 1; }
pkgs=()
for crate in "$@"; do pkgs+=(-p "$crate"); done
out=$(cargo clippy "${pkgs[@]}" --all-targets -- -D warnings 2>&1)
rc=$?
diag=$(echo "$out" | grep -E "^(error|warning)" -A9 | head -40)
# A clippy that never finished (toolchain, ICE, dependency fetch) exits
# non-zero without an error/warning line: fail on the status too.
if [ $rc -ne 0 ] || [ -n "$diag" ]; then
  echo "LINT FAILED $name (clippy exit $rc)"
  echo "${diag:-$(echo "$out" | tail -20)}"
  exit 1
fi
echo "LINT OK $name"
