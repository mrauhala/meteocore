#!/usr/bin/env python3
"""Write `pressure_13_levels.cdl`, a synthetic CF-NetCDF file on MEPS's 13
pressure levels, for engine-netcdf's OPeNDAP EDR window tests.

Usage: make_cdl.py <output dir>; then
`ncgen -k nc4 -o pressure_13_levels.nc pressure_13_levels.cdl`.

MEPS's pressure axis as MEPS stores it: 13 levels in hPa, top first (50 to
1000). A 0.5° latitude/longitude grid of 8 rows from 59°N, stored south to
north as MEPS stores its rows, and 10 columns from 20°E. One model run,
2026-10-15T06Z, and its first six hourly steps in one file. Each value
follows from its indices:

- air_temperature_pl = 300 - 0.05 * hPa + step + 0.1 * column - 0.2 * row
  (K), but a fill at step 0, 500 hPa, row 4, column 5;
- relative_humidity_pl = 0.3 + 0.004 * k + 0.01 * column + 0.02 * row
  + 0.01 * step (1);

with k the stored level index, row counted from the south as stored and
step the time index. Each level of each field is its own chunk, shuffled
and deflated.
"""

import sys
from pathlib import Path

LEVELS_HPA = [50, 100, 150, 200, 250, 300, 400, 500, 700, 800, 850, 925, 1000]
LAT = [59.0 + 0.5 * i for i in range(8)]
LON = [20.0 + 0.5 * i for i in range(10)]
RUN = 1792044000  # 2026-10-15T06:00:00Z
STEPS = 6
FILL = -9999.0


def t(step, k, row, col):
    if (step, LEVELS_HPA[k], row, col) == (0, 500, 4, 5):
        return FILL
    return 300 - 0.05 * LEVELS_HPA[k] + step + 0.1 * col - 0.2 * row


def rh(step, k, row, col):
    return 0.3 + 0.004 * k + 0.01 * col + 0.02 * row + 0.01 * step


FIELDS = {
    "air_temperature_pl": ("air_temperature", "Air temperature pressure levels", "K", t),
    "relative_humidity_pl": (
        "relative_humidity",
        "Relative humidity pressure levels",
        "1",
        rh,
    ),
}


def main():
    target = Path(sys.argv[1])
    ny, nx = len(LAT), len(LON)
    out = [
        "netcdf pressure_13_levels {",
        "dimensions:",
        "\ttime = UNLIMITED ; // (6 currently)",
        f"\tpressure = {len(LEVELS_HPA)} ;",
        f"\tlatitude = {ny} ;",
        f"\tlongitude = {nx} ;",
        "variables:",
        "\tdouble time(time) ;",
        '\t\ttime:standard_name = "time" ;',
        '\t\ttime:units = "seconds since 1970-01-01 00:00:00 +00:00" ;',
        "\tdouble forecast_reference_time ;",
        '\t\tforecast_reference_time:standard_name = "forecast_reference_time" ;',
        '\t\tforecast_reference_time:units = "seconds since 1970-01-01 00:00:00 +00:00" ;',
        "\tfloat pressure(pressure) ;",
        '\t\tpressure:standard_name = "air_pressure" ;',
        '\t\tpressure:long_name = "pressure" ;',
        '\t\tpressure:units = "hPa" ;',
        '\t\tpressure:positive = "down" ;',
        "\tfloat latitude(latitude) ;",
        '\t\tlatitude:standard_name = "latitude" ;',
        '\t\tlatitude:units = "degrees_north" ;',
        "\tfloat longitude(longitude) ;",
        '\t\tlongitude:standard_name = "longitude" ;',
        '\t\tlongitude:units = "degrees_east" ;',
    ]
    for name, (standard_name, long_name, units, _) in FIELDS.items():
        out += [
            f"\tfloat {name}(time, pressure, latitude, longitude) ;",
            f'\t\t{name}:standard_name = "{standard_name}" ;',
            f'\t\t{name}:long_name = "{long_name}" ;',
            f'\t\t{name}:units = "{units}" ;',
            f"\t\t{name}:_FillValue = {FILL}f ;",
            f'\t\t{name}:_Storage = "chunked" ;',
            f"\t\t{name}:_ChunkSizes = 1, 1, {ny}, {nx} ;",
            f"\t\t{name}:_DeflateLevel = 3 ;",
            f'\t\t{name}:_Shuffle = "true" ;',
        ]
    out += [
        "",
        "// global attributes:",
        '\t\t:Conventions = "CF-1.8" ;',
        '\t\t:title = "Synthetic MEPS-like pressure levels, MeteoCore test fixture" ;',
        "data:",
        "",
        f" time = {', '.join(str(RUN + 3600 * s) for s in range(STEPS))} ;",
        "",
        f" forecast_reference_time = {RUN} ;",
        "",
        f" pressure = {', '.join(str(x) for x in LEVELS_HPA)} ;",
        "",
        f" latitude = {', '.join(str(x) for x in LAT)} ;",
        "",
        f" longitude = {', '.join(str(x) for x in LON)} ;",
    ]
    for name, (_, _, _, value) in FIELDS.items():
        rows = []
        for step in range(STEPS):
            for k in range(len(LEVELS_HPA)):
                for row in range(ny):
                    rows.append(", ".join(f"{value(step, k, row, col):.3f}" for col in range(nx)))
        out += ["", f" {name} =\n    " + ",\n    ".join(rows) + " ;"]
    out += ["}", ""]
    (target / "pressure_13_levels.cdl").write_text("\n".join(out))


if __name__ == "__main__":
    main()
