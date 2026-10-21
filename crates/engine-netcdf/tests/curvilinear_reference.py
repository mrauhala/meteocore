#!/usr/bin/env python3
"""Independent reference for tests/curvilinear.rs (#1174): where the
engine must place a point on testdata/netcdf-curvilinear, and the
`column` and `row` values it must read there.

Usage: curvilinear_reference.py [path/to/svalbard_curvilinear.cdl]

Python's standard library only, over the CDL's own `lat`/`lon` arrays (the
spherical inverse its make_cdl.py wrote), by brute force, sharing no code
or search with the engine:

- **Inside the nodes' hull:** every cell is tried. Its four corners are
  projected gnomonically onto the plane tangent to the sphere at the point
  (the cell's edges are then straight, as the engine defines a cell), and
  the bilinear map through them is inverted in closed form (the quadratic,
  not the engine's Newton iteration). The cell whose inversion lands in
  [0, 1]^2 holds the point; (i + s, j + t) is its position, and since the
  fixture's fields are their own indices, the values read there are that
  position.
- **Past the outer nodes, within half a node of one:** no cell holds the
  point, and the shared sampler reads the nearest node's value (it draws
  up to the half cell past the outer cell edges, a node past the outer
  nodes), found here by great-circle distance over every node.
- **One and a half nodes or more from every node:** off the grid, null.
- In between, the answer turns on where exactly the sampler's cut lies:
  `AMBIGUOUS`, and no test pins such a point.

Prints the EDR points' and the maps' pixels' expected values, as the
test's tables hold them.
"""

import math
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_CDL = HERE / "../../../testdata/netcdf-curvilinear/svalbard_curvilinear.cdl"


def read_cdl(path):
    text = Path(path).read_text()
    dims = {m.group(1): int(m.group(2)) for m in re.finditer(r"\t(\w+) = (\d+) ;", text)}

    def array(name):
        block = re.search(r"\n %s =\s*(.*?);" % name, text, re.S).group(1)
        return [float(v) for v in block.replace("\n", " ").split(",")]

    return dims["x"], dims["y"], array("lon"), array("lat")


NX, NY, LON, LAT = read_cdl(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_CDL)


def unit(lon, lat):
    lon, lat = math.radians(lon), math.radians(lat)
    return (math.cos(lat) * math.cos(lon), math.cos(lat) * math.sin(lon), math.sin(lat))


NODES = [unit(LON[k], LAT[k]) for k in range(NX * NY)]


def dot(a, b):
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def tangent_plane(lon, lat):
    """The point's unit vector and the unit vectors east and north of it."""
    p = unit(lon, lat)
    lo, la = math.radians(lon), math.radians(lat)
    east = (-math.sin(lo), math.cos(lo), 0.0)
    north = (-math.sin(la) * math.cos(lo), -math.sin(la) * math.sin(lo), math.cos(la))
    return p, east, north


def gnomonic(q, plane):
    p, east, north = plane
    d = dot(q, p)
    if d <= 0.5:
        return None
    return (dot(q, east) / d, dot(q, north) / d)


def cross(a, b):
    return a[0] * b[1] - a[1] * b[0]


def inverse_bilinear(a, b, c, d):
    """(s, t) with the point (the origin) = bilinear(a, b, c, d) at (s, t),
    a at (0, 0), b at (1, 0), c at (1, 1), d at (0, 1): the closed form of
    the quadratic in t. Every real root, as (s, t) pairs."""
    e = (b[0] - a[0], b[1] - a[1])
    f = (d[0] - a[0], d[1] - a[1])
    g = (a[0] - b[0] + c[0] - d[0], a[1] - b[1] + c[1] - d[1])
    h = (-a[0], -a[1])
    k2 = cross(g, f)
    k1 = cross(e, f) + cross(h, g)
    k0 = cross(h, e)
    if abs(k2) < 1e-18:
        ts = [-k0 / k1]
    else:
        w = k1 * k1 - 4.0 * k0 * k2
        if w < 0.0:
            return []
        w = math.sqrt(w)
        ts = [(-k1 - w) / (2.0 * k2), (-k1 + w) / (2.0 * k2)]
    out = []
    for t in ts:
        dx, dy = e[0] + g[0] * t, e[1] + g[1] * t
        s = (h[0] - f[0] * t) / dx if abs(dx) > abs(dy) else (h[1] - f[1] * t) / dy
        out.append((s, t))
    return out


def node(i, j):
    return NODES[j * NX + i]


def position(lon, lat):
    """(column, row) of a point inside the nodes' hull, or None."""
    plane = tangent_plane(lon, lat)
    eps = 1e-9
    for j in range(NY - 1):
        for i in range(NX - 1):
            corners = [gnomonic(node(i, j), plane), gnomonic(node(i + 1, j), plane),
                       gnomonic(node(i + 1, j + 1), plane), gnomonic(node(i, j + 1), plane)]
            if any(c is None for c in corners):
                continue
            for s, t in inverse_bilinear(*corners):
                if -eps <= s <= 1 + eps and -eps <= t <= 1 + eps:
                    return (i + s, j + t)
    return None


def nearest_node(lon, lat):
    p = unit(lon, lat)
    k = max(range(NX * NY), key=lambda k: dot(NODES[k], p))
    angle = math.acos(max(-1.0, min(1.0, dot(NODES[k], p))))
    return k % NX, k // NX, angle


def spacing(i, j):
    """The largest great-circle angle from node (i, j) to a neighbour."""
    out = 0.0
    for a, b in [(i - 1, j), (i + 1, j), (i, j - 1), (i, j + 1)]:
        if 0 <= a < NX and 0 <= b < NY:
            out = max(out, math.acos(min(1.0, dot(node(i, j), node(a, b)))))
    return out


def values(lon, lat):
    """The (column, row) values the engine reads at the point, None off
    the grid, or "AMBIGUOUS"."""
    found = position(lon, lat)
    if found is not None:
        return found
    i, j, angle = nearest_node(lon, lat)
    on_edge = i in (0, NX - 1) or j in (0, NY - 1)
    if on_edge and angle <= 0.5 * spacing(i, j):
        return (float(i), float(j))
    if angle >= 1.5 * spacing(i, j):
        return None
    return "AMBIGUOUS"


def mercator_lat(north, south, fy):
    """Latitude at fraction fy down a Web Mercator map from north to south."""
    y = lambda lat: math.log(math.tan(math.pi / 4 + math.radians(lat) / 2))
    yn, ys = y(north), y(south)
    return math.degrees(2 * math.atan(math.exp(yn - fy * (yn - ys))) - math.pi / 2)


def pixel_centre(bbox, width, height, crs, ox, oy):
    west, south, east, north = bbox
    if east < west:
        east += 360.0
    fx, fy = (ox + 0.5) / width, (oy + 0.5) / height
    lon = west + fx * (east - west)
    lat = north - fy * (north - south) if crs == "CRS84" else mercator_lat(north, south, fy)
    return lon, lat


POINTS = [
    ("Longyearbyen", 15.633, 78.217),
    ("North Pole", 0.0, 90.0),
    ("North Pole from 123E", 123.0, 90.0),
    ("antimeridian west", 179.5, 84.0),
    ("antimeridian east", -179.5, 84.0),
    ("Kvitoya", 33.0, 80.1),
    ("near row 0", -150.0, 79.0),
    ("a third of a node past row 27", 15.0, 75.0),
    ("Hopen", 25.0, 76.5),
    ("Tromso", 18.96, 69.65),
    ("off the grid", -100.0, 40.0),
]

MAPS = [
    ("CRS84 across the antimeridian", [170.0, 80.0, -170.0, 88.0], 20, 12, "CRS84",
     [(0, 0), (9, 5), (10, 5), (19, 11), (5, 2), (14, 9), (0, 11), (19, 0)]),
    ("EPSG:3857 over Svalbard", [0.0, 73.0, 40.0, 82.0], 32, 24, "EPSG:3857",
     [(0, 0), (12, 10), (16, 12), (30, 19), (5, 20), (25, 3), (0, 23), (8, 16)]),
]


def fmt(v):
    if isinstance(v, str):
        return v
    return "None" if v is None else "Some((%.6f, %.6f))" % v


def main():
    print("// EDR points: (name, lon, lat, (column, row))")
    for name, lon, lat in POINTS:
        print('("%s", %r, %r, %s),' % (name, lon, lat, fmt(values(lon, lat))))
    for name, bbox, width, height, crs, pixels in MAPS:
        print("// %s %s %dx%d: (ox, oy, (column, row))" % (name, bbox, width, height))
        for ox, oy in pixels:
            lon, lat = pixel_centre(bbox, width, height, crs, ox, oy)
            print("(%d, %d, %s),  // (%.4f, %.4f)" % (ox, oy, fmt(values(lon, lat)), lon, lat))


if __name__ == "__main__":
    main()
