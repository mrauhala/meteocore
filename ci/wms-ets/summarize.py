#!/usr/bin/env python3
"""Summarize an ets-wms13 run (TEAM Engine, CTL) for a GitHub job summary.

Reads the response of TEAM Engine's REST run with `Accept: application/xml`:
its report_logs.xml with the XIncludes resolved, an <execution> root holding
one <log> per test call. Each log has a <starttest> (prefix, local-name,
path, assertion, params), the <message>s and <request>s the test made, and
an <endtest result="N"/> with TEAM Engine's result code. If the report holds
no logs, --logs-dir names a copy of the session directory, whose log.xml
files are read instead.

Writes Markdown (stdout, or --markdown FILE) and, with --json FILE, the counts
and every failure for later tooling. Standard library only.

Exit status: 0 whenever the suite ran, whatever it found. Test failures are
reported, never enforced (#1065). 2 when the report is missing or unreadable,
is not a TEAM Engine log, or no conformance class ran, which is what a
capabilities document the suite could not read looks like.
"""

import argparse
import html
import json
import os
import sys
import xml.etree.ElementTree as ET
from collections import Counter

CTL_NS = "{http://www.occamlab.com/ctl}"

# TEAM Engine's result codes (com.occamlab.te.TECore).
OUTCOMES = {
    -1: "continue",
    0: "best practice",
    1: "passed",
    2: "not tested",
    3: "skipped",
    4: "warning",
    5: "inherited failure",
    6: "failed",
}
NO_RESULT = "no result"
# Column order of the count tables.
COLUMNS = ["passed", "failed", "inherited failure", "skipped", "not tested", "warning",
           "best practice", "continue", NO_RESULT]
TEXT_LIMIT = 400
# Conformance classes that test OGC's "Blue Lake" dataset, the cite:* layers,
# which MeteoCore does not serve. Their failures are expected, so they are
# listed collapsed, one line each, after the classes that judge MeteoCore's
# own layers (main:data-independent).
DATASET_CLASSES = ("main:data-preconditions", "basic:basic", "queryable:queryable")
# The tests of a DATASET_CLASSES class that read MeteoCore's own capabilities
# document, not the Blue Lake layers: whether GetMap offers image/gif or
# image/png, the options-requirements test that calls it, and whether
# GetFeatureInfo is advertised. Their failures are listed in full, like the
# data-independent ones.
OWN_TESTS = (
    "basic:gif-or-png",
    "basic:options-requirements",
    "queryable:getfeatureinfo-supported",
)
# Failures listed in full, with request and response; the rest get one line.
MAX_EXPANDED = 100
# GitHub drops a step summary over 1 MiB, leaving only an annotation.
SUMMARY_LIMIT = 900_000


def squash(text, limit=TEXT_LIMIT):
    """Collapse whitespace and cut to `limit` characters."""
    text = " ".join((text or "").split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def md_text(text, limit=TEXT_LIMIT):
    """Plain text for Markdown: no raw HTML, which GitHub would drop."""
    return html.escape(squash(text, limit), quote=False).replace("|", "\\|")


def md_code(text, limit=TEXT_LIMIT):
    """An inline code span; a backtick inside would end it."""
    return "`" + squash(text, limit).replace("`", "'") + "`"


def request_line(request):
    """`METHOD url?params` of a logged <request>, or None."""
    ctl = request.find(CTL_NS + "request")
    if ctl is None:
        return None
    url = (ctl.findtext(CTL_NS + "url") or "").strip()
    method = (ctl.findtext(CTL_NS + "method") or "get").strip().upper()
    pairs = [f"{p.get('name', '')}={(p.text or '').strip()}" for p in ctl.findall(CTL_NS + "param")]
    if pairs:
        sep = "" if url.endswith(("?", "&")) else ("&" if "?" in url else "?")
        url = url + sep + "&".join(pairs)
    return f"{method} {url}"


def response_text(request):
    """What the server answered: the status and content type when TEAM
    Engine's HTTPParser wrapped the response, then the body's text. The body
    text is left out for an image, whose parsed form is a number or two.

    HTTPParser, which most GetMap tests use, logs content/response with a
    <status code>, the <headers> and an inner <content> holding what its
    subsidiary parser read. Flattening that whole tree would print the
    header values instead of the body."""
    content = request.find("response/content")
    if content is None:
        return ""
    head = ""
    http = content.find("response")
    if http is not None and http.find("status") is not None:
        head = f"HTTP {http.find('status').get('code', '?')}"
        for header in http.iter("header"):
            if (header.get("name") or "").lower() == "content-type":
                head += " " + squash(header.text, 80)
                break
        content = http.find("content")
    text = squash("".join(content.itertext()), 300) if content is not None else ""
    if not any(c.isalpha() for c in text):
        text = ""
    return ": ".join(part for part in (head, text) if part)


def read_log(log):
    start = log.find("starttest")
    if start is None:
        return None
    end = log.find("endtest")
    code = None
    if end is not None:
        try:
            code = int(end.get("result", ""))
        except ValueError:
            code = None
    labels = []
    params = start.find("params")
    if params is not None:
        for param in params.findall("param"):
            label = (param.get("label") or "").strip()
            if label:
                labels.append(label)
    requests = log.findall("request")
    last = requests[-1] if requests else None
    cc = log.find("conformanceClass")
    return {
        "name": f"{start.get('prefix', '')}:{start.get('local-name', '')}",
        "path": start.get("path", ""),
        "assertion": squash(start.findtext("assertion")),
        "params": labels,
        "outcome": OUTCOMES.get(code, NO_RESULT),
        "messages": [squash(m.text) for m in log.findall("message") if (m.text or "").strip()],
        "exception": squash(log.findtext("exception")),
        "request": request_line(last) if last is not None else None,
        "response": response_text(last) if last is not None else "",
        "conformance_class": cc.get("name") if cc is not None else None,
    }


def load_logs(report, logs_dir):
    """The <log> elements of the report, else of every log.xml under logs_dir."""
    logs = []
    try:
        root = ET.parse(report).getroot()
    except (OSError, ET.ParseError) as err:
        print(f"cannot read the report {report}: {err}", file=sys.stderr)
        root = None
    if root is not None:
        if root.tag != "execution":
            print(f"the report's root is <{root.tag}>, not TEAM Engine's <execution>", file=sys.stderr)
        else:
            logs = list(root.iter("log"))
    if not logs and logs_dir and os.path.isdir(logs_dir):
        for dirpath, _, files in sorted(os.walk(logs_dir)):
            if "log.xml" in files:
                try:
                    logs.append(ET.parse(os.path.join(dirpath, "log.xml")).getroot())
                except ET.ParseError as err:
                    print(f"skipping {dirpath}/log.xml: {err}", file=sys.stderr)
        if logs:
            print(f"read {len(logs)} log.xml files under {logs_dir}", file=sys.stderr)
    return [t for t in (read_log(log) for log in logs if log.tag == "log") if t]


def class_of(test, classes):
    """The innermost conformance class whose path contains the test."""
    best = None
    for path, name in classes.items():
        if test["path"] == path or test["path"].startswith(path + "/"):
            if best is None or len(path) > len(best[0]):
                best = (path, name)
    return best


def summarize(tests):
    """Split the tests into the conformance classes' own logs, the tests
    inside a class (counted, as TEAM Engine's EARL report counts them) and
    the rest (the suite's entry test)."""
    classes = {t["path"]: t["conformance_class"] for t in tests if t["conformance_class"]}
    class_logs, scored, outside = [], [], []
    for test in tests:
        owner = class_of(test, classes)
        if owner is None:
            outside.append(test)
        elif owner[0] == test["path"]:
            class_logs.append(dict(test, cls=owner[1]))
        else:
            scored.append(dict(test, cls=owner[1]))
    return class_logs, scored, outside


def is_failure(test):
    return test["outcome"] in ("failed", NO_RESULT)


def needs_dataset(test, class_log):
    """Whether a failure only shows that OGC's Blue Lake dataset is absent:
    it is in a DATASET_CLASSES class and is not one of the OWN_TESTS."""
    return class_log["name"] in DATASET_CLASSES and test["name"] not in OWN_TESTS


def count_row(counter, columns):
    return [str(counter.get(c, 0)) for c in columns]


def failure_head(test):
    head = f"- **{md_code(test['name'], 80)}**"
    if test["params"]:
        head += " (" + md_text("; ".join(test["params"]), 200) + ")"
    return head


def failure_line(test):
    """One line: the name and the first message, else the assertion."""
    detail = test["messages"][0] if test["messages"] else test["exception"] or test["assertion"]
    return failure_head(test) + ": " + md_text(detail, 200)


def failure_lines(test):
    lines = [failure_head(test) + ": " + md_text(test["assertion"])]
    for message in test["messages"]:
        lines.append("  - " + md_text(message))
    if test["exception"]:
        lines.append("  - exception: " + md_text(test["exception"]))
    if test["request"]:
        lines.append("  - request: " + md_code(test["request"], 500))
    if test["response"]:
        lines.append("  - response: " + md_text(test["response"], 300))
    return lines


def markdown(class_logs, scored, outside, title, expand=MAX_EXPANDED):
    """The job summary. At most `expand` failures and warnings are listed in
    full; the rest, and every failure of a DATASET_CLASSES class outside
    OWN_TESTS, get one line each."""
    budget = [expand]

    def entries(tests):
        lines = []
        for test in tests:
            if budget[0] > 0:
                budget[0] -= 1
                lines += failure_lines(test)
            else:
                lines.append(failure_line(test))
        return lines

    totals = Counter(t["outcome"] for t in scored)
    out = [f"## {title}", ""]
    if not scored:
        out += ["**The suite did not run: no conformance class holds a test. This fails the job.**", ""]
    out.append(
        f"**{len(scored)} tests: {totals['passed']} passed, {totals['failed']} failed, "
        f"{totals['inherited failure']} failed through a subtest, {totals['skipped']} skipped, "
        f"{totals['not tested']} not tested, {totals['warning']} warnings.**"
    )
    # Passed, failed and skipped always; the rarer outcomes when they occur.
    columns = [c for c in COLUMNS if c in ("passed", "failed", "skipped") or totals[c]]
    out += [
        "",
        "Test failures do not fail this job. The OGC test suite section of "
        "`crates/api-wms/README.md` lists the failures expected from MeteoCore's "
        "deliberate limits. `data-preconditions`, `basic` and `queryable` test OGC's "
        "Blue Lake dataset, which MeteoCore does not serve: their failures are "
        "expected and listed collapsed, except `basic:gif-or-png`, "
        "`basic:options-requirements` and `queryable:getfeatureinfo-supported`, which "
        "read MeteoCore's own capabilities and are listed in full. The full report is "
        "the run's `wms-ets-report` artifact.",
        "",
        "| Conformance class | Verdict | " + " | ".join(c.capitalize() for c in columns) + " |",
        "|---|---|" + "---:|" * len(columns),
    ]
    for log in class_logs:
        counter = Counter(t["outcome"] for t in scored if t["cls"] == log["cls"])
        out.append(f"| {md_text(log['cls'])} | {log['outcome']} | " + " | ".join(count_row(counter, columns)) + " |")
    out.append("")

    # A class's own log fails directly when its preconditions do not hold
    # (data-preconditions: no cite:* layers); list it before its subtests.
    # The failures that judge MeteoCore come first, in full, the classes
    # that judge its own layers ahead of the dataset classes' OWN_TESTS;
    # then the failures that only need the Blue Lake dataset, collapsed.
    failed = [t for t in class_logs + scored if is_failure(t)]
    out.append(f"### Failed ({len(failed)})")
    out.append("")
    if not failed:
        out.append("None.")
    ordered = sorted(class_logs, key=lambda log: log["name"] in DATASET_CLASSES)
    for log in ordered:
        own = [t for t in failed if t["cls"] == log["cls"] and not needs_dataset(t, log)]
        if own:
            out += ["", f"#### {md_text(log['cls'])} ({len(own)})", ""]
            out += entries(own)
    for log in ordered:
        dataset = [t for t in failed if t["cls"] == log["cls"] and needs_dataset(t, log)]
        if dataset:
            heading = f"{md_text(log['cls'])} ({len(dataset)})"
            out += ["", f"<details><summary>{heading}: needs OGC's Blue Lake dataset</summary>", ""]
            out += [failure_line(test) for test in dataset]
            out += ["", "</details>"]
    out.append("")

    root_failures = [t for t in outside if is_failure(t)]
    if root_failures:
        out += ["### Failed outside a conformance class", ""]
        out += entries(root_failures)
        out.append("")

    warnings = [t for t in scored if t["outcome"] == "warning"]
    if warnings:
        out += [f"### Warnings ({len(warnings)})", ""]
        out += entries(warnings)
        out.append("")

    def collapsed(label, tests):
        names = Counter(t["name"] for t in tests)
        block = [f"<details><summary>{label} ({len(tests)})</summary>", ""]
        for name, n in sorted(names.items()):
            block.append(f"- {md_code(name, 80)}" + (f" ×{n}" if n > 1 else ""))
        return block + ["", "</details>", ""]

    skipped = [t for t in scored if t["outcome"] in ("skipped", "not tested")]
    out += collapsed("Skipped and not tested", skipped)
    out += collapsed("Passed", [t for t in scored if t["outcome"] in ("passed", "best practice")])
    return "\n".join(out) + "\n"


def render(class_logs, scored, outside, title, limit=SUMMARY_LIMIT):
    """markdown() within `limit` bytes: one line per failure when the full
    form is too long, then cut at a line end with a note."""
    text = markdown(class_logs, scored, outside, title)
    if len(text.encode("utf-8")) <= limit:
        return text
    text = markdown(class_logs, scored, outside, title, expand=0)
    if len(text.encode("utf-8")) <= limit:
        return text
    note = (
        "\n\n**The summary is cut here to fit GitHub's size limit. `summary.json` "
        "in the `wms-ets-report` artifact lists every failure.**\n"
    )
    room = limit - len(note.encode("utf-8")) - len("\n\n</details>")
    cut = text.encode("utf-8")[:room].decode("utf-8", "ignore")
    cut = cut[: cut.rfind("\n")]
    if cut.count("<details>") > cut.count("</details>"):
        cut += "\n\n</details>"
    return cut + note


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("report", help="TEAM Engine's application/xml run response")
    parser.add_argument("--logs-dir", help="session directory to read when the report holds no logs")
    parser.add_argument("--markdown", help="write the Markdown here instead of stdout")
    parser.add_argument("--json", help="also write counts and failures as JSON here")
    parser.add_argument("--title", default="OGC WMS 1.3.0 test suite (ets-wms13)")
    args = parser.parse_args()

    tests = load_logs(args.report, args.logs_dir)
    class_logs, scored, outside = summarize(tests)
    text = render(class_logs, scored, outside, args.title)
    if args.markdown:
        with open(args.markdown, "w", encoding="utf-8") as fh:
            fh.write(text)
    else:
        sys.stdout.write(text)
    if args.json:
        doc = {
            "counts": dict(Counter(t["outcome"] for t in scored)),
            "classes": {
                log["cls"]: {
                    "verdict": log["outcome"],
                    "counts": dict(Counter(t["outcome"] for t in scored if t["cls"] == log["cls"])),
                }
                for log in class_logs
            },
            "failures": [t for t in class_logs + scored + outside if is_failure(t)],
            "warnings": [t for t in scored if t["outcome"] == "warning"],
            "skipped": [t["name"] for t in scored if t["outcome"] in ("skipped", "not tested")],
        }
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump(doc, fh, indent=2)

    if not scored:
        print("the suite did not run: no test inside a conformance class", file=sys.stderr)
        for test in outside:
            if test["outcome"] != "passed":
                print(f"  {test['name']}: {test['outcome']}: {'; '.join(test['messages'])}", file=sys.stderr)
        return 2
    totals = Counter(t["outcome"] for t in scored)
    print(
        f"ets-wms13: {len(scored)} tests, {totals['passed']} passed, {totals['failed']} failed, "
        f"{totals['skipped'] + totals['not tested']} skipped or not tested",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
