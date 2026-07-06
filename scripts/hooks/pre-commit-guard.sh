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

# Detect an actual `git commit` INVOCATION, not the substring: `git` must sit
# at a command position (start of line, or after ;, &, |, (, backtick, or a
# newline), optionally preceded by NAME=value env assignments, followed only
# by flag tokens (each may take one non-flag argument: -C <dir>, -c k=v)
# before the `commit` subcommand (optionally quoted). Heredoc bodies are
# stripped first so a data line reading "git commit" (e.g. writing an example
# script) is not treated as an invocation. This avoids denying
# `git log --grep="git commit"` and heredoc data while still catching
# `git add x && git commit`, `git  commit`, `git "commit"`,
# `git -C /tmp commit`, `git -c user.email=x commit`, and `V=1 git commit`.
# A variable-built command can still evade this — it is a guardrail against
# habitual mistakes, not a security boundary.
is_commit=$(printf '%s' "$input" | python3 -c '
import json, re, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
cmd = (data.get("tool_input") or {}).get("command", "")
# Strip heredoc bodies (<<EOF … EOF, <<-EOF, quoted delimiters).
cmd = re.sub(
    r"<<-?\s*([\"\x27]?)(\w+)\1.*?\n\s*\2\s*(?=\n|$)",
    "<<HEREDOC_STRIPPED",
    cmd,
    flags=re.S,
)
pat = re.compile(
    r"(?:^|[;&|(`\n])\s*"                    # command position
    r"(?:[A-Za-z_][A-Za-z0-9_]*=\S*\s+)*"    # env-var prefixes
    r"git\s+"
    r"(?:-\S+(?:\s+[^-\s]\S*)?\s+)*"         # flags, each w/ optional argument
    r"[\"\x27]?commit\b"
)
print("commit" if pat.search(cmd) else "")
' 2>/dev/null) || exit 0

[ "$is_commit" = "commit" ] || exit 0

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
