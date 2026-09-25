//! Response caching for this router: the shared conditional-GET middleware
//! (#499), now hosted by api-common so every OGC API surface uses one copy.

pub use api_common::caching::conditional_get;
