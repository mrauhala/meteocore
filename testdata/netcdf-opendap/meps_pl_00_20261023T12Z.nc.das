Attributes {
    time {
        String long_name "time";
        String standard_name "time";
        String units "seconds since 1970-01-01 00:00:00 +00:00";
        UInt32 _ChunkSizes 1;
    }
    forecast_reference_time {
        String units "seconds since 1970-01-01 00:00:00 +00:00";
        String standard_name "forecast_reference_time";
    }
    pressure {
        String description "pressure";
        String long_name "pressure";
        String standard_name "air_pressure";
        String positive "down";
        String units "hPa";
        UInt32 _ChunkSizes 13;
    }
    projection_lambert {
        String grid_mapping_name "lambert_conformal_conic";
        Float64 standard_parallel 63.3, 63.3;
        Float64 longitude_of_central_meridian 15.0;
        Float64 latitude_of_projection_origin 63.3;
        Float64 earth_radius 6371000.0;
    }
    x {
        String long_name "x-coordinate in Cartesian system";
        String standard_name "projection_x_coordinate";
        String units "m";
        UInt32 _ChunkSizes 949;
    }
    y {
        String long_name "y-coordinate in Cartesian system";
        String standard_name "projection_y_coordinate";
        String units "m";
        UInt32 _ChunkSizes 1069;
    }
    longitude {
        String units "degree_east";
        String long_name "longitude";
        String standard_name "longitude";
        UInt32 _ChunkSizes 1069, 949;
    }
    latitude {
        String units "degree_north";
        String long_name "latitude";
        String standard_name "latitude";
        UInt32 _ChunkSizes 1069, 949;
    }
    x_wind_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Wind along x-coordinate on pressure levels";
        String standard_name "x_wind";
        String units "m/s";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    y_wind_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Wind along y-coordinate on pressure levels";
        String standard_name "y_wind";
        String units "m/s";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    air_temperature_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Air temperature pressure levels";
        String standard_name "air_temperature";
        String units "K";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    specific_humidity_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Specific humidity pressure levels";
        String standard_name "specific_humidity";
        String units "kg/kg";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    mass_fraction_of_cloud_condensed_water_in_air_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Atmospheric cloud condensed water content in pressure levels";
        String standard_name "mass_fraction_of_cloud_condensed_water_in_air";
        String units "kg/kg";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    mass_fraction_of_cloud_ice_in_air_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Mass fraction of cloud ice in pressure levels";
        String standard_name "mass_fraction_of_cloud_ice_in_air";
        String units "kg/kg";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    cloud_area_fraction_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Total cloud cover (TCC) in pressure levels";
        String standard_name "cloud_area_fraction";
        String units "1";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    mass_fraction_of_snow_in_air_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Instantaneous snow in pressure levels";
        String standard_name "mass_fraction_of_snow_in_air";
        String units "kg/kg";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    mass_fraction_of_rain_in_air_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Instantanous rain in pressure levels";
        String standard_name "mass_fraction_of_rain_in_air";
        String units "kg/kg";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    mass_fraction_of_graupel_in_air_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Instantaneous graupel in pressure levels";
        String standard_name "mass_fraction_of_graupel_in_air";
        String units "kg/kg";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    turbulent_kinetic_energy_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Turbulent kinetic energy (TKE)";
        String metno_name "turbulent_kinetic_energy";
        String units "m^2/s^2";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    geopotential_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Geopotential pressure levels";
        String standard_name "geopotential";
        String units "m^2/s^2";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    relative_humidity_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Relative humidity pressure levels";
        String standard_name "relative_humidity";
        String units "1";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    upward_air_velocity_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Vertical vind pressure levels";
        String standard_name "upward_air_velocity";
        String units "m/s";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    ertel_potential_vorticity_pl {
        Float32 _FillValue 9.96921E36;
        String long_name "Potential vorticity";
        String standard_name "ertel_potential_vorticity";
        String units "K m2 kg-1 s-1";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    NC_GLOBAL {
        String min_time "2026-10-23T12:00:00Z";
        String geospatial_lat_min "49.8";
        String geospatial_lat_max "75.2";
        String geospatial_lon_min "-18.1";
        String geospatial_lon_max "54.2";
        String comment "For more information, please visit https://github.com/metno/NWPdocs/wiki";
        String history "2026-10-23T13:24:38 creation by fimex";
        String ensemble_member "00";
        String institution "Norwegian Meteorological Institute (MET Norway)";
        String id "4125d51d-773e-41ce-8851-e2e8a4e5b4a6";
        String Conventions "CF-1.6, ACDD-1.3";
        String creator_url "https://met.no";
        String source "Numerical Simulation";
        String max_time "2026-10-23T12:00:00Z";
        String references "https://doi.org/10.1002/qj.3525 (Scientific publication), https://doi.org/10.1175/WAF-D-16-0099.1 (Scientific publication), https://github.com/metno/NWPdocs/wiki/MEPS-dataset (Users guide)";
        String license "https://spdx.org/licenses/CC-BY-4.0 (CC-BY-4.0)";
        String time_coverage_start "2026-10-23T12:00:00Z";
        String time_coverage_end "2026-10-23T12:00:00Z";
        String keywords_vocabulary "GCMDSK:GCMD Science Keywords:https://gcmd.earthdata.nasa.gov/kms/concepts/concept_scheme/sciencekeywords, GEMET:INSPIRE Themes:https://inspire.ec.europa.eu/theme, NORTHEMES:GeoNorge Themes:https://register.geonorge.no/metadata-kodelister/nasjonal-temainndeling";
        String keywords "GCMDSK:WEATHER FORECAST, GEMET:Meteorological geographical features, GEMET:Atmospheric conditions, NORTHEMES:Weather and climate";
        String dataset_production_status "Complete";
        String dataset_production_version "v0.0.1";
        String iso_topic_category "climatologyMeteorologyAtmosphere";
        String spatial_representation "grid";
        String date_created "2026-10-23T13:47:37Z";
        String naming_authority "no.met";
        String collections "ADC, METNCS";
        String processing_level "Operational";
        String creator_name "Senter for utvikling av værvarslingstjenesten";
        String creator_institution "Norwegian Meteorological Institute";
        String creator_type "group";
        String creator_email "post@met.no";
        String publisher_type "institution";
        String publisher_email "data-management-group@met.no";
        String publisher_url "https://data.met.no";
        String publisher_name "Norwegian Meteorological Institute";
        String summary "This file contains pressure level parameters from control member 0. For more information, please visit https://github.com/metno/NWPdocs/wiki";
        String summary_no "Denne filen inneholder overflateparametere fra kontrollmedlem 0.For mer informasjon, se https://github.com/metno/NWPdocs/wiki ";
        String title "Meps 2.5Km deterministic pressure level parameters 2026-10-23T12:00:00Z + 0 hours";
        String title_no "Meps 2.5Km deterministisk trykknivåparamere 2026-10-23T12:00:00Z + 0 hours";
        String related_dataset "no.met:0f5116ba-43dd-42b8-a0ef-1f49c4b27812 (parent)";
    }
    DODS_EXTRA {
        String Unlimited_Dimension "time";
    }
}
