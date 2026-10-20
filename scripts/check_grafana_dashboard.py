#!/usr/bin/env python3
"""check_grafana_dashboard.py — every /metrics family is on the dashboard.

Root CLAUDE.md (Admin & Operations) asks for a Grafana panel for every new
/metrics family. This guard enforces that against the Rust sources, and
lints the dashboard JSON so a broken panel fails CI instead of reading
"No data" in Grafana.

Families are read from the Rust sources: every non-test file under src/ of
every crate that depends on `prometheus`. A family is registered by
  - a prometheus constructor with a string-literal name: IntGauge::new,
    Gauge::new, IntCounter::new, Counter::new, Opts::new,
    HistogramOpts::new, opts!, histogram_opts!, register_*!;
  - a call with a literal to a helper that hands one of its own parameters
    on as the name (int_gauge, DeltaCounter::new, the `mk` closures, ...).
    Helpers are found, not listed: a new one needs no change here;
  - a call to a builder in TEMPLATES, which derives several names from one
    prefix with format! (the cache metric sets).
A name the scan cannot resolve fails the check instead of being skipped.

Each dashboard (default: docker/grafana/dashboards/*.json) is a Grafana 13
dashboard.grafana.app/v2 resource: panels are `spec.elements`, and
`spec.layout` places them in tabs, rows and grids. Each must
  - query every family from a panel, by name or by a __name__ matcher;
  - name only registered families in PromQL, with variables expanded;
  - be apiVersion dashboard.grafana.app/v2, kind Dashboard, with the uid as
    metadata.name and tabs at the top of the layout;
  - place every element exactly once, reference no missing element, keep
    grid items inside 24 columns without overlaps, and keep rows open;
  - name elements panel-<id>, with unique integer ids;
  - reference datasources only through datasource variables of the query's
    type, define every variable it uses, and contain no host name or
    absolute URL;
  - give every panel a title and a description, and every stat, gauge, bar
    gauge and time series a unit; sort table legends by a calc they show;
    link only to its own panels, tabs and dashboards;
  - match Grafana's own schema for the v2 spec (SCHEMA, vendored from the
    Grafana release): required fields present, enums and kinds valid, and
    no field the schema lacks, which Grafana would drop silently.

Usage, from anywhere in the repository:
    python3 scripts/check_grafana_dashboard.py [--list-families] [DASHBOARD.json ...]

Exits 0 when clean, 1 on any problem, and 2 when the tree is missing.
Standard library only, Python 3.10+. Tests: scripts/tests/.
"""

from __future__ import annotations

import argparse
import hashlib
import itertools
import json
import re
import sys
from bisect import bisect_left
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DASHBOARD_GLOB = "docker/grafana/dashboards/*.json"
RULE = ("Every /metrics family needs a dashboard panel, and every metric a "
        "panel queries must be a family (root CLAUDE.md, Admin & Operations).")

# ---------------------------------------------------------------------------
# Families, from the Rust sources
# ---------------------------------------------------------------------------

# Builders that derive several family names from one literal prefix with
# format!. They are too dynamic to read generically, so they are declared:
# `families` maps each suffix after "<prefix>_" to the index of the boolean
# argument that gates it (None: always registered). `parts` is every
# name-like string literal in the builder's body. When the body changes, the
# check fails until `families` has been reviewed and `parts` updated.
TEMPLATES: dict[str, dict] = {
    "CacheMetricSet::new": {
        "parts": {"{prefix}_hits_total", "{prefix}_misses_total", "{prefix}_bytes",
                  "{prefix}_capacity_bytes", "{prefix}_entries"},
        "families": {"hits_total": None, "misses_total": None, "bytes": None,
                     "capacity_bytes": None, "entries": 4},
    },
    "CollectionCacheMetricSet::new": {
        "parts": {"hits", "misses", "bytes", "capacity_bytes", "collection",
                  "{prefix}_{suffix}_total", "{prefix}_{suffix}"},
        "families": {"hits_total": None, "misses_total": None, "bytes": None,
                     "capacity_bytes": None},
    },
}

# prometheus constructors whose first argument is the metric name.
CONSTRUCTOR = re.compile(
    r"(?<!\w)((?:IntGauge|Gauge|IntCounter|Counter|HistogramOpts|Opts)::new"
    r"|(?:histogram_)?opts!|register_\w+!)\s*\(")
PROMETHEUS_DEP = re.compile(
    r"(?m)^\s*(?:prometheus\s*[.=]|\[[\w.-]*dependencies\.prometheus\])")
METRIC_NAME = re.compile(r"[a-zA-Z_:][a-zA-Z0-9_:]*")
NAME_PART = re.compile(r"[a-z0-9_{}]+")

_LEX = re.compile(r'//|/\*|(?<!\w)b?r#*"|"|\'')
_CHAR = re.compile(r"'(?:[^'\\\n]|\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.))'")
_BRACKET = re.compile(r"[()\[\]{}]")
_CFG_TEST = re.compile(r"#\[cfg\(test\)\]")
_ATTR = re.compile(r"\s*#\[")
_MOD_DECL = re.compile(r"\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")
_IMPL = re.compile(
    r"(?m)^[ \t]*(?:unsafe\s+)?impl\b(?:\s*<[^{;]*?>)?\s+"
    r"(?:[\w:]+(?:<[^{;]*?>)?\s+for\s+)?(?:\w+::)*(\w+)[^{;]*\{")
_FN = re.compile(r"\bfn\s+(\w+)\s*(?:<[^{;]*?>)?\s*\(")
_CLOSURE = re.compile(
    r"\blet\s+(?:mut\s+)?(\w+)\s*(?::[^=;]*)?=\s*(?:move\s+)?\|([^|]*)\|")
_CHAIN = re.compile(r"\s*\.\s*(\w+)\s*\(")
_WRAPPER = re.compile(r"([\w:]+!?)\s*\(\s*$")
_LITERAL = re.compile(r'"((?:[^"\\]|\\.)*)"')
_FORMAT = re.compile(r"&?\s*format!\s*\(")
_IDENT = re.compile(
    r"&?\s*(\w+)(?:\.(?:as_str|as_ref|to_string|to_owned|clone|into)\(\))?")
_NESTED_OPTS = re.compile(r"(?:prometheus::)?(?:(?:histogram_)?opts!|(?:Histogram)?Opts::new)\s*\(")


def kind_of(token: str) -> str | None:
    token = token.lower()
    return next((k for k in ("histogram", "counter", "gauge") if k in token), None)


def _blank(buf: list[str], a: int, b: int) -> None:
    for k in range(a, b):
        if buf[k] != "\n":
            buf[k] = " "


def lex(src: str) -> tuple[str, str, list[tuple[int, int]]]:
    """Return (code, skel, strings) for Rust source. Comments are blanked in
    both texts, string and char literal contents in `skel` as well; offsets
    and newlines are kept. `strings` holds each string literal's content span."""
    code, skel, strings = list(src), list(src), []
    i, n = 0, len(src)
    while m := _LEX.search(src, i):
        tok, s = m.group(0), m.start()
        if tok == "//":
            e = src.find("\n", s)
            e = n if e < 0 else e
            _blank(code, s, e)
            _blank(skel, s, e)
        elif tok == "/*":
            depth, e = 1, s + 2
            while depth and e < n:
                step = src[e:e + 2]
                depth += {"/*": 1, "*/": -1}.get(step, 0)
                e += 2 if step in ("/*", "*/") else 1
            _blank(code, s, e)
            _blank(skel, s, e)
        elif tok == "'":
            c = _CHAR.match(src, s)
            e = c.end() if c else s + 1        # otherwise a lifetime or a label
            if c:
                _blank(skel, s + 1, e - 1)
        elif "r" in tok:                        # raw string
            close = '"' + "#" * tok.count("#")
            e = src.find(close, m.end())
            e = n if e < 0 else e
            strings.append((m.end(), e))
            _blank(skel, m.end(), e)
            e += len(close)
        else:
            e = m.end()
            while e < n and src[e] != '"':
                e += 2 if src[e] == "\\" else 1
            strings.append((m.end(), e))
            _blank(skel, m.end(), e)
            e += 1
        i = e
    return "".join(code), "".join(skel), strings


def _pairs(skel: str) -> dict[int, int]:
    pairs, stack = {}, []
    for m in _BRACKET.finditer(skel):
        if m.group(0) in "([{":
            stack.append(m.start())
        elif stack:
            pairs[stack.pop()] = m.start()
    return pairs


def _split_params(text: str) -> list[str]:
    parts, depth, start = [], 0, 0
    for i, c in enumerate(text.replace("->", "  ")):
        if c in "([{<":
            depth += 1
        elif c in ")]}>":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append(text[start:i])
            start = i + 1
    return parts + [text[start:]]


def param_names(text: str) -> list[str | None]:
    """Parameter names as call sites pass them (a receiver is not one)."""
    names: list[str | None] = []
    for part in (p.strip() for p in _split_params(text)):
        if not part or re.fullmatch(r"&?\s*(?:'\w+\s+)?(?:mut\s+)?self\b.*", part, re.S):
            continue
        m = re.match(r"(?:mut\s+)?(\w+)\s*(?::|$)", part)
        names.append(m.group(1) if m else None)
    return names


@dataclass
class Def:
    """A named function or closure."""
    name: str                       # "int_gauge", "DeltaCounter::new", or a closure's name
    params: list[str | None]
    body: tuple[int, int]
    scope: tuple[int, int] | None   # closures: the block they are visible in
    file: "RustFile"
    start: int


@dataclass
class Family:
    name: str
    kind: str                       # counter | gauge | histogram
    where: str                      # file:line of the registration


class RustFile:
    def __init__(self, path: str, src: str):
        self.path = path
        self.code, self.skel, self.strings = lex(src)
        self.test_mods: list[str] = []
        self._cut_tests()
        self.newlines = [m.start() for m in re.finditer("\n", src)]
        self.blocks = sorted((o, c) for o, c in self.pairs.items() if self.skel[o] == "{")
        self.impls = [(m.group(1), m.end() - 1, self.pairs.get(m.end() - 1, len(self.skel)))
                      for m in _IMPL.finditer(self.skel)]
        self.defs = self._defs()

    def _cut_tests(self) -> None:
        """Blank #[cfg(test)] items; remember #[cfg(test)] mod x; files."""
        self.pairs = _pairs(self.skel)
        cuts = []
        for m in _CFG_TEST.finditer(self.skel):
            j = m.end()
            while a := _ATTR.match(self.skel, j):
                j = self.pairs.get(a.end() - 1, a.end()) + 1
            if d := _MOD_DECL.match(self.skel, j):
                self.test_mods.append(d.group(1))
                cuts.append((m.start(), d.end()))
                continue
            k = self.skip_to(j, "{;")
            end = self.skel[k:k + 1]       # the item's block, its ";", or its parent's closing bracket
            cuts.append((m.start(), self.pairs.get(k, k) + 1 if end == "{" else
                         k if end in (")", "]", "}") else k + 1))
        if cuts:
            code, skel = list(self.code), list(self.skel)
            for a, b in cuts:
                _blank(code, a, b)
                _blank(skel, a, b)
            self.code, self.skel = "".join(code), "".join(skel)
            self.strings = [s for s in self.strings if not any(a <= s[0] < b for a, b in cuts)]
            self.pairs = _pairs(self.skel)

    def skip_to(self, i: int, stops: str) -> int:
        """First position from i holding one of `stops` outside brackets, or
        the closing bracket that ends the enclosing expression."""
        n = len(self.skel)
        while i < n:
            c = self.skel[i]
            if c in stops:
                return i
            if c in "([{":
                i = self.pairs.get(i, i)
            elif c in ")]}":
                return i
            i += 1
        return n

    def line(self, pos: int) -> int:
        return bisect_left(self.newlines, pos) + 1

    def block_at(self, pos: int) -> tuple[int, int] | None:
        """The innermost {} block around pos."""
        for o, c in reversed(self.blocks[:bisect_left(self.blocks, (pos, -1))]):
            if c > pos:
                return o, c
        return None

    def _defs(self) -> list[Def]:
        defs = []
        for m in _FN.finditer(self.skel):
            op = m.end() - 1
            k = self.skip_to(self.pairs.get(op, op) + 1, "{;")
            if self.skel[k:k + 1] != "{":
                continue                       # a declaration without a body
            name = m.group(1)
            block = self.block_at(m.start())
            owner = next((t for t, o, c in self.impls if block == (o, c)), None)
            defs.append(Def(f"{owner}::{name}" if owner else name,
                            param_names(self.code[op + 1:self.pairs.get(op, op)]),
                            (k, self.pairs.get(k, len(self.skel))), None, self, m.start()))
        for m in _CLOSURE.finditer(self.skel):
            k = m.end()
            while self.skel[k:k + 1].isspace():
                k += 1
            # An expression body runs from the closing | to the end of the expression.
            body = ((k, self.pairs.get(k, k)) if self.skel[k:k + 1] == "{"
                    else (m.end() - 1, self.skip_to(k, ";,")))
            block = self.block_at(m.start())
            defs.append(Def(m.group(1), param_names(self.code[m.start(2):m.end(2)]), body,
                            (m.start(), block[1] if block else len(self.skel)), self, m.start()))
        return defs

    def enclosing(self, pos: int) -> Def | None:
        inside = [d for d in self.defs if d.body[0] < pos < d.body[1]]
        return min(inside, key=lambda d: d.body[1] - d.body[0], default=None)

    def args_at(self, open_paren: int) -> list[str]:
        close = self.pairs.get(open_paren)
        if close is None:
            return []
        out, depth, start = [], 0, open_paren + 1
        for m in re.compile(r"[()\[\]{},]").finditer(self.skel, open_paren + 1, close):
            c = m.group(0)
            if c in "([{":
                depth += 1
            elif c in ")]}":
                depth -= 1
            elif depth == 0:
                out.append(self.code[start:m.start()].strip())
                start = m.end()
        tail = self.code[start:close].strip()
        return out + [tail] if tail else out

    def wrapper(self, pos: int) -> str:
        m = _WRAPPER.search(self.skel, max(0, pos - 120), pos)
        return m.group(1) if m else ""

    def chain(self, close: int) -> list[str]:
        names, i = [], close + 1
        while m := _CHAIN.match(self.skel, i):
            names.append(m.group(1))
            i = self.pairs.get(m.end() - 1, m.end()) + 1
        return names


def calls_of(d: Def, files: list[RustFile]):
    """(file, match) for every call of `d`; the match ends at its "("."""
    if d.scope is not None:
        pat = re.compile(rf"(?<![\w.:]){re.escape(d.name)}\s*\(")
        yield from ((d.file, m) for m in pat.finditer(d.file.skel, *d.scope))
        return
    if "::" in d.name:
        owner, short = d.name.split("::")
        pat = re.compile(rf"(?<!\w){re.escape(d.name)}\s*\(")
        own = re.compile(rf"(?<!\w)Self::{re.escape(short)}\s*\(")
        for f in files:
            yield from ((f, m) for m in pat.finditer(f.skel))
            for t, o, c in f.impls:
                if t == owner:
                    yield from ((f, m) for m in own.finditer(f.skel, o, c))
        return
    pat = re.compile(rf"(?<![\w.])(?<!fn ){re.escape(d.name)}\s*\(")
    for f in files:
        yield from ((f, m) for m in pat.finditer(f.skel))


def extract_families(files: list[RustFile], templates: dict | None = None
                     ) -> tuple[dict[str, Family], list[str]]:
    """Every family the sources register, and what could not be read."""
    templates = TEMPLATES if templates is None else templates
    families: dict[str, Family] = {}
    problems: list[str] = []

    def add(name: str, kind: str | None, f: RustFile, pos: int) -> None:
        where = f"{f.path}:{f.line(pos)}"
        if not METRIC_NAME.fullmatch(name):
            problems.append(f"{where}: '{name}' is not a valid metric name")
            return
        families.setdefault(name, Family(
            name, kind or ("counter" if name.endswith("_total") else "gauge"), where))

    for d in (d for f in files for d in f.defs if d.name in templates):
        spec, f = templates[d.name], d.file
        parts = {f.code[a:b] for a, b in f.strings
                 if d.body[0] < a < d.body[1] and NAME_PART.fullmatch(f.code[a:b])}
        if parts != spec["parts"]:
            changed = sorted(f"+{p}" for p in parts - spec["parts"]) + \
                sorted(f"-{p}" for p in spec["parts"] - parts)
            problems.append(f"{f.path}:{f.line(d.start)}: {d.name} changed the names it builds "
                            f"({', '.join(changed)}): review its entry in TEMPLATES in "
                            "scripts/check_grafana_dashboard.py")
        for g, m in calls_of(d, files):
            where = f"{g.path}:{g.line(m.start())}"
            args = g.args_at(m.end() - 1)
            prefix = _LITERAL.fullmatch(args[0]) if args else None
            if not prefix:
                problems.append(f"{where}: {d.name} needs a string-literal prefix")
                continue
            for suffix, gate in spec["families"].items():
                flag = "true" if gate is None else (args[gate] if gate < len(args) else "")
                if flag not in ("true", "false"):
                    problems.append(f"{where}: argument {gate} of {d.name} must be a literal "
                                    "true or false")
                elif flag == "true":
                    add(f"{prefix.group(1)}_{suffix}", None, g, m.start())

    work = []           # (file, call start, "(" position, callee, kind, name argument index)
    for f in files:
        if "Registry::new_custom" in f.skel:
            problems.append(f"{f.path}: Registry::new_custom prefixes every name, which this "
                            "check does not read")
        for m in CONSTRUCTOR.finditer(f.skel):
            tok, op = m.group(1), m.end() - 1
            work.append((f, m.start(), op, tok, kind_of(tok) or kind_of(f.wrapper(m.start())), 0))
            if tok.endswith("Opts::new") and {"namespace", "subsystem"} & set(f.chain(f.pairs.get(op, op))):
                problems.append(f"{f.path}:{f.line(m.start())}: a namespace or subsystem prefixes "
                                "the name, which this check does not read")
    seen: set[tuple[str, int]] = set()
    while work:
        f, start, op, callee, kind, index = work.pop()
        encl = f.enclosing(start)
        if encl is not None and encl.name in templates:
            continue
        args = f.args_at(op)
        arg = args[index] if index < len(args) else ""
        ident = _IDENT.fullmatch(arg)
        if lit := _LITERAL.fullmatch(arg):
            add(lit.group(1), kind, f, start)
        elif callee.endswith("!") and _NESTED_OPTS.match(arg):
            continue                            # the nested Opts is a call of its own
        elif ident and encl is not None and ident.group(1) in encl.params:
            if (encl.file.path, encl.start) not in seen:   # encl is a helper: read its callers
                seen.add((encl.file.path, encl.start))
                index = encl.params.index(ident.group(1))
                work += [(g, m.start(), m.end() - 1, encl.name, kind, index)
                         for g, m in calls_of(encl, files)]
        else:
            where = f"{f.path}:{f.line(start)}"
            if _FORMAT.match(arg):
                problems.append(f"{where}: {callee} gets a name built with format!: add "
                                f"{encl.name if encl else 'the builder'} to TEMPLATES in "
                                "scripts/check_grafana_dashboard.py")
            else:
                problems.append(f"{where}: cannot read the metric name `{' '.join(arg.split())}` "
                                f"passed to {callee}: use a string literal, or a parameter of a "
                                "helper whose callers pass literals")
    return families, problems


def rust_sources(root: Path) -> list[RustFile]:
    """Every non-test src/ file of the crates that depend on prometheus."""
    lexed: list[tuple[Path, RustFile]] = []
    for toml in sorted(root.glob("crates/*/Cargo.toml")):
        if not PROMETHEUS_DEP.search(toml.read_text(encoding="utf-8")):
            continue
        for p in sorted((toml.parent / "src").rglob("*.rs")):
            lexed.append((p, RustFile(p.relative_to(root).as_posix(),
                                      p.read_text(encoding="utf-8"))))
    excluded: list[Path] = []
    for p, f in lexed:
        base = p.parent if p.name in ("lib.rs", "main.rs", "mod.rs") else p.parent / p.stem
        excluded += [base / m for m in f.test_mods] + [base / f"{m}.rs" for m in f.test_mods]
    return [f for p, f in lexed
            if not any(p == x or x in p.parents for x in excluded)]


# ---------------------------------------------------------------------------
# Grafana's dashboard v2 schema, read from its CUE source
# ---------------------------------------------------------------------------

# Vendored verbatim; `git hash-object` of the file must equal SCHEMA_BLOB. To
# move to a newer Grafana, fetch the same path at the new tag over it and
# update both constants.
SCHEMA = REPO / "scripts" / "grafana" / "dashboard_v2_spec.cue"
SCHEMA_SOURCE = "grafana/grafana v13.1.8 apps/dashboard/pkg/apis/dashboard/v2/dashboard_spec.cue"
SCHEMA_BLOB = "2ba90cd56bf57cb8752a3f0487aa8cdd819a3e4c"

_CUE_TOKEN = re.compile(r'''(?P<ws>[ \t\r]+) | (?P<nl>\n) | (?P<comment>//[^\n]*)
    | (?P<attr>@\w+\() | (?P<str>"(?:[^"\\\n]|\\.)*") | (?P<num>-?\d+(?:\.\d+)?)
    | (?P<ellipsis>\.\.\.) | (?P<ident>[A-Za-z_$][\w$]*) | (?P<punct>[{}\[\]():,|&*?.])
    | (?P<bad>.)''', re.X)
_CUE_TYPES = {"string": str, "bool": bool, "number": float, "float": float, "float32": float,
              "float64": float, "int": int, "int8": int, "int16": int, "int32": int,
              "int64": int, "uint": int, "uint8": int, "uint16": int, "uint32": int,
              "uint64": int, "bytes": str}
_INT_RANGE = {"int8": 8, "int16": 16, "int32": 32, "int64": 64}


class CueSchema:
    """The subset of CUE Grafana's generated dashboard schema is written in:
    struct definitions, optional fields, defaults, disjunctions, unification
    with literals, lists, open structs and pattern fields.

    A value is checked the way Grafana's Go types decode it: a field the
    schema does not mark optional must be present, and a field it does not
    declare is an error, because Grafana drops it silently (provisioning
    saves with field validation off). Structs with `...` or a pattern field
    take any key."""

    def __init__(self, src: str):
        self.toks = self._lex(src)
        self.i = 0
        self.defs: dict[str, tuple] = {}
        if self._next() != ("ident", "package"):
            raise ValueError("expected a package clause")
        self._next()
        self._commas()
        while self.toks[self.i][0] != "eof":
            label, pattern, _, expr = self._field()
            if label is None:
                raise ValueError("a pattern field at the top level")
            self.defs[label] = expr
            self._commas()
        missing = sorted({n for e in self.defs.values() for n in self._refs(e)} - set(self.defs))
        if missing:
            raise ValueError(f"undefined: {', '.join(missing)}")

    # -- parsing ------------------------------------------------------------

    @staticmethod
    def _lex(src: str) -> list[tuple[str, str]]:
        out: list[tuple[str, str]] = []
        it = _CUE_TOKEN.finditer(src)
        for m in it:
            kind, text = m.lastgroup, m.group()
            if kind in ("ws", "comment"):
                continue
            if kind == "nl":                        # CUE inserts a comma like Go a semicolon
                if out and (out[-1][0] in ("ident", "str", "num", "ellipsis")
                            or out[-1][1] in (")", "]", "}")):
                    out.append(("punct", ","))
                continue
            if kind == "attr":                      # @cog(...): skip to its closing paren
                depth = 1
                for a in it:
                    if a.lastgroup == "punct" and a.group() in "()":
                        depth += 1 if a.group() == "(" else -1
                        if not depth:
                            break
                continue
            if kind == "bad":
                raise ValueError(f"unexpected {text!r} at offset {m.start()}")
            out.append((kind, text))
        return out + [("eof", "")]

    def _next(self) -> tuple[str, str]:
        tok = self.toks[self.i]
        self.i += 1
        return tok

    def _accept(self, text: str) -> bool:
        if self.toks[self.i][1] == text and self.toks[self.i][0] != "str":
            self.i += 1
            return True
        return False

    def _expect(self, text: str) -> None:
        if not self._accept(text):
            raise ValueError(f"expected {text!r}, found {self.toks[self.i][1]!r} "
                             f"(token {self.i})")

    def _commas(self) -> None:
        while self._accept(","):
            pass

    def _field_starts(self) -> bool:
        """A label followed by `:` (or `?:`) starts a field, also as a value:
        `a: [string]: T` is `a: {[string]: T}`."""
        j = self.i
        kind, text = self.toks[j]
        if kind in ("ident", "str"):
            j += 1
        elif text == "[":
            depth = 0
            while True:
                t = self.toks[j][1]
                depth += (t == "[") - (t == "]")
                j += 1
                if not depth or self.toks[j - 1][0] == "eof":
                    break
        else:
            return False
        if self.toks[j][1] == "?":
            j += 1
        return self.toks[j][1] == ":" and self.toks[j][0] == "punct"

    def _field(self):
        kind, text = self._next()
        label, pattern = None, None
        if kind == "ident":
            label = text
        elif kind == "str":
            label = json.loads(text)
        elif text == "[":
            pattern = self._expr()
            self._expect("]")
        else:
            raise ValueError(f"expected a field, found {text!r}")
        optional = self._accept("?")
        self._expect(":")
        if self._field_starts():
            sub_label, sub_pattern, sub_opt, sub_expr = self._field()
            expr = ("struct", {} if sub_label is None else {sub_label: (sub_opt, sub_expr)},
                    [] if sub_pattern is None else [(sub_pattern, sub_expr)], False)
        else:
            expr = self._expr()
        return label, pattern, optional, expr

    def _expr(self):
        alts = [self._conj()]
        while self._accept("|"):
            alts.append(self._conj())
        return alts[0] if len(alts) == 1 else ("or", alts)

    def _conj(self):
        parts = [self._unary()]
        while self._accept("&"):
            parts.append(self._unary())
        return parts[0] if len(parts) == 1 else ("and", parts)

    def _unary(self):
        if self._accept("*"):
            return ("default", self._unary())
        return self._primary()

    def _primary(self):
        kind, text = self._next()
        if kind == "str":
            return ("lit", json.loads(text))
        if kind == "num":
            return ("lit", float(text) if "." in text else int(text))
        if kind == "ident":
            if text in ("true", "false"):
                return ("lit", text == "true")
            if text == "null":
                return ("lit", None)
            if text == "_":
                return ("top",)
            if self._accept("."):
                return ("select", text, self._next()[1])
            return ("type", text) if text in _CUE_TYPES else ("ref", text)
        if text == "(":
            expr = self._expr()
            self._expect(")")
            return expr
        if text == "{":
            fields, patterns, is_open = {}, [], False
            self._commas()
            while not self._accept("}"):
                if self._accept("..."):
                    is_open = True
                else:
                    label, pattern, optional, expr = self._field()
                    if pattern is None:
                        fields[label] = (optional, expr)
                    else:
                        patterns.append((pattern, expr))
                self._commas()
            return ("struct", fields, patterns, is_open)
        if text == "[":
            elems, rest = [], None
            while not self._accept("]"):
                if self._accept("..."):
                    rest = ("top",) if self.toks[self.i][1] in ("]", ",") else self._expr()
                else:
                    elems.append(self._expr())
                self._accept(",")
            return ("list", elems, rest)
        raise ValueError(f"unexpected {text!r} (token {self.i - 1})")

    def _refs(self, node):
        if not isinstance(node, tuple):
            return
        if node[0] in ("ref", "select"):
            yield node[1]
        for part in node[1:]:
            if isinstance(part, tuple):
                yield from self._refs(part)
            elif isinstance(part, list):
                for p in part:
                    yield from self._refs(p)
            elif isinstance(part, dict):
                for _, e in part.values():
                    yield from self._refs(e)

    # -- checking -----------------------------------------------------------

    def _resolve(self, node):
        while node[0] in ("ref", "default"):
            node = self.defs[node[1]] if node[0] == "ref" else node[1]
        if node[0] == "and":
            structs = [p for p in map(self._resolve, node[1]) if p[0] == "struct"]
            if len(structs) == len(node[1]):
                return ("struct", {k: v for s in structs for k, v in s[1].items()},
                        [p for s in structs for p in s[2]], any(s[3] for s in structs))
        return node

    def _literal(self, node):
        node = self._resolve(node)
        if node[0] == "lit":
            return node[1]
        if node[0] == "and":
            return next((n[1] for n in map(self._resolve, node[1]) if n[0] == "lit"), None)
        return None

    def _means(self, value, node) -> bool:
        """Whether `value` names this branch by a literal-valued field (kind, type)."""
        node = self._resolve(node)
        if node[0] != "struct" or not isinstance(value, dict):
            return False
        tags = {k: lit for k, (_, e) in node[1].items()
                if isinstance(lit := self._literal(e), str)}
        return bool(tags) and all(value.get(k) == v for k, v in tags.items())

    def _alternatives(self, node):
        """The branches of a disjunction, nested ones flattened, defaults unmarked."""
        for branch in node[1]:
            while branch[0] == "default":
                branch = branch[1]
            target = self.defs[branch[1]] if branch[0] == "ref" else branch
            if target[0] == "or":
                yield from self._alternatives(target)
            else:
                yield branch

    def _fits(self, value, node) -> bool:
        """Whether `value` has the shape of this branch, before any detail."""
        node = self._resolve(node)
        kind = node[0]
        if kind == "struct":
            return isinstance(value, dict) and (node[2] or node[3]
                                                or set(value) <= set(node[1]))
        if kind == "list":
            return isinstance(value, list) and (node[2] is not None
                                                or len(value) == len(node[1]))
        if kind == "lit":
            want = node[1]
            return (value is None if want is None else isinstance(value, bool)
                    if isinstance(want, bool) else isinstance(value, type(want)))
        if kind == "type":
            return not self.check(value, node)
        if kind == "and":
            return all(self._fits(value, p) for p in node[1])
        return True

    def _name(self, node) -> str:
        if node[0] in ("ref", "type"):
            return node[1]
        if node[0] == "default":
            return self._name(node[1])
        if node[0] == "lit":
            return json.dumps(node[1])
        return {"struct": "an object", "list": "a list", "top": "anything"}.get(node[0], node[0])

    @staticmethod
    def _short(value) -> str:
        text = json.dumps(value, ensure_ascii=False)
        return text if len(text) <= 40 else text[:37] + "..."

    def check(self, value, node=("ref", "DashboardSpec"), path: str = "spec") -> list[str]:
        errs: list[str] = []
        self._check(value, node, path, errs)
        return errs

    def _check(self, value, node, path, errs) -> None:
        kind = node[0]
        if kind in ("ref", "default"):
            self._check(value, self.defs[node[1]] if kind == "ref" else node[1], path, errs)
        elif kind in ("top", "select"):
            pass
        elif kind == "lit":
            want = node[1]
            same = (value is None if want is None
                    else type(value) is type(want) and value == want if isinstance(want, (bool, str))
                    else not isinstance(value, bool) and isinstance(value, (int, float))
                    and value == want)
            if not same:
                errs.append(f"{path}: {self._short(value)} is not {json.dumps(want)}")
        elif kind == "type":
            want = _CUE_TYPES[node[1]]
            ok = (isinstance(value, str) if want is str
                  else isinstance(value, bool) if want is bool
                  else isinstance(value, int) and not isinstance(value, bool) if want is int
                  else isinstance(value, (int, float)) and not isinstance(value, bool))
            if ok and want is int:
                bits = _INT_RANGE.get(node[1].lstrip("u"), 64)
                lo, hi = ((0, 2 ** bits - 1) if node[1].startswith("u")
                          else (-2 ** (bits - 1), 2 ** (bits - 1) - 1))
                ok = lo <= value <= hi
            if not ok:
                errs.append(f"{path}: {self._short(value)} is not {node[1]}")
        elif kind == "and":
            merged = self._resolve(node)
            if merged[0] == "struct":
                self._check(value, merged, path, errs)
            else:
                for part in node[1]:
                    self._check(value, part, path, errs)
        elif kind == "or":
            branches = list(self._alternatives(node))
            tried = []
            for branch in branches:
                sub: list[str] = []
                self._check(value, branch, path, sub)
                if not sub:
                    return
                tried.append((branch, sub))
            # Report inside the branch the value is evidently meant to be:
            # the only one of its shape, or the one its kind/type names.
            fits = [(b, sub) for b, sub in tried if self._fits(value, b)]
            meant = [(b, sub) for b, sub in fits if self._means(value, b)]
            pick = meant if len(meant) == 1 else fits if len(fits) == 1 else []
            if pick:
                errs.extend(pick[0][1])
            else:
                errs.append(f"{path}: {self._short(value)} is none of "
                            + " | ".join(self._name(b) for b in branches))
        elif kind == "list":
            _, elems, rest = node
            if not isinstance(value, list):
                errs.append(f"{path}: {self._short(value)} is not a list")
                return
            if rest is None and len(value) != len(elems):
                errs.append(f"{path}: expected {len(elems)} items, found {len(value)}")
            for i, item in enumerate(value):
                sub_node = elems[i] if i < len(elems) else rest
                if sub_node is not None:
                    self._check(item, sub_node, f"{path}[{i}]", errs)
        elif kind == "struct":
            _, fields, patterns, is_open = node
            if not isinstance(value, dict):
                errs.append(f"{path}: {self._short(value)} is not an object")
                return
            for name, (optional, sub_node) in fields.items():
                if name in value:
                    self._check(value[name], sub_node, f"{path}.{name}", errs)
                elif not optional:
                    errs.append(f"{path}: missing field '{name}'")
            for key, item in value.items():
                if key in fields:
                    continue
                for _, sub_node in patterns:
                    self._check(item, sub_node, f"{path}.{key}", errs)
                if not patterns and not is_open:
                    errs.append(f"{path}: field '{key}' is not in the schema, so Grafana "
                                "drops it")
        else:
            raise ValueError(f"unknown schema node {kind}")


def git_blob_sha(data: bytes) -> str:
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def load_schema() -> tuple[CueSchema | None, str | None]:
    """The vendored schema, or why it cannot be used."""
    shown = SCHEMA.relative_to(REPO) if SCHEMA.is_relative_to(REPO) else SCHEMA
    try:
        data = SCHEMA.read_bytes()
    except OSError as e:
        return None, f"{shown}: cannot read the Grafana schema: {e}"
    if git_blob_sha(data) != SCHEMA_BLOB:
        return None, (f"{shown}: differs from {SCHEMA_SOURCE}: vendor the upstream file "
                      "unchanged and update SCHEMA_BLOB")
    try:
        return CueSchema(data.decode("utf-8")), None
    except ValueError as e:
        return None, f"{shown}: cannot read the schema: {e}"


# ---------------------------------------------------------------------------
# Dashboard checks
# ---------------------------------------------------------------------------

API_VERSION = "dashboard.grafana.app/v2"
MAX_NESTING = 3                 # tabs and rows inside each other, as Grafana 13 offers
UID_RE = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?")
VARIABLE_KINDS = {"QueryVariable": "query", "DatasourceVariable": "datasource",
                  "CustomVariable": "custom", "ConstantVariable": "constant",
                  "TextVariable": "textbox", "IntervalVariable": "interval",
                  "AdhocVariable": "adhoc", "GroupByVariable": "groupby",
                  "SwitchVariable": "switch"}
BUILTIN_VARS = {"__rate_interval", "__interval", "__interval_ms", "__range", "__range_s",
                "__range_ms", "__auto", "__all_variables", "__url_time_range", "__field",
                "__value", "__series", "__data", "__from", "__to", "__dashboard", "__org",
                "__user", "__timeFilter", "__timezone", "__name"}
PROMETHEUS_SERIES = {"up", "scrape_duration_seconds", "scrape_samples_scraped",
                     "scrape_samples_post_metric_relabeling", "scrape_series_added"}
PROMQL_WORDS = {"by", "without", "on", "ignoring", "group_left", "group_right", "offset",
                "bool", "and", "or", "unless", "inf", "nan", "start", "end", "sum", "min",
                "max", "avg", "group", "stddev", "stdvar", "count", "count_values", "bottomk",
                "topk", "quantile", "limitk", "limit_ratio", "atan2"}
SPECIAL_DATASOURCES = {"-- Mixed --", "-- Dashboard --", "-- Grafana --", "grafana"}
UNIT_PANELS = {"timeseries", "stat", "bargauge", "gauge"}
QUERYLESS_PANELS = {"text", "news", "dashlist"}
CALC_NAMES = {"mean": "Mean", "max": "Max", "min": "Min", "lastNotNull": "Last *",
              "last": "Last", "sum": "Total"}
VAR_RE = re.compile(r"\$\{([A-Za-z_]\w*)(?:[.:][^}]*)?\}|\$([A-Za-z_]\w*)"
                    r"|\[\[([A-Za-z_]\w*)(?::[^\]]*)?\]\]")
URL_RE = re.compile(r"(?i)\b[a-z][a-z0-9+.-]*://")
IP_RE = re.compile(r"(?i)\blocalhost\b|\b\d{1,3}(?:\.\d{1,3}){3}\b")
HOST_RE = re.compile(r"(?i)\b(?:[a-z0-9-]+\.)+(?:fi|com|net|org|io|no|eu|app|de|se|dk|ee|is"
                     r"|cloud|dev|local|internal)\b")
STRING_RE = re.compile(r'"(?:[^"\\]|\\.)*"|\'(?:[^\'\\]|\\.)*\'|`[^`]*`')
TAB_PARAM_RE = re.compile(r"(?:^|[?&])((?:[^&=?#]+-)?dtab)=([^&#]*)")


# What may take a selector of several metric names as its argument: an
# aggregation, label_replace keeping the name in a label, or Grafana's
# label_values() series lookup. Anything else
# (rate, increase, arithmetic, ...) drops __name__ and Prometheus rejects the
# result with "vector cannot contain metrics with the same labelset" as soon
# as two of the names share their labels.
NAME_SAFE_OPEN_RE = re.compile(
    r"(?:\b(?:sum|count|min|max|avg|group|stddev|stdvar)\s*"
    r"(?:(?:by|without)\s*\([^()]*\)\s*)?|\blabel_replace\s*|\blabel_values\s*)\(\s*$")


def bare_name_selectors(expr: str) -> list[tuple[str, str]]:
    """The __name__ matchers (op, value) of selectors that a name-dropping
    function or operator receives directly."""
    bare = []
    for m in re.finditer(r"\{([^{}]*)\}", expr):
        prefix = expr[:m.start()]
        if re.search(r"[A-Za-z_:][\w:]*\s*$", prefix):
            continue                      # a named selector: one metric name
        for op, val in re.findall(r'__name__\s*(=~|!~|!=)\s*"((?:[^"\\]|\\.)*)"', m.group(1)):
            if not NAME_SAFE_OPEN_RE.search(prefix):
                bare.append((op, val))
    return bare


def promql_names(expr: str) -> tuple[list[str], list[tuple[str, str]]]:
    """The metric names and the __name__ matchers of a PromQL expression
    (or of a Grafana label_values() variable query)."""
    if lv := re.fullmatch(r"\s*label_values\((.*),\s*[A-Za-z_]\w*\s*\)\s*", expr, re.S):
        expr = lv.group(1)
    elif re.fullmatch(r"\s*label_values\(\s*[A-Za-z_]\w*\s*\)\s*", expr):
        return [], []
    matchers = [(op, val) for body in re.findall(r"\{([^{}]*)\}", expr)
                for op, val in re.findall(r'__name__\s*(=~|!~|!=|=)\s*"((?:[^"\\]|\\.)*)"', body)]
    s = STRING_RE.sub('""', expr)
    s = re.sub(r"\{[^{}]*\}", " ", s)                                    # label matchers
    s = re.sub(r"\[[^\]]*\]", " ", s)                                    # ranges, subqueries
    s = re.sub(r"\b(by|without|on|ignoring|group_left|group_right)\s*\([^)]*\)", r" \1 ", s)
    s = re.sub(r"\b\d+(?:\.\d+)?(?:ms|s|m|h|d|w|y)\b", " ", s)          # durations
    s = re.sub(r"\b\d+(?:\.\d+)?(?:[eE][-+]?\d+)?\b", " ", s)            # numbers
    names = [m.group(0) for m in re.finditer(r"[A-Za-z_:][\w:]*", s)
             if m.group(0) not in PROMQL_WORDS and not s[m.end():].lstrip().startswith("(")]
    return names, matchers


def grafana_regex(text: str) -> re.Pattern | None:
    if not text:
        return None
    m = re.fullmatch(r"/(.*)/([a-z]*)", text, re.S)
    pattern, flags = (m.group(1), m.group(2)) if m else (text, "")
    return re.compile(pattern, re.I if "i" in flags else 0)


def tab_slugs(title: str) -> set[str]:
    """The URL forms Grafana accepts for a tab: its title with spaces as
    dashes, and the lowercase kbn.slugifyForUrl form."""
    legacy = re.sub(r" +", "-", re.sub(r"[^A-Za-z0-9_ ]+", "", title.lower()))
    return {re.sub(r" +", "-", title), legacy}


def _strings(obj):
    if isinstance(obj, dict):
        for v in obj.values():
            yield from _strings(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from _strings(v)
    elif isinstance(obj, str):
        yield obj


def _nodes(obj):
    if isinstance(obj, dict):
        yield obj
        for v in obj.values():
            yield from _nodes(v)
    elif isinstance(obj, list):
        for v in obj:
            yield from _nodes(v)


def _get(obj, *keys, default=None):
    for k in keys:
        if not isinstance(obj, dict) or k not in obj:
            return default
        obj = obj[k]
    return obj


@dataclass
class Placement:
    where: str                      # e.g. tab "Engines" › tab "Radar"
    tabs: tuple[str, ...]           # tab titles from the top
    repeat: str | None              # the variable the item repeats over


class DashboardCheck:
    """One dashboard.grafana.app/v2 resource."""

    def __init__(self, label: str, doc, families: dict[str, Family], uids: set[str],
                 schema: CueSchema | None = None):
        self.label, self.doc, self.families, self.uids = label, doc, families, uids
        self.schema = schema
        self.problems: list[str] = []
        self.queried: set[str] = set()
        self.spec = _get(doc, "spec", default={})
        self.spec = self.spec if isinstance(self.spec, dict) else {}
        elements = self.spec.get("elements")
        self.elements: dict = elements if isinstance(elements, dict) else {}
        self.variables = {_get(v, "spec", "name"): v for v in self.spec.get("variables") or []
                          if isinstance(_get(v, "spec", "name"), str)}
        self.annotations = [a for a in self.spec.get("annotations") or [] if isinstance(a, dict)]
        self.name_values: dict[str, list[str]] = {}
        self.placements: dict[str, list[Placement]] = {}
        self.tabs: dict[tuple[str, ...], list[str]] = {}    # tab path -> its sub-tab titles
        self.rows = 0
        self.ids = {_get(e, "spec", "id") for e in self.elements.values()}

    def err(self, where: str, msg: str) -> None:
        self.problems.append(f"{self.label}: {where}: {msg}")

    # -- reading the resource ---------------------------------------------------

    def panels(self):
        """(element name, panel spec) for every Panel element."""
        for name, el in self.elements.items():
            if isinstance(el, dict) and el.get("kind") == "Panel" and isinstance(el.get("spec"), dict):
                yield name, el["spec"]

    @staticmethod
    def where(p: dict) -> str:
        return f"panel {p.get('id')} \"{p.get('title', '')}\""

    @staticmethod
    def panel_type(p: dict):
        return _get(p, "vizConfig", "group")

    @staticmethod
    def queries(p: dict) -> list[dict]:
        """The PanelQuery specs of a panel."""
        return [q["spec"] for q in _get(p, "data", "spec", "queries", default=[]) or []
                if isinstance(q, dict) and isinstance(q.get("spec"), dict)]

    def var_type(self, name: str) -> str | None:
        return VARIABLE_KINDS.get((self.variables.get(name) or {}).get("kind"))

    def var_query(self, name: str) -> str | None:
        """The query string of a query variable."""
        spec = _get(self.variables.get(name), "spec", "query", "spec", default={})
        if isinstance(spec, dict):
            q = spec.get("__legacyStringValue") or spec.get("query") or spec.get("expr")
            return q if isinstance(q, str) else None
        return None

    def ds_kind(self, query) -> str | None:
        """The datasource type a DataQuery runs on, through its datasource variable."""
        name = _get(query, "datasource", "name")
        if isinstance(name, str) and (m := re.fullmatch(r"\$\{?(\w+)\}?", name)):
            if self.var_type(m.group(1)) == "datasource":
                return _get(self.variables[m.group(1)], "spec", "pluginId")
        return None

    def sections(self):
        """(where, object) for every part of the resource."""
        for name, el in self.elements.items():
            spec = el.get("spec") if isinstance(el, dict) else None
            yield (self.where(spec) if isinstance(spec, dict) else f"element {name}"), el
        for name, v in self.variables.items():
            yield f"variable ${name}", v
        for a in self.annotations:
            yield f"annotation \"{_get(a, 'spec', 'name')}\"", a
        yield "layout", self.spec.get("layout")
        yield "dashboard", {"metadata": self.doc.get("metadata"),
                            **{k: v for k, v in self.spec.items()
                               if k not in ("elements", "variables", "annotations", "layout")}}

    def run(self) -> bool:
        """Check everything; False when this is not a v2 resource at all."""
        if not self.check_resource():
            return False
        self.check_layout()
        self.check_elements()
        self.check_datasources()
        self.check_queries()
        for where, obj in self.sections():
            self.check_strings(where, obj)
        self.check_panels()
        self.check_schema()
        return True

    # -- the resource and its layout ---------------------------------------------

    def check_resource(self) -> bool:
        doc = self.doc
        if not isinstance(doc, dict):
            self.err("dashboard", "is not a JSON object")
            return False
        if "apiVersion" not in doc and "panels" in doc:
            self.err("dashboard", f"is classic dashboard JSON: write it as a {API_VERSION} "
                     "resource (apiVersion, kind, metadata, spec), which Grafana 13 provisions")
            return False
        if doc.get("apiVersion") != API_VERSION:
            self.err("dashboard", f"apiVersion is {json.dumps(doc.get('apiVersion'))}, not "
                     f"\"{API_VERSION}\": Grafana reads a v2 spec only under that version")
        if doc.get("kind") != "Dashboard":
            self.err("dashboard", f"kind is {json.dumps(doc.get('kind'))}, not \"Dashboard\"")
        name = _get(doc, "metadata", "name")
        if not isinstance(name, str) or not UID_RE.fullmatch(name):
            self.err("dashboard", f"metadata.name {json.dumps(name)} must be the dashboard uid: "
                     "1-40 lowercase letters, digits and dashes")
        if not isinstance(doc.get("spec"), dict):
            self.err("dashboard", "has no spec object")
            return False
        if not self.spec.get("title"):
            self.err("dashboard", "has no title")
        if _get(self.spec, "layout", "kind") != "TabsLayout":
            self.err("layout", f"is a {_get(self.spec, 'layout', 'kind')}: the top level must "
                     "be a TabsLayout, so the dashboard opens on tabs")
        return True

    def check_layout(self) -> None:
        self.walk(self.spec.get("layout"), "layout", (), 0, None)
        for name in self.elements:
            n = len(self.placements.get(name, []))
            if n == 0:
                self.err(f"element {name}", "is not placed in the layout")
            elif n > 1:
                self.err(f"element {name}", f"is placed {n} times: "
                         + ", ".join(p.where for p in self.placements[name]))

    def place(self, ref, where: str, tabs, repeat) -> None:
        name = ref.get("name") if isinstance(ref, dict) else None
        if not isinstance(name, str) or ref.get("kind") != "ElementReference":
            self.err(where, "has an item without an ElementReference")
        elif name not in self.elements:
            self.err(where, f"references element '{name}', which does not exist")
        else:
            self.placements.setdefault(name, []).append(Placement(where, tabs, repeat))

    def walk(self, layout, where: str, tabs: tuple[str, ...], depth: int, row: str | None) -> None:
        kind, spec = _get(layout, "kind"), _get(layout, "spec", default={})
        if depth > MAX_NESTING:
            self.err(where, f"nests tabs and rows {depth} deep; Grafana offers {MAX_NESTING}")
        if kind == "TabsLayout":
            titles = []
            for tab in spec.get("tabs") or []:
                title = _get(tab, "spec", "title")
                here = f"{where} › tab \"{title}\"" if tabs or row else f"tab \"{title}\""
                if not title:
                    self.err(here, "has no title")
                elif any(tab_slugs(title) & tab_slugs(t) for t in titles):
                    self.err(here, "has the URL slug of an earlier tab: links cannot tell "
                             "them apart")
                titles.append(title or "")
                self.walk(_get(tab, "spec", "layout"), here, tabs + (title or "",), depth + 1, None)
            self.tabs[tabs] = titles
        elif kind == "RowsLayout":
            for r in spec.get("rows") or []:
                rs = r.get("spec") or {}
                here = f"{where} › row \"{rs.get('title', '')}\""
                self.rows += 1
                if not rs.get("title") and not rs.get("hideHeader"):
                    self.err(here, "has no title")
                if rs.get("collapse"):
                    self.err(here, "is collapsed: sections stay open (collapse: false)")
                self.walk(rs.get("layout"), here, tabs, depth + 1, rs.get("title") or "")
        elif kind == "GridLayout":
            boxes = []
            for item in spec.get("items") or []:
                s = item.get("spec") or {}
                ref = s.get("element") or {}
                here = f"{where} › {ref.get('name')}"
                self.place(ref, here, tabs, _get(s, "repeat", "value"))
                box = [s.get(k) for k in ("x", "y", "width", "height")]
                if not all(isinstance(v, int) and not isinstance(v, bool) for v in box):
                    self.err(here, "needs integer x, y, width and height")
                    continue
                x, y, w, h = box
                if not 1 <= w <= 24 or x < 0 or x + w > 24 or h < 1 or y < 0:
                    self.err(here, f"x {x}, width {w} leaves the 24-column grid")
                    continue
                for other, (ox, oy, ow, oh) in boxes:
                    if x < ox + ow and ox < x + w and y < oy + oh and oy < y + h:
                        self.err(here, f"overlaps {other}")
                boxes.append((ref.get("name"), box))
        elif kind == "AutoGridLayout":
            for item in spec.get("items") or []:
                s = item.get("spec") or {}
                self.place(s.get("element"), f"{where} › {_get(s, 'element', 'name')}", tabs,
                           _get(s, "repeat", "value"))
        else:
            self.err(where, f"has an unknown layout kind {json.dumps(kind)}")

    def check_elements(self) -> None:
        owners: dict[object, list[str]] = {}
        for name, el in self.elements.items():
            if not isinstance(el, dict) or el.get("kind") != "Panel":
                self.err(f"element {name}", "must be a Panel: library panels are not "
                         "provisioned with the dashboard")
                continue
            pid = _get(el, "spec", "id")
            if not isinstance(pid, int) or isinstance(pid, bool):
                self.err(f"element {name}", "has no integer spec.id")
                continue
            owners.setdefault(pid, []).append(name)
            if name != f"panel-{pid}":
                self.err(self.where(el["spec"]), f"is element '{name}': name it 'panel-{pid}', "
                         "the key Grafana's viewPanel links use")
        for pid, names in owners.items():
            if len(names) > 1:
                self.err(f"panel id {pid}", "is used by " + " and ".join(names))

    def check_schema(self) -> None:
        if self.schema is not None:
            for e in self.schema.check(self.spec):
                self.err("schema", e)

    # -- datasources and queries --------------------------------------------------

    def check_datasources(self) -> None:
        for where, obj in self.sections():
            for node in _nodes(obj):
                if node.get("kind") != "DataQuery":
                    continue
                name = _get(node, "datasource", "name")
                m = re.fullmatch(r"\$\{?(\w+)\}?", name if isinstance(name, str) else "")
                if name in SPECIAL_DATASOURCES:
                    continue
                if name is None:
                    self.err(where, f"a {node.get('group')} query names no datasource: use "
                             "${DS_PROMETHEUS} or ${DS_LOKI}")
                elif not m:
                    self.err(where, f"datasource '{name}' is hard-coded: use ${{DS_PROMETHEUS}} "
                             "or ${DS_LOKI}")
                elif self.var_type(m.group(1)) != "datasource":
                    self.err(where, f"datasource ${m.group(1)} is not a datasource variable")
                elif (plugin := _get(self.variables[m.group(1)], "spec", "pluginId")) \
                        != node.get("group"):
                    self.err(where, f"a {node.get('group')} query runs on ${m.group(1)}, a "
                             f"{plugin} datasource")

    def suffixes(self, family: str) -> tuple[str, ...]:
        return ("_bucket", "_sum", "_count") if self.families[family].kind == "histogram" else ("",)

    def name_variable_values(self, name: str) -> list[str] | None:
        """Values of a variable that lists metric names, else None."""
        q = self.var_query(name)
        if self.var_type(name) != "query" or not isinstance(q, str):
            return None
        m = re.fullmatch(r"\s*label_values\((.*),\s*__name__\s*\)\s*", q, re.S)
        if not m:
            return None
        names, matchers = promql_names(m.group(1))
        rx = grafana_regex(_get(self.variables[name], "spec", "regex", default=""))
        out = set()
        for fam in self.families:
            for series in (fam + s for s in self.suffixes(fam)):
                if names and series not in names:
                    continue
                if not all((re.fullmatch(val if "~" in op else re.escape(val), series) is not None)
                           == op.startswith("=") for op, val in matchers):
                    continue
                if rx and not (hit := rx.search(series)):
                    continue
                out.add(hit.group("value") if rx and "value" in rx.groupindex
                        else hit.group(1) if rx and rx.groups else series)
        return sorted(out)

    def environments(self, expr: str, repeat: str | None):
        """Variable values to check `expr` with: a repeated or single-valued
        metric-name variable one value at a time, a multi-value one joined."""
        used = {next(g for g in m.groups() if g) for m in VAR_RE.finditer(expr)}
        fixed, loop = {}, []
        for name in sorted(used & set(self.name_values)):
            vals, v = self.name_values[name], _get(self.variables[name], "spec", default={})
            if name == repeat or not (v.get("multi") or v.get("includeAll")):
                loop.append([(name, x) for x in vals])
            else:
                fixed[name] = "(" + "|".join(re.escape(x) for x in vals) + ")"
        for combo in itertools.product(*loop):
            yield {**fixed, **dict(combo)}

    @staticmethod
    def expand(expr: str, env: dict[str, str]) -> str:
        def sub(m: re.Match) -> str:
            name = next(g for g in m.groups() if g)
            if name.startswith("__"):
                return "5m"
            return env.get(name, f"__VAR_{name}__")
        return VAR_RE.sub(sub, expr)

    def check_promql(self, where: str, expr: str, env: dict[str, str], panel: bool) -> None:
        e = self.expand(expr, env)
        for name in sorted(set(re.findall(r"__VAR_(\w+?)__", STRING_RE.sub("", e)))):
            self.err(where, f"${name} is used outside a label matcher, where this check "
                     "cannot resolve it")
        names, matchers = promql_names(e)
        bare = bare_name_selectors(e)
        for n in names:
            if n.startswith("__VAR_"):
                continue
            fam = next((f for f in self.families for s in self.suffixes(f) if f + s == n), None)
            if fam is not None:
                self.queried |= {fam} if panel else set()
            elif n in self.families:
                self.err(where, f"'{n}' is a histogram: query {n}_bucket, {n}_sum or {n}_count")
            elif n not in PROMETHEUS_SERIES:
                self.err(where, f"metric '{n}' is not a registered /metrics family")
        for op, val in matchers:
            shown = re.sub(r"__VAR_(\w+?)__", r"$\1", val)
            shown = f'__name__{op}"{shown if len(shown) <= 60 else shown[:57] + "..."}"'
            if "__VAR_" in val:
                self.err(where, f"{shown} uses a variable whose values are not metric names")
                continue
            if op not in ("=", "=~"):
                continue
            try:
                rx = re.compile(val if op == "=~" else re.escape(val))
            except re.error:
                self.err(where, f"{shown} is not a valid regex")
                continue
            hit = {f for f in self.families for s in self.suffixes(f) if rx.fullmatch(f + s)}
            names = {f + s for f in self.families for s in self.suffixes(f) if rx.fullmatch(f + s)}
            if len(names) > 1 and (op, val) in bare:
                self.err(where, f"{shown} selects {len(names)} metrics, but a function or "
                         "operator that drops the metric name receives them: Prometheus "
                         "rejects that with \"vector cannot contain metrics with the same "
                         "labelset\". Aggregate them first, or label_replace the name into "
                         "a label")
            if not hit:
                self.err(where, f"{shown} matches no registered family")
            elif len(hit) == len(self.families) > 1:
                self.err(where, f"{shown} matches every family, which would hide unqueried ones")
            elif panel:
                self.queried |= hit

    def check_queries(self) -> None:
        for name in self.variables:
            spec = _get(self.variables[name], "spec", default={})
            if self.var_type(name) == "query":
                q = self.var_query(name)
                if not spec.get("definition"):
                    self.err(f"variable ${name}", "has no definition")
                elif spec["definition"] != q:
                    self.err(f"variable ${name}", "definition differs from query")
            try:
                vals = self.name_variable_values(name)
            except re.error as e:
                self.err(f"variable ${name}", f"has an invalid regex: {e}")
                continue
            if vals is not None:
                self.name_values[name] = vals
                if not vals:
                    self.err(f"variable ${name}", "lists no registered family")
        for name, p in self.panels():
            repeat = next((pl.repeat for pl in self.placements.get(name, []) if pl.repeat), None)
            for q in self.queries(p):
                query = q.get("query") or {}
                expr = _get(query, "spec", "expr")
                if not expr:
                    continue
                if self.ds_kind(query) == "prometheus":
                    for env in self.environments(expr, repeat):
                        self.check_promql(f"{self.where(p)} query {q.get('refId')}", expr, env, True)
        others = []
        for a in self.annotations:
            where, query = f"annotation \"{_get(a, 'spec', 'name')}\"", _get(a, "spec", "query")
            expr, legacy = _get(query, "spec", "expr"), _get(a, "spec", "legacyOptions", "expr")
            if legacy is not None and legacy != expr:
                self.err(where, "legacyOptions.expr differs from query.spec.expr")
            others.append((where, query, expr))
        others += [(f"variable ${n}", _get(self.variables[n], "spec", "query"), self.var_query(n))
                   for n in self.variables if self.var_type(n) == "query"]
        for where, query, expr in others:
            if expr and self.ds_kind(query) == "prometheus":
                for env in self.environments(expr, None):
                    self.check_promql(where, expr, env, False)

    # -- strings: variables, hosts, links -------------------------------------

    def panel_tabs(self, pid) -> tuple[str, ...] | None:
        places = self.placements.get(f"panel-{pid}")
        return places[0].tabs if places else None

    def check_tab_link(self, where: str, s: str) -> None:
        params = dict((k, v) for k, v in TAB_PARAM_RE.findall(s))
        if not params:
            return
        path: tuple[str, ...] = ()
        key = "dtab"
        while key in params:
            slug = params.pop(key)
            if "$" in slug:
                return
            children = self.tabs.get(path, [])
            title = next((t for t in children if slug in tab_slugs(t)), None)
            if title is None:
                shown = " › ".join(path + (slug,))
                self.err(where, f"links to tab '{shown}', which does not exist")
                return
            path += (title,)
            # Grafana keys a nested tab set by its ancestors' slugs: <a>-<b>-dtab
            key = next((k for k in params for s in tab_slugs(title)
                        if k == f"{s}-dtab" or k.endswith(f"-{s}-dtab")), "")
        for k in params:
            self.err(where, f"link parameter {k} names no tab on the way to it")
        for m in re.finditer(r"viewPanel=(?:panel-)?(\d+)", s):
            tabs = self.panel_tabs(int(m.group(1)))
            if tabs is not None and tabs[:len(path)] != path:
                self.err(where, f"links to panel {m.group(1)} on tab '{' › '.join(path)}', but "
                         f"the panel is on '{' › '.join(tabs)}'")

    def check_strings(self, where: str, obj) -> None:
        for s in _strings(obj):
            for m in VAR_RE.finditer(s):
                name = next(g for g in m.groups() if g)
                if name not in self.variables and name not in BUILTIN_VARS and not name.startswith("__"):
                    self.err(where, f"variable ${name} is not defined")
            if URL_RE.search(s):
                self.err(where, f"absolute URL in {s[:70]!r}: link relatively")
            elif h := (IP_RE.search(s) or HOST_RE.search(s)):
                self.err(where, f"hard-coded host '{h.group(0)}'")
            for m in re.finditer(r"viewPanel=(?:panel-)?(\d+)", s):
                if int(m.group(1)) not in self.ids:
                    self.err(where, f"links to panel {m.group(1)}, which does not exist")
            for m in re.finditer(r"/d/([\w-]+)", s):
                if m.group(1) not in self.uids:
                    self.err(where, f"links to dashboard uid '{m.group(1)}', which is not in "
                             f"{DASHBOARD_GLOB}")
            if "/d/" in s:
                self.check_tab_link(where, s)
            if "@@" in s:
                self.err(where, f"unresolved placeholder in {s[:70]!r}")

    # -- panels ---------------------------------------------------------------

    def check_panels(self) -> None:
        for name, p in self.panels():
            t, where = self.panel_type(p), self.where(p)
            if not p.get("title"):
                self.err(where, "has no title")
            if not p.get("description"):
                self.err(where, "has no description")
            queries = self.queries(p)
            if t not in QUERYLESS_PANELS and not queries:
                self.err(where, "has no queries")
            field_defaults = _get(p, "vizConfig", "spec", "fieldConfig", "defaults", default={})
            if t in UNIT_PANELS and not (field_defaults or {}).get("unit"):
                self.err(where, "has no unit (fieldConfig.defaults.unit)")
            refids = [q.get("refId") for q in queries]
            if len(refids) != len(set(refids)):
                self.err(where, f"repeats a query refId: {refids}")
            legend = _get(p, "vizConfig", "spec", "options", "legend", default={}) or {}
            if legend.get("sortBy") and legend.get("displayMode") == "table":
                shown = {CALC_NAMES.get(c, c) for c in legend.get("calcs", [])}
                if legend["sortBy"] not in shown:
                    self.err(where, f"legend sorts by '{legend['sortBy']}', which it does not "
                             f"show (calcs: {sorted(shown)})")
            for pl in self.placements.get(name, []):
                if pl.repeat and pl.repeat not in self.variables:
                    self.err(where, f"repeats over undefined variable ${pl.repeat}")


def check_dashboards(families: dict[str, Family], dashboards: list[tuple[str, object]],
                     schema: CueSchema | None = None) -> tuple[list[str], list[str]]:
    """Problems with the dashboards and coverage, and one summary per dashboard."""
    problems, summaries, queried, read = [], [], set(), 0
    uids = {_get(d, "metadata", "name") for _, d in dashboards}
    for label, dash in dashboards:
        chk = DashboardCheck(label, dash, families, uids, schema)
        if chk.run():
            read += 1
        problems += chk.problems
        queried |= chk.queried
        top = len(chk.tabs.get((), []))
        nested = sum(len(v) for k, v in chk.tabs.items() if k)
        summaries.append(f"{label} ({top} tabs, {nested} sub-tabs, {chk.rows} rows, "
                         f"{sum(1 for _ in chk.panels())} panels)")
    if read:
        problems += [f"{f.where}: family '{f.name}' ({f.kind}) is not queried by any dashboard panel"
                     for f in sorted(families.values(), key=lambda f: f.name)
                     if f.name not in queried]
    return list(dict.fromkeys(problems)), summaries


def read_json(text: str):
    """Parse JSON, and list the keys an object repeats (json keeps the last)."""
    repeated: list[str] = []

    def pairs(items):
        out = {}
        for k, v in items:
            if k in out:
                repeated.append(k)
            out[k] = v
        return out
    return json.loads(text, object_pairs_hook=pairs), repeated


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        description="Check that the Grafana dashboards query every /metrics family "
                    "and are well-formed dashboard.grafana.app/v2 resources.")
    ap.add_argument("dashboards", nargs="*", type=Path,
                    help=f"dashboard JSON files (default: {DASHBOARD_GLOB})")
    ap.add_argument("--root", type=Path, default=REPO,
                    help="repository root (default: the one holding this script)")
    ap.add_argument("--list-families", action="store_true",
                    help="print the families found in the Rust sources and exit")
    a = ap.parse_args(argv)
    root = a.root.resolve()
    if not (root / "crates").is_dir():
        print(f"check_grafana_dashboard: {root / 'crates'} not found", file=sys.stderr)
        return 2
    files = rust_sources(root)
    families, problems = extract_families(files)
    if not families:
        problems.append("crates/: no /metrics family found: does a crate still depend on "
                        "prometheus?")
    if a.list_families:
        for f in sorted(families.values(), key=lambda f: f.name):
            print(f"{f.name} {f.kind} {f.where}")
        for p in problems:
            print(p, file=sys.stderr)
        return 1 if problems else 0

    paths = a.dashboards or sorted(root.glob(DASHBOARD_GLOB))
    if not paths:
        print(f"check_grafana_dashboard: no dashboards match {root / DASHBOARD_GLOB}", file=sys.stderr)
        return 2
    schema, why = load_schema()
    if why:
        problems.append(why)
    dashboards = []
    for path in paths:
        path = path.resolve()
        label = path.relative_to(root).as_posix() if path.is_relative_to(root) else str(path)
        try:
            doc, repeated = read_json(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as e:
            problems.append(f"{label}: cannot read the dashboard: {e}")
            continue
        problems += [f"{label}: key '{k}' appears twice in one object; JSON keeps only the last"
                     for k in dict.fromkeys(repeated)]
        dashboards.append((label, doc))
    more, summaries = check_dashboards(families, dashboards, schema)
    problems += more
    if problems:
        print("\n".join(problems))
        print(f"check_grafana_dashboard: {len(problems)} problem(s). {RULE}")
        return 1
    sources = sorted({f.where.rsplit(":", 1)[0] for f in families.values()})
    print(f"check_grafana_dashboard: OK: {len(families)} families from {', '.join(sources)}, "
          f"all queried by {', '.join(summaries)}; schema {SCHEMA_SOURCE}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
