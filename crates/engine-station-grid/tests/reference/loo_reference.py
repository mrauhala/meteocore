#!/usr/bin/env python3
"""Independent reference values for the station-grid leave-one-out residuals.

The residual of a station is the IDW or ADW estimate at its own position
from every OTHER station, minus its value. The estimates come from
`idw_adw_reference.py` (haversine distances, compass bearings, a plain sort
for the neighbours; standard library only), applied to the slice without
the station. Same settings: up to 8 stations within 150 km; IDW d**-2; ADW
exp(-d / 150 km)**4 times the angular term.

    python3 crates/engine-station-grid/tests/reference/loo_reference.py

prints the values `tests/residuals.rs` pins:

1. testdata/weather.csv at 2026-01-15T12:00Z (the slice of
   `idw_adw_reference.py`): the stations with a residual, the RMSE, and the
   station with the largest |residual| as (lon, lat, value, estimate,
   residual, neighbours), for IDW and ADW.
2. A synthetic outlier layout, `LATTICE`: 25 stations on a 5 x 5 lattice
   over 24-26 E, 60-61 N, 0.5 deg apart in longitude and 0.25 deg in
   latitude, each but the centre moved by up to 0.06 deg so that no two
   neighbours tie. They carry the plane 3 + 0.8 (lon - 24) - 1.5 (lat - 60)
   rounded to 0.01, with 6.0 added at the centre, 25 E 60.5 N. For IDW,
   ADW and the nearest other station: the outlier's estimate and residual,
   the largest |residual| elsewhere, and the RMSE.
"""

import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import idw_adw_reference as ref  # noqa: E402


def nearest(stations, lon, lat):
    near = ref.neighbours(stations, lon, lat)
    return near[0][1][2]


METHODS = {"idw": ref.idw, "adw": ref.adw, "nearest": nearest}


def leave_one_out(stations, method):
    """(lon, lat, value, estimate, residual, neighbours) per station: the
    estimate is None without a station of the others within the radius,
    and `neighbours` counts the others it used (1 for nearest)."""
    out = []
    for i, (lon, lat, value) in enumerate(stations):
        others = stations[:i] + stations[i + 1:]
        near = ref.neighbours(others, lon, lat)
        if not near:
            out.append((lon, lat, value, None, None, 0))
            continue
        estimate = METHODS[method](others, lon, lat)
        used = 1 if method == "nearest" else len(near)
        out.append((lon, lat, value, estimate, estimate - value, used))
    return out


def rmse(rows):
    residuals = [r[4] for r in rows if r[4] is not None]
    return len(residuals), math.sqrt(sum(r * r for r in residuals) / len(residuals))


def largest(rows):
    return max((r for r in rows if r[4] is not None), key=lambda r: abs(r[4]))


OUTLIER = (25.0, 60.5)

# (lon, lat, value); `tests/residuals.rs` holds the same list.
LATTICE = [
    (24.019, 59.984, 3.04), (24.021, 60.219, 2.69), (24.002, 60.499, 2.25),
    (24.012, 60.74, 1.9), (23.971, 60.99, 1.49), (24.539, 59.974, 3.47),
    (24.476, 60.261, 2.99), (24.535, 60.539, 2.62), (24.536, 60.747, 2.31),
    (24.505, 61.01, 1.89), (24.97, 60.016, 3.75), (25.026, 60.288, 3.39),
    (25.0, 60.5, 9.05), (24.979, 60.746, 2.66), (25.025, 61.019, 2.29),
    (25.461, 59.961, 4.23), (25.534, 60.213, 3.91), (25.512, 60.48, 3.49),
    (25.507, 60.751, 3.08), (25.488, 60.975, 2.73), (26.018, 60.017, 4.59),
    (25.977, 60.288, 4.15), (26.04, 60.491, 3.9), (26.025, 60.72, 3.54),
    (26.013, 61.004, 3.1),
]


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", "..", "..", "..", "testdata", "weather.csv")
    stations = ref.load_slice(path)
    print(f"weather.csv at {ref.T}: {len(stations)} stations")
    for method in ("idw", "adw"):
        rows = leave_one_out(stations, method)
        count, value = rmse(rows)
        print(f"  {method}: {count} residuals, rmse {value!r}, largest {largest(rows)!r}")

    print("outlier lattice:")
    stations = LATTICE
    for method in ("idw", "adw", "nearest"):
        rows = leave_one_out(stations, method)
        count, value = rmse(rows)
        outlier = next(r for r in rows if (r[0], r[1]) == OUTLIER)
        others = largest([r for r in rows if (r[0], r[1]) != OUTLIER])
        print(
            f"  {method}: outlier estimate {outlier[3]!r} residual {outlier[4]!r}; "
            f"largest elsewhere {abs(others[4])!r} at {others[0]}, {others[1]}; "
            f"{count} residuals, rmse {value!r}"
        )


if __name__ == "__main__":
    main()
