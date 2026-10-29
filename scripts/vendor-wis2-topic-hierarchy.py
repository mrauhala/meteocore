#!/usr/bin/env python3
"""vendor-wis2-topic-hierarchy.py — flatten one WIS2 Topic Hierarchy release.

ds-core validates a collection's WIS2 topic (`[discovery] wis2_topic`, #1356)
against the earth-system-discipline levels of the WIS2 Topic Hierarchy
(https://github.com/wmo-im/wis2-topic-hierarchy). Upstream keeps them as one
`index.csv` per level: the names a level's children may take. This script
reads every one of a release tag and writes one table of every valid path,
`crates/core/assets/wis2-topic-hierarchy-<tag>/earth-system-discipline.tsv`:

    path<TAB>status<TAB>description

A leaf has no `index.csv` of its own; it is a row of its parent's. A level
listed by an `index-flat.csv` instead (space-based observations) holds two
levels per row, satellite then instrument, each with its own description.
Rows are sorted by path. Upstream's LICENSE (Apache-2.0) is copied next to
the table.

Bumping the release:
  1. Run `scripts/vendor-wis2-topic-hierarchy.py <tag>` (network: GitHub).
  2. Move PROVENANCE.md to the new directory with its version and checksums
     updated, point the `include_str!` in `crates/core/src/discovery.rs` at
     the new table, and remove the old directory.
  3. Run `cargo test -p ds-core discovery`.

`--source DIR` reads a local checkout of the tag instead of GitHub.
"""

import argparse
import csv
import io
import json
import pathlib
import sys
import urllib.request

REPO = "wmo-im/wis2-topic-hierarchy"
ROOT = "topic-hierarchy/earth-system-discipline"


def fetch(url):
    with urllib.request.urlopen(url, timeout=30) as response:
        return response.read().decode("utf-8")


def remote_files(tag):
    """Every CSV under the discipline tree, plus LICENSE, as {path: text}."""
    tree = json.loads(
        fetch(f"https://api.github.com/repos/{REPO}/git/trees/{tag}?recursive=1")
    )
    if tree.get("truncated"):
        sys.exit("the GitHub tree listing was truncated")
    paths = [
        item["path"]
        for item in tree["tree"]
        if item["type"] == "blob"
        and item["path"].startswith(ROOT + "/")
        and item["path"].endswith(".csv")
    ] + ["LICENSE"]
    raw = f"https://raw.githubusercontent.com/{REPO}/{tag}"
    return {path: fetch(f"{raw}/{path}") for path in paths}


def local_files(source):
    source = pathlib.Path(source)
    files = {
        str(path.relative_to(source)): path.read_text(encoding="utf-8")
        for path in (source / ROOT).rglob("*.csv")
    }
    files["LICENSE"] = (source / "LICENSE").read_text(encoding="utf-8")
    return files


def rows(files):
    """{path: (status, description)} for every level below the discipline."""
    out = {}

    def add(path, status, description):
        description = " ".join(description.split())
        if any(c in description for c in "\t\r\n"):
            sys.exit(f"{path}: description holds a tab or line break")
        if path in out:
            # 1.3.0 lists goes-14/imager twice, the second as the sounder.
            if out[path] != (status, description):
                print(f"{path}: listed twice; keeping the first row", file=sys.stderr)
            return
        out[path] = (status, description)

    for name, text in sorted(files.items()):
        if not name.startswith(ROOT + "/"):
            continue
        # The level the file lists the children of, "" for the disciplines.
        level = name[len(ROOT) + 1 :].rpartition("/")[0]
        prefix = level + "/" if level else ""
        reader = csv.DictReader(io.StringIO(text))
        if name.endswith("/index.csv"):
            for row in reader:
                # Some rows lack the empty Source column: their status is
                # then the last field.
                status = row["Status"] if row["Status"] is not None else row["Source"]
                add(prefix + row["Name"].strip(), status.strip(), row["Description"])
        elif name.endswith("/index-flat.csv"):
            satellites = {}
            for row in reader:
                satellite = prefix + row["satellite"].strip()
                status = row["Status"].strip()
                add(satellite + "/" + row["instrument"].strip(), status, row["instrument-description"])
                # A satellite is as current as its most current instrument.
                _, statuses = satellites.setdefault(satellite, (row["satellite-description"], set()))
                statuses.add(status)
            for satellite, (description, statuses) in satellites.items():
                status = "Operational" if "Operational" in statuses else sorted(statuses)[0]
                add(satellite, status, description)
        else:
            sys.exit(f"{name}: not an index.csv or index-flat.csv")
    for path in out:
        parent = path.rsplit("/", 1)[0]
        if "/" in path and parent not in out:
            sys.exit(f"{path}: its parent level {parent} is listed nowhere")
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("tag", help="release tag, e.g. 1.3.0")
    parser.add_argument("--source", help="a local checkout of the tag")
    args = parser.parse_args()

    files = local_files(args.source) if args.source else remote_files(args.tag)
    table = rows(files)
    repo_root = pathlib.Path(__file__).resolve().parent.parent
    out_dir = repo_root / "crates/core/assets" / f"wis2-topic-hierarchy-{args.tag}"
    out_dir.mkdir(parents=True, exist_ok=True)
    lines = [
        f"# WIS2 Topic Hierarchy {args.tag}: every earth-system-discipline level, one path per row.",
        f"# Flattened from https://github.com/{REPO}/tree/{args.tag}/{ROOT}",
        "# by scripts/vendor-wis2-topic-hierarchy.py; do not edit. Apache-2.0, see LICENSE.",
        "path\tstatus\tdescription",
    ]
    lines += [f"{path}\t{status}\t{description}" for path, (status, description) in sorted(table.items())]
    (out_dir / "earth-system-discipline.tsv").write_text("\n".join(lines) + "\n", encoding="utf-8")
    (out_dir / "LICENSE").write_text(files["LICENSE"], encoding="utf-8")
    print(f"Wrote {len(table)} paths to {out_dir.relative_to(repo_root)}", file=sys.stderr)


if __name__ == "__main__":
    main()
