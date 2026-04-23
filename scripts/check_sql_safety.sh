#!/bin/sh
# check_sql_safety.sh — guard rail for engine-postgis.
#
# The engine-postgis crate MUST build every SQL statement with bound query
# parameters ($1, $2, ...) via tokio-postgres, and MUST route any identifier
# that can't be parameterized through `security::quote_ident`. This script
# fails CI if it ever finds SQL being assembled via `format!`, `concat!`, or
# `+ "..."` string concatenation, which are the common vectors for SQL
# injection through config-supplied table/column names.
#
# Wire into CI as a required check, for example in .github/workflows:
#   - name: SQL-safety guard
#     run: bash scripts/check_sql_safety.sh
#
# Exits 0 when the crate is clean, 1 when suspicious patterns are found,
# 2 when the target directory is missing. POSIX sh; uses BSD-compatible
# grep so it runs on macOS without GNU coreutils.

set -eu

CRATE_DIR="crates/engine-postgis/src"

if [ ! -d "$CRATE_DIR" ]; then
    echo "check_sql_safety: directory $CRATE_DIR not found" >&2
    exit 2
fi

found=0

# format!(...) / concat!(...) with a SQL verb inside the template.
if grep -rEn \
    '(format!|concat!)[[:space:]]*\([^)]*(SELECT|INSERT|UPDATE|DELETE|DROP)' \
    "$CRATE_DIR"
then
    echo >&2
    echo "check_sql_safety: format!/concat! used with SQL verbs." >&2
    echo "  Build queries with bound parameters (\$1, \$2, ...) instead," >&2
    echo "  and route identifiers through security::quote_ident." >&2
    found=1
fi

# String concatenation onto a SQL-verb literal: `foo + "SELECT ..."`.
if grep -rEn \
    '\+[[:space:]]*"[[:space:]]*(SELECT|INSERT|UPDATE|DELETE|DROP)' \
    "$CRATE_DIR"
then
    echo >&2
    echo "check_sql_safety: string concatenation with SQL verbs." >&2
    echo "  Build queries with bound parameters (\$1, \$2, ...) instead." >&2
    found=1
fi

# push_str(variable) — same injection class as format! when the variable
# carries config-supplied content. Static literals are safe; we only flag
# identifier-form args (bare name or `&name`). Individual call sites can
# opt out with a `// nosqlcheck` trailing comment — use sparingly and
# pair with a SAFETY block explaining why the variable is trusted.
if grep -rEn \
    '\.push_str[[:space:]]*\([[:space:]]*&?[a-z_][a-zA-Z0-9_]*[[:space:]]*\)' \
    "$CRATE_DIR" \
    | grep -v '// nosqlcheck$'
then
    echo >&2
    echo "check_sql_safety: push_str(variable) — inlining a string variable" >&2
    echo "  directly into SQL is an injection vector when the source is" >&2
    echo "  config or request data. Route identifiers through security::" >&2
    echo "  quote_ident and values through \$N bind parameters. If the" >&2
    echo "  variable is definitely-safe (whitelist-validated), refactor to" >&2
    echo "  inline the literal or document with a // SAFETY: comment." >&2
    found=1
fi

# Multi-line format!() with a SQL verb further down the macro body.
# grep is line-oriented and misses these; use perl in slurp mode.
# Falls back silently if perl is unavailable (only macOS/Linux primary
# CI jobs run this script, both have perl).
if command -v perl >/dev/null 2>&1; then
    if find "$CRATE_DIR" -name '*.rs' -print0 | \
        xargs -0 perl -0ne 'if (/(format!|concat!)\s*\([^)]*(SELECT|INSERT|UPDATE|DELETE|DROP)/) { print STDERR "$ARGV: multi-line format!/concat! with SQL verb\n"; exit 1 }' \
        2>/dev/null; then
        : # no matches
    else
        echo >&2
        echo "check_sql_safety: multi-line format!()/concat!() embedding a SQL" >&2
        echo "  verb. Split the SELECT keyword off the dynamic portion or" >&2
        echo "  build the statement with push_str + quote_ident." >&2
        found=1
    fi
fi

if [ "$found" -ne 0 ]; then
    exit 1
fi

echo "check_sql_safety: $CRATE_DIR clean"
exit 0
