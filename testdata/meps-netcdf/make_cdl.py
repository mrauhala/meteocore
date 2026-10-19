#!/usr/bin/env python3
"""Write the MEPS fixture CDLs from MET Norway OPeNDAP ASCII subsets.

Usage: make_cdl.py <dir with the .ascii responses> <output dir>

The inputs are the responses README.md lists: one `<file>.nc.ascii` per
fixture file, plus `latlon.ascii`. Each CDL keeps the source's values, its
attributes (from the `.das` of the same run) and its storage: one whole 2-D
field per chunk, shuffle and deflate level 3. Build each `.nc` with
`ncgen -k nc4 -o <name>.nc <name>.cdl`.
"""

import re
import sys
from pathlib import Path

FIELDS = {
    # name: (level dimension, long_name, standard_name, units)
    "air_temperature_2m": (
        "height1",
        "Screen level temperature (T2M)",
        "air_temperature",
        "K",
    ),
    "relative_humidity_2m": (
        "height1",
        "Screen level relative humidity (RH2M)",
        "relative_humidity",
        "1",
    ),
    "x_wind_10m": (
        "height6",
        "10 metre wind along x-coordinate (U10M)",
        "x_wind",
        "m/s",
    ),
    "y_wind_10m": (
        "height6",
        "10 metre wind along y-coordinate (V10M)",
        "y_wind",
        "m/s",
    ),
    "wind_speed_of_gust": ("height6", "Wind gust", "wind_speed_of_gust", "m/s"),
    "air_pressure_at_sea_level": (
        "height_above_msl",
        "Mean Sea Level Pressure (MSLP)",
        "air_pressure_at_sea_level",
        "Pa",
    ),
    "precipitation_amount_acc": (
        "height0",
        "Accumulated total precipitation",
        "precipitation_amount",
        "kg/m^2",
    ),
    "cloud_area_fraction": (
        "height0",
        "Total cloud cover (TCC)",
        "cloud_area_fraction",
        "1",
    ),
}

LEVELS = {
    # name: (description, value)
    "height0": ("height above ground", "0"),
    "height1": ("height above ground", "2"),
    "height6": ("height above ground", "10"),
    "height_above_msl": ("height above MSL", "0"),
}

TIME_UNITS = "seconds since 1970-01-01 00:00:00 +00:00"
NAME = re.compile(r"meps_sfc_(\d\d)_(\d{8})T(\d\d)Z\.nc\.ascii$")
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


def cdl(stem, lead, run_date, run_hour, data, latlon):
    frt = float(data["forecast_reference_time"][0])
    time = float(data["time"][0])
    assert time == frt + 3600 * int(lead), (stem, time, frt)
    iso = f"{run_date[:4]}-{run_date[4:6]}-{run_date[6:]}T{run_hour}:00:00Z"
    out = [f"netcdf {stem} {{", "dimensions:"]
    out.append("\ttime = UNLIMITED ; // (1 currently)")
    for level in LEVELS:
        out.append(f"\t{level} = 1 ;")
    out += [f"\tx = {NX} ;", f"\ty = {NY} ;", "variables:"]
    out += [
        "\tdouble time(time) ;",
        '\t\ttime:long_name = "time" ;',
        '\t\ttime:standard_name = "time" ;',
        f'\t\ttime:units = "{TIME_UNITS}" ;',
        "\tdouble forecast_reference_time ;",
        f'\t\tforecast_reference_time:units = "{TIME_UNITS}" ;',
        '\t\tforecast_reference_time:standard_name = "forecast_reference_time" ;',
    ]
    for level, (description, _) in LEVELS.items():
        out += [
            f"\tfloat {level}({level}) ;",
            f'\t\t{level}:description = "{description}" ;',
            f'\t\t{level}:long_name = "height" ;',
            f'\t\t{level}:positive = "up" ;',
            f'\t\t{level}:units = "m" ;',
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
    for name, units in [("latitude", "degree_north"), ("longitude", "degree_east")]:
        out += [
            f"\tdouble {name}(y, x) ;",
            f'\t\t{name}:units = "{units}" ;',
            f'\t\t{name}:long_name = "{name}" ;',
            f'\t\t{name}:standard_name = "{name}" ;',
        ]
    for name, (level, long_name, standard_name, units) in FIELDS.items():
        out += [
            f"\tfloat {name}(time, {level}, y, x) ;",
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
        f'\t\t:title = "Meps 2.5Km deterministic surface parameters {iso} + {lead} hours" ;',
        '\t\t:ensemble_member = "00" ;',
        # As in the source: the run time, not the valid time.
        f'\t\t:min_time = "{iso}" ;',
        f'\t\t:max_time = "{iso}" ;',
        '\t\t:meteocore_fixture = "Every 100th row and column (y 0:100:1000, x 0:100:900) '
        f'of meps25epsarchive/{run_date[:4]}/{run_date[4:6]}/{run_date[6:]}/{run_hour}/'
        f'member_00/meps_sfc_{lead}_{run_date}T{run_hour}Z.nc on thredds.met.no, '
        'fetched 2026-10-17 through OPeNDAP" ;',
        "data:",
        "",
        f" time = {data['time'][0]} ;",
        "",
        f" forecast_reference_time = {data['forecast_reference_time'][0]} ;",
    ]
    for level, (_, value) in LEVELS.items():
        out += ["", f" {level} = {value} ;"]
    for axis, n in [("x", NX), ("y", NY)]:
        values = data[axis]
        assert len(values) == n, (stem, axis, len(values))
        out += ["", f" {axis} = {', '.join(values)} ;"]
    out += ["", " projection_lambert = 0 ;"]
    for name in ["latitude", "longitude"]:
        values = latlon[name]
        assert len(values) == NY * NX, (name, len(values))
        out += ["", f" {name} =\n    {block(values, NX)} ;"]
    for name in FIELDS:
        values = data[name]
        assert len(values) == NY * NX, (stem, name, len(values))
        out += ["", f" {name} =\n    {block(values, NX)} ;"]
    out += ["}", ""]
    return "\n".join(out)


def main():
    source, target = Path(sys.argv[1]), Path(sys.argv[2])
    latlon = arrays((source / "latlon.ascii").read_text())
    for path in sorted(source.glob("meps_sfc_*.nc.ascii")):
        lead, run_date, run_hour = NAME.search(path.name).groups()
        stem = path.name.removesuffix(".nc.ascii")
        data = arrays(path.read_text())
        (target / f"{stem}.cdl").write_text(cdl(stem, lead, run_date, run_hour, data, latlon))


if __name__ == "__main__":
    main()
