//! Fixed-algorithm hash used for HTTP `ETag` values.
//!
//! `std::collections::hash_map::DefaultHasher` carries a doc-stable seed but
//! an explicitly unspecified algorithm — its output can rotate on a `rustup
//! update`. That's fine for in-process hash maps, but if the hash bytes ever
//! reach the network (as they do in `VectorTileKey::etag()` and
//! `ds_render::CacheKey::etag()`) a silent rotation means every outstanding
//! client `If-None-Match` flips from `304` to `200` overnight.
//!
//! FNV-1a is the simplest fixed algorithm that meets the requirement: tiny,
//! dependency-free, and stable forever by definition.

pub(crate) const FNV1A_OFFSET: u64 = 0xcbf29ce484222325;
pub(crate) const FNV1A_PRIME: u64 = 0x100000001b3;

#[inline]
pub(crate) fn fnv1a_mix(state: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *state ^= b as u64;
        *state = state.wrapping_mul(FNV1A_PRIME);
    }
}
