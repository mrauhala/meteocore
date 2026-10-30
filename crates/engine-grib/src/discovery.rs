//! Discovery descriptors are rebuilt on catalog/header publication, never on
//! map/tile request paths. Each family uses its own latest-run representative.

use super::*;
use crate::catalog::MessageEntry;
use ds_cache::plan::{CacheDemand, DemandSlot, Scope, DEMAND_FIELDS};
use ds_core::wind::{GridAxes, ParameterFacts, WindFacts};
use metadata::GridGeometry;

type MessageId = (String, u64);

pub(super) struct Discovery {
    pub views: BTreeMap<Option<GribLevelType>, Arc<RasterInfo>>,
    /// Each view's wind components (#897), rebuilt with `views`.
    pub wind: BTreeMap<Option<GribLevelType>, Arc<WindFacts>>,
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
            wind: BTreeMap::new(),
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
                native_grid: None,
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

/// How the decoded-grid cache's demand is computed (#1309).
const GRID_DEMAND_RULE: &str = "16 × decoded field bytes";
/// How the compressed-message cache's demand is computed (#1309).
const MESSAGE_DEMAND_RULE: &str = "16 × mean message bytes of the latest run";

/// A slot for a source's demand on its cache `cache`, as the server's
/// memory plan lists it: scoped to the source's collection.
pub(super) fn demand_slot(cache: &'static str, collection_id: &str) -> DemandSlot {
    DemandSlot::new(
        cache,
        Scope::Source {
            engine: "grib",
            id: collection_id.to_string(),
        },
        collection_id,
    )
}

/// The decoded-grid cache's demand for fields of `nodes` values (#1309):
/// [`DEMAND_FIELDS`] of them, each weighed as the cache weighs it.
pub(super) fn grid_demand(nodes: u64) -> CacheDemand {
    let field = GridCache::estimated_weight(nodes);
    CacheDemand::demand(
        DEMAND_FIELDS.saturating_mul(field),
        GRID_DEMAND_RULE,
        "[grib] grid_cache_mb",
    )
    .input("fields", DEMAND_FIELDS)
    .input("nodes", nodes)
    .input("field_bytes", field)
}

/// The compressed-message cache's demand from `catalog` (#1309):
/// [`DEMAND_FIELDS`] messages of the mean weight of the latest run's, each
/// weighed as the cache weighs it. `None` before a run with a known message
/// length.
pub(super) fn message_demand(catalog: &Catalog) -> Option<CacheDemand> {
    let (mut bytes, mut messages) = (0u64, 0u64);
    for file in catalog.latest_run()?.steps.values() {
        for entry in &file.messages {
            if let Some(length) = entry.length {
                bytes += crate::message_cache::message_weight(file.message_url(entry), length);
                messages += 1;
            }
        }
    }
    let mean = bytes.checked_div(messages)?;
    Some(
        CacheDemand::demand(
            DEMAND_FIELDS.saturating_mul(mean),
            MESSAGE_DEMAND_RULE,
            "[grib] message_cache_mb",
        )
        .input("fields", DEMAND_FIELDS)
        .input("messages", messages)
        .input("message_bytes", mean),
    )
}

impl GribEngine {
    pub(super) fn publish_catalog(&self, catalog: Catalog) {
        if let Some(demand) = message_demand(&catalog) {
            self.publish_demand(&self.source.message_demand, demand);
        }
        self.source.catalog.store(Arc::new(catalog));
        // A new run may pack a parameter differently: let the CCSDS point
        // path try each parameter again (#1300).
        if let Some(index) = &self.source.point_index {
            index.forget_layouts();
        }
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
        let mut wind = BTreeMap::new();
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
                wind.insert(family, Arc::new(view.build_wind_facts(catalog)));
                (family, Arc::new(view.build_raster_info(catalog, geometry)))
            })
            .collect();
        discovery.wind = wind;
        let nodes = discovery
            .views
            .values()
            .filter_map(|info| decoded_nodes(info.grid_size))
            .max();
        drop(discovery);
        // A decode on a query path publishes a geometry too: only a change
        // is published, so a query pays no plan check.
        if let Some(demand) = nodes.map(grid_demand) {
            if self.source.grid_demand.get().as_ref() != Some(&demand) {
                self.publish_demand(&self.source.grid_demand, demand);
            }
        }
    }

    /// Publish `demand` in `slot`, and log the plan's WARN when the cache
    /// has just fallen below it.
    fn publish_demand(&self, slot: &DemandSlot, demand: CacheDemand) {
        if let Some(short) = slot.publish(demand) {
            tracing::warn!("{short}");
        }
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
    /// What each parameter's metadata states for the derived parameters
    /// (wind #897, geopotential height #948, humidity #898): its WMO triple
    /// and GRIB2 component flag, read by the header probe or a decode
    /// (`Unknown` until then), its served unit, and the canonical level it
    /// is served at, or the view's vertical axis it spans (a pressure
    /// view's level is the pressure a humidity from `q` reads). All of it
    /// from the newest run that has the parameter (#1026), so a component
    /// only an older run still has pairs at its own level.
    fn build_wind_facts(&self, catalog: &Catalog) -> WindFacts {
        let keys = catalog.advertised_keys();
        // A pressure or model-level view's parameters span its vertical
        // axis; a request reads every component at the same level.
        let single_level = self.vertical_kind().is_none();
        let parameters = catalog
            .all_params()
            .into_iter()
            .map(|name| {
                let meta = self.param_metadata(catalog, &name);
                let level = keys
                    .get(&name)
                    .filter(|_| single_level)
                    .map(|key| match key.level {
                        Some(level) => format!("{}:{level}", key.levtype),
                        None => key.levtype.clone(),
                    });
                let level_label = meta
                    .first_surface_type
                    .and_then(|t| units::format_level_qualifier(t, meta.first_surface_value));
                ParameterFacts {
                    grib: meta.triple,
                    frame: meta.uv_frame,
                    level,
                    level_label,
                    vertical: self.vertical_kind(),
                    unit: meta.display.display_unit.to_string(),
                    ..ParameterFacts::new(name)
                }
            })
            .collect();
        WindFacts {
            // Template 3.0 only (`reader::extract_grid_params` rejects every
            // other grid): x is east and y north, so a grid-relative flag
            // needs no rotation. A rotated or projected template must
            // report its own axes here.
            grid: GridAxes::NorthAligned,
            parameters,
        }
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
            native_grid: None,
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

    /// #1309: the decoded-grid cache's demand is sixteen fields as the
    /// cache weighs them. A 0.1° global grid suggests about 0.44 GiB, which
    /// the 256 MiB default holds ten fields of; a 0.25° one 73 MiB.
    #[test]
    fn the_grid_cache_demands_sixteen_fields() {
        let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
        let tenth = grid_demand(decoded_nodes(Some([3600, 1800])).unwrap());
        assert_eq!(tenth.setting, "[grib] grid_cache_mb");
        assert_eq!(tenth.inputs[..2], [("fields", 16), ("nodes", 3601 * 1801)]);
        assert_eq!(tenth.bytes, 16 * tenth.inputs[2].1);
        assert_eq!(tenth.inputs[2].1, GridCache::estimated_weight(3601 * 1801));
        let suggested = gib(tenth.suggested_bytes());
        assert!((0.4..0.5).contains(&suggested), "{suggested} GiB");
        let quarter = grid_demand(decoded_nodes(Some([1440, 720])).unwrap());
        assert_eq!(quarter.suggested_bytes(), 73 * ds_cache::MIB);
    }

    /// #1309: a source publishes its grid demand once its geometry is
    /// known, and its message cache's from the latest run's messages of a
    /// known length: sixteen of their mean weight.
    #[test]
    fn a_source_publishes_its_grid_and_message_demand() {
        let source = TestSource::new();
        for (name, step) in [("f000", 0), ("f003", 3)] {
            source.write(
                name,
                &[
                    ("TMP", "2 m above ground", message(0, 280.0, [0; 4], 103, 2)),
                    // The last record's length is unknown to a wgrib2 index.
                    ("TAIL", "surface", message(0, 0.0, [0; 4], 1, 0)),
                ],
                step,
            );
        }
        let engine = GribEngine::new("demand", &source.config()).unwrap();
        let grid = engine.source.grid_demand.get().expect("geometry published");
        assert_eq!(grid, grid_demand(4));
        let messages = engine.source.message_demand.get().expect("lengths known");
        assert_eq!(messages.setting, "[grib] message_cache_mb");
        assert_eq!(messages.inputs[..2], [("fields", 16), ("messages", 2)]);
        let mean = messages.inputs[2].1;
        let length = message(0, 280.0, [0; 4], 103, 2).len() as u64;
        assert!((length + 1..length + 1024).contains(&mean), "{mean}");
        assert_eq!(messages.bytes, 16 * mean);
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
                None,
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
