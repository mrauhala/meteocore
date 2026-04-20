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

if [ "$found" -ne 0 ]; then
    exit 1
fi

echo "check_sql_safety: $CRATE_DIR clean"
exit 0
