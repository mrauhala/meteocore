use chrono::{DateTime, Utc};

use crate::error::DataServerError;
use crate::feature::{DatetimeInterval, Feature, FeaturePage, FeatureQuery, FilterableProperties};

pub trait FeatureEngine: Send + Sync {
    /// Get a page of features matching the query.
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError>;

    /// Get a single feature by ID.
    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError>;

    /// One feature by ID as it stood in the time slice `get_features` selects
    /// for `datetime` — the by-id counterpart of a `datetime` query, for an
    /// engine that retains history (engine-nowcast's cell snapshots). Absent
    /// from that slice, or no slice selected, is `FeatureNotFound`.
    ///
    /// Following one feature back through history otherwise means paging
    /// every slice to find it (#646). The default refuses rather than doing
    /// that scan: an engine without retained slices has no cheaper answer,
    /// and a silent full scan is the cost this method exists to avoid.
    fn get_feature_at(
        &self,
        feature_id: &str,
        datetime: &DatetimeInterval,
    ) -> Result<Feature, DataServerError> {
        let _ = (feature_id, datetime);
        Err(DataServerError::InvalidParameter(
            "this collection does not serve a feature by id at a time".into(),
        ))
    }

    /// Total number of features in the collection. Used for collection metadata.
    fn feature_count(&self) -> usize {
        self.get_features(&FeatureQuery {
            limit: 0,
            ..Default::default()
        })
        .map(|p| p.number_matched)
        .unwrap_or(0)
    }

    /// Feature properties this engine can sort on, in the order they should be
    /// advertised. Empty (the default) means sorting is not supported and the
    /// API layer rejects `sortby` with a 400.
    ///
    /// Opt-in rather than universal on purpose: sorting must happen before
    /// pagination, so an engine that streams or pages lazily would have to
    /// materialize its whole collection to honour it (the cost #532 exists to
    /// avoid). An engine advertises a property here only if it can sort on it
    /// without that.
    ///
    /// A property whose natural order differs from its serialized order must
    /// NOT be listed. `severity` is the live example: sorted as a string it
    /// reads `moderate < severe < very_severe < weak`, putting the weakest
    /// cells last and looking almost right — sort on `significance` instead,
    /// which already incorporates it.
    fn sortables(&self) -> &[&'static str] {
        &[]
    }

    /// Properties accepted as Part 1 equality filters. Engines must apply
    /// them before paging and number_matched, using the shared matcher.
    /// Dynamic names belong in a prebuilt snapshot, never a per-request scan.
    /// Empty means no property filtering; the API rejects unknown names.
    fn filterables(&self) -> FilterableProperties {
        static EMPTY: std::sync::LazyLock<FilterableProperties> =
            std::sync::LazyLock::new(Default::default);
        EMPTY.clone()
    }

    /// Spatial extent as [west, south, east, north], if available.
    fn spatial_extent(&self) -> Option<[f64; 4]> {
        None
    }

    /// Temporal extent `(start, end)` of the collection, if the features carry a
    /// time dimension (e.g. CAP alert validity windows). Surfaced as the
    /// `extent.temporal.interval` in the OGC API – Features collection metadata.
    /// `None` (the default) means the collection has no temporal extent — per
    /// OGC API – Common – Part 2, the element is then simply omitted.
    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        None
    }

    /// Instants of the retained time slices a `datetime` query selects
    /// between, oldest first — for engine-nowcast one per cell snapshot,
    /// including snapshots that hold no features. A caller walking history
    /// steps from slice to slice instead of probing instants until one
    /// resolves (#646). Empty (the default) means the engine has no discrete
    /// slices to enumerate; `temporal_extent` still describes its span.
    fn available_times(&self) -> Vec<DateTime<Utc>> {
        Vec::new()
    }

    /// Whether `FeatureQuery::datetime` filters this collection's features.
    /// An engine whose features carry no time (a static GeoJSON file, a
    /// station inventory) returns `false`, and the API answers `datetime`
    /// with a 400 rather than an unfiltered 200 (#682; root CLAUDE.md:
    /// an unsupported parameter must not be silently ignored).
    fn has_time_dimension(&self) -> bool {
        true
    }

    /// Opaque token that changes when the underlying feature data changes.
    ///
    /// Used as a data-version component in vector-tile ETags so that an
    /// `/admin/collections/reload` (or any in-process refresh) invalidates
    /// previously-issued tile ETags instead of serving `304 Not Modified`
    /// indefinitely. Engines that load once and never change can leave the
    /// default `0`; consumers should treat the value as opaque.
    fn data_version(&self) -> u64 {
        0
    }
}
