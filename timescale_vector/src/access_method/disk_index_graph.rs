use pgrx::PgRelation;

use crate::util::ItemPointer;

use super::{
    graph::NodeNeighbor, meta_page::MetaPage, model::NeighborWithDistance, storage::Storage,
};

pub struct DiskIndexGraph {}

impl DiskIndexGraph {
    pub fn new() -> Self {
        Self {}
    }

    pub fn get_neighbors<N: NodeNeighbor>(&self, node: &N) -> Vec<ItemPointer> {
        node.get_index_pointer_to_neighbors()
    }

    pub fn get_neighbors_with_full_vector_distances<S: Storage>(
        &self,
        index: &PgRelation,
        neighbors_of: ItemPointer,
        storage: &S,
        result: &mut Vec<NeighborWithDistance>,
    ) -> bool {
        storage.get_neighbors_with_full_vector_distances_from_disk(index, neighbors_of, result)
    }

    pub fn set_neighbors<S: Storage>(
        &mut self,
        storage: &S,
        index: &PgRelation,
        meta_page: &MetaPage,
        neighbors_of: ItemPointer,
        new_neighbors: Vec<NeighborWithDistance>,
    ) {
        storage.set_neighbors_on_disk(index, meta_page, neighbors_of, new_neighbors.as_slice());
    }

    pub fn max_neighbors(&self, meta_page: &MetaPage) -> usize {
        meta_page.get_num_neighbors() as _
    }
}
