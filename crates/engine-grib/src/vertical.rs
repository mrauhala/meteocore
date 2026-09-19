//! Vertical selection for the pressure and model-level collection views.

use super::*;
use ds_core::vertical::{VerticalDimension, VerticalKind};

impl GribEngine {
    pub(crate) fn vertical_kind(&self) -> Option<VerticalKind> {
        match self.family {
            Some(GribLevelType::Pressure) => Some(VerticalKind::Pressure),
            Some(GribLevelType::Model) => Some(VerticalKind::ModelLevel),
            _ => None,
        }
    }

    pub(crate) fn vertical_extent(&self, catalog: &Catalog) -> Option<VerticalDimension> {
        self.vertical_kind()
            .filter(|_| !catalog.vertical_levels.is_empty())
            .map(|kind| VerticalDimension::new(kind, catalog.vertical_levels.clone()))
    }

    /// Exact discrete selection; unavailable levels never silently snap to a
    /// different surface. Missing fields at a valid level remain missing.
    pub(crate) fn selected_levels(
        &self,
        catalog: &Catalog,
        run: DateTime<Utc>,
        z: Option<&[f64]>,
    ) -> Result<Vec<Option<f64>>, DataServerError> {
        if self.vertical_kind().is_none() {
            if z.is_some() {
                return Err(DataServerError::InvalidParameter(
                    "This collection has no vertical axis".into(),
                ));
            }
            return Ok(vec![None]);
        }
        let available = catalog
            .levels
            .get(&run)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let selected = z.unwrap_or(available);
        if selected.is_empty()
            || selected
                .iter()
                .any(|v| !v.is_finite() || !available.contains(v))
        {
            return Err(DataServerError::InvalidParameter(format!(
                "Requested vertical level is unavailable in this run; available levels: {available:?}"
            )));
        }
        let mut levels = Vec::new();
        for &value in selected {
            if !levels.contains(&Some(value)) {
                levels.push(Some(value));
            }
        }
        Ok(levels)
    }

    pub(crate) fn keys_at_level(keys: &ParameterKeys, level: Option<f64>) -> ParameterKeys {
        let mut keys = keys.clone();
        if let Some(level) = level {
            for key in keys.values_mut() {
                key.level = Some(level as u32);
            }
        }
        keys
    }

    pub(crate) fn validate_parameters(
        &self,
        keys: &ParameterKeys,
        params: &[String],
    ) -> Result<(), DataServerError> {
        for name in params {
            if !keys.contains_key(name) {
                return Err(DataServerError::InvalidParameter(format!(
                    "Parameter '{name}' is unavailable in this collection and run"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{message, TestSource};

    fn split_config(source: &TestSource) -> GribConfig {
        let mut config = source.config();
        config.level_types = Some(vec![
            GribLevelType::Single,
            GribLevelType::Pressure,
            GribLevelType::Model,
        ]);
        config
    }

    #[test]
    fn metadata_fallback_preserves_type_name_and_exact_level_identity() {
        let source = TestSource::new();
        let owner = GribEngine::new("metadata", &source.config()).unwrap();
        let view = |family: GribLevelType| GribEngine {
            collection_id: format!("metadata-{}", family.suffix()),
            family: Some(family),
            source: owner.source.clone(),
        };
        let pressure = view(GribLevelType::Pressure);
        let model = view(GribLevelType::Model);
        let single = view(GribLevelType::Single);
        let key = |name: &str, levtype: &str, level| ParameterKey {
            param: name.into(),
            levtype: levtype.into(),
            level: Some(level),
        };
        let lookup = |engine: &GribEngine, key: ParameterKey| {
            engine.param_metadata_for(
                &[(key.param.clone(), key.clone())].into_iter().collect(),
                &key.param,
            )
        };
        let mut grid =
            reader::decode_message(&message(0, 280.0, [0; 4], 100, 50000), "FIELD").unwrap();
        owner.populate_metadata(
            &key("FIELD", "pl", 500),
            &grid,
            wgrib2_index::StepKind::Average { start: 0, end: 6 },
        );
        // Distinct WMO units under the same provider name must not leak
        // between the pressure and model indexes.
        grid.triple = (0, 2, 2); // u-component of wind
        grid.first_surface_type = 105;
        grid.first_surface_value = Some(1.0);
        owner.populate_metadata(
            &key("FIELD", "ml", 1),
            &grid,
            wgrib2_index::StepKind::Instant,
        );

        let fallback = lookup(&pressure, key("FIELD", "pl", 850));
        assert_eq!(fallback.source_unit, SourceUnit::Kelvin);
        assert!((fallback.display.convert(280.0) - 6.85).abs() < 1e-9);
        assert!(fallback.label().contains("6 h average"));
        assert_eq!(fallback.first_surface_type, None);
        assert_eq!(fallback.first_surface_value, None);
        let model_meta = lookup(&model, key("FIELD", "ml", 137));
        assert_eq!(model_meta.source_unit, SourceUnit::MetresPerSecond);
        assert_eq!(model_meta.window_qualifier, None);
        for missing in [key("OTHER", "pl", 500), key("FIELD", "sol", 500)] {
            let meta = lookup(&pressure, missing.clone());
            assert_eq!(meta.label(), missing.param);
            assert_eq!(meta.display.display_unit, "");
        }

        // Once the exact level is decoded its own metadata takes precedence,
        // without changing the representative used for other unprobed levels.
        grid.triple = (0, 3, 0); // pressure
        grid.first_surface_type = 100;
        grid.first_surface_value = Some(85000.0);
        owner.populate_metadata(
            &key("FIELD", "pl", 850),
            &grid,
            wgrib2_index::StepKind::Instant,
        );
        assert_eq!(
            lookup(&pressure, key("FIELD", "pl", 850)).source_unit,
            SourceUnit::Pascal
        );
        assert_eq!(
            lookup(&pressure, key("FIELD", "pl", 700)).source_unit,
            SourceUnit::Kelvin
        );

        grid.triple = (0, 0, 0);
        grid.first_surface_type = 103;
        grid.first_surface_value = Some(2.0);
        owner.populate_metadata(
            &key("FIELD", "hag", 2),
            &grid,
            wgrib2_index::StepKind::Instant,
        );
        for engine in [&single, &owner] {
            assert!(lookup(engine, key("FIELD", "hag", 2))
                .label()
                .contains("2 m above ground"));
            assert_eq!(
                lookup(engine, key("FIELD", "hag", 10)).display.display_unit,
                ""
            );
        }
        assert_eq!(
            owner.storage_bytes_read(),
            0,
            "metadata lookup must not fetch data"
        );
    }

    #[test]
    fn render_version_follows_late_default_levels_without_churning_other_families() {
        for (family, surface, first, first_encoded, later, later_encoded) in [
            (
                GribLevelType::Pressure,
                100,
                "500 mb",
                50000,
                "850 mb",
                85000,
            ),
            (
                GribLevelType::Model,
                105,
                "1 hybrid level",
                1,
                "137 hybrid level",
                137,
            ),
        ] {
            let source = TestSource::new();
            source.write(
                "single",
                &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
                0,
            );
            source.write(
                "first",
                &[(
                    "TMP",
                    first,
                    message(0, 250.0, [0; 4], surface, first_encoded),
                )],
                0,
            );
            let engine = GribEngine::new("forecast", &split_config(&source)).unwrap();
            let views = engine.level_collections();
            let view = views
                .iter()
                .find(|view| view.family == Some(family))
                .unwrap();
            let single = views
                .iter()
                .find(|view| view.family == Some(GribLevelType::Single))
                .unwrap();
            let reference = "2026-04-05T00:00:00Z".parse().unwrap();
            let render = |z| {
                view.get_raster_tile(
                    [0.0, 0.0, 1.0, 1.0],
                    1,
                    1,
                    Some(reference),
                    &OutputCrs::Wgs84,
                    Some("TMP"),
                    z,
                    Some(reference),
                )
                .unwrap()
                .values
                .iter_values()
                .next()
                .unwrap()
                .unwrap()
            };
            let version = view.content_version();
            let single_version = single.content_version();
            assert_ne!(version, 0, "explicit-TIME responses must revalidate");
            assert!((render(None) + 23.15).abs() < 1e-9);

            source.write(
                "later",
                &[(
                    "TMP",
                    later,
                    message(0, 270.0, [0; 4], surface, later_encoded),
                )],
                0,
            );
            engine.scan_once().unwrap();
            // Both resolved times and the omitted z are unchanged. The content
            // version must invalidate rendered tiles and WMS metatiles instead.
            assert_eq!(
                view.resolve_time(Some(reference), Some(reference)),
                Some(reference)
            );
            assert_eq!(
                view.resolve_reference_time(Some(reference), Some(reference)),
                Some(reference)
            );
            assert!((render(None) + 3.15).abs() < 1e-9);
            let first_level = if family == GribLevelType::Pressure {
                500.0
            } else {
                1.0
            };
            assert!((render(Some(first_level)) + 23.15).abs() < 1e-9);
            assert_ne!(view.content_version(), version);
            assert_eq!(single.content_version(), single_version);

            let updated = view.content_version();
            // A new file in another family forces catalog rebuilding, but
            // must preserve the already rendered upper-air tiles.
            source.write(
                "single-later",
                &[("OTHER", "surface", message(0, 280.0, [0; 4], 1, 0))],
                0,
            );
            engine.scan_once().unwrap();
            assert_eq!(view.content_version(), updated);
            assert_ne!(single.content_version(), single_version);
            engine.scan_once().unwrap();
            assert_eq!(view.content_version(), updated);
        }
    }

    #[test]
    fn render_version_follows_late_canonical_level_in_single_and_legacy_collections() {
        for split in [false, true] {
            let source = TestSource::new();
            source.write(
                "skin",
                &[("TMP", "surface", message(0, 280.0, [0; 4], 1, 0))],
                0,
            );
            let config = if split {
                split_config(&source)
            } else {
                source.config()
            };
            let engine = GribEngine::new("forecast", &config).unwrap();
            let views = engine.level_collections();
            let view = if split { &views[0] } else { &engine };
            let render = || {
                view.get_raster_tile(
                    [0.0, 0.0, 1.0, 1.0],
                    1,
                    1,
                    None,
                    &OutputCrs::Wgs84,
                    Some("TMP"),
                    None,
                    None,
                )
                .unwrap()
                .values
                .iter_values()
                .next()
                .unwrap()
                .unwrap()
            };
            let version = view.content_version();
            assert!((render() - 6.85).abs() < 1e-9);
            source.write(
                "air",
                &[("TMP", "2 m above ground", message(0, 290.0, [0; 4], 103, 2))],
                0,
            );
            engine.scan_once().unwrap();
            assert!((render() - 16.85).abs() < 1e-9);
            assert_ne!(view.content_version(), version);
        }
    }

    #[test]
    fn separate_files_share_one_source_and_keep_vertical_families_separate() {
        let source = TestSource::new();
        source.write(
            "surface",
            &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
            0,
        );
        source.write(
            "pressure",
            &[
                ("TMP", "850 mb", message(0, 270.0, [0; 4], 100, 85000)),
                ("TMP", "500 mb", message(0, 250.0, [0; 4], 100, 50000)),
            ],
            0,
        );
        source.write(
            "model",
            &[
                ("TMP", "1 hybrid level", message(0, 210.0, [0; 4], 105, 1)),
                ("TMP", "2 hybrid level", message(0, 220.0, [0; 4], 105, 2)),
            ],
            0,
        );
        let engine = GribEngine::new("forecast", &split_config(&source)).unwrap();
        let views = engine.level_collections();
        assert_eq!(views.len(), 3);
        for view in &views {
            assert!(Arc::ptr_eq(&engine.source, &view.source));
        }
        let [single, pressure, model] = views.as_slice() else {
            panic!()
        };
        assert_eq!(single.collection_id(), "forecast-single");
        assert_eq!(pressure.collection_id(), "forecast-pressure");
        assert_eq!(model.collection_id(), "forecast-model");
        assert_eq!(single.get_vertical_extent(), None);
        assert_eq!(single.raster_info().vertical, None);
        for (view, kind, levels) in [
            (pressure, VerticalKind::Pressure, vec![850.0, 500.0]),
            (model, VerticalKind::ModelLevel, vec![2.0, 1.0]),
        ] {
            let vertical = view.get_vertical_extent().unwrap();
            assert_eq!(vertical.kind, kind);
            assert_eq!(vertical.levels, levels);
            assert_eq!(view.raster_info().vertical, Some(vertical));
            assert_eq!(view.get_parameters(), ["TMP"]);
            assert_eq!(
                view.get_parameter_descriptions()["TMP"].label,
                "Temperature"
            );
        }
        assert!(single.get_parameter_descriptions()["TMP"]
            .label
            .contains("2 m above ground"));
        let tile = |view: &GribEngine, z| {
            view.get_raster_tile(
                [0.0, 0.0, 1.0, 1.0],
                1,
                1,
                None,
                &OutputCrs::Wgs84,
                Some("TMP"),
                z,
                None,
            )
            .unwrap()
            .values
            .iter_values()
            .next()
            .unwrap()
            .unwrap()
        };
        assert!((tile(single, None) - 6.85).abs() < 1e-9);
        assert!((tile(pressure, Some(500.0)) + 23.15).abs() < 1e-9);
        assert!((tile(model, Some(1.0)) + 63.15).abs() < 1e-9);
        assert!((tile(pressure, None) + 3.15).abs() < 1e-9);
        let bytes = engine.storage_bytes_read();
        tile(pressure, Some(500.0));
        assert_eq!(
            engine.storage_bytes_read(),
            bytes,
            "view reads reuse the source cache"
        );
        assert!(single
            .query_position("POINT(0.5 0.5)", None, None, Some(&[2.0]), None)
            .is_err());
        assert!(pressure
            .query_position("POINT(0.5 0.5)", None, None, Some(&[2.0]), None)
            .is_err());
        assert!(model
            .query_position("POINT(0.5 0.5)", None, None, Some(&[850.0]), None)
            .is_err());
        assert!(pressure
            .query_position(
                "POINT(0.5 0.5)",
                None,
                Some(&["missing".into()]),
                None,
                None
            )
            .is_err());

        // A no-change scan must not append already-known messages again.
        engine.scan_once().unwrap();
        assert_eq!(
            engine.catalog().latest_run().unwrap().steps[&0]
                .messages
                .len(),
            5
        );
    }

    #[test]
    fn profiles_and_area_preserve_levels_and_missing_fields() {
        let source = TestSource::new();
        source.write(
            "f000",
            &[
                ("TMP", "850 mb", message(0, 270.0, [0; 4], 100, 85000)),
                ("TMP", "500 mb", message(0, 250.0, [0; 4], 100, 50000)),
                ("OTHER", "850 mb", message(0, 260.0, [0; 4], 100, 85000)),
            ],
            0,
        );
        source.write(
            "f001",
            &[("TMP", "850 mb", message(0, 271.0, [0; 4], 100, 85000))],
            1,
        );
        let engine = GribEngine::new("forecast", &split_config(&source)).unwrap();
        let views = engine.level_collections();
        assert_eq!(views.len(), 1, "absent families are not collections");
        let pressure = &views[0];
        let CoverageResponse::Collection(profiles) = pressure
            .query_position("POINT(0.5 0.5)", None, Some(&["TMP".into()]), None, None)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(profiles.len(), 2);
        for profile in &profiles {
            let DomainDescription::VerticalProfile { z, .. } = &profile.domain else {
                panic!()
            };
            assert_eq!(z.values, [850.0, 500.0]);
            assert_eq!(profile.ranges["TMP"].shape, [2]);
            assert_eq!(profile.ranges["TMP"].axis_names, ["z"]);
        }
        assert_eq!(profiles[1].ranges["TMP"].values[1], None);
        let CoverageResponse::Single(series) = pressure
            .query_position(
                "POINT(0.5 0.5)",
                None,
                Some(&["TMP".into()]),
                Some(&[500.0]),
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        let DomainDescription::PointSeries { z: Some(z), .. } = &series.domain else {
            panic!()
        };
        assert_eq!(z.values, [500.0]);
        assert_eq!(series.ranges["TMP"].values[1], None);
        let reference: DateTime<Utc> = "2026-04-05T00:00:00Z".parse().unwrap();
        let CoverageResponse::Single(area) = pressure
            .query_area(
                "0,0,1,1",
                Some((reference, reference)),
                Some(&["TMP".into(), "OTHER".into()]),
                None,
                None,
            )
            .unwrap()
        else {
            panic!()
        };
        let DomainDescription::Grid { z: Some(z), .. } = &area.domain else {
            panic!()
        };
        assert_eq!(z.values, [850.0, 500.0]);
        assert_eq!(area.ranges["TMP"].shape, [2, 2, 2]);
        assert_eq!(area.ranges["TMP"].axis_names, ["z", "y", "x"]);
        assert!(area.ranges["OTHER"].values[4..].iter().all(Option::is_none));
        assert!((area.ranges["TMP"].values[4].unwrap() + 23.15).abs() < 1e-9);
    }

    #[test]
    fn family_instances_and_run_selection_follow_only_that_familys_files() {
        let source = TestSource::new();
        for (name, step) in [("pressure0", 0), ("pressure12", 12)] {
            source.write(
                name,
                &[("TMP", "850 mb", message(0, 270.0, [0; 4], 100, 85000))],
                step,
            );
        }
        source.write(
            "surface6",
            &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
            0,
        );
        let index_path = source.dir.join("surface6.idx");
        let index = std::fs::read_to_string(&index_path)
            .unwrap()
            .replace("d=2026040500", "d=2026040506");
        std::fs::write(index_path, index).unwrap();
        let engine = GribEngine::new("forecast", &split_config(&source)).unwrap();
        let views = engine.level_collections();
        let [single, pressure] = views.as_slice() else {
            panic!()
        };
        let old: DateTime<Utc> = "2026-04-05T00:00:00Z".parse().unwrap();
        let new = old + chrono::Duration::hours(6);
        let valid = old + chrono::Duration::hours(12);
        assert_eq!(single.get_instances()[0].reference_time, new);
        assert_eq!(pressure.get_instances()[0].reference_time, old);
        assert_eq!(pressure.get_temporal_extent(), Some((old, valid)));
        assert_eq!(
            pressure.resolve_reference_time(Some(valid), None),
            Some(old)
        );
        assert_eq!(pressure.resolve_time(Some(valid), None), Some(valid));
        assert!(pressure
            .query_position("POINT(0.5 0.5)", None, None, None, Some(new))
            .is_err());
        assert!(single
            .query_position("POINT(0.5 0.5)", None, None, None, Some(old))
            .is_err());
    }

    #[test]
    fn views_follow_poll_updates_and_respect_enabled_families_and_parameters() {
        let source = TestSource::new();
        source.write(
            "surface",
            &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
            0,
        );
        let mut config = split_config(&source);
        config.level_types = Some(vec![GribLevelType::Single, GribLevelType::Pressure]);
        config.parameters = Some(vec!["TMP".into()]);
        let engine = GribEngine::new("forecast", &config).unwrap();
        let single = engine.level_collections().remove(0);
        source.write(
            "pressure",
            &[("TMP", "850 mb", message(0, 270.0, [0; 4], 100, 85000))],
            0,
        );
        source.write(
            "model",
            &[("TMP", "1 hybrid level", message(0, 220.0, [0; 4], 105, 1))],
            0,
        );
        source.write(
            "next",
            &[("TMP", "2 m above ground", message(0, 281.0, [0; 4], 103, 2))],
            1,
        );
        engine.scan_once().unwrap();
        assert_eq!(engine.level_collections().len(), 2);
        assert_eq!(
            single.get_available_times().unwrap().len(),
            2,
            "existing views see new data"
        );
        assert_eq!(single.get_vertical_extent(), None);
        let mut config = split_config(&source);
        config.parameters = Some(vec!["unavailable".into()]);
        assert!(GribEngine::new("empty", &config)
            .unwrap()
            .level_collections()
            .is_empty());
    }
}
