#!/usr/bin/env python3
"""Measure local projected WMS panning; stdout is JSON, no external services."""

import argparse
import hashlib
import json
import pathlib
import socket
import statistics
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request


def main():
    repo = pathlib.Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, default=repo / "target/debug/server")
    parser.add_argument("--cache-mb", type=int, default=128)
    parser.add_argument("--crs", choices=["EPSG:3067", "EPSG:3035"], default="EPSG:3067")
    parser.add_argument("--frames-dir", type=pathlib.Path, help="Optionally save rendered PNGs")
    args = parser.parse_args()
    if args.frames_dir:
        args.frames_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="meteocore-projected-pan-") as directory:
        root = pathlib.Path(directory)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        config = root / "config.toml"
        # JSON string quoting is also valid TOML basic-string quoting here.
        source = json.dumps(str(repo / "testdata/radar-tm35fin"))
        config.write_text(f'''[server]
host = "127.0.0.1"
port = {port}
metatile_cache_mb = {args.cache_mb}
[[collections]]
id = "radar"
title = "Radar"
description = "Projected pan benchmark"
engine_type = "geotiff"
data_path = {source}
apis = ["wms"]
[collections.geotiff]
filename_template = "radar_tm35_%Y%m%dT%H%MZ.tif"
parameter = "reflectivity"
unit = "dBZ"
[collections.wms]
colormap = "radar_dbz"
''')

        def request(path):
            with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=60) as response:
                return response.read()

        with (root / "server.log").open("w") as log:
            process = subprocess.Popen(
                [str(args.binary.resolve()), "--config", str(config)],
                cwd=repo, stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 30
                while True:
                    try:
                        request("/health")
                        break
                    except OSError:
                        if process.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError((root / "server.log").read_text())
                        time.sleep(0.1)
                times, hashes = [], []
                left, south = (100000, 6500000) if args.crs == "EPSG:3067" else (4800000, 4050000)
                for index in range(25):
                    west = left + index * 16000
                    query = urllib.parse.urlencode({
                        "SERVICE": "WMS", "VERSION": "1.3.0", "REQUEST": "GetMap",
                        "LAYERS": "radar", "CRS": args.crs,
                        "BBOX": f"{west},{south},{west + 512000},{south + 384000}",
                        "WIDTH": 1024, "HEIGHT": 768, "FORMAT": "image/png",
                    })
                    start = time.perf_counter()
                    body = request("/wms?" + query)
                    times.append((time.perf_counter() - start) * 1000)
                    assert body.startswith(b"\x89PNG"), body[:200]
                    hashes.append(hashlib.sha256(body).hexdigest())
                    if args.frames_dir:
                        (args.frames_dir / f"{index:02}.png").write_bytes(body)
                metrics = request("/metrics").decode()
                print(json.dumps({
                    "crs": args.crs, "cache_mb": args.cache_mb,
                    "first_ms": times[0], "pan_median_ms": statistics.median(times[1:]),
                    "pan_p95_ms": sorted(times[1:])[22], "total_ms": sum(times),
                    "times_ms": times, "png_sha256": hashes,
                    "metatile_metrics": [line for line in metrics.splitlines()
                                         if "metatile" in line and not line.startswith("#")],
                }, indent=2))
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    main()
