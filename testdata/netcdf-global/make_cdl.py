#!/usr/bin/env python3
"""Write the synthetic global latitude/longitude fixture's CDL.

Usage: make_cdl.py > global_5deg.cdl
Then:  ncgen -k nc4 -o global_5deg.nc global_5deg.cdl

A 5° grid (72 × 36 cell centres, longitude -177.5…177.5, latitude stored
north to south), three 6-hourly steps and no forecast_reference_time. The
values are smooth in longitude across the antimeridian, so a map across it
interpolates between the cells either side of 180°:

  air_temperature = 273.15 + 30 cos(lat) + 20 sin(lon) + step
  t_packed        = the same, packed in a short (scale 0.01, offset 273.15),
                    with the _FillValue in the cells 0°…10°E, 0°…10°N
"""

import math

NX, NY, STEPS = 72, 36, 3
LON = [-177.5 + 5 * i for i in range(NX)]
LAT = [87.5 - 5 * j for j in range(NY)]
FILL = -32767


def value(step, lat, lon):
    return 273.15 + 30 * math.cos(math.radians(lat)) + 20 * math.sin(math.radians(lon)) + step


def rows(values, per_line):
    lines = [", ".join(values[i : i + per_line]) for i in range(0, len(values), per_line)]
    return ",\n    ".join(lines)


def main():
    floats, packed = [], []
    for step in range(STEPS):
        for lat in LAT:
            for lon in LON:
                v = value(step, lat, lon)
                floats.append(f"{v:.2f}")
                masked = 0 < lon < 10 and 0 < lat < 10
                packed.append(str(FILL if masked else round((v - 273.15) / 0.01)))
    print(
        f"""netcdf global_5deg {{
dimensions:
	time = {STEPS} ;
	lat = {NY} ;
	lon = {NX} ;
variables:
	double time(time) ;
		time:standard_name = "time" ;
		time:units = "hours since 2026-01-01 00:00:00" ;
		time:calendar = "standard" ;
	float lat(lat) ;
		lat:standard_name = "latitude" ;
		lat:units = "degrees_north" ;
	float lon(lon) ;
		lon:standard_name = "longitude" ;
		lon:units = "degrees_east" ;
	float air_temperature(time, lat, lon) ;
		air_temperature:standard_name = "air_temperature" ;
		air_temperature:long_name = "Air temperature" ;
		air_temperature:units = "K" ;
		air_temperature:_FillValue = -999.f ;
		air_temperature:_Storage = "chunked" ;
		air_temperature:_ChunkSizes = 1, {NY}, {NX} ;
		air_temperature:_DeflateLevel = 3 ;
		air_temperature:_Shuffle = "true" ;
	short t_packed(time, lat, lon) ;
		t_packed:standard_name = "air_temperature" ;
		t_packed:long_name = "Air temperature, packed" ;
		t_packed:units = "K" ;
		t_packed:scale_factor = 0.01 ;
		t_packed:add_offset = 273.15 ;
		t_packed:_FillValue = {FILL}s ;
		t_packed:_Storage = "chunked" ;
		t_packed:_ChunkSizes = 1, {NY}, {NX} ;
		t_packed:_DeflateLevel = 3 ;
		t_packed:_Shuffle = "true" ;

// global attributes:
		:Conventions = "CF-1.8" ;
		:title = "Synthetic global 5 degree grid, MeteoCore test fixture" ;
data:

 time = 0, 6, 12 ;

 lat = {", ".join(f"{v}" for v in LAT)} ;

 lon = {", ".join(f"{v}" for v in LON)} ;

 air_temperature =
    {rows(floats, 12)} ;

 t_packed =
    {rows(packed, 18)} ;
}}"""
    )


if __name__ == "__main__":
    main()
