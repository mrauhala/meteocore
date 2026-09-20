//! Discovery descriptors are rebuilt on catalog/header publication, never on
//! map/tile request paths. Each family uses its own latest-run representative.

use super::*;
use crate::catalog::MessageEntry;
use metadata::GridGeometry;

type MessageId = (String, u64);

pub(super) struct Discovery {
    pub views: BTreeMap<Option<GribLevelType>, Arc<RasterInfo>>,
    pub empty: Arc<RasterInfo>,
    // At most one representative per view. Cache unknown/unsupported geometry
    // too, so a successful header probe is not repeated every poll.
    grids: HashMap<MessageId, Option<GridGeometry>>,
    wanted: HashSet<MessageId>,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            views: BTreeMap::new(),
            grids: HashMap::new(),
            wanted: HashSet::new(),
            empty: Arc::new(RasterInfo {
                native_crs: "CRS:84".into(),
                spatial_extent: None,
                times: vec![],
                parameter: "2t".into(),
                unit: String::new(),
                parameters: vec![],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: vec![],
            }),
        }
    }
}

impl Discovery {
    fn needs_geometry(&self, url: &str, offset: u64, geometry: Option<GridGeometry>) -> bool {
        self.wanted
            .iter()
            .any(|(key, pos)| key == url && *pos == offset)
            && !self.grids.iter().any(|((key, pos), old)| {
                key == url && *pos == offset && (old.is_some() || geometry.is_none())
            })
    }
}

fn representative(catalog: &Catalog) -> Option<(&StepFile, &MessageEntry)> {
    catalog
        .latest_run()?
        .steps
        .values()
        .find_map(|file| file.default_message().map(|message| (file, message)))
}

fn catalogs(catalog: &Catalog) -> impl Iterator<Item = (Option<GribLevelType>, &Catalog)> {
    std::iter::once((None, catalog)).chain(
        catalog
            .families
            .iter()
            .map(|(&family, catalog)| (Some(family), catalog.as_ref())),
    )
}

impl GribEngine {
    pub(super) fn publish_catalog(&self, catalog: Catalog) {
        self.source.catalog.store(Arc::new(catalog));
        self.refresh_discovery();
    }

    pub(super) fn refresh_discovery(&self) {
        // Serialize rebuilds before loading the catalog. A late query/header
        // completion cannot overwrite a newer poll's descriptors.
        let mut discovery = self.source.discovery.write().unwrap();
        let catalog = self.source.catalog.load_full();
        let wanted: HashSet<_> = catalogs(&catalog)
            .filter_map(|(_, c)| representative(c))
            .map(|(file, entry)| (file.message_url(entry).to_owned(), entry.offset))
            .collect();
        discovery.grids.retain(|id, _| wanted.contains(id));
        discovery.wanted = wanted;
        discovery.views = catalogs(&catalog)
            .map(|(family, catalog)| {
                let geometry = representative(catalog).and_then(|(file, entry)| {
                    discovery
                        .grids
                        .get(&(file.message_url(entry).to_owned(), entry.offset))
                        .copied()
                        .flatten()
                });
                let view = Self {
                    collection_id: self.collection_id.clone(),
                    family,
                    source: self.source.clone(),
                };
                (family, Arc::new(view.build_raster_info(catalog, geometry)))
            })
            .collect();
    }

    pub(super) fn geometry_probes<'a>(
        &self,
        catalog: &'a Catalog,
    ) -> Vec<(&'a StepFile, &'a MessageEntry)> {
        let discovery = self.source.discovery.read().unwrap();
        catalogs(catalog)
            .filter_map(|(_, c)| representative(c))
            .filter(|(file, entry)| {
                !discovery
                    .grids
                    .contains_key(&(file.message_url(entry).to_owned(), entry.offset))
            })
            .collect()
    }

    pub(super) fn populate_geometry(
        &self,
        url: &str,
        entry: &MessageEntry,
        geometry: Option<GridGeometry>,
    ) -> bool {
        // The usual decoded-cache hit needs neither a write lock nor a URL
        // allocation. The representative sets contain at most four identities.
        if !self
            .source
            .discovery
            .read()
            .unwrap()
            .needs_geometry(url, entry.offset, geometry)
        {
            return false;
        }
        let mut discovery = self.source.discovery.write().unwrap();
        if !discovery.needs_geometry(url, entry.offset, geometry) {
            return false;
        }
        discovery
            .grids
            .insert((url.to_owned(), entry.offset), geometry);
        true
    }
    fn build_raster_info(
        &self,
        catalog: &Catalog,
        geometry: Option<metadata::GridGeometry>,
    ) -> RasterInfo {
        let times = catalog.all_valid_times();

        // Build parameter list from catalog using cached metadata (populated
        // as each parameter is first probed or decoded).
        let params: Vec<ds_core::map_engine::ParameterInfo> = catalog
            .all_params()
            .into_iter()
            .map(|p| {
                let meta = self.param_metadata(catalog, &p);
                let label = meta.label();
                ds_core::map_engine::ParameterInfo {
                    name: p,
                    title: label,
                    unit: meta.display.display_unit.to_string(),
                }
            })
            .collect();

        let default_param = catalog
            .latest_run()
            .and_then(|run| run.steps.values().next_back())
            .and_then(StepFile::default_message)
            .map(|m| m.param.clone())
            .unwrap_or_else(|| "2t".to_string());

        let default_unit = self
            .param_metadata(catalog, &default_param)
            .display
            .display_unit
            .to_string();

        RasterInfo {
            // Regular lat/lon grids served lon-first -> CRS:84, not the
            // lat-first EPSG:4326 (which would make a conformant client swap
            // axes). Matches engine-geotiff/odim/querydata for `storageCrs`.
            native_crs: "CRS:84".to_string(),
            spatial_extent: geometry.map(|g| g.bbox),
            times,
            parameter: default_param,
            unit: default_unit,
            parameters: params,
            vertical: self.vertical_extent(catalog),
            grid_size: geometry.map(|g| g.cells),
            layer_subtitle: None,
            // Each retained forecast run is a selectable reference time (WMS
            // `reference_time` dimension); ascending, latest last.
            reference_times: catalog.runs.keys().copied().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{message, TestSource};

    #[test]
    fn regional_metadata_is_shared_without_io_or_value_decoding() {
        let source = TestSource::new();
        source.write(
            "f000",
            &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
            0,
        );
        let engine = GribEngine::new("regional", &source.config()).unwrap();
        let before = engine.storage_bytes_read();
        let info = engine.raster_info_shared();
        assert_eq!(info.spatial_extent, Some([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(engine.get_spatial_extent(), info.spatial_extent);
        assert_eq!(info.grid_size, Some([1, 1]));
        assert_eq!(info.parameters[0].unit, "°C");
        assert_eq!(engine.source.grid_cache.as_ref().unwrap().len(), 0);
        for _ in 0..100 {
            assert!(Arc::ptr_eq(&info, &engine.raster_info_shared()));
            assert_eq!(
                engine.get_temporal_extent(),
                Some((info.times[0], info.times[0]))
            );
        }
        assert_eq!(before, engine.storage_bytes_read());
        engine.probe_new_parameters();
        assert!(Arc::ptr_eq(&info, &engine.raster_info_shared()));
        assert_eq!(
            before,
            engine.storage_bytes_read(),
            "no duplicate geometry probe"
        );
    }

    #[test]
    fn geometry_matches_global_regional_seam_and_duplicate_seam_grids() {
        for ni in [1440, 1441] {
            let grid = GridGeometry::new(ni, 721, 0.0, 90.0, 0.25, -0.25).unwrap();
            assert_eq!(grid.bbox, [-180.0, -90.0, 180.0, 90.0]);
            assert_eq!(grid.cells, [1440, 720]);
            let extent = ds_core::ogc_extent::build_extent(
                Some(grid.bbox),
                Some(grid.cells),
                "CRS:84",
                &[],
                None,
            )
            .unwrap();
            let axes = extent.spatial.unwrap().grid.unwrap();
            assert_eq!(axes[0].resolution, 0.25);
            assert_eq!(axes[1].resolution, 0.25);
        }
        assert_eq!(
            GridGeometry::new(3, 2, 179.0, 2.0, 1.0, -1.0).unwrap().bbox,
            [179.0, 1.0, -179.0, 2.0]
        );
        assert!(GridGeometry::new(2, 2, 0.0, 91.0, 1.0, -1.0).is_none());
    }

    #[test]
    fn failed_headers_retry_and_new_runs_refresh_geometry_even_for_known_parameters() {
        let source = TestSource::new();
        source.write(
            "f000",
            &[("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2))],
            0,
        );
        let bytes = std::fs::read(source.dir.join("f000.grib2")).unwrap();
        std::fs::remove_file(source.dir.join("f000.grib2")).unwrap();
        let engine = GribEngine::new("retry", &source.config()).unwrap();
        let unknown = engine.raster_info_shared();
        assert!(unknown.spatial_extent.is_none());
        std::fs::write(source.dir.join("f000.grib2"), bytes).unwrap();
        engine.scan_once().unwrap();
        let first = engine.raster_info_shared();
        assert_eq!(first.spatial_extent, Some([0.0, 0.0, 1.0, 1.0]));
        assert!(!Arc::ptr_eq(&unknown, &first));
        let mut shifted = message(0, 280.0, [0; 4], 103, 2);
        shifted[87..91].copy_from_slice(&20_000_000u32.to_be_bytes());
        shifted[96..100].copy_from_slice(&21_000_000u32.to_be_bytes());
        source.write("next", &[("TMP", "2 m above ground", shifted)], 0);
        let path = source.dir.join("next.idx");
        let index = std::fs::read_to_string(&path)
            .unwrap()
            .replace("2026040500", "2026040506");
        std::fs::write(path, index).unwrap();
        engine.scan_once().unwrap();
        let next = engine.raster_info_shared();
        assert_eq!(next.spatial_extent, Some([20.0, 0.0, 21.0, 1.0]));
        assert_eq!(next.reference_times.len(), 2);
        assert_eq!(next.times.len(), 2);
        assert_eq!(
            first.spatial_extent,
            Some([0.0, 0.0, 1.0, 1.0]),
            "old snapshots stay immutable"
        );
        assert_eq!(engine.source.discovery.read().unwrap().grids.len(), 1);
        let mut empty = Catalog::new();
        empty.refresh_metadata();
        engine.publish_catalog(empty);
        assert!(engine.raster_info_shared().spatial_extent.is_none());
        assert!(engine.raster_info_shared().times.is_empty());
        assert!(engine.source.discovery.read().unwrap().grids.is_empty());
    }

    #[test]
    fn each_vertical_family_uses_its_own_grid_and_levels() {
        let source = TestSource::new();
        let mut pressure = message(0, 250.0, [0; 4], 100, 85000);
        pressure[87..91].copy_from_slice(&10_000_000u32.to_be_bytes());
        pressure[96..100].copy_from_slice(&11_000_000u32.to_be_bytes());
        source.write(
            "f000",
            &[
                ("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
                ("TMP", "850 mb", pressure),
            ],
            0,
        );
        let mut config = source.config();
        config.level_types = Some(vec![
            GribLevelType::Single,
            GribLevelType::Pressure,
            GribLevelType::Model,
        ]);
        let owner = GribEngine::new("families", &config).unwrap();
        for (family, bbox, level) in [
            (GribLevelType::Single, Some([0.0, 0.0, 1.0, 1.0]), None),
            (
                GribLevelType::Pressure,
                Some([10.0, 0.0, 11.0, 1.0]),
                Some(850.0),
            ),
            (GribLevelType::Model, None, None),
        ] {
            let view = GribEngine {
                collection_id: "view".into(),
                family: Some(family),
                source: owner.source.clone(),
            };
            let info = view.raster_info_shared();
            assert_eq!(info.spatial_extent, bbox);
            assert_eq!(info.vertical.as_ref().map(|v| v.levels[0]), level);
            assert_eq!(view.get_vertical_extent(), info.vertical);
        }
    }
}
