//! Estimated heap a CAP collection holds outside every cache (#1307): the
//! WIS2 accumulator, the published catalog and the geocode lookup, weighed
//! through `ds_core::heap`. Each figure is computed when its part changes —
//! the accumulator and the catalog at a rebuild, the lookup at load — and
//! read by `/metrics` as a number ([`crate::CapEngine::memory`]).

use std::sync::Arc;

use ds_core::heap::HeapSize;

use crate::parser::{CapAlert, CapArea, CapAreaHint, CapCircle, CapInfo, HintPart};
use crate::supersede::{AlertKey, MessageKey};

/// Estimated heap bytes of a CAP collection's parts, for
/// `cap_alert_bytes{part}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapMemory {
    /// WIS2 mode: the accumulator — held alerts with their geometry hints,
    /// both tombstone maps and the `data_id` index — as of the last
    /// rebuild. `None` for directory and feed sources.
    pub accumulator_bytes: Option<u64>,
    /// The published catalog: its area records with their geometry (a deep
    /// copy of the alert's) and properties, and its indexes.
    pub catalog_bytes: u64,
    /// The `geocode_geometry` zone lookup; `None` without one.
    pub geocode_bytes: Option<u64>,
}

impl HeapSize for CapCircle {
    fn heap_size(&self) -> usize {
        0
    }
}

impl HeapSize for HintPart {
    fn heap_size(&self) -> usize {
        0
    }
}

impl HeapSize for CapAreaHint {
    /// Every part's geometry as owned; an accumulator whose areas share a
    /// part counts it once itself.
    fn heap_size(&self) -> usize {
        self.parts.heap_size()
    }
}

impl HeapSize for CapArea {
    fn heap_size(&self) -> usize {
        self.area_desc.heap_size()
            + self.polygons.heap_size()
            + self.circles.heap_size()
            + self.geocodes.heap_size()
            + self.hint_geometry.heap_size()
    }
}

impl HeapSize for CapInfo {
    fn heap_size(&self) -> usize {
        self.language.heap_size()
            + self.categories.heap_size()
            + self.event.heap_size()
            + self.response_types.heap_size()
            + self.urgency.heap_size()
            + self.severity.heap_size()
            + self.certainty.heap_size()
            + self.sender_name.heap_size()
            + self.headline.heap_size()
            + self.description.heap_size()
            + self.instruction.heap_size()
            + self.web.heap_size()
            + self.event_codes.heap_size()
            + self.parameters.heap_size()
            + self.areas.heap_size()
    }
}

impl HeapSize for CapAlert {
    fn heap_size(&self) -> usize {
        self.identifier.heap_size()
            + self.sender.heap_size()
            + self.status.heap_size()
            + self.msg_type.heap_size()
            + self.scope.heap_size()
            + self.references.heap_size()
            + self.infos.heap_size()
    }
}

impl HeapSize for AlertKey {
    fn heap_size(&self) -> usize {
        self.sender.heap_size() + self.identifier.heap_size()
    }
}

impl HeapSize for MessageKey {
    fn heap_size(&self) -> usize {
        self.alert.heap_size()
    }
}

/// Bytes the hint geometries of `alerts` count more than once: a part
/// shared by several areas (a restored snapshot shares each zone polygon
/// between the `<info>` languages naming it) is held once.
pub(crate) fn shared_hint_bytes<'a>(alerts: impl Iterator<Item = &'a CapAlert>) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut repeated = 0;
    let parts = alerts
        .flat_map(|alert| &alert.infos)
        .flat_map(|info| &info.areas)
        .filter_map(|area| area.hint_geometry.as_ref())
        .flat_map(|hint| hint.parts.values());
    for part in parts {
        if !seen.insert(Arc::as_ptr(part)) {
            repeated += HeapSize::heap_size(part);
        }
    }
    repeated
}
