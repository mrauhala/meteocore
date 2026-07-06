#!/usr/bin/env python3
"""Claude Code PostToolUse hook (matcher: Edit|Write).

Per-file guards that run right after an edit lands, instead of waiting
for CI:

- An edit under crates/engine-postgis/ re-runs scripts/check_sql_safety.sh
  (the CI SQL-injection tripwire); a failure is fed back to the model as a
  blocking reason so it fixes the pattern immediately.
- An edit to crates/api-edr/src/response.rs injects a reminder that all
  CoverageJSON output must validate against the OGC schema
  (`cargo test -p api-edr`, CLAUDE.md Critical Rule 12).

Wired up in .claude/settings.json. Exits 0 with no output when nothing
applies.
"""

import json
import subprocess
import sys


def main() -> None:
    try:
        data = json.load(sys.stdin)
    except Exception:
        return

    tool_input = data.get("tool_input") or {}
    tool_response = data.get("tool_response") or {}
    path = tool_input.get("file_path") or tool_response.get("filePath") or ""

    if "crates/engine-postgis/" in path and path.endswith(".rs"):
        result = subprocess.run(
            ["sh", "scripts/check_sql_safety.sh"],
            capture_output=True,
            text=True,
        )
        if result.returncode == 1:
            output = (result.stdout + result.stderr)[-2000:]
            print(
                json.dumps(
                    {
                        "decision": "block",
                        "reason": (
                            "scripts/check_sql_safety.sh failed after this "
                            "edit - fix the flagged SQL pattern before "
                            "continuing:\n" + output
                        ),
                    }
                )
            )
        elif result.returncode != 0:
            # Exit 2 = the script couldn't find its target directory (wrong
            # cwd / moved tree) - a hook-config problem, not a violation.
            # Warn without blocking.
            print(
                json.dumps(
                    {
                        "systemMessage": (
                            "post-edit-checks: check_sql_safety.sh exited "
                            f"{result.returncode} (config problem, not a "
                            "violation) - run it manually from the repo root."
                        )
                    }
                )
            )
        return

    if path.endswith("crates/api-edr/src/response.rs"):
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "PostToolUse",
                        "additionalContext": (
                            "response.rs (CoverageJSON output) was modified. "
                            "All CoverageJSON must validate against the OGC "
                            "schema - run `cargo test -p api-edr` before "
                            "finishing (CLAUDE.md Critical Rule 12)."
                        ),
                    }
                }
            )
        )


if __name__ == "__main__":
    main()
