"""Tests for summarize.py, the ets-wms13 job summary (#1065).

Run from the repository root, as the workflow does:

    python3 -m unittest discover -s ci/wms-ets -v

The fixture is a trimmed TEAM Engine CTL report in the shape TECore writes:
an <execution> of <log>s, each with a <starttest>, its <message>s and
<request>s, an <endtest> result code and, for a conformance class, a
<conformanceClass> element. Standard library only.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET

import summarize

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "summarize.py")

ROOT = "s0001/d1e1_1"
INDEPENDENT = ROOT + "/d1e10_1"
PRECONDITIONS = ROOT + "/d1e30_1"
BASIC = ROOT + "/d1e40_1"
QUERYABLE = ROOT + "/d1e60_1"

ENTRY = f"""
  <log mode="test">
    <starttest local-name="main-auto" prefix="main" path="{ROOT}">
      <assertion>The WMS passes the selected conformance classes.</assertion>
    </starttest>
    <endtest result="5"/>
  </log>"""

CLASSES = f"""
  <log mode="test">
    <starttest local-name="data-independent" prefix="main" path="{INDEPENDENT}">
      <assertion>The data-independent tests pass.</assertion>
    </starttest>
    <endtest result="5"/>
    <conformanceClass name="data-independent" isBasic="true" result="5"/>
  </log>
  <log mode="test">
    <starttest local-name="getmap-layers" prefix="getmap" path="{INDEPENDENT}/d1e20_1">
      <assertion>A GetMap for an advertised layer returns an image.</assertion>
      <params><param name="layer" label="Layer ets-radar"/></params>
    </starttest>
    <request id="r1">
      <ctl:request>
        <ctl:url>http://127.0.0.1:8000/wms?</ctl:url>
        <ctl:method>get</ctl:method>
        <ctl:param name="REQUEST">GetMap</ctl:param>
        <ctl:param name="LAYERS">ets-radar</ctl:param>
      </ctl:request>
      <response><content><ServiceExceptionReport><ServiceException code="LayerNotDefined">No such layer</ServiceException></ServiceExceptionReport></content></response>
    </request>
    <message id="m1"><![CDATA[Expected an image, got a service exception.]]></message>
    <endtest result="6"/>
  </log>
  <log mode="test">
    <starttest local-name="getmap-format" prefix="getmap" path="{INDEPENDENT}/d1e20_1/d1e21_1">
      <assertion>The image has the requested format.</assertion>
    </starttest>
    <endtest result="1"/>
  </log>
  <log mode="test">
    <starttest local-name="getfeatureinfo" prefix="getfeatureinfo" path="{INDEPENDENT}/d1e25_1">
      <assertion>GetFeatureInfo on a queryable layer.</assertion>
    </starttest>
    <endtest result="3"/>
  </log>
  <log mode="test">
    <starttest local-name="data-preconditions" prefix="main" path="{PRECONDITIONS}">
      <assertion>The server implements the CITE standard dataset.</assertion>
    </starttest>
    <message id="m2"><![CDATA[Layer cite:BasicPolygons is missing.]]></message>
    <endtest result="6"/>
    <conformanceClass name="data-preconditions" isBasic="true" result="6"/>
  </log>
  <log mode="test">
    <starttest local-name="basic" prefix="basic" path="{BASIC}">
      <assertion>The basic class passes.</assertion>
    </starttest>
    <endtest result="5"/>
    <conformanceClass name="basic" isBasic="true" result="5"/>
  </log>
  <log mode="test">
    <starttest local-name="blue-lake-sanity-check" prefix="basic" path="{BASIC}/d1e50_1">
      <assertion>The Blue Lake layer renders blue.</assertion>
    </starttest>
    <request id="r2">
      <ctl:request>
        <ctl:url>http://127.0.0.1:8000/wms?LAYERS=cite:Lakes</ctl:url>
        <ctl:method>get</ctl:method>
      </ctl:request>
    </request>
    <message id="m3"><![CDATA[Layer cite:Lakes is not defined.]]></message>
    <endtest result="6"/>
  </log>"""


def report(*logs):
    return '<execution xmlns:ctl="http://www.occamlab.com/ctl">' + "".join(logs) + "\n</execution>\n"


def parse(text):
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "report_logs.xml")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(text)
        return summarize.summarize(summarize.load_logs(path, None))


def run_script(*args):
    return subprocess.run(
        [sys.executable, SCRIPT, *args], capture_output=True, text=True, check=False
    )


class SummarizeTest(unittest.TestCase):
    def setUp(self):
        self.class_logs, self.scored, self.outside = parse(report(ENTRY, CLASSES))

    def outcome(self, name):
        return next(t for t in self.scored if t["name"] == name)

    def test_tests_belong_to_their_innermost_class(self):
        self.assertEqual(
            [log["name"] for log in self.class_logs],
            ["main:data-independent", "main:data-preconditions", "basic:basic"],
        )
        self.assertEqual([t["name"] for t in self.outside], ["main:main-auto"])
        # A subtest of a subtest still counts for the class.
        self.assertEqual(self.outcome("getmap:getmap-format")["cls"], "data-independent")
        self.assertEqual(self.outcome("basic:blue-lake-sanity-check")["cls"], "basic")

    def test_result_codes(self):
        self.assertEqual(self.outcome("getmap:getmap-layers")["outcome"], "failed")
        self.assertEqual(self.outcome("getmap:getmap-format")["outcome"], "passed")
        self.assertEqual(self.outcome("getfeatureinfo:getfeatureinfo")["outcome"], "skipped")
        self.assertEqual(self.class_logs[0]["outcome"], "inherited failure")

    def test_own_failures_in_full_dataset_classes_collapsed(self):
        text = summarize.markdown(self.class_logs, self.scored, self.outside, "T")
        self.assertIn("**4 tests: 1 passed, 2 failed", text)
        own = text.index("#### data-independent (1)")
        self.assertIn(
            "`GET http://127.0.0.1:8000/wms?REQUEST=GetMap&LAYERS=ets-radar`", text[own:]
        )
        self.assertIn("Expected an image, got a service exception.", text)
        self.assertIn("No such layer", text)
        # The classes that need OGC's dataset follow, one line per failure.
        pre = text.index("<details><summary>data-preconditions (1)")
        basic = text.index("<details><summary>basic (1)")
        self.assertLess(own, pre)
        self.assertIn(
            "- **`main:data-preconditions`**: Layer cite:BasicPolygons is missing.", text
        )
        self.assertIn("- **`basic:blue-lake-sanity-check`**: Layer cite:Lakes is not defined.", text)
        self.assertNotIn("LAYERS=cite:Lakes", text[basic:])

    def test_basic_tests_of_own_capabilities_listed_in_full(self):
        # basic:gif-or-png reads MeteoCore's capabilities document, not the
        # Blue Lake layers, so its failure is listed with the
        # data-independent ones, not collapsed with the dataset tests.
        own = f"""
  <log mode="test">
    <starttest local-name="options-requirements" prefix="basic" path="{BASIC}/d1e44_1">
      <assertion>The requirements for testing the selected options are met.</assertion>
    </starttest>
    <endtest result="6"/>
  </log>
  <log mode="test">
    <starttest local-name="gif-or-png" prefix="basic" path="{BASIC}/d1e44_1/d1e45_1">
      <assertion>The server supports GetMap format image/gif and/or image/png.</assertion>
    </starttest>
    <endtest result="6"/>
  </log>"""
        class_logs, scored, outside = parse(report(ENTRY, CLASSES, own))
        text = summarize.markdown(class_logs, scored, outside, "T")
        full = text.index("#### basic (2)")
        collapsed = text.index("<details><summary>basic (1): needs OGC's Blue Lake dataset")
        # After the data-independent failures, before every collapsed block.
        self.assertLess(text.index("#### data-independent (1)"), full)
        self.assertLess(full, text.index("<details><summary>data-preconditions (1)"))
        self.assertLess(full, collapsed)
        own_section = text[full:collapsed]
        self.assertIn(
            "- **`basic:gif-or-png`**: The server supports GetMap format image/gif and/or image/png.",
            own_section,
        )
        self.assertIn("- **`basic:options-requirements`**:", own_section)
        self.assertNotIn("blue-lake", own_section)
        dataset_section = text[collapsed:]
        self.assertIn("basic:blue-lake-sanity-check", dataset_section)
        self.assertNotIn("gif-or-png", dataset_section)
        self.assertNotIn("options-requirements", dataset_section)

    def test_queryable_dataset_tests_collapsed_its_own_test_in_full(self):
        # The queryable class (#1064) queries the Blue Lake layers, except
        # getfeatureinfo-supported, which reads MeteoCore's capabilities.
        queryable = f"""
  <log mode="test">
    <starttest local-name="queryable" prefix="queryable" path="{QUERYABLE}">
      <assertion>The queryable class passes.</assertion>
    </starttest>
    <endtest result="5"/>
    <conformanceClass name="queryable" isBasic="false" result="5"/>
  </log>
  <log mode="test">
    <starttest local-name="getfeatureinfo-supported" prefix="queryable" path="{QUERYABLE}/d1e61_1">
      <assertion>The server advertises GetFeatureInfo.</assertion>
    </starttest>
    <endtest result="6"/>
  </log>
  <log mode="test">
    <starttest local-name="std-data-queryable" prefix="queryable" path="{QUERYABLE}/d1e62_1">
      <assertion>The CITE standard dataset layers containing polygons are queryable.</assertion>
    </starttest>
    <message id="m4"><![CDATA[No layer named Lakes found.]]></message>
    <endtest result="6"/>
  </log>"""
        class_logs, scored, outside = parse(report(ENTRY, CLASSES, queryable))
        text = summarize.markdown(class_logs, scored, outside, "T")
        full = text.index("#### queryable (1)")
        collapsed = text.index("<details><summary>queryable (1): needs OGC's Blue Lake dataset")
        self.assertLess(full, collapsed)
        self.assertIn("- **`queryable:getfeatureinfo-supported`**:", text[full:collapsed])
        self.assertIn(
            "- **`queryable:std-data-queryable`**: No layer named Lakes found.", text[collapsed:]
        )
        self.assertNotIn("getfeatureinfo-supported", text[collapsed:])

    def test_past_the_budget_failures_get_one_line(self):
        text = summarize.markdown(self.class_logs, self.scored, self.outside, "T", expand=0)
        self.assertNotIn("request:", text)
        self.assertIn(
            "- **`getmap:getmap-layers`** (Layer ets-radar): Expected an image, got a service exception.",
            text,
        )

    def test_render_fits_the_limit(self):
        many = "".join(
            f"""
  <log mode="test">
    <starttest local-name="t{i}" prefix="getmap" path="{INDEPENDENT}/d2e{i}_1">
      <assertion>{"long assertion " * 20}</assertion>
    </starttest>
    <message id="x{i}"><![CDATA[{"long message " * 20}]]></message>
    <endtest result="6"/>
  </log>"""
            for i in range(300)
        )
        class_logs, scored, outside = parse(report(ENTRY, CLASSES, many))
        full = summarize.markdown(class_logs, scored, outside, "T")
        limit = 20_000
        self.assertGreater(len(full.encode("utf-8")), limit)
        text = summarize.render(class_logs, scored, outside, "T", limit=limit)
        self.assertLessEqual(len(text.encode("utf-8")), limit)
        self.assertIn("summary.json", text)
        self.assertEqual(text.count("<details>"), text.count("</details>"))


class HttpParserResponseTest(unittest.TestCase):
    """Most GetMap tests read the answer through TEAM Engine's HTTPParser,
    which logs content/response with a status, the headers and an inner
    content holding what its subsidiary parser read."""

    def request(self, code, content_type, inner):
        return ET.fromstring(f"""
<request id="r9" xmlns:ctl="http://www.occamlab.com/ctl">
  <ctl:request>
    <ctl:url>http://127.0.0.1:8000/wms?</ctl:url>
    <ctl:method>get</ctl:method>
  </ctl:request>
  <response>
    <parser local-name="HTTPParser" namespace-uri="http://www.occamlab.com/te/parsers" prefix="parsers"/>
    <content>
      <response>
        <status code="{code}" protocol="HTTP/1.1">Status</status>
        <headers>
          <header name="Date">Thu, 15 Oct 2026 12:00:00 GMT</header>
          <header name="content-type">{content_type}</header>
          <header name="Connection">keep-alive</header>
        </headers>
        <content>{inner}</content>
      </response>
    </content>
  </response>
</request>""")

    def test_status_type_and_exception_text(self):
        request = self.request(
            400,
            "application/vnd.ogc.se_xml",
            '<ServiceExceptionReport><ServiceException code="MissingParameterValue">'
            "Missing required parameter: REQUEST</ServiceException></ServiceExceptionReport>",
        )
        self.assertEqual(
            summarize.response_text(request),
            "HTTP 400 application/vnd.ogc.se_xml: Missing required parameter: REQUEST",
        )

    def test_image_gives_status_and_type_only(self):
        request = self.request(200, "image/png", "")
        self.assertEqual(summarize.response_text(request), "HTTP 200 image/png")


class ExitStatusTest(unittest.TestCase):
    def test_without_a_list_failures_do_not_fail_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "report_logs.xml")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(report(ENTRY, CLASSES))
            out = os.path.join(tmp, "summary.json")
            result = run_script(path, "--markdown", os.path.join(tmp, "s.md"), "--json", out)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(out, encoding="utf-8") as fh:
                doc = json.load(fh)
            self.assertEqual(doc["counts"], {"failed": 2, "passed": 1, "skipped": 1})
            self.assertEqual(
                sorted(f["name"] for f in doc["failures"]),
                ["basic:blue-lake-sanity-check", "getmap:getmap-layers", "main:data-preconditions"],
            )

    def test_no_conformance_class_ran(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "report_logs.xml")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(report(ENTRY))
            result = run_script(path, "--markdown", os.path.join(tmp, "s.md"))
            self.assertEqual(result.returncode, 2)

    def test_missing_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = run_script(os.path.join(tmp, "absent.xml"), "--markdown", os.path.join(tmp, "s.md"))
            self.assertEqual(result.returncode, 2)

    def test_session_logs_when_the_report_has_none(self):
        # The session directory: one log.xml per test call, in nested
        # directories, as TEAM Engine writes them.
        root = ET.fromstring(report(ENTRY, CLASSES))
        with tempfile.TemporaryDirectory() as tmp:
            session = os.path.join(tmp, "session")
            for i, log in enumerate(root.iter("log")):
                path = log.find("starttest").get("path")
                directory = os.path.join(session, path, f"call{i}")
                os.makedirs(directory)
                ET.ElementTree(log).write(os.path.join(directory, "log.xml"), encoding="utf-8")
            empty = os.path.join(tmp, "report_logs.xml")
            with open(empty, "w", encoding="utf-8") as fh:
                fh.write("<execution/>")
            out = os.path.join(tmp, "summary.json")
            result = run_script(
                empty, "--logs-dir", session, "--markdown", os.path.join(tmp, "s.md"), "--json", out
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(out, encoding="utf-8") as fh:
                self.assertEqual(json.load(fh)["counts"]["failed"], 2)


# The names the fixture report fails: the data-preconditions class's own log,
# one data-independent test and one Blue Lake test.
FAILING = ["main:data-preconditions", "getmap:getmap-layers", "basic:blue-lake-sanity-check"]
COMMITTED_LIST = os.path.join(os.path.dirname(SCRIPT), "expected-failures.txt")


def write(directory, name, text):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(text)
    return path


class ExpectedListTest(unittest.TestCase):
    """The expected-failure list's format (load_expected)."""

    def load(self, text):
        with tempfile.TemporaryDirectory() as tmp:
            return summarize.load_expected(write(tmp, "expected.txt", text))

    def test_names_comments_and_blank_lines(self):
        names = self.load(
            "# The cause, as a comment.\n"
            "getmap:two-layers\n"
            "\n"
            "   getfeatureinfo:two-query_layers   # a reason after the name\n"
            "# getmap:commented-out\n"
            "main:data-preconditions\n"
        )
        self.assertEqual(
            names, ["getmap:two-layers", "getfeatureinfo:two-query_layers", "main:data-preconditions"]
        )

    def test_a_line_that_is_not_a_test_name(self):
        for line in ("two-layers", "getmap: two-layers", "getmap:two-layers extra", "a:b:c"):
            with self.subTest(line=line):
                with self.assertRaisesRegex(ValueError, r"expected\.txt:2: not a test name"):
                    self.load(f"# header\n{line}\n")

    def test_a_name_listed_twice(self):
        with self.assertRaisesRegex(ValueError, r":3: getmap:no-version is listed already, on line 1"):
            self.load("getmap:no-version\ngetmap:two-layers\ngetmap:no-version\n")

    def test_the_committed_list_parses(self):
        # A typo there would fail the job only after the whole suite ran.
        names = summarize.load_expected(COMMITTED_LIST)
        self.assertTrue(names)
        self.assertIn("main:data-preconditions", names)


class CheckTest(unittest.TestCase):
    """The run's failures against the list (check)."""

    def setUp(self):
        self.class_logs, self.scored, self.outside = parse(report(ENTRY, CLASSES))

    def check(self, expected):
        return summarize.check(self.class_logs, self.scored + self.outside, expected, "list.txt")

    def test_every_failure_listed(self):
        checked = self.check(FAILING)
        self.assertEqual(checked["unexpected"], [])
        self.assertEqual(sorted(t["name"] for t in checked["expected"]), sorted(FAILING))
        self.assertEqual(checked["expected_classes"], 1)
        self.assertEqual(checked["not_failing"], [])

    def test_a_failure_not_listed(self):
        checked = self.check(["main:data-preconditions", "basic:blue-lake-sanity-check"])
        self.assertEqual([t["name"] for t in checked["unexpected"]], ["getmap:getmap-layers"])

    def test_listed_tests_that_did_not_fail(self):
        checked = self.check(
            FAILING + ["getmap:getmap-format", "getfeatureinfo:getfeatureinfo", "getmap:not-in-the-run"]
        )
        self.assertEqual(checked["unexpected"], [])
        self.assertEqual(
            checked["not_failing"],
            [
                {"name": "getmap:getmap-format", "outcomes": ["passed"]},
                {"name": "getfeatureinfo:getfeatureinfo", "outcomes": ["skipped"]},
                {"name": "getmap:not-in-the-run", "outcomes": []},
            ],
        )

    def test_one_failing_call_keeps_a_name_expected(self):
        # A test called once per layer: one failing call is still a failure
        # of the listed name, not a listed test that did not fail.
        again = f"""
  <log mode="test">
    <starttest local-name="getmap-layers" prefix="getmap" path="{INDEPENDENT}/d1e22_1">
      <assertion>A GetMap for an advertised layer returns an image.</assertion>
      <params><param name="layer" label="Layer ets-geotiff"/></params>
    </starttest>
    <endtest result="1"/>
  </log>"""
        self.class_logs, self.scored, self.outside = parse(report(ENTRY, CLASSES, again))
        checked = self.check(FAILING)
        self.assertEqual(checked["unexpected"], [])
        self.assertEqual(checked["not_failing"], [])

    def test_summary_verdicts(self):
        def text(expected):
            return summarize.markdown(
                self.class_logs, self.scored, self.outside, "T", checked=self.check(expected)
            )

        passing = text(FAILING)
        self.assertIn(
            "**Every failure is expected: the 2 failed tests and 1 failed class are on `list.txt`.**",
            passing,
        )
        self.assertNotIn("Unexpected failures", passing)
        self.assertNotIn("did not fail", passing)

        failing = text(["main:data-preconditions", "basic:blue-lake-sanity-check"])
        self.assertIn("**1 unexpected failure fails this job: its test is not on `list.txt`.**", failing)
        section = failing.index("### Unexpected failures (1)")
        # Ahead of the class table and the full failure list.
        self.assertLess(section, failing.index("| Conformance class |"))
        self.assertIn(
            "- **`getmap:getmap-layers`** (Layer ets-radar): Expected an image, got a service exception.",
            failing[section:failing.index("| Conformance class |")],
        )

        warned = text(FAILING + ["getmap:getmap-format", "getmap:not-in-the-run"])
        section = warned.index("### Warning: expected failures that did not fail (2)")
        self.assertIn("- `getmap:getmap-format`: passed", warned[section:])
        self.assertIn("- `getmap:not-in-the-run`: not run", warned[section:])
        self.assertIn("**Every failure is expected", warned)

        without = summarize.markdown(self.class_logs, self.scored, self.outside, "T")
        self.assertIn("No expected-failure list was given", without)
        self.assertNotIn("Every failure is expected", without)

    def test_the_verdict_survives_the_cut(self):
        many = "".join(
            f"""
  <log mode="test">
    <starttest local-name="t{i}" prefix="getmap" path="{INDEPENDENT}/d2e{i}_1">
      <assertion>{"long assertion " * 20}</assertion>
    </starttest>
    <message id="x{i}"><![CDATA[{"long message " * 20}]]></message>
    <endtest result="6"/>
  </log>"""
            for i in range(300)
        )
        self.class_logs, self.scored, self.outside = parse(report(ENTRY, CLASSES, many))
        text = summarize.render(
            self.class_logs, self.scored, self.outside, "T", limit=20_000, checked=self.check(FAILING)
        )
        self.assertLessEqual(len(text.encode("utf-8")), 20_000)
        self.assertIn("**300 unexpected failures fail this job", text)
        self.assertIn("- **`getmap:t0`**", text)

    def test_workflow_commands(self):
        checked = self.check(["main:data-preconditions", "getmap:getmap-format"])
        lines = summarize.annotations(checked)
        self.assertEqual(
            lines,
            [
                "::error title=ets-wms13 unexpected failure::getmap:getmap-layers (Layer ets-radar) "
                "failed and is not on list.txt: Expected an image, got a service exception.",
                "::error title=ets-wms13 unexpected failure::basic:blue-lake-sanity-check "
                "failed and is not on list.txt: Layer cite:Lakes is not defined.",
                "::warning title=ets-wms13 expected failure did not fail::getmap:getmap-format "
                "is on list.txt but did not fail (passed): remove it from the list",
            ],
        )
        # The values a workflow command escapes.
        self.assertEqual(
            summarize.workflow_command("error", "a: b, c", "100% done\nnext"),
            "::error title=a%3A b%2C c::100%25 done%0Anext",
        )


class GateExitStatusTest(unittest.TestCase):
    """The script's verdict with --expected, as the workflow runs it."""

    def run_gate(self, expected, logs=(ENTRY, CLASSES), *extra):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "report_logs.xml", report(*logs))
            listed = write(tmp, "expected.txt", "# expected\n" + "".join(f"{n}\n" for n in expected))
            md, js = os.path.join(tmp, "s.md"), os.path.join(tmp, "s.json")
            result = run_script(path, "--expected", listed, "--markdown", md, "--json", js, *extra)
            text = doc = None
            if os.path.exists(md):
                with open(md, encoding="utf-8") as fh:
                    text = fh.read()
            if os.path.exists(js):
                with open(js, encoding="utf-8") as fh:
                    doc = json.load(fh)
            return result, text, doc

    def test_expected_failures_pass(self):
        result, text, doc = self.run_gate(FAILING)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("every failure is on", result.stderr)
        self.assertIn("**Every failure is expected", text)
        self.assertEqual(doc["expected_failures"]["unexpected"], [])
        self.assertEqual(sorted(doc["expected_failures"]["expected"]), sorted(FAILING))

    def test_an_unexpected_failure_fails(self):
        result, text, doc = self.run_gate(["main:data-preconditions", "basic:blue-lake-sanity-check"])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("1 failure not on", result.stderr)
        self.assertIn("  getmap:getmap-layers", result.stderr)
        self.assertIn("**1 unexpected failure fails this job", text)
        self.assertEqual(doc["expected_failures"]["unexpected"], ["getmap:getmap-layers"])
        # Without --annotate, stdout holds no workflow command.
        self.assertNotIn("::error", result.stdout)

    def test_an_unlisted_class_failure_fails(self):
        # The data-preconditions class's own log counts like a test.
        result, _, doc = self.run_gate(["getmap:getmap-layers", "basic:blue-lake-sanity-check"])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(doc["expected_failures"]["unexpected"], ["main:data-preconditions"])

    def test_a_listed_test_that_passes_warns(self):
        result, text, doc = self.run_gate(FAILING + ["getmap:getmap-format"], (ENTRY, CLASSES), "--annotate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("### Warning: expected failures that did not fail (1)", text)
        self.assertIn("warning: getmap:getmap-format is on", result.stderr)
        self.assertIn(
            "::warning title=ets-wms13 expected failure did not fail::getmap:getmap-format is on",
            result.stdout,
        )
        self.assertEqual(
            doc["expected_failures"]["not_failing"], [{"name": "getmap:getmap-format", "outcomes": ["passed"]}]
        )

    def test_annotate_prints_an_error_per_unexpected_failure(self):
        result, _, _ = self.run_gate(["main:data-preconditions"], (ENTRY, CLASSES), "--annotate")
        self.assertEqual(result.returncode, 1, result.stderr)
        errors = [line for line in result.stdout.splitlines() if line.startswith("::error ")]
        self.assertEqual(len(errors), 2)
        self.assertTrue(errors[0].startswith("::error title=ets-wms13 unexpected failure::getmap:getmap-layers"))

    def test_the_suite_did_not_run_stays_2(self):
        result, text, _ = self.run_gate(FAILING, (ENTRY,))
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertNotIn("unexpected failure", text)

    def test_a_malformed_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "report_logs.xml", report(ENTRY, CLASSES))
            listed = write(tmp, "expected.txt", "getmap:two-layers\nnot a name\n")
            md = os.path.join(tmp, "s.md")
            result = run_script(path, "--expected", listed, "--markdown", md, "--annotate")
            self.assertEqual(result.returncode, 2)
            self.assertIn("expected.txt:2: not a test name", result.stderr)
            self.assertTrue(result.stdout.startswith("::error title=ets-wms13 expected-failure list::"))
            self.assertFalse(os.path.exists(md))

    def test_a_missing_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write(tmp, "report_logs.xml", report(ENTRY, CLASSES))
            result = run_script(path, "--expected", os.path.join(tmp, "absent.txt"))
            self.assertEqual(result.returncode, 2)
            self.assertIn("cannot use the expected-failure list", result.stderr)


if __name__ == "__main__":
    unittest.main()
