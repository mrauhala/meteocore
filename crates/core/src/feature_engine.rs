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
        .ok()
        .and_then(|p| p.number_matched)
        .unwrap_or(0)
    }

    /// The collection metadata's `numberItems`: [`feature_count`](Self::
    /// feature_count) by default. `None` leaves it out, for a collection
    /// that cannot count its features from memory: an event table with
    /// unbounded history (engine-postgis `events`, #503).
    fn number_items(&self) -> Option<usize> {
        Some(self.feature_count())
    }

    /// Whether `/items` pages this collection by cursor instead of offset
    /// (#503): a source read in key order per request, where an offset would
    /// re-read every row before it. The API layer then passes the previous
    /// page's [`FeaturePage::next_cursor`] as [`FeatureQuery::cursor`],
    /// links `next` with it and turns a non-zero `offset` down with a 400.
    /// An engine that pages by offset (the default) gets a `cursor` turned
    /// down instead.
    fn pages_by_cursor(&self) -> bool {
        false
    }

    /// Whether [`get_features`](Self::get_features) and
    /// [`get_feature`](Self::get_feature) read a database or another source
    /// on each call instead of answering from memory (#503). The API layer
    /// then runs them on ds-executor's shared query runtime, under its own
    /// admission and deadline, never on a request worker (root CLAUDE.md
    /// Critical Rule 7). An engine that answers from memory (the default) is
    /// called in place.
    fn reads_on_request(&self) -> bool {
        false
    }

    /// The interval `/items` reads when a request has no `datetime`, for an
    /// engine whose features are then the window ending `now` (engine-postgis
    /// `events`, its `default_datetime`, #503). The API layer resolves it
    /// once per request and passes it as [`FeatureQuery::datetime`], and the
    /// request's `next` and `prev` links carry it as an explicit `datetime`,
    /// so every page of a chain reads one fixed interval. Recomputed per
    /// page, the window would slide, and newest first its lower edge would
    /// cut off the oldest features the chain still had to reach. `None`
    /// (the default): no such window, and a query without `datetime` is
    /// answered as before.
    fn default_datetime(&self, now: DateTime<Utc>) -> Option<DatetimeInterval> {
        let _ = now;
        None
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

    /// A readable name for `feature` built from its own properties, for
    /// listings and pages that would otherwise show an opaque id (#1143):
    /// engine-cap's `event · areaDesc`. The API layer serves it as the
    /// feature's `title` property unless the feature has one. `None` (the
    /// default) leaves the generic label rule: a `name`-like property, else
    /// the id. A label states values the feature carries; it never adds any.
    fn label(&self, feature: &Feature) -> Option<String> {
        let _ = feature;
        None
    }

    /// The properties a listing shows as its first columns, most telling
    /// first (#1143): a storm cell's significance before its age. Empty (the
    /// default) falls back to [`sortables`](Self::sortables), then to the
    /// remaining properties alphabetically.
    fn summary_properties(&self) -> &[&'static str] {
        &[]
    }

    /// What one feature of this collection is, in one line, where the
    /// collection's metadata alone could mislead (#1143): a radar network's
    /// description speaks of volumes, while its items are the sites. Shown
    /// on the collection's HTML overview.
    fn items_description(&self) -> Option<&'static str> {
        None
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
