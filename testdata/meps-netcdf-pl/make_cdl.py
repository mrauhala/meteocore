#!/usr/bin/env python3
"""Write the MEPS pressure-level fixture CDLs from MET Norway OPeNDAP ASCII
subsets.

Usage: make_cdl.py <dir with the .ascii responses> <output dir>

The inputs are the responses README.md lists: one
`<file>.nc.k<index>.ascii` per fixture file and pressure index, plus
`latlon.ascii`. Each CDL keeps the source's values, its attributes (from
the `.das` of the same family) and its storage: one whole 2-D field per
chunk, shuffle and deflate level 3. Build each `.nc` with
`ncgen -k nc4 -o <name>.nc <name>.cdl`.
"""

import re
import sys
from pathlib import Path

FIELDS = {
    # name: (long_name, standard_name, units)
    "air_temperature_pl": ("Air temperature pressure levels", "air_temperature", "K"),
    "x_wind_pl": ("Wind along x-coordinate on pressure levels", "x_wind", "m/s"),
    "y_wind_pl": ("Wind along y-coordinate on pressure levels", "y_wind", "m/s"),
    "geopotential_pl": ("Geopotential pressure levels", "geopotential", "m^2/s^2"),
    "relative_humidity_pl": (
        "Relative humidity pressure levels",
        "relative_humidity",
        "1",
    ),
}

# The source's pressure axis is 50, 100, …, 1000 hPa (13 levels, ascending);
# the fixture keeps four of them, at these indices, in the source's order.
LEVELS = {5: "300", 7: "500", 10: "850", 12: "1000"}

TIME_UNITS = "seconds since 1970-01-01 00:00:00 +00:00"
NAME = re.compile(r"meps_pl_(\d\d)_(\d{8})T(\d\d)Z\.nc\.k(\d+)\.ascii$")
NY, NX = 11, 10


def arrays(text):
    """Every `name[...]` block of an OPeNDAP ASCII response, as lists of
    value strings in file order."""
    out = {}
    lines = text.split("---------------------------------------------", 1)[1]
    lines = lines.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i].strip()
        head = re.match(r"^([A-Za-z_.0-9]+)(\[[\]\[0-9]*\])?(, (.*))?$", line)
        if not line or not head:
            i += 1
            continue
        name = head.group(1).split(".")[-1]
        if head.group(4) is not None:  # a scalar: `name, value`
            out.setdefault(name, [head.group(4)])
            i += 1
            continue
        values = []
        i += 1
        while i < len(lines) and lines[i].strip():
            row = lines[i].strip()
            if row.startswith("["):
                row = row.split("], ", 1)[1] if "], " in row else ""
                row = re.sub(r"^(\[\d+\])*,\s*", "", row)
            values.extend(v.strip() for v in row.split(",") if v.strip())
            i += 1
        out.setdefault(name, values)
    return out


def block(values, per_line):
    rows = [", ".join(values[i : i + per_line]) for i in range(0, len(values), per_line)]
    return ",\n    ".join(rows)


def cdl(stem, lead, run_date, run_hour, levels, latlon):
    """`levels`: the parsed response of each pressure index, in index order."""
    first = levels[0][1]
    frt = float(first["forecast_reference_time"][0])
    time = float(first["time"][0])
    assert time == frt + 3600 * int(lead), (stem, time, frt)
    for k, data in levels:
        assert float(data["time"][0]) == time, (stem, k)
        assert data["pressure"] == [f"{LEVELS[k]}.0"], (stem, k, data["pressure"])
    iso = f"{run_date[:4]}-{run_date[4:6]}-{run_date[6:]}T{run_hour}:00:00Z"
    out = [f"netcdf {stem} {{", "dimensions:"]
    out.append("\ttime = UNLIMITED ; // (1 currently)")
    out += [f"\tpressure = {len(levels)} ;", f"\tx = {NX} ;", f"\ty = {NY} ;", "variables:"]
    out += [
        "\tdouble time(time) ;",
        '\t\ttime:long_name = "time" ;',
        '\t\ttime:standard_name = "time" ;',
        f'\t\ttime:units = "{TIME_UNITS}" ;',
        "\tdouble forecast_reference_time ;",
        f'\t\tforecast_reference_time:units = "{TIME_UNITS}" ;',
        '\t\tforecast_reference_time:standard_name = "forecast_reference_time" ;',
        "\tfloat pressure(pressure) ;",
        '\t\tpressure:description = "pressure" ;',
        '\t\tpressure:long_name = "pressure" ;',
        '\t\tpressure:standard_name = "air_pressure" ;',
        '\t\tpressure:positive = "down" ;',
        '\t\tpressure:units = "hPa" ;',
    ]
    for axis, long_name in [("x", "x-coordinate"), ("y", "y-coordinate")]:
        out += [
            f"\tfloat {axis}({axis}) ;",
            f'\t\t{axis}:long_name = "{long_name} in Cartesian system" ;',
            f'\t\t{axis}:standard_name = "projection_{axis}_coordinate" ;',
            f'\t\t{axis}:units = "m" ;',
        ]
    out += [
        "\tint projection_lambert ;",
        '\t\tprojection_lambert:grid_mapping_name = "lambert_conformal_conic" ;',
        "\t\tprojection_lambert:standard_parallel = 63.3, 63.3 ;",
        "\t\tprojection_lambert:longitude_of_central_meridian = 15. ;",
        "\t\tprojection_lambert:latitude_of_projection_origin = 63.3 ;",
        "\t\tprojection_lambert:earth_radius = 6371000. ;",
    ]
    for name, units in [("longitude", "degree_east"), ("latitude", "degree_north")]:
        out += [
            f"\tdouble {name}(y, x) ;",
            f'\t\t{name}:units = "{units}" ;',
            f'\t\t{name}:long_name = "{name}" ;',
            f'\t\t{name}:standard_name = "{name}" ;',
        ]
    for name, (long_name, standard_name, units) in FIELDS.items():
        out += [
            f"\tfloat {name}(time, pressure, y, x) ;",
            f"\t\t{name}:_FillValue = 9.96921e+36f ;",
            f'\t\t{name}:long_name = "{long_name}" ;',
            f'\t\t{name}:standard_name = "{standard_name}" ;',
            f'\t\t{name}:units = "{units}" ;',
            f'\t\t{name}:grid_mapping = "projection_lambert" ;',
            f'\t\t{name}:coordinates = "longitude latitude" ;',
            f'\t\t{name}:_Storage = "chunked" ;',
            f"\t\t{name}:_ChunkSizes = 1, 1, {NY}, {NX} ;",
            f"\t\t{name}:_DeflateLevel = 3 ;",
            f'\t\t{name}:_Shuffle = "true" ;',
        ]
    out += [
        "",
        "// global attributes:",
        '\t\t:Conventions = "CF-1.6, ACDD-1.3" ;',
        '\t\t:institution = "Norwegian Meteorological Institute (MET Norway)" ;',
        '\t\t:license = "https://spdx.org/licenses/CC-BY-4.0 (CC-BY-4.0)" ;',
        f'\t\t:title = "Meps 2.5Km deterministic pressure level parameters {iso} + {int(lead)} hours" ;',
        '\t\t:ensemble_member = "00" ;',
        # As in the source: the run time, not the valid time.
        f'\t\t:min_time = "{iso}" ;',
        f'\t\t:max_time = "{iso}" ;',
        '\t\t:meteocore_fixture = "Every 100th row and column (y 0:100:1000, x 0:100:900) '
        "at pressure indices 5, 7, 10 and 12 (300, 500, 850, 1000 hPa) "
        f"of meps25epsarchive/{run_date[:4]}/{run_date[4:6]}/{run_date[6:]}/{run_hour}/"
        f"member_00/meps_pl_{lead}_{run_date}T{run_hour}Z.nc on thredds.met.no, "
        'fetched 2026-10-18 through OPeNDAP" ;',
        "data:",
        "",
        f" time = {first['time'][0]} ;",
        "",
        f" forecast_reference_time = {first['forecast_reference_time'][0]} ;",
        "",
        f" pressure = {', '.join(LEVELS[k] for k, _ in levels)} ;",
    ]
    for axis, n in [("x", NX), ("y", NY)]:
        values = latlon[axis]
        assert len(values) == n, (stem, axis, len(values))
        for _, data in levels:
            assert data[axis] == values, (stem, axis)
        out += ["", f" {axis} = {', '.join(values)} ;"]
    out += ["", " projection_lambert = 0 ;"]
    for name in ["longitude", "latitude"]:
        values = latlon[name]
        assert len(values) == NY * NX, (name, len(values))
        out += ["", f" {name} =\n    {block(values, NX)} ;"]
    for name in FIELDS:
        values = []
        for k, data in levels:
            assert len(data[name]) == NY * NX, (stem, name, k, len(data[name]))
            values += data[name]
        out += ["", f" {name} =\n    {block(values, NX)} ;"]
    out += ["}", ""]
    return "\n".join(out)


def main():
    source, target = Path(sys.argv[1]), Path(sys.argv[2])
    latlon = arrays((source / "latlon.ascii").read_text())
    files = {}
    for path in source.glob("meps_pl_*.nc.k*.ascii"):
        lead, run_date, run_hour, k = NAME.search(path.name).groups()
        stem = path.name.split(".nc.", 1)[0]
        files.setdefault((stem, lead, run_date, run_hour), []).append(
            (int(k), arrays(path.read_text()))
        )
    for (stem, lead, run_date, run_hour), levels in sorted(files.items()):
        levels.sort(key=lambda level: level[0])
        assert [k for k, _ in levels] == sorted(LEVELS), (stem, [k for k, _ in levels])
        (target / f"{stem}.cdl").write_text(cdl(stem, lead, run_date, run_hour, levels, latlon))


if __name__ == "__main__":
    main()
