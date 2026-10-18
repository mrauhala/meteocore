#!/usr/bin/env python3
"""Independent reference values for the station-grid IDW and ADW methods.

Written from the method definitions, not from the Rust code: haversine
distances, compass bearings (atan2) for ADW's angular term, and a plain
sort for the nearest stations. Standard library only.

    python3 crates/engine-station-grid/tests/reference/idw_adw_reference.py

reads testdata/weather.csv (real FMI station observations) and prints the
estimates `tests/integration.rs::matches_the_independent_reference` pins.

Slice: every station's latest `temperature` at or before 2026-01-15T12:00Z
and at most one hour old (both window ends included). Methods: up to 8
stations within 150 km; IDW weight d**-2; ADW weight exp(-d / 150 km)**4
times (1 + a_k), a_k = sum_{l != k} w_l (1 - cos(theta_k - theta_l)) /
sum_{l != k} w_l with theta the bearing from the target to each station.
"""

import csv
import math
import os

R = 6371000.0  # ds_core::geo::EARTH_RADIUS_M
T = "2026-01-15T12:00:00Z"
WINDOW_START = "2026-01-15T11:00:00Z"
K = 8
RADIUS = 150_000.0
DECAY = 150_000.0
EXPONENT = 4.0
POWER = 2.0

# Cell centres of the test grid ([19, 59, 32, 71] at 0.1°): lon, lat.
TARGETS = [(24.95, 60.25), (25.45, 62.15), (27.65, 64.95), (22.05, 63.85), (28.05, 69.95)]


def haversine(lon1, lat1, lon2, lat2):
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dp = math.radians(lat2 - lat1)
    dl = math.radians(lon2 - lon1)
    a = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * R * math.asin(min(1.0, math.sqrt(a)))


def bearing(lon1, lat1, lon2, lat2):
    p1, p2 = math.radians(lat1), math.radians(lat2)
    dl = math.radians(lon2 - lon1)
    y = math.sin(dl) * math.cos(p2)
    x = math.cos(p1) * math.sin(p2) - math.sin(p1) * math.cos(p2) * math.cos(dl)
    return math.atan2(y, x)


def load_slice(path):
    latest = {}
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            t, value = row["time"], row["temperature"]
            if not (WINDOW_START <= t <= T) or value == "":
                continue
            key = (float(row["longitude"]), float(row["latitude"]))
            if key not in latest or latest[key][0] < t:
                latest[key] = (t, float(value))
    return [(lon, lat, v) for (lon, lat), (_, v) in latest.items()]


def neighbours(stations, lon, lat):
    found = sorted((haversine(lon, lat, s[0], s[1]), s) for s in stations)
    return [(d, s) for d, s in found if d <= RADIUS][:K]


def idw(stations, lon, lat):
    near = neighbours(stations, lon, lat)
    w = [d ** -POWER for d, _ in near]
    return sum(wi * s[2] for wi, (_, s) in zip(w, near)) / sum(w)


def adw(stations, lon, lat):
    near = neighbours(stations, lon, lat)
    w = [math.exp(-d / DECAY) ** EXPONENT for d, _ in near]
    theta = [bearing(lon, lat, s[0], s[1]) for _, s in near]
    total = []
    for k in range(len(near)):
        others = sum(w[l] for l in range(len(near)) if l != k)
        spread = sum(
            w[l] * (1 - math.cos(theta[k] - theta[l])) for l in range(len(near)) if l != k
        )
        total.append(w[k] * (1 + (spread / others if others > 0 else 0.0)))
    return sum(wk * s[2] for wk, (_, s) in zip(total, near)) / sum(total)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", "..", "..", "..", "testdata", "weather.csv")
    stations = load_slice(path)
    print(f"stations: {len(stations)}")
    for lon, lat in TARGETS:
        print(f"({lon}, {lat}, {idw(stations, lon, lat)!r}, {adw(stations, lon, lat)!r}),")


if __name__ == "__main__":
    main()
