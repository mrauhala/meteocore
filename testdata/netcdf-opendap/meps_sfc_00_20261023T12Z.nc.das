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
    mean_sea_level {
        String description "mean sea level";
        String long_name "mean_sea_level";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    surface {
        String description "ground or water surface";
        String long_name "surface";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height_above_msl {
        String description "height above MSL";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height0 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height1 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height2 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height3 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height4 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 10;
    }
    height5 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    height6 {
        String description "height above ground";
        String long_name "height";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    grib2_vLevel14 {
        String units "1";
        String long_name "grib2-level 14";
        UInt32 _ChunkSizes 1;
    }
    grib2_vLevel16 {
        String units "1";
        String long_name "grib2-level 16";
        UInt32 _ChunkSizes 1;
    }
    local_vertical_level_192 {
        String description "level of isotherm zero wet bulb";
        String long_name "isotherm zero wet bulb";
        String positive "up";
        UInt32 _ChunkSizes 1;
    }
    isotherm_0C_level {
        String description "level of 0degreeC isotherm";
        String long_name "isotherm 0C";
        String positive "up";
        UInt32 _ChunkSizes 1;
    }
    isotherm_0C_level_from_above {
        String description "level of 0degreeC isotherm from above";
        String long_name "isotherm 0C from above";
        String positive "up";
        UInt32 _ChunkSizes 1;
    }
    adiabatic_condensation_level {
        String long_name "Adiabatic condensation level";
        String positive "up";
        String units "m";
        UInt32 _ChunkSizes 1;
    }
    maximum_wind_level {
        String long_name "max wind level";
        String positive "up";
        UInt32 _ChunkSizes 1;
    }
    tropopause {
        String long_name "Tropopause";
        String positive "up";
        UInt32 _ChunkSizes 1;
    }
    top_of_atmosphere {
        String long_name "nominal top of atmosphere";
        String positive "up";
        UInt32 _ChunkSizes 1;
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
    air_temperature_2m {
        Float32 _FillValue 9.96921E36;
        String long_name "Screen level temperature (T2M)";
        String standard_name "air_temperature";
        String units "K";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    relative_humidity_2m {
        Float32 _FillValue 9.96921E36;
        String long_name "Screen level relative humidity (RH2M)";
        String standard_name "relative_humidity";
        String units "1";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    x_wind_10m {
        Float32 _FillValue 9.96921E36;
        String long_name "10 metre wind along x-coordinate (U10M)";
        String standard_name "x_wind";
        String units "m/s";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    y_wind_10m {
        Float32 _FillValue 9.96921E36;
        String long_name "10 metre wind along y-coordinate (V10M)";
        String standard_name "y_wind";
        String units "m/s";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    cloud_area_fraction {
        Float32 _FillValue 9.96921E36;
        String long_name "Total cloud cover (TCC)";
        String standard_name "cloud_area_fraction";
        String units "1";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    air_pressure_at_sea_level {
        Float32 _FillValue 9.96921E36;
        String long_name "Mean Sea Level Pressure (MSLP)";
        String standard_name "air_pressure_at_sea_level";
        String units "Pa";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    precipitation_amount_acc {
        Float32 _FillValue 9.96921E36;
        String long_name "Accumulated total precipitation";
        String standard_name "precipitation_amount";
        String units "kg/m^2";
        String grid_mapping "projection_lambert";
        String coordinates "longitude latitude";
        UInt32 _ChunkSizes 1, 1, 1069, 949;
    }
    wind_speed_of_gust {
        Float32 _FillValue 9.96921E36;
        String long_name "Wind gust";
        String standard_name "wind_speed_of_gust";
        String units "m/s";
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
        String id "4e82cf58-af79-42cb-802e-b9728c023c55";
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
        String date_created "2026-10-23T13:43:59Z";
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
        String summary "This file contains surface parameters control member 0. For more information, please visit https://github.com/metno/NWPdocs/wiki";
        String summary_no "Denne filen inneholder overflateparametere fra kontrollmedlem 0.For mer informasjon, se https://github.com/metno/NWPdocs/wiki ";
        String title "Meps 2.5Km deterministic surface parameters 2026-10-23T12:00:00Z + 00 hours";
        String title_no "Meps 2.5Km deterministisk overflateparametere 2026-10-23T12:00:00Z + 00 hours";
        String related_dataset "no.met:fc0974ef-8f78-406f-8c62-165f7e882212 (parent)";
    }
    DODS_EXTRA {
        String Unlimited_Dimension "time";
    }
}
