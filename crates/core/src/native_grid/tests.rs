use super::*;

/// The WGS 84 base of the projections MeteoCore computes on that ellipsoid.
macro_rules! wgs84 {
    () => {
        r#"BASEGEOGCRS["Unknown based on WGS 84 ellipsoid",DATUM["Unknown based on WGS 84 ellipsoid",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]]"#
    };
}

/// Easting then northing, in metres.
macro_rules! east_north {
    () => {
        r#"CS[Cartesian,2],AXIS["easting (X)",east,ORDER[1],LENGTHUNIT["metre",1]],AXIS["northing (Y)",north,ORDER[2],LENGTHUNIT["metre",1]]]"#
    };
}

// Each WKT2 below was read back by PROJ 9.8.1 (`projinfo '<wkt>' -o PROJ`)
// to the parameters of the `Crs` it pins, and `cs2cs EPSG:4326 '<wkt>'`
// projected a point (25°E 60°N; 10° east of the sub-satellite point at
// 20°N for the geostationary views) to within 0.1 mm of `Crs::forward`.
// CI has no PROJ, so the strings are pinned here (#1237):
//
// - TM35: +proj=utm +zone=35 +ellps=WGS84
// - OPERA_LAEA: +proj=laea +lat_0=55 +lon_0=10 +x_0=1950000 +y_0=-2100000 +ellps=WGS84
// - MEPS_LCC: +proj=lcc +lat_0=63.3 +lon_0=15 +lat_1=63.3 +lat_2=63.3 +R=6371000
// - LCC_WGS84: +proj=lcc +lat_0=0 +lon_0=0 +lat_1=58.964 +lat_2=69.987 +ellps=WGS84
// - POLAR_STERE: +proj=stere +lat_0=90 +lon_0=10 +k=0.933012701892 +ellps=WGS84
// - DMI_STERE: +proj=stere +lat_0=56 +lon_0=10.5666 +k=1 +ellps=WGS84
// - ROTATED: +proj=ob_tran +o_proj=longlat +o_lon_p=0 +o_lat_p=30 +lon_0=0 +ellps=WGS84
// - GOES_GEOS: +proj=geos +sweep=x +lon_0=-75 +h=35786023 +ellps=GRS80
// - HIMAWARI_GEOS: +proj=geos +lon_0=140.7 +h=35785863 +a=6378137 +rf=298.257024882273
// - WEB_MERCATOR: +proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 +k=1 +nadgrids=@null

const TM35: &str = concat!(
    r#"PROJCRS["Transverse Mercator","#,
    wgs84!(),
    r#",CONVERSION["Transverse Mercator",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",27,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const OPERA_LAEA: &str = concat!(
    r#"PROJCRS["Lambert azimuthal equal area","#,
    wgs84!(),
    r#",CONVERSION["Lambert azimuthal equal area",METHOD["Lambert Azimuthal Equal Area",ID["EPSG",9820]],PARAMETER["Latitude of natural origin",55,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",10,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["False easting",1950000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",-2100000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const MEPS_LCC: &str = concat!(
    r#"PROJCRS["Lambert conformal conic",BASEGEOGCRS["Unknown based on a sphere",DATUM["Unknown based on a sphere of radius 6371000 m",ELLIPSOID["Sphere",6371000,0,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Lambert conformal conic",METHOD["Lambert Conic Conformal (2SP)",ID["EPSG",9802]],PARAMETER["Latitude of false origin",63.3,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8821]],PARAMETER["Longitude of false origin",15,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8822]],PARAMETER["Latitude of 1st standard parallel",63.3,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8823]],PARAMETER["Latitude of 2nd standard parallel",63.3,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8824]],PARAMETER["Easting at false origin",0,LENGTHUNIT["metre",1],ID["EPSG",8826]],PARAMETER["Northing at false origin",0,LENGTHUNIT["metre",1],ID["EPSG",8827]]],"#,
    east_north!(),
);

const LCC_WGS84: &str = concat!(
    r#"PROJCRS["Lambert conformal conic","#,
    wgs84!(),
    r#",CONVERSION["Lambert conformal conic",METHOD["Lambert Conic Conformal (2SP)",ID["EPSG",9802]],PARAMETER["Latitude of false origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8821]],PARAMETER["Longitude of false origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8822]],PARAMETER["Latitude of 1st standard parallel",58.964,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8823]],PARAMETER["Latitude of 2nd standard parallel",69.987,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8824]],PARAMETER["Easting at false origin",0,LENGTHUNIT["metre",1],ID["EPSG",8826]],PARAMETER["Northing at false origin",0,LENGTHUNIT["metre",1],ID["EPSG",8827]]],"#,
    east_north!(),
);

const POLAR_STERE: &str = concat!(
    r#"PROJCRS["Polar stereographic","#,
    wgs84!(),
    r#",CONVERSION["Polar stereographic",METHOD["Polar Stereographic (variant A)",ID["EPSG",9810]],PARAMETER["Latitude of natural origin",90,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",10,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.933012701892,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const DMI_STERE: &str = concat!(
    r#"PROJCRS["Stereographic","#,
    wgs84!(),
    r#",CONVERSION["Stereographic",METHOD["Stereographic"],PARAMETER["Latitude of natural origin",56,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",10.5666,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",1,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const ROTATED: &str = concat!(
    r#"GEOGCRS["Rotated pole","#,
    wgs84!(),
    r#",DERIVINGCONVERSION["Pole rotation (GRIB convention)",METHOD["Pole rotation (GRIB convention)"],PARAMETER["Latitude of the southern pole (GRIB convention)",-30,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Longitude of the southern pole (GRIB convention)",0,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Axis rotation (GRIB convention)",0,ANGLEUNIT["degree",0.0174532925199433]]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]]"#,
);

const GOES_GEOS: &str = concat!(
    r#"PROJCRS["Geostationary satellite view",BASEGEOGCRS["Unknown based on an ellipsoid",DATUM["Unknown based on an ellipsoid",ELLIPSOID["Unknown",6378137,298.257222096042,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Geostationary satellite view",METHOD["Geostationary Satellite (Sweep X)"],PARAMETER["Longitude of natural origin",-75,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Satellite Height",35786023,LENGTHUNIT["metre",1]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const HIMAWARI_GEOS: &str = concat!(
    r#"PROJCRS["Geostationary satellite view",BASEGEOGCRS["Unknown based on an ellipsoid",DATUM["Unknown based on an ellipsoid",ELLIPSOID["Unknown",6378137,298.257024882273,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Geostationary satellite view",METHOD["Geostationary Satellite (Sweep Y)"],PARAMETER["Longitude of natural origin",140.7,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Satellite Height",35785863,LENGTHUNIT["metre",1]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const WEB_MERCATOR: &str = concat!(
    r#"PROJCRS["Web Mercator","#,
    wgs84!(),
    r#",CONVERSION["Web Mercator",METHOD["Popular Visualisation Pseudo Mercator",ID["EPSG",1024]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);
// The earth figures of #1192, each read back by PROJ 9.8.1 (`projinfo
// '<wkt>' -o PROJ`) to its `Crs`, and `cs2cs EPSG:4326 '<wkt>'` of 25°E
// 60°N (BNG: 0.1276°W 51.5072°N) and 10°E 50°N within 0.1 mm of
// `Crs::forward`, pinned below in `figure_wkt2_projects_where_proj_reads_it`:
//
// - TM35_GRS80: +proj=utm +zone=35 +ellps=GRS80
// - BNG_AIRY: +proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +ellps=airy
// - POLAR_STERE_SPHERE: +proj=stere +lat_0=90 +lon_0=-105 +k=0.933012701892 +R=6371229
// - POLAR_STERE_HUGHES: +proj=stere +lat_0=90 +lon_0=-45 +k=0.969858189439 +a=6378273 +rf=298.279411123064
// - EASE_LAEA_SPHERE: +proj=laea +lat_0=90 +lon_0=0 +R=6371228
// - OBLIQUE_STERE_SPHERE: +proj=stere +lat_0=60 +lon_0=25 +k=0.9999 +x_0=600000 +y_0=-200000 +R=6371000

const TM35_GRS80: &str = concat!(
    r#"PROJCRS["Transverse Mercator",BASEGEOGCRS["Unknown based on GRS 1980 ellipsoid",DATUM["Unknown based on GRS 1980 ellipsoid",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Transverse Mercator",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",27,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const BNG_AIRY: &str = concat!(
    r#"PROJCRS["Transverse Mercator",BASEGEOGCRS["Unknown based on an ellipsoid",DATUM["Unknown based on an ellipsoid",ELLIPSOID["Unknown",6377563.396,299.3249646,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Transverse Mercator",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",49,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",-2,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996012717,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",400000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",-100000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const POLAR_STERE_SPHERE: &str = concat!(
    r#"PROJCRS["Polar stereographic",BASEGEOGCRS["Unknown based on a sphere",DATUM["Unknown based on a sphere of radius 6371229 m",ELLIPSOID["Sphere",6371229,0,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Polar stereographic",METHOD["Polar Stereographic (variant A)",ID["EPSG",9810]],PARAMETER["Latitude of natural origin",90,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",-105,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.933012701892,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const POLAR_STERE_HUGHES: &str = concat!(
    r#"PROJCRS["Polar stereographic",BASEGEOGCRS["Unknown based on an ellipsoid",DATUM["Unknown based on an ellipsoid",ELLIPSOID["Unknown",6378273,298.279411123064,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Polar stereographic",METHOD["Polar Stereographic (variant A)",ID["EPSG",9810]],PARAMETER["Latitude of natural origin",90,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",-45,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.969858189439,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const EASE_LAEA_SPHERE: &str = concat!(
    r#"PROJCRS["Lambert azimuthal equal area",BASEGEOGCRS["Unknown based on a sphere",DATUM["Unknown based on a sphere of radius 6371228 m",ELLIPSOID["Sphere",6371228,0,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Lambert azimuthal equal area",METHOD["Lambert Azimuthal Equal Area",ID["EPSG",9820]],PARAMETER["Latitude of natural origin",90,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

const OBLIQUE_STERE_SPHERE: &str = concat!(
    r#"PROJCRS["Stereographic",BASEGEOGCRS["Unknown based on a sphere",DATUM["Unknown based on a sphere of radius 6371000 m",ELLIPSOID["Sphere",6371000,0,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Stereographic",METHOD["Stereographic"],PARAMETER["Latitude of natural origin",60,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",25,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9999,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",600000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",-200000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],"#,
    east_north!(),
);

fn d(degrees: f64) -> f64 {
    degrees.to_radians()
}

fn tm35fin() -> Crs {
    Crs::TransverseMercator {
        lat0: 0.0,
        lon0: d(27.0),
        k0: 0.9996,
        false_e: 500_000.0,
        false_n: 0.0,
        figure: Ellipsoid::WGS84,
    }
}

fn meps() -> Crs {
    Crs::LambertConformalConic {
        lat1: d(63.3),
        lat2: d(63.3),
        lat0: d(63.3),
        lon0: d(15.0),
        false_e: 0.0,
        false_n: 0.0,
        radius: Some(6_371_000.0),
    }
}

#[test]
fn every_crs_has_its_pinned_wkt2() {
    let cases = [
        (tm35fin(), TM35),
        (
            Crs::LambertAzimuthalEqualArea {
                lat0: d(55.0),
                lon0: d(10.0),
                false_e: 1_950_000.0,
                false_n: -2_100_000.0,
                figure: Ellipsoid::WGS84,
            },
            OPERA_LAEA,
        ),
        (meps(), MEPS_LCC),
        (
            Crs::LambertConformalConic {
                lat1: d(58.964),
                lat2: d(69.987),
                lat0: 0.0,
                lon0: 0.0,
                false_e: 0.0,
                false_n: 0.0,
                radius: None,
            },
            LCC_WGS84,
        ),
        (
            Crs::Stereographic {
                lat0: d(90.0),
                lon0: d(10.0),
                k0: 0.933_012_701_892_219_3,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::WGS84,
            },
            POLAR_STERE,
        ),
        (
            Crs::Stereographic {
                lat0: d(56.0),
                lon0: d(10.5666),
                k0: 1.0,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::WGS84,
            },
            DMI_STERE,
        ),
        (
            Crs::RotatedLatLon {
                south_pole_lat: d(-30.0),
                south_pole_lon: 0.0,
            },
            ROTATED,
        ),
        (
            Crs::Geostationary {
                lon0: d(-75.0),
                height: 35_786_023.0,
                semi_major: 6_378_137.0,
                semi_minor: 6_356_752.314_14,
                sweep: SweepAxis::X,
            },
            GOES_GEOS,
        ),
        (
            Crs::Geostationary {
                lon0: d(140.7),
                height: 35_785_863.0,
                semi_major: 6_378_137.0,
                semi_minor: 6_356_752.3,
                sweep: SweepAxis::Y,
            },
            HIMAWARI_GEOS,
        ),
        (Crs::WebMercator, WEB_MERCATOR),
        (Crs::Wgs84, CRS84_WKT),
    ];
    for (crs, wkt) in cases {
        assert_eq!(crs_wkt2(&crs), wkt, "{crs:?}");
    }
}

/// The figured CRSs of #1192 with their WKT2 and where PROJ projects 10°E
/// 50°N through it.
fn figured() -> Vec<(Crs, &'static str, (f64, f64))> {
    let hughes = Ellipsoid::from_inverse_flattening(6_378_273.0, 298.279_411_123_064);
    let sphere = Ellipsoid::sphere(6_371_229.0);
    vec![
        (
            Crs::TransverseMercator {
                lat0: 0.0,
                lon0: d(27.0),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
                figure: Ellipsoid::GRS80,
            },
            TM35_GRS80,
            (-715_001.983_5, 5_678_591.055_2),
        ),
        (
            Crs::TransverseMercator {
                lat0: d(49.0),
                lon0: d(-2.0),
                k0: 0.999_601_271_7,
                false_e: 400_000.0,
                false_n: -100_000.0,
                figure: Ellipsoid::from_inverse_flattening(6_377_563.396, 299.324_964_6),
            },
            BNG_AIRY,
            (1_258_793.770_2, 80_522.676_9),
        ),
        (
            Crs::Stereographic {
                lat0: d(90.0),
                lon0: d(-105.0),
                k0: crate::geo::polar_stereographic_k0(d(60.0), &sphere),
                false_e: 0.0,
                false_n: 0.0,
                figure: sphere,
            },
            POLAR_STERE_SPHERE,
            (3_921_772.047_0, 1_828_752.339_0),
        ),
        (
            Crs::Stereographic {
                lat0: d(90.0),
                lon0: d(-45.0),
                k0: crate::geo::polar_stereographic_k0(d(70.0), &hughes),
                false_e: 0.0,
                false_n: 0.0,
                figure: hughes,
            },
            POLAR_STERE_HUGHES,
            (3_695_272.273_2, -2_587_457.501_4),
        ),
        (
            Crs::LambertAzimuthalEqualArea {
                lat0: d(90.0),
                lon0: 0.0,
                false_e: 0.0,
                false_n: 0.0,
                figure: Ellipsoid::sphere(6_371_228.0),
            },
            EASE_LAEA_SPHERE,
            (756_789.429_3, -4_291_966.131_7),
        ),
        (
            Crs::Stereographic {
                lat0: d(60.0),
                lon0: d(25.0),
                k0: 0.9999,
                false_e: 600_000.0,
                false_n: -200_000.0,
                figure: Ellipsoid::sphere(6_371_000.0),
            },
            OBLIQUE_STERE_SPHERE,
            (-473_846.769_3, -1_198_419.407_4),
        ),
    ]
}

/// A figure flows into the WKT2: WGS 84 and GRS 1980 by name, a sphere,
/// or another ellipsoid by its axis and inverse flattening (#1192).
#[test]
fn figure_wkt2_names_the_ellipsoid() {
    for (crs, wkt, _) in figured() {
        assert_eq!(crs_wkt2(&crs), wkt, "{crs:?}");
    }
    assert!(TM35_GRS80.contains(r#"DATUM["Unknown based on GRS 1980 ellipsoid""#));
    assert!(POLAR_STERE_SPHERE.contains(r#"ELLIPSOID["Sphere",6371229,0,"#));
    assert!(BNG_AIRY.contains(r#"ELLIPSOID["Unknown",6377563.396,299.3249646,"#));
}

/// Each figured CRS projects 10°E 50°N where PROJ puts it reading its
/// WKT2 (`cs2cs EPSG:4326 '<wkt>'`), so the text describes the math.
#[test]
fn figure_wkt2_projects_where_proj_reads_it() {
    for (crs, _, (x_proj, y_proj)) in figured() {
        let (x, y) = crs.forward(10.0, 50.0);
        assert!(
            (x - x_proj).abs() < 1e-3 && (y - y_proj).abs() < 1e-3,
            "{crs:?}: ({x}, {y}), PROJ ({x_proj}, {y_proj})"
        );
    }
}

/// A grid from a north-up `GeoTransform`: the outer corner of minimum x
/// and y, the positive cell size, the edges `origin + size × spacing`.
#[test]
fn a_geo_transform_grid_is_its_outer_cells() {
    // The EPSG:3067 radar fixture (testdata/radar-tm35fin).
    let gt = GeoTransform {
        origin_x: -196_593.004,
        origin_y: 8_084_432.005,
        pixel_width: 2_584.937_385_416_667,
        pixel_height: 5_080.839_738_888_889,
        width: 480,
        height: 360,
        crs: tm35fin(),
    };
    let grid = NativeGrid::from_geo_transform(&gt, Some(3067)).unwrap();
    assert_eq!(grid.size(), [480, 360]);
    assert_eq!(
        grid.spacing(),
        [2_584.937_385_416_667, 5_080.839_738_888_889]
    );
    let [x0, y0, x1, y1] = grid.bbox();
    assert_eq!(x0, -196_593.004);
    assert!((y0 - 6_255_329.699).abs() < 1e-6, "{y0}");
    assert!((x1 - 1_044_176.941).abs() < 1e-6, "{x1}");
    assert_eq!(y1, 8_084_432.005);
    assert_eq!(grid.unit(), "m");
    assert!(!grid.is_lon_lat());
    // The source's code is the reference; the parameters are not consulted.
    assert_eq!(grid.crs_ref(), "http://www.opengis.net/def/crs/EPSG/0/3067");
    assert_eq!(grid.authority_uri(), Some(grid.crs_ref()));
    assert_eq!(grid.epsg(), Some(3067));
}

/// Without a stated code the reference is the WKT2, even for parameters
/// that match a registered CRS.
#[test]
fn a_grid_without_a_stated_code_is_described_by_its_parameters() {
    let grid = NativeGrid::new(tm35fin(), None, [2, 3], [0.0, 0.0], [1000.0, 1000.0]).unwrap();
    assert_eq!(grid.crs_ref(), TM35);
    assert_eq!(grid.authority_uri(), None);
    let meps = NativeGrid::from_edges(
        meps(),
        None,
        [949, 1069],
        [-1_060_084.0, -1_332_469.0, 1_312_416.0, 1_340_031.0],
    )
    .unwrap();
    assert_eq!(meps.spacing(), [2500.0, 2500.0]);
    assert_eq!(meps.crs_ref(), MEPS_LCC);
    assert_eq!(wkt_name(meps.crs_ref()), Some("Lambert conformal conic"));
    let rotated = NativeGrid::new(
        Crs::RotatedLatLon {
            south_pole_lat: d(-30.0),
            south_pole_lon: 0.0,
        },
        None,
        [10, 10],
        [-5.0, -5.0],
        [1.0, 1.0],
    )
    .unwrap();
    assert_eq!(rotated.unit(), "deg");
    assert_eq!(wkt_name(rotated.crs_ref()), Some("Rotated pole"));
    let lonlat = NativeGrid::new(Crs::Wgs84, None, [4, 2], [0.0, 0.0], [0.5, 0.5]).unwrap();
    assert!(lonlat.is_lon_lat());
    assert_eq!(lonlat.unit(), "deg");
}

#[test]
fn an_empty_or_inverted_grid_is_none() {
    assert!(NativeGrid::new(Crs::Wgs84, None, [0, 2], [0.0, 0.0], [1.0, 1.0]).is_none());
    assert!(NativeGrid::new(Crs::Wgs84, None, [2, 2], [0.0, 0.0], [1.0, 0.0]).is_none());
    assert!(NativeGrid::new(Crs::Wgs84, None, [2, 2], [f64::NAN, 0.0], [1.0, 1.0]).is_none());
    assert!(NativeGrid::from_edges(Crs::Wgs84, None, [2, 2], [1.0, 0.0, 0.0, 1.0]).is_none());
    assert!(NativeGrid::from_edges(Crs::Wgs84, None, [0, 2], [0.0, 0.0, 1.0, 1.0]).is_none());
}

/// The registered CRSs MeteoCore advertises, with PROJ 9.8's names and
/// axis orders (`projinfo EPSG:<code>`): EPSG:3035 northing first.
#[test]
fn registered_crs_names_and_axis_orders() {
    let tm35 = registered_crs("http://www.opengis.net/def/crs/EPSG/0/3067").unwrap();
    assert_eq!(
        (tm35.name, tm35.unit, tm35.northing_first),
        ("EUREF-FIN / TM35FIN(E,N)", "m", false)
    );
    let laea = registered_crs("http://www.opengis.net/def/crs/EPSG/0/3035").unwrap();
    assert_eq!(
        (laea.name, laea.northing_first),
        ("ETRS89-extended / LAEA Europe", true)
    );
    let crs84 = registered_crs("http://www.opengis.net/def/crs/OGC/1.3/CRS84").unwrap();
    assert_eq!((crs84.unit, crs84.northing_first), ("deg", false));
    assert!(registered_crs("http://www.opengis.net/def/crs/EPSG/0/32635").is_none());
    // Every advertised storage CRS has an entry.
    for label in ["CRS:84", "EPSG:4326", "EPSG:3857", "EPSG:3067", "EPSG:3035"] {
        let uri = crate::geo::native_crs_uri(label).unwrap();
        assert!(registered_crs(uri).is_some(), "{label}");
    }
}

#[test]
fn wkt_names() {
    assert_eq!(wkt_name(CRS84_WKT), Some("WGS 84 (CRS84)"));
    assert_eq!(wkt_name(GOES_GEOS), Some("Geostationary satellite view"));
    assert_eq!(wkt_name(r#"PROJCRS["",…]"#), None);
    assert_eq!(wkt_name("PROJCRS"), None);
}

#[test]
fn wkt_numbers_drop_float_noise() {
    assert_eq!(num(27.000_000_000_000_004), "27");
    assert_eq!(num(-0.0), "0");
    assert_eq!(num(0.9996), "0.9996");
    assert_eq!(num(-2_100_000.0), "-2100000");
    assert_eq!(deg(d(10.5666)), "10.5666");
    // Twelve decimals of a seven-digit number are past an f64's digits.
    assert_eq!(num(6_377_563.396), "6377563.396");
    assert_eq!(num(1.0 / (1.0 / 299.324_964_6)), "299.3249646");
}
