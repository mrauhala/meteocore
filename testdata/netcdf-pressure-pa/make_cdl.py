#!/usr/bin/env python3
"""Write `pressure_levels_pa.cdl`, a synthetic CF-NetCDF file on pressure
levels stored in Pa, for engine-netcdf's tests (#1067 PR 8).

Usage: make_cdl.py <output dir>; then
`ncgen -k nc4 -o pressure_levels_pa.nc pressure_levels_pa.cdl`.

A 1° latitude/longitude grid (4 rows from 62°N down, 5 columns from 20°E),
two steps six hours apart without a model run, and three pressure levels
stored top first in Pa, as many CF writers store them: 50000, 85000 and
100000 Pa, served as 1000, 850 and 500 hPa. Each value follows from its
indices, so a test can recompute it:

- air_temperature `t` = 300 - 0.05 * hPa + time + 0.1 * column - 0.2 * row (K);
- eastward wind `u` = 10 * (k + 1) + time + 0.5 * column (m s-1);
- northward wind `v` = -5 * (k + 1) + 0.25 * row (m s-1);

with k the stored level index, row counted from the north and time the
step index. Each field is its own chunk, shuffled and deflated.
"""

import sys
from pathlib import Path

LEVELS_PA = [50000, 85000, 100000]
LAT = [62.0, 61.0, 60.0, 59.0]
LON = [20.0, 21.0, 22.0, 23.0, 24.0]
TIMES = [0, 6]


def t(time, k, row, col):
    return 300 - 0.05 * LEVELS_PA[k] / 100 + time + 0.1 * col - 0.2 * row


def u(time, k, row, col):
    return 10 * (k + 1) + time + 0.5 * col


def v(time, k, row, col):
    return -5 * (k + 1) + 0.25 * row


FIELDS = {
    "t": ("air_temperature", "Temperature", "K", t),
    "u": ("eastward_wind", "U component of wind", "m s-1", u),
    "v": ("northward_wind", "V component of wind", "m s-1", v),
}


def main():
    target = Path(sys.argv[1])
    ny, nx = len(LAT), len(LON)
    out = [
        "netcdf pressure_levels_pa {",
        "dimensions:",
        f"\ttime = {len(TIMES)} ;",
        f"\tlevel = {len(LEVELS_PA)} ;",
        f"\tlatitude = {ny} ;",
        f"\tlongitude = {nx} ;",
        "variables:",
        "\tdouble time(time) ;",
        '\t\ttime:standard_name = "time" ;',
        '\t\ttime:units = "hours since 2026-01-01 00:00:00" ;',
        '\t\ttime:calendar = "gregorian" ;',
        "\tfloat level(level) ;",
        '\t\tlevel:standard_name = "air_pressure" ;',
        '\t\tlevel:long_name = "pressure" ;',
        '\t\tlevel:units = "Pa" ;',
        '\t\tlevel:positive = "down" ;',
        "\tfloat latitude(latitude) ;",
        '\t\tlatitude:standard_name = "latitude" ;',
        '\t\tlatitude:units = "degrees_north" ;',
        "\tfloat longitude(longitude) ;",
        '\t\tlongitude:standard_name = "longitude" ;',
        '\t\tlongitude:units = "degrees_east" ;',
    ]
    for name, (standard_name, long_name, units, _) in FIELDS.items():
        out += [
            f"\tfloat {name}(time, level, latitude, longitude) ;",
            f'\t\t{name}:standard_name = "{standard_name}" ;',
            f'\t\t{name}:long_name = "{long_name}" ;',
            f'\t\t{name}:units = "{units}" ;',
            f"\t\t{name}:_FillValue = -9999.f ;",
            f'\t\t{name}:_Storage = "chunked" ;',
            f"\t\t{name}:_ChunkSizes = 1, 1, {ny}, {nx} ;",
            f"\t\t{name}:_DeflateLevel = 3 ;",
            f'\t\t{name}:_Shuffle = "true" ;',
        ]
    out += [
        "",
        "// global attributes:",
        '\t\t:Conventions = "CF-1.8" ;',
        '\t\t:title = "Synthetic pressure levels in Pa, MeteoCore test fixture" ;',
        "data:",
        "",
        f" time = {', '.join(str(x) for x in TIMES)} ;",
        "",
        f" level = {', '.join(str(x) for x in LEVELS_PA)} ;",
        "",
        f" latitude = {', '.join(str(x) for x in LAT)} ;",
        "",
        f" longitude = {', '.join(str(x) for x in LON)} ;",
    ]
    for name, (_, _, _, value) in FIELDS.items():
        rows = []
        for time in range(len(TIMES)):
            for k in range(len(LEVELS_PA)):
                for row in range(ny):
                    rows.append(", ".join(f"{value(time, k, row, col):.2f}" for col in range(nx)))
        out += ["", f" {name} =\n    " + ",\n    ".join(rows) + " ;"]
    out += ["}", ""]
    (target / "pressure_levels_pa.cdl").write_text("\n".join(out))


if __name__ == "__main__":
    main()
