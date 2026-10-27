//! Per-request metadata snapshot (Critical Rule 10: capability accessors are
//! O(1) from a snapshot). Rebuilt by the poll loop from the store when
//! something changed; served through `ArcSwap`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::feature::{Feature, Geometry, PropertyValue, Related};
use ds_core::heap::{HeapSize, ARC_COUNTS};
use ds_core::map_engine::{ParameterInfo, RasterInfo};
use ds_core::model::{Location, LocationSnapshot};
use ds_render::station_symbols::StationLayers;

use crate::params::ParameterTable;
use crate::store::{ObsStore, StationInfo};

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub stations: Arc<Vec<StationInfo>>,
    /// What `get_locations` hands every request (#1083).
    pub locations: Arc<LocationSnapshot>,
    pub features: Arc<Vec<Feature>>,
    /// station id → index into `stations` / `locations` / `features`.
    pub index: Arc<HashMap<String, usize>>,
    pub spatial_extent: Option<[f64; 4]>,
    pub temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// Every distinct report time in the store, ascending: a station grid's
    /// analysis times without a `cadence` (#1206). Off-hour reports are
    /// instants too.
    pub instants: Arc<[DateTime<Utc>]>,
    pub report_count: usize,
    /// Bumped on every rebuild that changed anything (Features ETag, and
    /// the station map layers' `content_version`, #1204).
    pub version: u64,
    /// The station-symbol map layers' metadata (#1204): one layer per
    /// parameter, TIME steps over the newest 6 h of reports.
    pub raster_info: Arc<RasterInfo>,
    /// Estimated heap bytes of the store this snapshot was built from
    /// ([`ObsStore::heap_bytes`], #1307), as of the build.
    pub store_bytes: u64,
    /// Estimated heap bytes of this snapshot ([`Self::heap_bytes`]).
    pub bytes: u64,
}

impl Snapshot {
    pub fn empty(table: &ParameterTable, layers: &StationLayers) -> Self {
        Snapshot {
            stations: Arc::new(Vec::new()),
            locations: LocationSnapshot::empty(),
            features: Arc::new(Vec::new()),
            index: Arc::new(HashMap::new()),
            spatial_extent: None,
            temporal_extent: None,
            instants: Arc::from([]),
            report_count: 0,
            version: 0,
            raster_info: Arc::new(crate::map::raster_info(table, layers, None, None)),
            store_bytes: 0,
            bytes: 0,
        }
        .measured(0)
    }

    /// This snapshot with its own estimate and `store_bytes`.
    fn measured(mut self, store_bytes: u64) -> Self {
        self.store_bytes = store_bytes;
        self.bytes = self.heap_bytes();
        self
    }

    /// Estimated heap bytes of the snapshot (#1307): the station list (each
    /// name; the ids are the store's), the locations, the features, the id
    /// index, the instants and the map layers' metadata.
    fn heap_bytes(&self) -> u64 {
        (self.stations.heap_size()
            + self.locations.heap_size()
            + self.features.heap_size()
            + self.index.heap_size()
            + self.instants.heap_size()
            + raster_info_heap(&self.raster_info)) as u64
    }

    /// Build from the store. Stations are ordered by id for stable paging
    /// (and a stable station-map draw order).
    pub fn build(
        store: &ObsStore,
        version: u64,
        table: &ParameterTable,
        layers: &StationLayers,
    ) -> Self {
        let mut stations: Vec<StationInfo> = store.stations().map(|s| s.info.clone()).collect();
        stations.sort_by(|a, b| a.id.cmp(&b.id));
        let report_counts: HashMap<&str, usize> = store
            .stations()
            .map(|s| (&*s.info.id, s.rows.len()))
            .collect();
        // O(reports) into a set of the few distinct times: a global store
        // (~270 k reports) takes milliseconds, at most once per 10 s rebuild.
        let instants: BTreeSet<DateTime<Utc>> = store
            .stations()
            .flat_map(|s| s.rows.keys().copied())
            .collect();

        let mut spatial: Option<[f64; 4]> = None;
        let mut temporal: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
        let mut locations = Vec::with_capacity(stations.len());
        let mut features = Vec::with_capacity(stations.len());
        let mut index = HashMap::with_capacity(stations.len());
        for (i, s) in stations.iter().enumerate() {
            index.insert(s.id.to_string(), i);
            spatial = Some(match spatial {
                None => [s.lon, s.lat, s.lon, s.lat],
                Some(b) => [
                    b[0].min(s.lon),
                    b[1].min(s.lat),
                    b[2].max(s.lon),
                    b[3].max(s.lat),
                ],
            });
            temporal = Some(match temporal {
                None => (s.first_report, s.last_report),
                Some((a, b)) => (a.min(s.first_report), b.max(s.last_report)),
            });
            locations.push(Location {
                id: s.id.to_string(),
                label: s.name.clone().unwrap_or_else(|| s.id.to_string()),
                latitude: s.lat,
                longitude: s.lon,
            });
            let mut props: HashMap<String, PropertyValue> = HashMap::new();
            props.insert(
                "wigos_station_identifier".into(),
                PropertyValue::String(s.id.to_string()),
            );
            if let Some(n) = &s.name {
                props.insert("name".into(), PropertyValue::String(n.clone()));
            }
            if let Some(e) = s.elevation {
                props.insert("elevation".into(), PropertyValue::Float(e));
            }
            props.insert(
                "first_report".into(),
                PropertyValue::String(rfc3339(s.first_report)),
            );
            props.insert(
                "last_report".into(),
                PropertyValue::String(rfc3339(s.last_report)),
            );
            props.insert(
                "report_count".into(),
                PropertyValue::Integer(report_counts.get(&*s.id).copied().unwrap_or(0) as i64),
            );
            features.push(Feature {
                id: s.id.to_string(),
                geometry: Arc::new(Geometry::Point { x: s.lon, y: s.lat }),
                properties: Arc::new(props),
                // The station is the EDR location of its id (#1143).
                related: vec![Related::edr_location(&s.id)],
            });
        }
        Snapshot {
            stations: Arc::new(stations),
            locations: Arc::new(LocationSnapshot::new(locations)),
            features: Arc::new(features),
            index: Arc::new(index),
            spatial_extent: spatial,
            temporal_extent: temporal,
            instants: instants.into_iter().collect(),
            report_count: store.row_count(),
            version,
            raster_info: Arc::new(crate::map::raster_info(table, layers, spatial, temporal)),
            store_bytes: 0,
            bytes: 0,
        }
        .measured(store.heap_bytes())
    }
}

/// Estimated heap of the station layers' `RasterInfo`: its strings, TIME
/// steps and parameter list (station layers have no vertical axis or
/// native grid).
fn raster_info_heap(info: &Arc<RasterInfo>) -> usize {
    let parameters = info.parameters.capacity() * std::mem::size_of::<ParameterInfo>()
        + info
            .parameters
            .iter()
            .map(|p| p.name.heap_size() + p.title.heap_size() + p.unit.heap_size())
            .sum::<usize>();
    ARC_COUNTS
        + std::mem::size_of::<RasterInfo>()
        + info.native_crs.heap_size()
        + info.times.heap_size()
        + info.parameter.heap_size()
        + info.unit.heap_size()
        + parameters
        + info.layer_subtitle.heap_size()
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
