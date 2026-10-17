#!/usr/bin/env python3
"""Write the remote-read fixture: two per-lead files of one synthetic MEPS-like
run whose fields are chunked like MEPS (one whole field per chunk, shuffle and
deflate level 3) and noisy enough that a compressed chunk is about 20 KB.

    make_fixture.py <output dir>

Needs `ncgen` (netCDF-C) on PATH. Deterministic: the same files every run.
"""

import os
import random
import subprocess
import sys
import tempfile

NY, NX = 64, 96
STEP = 2500.0
X0, Y0 = -1060084.0, -1332517.9
RUN = 1792044000  # 2026-10-15T06:00:00Z


def values(seed, base, spread):
    rng = random.Random(seed)
    out = []
    for row in range(NY):
        for col in range(NX):
            smooth = base + spread * ((row / NY) - (col / NX))
            out.append(f"{smooth + rng.uniform(-2.0, 2.0):.3f}")
    return out


def lines(numbers, per_line=12):
    return ",\n    ".join(
        ", ".join(numbers[i : i + per_line]) for i in range(0, len(numbers), per_line)
    )


def cdl(lead):
    name = f"meps_sfc_{lead:02d}_20261015T06Z"
    x = [f"{X0 + STEP * i:.1f}" for i in range(NX)]
    y = [f"{Y0 + STEP * j:.1f}" for j in range(NY)]
    temperature = values(1000 + lead, 280.0, 10.0)
    gust = values(2000 + lead, 8.0, 6.0)
    return f"""netcdf {name} {{
dimensions:
	time = UNLIMITED ;
	height0 = 1 ;
	height1 = 1 ;
	x = {NX} ;
	y = {NY} ;
variables:
	double time(time) ;
		time:standard_name = "time" ;
		time:units = "seconds since 1970-01-01 00:00:00 +00:00" ;
	double forecast_reference_time ;
		forecast_reference_time:standard_name = "forecast_reference_time" ;
		forecast_reference_time:units = "seconds since 1970-01-01 00:00:00 +00:00" ;
	float height0(height0) ;
		height0:units = "m" ;
		height0:positive = "up" ;
	float height1(height1) ;
		height1:units = "m" ;
		height1:positive = "up" ;
	float x(x) ;
		x:standard_name = "projection_x_coordinate" ;
		x:units = "m" ;
	float y(y) ;
		y:standard_name = "projection_y_coordinate" ;
		y:units = "m" ;
	int projection_lambert ;
		projection_lambert:grid_mapping_name = "lambert_conformal_conic" ;
		projection_lambert:standard_parallel = 63.3, 63.3 ;
		projection_lambert:longitude_of_central_meridian = 15. ;
		projection_lambert:latitude_of_projection_origin = 63.3 ;
		projection_lambert:earth_radius = 6371000. ;
	float air_temperature_2m(time, height1, y, x) ;
		air_temperature_2m:_FillValue = 9.96921e+36f ;
		air_temperature_2m:long_name = "Screen level temperature (T2M)" ;
		air_temperature_2m:standard_name = "air_temperature" ;
		air_temperature_2m:units = "K" ;
		air_temperature_2m:grid_mapping = "projection_lambert" ;
		air_temperature_2m:_Storage = "chunked" ;
		air_temperature_2m:_ChunkSizes = 1, 1, {NY}, {NX} ;
		air_temperature_2m:_DeflateLevel = 3 ;
		air_temperature_2m:_Shuffle = "true" ;
	float wind_speed_of_gust(time, height0, y, x) ;
		wind_speed_of_gust:_FillValue = 9.96921e+36f ;
		wind_speed_of_gust:long_name = "Wind speed of gust" ;
		wind_speed_of_gust:standard_name = "wind_speed_of_gust" ;
		wind_speed_of_gust:units = "m/s" ;
		wind_speed_of_gust:grid_mapping = "projection_lambert" ;
		wind_speed_of_gust:_Storage = "chunked" ;
		wind_speed_of_gust:_ChunkSizes = 1, 1, {NY}, {NX} ;
		wind_speed_of_gust:_DeflateLevel = 3 ;
		wind_speed_of_gust:_Shuffle = "true" ;

// global attributes:
		:meteocore_fixture = "synthetic, testdata/netcdf-remote/make_fixture.py" ;
data:

 time = {RUN + 3600 * lead} ;

 forecast_reference_time = {RUN} ;

 height0 = 0 ;

 height1 = 2 ;

 x = {lines(x)} ;

 y = {lines(y)} ;

 projection_lambert = 0 ;

 air_temperature_2m =
    {lines(temperature)} ;

 wind_speed_of_gust =
    {lines(gust)} ;
}}
"""


def main():
    out = sys.argv[1]
    for lead in (0, 1):
        name = f"meps_sfc_{lead:02d}_20261015T06Z"
        with tempfile.NamedTemporaryFile("w", suffix=".cdl", delete=False) as f:
            f.write(cdl(lead))
            source = f.name
        try:
            subprocess.run(
                ["ncgen", "-k", "nc4", "-o", os.path.join(out, f"{name}.nc"), source],
                check=True,
            )
        finally:
            os.unlink(source)


if __name__ == "__main__":
    main()
