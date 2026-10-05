//! Trajectory queries (#926): values along a path, each forecast field a
//! path crosses fetched and decoded once.
//!
//! The path, time and level rules and the `Trajectory` coverages come from
//! `ds_core::trajectory`; this module selects the run, steps, levels and
//! parameters exactly like a position query, and samples each planned
//! field (a step × level of one parameter) at all its points with the
//! position query's bilinear interpolation and display conversion.

use super::*;
use crate::runtime::run_field_jobs;
use ds_core::trajectory::{GridSpacing, TrajectoryAxes, TrajectoryPath, TrajectoryPlan};
use ds_core::vertical::VerticalDimension;

/// Densification spacing before the representative geometry probe has
/// published the grid (it normally has by the first query).
const FALLBACK_SPACING_DEG: f64 = 0.1;

impl GribEngine {
    pub(crate) fn query_batched_trajectory(
        &self,
        coords: &str,
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        ds_core::deadline::check()?;
        let path = TrajectoryPath::parse(coords)?;
        let catalog = self.catalog();
        // An M path's samples snap to the run's nearest step, so its run
        // only has to cover the path's first time, as a map `TIME` does; a
        // 2-D or Z path selects by `datetime`, like position.
        let run = match path.time_window() {
            Some((first, _)) => covering_run(&catalog, reference_time, Some(first))?,
            None => resolve_run(&catalog, reference_time, datetime)?,
        };
        let keys = catalog
            .parameter_keys(&run.reference_time)
            .cloned()
            .unwrap_or_default();
        // A 2-D or Z path takes the position query's steps; an M path snaps
        // to any step of the run.
        let steps: Vec<(DateTime<Utc>, &StepFile)> = run
            .steps
            .iter()
            .filter_map(|(&step, file)| {
                let time = run.reference_time + chrono::Duration::hours(i64::from(step));
                (path.has_m || datetime.is_none_or(|(start, end)| time >= start && time <= end))
                    .then_some((time, file))
            })
            .collect();
        let params = self.position_parameters(parameters, &steps, &keys)?;
        let vertical = self.vertical_kind().map(|kind| {
            VerticalDimension::new(
                kind,
                catalog
                    .levels
                    .get(&run.reference_time)
                    .cloned()
                    .unwrap_or_default(),
            )
        });
        // Exact levels, like position: the run's levels `z` names, a 400
        // when it names none.
        let z_levels: Option<Vec<f64>> = z
            .map(|z| {
                self.selected_levels(&catalog, run.reference_time, Some(z))
                    .map(|levels| levels.into_iter().flatten().collect())
            })
            .transpose()?;
        let info = self.raster_info_shared();
        let spacing = info
            .spatial_extent
            .zip(info.grid_size)
            .and_then(|(extent, [nx, ny])| {
                GridSpacing::from_extent(extent, [nx as usize, ny as usize])
            })
            .or_else(|| GridSpacing::new(FALLBACK_SPACING_DEG, FALLBACK_SPACING_DEG))
            .expect("the fallback spacing is positive");
        let times: Vec<DateTime<Utc>> = steps.iter().map(|(time, _)| *time).collect();
        let plan = TrajectoryPlan::new(
            &path,
            spacing,
            TrajectoryAxes {
                times: &times,
                vertical: vertical.as_ref(),
                z: z_levels.as_deref(),
            },
            params.len(),
        )?;
        plan.require_extent(info.spatial_extent)?;
        let values = self.sample_trajectory(&plan, &params, &steps, &keys)?;
        plan.into_response(&self.position_descriptions(&params, &keys), &values)
    }

    /// `values[parameter][field][point]` for every planned field. One job
    /// per (parameter, field) on the shared four-worker scheduler: the
    /// field is fetched and decoded once, sampled at every point the path
    /// reads from it, and released. A missing or unreadable field stays
    /// null, as in a position series; deadlines are fatal.
    fn sample_trajectory(
        &self,
        plan: &TrajectoryPlan,
        params: &[String],
        steps: &[(DateTime<Utc>, &StepFile)],
        keys: &ParameterKeys,
    ) -> Result<Vec<Vec<Vec<Option<f64>>>>, DataServerError> {
        let fields = plan.fields();
        let mut values: Vec<Vec<Vec<Option<f64>>>> = params
            .iter()
            .map(|_| fields.iter().map(|f| vec![None; f.points.len()]).collect())
            .collect();
        let points: Arc<Vec<(f64, f64)>> = Arc::new(plan.points().to_vec());
        let field_points: Arc<Vec<Vec<usize>>> =
            Arc::new(fields.iter().map(|f| f.points.clone()).collect());
        let mut level_keys: HashMap<Option<u64>, Arc<ParameterKeys>> = HashMap::new();
        for field in fields {
            level_keys
                .entry(field.level.map(f64::to_bits))
                .or_insert_with(|| Arc::new(Self::keys_at_level(keys, field.level)));
        }
        let jobs = params.iter().enumerate().flat_map(|(p, name)| {
            let level_keys = &level_keys;
            fields.iter().enumerate().filter_map(move |(f, field)| {
                let (_, file) = steps[field.time];
                let keys = level_keys[&field.level.map(f64::to_bits)].clone();
                let entry = keys
                    .get(name)
                    .and_then(|key| file.messages.iter().find(|m| key.matches(m)))?;
                Some((
                    p,
                    f,
                    name.clone(),
                    file.message_url(entry).to_owned(),
                    entry.clone(),
                    keys,
                ))
            })
        });
        let engine = Arc::new(Self {
            collection_id: self.collection_id.clone(),
            family: self.family,
            source: self.source.clone(),
        });
        run_field_jobs(
            jobs,
            move |(p, f, name, url, entry, keys)| {
                let samples = engine.fetch_grid_by_entry(&url, &entry).map(|grid| {
                    let meta = engine.param_metadata_for(&keys, &name);
                    field_points[f]
                        .iter()
                        .map(|&i| {
                            let (lon, lat) = points[i];
                            grid.bilinear_value(lon, lat)
                                .map(|v| meta.display.convert(v))
                        })
                        .collect::<Vec<_>>()
                });
                Ok((p, f, samples))
            },
            |(p, f, samples)| {
                match samples {
                    Ok(samples) => values[p][f] = samples,
                    Err(error) => tracing::debug!(
                        parameter = %params[p],
                        error = %error,
                        "GRIB trajectory field unavailable"
                    ),
                }
                Ok(())
            },
        )?;
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{message, TestSource};
    use chrono::TimeZone;

    /// `message(…, base, [nw, ne, sw, se], …)` is a 2 × 2 grid over
    /// lon 0..1, lat 0..1 whose display value (°C) at (lon, lat) is
    /// `base − 273.15 + 2·lon + 4·(1 − lat)` for these corner offsets.
    fn source(steps: u32, levels: &[(&str, u8, u32, f32)]) -> TestSource {
        let source = TestSource::new();
        for step in 0..steps {
            let messages: Vec<_> = levels
                .iter()
                .map(|&(level, surface, encoded, base)| {
                    (
                        "TMP",
                        level,
                        message(0, base + step as f32, [0, 2, 4, 6], surface, encoded),
                    )
                })
                .collect();
            source.write(&format!("f{step:03}"), &messages, step);
        }
        source
    }

    fn engine(source: &TestSource, family: Option<GribLevelType>) -> GribEngine {
        let mut config = source.config();
        config.grid_cache_mb = 0; // one read per field without cache help
        config.parameters = Some(vec!["TMP".into()]);
        config.level_types = family.map(|f| vec![f]);
        GribEngine::new("trajectory", &config).unwrap()
    }

    fn expected(base: f64, step: f64, lon: f64, lat: f64) -> f64 {
        base - 273.15 + step + 2.0 * lon + 4.0 * (1.0 - lat)
    }

    #[test]
    fn a_2d_path_reads_each_field_once_for_every_sample() {
        let source = source(3, &[("2 m above ground", 103, 2, 280.0)]);
        let engine = engine(&source, None);
        assert_eq!(
            engine.supported_query_types(),
            ["position", "area", "radius", "trajectory"]
        );
        let before = engine.storage_bytes_read();
        let response = engine
            .query_trajectory("LINESTRING(0.1 0.2, 0.9 0.8)", None, None, None, None)
            .unwrap();
        let bytes = message(0, 280.0, [0, 2, 4, 6], 103, 2).len() as u64;
        assert_eq!(
            engine.storage_bytes_read() - before,
            3 * bytes,
            "one read per step, not per sample"
        );
        let CoverageResponse::Collection(coverages) = response else {
            panic!("three steps → three coverages")
        };
        assert_eq!(coverages.len(), 3);
        for (step, coverage) in coverages.iter().enumerate() {
            let DomainDescription::Trajectory { nodes, node_z, z } = &coverage.domain else {
                panic!("expected a Trajectory domain")
            };
            assert!(node_z.is_none() && z.is_none());
            assert!(nodes.len() >= 2, "{nodes:?}");
            assert!(nodes.iter().all(|n| n.0 == nodes[0].0));
            let range = &coverage.ranges["TMP"];
            assert_eq!(range.axis_names, ["composite"]);
            assert_eq!(range.shape, [nodes.len()]);
            for (&(_, lon, lat), value) in nodes.iter().zip(&range.values) {
                let want = expected(280.0, step as f64, lon, lat);
                assert!((value.unwrap() - want).abs() < 1e-6, "{value:?} vs {want}");
            }
            assert_eq!(coverage.parameters["TMP"].unit, "°C");
        }
    }

    #[test]
    fn an_m_path_snaps_to_the_nearest_step_of_the_run() {
        let source = source(3, &[("2 m above ground", 103, 2, 280.0)]);
        let engine = engine(&source, None);
        let run = engine.catalog().latest_run().unwrap().reference_time;
        let at = |hours: f64| run.timestamp() as f64 + hours * 3600.0;
        // The 1° test grid keeps just these vertices as samples: 0, 0.5, 1,
        // 1.5 and 2 h snap to steps 0, 0, 1, 1, 2 (ties go to the earlier).
        let coords = format!(
            "LINESTRING M(0 0.5 {}, 0.25 0.5 {}, 0.5 0.5 {}, 0.75 0.5 {}, 1 0.5 {})",
            at(0.0),
            at(0.5),
            at(1.0),
            at(1.5),
            at(2.0)
        );
        let response = engine
            .query_trajectory(&coords, None, None, None, None)
            .unwrap();
        let CoverageResponse::Single(coverage) = response else {
            panic!("an M path is one coverage")
        };
        let DomainDescription::Trajectory { nodes, .. } = &coverage.domain else {
            panic!()
        };
        let range = &coverage.ranges["TMP"];
        for (&(t, lon, lat), value) in nodes.iter().zip(&range.values) {
            let step = (t - run).num_hours() as f64;
            let want = expected(280.0, step, lon, lat);
            assert!(
                (value.unwrap() - want).abs() < 1e-6,
                "{t}: {value:?} vs {want}"
            );
        }
        let steps: Vec<i64> = nodes.iter().map(|n| (n.0 - run).num_hours()).collect();
        assert_eq!(steps, [0, 0, 1, 1, 2]);

        // Past the run's last step → 400 (EDR coords-param-invalid-time).
        let late = format!("LINESTRING M(0 0.5 {}, 1 0.5 {})", at(0.0), at(5.0));
        assert!(matches!(
            engine.query_trajectory(&late, None, None, None, None),
            Err(DataServerError::InvalidParameter(_))
        ));
    }

    #[test]
    fn a_z_path_follows_the_pressure_levels() {
        let source = source(
            1,
            &[
                ("850 mb", 100, 85000, 280.0),
                ("700 mb", 100, 70000, 270.0),
                ("500 mb", 100, 50000, 250.0),
            ],
        );
        let owner = engine(&source, Some(GribLevelType::Pressure));
        let views = owner.level_collections();
        let view = &views[0];
        let response = view
            .query_trajectory(
                "LINESTRING Z(0 0.5 850, 0.5 0.5 690, 1 0.5 500)",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let CoverageResponse::Single(coverage) = response else {
            panic!("one step, per-sample levels → one coverage")
        };
        let DomainDescription::Trajectory { nodes, node_z, z } = &coverage.domain else {
            panic!()
        };
        assert!(z.is_none());
        let levels = &node_z.as_ref().expect("levels ride in the tuples").values;
        // 690 hPa snaps to the nearest advertised level.
        assert_eq!(levels, &[850.0, 700.0, 500.0]);
        let base = |level: f64| match level as u32 {
            850 => 280.0,
            700 => 270.0,
            500 => 250.0,
            other => panic!("unexpected level {other}"),
        };
        for ((&(_, lon, lat), &level), value) in
            nodes.iter().zip(levels).zip(&coverage.ranges["TMP"].values)
        {
            let want = expected(base(level), 0.0, lon, lat);
            assert!(
                (value.unwrap() - want).abs() < 1e-6,
                "{level}: {value:?} vs {want}"
            );
        }

        // A level outside 500–850 hPa is a 400; so is an unavailable `z`.
        assert!(matches!(
            view.query_trajectory("LINESTRING Z(0 0.5 850, 1 0.5 300)", None, None, None, None),
            Err(DataServerError::InvalidParameter(_))
        ));
        assert!(matches!(
            view.query_trajectory("LINESTRING(0 0.5, 1 0.5)", None, None, Some(&[600.0]), None),
            Err(DataServerError::InvalidParameter(_))
        ));
        // A 2-D path on the pressure view: one coverage per level.
        let CoverageResponse::Collection(per_level) = view
            .query_trajectory("LINESTRING(0 0.5, 1 0.5)", None, None, None, None)
            .unwrap()
        else {
            panic!("three levels → three coverages")
        };
        assert_eq!(per_level.len(), 3);
        for coverage in &per_level {
            let DomainDescription::Trajectory { z, .. } = &coverage.domain else {
                panic!()
            };
            assert_eq!(z.as_ref().unwrap().values.len(), 1);
        }
    }

    #[test]
    fn datetime_selects_steps_like_position() {
        let source = source(3, &[("2 m above ground", 103, 2, 280.0)]);
        let engine = engine(&source, None);
        let run = engine.catalog().latest_run().unwrap().reference_time;
        let t = run + chrono::Duration::hours(1);
        let CoverageResponse::Single(coverage) = engine
            .query_trajectory("LINESTRING(0 0, 1 1)", Some((t, t)), None, None, None)
            .unwrap()
        else {
            panic!("one step → one coverage")
        };
        let DomainDescription::Trajectory { nodes, .. } = &coverage.domain else {
            panic!()
        };
        assert!(nodes.iter().all(|n| n.0 == t));
        // A window with no step is the position query's 404.
        let empty = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        assert!(matches!(
            engine.query_trajectory(
                "LINESTRING(0 0, 1 1)",
                Some((empty, empty)),
                None,
                None,
                None
            ),
            Err(DataServerError::LocationNotFound(_))
        ));
    }
}
