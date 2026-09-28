#!/usr/bin/env python3
"""nested_parens.py <file> [...]: fail when a line nests parentheses.

A PR body becomes the squash-commit body, and the release-notes parser drops
a commit whose body nests parentheses, e.g. "(see (a) and (b))". Exit 1 and
list the offending lines; exit 0 when every file is clean.
"""
import sys

bad = False
for name in sys.argv[1:]:
    with open(name, encoding="utf-8") as fh:
        for number, line in enumerate(fh, 1):
            depth = deepest = 0
            for ch in line:
                if ch == "(":
                    depth += 1
                    deepest = max(deepest, depth)
                elif ch == ")":
                    depth = max(0, depth - 1)
            if deepest >= 2:
                bad = True
                print(f"{name}:{number}: nested parentheses: {line.strip()[:120]}")
sys.exit(1 if bad else 0)
