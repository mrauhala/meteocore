#!/bin/sh
# Claude Code PreToolUse hook (matcher: Bash). Guards `git commit`:
#
#   1. Blocks committing directly on `main` (CLAUDE.md Critical Rule 1 —
#      always branch + PR).
#   2. Blocks committing with formatting drift (Critical Rule 2 — CI runs
#      `cargo fmt -- --check` and would reject the push anyway; failing
#      here saves the round-trip).
#
# Reads the tool-call JSON on stdin; exits silently (allow) for anything
# that is not a `git commit`. Emits a PreToolUse permissionDecision JSON
# to deny, so the reason is fed back to the model.
#
# Wired up in .claude/settings.json. POSIX sh; python3 for JSON parsing.

set -u

input=$(cat)

cmd=$(printf '%s' "$input" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
print((data.get("tool_input") or {}).get("command", ""))
' 2>/dev/null) || exit 0

case "$cmd" in
*"git commit"*) ;;
*) exit 0 ;;
esac

deny() {
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$1"
    exit 0
}

branch=$(git branch --show-current 2>/dev/null || true)
if [ "$branch" = "main" ]; then
    deny "Blocked: committing directly to main violates CLAUDE.md Critical Rule 1 - create a branch first (git checkout -b <branch>) and open a PR."
fi

if ! cargo fmt --check >/dev/null 2>&1; then
    deny "Blocked: cargo fmt --check failed - run cargo fmt before committing (CLAUDE.md Critical Rule 2; CI rejects formatting drift)."
fi

exit 0
