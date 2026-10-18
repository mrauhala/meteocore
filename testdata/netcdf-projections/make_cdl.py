#!/usr/bin/env python3
"""Write the synthetic CF grid-mapping fixtures' CDL, one per mapping (#1167).

Usage: make_cdl.py <output dir>; then in each subdirectory
`ncgen -k nc4 -o <name>.nc <name>.cdl`.

Each file is a small regular grid on 1-D projected x/y in metres with a CF
grid-mapping variable `crs`, one step and no forecast_reference_time. Its
two fields hold each cell's own projected coordinates:

  easting  = x of the cell centre (m)
  northing = y of the cell centre (m)

Both are linear in the grid, so a bilinear read anywhere between cell
centres returns the projected coordinates of the point read: a test reads
them at a longitude/latitude and compares them with PROJ's cs2cs.

- polar-stereographic/: EPSG:3413's parameters (true scale at 70°N,
  straight vertical longitude -45°, WGS84), 9 x 9 cells of 250 km around
  the North Pole, which is the centre cell, rows stored north to south.
- laea/: EPSG:3035's parameters (origin 52°N 10°E, false origin 4321 km,
  3210 km, GRS80), 9 x 9 cells of 250 km over central Europe, rows stored
  south to north.
- transverse-mercator/: UTM zone 33N (central meridian 15°E, k0 0.9996,
  false easting 500 km, WGS84), 11 x 13 cells of 50 km over southern
  Scandinavia, rows stored north to south.
"""

import sys
from pathlib import Path

WGS84 = [("semi_major_axis", "6378137."), ("inverse_flattening", "298.257223563")]
GRS80 = [("semi_major_axis", "6378137."), ("inverse_flattening", "298.257222101")]

FIXTURES = {
    "polar-stereographic": {
        "x": [-1_000_000 + 250_000 * i for i in range(9)],
        "y": [1_000_000 - 250_000 * j for j in range(9)],
        "mapping": [
            ("grid_mapping_name", '"polar_stereographic"'),
            ("straight_vertical_longitude_from_pole", "-45."),
            ("latitude_of_projection_origin", "90."),
            ("standard_parallel", "70."),
            ("false_easting", "0."),
            ("false_northing", "0."),
        ]
        + WGS84,
    },
    "laea": {
        "x": [3_500_000 + 250_000 * i for i in range(9)],
        "y": [2_250_000 + 250_000 * j for j in range(9)],
        "mapping": [
            ("grid_mapping_name", '"lambert_azimuthal_equal_area"'),
            ("latitude_of_projection_origin", "52."),
            ("longitude_of_projection_origin", "10."),
            ("false_easting", "4321000."),
            ("false_northing", "3210000."),
        ]
        + GRS80,
    },
    "transverse-mercator": {
        "x": [250_000 + 50_000 * i for i in range(11)],
        "y": [7_000_000 - 50_000 * j for j in range(13)],
        "mapping": [
            ("grid_mapping_name", '"transverse_mercator"'),
            ("scale_factor_at_central_meridian", "0.9996"),
            ("longitude_of_central_meridian", "15."),
            ("latitude_of_projection_origin", "0."),
            ("false_easting", "500000."),
            ("false_northing", "0."),
        ]
        + WGS84,
    },
}


def rows(values, per_line):
    lines = [", ".join(values[i : i + per_line]) for i in range(0, len(values), per_line)]
    return ",\n    ".join(lines)


def cdl(name, fixture):
    xs, ys = fixture["x"], fixture["y"]
    nx, ny = len(xs), len(ys)
    easting = [str(x) for _ in ys for x in xs]
    northing = [str(y) for y in ys for _ in xs]
    out = [
        f"netcdf {name.replace('-', '_')} {{",
        "dimensions:",
        "\ttime = 1 ;",
        f"\ty = {ny} ;",
        f"\tx = {nx} ;",
        "variables:",
        "\tdouble time(time) ;",
        '\t\ttime:standard_name = "time" ;',
        '\t\ttime:units = "hours since 2026-01-01 00:00:00" ;',
        '\t\ttime:calendar = "standard" ;',
        "\tdouble y(y) ;",
        '\t\ty:standard_name = "projection_y_coordinate" ;',
        '\t\ty:units = "m" ;',
        "\tdouble x(x) ;",
        '\t\tx:standard_name = "projection_x_coordinate" ;',
        '\t\tx:units = "m" ;',
        "\tint crs ;",
    ]
    out += [f"\t\tcrs:{key} = {value} ;" for key, value in fixture["mapping"]]
    for field, long_name in [("easting", "Projected x"), ("northing", "Projected y")]:
        out += [
            f"\tdouble {field}(time, y, x) ;",
            f'\t\t{field}:long_name = "{long_name} of the cell centre" ;',
            f'\t\t{field}:units = "m" ;',
            f'\t\t{field}:grid_mapping = "crs" ;',
        ]
    out += [
        "",
        "// global attributes:",
        '\t\t:Conventions = "CF-1.8" ;',
        f'\t\t:title = "Synthetic {name} grid, MeteoCore test fixture" ;',
        "data:",
        "",
        " time = 0 ;",
        "",
        f" y = {', '.join(str(v) for v in ys)} ;",
        "",
        f" x = {', '.join(str(v) for v in xs)} ;",
        "",
        " crs = 0 ;",
        "",
        " easting =",
        f"    {rows(easting, nx)} ;",
        "",
        " northing =",
        f"    {rows(northing, nx)} ;",
        "}",
    ]
    return "\n".join(out) + "\n"


def main():
    target = Path(sys.argv[1])
    for name, fixture in FIXTURES.items():
        directory = target / name
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"{name}.cdl").write_text(cdl(name, fixture))


if __name__ == "__main__":
    main()
