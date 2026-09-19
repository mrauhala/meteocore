//! Sample every requested coordinate while each forecast field is resident.

use std::sync::LazyLock;

use tokio::task::JoinSet;

use super::*;

// Per admitted EDR query: bound simultaneous fetches, decodes and live grids.
const POSITION_CONCURRENCY: usize = 4;

// Direct synchronous engine callers (tests/CLI) also share one I/O runtime,
// rather than constructing one for every forecast step. HTTP calls use the
// existing dedicated EDR runtime, never the request-serving runtime.
static POSITION_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(POSITION_CONCURRENCY)
        .thread_name("grib-position")
        .enable_all()
        .build()
        .expect("GRIB position runtime")
});

fn run_fetches<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => POSITION_RUNTIME.block_on(future),
    }
}

impl GribEngine {
    pub(crate) fn query_batched_positions(
        &self,
        points: &[String],
        datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
        parameters: Option<&[String]>,
        z: Option<&[f64]>,
        reference_time: Option<DateTime<Utc>>,
        emit: &mut dyn FnMut(CoverageResponse) -> Result<(), DataServerError>,
    ) -> Result<(), DataServerError> {
        ds_core::deadline::check()?;
        let coords: Arc<Vec<_>> = Arc::new(
            points
                .iter()
                .map(|p| parse_coords(p))
                .collect::<Result<_, _>>()?,
        );
        if coords.is_empty() {
            return Err(DataServerError::InvalidParameter(
                "No positions supplied".into(),
            ));
        }
        let catalog = self.catalog();
        let run = resolve_run(&catalog, reference_time, datetime)?;
        let levels = self.selected_levels(&catalog, run.reference_time, z)?;
        let keys = catalog
            .parameter_keys(&run.reference_time)
            .cloned()
            .unwrap_or_default();
        let steps: Vec<_> = run
            .steps
            .iter()
            .filter_map(|(&step, file)| {
                let time = run.reference_time + chrono::Duration::hours(i64::from(step));
                datetime
                    .is_none_or(|(start, end)| time >= start && time <= end)
                    .then_some((time, file))
            })
            .collect();
        let params: Vec<String> = match parameters {
            Some(params) => params.to_vec(),
            None if self.vertical_kind().is_some() => keys.keys().cloned().collect(),
            None => {
                let mut seen = HashSet::new();
                steps
                    .iter()
                    .flat_map(|(_, file)| &file.messages)
                    .filter(|m| self.family == Some(GribLevelType::Single) || m.is_near_surface())
                    .filter(|m| seen.insert(m.param.clone()))
                    .map(|m| m.param.clone())
                    .collect()
            }
        };
        if self.family.is_some() {
            self.validate_parameters(&keys, &params)?;
        }
        if steps.is_empty() || (params.is_empty() && self.vertical_kind().is_some()) {
            return Err(DataServerError::InvalidParameter(
                "No forecast data matches the query".into(),
            ));
        }
        // Reject the complete batch before allocating samples or fetching any
        // field. The API also enforces the budget for other engines via emit.
        ds_core::feature::check_area_budget(steps.len(), levels.len(), coords.len(), params.len())?;
        let level_keys: Vec<_> = levels
            .iter()
            .map(|&z| Arc::new(Self::keys_at_level(&keys, z)))
            .collect();
        let worker = Arc::new(Self {
            collection_id: self.collection_id.clone(),
            family: self.family,
            source: self.source.clone(),
        });
        let mut values: Vec<HashMap<String, Vec<Option<f64>>>> = vec![HashMap::new(); coords.len()];
        for name in &params {
            let samples = worker.sample_parameter(name, &steps, &level_keys, &coords)?;
            for (point, samples) in values.iter_mut().zip(samples) {
                point.insert(name.clone(), samples);
            }
        }
        let descriptions: HashMap<_, _> = params
            .iter()
            .map(|name| {
                let meta = self.param_metadata_for(&keys, name);
                (
                    name.clone(),
                    ParameterDescription {
                        label: meta.label(),
                        unit: meta.display.display_unit.into(),
                        observed_property: name.clone(),
                    },
                )
            })
            .collect();
        let vertical = self.vertical_kind().map(|kind| VerticalCoord {
            kind,
            values: levels.iter().flatten().copied().collect(),
        });
        for (&(lon, lat), values) in coords.iter().zip(values) {
            ds_core::deadline::check()?;
            let response = if levels.len() == 1 {
                CoverageResponse::Single(QueryResult {
                    domain: DomainDescription::PointSeries {
                        x: lon,
                        y: lat,
                        t: steps.iter().map(|(t, _)| *t).collect(),
                        z: vertical.clone(),
                    },
                    parameters: descriptions.clone(),
                    ranges: values
                        .into_iter()
                        .map(|(name, values)| {
                            (
                                name,
                                NdArray {
                                    shape: vec![steps.len()],
                                    axis_names: vec!["t".into()],
                                    values,
                                },
                            )
                        })
                        .collect(),
                })
            } else {
                let nz = levels.len();
                CoverageResponse::Collection(
                    steps
                        .iter()
                        .enumerate()
                        .map(|(i, (time, _))| QueryResult {
                            domain: DomainDescription::VerticalProfile {
                                x: lon,
                                y: lat,
                                t: Some(*time),
                                z: vertical.clone().expect("vertical collection"),
                            },
                            parameters: descriptions.clone(),
                            ranges: values
                                .iter()
                                .map(|(name, values)| {
                                    (
                                        name.clone(),
                                        NdArray {
                                            shape: vec![nz],
                                            axis_names: vec!["z".into()],
                                            values: values[i * nz..(i + 1) * nz].to_vec(),
                                        },
                                    )
                                })
                                .collect(),
                        })
                        .collect(),
                )
            };
            emit(response)?;
        }
        Ok(())
    }

    /// Results are small point samples, not retained global grids. Completion
    /// order cannot reorder the time/level axes, and cache size does not affect
    /// reuse across coordinates within this request.
    fn sample_parameter(
        self: &Arc<Self>,
        name: &str,
        steps: &[(DateTime<Utc>, &StepFile)],
        level_keys: &[Arc<ParameterKeys>],
        coords: &Arc<Vec<(f64, f64)>>,
    ) -> Result<Vec<Vec<Option<f64>>>, DataServerError> {
        let deadline = ds_core::deadline::current();
        let mut samples = vec![vec![None; steps.len() * level_keys.len()]; coords.len()];
        let mut fields = steps
            .iter()
            .flat_map(|(_, file)| level_keys.iter().map(move |keys| (*file, keys)))
            .enumerate();
        run_fetches(async {
            let mut jobs = JoinSet::new();
            let mut exhausted = false;
            let mut fatal = None;
            loop {
                if let Err(error) = ds_core::deadline::check() {
                    fatal = Some(error);
                }
                while jobs.len() < POSITION_CONCURRENCY && !exhausted && fatal.is_none() {
                    let Some((index, (file, keys))) = fields.next() else {
                        exhausted = true;
                        break;
                    };
                    let Some(entry) = keys
                        .get(name)
                        .and_then(|key| file.messages.iter().find(|m| key.matches(m)))
                    else {
                        continue; // missing canonical field stays null at every point
                    };
                    let url = file.message_url(entry).to_owned();
                    let entry = entry.clone();
                    let engine = self.clone();
                    let coords = coords.clone();
                    let keys = keys.clone();
                    let name = name.to_owned();
                    jobs.spawn(async move {
                        // Cache single-flight waiters are synchronous too. Free
                        // this runtime worker during the whole operation, so
                        // waiting for another fill cannot starve its I/O driver.
                        tokio::task::block_in_place(|| {
                            let _deadline = ds_core::deadline::enter(deadline);
                            ds_core::deadline::check()?;
                            let result = engine.fetch_grid_by_entry(&url, &entry);
                            ds_core::deadline::check()?;
                            let values = result.map(|grid| {
                                let meta = engine.param_metadata_for(&keys, &name);
                                coords
                                    .iter()
                                    .map(|&(lon, lat)| {
                                        grid.bilinear_value(lon, lat)
                                            .map(|v| meta.display.convert(v))
                                    })
                                    .collect::<Vec<_>>()
                            });
                            Ok::<_, DataServerError>((index, name, values))
                        })
                    });
                }
                let Some(result) = jobs.join_next().await else {
                    break;
                };
                match result {
                    Ok(Ok((index, _, Ok(values)))) => {
                        for (point, value) in samples.iter_mut().zip(values) {
                            point[index] = value;
                        }
                    }
                    Ok(Ok((_, name, Err(error)))) => {
                        tracing::debug!(parameter = name, error = %error, "GRIB position field unavailable");
                    }
                    Ok(Err(error)) => fatal = Some(error),
                    Err(error) => {
                        fatal = Some(DataServerError::Engine(format!(
                            "GRIB position worker failed: {error}"
                        )))
                    }
                }
                // On failure/deadline, drain running workers before returning:
                // the EDR admission permit must outlive all of this query's I/O.
            }
            match fatal {
                Some(error) => Err(error),
                None => Ok(samples),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{message, TestSource};
    use ds_storage::object_store::throttle::{ThrottleConfig, ThrottledStore};
    use ds_storage::object_store::ObjectStoreExt;

    fn source_with_steps(count: u32, level: &str, surface: u8, encoded_level: u32) -> TestSource {
        let source = TestSource::new();
        for step in 0..count {
            source.write(
                &format!("f{step:03}"),
                &[
                    (
                        "TMP",
                        level,
                        message(0, 280.0 + step as f32, [0, 2, 4, 6], surface, encoded_level),
                    ),
                    // Give TMP an explicit length, as RH has in real GFS indexes.
                    ("TAIL", "surface", message(0, 200.0, [0; 4], 1, 0)),
                ],
                step,
            );
        }
        source
    }

    fn config(source: &TestSource, family: Option<GribLevelType>) -> GribConfig {
        let mut config = source.config();
        config.grid_cache_mb = 0; // prove reuse without relying on cache residency
        config.parameters = Some(vec!["TMP".into()]);
        config.level_types = family.map(|f| vec![f]);
        config
    }

    fn collect(engine: &GribEngine, points: &[String], z: Option<&[f64]>) -> Vec<CoverageResponse> {
        let mut output = Vec::new();
        engine
            .query_positions(points, None, None, z, None, &mut |response| {
                output.push(response);
                Ok(())
            })
            .unwrap();
        output
    }

    fn throttle(engine: &mut GribEngine, delay: Duration) {
        let source = Arc::get_mut(&mut engine.source).unwrap();
        // ThrottledStore supports streamed payloads, not filesystem payloads.
        // Copy the fixture to memory before measuring simulated network I/O.
        let memory = ds_storage::object_store::memory::InMemory::new();
        let catalog = source.catalog.load_full();
        run_fetches(async {
            for file in catalog.runs.values().flat_map(|run| run.steps.values()) {
                let path = ds_storage::object_store::path::Path::from(file.grib_url.as_str());
                let bytes = source
                    .store
                    .inner()
                    .get(&path)
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap();
                memory.put(&path, bytes.into()).await.unwrap();
            }
        });
        source.store = ds_storage::DataStore::new(Arc::new(ThrottledStore::new(
            memory,
            ThrottleConfig {
                wait_get_per_call: delay,
                ..Default::default()
            },
        )));
    }

    #[test]
    fn one_read_per_field_serves_all_points_with_cache_disabled() {
        let coords = [(0.1, 0.2), (0.9, 0.8), (0.5, 0.5)];
        let points: Vec<_> = coords
            .iter()
            .map(|(x, y)| format!("POINT({x} {y})"))
            .collect();
        for (family, level, surface, encoded, z) in [
            (None, "2 m above ground", 103, 2, None),
            (
                Some(GribLevelType::Single),
                "2 m above ground",
                103,
                2,
                None,
            ),
            (
                Some(GribLevelType::Pressure),
                "850 mb",
                100,
                85000,
                Some(850.0),
            ),
            (
                Some(GribLevelType::Model),
                "137 hybrid level",
                105,
                137,
                Some(137.0),
            ),
        ] {
            let source = source_with_steps(12, level, surface, encoded);
            let owner = GribEngine::new("batch", &config(&source, family)).unwrap();
            let views = owner.level_collections();
            let engine = views.first().unwrap_or(&owner);
            let before = engine.storage_bytes_read();
            let output = collect(engine, &points, z.as_ref().map(std::slice::from_ref));
            let message_bytes = message(0, 280.0, [0, 2, 4, 6], surface, encoded).len() as u64;
            assert_eq!(engine.storage_bytes_read() - before, 12 * message_bytes);
            assert_eq!(output.len(), coords.len());
            for (response, &(lon, lat)) in output.iter().zip(&coords) {
                let CoverageResponse::Single(result) = response else {
                    panic!("expected series")
                };
                let DomainDescription::PointSeries {
                    x,
                    y,
                    t,
                    z: vertical,
                } = &result.domain
                else {
                    panic!()
                };
                assert_eq!((*x, *y), (lon, lat));
                assert_eq!(t.len(), 12);
                assert!(t.windows(2).all(|p| p[0] < p[1]));
                assert_eq!(vertical.as_ref().map(|v| v.values[0]), z);
                let range = &result.ranges["TMP"];
                assert_eq!(range.shape, [12]);
                assert_eq!(range.axis_names, ["t"]);
                for (step, value) in range.values.iter().enumerate() {
                    let expected = 6.85 + step as f64 + 2.0 * lon + 4.0 * (1.0 - lat);
                    assert!((value.unwrap() - expected).abs() < 1e-9);
                }
                assert_eq!(result.parameters["TMP"].unit, "°C");
            }
        }
    }

    #[test]
    fn batch_profiles_preserve_point_time_level_order_and_missing_values() {
        let source = TestSource::new();
        source.write(
            "f000",
            &[
                ("TMP", "850 mb", message(0, 280.0, [0, 2, 4, 6], 100, 85000)),
                ("TMP", "500 mb", message(0, 250.0, [0, 2, 4, 6], 100, 50000)),
            ],
            0,
        );
        source.write(
            "f001",
            &[("TMP", "850 mb", message(0, 281.0, [0, 2, 4, 6], 100, 85000))],
            1,
        );
        let owner =
            GribEngine::new("profiles", &config(&source, Some(GribLevelType::Pressure))).unwrap();
        let views = owner.level_collections();
        let points = ["POINT(0 1)".into(), "POINT(1 0)".into()];
        let output = collect(&views[0], &points, Some(&[500.0, 850.0]));
        for (point, response) in output.iter().enumerate() {
            let CoverageResponse::Collection(profiles) = response else {
                panic!()
            };
            assert_eq!(profiles.len(), 2);
            for (step, profile) in profiles.iter().enumerate() {
                let DomainDescription::VerticalProfile { x, y, z, .. } = &profile.domain else {
                    panic!()
                };
                assert_eq!((*x, *y), (point as f64, 1.0 - point as f64));
                assert_eq!(z.values, [500.0, 850.0]);
                let range = &profile.ranges["TMP"];
                assert_eq!(range.shape, [2]);
                assert_eq!(range.axis_names, ["z"]);
                let offset = point as f64 * 6.0;
                if step == 0 {
                    assert!((range.values[0].unwrap() - (-23.15 + offset)).abs() < 1e-9);
                } else {
                    assert_eq!(range.values[0], None);
                }
                assert!((range.values[1].unwrap() - (6.85 + step as f64 + offset)).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn invalid_or_oversized_batches_do_no_field_reads() {
        let source = source_with_steps(2, "850 mb", 100, 85000);
        let owner =
            GribEngine::new("limits", &config(&source, Some(GribLevelType::Pressure))).unwrap();
        let views = owner.level_collections();
        let engine = &views[0];
        let before = engine.storage_bytes_read();
        let mut emit = |_| -> Result<(), DataServerError> { panic!("invalid batch emitted data") };
        assert!(matches!(
            engine.query_positions(
                &["POINT(0 0)".into(), "POINT(NaN 0)".into()],
                None,
                None,
                None,
                None,
                &mut emit,
            ),
            Err(DataServerError::InvalidParameter(_))
        ));
        assert!(matches!(
            engine.query_positions(
                &["POINT(0 0)".into(), "POINT(1 1)".into()],
                None,
                Some(&vec!["TMP".into(); 250001]),
                None,
                None,
                &mut emit,
            ),
            Err(DataServerError::QueryTooLarge(_))
        ));
        assert_eq!(engine.storage_bytes_read(), before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_reads_obey_deadline_and_drain_before_return() {
        let source = source_with_steps(12, "2 m above ground", 103, 2);
        let mut engine = GribEngine::new("deadline", &config(&source, None)).unwrap();
        throttle(&mut engine, Duration::from_secs(30));
        let before = engine.storage_bytes_read();
        let _deadline = ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(100)));
        let result = engine.query_position("POINT(0 0)", None, None, None, None);
        assert!(
            matches!(result, Err(DataServerError::DeadlineExceeded)),
            "{result:?}"
        );
        assert_eq!(engine.storage_bytes_read(), before);
        assert_eq!(
            Arc::strong_count(&engine.source),
            1,
            "no detached workers after deadline"
        );
    }

    #[test]
    #[ignore = "manual latency replay; prints measurements rather than asserting wall-clock thresholds"]
    fn position_latency_replay() {
        let source = source_with_steps(120, "850 mb", 100, 85000);
        let mut owner =
            GribEngine::new("replay", &config(&source, Some(GribLevelType::Pressure))).unwrap();
        throttle(&mut owner, Duration::from_millis(150));
        let views = owner.level_collections();
        let engine = &views[0];
        let points: [String; 3] = [
            "POINT(0.1 0.2)".into(),
            "POINT(0.9 0.8)".into(),
            "POINT(0.5 0.5)".into(),
        ];
        let catalog = engine.catalog();
        let run = catalog.latest_run().unwrap();
        let keys = GribEngine::keys_at_level(
            catalog.parameter_keys(&run.reference_time).unwrap(),
            Some(850.0),
        );
        for n in [1, 3] {
            let before = engine.storage_bytes_read();
            let start = Instant::now();
            let serial: Vec<Vec<_>> = POSITION_RUNTIME.block_on(async {
                points[..n]
                    .iter()
                    .map(|point| {
                        let (lon, lat) = parse_coords(point).unwrap();
                        run.steps
                            .values()
                            .map(|file| {
                                let grid = engine.fetch_grid(file, "TMP", &keys).unwrap();
                                grid.bilinear_value(lon, lat).map(|v| {
                                    engine.param_metadata_for(&keys, "TMP").display.convert(v)
                                })
                            })
                            .collect()
                    })
                    .collect()
            });
            let serial_ms = start.elapsed().as_millis();
            let serial_bytes = engine.storage_bytes_read() - before;
            let before = engine.storage_bytes_read();
            let start = Instant::now();
            let output = collect(engine, &points[..n], Some(&[850.0]));
            let batch_ms = start.elapsed().as_millis();
            for (response, expected) in output.iter().zip(serial) {
                let CoverageResponse::Single(series) = response else {
                    panic!()
                };
                assert_eq!(series.ranges["TMP"].values, expected);
            }
            eprintln!("points={n} steps=120 serial_ms={serial_ms} batch_ms={batch_ms} serial_bytes={serial_bytes} batch_bytes={}", engine.storage_bytes_read() - before);
        }
    }
}
