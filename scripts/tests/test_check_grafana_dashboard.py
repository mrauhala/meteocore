"""Tests for scripts/check_grafana_dashboard.py.

Run from the repository root, as CI does:

    python3 -m unittest discover -s scripts/tests -v

The dashboard tests inject one defect each into a copy of the shipped
dashboard (a dashboard.grafana.app/v2 resource) and check it against the
families read from this tree and the vendored Grafana schema. The
extraction tests read small Rust snippets, the schema tests small CUE
snippets, and the CLI tests run the script on a throwaway tree. Standard
library only.
"""

import copy
import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check_grafana_dashboard.py"
DASHBOARD = ROOT / "docker" / "grafana" / "dashboards" / "meteocore-overview.json"
sys.path.insert(0, str(SCRIPT.parent))

import check_grafana_dashboard as cgd  # noqa: E402


# -- navigating a v2 resource ------------------------------------------------

def tabs(d):
    return d["spec"]["layout"]["spec"]["tabs"]


def grid_items(layout):
    """Every GridLayoutItem spec under a layout, in order."""
    kind, spec = layout["kind"], layout["spec"]
    if kind == "GridLayout":
        return [i["spec"] for i in spec["items"]]
    if kind == "RowsLayout":
        return [i for r in spec["rows"] for i in grid_items(r["spec"]["layout"])]
    if kind == "TabsLayout":
        return [i for t in spec["tabs"] for i in grid_items(t["spec"]["layout"])]
    return []


def first_row(d):
    """The first row: the first tab laid out in rows."""
    tab = next(t for t in tabs(d) if t["spec"]["layout"]["kind"] == "RowsLayout")
    return tab["spec"]["layout"]["spec"]["rows"][0]["spec"]


def row_items(d):
    return first_row(d)["layout"]["spec"]["items"]


def element(d, item):
    return d["spec"]["elements"][item["spec"]["element"]["name"]]["spec"]


def first_panel(d):
    """The first panel of the first row: a time series."""
    return element(d, row_items(d)[0])


def set_expr(expr):
    def mutate(d):
        first_panel(d)["data"]["spec"]["queries"][0]["spec"]["query"]["spec"]["expr"] = expr
    return mutate


def unquery(family):
    def mutate(d):
        for e in d["spec"]["elements"].values():
            qs = e["spec"]["data"]["spec"]
            qs["queries"] = [q for q in qs["queries"]
                             if not re.search(rf"\b{family}\b",
                                              q["spec"]["query"]["spec"].get("expr", ""))]
    return mutate


def overlap(d):
    a, b = row_items(d)[:2]
    b["spec"].update(x=a["spec"]["x"], y=a["spec"]["y"])


def wide(d):
    row_items(d)[0]["spec"].update(x=0, width=25)


def collapse(d):
    first_row(d)["collapse"] = True


def unplace(d):
    first_row(d)["layout"]["spec"]["items"].pop()


def place_twice(d):
    items = row_items(d)
    items.append(copy.deepcopy(items[0]))
    items[-1]["spec"]["y"] = 100


def place_missing(d):
    items = row_items(d)
    items.append(copy.deepcopy(items[0]))
    items[-1]["spec"].update(y=100, element={"kind": "ElementReference", "name": "panel-999"})


def rows_at_top(d):
    d["spec"]["layout"] = tabs(d)[1]["spec"]["layout"]


def duplicate_id(d):
    a, b = (element(d, i) for i in row_items(d)[:2])
    b["id"] = a["id"]


def misnamed_element(d):
    els = d["spec"]["elements"]
    name = row_items(d)[0]["spec"]["element"]["name"]
    els["chart-x"] = els.pop(name)
    row_items(d)[0]["spec"]["element"]["name"] = "chart-x"


def set_top(key, value):
    def mutate(d):
        d[key] = value
    return mutate


def classic(d):
    d.clear()
    d.update({"uid": "meteocore-overview", "title": "MeteoCore", "panels": []})


def set_field(path, value):
    """Set (or with None remove) a field of the first panel's spec."""
    def mutate(d):
        node = first_panel(d)
        for key in path[:-1]:
            node = node.setdefault(key, {})
        if value is None:
            node.pop(path[-1], None)
        else:
            node[path[-1]] = value
    return mutate


def set_defaults(key, value):
    return set_field(["vizConfig", "spec", "fieldConfig", "defaults", key], value)


def set_datasource(name):
    def mutate(d):
        first_panel(d)["data"]["spec"]["queries"][0]["spec"]["query"]["datasource"] = {"name": name}
    return mutate


def link(url):
    return set_defaults("links", [{"title": "x", "url": url}])


def annotation_expr_drift(d):
    d["spec"]["annotations"][0]["spec"]["legacyOptions"]["expr"] = "up == 0"


def bad_enum(d):
    d["spec"]["variables"][0]["spec"]["hide"] = "hidden"


def unknown_field(d):
    first_row(d)["collapsed"] = False


def missing_field(d):
    first_panel(d)["data"]["spec"].pop("queryOptions")


def wrong_type(d):
    row_items(d)[0]["spec"]["x"] = "0"


DASH = "/d/meteocore-overview/meteocore?"

# (defect, mutation, a substring of the problem it must cause)
MUTATIONS = [
    ("unqueried family", unquery("http_requests_total"),
     "family 'http_requests_total' (counter) is not queried by any dashboard panel"),
    ("unknown metric", set_expr('sum(rate(http_requests_totl{job=~"$job"}[5m]))'),
     "metric 'http_requests_totl' is not a registered /metrics family"),
    ("unknown __name__ regex", set_expr('sum({job=~"$job", __name__=~".+_cahce_bytes"})'),
     '__name__=~".+_cahce_bytes" matches no registered family'),
    ("__name__ regex matching everything", set_expr('count({job=~"$job", __name__=~".+"})'),
     "matches every family"),
    ("histogram without a suffix", set_expr('sum(rate(http_request_duration_seconds[5m]))'),
     "'http_request_duration_seconds' is a histogram"),
    ("wrong apiVersion", set_top("apiVersion", "dashboard.grafana.app/v1"),
     'apiVersion is "dashboard.grafana.app/v1"'),
    ("wrong kind", set_top("kind", "Folder"), 'kind is "Folder"'),
    ("bad uid", set_top("metadata", {"name": "MeteoCore Overview"}), "must be the dashboard uid"),
    ("classic dashboard JSON", classic, "is classic dashboard JSON"),
    ("rows at the top", rows_at_top, "the top level must be a TabsLayout"),
    ("overlap", overlap, "overlaps panel-"),
    ("outside the grid", wide, "leaves the 24-column grid"),
    ("collapsed row", collapse, "is collapsed: sections stay open"),
    ("element not placed", unplace, "is not placed in the layout"),
    ("element placed twice", place_twice, "is placed 2 times"),
    ("missing element", place_missing, "references element 'panel-999', which does not exist"),
    ("duplicate panel id", duplicate_id, "is used by"),
    ("element not named after its id", misnamed_element, "is element 'chart-x': name it"),
    ("undefined variable",
     set_expr('sum(rate(http_requests_total{job=~"$job", x=~"$nosuchvar"}[5m]))'),
     "variable $nosuchvar is not defined"),
    ("variable outside a label matcher", set_expr('sum(${api}_requests_total)'),
     "$api is used outside a label matcher"),
    ("__name__ from a non-name variable", set_expr('sum({__name__=~"$api"})'),
     '__name__=~"$api" uses a variable whose values are not metric names'),
    ("hard-coded URL", link("https://grafana.example.com/d/x"), "absolute URL"),
    ("hard-coded host name", set_field(["description"], "Scraped from prom.example.com."),
     "hard-coded host 'prom.example.com'"),
    ("hard-coded IP", set_field(["description"], "Scraped from 10.1.2.3:9090."),
     "hard-coded host '10.1.2.3'"),
    ("hard-coded datasource", set_datasource("abc123"), "datasource 'abc123' is hard-coded"),
    ("no datasource", set_datasource(None), "names no datasource"),
    ("datasource of the wrong type", set_datasource("${DS_LOKI}"),
     "a prometheus query runs on $DS_LOKI, a loki datasource"),
    ("missing title", set_field(["title"], None), "has no title"),
    ("missing description", set_field(["description"], None), "has no description"),
    ("missing unit", set_defaults("unit", None), "has no unit"),
    ("link to a missing panel", link(f"{DASH}viewPanel=panel-999"),
     "links to panel 999, which does not exist"),
    ("link to another dashboard", link("/d/somewhere-else/m"),
     "links to dashboard uid 'somewhere-else'"),
    ("link to a missing tab", link(f"{DASH}dtab=nowhere"), "links to tab 'nowhere'"),
    ("link to a missing sub-tab", link(f"{DASH}dtab=engines&engines-dtab=nowhere"),
     "links to tab 'Engines › nowhere'"),
    ("link to a panel on another tab", link(f"{DASH}dtab=logs&viewPanel=panel-2"),
     "links to panel 2 on tab 'Logs', but the panel is on 'Overview'"),
    ("legend sorted by a hidden calc",
     set_field(["vizConfig", "spec", "options", "legend"],
               {"displayMode": "table", "calcs": ["mean"], "sortBy": "Max"}),
     "legend sorts by 'Max'"),
    ("annotation query and legacy expr differ", annotation_expr_drift,
     "legacyOptions.expr differs from query.spec.expr"),
    ("schema: bad enum", bad_enum,
     'schema: spec.variables[0].spec.hide: "hidden" is none of "dontHide"'),
    ("schema: unknown field", unknown_field,
     "field 'collapsed' is not in the schema, so Grafana drops it"),
    ("schema: missing field", missing_field, "missing field 'queryOptions'"),
    ("schema: wrong type", wrong_type, '.spec.x: "0" is not int'),
]


class DashboardTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.families, problems = cgd.extract_families(cgd.rust_sources(ROOT))
        if problems:
            raise AssertionError(f"the tree's families do not extract cleanly: {problems}")
        cls.schema, why = cgd.load_schema()
        if why:
            raise AssertionError(why)
        cls.dash = json.loads(DASHBOARD.read_text(encoding="utf-8"))

    def problems(self, dash):
        return cgd.check_dashboards(self.families, [("dash.json", dash)], self.schema)[0]

    def test_shipped_dashboard_is_clean(self):
        self.assertEqual(self.problems(self.dash), [])

    def test_each_defect_is_caught(self):
        for defect, mutate, expected in MUTATIONS:
            with self.subTest(defect):
                dash = copy.deepcopy(self.dash)
                mutate(dash)
                problems = self.problems(dash)
                self.assertTrue(any(expected in p for p in problems),
                                f"no problem contains {expected!r}: {problems}")

    def test_classic_json_does_not_flood_coverage(self):
        dash = copy.deepcopy(self.dash)
        classic(dash)
        self.assertEqual(len(self.problems(dash)), 1)

    def test_problems_name_the_file_and_panel(self):
        dash = copy.deepcopy(self.dash)
        set_expr("sum(no_such_metric_total)")(dash)
        p = first_panel(dash)
        self.assertIn(f'dash.json: panel {p["id"]} "{p["title"]}" query A: metric '
                      "'no_such_metric_total'", "\n".join(self.problems(dash)))

    def test_layout_problems_name_the_tab_and_row(self):
        dash = copy.deepcopy(self.dash)
        collapse(dash)
        tab = next(t for t in tabs(dash) if t["spec"]["layout"]["kind"] == "RowsLayout")
        self.assertIn(f'dash.json: tab "{tab["spec"]["title"]}" › row "{first_row(dash)["title"]}"'
                      ": is collapsed", "\n".join(self.problems(dash)))

    def test_unqueried_family_points_at_its_registration(self):
        dash = copy.deepcopy(self.dash)
        unquery("http_requests_total")(dash)
        where = self.families["http_requests_total"].where
        self.assertRegex(where, r"^crates/server/src/\w+\.rs:\d+$")
        self.assertIn(f"{where}: family 'http_requests_total'", "\n".join(self.problems(dash)))

    def test_overview_is_the_first_tab(self):
        self.assertEqual(tabs(self.dash)[0]["spec"]["title"], "Overview")


class SchemaTest(unittest.TestCase):
    def test_vendored_schema_is_upstream_and_reads(self):
        schema, why = cgd.load_schema()
        self.assertIsNone(why)
        self.assertIn("DashboardSpec", schema.defs)
        self.assertIn("TabsLayoutKind", schema.defs)

    def test_edited_schema_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            edited = Path(tmp) / "dashboard_v2_spec.cue"
            edited.write_bytes(cgd.SCHEMA.read_bytes() + b"\n// local edit\n")
            saved, cgd.SCHEMA = cgd.SCHEMA, edited
            try:
                schema, why = cgd.load_schema()
            finally:
                cgd.SCHEMA = saved
        self.assertIsNone(schema)
        self.assertIn("differs from grafana/grafana v13.1.8", why)

    def check(self, src, value, root="A"):
        return cgd.CueSchema("package demo\n" + src).check(value, ("ref", root), "a")

    def test_required_optional_and_unknown_fields(self):
        src = 'A: {\n\tname: string\n\tnote?: string\n\tsize: int | *3\n}\n'
        self.assertEqual(self.check(src, {"name": "x", "size": 1}), [])
        self.assertEqual(self.check(src, {"size": 1}), ["a: missing field 'name'"])
        self.assertEqual(self.check(src, {"name": "x", "size": 1, "nmae": 1}),
                         ["a: field 'nmae' is not in the schema, so Grafana drops it"])
        self.assertEqual(self.check(src, {"name": "x", "size": 1.5}),
                         ["a.size: 1.5 is none of int | 3"])

    def test_disjunctions_report_inside_the_meant_kind(self):
        src = ('A: B | C\nB: {\n\tkind: "B"\n\tspec: {n: int}\n}\n'
               'C: {\n\tkind: "C"\n\tspec: {s: string}\n}\n'
               'Mode: "on" | *"off" @cog(kind="enum")\n')
        self.assertEqual(self.check(src, {"kind": "C", "spec": {"s": "x"}}), [])
        self.assertEqual(self.check(src, {"kind": "C", "spec": {"s": 1}}), ["a.spec.s: 1 is not string"])
        self.assertEqual(self.check(src, "dim", "Mode"), ['a: "dim" is none of "on" | "off"'])

    def test_lists_patterns_and_open_structs(self):
        src = ('A: {\n\titems: [...B] | *[]\n\tlabels?: [string]: string\n\tcustom?: {...}\n}\n'
               'B: {x: int}\n')
        self.assertEqual(self.check(src, {"items": [{"x": 1}], "labels": {"a": "b"},
                                          "custom": {"any": [1]}}), [])
        self.assertEqual(self.check(src, {"items": [{"x": "1"}]}), ['a.items[0].x: "1" is not int'])
        self.assertEqual(self.check(src, {"items": [], "labels": {"a": 1}}),
                         ["a.labels.a: 1 is not string"])

    def test_unknown_reference_fails_to_load(self):
        with self.assertRaisesRegex(ValueError, "undefined: Missing"):
            cgd.CueSchema("package demo\nA: Missing\n")


EXTRACTION_SRC = r'''
use prometheus::{HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts};

/// IntGauge::new("in_a_doc_comment", "never registered")
fn int_gauge(name: &str, help: &str) -> IntGauge {
    IntGauge::new(name, help).unwrap()
}

struct Delta(IntCounter);

impl Delta {
    fn new(name: &str, help: &str) -> Self {
        Self::with_opts(Opts::new(name, help))
    }
    fn with_opts(opts: Opts) -> Self {
        Delta(IntCounter::with_opts(opts).unwrap())
    }
}

fn build() {
    let requests = IntCounterVec::new(Opts::new("demo_requests_total", "Requests"), &["api"]);
    let latency = HistogramVec::new(HistogramOpts::new("demo_latency_seconds", "Latency"), &["api"]);
    let up = int_gauge("demo_up", "1 when up");
    let fills = Delta::new("demo_fills_total", "Fills");
    let mk = |name: &str, help: &str| IntGauge::new(name, help).unwrap();
    let rss = mk("demo_rss_bytes", "Resident bytes");
    // IntGauge::new("commented_out", "never registered");
    let text = "IntGauge::new(\"in_a_string\", \"never registered\")";
    let quote = '"';
    let resident = int_gauge("demo_resident_bytes", "Resident bytes");
}

#[cfg(test)]
mod tests {
    #[test]
    fn registers() {
        super::int_gauge("test_only_gauge", "never served");
    }
}
'''


def extract(src, templates=None):
    return cgd.extract_families([cgd.RustFile("crates/demo/src/lib.rs", src)], templates or {})


class ExtractionTest(unittest.TestCase):
    def test_registration_forms(self):
        families, problems = extract(EXTRACTION_SRC)
        self.assertEqual(problems, [])
        self.assertEqual({n: f.kind for n, f in families.items()}, {
            "demo_requests_total": "counter",
            "demo_latency_seconds": "histogram",
            "demo_up": "gauge",
            "demo_fills_total": "counter",
            "demo_rss_bytes": "gauge",
            "demo_resident_bytes": "gauge",
        })
        self.assertEqual(families["demo_up"].where, "crates/demo/src/lib.rs:23")

    def test_unresolvable_name_fails(self):
        _, problems = extract('''
            fn register(n: &str) {
                let name = n.to_uppercase();
                IntGauge::new(&name, "help").unwrap();
            }''')
        self.assertEqual(len(problems), 1)
        self.assertIn("cannot read the metric name `&name` passed to IntGauge::new", problems[0])

    def test_format_outside_templates_fails(self):
        _, problems = extract('''
            fn int_gauge(name: &str, help: &str) -> IntGauge { IntGauge::new(name, help).unwrap() }
            fn cache_set(prefix: &str) { int_gauge(&format!("{prefix}_bytes"), "Bytes"); }''')
        self.assertEqual(len(problems), 1)
        self.assertIn("add cache_set to TEMPLATES", problems[0])

    TEMPLATE_SRC = '''
        fn int_gauge(name: &str, help: &str) -> IntGauge { IntGauge::new(name, help).unwrap() }
        struct Set;
        impl Set {
            fn new(prefix: &str, with_entries: bool) -> Self {
                int_gauge(&format!("{prefix}_bytes"), "Bytes held");
                if with_entries { int_gauge(&format!("{prefix}_entries"), "Entries held"); }
                Set
            }
        }
        static A: LazyLock<Set> = LazyLock::new(|| Set::new("a_cache", true));
        static B: LazyLock<Set> = LazyLock::new(|| Set::new("b_cache", false));'''

    def test_template_families(self):
        templates = {"Set::new": {"parts": {"{prefix}_bytes", "{prefix}_entries"},
                                  "families": {"bytes": None, "entries": 1}}}
        families, problems = extract(self.TEMPLATE_SRC, templates)
        self.assertEqual(problems, [])
        self.assertEqual(sorted(families), ["a_cache_bytes", "a_cache_entries", "b_cache_bytes"])

    def test_template_drift_fails(self):
        templates = {"Set::new": {"parts": {"{prefix}_bytes"}, "families": {"bytes": None}}}
        _, problems = extract(self.TEMPLATE_SRC, templates)
        self.assertEqual(len(problems), 1)
        self.assertIn("Set::new changed the names it builds (+{prefix}_entries)", problems[0])

    def test_shipped_templates_match_the_tree(self):
        files = cgd.rust_sources(ROOT)
        defined = {d.name for f in files for d in f.defs}
        self.assertLessEqual(set(cgd.TEMPLATES), defined)


DEMO_SRC = '''
use prometheus::{IntCounterVec, IntGauge, Opts};

fn int_gauge(name: &str, help: &str) -> IntGauge {
    IntGauge::new(name, help).unwrap()
}

static UP: LazyLock<IntGauge> = LazyLock::new(|| int_gauge("demo_up", "1 when up"));
static REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(Opts::new("demo_requests_total", "Requests"), &["api"]).unwrap()
});

#[cfg(test)]
mod metrics_tests;
'''


def demo_query(expr):
    return {"kind": "DataQuery", "group": "prometheus", "version": "v0",
            "datasource": {"name": "${DS_PROMETHEUS}"}, "spec": {"expr": expr}}


def demo_dashboard(*exprs):
    """A minimal v2 resource with one tab and one time series."""
    panel = {"kind": "Panel", "spec": {
        "id": 1, "title": "Demo", "description": "Demo series.", "links": [],
        "data": {"kind": "QueryGroup", "spec": {
            "queries": [{"kind": "PanelQuery", "spec": {
                "refId": chr(65 + i), "hidden": False, "query": demo_query(e)}}
                for i, e in enumerate(exprs)],
            "transformations": [], "queryOptions": {}}},
        "vizConfig": {"kind": "VizConfig", "group": "timeseries", "version": "", "spec": {
            "options": {}, "fieldConfig": {"defaults": {"unit": "short"}, "overrides": []}}}}}
    variable_common = {"current": {"text": "", "value": ""}, "options": [], "multi": False,
                       "includeAll": False, "regex": "", "refresh": "onDashboardLoad",
                       "hide": "dontHide", "skipUrlSync": False, "allowCustomValue": True}
    return {
        "apiVersion": "dashboard.grafana.app/v2", "kind": "Dashboard",
        "metadata": {"name": "demo"},
        "spec": {
            "title": "Demo", "tags": [], "cursorSync": "Off", "preload": False, "links": [],
            "timeSettings": {"from": "now-1h", "to": "now", "autoRefresh": "",
                             "autoRefreshIntervals": [], "hideTimepicker": False,
                             "fiscalYearStartMonth": 0},
            "variables": [
                {"kind": "DatasourceVariable", "spec": {
                    "name": "DS_PROMETHEUS", "pluginId": "prometheus", **variable_common}},
                {"kind": "QueryVariable", "spec": {
                    "name": "job", "query": {**demo_query(""), "spec": {
                        "__legacyStringValue": "label_values(demo_up, job)"}},
                    "definition": "label_values(demo_up, job)", "sort": "alphabeticalAsc",
                    **variable_common}},
            ],
            "annotations": [],
            "layout": {"kind": "TabsLayout", "spec": {"tabs": [
                {"kind": "TabsLayoutTab", "spec": {"title": "Demo", "layout": {
                    "kind": "GridLayout", "spec": {"items": [
                        {"kind": "GridLayoutItem", "spec": {
                            "x": 0, "y": 0, "width": 24, "height": 8,
                            "element": {"kind": "ElementReference", "name": "panel-1"}}}]}}}}]}},
            "elements": {"panel-1": panel},
        },
    }


class CommandLineTest(unittest.TestCase):
    def run_script(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), *args],
                              capture_output=True, text=True, check=False)

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        src = self.root / "crates" / "demo" / "src"
        src.mkdir(parents=True)
        (src.parent / "Cargo.toml").write_text(
            '[package]\nname = "demo"\n\n[dependencies]\nprometheus = "0.14"\n')
        (src / "main.rs").write_text(DEMO_SRC)
        (src / "metrics_tests.rs").write_text(
            'fn t() { IntGauge::new("test_only_gauge", "never served").unwrap(); }\n')
        self.dashboards = self.root / "docker" / "grafana" / "dashboards"
        self.dashboards.mkdir(parents=True)

    def write_dashboard(self, dash):
        (self.dashboards / "demo.json").write_text(json.dumps(dash))

    def test_shipped_tree_passes(self):
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertTrue(r.stdout.startswith("check_grafana_dashboard: OK"), r.stdout)

    def test_clean_demo_tree_passes(self):
        self.write_dashboard(demo_dashboard('sum(demo_up{job=~"$job"})',
                                            'sum(rate(demo_requests_total{job=~"$job"}[5m]))'))
        r = self.run_script("--root", str(self.root))
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("2 families from crates/demo/src/main.rs", r.stdout)
        self.assertIn("demo.json (1 tabs, 0 sub-tabs, 0 rows, 1 panels)", r.stdout)

    def test_unqueried_family_and_unknown_metric_fail(self):
        self.write_dashboard(demo_dashboard('sum(demo_up{job=~"$job"})', "sum(demo_upp)"))
        r = self.run_script("--root", str(self.root))
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("crates/demo/src/main.rs:10: family 'demo_requests_total' (counter) "
                      "is not queried by any dashboard panel", r.stdout)
        self.assertIn('docker/grafana/dashboards/demo.json: panel 1 "Demo" query B: '
                      "metric 'demo_upp' is not a registered /metrics family", r.stdout)

    def test_repeated_key_fails(self):
        text = json.dumps(demo_dashboard('sum(demo_up{job=~"$job"})',
                                         'sum(rate(demo_requests_total{job=~"$job"}[5m]))'))
        (self.dashboards / "demo.json").write_text(text.replace('"title": "Demo",',
                                                                '"title": "Demo", "title": "Again",', 1))
        r = self.run_script("--root", str(self.root))
        self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
        self.assertIn("demo.json: key 'title' appears twice in one object", r.stdout)

    def test_invalid_json_fails(self):
        (self.dashboards / "demo.json").write_text("{")
        r = self.run_script("--root", str(self.root))
        self.assertEqual(r.returncode, 1)
        self.assertIn("demo.json: cannot read the dashboard", r.stdout)

    def test_missing_tree_exits_2(self):
        r = self.run_script("--root", str(self.root / "nowhere"))
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
