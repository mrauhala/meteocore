use super::*;

mod derived;
mod turned;

const EPS: f64 = 1e-9;

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < EPS)
}

/// The meteorological convention, pinned by hand: a wind from the north
/// blows southward (v < 0) and has direction 0°; from the east, westward
/// (u < 0), 90°. Speed is the hypotenuse.
#[test]
fn direction_is_where_the_wind_blows_from() {
    for (u, v, from) in [
        (0.0, -10.0, 0.0),   // northerly
        (-10.0, 0.0, 90.0),  // easterly
        (0.0, 10.0, 180.0),  // southerly
        (10.0, 0.0, 270.0),  // westerly
        (-1.0, -1.0, 45.0),  // north-easterly
        (1.0, 1.0, 225.0),   // south-westerly
        (-0.0, -3.0, 0.0),   // signed zero stays north
        (0.0, 3.0, 180.0),   // atan2(-0, -3) = -π folds to 180
        (1e-300, -1.0, 0.0), // a hair west of north rounds to 0, never 360
    ] {
        let got = from_direction(Some(u), Some(v)).unwrap();
        assert!((got - from).abs() < EPS, "({u}, {v}): {got} ≠ {from}");
        assert!((0.0..360.0).contains(&got), "({u}, {v}): {got}");
        assert!(close(speed(Some(u), Some(v)), u.hypot(v)));
    }
    // 10 m/s from 150°: u = −10 sin 150°, v = −10 cos 150°.
    let (u, v) = (-5.0, 10.0 * (3f64.sqrt() / 2.0));
    assert!(close(from_direction(Some(u), Some(v)), 150.0));
    assert!(close(speed(Some(u), Some(v)), 10.0));
    assert!(close(speed(Some(3.0), Some(-4.0)), 5.0));
}

#[test]
fn calm_has_a_speed_but_no_direction_and_nodata_has_neither() {
    assert_eq!(speed(Some(0.0), Some(0.0)), Some(0.0));
    assert_eq!(from_direction(Some(0.0), Some(0.0)), None);
    assert_eq!(from_direction(Some(-0.0), Some(0.0)), None);
    for (u, v) in [
        (None, Some(1.0)),
        (Some(1.0), None),
        (None, None),
        (Some(f64::NAN), Some(1.0)),
        (Some(1.0), Some(f64::INFINITY)),
    ] {
        assert_eq!(speed(u, v), None, "{u:?} {v:?}");
        assert_eq!(from_direction(u, v), None, "{u:?} {v:?}");
    }
}

/// GRIB2 flag table 3.3 bit 5 (0x08): ECMWF and NCEP lat/lon grids carry
/// 0x30 (both increments given, earth-relative); a grid-relative source
/// sets 0x08 on top.
#[test]
fn frames_come_from_the_grib_flag_and_the_cf_standard_name() {
    assert_eq!(VectorFrame::from_grib_flags(0x30), VectorFrame::Earth);
    assert_eq!(VectorFrame::from_grib_flags(0x38), VectorFrame::Grid);
    assert_eq!(VectorFrame::from_grib_flags(0x08), VectorFrame::Grid);
    assert_eq!(VectorFrame::from_grib_flags(0x00), VectorFrame::Earth);
    for (name, frame) in [
        ("eastward_wind", VectorFrame::Earth),
        ("northward_wind", VectorFrame::Earth),
        ("x_wind", VectorFrame::Grid),
        ("y_wind", VectorFrame::Grid),
        ("grid_eastward_wind", VectorFrame::Grid),
        ("grid_northward_wind", VectorFrame::Grid),
        ("air_temperature", VectorFrame::Unknown),
    ] {
        assert_eq!(VectorFrame::from_standard_name(name), frame, "{name}");
    }
    assert_eq!(GridAxes::of(&Crs::Wgs84), GridAxes::NorthAligned);
    assert_eq!(GridAxes::of(&Crs::WebMercator), GridAxes::NorthAligned);
    let lcc = Crs::LambertConformalConic {
        lat1: 1.1,
        lat2: 1.1,
        lat0: 1.1,
        lon0: 0.26,
        false_e: 0.0,
        false_n: 0.0,
        radius: Some(6_371_220.0),
    };
    assert_eq!(GridAxes::of(&lcc), GridAxes::Rotated(Convergence::of(&lcc)));
    assert!(matches!(GridAxes::of(&lcc), GridAxes::Rotated(Some(_))));
    let rotated = Crs::RotatedLatLon {
        south_pole_lat: -0.5,
        south_pole_lon: 0.3,
    };
    assert!(matches!(GridAxes::of(&rotated), GridAxes::Rotated(Some(_))));
    // Parallels symmetric about the equator make no cone: a conformal grid
    // whose convergence is unknown.
    let no_cone = Crs::LambertConformalConic {
        lat1: 0.5,
        lat2: -0.5,
        lat0: 0.0,
        lon0: 0.26,
        false_e: 0.0,
        false_n: 0.0,
        radius: Some(6_371_220.0),
    };
    assert_eq!(GridAxes::of(&no_cone), GridAxes::Rotated(None));
    let laea = Crs::LambertAzimuthalEqualArea {
        lat0: 0.9,
        lon0: 0.17,
        false_e: 0.0,
        false_n: 0.0,
        figure: crate::geo::Ellipsoid::WGS84,
    };
    assert_eq!(GridAxes::of(&laea), GridAxes::Skewed);
}

#[test]
fn the_frame_rules() {
    use Derivable::*;
    use GridAxes::*;
    use VectorFrame::*;
    let convergence = Convergence::of(&meps_lcc()).unwrap();
    let turned = Rotated(Some(convergence));
    for grid in [NorthAligned, turned, Rotated(None), Skewed] {
        // Earth-relative: always both. Mixed: never anything.
        assert_eq!(derivable(Earth, Earth, grid), SpeedAndDirection);
        assert!(matches!(derivable(Earth, Grid, grid), Nothing(_)));
        assert!(matches!(derivable(Grid, Earth, grid), Nothing(_)));
    }
    // Grid-relative: no turn on a north-aligned grid, the grid's own on a
    // rotated one, and speed only when the grid's convergence is unknown.
    assert_eq!(derivable(Grid, Grid, NorthAligned), SpeedAndDirection);
    assert_eq!(
        derivable(Grid, Grid, turned),
        SpeedAndTurnedDirection(convergence)
    );
    assert!(matches!(derivable(Grid, Grid, Rotated(None)), SpeedOnly(_)));
    assert!(matches!(derivable(Grid, Grid, Skewed), Nothing(_)));
    // Unknown: never a direction; a speed only where the axes are orthogonal.
    for (u, v) in [(Unknown, Unknown), (Unknown, Earth), (Grid, Unknown)] {
        assert!(matches!(derivable(u, v, NorthAligned), SpeedOnly(_)));
        assert!(matches!(derivable(u, v, turned), SpeedOnly(_)));
        assert!(matches!(derivable(u, v, Rotated(None)), SpeedOnly(_)));
        assert!(matches!(derivable(u, v, Skewed), Nothing(_)));
    }
}

/// MEPS's grid mapping: a cone tangent at 63.3° about 15°E, on the
/// 6 371 km sphere.
fn meps_lcc() -> Crs {
    Crs::LambertConformalConic {
        lat1: 63.3f64.to_radians(),
        lat2: 63.3f64.to_radians(),
        lat0: 63.3f64.to_radians(),
        lon0: 15f64.to_radians(),
        false_e: 0.0,
        false_n: 0.0,
        radius: Some(6_371_000.0),
    }
}

fn lambert() -> GridAxes {
    GridAxes::of(&meps_lcc())
}

/// The sign convention, worked by hand. On MEPS's cone, at 25°E, 10° east
/// of the central meridian, the meridians lean in toward the apex above
/// 15°E: true north points up and to the left on the grid, so grid north
/// lies γ = n (λ − λ₀) = 10° · sin 63.3° ≈ 8.934° east of true north. A
/// wind from grid north there (u = 0, v < 0 along the grid) comes from
/// 8.934°, east of true north; one from grid west (u > 0) from 278.934°.
/// West of the meridian, at 5°E, grid north lies 8.934° west of true north.
/// The speed is the same in either frame.
#[test]
fn a_wind_from_grid_north_east_of_the_meridian_comes_from_east_of_true_north() {
    let gamma = 10.0 * 63.3f64.to_radians().sin();
    assert!((gamma - 8.933_714).abs() < 1e-6);
    let convergence = Convergence::of(&meps_lcc()).unwrap();
    let east = convergence.degrees(25.0, 60.0);
    let west = convergence.degrees(5.0, 60.0);
    assert!((east.unwrap() - gamma).abs() < 1e-9);
    assert!((west.unwrap() + gamma).abs() < 1e-9);
    for (u, v, at, want) in [
        (0.0, -10.0, east, gamma),
        (10.0, 0.0, east, 270.0 + gamma),
        (-10.0, 0.0, east, 90.0 + gamma),
        (0.0, -10.0, west, 360.0 - gamma),
        (0.0, 10.0, west, 180.0 - gamma),
    ] {
        let got = from_grid_direction(Some(u), Some(v), at).unwrap();
        assert!((got - want).abs() < 1e-9, "({u}, {v}): {got} ≠ {want}");
        let (ue, ve) = to_earth_relative(u, v, at.unwrap());
        assert!(
            (ue.hypot(ve) - 10.0).abs() < 1e-12,
            "the turn keeps the speed"
        );
    }
    // The wind from grid north east of the meridian blows toward the
    // south-southwest: earth-relative u and v both negative.
    let (ue, ve) = to_earth_relative(0.0, -10.0, gamma);
    assert!((ue + 10.0 * gamma.to_radians().sin()).abs() < 1e-12);
    assert!((ve + 10.0 * gamma.to_radians().cos()).abs() < 1e-12);
    // Calm has no direction in any frame; a missing angle gives none.
    assert_eq!(from_grid_direction(Some(0.0), Some(0.0), east), None);
    assert_eq!(from_grid_direction(Some(0.0), Some(-1.0), None), None);
    assert_eq!(
        from_grid_direction(Some(0.0), Some(-1.0), Some(f64::NAN)),
        None
    );
    assert_eq!(from_grid_direction(None, Some(-1.0), east), None);
}

/// A polar stereographic and a Transverse Mercator grid, by their PROJ
/// convergences (`proj -V`, PROJ 9.8.1): `+proj=stere +lat_0=90 +lat_ts=60
/// +lon_0=-45 +ellps=WGS84` at (10°E, 70°N) is 55°, and TM35FIN
/// (`+proj=utm +zone=35 +ellps=GRS80`) at (30°E, 65°N) is 2.71936863°. A
/// wind from grid north comes from γ, one from grid south from 180° + γ.
#[test]
fn turned_directions_on_polar_stereographic_and_transverse_mercator_grids() {
    let wgs84 = crate::geo::Ellipsoid::WGS84;
    let polar = Crs::Stereographic {
        lat0: 90f64.to_radians(),
        lon0: (-45f64).to_radians(),
        k0: crate::geo::polar_stereographic_k0(60f64.to_radians(), &wgs84),
        false_e: 0.0,
        false_n: 0.0,
        figure: wgs84,
    };
    let tm35fin = Crs::TransverseMercator {
        lat0: 0.0,
        lon0: 27f64.to_radians(),
        k0: 0.9996,
        false_e: 500_000.0,
        false_n: 0.0,
        figure: crate::geo::Ellipsoid::GRS80,
    };
    for (crs, lon, lat, gamma) in [
        (polar, 10.0, 70.0, 55.0),
        (tm35fin, 30.0, 65.0, 2.719_368_63),
    ] {
        let GridAxes::Rotated(Some(convergence)) = GridAxes::of(&crs) else {
            panic!("{crs:?} is conformal");
        };
        let at = convergence.degrees(lon, lat);
        let north = from_grid_direction(Some(0.0), Some(-5.0), at).unwrap();
        let south = from_grid_direction(Some(0.0), Some(5.0), at).unwrap();
        assert!((north - gamma).abs() < 1e-6, "{crs:?}: {north}");
        assert!((south - 180.0 - gamma).abs() < 1e-6, "{crs:?}: {south}");
    }
}

fn grib(
    name: &str,
    triple: Option<(u8, u8, u8)>,
    frame: VectorFrame,
    level: &str,
) -> ParameterFacts {
    ParameterFacts {
        grib: triple,
        frame,
        level: Some(level.to_string()),
        level_label: Some("10 m above ground".into()),
        unit: "m s-1".into(),
        ..ParameterFacts::new(name)
    }
}

fn facts(grid: GridAxes, parameters: Vec<ParameterFacts>) -> WindFacts {
    WindFacts { grid, parameters }
}

const U: Option<(u8, u8, u8)> = Some((0, 2, 2));
const V: Option<(u8, u8, u8)> = Some((0, 2, 3));

fn names(plan: &WindPlan) -> Vec<&str> {
    plan.derived.iter().map(|d| d.name.as_str()).collect()
}

#[test]
fn ecmwf_10u_10v_give_10si_and_10wdir() {
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![
            grib("2t", Some((0, 0, 0)), VectorFrame::Earth, "sfc"),
            grib("10u", U, VectorFrame::Earth, "sfc"),
            grib("10v", V, VectorFrame::Earth, "sfc"),
        ],
    ));
    assert_eq!(names(&plan), ["10si", "10wdir"]);
    let speed = plan.map_parameter("10si").unwrap();
    assert_eq!((speed.u.as_str(), speed.v.as_str()), ("10u", "10v"));
    assert_eq!(speed.title, "Wind speed (10 m above ground)");
    assert_eq!(speed.unit, "m s-1");
    assert_eq!(speed.kind.standard_name(), "wind_speed");
    let direction = plan.get("10wdir").unwrap();
    assert_eq!(direction.unit, "°");
    assert_eq!(direction.title, "Wind direction (10 m above ground)");
    assert_eq!(direction.kind.standard_name(), "wind_from_direction");
    // Direction is not a map parameter; EDR names match case-insensitively.
    assert!(plan.map_parameter("10wdir").is_none());
    assert_eq!(plan.edr_parameter("10SI").unwrap().name, "10si");
    assert_eq!(
        plan.outcomes[0].to_string(),
        "10u/10v: derived speed '10si' and direction '10wdir'"
    );
}

#[test]
fn a_collection_with_a_speed_gets_only_the_direction() {
    for existing in [
        // By vocabulary name, before its metadata is known…
        ParameterFacts {
            level: Some("sfc".into()),
            ..ParameterFacts::new("10si")
        },
        // …by GRIB 0/2/1 under another name at the same level…
        grib("si", Some((0, 2, 1)), VectorFrame::Unknown, "sfc"),
        // …or by CF standard name.
        ParameterFacts {
            standard_name: Some("wind_speed".into()),
            level: Some("sfc".into()),
            ..ParameterFacts::new("speed")
        },
    ] {
        let plan = WindPlan::build(&facts(
            GridAxes::NorthAligned,
            vec![
                grib("10u", U, VectorFrame::Earth, "sfc"),
                grib("10v", V, VectorFrame::Earth, "sfc"),
                existing.clone(),
            ],
        ));
        assert_eq!(names(&plan), ["10wdir"], "{existing:?}");
        assert_eq!(
            plan.outcomes[0].speed,
            Err(format!("the collection already has '{}'", existing.name))
        );
    }
}

#[test]
fn a_speed_at_another_level_is_no_duplicate_but_its_name_is_taken() {
    // GFS: WIND at the max-wind level, UGRD/VGRD at 10 m.
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![
            grib("UGRD", U, VectorFrame::Earth, "hag:10"),
            grib("VGRD", V, VectorFrame::Earth, "hag:10"),
            grib("WIND", Some((0, 2, 1)), VectorFrame::Earth, "max_wind"),
        ],
    ));
    assert_eq!(names(&plan), ["WDIR"]);
    assert_eq!(
        plan.outcomes[0].speed,
        Err("the name 'WIND' is taken by another parameter".into())
    );
    // A speed at another level under another name does not block one here.
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![
            grib("UGRD", U, VectorFrame::Earth, "hag:10"),
            grib("VGRD", V, VectorFrame::Earth, "hag:10"),
            grib("GUST", Some((0, 2, 1)), VectorFrame::Earth, "sfc"),
        ],
    ));
    assert_eq!(names(&plan), ["WIND", "WDIR"]);
}

#[test]
fn pressure_level_components_give_ws_and_wdir() {
    let level = |name, triple| ParameterFacts {
        grib: triple,
        frame: VectorFrame::Earth,
        unit: "m s-1".into(),
        ..ParameterFacts::new(name)
    };
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![level("u", U), level("v", V), level("t", Some((0, 0, 0)))],
    ));
    assert_eq!(names(&plan), ["ws", "wdir"]);
    assert_eq!(plan.get("ws").unwrap().title, "Wind speed");
}

#[test]
fn unknown_and_grid_relative_frames_follow_the_rules() {
    let pair = |frame_u, frame_v, grid| {
        WindPlan::build(&facts(
            grid,
            vec![
                grib("10u", U, frame_u, "sfc"),
                grib("10v", V, frame_v, "sfc"),
            ],
        ))
    };
    use GridAxes::*;
    use VectorFrame::*;
    // Unknown frame (not yet read, or not stated): speed only.
    let plan = pair(Unknown, Unknown, NorthAligned);
    assert_eq!(names(&plan), ["10si"]);
    assert!(plan.outcomes[0].to_string().starts_with(
        "10u/10v: derived speed '10si' only; no direction: whether u/v are earth- or grid-relative is unknown"
    ));
    // Grid-relative on a regular lat/lon grid: both (zero convergence).
    let plan = pair(Grid, Grid, NorthAligned);
    assert_eq!(names(&plan), ["10si", "10wdir"]);
    assert_eq!(plan.get("10wdir").unwrap().convergence, None);
    // Grid-relative on a rotated grid: both, the direction turned by the
    // grid's convergence; speed only when the grid's is unknown.
    let plan = pair(Grid, Grid, lambert());
    assert_eq!(names(&plan), ["10si", "10wdir"]);
    assert_eq!(
        plan.get("10wdir").unwrap().convergence,
        Convergence::of(&meps_lcc())
    );
    assert_eq!(plan.get("10si").unwrap().convergence, None);
    let plan = pair(Grid, Grid, Rotated(None));
    assert_eq!(names(&plan), ["10si"]);
    assert_eq!(
        plan.outcomes[0].to_string(),
        "10u/10v: derived speed '10si' only; no direction: u/v are grid-relative on a \
         rotated grid whose convergence angle is unknown (its projection parameters give none)"
    );
    // Earth-relative components are never turned, whatever the grid.
    let plan = pair(Earth, Earth, lambert());
    assert_eq!(plan.get("10wdir").unwrap().convergence, None);
    // An unknown frame on a rotated grid: speed only.
    assert_eq!(names(&pair(Unknown, Unknown, lambert())), ["10si"]);
    // Non-conformal grid, or mixed frames: nothing.
    for plan in [pair(Grid, Grid, Skewed), pair(Earth, Grid, NorthAligned)] {
        assert!(plan.derived.is_empty());
        assert!(plan.outcomes[0].to_string().contains("nothing derived"));
    }
}

#[test]
fn a_pair_needs_one_level_one_unit_and_component_metadata() {
    let mut v = grib("10v", V, VectorFrame::Earth, "hag:10");
    let u = grib("10u", U, VectorFrame::Earth, "sfc");
    let plan = WindPlan::build(&facts(GridAxes::NorthAligned, vec![u.clone(), v.clone()]));
    assert!(plan.derived.is_empty());
    assert_eq!(
        plan.outcomes[0].to_string(),
        "10u/10v: nothing derived: u and v are at different levels"
    );
    v.level = u.level.clone();
    v.unit = "kt".into();
    let plan = WindPlan::build(&facts(GridAxes::NorthAligned, vec![u.clone(), v.clone()]));
    assert!(plan.derived.is_empty());
    v.unit = u.unit.clone();
    // A vocabulary name whose GRIB triple says it is something else.
    v.grib = Some((0, 0, 0));
    let plan = WindPlan::build(&facts(GridAxes::NorthAligned, vec![u, v]));
    assert!(plan.derived.is_empty());
    assert!(plan.outcomes[0].to_string().contains("does not identify"));
    // A lone component is no pair and logs nothing.
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![grib("10u", U, VectorFrame::Earth, "sfc")],
    ));
    assert_eq!(plan, WindPlan::default());
}

/// Two spellings of one unit are one unit (`crate::units::same_unit`): a
/// store whose u says `m s-1` and v `m/s` still pairs, and the speed takes
/// u's spelling.
#[test]
fn spellings_of_one_unit_pair() {
    let u = grib("10u", U, VectorFrame::Earth, "sfc");
    let mut v = grib("10v", V, VectorFrame::Earth, "sfc");
    for spelling in ["m/s", "m s**-1", "m.s-1"] {
        v.unit = spelling.into();
        let plan = WindPlan::build(&facts(GridAxes::NorthAligned, vec![u.clone(), v.clone()]));
        assert_eq!(names(&plan), ["10si", "10wdir"], "{spelling}");
        assert_eq!(plan.map_parameter("10si").unwrap().unit, "m s-1");
    }
}

fn cf(name: &str, standard_name: Option<&str>) -> ParameterFacts {
    ParameterFacts {
        frame: standard_name.map_or(VectorFrame::Unknown, VectorFrame::from_standard_name),
        standard_name: standard_name.map(String::from),
        unit: "m s-1".into(),
        ..ParameterFacts::new(name)
    }
}

/// Zarr: CF-identified pairs named in the store's own vocabulary.
#[test]
fn cf_pairs_are_named_in_the_store_vocabulary() {
    for (u, v, sn_u, sn_v, speed, direction) in [
        (
            "wind_u_10m",
            "wind_v_10m",
            "eastward_wind",
            "northward_wind",
            "wind_speed_10m",
            "wind_direction_10m",
        ),
        (
            "10m_u_component_of_wind",
            "10m_v_component_of_wind",
            "eastward_wind",
            "northward_wind",
            "10m_wind_speed",
            "10m_wind_direction",
        ),
        (
            "x_wind_10m",
            "y_wind_10m",
            "x_wind",
            "y_wind",
            "wind_speed_10m",
            "wind_direction_10m",
        ),
        (
            "eastward_wind",
            "northward_wind",
            "eastward_wind",
            "northward_wind",
            "wind_speed",
            "wind_direction",
        ),
        // ECMWF netCDF names are a vocabulary of their own.
        (
            "u10",
            "v10",
            "eastward_wind",
            "northward_wind",
            "si10",
            "wdir10",
        ),
    ] {
        let plan = WindPlan::build(&facts(
            GridAxes::NorthAligned,
            vec![cf(u, Some(sn_u)), cf(v, Some(sn_v))],
        ));
        assert_eq!(names(&plan), [speed, direction], "{u}");
    }
    // x/y components on MEPS's Lambert grid: speed, and the direction
    // turned to true north at each value's position.
    let plan = WindPlan::build(&facts(
        lambert(),
        vec![
            cf("x_wind_10m", Some("x_wind")),
            cf("y_wind_10m", Some("y_wind")),
        ],
    ));
    assert_eq!(names(&plan), ["wind_speed_10m", "wind_direction_10m"]);
    let direction = plan.get("wind_direction_10m").unwrap();
    // From grid north at 25°E and 15°E: γ ≈ 8.934° and 0°.
    let at = |lon| direction.value(Some(0.0), Some(-3.0), lon, 60.0).unwrap();
    assert!((at(25.0) - 10.0 * 63.3f64.to_radians().sin()).abs() < 1e-9);
    assert!(at(15.0).abs() < 1e-9);
    let speed = plan.get("wind_speed_10m").unwrap();
    assert_eq!(speed.value(Some(3.0), Some(-4.0), 25.0, 60.0), Some(5.0));
    // Without a standard name a store's own names pair nothing.
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![cf("wind_u_10m", None), cf("wind_v_10m", None)],
    ));
    assert!(plan.outcomes.is_empty());
    // A phrase must be whole tokens: `max_wind_u` holds no `x_wind`.
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![
            cf("max_wind_u", Some("eastward_wind")),
            cf("max_wind_v", Some("northward_wind")),
        ],
    ));
    assert_eq!(names(&plan), ["max_wind_speed", "max_wind_direction"]);
}

/// QueryData: FMI parameter numbers, an EDR name of its own, no frame.
#[test]
fn fmi_components_keep_their_edr_names() {
    let qd = |name: &str, id| ParameterFacts {
        edr_name: Some(format!("{name} (full)")),
        fmi_param: Some(id),
        ..ParameterFacts::new(name)
    };
    let plan = WindPlan::build(&facts(
        lambert(),
        vec![qd("WindUMS", 23), qd("WindVMS", 24)],
    ));
    // No frame stated: speed only, on a rotated grid as on any.
    assert_eq!(names(&plan), ["WindSpeedMS"]);
    let speed = &plan.derived[0];
    assert_eq!(speed.u, "WindUMS");
    assert_eq!(speed.edr_u, "WindUMS (full)");
    assert_eq!(speed.edr_v, "WindVMS (full)");
    // Paired by parameter number whatever the producer called them.
    let plan = WindPlan::build(&facts(
        GridAxes::NorthAligned,
        vec![qd("U wind", 23), qd("V wind", 24), qd("Temperature", 4)],
    ));
    assert_eq!(names(&plan), ["WindSpeedMS"]);
    assert_eq!(plan.derived[0].edr_v, "V wind (full)");
    // A native WindSpeedMS (id 21) blocks the duplicate.
    let plan = WindPlan::build(&facts(
        lambert(),
        vec![qd("WindUMS", 23), qd("WindVMS", 24), qd("WindSpeedMS", 21)],
    ));
    assert!(plan.derived.is_empty());
    // Grid-relative, as the QueryData engine states them: the direction
    // too, unless the collection has one (FMI 20).
    let grid = |name: &str, id| ParameterFacts {
        frame: VectorFrame::Grid,
        ..qd(name, id)
    };
    let plan = WindPlan::build(&facts(
        lambert(),
        vec![grid("WindUMS", 23), grid("WindVMS", 24)],
    ));
    assert_eq!(names(&plan), ["WindSpeedMS", "WindDirection"]);
    let plan = WindPlan::build(&facts(
        lambert(),
        vec![
            grid("WindUMS", 23),
            grid("WindVMS", 24),
            qd("WindDirection", 20),
        ],
    ));
    assert_eq!(names(&plan), ["WindSpeedMS"]);
}
