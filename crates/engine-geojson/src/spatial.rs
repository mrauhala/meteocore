use ds_core::feature::Bbox;
use rstar::{RTree, RTreeObject, AABB};

#[derive(Debug, Clone)]
pub struct IndexedFeature {
    pub index: usize,
    envelope: AABB<[f64; 2]>,
}

impl RTreeObject for IndexedFeature {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

pub struct SpatialIndex {
    tree: RTree<IndexedFeature>,
}

impl SpatialIndex {
    /// Build an R-tree from (original_index, bbox) pairs.
    pub fn build_indexed(indexed_bboxes: &[(usize, [f64; 4])]) -> Self {
        let entries: Vec<IndexedFeature> = indexed_bboxes
            .iter()
            .map(|(i, bbox)| IndexedFeature {
                index: *i,
                envelope: AABB::from_corners([bbox[0], bbox[1]], [bbox[2], bbox[3]]),
            })
            .collect();

        SpatialIndex {
            tree: RTree::bulk_load(entries),
        }
    }

    /// Indices, in ascending (file) order, of features whose bounding box
    /// intersects the query bbox: the candidates a geometry test refines.
    /// A box crossing the antimeridian (west > east) is queried as its two
    /// halves; as one AABB its corners would normalise into the complement.
    pub fn query(&self, bbox: &Bbox) -> Vec<usize> {
        let halves: &[(f64, f64)] = if bbox.crosses_antimeridian() {
            &[(bbox.west, 180.0), (-180.0, bbox.east)]
        } else {
            &[(bbox.west, bbox.east)]
        };
        let mut indices: Vec<usize> = halves
            .iter()
            .flat_map(|&(west, east)| {
                let query_aabb = AABB::from_corners([west, bbox.south], [east, bbox.north]);
                self.tree
                    .locate_in_envelope_intersecting(&query_aabb)
                    .map(|entry| entry.index)
                    .collect::<Vec<_>>()
            })
            .collect();
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}
