#!/usr/bin/env python3
"""Independent reference values for the station-grid lapse-rate correction.

Written from the method's definition, not from the Rust code: haversine
distances, IDW over the reduced station values, the terrain as a formula.
Standard library only.

    python3 crates/engine-station-grid/tests/reference/lapse_rate_reference.py

prints the values `tests/corrections.rs` pins:

1. `valley_analysis_follows_the_terrain`: corrected and uncorrected
   estimates at cell centres, as (lon, lat, corrected, uncorrected).
2. `residuals_with_corrections`: the leave-one-out residual of each station
   of a four-station valley, as (lon, lat, value, estimate, residual).

Valley: terrain z(lon) = 50 + 950 |lon - 24.5| / 0.5 m (ridges at 24.0 and
25.0 E, floor at 24.5 E), the same at every latitude. A ridge station at
(24.05, 60.25) reports -4.0 at its stated height of 900 m; a valley station
at (24.45, 60.15) reports 1.5 and states no height, so it takes the terrain
under it, 145 m. Lapse rate -6.5 K/km: each value is reduced to sea level,
v0 = v - G z_station, interpolated by IDW (power 2, up to 8 stations within
100 km, a station within 1 mm of the target is its value), and the cell gets
G z_cell back.

Grid: [24.0, 60.0, 25.0, 60.4] at 0.1 deg; targets are cell centres.

Residuals: LOO_STATIONS adds an east-ridge station at (24.95, 60.35), -3.0
on the terrain under it (905 m), and an east-slope one at (24.75, 60.05),
0.5 at its stated 400 m (the terrain says 525 m). A station's estimate is
the IDW, from every OTHER station's sea-level value, at its own position,
with G z_station added back; its residual is that estimate minus its
value.
"""

import math

R = 6371000.0  # ds_core::geo::EARTH_RADIUS_M
LAPSE_K_PER_M = -6.5 / 1000.0
RADIUS = 100_000.0
K = 8
POWER = 2.0


def terrain(lon):
    return 50.0 + 950.0 * abs(lon - 24.5) / 0.5


# (lon, lat, value, stated height or None)
STATIONS = [(24.05, 60.25, -4.0, 900.0), (24.45, 60.15, 1.5, None)]

TARGETS = [
    (24.05, 60.25),  # the ridge station's cell
    (24.45, 60.15),  # the valley station's cell
    (24.55, 60.25),  # valley floor, east side
    (24.95, 60.35),  # east ridge
    (24.25, 60.05),  # west slope
    (24.75, 60.15),  # east slope
]


def haversine(lon1, lat1, lon2, lat2):
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dp = math.radians(lat2 - lat1)
    dl = math.radians(lon2 - lon1)
    a = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * R * math.asin(min(1.0, math.sqrt(a)))


def reduced():
    out = []
    for lon, lat, value, stated in STATIONS:
        z = stated if stated is not None else terrain(lon)
        out.append((lon, lat, value - LAPSE_K_PER_M * z))
    return out


def idw(stations, lon, lat):
    found = sorted((haversine(lon, lat, s[0], s[1]), s) for s in stations)
    near = [(d, s) for d, s in found if d <= RADIUS][:K]
    coincident = [s[2] for d, s in near if d < 1e-3]
    if coincident:
        return sum(coincident) / len(coincident)
    w = [d ** -POWER for d, _ in near]
    return sum(wi * s[2] for wi, (_, s) in zip(w, near)) / sum(w)


LOO_STATIONS = STATIONS + [(24.95, 60.35, -3.0, None), (24.75, 60.05, 0.5, 400.0)]


def height(station):
    lon, _, _, stated = station
    return stated if stated is not None else terrain(lon)


def main():
    stations = reduced()
    print("valley:")
    for lon, lat in TARGETS:
        corrected = idw(stations, lon, lat) + LAPSE_K_PER_M * terrain(lon)
        plain = idw([(s[0], s[1], s[2]) for s in STATIONS], lon, lat)
        print(f"({lon}, {lat}, {corrected!r}, {plain!r}),")
    print("residuals:")
    for k, station in enumerate(LOO_STATIONS):
        lon, lat, value, _ = station
        others = [
            (s[0], s[1], s[2] - LAPSE_K_PER_M * height(s))
            for i, s in enumerate(LOO_STATIONS)
            if i != k
        ]
        estimate = idw(others, lon, lat) + LAPSE_K_PER_M * height(station)
        print(f"({lon}, {lat}, {value!r}, {estimate!r}, {estimate - value!r}),")


if __name__ == "__main__":
    main()
