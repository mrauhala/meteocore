#!/usr/bin/env python3
"""Write the synthetic curvilinear NetCDF fixture's CDL (#1174).

Usage: make_cdl.py <output dir>; then in that directory
`ncgen -k nc4 -o svalbard_curvilinear.nc svalbard_curvilinear.cdl`.

A grid with 2-D `lat(y, x)` and `lon(y, x)` auxiliary coordinates and
nothing else to place it: no 1-D coordinate variables on `y` and `x`, no
grid mapping. The nodes are those of a north polar stereographic plane on
a sphere (radius 6371 km, true scale at 70°N, 15°E along the plane's
negative y axis), 19 x 28 nodes 100 km apart, x -900…900 km, y 1100…-1600
km, rows stored from y = 1100 km down. The grid holds the North Pole
(node (9, 11)), runs south over Svalbard to 75.2°N along 15°E and 73.1°N
at its southern corners, and the antimeridian crosses it from the pole to
row 0 at 79.5°N. Latitude and longitude are the exact spherical inverse
of each node's x/y, as `cs2cs -f %.9f +proj=stere +lat_0=90 +lat_ts=70
+lon_0=15 +R=6371000 +to +proj=longlat +R=6371000` gives them (to the
ninth decimal at the corners); the engine reads only them.

Two fields, one step (2026-10-21T00Z) and no forecast_reference_time:

  column = the node's column index i (0…18)
  row    = the node's row index j (0…27)

Both are linear in the indices, so a read anywhere between nodes returns
the fractional (column, row) the server put the point at: what
crates/engine-netcdf/tests/curvilinear_reference.py computes, by brute
force over the CDL's own arrays.
"""

import math
import sys
from pathlib import Path

R = 6371000.0
K = (1.0 + math.sin(math.radians(70.0))) / 2.0
LON0 = 15.0
NX, NY = 19, 28
X0, Y0, STEP = -900_000.0, 1_100_000.0, 100_000.0


def inverse(x, y):
    """(lon, lat) of plane point (x, y): the spherical polar stereographic
    inverse, longitudes in [-180, 180)."""
    rho = math.hypot(x, y)
    lat = 90.0 - 2.0 * math.degrees(math.atan(rho / (2.0 * R * K)))
    lon = LON0 + math.degrees(math.atan2(x, -y))
    lon = (lon + 180.0) % 360.0 - 180.0
    return lon, lat


def rows(values, per_line):
    lines = [", ".join(values[i : i + per_line]) for i in range(0, len(values), per_line)]
    return ",\n    ".join(lines)


def cdl():
    lon, lat, column, row = [], [], [], []
    for j in range(NY):
        for i in range(NX):
            a, b = inverse(X0 + i * STEP, Y0 - j * STEP)
            # Node (9, 11) is the pole, where atan2 gives any longitude:
            # 0 by convention.
            lon.append(repr(round(a, 9) if (i, j) != (9, 11) else 0.0))
            lat.append(repr(round(b, 9)))
            column.append(str(i))
            row.append(str(j))
    out = [
        "netcdf svalbard_curvilinear {",
        "dimensions:",
        "\ttime = 1 ;",
        f"\ty = {NY} ;",
        f"\tx = {NX} ;",
        "variables:",
        "\tdouble time(time) ;",
        '\t\ttime:standard_name = "time" ;',
        '\t\ttime:units = "hours since 2026-10-21 00:00:00" ;',
        '\t\ttime:calendar = "standard" ;',
        "\tdouble lat(y, x) ;",
        '\t\tlat:standard_name = "latitude" ;',
        '\t\tlat:long_name = "latitude" ;',
        '\t\tlat:units = "degrees_north" ;',
        "\tdouble lon(y, x) ;",
        '\t\tlon:standard_name = "longitude" ;',
        '\t\tlon:long_name = "longitude" ;',
        '\t\tlon:units = "degrees_east" ;',
    ]
    for field, long_name in [("column", "Grid column index"), ("row", "Grid row index")]:
        out += [
            f"\tfloat {field}(time, y, x) ;",
            f'\t\t{field}:long_name = "{long_name}" ;',
            f'\t\t{field}:units = "1" ;',
            f'\t\t{field}:coordinates = "lon lat" ;',
        ]
    out += [
        "",
        "// global attributes:",
        '\t\t:Conventions = "CF-1.8" ;',
        '\t\t:title = "Synthetic curvilinear grid around Svalbard and the North Pole, '
        'MeteoCore test fixture" ;',
        "data:",
        "",
        " time = 0 ;",
        "",
        " lat =",
        f"    {rows(lat, 4)} ;",
        "",
        " lon =",
        f"    {rows(lon, 4)} ;",
        "",
        " column =",
        f"    {rows(column, NX)} ;",
        "",
        " row =",
        f"    {rows(row, NX)} ;",
        "}",
    ]
    return "\n".join(out) + "\n"


def main():
    target = Path(sys.argv[1])
    target.mkdir(parents=True, exist_ok=True)
    (target / "svalbard_curvilinear.cdl").write_text(cdl())


if __name__ == "__main__":
    main()
